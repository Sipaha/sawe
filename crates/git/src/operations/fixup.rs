//! S-DST fixup — like [`super::squash`] but uses `fixup` verb instead
//! of `squash`. Drops the messages of the squashed commits; the final
//! commit keeps the message of the oldest pick.

use anyhow::{Result, anyhow, bail};
use std::path::Path;
use std::process::Command;

use super::rebase::{RebaseCallbacks, RebaseHandle, RebaseTodoBuilder, run_rebase_with_op_name};

pub struct FixupOp {
    pub shas: Vec<String>,
}

impl FixupOp {
    pub async fn run(self, repo_path: &Path, callbacks: RebaseCallbacks) -> Result<RebaseHandle> {
        let FixupOp { shas } = self;
        if shas.is_empty() {
            bail!("fixup: no commits supplied");
        }
        let oldest = shas
            .first()
            .ok_or_else(|| anyhow!("fixup: no commits supplied"))?
            .clone();
        let parent = format!("{oldest}^");
        let base_sha = rev_parse(repo_path, &parent)?;
        let history = list_commits_to_pick(repo_path, &base_sha)?;

        let mut builder = RebaseTodoBuilder::new();
        let first = shas
            .first()
            .ok_or_else(|| anyhow!("fixup: no commits"))?
            .clone();
        let mut squash_targets: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for s in shas.iter().skip(1) {
            squash_targets.insert(s.to_lowercase());
        }

        let mut emitted_first = false;
        let mut emitted_any_target = false;
        for commit in history {
            if !emitted_first && shas_equal(&commit, &first) {
                builder = builder.pick(commit.clone());
                emitted_first = true;
                continue;
            }
            if !emitted_first {
                builder = builder.pick(commit);
                continue;
            }
            if squash_targets.iter().any(|s| shas_equal(&commit, s)) {
                builder = builder.fixup(commit);
                emitted_any_target = true;
            } else if shas.iter().skip(1).any(|s| shas_equal(&commit, s)) {
                // already covered above; redundancy guard
                builder = builder.fixup(commit);
                emitted_any_target = true;
            } else if shas
                .iter()
                .skip(1)
                .any(|s| s.to_lowercase() == commit.to_lowercase())
            {
                builder = builder.fixup(commit);
                emitted_any_target = true;
            } else {
                bail!(
                    "fixup targets are not contiguous: commit {} sits between selected commits",
                    commit
                );
            }
        }
        if shas.len() > 1 && !emitted_any_target {
            bail!("fixup: not all selected commits found in history");
        }
        let todo = builder.build();
        run_rebase_with_op_name(repo_path, &base_sha, todo, callbacks, "fixup").await
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

fn shas_equal(a: &str, b: &str) -> bool {
    let min = a.len().min(b.len());
    if min == 0 {
        return false;
    }
    a[..min].eq_ignore_ascii_case(&b[..min])
}
