//! S-DST linear rebase — `git rebase [--autostash] <target>`. Used for
//! the "Rebase Current onto…" entry from the branches popup. Does NOT
//! use the interactive-rebase machinery in [`super::rebase`]; we pass
//! through to git directly and surface the conflict-pause if one
//! happens.

use anyhow::{Result, anyhow};
use std::path::Path;

use super::direct::{
    current_branch, dot_git_dir, list_conflicted_paths, no_editor_envs, run_git_with_envs,
};
use super::{AtomicGitOp, RunOutcome};

pub struct LinearRebaseOp {
    pub target_branch: String,
    pub autostash: bool,
}

impl AtomicGitOp for LinearRebaseOp {
    type Output = RunOutcome;

    fn op_name(&self) -> &'static str {
        "rebase"
    }

    fn is_destructive(&self) -> bool {
        true
    }

    fn affected_branches(&self, repo_path: &Path) -> Vec<String> {
        current_branch(repo_path).into_iter().collect()
    }

    fn affects_branch(&self) -> Option<String> {
        None
    }

    fn run(&mut self, repo_path: &Path) -> Result<RunOutcome> {
        let mut args: Vec<String> = vec!["rebase".into()];
        if self.autostash {
            args.push("--autostash".into());
        }
        args.push(self.target_branch.clone());
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let envs = no_editor_envs();
        let output = run_git_with_envs(repo_path, &arg_refs, &envs)?;
        if output.status.success() {
            return Ok(RunOutcome::Completed);
        }
        let dot_git = dot_git_dir(repo_path)?;
        let in_progress =
            dot_git.join("rebase-merge").exists() || dot_git.join("rebase-apply").exists();
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
    fn rebase_clean_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.name", "T"]);
        git(dir.path(), &["config", "user.email", "t@x"]);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "base"]);
        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "feature"]);
        git(dir.path(), &["checkout", "-q", "main"]);
        std::fs::write(dir.path().join("c.txt"), "c\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "main2"]);
        git(dir.path(), &["checkout", "-q", "feature"]);

        crate::undo_registry::test_override::set(dir.path().to_path_buf());
        let outcome = OpRunner::run(
            LinearRebaseOp {
                target_branch: "main".into(),
                autostash: false,
            },
            dir.path(),
        )
        .expect("rebase");
        assert!(matches!(outcome, RunOutcome::Completed));
        crate::undo_registry::test_override::clear();
    }
}
