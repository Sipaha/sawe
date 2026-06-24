//! S-DST fixup handler.

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, Task};
use std::path::PathBuf;

use git::operations::fixup::FixupOp;
use git::operations::rebase::{RebaseCallbacks, RebaseHandle};

use crate::handlers::protection;

pub fn run(
    repo_path: PathBuf,
    shas: Vec<String>,
    callbacks: RebaseCallbacks,
    cx: &mut App,
) -> Task<Result<RebaseHandle>> {
    run_with_confirmation(repo_path, shas, callbacks, false, cx)
}

pub fn run_with_confirmation(
    repo_path: PathBuf,
    shas: Vec<String>,
    callbacks: RebaseCallbacks,
    confirmed: bool,
    cx: &mut App,
) -> Task<Result<RebaseHandle>> {
    cx.background_spawn(async move {
        protection::enforce_current_branch(&repo_path, "fixup", confirmed)
            .map_err(|e| anyhow!("branch protection: {e}"))?;
        FixupOp { shas }.run(&repo_path, callbacks).await
    })
}
