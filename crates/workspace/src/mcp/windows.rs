//! `windows.*` MCP tools — list/focus/close/dispatch_action plus programmatic
//! input (keystrokes, text, mouse click, hover) for autonomous UI testing.
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{
    App, AsyncApp, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, PlatformInput, Point, px,
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

/// List all open editor windows. Returns metadata (id, kind, root paths,
/// focused state, bounds, title) for each window currently managed by the
/// editor.
#[derive(Debug, Clone, Default, JsonSchema)]
pub struct ListWindowsParams {}

// Custom deserializer accepts JSON null, missing, or `{}` — matches the
// pattern used by other zero-field tool inputs (capabilities, etc.).
impl<'de> Deserialize<'de> for ListWindowsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let _ = serde::de::IgnoredAny::deserialize(de)?;
        Ok(ListWindowsParams {})
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WindowInfo {
    pub window_id: String,
    pub kind: String, // "folder" | "welcome"
    pub root_paths: Vec<String>,
    pub focused: bool,
    pub bounds: [i32; 4],
    pub title: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListWindowsResult {
    pub windows: Vec<WindowInfo>,
}

#[derive(Clone)]
pub struct ListWindowsTool;

impl McpServerTool for ListWindowsTool {
    type Input = ListWindowsParams;
    type Output = ListWindowsResult;
    const NAME: &'static str = "windows.list";

    async fn run(
        &self,
        _input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let windows: Vec<WindowInfo> = cx.update(collect_windows);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} window(s) open", windows.len()),
            }],
            structured_content: ListWindowsResult { windows },
        })
    }
}

fn collect_windows(cx: &mut App) -> Vec<WindowInfo> {
    let active_window_id = cx.active_window().map(|h| h.window_id());
    // Prefer Z-ordered window stack for stable, meaningful ordering. SlotMap
    // iteration via `cx.windows()` is unstable across calls, which the
    // fallback compensates for with a deterministic sort by window id.
    //
    // `window_stack()` reads `_NET_CLIENT_LIST_STACKING` from the X11 root;
    // under Xvfb (no window manager) that property is empty and the call
    // returns `Some(vec![])`. Treat an empty stack the same as `None` —
    // otherwise headless mode silently reports "0 windows" while editor
    // windows are actually open.
    let handles = cx
        .window_stack()
        .filter(|stack| !stack.is_empty())
        .unwrap_or_else(|| cx.windows());
    let mut out = Vec::new();
    for handle in handles {
        let Some(window_handle) = handle.downcast::<crate::MultiWorkspace>() else {
            continue;
        };
        let window_id = handle.window_id();
        let info = window_handle.update(cx, |multi, window, cx| {
            build_window_info(window_id, active_window_id, multi, window, cx)
        });
        if let Ok(info) = info {
            out.push(info);
        }
    }
    out
}

fn build_window_info(
    window_id: gpui::WindowId,
    active_window_id: Option<gpui::WindowId>,
    multi: &mut crate::MultiWorkspace,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<crate::MultiWorkspace>,
) -> WindowInfo {
    // Solution windows retain multiple workspaces; reading only the active
    // one would miss worktrees of non-active members. Walk every retained
    // workspace and dedupe paths so the response reflects the full window.
    let mut root_paths: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for workspace_entity in multi.workspaces() {
        let workspace = workspace_entity.read(cx);
        let project = workspace.project().read(cx);
        for tree in project.visible_worktrees(cx) {
            let path = tree.read(cx).abs_path().to_string_lossy().into_owned();
            if seen.insert(path.clone()) {
                root_paths.push(path);
            }
        }
    }

    // Solution-id cross-reference moved to `solutions::mcp` to avoid a
    // cycle between `editor_mcp` and the `solutions` crate. Clients that
    // need solution membership should call `solutions.list` and join on
    // `root_paths`.
    let kind = if root_paths.is_empty() {
        "welcome"
    } else {
        "folder"
    }
    .to_string();

    let bounds = window.bounds();
    // Window origin can be negative on multi-monitor / off-screen setups, so
    // use signed integers and route through `f32` (the only `From<Pixels>`
    // impl that preserves sign).
    let bounds_arr = [
        f32::from(bounds.origin.x) as i32,
        f32::from(bounds.origin.y) as i32,
        f32::from(bounds.size.width) as i32,
        f32::from(bounds.size.height) as i32,
    ];

    let title = compute_title(&root_paths);

    WindowInfo {
        window_id: editor_mcp::format_window_id(window_id),
        kind,
        root_paths,
        focused: active_window_id == Some(window_id),
        bounds: bounds_arr,
        title,
    }
}

// `Workspace::update_window_title` is private and only writes to the OS
// window title; there is no public getter for the cached value. To keep the
// MCP response self-contained, derive a simple human-readable title from the
// known root paths, falling back to the product name when no folder is open.
fn compute_title(root_paths: &[String]) -> String {
    if root_paths.is_empty() {
        return String::from("Sawe");
    }
    let names: Vec<String> = root_paths
        .iter()
        .filter_map(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    if names.is_empty() {
        String::from("Sawe")
    } else {
        names.join(", ")
    }
}

/// Focus the editor window with the given window_id (raises it to front).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct FocusWindowParams {
    pub window_id: String,
}

impl<'de> Deserialize<'de> for FocusWindowParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            window_id: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(FocusWindowParams {
            window_id: inner.window_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FocusWindowResult {
    pub focused: bool,
}

#[derive(Clone)]
pub struct FocusWindowTool;

impl McpServerTool for FocusWindowTool {
    type Input = FocusWindowParams;
    type Output = FocusWindowResult;
    const NAME: &'static str = "windows.focus";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let focused = cx.update(|cx| -> anyhow::Result<bool> {
            let handle = find_window_by_id(&input.window_id, cx)?;
            // `AnyWindowHandle::update` requires the window to still exist; if
            // it has been closed concurrently we surface that to the caller.
            handle
                .update(cx, |_view, window, _cx| window.activate_window())
                .map_err(|err| anyhow::anyhow!("activate_window failed: {err}"))?;
            Ok(true)
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("focused: {focused}"),
            }],
            structured_content: FocusWindowResult { focused },
        })
    }
}

/// Close the editor window with the given window_id.
///
/// **Warning**: forces close — does NOT prompt the user to save unsaved
/// buffers. Callers should ensure modifications are saved beforehand.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CloseWindowParams {
    pub window_id: String,
}

impl<'de> Deserialize<'de> for CloseWindowParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            window_id: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(CloseWindowParams {
            window_id: inner.window_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CloseWindowResult {
    pub closed: bool,
}

#[derive(Clone)]
pub struct CloseWindowTool;

impl McpServerTool for CloseWindowTool {
    type Input = CloseWindowParams;
    type Output = CloseWindowResult;
    const NAME: &'static str = "windows.close";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let closed = cx.update(|cx| -> anyhow::Result<bool> {
            let handle = find_window_by_id(&input.window_id, cx)?;
            // `Window::remove_window` flips the `removed` flag; the window is
            // actually torn down on the next platform tick. Failure here means
            // the handle is stale (window already gone).
            handle
                .update(cx, |_view, window, _cx| window.remove_window())
                .map_err(|err| anyhow::anyhow!("remove_window failed: {err}"))?;
            Ok(true)
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("closed: {closed}"),
            }],
            structured_content: CloseWindowResult { closed },
        })
    }
}

/// Dispatch a registered action to the window with the given window_id.
///
/// Action name is the fully-qualified path like `workspace::ToggleLeftDock`.
/// Optional `args` are deserialized into the action's payload type.
///
/// Note: returns `dispatched: true` once the action was successfully built
/// and queued onto the window's dispatcher. The dispatch itself runs on a
/// later tick; this tool does NOT report whether a handler eventually
/// fired or refused the action.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct DispatchActionParams {
    pub window_id: String,
    pub action_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
}

impl<'de> Deserialize<'de> for DispatchActionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            window_id: String,
            action_name: String,
            #[serde(default)]
            args: Option<serde_json::Value>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(DispatchActionParams {
            window_id: inner.window_id,
            action_name: inner.action_name,
            args: inner.args,
        })
    }
}

/// Result of `windows.dispatch_action`. `dispatched` indicates the action
/// was built and queued, NOT that a handler subsequently fired.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DispatchActionResult {
    pub dispatched: bool,
}

#[derive(Clone)]
pub struct DispatchActionTool;

impl McpServerTool for DispatchActionTool {
    type Input = DispatchActionParams;
    type Output = DispatchActionResult;
    const NAME: &'static str = "windows.dispatch_action";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let action_name = input.action_name.clone();
        let dispatched = cx.update(|cx| -> anyhow::Result<bool> {
            let handle = find_window_by_id(&input.window_id, cx)?;
            // Build the action up-front so a deserialization error surfaces
            // before we touch the window. Once built, dispatch is infallible
            // — the window itself routes the action through its keybinding /
            // focus tree.
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

// =====================================================================
// windows.send_keystroke
// =====================================================================

/// Dispatch a single keystroke to the focused element of a window.
/// Useful for triggering keybindings or single-character input. The
/// `keystroke` string follows GPUI's parser: e.g. `"ctrl-shift-p"`,
/// `"escape"`, `"enter"`, `"a"`, `"alt-tab"`.
///
/// Returns `handled: true` if the keystroke matched a binding or was
/// consumed by the focused input; `false` means it propagated past
/// every handler (i.e. nothing acted on it).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SendKeystrokeParams {
    pub window_id: String,
    pub keystroke: String,
}

impl<'de> Deserialize<'de> for SendKeystrokeParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            window_id: String,
            keystroke: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            window_id: inner.window_id,
            keystroke: inner.keystroke,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SendKeystrokeResult {
    pub handled: bool,
}

#[derive(Clone)]
pub struct SendKeystrokeTool;

impl McpServerTool for SendKeystrokeTool {
    type Input = SendKeystrokeParams;
    type Output = SendKeystrokeResult;
    const NAME: &'static str = "windows.send_keystroke";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.keystroke.is_empty(),
            "invalid_params: keystroke is required"
        );
        let keystroke = Keystroke::parse(&input.keystroke)
            .map_err(|err| anyhow::anyhow!("parse_keystroke({}): {err}", input.keystroke))?;
        let handled = cx.update(|cx| -> anyhow::Result<bool> {
            let handle = find_window_by_id(&input.window_id, cx)?;
            handle
                .update(cx, |_view, window, cx| {
                    window.dispatch_keystroke(keystroke.clone(), cx)
                })
                .map_err(|err| anyhow::anyhow!("dispatch_keystroke failed: {err}"))
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("keystroke {} handled: {handled}", input.keystroke),
            }],
            structured_content: SendKeystrokeResult { handled },
        })
    }
}

// =====================================================================
// windows.send_text
// =====================================================================

/// Dispatch a string of text as individual character keystrokes. Each
/// character becomes one `dispatch_keystroke` call so the focused input
/// receives them as if typed. Newlines are translated to `enter`.
///
/// For multi-key shortcuts (e.g. `ctrl-c`) use `windows.send_keystroke`
/// instead — this tool is for plain text entry into search fields,
/// editors, etc.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SendTextParams {
    pub window_id: String,
    pub text: String,
}

impl<'de> Deserialize<'de> for SendTextParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            window_id: String,
            text: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            window_id: inner.window_id,
            text: inner.text,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SendTextResult {
    pub characters_sent: usize,
}

#[derive(Clone)]
pub struct SendTextTool;

impl McpServerTool for SendTextTool {
    type Input = SendTextParams;
    type Output = SendTextResult;
    const NAME: &'static str = "windows.send_text";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let mut sent = 0usize;
        cx.update(|cx| -> anyhow::Result<()> {
            let handle = find_window_by_id(&input.window_id, cx)?;
            handle
                .update(cx, |_view, window, cx| {
                    for ch in input.text.chars() {
                        let key_str = match ch {
                            '\n' => "enter".to_string(),
                            '\t' => "tab".to_string(),
                            ' ' => "space".to_string(),
                            other => other.to_string(),
                        };
                        if let Ok(keystroke) = Keystroke::parse(&key_str) {
                            window.dispatch_keystroke(keystroke, cx);
                            sent += 1;
                        }
                    }
                })
                .map_err(|err| anyhow::anyhow!("send_text dispatch failed: {err}"))?;
            Ok(())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("sent {sent} characters"),
            }],
            structured_content: SendTextResult {
                characters_sent: sent,
            },
        })
    }
}

// =====================================================================
// windows.click_at
// =====================================================================

/// Dispatch a synthetic left-mouse click at window-relative coordinates
/// (DIP / logical pixels — same units as `windows.list` `bounds`).
/// Sends MouseDown immediately followed by MouseUp at the same point,
/// matching GPUI's `simulate_click` semantics.
///
/// Coordinates are LOGICAL window pixels with `(0, 0)` at the top-left
/// of the window's content area; modifiers default to none.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ClickAtParams {
    pub window_id: String,
    pub x: f32,
    pub y: f32,
    /// Modifiers held during the click. Recognized: `"ctrl"`, `"alt"`,
    /// `"shift"`, `"cmd"` / `"platform"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
    /// `"left"` (default), `"right"`, `"middle"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub button: Option<String>,
}

impl<'de> Deserialize<'de> for ClickAtParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            window_id: String,
            x: f32,
            y: f32,
            #[serde(default)]
            modifiers: Vec<String>,
            #[serde(default)]
            button: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            window_id: inner.window_id,
            x: inner.x,
            y: inner.y,
            modifiers: inner.modifiers,
            button: inner.button,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ClickAtResult {
    pub clicked: bool,
}

#[derive(Clone)]
pub struct ClickAtTool;

impl McpServerTool for ClickAtTool {
    type Input = ClickAtParams;
    type Output = ClickAtResult;
    const NAME: &'static str = "windows.click_at";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let modifiers = parse_modifiers(&input.modifiers)?;
        let button = parse_button(input.button.as_deref())?;
        let position = Point::new(px(input.x), px(input.y));
        let clicked = cx.update(|cx| -> anyhow::Result<bool> {
            let handle = find_window_by_id(&input.window_id, cx)?;
            handle
                .update(cx, |_view, window, cx| {
                    dispatch_mouse_click(window, cx, position, button, modifiers);
                })
                .map_err(|err| anyhow::anyhow!("click_at dispatch failed: {err}"))?;
            Ok(true)
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("click at ({}, {})", input.x, input.y),
            }],
            structured_content: ClickAtResult { clicked },
        })
    }
}

fn parse_modifiers(names: &[String]) -> anyhow::Result<Modifiers> {
    let mut modifiers = Modifiers::default();
    for name in names {
        match name.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.control = true,
            "alt" | "option" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            "cmd" | "platform" | "meta" | "super" => modifiers.platform = true,
            other => anyhow::bail!("unknown modifier: {other}"),
        }
    }
    Ok(modifiers)
}

fn parse_button(name: Option<&str>) -> anyhow::Result<MouseButton> {
    Ok(match name.unwrap_or("left") {
        "left" => MouseButton::Left,
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        other => anyhow::bail!("unknown button: {other}"),
    })
}

fn dispatch_mouse_click(
    window: &mut gpui::Window,
    cx: &mut App,
    position: Point<Pixels>,
    button: MouseButton,
    modifiers: Modifiers,
) {
    window.dispatch_event(
        PlatformInput::MouseDown(MouseDownEvent {
            position,
            modifiers,
            button,
            click_count: 1,
            first_mouse: false,
        }),
        cx,
    );
    window.dispatch_event(
        PlatformInput::MouseUp(MouseUpEvent {
            position,
            modifiers,
            button,
            click_count: 1,
        }),
        cx,
    );
}

// =====================================================================
// windows.click_id
// =====================================================================

/// Click a clickable region by the stable `id` previously surfaced from
/// `workspace.dump_visual_structure` / `windows.dump_visual_structure`.
/// Avoids the brittle bounds-arithmetic of `windows.click_at` — the agent
/// reads the dump, picks an item by `kind`+`label`, then passes its `id`
/// here without computing any geometry itself.
///
/// Re-enumerates the rendered frame's hitboxes and the dump tree the same
/// way the dump tool does, recomputes each stable id, then dispatches a
/// MouseDown / MouseUp pair to the matched item's centre.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ClickIdParams {
    pub window_id: String,
    pub id: String,
    /// `"left"` (default), `"right"`, `"middle"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub button: Option<String>,
    /// Modifiers held during the click. Recognized: `"ctrl"`, `"alt"`,
    /// `"shift"`, `"cmd"` / `"platform"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
}

impl<'de> Deserialize<'de> for ClickIdParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            window_id: String,
            id: String,
            #[serde(default)]
            button: Option<String>,
            #[serde(default)]
            modifiers: Vec<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            window_id: inner.window_id,
            id: inner.id,
            button: inner.button,
            modifiers: inner.modifiers,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ClickIdResult {
    pub clicked: bool,
    /// `[x, y, w, h]` of the matched clickable in logical pixels — echoed
    /// so the caller can sanity-check what was actually clicked.
    pub bounds: [i32; 4],
}

#[derive(Clone)]
pub struct ClickIdTool;

impl McpServerTool for ClickIdTool {
    type Input = ClickIdParams;
    type Output = ClickIdResult;
    const NAME: &'static str = "windows.click_id";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(!input.id.is_empty(), "invalid_params: id is required");
        let modifiers = parse_modifiers(&input.modifiers)?;
        let button = parse_button(input.button.as_deref())?;
        let id = input.id.clone();
        let bounds = cx.update(|cx| -> anyhow::Result<[i32; 4]> {
            let handle = find_window_by_id(&input.window_id, cx)?;
            handle
                .update(cx, |_view, window, cx| -> anyhow::Result<[i32; 4]> {
                    let window_id = window.window_handle().window_id();
                    let clickables =
                        super::clickables::enumerate_window_clickables(window_id, window);
                    let matched = clickables
                        .iter()
                        .find(|c| c.id == id)
                        .ok_or_else(|| anyhow::anyhow!("clickable_not_found: id={id}"))?;
                    let center = super::clickables::clickable_center(matched);
                    let arr = matched.bounds;
                    dispatch_mouse_click(window, cx, center, button, modifiers);
                    Ok(arr)
                })
                .map_err(|err| anyhow::anyhow!("click_id dispatch failed: {err}"))?
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("click_id {} -> bounds {:?}", input.id, bounds),
            }],
            structured_content: ClickIdResult {
                clicked: true,
                bounds,
            },
        })
    }
}

// =====================================================================
// windows.hover_at / windows.hover_id
// =====================================================================

/// Move the synthetic cursor to window-relative coordinates and leave it
/// there, so the next render paints hover-driven UI (`visible_on_hover`
/// timestamps / copy buttons, `:hover` styles, tooltips that fire on
/// pointer-rest). There is no MouseDown/Up — purely a `MouseMove`, which
/// updates `Window::mouse_position` AND flips the input modality back to
/// mouse (so `Hitbox::is_hovered` stops short-circuiting to `false` after
/// a prior keyboard event). Pair with `workspace.screenshot`: the
/// screenshot forces a fresh paint that recomputes the mouse hit-test
/// from the position left here, so hover-only elements show up in the PNG.
///
/// Coordinates are LOGICAL window pixels with `(0, 0)` at the top-left of
/// the window's content area — same units as `windows.click_at`.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct HoverAtParams {
    pub window_id: String,
    pub x: f32,
    pub y: f32,
    /// Modifiers held while moving. Recognized: `"ctrl"`, `"alt"`,
    /// `"shift"`, `"cmd"` / `"platform"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
}

impl<'de> Deserialize<'de> for HoverAtParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            window_id: String,
            x: f32,
            y: f32,
            #[serde(default)]
            modifiers: Vec<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            window_id: inner.window_id,
            x: inner.x,
            y: inner.y,
            modifiers: inner.modifiers,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HoverAtResult {
    pub hovered: bool,
}

#[derive(Clone)]
pub struct HoverAtTool;

impl McpServerTool for HoverAtTool {
    type Input = HoverAtParams;
    type Output = HoverAtResult;
    const NAME: &'static str = "windows.hover_at";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let modifiers = parse_modifiers(&input.modifiers)?;
        let position = Point::new(px(input.x), px(input.y));
        cx.update(|cx| -> anyhow::Result<()> {
            let handle = find_window_by_id(&input.window_id, cx)?;
            handle
                .update(cx, |_view, window, cx| {
                    dispatch_mouse_move(window, cx, position, modifiers);
                })
                .map_err(|err| anyhow::anyhow!("hover_at dispatch failed: {err}"))?;
            Ok(())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("hover at ({}, {})", input.x, input.y),
            }],
            structured_content: HoverAtResult { hovered: true },
        })
    }
}

/// Hover a clickable region by the stable `id` from
/// `workspace.dump_visual_structure` / `windows.dump_visual_structure`.
/// The `windows.hover_at` analogue of `windows.click_id` — moves the
/// synthetic cursor to the matched item's centre without pressing, so a
/// subsequent `workspace.screenshot` captures its hover state. Use this
/// to reveal a `visible_on_hover` affordance (e.g. a message-bubble
/// timestamp) the agent otherwise can't see in a static screenshot.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct HoverIdParams {
    pub window_id: String,
    pub id: String,
    /// Modifiers held while moving. Recognized: `"ctrl"`, `"alt"`,
    /// `"shift"`, `"cmd"` / `"platform"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
}

impl<'de> Deserialize<'de> for HoverIdParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            window_id: String,
            id: String,
            #[serde(default)]
            modifiers: Vec<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            window_id: inner.window_id,
            id: inner.id,
            modifiers: inner.modifiers,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HoverIdResult {
    pub hovered: bool,
    /// `[x, y, w, h]` of the matched clickable in logical pixels — echoed
    /// so the caller can sanity-check what was actually hovered.
    pub bounds: [i32; 4],
}

#[derive(Clone)]
pub struct HoverIdTool;

impl McpServerTool for HoverIdTool {
    type Input = HoverIdParams;
    type Output = HoverIdResult;
    const NAME: &'static str = "windows.hover_id";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(!input.id.is_empty(), "invalid_params: id is required");
        let modifiers = parse_modifiers(&input.modifiers)?;
        let id = input.id.clone();
        let bounds = cx.update(|cx| -> anyhow::Result<[i32; 4]> {
            let handle = find_window_by_id(&input.window_id, cx)?;
            handle
                .update(cx, |_view, window, cx| -> anyhow::Result<[i32; 4]> {
                    let window_id = window.window_handle().window_id();
                    let clickables =
                        super::clickables::enumerate_window_clickables(window_id, window);
                    let matched = clickables
                        .iter()
                        .find(|c| c.id == id)
                        .ok_or_else(|| anyhow::anyhow!("clickable_not_found: id={id}"))?;
                    let center = super::clickables::clickable_center(matched);
                    let arr = matched.bounds;
                    dispatch_mouse_move(window, cx, center, modifiers);
                    Ok(arr)
                })
                .map_err(|err| anyhow::anyhow!("hover_id dispatch failed: {err}"))?
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("hover_id {} -> bounds {:?}", input.id, bounds),
            }],
            structured_content: HoverIdResult {
                hovered: true,
                bounds,
            },
        })
    }
}

fn dispatch_mouse_move(
    window: &mut gpui::Window,
    cx: &mut App,
    position: Point<Pixels>,
    modifiers: Modifiers,
) {
    window.dispatch_event(
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers,
        }),
        cx,
    );
}

fn find_window_by_id(window_id: &str, cx: &mut App) -> anyhow::Result<gpui::AnyWindowHandle> {
    // Mirror the iteration order used by `windows.list`: prefer Z-ordered
    // stack, fall back to the unstable slot-map iteration so both tools
    // observe the same set of handles. An empty stack (Xvfb / no WM) is
    // treated as no stack at all — see `collect_windows`.
    let candidates = cx
        .window_stack()
        .filter(|stack| !stack.is_empty())
        .unwrap_or_else(|| cx.windows());
    for handle in candidates {
        if editor_mcp::format_window_id(handle.window_id()) == window_id {
            return Ok(handle);
        }
    }
    anyhow::bail!("window_not_found: {window_id}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_params_deserialize_from_null() {
        let _: ListWindowsParams =
            serde_json::from_value(serde_json::Value::Null).expect("null accepted");
    }

    #[test]
    fn list_params_deserialize_from_empty_object() {
        let _: ListWindowsParams =
            serde_json::from_value(serde_json::json!({})).expect("empty object accepted");
    }

    #[test]
    fn focus_params_round_trip() {
        let p: FocusWindowParams = serde_json::from_value(serde_json::json!({
            "window_id": "window:42"
        }))
        .expect("parse");
        assert_eq!(p.window_id, "window:42");
    }

    #[test]
    fn focus_params_accepts_null() {
        let p: FocusWindowParams =
            serde_json::from_value(serde_json::Value::Null).expect("null accepted");
        assert!(p.window_id.is_empty());
    }

    #[test]
    fn close_params_round_trip() {
        let p: CloseWindowParams = serde_json::from_value(serde_json::json!({
            "window_id": "window:7"
        }))
        .expect("parse");
        assert_eq!(p.window_id, "window:7");
    }

    #[test]
    fn dispatch_action_params_with_args() {
        let p: DispatchActionParams = serde_json::from_value(serde_json::json!({
            "window_id": "window:5",
            "action_name": "workspace::ToggleLeftDock",
            "args": null
        }))
        .expect("parse");
        assert_eq!(p.window_id, "window:5");
        assert_eq!(p.action_name, "workspace::ToggleLeftDock");
    }

    #[test]
    fn send_keystroke_params_round_trip() {
        let p: SendKeystrokeParams = serde_json::from_value(serde_json::json!({
            "window_id": "window:1",
            "keystroke": "ctrl-shift-p"
        }))
        .expect("parse");
        assert_eq!(p.keystroke, "ctrl-shift-p");
    }

    #[test]
    fn send_text_params_round_trip() {
        let p: SendTextParams = serde_json::from_value(serde_json::json!({
            "window_id": "window:1",
            "text": "hello"
        }))
        .expect("parse");
        assert_eq!(p.text, "hello");
    }

    #[test]
    fn click_at_params_default_button_and_modifiers() {
        let p: ClickAtParams = serde_json::from_value(serde_json::json!({
            "window_id": "window:1",
            "x": 100.0,
            "y": 50.0
        }))
        .expect("parse");
        assert_eq!(p.x, 100.0);
        assert_eq!(p.y, 50.0);
        assert!(p.modifiers.is_empty());
        assert!(p.button.is_none());
    }

    #[test]
    fn click_at_params_with_modifiers_and_button() {
        let p: ClickAtParams = serde_json::from_value(serde_json::json!({
            "window_id": "window:1",
            "x": 1.0, "y": 2.0,
            "modifiers": ["ctrl", "shift"],
            "button": "right"
        }))
        .expect("parse");
        assert_eq!(p.modifiers, vec!["ctrl".to_string(), "shift".to_string()]);
        assert_eq!(p.button.as_deref(), Some("right"));
    }

    #[test]
    fn parse_modifiers_handles_known_aliases() {
        let m = parse_modifiers(&[
            "Ctrl".to_string(),
            "alt".to_string(),
            "shift".to_string(),
            "cmd".to_string(),
        ])
        .expect("parse");
        assert!(m.control && m.alt && m.shift && m.platform);
    }

    #[test]
    fn parse_modifiers_rejects_unknown() {
        let err = parse_modifiers(&["spongebob".to_string()]).unwrap_err();
        assert!(err.to_string().contains("spongebob"));
    }

    #[test]
    fn parse_button_defaults_to_left() {
        assert_eq!(parse_button(None).unwrap(), MouseButton::Left);
        assert_eq!(parse_button(Some("right")).unwrap(), MouseButton::Right);
        assert_eq!(parse_button(Some("middle")).unwrap(), MouseButton::Middle);
        assert!(parse_button(Some("scroll-down")).is_err());
    }

    #[test]
    fn hover_at_params_default_modifiers() {
        let p: HoverAtParams = serde_json::from_value(serde_json::json!({
            "window_id": "window:1",
            "x": 100.0,
            "y": 50.0
        }))
        .expect("parse");
        assert_eq!(p.x, 100.0);
        assert_eq!(p.y, 50.0);
        assert!(p.modifiers.is_empty());
    }

    #[test]
    fn hover_id_params_round_trip() {
        let p: HoverIdParams = serde_json::from_value(serde_json::json!({
            "window_id": "window:1",
            "id": "abc123",
            "modifiers": ["shift"]
        }))
        .expect("parse");
        assert_eq!(p.id, "abc123");
        assert_eq!(p.modifiers, vec!["shift".to_string()]);
    }
}
