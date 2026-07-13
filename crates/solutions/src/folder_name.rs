//! Derivation of an on-disk folder name from a user-visible display name.
//!
//! Unicode-preserving sanitization, **not** transliteration: `Мой Проект`
//! becomes `Мой-Проект`, not `moy-proekt`. Nothing here touches the
//! filesystem or the database — collision checks live in `crate::rename`.

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

    // `-` is in the trim set because a dropped edge character can leave the
    // separator it introduced behind: `" . mixed . "` builds `.-mixed-.`, and
    // trimming only dots would yield `-mixed-`.
    let trimmed = out.trim_matches(TRIMMED_EDGE_CHARS);
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
        assert_eq!(derive("MiXeD CaSe").as_deref(), Ok("MiXeD-CaSe"));
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
