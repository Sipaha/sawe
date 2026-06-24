//! Soft credential-leak detector: scans the staged-files diff for a small
//! handful of high-confidence patterns (AWS keys, PEM-armored private keys,
//! GitHub PATs) and shows a non-blocking toast when one matches. The toast
//! is a *warning*, not a block — users can disable it via the
//! `WARN_ON_CREDENTIALS` constant if false-positives become a problem.
//!
//! `WARN_ON_CREDENTIALS` is a fork-local module-level switch rather than a
//! flowed-through `settings.json` key because the `settings` crate is in
//! the upstream-shaped zone (see FORK.md / .rules) and adding a single
//! boolean would touch multiple settings-pipeline files. Promoting this to
//! a real setting is a follow-up task.

use std::sync::OnceLock;

use regex::Regex;

/// Master toggle for the credential warning. Default `true`.
pub const WARN_ON_CREDENTIALS: bool = true;

/// One match against the staged diff.
#[derive(Debug, Clone)]
pub struct CredentialMatch {
    /// File path as reported in the diff header (after `b/`, repo-relative).
    pub path: String,
    /// 1-indexed line number within `path` where the match occurred. `None`
    /// when the match was found in a hunk header (still want to warn).
    pub line: Option<u32>,
    /// Short label of the matched pattern: `"aws_key"`, `"private_key"`,
    /// `"github_pat"`, `"github_fine_grained"`.
    pub kind: &'static str,
}

/// Scan a unified diff for credential patterns. Only matches added lines
/// (`+`-prefixed within hunks) so existing committed credentials don't keep
/// re-warning forever.
pub fn scan_diff(diff: &str) -> Vec<CredentialMatch> {
    if !WARN_ON_CREDENTIALS {
        return Vec::new();
    }
    let patterns = compiled_patterns();
    let mut matches = Vec::new();
    let mut current_path: Option<String> = None;
    let mut hunk_new_line: u32 = 0;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            current_path = Some(rest.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            current_path = Some(rest.to_string());
            continue;
        }
        if line.starts_with("@@") {
            // Parse `@@ -a,b +c,d @@` — pick `c` as the starting new-file line.
            if let Some(start) = parse_hunk_new_start(line) {
                hunk_new_line = start;
            } else {
                hunk_new_line = 0;
            }
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if let Some(payload) = line.strip_prefix('+') {
            // Skip the diff header `+++ a/...` already handled above; what
            // remains here is an added content line.
            for entry in patterns.iter() {
                let (kind, pattern) = entry;
                if pattern.is_match(payload) {
                    matches.push(CredentialMatch {
                        path: current_path.clone().unwrap_or_default(),
                        line: Some(hunk_new_line),
                        kind,
                    });
                    break;
                }
            }
            hunk_new_line = hunk_new_line.saturating_add(1);
        } else if line.starts_with(' ') {
            hunk_new_line = hunk_new_line.saturating_add(1);
        }
        // `-` lines don't advance the new-file cursor.
    }
    matches
}

fn parse_hunk_new_start(line: &str) -> Option<u32> {
    // `@@ -a,b +c,d @@ optional`
    let plus = line.split_whitespace().find(|tok| tok.starts_with('+'))?;
    let stripped = plus.trim_start_matches('+');
    let start = stripped.split(',').next().unwrap_or(stripped);
    start.parse().ok()
}

fn compiled_patterns() -> &'static [(&'static str, Regex)] {
    static CELL: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    CELL.get_or_init(|| {
        // High-confidence patterns. False-positive avoidance: each pattern
        // anchors on a fixed prefix that doesn't appear in normal source.
        vec![
            ("aws_key", Regex::new(r"AKIA[0-9A-Z]{16}").expect("regex")),
            (
                "private_key",
                Regex::new(r"-----BEGIN (?:RSA |OPENSSH |EC )?PRIVATE KEY-----").expect("regex"),
            ),
            (
                "github_pat",
                Regex::new(r"ghp_[A-Za-z0-9]{36}").expect("regex"),
            ),
            (
                "github_fine_grained",
                Regex::new(r"github_pat_[A-Za-z0-9_]{82}").expect("regex"),
            ),
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff_with(added: &str) -> String {
        format!("--- a/secrets.txt\n+++ b/secrets.txt\n@@ -0,0 +1,1 @@\n+{added}\n")
    }

    #[test]
    fn detects_aws_key() {
        let diff = diff_with("aws_key = AKIAIOSFODNN7EXAMPLE");
        let m = scan_diff(&diff);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].kind, "aws_key");
        assert_eq!(m[0].path, "secrets.txt");
    }

    #[test]
    fn detects_private_key() {
        let diff = diff_with("-----BEGIN RSA PRIVATE KEY-----");
        let m = scan_diff(&diff);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].kind, "private_key");
    }

    #[test]
    fn detects_github_pat() {
        let diff = diff_with("token = ghp_abcdefghijklmnopqrstuvwxyz0123456789AB");
        let m = scan_diff(&diff);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].kind, "github_pat");
    }

    #[test]
    fn ignores_removed_lines() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,1 +1,0 @@\n-aws_key = AKIAIOSFODNN7EXAMPLE\n";
        let m = scan_diff(diff);
        assert!(m.is_empty(), "removed lines should not match");
    }

    #[test]
    fn ignores_unchanged_lines() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,1 +1,1 @@\n aws_key = AKIAIOSFODNN7EXAMPLE\n";
        let m = scan_diff(diff);
        assert!(m.is_empty(), "context lines should not match");
    }

    #[test]
    fn line_numbers_increment_for_multi_addition() {
        let diff = "--- a/x\n+++ b/x\n@@ -0,0 +5,2 @@\n+plain text\n+ghp_abcdefghijklmnopqrstuvwxyz0123456789AB\n";
        let m = scan_diff(diff);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, Some(6));
    }
}
