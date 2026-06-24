//! Backup-refs framework — `refs/spke/backup/<branch>/<timestamp>-<op>` is
//! created before every destructive operation so a misstep can be undone via
//! `editor.git.undo_last`.
//!
//! All operations shell out to the git binary (`git update-ref`,
//! `git for-each-ref`, `git rev-parse`) using the host's `git` on `$PATH`.
//! That matches the rest of the high-level S-BAK plumbing which runs on a
//! background task and doesn't need the bundled git lookup.

use anyhow::{Context as _, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;

const REF_PREFIX: &str = "refs/spke/backup";

/// One backup-ref entry. The reference itself lives at
/// `refs/spke/backup/<branch>/<timestamp>-<op>` and points at the tip of
/// `branch` immediately before the operation ran.
#[derive(Debug, Clone)]
pub struct BackupRef {
    pub repo_path: PathBuf,
    pub branch: String,
    pub op: String,
    pub timestamp_unix: i64,
    pub before_sha: String,
}

impl BackupRef {
    /// Full ref name (e.g. `refs/spke/backup/main/1700000000-cherry_pick`).
    pub fn ref_name(&self) -> String {
        format!(
            "{REF_PREFIX}/{}/{}-{}",
            sanitize_branch(&self.branch),
            self.timestamp_unix,
            self.op
        )
    }
}

/// Create a backup-ref for `branch` in `repo_path`. Returns the materialized
/// [`BackupRef`].
pub fn create(repo_path: &Path, branch: &str, op: &str) -> Result<BackupRef> {
    let before_sha = read_branch_tip(repo_path, branch)?;
    let timestamp_unix = current_unix_seconds();
    let backup = BackupRef {
        repo_path: repo_path.to_path_buf(),
        branch: branch.to_string(),
        op: op.to_string(),
        timestamp_unix,
        before_sha: before_sha.clone(),
    };
    let ref_name = backup.ref_name();
    run_git_void(repo_path, &["update-ref", &ref_name, &before_sha])
        .with_context(|| format!("creating backup ref {ref_name}"))?;
    Ok(backup)
}

/// List backup-refs in `repo_path`, optionally filtered to a single `branch`
/// or to entries newer than `since_unix` (inclusive lower bound).
pub fn list(
    repo_path: &Path,
    branch: Option<&str>,
    since_unix: Option<i64>,
) -> Result<Vec<BackupRef>> {
    let pattern = if let Some(branch) = branch {
        format!("{REF_PREFIX}/{}", sanitize_branch(branch))
    } else {
        REF_PREFIX.to_string()
    };
    let stdout = run_git_capture(
        repo_path,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            &pattern,
        ],
    )?;

    let mut entries = Vec::new();
    for line in stdout.lines() {
        let Some((sha, refname)) = line.split_once(' ') else {
            continue;
        };
        let Some(rest) = refname.strip_prefix(&format!("{REF_PREFIX}/")) else {
            continue;
        };
        let Some(slash) = rest.rfind('/') else {
            continue;
        };
        let raw_branch = &rest[..slash];
        let leaf = &rest[slash + 1..];
        let Some(dash) = leaf.find('-') else {
            continue;
        };
        let timestamp_part = &leaf[..dash];
        let op_part = &leaf[dash + 1..];
        let timestamp_unix: i64 = match timestamp_part.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(since) = since_unix {
            if timestamp_unix < since {
                continue;
            }
        }
        entries.push(BackupRef {
            repo_path: repo_path.to_path_buf(),
            branch: unsanitize_branch(raw_branch),
            op: op_part.to_string(),
            timestamp_unix,
            before_sha: sha.to_string(),
        });
    }
    entries.sort_by(|a, b| b.timestamp_unix.cmp(&a.timestamp_unix));
    Ok(entries)
}

/// Delete backup-refs older than `older_than_days` in `repo_path`. Returns
/// the count of removed refs.
pub fn cleanup(repo_path: &Path, older_than_days: u32) -> Result<usize> {
    let cutoff =
        current_unix_seconds().saturating_sub((older_than_days as i64).saturating_mul(86_400));
    let entries = list(repo_path, None, None)?;
    let mut removed = 0usize;
    for entry in entries {
        if entry.timestamp_unix >= cutoff {
            continue;
        }
        let ref_name = entry.ref_name();
        if let Err(err) = run_git_void(repo_path, &["update-ref", "-d", &ref_name]) {
            log::warn!("git::backup: failed to delete {ref_name}: {err}");
            continue;
        }
        removed += 1;
    }
    Ok(removed)
}

/// Read the current tip of `branch` in `repo_path`. Returns the full SHA.
pub fn read_branch_tip(repo_path: &Path, branch: &str) -> Result<String> {
    // `--verify` makes `git rev-parse` exit non-zero if the branch is unknown
    // instead of echoing the literal back.
    let stdout = run_git_capture(
        repo_path,
        &["rev-parse", "--verify", &format!("{branch}^{{commit}}")],
    )
    .with_context(|| format!("reading tip of branch {branch}"))?;
    Ok(stdout.trim().to_string())
}

/// Replace `/` in branch names with `__` so they fit cleanly inside a single
/// path segment of the backup-ref. `feature/foo` → `feature__foo`. The
/// reverse mapping in [`unsanitize_branch`] is best-effort — any pre-existing
/// `__` in the branch name is unrecoverable, but we never use the
/// reconstituted name to dispatch git operations, only for display.
fn sanitize_branch(branch: &str) -> String {
    branch.replace('/', "__")
}

fn unsanitize_branch(sanitized: &str) -> String {
    sanitized.replace("__", "/")
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Run `git <args>` as a blocking subprocess. `OpRunner` is itself
/// synchronous (runs on a background task) and the high-level callers
/// don't want to thread `async` through every backup helper, so this
/// uses blocking `std::process::Command`. Allowed-list opt-out is
/// scoped to this one helper.
#[allow(clippy::disallowed_methods)]
fn run_git_capture(repo_path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .with_context(|| format!("spawning `git {}`", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "`git {}` failed: {}",
            args.join(" "),
            stderr.trim()
        ));
    }
    String::from_utf8(output.stdout).context("non-utf8 git output")
}

fn run_git_void(repo_path: &Path, args: &[&str]) -> Result<()> {
    run_git_capture(repo_path, args).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
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
                "user.name=Test",
                "-c",
                "user.email=test@x",
                "commit",
                "-qm",
                "init",
            ],
        );
        dir
    }

    #[test]
    fn create_then_list_roundtrip() {
        let dir = init_repo();
        let entry = create(dir.path(), "main", "test_op").expect("create");
        assert_eq!(entry.op, "test_op");
        assert_eq!(entry.branch, "main");
        assert_eq!(entry.before_sha.len(), 40);

        let listed = list(dir.path(), None, None).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].branch, "main");
        assert_eq!(listed[0].op, "test_op");
        assert_eq!(listed[0].before_sha, entry.before_sha);
    }

    #[test]
    fn list_filters_by_since() {
        let dir = init_repo();
        let entry = create(dir.path(), "main", "op1").expect("create");
        let after = list(dir.path(), None, Some(entry.timestamp_unix + 10)).expect("list");
        assert!(after.is_empty(), "should be empty above cutoff");
        let before = list(dir.path(), None, Some(entry.timestamp_unix)).expect("list");
        assert_eq!(before.len(), 1);
    }

    #[test]
    fn cleanup_removes_old_entries() {
        let dir = init_repo();
        let _entry = create(dir.path(), "main", "old_op").expect("create");
        // Right after creation: 0 days old → won't be removed.
        let removed = cleanup(dir.path(), 0).expect("cleanup");
        // With `older_than_days = 0`, anything strictly older than now is
        // removed; the entry was made within the same second so its
        // timestamp == cutoff. Neither side is greater so it's kept.
        assert_eq!(removed, 0);
        let listed = list(dir.path(), None, None).expect("list");
        assert_eq!(listed.len(), 1);
    }

    #[test]
    fn sanitize_handles_slashes() {
        let dir = init_repo();
        // Set up a branch with a slash.
        git(dir.path(), &["checkout", "-q", "-b", "feature/x"]);
        let entry = create(dir.path(), "feature/x", "drop").expect("create");
        let listed = list(dir.path(), Some("feature/x"), None).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].branch, "feature/x");
        assert_eq!(listed[0].before_sha, entry.before_sha);
    }
}
