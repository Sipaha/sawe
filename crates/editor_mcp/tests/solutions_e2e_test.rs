//! End-to-end integration test: solutions/catalog flow over real Unix socket.
//!
//! Boots a TestAppContext, registers solutions MCP tools alongside the
//! editor_mcp built-ins, starts the server, and drives a real JSON-RPC
//! client through `solutions.create` -> `catalog.add_project` ->
//! `solutions.list` -> `catalog.list` -> `solutions.delete` ->
//! `solutions.list`.
//!
//! Isolation: pins lock + socket to a tempdir via
//! `editor_mcp::set_runtime_dir_for_test` so it never touches the user's
//! `~/.config/sawe/mcp.{lock,sock}`.

use gpui::{TestAppContext, UpdateGlobal as _};
use serde_json::json;
use settings::{Settings as _, SettingsStore};
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use std::time::Duration;

#[gpui::test]
async fn solutions_flow_over_socket(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let runtime_dir = tempfile::tempdir().expect("tempdir");
    editor_mcp::set_runtime_dir_for_test(runtime_dir.path().to_path_buf());

    cx.update(|cx| {
        editor_mcp::init(cx);
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let cfg_path = dir.path().join("solutions.json");
    let store = cx.update(|cx| solutions::SolutionStore::for_test(cfg_path, cx));
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        solutions::SolutionsSettings::register(cx);
        solutions::install_global_for_test(store.clone(), cx);
        solutions::mcp::register(cx);
    });

    // Override the solutions root so create_solution doesn't try to mkdir
    // under the real ~/sawe/solutions path.
    let solutions_root = dir.path().join("sol-root");
    std::fs::create_dir_all(&solutions_root).expect("mkdir sol-root");
    let user_settings = json!({
        "solutions": { "root": solutions_root.to_string_lossy() }
    })
    .to_string();
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(&user_settings, cx)
                .expect("set_user_settings");
        });
    });

    let start_result = cx.update(|cx| editor_mcp::start_server(cx));
    assert!(
        start_result.is_ok(),
        "start_server: {:?}",
        start_result.err()
    );

    let socket_path = runtime_dir.path().join("mcp.sock");
    let mut waited = Duration::ZERO;
    let timeout = Duration::from_secs(10);
    while !socket_path.exists() && waited < timeout {
        cx.executor().timer(Duration::from_millis(100)).await;
        waited += Duration::from_millis(100);
    }
    assert!(socket_path.exists(), "mcp.sock did not appear");

    let mut stream = UnixStream::connect(&socket_path)
        .await
        .expect("connect to socket");

    // 1. solutions.create
    let resp = call_tool(&mut stream, 1, "solutions.create", json!({"name": "Demo"})).await;
    let data = resp
        .pointer("/result/structuredContent")
        .expect("structuredContent");
    let solution_id = data
        .get("solution_id")
        .and_then(|v| v.as_i64())
        .expect("solution_id");
    assert!(solution_id > 0, "ids are counters: {solution_id}");

    // 2. catalog.add_project
    let resp = call_tool(
        &mut stream,
        2,
        "catalog.add_project",
        json!({
            "name": "Demo Repo",
            "remote_url": "git@example.com:demo.git",
        }),
    )
    .await;
    let data = resp
        .pointer("/result/structuredContent")
        .expect("structuredContent");
    let catalog_id = data
        .get("catalog_id")
        .and_then(|v| v.as_i64())
        .expect("catalog_id");
    assert!(catalog_id > 0, "ids are counters: {catalog_id}");

    // 3. solutions.list
    let resp = call_tool(&mut stream, 3, "solutions.list", json!({})).await;
    let arr = resp
        .pointer("/result/structuredContent/solutions")
        .and_then(|v| v.as_array())
        .expect("solutions array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0].get("name").and_then(|v| v.as_str()), Some("Demo"));
    assert_eq!(arr[0].get("member_count").and_then(|v| v.as_u64()), Some(0));

    // 4. catalog.list
    let resp = call_tool(&mut stream, 4, "catalog.list", json!({})).await;
    let arr = resp
        .pointer("/result/structuredContent/projects")
        .and_then(|v| v.as_array())
        .expect("projects array");
    assert_eq!(arr.len(), 1);

    // 5. solutions.delete
    let resp = call_tool(
        &mut stream,
        5,
        "solutions.delete",
        json!({"solution_id": solution_id}),
    )
    .await;
    let deleted = resp
        .pointer("/result/structuredContent/deleted")
        .and_then(|v| v.as_bool());
    assert_eq!(deleted, Some(true));

    // 6. solutions.list (should be empty now)
    let resp = call_tool(&mut stream, 6, "solutions.list", json!({})).await;
    let arr = resp
        .pointer("/result/structuredContent/solutions")
        .and_then(|v| v.as_array())
        .expect("solutions array");
    assert_eq!(arr.len(), 0);
}

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
    let resp = read_line(stream).await;
    serde_json::from_slice(&resp).expect("parse response")
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
