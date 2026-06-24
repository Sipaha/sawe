//! S-DST edit-message handler. Splits the HEAD-amend path (sync) from
//! the past-commit path (async via rebase).

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, Task};
use std::path::PathBuf;

use git::operations::edit_commit_message::{EditMessageOp, EditMessageOutcome};
use git::operations::rebase::RebaseCallbacks;

use crate::handlers::protection;

pub fn run(
    repo_path: PathBuf,
    sha: String,
    new_message: String,
    callbacks: RebaseCallbacks,
    cx: &mut App,
) -> Task<Result<EditMessageOutcome>> {
    run_with_confirmation(repo_path, sha, new_message, callbacks, false, cx)
}

pub fn run_with_confirmation(
    repo_path: PathBuf,
    sha: String,
    new_message: String,
    callbacks: RebaseCallbacks,
    confirmed: bool,
    cx: &mut App,
) -> Task<Result<EditMessageOutcome>> {
    cx.background_spawn(async move {
        protection::enforce_current_branch(&repo_path, "edit_commit_message", confirmed)
            .map_err(|e| anyhow!("branch protection: {e}"))?;
        EditMessageOp { sha, new_message }
            .run(&repo_path, callbacks)
            .await
    })
}
