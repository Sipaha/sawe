//! File-history (S-FHT) support for the Git Graph view.
//!
//! Hosts the [`FileHistoryOptions`] toggle struct (Follow Renames / With
//! Local Changes / Show Inline Diff), the [`RenameEntry`] record produced
//! by `git log --follow --name-status`, and the parser that turns a raw
//! `R<score>\told\tnew` line into a [`RenameEntry`].
//!
//! The mode itself is implemented as a thin preset on top of the existing
//! [`crate::GitGraph`] infrastructure — see [`crate::GitGraph::for_file_history`].

use git::repository::RepoPath;

/// Toggle state for the file-history preset. Each field maps to a toolbar
/// IconButton shown only when [`crate::GraphMode::FileHistory`] is active.
///
/// `follow_renames` toggles the `--follow` opt-in (default `true`). When
/// off, `--no-follow` is appended to the extra-args list, which negates the
/// `--follow` that `LogSource::File` adds by default.
///
/// `with_local_changes` controls whether a synthetic row representing the
/// uncommitted state of the file is prepended at index 0 of the log.
///
/// `show_inline_diff` is a v1 stub — the heavy rendering refactor it
/// requires is deferred. The toolbar surface is wired so the persistence /
/// MCP shape is stable; the toggle currently has no rendering effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHistoryOptions {
    pub follow_renames: bool,
    pub with_local_changes: bool,
    pub show_inline_diff: bool,
}

impl Default for FileHistoryOptions {
    fn default() -> Self {
        Self {
            follow_renames: true,
            with_local_changes: false,
            show_inline_diff: false,
        }
    }
}

impl FileHistoryOptions {
    /// Extra `git log` arguments produced by these toggles. Appended to the
    /// existing [`crate::filters::LogFilters::to_git_args`] output.
    pub fn extra_git_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if !self.follow_renames {
            // `LogSource::File` always adds `--follow`; `--no-follow` later
            // in argv negates it.
            args.push("--no-follow".to_string());
        }
        args
    }
}

/// One entry produced by `git log --follow --name-status` when a commit
/// renames a file. The score is the similarity percent reported by git's
/// rename detection (`R75` → `score: 75`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameEntry {
    pub score: u8,
    pub old: String,
    pub new: String,
}

impl RenameEntry {
    /// Parse one line of `--name-status` output. Returns `None` for lines
    /// that aren't rename records (Adds, Modifies, Deletes, blank lines).
    ///
    /// Accepts either tab-separated (`R75\told.rs\tnew.rs`) or
    /// space-separated (`R75 old.rs new.rs`) input — git emits tabs but
    /// callers occasionally pre-normalize whitespace, and the parser is
    /// cheap to make permissive.
    pub fn parse(line: &str) -> Option<Self> {
        let trimmed = line.trim();
        if !trimmed.starts_with('R') {
            return None;
        }
        // Split on tabs first (git's native format); fall back to whitespace
        // if no tabs are present.
        let mut iter: Box<dyn Iterator<Item = &str>> = if trimmed.contains('\t') {
            Box::new(trimmed.split('\t'))
        } else {
            Box::new(trimmed.split_whitespace())
        };
        let head = iter.next()?;
        let old = iter.next()?.trim();
        let new = iter.next()?.trim();
        if old.is_empty() || new.is_empty() {
            return None;
        }
        // `head` is `R<score>` — strip the leading R, parse the digits.
        let score_str = head.strip_prefix('R')?;
        let score: u8 = score_str.parse().ok()?;
        Some(Self {
            score,
            old: old.to_string(),
            new: new.to_string(),
        })
    }
}

/// Repository-relative path identifying which file the file-history view is
/// pinned to. Thin newtype over `RepoPath` so callers don't accidentally
/// pass an arbitrary path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHistoryPath(pub RepoPath);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tab_separated_rename() {
        let entry = RenameEntry::parse("R75\told.rs\tnew.rs").expect("rename");
        assert_eq!(
            entry,
            RenameEntry {
                score: 75,
                old: "old.rs".to_string(),
                new: "new.rs".to_string(),
            }
        );
    }

    #[test]
    fn parses_space_separated_rename() {
        let entry = RenameEntry::parse("R75 old.rs new.rs").expect("rename");
        assert_eq!(entry.score, 75);
        assert_eq!(entry.old, "old.rs");
        assert_eq!(entry.new, "new.rs");
    }

    #[test]
    fn ignores_non_rename_lines() {
        assert!(RenameEntry::parse("M\tsome/file.rs").is_none());
        assert!(RenameEntry::parse("A\tnew/file.rs").is_none());
        assert!(RenameEntry::parse("D\tgone/file.rs").is_none());
        assert!(RenameEntry::parse("").is_none());
    }

    #[test]
    fn rejects_malformed_score() {
        assert!(RenameEntry::parse("Rfoo\told\tnew").is_none());
    }

    #[test]
    fn default_follow_renames_is_on() {
        let opts = FileHistoryOptions::default();
        assert!(opts.follow_renames);
        assert!(!opts.with_local_changes);
        assert!(!opts.show_inline_diff);
    }

    #[test]
    fn extra_git_args_emits_no_follow_when_off() {
        let mut opts = FileHistoryOptions::default();
        assert!(opts.extra_git_args().is_empty());
        opts.follow_renames = false;
        assert_eq!(opts.extra_git_args(), vec!["--no-follow".to_string()]);
    }
}
