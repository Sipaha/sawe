//! S-PSH MCP tools — preview / push / push --force-with-lease /
//! push --force.
//!
//! Mirror of `handlers_mcp.rs` for the push surface. The preview tool is
//! `ReadOnly` (subagents may dry-run a push), `push` and
//! `push_force_with_lease` are `Write` (no history loss in the success
//! path), and `push_force` is `Destructive` (can overwrite remote work).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use editor_mcp::{ToolTier, register_typed_tool_with_tier};
use gpui::{App, AsyncApp};
use project::git_store::RepositoryId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::push_dialog::{build_preview, current_branch, run_force_with_lease, run_plain_push};

pub(crate) fn register(cx: &mut App) {
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, PreviewPushTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, PushTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, PushForceWithLeaseTool);
    register_typed_tool_with_tier(cx, ToolTier::Destructive, PushForceTool);
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.preview_push`. Dry-runs a push without
/// touching the remote; useful for subagents that want to assert "what
/// would my push send?" before doing it.
pub struct PreviewPushInput {
    /// Override the configured upstream remote — e.g. push to a fork
    /// instead of the tracked remote. `None` = use upstream.
    pub remote: Option<String>,
    /// Override the remote branch name. `None` = use upstream's.
    pub remote_branch: Option<String>,
    /// Repository to operate on. Defaults to focused window's active.
    pub repo_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MiniCommitPayload {
    pub sha: String,
    pub subject: String,
    pub author_email: String,
    pub committer_date_unix: i64,
}

/// Output of the preview push tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PreviewPushOutput {
    pub branch: String,
    pub remote: String,
    pub remote_branch: String,
    pub ahead: Vec<MiniCommitPayload>,
    pub behind: Vec<MiniCommitPayload>,
    pub divergence: bool,
    pub will_create_remote_branch: bool,
}

#[derive(Clone)]
pub struct PreviewPushTool;

impl McpServerTool for PreviewPushTool {
    type Input = PreviewPushInput;
    type Output = PreviewPushOutput;
    const NAME: &'static str = "editor.git.preview_push";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let branch = current_branch(&work_dir_buf).await?;
        let remote_override = input.remote_branch.unwrap_or_default();
        let preview = build_preview(&work_dir_buf, &branch, remote_override.as_str()).await?;
        // Caller's optional `remote` override only matters when it
        // differs from the inferred upstream's remote. We keep the
        // simple semantics: report the inferred remote unless input
        // specifies one.
        let remote = input.remote.unwrap_or_else(|| preview.remote.clone());
        let summary = format!(
            "{} ahead, {} behind ({}{})",
            preview.ahead.len(),
            preview.behind.len(),
            preview.remote,
            if preview.will_create_remote_branch {
                " — will create remote branch"
            } else {
                ""
            },
        );
        let divergence = !preview.behind.is_empty();
        let will_create_remote_branch = preview.will_create_remote_branch;
        let payload = PreviewPushOutput {
            branch: preview.branch,
            remote,
            remote_branch: preview.remote_branch,
            ahead: preview
                .ahead
                .into_iter()
                .map(|c| MiniCommitPayload {
                    sha: c.sha,
                    subject: c.subject,
                    author_email: c.author_email,
                    committer_date_unix: c.committer_date_unix,
                })
                .collect(),
            behind: preview
                .behind
                .into_iter()
                .map(|c| MiniCommitPayload {
                    sha: c.sha,
                    subject: c.subject,
                    author_email: c.author_email,
                    committer_date_unix: c.committer_date_unix,
                })
                .collect(),
            divergence,
            will_create_remote_branch,
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: payload,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.push`. Plain push, no force.
pub struct PushInput {
    pub set_upstream: bool,
    pub tags: bool,
    pub no_verify: bool,
    pub repo_id: Option<u64>,
}

/// Output of the push tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PushOutput {
    pub branch: String,
    pub remote: String,
    pub remote_branch: String,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone)]
pub struct PushTool;

impl McpServerTool for PushTool {
    type Input = PushInput;
    type Output = PushOutput;
    const NAME: &'static str = "editor.git.push";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let branch = current_branch(&work_dir_buf).await?;
        let preview = build_preview(&work_dir_buf, &branch, "").await?;
        let output = run_plain_push(
            &work_dir_buf,
            &branch,
            &preview.remote,
            &preview.remote_branch,
            input.set_upstream || preview.will_create_remote_branch,
            input.tags,
            input.no_verify,
            false,
        )
        .await?;
        let summary = format!(
            "pushed {} to {}/{}",
            branch, preview.remote, preview.remote_branch
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: PushOutput {
                branch,
                remote: preview.remote,
                remote_branch: preview.remote_branch,
                stdout: output.stdout,
                stderr: output.stderr,
            },
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.push_force_with_lease`. Atomic: when
/// `expected_remote_sha` is `Some`, git refuses if the remote moved
/// between preview and push.
pub struct PushForceWithLeaseInput {
    pub set_upstream: bool,
    pub tags: bool,
    pub no_verify: bool,
    /// Pin the lease to this remote sha. Omit to fall back to plain
    /// `--force-with-lease` (git auto-detects).
    pub expected_remote_sha: Option<String>,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct PushForceWithLeaseTool;

impl McpServerTool for PushForceWithLeaseTool {
    type Input = PushForceWithLeaseInput;
    type Output = PushOutput;
    const NAME: &'static str = "editor.git.push_force_with_lease";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let branch = current_branch(&work_dir_buf).await?;
        let preview = build_preview(&work_dir_buf, &branch, "").await?;
        let output = run_force_with_lease(
            &work_dir_buf,
            &branch,
            &preview.remote,
            &preview.remote_branch,
            input.expected_remote_sha.as_deref(),
            input.set_upstream || preview.will_create_remote_branch,
            input.tags,
            input.no_verify,
        )
        .await?;
        let summary = format!(
            "force-with-lease pushed {} to {}/{}",
            branch, preview.remote, preview.remote_branch
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: PushOutput {
                branch,
                remote: preview.remote,
                remote_branch: preview.remote_branch,
                stdout: output.stdout,
                stderr: output.stderr,
            },
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.push_force`. Plain `--force`, no atomic check —
/// classed `Destructive` because it can overwrite a collaborator's work.
pub struct PushForceInput {
    pub set_upstream: bool,
    pub tags: bool,
    pub no_verify: bool,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct PushForceTool;

impl McpServerTool for PushForceTool {
    type Input = PushForceInput;
    type Output = PushOutput;
    const NAME: &'static str = "editor.git.push_force";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let branch = current_branch(&work_dir_buf).await?;
        let preview = build_preview(&work_dir_buf, &branch, "").await?;
        let output = run_plain_push(
            &work_dir_buf,
            &branch,
            &preview.remote,
            &preview.remote_branch,
            input.set_upstream || preview.will_create_remote_branch,
            input.tags,
            input.no_verify,
            true,
        )
        .await?;
        let summary = format!(
            "force-pushed {} to {}/{} (no atomic check)",
            branch, preview.remote, preview.remote_branch
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: PushOutput {
                branch,
                remote: preview.remote,
                remote_branch: preview.remote_branch,
                stdout: output.stdout,
                stderr: output.stderr,
            },
        })
    }
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
