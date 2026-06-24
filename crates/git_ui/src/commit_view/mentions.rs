//! Lightweight tokeniser for commit-message subject + body. Splits text
//! into plain runs, URLs, GitHub-style `#1234` refs, and Jira-style
//! `[ABC-123]` refs. Per the S-DET plan the patterns are hardcoded for
//! v1; the configurable variant is deferred.

/// One slice of a commit message after mention parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MessageToken {
    /// Plain text run with no mention markup. Newlines are preserved
    /// inline; the renderer wraps via `flex_wrap`.
    Text(String),
    /// Absolute URL (`http://` or `https://`).
    Url(String),
    /// GitHub-style issue ref. Stored without the leading `#`.
    IssueRef(String),
    /// Jira-style ref. Stored without the surrounding brackets,
    /// preserving the project-key + numeric tail (e.g. `ABC-123`).
    JiraRef(String),
}

/// Tokenise `text` into a sequence of [`MessageToken`]s. The output
/// preserves the relative ordering of plain text and mentions so the
/// renderer can reconstruct the original message verbatim.
pub(crate) fn parse_message_tokens(text: &str, parse_issue_refs: bool) -> Vec<MessageToken> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<MessageToken> = Vec::new();
    let mut buffer = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < text.len() {
        // URL: scan ahead for `http://` or `https://`.
        if let Some((url, consumed)) = match_url_at(text, i) {
            if !buffer.is_empty() {
                out.push(MessageToken::Text(std::mem::take(&mut buffer)));
            }
            out.push(MessageToken::Url(url));
            i += consumed;
            continue;
        }

        if parse_issue_refs {
            // GitHub-style `#1234`. Boundary: must be at the start of input
            // or follow a non-alphanumeric character (avoid eating mid-word
            // hash signs from URLs/anchors which are caught by URL match
            // earlier — but keep the check defensive).
            if bytes[i] == b'#' && i + 1 < text.len() && is_word_boundary_before(text, i) {
                if let Some((num, consumed)) = match_issue_ref_at(text, i) {
                    if !buffer.is_empty() {
                        out.push(MessageToken::Text(std::mem::take(&mut buffer)));
                    }
                    out.push(MessageToken::IssueRef(num));
                    i += consumed;
                    continue;
                }
            }

            // Jira-style `[ABC-123]`.
            if bytes[i] == b'[' {
                if let Some((key, consumed)) = match_jira_ref_at(text, i) {
                    if !buffer.is_empty() {
                        out.push(MessageToken::Text(std::mem::take(&mut buffer)));
                    }
                    out.push(MessageToken::JiraRef(key));
                    i += consumed;
                    continue;
                }
            }
        }

        // No mention starts here — accumulate one byte (the input is
        // ASCII for the boundary checks above; we still index by byte to
        // preserve UTF-8 contents).
        let next_char_len = next_char_len(text, i);
        buffer.push_str(&text[i..i + next_char_len]);
        i += next_char_len;
    }

    if !buffer.is_empty() {
        out.push(MessageToken::Text(buffer));
    }
    out
}

fn next_char_len(text: &str, i: usize) -> usize {
    text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1)
}

fn is_word_boundary_before(text: &str, i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = text[..i]
        .chars()
        .next_back()
        .map(|c| !c.is_alphanumeric())
        .unwrap_or(true);
    prev
}

fn match_url_at(text: &str, i: usize) -> Option<(String, usize)> {
    const SCHEMES: &[&str] = &["https://", "http://"];
    for scheme in SCHEMES {
        if text[i..].starts_with(scheme) {
            // URL is greedy until whitespace or one of the structural
            // delimiters that almost certainly isn't part of the URL.
            let rest = &text[i..];
            let end = rest
                .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '\u{0}'))
                .unwrap_or(rest.len());
            // Don't include trailing punctuation that almost always
            // belongs to the surrounding sentence.
            let mut end = end;
            while end > scheme.len() {
                let last = rest[..end].chars().next_back().unwrap_or(' ');
                if matches!(last, '.' | ',' | ';' | ':' | ')' | ']' | '!' | '?') {
                    end -= last.len_utf8();
                } else {
                    break;
                }
            }
            if end > scheme.len() {
                return Some((rest[..end].to_string(), end));
            }
        }
    }
    None
}

fn match_issue_ref_at(text: &str, i: usize) -> Option<(String, usize)> {
    debug_assert_eq!(text.as_bytes()[i], b'#');
    let rest = &text.as_bytes()[i + 1..];
    let digits = rest.iter().take_while(|&&b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    // Boundary after digits: end-of-string or non-word char.
    let after = i + 1 + digits;
    if after < text.len() {
        let next = text[after..].chars().next().unwrap_or(' ');
        if next.is_alphanumeric() || next == '_' {
            return None;
        }
    }
    let num = std::str::from_utf8(&rest[..digits]).ok()?.to_string();
    Some((num, 1 + digits))
}

fn match_jira_ref_at(text: &str, i: usize) -> Option<(String, usize)> {
    debug_assert_eq!(text.as_bytes()[i], b'[');
    let rest = &text[i + 1..];
    let close = rest.find(']')?;
    let inner = &rest[..close];

    // Validate `^[A-Z][A-Z0-9_]*-\d+$` — Jira's canonical issue-key shape.
    let mut chars = inner.chars();
    let first = chars.next()?;
    if !first.is_ascii_uppercase() {
        return None;
    }
    let mut saw_dash = false;
    let mut saw_digit_after_dash = false;
    for c in chars {
        if !saw_dash {
            if c == '-' {
                saw_dash = true;
            } else if !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
                return None;
            }
        } else if c.is_ascii_digit() {
            saw_digit_after_dash = true;
        } else {
            return None;
        }
    }
    if !saw_dash || !saw_digit_after_dash {
        return None;
    }
    Some((inner.to_string(), 1 + close + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_text() {
        let toks = parse_message_tokens("Hello world", true);
        assert_eq!(toks, vec![MessageToken::Text("Hello world".into())]);
    }

    #[test]
    fn parses_url_in_subject() {
        let toks = parse_message_tokens("see https://example.com/foo for details", true);
        assert_eq!(
            toks,
            vec![
                MessageToken::Text("see ".into()),
                MessageToken::Url("https://example.com/foo".into()),
                MessageToken::Text(" for details".into()),
            ]
        );
    }

    #[test]
    fn url_drops_trailing_punctuation() {
        let toks = parse_message_tokens("at https://example.com/page.", true);
        assert_eq!(
            toks,
            vec![
                MessageToken::Text("at ".into()),
                MessageToken::Url("https://example.com/page".into()),
                MessageToken::Text(".".into()),
            ]
        );
    }

    #[test]
    fn parses_issue_ref() {
        let toks = parse_message_tokens("Fixes #1234 (was #abc).", true);
        assert_eq!(
            toks,
            vec![
                MessageToken::Text("Fixes ".into()),
                MessageToken::IssueRef("1234".into()),
                MessageToken::Text(" (was #abc).".into()),
            ]
        );
    }

    #[test]
    fn issue_ref_disabled_by_setting() {
        let toks = parse_message_tokens("Fixes #1234.", false);
        assert_eq!(toks, vec![MessageToken::Text("Fixes #1234.".into())]);
    }

    #[test]
    fn parses_jira_ref() {
        let toks = parse_message_tokens("[ABC-123] Fix the thing", true);
        assert_eq!(
            toks,
            vec![
                MessageToken::JiraRef("ABC-123".into()),
                MessageToken::Text(" Fix the thing".into()),
            ]
        );
    }

    #[test]
    fn rejects_non_canonical_jira_shape() {
        let toks = parse_message_tokens("[abc-123] foo", true);
        assert_eq!(toks, vec![MessageToken::Text("[abc-123] foo".into())]);
        let toks = parse_message_tokens("[ABC-x] foo", true);
        assert_eq!(toks, vec![MessageToken::Text("[ABC-x] foo".into())]);
    }

    #[test]
    fn issue_ref_requires_word_boundary() {
        let toks = parse_message_tokens("foo#1234", true);
        // No leading boundary — leave verbatim.
        assert_eq!(toks, vec![MessageToken::Text("foo#1234".into())]);
    }
}
