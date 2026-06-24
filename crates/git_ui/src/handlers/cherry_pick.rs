//! S-DST cherry-pick handler. Runs [`git::operations::cherry_pick::CherryPickOp`]
//! through `OpRunner` on a background task. On `PausedForConflict`, the
//! caller is expected to surface the conflict resolver — wired by
//! `git_graph::context_menu` after this returns.

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, Task};
use std::path::PathBuf;

use git::operations::OpRunner;
use git::operations::RunOutcome;
use git::operations::cherry_pick::CherryPickOp;

use crate::handlers::protection;

pub fn run(
    repo_path: PathBuf,
    shas: Vec<String>,
    no_commit: bool,
    mainline: Option<u32>,
    x: bool,
    cx: &mut App,
) -> Task<Result<RunOutcome>> {
    run_with_confirmation(repo_path, shas, no_commit, mainline, x, false, cx)
}

/// Variant that lets callers opt into S-SOL-PRT confirmation. When
/// `confirmed = true`, the handler skips the type-the-branch-name
/// gate but still respects `Forbidden` decisions.
pub fn run_with_confirmation(
    repo_path: PathBuf,
    shas: Vec<String>,
    no_commit: bool,
    mainline: Option<u32>,
    x: bool,
    confirmed: bool,
    cx: &mut App,
) -> Task<Result<RunOutcome>> {
    cx.background_spawn(async move {
        protection::enforce_current_branch(&repo_path, "cherry_pick", confirmed)
            .map_err(|e| anyhow!("branch protection: {e}"))?;
        OpRunner::run(
            CherryPickOp {
                shas,
                no_commit,
                mainline,
                x,
            },
            &repo_path,
        )
        .map_err(|err| anyhow!("cherry_pick failed: {err}"))
    })
}
