//! S-SOL-PRT — synchronous helpers for handlers that need to consult
//! the branch-protection policy before invoking an `OpRunner` /
//! `run_rebase_with_op_name` path.
//!
//! Handlers are pure functions returning `Task<Result<…>>` — they
//! don't have a `Window` and can't show modals. The contract is:
//!
//! - `Allowed` → return `Ok(())`, caller proceeds normally.
//! - `Forbidden { reason }` → return `Err(BranchProtectionError::Forbidden)`.
//!   Callers surface the reason via the existing
//!   `detach_and_prompt_err` toast / notification path.
//! - `RequiresConfirmation { reason }` → if the caller already
//!   confirmed via its own modal, it passes `confirmed: true` and the
//!   helper returns `Ok(())`; otherwise the helper returns
//!   `Err(BranchProtectionError::RequiresConfirmation)` so the caller
//!   can drive the type-the-branch-name modal and re-invoke the
//!   handler with `confirmed: true` once the user typed it correctly.
//!
//! `current_branch` reads the on-disk HEAD synchronously via
//! `git symbolic-ref HEAD` — used by ops that don't surface their
//! target branch in the input (reset / drop / squash / fixup / cherry-pick / revert).

use std::path::Path;
use std::process::Command;

use anyhow::{Result, anyhow};

/// Failure produced by [`enforce`] when the current policy refuses an
/// op. Callers pattern-match the variant — `Forbidden` is a hard stop;
/// `RequiresConfirmation` indicates the UI should drive a confirm
/// modal and re-invoke the handler with `confirmed = true`.
#[derive(Debug, Clone)]
pub enum BranchProtectionError {
    Forbidden { reason: String },
    RequiresConfirmation { reason: String },
}

impl std::fmt::Display for BranchProtectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forbidden { reason } => write!(f, "{reason}"),
            Self::RequiresConfirmation { reason } => {
                write!(f, "requires confirmation: {reason}")
            }
        }
    }
}

impl std::error::Error for BranchProtectionError {}

/// Run the branch-protection check and turn the result into a
/// `Result<()>`. `confirmed = true` short-circuits
/// `RequiresConfirmation` to `Ok(())`, mirroring the subagent
/// payload-flag pattern used by destructive MCP tools.
pub fn enforce(
    repo_path: &Path,
    branch: &str,
    op: &str,
    confirmed: bool,
) -> std::result::Result<(), BranchProtectionError> {
    use solutions::branch_protection::Decision;
    match solutions::branch_protection::check(repo_path, branch, op) {
        Decision::Allowed => Ok(()),
        Decision::Forbidden { reason } => Err(BranchProtectionError::Forbidden { reason }),
        Decision::RequiresConfirmation { reason } => {
            if confirmed {
                Ok(())
            } else {
                Err(BranchProtectionError::RequiresConfirmation { reason })
            }
        }
    }
}

/// Synchronous read of the current HEAD branch via `git symbolic-ref
/// HEAD`. Returns the bare branch name (no `refs/heads/` prefix).
/// Errors when HEAD is detached or git invocation fails — callers
/// generally treat that as "no branch to protect" and proceed.
#[allow(clippy::disallowed_methods)]
pub fn current_branch(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .map_err(|err| anyhow!("spawn git symbolic-ref: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git symbolic-ref --short HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        return Err(anyhow!("git symbolic-ref returned empty"));
    }
    Ok(branch)
}

/// Convenience wrapper: read the current branch and enforce the
/// policy. Skips the check when HEAD is detached (no branch to
/// protect) — protected branches are by name, not by sha.
pub fn enforce_current_branch(
    repo_path: &Path,
    op: &str,
    confirmed: bool,
) -> std::result::Result<(), BranchProtectionError> {
    let Ok(branch) = current_branch(repo_path) else {
        return Ok(());
    };
    enforce(repo_path, &branch, op, confirmed)
}
