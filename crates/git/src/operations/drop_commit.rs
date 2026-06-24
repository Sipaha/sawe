//! S-DST drop — `git rebase -i <merge-base>` with `<sha>` marked `drop`
//! and the rest `pick`. Uses [`super::rebase::RebaseTodoBuilder`]; the
//! conflict-pause / exec-failure handling lives in
//! [`super::rebase::run_rebase_with_op_name`].
//!
//! Why not a vanilla [`super::AtomicGitOp`]: `run_rebase` is async and
//! acquires the per-repo lock itself, so wrapping it in
//! [`super::OpRunner`] would deadlock. UI callers invoke
//! [`run_drop`] directly; backup-ref + undo registration happen inside
//! `run_rebase_with_op_name`.

use anyhow::{Context as _, Result, anyhow, bail};
use std::path::Path;
use std::process::Command;

use super::rebase::{RebaseCallbacks, RebaseHandle, RebaseTodoBuilder, run_rebase_with_op_name};

/// Drop a single non-merge commit from the current branch's history.
/// `sha` must be reachable from HEAD.
pub async fn run_drop(
    repo_path: &Path,
    sha: &str,
    callbacks: RebaseCallbacks,
) -> Result<RebaseHandle> {
    if is_merge_commit(repo_path, sha)? {
        bail!(
            "cannot drop merge commit {sha}: linear rebase cannot preserve the second parent. \
             Use Revert with --mainline N instead."
        );
    }
    let parent = format!("{sha}^");
    let base_sha =
        rev_parse(repo_path, &parent).with_context(|| format!("resolving parent of {sha}"))?;
    let commits = list_commits_to_pick(repo_path, &base_sha)?;
    if commits.is_empty() {
        bail!("nothing to drop: HEAD has no commits since {base_sha}");
    }

    let mut builder = RebaseTodoBuilder::new();
    let mut found = false;
    for commit in commits {
        if shas_equal(&commit, sha) {
            builder = builder.drop(commit);
            found = true;
        } else {
            builder = builder.pick(commit);
        }
    }
    if !found {
        bail!("commit {sha} is not reachable from HEAD; cannot drop");
    }
    let todo = builder.build();
    run_rebase_with_op_name(repo_path, &base_sha, todo, callbacks, "drop").await
}

#[allow(clippy::disallowed_methods)]
fn rev_parse(repo_path: &Path, rev: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "--verify", rev])
        .output()
        .map_err(|err| anyhow!("spawn git rev-parse: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git rev-parse {} failed: {}",
            rev,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[allow(clippy::disallowed_methods)]
fn list_commits_to_pick(repo_path: &Path, base: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-list", "--reverse", &format!("{base}..HEAD")])
        .output()
        .map_err(|err| anyhow!("spawn git rev-list: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git rev-list {base}..HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

#[allow(clippy::disallowed_methods)]
fn is_merge_commit(repo_path: &Path, sha: &str) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-list", "--no-walk", "--parents", sha])
        .output()
        .map_err(|err| anyhow!("spawn git rev-list --parents: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git rev-list --parents {sha} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let parts: Vec<&str> = line.split_ascii_whitespace().collect();
    Ok(parts.len() > 2)
}

fn shas_equal(a: &str, b: &str) -> bool {
    let min = a.len().min(b.len());
    if min == 0 {
        return false;
    }
    a[..min].eq_ignore_ascii_case(&b[..min])
}
