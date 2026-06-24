use crate::Oid;
use anyhow::{Context, Result, anyhow};
use std::{str::FromStr, sync::Arc};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StashEntry {
    pub index: usize,
    pub oid: Oid,
    pub message: String,
    pub branch: Option<String>,
    pub timestamp: i64,
}

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct GitStash {
    pub entries: Arc<[StashEntry]>,
}

/// Lightweight stash badge data parsed from
/// `git stash show --stat --include-untracked <stash>`. Surfaces just what
/// the S-STH list rows need: how many files this stash touches, and
/// whether any of them are tracked-untracked extras.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct StashStat {
    pub file_count: usize,
    pub has_untracked: bool,
}

impl StashStat {
    /// Parse the trailing summary line of `git stash show --stat`. The
    /// format is `N files changed[, A insertions(+)][, D deletions(-)]`
    /// (or "1 file changed" for a singleton). Untracked entries appear as
    /// "create mode 100644 <path>" lines mixed in with the diffstat — we
    /// detect them via the `(untracked)` suffix git stash adds when it
    /// included untracked files.
    pub fn from_stat_output(stat: &str) -> Self {
        let mut file_count: usize = 0;
        let mut has_untracked = false;
        for line in stat.lines() {
            if line.contains("(untracked)") {
                has_untracked = true;
            }
            if let Some(num) = parse_summary_line(line) {
                file_count = num;
            }
        }
        if file_count == 0 {
            for line in stat.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.contains('|') {
                    file_count += 1;
                }
            }
        }
        Self {
            file_count,
            has_untracked,
        }
    }
}

fn parse_summary_line(line: &str) -> Option<usize> {
    let trimmed = line.trim();
    let mut iter = trimmed.splitn(2, ' ');
    let num: usize = iter.next()?.parse().ok()?;
    let rest = iter.next()?;
    if rest.starts_with("file changed") || rest.starts_with("files changed") {
        Some(num)
    } else {
        None
    }
}

impl GitStash {
    pub fn apply(&mut self, other: GitStash) {
        self.entries = other.entries;
    }
}

impl FromStr for GitStash {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.trim().is_empty() {
            return Ok(Self::default());
        }

        let mut entries = Vec::new();
        let mut errors = Vec::new();

        for (line_num, line) in s.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }

            match parse_stash_line(line) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    errors.push(format!("Line {}: {}", line_num + 1, e));
                }
            }
        }

        // If we have some valid entries but also some errors, log the errors but continue
        if !errors.is_empty() && !entries.is_empty() {
            log::warn!("Failed to parse some stash entries: {}", errors.join(", "));
        } else if !errors.is_empty() {
            return Err(anyhow!(
                "Failed to parse stash entries: {}",
                errors.join(", ")
            ));
        }

        Ok(Self {
            entries: entries.into(),
        })
    }
}

/// Parse a single stash line in the format: "stash@{N}\0<oid>\0<timestamp>\0<message>"
fn parse_stash_line(line: &str) -> Result<StashEntry> {
    let parts: Vec<&str> = line.splitn(4, '\0').collect();

    if parts.len() != 4 {
        return Err(anyhow!(
            "Expected 4 null-separated parts, got {}",
            parts.len()
        ));
    }

    let index = parse_stash_index(parts[0])
        .with_context(|| format!("Failed to parse stash index from '{}'", parts[0]))?;

    let oid = Oid::from_str(parts[1])
        .with_context(|| format!("Failed to parse OID from '{}'", parts[1]))?;

    let timestamp = parts[2]
        .parse::<i64>()
        .with_context(|| format!("Failed to parse timestamp from '{}'", parts[2]))?;

    let (branch, message) = parse_stash_message(parts[3]);

    Ok(StashEntry {
        index,
        oid,
        message: message.to_string(),
        branch: branch.map(Into::into),
        timestamp,
    })
}

/// Parse stash index from format "stash@{N}" where N is the index
fn parse_stash_index(input: &str) -> Result<usize> {
    let trimmed = input.trim();

    if !trimmed.starts_with("stash@{") || !trimmed.ends_with('}') {
        return Err(anyhow!(
            "Invalid stash index format: expected 'stash@{{N}}'"
        ));
    }

    let index_str = trimmed
        .strip_prefix("stash@{")
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| anyhow!("Failed to extract index from stash reference"))?;

    index_str
        .parse::<usize>()
        .with_context(|| format!("Invalid stash index number: '{}'", index_str))
}

/// Parse stash message and extract branch information if present
///
/// Handles the following formats:
/// - "WIP on <branch>: <message>" -> (Some(branch), message)
/// - "On <branch>: <message>" -> (Some(branch), message)
/// - "<message>" -> (None, message)
fn parse_stash_message(input: &str) -> (Option<&str>, &str) {
    // Handle "WIP on <branch>: <message>" pattern
    if let Some(stripped) = input.strip_prefix("WIP on ")
        && let Some(colon_pos) = stripped.find(": ")
    {
        let branch = &stripped[..colon_pos];
        let message = &stripped[colon_pos + 2..];
        if !branch.is_empty() && !message.is_empty() {
            return (Some(branch), message);
        }
    }

    // Handle "On <branch>: <message>" pattern
    if let Some(stripped) = input.strip_prefix("On ")
        && let Some(colon_pos) = stripped.find(": ")
    {
        let branch = &stripped[..colon_pos];
        let message = &stripped[colon_pos + 2..];
        if !branch.is_empty() && !message.is_empty() {
            return (Some(branch), message);
        }
    }

    // Fallback: treat entire input as message with no branch
    (None, input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stash_index() {
        assert_eq!(parse_stash_index("stash@{0}").unwrap(), 0);
        assert_eq!(parse_stash_index("stash@{42}").unwrap(), 42);
        assert_eq!(parse_stash_index("  stash@{5}  ").unwrap(), 5);

        assert!(parse_stash_index("invalid").is_err());
        assert!(parse_stash_index("stash@{not_a_number}").is_err());
        assert!(parse_stash_index("stash@{0").is_err());
    }

    #[test]
    fn test_parse_stash_message() {
        // WIP format
        let (branch, message) = parse_stash_message("WIP on main: working on feature");
        assert_eq!(branch, Some("main"));
        assert_eq!(message, "working on feature");

        // On format
        let (branch, message) = parse_stash_message("On feature-branch: some changes");
        assert_eq!(branch, Some("feature-branch"));
        assert_eq!(message, "some changes");

        // No branch format
        let (branch, message) = parse_stash_message("just a regular message");
        assert_eq!(branch, None);
        assert_eq!(message, "just a regular message");

        // Edge cases
        let (branch, message) = parse_stash_message("WIP on : empty message");
        assert_eq!(branch, None);
        assert_eq!(message, "WIP on : empty message");

        let (branch, message) = parse_stash_message("On branch-name:");
        assert_eq!(branch, None);
        assert_eq!(message, "On branch-name:");
    }

    #[test]
    fn test_parse_stash_line() {
        let line = "stash@{0}\u{0000}abc123\u{0000}1234567890\u{0000}WIP on main: test commit";
        let entry = parse_stash_line(line).unwrap();

        assert_eq!(entry.index, 0);
        assert_eq!(entry.message, "test commit");
        assert_eq!(entry.branch, Some("main".to_string()));
        assert_eq!(entry.timestamp, 1234567890);
    }

    #[test]
    fn test_git_stash_from_str() {
        let input = "stash@{0}\u{0000}abc123\u{0000}1234567890\u{0000}WIP on main: first stash\nstash@{1}\u{0000}def456\u{0000}1234567891\u{0000}On feature: second stash";
        let stash = GitStash::from_str(input).unwrap();

        assert_eq!(stash.entries.len(), 2);
        assert_eq!(stash.entries[0].index, 0);
        assert_eq!(stash.entries[0].branch, Some("main".to_string()));
        assert_eq!(stash.entries[1].index, 1);
        assert_eq!(stash.entries[1].branch, Some("feature".to_string()));
    }

    #[test]
    fn test_git_stash_empty_input() {
        let stash = GitStash::from_str("").unwrap();
        assert_eq!(stash.entries.len(), 0);

        let stash = GitStash::from_str("   \n  \n  ").unwrap();
        assert_eq!(stash.entries.len(), 0);
    }

    #[test]
    fn test_stash_stat_summary_line() {
        let raw =
            " a.txt | 2 +-\n b.txt | 4 ++--\n 2 files changed, 3 insertions(+), 3 deletions(-)\n";
        let stat = StashStat::from_stat_output(raw);
        assert_eq!(stat.file_count, 2);
        assert!(!stat.has_untracked);
    }

    #[test]
    fn test_stash_stat_singleton() {
        let raw = " a.txt | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)\n";
        let stat = StashStat::from_stat_output(raw);
        assert_eq!(stat.file_count, 1);
    }

    #[test]
    fn test_stash_stat_falls_back_to_pipe_count() {
        let raw = " a.txt | 2 +-\n b.txt | 4 ++--\n";
        let stat = StashStat::from_stat_output(raw);
        assert_eq!(stat.file_count, 2);
    }

    #[test]
    fn test_stash_stat_detects_untracked() {
        let raw = " (untracked)\n new.txt | 1 +\n 1 file changed, 1 insertion(+)\n";
        let stat = StashStat::from_stat_output(raw);
        assert_eq!(stat.file_count, 1);
        assert!(stat.has_untracked);
    }

    #[test]
    fn test_stash_stat_empty() {
        let stat = StashStat::from_stat_output("");
        assert_eq!(stat.file_count, 0);
        assert!(!stat.has_untracked);
    }
}
