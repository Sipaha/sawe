//! Shared helpers for non-rebase atomic git operations
//! (cherry-pick / revert / reset / merge / linear-rebase). Each impl
//! shells out via these helpers so the conflict-pause / output-parsing
//! logic is centralised.

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::RunOutcome;

/// Returns the current branch tip's short ref name. `None` if HEAD is
/// detached. Used by atomic-op impls to compute `affected_branches` /
/// `affects_branch`.
pub fn current_branch(repo_path: &Path) -> Option<String> {
    let output = run_git(repo_path, &["symbolic-ref", "--short", "-q", "HEAD"]).ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        None
    } else {
        Some(stdout)
    }
}

/// Run `git <args>` as a blocking subprocess capturing stdout+stderr.
#[allow(clippy::disallowed_methods)]
pub fn run_git(repo_path: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))
}

/// Run `git <args>` with extra env vars (used by merge/cherry-pick to
/// suppress GIT_EDITOR).
#[allow(clippy::disallowed_methods)]
pub fn run_git_with_envs(
    repo_path: &Path,
    args: &[&str],
    envs: &HashMap<String, String>,
) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .envs(envs)
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))
}

/// `git status --porcelain` parsed for unmerged paths. Used to detect
/// conflict-pause states for cherry-pick / revert / merge / rebase.
pub fn list_conflicted_paths(repo_path: &Path) -> Result<Vec<PathBuf>> {
    let output = run_git(repo_path, &["status", "--porcelain"])?;
    if !output.status.success() {
        return Err(anyhow!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let body = String::from_utf8_lossy(&output.stdout);
    let mut paths = Vec::new();
    for line in body.lines() {
        if line.len() < 4 {
            continue;
        }
        let xy = &line[..2];
        let bytes = xy.as_bytes();
        let conflict = matches!(xy, "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU")
            || bytes[0] == b'U'
            || bytes[1] == b'U';
        if conflict {
            paths.push(PathBuf::from(line[3..].trim()));
        }
    }
    Ok(paths)
}

/// Resolves the literal `.git` directory. Handles the `gitdir:` redirection
/// used by submodules and worktrees.
pub fn dot_git_dir(repo_path: &Path) -> Result<PathBuf> {
    let candidate = repo_path.join(crate::DOT_GIT);
    let metadata = std::fs::metadata(&candidate)
        .map_err(|err| anyhow!("stat {}: {err}", candidate.display()))?;
    if metadata.is_dir() {
        return Ok(candidate);
    }
    let body = std::fs::read_to_string(&candidate)
        .map_err(|err| anyhow!("read {}: {err}", candidate.display()))?;
    let target = body
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))
        .ok_or_else(|| anyhow!("no gitdir: line in {}", candidate.display()))?;
    let path = PathBuf::from(target);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(repo_path.join(path))
    }
}

/// Did the op leave behind a CHERRY_PICK_HEAD / REVERT_HEAD / MERGE_HEAD
/// / rebase-* directory? If so combined with conflicts → PausedForConflict.
pub fn op_in_progress(repo_path: &Path, marker: &str) -> Result<bool> {
    let dot_git = dot_git_dir(repo_path)?;
    Ok(dot_git.join(marker).exists())
}

/// Wrap a direct git invocation in the [`RunOutcome`] vocabulary used by
/// pausable ops. If git exited 0 → Completed. If git exited non-zero
/// AND the working tree shows unmerged paths → PausedForConflict.
/// Otherwise the original error is propagated.
pub fn outcome_from_output(repo_path: &Path, args: &[&str], output: &Output) -> Result<RunOutcome> {
    if output.status.success() {
        return Ok(RunOutcome::Completed);
    }
    let conflicts = list_conflicted_paths(repo_path).unwrap_or_default();
    if !conflicts.is_empty() {
        return Ok(RunOutcome::PausedForConflict {
            conflicted_files: conflicts,
        });
    }
    Err(anyhow!(
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Default env block for non-interactive git runs. Pinning GIT_EDITOR /
/// GIT_SEQUENCE_EDITOR to `true` prevents a stray git command from
/// blocking on a terminal-only editor prompt.
pub fn no_editor_envs() -> HashMap<String, String> {
    let mut envs = HashMap::new();
    envs.insert("GIT_EDITOR".into(), "true".into());
    envs.insert("GIT_SEQUENCE_EDITOR".into(), "true".into());
    envs
}
