//! Marker-based extraction of conflict chunks from working-tree text.
//!
//! Operates on the `<<<<<<<` / `=======` / `>>>>>>>` markers Git places
//! in working-tree files. Doesn't attempt diff3 reconstruction — for
//! resolver UX, chunk boundaries from markers are sufficient because
//! Accept-Yours / Accept-Theirs replace the whole marker range with one
//! side's content.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictChunk {
    /// Byte range in the original text that the *entire* conflict
    /// occupies — from the `<<<<<<<` line through the trailing newline
    /// after `>>>>>>>`. Replacing this range with the chosen side's
    /// content (no markers) produces the resolved buffer.
    pub range: Range<usize>,
    pub ours: String,
    pub theirs: String,
    /// Optional base section, only present when the file was generated
    /// with `git config merge.conflictStyle diff3` (i.e. `|||||||`
    /// markers exist between `<<<<<<<` and `=======`). `None` for the
    /// default 2-way style.
    pub base: Option<String>,
}

/// Parse `text` for conflict markers. Markers must appear at the start
/// of a line; any leading whitespace disqualifies the line as a marker
/// (matches Git's own rule). Malformed regions (start without end, etc.)
/// are skipped silently — the parser recovers at the next valid start.
pub fn extract_conflict_chunks(text: &str) -> Vec<ConflictChunk> {
    let bytes = text.as_bytes();
    let mut chunks = Vec::new();
    let mut cursor = 0;

    while cursor < bytes.len() {
        let Some(start_offset) = find_line_marker(bytes, cursor, b"<<<<<<<") else {
            break;
        };
        let after_start_marker_line = match end_of_line(bytes, start_offset) {
            Some(end) => end,
            None => break,
        };
        // search for separators
        let sep_marker = find_line_marker(bytes, after_start_marker_line, b"=======");
        let base_marker = find_line_marker(bytes, after_start_marker_line, b"|||||||");
        let Some(separator) = sep_marker else {
            cursor = after_start_marker_line;
            continue;
        };
        let after_separator_line = match end_of_line(bytes, separator) {
            Some(end) => end,
            None => break,
        };
        let Some(end_marker) = find_line_marker(bytes, after_separator_line, b">>>>>>>") else {
            cursor = after_separator_line;
            continue;
        };
        let after_end_marker_line = end_of_line(bytes, end_marker).unwrap_or(bytes.len());

        // diff3-style: ||||||| comes between <<<<<<< and =======
        let (ours_end, base_section) = match base_marker {
            Some(base_off) if base_off > after_start_marker_line && base_off < separator => {
                let after_base_marker_line = match end_of_line(bytes, base_off) {
                    Some(end) => end,
                    None => break,
                };
                let base_text =
                    String::from_utf8_lossy(&bytes[after_base_marker_line..separator]).into_owned();
                (base_off, Some(base_text))
            }
            _ => (separator, None),
        };

        let ours = String::from_utf8_lossy(&bytes[after_start_marker_line..ours_end]).into_owned();
        let theirs = String::from_utf8_lossy(&bytes[after_separator_line..end_marker]).into_owned();

        chunks.push(ConflictChunk {
            range: start_offset..after_end_marker_line,
            ours,
            theirs,
            base: base_section,
        });
        cursor = after_end_marker_line;
    }

    chunks
}

/// Returns the byte offset of the start of a line in `bytes` (at or
/// after `from`) that begins with `marker`, or `None` if no such line
/// exists. The marker must be the first 7 bytes of the line — leading
/// whitespace disqualifies.
fn find_line_marker(bytes: &[u8], from: usize, marker: &[u8]) -> Option<usize> {
    let mut pos = from;
    while pos < bytes.len() {
        let line_end = end_of_line(bytes, pos).unwrap_or(bytes.len());
        let line = &bytes[pos..line_end];
        if line.starts_with(marker) {
            return Some(pos);
        }
        pos = line_end;
    }
    None
}

/// Returns one past the trailing `\n` for the line beginning at `from`,
/// or `None` if there is no terminating newline (caller treats end-of-
/// buffer as the line's end).
fn end_of_line(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            return Some(i + 1);
        }
        i += 1;
    }
    if from < bytes.len() {
        Some(bytes.len())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_single_two_way_chunk() {
        let text = "before\n\
<<<<<<< HEAD\n\
ours line\n\
=======\n\
theirs line\n\
>>>>>>> branch\n\
after\n";
        let chunks = extract_conflict_chunks(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].ours, "ours line\n");
        assert_eq!(chunks[0].theirs, "theirs line\n");
        assert!(chunks[0].base.is_none());
        // Replacing the marker range produces marker-free text
        let mut out = text.to_string();
        out.replace_range(chunks[0].range.clone(), &chunks[0].ours);
        assert_eq!(out, "before\nours line\nafter\n");
    }

    #[test]
    fn extracts_multiple_chunks() {
        let text = "x\n\
<<<<<<<\n\
A\n\
=======\n\
B\n\
>>>>>>>\n\
y\n\
<<<<<<<\n\
C\n\
=======\n\
D\n\
>>>>>>>\n\
z\n";
        let chunks = extract_conflict_chunks(text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].ours, "A\n");
        assert_eq!(chunks[1].theirs, "D\n");
    }

    #[test]
    fn extracts_diff3_base_section() {
        let text = "<<<<<<< HEAD\n\
our\n\
||||||| common\n\
base\n\
=======\n\
their\n\
>>>>>>> incoming\n";
        let chunks = extract_conflict_chunks(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].ours, "our\n");
        assert_eq!(chunks[0].theirs, "their\n");
        assert_eq!(chunks[0].base.as_deref(), Some("base\n"));
    }

    #[test]
    fn no_chunks_in_clean_text() {
        let text = "no conflicts here\nanother line\n";
        assert!(extract_conflict_chunks(text).is_empty());
    }

    #[test]
    fn ignores_marker_with_leading_whitespace() {
        // Indented "<<<<<<<" inside a string literal must NOT be treated as a marker.
        let text = "let s = \"  <<<<<<< nope\";\n";
        assert!(extract_conflict_chunks(text).is_empty());
    }

    #[test]
    fn skips_malformed_unterminated_region() {
        let text = "<<<<<<<\nA\n=======\nB\n"; // no closing >>>>>>>
        let chunks = extract_conflict_chunks(text);
        assert!(chunks.is_empty());
    }
}
