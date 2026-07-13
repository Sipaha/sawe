//! A scoped-socket call with NO `solution_id` must succeed (the listener
//! injects the bound id), and a call carrying a FOREIGN `solution_id` must be
//! overridden to the bound one — a scoped subagent cannot reach across
//! Solutions. Both properties hang off `RegisteredTool::wants_solution_id`,
//! which is derived from the `solution_id` *property* existing in the tool's
//! input schema (`context_server::listener`), so an `Option<i64>` keeps them.
//!
//! The mirror image is the editor-global socket: it has no bound Solution, so
//! an omitted `solution_id` must come back as an explicit error rather than a
//! silent success against some arbitrary Solution.

use gpui::{TestAppContext, UpdateGlobal as _};
use serde_json::{Value, json};
use settings::{Settings as _, SettingsStore};
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use std::time::Duration;

#[gpui::test]
async fn scoped_socket_injects_and_overrides_the_solution_id(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    editor_mcp::set_runtime_dir_for_test(runtime_dir.path().to_path_buf());

    cx.update(|cx| {
        editor_mcp::init(cx);
    });

    let work_dir = tempfile::tempdir().expect("work tempdir");
    let cfg_path = work_dir.path().join("solutions.json");
    let solutions_root = work_dir.path().join("sol-root");
    std::fs::create_dir_all(&solutions_root).expect("mkdir sol-root");

    let store = cx.update(|cx| solutions::SolutionStore::for_test(cfg_path, cx));
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        solutions::SolutionsSettings::register(cx);
        solutions::install_global_for_test(store.clone(), cx);
        solutions::mcp::register(cx);
    });

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

    cx.update(|cx| editor_mcp::start_server(cx)).expect("start_server");

    let global_socket = runtime_dir.path().join("mcp.sock");
    wait_for(cx, &global_socket).await;
    let mut global = UnixStream::connect(&global_socket).await.expect("connect");

    let bound = create_solution(&mut global, 1, "Bound").await;
    let foreign = create_solution(&mut global, 2, "Foreign").await;
    assert_ne!(bound, foreign);

    // The editor-global socket has no bound Solution: an omitted id must be a
    // loud error naming the per-solution socket, never a wrong-Solution answer.
    let response = call_tool(&mut global, 3, "solutions.get", json!({})).await;
    assert_eq!(
        response.pointer("/result/isError"),
        Some(&json!(true)),
        "the global socket must refuse an omitted solution_id: {response}"
    );
    let message = response
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        message.contains("per-solution socket"),
        "the error must point the caller at the scoped socket: {message}"
    );

    let root = solutions_root.join("Bound");
    cx.update(|cx| editor_mcp::open_solution_socket(cx, bound, root));
    let scoped_socket = editor_mcp::solution_socket_path(bound);
    wait_for(cx, &scoped_socket).await;
    let mut scoped = UnixStream::connect(&scoped_socket).await.expect("connect");

    // 1. No solution_id at all -> the bound Solution answers.
    let response = call_tool(&mut scoped, 10, "solutions.get", json!({})).await;
    assert_eq!(
        response.pointer("/result/structuredContent/solution/id"),
        Some(&json!(bound)),
        "the scoped socket must inject its bound id: {response}"
    );

    // 2. A foreign solution_id -> still the bound Solution.
    let response = call_tool(
        &mut scoped,
        11,
        "solutions.get",
        json!({ "solution_id": foreign }),
    )
    .await;
    assert_eq!(
        response.pointer("/result/structuredContent/solution/id"),
        Some(&json!(bound)),
        "the per-socket injection must overwrite a caller-supplied id: {response}"
    );

    // 3. A scoped project tool with no id resolves against the bound Solution.
    let response = call_tool(&mut scoped, 12, "project.list_files", json!({})).await;
    assert_eq!(
        response.pointer("/result/isError"),
        Some(&json!(false)),
        "project.list_files must not need an explicit solution_id: {response}"
    );
}

async fn wait_for(cx: &mut TestAppContext, path: &std::path::Path) {
    let mut waited = Duration::ZERO;
    while std::fs::symlink_metadata(path).is_err() && waited < Duration::from_secs(10) {
        cx.executor().timer(Duration::from_millis(50)).await;
        waited += Duration::from_millis(50);
    }
    assert!(
        std::fs::symlink_metadata(path).is_ok(),
        "{} did not appear",
        path.display()
    );
}

async fn create_solution(stream: &mut UnixStream, id: u64, name: &str) -> i64 {
    let response = call_tool(stream, id, "solutions.create", json!({ "name": name })).await;
    response
        .pointer("/result/structuredContent/solution_id")
        .and_then(|value| value.as_i64())
        .unwrap_or_else(|| panic!("solutions.create returned: {response}"))
}

async fn call_tool(stream: &mut UnixStream, id: u64, name: &str, args: Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": args }
    });
    let mut bytes = serde_json::to_vec(&request).expect("serialize request");
    bytes.push(b'\n');
    stream.write_all(&bytes).await.expect("write");
    // Skip server-pushed notifications (no `id` or non-matching `id`) — they
    // interleave with responses on the same socket.
    loop {
        let line = read_line(stream).await;
        if line.is_empty() {
            panic!("socket closed while waiting for response to {name}");
        }
        let frame: Value = serde_json::from_slice(&line).expect("parse frame");
        match frame.get("id").and_then(|value| value.as_u64()) {
            Some(frame_id) if frame_id == id => return frame,
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
