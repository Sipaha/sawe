use crate::{Solution, SolutionStore};
use anyhow::{Context as _, Result};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use util::ResultExt as _;

pub(crate) fn register_solutions_lifecycle(cx: &mut App) {
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
    pub id: i64,
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
        .map(|store| store.read(cx).is_open(sol.id))
        .unwrap_or(false);
    let mcp_socket = open.then(|| {
        editor_mcp::solution_socket_path(sol.id.0)
            .to_string_lossy()
            .into_owned()
    });
    SolutionSummary {
        id: sol.id.0,
        name: sol.name.clone(),
        root: sol.root.to_string_lossy().into_owned(),
        member_count: sol.members.len(),
        last_opened_at: format_last_opened(sol.last_opened_at),
        open,
        main_window_id,
        mcp_socket,
    }
}

/// Epoch millis → RFC3339, the wire format every MCP consumer already expects.
fn format_last_opened(ms: Option<i64>) -> Option<String> {
    ms.and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .map(|t| t.to_rfc3339())
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
    pub solution_id: i64,
}

impl<'de> Deserialize<'de> for GetSolutionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: i64,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SolutionDetail {
    pub id: i64,
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
    pub id: i64,
    pub name: String,
    pub local_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_catalog_id: Option<i64>,
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
                    .find(|sol| sol.id.0 == input.solution_id)
                    .map(|sol| (build_detail(sol, s.is_open(sol.id)), sol.root.clone()))
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
        editor_mcp::solution_socket_path(sol.id.0)
            .to_string_lossy()
            .into_owned()
    });
    SolutionDetail {
        id: sol.id.0,
        name: sol.name.clone(),
        root: sol.root.to_string_lossy().into_owned(),
        members: sol
            .members
            .iter()
            .map(|m| {
                let exists = m.local_path.exists();
                MemberDetail {
                    id: m.id.0,
                    name: m.name.clone(),
                    local_path: m.local_path.to_string_lossy().into_owned(),
                    origin_catalog_id: m.origin_catalog_id.map(|c| c.0),
                    status: if exists { "ok" } else { "missing_on_disk" }.to_string(),
                }
            })
            .collect(),
        last_opened_at: format_last_opened(sol.last_opened_at),
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
    pub solution_id: i64,
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
        let id = cx.update(|cx| -> Result<i64> {
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
            store.update(cx, |s, cx| s.mark_open(id, cx));
            Ok(id.0)
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
    pub solution_id: i64,
    pub new_name: String,
}

impl<'de> Deserialize<'de> for RenameSolutionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: i64,
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
    pub solution_id: i64,
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
            input.solution_id > 0,
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(
            !input.new_name.trim().is_empty(),
            "invalid_params: new_name is required"
        );
        let solution_id = input.solution_id;
        cx.update(|cx| -> Result<()> {
            let store = SolutionStore::global(cx);
            let id = crate::SolutionId(input.solution_id);
            store.update(cx, |s, cx| s.rename_solution(id, &input.new_name, cx))?;
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
    pub solution_id: i64,
}

impl<'de> Deserialize<'de> for DeleteSolutionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: i64,
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
            input.solution_id > 0,
            "invalid_params: solution_id is required"
        );
        let root = cx.update(|cx| -> Result<Option<std::path::PathBuf>> {
            let store = SolutionStore::global(cx);
            let id = crate::SolutionId(input.solution_id);
            // Capture the root before removal so we can delete its on-disk
            // worktrees afterwards.
            let root = store.read_with(cx, |s, _| {
                s.solutions()
                    .iter()
                    .find(|sol| sol.id == id)
                    .map(|sol| sol.root.clone())
            });
            store.update(cx, |s, cx| s.delete_solution(id, cx))?;
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
    pub solution_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<bool>,
}

impl<'de> Deserialize<'de> for OpenSolutionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: i64,
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
            input.solution_id > 0,
            "invalid_params: solution_id is required"
        );
        let sol_id = crate::SolutionId(input.solution_id);

        let paths = cx.update(|cx| -> Result<Vec<std::path::PathBuf>> {
            let store = SolutionStore::global(cx);
            store.read_with(cx, |s, _| s.paths_for_open(sol_id))
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
                s.touch_last_opened(sol_id, cx).log_err();
                s.mark_open(sol_id, cx);
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
    pub solution_id: i64,
}

impl<'de> Deserialize<'de> for CloseSolutionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: i64,
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
            input.solution_id > 0,
            "invalid_params: solution_id is required"
        );
        let closed = cx.update(|cx| -> Result<bool> {
            let store = SolutionStore::global(cx);
            let root = store
                .read_with(cx, |s, _| {
                    s.solutions()
                        .iter()
                        .find(|sol| sol.id.0 == input.solution_id)
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
    pub solution_id: i64,
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
                        solution_id: sol.id.0,
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

