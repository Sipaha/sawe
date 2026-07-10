use anyhow::Result;
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::AsyncApp;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::{EditSpec, project_for_solution, resolve_project_path, validate_path_in_solution};
use crate::mcp::workspace_state::find_window_for_solution;

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
