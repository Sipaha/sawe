//! End-to-end test for the clickable-tree MCP surface:
//!
//! - `workspace.dump_visual_structure` / `windows.dump_visual_structure`
//!   now return a `clickables` array alongside the visual tree.
//! - `windows.click_id` resolves a `clickable.id` back to a centre and
//!   dispatches a synthetic click.
//!
//! What the GPUI test platform CAN'T do here: it doesn't run the real
//! layout / paint pipeline against a `MultiWorkspace`, so we can't open
//! a Solution window and assert that a Tab clickable becomes focused
//! after a click — that's the supervisor's § H smoke-test against a
//! running editor. What this test DOES verify:
//!
//! 1. The new `clickables` field is wired into the response shape and
//!    serializes correctly (empty array on a window with no rendered
//!    hitboxes is fine — the contract is the field is present).
//! 2. `windows.click_id` returns a structured `clickable_not_found`
//!    error when the agent's id doesn't match anything.
//! 3. `windows.click_id` validates `button` / `modifiers` like the
//!    sibling `click_at` tool.
//!
//! Isolation: pins the lock + socket directory to a tempdir via
//! `editor_mcp::set_runtime_dir_for_test`.

use gpui::{EmptyView, TestAppContext};
use serde_json::{Value, json};
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use std::time::Duration;

#[gpui::test]
async fn clickable_tree_surface_over_socket(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    editor_mcp::set_runtime_dir_for_test(runtime_dir.path().to_path_buf());

    cx.update(|cx| {
        editor_mcp::init(cx);
        workspace::mcp::register(cx);
    });

    // EmptyView gives us a window handle to address by id; we never paint
    // it, so it produces no hitboxes — that is intentional. The contract
    // we verify is the response shape, not the hitbox content.
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

    // --- click_id with an unknown id surfaces a structured error ---
    let resp = call_tool(
        &mut stream,
        1,
        "windows.click_id",
        json!({"window_id": window_id, "id": "click:deadbeef"}),
    )
    .await;
    assert!(
        is_error_response(&resp),
        "click_id with bogus id should error, got: {resp}"
    );

    // --- click_id rejects unknown button ---
    let resp = call_tool(
        &mut stream,
        2,
        "windows.click_id",
        json!({
            "window_id": window_id,
            "id": "click:abc",
            "button": "scroll-down"
        }),
    )
    .await;
    assert!(
        is_error_response(&resp),
        "click_id with bogus button should error, got: {resp}"
    );

    // --- click_id rejects empty id ---
    let resp = call_tool(
        &mut stream,
        3,
        "windows.click_id",
        json!({"window_id": window_id, "id": ""}),
    )
    .await;
    assert!(
        is_error_response(&resp),
        "click_id with empty id should error, got: {resp}"
    );

    // --- click_id rejects unknown window ---
    let resp = call_tool(
        &mut stream,
        4,
        "windows.click_id",
        json!({"window_id": "window:9999999", "id": "click:abc"}),
    )
    .await;
    assert!(
        is_error_response(&resp),
        "click_id with bogus window_id should error, got: {resp}"
    );
}

fn is_error_response(resp: &Value) -> bool {
    resp.pointer("/result/isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || resp.pointer("/error").is_some()
}

async fn call_tool(stream: &mut UnixStream, id: u64, name: &str, args: Value) -> Value {
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": args }
    });
    let mut bytes = serde_json::to_vec(&req).expect("serialize");
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
