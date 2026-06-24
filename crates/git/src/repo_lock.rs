//! Per-repo busy guard (P-10) — only one git operation at tier ≥ Write may run
//! at a time per repository. Uses `.git/spke/op.lock` plus a sanity check on
//! `.git/index.lock` (which would mean an external `git` invocation is mid-flight).
//!
//! Stale-lock cleanup: on `acquire`, if the existing lock file's PID is not
//! alive, the lock is silently overwritten (with a warn-level log entry).

use anyhow::{Context as _, Result};
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

const SPKE_DIR: &str = "spke";
const OP_LOCK_FILE: &str = "op.lock";

/// Reason the repo is busy. Surfaced through [`RepoBusyError`] so callers can
/// show a useful message ("Repository busy: cherry_pick in progress").
#[derive(Debug, Clone)]
pub enum BusyReason {
    /// Another spk-editor operation is in flight, identified by `AtomicGitOp::op_name`.
    OtherOp(String),
    /// `.git/index.lock` is present — an external `git` process is running.
    ExternalGit,
}

#[derive(Debug, thiserror::Error)]
#[error("repository busy: {reason:?}")]
pub struct RepoBusyError {
    pub reason: BusyReason,
}

/// Guard returned by [`acquire`]. The lock is released on drop.
#[derive(Debug)]
pub struct RepoLock {
    lock_path: PathBuf,
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.lock_path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "git::repo_lock: failed to remove lock {}: {err}",
                    self.lock_path.display()
                );
            }
        }
    }
}

/// Try to acquire the busy guard for `repo_path` for operation `op_name`.
/// Returns `Err(RepoBusyError)` if another operation is already running.
pub fn acquire(repo_path: &Path, op_name: &'static str) -> Result<RepoLock, RepoBusyError> {
    let dot_git = dot_git_dir(repo_path).map_err(|err| RepoBusyError {
        reason: BusyReason::OtherOp(format!("locating .git: {err}")),
    })?;

    if dot_git.join(crate::INDEX_LOCK).exists() {
        return Err(RepoBusyError {
            reason: BusyReason::ExternalGit,
        });
    }

    let spke_dir = dot_git.join(SPKE_DIR);
    if let Err(err) = std::fs::create_dir_all(&spke_dir) {
        return Err(RepoBusyError {
            reason: BusyReason::OtherOp(format!("creating {}: {err}", spke_dir.display())),
        });
    }
    let lock_path = spke_dir.join(OP_LOCK_FILE);

    if let Some(existing) = read_existing_lock(&lock_path) {
        if pid_is_alive(existing.pid) {
            return Err(RepoBusyError {
                reason: BusyReason::OtherOp(existing.op_name),
            });
        }
        log::warn!(
            "git::repo_lock: stale lock from PID {} ({:?}) — overwriting",
            existing.pid,
            existing.op_name
        );
        std::fs::remove_file(&lock_path).ok();
    }

    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            // Race with a concurrent acquire: re-read and report whichever op
            // won the race.
            let other = read_existing_lock(&lock_path)
                .map(|e| e.op_name)
                .unwrap_or_else(|| "unknown".to_string());
            return Err(RepoBusyError {
                reason: BusyReason::OtherOp(other),
            });
        }
        Err(err) => {
            return Err(RepoBusyError {
                reason: BusyReason::OtherOp(format!("opening lock: {err}")),
            });
        }
    };

    let pid = std::process::id();
    let unix = current_unix_seconds();
    let body = format!("{op_name}\n{pid}\n{unix}\n");
    if let Err(err) = file.write_all(body.as_bytes()) {
        std::fs::remove_file(&lock_path).ok();
        return Err(RepoBusyError {
            reason: BusyReason::OtherOp(format!("writing lock: {err}")),
        });
    }
    Ok(RepoLock { lock_path })
}

struct ExistingLock {
    op_name: String,
    pid: u32,
}

fn read_existing_lock(path: &Path) -> Option<ExistingLock> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut body = String::new();
    file.read_to_string(&mut body).ok()?;
    let mut lines = body.lines();
    let op_name = lines.next()?.trim().to_string();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    Some(ExistingLock { op_name, pid })
}

fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // `kill(pid, 0)` returns 0 if a signal could be delivered (process
        // exists), -1 with errno=ESRCH if the process is gone. EPERM still
        // means the process exists but we lack permission — treat as alive.
        let pid_i32 = pid as libc::pid_t;
        let ret = unsafe { libc::kill(pid_i32, 0) };
        if ret == 0 {
            return true;
        }
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        errno != libc::ESRCH
    }
    #[cfg(not(unix))]
    {
        // Best-effort on Windows: treat all PIDs as alive so we never drop a
        // legit lock. Real cleanup happens when the holder drops the guard.
        let _ = pid;
        true
    }
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Resolve the `.git` directory for `repo_path`. Handles the common case of
/// a worktree where `<repo>/.git` is a file pointing at `<main>/.git/worktrees/<name>`.
fn dot_git_dir(repo_path: &Path) -> Result<PathBuf> {
    let candidate = repo_path.join(crate::DOT_GIT);
    let metadata =
        std::fs::metadata(&candidate).with_context(|| format!("stat {}", candidate.display()))?;
    if metadata.is_dir() {
        return Ok(candidate);
    }
    // `.git` file: `gitdir: <path>` per git's worktree mechanism.
    let body = std::fs::read_to_string(&candidate)
        .with_context(|| format!("read {}", candidate.display()))?;
    let target = body
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:").map(str::trim))
        .with_context(|| format!("no gitdir: line in {}", candidate.display()))?;
    let target_path = PathBuf::from(target);
    if target_path.is_absolute() {
        Ok(target_path)
    } else {
        Ok(repo_path.join(target_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join(crate::DOT_GIT)).expect("create .git dir");
    }

    #[test]
    fn acquire_then_release_succeeds() {
        let dir = tempdir().expect("tempdir");
        init_repo(dir.path());
        {
            let lock = acquire(dir.path(), "test_op").expect("acquire");
            assert!(
                dir.path()
                    .join(".git")
                    .join(SPKE_DIR)
                    .join(OP_LOCK_FILE)
                    .exists()
            );
            drop(lock);
        }
        assert!(
            !dir.path()
                .join(".git")
                .join(SPKE_DIR)
                .join(OP_LOCK_FILE)
                .exists()
        );
    }

    #[test]
    fn second_acquire_fails_while_held() {
        let dir = tempdir().expect("tempdir");
        init_repo(dir.path());
        let _first = acquire(dir.path(), "first_op").expect("first acquire");
        match acquire(dir.path(), "second_op") {
            Err(RepoBusyError {
                reason: BusyReason::OtherOp(name),
            }) => assert_eq!(name, "first_op"),
            other => panic!("expected Busy(first_op), got {other:?}"),
        }
    }

    #[test]
    fn external_git_lock_blocks_acquire() {
        let dir = tempdir().expect("tempdir");
        init_repo(dir.path());
        std::fs::write(dir.path().join(".git").join(crate::INDEX_LOCK), "").expect("write");
        match acquire(dir.path(), "op") {
            Err(RepoBusyError {
                reason: BusyReason::ExternalGit,
            }) => {}
            other => panic!("expected ExternalGit, got {other:?}"),
        }
    }

    #[test]
    fn stale_lock_is_overwritten() {
        let dir = tempdir().expect("tempdir");
        init_repo(dir.path());
        let lock_path = dir.path().join(".git").join(SPKE_DIR).join(OP_LOCK_FILE);
        std::fs::create_dir_all(lock_path.parent().unwrap()).expect("mkdir");
        // PID 1 exists on every Unix system (init), so use a PID that's
        // virtually guaranteed not to exist (i32::MAX).
        std::fs::write(&lock_path, format!("ghost_op\n{}\n0\n", i32::MAX as u32))
            .expect("write fake lock");

        #[cfg(unix)]
        {
            let lock = acquire(dir.path(), "real_op").expect("acquire over stale lock");
            drop(lock);
        }
    }
}
