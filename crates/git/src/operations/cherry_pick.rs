//! S-DST cherry-pick — `git cherry-pick [--no-commit] [-x] [-m N] <sha>...`.
//!
//! Single or multi-sha. Conflict pause is detected by `CHERRY_PICK_HEAD`
//! plus unmerged paths; UI handlers re-use `git_conflict_ui`'s resolver +
//! `git cherry-pick --continue` to finish the queue.

use anyhow::{Result, anyhow};
use std::path::Path;

use super::direct::{
    current_branch, list_conflicted_paths, no_editor_envs, op_in_progress, run_git_with_envs,
};
use super::{AtomicGitOp, RunOutcome};

pub struct CherryPickOp {
    pub shas: Vec<String>,
    pub no_commit: bool,
    /// Mainline parent number for cherry-picking a merge commit
    /// (`-m N`). `None` is the default.
    pub mainline: Option<u32>,
    /// `-x`: append `(cherry picked from commit <sha>)` to the log message.
    pub x: bool,
}

impl AtomicGitOp for CherryPickOp {
    type Output = RunOutcome;

    fn op_name(&self) -> &'static str {
        "cherry_pick"
    }

    fn affected_branches(&self, repo_path: &Path) -> Vec<String> {
        current_branch(repo_path).into_iter().collect()
    }

    fn affects_branch(&self) -> Option<String> {
        // Resolved at run-time via affected_branches; the protection
        // check upstream calls affects_branch() pre-run, but cherry-pick
        // operates on whatever HEAD points at. Returning None means the
        // protection layer skips the static check; the runtime branch
        // ends up in affected_branches() for backup-ref purposes.
        None
    }

    fn run(&mut self, repo_path: &Path) -> Result<RunOutcome> {
        if self.shas.is_empty() {
            return Err(anyhow!("cherry_pick: no commits supplied"));
        }
        let mut args: Vec<String> = vec!["cherry-pick".into()];
        if self.no_commit {
            args.push("--no-commit".into());
        }
        if self.x {
            args.push("-x".into());
        }
        if let Some(parent) = self.mainline {
            args.push("-m".into());
            args.push(parent.to_string());
        }
        for sha in &self.shas {
            args.push(sha.clone());
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let envs = no_editor_envs();
        let output = run_git_with_envs(repo_path, &arg_refs, &envs)?;
        if output.status.success() {
            return Ok(RunOutcome::Completed);
        }
        // Did cherry-pick pause for conflict? `CHERRY_PICK_HEAD` exists
        // and `git status` shows unmerged paths.
        let in_progress = op_in_progress(repo_path, "CHERRY_PICK_HEAD").unwrap_or(false);
        if in_progress {
            let conflicts = list_conflicted_paths(repo_path).unwrap_or_default();
            return Ok(RunOutcome::PausedForConflict {
                conflicted_files: conflicts,
            });
        }
        Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::OpRunner;
    use std::process::Command;
    use tempfile::TempDir;

    #[allow(clippy::disallowed_methods)]
    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@x")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@x")
            .status()
            .expect("spawn git");
        assert!(status.success(), "`git {}` failed", args.join(" "));
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

    fn setup_repo() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.name", "T"]);
        git(dir.path(), &["config", "user.email", "t@x"]);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "init"]);
        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "feature change"]);
        dir
    }

    #[test]
    fn cherry_pick_clean_path() {
        let dir = setup_repo();
        crate::undo_registry::test_override::set(dir.path().to_path_buf());
        let feature_sha = rev_parse(dir.path(), "feature");
        git(dir.path(), &["checkout", "-q", "main"]);

        let outcome = OpRunner::run(
            CherryPickOp {
                shas: vec![feature_sha],
                no_commit: false,
                mainline: None,
                x: false,
            },
            dir.path(),
        )
        .expect("cherry-pick should run");
        assert!(matches!(outcome, RunOutcome::Completed));
        // b.txt should now exist on main.
        assert!(dir.path().join("b.txt").exists());

        let backups = crate::backup::list(dir.path(), Some("main"), None).expect("list");
        assert_eq!(backups.len(), 1);
        assert_eq!(backups[0].op, "cherry_pick");
        crate::undo_registry::test_override::clear();
    }

    #[test]
    fn cherry_pick_conflict_pauses() {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.name", "T"]);
        git(dir.path(), &["config", "user.email", "t@x"]);
        std::fs::write(dir.path().join("a.txt"), "base\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "init"]);
        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.path().join("a.txt"), "feature\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "feature"]);
        let feature_sha = rev_parse(dir.path(), "feature");
        git(dir.path(), &["checkout", "-q", "main"]);
        std::fs::write(dir.path().join("a.txt"), "main\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "main change"]);

        crate::undo_registry::test_override::set(dir.path().to_path_buf());
        let outcome = OpRunner::run(
            CherryPickOp {
                shas: vec![feature_sha],
                no_commit: false,
                mainline: None,
                x: false,
            },
            dir.path(),
        )
        .expect("cherry-pick run should succeed (paused)");
        match outcome {
            RunOutcome::PausedForConflict { conflicted_files } => {
                assert!(!conflicted_files.is_empty());
            }
            other => panic!("expected PausedForConflict, got {other:?}"),
        }
        crate::undo_registry::test_override::clear();
    }
}
