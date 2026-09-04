use sha2::{Digest, Sha256};

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharKind {
    Lower,
    Upper,
    Digit,
    Other,
}

fn char_kind(ch: char) -> CharKind {
    if ch.is_ascii_uppercase() {
        CharKind::Upper
    } else if ch.is_ascii_lowercase() {
        CharKind::Lower
    } else if ch.is_ascii_digit() {
        CharKind::Digit
    } else {
        CharKind::Other
    }
}

/// A camelCase/PascalCase boundary gets a separator inserted before it, so
/// the generated folder name reads as words rather than one run of letters.
/// Two rules, applied to consecutive alphanumeric characters:
///
/// - lower/digit -> upper (`updateDeps` / `v2Module`): split before the
///   upper char, e.g. `UpdateDeps` -> `update-deps`.
/// - upper -> upper -> lower (an acronym run ending): split before the last
///   upper of the run, e.g. `ECOSRecords` -> `ecos-records`, but a trailing
///   acronym with nothing after it stays whole (`ECOS` -> `ecos`).
///
/// Letter -> digit is deliberately NOT a boundary (`ecosV2` -> `ecos-v2`,
/// never `ecos-v-2`) — digits are treated as ordinary word characters, only
/// digit -> upper (symmetric with lower -> upper) introduces a split.
pub fn slugify(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut last_was_sep = true;
    let mut prev_kind: Option<CharKind> = None;

    for i in 0..chars.len() {
        let ch = chars[i];
        let kind = char_kind(ch);

        if kind == CharKind::Other {
            if !last_was_sep {
                out.push('-');
                last_was_sep = true;
            }
            prev_kind = None;
            continue;
        }

        if !last_was_sep {
            let boundary = match (prev_kind, kind) {
                (Some(CharKind::Lower), CharKind::Upper) => true,
                (Some(CharKind::Digit), CharKind::Upper) => true,
                (Some(CharKind::Upper), CharKind::Upper) => {
                    matches!(chars.get(i + 1).map(|c| char_kind(*c)), Some(CharKind::Lower))
                }
                _ => false,
            };
            if boundary {
                out.push('-');
            }
        }

        out.push(ch.to_ascii_lowercase());
        last_was_sep = false;
        prev_kind = Some(kind);
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        let hash = hasher.finalize();
        let prefix: u32 = hash[..4]
            .iter()
            .fold(0u32, |acc, byte| (acc << 8) | u32::from(*byte));
        out = format!("repo-{prefix:x}");
    }
    out
}

pub fn unique_slug(name: &str, taken: &[String]) -> String {
    let base = slugify(name);
    if !taken.iter().any(|t| t == &base) {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.iter().any(|t| t == &candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugifies_simple_name() {
        assert_eq!(slugify("ECOS Records"), "ecos-records");
    }

    #[test]
    fn collapses_runs_of_separators() {
        assert_eq!(slugify("foo  bar/baz"), "foo-bar-baz");
    }

    #[test]
    fn strips_leading_trailing_separators() {
        assert_eq!(slugify("--foo--"), "foo");
    }

    #[test]
    fn keeps_digits() {
        assert_eq!(slugify("ecos v2 module"), "ecos-v2-module");
    }

    #[test]
    fn splits_camel_case_boundary() {
        assert_eq!(slugify("UpdateDeps"), "update-deps");
    }

    #[test]
    fn splits_acronym_run_before_trailing_word() {
        assert_eq!(slugify("ECOSRecords"), "ecos-records");
    }

    #[test]
    fn keeps_lone_leading_acronym_whole() {
        assert_eq!(slugify("ECOS"), "ecos");
    }

    #[test]
    fn does_not_split_letter_to_digit() {
        assert_eq!(slugify("ecosV2"), "ecos-v2");
    }

    #[test]
    fn splits_digit_to_upper_boundary() {
        assert_eq!(slugify("v2Config"), "v2-config");
    }

    #[test]
    fn does_not_double_separator_after_explicit_hyphen() {
        assert_eq!(slugify("foo-Bar"), "foo-bar");
    }

    #[test]
    fn does_not_double_separator_after_space() {
        assert_eq!(slugify("foo Bar"), "foo-bar");
    }

    #[test]
    fn falls_back_to_hash_for_empty_after_normalisation() {
        let s = slugify("漢字");
        assert!(
            !s.is_empty(),
            "got empty slug for non-ASCII-only input: {s:?}"
        );
        assert!(s.starts_with("repo-"), "expected hash fallback, got: {s:?}");
    }

    #[test]
    fn dedupes_against_existing() {
        let existing: Vec<String> = vec!["foo".into(), "foo-2".into()];
        assert_eq!(unique_slug("Foo", &existing), "foo-3");
    }

    #[test]
    fn dedupe_no_collision_returns_base() {
        let existing: Vec<String> = vec!["bar".into()];
        assert_eq!(unique_slug("Foo", &existing), "foo");
    }
}
