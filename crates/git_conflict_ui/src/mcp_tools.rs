//! MCP tool surface — `editor.git.list_conflicts`,
//! `editor.git.resolve_conflict`, `editor.git.mark_resolved`,
//! `editor.git.continue_merge`, `editor.git.abort_merge`.
//!
//! Mirrors the pattern in `git_ui::handlers_mcp` — invokes raw `git`
//! against the active workspace's repo directory, with `repo_id` as an
//! optional override for selecting a specific repository among multiple
//! solutions.

use anyhow::{Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use editor_mcp::{ToolTier, register_typed_tool_with_tier};
use git::operations::OpRunner;
use gpui::{App, AppContext as _, AsyncApp};
use project::git_store::RepositoryId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::conflict_parser::{ConflictedFile, list_conflicts_async};
use crate::operations::{ContinueMergeOp, op_for_dir, run_git_void};

pub(crate) fn register(cx: &mut App) {
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, ListConflictsTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, ResolveConflictTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, MarkResolvedTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, ContinueMergeTool);
    register_typed_tool_with_tier(cx, ToolTier::Destructive, AbortMergeTool);
}

/// Input parameters for the list conflicts tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ListConflictsInput {
    pub repo_id: Option<u64>,
}

/// Output of the list conflicts tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListConflictsOutput {
    pub files: Vec<ConflictFileWire>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ConflictFileWire {
    pub path: String,
    pub has_base: bool,
    pub has_ours: bool,
    pub has_theirs: bool,
    pub is_binary: bool,
}

impl From<ConflictedFile> for ConflictFileWire {
    fn from(value: ConflictedFile) -> Self {
        Self {
            path: value.path.as_std_path().to_string_lossy().into_owned(),
            has_base: value.has_base,
            has_ours: value.has_ours,
            has_theirs: value.has_theirs,
            is_binary: value.is_binary,
        }
    }
}

#[derive(Clone)]
pub struct ListConflictsTool;

impl McpServerTool for ListConflictsTool {
    type Input = ListConflictsInput;
    type Output = ListConflictsOutput;
    const NAME: &'static str = "editor.git.list_conflicts";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let files = list_conflicts_async(&work_dir).await?;
        let summary = format!("{} conflicted file(s)", files.len());
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: ListConflictsOutput {
                files: files.into_iter().map(Into::into).collect(),
            },
        })
    }
}

/// Input parameters for the resolve conflict tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ResolveConflictInput {
    pub path: String,
    /// One of `"ours"`, `"theirs"`, `"manual"`. For `"manual"`, `content` is required.
    pub resolution: String,
    pub content: Option<String>,
    pub repo_id: Option<u64>,
}

/// Output of the resolve conflict tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ResolveConflictOutput {
    pub path: String,
    pub resolution: String,
}

#[derive(Clone)]
pub struct ResolveConflictTool;

impl McpServerTool for ResolveConflictTool {
    type Input = ResolveConflictInput;
    type Output = ResolveConflictOutput;
    const NAME: &'static str = "editor.git.resolve_conflict";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let path_repo = git::repository::RepoPath::new(&input.path)?;

        match input.resolution.as_str() {
            "ours" => {
                run_git_void(&work_dir, &["checkout", "--ours", "--", &input.path]).await?;
                run_git_void(&work_dir, &["add", "--", &input.path]).await?;
            }
            "theirs" => {
                run_git_void(&work_dir, &["checkout", "--theirs", "--", &input.path]).await?;
                run_git_void(&work_dir, &["add", "--", &input.path]).await?;
            }
            "manual" => {
                let content = input
                    .content
                    .as_ref()
                    .ok_or_else(|| anyhow!("manual resolution requires `content`"))?;
                let abs = work_dir.join(path_repo.as_std_path());
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&abs, content)?;
                run_git_void(&work_dir, &["add", "--", &input.path]).await?;
            }
            other => return Err(anyhow!("unknown resolution: {other}")),
        }

        let summary = format!("resolved {} ({})", input.path, input.resolution);
        let path_for_output = input.path.clone();
        let resolution_for_output = input.resolution.clone();
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: ResolveConflictOutput {
                path: path_for_output,
                resolution: resolution_for_output,
            },
        })
    }
}

/// Input parameters for the mark resolved tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MarkResolvedInput {
    pub path: String,
    pub repo_id: Option<u64>,
}

/// Output of the mark resolved tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MarkResolvedOutput {
    pub path: String,
}

#[derive(Clone)]
pub struct MarkResolvedTool;

impl McpServerTool for MarkResolvedTool {
    type Input = MarkResolvedInput;
    type Output = MarkResolvedOutput;
    const NAME: &'static str = "editor.git.mark_resolved";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        run_git_void(&work_dir, &["add", "--", &input.path]).await?;
        let summary = format!("staged {}", input.path);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: MarkResolvedOutput { path: input.path },
        })
    }
}

/// Input parameters for the continue merge tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ContinueMergeInput {
    pub repo_id: Option<u64>,
}

/// Output of the continue merge tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ContinueMergeOutput {
    pub op: String,
}

#[derive(Clone)]
pub struct ContinueMergeTool;

impl McpServerTool for ContinueMergeTool {
    type Input = ContinueMergeInput;
    type Output = ContinueMergeOutput;
    const NAME: &'static str = "editor.git.continue_merge";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let op = op_for_dir(&work_dir).ok_or_else(|| anyhow!("no in-progress op"))?;
        let cli = op.cli_subcommand().to_string();
        let work_buf = work_dir.to_path_buf();
        cx.background_spawn(async move { OpRunner::run(ContinueMergeOp { op }, &work_buf) })
            .await?;
        let summary = format!("git {cli} --continue");
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: ContinueMergeOutput { op: cli },
        })
    }
}

/// Input parameters for the abort merge tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AbortMergeInput {
    pub repo_id: Option<u64>,
}

/// Output of the abort merge tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AbortMergeOutput {
    pub op: String,
}

#[derive(Clone)]
pub struct AbortMergeTool;

impl McpServerTool for AbortMergeTool {
    type Input = AbortMergeInput;
    type Output = AbortMergeOutput;
    const NAME: &'static str = "editor.git.abort_merge";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let op = op_for_dir(&work_dir).ok_or_else(|| anyhow!("no in-progress op"))?;
        let cli = op.cli_subcommand().to_string();
        let work_buf = work_dir.to_path_buf();
        cx.background_spawn(async move {
            OpRunner::run(crate::operations::AbortMergeOp { op }, &work_buf)
        })
        .await?;
        let summary = format!("git {cli} --abort");
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: AbortMergeOutput { op: cli },
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

#[allow(dead_code)]
fn _types(_buf: PathBuf) {}
