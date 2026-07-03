//! MCP tools exposed by the `solutions` crate. Tools register with the
//! central `editor_mcp` registry from `solutions::init` so that
//! `start_server` (called later from `crates/zed/src/main.rs`) sees them
//! when binding the socket.
use crate::{Solution, SolutionStore};
use anyhow::{Context as _, Result};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use util::ResultExt as _;

pub fn register(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ListSolutionsTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetSolutionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(CreateSolutionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RenameSolutionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(DeleteSolutionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(OpenSolutionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(CloseSolutionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(FindForPathTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ListCatalogTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(AddCatalogProjectTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RemoveCatalogProjectTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(EditCatalogProjectTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ClearCacheTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RefreshCacheTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(AddMemberTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(AddEmptyMemberTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RemoveMemberTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ReorderMembersTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SetActiveMemberTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ListBuffersTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetEffectiveSettingsTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(DispatchActionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ScreenshotTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(DumpVisualStructureTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(DumpWindowStructureTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetDiagnosticsTool);
    });
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
// solutions.list
// =====================================================================

/// List all configured Solutions with summary metadata.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ListSolutionsParams {}

impl<'de> Deserialize<'de> for ListSolutionsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let _ = serde::de::IgnoredAny::deserialize(de)?;
        Ok(ListSolutionsParams {})
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SolutionSummary {
    pub id: String,
    pub name: String,
    pub root: String,
    pub member_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_opened_at: Option<String>,
    pub open: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_window_id: Option<String>,
    /// Path of this Solution's per-solution MCP socket. Present only while the
    /// Solution is open. A subagent scoped to this Solution connects here
    /// (`sawe --nc <mcp_socket>`); the socket serves only the solution-scoped
    /// tool subset with `solution_id` force-injected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_socket: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListSolutionsResult {
    pub solutions: Vec<SolutionSummary>,
}

#[derive(Clone)]
pub struct ListSolutionsTool;

impl McpServerTool for ListSolutionsTool {
    type Input = ListSolutionsParams;
    type Output = ListSolutionsResult;
    const NAME: &'static str = "solutions.list";

    async fn run(
        &self,
        _input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let summaries = cx.update(|cx| {
            let store = SolutionStore::global(cx);
            let solutions = store.read_with(cx, |store, _| store.solutions().to_vec());
            solutions
                .iter()
                .map(|sol| build_summary(sol, cx))
                .collect::<Vec<_>>()
        });
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} solution(s)", summaries.len()),
            }],
            structured_content: ListSolutionsResult {
                solutions: summaries,
            },
        })
    }
}

pub fn build_summary(sol: &Solution, cx: &App) -> SolutionSummary {
    // `main_window_id` is still derived from live window enumeration (needed
    // by the UI to focus the correct window). Only the `open` flag moves to
    // stored runtime data so tests can flip it without spawning real windows.
    let main_window_id = find_window_id_for_solution(&sol.root, cx);
    let open = SolutionStore::try_global(cx)
        .map(|store| store.read(cx).is_open(&sol.id))
        .unwrap_or(false);
    let mcp_socket = open.then(|| {
        editor_mcp::solution_socket_path(sol.id.as_str())
            .to_string_lossy()
            .into_owned()
    });
    SolutionSummary {
        id: sol.id.as_str().to_string(),
        name: sol.name.clone(),
        root: sol.root.to_string_lossy().into_owned(),
        member_count: sol.members.len(),
        last_opened_at: sol.last_opened_at.map(|t| t.to_rfc3339()),
        open,
        main_window_id,
        mcp_socket,
    }
}

fn find_window_id_for_solution(solution_root: &std::path::Path, cx: &App) -> Option<String> {
    for handle in cx.windows() {
        let Some(window) = handle.downcast::<workspace::MultiWorkspace>() else {
            continue;
        };
        let matches = window
            .read_with(cx, |multi, cx| {
                multi.workspaces().any(|ws| {
                    ws.read(cx)
                        .project()
                        .read(cx)
                        .visible_worktrees(cx)
                        .any(|tree| tree.read(cx).abs_path().starts_with(solution_root))
                })
            })
            .ok()
            .unwrap_or(false);
        if matches {
            return Some(editor_mcp::format_window_id(handle.window_id()));
        }
    }
    None
}

// =====================================================================
// solutions.get
// =====================================================================

/// Get full details of a Solution by id, including any active window info.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetSolutionParams {
    pub solution_id: String,
}

impl<'de> Deserialize<'de> for GetSolutionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SolutionDetail {
    pub id: String,
    pub name: String,
    pub root: String,
    pub members: Vec<MemberDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_opened_at: Option<String>,
    pub open: bool,
    /// Path of this Solution's per-solution MCP socket. Present only while the
    /// Solution is open — same value `solutions.list` exposes. A subagent
    /// scoped to this Solution reaches `solution_agent.compact_session` (and
    /// every other solution-scoped tool) ONLY through this socket; the
    /// editor-global `mcp.sock` does not carry them. `solutions.get` is in
    /// SHARED_TOOLS, so the subagent can read this off its own per-solution
    /// socket to recover the path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_socket: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MemberDetail {
    pub catalog_id: String,
    pub local_path: String,
    pub status: String, // "ok" | "missing_on_disk"
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WindowDetail {
    pub window_id: String,
    pub focused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_buffer: Option<String>,
    pub worktree_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSolutionResult {
    pub solution: SolutionDetail,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowDetail>,
}

#[derive(Clone)]
pub struct GetSolutionTool;

impl McpServerTool for GetSolutionTool {
    type Input = GetSolutionParams;
    type Output = GetSolutionResult;
    const NAME: &'static str = "solutions.get";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let (detail, root) = cx.update(|cx| -> Result<(SolutionDetail, std::path::PathBuf)> {
            let store = SolutionStore::global(cx);
            store.read_with(cx, |s, _| {
                s.solutions()
                    .iter()
                    .find(|sol| sol.id.as_str() == input.solution_id)
                    .map(|sol| (build_detail(sol, s.is_open(&sol.id)), sol.root.clone()))
                    .with_context(|| format!("solution_not_found: {}", input.solution_id))
            })
        })?;

        let window = cx.update(|cx| build_window_detail(&root, cx));

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: detail.name.clone(),
            }],
            structured_content: GetSolutionResult {
                solution: detail,
                window,
            },
        })
    }
}

fn build_detail(sol: &Solution, open: bool) -> SolutionDetail {
    let mcp_socket = open.then(|| {
        editor_mcp::solution_socket_path(sol.id.as_str())
            .to_string_lossy()
            .into_owned()
    });
    SolutionDetail {
        id: sol.id.as_str().to_string(),
        name: sol.name.clone(),
        root: sol.root.to_string_lossy().into_owned(),
        members: sol
            .members
            .iter()
            .map(|m| {
                let exists = m.local_path.exists();
                MemberDetail {
                    catalog_id: m.catalog_id.as_str().to_string(),
                    local_path: m.local_path.to_string_lossy().into_owned(),
                    status: if exists { "ok" } else { "missing_on_disk" }.to_string(),
                }
            })
            .collect(),
        last_opened_at: sol.last_opened_at.map(|t| t.to_rfc3339()),
        open,
        mcp_socket,
    }
}

fn build_window_detail(solution_root: &std::path::Path, cx: &mut App) -> Option<WindowDetail> {
    let active_window_id = cx.active_window().map(|h| h.window_id());
    for handle in cx.windows() {
        let Some(window) = handle.downcast::<workspace::MultiWorkspace>() else {
            continue;
        };
        let detail = window
            .update(cx, |multi, _window, cx| {
                let mut worktree_paths: Vec<String> = Vec::new();
                let mut active_buffer: Option<String> = None;
                let mut matches = false;

                for ws in multi.workspaces() {
                    let workspace = ws.read(cx);
                    let project = workspace.project().read(cx);
                    for tree in project.visible_worktrees(cx) {
                        let p = tree.read(cx).abs_path().to_string_lossy().into_owned();
                        if std::path::Path::new(&p).starts_with(solution_root) {
                            matches = true;
                        }
                        worktree_paths.push(p);
                    }
                    if active_buffer.is_none() {
                        active_buffer = workspace
                            .active_item(cx)
                            .and_then(|item| item.project_path(cx))
                            .map(|pp| pp.path.as_unix_str().to_string());
                    }
                }

                if !matches {
                    return None;
                }

                Some(WindowDetail {
                    window_id: editor_mcp::format_window_id(handle.window_id()),
                    focused: active_window_id == Some(handle.window_id()),
                    active_buffer,
                    worktree_paths,
                })
            })
            .ok()
            .flatten();
        if detail.is_some() {
            return detail;
        }
    }
    None
}

// =====================================================================
// solutions.create
// =====================================================================

/// Create a new empty Solution. Generates a slug from `name`, creates the
/// on-disk root directory under `SolutionsSettings::root`, persists the new
/// entry. Returns the assigned id.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CreateSolutionParams {
    pub name: String,
}

impl<'de> Deserialize<'de> for CreateSolutionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            name: String,
        }
        Ok(Self {
            name: Option::<Inner>::deserialize(de)?.unwrap_or_default().name,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CreateSolutionResult {
    pub solution_id: String,
}

#[derive(Clone)]
pub struct CreateSolutionTool;

impl McpServerTool for CreateSolutionTool {
    type Input = CreateSolutionParams;
    type Output = CreateSolutionResult;
    const NAME: &'static str = "solutions.create";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.name.trim().is_empty(),
            "invalid_params: name is required"
        );
        let id = cx.update(|cx| -> Result<String> {
            use ::settings::Settings as _;
            let store = SolutionStore::global(cx);
            let root_base = crate::SolutionsSettings::get_global(cx).root.clone();
            let id = store.update(cx, |s, cx| s.create_solution(&input.name, root_base, cx))?;
            // Auto-open the freshly-created solution. Without this the new
            // entry stays in the closed-solutions picker only — the workspace
            // mirror (mobile + desktop) doesn't surface it, and a user who
            // just typed a name + tapped Create sees nothing change on either
            // side. `mark_open` is idempotent and emits both `Changed` +
            // `Opened` events, which fan out to the desktop window observer
            // (no-op for a fresh empty solution — no member paths yet) and
            // the mobile `workspace.solution_opened` wire delta the mirror
            // listens for.
            store.update(cx, |s, cx| s.mark_open(id.clone(), cx));
            Ok(id.as_str().to_string())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("created: {id}"),
            }],
            structured_content: CreateSolutionResult { solution_id: id },
        })
    }
}

// =====================================================================
// solutions.rename
// =====================================================================

/// Rename an existing Solution. Mutates `name` only; `id` and on-disk paths
/// are unchanged.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RenameSolutionParams {
    pub solution_id: String,
    pub new_name: String,
}

impl<'de> Deserialize<'de> for RenameSolutionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            new_name: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            new_name: inner.new_name,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RenameSolutionResult {
    pub solution_id: String,
}

#[derive(Clone)]
pub struct RenameSolutionTool;

impl McpServerTool for RenameSolutionTool {
    type Input = RenameSolutionParams;
    type Output = RenameSolutionResult;
    const NAME: &'static str = "solutions.rename";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(
            !input.new_name.trim().is_empty(),
            "invalid_params: new_name is required"
        );
        let solution_id = input.solution_id.clone();
        cx.update(|cx| -> Result<()> {
            let store = SolutionStore::global(cx);
            let id = crate::SolutionId(input.solution_id);
            store.update(cx, |s, cx| s.rename_solution(&id, &input.new_name, cx))?;
            Ok(())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("renamed: {solution_id}"),
            }],
            structured_content: RenameSolutionResult { solution_id },
        })
    }
}

// =====================================================================
// solutions.delete
// =====================================================================

/// Delete a Solution and its on-disk worktrees: removes the config entry
/// and then deletes the solution's `root` directory (mirroring the desktop
/// `delete_solution_with_cleanup`). The catalog cache is untouched, so
/// catalog-backed projects can be re-cloned later.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct DeleteSolutionParams {
    pub solution_id: String,
}

impl<'de> Deserialize<'de> for DeleteSolutionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
        }
        Ok(Self {
            solution_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .solution_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeleteSolutionResult {
    pub deleted: bool,
}

#[derive(Clone)]
pub struct DeleteSolutionTool;

impl McpServerTool for DeleteSolutionTool {
    type Input = DeleteSolutionParams;
    type Output = DeleteSolutionResult;
    const NAME: &'static str = "solutions.delete";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        let root = cx.update(|cx| -> Result<Option<std::path::PathBuf>> {
            let store = SolutionStore::global(cx);
            let id = crate::SolutionId(input.solution_id.clone());
            // Capture the root before removal so we can delete its on-disk
            // worktrees afterwards.
            let root = store.read_with(cx, |s, _| {
                s.solutions()
                    .iter()
                    .find(|sol| sol.id == id)
                    .map(|sol| sol.root.clone())
            });
            store.update(cx, |s, cx| s.delete_solution(&id, cx))?;
            Ok(root)
        })?;
        // Match the desktop delete (`delete_solution_with_cleanup`): a
        // deleted solution takes its on-disk project files with it. Done in
        // the background — a slow / partial `remove_dir_all` shouldn't block
        // the tool response, and `NotFound` (already gone) is not an error.
        if let Some(root) = root {
            let root_display = root.display().to_string();
            cx.background_executor()
                .spawn(async move {
                    // Direct `remove_dir_all` on a background-executor thread
                    // (no `smol::unblock`): unblock spawns its own OS thread,
                    // which the deterministic gpui test scheduler rejects.
                    if let Err(err) = std::fs::remove_dir_all(&root)
                        && err.kind() != std::io::ErrorKind::NotFound
                    {
                        log::warn!(
                            "solutions.delete: removing {root_display} failed: {err} (orphaned files left in place)"
                        );
                    }
                })
                .detach();
        }
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "deleted".to_string(),
            }],
            structured_content: DeleteSolutionResult { deleted: true },
        })
    }
}

// =====================================================================
// solutions.open
// =====================================================================

/// Open a Solution: collects member paths, calls `workspace::open_paths`,
/// updates `last_opened_at` (only after a successful open), returns the
/// resulting window info. `focus` is plumbed into `OpenOptions.focus`:
/// `Some(true)` requests focus, `Some(false)` requests no focus, and
/// `None` leaves the workspace's default behaviour intact.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct OpenSolutionParams {
    pub solution_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<bool>,
}

impl<'de> Deserialize<'de> for OpenSolutionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            focus: Option<bool>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            focus: inner.focus,
        })
    }
}

/// Result of `solutions.open`. `focused` reflects the FOCUS REQUEST sent to
/// the workspace (`input.focus.unwrap_or(true)`); the OS may not honor it on
/// all platforms, and we cannot synchronously observe the resulting OS
/// focus state, so the value is the request, not the post-condition.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OpenSolutionResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    pub focused: bool,
    pub opened_paths: Vec<String>,
}

#[derive(Clone)]
pub struct OpenSolutionTool;

impl McpServerTool for OpenSolutionTool {
    type Input = OpenSolutionParams;
    type Output = OpenSolutionResult;
    const NAME: &'static str = "solutions.open";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        let sol_id = crate::SolutionId(input.solution_id.clone());

        let paths = cx.update(|cx| -> Result<Vec<std::path::PathBuf>> {
            let store = SolutionStore::global(cx);
            store.read_with(cx, |s, _| s.paths_for_open(&sol_id))
        })?;

        anyhow::ensure!(
            !paths.is_empty(),
            "solution {} has no members",
            input.solution_id
        );

        // Open first; only stamp last_opened_at after the open actually
        // succeeds, so a failed open does not lie about recency.
        let (task, welcome_window) = cx.update(|cx| {
            let app_state = workspace::AppState::global(cx);
            let mut options = workspace::OpenOptions::default();
            options.focus = input.focus;
            // Phone-driven `solutions.open` should reuse an existing
            // editor window whenever possible: if the solution is
            // already open, `Activate` mode causes `open_paths`'s
            // find_existing_workspace path to pick that window and
            // surface it. If the solution isn't open yet, Activate
            // falls back to the currently-active MultiWorkspace
            // window — i.e. the user's main window. Only when no
            // MultiWorkspace exists does it spawn a fresh one. This
            // replaces the previous always-`NewWindow` behaviour,
            // which left the user with a new window per phone-driven
            // navigation. Autonomous agents that genuinely need a
            // fresh window can be added back behind an explicit
            // input.open_mode parameter when the need arises.
            options.open_mode = workspace::OpenMode::Activate;
            // Capture the launcher (if any) so we can retire it once the
            // solution window is up — matches the UI flow's behaviour
            // when clicking a row in the launcher.
            let welcome = workspace::welcome::find_existing(cx);
            (
                workspace::open_paths(&paths, app_state, options, cx),
                welcome,
            )
        });
        let open_result = task.await?;
        let window_id = editor_mcp::format_window_id(open_result.window.window_id());

        // Persist failure here is non-fatal: the open already happened and the
        // user should see a window even if we lose the recency update.
        // mark_open is also fired here because OpenMode::Activate reuses an
        // existing MultiWorkspace and so event_sources.rs::observe_new (which
        // is the canonical mark_open trigger) never fires for this add — the
        // remote client would otherwise see the new solution missing from
        // workspace.snapshot until the next desktop restart. mark_open is
        // idempotent on the HashSet, so a duplicate call from observe_new on
        // a NewWindow path no-ops.
        cx.update(|cx| {
            let store = SolutionStore::global(cx);
            store.update(cx, |s, cx| {
                s.touch_last_opened(&sol_id, cx).log_err();
                s.mark_open(sol_id.clone(), cx);
            });
            if let Some(welcome) = welcome_window {
                welcome
                    .update(cx, |_, window, _| window.remove_window())
                    .log_err();
            }
        });

        let focused = input.focus.unwrap_or(true);

        let opened_paths: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("opened: {}", input.solution_id),
            }],
            structured_content: OpenSolutionResult {
                window_id: Some(window_id),
                focused,
                opened_paths,
            },
        })
    }
}

// =====================================================================
// solutions.close
// =====================================================================

/// Close the editor window currently displaying the given Solution, if any.
/// Returns `closed: false` if no window matches (not an error).
///
/// **Warning**: forces close — does NOT prompt the user to save unsaved
/// buffers. Callers should ensure modifications are saved beforehand.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CloseSolutionParams {
    pub solution_id: String,
}

impl<'de> Deserialize<'de> for CloseSolutionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
        }
        Ok(Self {
            solution_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .solution_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CloseSolutionResult {
    pub closed: bool,
}

#[derive(Clone)]
pub struct CloseSolutionTool;

impl McpServerTool for CloseSolutionTool {
    type Input = CloseSolutionParams;
    type Output = CloseSolutionResult;
    const NAME: &'static str = "solutions.close";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        let closed = cx.update(|cx| -> Result<bool> {
            let store = SolutionStore::global(cx);
            let root = store
                .read_with(cx, |s, _| {
                    s.solutions()
                        .iter()
                        .find(|sol| sol.id.as_str() == input.solution_id)
                        .map(|sol| sol.root.clone())
                })
                .with_context(|| format!("solution_not_found: {}", input.solution_id))?;
            for handle in cx.windows() {
                let Some(window) = handle.downcast::<workspace::MultiWorkspace>() else {
                    continue;
                };
                let matched = window
                    .read_with(cx, |multi, cx| {
                        multi.workspaces().any(|ws| {
                            ws.read(cx)
                                .project()
                                .read(cx)
                                .visible_worktrees(cx)
                                .any(|tree| tree.read(cx).abs_path().starts_with(&root))
                        })
                    })
                    .ok()
                    .unwrap_or(false);
                if matched {
                    window
                        .update(cx, |_view, window, _cx| window.remove_window())
                        .log_err();
                    return Ok(true);
                }
            }
            Ok(false)
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("closed: {closed}"),
            }],
            structured_content: CloseSolutionResult { closed },
        })
    }
}

// =====================================================================
// solutions.find_for_path
// =====================================================================

/// Given an absolute filesystem path, return the Solution (if any) whose
/// root contains it. This is the same matching the title bar uses to
/// pick its Solution segment for the active worktree, exposed as a
/// pure-data tool so agents can verify the title-bar logic without
/// rendering pixels or attaching to a window.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct FindForPathParams {
    pub abs_path: String,
}

impl<'de> Deserialize<'de> for FindForPathParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            abs_path: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            abs_path: inner.abs_path,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FindForPathMatch {
    pub solution_id: String,
    pub solution_name: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FindForPathResult {
    /// `None` if no Solution's root contains `abs_path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#match: Option<FindForPathMatch>,
}

#[derive(Clone)]
pub struct FindForPathTool;

impl McpServerTool for FindForPathTool {
    type Input = FindForPathParams;
    type Output = FindForPathResult;
    const NAME: &'static str = "solutions.find_for_path";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.abs_path.is_empty(),
            "invalid_params: abs_path is required"
        );
        let path = std::path::PathBuf::from(&input.abs_path);
        let r#match = cx.update(|cx| {
            SolutionStore::try_global(cx).and_then(|store| {
                store.read_with(cx, |s, _| {
                    s.solution_for_path(&path).map(|sol| FindForPathMatch {
                        solution_id: sol.id.as_str().to_string(),
                        solution_name: sol.name.clone(),
                    })
                })
            })
        });
        let summary = match &r#match {
            Some(m) => format!("matched: {}", m.solution_name),
            None => "no match".to_string(),
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: FindForPathResult { r#match },
        })
    }
}

// =====================================================================
// catalog.list
// =====================================================================

/// List all catalog entries with their on-disk cache status.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ListCatalogParams {}

impl<'de> Deserialize<'de> for ListCatalogParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let _ = serde::de::IgnoredAny::deserialize(de)?;
        Ok(ListCatalogParams {})
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CatalogProjectInfo {
    pub id: String,
    pub name: String,
    pub remote_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// `"absent"` when no cache directory exists, `"present"` when one does.
    pub cache_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_last_fetched: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListCatalogResult {
    pub projects: Vec<CatalogProjectInfo>,
}

#[derive(Clone)]
pub struct ListCatalogTool;

impl McpServerTool for ListCatalogTool {
    type Input = ListCatalogParams;
    type Output = ListCatalogResult;
    const NAME: &'static str = "catalog.list";

    async fn run(
        &self,
        _input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let projects: Vec<CatalogProjectInfo> = cx.update(|cx| {
            let store = SolutionStore::global(cx);
            let cache_root = crate::default_cache_root();
            store.read_with(cx, |s, _| {
                s.catalog()
                    .iter()
                    .map(|p| build_catalog_info(p, &cache_root))
                    .collect()
            })
        });
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} project(s)", projects.len()),
            }],
            structured_content: ListCatalogResult { projects },
        })
    }
}

fn build_catalog_info(
    p: &crate::CatalogProject,
    cache_root: &std::path::Path,
) -> CatalogProjectInfo {
    let entry_path = crate::cache::cache_path(cache_root, &p.remote_url);
    let exists = entry_path.exists();
    let cache_last_fetched = if exists {
        std::fs::metadata(&entry_path)
            .and_then(|m| m.modified())
            .ok()
            .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
    } else {
        None
    };
    CatalogProjectInfo {
        id: p.id.as_str().to_string(),
        name: p.name.clone(),
        remote_url: p.remote_url.clone(),
        default_branch: p.default_branch.clone(),
        cache_status: if exists { "present" } else { "absent" }.to_string(),
        cache_last_fetched,
    }
}

// =====================================================================
// catalog.add_project
// =====================================================================

/// Add a new catalog entry. The id is derived from `name` (slug) and is
/// returned in `catalog_id`. `remote_url` is immutable after creation.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct AddCatalogProjectParams {
    pub name: String,
    pub remote_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

impl<'de> Deserialize<'de> for AddCatalogProjectParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            name: String,
            remote_url: String,
            default_branch: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            name: inner.name,
            remote_url: inner.remote_url,
            default_branch: inner.default_branch,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AddCatalogProjectResult {
    pub catalog_id: String,
}

#[derive(Clone)]
pub struct AddCatalogProjectTool;

impl McpServerTool for AddCatalogProjectTool {
    type Input = AddCatalogProjectParams;
    type Output = AddCatalogProjectResult;
    const NAME: &'static str = "catalog.add_project";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.name.trim().is_empty(),
            "invalid_params: name is required"
        );
        anyhow::ensure!(
            !input.remote_url.trim().is_empty(),
            "invalid_params: remote_url is required"
        );
        let id = cx.update(|cx| -> Result<String> {
            let store = SolutionStore::global(cx);
            let id = store.update(cx, |s, cx| {
                s.add_catalog_project(
                    &input.name,
                    &input.remote_url,
                    input.default_branch.clone(),
                    cx,
                )
            })?;
            Ok(id.as_str().to_string())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("added: {id}"),
            }],
            structured_content: AddCatalogProjectResult { catalog_id: id },
        })
    }
}

// =====================================================================
// catalog.remove_project
// =====================================================================

/// Remove a catalog entry. Refused (with an error) if any Solution still
/// references it; remove the member from the Solution(s) first.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RemoveCatalogProjectParams {
    pub catalog_id: String,
}

impl<'de> Deserialize<'de> for RemoveCatalogProjectParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            catalog_id: String,
        }
        Ok(Self {
            catalog_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .catalog_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RemoveCatalogProjectResult {
    pub removed: bool,
}

#[derive(Clone)]
pub struct RemoveCatalogProjectTool;

impl McpServerTool for RemoveCatalogProjectTool {
    type Input = RemoveCatalogProjectParams;
    type Output = RemoveCatalogProjectResult;
    const NAME: &'static str = "catalog.remove_project";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.catalog_id.is_empty(),
            "invalid_params: catalog_id is required"
        );
        cx.update(|cx| -> Result<()> {
            let store = SolutionStore::global(cx);
            let id = crate::CatalogId(input.catalog_id);
            store.update(cx, |s, cx| s.remove_catalog_project(&id, cx))?;
            Ok(())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "removed".to_string(),
            }],
            structured_content: RemoveCatalogProjectResult { removed: true },
        })
    }
}

// =====================================================================
// catalog.edit_project
// =====================================================================

/// Edit `name` and/or `default_branch` of a catalog entry via the MCP
/// surface. The UI modal also lets the user change `remote_url` (which
/// rewrites every existing clone's `origin`); that capability is
/// intentionally not exposed here — agent-driven URL changes would need
/// separate plumbing to confirm-or-rollback the cascading remote-rewrite,
/// and no use case has come up yet. Use the UI modal for URL edits.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct EditCatalogProjectParams {
    pub catalog_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

impl<'de> Deserialize<'de> for EditCatalogProjectParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            catalog_id: String,
            name: Option<String>,
            default_branch: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            catalog_id: inner.catalog_id,
            name: inner.name,
            default_branch: inner.default_branch,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EditCatalogProjectResult {
    pub catalog_id: String,
}

#[derive(Clone)]
pub struct EditCatalogProjectTool;

impl McpServerTool for EditCatalogProjectTool {
    type Input = EditCatalogProjectParams;
    type Output = EditCatalogProjectResult;
    const NAME: &'static str = "catalog.edit_project";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.catalog_id.is_empty(),
            "invalid_params: catalog_id is required"
        );
        let catalog_id = input.catalog_id.clone();
        cx.update(|cx| -> Result<()> {
            let store = SolutionStore::global(cx);
            let id = crate::CatalogId(input.catalog_id);
            store.update(cx, |s, cx| {
                s.edit_catalog_project(&id, input.name, input.default_branch, None, cx)
            })?;
            Ok(())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("edited: {catalog_id}"),
            }],
            structured_content: EditCatalogProjectResult { catalog_id },
        })
    }
}

// =====================================================================
// catalog.clear_cache
// =====================================================================

/// Delete the on-disk warm clone cache for one catalog entry (when
/// `catalog_id` is provided) or for every entry (when omitted). Useful
/// for autonomous test teardown and for forcing the next add_member /
/// refresh_cache to start from a fresh clone.
///
/// Synchronous: runs an `std::fs::remove_dir_all` per affected entry on
/// the calling thread. Returns the list of removed cache directories.
/// A missing directory is not an error — it counts as already cleared.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ClearCacheParams {
    /// Specific catalog entry to clear. If omitted, clears the cache for
    /// every catalog entry currently in the store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_id: Option<String>,
}

impl<'de> Deserialize<'de> for ClearCacheParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            catalog_id: Option<String>,
        }
        Ok(Self {
            catalog_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .catalog_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ClearCacheResult {
    pub removed_paths: Vec<String>,
}

#[derive(Clone)]
pub struct ClearCacheTool;

impl McpServerTool for ClearCacheTool {
    type Input = ClearCacheParams;
    type Output = ClearCacheResult;
    const NAME: &'static str = "catalog.clear_cache";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let urls = cx.update(|cx| -> Result<Vec<String>> {
            let store = SolutionStore::global(cx);
            store.read_with(cx, |s, _| {
                if let Some(id) = input.catalog_id.as_deref() {
                    let url = s
                        .catalog()
                        .iter()
                        .find(|p| p.id.as_str() == id)
                        .map(|p| p.remote_url.clone())
                        .with_context(|| format!("catalog_not_found: {id}"))?;
                    Ok(vec![url])
                } else {
                    Ok(s.catalog().iter().map(|p| p.remote_url.clone()).collect())
                }
            })
        })?;

        let cache_root = crate::default_cache_root();
        let mut removed = Vec::new();
        for url in urls {
            let path = crate::cache::cache_path(&cache_root, &url);
            if path.exists() {
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                removed.push(path.to_string_lossy().into_owned());
            }
        }

        let summary = match removed.len() {
            0 => "no cache directories to remove".to_string(),
            n => format!(
                "removed {n} cache director{}",
                if n == 1 { "y" } else { "ies" }
            ),
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: ClearCacheResult {
                removed_paths: removed,
            },
        })
    }
}

// =====================================================================
// catalog.refresh_cache
// =====================================================================

/// Refresh the on-disk cache for a catalog entry by running `git fetch`
/// (or cloning if the cache is absent). Returns an `operation_id`
/// immediately; the work is spawned in the background and progress can be
/// polled via `editor.get_operation`.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RefreshCacheParams {
    pub catalog_id: String,
}

impl<'de> Deserialize<'de> for RefreshCacheParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            catalog_id: String,
        }
        Ok(Self {
            catalog_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .catalog_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefreshCacheResult {
    pub operation_id: String,
}

#[derive(Clone)]
pub struct RefreshCacheTool;

impl McpServerTool for RefreshCacheTool {
    type Input = RefreshCacheParams;
    type Output = RefreshCacheResult;
    const NAME: &'static str = "catalog.refresh_cache";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.catalog_id.is_empty(),
            "invalid_params: catalog_id is required"
        );
        let remote_url = cx.update(|cx| -> Result<String> {
            let store = SolutionStore::global(cx);
            let url = store.read_with(cx, |s, _| {
                s.catalog()
                    .iter()
                    .find(|p| p.id.as_str() == input.catalog_id)
                    .map(|p| p.remote_url.clone())
            });
            url.with_context(|| format!("catalog_not_found: {}", input.catalog_id))
        })?;

        let operation_id = cx.update(|cx| editor_mcp::op_start("catalog.refresh_cache", cx));

        let op_id_for_task = operation_id.clone();
        let catalog_id_for_log = input.catalog_id.clone();
        let cache_root = crate::default_cache_root();

        cx.spawn(async move |cx| {
            cx.update(|cx| {
                editor_mcp::op_record_progress(
                    &op_id_for_task,
                    "fetching".to_string(),
                    Some(0),
                    cx,
                );
            });

            // Note: the progress callback runs synchronously inside the future
            // and has no App handle, so intermediate progress updates can't
            // call op_record_progress here. We only record the initial state.
            let result = crate::cache::refresh_cache(&cache_root, &remote_url, |_| {}).await;

            cx.update(|cx| match result {
                Ok(_) => {
                    editor_mcp::op_complete_ok(
                        &op_id_for_task,
                        serde_json::json!({ "catalog_id": catalog_id_for_log }),
                        cx,
                    );
                }
                Err(err) => {
                    editor_mcp::op_complete_err(&op_id_for_task, err.to_string(), cx);
                }
            });
        })
        .detach();

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("queued refresh_cache: {}", input.catalog_id),
            }],
            structured_content: RefreshCacheResult { operation_id },
        })
    }
}

// =====================================================================
// solutions.add_member
// =====================================================================

/// Add a catalog project as a member of a Solution. Clones the project into
/// the Solution's root (using cached source if available) and registers it.
/// Returns `operation_id` immediately; the clone is spawned in the
/// background and progress can be polled via `editor.get_operation`.
///
/// **Slow**: cloning can take seconds-to-minutes for large repos.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct AddMemberParams {
    pub solution_id: String,
    pub catalog_id: String,
}

impl<'de> Deserialize<'de> for AddMemberParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            catalog_id: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            catalog_id: inner.catalog_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AddMemberResult {
    pub operation_id: String,
}

#[derive(Clone)]
pub struct AddMemberTool;

impl McpServerTool for AddMemberTool {
    type Input = AddMemberParams;
    type Output = AddMemberResult;
    const NAME: &'static str = "solutions.add_member";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(
            !input.catalog_id.is_empty(),
            "invalid_params: catalog_id is required"
        );

        let sol_id = crate::SolutionId(input.solution_id.clone());
        let cat_id = crate::CatalogId(input.catalog_id.clone());
        let cache_root = crate::default_cache_root();

        let operation_id = cx.update(|cx| editor_mcp::op_start("solutions.add_member", cx));

        let op_id_for_task = operation_id.clone();
        let solution_id_for_log = input.solution_id.clone();
        let catalog_id_for_log = input.catalog_id.clone();

        cx.spawn(async move |cx| {
            // Forward every git progress tick to op_record_progress so the
            // operation's published `operation_progress` notifications stay
            // in sync with what the in-process store events broadcast.
            let op_id_for_cb = op_id_for_task.clone();
            let on_progress: crate::add_member::AddProgressCallback = Box::new(
                move |stage: &str, percent: Option<u8>, app: &mut gpui::App| {
                    editor_mcp::op_record_progress(&op_id_for_cb, stage.to_string(), percent, app);
                },
            );

            let task = cx.update(|cx| {
                let store = SolutionStore::global(cx);
                store.update(cx, |s, cx| {
                    s.add_member_with_progress(sol_id, cat_id, cache_root, on_progress, cx)
                })
            });
            let result = task.await;

            cx.update(|cx| match result {
                Ok(()) => {
                    editor_mcp::op_complete_ok(
                        &op_id_for_task,
                        serde_json::json!({
                            "solution_id": solution_id_for_log,
                            "catalog_id": catalog_id_for_log,
                        }),
                        cx,
                    );
                }
                Err(err) => {
                    editor_mcp::op_complete_err(&op_id_for_task, err.to_string(), cx);
                }
            });
        })
        .detach();

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "queued add_member: {}/{}",
                    input.solution_id, input.catalog_id
                ),
            }],
            structured_content: AddMemberResult { operation_id },
        })
    }
}

// =====================================================================
// solutions.add_empty_member
// =====================================================================

/// Create a new empty project as a member of a Solution. Creates the
/// directory `solution.root/<slug>` (slug derived from `name` and
/// uniquified against existing members), `git init`s it with no remote so
/// history can be pushed somewhere later, and registers it — no clone. The
/// member never enters the catalog, so a remote-less local project is not
/// offered in the picker for other solutions. Returns the new member's
/// `catalog_id` synchronously.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct AddEmptyMemberParams {
    pub solution_id: String,
    pub name: String,
}

impl<'de> Deserialize<'de> for AddEmptyMemberParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            name: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            name: inner.name,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AddEmptyMemberResult {
    pub catalog_id: String,
}

#[derive(Clone)]
pub struct AddEmptyMemberTool;

impl McpServerTool for AddEmptyMemberTool {
    type Input = AddEmptyMemberParams;
    type Output = AddEmptyMemberResult;
    const NAME: &'static str = "solutions.add_empty_member";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(
            !input.name.trim().is_empty(),
            "invalid_params: name is required"
        );

        let sol_id = crate::SolutionId(input.solution_id.clone());
        let cat_id = cx.update(|cx| -> Result<crate::CatalogId> {
            let store = SolutionStore::global(cx);
            store.update(cx, |s, cx| s.add_empty_member(&sol_id, &input.name, cx))
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: cat_id.0.clone(),
            }],
            structured_content: AddEmptyMemberResult {
                catalog_id: cat_id.0,
            },
        })
    }
}

// =====================================================================
// solutions.remove_member
// =====================================================================

/// Remove a member from a Solution. Config-only: the on-disk worktree
/// directory is NOT deleted; the user can re-add later by `add_member`
/// (the existing dir will be reused if origin matches).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RemoveMemberParams {
    pub solution_id: String,
    pub catalog_id: String,
}

impl<'de> Deserialize<'de> for RemoveMemberParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            catalog_id: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            catalog_id: inner.catalog_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RemoveMemberResult {
    pub removed: bool,
}

#[derive(Clone)]
pub struct RemoveMemberTool;

impl McpServerTool for RemoveMemberTool {
    type Input = RemoveMemberParams;
    type Output = RemoveMemberResult;
    const NAME: &'static str = "solutions.remove_member";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(
            !input.catalog_id.is_empty(),
            "invalid_params: catalog_id is required"
        );
        cx.update(|cx| -> Result<()> {
            let store = SolutionStore::global(cx);
            let sol_id = crate::SolutionId(input.solution_id);
            let cat_id = crate::CatalogId(input.catalog_id);
            store.update(cx, |s, cx| s.remove_member(&sol_id, &cat_id, cx))?;
            Ok(())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "removed".to_string(),
            }],
            structured_content: RemoveMemberResult { removed: true },
        })
    }
}

// =====================================================================
// solutions.reorder_members
// =====================================================================

/// Reorder Solution members. The new order MUST contain exactly the same
/// catalog_ids as the current member list (same set, different order).
/// Order matters — the first member becomes the agent CWD.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ReorderMembersParams {
    pub solution_id: String,
    pub ordered_catalog_ids: Vec<String>,
}

impl<'de> Deserialize<'de> for ReorderMembersParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            ordered_catalog_ids: Vec<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            ordered_catalog_ids: inner.ordered_catalog_ids,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReorderMembersResult {
    pub ok: bool,
}

#[derive(Clone)]
pub struct ReorderMembersTool;

impl McpServerTool for ReorderMembersTool {
    type Input = ReorderMembersParams;
    type Output = ReorderMembersResult;
    const NAME: &'static str = "solutions.reorder_members";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        cx.update(|cx| -> Result<()> {
            let store = SolutionStore::global(cx);
            let sol_id = crate::SolutionId(input.solution_id);
            let order: Vec<crate::CatalogId> = input
                .ordered_catalog_ids
                .into_iter()
                .map(crate::CatalogId)
                .collect();
            store.update(cx, |s, cx| s.reorder_members(&sol_id, order, cx))?;
            Ok(())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "reordered".to_string(),
            }],
            structured_content: ReorderMembersResult { ok: true },
        })
    }
}

// =====================================================================
// solutions.set_active_member
// =====================================================================

/// Set the solution-wide active member (the selected project tab). Emits
/// `ActiveMemberChanged`, which drives the per-member layout swap and the
/// project-panel tree rebuild — the same path a project-tab click triggers.
/// No-op if `catalog_id` is already the active member.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SetActiveMemberParams {
    pub solution_id: String,
    pub catalog_id: String,
}

impl<'de> Deserialize<'de> for SetActiveMemberParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            catalog_id: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            catalog_id: inner.catalog_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SetActiveMemberResult {
    pub solution_id: String,
    pub active_member: String,
}

#[derive(Clone)]
pub struct SetActiveMemberTool;

impl McpServerTool for SetActiveMemberTool {
    type Input = SetActiveMemberParams;
    type Output = SetActiveMemberResult;
    const NAME: &'static str = "solutions.set_active_member";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(
            !input.catalog_id.is_empty(),
            "invalid_params: catalog_id is required"
        );
        let (solution_id, catalog_id) = (input.solution_id.clone(), input.catalog_id.clone());
        cx.update(|cx| -> Result<()> {
            let store = SolutionStore::global(cx);
            let sol = crate::SolutionId(input.solution_id);
            let cat = crate::CatalogId(input.catalog_id);
            // Guard against recording a bogus active member: the catalog must be
            // an actual member of the solution (a non-member would leave the
            // window pointing at a project with no worktree).
            let is_member = store.read(cx).solutions().iter().find(|s| s.id == sol).is_some_and(
                |s| s.members.iter().any(|m| m.catalog_id == cat),
            );
            anyhow::ensure!(
                is_member,
                "not_found: catalog_id is not a member of the solution"
            );
            store.update(cx, |s, cx| s.set_active_member(sol, cat, cx));
            Ok(())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("active_member: {solution_id} -> {catalog_id}"),
            }],
            structured_content: SetActiveMemberResult {
                solution_id,
                active_member: catalog_id,
            },
        })
    }
}

// =====================================================================
// workspace.list_buffers
// =====================================================================

/// List open buffers in the editor window for a Solution. Each entry
/// reports the project-relative `path`, dirty/focused flags, and (when
/// available) the language name. Buffers from every pane in the window
/// are returned; a single buffer open in multiple panes appears once
/// per pane (matching the editor UI). Returns an empty list when no
/// window is currently open for the Solution.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ListBuffersParams {
    pub solution_id: String,
}

impl<'de> Deserialize<'de> for ListBuffersParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
        }
        Ok(Self {
            solution_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .solution_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BufferInfo {
    pub path: String,
    pub dirty: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub focused: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListBuffersResult {
    pub buffers: Vec<BufferInfo>,
}

#[derive(Clone)]
pub struct ListBuffersTool;

impl McpServerTool for ListBuffersTool {
    type Input = ListBuffersParams;
    type Output = ListBuffersResult;
    const NAME: &'static str = "workspace.list_buffers";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        let buffers = cx.update(|cx| collect_buffers(&input.solution_id, cx));
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} buffer(s)", buffers.len()),
            }],
            structured_content: ListBuffersResult { buffers },
        })
    }
}

fn collect_buffers(solution_id: &str, cx: &mut App) -> Vec<BufferInfo> {
    let Some(store) = SolutionStore::try_global(cx) else {
        return Vec::new();
    };
    let Some(root) = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .find(|sol| sol.id.as_str() == solution_id)
            .map(|sol| sol.root.clone())
    }) else {
        return Vec::new();
    };

    for handle in cx.windows() {
        let Some(window_handle) = handle.downcast::<workspace::MultiWorkspace>() else {
            continue;
        };
        let collected = window_handle
            .update(cx, |multi, _window, cx| {
                let workspace = multi.workspace().read(cx);
                let project = workspace.project().read(cx);
                let matches_solution = project
                    .visible_worktrees(cx)
                    .any(|tree| tree.read(cx).abs_path().starts_with(&root))
                    || multi.workspaces().any(|ws| {
                        ws.read(cx)
                            .project()
                            .read(cx)
                            .visible_worktrees(cx)
                            .any(|tree| tree.read(cx).abs_path().starts_with(&root))
                    });
                if !matches_solution {
                    return None;
                }

                // The active item resolves through the active pane; capture its
                // project_path so we can flag exactly the entry the user is
                // currently looking at, even if the same buffer is open in
                // another pane.
                let active_project_path = workspace
                    .active_item(cx)
                    .and_then(|item| item.project_path(cx));
                let active_pane_id = workspace.active_pane().entity_id();

                let mut buffers = Vec::new();
                for pane_entity in workspace.panes() {
                    let pane = pane_entity.read(cx);
                    let pane_is_active = pane_entity.entity_id() == active_pane_id;
                    let pane_active_item_id = pane.active_item().map(|item| item.item_id());
                    for item in pane.items() {
                        let Some(project_path) = item.project_path(cx) else {
                            continue;
                        };
                        let is_active_in_pane = pane_active_item_id == Some(item.item_id());
                        let focused = pane_is_active
                            && is_active_in_pane
                            && active_project_path
                                .as_ref()
                                .map(|p| p == &project_path)
                                .unwrap_or(true);
                        buffers.push(BufferInfo {
                            path: project_path.path.as_unix_str().to_string(),
                            dirty: item.is_dirty(cx),
                            // Language detection requires `Buffer` access via
                            // `act_as::<Editor>` and is left for a follow-up;
                            // the field is reserved in the schema so clients
                            // can rely on the shape today.
                            language: None,
                            focused,
                        });
                    }
                }
                Some(buffers)
            })
            .ok()
            .flatten();
        if let Some(buffers) = collected {
            return buffers;
        }
    }
    Vec::new()
}

// =====================================================================
// workspace.get_effective_settings
// =====================================================================

/// Get effective editor settings for a Solution as a JSON object. v1
/// returns the merged `SettingsContent` (default + user + profile)
/// without per-path scoping; the optional `path` argument is reserved
/// for a future revision that will resolve project-local + editorconfig
/// overrides via `SettingsLocation`. Today, supplying `path` is accepted
/// but does not change the response.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetEffectiveSettingsParams {
    pub solution_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl<'de> Deserialize<'de> for GetEffectiveSettingsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            path: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            path: inner.path,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetEffectiveSettingsResult {
    pub settings: serde_json::Value,
}

#[derive(Clone)]
pub struct GetEffectiveSettingsTool;

impl McpServerTool for GetEffectiveSettingsTool {
    type Input = GetEffectiveSettingsParams;
    type Output = GetEffectiveSettingsResult;
    const NAME: &'static str = "workspace.get_effective_settings";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        let settings = cx.update(|cx| -> serde_json::Value {
            // `merged_settings` returns the default+user+profile-resolved view.
            // Path-scoped resolution requires `SettingsLocation`, which we
            // don't have a clean surface for from the MCP layer yet; leaving
            // the `path` parameter as an explicit no-op is preferable to
            // silently returning the wrong scope.
            let store = cx.global::<::settings::SettingsStore>();
            serde_json::to_value(store.merged_settings()).unwrap_or(serde_json::Value::Null)
        });
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "settings".to_string(),
            }],
            structured_content: GetEffectiveSettingsResult { settings },
        })
    }
}

// =====================================================================
// workspace.dispatch_action
// =====================================================================

/// Dispatch a registered action to the editor window for a Solution.
/// Action name is the fully-qualified path like `workspace::ToggleLeftDock`.
/// Optional `args` are deserialized into the action's payload type.
///
/// Note: returns `dispatched: true` once the action was successfully
/// built and queued onto the window's dispatcher. The dispatch itself
/// runs on a later tick; this tool does NOT report whether a handler
/// eventually fired or refused the action. Returns `dispatched: false`
/// when no window is currently open for the Solution.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct DispatchActionParams {
    pub solution_id: String,
    pub action_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
}

impl<'de> Deserialize<'de> for DispatchActionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            action_name: String,
            args: Option<serde_json::Value>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            action_name: inner.action_name,
            args: inner.args,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DispatchActionResult {
    pub dispatched: bool,
}

#[derive(Clone)]
pub struct DispatchActionTool;

impl McpServerTool for DispatchActionTool {
    type Input = DispatchActionParams;
    type Output = DispatchActionResult;
    const NAME: &'static str = "workspace.dispatch_action";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(
            !input.action_name.is_empty(),
            "invalid_params: action_name is required"
        );
        let action_name = input.action_name.clone();
        let dispatched = cx.update(|cx| -> Result<bool> {
            let Some(handle) = find_window_for_solution(&input.solution_id, cx) else {
                return Ok(false);
            };
            // Build the action up-front so a deserialization error surfaces
            // before we touch the window. Once built, dispatch is infallible
            // — the window itself routes the action through its keybinding
            // and focus tree on a later tick.
            let action = cx
                .build_action(&input.action_name, input.args.clone())
                .map_err(|err| anyhow::anyhow!("build_action({}): {err}", input.action_name))?;
            handle
                .update(cx, |_view, window, cx| {
                    window.dispatch_action(action, cx);
                })
                .map_err(|err| anyhow::anyhow!("dispatch_action failed: {err}"))?;
            Ok(true)
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("dispatched: {action_name} ({dispatched})"),
            }],
            structured_content: DispatchActionResult { dispatched },
        })
    }
}

fn find_window_for_solution(solution_id: &str, cx: &mut App) -> Option<gpui::AnyWindowHandle> {
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
        let matches_solution = window_handle
            .read_with(cx, |multi, cx| {
                multi.workspaces().any(|ws| {
                    ws.read(cx)
                        .project()
                        .read(cx)
                        .visible_worktrees(cx)
                        .any(|tree| tree.read(cx).abs_path().starts_with(&root))
                })
            })
            .ok()
            .unwrap_or(false);
        if matches_solution {
            return Some(handle);
        }
    }
    None
}

// =====================================================================
// workspace.screenshot
// =====================================================================

/// Capture a screenshot of an editor window. Identify the window either by
/// `solution_id` (a Solution's main window) OR by `window_id` (from
/// `windows.list` — needed for non-Solution top-level windows like the Welcome
/// launcher). Exactly one of the two must be supplied. Returns the image as
/// base64-encoded data, with default JPEG quality 80 for token efficiency. Use
/// `format: "png"` for pixel-perfect captures.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ScreenshotParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solution_id: Option<String>,
    /// A `window:N` id from `windows.list`. Use this to screenshot a window
    /// that isn't a Solution workspace (e.g. the Welcome launcher).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    /// Image format: "jpeg" (default), "png", or "webp".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Quality 1..=100 for jpeg/webp (ignored for png). Default: 80.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    /// Optional max dimension; if either width or height exceeds this,
    /// the image is downscaled while preserving aspect ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_dimension: Option<u32>,
}

impl<'de> Deserialize<'de> for ScreenshotParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: Option<String>,
            window_id: Option<String>,
            format: Option<String>,
            quality: Option<u8>,
            max_dimension: Option<u32>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            window_id: inner.window_id,
            format: inner.format,
            quality: inner.quality,
            max_dimension: inner.max_dimension,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ScreenshotResult {
    pub width: u32,
    pub height: u32,
    pub media_type: String,
    /// Base64-encoded image bytes.
    pub base64_data: String,
}

#[derive(Clone)]
pub struct ScreenshotTool;

impl McpServerTool for ScreenshotTool {
    type Input = ScreenshotParams;
    type Output = ScreenshotResult;
    const NAME: &'static str = "workspace.screenshot";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let solution_id = input.solution_id.clone().filter(|s| !s.is_empty());
        let window_id = input.window_id.clone().filter(|s| !s.is_empty());
        anyhow::ensure!(
            solution_id.is_some() != window_id.is_some(),
            "invalid_params: provide exactly one of solution_id or window_id"
        );
        let format = input
            .format
            .as_deref()
            .unwrap_or("jpeg")
            .to_ascii_lowercase();
        let quality = input.quality.unwrap_or(80).clamp(1, 100);

        let rgba = cx.update(|cx| -> anyhow::Result<image::RgbaImage> {
            let handle = if let Some(solution_id) = solution_id.as_deref() {
                find_window_for_solution(solution_id, cx)
                    .ok_or_else(|| anyhow::anyhow!("solution_not_open: {solution_id}"))?
            } else {
                let window_id = window_id.as_deref().unwrap_or_default();
                cx.windows()
                    .into_iter()
                    .find(|h| editor_mcp::format_window_id(h.window_id()) == window_id)
                    .ok_or_else(|| anyhow::anyhow!("window_not_found: {window_id}"))?
            };
            render_window_to_image(handle, cx)
        })?;

        let (orig_w, orig_h) = rgba.dimensions();
        let resized = if let Some(max_dim) = input.max_dimension {
            let max_side = orig_w.max(orig_h);
            if max_side > max_dim {
                let scale = max_dim as f32 / max_side as f32;
                let new_w = ((orig_w as f32 * scale).round() as u32).max(1);
                let new_h = ((orig_h as f32 * scale).round() as u32).max(1);
                image::imageops::resize(&rgba, new_w, new_h, image::imageops::FilterType::Lanczos3)
            } else {
                rgba
            }
        } else {
            rgba
        };

        let mut buf: Vec<u8> = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        let media_type: &'static str = match format.as_str() {
            "png" => {
                resized
                    .write_to(&mut cursor, image::ImageFormat::Png)
                    .with_context(|| "encode png")?;
                "image/png"
            }
            "webp" => {
                resized
                    .write_to(&mut cursor, image::ImageFormat::WebP)
                    .with_context(|| "encode webp")?;
                "image/webp"
            }
            "jpeg" | "jpg" => {
                let dyn_image = image::DynamicImage::ImageRgba8(resized.clone());
                let rgb = dyn_image.to_rgb8();
                let mut encoder =
                    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, quality);
                encoder.encode_image(&rgb).with_context(|| "encode jpeg")?;
                "image/jpeg"
            }
            other => anyhow::bail!("unsupported_format: {other}"),
        };

        use base64::Engine as _;
        let base64_data = base64::engine::general_purpose::STANDARD.encode(&buf);
        let width = resized.width();
        let height = resized.height();

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Image {
                data: base64_data.clone(),
                mime_type: media_type.to_string(),
            }],
            structured_content: ScreenshotResult {
                width,
                height,
                media_type: media_type.to_string(),
                base64_data,
            },
        })
    }
}

// SPK fork: `gpui::Window::render_to_image` is now ungated, and the wgpu
// renderer (`gpui_wgpu`) implements offscreen render-to-image for the Linux
// X11/Wayland backends, so this works in normal builds. Backends that don't
// implement it (e.g. the headless test platform with no `HeadlessRenderer`)
// still surface an error from `render_to_image` itself.
fn render_window_to_image(
    handle: gpui::AnyWindowHandle,
    cx: &mut App,
) -> anyhow::Result<image::RgbaImage> {
    handle
        .update(cx, |_view, window, _cx| window.render_to_image())
        .map_err(|err| anyhow::anyhow!("render_to_image failed: {err}"))?
}

// =====================================================================
// workspace.dump_visual_structure
// =====================================================================

/// Dump a logical tree of the editor window for a Solution. Returns a
/// hierarchical view of `Workspace` -> `TitleBar` / `Dock(side)` /
/// `PaneArea` / `Pane` / `Tab` / `StatusBar` nodes with visibility and
/// focus state.
///
/// This is a logical structure (suitable for assertions like "which
/// pane is focused"), NOT the full GPUI element tree.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct DumpVisualStructureParams {
    pub solution_id: String,
}

impl<'de> Deserialize<'de> for DumpVisualStructureParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VisualNode {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub visible: bool,
    pub focused: bool,
    pub children: Vec<VisualNode>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DumpVisualStructureResult {
    pub tree: VisualNode,
    /// Hitboxes from the most recently rendered frame, cross-referenced
    /// against the `VisualNode` tree where the deepest enclosing node
    /// (by bounds containment) can lend its `kind` / `label`. Anonymous
    /// clickables (no labelled ancestor) are still emitted so an agent
    /// can fall back on click-by-coordinates if needed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clickables: Vec<workspace::mcp::clickables::Clickable>,
}

#[derive(Clone)]
pub struct DumpVisualStructureTool;

impl McpServerTool for DumpVisualStructureTool {
    type Input = DumpVisualStructureParams;
    type Output = DumpVisualStructureResult;
    const NAME: &'static str = "workspace.dump_visual_structure";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        let (tree, clickables) = cx
            .update(|cx| build_visual_tree(&input.solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "structure for {} ({} clickables)",
                    input.solution_id,
                    clickables.len()
                ),
            }],
            structured_content: DumpVisualStructureResult { tree, clickables },
        })
    }
}

// =====================================================================
// windows.dump_visual_structure
// =====================================================================

/// Like `workspace.dump_visual_structure` but keyed by `window_id`
/// rather than solution. Lets agents introspect any window — including
/// the welcome window where `solutions.find_for_path` does not apply
/// and modals belonging to no solution can still be observed.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct DumpWindowStructureParams {
    pub window_id: String,
}

impl<'de> Deserialize<'de> for DumpWindowStructureParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            window_id: String,
        }
        Ok(Self {
            window_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .window_id,
        })
    }
}

#[derive(Clone)]
pub struct DumpWindowStructureTool;

impl McpServerTool for DumpWindowStructureTool {
    type Input = DumpWindowStructureParams;
    type Output = DumpVisualStructureResult;
    const NAME: &'static str = "windows.dump_visual_structure";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.window_id.is_empty(),
            "invalid_params: window_id is required"
        );
        let (tree, clickables) = cx.update(
            |cx| -> anyhow::Result<(VisualNode, Vec<workspace::mcp::clickables::Clickable>)> {
                let handle = cx
                    .windows()
                    .into_iter()
                    .find(|h| editor_mcp::format_window_id(h.window_id()) == input.window_id)
                    .with_context(|| format!("window_not_found: {}", input.window_id))?;
                build_visual_tree_for_window(handle, cx)
                    .with_context(|| format!("window_not_multi_workspace: {}", input.window_id))
            },
        )?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "structure for {} ({} clickables)",
                    input.window_id,
                    clickables.len()
                ),
            }],
            structured_content: DumpVisualStructureResult { tree, clickables },
        })
    }
}

fn build_visual_tree(
    solution_id: &str,
    cx: &mut App,
) -> Option<(VisualNode, Vec<workspace::mcp::clickables::Clickable>)> {
    let handle = find_window_for_solution(solution_id, cx)?;
    build_visual_tree_for_window(handle, cx)
}

fn build_visual_tree_for_window(
    handle: gpui::AnyWindowHandle,
    cx: &mut App,
) -> Option<(VisualNode, Vec<workspace::mcp::clickables::Clickable>)> {
    let window_handle = handle.downcast::<workspace::MultiWorkspace>()?;
    window_handle
        .update(cx, |multi, window, cx| {
            let tree = build_workspace_node(multi, cx);
            let window_id = window.window_handle().window_id();
            let clickables = enrich_clickables(
                workspace::mcp::clickables::enumerate_window_clickables(window_id, window),
                window_id,
                &tree,
            );
            (tree, clickables)
        })
        .ok()
}

/// Cross-reference each clickable against the visual tree by trying to
/// match a known logical region (`TitleBar` / `Dock(left|right|bottom)` /
/// `PaneArea` / `StatusBar` / `Modal(...)`) — the tree node whose
/// position is fixed by the workspace layout. Phase 1 surfaces every
/// hitbox even when no node matches (`kind` / `label` left `None`) so
/// the agent can still fall back on `click_at` with the bounds.
///
/// Once `VisualNode` carries actual `Bounds<Pixels>` from the rendered
/// frame (phase 2), this function will deepest-enclosing-match every
/// node — see follow-up.
fn enrich_clickables(
    mut clickables: Vec<workspace::mcp::clickables::Clickable>,
    window_id: gpui::WindowId,
    _tree: &VisualNode,
) -> Vec<workspace::mcp::clickables::Clickable> {
    // Phase-1 placeholder: the synthetic visual tree carries no bounds,
    // so we can't reliably map a hitbox to a node yet. Leaving kind/label
    // as None is correct — `windows.click_id` only needs the hash, which
    // is computed from `(window_id, "", "", bounds_rounded)` in the
    // anonymous case and stays stable across redraws.
    //
    // We still recompute IDs here (using the final kind/label) so any
    // future enrichment slots in without breaking the click_id contract.
    for clickable in clickables.iter_mut() {
        clickable.id = workspace::mcp::clickables::stable_id(
            window_id,
            clickable.kind.as_deref(),
            clickable.label.as_deref(),
            clickable.bounds,
        );
    }
    clickables
}

/// Synthesize what the TitleBar would render for this workspace, without
/// reaching into the title_bar crate's internals. Mirrors the matching
/// logic in `crates/title_bar/src/title_bar.rs::TitleBar::active_solution`
/// + `effective_active_worktree`. Surfacing this here lets autonomous
/// agents assert on the title bar's semantic content via dump_visual_structure
/// without needing real-pixel rendering.
fn build_title_bar_node(workspace: &workspace::Workspace, cx: &App) -> VisualNode {
    let project = workspace.project().read(cx);
    let mut children: Vec<VisualNode> = Vec::new();

    // Pick the same worktree the title bar would: the one owning the
    // active repository, falling back to the first visible worktree.
    let active_worktree = project
        .active_repository(cx)
        .and_then(|repo| {
            let repo_path = repo.read(cx).work_directory_abs_path.clone();
            project.visible_worktrees(cx).find(|tree| {
                let p = tree.read(cx).abs_path();
                *p == *repo_path || p.starts_with(repo_path.as_ref())
            })
        })
        .or_else(|| project.visible_worktrees(cx).next());
    let active_worktree_path = active_worktree
        .as_ref()
        .map(|tree| tree.read(cx).abs_path());

    if let Some(path) = &active_worktree_path
        && let Some(store) = SolutionStore::try_global(cx)
    {
        let segment = store.read_with(cx, |s, _| {
            s.solution_for_path(path).map(|sol| sol.name.clone())
        });
        if let Some(name) = segment {
            children.push(VisualNode {
                kind: "SolutionSegment".to_string(),
                label: Some(name),
                visible: true,
                focused: false,
                children: Vec::new(),
            });
        }
    }

    if let Some(tree) = active_worktree {
        let tree = tree.read(cx);
        let name = tree.root_name_str().to_string();
        if !name.is_empty() {
            children.push(VisualNode {
                kind: "ProjectName".to_string(),
                label: Some(name),
                visible: true,
                focused: false,
                children: Vec::new(),
            });
        }
    }

    if let Some(repo) = project.active_repository(cx) {
        let repo = repo.read(cx);
        if let Some(branch) = repo.branch.as_ref().map(|b| b.name().to_string()) {
            children.push(VisualNode {
                kind: "Branch".to_string(),
                label: Some(branch),
                visible: true,
                focused: false,
                children: Vec::new(),
            });
        }
    }

    VisualNode {
        kind: "TitleBar".to_string(),
        label: None,
        visible: true,
        focused: false,
        children,
    }
}

/// Synthesize what the StatusBar would render for this workspace.
/// Currently surfaces the SolutionsStatusItem widget content
/// (`<solution name>`, `<member count>`); other status bar widgets
/// remain opaque for now.
fn build_status_bar_node(workspace: &workspace::Workspace, cx: &App) -> VisualNode {
    let mut children: Vec<VisualNode> = Vec::new();

    if let Some(store) = SolutionStore::try_global(cx) {
        let project = workspace.project().read(cx);
        let mut matching: Option<(String, usize)> = None;
        for tree in project.worktrees(cx) {
            let path = tree.read(cx).abs_path();
            let solution = store.read_with(cx, |s, _| {
                s.solution_for_path(&path)
                    .map(|sol| (sol.name.clone(), sol.members.len()))
            });
            if let Some(found) = solution {
                matching = Some(found);
                break;
            }
        }
        if let Some((name, count)) = matching {
            children.push(VisualNode {
                kind: "SolutionsStatusItem".to_string(),
                label: Some(format!("● {name} · {count} projects")),
                visible: true,
                focused: false,
                children: Vec::new(),
            });
        }
    }

    VisualNode {
        kind: "StatusBar".to_string(),
        label: None,
        visible: workspace.status_bar_visible(cx),
        focused: false,
        children,
    }
}

fn build_workspace_node(multi: &workspace::MultiWorkspace, cx: &App) -> VisualNode {
    let workspace = multi.workspace().read(cx);
    let mut children = vec![
        build_title_bar_node(workspace, cx),
        build_dock_node("left", workspace.left_dock(), cx),
        build_pane_area_node(workspace, cx),
        build_dock_node("right", workspace.right_dock(), cx),
        build_dock_node("bottom", workspace.bottom_dock(), cx),
        build_status_bar_node(workspace, cx),
    ];

    if let Some(modal) = build_modal_node(workspace, cx) {
        children.push(modal);
    }

    VisualNode {
        kind: "Workspace".to_string(),
        label: None,
        visible: true,
        focused: false,
        children,
    }
}

fn build_dock_node(side: &str, dock: &gpui::Entity<workspace::dock::Dock>, cx: &App) -> VisualNode {
    let dock = dock.read(cx);
    let is_open = dock.is_open();
    let active_panel_label = dock
        .active_panel()
        .map(|panel| panel.persistent_name().to_string());

    let panel_node = active_panel_label.map(|name| VisualNode {
        kind: "Panel".to_string(),
        label: Some(name),
        visible: is_open,
        focused: false,
        children: Vec::new(),
    });

    VisualNode {
        kind: format!("Dock({side})"),
        label: None,
        visible: is_open,
        focused: false,
        children: panel_node.into_iter().collect(),
    }
}

fn build_pane_area_node(workspace: &workspace::Workspace, cx: &App) -> VisualNode {
    let active_pane_id = workspace.active_pane().entity_id();
    let pane_children: Vec<VisualNode> = workspace
        .panes()
        .iter()
        .map(|pane_entity| {
            let pane_is_active = pane_entity.entity_id() == active_pane_id;
            let pane = pane_entity.read(cx);
            let active_item_id = pane.active_item().map(|item| item.item_id());
            let tabs: Vec<VisualNode> = pane
                .items()
                .map(|item| {
                    let label = item
                        .project_path(cx)
                        .map(|p| p.path.as_unix_str().to_string())
                        .unwrap_or_else(|| item.tab_content_text(0, cx).to_string());
                    let is_active = active_item_id
                        .map(|id| id == item.item_id())
                        .unwrap_or(false);
                    VisualNode {
                        kind: format!("Tab({label})"),
                        label: Some(label),
                        visible: true,
                        focused: is_active,
                        children: Vec::new(),
                    }
                })
                .collect();

            VisualNode {
                kind: "Pane".to_string(),
                label: None,
                visible: true,
                focused: pane_is_active,
                children: tabs,
            }
        })
        .collect();

    VisualNode {
        kind: "PaneArea".to_string(),
        label: None,
        visible: true,
        focused: false,
        children: pane_children,
    }
}

/// Surface the active modal as a `Modal(<kind>)` leaf so introspection
/// tools can verify which modal is open. The kind comes from
/// [`workspace::ModalView::debug_kind`] — solutions modals override it
/// with stable strings (`"NewSolution"`, `"AddCatalogProject"`,
/// `"OpenSolution"`, `"AddMember"`); generic upstream modals fall back
/// to `"Modal"`.
fn build_modal_node(workspace: &workspace::Workspace, cx: &App) -> Option<VisualNode> {
    let kind = workspace.active_modal_kind(cx)?;
    Some(VisualNode {
        kind: format!("Modal({kind})"),
        label: Some(kind.to_string()),
        visible: true,
        focused: true,
        children: Vec::new(),
    })
}

// =====================================================================
// diagnostics.get
// =====================================================================

/// Get LSP diagnostics for files in a Solution. Returns both per-path
/// summary counts (`error_count` / `warning_count` aggregated across all
/// language servers reporting on that file) and detailed per-diagnostic
/// items (path, range, severity, message, source, code). Optional
/// `buffer_path` filters results to a single project-relative path.
///
/// `info_count` / `hint_count` are intentionally absent from the
/// summary: the underlying `project::DiagnosticSummary` only tracks
/// errors and warnings today. Use the `items` array for full severity
/// detail (`"info"`, `"hint"`).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetDiagnosticsParams {
    pub solution_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer_path: Option<String>,
}

impl<'de> Deserialize<'de> for GetDiagnosticsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            buffer_path: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            buffer_path: inner.buffer_path,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiagnosticPathSummary {
    pub path: String,
    pub error_count: usize,
    pub warning_count: usize,
}

/// A single diagnostic emitted by an LSP server, resolved to
/// zero-based `(line, col)` byte coordinates within its buffer.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiagnosticItem {
    /// Project-relative path of the buffer that owns this diagnostic.
    pub path: String,
    pub range: EditRange,
    /// Lower-cased severity: `"error"`, `"warning"`, `"info"`, or `"hint"`.
    pub severity: String,
    pub message: String,
    /// LSP `source` field (e.g. `"rust-analyzer"`, `"clippy"`), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// LSP `code` field rendered to a string, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetDiagnosticsResult {
    /// Per-file summary counts (always present).
    pub summaries: Vec<DiagnosticPathSummary>,
    /// Detailed per-diagnostic items, one entry per LSP diagnostic.
    pub items: Vec<DiagnosticItem>,
}

#[derive(Clone)]
pub struct GetDiagnosticsTool;

impl McpServerTool for GetDiagnosticsTool {
    type Input = GetDiagnosticsParams;
    type Output = GetDiagnosticsResult;
    const NAME: &'static str = "diagnostics.get";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        let summaries = cx.update(|cx| collect_diagnostic_summaries(&input, cx));
        let items = collect_diagnostic_items(&input, cx).await;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "{} file(s) with {} diagnostic(s)",
                    summaries.len(),
                    items.len(),
                ),
            }],
            structured_content: GetDiagnosticsResult { summaries, items },
        })
    }
}

fn collect_diagnostic_summaries(
    input: &GetDiagnosticsParams,
    cx: &mut App,
) -> Vec<DiagnosticPathSummary> {
    let Some(store) = SolutionStore::try_global(cx) else {
        return Vec::new();
    };
    let Some(root) = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .find(|sol| sol.id.as_str() == input.solution_id)
            .map(|sol| sol.root.clone())
    }) else {
        return Vec::new();
    };

    for handle in cx.windows() {
        let Some(window_handle) = handle.downcast::<workspace::MultiWorkspace>() else {
            continue;
        };
        let collected = window_handle
            .update(cx, |multi, _window, cx| {
                let workspace = multi.workspace().read(cx);
                let project = workspace.project().read(cx);
                let matches_solution = project
                    .visible_worktrees(cx)
                    .any(|tree| tree.read(cx).abs_path().starts_with(&root))
                    || multi.workspaces().any(|ws| {
                        ws.read(cx)
                            .project()
                            .read(cx)
                            .visible_worktrees(cx)
                            .any(|tree| tree.read(cx).abs_path().starts_with(&root))
                    });
                if !matches_solution {
                    return None;
                }

                // A path may have multiple language servers reporting on it
                // (e.g. rust-analyzer + clippy). Aggregate counts across all
                // servers for a single per-path summary, matching the rollup
                // shown in the editor's diagnostics panel.
                let mut by_path: std::collections::BTreeMap<String, DiagnosticPathSummary> =
                    std::collections::BTreeMap::new();

                for (project_path, _server_id, summary) in project.diagnostic_summaries(false, cx) {
                    let path_str = project_path.path.as_unix_str().to_string();
                    if let Some(filter) = input.buffer_path.as_deref() {
                        if path_str != filter {
                            continue;
                        }
                    }
                    let entry = by_path
                        .entry(path_str.clone())
                        .or_insert(DiagnosticPathSummary {
                            path: path_str,
                            error_count: 0,
                            warning_count: 0,
                        });
                    entry.error_count += summary.error_count;
                    entry.warning_count += summary.warning_count;
                }

                Some(by_path.into_values().collect::<Vec<_>>())
            })
            .ok()
            .flatten();

        if let Some(diagnostics) = collected {
            return diagnostics;
        }
    }
    Vec::new()
}

async fn collect_diagnostic_items(
    input: &GetDiagnosticsParams,
    cx: &mut AsyncApp,
) -> Vec<DiagnosticItem> {
    let Some(project) = cx.update(|cx| project_for_solution(&input.solution_id, cx)) else {
        return Vec::new();
    };

    // Snapshot the project paths that have any diagnostic summary.
    // Multiple language servers may report on the same file, so dedupe by
    // (worktree, path) before opening buffers — `Buffer::diagnostics_in_range`
    // already aggregates entries across all servers for that buffer.
    let project_paths: Vec<project::ProjectPath> = cx.update(|cx| {
        let project_ref = project.read(cx);
        let mut seen = collections::HashSet::default();
        let mut paths = Vec::new();
        for (project_path, _server_id, _summary) in project_ref.diagnostic_summaries(false, cx) {
            let path_str = project_path.path.as_unix_str().to_string();
            if let Some(filter) = input.buffer_path.as_deref() {
                if path_str != filter {
                    continue;
                }
            }
            let key = (project_path.worktree_id, project_path.path.clone());
            if seen.insert(key) {
                paths.push(project_path);
            }
        }
        paths
    });

    let mut items = Vec::new();
    for project_path in project_paths {
        let path_str = project_path.path.as_unix_str().to_string();
        let buffer_task = project.update(cx, |project, cx| project.open_buffer(project_path, cx));
        let buffer = match buffer_task.await {
            Ok(buffer) => buffer,
            Err(err) => {
                log::debug!("diagnostics.get: open_buffer failed for {path_str}: {err}");
                continue;
            }
        };
        let entries = cx.update(|cx| {
            use language::OffsetRangeExt as _;
            let snapshot = buffer.read(cx).snapshot();
            let max_point = snapshot.max_point();
            snapshot
                .diagnostics_in_range::<_, language::Anchor>(
                    language::Point::zero()..max_point,
                    false,
                )
                .map(|entry| {
                    let point_range = entry.range.to_point(&snapshot);
                    DiagnosticItem {
                        path: path_str.clone(),
                        range: EditRange {
                            start: EditPoint {
                                line: point_range.start.row,
                                col: point_range.start.column,
                            },
                            end: EditPoint {
                                line: point_range.end.row,
                                col: point_range.end.column,
                            },
                        },
                        severity: severity_to_string(entry.diagnostic.severity).to_string(),
                        message: entry.diagnostic.message.clone(),
                        source: entry.diagnostic.source.clone(),
                        code: entry.diagnostic.code.as_ref().map(|code| match code {
                            lsp::NumberOrString::Number(n) => n.to_string(),
                            lsp::NumberOrString::String(s) => s.clone(),
                        }),
                    }
                })
                .collect::<Vec<_>>()
        });
        items.extend(entries);
    }

    items
}

fn severity_to_string(severity: language::DiagnosticSeverity) -> &'static str {
    match severity {
        language::DiagnosticSeverity::ERROR => "error",
        language::DiagnosticSeverity::WARNING => "warning",
        language::DiagnosticSeverity::INFORMATION => "info",
        language::DiagnosticSeverity::HINT => "hint",
        _ => "info",
    }
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
fn project_for_solution(solution_id: &str, cx: &mut App) -> Option<gpui::Entity<project::Project>> {
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

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SolutionStore;
    use gpui::TestAppContext;
    use tempfile::tempdir;

    #[gpui::test]
    async fn list_returns_empty_when_store_empty(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store, cx));

        let response = cx
            .update(|cx| {
                let tool = ListSolutionsTool;
                cx.spawn(async move |cx| tool.run(ListSolutionsParams {}, cx).await)
            })
            .await
            .expect("run task");

        assert_eq!(response.structured_content.solutions.len(), 0);
    }

    #[gpui::test]
    async fn list_returns_created_solutions(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store.clone(), cx));

        store
            .update(cx, |s, cx| {
                s.create_solution("Test Sol", dir.path().to_path_buf(), cx)
            })
            .expect("create");

        let response = cx
            .update(|cx| {
                let tool = ListSolutionsTool;
                cx.spawn(async move |cx| tool.run(ListSolutionsParams {}, cx).await)
            })
            .await
            .expect("run task");

        let arr = response.structured_content.solutions;
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].name, "Test Sol");
        assert_eq!(arr[0].member_count, 0);
        assert!(!arr[0].open);
    }

    #[test]
    fn list_params_deserialize_from_null() {
        let _: ListSolutionsParams = serde_json::from_value(serde_json::Value::Null).expect("null");
    }

    #[test]
    fn get_params_round_trip() {
        let p: GetSolutionParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
    }

    #[test]
    fn get_params_accepts_null() {
        let p: GetSolutionParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
    }

    #[test]
    fn create_params_round_trip() {
        let p: CreateSolutionParams =
            serde_json::from_value(serde_json::json!({"name": "Demo"})).expect("parse");
        assert_eq!(p.name, "Demo");
    }

    #[test]
    fn create_params_accepts_null() {
        let p: CreateSolutionParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.name.is_empty());
    }

    #[test]
    fn rename_params_round_trip() {
        let p: RenameSolutionParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "new_name": "Renamed"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.new_name, "Renamed");
    }

    #[test]
    fn delete_params_round_trip() {
        let p: DeleteSolutionParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
    }

    #[test]
    fn open_params_with_focus() {
        let p: OpenSolutionParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "focus": false
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.focus, Some(false));
    }

    #[test]
    fn close_params_round_trip() {
        let p: CloseSolutionParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
    }

    // NOTE: live-runner test for `solutions.create` requires a `SettingsStore`
    // (the tool reads `root` from `SolutionsSettings::get_global`). Setting
    // that up here is gnarly; the create path is exercised end-to-end in the
    // Phase 8 integration tests where a real editor `App` is available.
    // `rename` and `delete` go through the store directly and need no
    // settings, so we cover them here.

    #[gpui::test]
    async fn rename_solution_updates_store(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store.clone(), cx));

        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("Original", dir.path().to_path_buf(), cx)
            })
            .expect("create");

        let response = cx
            .update(|cx| {
                let tool = RenameSolutionTool;
                let id = sol_id.as_str().to_string();
                cx.spawn(async move |cx| {
                    tool.run(
                        RenameSolutionParams {
                            solution_id: id,
                            new_name: "New Name".into(),
                        },
                        cx,
                    )
                    .await
                })
            })
            .await
            .expect("run task");

        assert_eq!(response.structured_content.solution_id, sol_id.as_str());

        let new_name = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|sol| sol.id == sol_id)
                .map(|sol| sol.name.clone())
        });
        assert_eq!(new_name, Some("New Name".to_string()));
    }

    #[gpui::test]
    async fn delete_solution_removes_from_store(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store.clone(), cx));

        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("Demo", dir.path().to_path_buf(), cx)
            })
            .expect("create");

        let response = cx
            .update(|cx| {
                let tool = DeleteSolutionTool;
                let id = sol_id.as_str().to_string();
                cx.spawn(async move |cx| {
                    tool.run(DeleteSolutionParams { solution_id: id }, cx).await
                })
            })
            .await
            .expect("run task");

        assert!(response.structured_content.deleted);
        let count = store.read_with(cx, |s, _| s.solutions().len());
        assert_eq!(count, 0);
    }

    #[test]
    fn list_catalog_params_accepts_null() {
        let _: ListCatalogParams = serde_json::from_value(serde_json::Value::Null).expect("null");
    }

    #[test]
    fn add_catalog_params_round_trip() {
        let p: AddCatalogProjectParams = serde_json::from_value(serde_json::json!({
            "name": "Demo",
            "remote_url": "git@example.com:demo.git",
            "default_branch": "main"
        }))
        .expect("parse");
        assert_eq!(p.name, "Demo");
        assert_eq!(p.remote_url, "git@example.com:demo.git");
        assert_eq!(p.default_branch.as_deref(), Some("main"));
    }

    #[test]
    fn add_catalog_params_accepts_null() {
        let p: AddCatalogProjectParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.name.is_empty());
        assert!(p.remote_url.is_empty());
        assert!(p.default_branch.is_none());
    }

    #[test]
    fn remove_catalog_params_round_trip() {
        let p: RemoveCatalogProjectParams = serde_json::from_value(serde_json::json!({
            "catalog_id": "demo"
        }))
        .expect("parse");
        assert_eq!(p.catalog_id, "demo");
    }

    #[test]
    fn edit_catalog_params_partial() {
        let p: EditCatalogProjectParams = serde_json::from_value(serde_json::json!({
            "catalog_id": "demo",
            "name": "Renamed"
        }))
        .expect("parse");
        assert_eq!(p.catalog_id, "demo");
        assert_eq!(p.name.as_deref(), Some("Renamed"));
        assert!(p.default_branch.is_none());
    }

    #[test]
    fn refresh_cache_params_round_trip() {
        let p: RefreshCacheParams = serde_json::from_value(serde_json::json!({
            "catalog_id": "demo"
        }))
        .expect("parse");
        assert_eq!(p.catalog_id, "demo");
    }

    #[gpui::test]
    async fn add_catalog_project_persists(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store.clone(), cx));

        let response = cx
            .update(|cx| {
                let tool = AddCatalogProjectTool;
                cx.spawn(async move |cx| {
                    tool.run(
                        AddCatalogProjectParams {
                            name: "Demo".into(),
                            remote_url: "git@example.com:demo.git".into(),
                            default_branch: Some("main".into()),
                        },
                        cx,
                    )
                    .await
                })
            })
            .await
            .expect("run task");

        assert_eq!(response.structured_content.catalog_id, "demo");
        let count = store.read_with(cx, |s, _| s.catalog().len());
        assert_eq!(count, 1);
    }

    #[test]
    fn add_member_params_round_trip() {
        let p: AddMemberParams = serde_json::from_value(serde_json::json!({
            "solution_id": "sol",
            "catalog_id": "cat"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "sol");
        assert_eq!(p.catalog_id, "cat");
    }

    #[test]
    fn remove_member_params_accepts_null() {
        let p: RemoveMemberParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.catalog_id.is_empty());
    }

    #[test]
    fn reorder_members_params_round_trip() {
        let p: ReorderMembersParams = serde_json::from_value(serde_json::json!({
            "solution_id": "sol",
            "ordered_catalog_ids": ["a", "b", "c"]
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "sol");
        assert_eq!(p.ordered_catalog_ids, vec!["a", "b", "c"]);
    }

    #[gpui::test]
    async fn remove_member_updates_store(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store.clone(), cx));

        let cat_id = store
            .update(cx, |s, cx| {
                s.add_catalog_project("Demo", "git@x:demo.git", None, cx)
            })
            .expect("add catalog");
        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("Sol", dir.path().to_path_buf(), cx)
            })
            .expect("create");
        store.update(cx, |s, _| {
            s.test_force_add_member(&sol_id, &cat_id);
        });

        let count_before = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|sol| sol.id == sol_id)
                .map(|sol| sol.members.len())
                .unwrap_or(0)
        });
        assert_eq!(count_before, 1);

        let response = cx
            .update(|cx| {
                let tool = RemoveMemberTool;
                let solution_id = sol_id.as_str().to_string();
                let catalog_id = cat_id.as_str().to_string();
                cx.spawn(async move |cx| {
                    tool.run(
                        RemoveMemberParams {
                            solution_id,
                            catalog_id,
                        },
                        cx,
                    )
                    .await
                })
            })
            .await
            .expect("run task");

        assert!(response.structured_content.removed);
        let count_after = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|sol| sol.id == sol_id)
                .map(|sol| sol.members.len())
                .unwrap_or(0)
        });
        assert_eq!(count_after, 0);
    }

    #[test]
    fn list_buffers_params_round_trip() {
        let p: ListBuffersParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
    }

    #[test]
    fn list_buffers_params_accepts_null() {
        let p: ListBuffersParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
    }

    #[test]
    fn get_effective_settings_params_round_trip() {
        let p: GetEffectiveSettingsParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "path": "src/foo.rs"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.path.as_deref(), Some("src/foo.rs"));
    }

    #[test]
    fn get_effective_settings_params_accepts_null() {
        let p: GetEffectiveSettingsParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.path.is_none());
    }

    #[test]
    fn dispatch_action_params_with_args() {
        let p: DispatchActionParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "action_name": "workspace::ToggleLeftDock",
            "args": null
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.action_name, "workspace::ToggleLeftDock");
    }

    #[test]
    fn dispatch_action_params_accepts_null() {
        let p: DispatchActionParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.action_name.is_empty());
    }

    #[test]
    fn screenshot_params_round_trip() {
        let p: ScreenshotParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "format": "jpeg",
            "quality": 75,
            "max_dimension": 1280
        }))
        .expect("parse");
        assert_eq!(p.solution_id.as_deref(), Some("demo"));
        assert_eq!(p.format.as_deref(), Some("jpeg"));
        assert_eq!(p.quality, Some(75));
        assert_eq!(p.max_dimension, Some(1280));
    }

    #[test]
    fn screenshot_params_by_window_id() {
        let p: ScreenshotParams = serde_json::from_value(serde_json::json!({
            "window_id": "window:3",
            "format": "png"
        }))
        .expect("parse");
        assert!(p.solution_id.is_none());
        assert_eq!(p.window_id.as_deref(), Some("window:3"));
        assert_eq!(p.format.as_deref(), Some("png"));
    }

    #[test]
    fn screenshot_params_accepts_null() {
        let p: ScreenshotParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_none());
        assert!(p.window_id.is_none());
        assert!(p.format.is_none());
        assert!(p.quality.is_none());
        assert!(p.max_dimension.is_none());
    }

    #[test]
    fn dump_visual_params_round_trip() {
        let p: DumpVisualStructureParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
    }

    #[test]
    fn dump_visual_params_accepts_null() {
        let p: DumpVisualStructureParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
    }

    #[test]
    fn diagnostics_params_round_trip() {
        let p: GetDiagnosticsParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "buffer_path": "src/foo.rs"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.buffer_path.as_deref(), Some("src/foo.rs"));
    }

    #[test]
    fn diagnostics_params_accepts_null() {
        let p: GetDiagnosticsParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.buffer_path.is_none());
    }

    #[test]
    fn list_files_params_round_trip() {
        let p: ListFilesParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "glob": "**/*.rs",
            "scope": "first_worktree",
            "max": 50
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.glob.as_deref(), Some("**/*.rs"));
        assert_eq!(p.scope.as_deref(), Some("first_worktree"));
        assert_eq!(p.max, Some(50));
    }

    #[test]
    fn list_files_params_accepts_null() {
        let p: ListFilesParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.glob.is_none());
        assert!(p.scope.is_none());
        assert!(p.cursor.is_none());
        assert!(p.max.is_none());
    }

    #[gpui::test]
    async fn validate_path_rejects_relative(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store, cx));
        let result = cx.update(|cx| validate_path_in_solution("any", "relative/path.rs", cx));
        assert!(matches!(result, Err(PathValidationError::InvalidPath)));
    }

    #[gpui::test]
    async fn validate_path_rejects_unknown_solution(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store, cx));
        let result = cx.update(|cx| validate_path_in_solution("nonexistent", "/tmp/foo", cx));
        assert!(matches!(result, Err(PathValidationError::SolutionNotFound)));
    }

    #[gpui::test]
    async fn validate_path_rejects_outside_solution(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store.clone(), cx));
        let _sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("Sol", dir.path().to_path_buf(), cx)
            })
            .expect("create");
        let result = cx.update(|cx| validate_path_in_solution("sol", "/etc/passwd", cx));
        assert!(matches!(
            result,
            Err(PathValidationError::PathOutsideSolution)
        ));
    }

    #[test]
    fn read_buffer_params_round_trip() {
        let p: ReadBufferParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "path": "/abs/foo.rs"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.path, "/abs/foo.rs");
    }

    #[test]
    fn read_buffer_params_accepts_null() {
        let p: ReadBufferParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.path.is_empty());
    }

    #[test]
    fn apply_edit_params_round_trip() {
        let p: ApplyEditParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "path": "/abs/foo.rs",
            "edits": [{
                "range": {"start": {"line": 0, "col": 0}, "end": {"line": 0, "col": 5}},
                "new_text": "hello"
            }]
        }))
        .expect("parse");
        assert_eq!(p.edits.len(), 1);
        assert_eq!(p.edits[0].new_text, "hello");
        assert_eq!(p.edits[0].range.start.line, 0);
        assert_eq!(p.edits[0].range.end.col, 5);
    }

    #[test]
    fn apply_edit_params_accepts_null() {
        let p: ApplyEditParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.path.is_empty());
        assert!(p.edits.is_empty());
    }

    #[test]
    fn save_buffer_params_round_trip() {
        let p: SaveBufferParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "path": "/abs/foo.rs"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.path, "/abs/foo.rs");
    }

    #[test]
    fn save_buffer_params_accepts_null() {
        let p: SaveBufferParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.path.is_empty());
    }

    #[test]
    fn open_file_params_round_trip() {
        let p: OpenFileParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "path": "/abs/foo.rs",
            "focus": false
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.path, "/abs/foo.rs");
        assert_eq!(p.focus, Some(false));
    }

    #[test]
    fn open_file_params_accepts_null() {
        let p: OpenFileParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.path.is_empty());
        assert!(p.focus.is_none());
    }

    #[test]
    fn close_buffer_params_round_trip() {
        let p: CloseBufferParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "path": "/abs/foo.rs",
            "save": true
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.path, "/abs/foo.rs");
        assert_eq!(p.save, Some(true));
    }

    #[test]
    fn close_buffer_params_accepts_null() {
        let p: CloseBufferParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.path.is_empty());
        assert!(p.save.is_none());
    }

    #[test]
    fn create_file_params_round_trip() {
        let p: CreateFileParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "path": "/abs/foo.rs",
            "content": "hello"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.path, "/abs/foo.rs");
        assert_eq!(p.content.as_deref(), Some("hello"));
    }

    #[test]
    fn create_file_params_accepts_null() {
        let p: CreateFileParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.path.is_empty());
        assert!(p.content.is_none());
    }

    #[test]
    fn delete_file_params_round_trip() {
        let p: DeleteFileParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "path": "/abs/foo.rs"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.path, "/abs/foo.rs");
    }

    #[test]
    fn delete_file_params_accepts_null() {
        let p: DeleteFileParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.path.is_empty());
    }

    #[test]
    fn rename_file_params_round_trip() {
        let p: RenameFileParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "from": "/abs/old.rs",
            "to": "/abs/new.rs"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.from, "/abs/old.rs");
        assert_eq!(p.to, "/abs/new.rs");
    }

    #[test]
    fn rename_file_params_accepts_null() {
        let p: RenameFileParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.from.is_empty());
        assert!(p.to.is_empty());
    }

    #[test]
    fn find_in_buffers_params_round_trip() {
        let p: FindInBuffersParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "query": "TODO",
            "case_sensitive": true,
            "regex": false,
            "scope": "all_files",
            "file_glob": "**/*.rs",
            "cursor": "/tmp|src/foo.rs",
            "max": 50
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.query, "TODO");
        assert_eq!(p.case_sensitive, Some(true));
        assert_eq!(p.regex, Some(false));
        assert_eq!(p.scope.as_deref(), Some("all_files"));
        assert_eq!(p.file_glob.as_deref(), Some("**/*.rs"));
        assert_eq!(p.cursor.as_deref(), Some("/tmp|src/foo.rs"));
        assert_eq!(p.max, Some(50));
    }

    #[test]
    fn find_in_buffers_params_accepts_null() {
        let p: FindInBuffersParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.query.is_empty());
        assert!(p.case_sensitive.is_none());
        assert!(p.regex.is_none());
        assert!(p.scope.is_none());
        assert!(p.file_glob.is_none());
        assert!(p.cursor.is_none());
        assert!(p.max.is_none());
    }

    #[test]
    fn goto_definition_params_round_trip() {
        let p: GotoDefinitionParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "path": "/abs/foo.rs",
            "line": 12,
            "col": 4
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.path, "/abs/foo.rs");
        assert_eq!(p.line, 12);
        assert_eq!(p.col, 4);
    }

    #[test]
    fn goto_definition_params_accepts_null() {
        let p: GotoDefinitionParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.path.is_empty());
        assert_eq!(p.line, 0);
        assert_eq!(p.col, 0);
    }

    #[test]
    fn find_references_params_round_trip() {
        let p: FindReferencesParams = serde_json::from_value(serde_json::json!({
            "solution_id": "demo",
            "path": "/abs/foo.rs",
            "line": 7,
            "col": 9,
            "include_declaration": true
        }))
        .expect("parse");
        assert_eq!(p.solution_id, "demo");
        assert_eq!(p.path, "/abs/foo.rs");
        assert_eq!(p.line, 7);
        assert_eq!(p.col, 9);
        assert_eq!(p.include_declaration, Some(true));
    }

    #[test]
    fn find_references_params_accepts_null() {
        let p: FindReferencesParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_empty());
        assert!(p.path.is_empty());
        assert_eq!(p.line, 0);
        assert_eq!(p.col, 0);
        assert!(p.include_declaration.is_none());
    }
}
