//! S-DST squash — fold N adjacent commits into a single commit with a
//! user-supplied final message. Uses [`super::rebase::RebaseTodoBuilder`]
//! to compose `pick (oldest)` + `squash <each-other>` + `reword <head>
//! <final_message>`.
//!
//! `shas` must be supplied in topological order, oldest first, AND the
//! commits must be contiguous in the current branch's history. The op
//! verifies contiguity by comparing them against `git rev-list
//! --reverse <oldest-sha>^..HEAD`.

use anyhow::{Result, anyhow, bail};
use std::path::Path;
use std::process::Command;

use super::rebase::{RebaseCallbacks, RebaseHandle, RebaseTodoBuilder, run_rebase_with_op_name};

pub struct SquashOp {
    pub shas: Vec<String>,
    pub final_message: String,
}

impl SquashOp {
    pub async fn run(self, repo_path: &Path, callbacks: RebaseCallbacks) -> Result<RebaseHandle> {
        let SquashOp {
            shas,
            final_message,
        } = self;
        if shas.is_empty() {
            bail!("squash: no commits supplied");
        }
        let oldest = shas
            .first()
            .ok_or_else(|| anyhow!("squash: no commits supplied"))?
            .clone();
        let parent = format!("{oldest}^");
        let base_sha = rev_parse(repo_path, &parent)?;
        let history = list_commits_to_pick(repo_path, &base_sha)?;

        // Strategy: reword the *first* selected commit (pick + exec that
        // rewrites the message to `final_message`), then fixup each
        // remaining selected commit. `fixup` folds content but keeps the
        // current tip's message, so the rewritten message survives all the
        // way to the final combined commit.
        //
        // The previous version used `reword` on the *last* commit and
        // `squash` for the middle, but for the common 2-commit case
        // [A, B] the last-sha branch fired *instead of* squash and the
        // commits were never folded — log ended up with two commits, the
        // newer one just having a renamed subject.
        let mut builder = RebaseTodoBuilder::new();
        let mut shas_iter = shas.iter();
        let first = shas_iter
            .next()
            .ok_or_else(|| anyhow!("squash: no commits supplied"))?;
        let mut squash_targets: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for s in shas_iter {
            squash_targets.insert(s.to_lowercase());
        }

        let mut emitted_first = false;
        let mut squashes_emitted = 0usize;
        for commit in history {
            if !emitted_first {
                if shas_equal(&commit, first) {
                    builder = builder.reword(commit.clone(), final_message.clone());
                    emitted_first = true;
                } else {
                    // History before the oldest squash target is part of
                    // base; nothing to do (rev-list excludes anything <= base).
                    builder = builder.pick(commit);
                }
                continue;
            }
            if squash_targets.iter().any(|s| shas_equal(&commit, s)) {
                builder = builder.fixup(commit);
                squashes_emitted += 1;
            } else {
                // A commit between squash targets that wasn't selected:
                // contiguity violation.
                bail!(
                    "squash targets are not contiguous: commit {} sits between selected commits",
                    commit
                );
            }
        }
        if !emitted_first {
            bail!("squash: oldest selected commit not found in history");
        }
        if squashes_emitted != squash_targets.len() {
            bail!(
                "squash: only {} of {} extra commits found in history (contiguity violation)",
                squashes_emitted,
                squash_targets.len()
            );
        }
        let todo = builder.build();
        run_rebase_with_op_name(repo_path, &base_sha, todo, callbacks, "squash").await
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
