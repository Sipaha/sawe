//! Derivation of an on-disk folder name from a user-visible display name.
//!
//! Unicode-preserving sanitization, **not** transliteration: `Мой Проект`
//! becomes `Мой-Проект`, not `moy-proekt`. Nothing here touches the
//! filesystem or the database — collision checks live in `crate::rename`.
//!
//! This is the *only* derivation in the fork: creating a Solution, adding a
//! member and renaming either of them all come through [`derive`], so the
//! same display name always yields the same directory regardless of which
//! path produced it. (It used to be two rules — an ASCII-lowercasing
//! `slug::slugify` for creation and this one for rename — so `UpdateDeps`
//! became `update-deps` when created and `Update-Deps` when renamed.)

use std::fmt;
use unicode_normalization::UnicodeNormalization as _;

/// ext4 / APFS cap a single path component at 255 **bytes**, not characters
/// (a Cyrillic character is 2 bytes in UTF-8, a CJK one is 3).
pub const MAX_FOLDER_NAME_BYTES: usize = 255;

/// Everything that can stop a rename. The three collision variants are
/// produced by `crate::rename::ensure_folder_available`; they live here so a
/// caller has a single error type to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderNameError {
    Empty { name: String },
    Reserved { folder: String },
    TakenBySolution { folder: String, owner: String },
    ExistsOnDisk { folder: String },
    HeldByLink { folder: String },
}

impl fmt::Display for FolderNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { name } => write!(
                f,
                "Cannot derive a folder name from '{name}' — use at least one ordinary character"
            ),
            Self::Reserved { folder } => write!(
                f,
                "'{folder}' is a reserved device name on Windows — choose another name"
            ),
            Self::TakenBySolution { folder, owner } => write!(
                f,
                "Directory '{folder}' is already taken by solution '{owner}'"
            ),
            Self::ExistsOnDisk { folder } => write!(
                f,
                "Directory '{folder}' already exists on disk (not owned by any solution)"
            ),
            Self::HeldByLink { folder } => write!(
                f,
                "Directory '{folder}' is held by a link from an unfinished rename — restart the editor"
            ),
        }
    }
}

impl std::error::Error for FolderNameError {}

/// Characters that are illegal or non-portable in a path component.
/// `/` and NUL are illegal on POSIX; the rest are the Windows set.
const ILLEGAL: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// A folder name may not start or end with any of these: a leading/trailing dot
/// hides the directory (or is stripped by Windows), and a leading/trailing dash
/// is only ever an artifact of sanitizing an edge character away.
const TRIMMED_EDGE_CHARS: [char; 3] = ['.', ' ', '-'];

const RESERVED_WINDOWS_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn derive(name: &str) -> Result<String, FolderNameError> {
    // NFC first: otherwise `й` can exist as two different byte sequences and
    // two "identical" folder names differ on disk.
    let normalized: String = name.nfc().collect();

    let mut out = String::with_capacity(normalized.len());
    let mut pending_separator = false;
    for ch in normalized.chars() {
        if ch.is_whitespace() {
            pending_separator = true;
            continue;
        }
        if ch == '\u{0}' || ch.is_control() || ILLEGAL.contains(&ch) {
            continue;
        }
        if pending_separator && !out.is_empty() {
            out.push('-');
        }
        pending_separator = false;
        out.push(ch);
    }

    let split = split_camel_case(&out);

    // `-` is in the trim set because a dropped edge character can leave the
    // separator it introduced behind: `" . mixed . "` builds `.-mixed-.`, and
    // trimming only dots would yield `-mixed-`.
    let trimmed = split.trim_matches(TRIMMED_EDGE_CHARS);
    // Truncating can expose a trailing dot that was legal mid-name, so trim
    // again after the cut.
    let folder = truncate_to_bytes(trimmed, MAX_FOLDER_NAME_BYTES)
        .trim_end_matches(TRIMMED_EDGE_CHARS)
        .to_string();

    if folder.is_empty() {
        return Err(FolderNameError::Empty {
            name: name.to_string(),
        });
    }
    if is_reserved(&folder) {
        return Err(FolderNameError::Reserved { folder });
    }
    Ok(folder)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharKind {
    Lower,
    Upper,
    Digit,
    Other,
}

/// Case is Unicode-wide, not ASCII-wide, so `МойПроект` splits the same way
/// `MyProject` does. A script without case (CJK, Arabic) is `Other`, which
/// never forms a boundary.
fn char_kind(ch: char) -> CharKind {
    if ch.is_uppercase() {
        CharKind::Upper
    } else if ch.is_lowercase() {
        CharKind::Lower
    } else if ch.is_ascii_digit() {
        CharKind::Digit
    } else {
        CharKind::Other
    }
}

/// Insert a `-` at each camelCase/PascalCase boundary, so the folder reads as
/// words rather than one run of letters. Two rules:
///
/// - lower/digit -> upper (`UpdateDeps`, `v2Config`): split before the upper
///   char, giving `Update-Deps` / `v2-Config`.
/// - upper -> upper -> lower (an acronym run ending): split before the last
///   upper of the run, so `ECOSRecords` -> `ECOS-Records`, while a trailing
///   acronym with nothing after it stays whole (`ECOS` -> `ECOS`).
///
/// Letter -> digit is deliberately NOT a boundary (`ecosV2` -> `ecos-V2`,
/// never `ecos-V-2`) — digits are ordinary word characters; only digit ->
/// upper introduces a split, symmetric with lower -> upper.
///
/// Runs after sanitization, so an already-present separator (`foo-Bar`, `foo
/// Bar`) sits between the two words as an `Other` char and no second one is
/// added.
fn split_camel_case(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    for (index, ch) in chars.iter().copied().enumerate() {
        let previous = index.checked_sub(1).map(|prev| char_kind(chars[prev]));
        let boundary = match (previous, char_kind(ch)) {
            (Some(CharKind::Lower | CharKind::Digit), CharKind::Upper) => true,
            (Some(CharKind::Upper), CharKind::Upper) => matches!(
                chars.get(index + 1).map(|next| char_kind(*next)),
                Some(CharKind::Lower)
            ),
            _ => false,
        };
        if boundary {
            out.push('-');
        }
        out.push(ch);
    }
    out
}

/// How far the `-2`, `-3`, … ladder climbs before giving up. Only a caller
/// that has somehow accumulated a thousand same-named directories can reach
/// it; the bound exists so a predicate that always answers "taken" cannot
/// spin forever.
const MAX_UNIQUIFY_ATTEMPTS: u32 = 1_000;

/// The `-2`, `-3`, … ladder shared by every "mint a fresh folder" caller.
/// `available` decides what free means — a disk + DB check for a Solution
/// root, an in-memory member list for a clone target — so there is one
/// suffix mechanism with several availability predicates rather than several
/// dedupe rules. `base` must already have come out of [`derive`].
///
/// Returns `None` only when the ladder is exhausted.
pub fn uniquify(base: &str, mut available: impl FnMut(&str) -> bool) -> Option<String> {
    if available(base) {
        return Some(base.to_string());
    }
    (2..=MAX_UNIQUIFY_ATTEMPTS)
        .map(|attempt| with_suffix(base, attempt))
        .find(|candidate| available(candidate))
}

/// The suffix has to fit *inside* the byte cap, so the base is shortened to
/// make room rather than the whole candidate being truncated afterwards —
/// truncating `…-2` back to `…` would defeat the uniquification.
fn with_suffix(base: &str, attempt: u32) -> String {
    let suffix = format!("-{attempt}");
    let room = MAX_FOLDER_NAME_BYTES.saturating_sub(suffix.len());
    let base = truncate_to_bytes(base, room).trim_end_matches(TRIMMED_EDGE_CHARS);
    format!("{base}{suffix}")
}

fn truncate_to_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Windows reserves these names with *any* extension (`NUL.txt` is still the
/// null device), so the check is on the stem.
fn is_reserved(folder: &str) -> bool {
    let stem = folder.split('.').next().unwrap_or(folder).to_uppercase();
    RESERVED_WINDOWS_NAMES.contains(&stem.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_folder_names() {
        let cases: &[(&str, &str)] = &[
            ("Citeck Forge", "Citeck-Forge"),
            ("Sawe", "Sawe"),
            ("sawe", "sawe"),
            ("Мой Проект", "Мой-Проект"),
            ("项目一", "项目一"),
            ("مشروع جديد", "مشروع-جديد"),
            ("rocket 🚀 ship", "rocket-🚀-ship"),
            ("  padded  name  ", "padded-name"),
            ("a\t\n b", "a-b"),
            ("a/b:c*d?e\"f<g>h|i\\j", "abcdefghij"),
            ("...dots...", "dots"),
            (" . mixed . ", "mixed"),
            ("Sawe (fork)", "Sawe-(fork)"),
        ];
        for (input, expected) in cases {
            assert_eq!(derive(input).as_deref(), Ok(*expected), "derive({input:?})");
        }
    }

    /// The unified rule, pinned. Every one of these used to have *two*
    /// answers: creation went through `slug::slugify` (ASCII-only,
    /// lowercased, `repo-{hash}` when nothing survived) and rename through
    /// `derive`. The right-hand column is now what both produce.
    #[test]
    fn unified_rule_table() {
        let cases: &[(&str, &str)] = &[
            // camelCase / PascalCase gains a separator (the request that
            // started this: `UpdateDeps` used to create `updatedeps`).
            ("UpdateDeps", "Update-Deps"),
            ("updateDeps", "update-Deps"),
            // An acronym run splits before the word that ends it, and stays
            // whole when nothing follows.
            ("ECOSRecords", "ECOS-Records"),
            ("ECOS", "ECOS"),
            // Letter -> digit is never a boundary; digit -> upper is.
            ("ecosV2", "ecos-V2"),
            ("v2Config", "v2-Config"),
            // Cyrillic survives as Cyrillic. The old creation path had no
            // ASCII left to keep and produced a `repo-{hash}` directory.
            ("Мой Проект", "Мой-Проект"),
            ("МойПроект", "Мой-Проект"),
            // An already-hyphenated name is left alone — no doubled separator
            // at the hump, no lowercasing.
            ("update-deps", "update-deps"),
            ("Update-Deps", "Update-Deps"),
            ("foo-Bar", "foo-Bar"),
            // Case is preserved verbatim, so two names that used to collapse
            // onto one folder no longer do.
            ("Sawe", "Sawe"),
            ("sawe", "sawe"),
        ];
        for (input, expected) in cases {
            assert_eq!(derive(input).as_deref(), Ok(*expected), "derive({input:?})");
        }

        // A name made only of illegal characters has nothing to derive from.
        assert_eq!(
            derive("/\\:*?\"<>|"),
            Err(FolderNameError::Empty {
                name: "/\\:*?\"<>|".to_string()
            })
        );
        // A Windows device name is refused rather than silently mangled.
        assert!(matches!(
            derive("NUL"),
            Err(FolderNameError::Reserved { .. })
        ));
        // At the cap: untouched. Over it: cut to the cap.
        let at_cap = "a".repeat(MAX_FOLDER_NAME_BYTES);
        assert_eq!(derive(&at_cap).as_deref(), Ok(at_cap.as_str()));
        let over_cap = "a".repeat(MAX_FOLDER_NAME_BYTES + 1);
        assert_eq!(derive(&over_cap).as_deref(), Ok(at_cap.as_str()));
    }

    #[test]
    fn uniquify_walks_the_suffix_ladder() {
        assert_eq!(uniquify("Sawe", |_| true).as_deref(), Some("Sawe"));
        let taken = ["Sawe".to_string(), "Sawe-2".to_string()];
        assert_eq!(
            uniquify("Sawe", |candidate| !taken.iter().any(|t| t == candidate)).as_deref(),
            Some("Sawe-3")
        );
        assert_eq!(uniquify("Sawe", |_| false), None);
    }

    #[test]
    fn uniquify_keeps_the_suffixed_name_under_the_byte_cap() {
        let base = "a".repeat(MAX_FOLDER_NAME_BYTES);
        let taken = base.clone();
        let candidate = uniquify(&base, |candidate| candidate != taken).expect("uniquifies");
        assert!(
            candidate.len() <= MAX_FOLDER_NAME_BYTES,
            "{} bytes",
            candidate.len()
        );
        assert!(candidate.ends_with("-2"), "{candidate}");
    }

    #[test]
    fn normalizes_to_nfc() {
        // "й" as U+0438 + U+0306 (decomposed) must derive to the composed form.
        let decomposed = "\u{0438}\u{0306}";
        let composed = "\u{0439}";
        assert_eq!(derive(decomposed).as_deref(), Ok(composed));
        assert_eq!(derive(composed).as_deref(), Ok(composed));
    }

    #[test]
    fn rejects_empty_derivations() {
        for input in ["", "   ", "...", " . . ", "/", "\u{0}", "\u{7}"] {
            assert_eq!(
                derive(input),
                Err(FolderNameError::Empty {
                    name: input.to_string()
                }),
                "derive({input:?})"
            );
        }
    }

    #[test]
    fn rejects_reserved_windows_names() {
        for input in [
            "CON", "con", "PRN", "AUX", "NUL", "COM1", "com9", "LPT1", "LPT9", "nul.txt",
        ] {
            let derived = derive(input);
            assert!(
                matches!(derived, Err(FolderNameError::Reserved { .. })),
                "derive({input:?}) = {derived:?}"
            );
        }
        // COM0 / LPT0 are NOT reserved.
        assert_eq!(derive("COM0").as_deref(), Ok("COM0"));
        assert_eq!(derive("LPT0").as_deref(), Ok("LPT0"));
    }

    #[test]
    fn truncates_to_255_bytes_on_a_char_boundary() {
        // 128 Cyrillic chars = 256 bytes; the 128th char must be dropped whole.
        let input = "я".repeat(128);
        let derived = derive(&input).expect("derives");
        assert_eq!(derived.len(), 254);
        assert_eq!(derived.chars().count(), 127);

        // Exactly 255 ASCII bytes survives untouched.
        let ascii = "a".repeat(255);
        assert_eq!(derive(&ascii).as_deref(), Ok(ascii.as_str()));

        // 256 ASCII bytes truncates to 255.
        let long = "a".repeat(256);
        assert_eq!(derive(&long).expect("derives").len(), MAX_FOLDER_NAME_BYTES);
    }

    #[test]
    fn truncation_never_leaves_a_trailing_dot() {
        let input = format!("{}.x", "a".repeat(254));
        let derived = derive(&input).expect("derives");
        assert!(!derived.ends_with('.'), "{derived:?}");
        assert_eq!(derived, "a".repeat(254));
    }

    #[test]
    fn never_changes_case() {
        assert_eq!(derive("MIXED case").as_deref(), Ok("MIXED-case"));
        // Every character keeps its case here too — the extra separators are
        // the camelCase rule doing its job: as far as it can tell `MiXeD` is
        // three humps. (This assertion read `MiXeD-CaSe` before the rule was
        // unified onto this module.)
        assert_eq!(derive("MiXeD CaSe").as_deref(), Ok("Mi-Xe-D-Ca-Se"));
    }

    #[test]
    fn error_messages_match_the_spec() {
        assert_eq!(
            FolderNameError::Empty { name: "...".into() }.to_string(),
            "Cannot derive a folder name from '...' — use at least one ordinary character"
        );
        assert_eq!(
            FolderNameError::TakenBySolution {
                folder: "citeck-forge".into(),
                owner: "Citeck Forge".into()
            }
            .to_string(),
            "Directory 'citeck-forge' is already taken by solution 'Citeck Forge'"
        );
        assert_eq!(
            FolderNameError::ExistsOnDisk {
                folder: "citeck-forge".into()
            }
            .to_string(),
            "Directory 'citeck-forge' already exists on disk (not owned by any solution)"
        );
        assert_eq!(
            FolderNameError::HeldByLink {
                folder: "citeck-forge".into()
            }
            .to_string(),
            "Directory 'citeck-forge' is held by a link from an unfinished rename — restart the editor"
        );
        assert_eq!(
            FolderNameError::Reserved {
                folder: "CON".into()
            }
            .to_string(),
            "'CON' is a reserved device name on Windows — choose another name"
        );
    }
}
