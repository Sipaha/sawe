//! MCP tools for the S-BAK backup-refs framework — listing recent backups,
//! restoring from one, and pruning old entries. Tier-classed so subagents
//! over the `--nc` bridge can read backups without a destructive cap but
//! must opt in (or be invoked by the user themselves) to actually undo or
//! prune.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use editor_mcp::{ToolTier, register_typed_tool_with_tier};
use git::{backup, undo_registry};
use gpui::{App, AsyncApp};
use project::git_store::RepositoryId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub(crate) fn register(cx: &mut App) {
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, ListBackupsTool);
    register_typed_tool_with_tier(cx, ToolTier::Destructive, UndoLastTool);
    register_typed_tool_with_tier(cx, ToolTier::Destructive, CleanupBackupsTool);
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.list_backups`. Lists S-BAK backup-refs and undo-registry
/// entries for the active repository, optionally filtered by branch and time.
pub struct ListBackupsInput {
    /// Restrict to a single branch name. `None` = all branches.
    pub branch: Option<String>,
    /// Inclusive lower bound on entry timestamp (Unix seconds). `None` = no
    /// lower bound.
    pub since_unix: Option<i64>,
    /// Repository to query. Omit to use the focused window's active repo.
    pub repo_id: Option<u64>,
}

/// Output of the list backups tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListBackupsOutput {
    pub backups: Vec<BackupEntry>,
    pub undo_entries: Vec<UndoEntryView>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BackupEntry {
    pub branch: String,
    pub op: String,
    pub timestamp_unix: i64,
    pub before_sha: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UndoEntryView {
    pub id: u64,
    pub op: String,
    pub branch: String,
    pub timestamp_unix: i64,
    pub before_sha: String,
    pub after_sha: Option<String>,
    pub failed: bool,
}

#[derive(Clone)]
pub struct ListBackupsTool;

impl McpServerTool for ListBackupsTool {
    type Input = ListBackupsInput;
    type Output = ListBackupsOutput;
    const NAME: &'static str = "editor.git.list_backups";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let branch_ref = input.branch.as_deref();
        let backups = backup::list(&work_dir, branch_ref, input.since_unix)?;
        let undo_entries = undo_registry::list(input.since_unix.unwrap_or(0))
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| {
                if entry.repo_path.as_path() != work_dir.as_ref() {
                    return false;
                }
                if let Some(want) = branch_ref {
                    if entry.branch != want {
                        return false;
                    }
                }
                true
            })
            .collect::<Vec<_>>();

        let summary = format!(
            "{} backup-ref(s), {} undo entries",
            backups.len(),
            undo_entries.len()
        );

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: ListBackupsOutput {
                backups: backups
                    .into_iter()
                    .map(|b| BackupEntry {
                        branch: b.branch,
                        op: b.op,
                        timestamp_unix: b.timestamp_unix,
                        before_sha: b.before_sha,
                    })
                    .collect(),
                undo_entries: undo_entries
                    .into_iter()
                    .map(|e| UndoEntryView {
                        id: e.id,
                        op: e.op,
                        branch: e.branch,
                        timestamp_unix: e.timestamp_unix,
                        before_sha: e.before_sha,
                        after_sha: e.after_sha,
                        failed: e.failed,
                    })
                    .collect(),
            },
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.undo_last`. Creates a new ref pointing at the
/// pre-op SHA recorded for `entry_id`, leaving the live branch untouched —
/// the user resolves the divergence (merge, reset, drop) interactively.
pub struct UndoLastInput {
    /// Identifier from a prior `editor.git.list_backups` call.
    pub entry_id: u64,
    /// Repository the undo-entry belongs to. When omitted the active repo is
    /// used; if the entry's recorded `repo_path` doesn't match, the call
    /// errors out.
    pub repo_id: Option<u64>,
}

/// Output of the undo last tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UndoLastOutput {
    pub created_ref: String,
    pub before_sha: String,
    pub branch: String,
}

#[derive(Clone)]
pub struct UndoLastTool;

impl McpServerTool for UndoLastTool {
    type Input = UndoLastInput;
    type Output = UndoLastOutput;
    const NAME: &'static str = "editor.git.undo_last";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let entries = undo_registry::list(0)?;
        let entry = entries
            .into_iter()
            .find(|e| e.id == input.entry_id)
            .ok_or_else(|| anyhow!("undo entry id {} not found", input.entry_id))?;
        if entry.repo_path.as_path() != work_dir.as_ref() {
            return Err(anyhow!(
                "undo entry {} belongs to a different repo ({}); pass matching repo_id",
                input.entry_id,
                entry.repo_path.display()
            ));
        }

        let created = create_restore_ref(&work_dir, &entry.branch, &entry.before_sha)?;
        let summary = format!(
            "created {} pointing at {}",
            created,
            short_sha(&entry.before_sha)
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: UndoLastOutput {
                created_ref: created,
                before_sha: entry.before_sha,
                branch: entry.branch,
            },
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.cleanup_backups`. Removes sawe backup-refs
/// older than `older_than_days`. The undo-registry entries themselves are
/// not pruned — they have independent retention.
pub struct CleanupBackupsInput {
    /// Default 30 if omitted.
    pub older_than_days: Option<u32>,
    pub repo_id: Option<u64>,
}

/// Output of the cleanup backups tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CleanupBackupsOutput {
    pub removed_count: usize,
}

#[derive(Clone)]
pub struct CleanupBackupsTool;

impl McpServerTool for CleanupBackupsTool {
    type Input = CleanupBackupsInput;
    type Output = CleanupBackupsOutput;
    const NAME: &'static str = "editor.git.cleanup_backups";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let days = input.older_than_days.unwrap_or(30);
        let removed = backup::cleanup(&work_dir, days)?;
        let summary = format!("removed {removed} backup ref(s) older than {days} day(s)");
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: CleanupBackupsOutput {
                removed_count: removed,
            },
        })
    }
}

/// Create `refs/spke/restore/<branch>/<n>` pointing at `before_sha`. The
/// numeric suffix increments to avoid colliding with prior restores. The
/// underlying ref creation is non-destructive (no existing branch is
/// rewritten), so we don't go through [`git::operations::OpRunner`].
pub(crate) fn create_restore_ref(
    repo_path: &Path,
    branch: &str,
    before_sha: &str,
) -> Result<String> {
    let sanitized = branch.replace('/', "__");
    let prefix = format!("refs/spke/restore/{sanitized}");
    let existing = run_git(repo_path, &["for-each-ref", "--format=%(refname)", &prefix])?;
    let next_n = existing
        .lines()
        .filter_map(|line| line.rsplit('/').next().and_then(|n| n.parse::<u32>().ok()))
        .max()
        .map(|n| n + 1)
        .unwrap_or(1);
    let ref_name = format!("{prefix}/{next_n}");
    run_git_void(repo_path, &["update-ref", &ref_name, before_sha])?;
    Ok(ref_name)
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(SHORT_SHA_LENGTH_FALLBACK).collect()
}

/// Sync `git` invocation. Restore-ref creation is a non-destructive op that
/// doesn't go through [`git::operations::OpRunner`], so it can run on the
/// caller's task without async hops. Allowed-list opt-out scoped to this
/// helper.
#[allow(clippy::disallowed_methods)]
fn run_git(repo_path: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn run_git_void(repo_path: &Path, args: &[&str]) -> Result<()> {
    run_git(repo_path, args).map(|_| ())
}

fn resolve_work_directory(repo_id: Option<RepositoryId>, cx: &mut App) -> Result<Arc<Path>> {
    let active_window_id = cx.active_window().map(|h| h.window_id());

    if let Some(want) = repo_id {
        for handle in cx.windows() {
            let Some(multi) = handle.downcast::<workspace::MultiWorkspace>() else {
                continue;
            };
            let found = multi
                .update(cx, |multi, _window, cx| {
                    for ws in multi.workspaces() {
                        let project = ws.read(cx).project();
                        let git_store = project.read(cx).git_store().clone();
                        let repo = git_store.read(cx).repositories().get(&want).cloned();
                        if let Some(repo) = repo {
                            return Some(repo.read(cx).work_directory_abs_path.clone());
                        }
                    }
                    None
                })
                .ok()
                .flatten();
            if let Some(dir) = found {
                return Ok(dir);
            }
        }
        return Err(anyhow!("repository_not_found: id={}", want.0));
    }

    for handle in cx.windows() {
        if active_window_id != Some(handle.window_id()) {
            continue;
        }
        let Some(multi) = handle.downcast::<workspace::MultiWorkspace>() else {
            continue;
        };
        let found = multi
            .update(cx, |multi, _window, cx| {
                for ws in multi.workspaces() {
                    let project = ws.read(cx).project();
                    if let Some(repo) = project.read(cx).active_repository(cx) {
                        return Some(repo.read(cx).work_directory_abs_path.clone());
                    }
                }
                None
            })
            .ok()
            .flatten();
        if let Some(dir) = found {
            return Ok(dir);
        }
    }
    Err(anyhow!("no_active_repository"))
}

/// Currently `git_ui` doesn't re-export a short-sha length constant; mirror
/// the value used by the `git` crate (7).
const SHORT_SHA_LENGTH_FALLBACK: usize = 7;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use tempfile::tempdir;

    #[allow(clippy::disallowed_methods)]
    fn git(dir: &PathBuf, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@x")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@x")
            .status()
            .expect("spawn git");
        assert!(status.success(), "`git {}` failed", args.join(" "));
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn restore_ref_increments_suffix() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        git(&path, &["init", "-q", "-b", "main"]);
        std::fs::write(path.join("a.txt"), "x").unwrap();
        git(&path, &["add", "."]);
        git(
            &path,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@x",
                "commit",
                "-qm",
                "init",
            ],
        );
        let head = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&path)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let head = head.trim();
        let r1 = create_restore_ref(&path, "main", head).expect("create");
        assert!(r1.ends_with("/1"));
        let r2 = create_restore_ref(&path, "main", head).expect("create");
        assert!(r2.ends_with("/2"));
        assert_ne!(r1, r2);
    }
}
