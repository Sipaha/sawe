use crate::SolutionStore;
use anyhow::{Context as _, Result};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::project_files::{EditPoint, EditRange, project_for_solution};

pub(crate) fn register_workspace_state(cx: &mut App) {
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

pub(crate) fn find_window_for_solution(solution_id: &str, cx: &mut App) -> Option<gpui::AnyWindowHandle> {
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

