//! S-DST linear rebase handler ("Rebase Current onto Branch…").

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, Task};
use std::path::PathBuf;

use git::operations::OpRunner;
use git::operations::RunOutcome;
use git::operations::linear_rebase::LinearRebaseOp;

use crate::handlers::protection;

pub fn run(
    repo_path: PathBuf,
    target_branch: String,
    autostash: bool,
    cx: &mut App,
) -> Task<Result<RunOutcome>> {
    run_with_confirmation(repo_path, target_branch, autostash, false, cx)
}

pub fn run_with_confirmation(
    repo_path: PathBuf,
    target_branch: String,
    autostash: bool,
    confirmed: bool,
    cx: &mut App,
) -> Task<Result<RunOutcome>> {
    cx.background_spawn(async move {
        // Rebase rewrites the *current* branch; key the policy off
        // that, not the upstream we're rebasing onto.
        protection::enforce_current_branch(&repo_path, "rebase", confirmed)
            .map_err(|e| anyhow!("branch protection: {e}"))?;
        OpRunner::run(
            LinearRebaseOp {
                target_branch,
                autostash,
            },
            &repo_path,
        )
        .map_err(|err| anyhow!("rebase failed: {err}"))
    })
}
