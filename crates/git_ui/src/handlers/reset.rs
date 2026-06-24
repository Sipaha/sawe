//! S-DST reset handler.

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, Task};
use std::path::PathBuf;

use git::operations::OpRunner;
use git::operations::RunOutcome;
use git::operations::reset::{ResetMode, ResetOp};

use crate::handlers::protection;

pub fn run(
    repo_path: PathBuf,
    sha: String,
    mode: ResetMode,
    cx: &mut App,
) -> Task<Result<RunOutcome>> {
    run_with_confirmation(repo_path, sha, mode, false, cx)
}

pub fn run_with_confirmation(
    repo_path: PathBuf,
    sha: String,
    mode: ResetMode,
    confirmed: bool,
    cx: &mut App,
) -> Task<Result<RunOutcome>> {
    cx.background_spawn(async move {
        // `reset --hard` is the lossy variant — gate behind the harder
        // `reset_hard` op key so a member can `no_force_reset = true`
        // without blocking soft / mixed resets entirely.
        let op = match mode {
            ResetMode::Hard => "reset_hard",
            _ => "reset",
        };
        protection::enforce_current_branch(&repo_path, op, confirmed)
            .map_err(|e| anyhow!("branch protection: {e}"))?;
        OpRunner::run(ResetOp { sha, mode }, &repo_path)
            .map_err(|err| anyhow!("reset failed: {err}"))
    })
}
