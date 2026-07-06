//! MCP tools owned by the `workspace` crate. Tools register with the
//! central `editor_mcp` registry from `workspace::init` so that
//! `start_server` (called later from `crates/zed/src/main.rs`) sees them
//! when binding the socket.
//!
//! These tools live here (rather than in `editor_mcp`) because they touch
//! workspace-domain types (`MultiWorkspace`, `AppState`, `open_paths`).
//! Keeping them out of `editor_mcp` breaks the would-be cycle between
//! `editor_mcp` and `workspace`.
pub mod clickables;
pub mod handle_cli_args;
pub mod windows;

use gpui::App;

pub fn register(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(handle_cli_args::HandleCliArgsTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(windows::ListWindowsTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(windows::FocusWindowTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(windows::CloseWindowTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(windows::DispatchActionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(windows::SendKeystrokeTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(windows::SendTextTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(windows::ClickAtTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(windows::ScrollAtTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(windows::ClickIdTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(windows::HoverAtTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(windows::HoverIdTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(windows::ScreenshotTool);
    });
}
