//! S-DST drop-commit handler.

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, Task};
use std::path::PathBuf;

use git::operations::drop_commit::run_drop;
use git::operations::rebase::{RebaseCallbacks, RebaseHandle};

use crate::handlers::protection;

pub fn run(
    repo_path: PathBuf,
    sha: String,
    callbacks: RebaseCallbacks,
    cx: &mut App,
) -> Task<Result<RebaseHandle>> {
    run_with_confirmation(repo_path, sha, callbacks, false, cx)
}

pub fn run_with_confirmation(
    repo_path: PathBuf,
    sha: String,
    callbacks: RebaseCallbacks,
    confirmed: bool,
    cx: &mut App,
) -> Task<Result<RebaseHandle>> {
    cx.background_spawn(async move {
        protection::enforce_current_branch(&repo_path, "drop_commit", confirmed)
            .map_err(|e| anyhow!("branch protection: {e}"))?;
        run_drop(&repo_path, &sha, callbacks).await
    })
}
