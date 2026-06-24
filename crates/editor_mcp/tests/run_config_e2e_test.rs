//! End-to-end test for the `run_config.*` MCP tools over a real Unix socket.
//!
//! Boots a TestAppContext, registers `run_config.*` tools via `run_config::init`,
//! starts the MCP server, and drives a JSON-RPC client through:
//!   run_config.list (empty)  →  run_config.create  →  run_config.list
//!   →  run_config.delete  →  run_config.list (empty again)
//!
//! `run_config.run`, `run_config.stop`, and `run_config.select` route commands
//! through the store's command sink, which is installed by `run_config_ui` and
//! requires a live `RunController` window — not available in a headless test
//! harness. Those tools return `{ "ok": false }` without a sink; we assert
//! that for `run_config.run` as a smoke check.
//!
//! Path isolation: `save_to_disk` is a no-op in this test because
//! `RunConfigStore::watch_project` is never called, so `store.fs` is `None`.
//! No real files are written. Lock + socket are pinned to a tempdir via
//! `editor_mcp::set_runtime_dir_for_test`.

use gpui::TestAppContext;
use serde_json::{Value, json};
use settings::SettingsStore;
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use std::time::Duration;

#[gpui::test]
async fn run_config_create_list_delete(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    editor_mcp::set_runtime_dir_for_test(runtime_dir.path().to_path_buf());

    cx.update(|cx| {
        editor_mcp::init(cx);
    });

    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        run_config::init(cx);
    });

    let start_result = cx.update(|cx| editor_mcp::start_server(cx));
    assert!(
        start_result.is_ok(),
        "start_server: {:?}",
        start_result.err()
    );

    let socket_path = runtime_dir.path().join("mcp.sock");
    let mut waited = Duration::ZERO;
    while !socket_path.exists() && waited < Duration::from_secs(10) {
        cx.executor().timer(Duration::from_millis(100)).await;
        waited += Duration::from_millis(100);
    }
    assert!(socket_path.exists(), "mcp.sock did not appear");

    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");

    // 1. List returns empty initially.
    let resp = call_tool(&mut stream, 1, "run_config.list", json!({})).await;
    let configurations = resp
        .pointer("/result/structuredContent/configurations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_else(|| panic!("run_config.list returned: {resp}"));
    assert!(
        configurations.is_empty(),
        "expected empty list initially, got: {configurations:?}"
    );

    // 2. Create a shell config — scope is global, but save_to_disk is a no-op
    // because store.fs is None (watch_project was never called in this test).
    let resp = call_tool(
        &mut stream,
        2,
        "run_config.create",
        json!({
            "type": "shell",
            "name": "Echo hi",
            "settings": { "command": "echo", "args": ["hi"] },
            "scope": "global"
        }),
    )
    .await;
    let config_id = resp
        .pointer("/result/structuredContent/id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("run_config.create returned: {resp}"))
        .to_string();
    // Ids are now fresh, name-independent (random uuid) — just check it's non-empty.
    assert!(
        !config_id.is_empty(),
        "expected a non-empty id, got: {resp}"
    );

    // 3. List now shows the created config.
    let resp = call_tool(&mut stream, 3, "run_config.list", json!({})).await;
    let configurations = resp
        .pointer("/result/structuredContent/configurations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_else(|| panic!("run_config.list returned: {resp}"));
    assert_eq!(
        configurations.len(),
        1,
        "expected one config, got: {configurations:?}"
    );
    let entry = &configurations[0];
    assert_eq!(
        entry.get("id").and_then(|v| v.as_str()),
        Some(config_id.as_str()),
        "id mismatch: {entry}"
    );
    assert_eq!(
        entry.get("name").and_then(|v| v.as_str()),
        Some("Echo hi"),
        "name mismatch: {entry}"
    );
    assert_eq!(
        entry.get("type").and_then(|v| v.as_str()),
        Some("shell"),
        "type mismatch: {entry}"
    );
    assert_eq!(
        entry.get("running").and_then(|v| v.as_bool()),
        Some(false),
        "should not be running: {entry}"
    );

    // 4. run_config.run returns ok:false when no run controller window is open.
    let resp = call_tool(&mut stream, 4, "run_config.run", json!({ "id": config_id })).await;
    let ok = resp
        .pointer("/result/structuredContent/ok")
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| panic!("run_config.run returned: {resp}"));
    assert!(
        !ok,
        "expected ok:false without a run controller, got: {resp}"
    );

    // 5. Delete the config.
    let resp = call_tool(
        &mut stream,
        5,
        "run_config.delete",
        json!({ "id": config_id }),
    )
    .await;
    let deleted = resp
        .pointer("/result/structuredContent/deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| panic!("run_config.delete returned: {resp}"));
    assert!(deleted, "expected deleted:true, got: {resp}");

    // 6. List is empty again.
    let resp = call_tool(&mut stream, 6, "run_config.list", json!({})).await;
    let configurations = resp
        .pointer("/result/structuredContent/configurations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_else(|| panic!("run_config.list returned: {resp}"));
    assert!(
        configurations.is_empty(),
        "expected empty list after delete, got: {configurations:?}"
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
    // Skip server-pushed notifications (no `id` or non-matching `id`).
    loop {
        let line = read_line(stream).await;
        if line.is_empty() {
            panic!("socket closed while waiting for response to {name}");
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
