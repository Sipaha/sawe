//! S-DST reword — rewrite the message of a commit reachable from HEAD
//! via interactive rebase. Uses
//! [`super::rebase::RebaseTodoBuilder::reword`] which translates to
//! `pick <sha>` + `exec <helper> <token>` — message rewriting happens
//! in the helper subcommand registered in
//! [`super::helpers::run_message_set`].

use anyhow::{Result, anyhow, bail};
use std::path::Path;
use std::process::Command;

use super::rebase::{RebaseCallbacks, RebaseHandle, RebaseTodoBuilder, run_rebase_with_op_name};

pub struct RewordOp {
    pub sha: String,
    pub new_message: String,
}

impl RewordOp {
    pub async fn run(self, repo_path: &Path, callbacks: RebaseCallbacks) -> Result<RebaseHandle> {
        let RewordOp { sha, new_message } = self;
        if is_merge_commit(repo_path, &sha)? {
            bail!("reword of merge commit {sha} requires --rebase-merges; not yet supported");
        }
        let parent = format!("{sha}^");
        let base_sha = rev_parse(repo_path, &parent)?;
        let commits = list_commits_to_pick(repo_path, &base_sha)?;
        if commits.is_empty() {
            bail!("nothing to reword: HEAD has no commits since {base_sha}");
        }

        let mut builder = RebaseTodoBuilder::new();
        let mut rewrote = false;
        for commit in commits {
            if !rewrote && shas_equal(&commit, &sha) {
                builder = builder.reword(commit, new_message.clone());
                rewrote = true;
            } else {
                builder = builder.pick(commit);
            }
        }
        if !rewrote {
            bail!("commit {sha} not reachable from HEAD; cannot reword");
        }
        let todo = builder.build();
        run_rebase_with_op_name(repo_path, &base_sha, todo, callbacks, "reword").await
    }
}

#[allow(clippy::disallowed_methods)]
fn rev_parse(repo_path: &Path, rev: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "--verify", rev])
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git rev-parse {rev} failed: {}",
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
        .map_err(|err| anyhow!("spawn git: {err}"))?;
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
        .map_err(|err| anyhow!("spawn git: {err}"))?;
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
