use crate::SolutionStore;
use anyhow::Result;
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::{project_for_solution, resolve_project_path, validate_path_in_solution};

// =====================================================================
// project.list_files
// =====================================================================

/// List files across the worktrees of a Solution. Supports an optional
/// glob filter (matched against each file's path relative to its
/// worktree root), a `scope` (`all_worktrees` (default) or
/// `first_worktree`), and opaque cursor-based pagination. The cursor is
/// the `worktree_root|path` of the last entry returned in the previous
/// page; the next page begins strictly after that point in lexicographic
/// order.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ListFilesParams {
    pub solution_id: String,
    /// Optional glob pattern (e.g. `**/*.rs`). When omitted, all files
    /// are returned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    /// Scope: `"all_worktrees"` (default) or `"first_worktree"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Opaque pagination cursor returned from the previous response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Maximum number of results in this page. Default 200, max 5000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<usize>,
}

impl<'de> Deserialize<'de> for ListFilesParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            glob: Option<String>,
            scope: Option<String>,
            cursor: Option<String>,
            max: Option<usize>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            glob: inner.glob,
            scope: inner.scope,
            cursor: inner.cursor,
            max: inner.max,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FileEntry {
    /// Path relative to `worktree_root`, in unix form.
    pub path: String,
    /// Absolute path of the worktree root containing this entry.
    pub worktree_root: String,
    /// File size in bytes, as reported by the worktree scan.
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListFilesResult {
    pub files: Vec<FileEntry>,
    /// Cursor for the next page, or absent when the list is exhausted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone)]
pub struct ListFilesTool;

impl McpServerTool for ListFilesTool {
    type Input = ListFilesParams;
    type Output = ListFilesResult;
    const NAME: &'static str = "project.list_files";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        let max = input.max.unwrap_or(200).clamp(1, 5000);
        let scope_first_only = match input.scope.as_deref() {
            None | Some("all_worktrees") => false,
            Some("first_worktree") => true,
            Some(other) => anyhow::bail!(
                "invalid_params: scope must be \"all_worktrees\" or \"first_worktree\", got {other:?}"
            ),
        };
        let glob_matcher = input
            .glob
            .as_deref()
            .map(globset::Glob::new)
            .transpose()
            .map_err(|err| anyhow::anyhow!("invalid_glob: {err}"))?
            .map(|g| g.compile_matcher());
        let start_after = input.cursor.clone().unwrap_or_default();

        let (files, next_cursor) = cx.update(|cx| {
            collect_files(
                &input.solution_id,
                scope_first_only,
                glob_matcher.as_ref(),
                &start_after,
                max,
                cx,
            )
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} file(s)", files.len()),
            }],
            structured_content: ListFilesResult { files, next_cursor },
        })
    }
}

fn cursor_for(file: &FileEntry) -> String {
    format!("{}|{}", file.worktree_root, file.path)
}

fn collect_files(
    solution_id: &str,
    first_only: bool,
    glob: Option<&globset::GlobMatcher>,
    start_after: &str,
    max: usize,
    cx: &mut App,
) -> (Vec<FileEntry>, Option<String>) {
    let Some(store) = SolutionStore::try_global(cx) else {
        return (Vec::new(), None);
    };
    let Some(root) = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .find(|sol| sol.id.0.to_string() == solution_id)
            .map(|sol| sol.root.clone())
    }) else {
        return (Vec::new(), None);
    };

    for handle in cx.windows() {
        let Some(window_handle) = handle.downcast::<workspace::MultiWorkspace>() else {
            continue;
        };
        let collected = window_handle
            .update(cx, |multi, _window, cx| {
                let primary_matches = multi
                    .workspace()
                    .read(cx)
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .any(|tree| tree.read(cx).abs_path().starts_with(&root));
                let any_matches = primary_matches
                    || multi.workspaces().any(|ws| {
                        ws.read(cx)
                            .project()
                            .read(cx)
                            .visible_worktrees(cx)
                            .any(|tree| tree.read(cx).abs_path().starts_with(&root))
                    });
                if !any_matches {
                    return None;
                }

                let mut files: Vec<FileEntry> = Vec::new();
                let mut reached_cap = false;
                'outer: for workspace_entity in multi.workspaces() {
                    let workspace = workspace_entity.read(cx);
                    let project = workspace.project().read(cx);
                    for tree_entity in project.visible_worktrees(cx) {
                        let tree = tree_entity.read(cx);
                        let abs_root = tree.abs_path();
                        if !abs_root.starts_with(&root) {
                            continue;
                        }
                        let worktree_root = abs_root.to_string_lossy().into_owned();
                        for entry in tree.entries(false, 0) {
                            if !entry.is_file() {
                                continue;
                            }
                            let path_str = entry.path.as_unix_str().to_string();
                            let candidate = FileEntry {
                                path: path_str,
                                worktree_root: worktree_root.clone(),
                                size: entry.size,
                            };
                            let key = cursor_for(&candidate);
                            if !start_after.is_empty() && key.as_str() <= start_after {
                                continue;
                            }
                            if let Some(matcher) = glob {
                                if !matcher.is_match(&candidate.path) {
                                    continue;
                                }
                            }
                            files.push(candidate);
                            if files.len() > max {
                                reached_cap = true;
                                break 'outer;
                            }
                        }
                        if first_only {
                            break 'outer;
                        }
                    }
                }

                let next_cursor = if reached_cap {
                    files.truncate(max);
                    files.last().map(cursor_for)
                } else {
                    None
                };
                Some((files, next_cursor))
            })
            .ok()
            .flatten();

        if let Some(result) = collected {
            return result;
        }
    }
    (Vec::new(), None)
}

// =====================================================================
// project.create_file
// =====================================================================

/// Create a new file under one of the Solution's worktrees. Optional
/// `content` is written via the project's filesystem layer; absent
/// content yields an empty file. Parent directories are created as
/// needed. Errors if the path already exists.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CreateFileParams {
    pub solution_id: String,
    /// Absolute path of the file to create. Must lie under one of the
    /// Solution's worktrees.
    pub path: String,
    /// Optional initial file content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

impl<'de> Deserialize<'de> for CreateFileParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            path: String,
            content: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            path: inner.path,
            content: inner.content,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CreateFileResult {
    pub created: bool,
}

#[derive(Clone)]
pub struct CreateFileTool;

impl McpServerTool for CreateFileTool {
    type Input = CreateFileParams;
    type Output = CreateFileResult;
    const NAME: &'static str = "project.create_file";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(!input.path.is_empty(), "invalid_params: path is required");

        cx.update(|cx| validate_path_in_solution(&input.solution_id, &input.path, cx))
            .map_err(|err| anyhow::anyhow!("{err}"))?;

        let abs_path = std::path::PathBuf::from(&input.path);
        if abs_path.exists() {
            anyhow::bail!("file_exists: {}", input.path);
        }

        let project = cx
            .update(|cx| project_for_solution(&input.solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;

        let fs: std::sync::Arc<dyn fs::Fs> = cx.update(|cx| project.read(cx).fs().clone());
        let bytes = input.content.unwrap_or_default().into_bytes();
        fs.write(&abs_path, &bytes).await?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("created {}", input.path),
            }],
            structured_content: CreateFileResult { created: true },
        })
    }
}

// =====================================================================
// project.delete_file
// =====================================================================

/// Delete a file via the project's worktree (move-to-trash semantics
/// disabled — the file is permanently removed from disk). Errors if
/// the path is not currently tracked by a worktree.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct DeleteFileParams {
    pub solution_id: String,
    /// Absolute path of the file to delete. Must lie under one of the
    /// Solution's worktrees.
    pub path: String,
}

impl<'de> Deserialize<'de> for DeleteFileParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            path: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            path: inner.path,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeleteFileResult {
    pub deleted: bool,
}

#[derive(Clone)]
pub struct DeleteFileTool;

impl McpServerTool for DeleteFileTool {
    type Input = DeleteFileParams;
    type Output = DeleteFileResult;
    const NAME: &'static str = "project.delete_file";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(!input.path.is_empty(), "invalid_params: path is required");

        cx.update(|cx| validate_path_in_solution(&input.solution_id, &input.path, cx))
            .map_err(|err| anyhow::anyhow!("{err}"))?;

        let project = cx
            .update(|cx| project_for_solution(&input.solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;

        let project_path = cx.update(|cx| resolve_project_path(&project, &input.path, cx))?;

        let task = cx.update(|cx| {
            project.update(cx, |project, cx| {
                let entry = project
                    .entry_for_path(&project_path, cx)
                    .ok_or_else(|| anyhow::anyhow!("path_not_in_worktree: {}", input.path))?;
                let entry_id = entry.id;
                project
                    .delete_entry(entry_id, false, cx)
                    .ok_or_else(|| anyhow::anyhow!("delete_entry returned no task"))
            })
        })?;

        task.await?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("deleted {}", input.path),
            }],
            structured_content: DeleteFileResult { deleted: true },
        })
    }
}

// =====================================================================
// project.rename_file
// =====================================================================

/// Rename or move a file within a single worktree. Both `from` and `to`
/// must resolve to the same Solution; cross-worktree moves are
/// rejected. The rename routes through `Project::rename_entry`, which
/// also notifies the LSP layer.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RenameFileParams {
    pub solution_id: String,
    /// Absolute source path. Must lie under one of the Solution's
    /// worktrees.
    pub from: String,
    /// Absolute destination path. Must lie under the same worktree as
    /// `from`.
    pub to: String,
}

impl<'de> Deserialize<'de> for RenameFileParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            from: String,
            to: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            from: inner.from,
            to: inner.to,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RenameFileResult {
    pub renamed: bool,
}

#[derive(Clone)]
pub struct RenameFileTool;

impl McpServerTool for RenameFileTool {
    type Input = RenameFileParams;
    type Output = RenameFileResult;
    const NAME: &'static str = "project.rename_file";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(!input.from.is_empty(), "invalid_params: from is required");
        anyhow::ensure!(!input.to.is_empty(), "invalid_params: to is required");

        cx.update(|cx| validate_path_in_solution(&input.solution_id, &input.from, cx))
            .map_err(|err| anyhow::anyhow!("from: {err}"))?;
        cx.update(|cx| validate_path_in_solution(&input.solution_id, &input.to, cx))
            .map_err(|err| anyhow::anyhow!("to: {err}"))?;

        let project = cx
            .update(|cx| project_for_solution(&input.solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;

        let from_project_path = cx.update(|cx| resolve_project_path(&project, &input.from, cx))?;
        let to_project_path = cx.update(|cx| resolve_project_path(&project, &input.to, cx))?;

        anyhow::ensure!(
            from_project_path.worktree_id == to_project_path.worktree_id,
            "cross_worktree_rename_unsupported"
        );

        let task = cx.update(|cx| {
            project.update(cx, |project, cx| {
                let entry = project
                    .entry_for_path(&from_project_path, cx)
                    .ok_or_else(|| anyhow::anyhow!("path_not_in_worktree: {}", input.from))?;
                let entry_id = entry.id;
                anyhow::Ok(project.rename_entry(entry_id, to_project_path.clone(), cx))
            })
        })?;

        task.await?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("renamed {} -> {}", input.from, input.to),
            }],
            structured_content: RenameFileResult { renamed: true },
        })
    }
}
