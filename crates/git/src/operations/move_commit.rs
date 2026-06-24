//! S-DST move-commit — relocate a single non-merge commit relative to
//! another commit in the current branch's history. Uses
//! [`super::rebase::RebaseTodoBuilder`].

use anyhow::{Result, anyhow, bail};
use std::path::Path;
use std::process::Command;

use super::rebase::{RebaseCallbacks, RebaseHandle, RebaseTodoBuilder, run_rebase_with_op_name};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeforeOrAfter {
    Before,
    After,
}

pub struct MoveCommitOp {
    pub source_sha: String,
    pub target_sha: String,
    pub position: BeforeOrAfter,
}

impl MoveCommitOp {
    pub async fn run(self, repo_path: &Path, callbacks: RebaseCallbacks) -> Result<RebaseHandle> {
        let MoveCommitOp {
            source_sha,
            target_sha,
            position,
        } = self;

        // Compute the merge-base of HEAD and (source^, target^), so the
        // rebase covers both commits' positions.
        let source_parent = rev_parse(repo_path, &format!("{source_sha}^"))?;
        let target_parent = rev_parse(repo_path, &format!("{target_sha}^"))?;
        let base_sha = merge_base(repo_path, &source_parent, &target_parent)?;
        let commits = list_commits_to_pick(repo_path, &base_sha)?;
        if commits.is_empty() {
            bail!("nothing to move: history is empty between {base_sha} and HEAD");
        }

        // Build new ordering: skip source_sha when iterating; insert it
        // before / after target_sha based on `position`.
        let mut reordered: Vec<String> = Vec::with_capacity(commits.len());
        let mut found_target = false;
        let mut found_source = false;
        for commit in &commits {
            if shas_equal(commit, &source_sha) {
                found_source = true;
                continue;
            }
            if shas_equal(commit, &target_sha) {
                found_target = true;
                if position == BeforeOrAfter::Before {
                    reordered.push(source_sha.clone());
                    reordered.push(commit.clone());
                } else {
                    reordered.push(commit.clone());
                    reordered.push(source_sha.clone());
                }
                continue;
            }
            reordered.push(commit.clone());
        }
        if !found_source {
            bail!("source commit {source_sha} not reachable from HEAD");
        }
        if !found_target {
            bail!("target commit {target_sha} not reachable from HEAD");
        }

        let mut builder = RebaseTodoBuilder::new();
        for commit in reordered {
            builder = builder.pick(commit);
        }
        let todo = builder.build();
        run_rebase_with_op_name(repo_path, &base_sha, todo, callbacks, "move_commit").await
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
fn merge_base(repo_path: &Path, a: &str, b: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["merge-base", a, b])
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git merge-base {a} {b} failed: {}",
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

fn shas_equal(a: &str, b: &str) -> bool {
    let min = a.len().min(b.len());
    if min == 0 {
        return false;
    }
    a[..min].eq_ignore_ascii_case(&b[..min])
}
