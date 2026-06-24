//! S-DST revert — `git revert [--no-commit] [-m N] <sha>...`. Mirrors
//! [`super::cherry_pick`]: same conflict-pause semantics via
//! `REVERT_HEAD`.

use anyhow::{Result, anyhow};
use std::path::Path;

use super::direct::{
    current_branch, list_conflicted_paths, no_editor_envs, op_in_progress, run_git_with_envs,
};
use super::{AtomicGitOp, RunOutcome};

pub struct RevertOp {
    pub shas: Vec<String>,
    pub no_commit: bool,
    pub mainline: Option<u32>,
}

impl AtomicGitOp for RevertOp {
    type Output = RunOutcome;

    fn op_name(&self) -> &'static str {
        "revert"
    }

    fn affected_branches(&self, repo_path: &Path) -> Vec<String> {
        current_branch(repo_path).into_iter().collect()
    }

    fn affects_branch(&self) -> Option<String> {
        None
    }

    fn run(&mut self, repo_path: &Path) -> Result<RunOutcome> {
        if self.shas.is_empty() {
            return Err(anyhow!("revert: no commits supplied"));
        }
        let mut args: Vec<String> = vec!["revert".into()];
        if self.no_commit {
            args.push("--no-commit".into());
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
        let in_progress = op_in_progress(repo_path, "REVERT_HEAD").unwrap_or(false);
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
            .expect("spawn git");
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
    fn revert_clean_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.name", "T"]);
        git(dir.path(), &["config", "user.email", "t@x"]);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "init"]);
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "second"]);
        let head = rev_parse(dir.path(), "HEAD");

        crate::undo_registry::test_override::set(dir.path().to_path_buf());
        let outcome = OpRunner::run(
            RevertOp {
                shas: vec![head],
                no_commit: false,
                mainline: None,
            },
            dir.path(),
        )
        .expect("revert should run");
        assert!(matches!(outcome, RunOutcome::Completed));
        assert!(!dir.path().join("b.txt").exists());
        crate::undo_registry::test_override::clear();
    }
}
