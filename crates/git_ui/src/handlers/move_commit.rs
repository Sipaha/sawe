//! S-DST move-commit handler.

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, Task};
use std::path::PathBuf;

use git::operations::move_commit::{BeforeOrAfter, MoveCommitOp};
use git::operations::rebase::{RebaseCallbacks, RebaseHandle};

use crate::handlers::protection;

pub fn run(
    repo_path: PathBuf,
    source_sha: String,
    target_sha: String,
    position: BeforeOrAfter,
    callbacks: RebaseCallbacks,
    cx: &mut App,
) -> Task<Result<RebaseHandle>> {
    run_with_confirmation(
        repo_path, source_sha, target_sha, position, callbacks, false, cx,
    )
}

pub fn run_with_confirmation(
    repo_path: PathBuf,
    source_sha: String,
    target_sha: String,
    position: BeforeOrAfter,
    callbacks: RebaseCallbacks,
    confirmed: bool,
    cx: &mut App,
) -> Task<Result<RebaseHandle>> {
    cx.background_spawn(async move {
        protection::enforce_current_branch(&repo_path, "move_commit", confirmed)
            .map_err(|e| anyhow!("branch protection: {e}"))?;
        MoveCommitOp {
            source_sha,
            target_sha,
            position,
        }
        .run(&repo_path, callbacks)
        .await
    })
}
