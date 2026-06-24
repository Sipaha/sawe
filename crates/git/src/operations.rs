//! High-level atomic git operations with auto-backup, undo registry, and
//! repo-busy guard.
//!
//! Each user-facing destructive operation (cherry-pick, revert, reset, drop,
//! squash, fixup, edit-message, move, rebase, interactive-rebase, merge) is a
//! struct that implements [`AtomicGitOp`]. UI handlers in
//! `git_ui::handlers::*` construct the struct and call [`OpRunner::run`]; no
//! operation invokes git CLI directly bypassing the runner.
//!
//! Concrete operations are added as their owning S-* tasks land
//! (S-DST, S-RBL, etc.). See `docs/superpowers/plans/git-panel-plan.md`.

pub mod cherry_pick;
pub mod direct;
pub mod drop_commit;
pub mod edit_commit_message;
pub mod fixup;
pub mod helpers;
pub mod linear_rebase;
pub mod merge;
pub mod move_commit;
pub mod patch;
pub mod rebase;
pub mod reset;
pub mod revert;
pub mod reword;
pub mod shelf;
pub mod squash;

use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;
use util::ResultExt as _;

use crate::{backup, repo_lock, undo_registry};

/// Outcome of an [`AtomicGitOp::run`] that can pause for user input.
/// Cherry-pick / revert / merge / rebase return a variant of this; ops
/// that are strictly atomic (delete-branch, rename-branch, reset) return
/// `Completed` from successful runs and propagate errors via `Result`.
#[derive(Debug, Clone)]
pub enum RunOutcome {
    Completed,
    PausedForConflict {
        /// Repo-relative paths (as reported by `git status --porcelain`)
        /// of files with unmerged stages.
        conflicted_files: Vec<PathBuf>,
    },
    PausedForExecFailure {
        command: String,
        stderr: String,
    },
}

/// A single atomic git operation. Implementors describe their identity and
/// affected branches; [`OpRunner::run`] handles the safety umbrella (lock,
/// backup, undo registration, error path).
pub trait AtomicGitOp {
    type Output;

    /// Stable identifier for backup-ref naming and undo registry. Examples:
    /// `"cherry_pick"`, `"drop"`, `"squash"`, `"rebase_interactive"`.
    fn op_name(&self) -> &'static str;

    /// Whether this operation can lose work without a backup. Default `false`.
    /// Explicit opt-in per P-3 (no implicit detection).
    fn is_destructive(&self) -> bool {
        false
    }

    /// Branches whose tips should be backed up before [`Self::run`]. Empty
    /// for ops that don't affect refs (e.g. pure index/working-tree changes).
    fn affected_branches(&self, repo_path: &Path) -> Vec<String>;

    /// Tries to extract the target branch from the operation payload for
    /// branch-protection enforcement (see `solution_git::branch_protection`
    /// in S-SOL-PRT). `None` means the op isn't tied to a single branch and
    /// protection is skipped.
    fn affects_branch(&self) -> Option<String> {
        None
    }

    /// Execute the operation. Called under the repo-busy lock with backup
    /// refs already in place. Takes `&mut self` so [`OpRunner::run`] can
    /// invoke [`Self::on_failure`] afterwards if `run` errors out.
    fn run(&mut self, repo_path: &Path) -> Result<Self::Output>;

    /// Hook invoked when [`Self::run`] returns `Err`. Default: no-op. Use
    /// for operation-specific cleanup (clearing intermediate state, etc.) —
    /// the backup ref and the undo entry are managed by the runner itself.
    fn on_failure(&self, _repo_path: &Path, _err: &anyhow::Error) -> Result<()> {
        Ok(())
    }
}

/// Runs an [`AtomicGitOp`] under the safety umbrella: repo-busy guard,
/// backup-ref creation, undo registration.
pub struct OpRunner;

impl OpRunner {
    /// Execute `op` with backup + undo registration + repo-busy guard.
    ///
    /// 1. Acquire repo lock via [`crate::repo_lock`] — propagate `RepoBusyError`.
    /// 2. For each branch in `op.affected_branches()`: create a backup-ref via [`crate::backup`].
    /// 3. If `op.is_destructive()` and at least one backup exists: register an undo entry.
    /// 4. Run the operation.
    /// 5. On `Ok`: complete the undo entry with the new branch tip. On `Err`:
    ///    mark it failed and call `op.on_failure`.
    /// 6. Release the lock (drop guard).
    pub fn run<O: AtomicGitOp>(mut op: O, repo_path: &Path) -> Result<O::Output> {
        let op_name = op.op_name();
        let _lock = repo_lock::acquire(repo_path, op_name)?;

        let branches = op.affected_branches(repo_path);
        let mut backups = Vec::with_capacity(branches.len());
        for branch in &branches {
            match backup::create(repo_path, branch, op_name) {
                Ok(b) => backups.push(b),
                Err(err) => {
                    log::warn!("git::operations: failed to back up {branch} for {op_name}: {err}");
                }
            }
        }

        let undo_id = if op.is_destructive() {
            backups.first().and_then(|first| {
                undo_registry::record(repo_path, op_name, &first.branch, &first.before_sha)
                    .log_err()
            })
        } else {
            None
        };

        let primary_branch = backups.first().map(|b| b.branch.clone());
        let result = op.run(repo_path);

        match &result {
            Ok(_) => {
                if let (Some(id), Some(branch)) = (undo_id, primary_branch.as_deref()) {
                    match backup::read_branch_tip(repo_path, branch) {
                        Ok(after) => {
                            undo_registry::complete(id, &after).log_err();
                        }
                        Err(err) => {
                            log::warn!(
                                "git::operations: completed {op_name} but couldn't read {branch} tip: {err}"
                            );
                        }
                    }
                }
            }
            Err(err) => {
                if let Some(id) = undo_id {
                    undo_registry::mark_failed(id).log_err();
                }
                if let Err(hook_err) = op.on_failure(repo_path, err) {
                    log::warn!(
                        "git::operations: on_failure hook for {op_name} errored: {hook_err}"
                    );
                }
            }
        }

        result
    }
}

/// S-BRP "Delete Branch" — `git branch -d <name>` (or `-D` when `force`).
/// `force` flips both the git CLI flag and [`Self::is_destructive`], so the
/// runner registers an undo entry only on lossy deletions.
pub struct DeleteBranchOp {
    pub name: String,
    pub force: bool,
}

impl AtomicGitOp for DeleteBranchOp {
    type Output = ();

    fn op_name(&self) -> &'static str {
        if self.force {
            "delete_branch_force"
        } else {
            "delete_branch"
        }
    }

    /// Always undoable: even a non-force `git branch -d` (which only
    /// succeeds when the branch is fully merged, so no commits are lost)
    /// still loses the *ref name*. The backup ref preserves the tip; the
    /// undo entry is what `editor.git.undo_last` walks to surface a
    /// "Restore branch" action.
    fn is_destructive(&self) -> bool {
        true
    }

    fn affected_branches(&self, _repo_path: &Path) -> Vec<String> {
        vec![self.name.clone()]
    }

    fn affects_branch(&self) -> Option<String> {
        Some(self.name.clone())
    }

    fn run(&mut self, repo_path: &Path) -> Result<()> {
        let flag = if self.force { "-D" } else { "-d" };
        run_git_void(repo_path, &["branch", flag, &self.name])
    }
}

/// S-BRP "Rename Branch…" — `git branch -m <old> <new>`. Reversible
/// (the old tip is preserved by the backup ref), so `is_destructive`
/// stays `false` even though the ref name changes.
pub struct RenameBranchOp {
    pub old: String,
    pub new: String,
}

impl AtomicGitOp for RenameBranchOp {
    type Output = ();

    fn op_name(&self) -> &'static str {
        "rename_branch"
    }

    /// Renames keep the tip but lose the old ref name. Mark as undoable so
    /// `undo_last` can restore the original ref pointing at the same SHA.
    fn is_destructive(&self) -> bool {
        true
    }

    fn affected_branches(&self, _repo_path: &Path) -> Vec<String> {
        vec![self.old.clone()]
    }

    fn affects_branch(&self) -> Option<String> {
        Some(self.old.clone())
    }

    fn run(&mut self, repo_path: &Path) -> Result<()> {
        run_git_void(repo_path, &["branch", "-m", &self.old, &self.new])
    }
}

/// S-BRP "Delete Tag" — `git tag -d <name>`. `affected_branches` is
/// empty (tags aren't branches), so the runner doesn't create a
/// branch-shaped backup ref. The undo registry still records the row,
/// but recovery for an unpushed tag is reflog-only.
pub struct DeleteTagOp {
    pub name: String,
}

impl AtomicGitOp for DeleteTagOp {
    type Output = ();

    fn op_name(&self) -> &'static str {
        "delete_tag"
    }

    fn is_destructive(&self) -> bool {
        true
    }

    fn affected_branches(&self, _repo_path: &Path) -> Vec<String> {
        Vec::new()
    }

    fn run(&mut self, repo_path: &Path) -> Result<()> {
        run_git_void(repo_path, &["tag", "-d", &self.name])
    }
}

/// Synchronous `git` invocation used by [`AtomicGitOp::run`] impls.
/// Operations execute on the caller's thread under [`OpRunner::run`]'s
/// repo-busy guard; the runner takes care of the async hop. Allowed-list
/// opt-out scoped to this helper.
#[allow(clippy::disallowed_methods)]
fn run_git_void(repo_path: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::tempdir;

    #[allow(clippy::disallowed_methods)]
    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .expect("spawn git");
        assert!(status.success(), "`git {}` failed", args.join(" "));
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("README.md"), "x").expect("write");
        git(dir.path(), &["add", "README.md"]);
        git(
            dir.path(),
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@x",
                "commit",
                "-qm",
                "init",
            ],
        );
        dir
    }

    struct NoopOp {
        ran: std::sync::Arc<AtomicBool>,
    }

    impl AtomicGitOp for NoopOp {
        type Output = ();
        fn op_name(&self) -> &'static str {
            "test_noop"
        }
        fn is_destructive(&self) -> bool {
            true
        }
        fn affected_branches(&self, _: &Path) -> Vec<String> {
            vec!["main".into()]
        }
        fn run(&mut self, _: &Path) -> Result<()> {
            self.ran.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn run_creates_backup_and_records_undo() {
        let dir = init_repo();
        crate::undo_registry::test_override::set(dir.path().to_path_buf());
        let ran = std::sync::Arc::new(AtomicBool::new(false));
        OpRunner::run(NoopOp { ran: ran.clone() }, dir.path()).expect("run");
        assert!(ran.load(Ordering::SeqCst));
        let backups = backup::list(dir.path(), None, None).expect("list");
        assert_eq!(backups.len(), 1);
        assert_eq!(backups[0].op, "test_noop");
        let undos = undo_registry::list(0).expect("list");
        let entry = undos.iter().find(|e| e.op == "test_noop").expect("entry");
        assert!(entry.after_sha.is_some());
        assert!(!entry.failed);
        crate::undo_registry::test_override::clear();
    }

    struct FailingOp;
    impl AtomicGitOp for FailingOp {
        type Output = ();
        fn op_name(&self) -> &'static str {
            "test_failing"
        }
        fn is_destructive(&self) -> bool {
            true
        }
        fn affected_branches(&self, _: &Path) -> Vec<String> {
            vec!["main".into()]
        }
        fn run(&mut self, _: &Path) -> Result<()> {
            anyhow::bail!("nope")
        }
    }

    #[test]
    fn run_marks_failed_on_error() {
        let dir = init_repo();
        crate::undo_registry::test_override::set(dir.path().to_path_buf());
        let err = OpRunner::run(FailingOp, dir.path()).expect_err("must fail");
        assert!(err.to_string().contains("nope"));
        let undos = undo_registry::list(0).expect("list");
        let entry = undos
            .iter()
            .find(|e| e.op == "test_failing")
            .expect("entry");
        assert!(entry.failed);
        crate::undo_registry::test_override::clear();
    }
}
