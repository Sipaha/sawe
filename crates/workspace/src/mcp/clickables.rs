//! Shared clickable-enumeration helper used by both
//! `solutions::mcp::DumpVisualStructureTool` (to surface the `clickables` array
//! on the dump result) and `workspace::mcp::windows::ClickIdTool` (to resolve
//! an agent-supplied ID back to coordinates).
//!
//! Both surfaces enumerate the same set the same way and compute the same
//! stable hash for each hitbox so that an ID returned from a dump can be
//! looked up by a subsequent click_id call.

use gpui::{Bounds, Pixels, Point, Window, WindowId};
use schemars::JsonSchema;
use serde::Serialize;
use std::hash::{Hash, Hasher};

/// A clickable region surfaced from the current rendered frame. `id` is a
/// stable hash that's portable across redraws (unlike `gpui::HitboxId`, which
/// resets per-frame). The hash is derived from `(window_id, kind_or_path,
/// label_or_empty, bounds_rounded_to_8px)` so that small layout reflows don't
/// invalidate it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Clickable {
    pub id: String,
    /// `[x, y, width, height]` in logical (DIP) window-relative pixels.
    pub bounds: [i32; 4],
    /// Logical role such as `"Tab"` / `"Panel"` / `"ContextMenuItem"` — set
    /// when the hitbox can be cross-referenced against a `VisualNode` from
    /// the dump tree. `None` for hitboxes deep inside opaque components
    /// (editor gutter, terminal grid, etc.) where the tree builder doesn't
    /// reach.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Human-readable label (tab title, action name, menu item label). `None`
    /// for the same reasons as `kind` above.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `true` if the hitbox is currently the topmost interactive hitbox under
    /// the mouse / has captured the pointer. Cheap proxy for "focused" since
    /// GPUI's focus model is keyed off a separate `FocusId` graph.
    pub focused: bool,
}

/// Round bounds to an 8 logical-pixel grid so layout reflows that shift
/// elements by a sub-grid amount don't invalidate stable IDs. Empirically,
/// the components we care about (tabs, panel rows, context menu items) are
/// laid out on integer pixel boundaries already, so 8 px is a safe coarseness.
const STABLE_ID_GRID_PX: i32 = 8;

/// Walk the rendered frame's hitboxes and emit a [`Clickable`] for each one.
///
/// `label` and `kind` are populated from the per-hitbox `InspectorElementId`
/// (source-location-derived identifier) that GPUI tracks in
/// `inspector_hitboxes`. This gives every clickable a `file:line` label
/// that is:
///   - stable across re-renders (path depends on element-build source loc,
///     not on per-frame slot ids),
///   - meaningful to a developer-agent (it points at the Rust file that
///     constructed the element),
///   - free — GPUI already populates the map in debug builds (see
///     `gpui::Window::insert_inspector_hitbox` in this fork).
///
/// `kind` is the file name without extension (e.g. `button`, `tab`,
/// `context_menu`), a cheap heuristic for grouping clickables by component
/// type. Callers that want richer semantic kind/label (e.g. tab title
/// instead of file:line) can still post-process by cross-referencing
/// against a `VisualNode` tree.
pub fn enumerate_window_clickables(window_id: WindowId, window: &Window) -> Vec<Clickable> {
    let mut out = Vec::new();
    for hitbox in window.iter_hitboxes() {
        let bounds = hitbox.bounds;
        let arr = bounds_to_array(bounds);
        let (kind, label) = inspector_kind_and_label(window, hitbox.id);
        let id = stable_id(window_id, kind.as_deref(), label.as_deref(), arr);
        let focused = hitbox.is_hovered(window);
        out.push(Clickable {
            id,
            bounds: arr,
            kind,
            label,
            focused,
        });
    }
    out
}

/// Look up the per-hitbox `InspectorElementId` and split it into a
/// `(kind, label)` pair. `kind` is the source file's stem (a coarse
/// component grouping); `label` is `file:line` (a stable, dev-meaningful
/// pointer to the element's construction site).
#[cfg(any(feature = "inspector", debug_assertions))]
fn inspector_kind_and_label(
    window: &Window,
    hitbox_id: gpui::HitboxId,
) -> (Option<String>, Option<String>) {
    let Some(inspector_id) = window.inspector_id_for_hitbox(hitbox_id) else {
        return (None, None);
    };
    let loc = inspector_id.path.source_location;
    let file = loc.file();
    let line = loc.line();
    let kind = std::path::Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_owned);
    let label = Some(format!("{file}:{line}"));
    (kind, label)
}

#[cfg(not(any(feature = "inspector", debug_assertions)))]
fn inspector_kind_and_label(
    _window: &Window,
    _hitbox_id: gpui::HitboxId,
) -> (Option<String>, Option<String>) {
    (None, None)
}

/// Compute the stable ID for a clickable, given the (kind, label) that the
/// caller may have already cross-referenced from a [`VisualNode`] tree. This
/// is the exact function `windows.click_id` re-runs to resolve an agent
/// supplied id.
pub fn stable_id(
    window_id: WindowId,
    kind: Option<&str>,
    label: Option<&str>,
    bounds: [i32; 4],
) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    window_id.as_u64().hash(&mut hasher);
    kind.unwrap_or("").hash(&mut hasher);
    label.unwrap_or("").hash(&mut hasher);
    for value in bounds.iter() {
        let rounded = round_to_grid(*value);
        rounded.hash(&mut hasher);
    }
    format!("click:{:016x}", hasher.finish())
}

fn round_to_grid(value: i32) -> i32 {
    (value / STABLE_ID_GRID_PX) * STABLE_ID_GRID_PX
}

/// Convert a hitbox bounds rectangle to the `[x, y, w, h]` integer array the
/// MCP surface uses. Origin can be negative on multi-monitor / off-screen
/// setups; route through `f32` (the only `From<Pixels>` impl that preserves
/// sign) then truncate to `i32`.
pub fn bounds_to_array(bounds: Bounds<Pixels>) -> [i32; 4] {
    [
        f32::from(bounds.origin.x) as i32,
        f32::from(bounds.origin.y) as i32,
        f32::from(bounds.size.width) as i32,
        f32::from(bounds.size.height) as i32,
    ]
}

/// Return the bounds-center point in logical window pixels for a clickable
/// (used by `windows.click_id` to compute the synthetic click position).
pub fn clickable_center(clickable: &Clickable) -> Point<Pixels> {
    use gpui::px;
    let [x, y, w, h] = clickable.bounds;
    Point::new(
        px(x as f32 + (w as f32) / 2.0),
        px(y as f32 + (h as f32) / 2.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::WindowId;

    fn fake_window_id(value: u64) -> WindowId {
        WindowId::from(value)
    }

    #[test]
    fn stable_id_is_deterministic() {
        let id1 = stable_id(
            fake_window_id(7),
            Some("Tab"),
            Some("README.md"),
            [10, 20, 100, 30],
        );
        let id2 = stable_id(
            fake_window_id(7),
            Some("Tab"),
            Some("README.md"),
            [10, 20, 100, 30],
        );
        assert_eq!(id1, id2);
        assert!(id1.starts_with("click:"));
    }

    #[test]
    fn stable_id_changes_with_window_id() {
        let id1 = stable_id(
            fake_window_id(1),
            Some("Tab"),
            Some("README.md"),
            [10, 20, 100, 30],
        );
        let id2 = stable_id(
            fake_window_id(2),
            Some("Tab"),
            Some("README.md"),
            [10, 20, 100, 30],
        );
        assert_ne!(id1, id2);
    }

    #[test]
    fn stable_id_tolerates_sub_grid_shifts() {
        let id1 = stable_id(fake_window_id(1), Some("Tab"), None, [10, 20, 100, 30]);
        let id2 = stable_id(fake_window_id(1), Some("Tab"), None, [12, 22, 100, 30]);
        assert_eq!(
            id1, id2,
            "bounds shifts within an 8px grid should not invalidate the id"
        );
    }

    #[test]
    fn stable_id_changes_when_grid_crossed() {
        let id1 = stable_id(fake_window_id(1), Some("Tab"), None, [10, 20, 100, 30]);
        let id2 = stable_id(fake_window_id(1), Some("Tab"), None, [40, 20, 100, 30]);
        assert_ne!(
            id1, id2,
            "moves across a grid boundary should change the id"
        );
    }

    #[test]
    fn round_to_grid_matches_floor_division() {
        assert_eq!(round_to_grid(0), 0);
        assert_eq!(round_to_grid(7), 0);
        assert_eq!(round_to_grid(8), 8);
        assert_eq!(round_to_grid(15), 8);
        assert_eq!(round_to_grid(16), 16);
    }
}
