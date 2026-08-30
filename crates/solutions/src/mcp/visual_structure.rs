use std::collections::{BTreeMap, HashMap};

use anyhow::Context as _;
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp, Global, Window};
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

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct VisualNode {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub visible: bool,
    pub focused: bool,
    /// Facts about this node that do not fit `kind` / `label` / `visible` /
    /// `focused` — geometry the user can drag (band height, divider ratio),
    /// persisted toggles, and honesty markers such as
    /// `occupant_introspectable: false` on a type-erased slot whose contents
    /// this crate cannot reach. Sorted by key so a diff between two dumps is
    /// stable, and omitted entirely when empty so nodes that predate this
    /// field serialize exactly as before.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, serde_json::Value>,
    pub children: Vec<VisualNode>,
}

impl VisualNode {
    /// A visible, unfocused, attribute-less node of `kind`. The three
    /// crates that build nodes (`solutions`, `title_bar`, `solution_agent`)
    /// all go through this so a new `VisualNode` field never has to be
    /// spelled out at every construction site.
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            visible: true,
            ..Default::default()
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn with_visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    pub fn with_focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn with_attribute(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        self.attributes.insert(key.to_string(), value.into());
        self
    }

    pub fn with_children(mut self, children: Vec<VisualNode>) -> Self {
        self.children = children;
        self
    }
}

/// Where in the workspace column a provider-contributed node is spliced.
///
/// The two rows below are painted from crates that `solutions` must not
/// depend on: the project toolbar lives in `title_bar` (it reaches
/// `solutions_ui` / `git_ui` / `run_config_ui`) and the Solution band lives
/// in `solution_agent` (which depends on `solutions`). Both are parked on
/// `Workspace` as a type-erased `AnyView`, so only the owning crate can
/// downcast one back into something it can describe. Rather than have this
/// module hand-synthesize children it cannot verify — the mistake
/// `build_title_bar_node` and `build_status_bar_node` were stripped back to
/// avoid — each owning crate registers a provider that builds its own node
/// from the same state its `render` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StructureSlot {
    /// The toolbar row directly below the title bar.
    ProjectToolbar,
    /// The full-width band between the project zone and the status bar.
    SolutionBand,
}

type StructureProvider =
    Box<dyn Fn(&workspace::Workspace, &Window, &App) -> Option<VisualNode> + 'static>;

#[derive(Default)]
struct StructureProviders(HashMap<StructureSlot, StructureProvider>);

impl Global for StructureProviders {}

/// Register the builder for `slot`. Called once from the owning crate's
/// `init`; the provider itself is looked up per dump, so registration order
/// against window construction does not matter. Registering a slot twice
/// replaces the earlier provider.
pub fn register_structure_provider(
    cx: &mut App,
    slot: StructureSlot,
    provider: impl Fn(&workspace::Workspace, &Window, &App) -> Option<VisualNode> + 'static,
) {
    cx.default_global::<StructureProviders>()
        .0
        .insert(slot, Box::new(provider));
}

fn provided_node(
    slot: StructureSlot,
    workspace: &workspace::Workspace,
    window: &Window,
    cx: &App,
) -> Option<VisualNode> {
    let provider = cx.try_global::<StructureProviders>()?.0.get(&slot)?;
    provider(workspace, window, cx)
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
            let tree = build_workspace_node(multi, window, cx);
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

/// Synthesize the TitleBar node.
///
/// Only the bar's presence is reported: this function does not walk the
/// title bar's real element tree, so it can only describe children it has
/// been taught about by hand. It used to synthesize `SolutionSegment`,
/// `ProjectName` and `Branch` from the workspace's active worktree and
/// repository, mirroring upstream's project-info chain. That chain is no
/// longer painted — `TitleBar::render` replaced it wholesale with the
/// solution tab strip, and the branch widget moved to `ProjectToolbar` — and
/// a hand-written child that no longer matches what is painted is worse than
/// no child at all (same reasoning as `build_status_bar_node` below). Use
/// `workspace.screenshot` to see the bar's real contents.
fn build_title_bar_node(_workspace: &workspace::Workspace, _cx: &App) -> VisualNode {
    VisualNode::new("TitleBar")
}

/// Synthesize what the StatusBar would render for this workspace.
///
/// Only the bar's visibility is reported: this function does not walk the
/// registered `StatusItemView`s, so it can only describe items it has been
/// taught about by hand. It used to synthesize a `SolutionsStatusItem` child
/// from the `SolutionStore`; that item was removed from the bar (phase 2a
/// task 8 — the title-bar Solution tab strip already names the Solution), and
/// a hand-written child that no longer matches what is painted is worse than
/// no child at all. Use `workspace.screenshot` to see the bar's real contents.
fn build_status_bar_node(workspace: &workspace::Workspace, cx: &App) -> VisualNode {
    VisualNode::new("StatusBar").with_visible(workspace.status_bar_visible(cx))
}

fn build_workspace_node(
    multi: &workspace::MultiWorkspace,
    window: &Window,
    cx: &App,
) -> VisualNode {
    let workspace = multi.workspace().read(cx);
    // Top-to-bottom in painted order, so a reader of the JSON can take the
    // child order as the window's vertical order (docks aside).
    let mut children = vec![build_title_bar_node(workspace, cx)];
    children.extend(provided_node(
        StructureSlot::ProjectToolbar,
        workspace,
        window,
        cx,
    ));
    children.push(build_dock_node("left", workspace.left_dock(), cx));
    children.push(build_pane_area_node(workspace, cx));
    children.push(build_dock_node("right", workspace.right_dock(), cx));
    children.push(build_dock_node("bottom", workspace.bottom_dock(), cx));
    children.extend(provided_node(
        StructureSlot::SolutionBand,
        workspace,
        window,
        cx,
    ));
    children.push(build_status_bar_node(workspace, cx));

    if let Some(modal) = build_modal_node(workspace, cx) {
        children.push(modal);
    }

    VisualNode::new("Workspace").with_children(children)
}

fn build_dock_node(side: &str, dock: &gpui::Entity<workspace::dock::Dock>, cx: &App) -> VisualNode {
    let dock = dock.read(cx);
    let is_open = dock.is_open();
    let active_panel_label = dock
        .active_panel()
        .map(|panel| panel.persistent_name().to_string());

    let panel_node = active_panel_label.map(|name| {
        VisualNode::new("Panel")
            .with_label(name)
            .with_visible(is_open)
    });

    VisualNode::new(format!("Dock({side})"))
        .with_visible(is_open)
        .with_children(panel_node.into_iter().collect())
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
                    VisualNode::new(format!("Tab({label})"))
                        .with_label(label)
                        .with_focused(is_active)
                })
                .collect();

            VisualNode::new("Pane")
                .with_focused(pane_is_active)
                .with_children(tabs)
        })
        .collect();

    VisualNode::new("PaneArea").with_children(pane_children)
}

/// Surface the active modal as a `Modal(<kind>)` leaf so introspection
/// tools can verify which modal is open. The kind comes from
/// [`workspace::ModalView::debug_kind`] — solutions modals override it
/// with stable strings (`"NewSolution"`, `"AddCatalogProject"`,
/// `"OpenSolution"`, `"AddMember"`); generic upstream modals fall back
/// to `"Modal"`.
fn build_modal_node(workspace: &workspace::Workspace, cx: &App) -> Option<VisualNode> {
    let kind = workspace.active_modal_kind(cx)?;
    Some(
        VisualNode::new(format!("Modal({kind})"))
            .with_label(kind.to_string())
            .with_focused(true),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::Project;
    use workspace::MultiWorkspace;

    fn child_kinds(tree: &VisualNode) -> Vec<&str> {
        tree.children
            .iter()
            .map(|child| child.kind.as_str())
            .collect()
    }

    async fn window_with_a_workspace(
        cx: &mut TestAppContext,
    ) -> gpui::WindowHandle<MultiWorkspace> {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let fs = FakeFs::new(cx.executor());
        let root = std::path::Path::new("/only");
        fs.insert_tree(root, serde_json::json!({ "a.txt": "a" }))
            .await;
        let project = Project::test(fs, [root], cx).await;
        cx.run_until_parked();
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx))
    }

    /// Without a registered provider the column is exactly what it was before
    /// the two slots existed — a fresh `solutions`-only test app registers
    /// none, and the dump must not grow phantom rows in that case.
    #[gpui::test]
    async fn unfilled_slots_contribute_no_nodes(cx: &mut TestAppContext) {
        let window = window_with_a_workspace(cx).await;
        let (tree, _) = cx
            .update(|cx| build_visual_tree_for_window(window.into(), cx))
            .expect("tree for the window just created");

        assert_eq!(
            child_kinds(&tree),
            vec![
                "TitleBar",
                "Dock(left)",
                "PaneArea",
                "Dock(right)",
                "Dock(bottom)",
                "StatusBar",
            ]
        );
    }

    /// The two slots splice where the rows are actually painted: the toolbar
    /// directly under the title bar, the band directly above the status bar.
    /// An agent reads the child order as the window's vertical order, so this
    /// is load-bearing, not cosmetic.
    #[gpui::test]
    async fn registered_providers_splice_in_painted_order(cx: &mut TestAppContext) {
        let window = window_with_a_workspace(cx).await;
        cx.update(|cx| {
            register_structure_provider(cx, StructureSlot::ProjectToolbar, |_, _, _| {
                Some(VisualNode::new("ProjectToolbar"))
            });
            register_structure_provider(cx, StructureSlot::SolutionBand, |_, _, _| {
                Some(VisualNode::new("SolutionBand"))
            });
        });

        let (tree, _) = cx
            .update(|cx| build_visual_tree_for_window(window.into(), cx))
            .expect("tree for the window just created");

        assert_eq!(
            child_kinds(&tree),
            vec![
                "TitleBar",
                "ProjectToolbar",
                "Dock(left)",
                "PaneArea",
                "Dock(right)",
                "Dock(bottom)",
                "SolutionBand",
                "StatusBar",
            ]
        );
    }

    /// A provider that answers `None` — the band's `AnyView` not installed in
    /// this window, say — leaves no placeholder behind.
    #[gpui::test]
    async fn a_provider_returning_none_contributes_no_node(cx: &mut TestAppContext) {
        let window = window_with_a_workspace(cx).await;
        cx.update(|cx| {
            register_structure_provider(cx, StructureSlot::SolutionBand, |_, _, _| None);
        });

        let (tree, _) = cx
            .update(|cx| build_visual_tree_for_window(window.into(), cx))
            .expect("tree for the window just created");

        assert!(!child_kinds(&tree).contains(&"SolutionBand"));
    }

    /// `attributes` is skipped when empty, so every node that predates the
    /// field serializes byte-for-byte as it used to.
    #[gpui::test]
    fn an_empty_attribute_bag_is_not_serialized(_cx: &mut TestAppContext) {
        let json = serde_json::to_value(VisualNode::new("Pane")).expect("serialize");
        assert!(json.get("attributes").is_none());

        let json =
            serde_json::to_value(VisualNode::new("SolutionBand").with_attribute("height", 320.0))
                .expect("serialize");
        assert_eq!(json["attributes"]["height"], serde_json::json!(320.0));
    }
}
