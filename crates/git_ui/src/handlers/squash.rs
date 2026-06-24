//! S-DST squash handler.

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, Task};
use std::path::PathBuf;

use git::operations::rebase::{RebaseCallbacks, RebaseHandle};
use git::operations::squash::SquashOp;

use crate::handlers::protection;

pub fn run(
    repo_path: PathBuf,
    shas: Vec<String>,
    final_message: String,
    callbacks: RebaseCallbacks,
    cx: &mut App,
) -> Task<Result<RebaseHandle>> {
    run_with_confirmation(repo_path, shas, final_message, callbacks, false, cx)
}

pub fn run_with_confirmation(
    repo_path: PathBuf,
    shas: Vec<String>,
    final_message: String,
    callbacks: RebaseCallbacks,
    confirmed: bool,
    cx: &mut App,
) -> Task<Result<RebaseHandle>> {
    cx.background_spawn(async move {
        protection::enforce_current_branch(&repo_path, "squash", confirmed)
            .map_err(|e| anyhow!("branch protection: {e}"))?;
        SquashOp {
            shas,
            final_message,
        }
        .run(&repo_path, callbacks)
        .await
    })
}
