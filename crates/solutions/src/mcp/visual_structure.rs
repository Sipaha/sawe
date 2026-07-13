use crate::SolutionStore;
use anyhow::Context as _;
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::workspace_state::find_window_for_solution;

pub(crate) fn register_visual_structure(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(DumpVisualStructureTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(DumpWindowStructureTool);
    });
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
    /// Absent on a per-solution socket: the server injects the socket's bound
    /// Solution and overrides any value sent here. Required only on the
    /// editor-global socket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solution_id: Option<i64>,
}

impl<'de> Deserialize<'de> for DumpVisualStructureParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: Option<i64>,
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
        let solution_id = crate::mcp::resolve_solution_id(input.solution_id)?.0;
        let (tree, clickables) = cx
            .update(|cx| build_visual_tree(solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", solution_id))?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "structure for {} ({} clickables)",
                    solution_id,
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
    solution_id: i64,
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
