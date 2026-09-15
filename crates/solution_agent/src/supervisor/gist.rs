//! Condensing long observer prose into a one-or-two-sentence gist.
//!
//! The supervisor writes operator-facing text (`ask` questions, `done`
//! reasoning) as a full markdown paragraph — bold, inline code, lists, several
//! hundred characters. That is exactly right for the in-chat Observer bubble,
//! which renders markdown and is scrollable, but wrong for the two ATTENTION
//! surfaces that sit outside the transcript:
//!
//! * the freedesktop toast (top-right corner) — it has no markdown renderer, so
//!   the raw `**bold**` / backticks leak, and a paragraph is unreadable in the
//!   two lines the shell gives it;
//! * the pinned question banner above the compose row — a plain `Label`, so it
//!   dumped the same raw markdown a second time directly under the bubble that
//!   had just rendered it properly.
//!
//! Both want the same thing: the lede, flattened to plain text and capped. This
//! module is that transform. It is pure text-in/text-out so it unit-tests in
//! isolation, and it is applied at the surface (not at the source) so the full
//! text always survives in the transcript and the session log.

/// Hard cap on a condensed gist, in characters (not bytes — the text is
/// routinely Cyrillic). Sized for the ~2 body lines a desktop notification
/// actually shows before the shell ellipsizes it.
pub const GIST_MAX_CHARS: usize = 180;

/// Sentence terminators recognised when splitting the flattened text.
const TERMINATORS: [char; 4] = ['.', '!', '?', '…'];

/// Flatten markdown to plain text and keep the first sentence — plus the second
/// when both still fit inside [`GIST_MAX_CHARS`]. Over-long single sentences are
/// cut at a word boundary and ellipsized.
pub fn short_gist(text: &str) -> String {
    first_sentences(&flatten_markdown(text), GIST_MAX_CHARS)
}

/// Strip markdown structure (fences, headings, quotes, bullets, emphasis,
/// links, inline code) and collapse the result onto a single whitespace-
/// normalised line. Fenced code blocks are dropped entirely: a gist is prose.
fn flatten_markdown(text: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut in_fence = false;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || line.is_empty() || is_thematic_break(line) {
            continue;
        }
        let body = strip_line_prefixes(line);
        if body.is_empty() {
            continue;
        }
        parts.push(strip_inline(body));
    }
    parts
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `---`, `***`, `___` rules carry no prose.
fn is_thematic_break(line: &str) -> bool {
    let stripped: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    stripped.len() >= 3
        && (stripped.chars().all(|c| c == '-')
            || stripped.chars().all(|c| c == '*')
            || stripped.chars().all(|c| c == '_'))
}

/// Peel block markers off the front of one line: blockquote `>`, ATX heading
/// `##`, bullet `- ` / `* ` / `+ `, ordered `12. `. Loops because they nest
/// (`> - **item**`). Every branch consumes at least one character, so it
/// terminates.
fn strip_line_prefixes(line: &str) -> &str {
    let mut s = line;
    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix('>') {
            s = rest;
            continue;
        }
        if t.starts_with('#') {
            let rest = t.trim_start_matches('#');
            if rest.is_empty() || rest.starts_with(' ') {
                s = rest;
                continue;
            }
        }
        if let Some(rest) = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .or_else(|| t.strip_prefix("+ "))
        {
            s = rest;
            continue;
        }
        if let Some(rest) = strip_ordered_marker(t) {
            s = rest;
            continue;
        }
        return t;
    }
}

/// `"12. rest"` → `Some("rest")`. Anything else → `None`.
fn strip_ordered_marker(line: &str) -> Option<&str> {
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    line[digits..].strip_prefix(". ")
}

/// Drop inline markup while keeping its text: `` `code` `` → `code`,
/// `**bold**` → `bold`, `[label](url)` → `label`, `![alt](url)` → `alt`.
///
/// `_` is deliberately NOT treated as emphasis — the supervisor's prose is full
/// of identifiers like `dev_local`, and stripping underscores would mangle them
/// into `devlocal`.
fn strip_inline(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            // Image bang: skip it and let the `[` arm keep the alt text.
            '!' if chars.get(i + 1) == Some(&'[') => i += 1,
            '[' => match find_char(&chars, i + 1, ']') {
                Some(close) => {
                    out.extend(&chars[i + 1..close]);
                    i = close + 1;
                    // Consume the `(target)` half of the link, if present.
                    if chars.get(i) == Some(&'(')
                        && let Some(paren) = find_char(&chars, i + 1, ')')
                    {
                        i = paren + 1;
                    }
                }
                None => {
                    out.push('[');
                    i += 1;
                }
            },
            '`' | '*' => i += 1,
            '~' if chars.get(i + 1) == Some(&'~') => i += 2,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    chars[from..]
        .iter()
        .position(|c| *c == needle)
        .map(|p| p + from)
}

/// Keep the first sentence, and the second only if the pair stays within
/// `max_chars`. A first sentence that alone busts the budget is ellipsized.
fn first_sentences(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for sentence in split_sentences(text).into_iter().take(2) {
        if out.is_empty() {
            out.push_str(sentence);
            continue;
        }
        if out.chars().count() + 1 + sentence.chars().count() > max_chars {
            break;
        }
        out.push(' ');
        out.push_str(sentence);
    }
    truncate_chars(&out, max_chars)
}

/// Split on `.`/`!`/`?`/`…` that is followed by whitespace or end-of-text.
/// Requiring the trailing space is what keeps `1.5`, `v1.7.2` and `ecos-ui.rs`
/// from being read as sentence ends; runs like `?!` or `...` are consumed whole.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !TERMINATORS.contains(&c) {
            continue;
        }
        let mut end = i + c.len_utf8();
        while let Some(&(j, next)) = chars.peek() {
            if TERMINATORS.contains(&next) {
                end = j + next.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        let at_boundary = chars.peek().is_none_or(|(_, next)| next.is_whitespace());
        if at_boundary {
            let sentence = text[start..end].trim();
            if !sentence.is_empty() {
                out.push(sentence);
            }
            start = end;
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// Cut to `max` characters at a word boundary, appending `…`. Falls back to a
/// hard character cut when the last space sits in the first half of the budget
/// (a single very long token would otherwise shrink the gist to almost nothing).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut hard_cut = 0usize;
    let mut last_space: Option<usize> = None;
    for (n, (i, c)) in s.char_indices().enumerate() {
        if n >= budget {
            break;
        }
        hard_cut = i + c.len_utf8();
        if c.is_whitespace() {
            last_space = Some(i);
        }
    }
    let end = match last_space {
        Some(space) if s[..space].chars().count() * 2 >= budget => space,
        _ => hard_cut,
    };
    let mut out = s[..end].trim_end().to_string();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_passes_through_unchanged() {
        assert_eq!(
            short_gist("Which API did you mean?"),
            "Which API did you mean?"
        );
    }

    #[test]
    fn strips_markdown_markup_but_keeps_identifiers() {
        assert_eq!(
            short_gist("**(а) Разрешаете** поднять `ecos-integrations` с профилем `dev_local`?"),
            "(а) Разрешаете поднять ecos-integrations с профилем dev_local?"
        );
    }

    #[test]
    fn keeps_link_labels_and_drops_targets() {
        assert_eq!(
            short_gist("See [the plan](https://example.com/a/b) first."),
            "See the plan first."
        );
    }

    #[test]
    fn drops_fenced_code_and_block_markers() {
        // The heading carries no terminator, so it glues onto the first bullet
        // — that is the intended reading, and the fenced command is gone.
        let src =
            "# Заголовок\n\n```sh\nrm -rf /\n```\n\n- Первый пункт.\n- Второй пункт.\n- Третий.";
        assert_eq!(short_gist(src), "Заголовок Первый пункт. Второй пункт.");
    }

    #[test]
    fn takes_at_most_two_sentences() {
        let src = "Раз. Два. Три. Четыре.";
        assert_eq!(short_gist(src), "Раз. Два.");
    }

    #[test]
    fn second_sentence_dropped_when_it_busts_the_budget() {
        let long = "б".repeat(GIST_MAX_CHARS);
        let gist = short_gist(&format!("Короткая первая. {long}."));
        assert_eq!(gist, "Короткая первая.");
    }

    #[test]
    fn over_long_single_sentence_is_ellipsized_at_a_word_boundary() {
        let src = format!("{} конец предложения.", "слово ".repeat(60));
        let gist = short_gist(&src);
        assert!(gist.chars().count() <= GIST_MAX_CHARS, "gist: {gist:?}");
        assert!(gist.ends_with('…'), "gist: {gist:?}");
        assert!(!gist.contains("слов…"), "must not cut mid-word: {gist:?}");
    }

    #[test]
    fn decimal_points_are_not_sentence_ends() {
        assert_eq!(
            short_gist("Версия 1.7.2 собрана. Дальше — тесты."),
            "Версия 1.7.2 собрана. Дальше — тесты."
        );
    }

    #[test]
    fn real_escalation_keeps_only_the_lede() {
        let src = "Этап Б закрыт полностью, кроме Б8 (живая проверка end-to-end). \
                   Числа с диска: медиатор 541 тест, хост 681, фронт 231 сьют / 2961 тест, \
                   `yarn build` зелёный (нужно 10 ГБ heap, при 8 ГБ падает молча).\n\n\
                   **(а) Разрешаете** поднять настоящий `ecos-integrations` локально?";
        let gist = short_gist(src);
        assert_eq!(
            gist,
            "Этап Б закрыт полностью, кроме Б8 (живая проверка end-to-end)."
        );
    }

    #[test]
    fn empty_and_whitespace_only_input_is_empty() {
        assert_eq!(short_gist(""), "");
        assert_eq!(short_gist("   \n\n  "), "");
    }
}
