use crate::SolutionStore;
use anyhow::{Context as _, Result};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

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
    pub solution_id: i64,
}

impl<'de> Deserialize<'de> for ListBuffersParams {
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
            input.solution_id > 0,
            "invalid_params: solution_id is required"
        );
        let buffers = cx.update(|cx| collect_buffers(input.solution_id, cx));
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} buffer(s)", buffers.len()),
            }],
            structured_content: ListBuffersResult { buffers },
        })
    }
}

fn collect_buffers(solution_id: i64, cx: &mut App) -> Vec<BufferInfo> {
    let Some(store) = SolutionStore::try_global(cx) else {
        return Vec::new();
    };
    let Some(root) = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .find(|sol| sol.id.0 == solution_id)
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
    pub solution_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl<'de> Deserialize<'de> for GetEffectiveSettingsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: i64,
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
            input.solution_id > 0,
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
    pub solution_id: i64,
    pub action_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
}

impl<'de> Deserialize<'de> for DispatchActionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: i64,
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
            input.solution_id > 0,
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(
            !input.action_name.is_empty(),
            "invalid_params: action_name is required"
        );
        let action_name = input.action_name.clone();
        let dispatched = cx.update(|cx| -> Result<bool> {
            let Some(handle) = find_window_for_solution(input.solution_id, cx) else {
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

pub(crate) fn find_window_for_solution(
    solution_id: i64,
    cx: &mut App,
) -> Option<gpui::AnyWindowHandle> {
    let store = SolutionStore::try_global(cx)?;
    let root = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .find(|sol| sol.id.0 == solution_id)
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
    pub solution_id: Option<i64>,
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
            solution_id: Option<i64>,
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
        let solution_id = input.solution_id.filter(|id| *id > 0);
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
            let handle = if let Some(solution_id) = solution_id {
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
