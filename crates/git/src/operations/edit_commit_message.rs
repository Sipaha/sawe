//! S-DST edit-commit-message — `git commit --amend -m <msg>` for the
//! HEAD path; for non-HEAD non-merge commits, delegates to
//! [`super::reword::RewordOp`].
//!
//! Editing the message of a non-HEAD merge commit is out of scope per
//! plan and returns Err.

use anyhow::{Result, anyhow, bail};
use std::path::Path;
use std::process::Command;

use super::rebase::{RebaseCallbacks, RebaseHandle};
use super::reword::RewordOp;
use super::{AtomicGitOp, RunOutcome};
use crate::{backup, repo_lock, undo_registry};
use util::ResultExt as _;

pub struct EditMessageOp {
    pub sha: String,
    pub new_message: String,
}

impl EditMessageOp {
    /// Direct synchronous path: only valid when `sha` resolves to HEAD.
    /// `git commit --amend -m <msg>` works for both regular and merge
    /// commits, since amending HEAD doesn't replay any other commit.
    pub fn run_at_head(self, repo_path: &Path) -> Result<RunOutcome> {
        let EditMessageOp { sha, new_message } = self;
        let head = head_sha(repo_path)?;
        if !shas_equal(&head, &sha) {
            bail!("edit_commit_message::run_at_head invoked for non-HEAD sha {sha}");
        }
        // Manual lock + backup-ref + undo (we can't use OpRunner here
        // because it would compete with run_async's lock).
        let _lock = repo_lock::acquire(repo_path, "edit_commit_message")
            .map_err(|err| anyhow!("repo busy: {err}"))?;
        let branch = current_branch(repo_path);
        let mut undo_id = None;
        if let Some(branch) = branch.as_deref() {
            if let Ok(b) = backup::create(repo_path, branch, "edit_commit_message") {
                undo_id = undo_registry::record(
                    repo_path,
                    "edit_commit_message",
                    &b.branch,
                    &b.before_sha,
                )
                .log_err();
            }
        }

        let result = run_amend(repo_path, &new_message);

        match (&result, branch.as_deref(), undo_id) {
            (Ok(_), Some(branch), Some(id)) => {
                if let Ok(after) = backup::read_branch_tip(repo_path, branch) {
                    undo_registry::complete(id, &after).log_err();
                }
            }
            (Err(_), _, Some(id)) => {
                undo_registry::mark_failed(id).log_err();
            }
            _ => {}
        }
        result.map(|_| RunOutcome::Completed)
    }

    /// Branch path: HEAD-or-not. If `sha == HEAD`, runs the synchronous
    /// amend path; otherwise spins up an interactive rebase via
    /// [`RewordOp`].
    pub async fn run(
        self,
        repo_path: &Path,
        callbacks: RebaseCallbacks,
    ) -> Result<EditMessageOutcome> {
        let head = head_sha(repo_path)?;
        if shas_equal(&head, &self.sha) {
            let outcome = self.run_at_head(repo_path)?;
            return Ok(EditMessageOutcome::Direct(outcome));
        }
        let handle = RewordOp {
            sha: self.sha,
            new_message: self.new_message,
        }
        .run(repo_path, callbacks)
        .await?;
        Ok(EditMessageOutcome::ViaRebase(handle))
    }
}

pub enum EditMessageOutcome {
    Direct(RunOutcome),
    ViaRebase(RebaseHandle),
}

/// AtomicGitOp impl for the HEAD-path. The trait is sync-only so the
/// non-HEAD path stays on the async [`EditMessageOp::run`] flow.
impl AtomicGitOp for EditMessageOp {
    type Output = RunOutcome;

    fn op_name(&self) -> &'static str {
        "edit_commit_message"
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
        let head = head_sha(repo_path)?;
        if !shas_equal(&head, &self.sha) {
            bail!(
                "AtomicGitOp::run for EditMessageOp only supports the HEAD path; \
                 call EditMessageOp::run async path for past commits"
            );
        }
        run_amend(repo_path, &self.new_message)?;
        Ok(RunOutcome::Completed)
    }
}

#[allow(clippy::disallowed_methods)]
fn run_amend(repo_path: &Path, new_message: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["commit", "--amend", "-m", new_message])
        .env("GIT_EDITOR", "true")
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git commit --amend -m … failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

#[allow(clippy::disallowed_methods)]
fn head_sha(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[allow(clippy::disallowed_methods)]
fn current_branch(repo_path: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["symbolic-ref", "--short", "-q", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        None
    } else {
        Some(stdout)
    }
}

fn shas_equal(a: &str, b: &str) -> bool {
    let min = a.len().min(b.len());
    if min == 0 {
        return false;
    }
    a[..min].eq_ignore_ascii_case(&b[..min])
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn edit_head_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.name", "T"]);
        git(dir.path(), &["config", "user.email", "t@x"]);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "old message"]);

        let head = rev_parse(dir.path(), "HEAD");
        crate::undo_registry::test_override::set(dir.path().to_path_buf());
        let op = EditMessageOp {
            sha: head,
            new_message: "new message".into(),
        };
        let outcome = op.run_at_head(dir.path()).expect("amend");
        assert!(matches!(outcome, RunOutcome::Completed));
        let log = String::from_utf8(
            #[allow(clippy::disallowed_methods)]
            Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["log", "-1", "--pretty=format:%s"])
                .output()
                .expect("log")
                .stdout,
        )
        .unwrap();
        assert_eq!(log, "new message");
        crate::undo_registry::test_override::clear();
    }

    #[test]
    fn rejects_non_head_sha_in_run_at_head() {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.name", "T"]);
        git(dir.path(), &["config", "user.email", "t@x"]);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "first"]);
        let first = rev_parse(dir.path(), "HEAD");
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "second"]);

        let op = EditMessageOp {
            sha: first,
            new_message: "new".into(),
        };
        let result = op.run_at_head(dir.path());
        assert!(result.is_err());
    }
}
