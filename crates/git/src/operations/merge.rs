//! S-DST merge — `git merge [--no-ff] [--squash] [-m <msg>] <branch>`.
//!
//! Creates a merge commit. Not "destructive" in the sense of losing
//! history — but the runner still creates a backup-ref so undo can
//! reset HEAD back to the pre-merge tip.

use anyhow::{Result, anyhow};
use std::path::Path;

use super::direct::{
    current_branch, list_conflicted_paths, no_editor_envs, op_in_progress, run_git_with_envs,
};
use super::{AtomicGitOp, RunOutcome};

pub struct MergeOp {
    pub target_branch: String,
    pub no_ff: bool,
    pub squash: bool,
    pub message: Option<String>,
}

impl AtomicGitOp for MergeOp {
    type Output = RunOutcome;

    fn op_name(&self) -> &'static str {
        "merge"
    }

    fn affected_branches(&self, repo_path: &Path) -> Vec<String> {
        current_branch(repo_path).into_iter().collect()
    }

    fn affects_branch(&self) -> Option<String> {
        None
    }

    fn run(&mut self, repo_path: &Path) -> Result<RunOutcome> {
        let mut args: Vec<String> = vec!["merge".into()];
        // --no-edit prevents git from launching an editor for the
        // merge-commit message; combined with the GIT_EDITOR=true env
        // pin in `no_editor_envs` it's belt + suspenders.
        args.push("--no-edit".into());
        if self.no_ff {
            args.push("--no-ff".into());
        }
        if self.squash {
            args.push("--squash".into());
        }
        if let Some(message) = &self.message {
            args.push("-m".into());
            args.push(message.clone());
        }
        args.push(self.target_branch.clone());
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let envs = no_editor_envs();
        let output = run_git_with_envs(repo_path, &arg_refs, &envs)?;
        if output.status.success() {
            return Ok(RunOutcome::Completed);
        }
        // --squash leaves the index dirty without committing; that's a
        // success even though git's exit code is 0. If a real conflict
        // happens, MERGE_HEAD will exist with unmerged paths.
        let in_progress = op_in_progress(repo_path, "MERGE_HEAD").unwrap_or(false);
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

    #[test]
    fn merge_clean_no_ff() {
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
        git(dir.path(), &["commit", "-qm", "feature"]);
        git(dir.path(), &["checkout", "-q", "main"]);

        crate::undo_registry::test_override::set(dir.path().to_path_buf());
        let outcome = OpRunner::run(
            MergeOp {
                target_branch: "feature".into(),
                no_ff: true,
                squash: false,
                message: Some("merge".into()),
            },
            dir.path(),
        )
        .expect("merge");
        assert!(matches!(outcome, RunOutcome::Completed));
        assert!(dir.path().join("b.txt").exists());
        crate::undo_registry::test_override::clear();
    }
}
