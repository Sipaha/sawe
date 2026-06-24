//! S-DST reset — `git reset {--soft|--mixed|--hard|--keep} <sha>`.
//!
//! HEAD moves; recovery is via the backup-ref the runner created.

use anyhow::{Result, anyhow};
use std::path::Path;

use super::direct::{current_branch, run_git_with_envs};
use super::{AtomicGitOp, RunOutcome};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
    Keep,
}

impl ResetMode {
    pub fn flag(self) -> &'static str {
        match self {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
            ResetMode::Keep => "--keep",
        }
    }
}

pub struct ResetOp {
    pub sha: String,
    pub mode: ResetMode,
}

impl AtomicGitOp for ResetOp {
    type Output = RunOutcome;

    fn op_name(&self) -> &'static str {
        match self.mode {
            ResetMode::Soft => "reset_soft",
            ResetMode::Mixed => "reset_mixed",
            ResetMode::Hard => "reset_hard",
            ResetMode::Keep => "reset_keep",
        }
    }

    fn is_destructive(&self) -> bool {
        // Every mode moves HEAD; even soft reset abandons commits ahead
        // of <sha> on the branch tip. Recovery is via backup-ref.
        true
    }

    fn affected_branches(&self, repo_path: &Path) -> Vec<String> {
        current_branch(repo_path).into_iter().collect()
    }

    fn affects_branch(&self) -> Option<String> {
        None
    }

    fn run(&mut self, repo_path: &Path) -> Result<RunOutcome> {
        let envs: HashMap<String, String> = HashMap::new();
        let output = run_git_with_envs(repo_path, &["reset", self.mode.flag(), &self.sha], &envs)?;
        if !output.status.success() {
            return Err(anyhow!(
                "git reset {} {} failed: {}",
                self.mode.flag(),
                self.sha,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(RunOutcome::Completed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::OpRunner;
    use std::process::Command;

    #[allow(clippy::disallowed_methods)]
    fn git(dir: &Path, args: &[&str]) {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@x")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@x")
            .status()
            .expect("git");
    }

    #[allow(clippy::disallowed_methods)]
    fn rev_parse(dir: &Path, rev: &str) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", rev])
            .output()
            .expect("rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn reset_hard_moves_head_and_creates_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.name", "T"]);
        git(dir.path(), &["config", "user.email", "t@x"]);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "first"]);
        let first = rev_parse(dir.path(), "HEAD");
        std::fs::write(dir.path().join("a.txt"), "b\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "second"]);
        let second = rev_parse(dir.path(), "HEAD");

        crate::undo_registry::test_override::set(dir.path().to_path_buf());
        let outcome = OpRunner::run(
            ResetOp {
                sha: first.clone(),
                mode: ResetMode::Hard,
            },
            dir.path(),
        )
        .expect("reset");
        assert!(matches!(outcome, RunOutcome::Completed));
        assert_eq!(rev_parse(dir.path(), "HEAD"), first);

        let backups = crate::backup::list(dir.path(), Some("main"), None).expect("list");
        assert_eq!(backups.len(), 1);
        assert_eq!(backups[0].before_sha, second);
        crate::undo_registry::test_override::clear();
    }
}
