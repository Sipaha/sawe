//! S-DST merge handler.

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, Task};
use std::path::PathBuf;

use git::operations::OpRunner;
use git::operations::RunOutcome;
use git::operations::merge::MergeOp;

use crate::handlers::protection;

pub fn run(
    repo_path: PathBuf,
    target_branch: String,
    no_ff: bool,
    squash: bool,
    message: Option<String>,
    cx: &mut App,
) -> Task<Result<RunOutcome>> {
    run_with_confirmation(repo_path, target_branch, no_ff, squash, message, false, cx)
}

pub fn run_with_confirmation(
    repo_path: PathBuf,
    target_branch: String,
    no_ff: bool,
    squash: bool,
    message: Option<String>,
    confirmed: bool,
    cx: &mut App,
) -> Task<Result<RunOutcome>> {
    cx.background_spawn(async move {
        // Merge writes the *current* branch (the merge commit lands
        // there), so the policy decision keys off it — matching the
        // spec's "commit / merge / cherry-pick / revert" rule.
        protection::enforce_current_branch(&repo_path, "merge", confirmed)
            .map_err(|e| anyhow!("branch protection: {e}"))?;
        OpRunner::run(
            MergeOp {
                target_branch,
                no_ff,
                squash,
                message,
            },
            &repo_path,
        )
        .map_err(|err| anyhow!("merge failed: {err}"))
    })
}
