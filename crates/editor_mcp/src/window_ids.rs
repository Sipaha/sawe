//! Canonical string format for `gpui::WindowId` exposed via MCP. Keep this
//! the SINGLE place that produces window-id strings — `windows.list` and
//! `editor.handle_cli_args` both must agree, since clients round-trip the
//! values to e.g. `windows.focus`.

use gpui::WindowId;

pub fn format(window_id: WindowId) -> String {
    format!("window:{}", window_id.as_u64())
}
