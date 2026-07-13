//! End-to-end test: `solutions.set_active_member` over a real Unix socket.
//!
//! Lives in its own `tests/*.rs` (i.e. its own test binary) rather than
//! alongside the other solutions e2e flow: `editor_mcp::set_runtime_dir_for_test`
//! pins a *process-global* runtime dir, so at most one server-starting test can
//! run per binary. Two of them in one file meant whichever ran second never got
//! a socket ("mcp.sock did not appear"). See the doc comment on
//! `editor_mcp::set_runtime_dir_for_test`.
//!
//! Isolation: lock + socket are pinned to a tempdir, so this never touches the
//! user's real `mcp.{lock,sock}`.

use gpui::{TestAppContext, UpdateGlobal as _};
use serde_json::json;
use settings::{Settings as _, SettingsStore};
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use std::time::Duration;

/// `solutions.set_active_member` is reachable over the socket and rejects a
/// catalog that is not a member of the solution (guarding against recording a
/// bogus active member that points at a worktree-less project). The success
/// path is covered by the store unit test `set_active_member_emits` plus live
/// verification.
#[gpui::test]
async fn set_active_member_rejects_non_member(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let runtime_dir = tempfile::tempdir().expect("tempdir");
    editor_mcp::set_runtime_dir_for_test(runtime_dir.path().to_path_buf());
    cx.update(|cx| editor_mcp::init(cx));

    let dir = tempfile::tempdir().expect("tempdir");
    let store =
        cx.update(|cx| solutions::SolutionStore::for_test(dir.path().join("solutions.json"), cx));
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        solutions::SolutionsSettings::register(cx);
        solutions::install_global_for_test(store.clone(), cx);
        solutions::mcp::register(cx);
    });
    let solutions_root = dir.path().join("sol-root");
    std::fs::create_dir_all(&solutions_root).expect("mkdir sol-root");
    let user_settings =
        json!({ "solutions": { "root": solutions_root.to_string_lossy() } }).to_string();
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(&user_settings, cx)
                .expect("set_user_settings");
        });
    });

    assert!(cx.update(|cx| editor_mcp::start_server(cx)).is_ok());
    let socket_path = runtime_dir.path().join("mcp.sock");
    let mut waited = Duration::ZERO;
    while !socket_path.exists() && waited < Duration::from_secs(10) {
        cx.executor().timer(Duration::from_millis(100)).await;
        waited += Duration::from_millis(100);
    }
    assert!(socket_path.exists(), "mcp.sock did not appear");
    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");

    let resp = call_tool(&mut stream, 1, "solutions.create", json!({"name": "Demo"})).await;
    let solution_id = resp
        .pointer("/result/structuredContent/solution_id")
        .and_then(|v| v.as_i64())
        .expect("solution_id");
    // No members yet -> any member id is a non-member -> tool must error.
    let resp = call_tool(
        &mut stream,
        2,
        "solutions.set_active_member",
        json!({"solution_id": solution_id, "member_id": 999_999}),
    )
    .await;
    let is_error = resp
        .pointer("/result/isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || resp.get("error").is_some();
    assert!(
        is_error,
        "set_active_member on a non-member must error, got: {resp}"
    );
}

async fn call_tool(
    stream: &mut UnixStream,
    id: u64,
    tool_name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": arguments,
        }
    });
    let mut bytes = serde_json::to_vec(&req).expect("serialize");
    bytes.push(b'\n');
    stream.write_all(&bytes).await.expect("write");
    // Skip server-pushed notifications (no `id` or non-matching `id`).
    loop {
        let line = read_line(stream).await;
        if line.is_empty() {
            panic!("socket closed while waiting for response to {tool_name}");
        }
        let value: serde_json::Value = serde_json::from_slice(&line).expect("parse frame");
        match value.get("id").and_then(|v| v.as_u64()) {
            Some(frame_id) if frame_id == id => return value,
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
