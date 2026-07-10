use crate::SolutionStore;
use anyhow::Result;
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::workspace_state::find_window_for_solution;

pub(crate) fn register_project_files(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ListFilesTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ReadBufferTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ApplyEditTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SaveBufferTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(OpenFileTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(CloseBufferTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(CreateFileTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(DeleteFileTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RenameFileTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(FindInBuffersTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GotoDefinitionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(FindReferencesTool);
    });
}

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
            .find(|sol| sol.id.as_str() == solution_id)
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
// Path-validation helper (cross-cutting security primitive)
// =====================================================================

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum PathValidationError {
    SolutionNotFound,
    PathOutsideSolution,
    InvalidPath,
}

impl std::fmt::Display for PathValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SolutionNotFound => write!(f, "solution_not_found"),
            Self::PathOutsideSolution => write!(f, "path_outside_solution"),
            Self::InvalidPath => write!(f, "invalid_path"),
        }
    }
}

impl std::error::Error for PathValidationError {}

/// Verify that `path` lies under at least one worktree root of the
/// named Solution. Returns the canonicalized absolute path.
///
/// Used by every Phase 6 `project.*` tool to prevent agents from
/// escaping into arbitrary filesystem via `apply_edit("/etc/passwd", ...)`.
#[allow(dead_code)]
pub fn validate_path_in_solution(
    solution_id: &str,
    path: &str,
    cx: &App,
) -> Result<std::path::PathBuf, PathValidationError> {
    let absolute = std::path::PathBuf::from(path);
    if !absolute.is_absolute() {
        // Relative paths require a cwd that we don't have here. Reject.
        return Err(PathValidationError::InvalidPath);
    }

    // Best-effort canonicalization. If the path doesn't exist yet (e.g.
    // create_file), we accept the absolute non-canonical form provided
    // its prefix is under a Solution member.
    let canonical = absolute.canonicalize().unwrap_or_else(|_| absolute.clone());

    let store = SolutionStore::try_global(cx).ok_or(PathValidationError::SolutionNotFound)?;
    let valid = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .find(|sol| sol.id.as_str() == solution_id)
            .map(|sol| {
                sol.members.iter().any(|m| {
                    let canon_member = m
                        .local_path
                        .canonicalize()
                        .unwrap_or_else(|_| m.local_path.clone());
                    canonical.starts_with(&canon_member)
                }) || canonical.starts_with(&sol.root)
            })
    });

    match valid {
        Some(true) => Ok(canonical),
        Some(false) => Err(PathValidationError::PathOutsideSolution),
        None => Err(PathValidationError::SolutionNotFound),
    }
}

// =====================================================================
// project.read_buffer
// =====================================================================

/// Read the content of a file via the editor's Buffer system. If the
/// file is already open in any workspace of the Solution, returns the
/// live (potentially-dirty) content. Otherwise opens it as a Buffer
/// without creating a tab; calling again is idempotent.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ReadBufferParams {
    pub solution_id: String,
    /// Absolute path of the file to read. Must lie under one of the
    /// Solution's worktrees.
    pub path: String,
}

impl<'de> Deserialize<'de> for ReadBufferParams {
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
pub struct ReadBufferResult {
    pub content: String,
    pub line_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub dirty: bool,
}

#[derive(Clone)]
pub struct ReadBufferTool;

impl McpServerTool for ReadBufferTool {
    type Input = ReadBufferParams;
    type Output = ReadBufferResult;
    const NAME: &'static str = "project.read_buffer";

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

        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await?;

        let result = cx.update(|cx| {
            let buffer_ref = buffer.read(cx);
            ReadBufferResult {
                content: buffer_ref.text(),
                line_count: buffer_ref.max_point().row + 1,
                language: buffer_ref
                    .language()
                    .map(|language| language.name().as_ref().to_string()),
                dirty: buffer_ref.is_dirty(),
            }
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("read {} ({} lines)", input.path, result.line_count),
            }],
            structured_content: result,
        })
    }
}

// =====================================================================
// project.apply_edit
// =====================================================================

/// Apply atomic edits to a file via a Buffer transaction. All edits in
/// the request are coalesced into a single edit call so the change is
/// applied as one undo/redo unit. The buffer is opened (without
/// creating a tab) if it is not already open. The edits become visible
/// to the user immediately and join the user's undo stack; saving is
/// not performed automatically.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ApplyEditParams {
    pub solution_id: String,
    /// Absolute path of the file to edit. Must lie under one of the
    /// Solution's worktrees.
    pub path: String,
    /// One or more edits to apply atomically.
    pub edits: Vec<EditSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EditSpec {
    pub range: EditRange,
    pub new_text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EditRange {
    pub start: EditPoint,
    pub end: EditPoint,
}

/// Zero-based `(line, col)` location. `col` is a UTF-8 byte offset
/// within the line, matching `language::Point`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EditPoint {
    pub line: u32,
    pub col: u32,
}

impl<'de> Deserialize<'de> for ApplyEditParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            path: String,
            #[serde(default)]
            edits: Vec<EditSpec>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            path: inner.path,
            edits: inner.edits,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AfterEditMeta {
    pub line_count: u32,
    pub dirty: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApplyEditResult {
    pub applied: bool,
    pub after: AfterEditMeta,
}

#[derive(Clone)]
pub struct ApplyEditTool;

impl McpServerTool for ApplyEditTool {
    type Input = ApplyEditParams;
    type Output = ApplyEditResult;
    const NAME: &'static str = "project.apply_edit";

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
        anyhow::ensure!(
            !input.edits.is_empty(),
            "invalid_params: at least one edit is required"
        );

        cx.update(|cx| validate_path_in_solution(&input.solution_id, &input.path, cx))
            .map_err(|err| anyhow::anyhow!("{err}"))?;

        let project = cx
            .update(|cx| project_for_solution(&input.solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;

        let project_path = cx.update(|cx| resolve_project_path(&project, &input.path, cx))?;

        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await?;

        let edit_count = input.edits.len();
        let after = buffer.update(cx, |buffer, cx| {
            let edits: Vec<(std::ops::Range<language::Point>, String)> = input
                .edits
                .iter()
                .map(|edit| {
                    let start = language::Point::new(edit.range.start.line, edit.range.start.col);
                    let end = language::Point::new(edit.range.end.line, edit.range.end.col);
                    (start..end, edit.new_text.clone())
                })
                .collect();
            buffer.edit(edits, None, cx);
            AfterEditMeta {
                line_count: buffer.max_point().row + 1,
                dirty: buffer.is_dirty(),
            }
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("applied {} edit(s) to {}", edit_count, input.path),
            }],
            structured_content: ApplyEditResult {
                applied: true,
                after,
            },
        })
    }
}

// Locate the `Project` whose worktrees back the named Solution. We walk
// every open `MultiWorkspace` window and return the first project whose
// visible worktrees include the Solution's root (or a member directory
// underneath it).
pub(crate) fn project_for_solution(solution_id: &str, cx: &mut App) -> Option<gpui::Entity<project::Project>> {
    let store = SolutionStore::try_global(cx)?;
    let root = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .find(|sol| sol.id.as_str() == solution_id)
            .map(|sol| sol.root.clone())
    })?;

    for handle in cx.windows() {
        let Some(window_handle) = handle.downcast::<workspace::MultiWorkspace>() else {
            continue;
        };
        let result = window_handle
            .update(cx, |multi, _window, cx| {
                for workspace_entity in multi.workspaces() {
                    let workspace = workspace_entity.read(cx);
                    let project = workspace.project();
                    let matches = project
                        .read(cx)
                        .visible_worktrees(cx)
                        .any(|tree| tree.read(cx).abs_path().starts_with(&root));
                    if matches {
                        return Some(project.clone());
                    }
                }
                None
            })
            .ok()
            .flatten();
        if let Some(project) = result {
            return Some(project);
        }
    }
    None
}

// Map an absolute path to a `ProjectPath` within one of the project's
// visible worktrees. Returns `path_not_in_worktree` if no worktree
// contains it.
fn resolve_project_path(
    project: &gpui::Entity<project::Project>,
    abs_path: &str,
    cx: &App,
) -> anyhow::Result<project::ProjectPath> {
    let abs = std::path::PathBuf::from(abs_path);
    let project_ref = project.read(cx);
    for tree_entity in project_ref.visible_worktrees(cx) {
        let tree = tree_entity.read(cx);
        let root = tree.abs_path();
        if abs.starts_with(root.as_ref()) {
            let rel = abs
                .strip_prefix(root.as_ref())
                .map_err(|err| anyhow::anyhow!("strip_prefix: {err}"))?;
            let rel_path = util::rel_path::RelPath::new(rel, tree.path_style())
                .map_err(|err| anyhow::anyhow!("rel_path: {err}"))?
                .into_owned()
                .into();
            return Ok(project::ProjectPath {
                worktree_id: tree.id(),
                path: rel_path,
            });
        }
    }
    anyhow::bail!("path_not_in_worktree: {abs_path}")
}

// =====================================================================
// project.save_buffer
// =====================================================================

/// Save the on-disk file for a path via the editor's Buffer system. If
/// the buffer is not currently open it is opened (without creating a
/// tab) so the save round-trip applies any pending project formatting
/// hooks; calling on a clean buffer is a no-op.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SaveBufferParams {
    pub solution_id: String,
    /// Absolute path of the file to save. Must lie under one of the
    /// Solution's worktrees.
    pub path: String,
}

impl<'de> Deserialize<'de> for SaveBufferParams {
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
pub struct SaveBufferResult {
    pub saved: bool,
    pub path: String,
}

#[derive(Clone)]
pub struct SaveBufferTool;

impl McpServerTool for SaveBufferTool {
    type Input = SaveBufferParams;
    type Output = SaveBufferResult;
    const NAME: &'static str = "project.save_buffer";

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

        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await?;

        project
            .update(cx, |project, cx| project.save_buffer(buffer, cx))
            .await?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("saved {}", input.path),
            }],
            structured_content: SaveBufferResult {
                saved: true,
                path: input.path,
            },
        })
    }
}

// =====================================================================
// project.open_file
// =====================================================================

/// Open a file as a tab in the Solution's workspace. Unlike
/// `project.read_buffer`, this surfaces the file in the UI by routing
/// through `Workspace::open_path`, which builds an `Editor` item and
/// adds it to the active pane.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct OpenFileParams {
    pub solution_id: String,
    /// Absolute path of the file to open. Must lie under one of the
    /// Solution's worktrees.
    pub path: String,
    /// Whether to focus the new tab. Defaults to `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<bool>,
}

impl<'de> Deserialize<'de> for OpenFileParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            path: String,
            focus: Option<bool>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            path: inner.path,
            focus: inner.focus,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OpenFileResult {
    pub opened: bool,
    pub focused: bool,
}

#[derive(Clone)]
pub struct OpenFileTool;

impl McpServerTool for OpenFileTool {
    type Input = OpenFileParams;
    type Output = OpenFileResult;
    const NAME: &'static str = "project.open_file";

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

        let focus = input.focus.unwrap_or(true);
        let window_handle = cx
            .update(|cx| find_window_for_solution(&input.solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;
        let typed_window = window_handle
            .downcast::<workspace::MultiWorkspace>()
            .ok_or_else(|| anyhow::anyhow!("window_not_multi_workspace"))?;

        let project = cx
            .update(|cx| project_for_solution(&input.solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;

        let project_path = cx.update(|cx| resolve_project_path(&project, &input.path, cx))?;

        let task = typed_window
            .update(cx, |multi, window, cx| {
                let workspace_entity = multi.workspace().clone();
                workspace_entity.update(cx, |workspace, cx| {
                    workspace.open_path(project_path, None, focus, window, cx)
                })
            })
            .map_err(|err| anyhow::anyhow!("open_path failed: {err}"))?;

        task.await?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("opened {} (focused={})", input.path, focus),
            }],
            structured_content: OpenFileResult {
                opened: true,
                focused: focus,
            },
        })
    }
}

// =====================================================================
// project.close_buffer
// =====================================================================

/// Close any tab(s) in the Solution's workspace whose project_path
/// matches the given absolute path. With `save: true`, dirty buffers
/// are saved first via the editor's normal save path; otherwise close
/// uses `SaveIntent::Skip` to avoid prompting the user.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CloseBufferParams {
    pub solution_id: String,
    /// Absolute path of the file whose tab should be closed. Must lie
    /// under one of the Solution's worktrees.
    pub path: String,
    /// Whether to save dirty buffers before closing. Defaults to `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save: Option<bool>,
}

impl<'de> Deserialize<'de> for CloseBufferParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            path: String,
            save: Option<bool>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            path: inner.path,
            save: inner.save,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CloseBufferResult {
    pub closed: bool,
    pub saved: bool,
}

#[derive(Clone)]
pub struct CloseBufferTool;

impl McpServerTool for CloseBufferTool {
    type Input = CloseBufferParams;
    type Output = CloseBufferResult;
    const NAME: &'static str = "project.close_buffer";

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

        let save = input.save.unwrap_or(false);

        let window_handle = cx
            .update(|cx| find_window_for_solution(&input.solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;
        let typed_window = window_handle
            .downcast::<workspace::MultiWorkspace>()
            .ok_or_else(|| anyhow::anyhow!("window_not_multi_workspace"))?;

        let project = cx
            .update(|cx| project_for_solution(&input.solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;

        let project_path = cx.update(|cx| resolve_project_path(&project, &input.path, cx))?;

        // Collect all tab item ids matching the project_path across every
        // pane of every workspace in the window. We close them all so a
        // file split into multiple panes is fully evicted.
        struct CloseTarget {
            pane: gpui::Entity<workspace::Pane>,
            item_id: gpui::EntityId,
            dirty: bool,
        }

        let targets = typed_window
            .update(cx, |multi, _window, cx| {
                let mut collected = Vec::new();
                for workspace_entity in multi.workspaces() {
                    let workspace = workspace_entity.read(cx);
                    for pane_entity in workspace.panes() {
                        let pane = pane_entity.read(cx);
                        for item in pane.items() {
                            if item.project_path(cx).as_ref() == Some(&project_path) {
                                collected.push(CloseTarget {
                                    pane: pane_entity.clone(),
                                    item_id: item.item_id(),
                                    dirty: item.is_dirty(cx),
                                });
                            }
                        }
                    }
                }
                collected
            })
            .map_err(|err| anyhow::anyhow!("collect close targets failed: {err}"))?;

        if targets.is_empty() {
            return Ok(ToolResponse {
                content: vec![ToolResponseContent::Text {
                    text: format!("no open tab for {}", input.path),
                }],
                structured_content: CloseBufferResult {
                    closed: false,
                    saved: false,
                },
            });
        }

        let any_dirty = targets.iter().any(|t| t.dirty);
        let mut saved_flag = false;
        if save && any_dirty {
            let buffer = project
                .update(cx, |project, cx| {
                    project.open_buffer(project_path.clone(), cx)
                })
                .await?;
            project
                .update(cx, |project, cx| project.save_buffer(buffer, cx))
                .await?;
            saved_flag = true;
        }

        let save_intent = if save {
            workspace::pane::SaveIntent::Save
        } else {
            workspace::pane::SaveIntent::Skip
        };

        let close_tasks: Vec<gpui::Task<anyhow::Result<()>>> = typed_window
            .update(cx, |_multi, window, cx| {
                targets
                    .iter()
                    .map(|target| {
                        target.pane.update(cx, |pane, cx| {
                            pane.close_item_by_id(target.item_id, save_intent, window, cx)
                        })
                    })
                    .collect()
            })
            .map_err(|err| anyhow::anyhow!("schedule close failed: {err}"))?;

        for task in close_tasks {
            task.await?;
        }

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("closed {}", input.path),
            }],
            structured_content: CloseBufferResult {
                closed: true,
                saved: saved_flag,
            },
        })
    }
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

// =====================================================================
// project.find_in_buffers
// =====================================================================

/// Search across files of a Solution. Defaults to a case-insensitive
/// substring match; opt into `regex` for a regex match (in either case
/// `case_sensitive: true` makes the match case-sensitive). The `scope`
/// parameter is reserved for future "open buffers only" behaviour and is
/// currently ignored — all searchable files are searched. Pagination via
/// an opaque cursor (`worktree_root|path:line` of the last match
/// returned); the cursor is advisory and not perfectly stable across
/// calls because the order of results from `Project::search` depends on
/// scan timing, so callers should treat it as a coarse "resume from
/// here" hint.
///
/// Backed by `Project::search`, so gitignore is respected and unsaved
/// open-buffer state is reflected. Files outside the Solution's root
/// (when the project owns extra worktrees) are filtered out post-hoc.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct FindInBuffersParams {
    pub solution_id: String,
    /// Substring or regex pattern.
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub case_sensitive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regex: Option<bool>,
    /// `"all_files"` (default) or `"open"`. v1: ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Optional glob pattern matched against the worktree-relative path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_glob: Option<String>,
    /// Opaque cursor returned from the previous response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Maximum number of matches in this page. Default 100, max 1000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<usize>,
}

impl<'de> Deserialize<'de> for FindInBuffersParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            query: String,
            case_sensitive: Option<bool>,
            regex: Option<bool>,
            scope: Option<String>,
            file_glob: Option<String>,
            cursor: Option<String>,
            max: Option<usize>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            query: inner.query,
            case_sensitive: inner.case_sensitive,
            regex: inner.regex,
            scope: inner.scope,
            file_glob: inner.file_glob,
            cursor: inner.cursor,
            max: inner.max,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchMatch {
    /// Path relative to `worktree_root`, in unix form.
    pub path: String,
    /// Absolute worktree root containing the file.
    pub worktree_root: String,
    /// Zero-based line index.
    pub line: u32,
    /// Zero-based UTF-8 byte column where the match starts.
    pub col: u32,
    /// Full text of the line containing the match (untruncated).
    pub line_text: String,
    /// `[start, end)` UTF-8 byte offsets within `line_text`.
    pub match_range: [u32; 2],
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FindInBuffersResult {
    pub matches: Vec<SearchMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone)]
pub struct FindInBuffersTool;

impl McpServerTool for FindInBuffersTool {
    type Input = FindInBuffersParams;
    type Output = FindInBuffersResult;
    const NAME: &'static str = "project.find_in_buffers";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(!input.query.is_empty(), "invalid_params: query is required");
        let max = input.max.unwrap_or(100).clamp(1, 1000);
        let case_sensitive = input.case_sensitive.unwrap_or(false);
        let use_regex = input.regex.unwrap_or(false);
        let start_after = input.cursor.clone().unwrap_or_default();

        let solution_root = cx
            .update(|cx| {
                SolutionStore::try_global(cx).and_then(|store| {
                    store.read_with(cx, |s, _| {
                        s.solutions()
                            .iter()
                            .find(|sol| sol.id.as_str() == input.solution_id)
                            .map(|sol| sol.root.clone())
                    })
                })
            })
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;

        let project = cx
            .update(|cx| project_for_solution(&input.solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;

        // Build the SearchQuery. We pass the optional file_glob through
        // the project search engine's own include matcher so gitignore
        // and the include/exclude pipeline stay consistent with
        // upstream's project search.
        let path_style = cx.update(|cx| project.read(cx).path_style(cx));
        let include_matcher = match input.file_glob.as_deref() {
            Some(glob) => util::paths::PathMatcher::new([glob], path_style)
                .map_err(|err| anyhow::anyhow!("invalid_glob: {err}"))?,
            None => util::paths::PathMatcher::default(),
        };
        let exclude_matcher = util::paths::PathMatcher::default();

        let query = if use_regex {
            project::search::SearchQuery::regex(
                &input.query,
                false,
                case_sensitive,
                false,
                false,
                include_matcher,
                exclude_matcher,
                false,
                None,
            )
        } else {
            project::search::SearchQuery::text(
                &input.query,
                false,
                case_sensitive,
                false,
                include_matcher,
                exclude_matcher,
                false,
                None,
            )
        };
        let query = query.map_err(|err| anyhow::anyhow!("invalid_query: {err}"))?;

        let results = cx.update(|cx| project.update(cx, |proj, cx| proj.search(query, cx)));
        let project::SearchResults { rx, _task_handle } = results;

        let mut all_matches: Vec<SearchMatch> = Vec::new();
        let mut hit_limit = false;
        // Pull matches from the search stream; bail out once we've
        // accumulated `max` matches so the caller can resume via the
        // returned cursor.
        loop {
            let Ok(result) = rx.recv().await else {
                break;
            };
            match result {
                project::search::SearchResult::Buffer { buffer, ranges } => {
                    if ranges.is_empty() {
                        continue;
                    }
                    let collected = cx.update(|cx| {
                        let buffer_ref = buffer.read(cx);
                        let snapshot = buffer_ref.snapshot();
                        let Some(file) = buffer_ref.file() else {
                            return Vec::new();
                        };
                        let Some(local) = file.as_local() else {
                            return Vec::new();
                        };
                        let abs_path = local.abs_path(cx);
                        // Filter out files that fall outside the
                        // Solution root, e.g. when the project owns
                        // extra worktrees added after the Solution was
                        // opened.
                        if !abs_path.starts_with(&solution_root) {
                            return Vec::new();
                        }
                        let worktree_id = file.worktree_id(cx);
                        let Some(worktree) = project.read(cx).worktree_for_id(worktree_id, cx)
                        else {
                            return Vec::new();
                        };
                        let worktree_root =
                            worktree.read(cx).abs_path().to_string_lossy().into_owned();
                        let rel_path = file.path().as_unix_str().to_string();

                        let mut local_matches = Vec::new();
                        for range in ranges.iter() {
                            use language::OffsetRangeExt as _;
                            let point_range = range.to_point(&snapshot);
                            let line = point_range.start.row;
                            // Restrict the match to the start line
                            // (multi-line matches are rare for typical
                            // text searches and the response shape is
                            // single-line).
                            let line_len = snapshot.line_len(line);
                            let line_start = language::Point::new(line, 0);
                            let line_end = language::Point::new(line, line_len);
                            let line_text: String =
                                snapshot.text_for_range(line_start..line_end).collect();
                            let start_col = point_range.start.column;
                            let end_col = if point_range.end.row == line {
                                point_range.end.column
                            } else {
                                line_len
                            };
                            local_matches.push(SearchMatch {
                                path: rel_path.clone(),
                                worktree_root: worktree_root.clone(),
                                line,
                                col: start_col,
                                line_text,
                                match_range: [start_col, end_col],
                            });
                        }
                        local_matches
                    });

                    // Apply the cursor filter (advisory: skip matches
                    // whose `worktree_root|rel_path:line` ordering is
                    // <= start_after).
                    for m in collected {
                        if !start_after.is_empty() {
                            let key = format!("{}|{}:{}", m.worktree_root, m.path, m.line);
                            if key.as_str() <= start_after.as_str() {
                                continue;
                            }
                        }
                        if all_matches.len() >= max {
                            hit_limit = true;
                            break;
                        }
                        all_matches.push(m);
                    }
                    if hit_limit {
                        break;
                    }
                }
                project::search::SearchResult::LimitReached => {
                    hit_limit = true;
                    break;
                }
                project::search::SearchResult::WaitingForScan
                | project::search::SearchResult::Searching => continue,
            }
        }

        let next_cursor = if hit_limit {
            all_matches
                .last()
                .map(|m| format!("{}|{}:{}", m.worktree_root, m.path, m.line))
        } else {
            None
        };

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} match(es)", all_matches.len()),
            }],
            structured_content: FindInBuffersResult {
                matches: all_matches,
                next_cursor,
            },
        })
    }
}

// =====================================================================
// project.goto_definition
// =====================================================================

/// Resolve LSP "goto definition" for a position in a file. Opens the
/// buffer (without surfacing a tab) so the language server is engaged,
/// then awaits the LSP query. Returns an empty list when no language
/// server provides definitions for the file (not an error).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GotoDefinitionParams {
    pub solution_id: String,
    /// Absolute path of the file. Must lie under one of the Solution's
    /// worktrees.
    pub path: String,
    /// Zero-based line index.
    pub line: u32,
    /// Zero-based UTF-8 byte column.
    pub col: u32,
}

impl<'de> Deserialize<'de> for GotoDefinitionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            path: String,
            line: u32,
            col: u32,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            path: inner.path,
            line: inner.line,
            col: inner.col,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LocationRef {
    /// Absolute path of the target file (the buffer's `abs_path`). Empty
    /// when the target buffer has no on-disk file (e.g. a scratch
    /// buffer).
    pub path: String,
    pub start: EditPoint,
    pub end: EditPoint,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GotoDefinitionResult {
    pub definitions: Vec<LocationRef>,
}

#[derive(Clone)]
pub struct GotoDefinitionTool;

impl McpServerTool for GotoDefinitionTool {
    type Input = GotoDefinitionParams;
    type Output = GotoDefinitionResult;
    const NAME: &'static str = "project.goto_definition";

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

        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await?;

        let position = language::Point::new(input.line, input.col);
        let task = project.update(cx, |project, cx| project.definitions(&buffer, position, cx));

        let definitions = match task.await {
            Ok(Some(links)) => cx.update(|cx| location_links_to_refs(&links, cx)),
            Ok(None) | Err(_) => Vec::new(),
        };

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} definition(s)", definitions.len()),
            }],
            structured_content: GotoDefinitionResult { definitions },
        })
    }
}

fn location_links_to_refs(links: &[project::LocationLink], cx: &App) -> Vec<LocationRef> {
    links
        .iter()
        .map(|link| location_to_ref(&link.target, cx))
        .collect()
}

fn locations_to_refs(locations: &[language::Location], cx: &App) -> Vec<LocationRef> {
    locations
        .iter()
        .map(|location| location_to_ref(location, cx))
        .collect()
}

fn location_to_ref(location: &language::Location, cx: &App) -> LocationRef {
    use language::ToPoint as _;

    let buffer = location.buffer.read(cx);
    let path = project::File::from_dyn(buffer.file())
        .map(|file| {
            <project::File as language::LocalFile>::abs_path(file, cx)
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_default();
    let snapshot = buffer.snapshot();
    let start_point = location.range.start.to_point(&snapshot);
    let end_point = location.range.end.to_point(&snapshot);
    LocationRef {
        path,
        start: EditPoint {
            line: start_point.row,
            col: start_point.column,
        },
        end: EditPoint {
            line: end_point.row,
            col: end_point.column,
        },
    }
}

// =====================================================================
// project.find_references
// =====================================================================

/// Resolve LSP "find references" for a position in a file. Opens the
/// buffer (without surfacing a tab) so the language server is engaged.
/// `include_declaration` is forwarded to the language server's
/// preference where applicable; v1 simply returns whatever set the
/// server reports. Returns an empty list when no language server
/// provides references for the file (not an error).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct FindReferencesParams {
    pub solution_id: String,
    /// Absolute path of the file. Must lie under one of the Solution's
    /// worktrees.
    pub path: String,
    pub line: u32,
    pub col: u32,
    /// Reserved for forwarding to LSP `includeDeclaration`. Currently
    /// the editor's `Project::references` does not expose this knob, so
    /// the parameter is accepted but ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_declaration: Option<bool>,
}

impl<'de> Deserialize<'de> for FindReferencesParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            path: String,
            line: u32,
            col: u32,
            include_declaration: Option<bool>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            path: inner.path,
            line: inner.line,
            col: inner.col,
            include_declaration: inner.include_declaration,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FindReferencesResult {
    pub references: Vec<LocationRef>,
}

#[derive(Clone)]
pub struct FindReferencesTool;

impl McpServerTool for FindReferencesTool {
    type Input = FindReferencesParams;
    type Output = FindReferencesResult;
    const NAME: &'static str = "project.find_references";

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

        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await?;

        let position = language::Point::new(input.line, input.col);
        let task = project.update(cx, |project, cx| project.references(&buffer, position, cx));

        let references = match task.await {
            Ok(Some(locations)) => cx.update(|cx| locations_to_refs(&locations, cx)),
            Ok(None) | Err(_) => Vec::new(),
        };

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} reference(s)", references.len()),
            }],
            structured_content: FindReferencesResult { references },
        })
    }
}

