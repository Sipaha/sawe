//! End-to-end test for `windows.send_keystroke`, `windows.send_text`, and
//! `windows.click_at` MCP tools.
//!
//! Opens a real test window with a bound action handler and asserts that
//! sending the corresponding keystroke over the MCP socket actually fires
//! the handler. The click_at path is exercised at "did not crash" depth
//! since hit-testing requires real layout the GPUI test platform doesn't
//! produce; the tools are still proven to dispatch valid PlatformInput
//! events to the window event loop.
//!
//! Isolation: pins the lock + socket directory to a tempdir via
//! `editor_mcp::set_runtime_dir_for_test`.

use gpui::{Action, App, EmptyView, KeyBinding, TestAppContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = mcp_input_test)]
struct TickAction;

#[gpui::test]
async fn keystroke_triggers_bound_action(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    editor_mcp::set_runtime_dir_for_test(runtime_dir.path().to_path_buf());

    let counter = Arc::new(AtomicUsize::new(0));

    cx.update(|cx| {
        editor_mcp::init(cx);
        workspace::mcp::register(cx);
        cx.bind_keys([KeyBinding::new("ctrl-x", TickAction, None)]);
        let c = counter.clone();
        cx.on_action(move |_: &TickAction, _: &mut App| {
            c.fetch_add(1, Ordering::Relaxed);
        });
    });

    // Add a window so we have something to dispatch to. EmptyView is a
    // minimal Render implementor; it does not need to react to the
    // keystroke — the App-level on_action handler captures the action
    // once it bubbles to the root.
    let window = cx.add_window(|_, _| EmptyView);

    cx.update(|cx| editor_mcp::start_server(cx))
        .expect("start_server");

    let socket_path = runtime_dir.path().join("mcp.sock");
    let mut waited = Duration::ZERO;
    while !socket_path.exists() && waited < Duration::from_secs(10) {
        cx.executor().timer(Duration::from_millis(100)).await;
        waited += Duration::from_millis(100);
    }
    assert!(socket_path.exists(), "mcp.sock did not appear");

    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");

    let window_id = editor_mcp::format_window_id(window.window_id());

    // --- send_keystroke: should fire the bound action ---
    let resp = call_tool(
        &mut stream,
        1,
        "windows.send_keystroke",
        json!({"window_id": window_id, "keystroke": "ctrl-x"}),
    )
    .await;
    assert_eq!(
        resp.pointer("/result/structuredContent/handled")
            .and_then(|v| v.as_bool()),
        Some(true),
        "expected handled=true, got: {resp}"
    );
    cx.run_until_parked();
    assert_eq!(
        counter.load(Ordering::Relaxed),
        1,
        "TickAction should have fired once",
    );

    // --- send_text: each character is dispatched as a keystroke ---
    let resp = call_tool(
        &mut stream,
        2,
        "windows.send_text",
        json!({"window_id": window_id, "text": "ab c"}),
    )
    .await;
    assert_eq!(
        resp.pointer("/result/structuredContent/characters_sent")
            .and_then(|v| v.as_u64()),
        Some(4),
        "expected 4 chars sent: {resp}"
    );

    // --- click_at: dispatches a synthetic click. Hit-test may not match
    //     anything on EmptyView, but the tool must still report success. ---
    let resp = call_tool(
        &mut stream,
        3,
        "windows.click_at",
        json!({"window_id": window_id, "x": 10.0, "y": 10.0}),
    )
    .await;
    assert_eq!(
        resp.pointer("/result/structuredContent/clicked")
            .and_then(|v| v.as_bool()),
        Some(true),
        "click_at should report clicked=true: {resp}"
    );

    // --- Bad keystroke string surfaces as an error response ---
    let resp = call_tool(
        &mut stream,
        4,
        "windows.send_keystroke",
        json!({"window_id": window_id, "keystroke": "completely-bogus-keystroke!"}),
    )
    .await;
    assert!(
        resp.pointer("/result/isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || resp.pointer("/error").is_some(),
        "bogus keystroke should error, got: {resp}"
    );

    // --- Bad window id surfaces as an error response ---
    let resp = call_tool(
        &mut stream,
        5,
        "windows.send_keystroke",
        json!({"window_id": "window:99999", "keystroke": "a"}),
    )
    .await;
    assert!(
        resp.pointer("/result/isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || resp.pointer("/error").is_some(),
        "unknown window should error, got: {resp}"
    );
}

async fn call_tool(stream: &mut UnixStream, id: u64, name: &str, args: Value) -> Value {
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": args }
    });
    let mut bytes = serde_json::to_vec(&req).expect("serialize request");
    bytes.push(b'\n');
    stream.write_all(&bytes).await.expect("write");
    loop {
        let line = read_line(stream).await;
        if line.is_empty() {
            panic!("socket closed waiting for response to {name}");
        }
        let v: Value = serde_json::from_slice(&line).expect("parse frame");
        match v.get("id").and_then(|v| v.as_u64()) {
            Some(frame_id) if frame_id == id => return v,
            _ => continue,
        }
    }
}

async fn read_line(stream: &mut UnixStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).await {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
            }
            Err(_) => break,
        }
    }
    buf
}
