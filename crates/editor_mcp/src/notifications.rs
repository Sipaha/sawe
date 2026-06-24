//! Notification dispatch helpers.
//!
//! `emit` broadcasts a JSON-RPC notification (`editor/notification`) to all
//! connected MCP clients. Used by OperationTracker and event sources.

use gpui::App;
use serde_json::Value;

/// Emit a notification to all connected MCP clients.
///
/// `kind` becomes the value of `params.kind`; `payload` becomes
/// `params.payload`. Subscriptions and filtering are client-side for now.
pub fn emit(cx: &App, kind: &str, payload: Value) {
    let Some(server_entity) = crate::lifecycle::server(cx) else {
        return;
    };
    let server = server_entity.read(cx);
    let params = serde_json::json!({
        "kind": kind,
        "payload": payload,
    });
    server.broadcast_notification("editor/notification", params);
}
