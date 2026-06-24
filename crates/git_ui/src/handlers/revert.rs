//! S-DST revert handler.

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, Task};
use std::path::PathBuf;

use git::operations::OpRunner;
use git::operations::RunOutcome;
use git::operations::revert::RevertOp;

use crate::handlers::protection;

pub fn run(
    repo_path: PathBuf,
    shas: Vec<String>,
    no_commit: bool,
    mainline: Option<u32>,
    cx: &mut App,
) -> Task<Result<RunOutcome>> {
    run_with_confirmation(repo_path, shas, no_commit, mainline, false, cx)
}

pub fn run_with_confirmation(
    repo_path: PathBuf,
    shas: Vec<String>,
    no_commit: bool,
    mainline: Option<u32>,
    confirmed: bool,
    cx: &mut App,
) -> Task<Result<RunOutcome>> {
    cx.background_spawn(async move {
        protection::enforce_current_branch(&repo_path, "revert", confirmed)
            .map_err(|e| anyhow!("branch protection: {e}"))?;
        OpRunner::run(
            RevertOp {
                shas,
                no_commit,
                mainline,
            },
            &repo_path,
        )
        .map_err(|err| anyhow!("revert failed: {err}"))
    })
}
