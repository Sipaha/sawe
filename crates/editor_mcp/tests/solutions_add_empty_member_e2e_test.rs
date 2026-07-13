//! End-to-end test for `solutions.add_empty_member` over the MCP socket —
//! the synchronous "create a new empty project in a Solution" path the
//! mobile project-registry feature drives. The new project is git-init'ed
//! with no remote so its history can be pushed somewhere later.
//!
//! Lives in its own test binary (one `start_server` per process — the
//! `editor_mcp` server singleton can't be re-bound within a process, so
//! each socket e2e scenario gets its own file, same as
//! `solutions_add_member_e2e_test.rs`).
//!
//! Isolation: pins the lock + socket to a tempdir via
//! `editor_mcp::set_runtime_dir_for_test`, so it is safe to run alongside
//! a live `sawe` instance.

use gpui::UpdateGlobal as _;
use serde_json::{Value, json};
use settings::{Settings as _, SettingsStore};
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use std::time::Duration;

#[gpui::test]
async fn add_empty_member_creates_git_member(cx: &mut gpui::TestAppContext) {
    cx.executor().allow_parking();

    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    editor_mcp::set_runtime_dir_for_test(runtime_dir.path().to_path_buf());

    cx.update(|cx| {
        editor_mcp::init(cx);
    });

    let work_dir = tempfile::tempdir().expect("work tempdir");
    let cfg_path = work_dir.path().join("solutions.json");
    let solutions_root = work_dir.path().join("sol-root");
    let cache_root = work_dir.path().join("cache");
    std::fs::create_dir_all(&solutions_root).expect("mkdir sol-root");
    std::fs::create_dir_all(&cache_root).expect("mkdir cache");

    let store = cx.update(|cx| solutions::SolutionStore::for_test(cfg_path, cx));
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        solutions::SolutionsSettings::register(cx);
        solutions::install_global_for_test(store.clone(), cx);
        solutions::mcp::register(cx);
    });

    let user_settings = json!({
        "solutions": {
            "root": solutions_root.to_string_lossy(),
            "cache_root": cache_root.to_string_lossy(),
        }
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
    while !socket_path.exists() && waited < Duration::from_secs(10) {
        cx.executor().timer(Duration::from_millis(100)).await;
        waited += Duration::from_millis(100);
    }
    assert!(socket_path.exists(), "mcp.sock did not appear");

    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");

    // --- 1. Create a Solution ---
    let resp = call_tool(
        &mut stream,
        1,
        "solutions.create",
        json!({"name": "Empty Demo"}),
    )
    .await;
    let solution_id = resp
        .pointer("/result/structuredContent/solution_id")
        .and_then(|v| v.as_i64())
        .expect("solution_id");
    assert!(solution_id > 0, "ids are counters: {solution_id}");

    // --- 2. add_empty_member returns the new member's member_id synchronously ---
    let resp = call_tool(
        &mut stream,
        2,
        "solutions.add_empty_member",
        json!({"solution_id": solution_id, "name": "Scratchpad"}),
    )
    .await;
    let member_id = resp
        .pointer("/result/structuredContent/member_id")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| panic!("add_empty_member returned: {resp}"));
    assert!(member_id > 0, "member ids are counters: {member_id}");

    // --- 3. solutions.get reports the member, on disk, git-init'ed ---
    let resp = call_tool(
        &mut stream,
        3,
        "solutions.get",
        json!({"solution_id": solution_id}),
    )
    .await;
    let members = resp
        .pointer("/result/structuredContent/solution/members")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_else(|| panic!("solutions.get returned: {resp}"));
    assert_eq!(members.len(), 1, "expected one member, got {members:?}");
    let member = &members[0];
    assert_eq!(member.get("id").and_then(|v| v.as_i64()), Some(member_id));
    assert_eq!(
        member.get("origin_catalog_id").and_then(|v| v.as_i64()),
        None,
        "an empty member has no catalog provenance: {member:?}"
    );
    assert_eq!(
        member.get("status").and_then(|v| v.as_str()),
        Some("ok"),
        "empty member dir should exist on disk: {member:?}"
    );
    let local_path = member
        .get("local_path")
        .and_then(|v| v.as_str())
        .expect("local_path");
    let local = std::path::Path::new(local_path);
    assert!(local.exists(), "local_path {local_path} does not exist");
    assert!(
        local.join(".git").exists(),
        "empty member must be git-initialised (no remote): {local_path}",
    );

    // --- 4. A second empty member with the same name gets its own id + folder ---
    let resp = call_tool(
        &mut stream,
        4,
        "solutions.add_empty_member",
        json!({"solution_id": solution_id, "name": "Scratchpad"}),
    )
    .await;
    let member_id_2 = resp
        .pointer("/result/structuredContent/member_id")
        .and_then(|v| v.as_i64())
        .expect("second member_id");
    assert_ne!(
        member_id, member_id_2,
        "two empty members from the same name must get distinct ids",
    );

    let resp = call_tool(
        &mut stream,
        41,
        "solutions.get",
        json!({"solution_id": solution_id}),
    )
    .await;
    let members = resp
        .pointer("/result/structuredContent/solution/members")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_else(|| panic!("solutions.get returned: {resp}"));
    let mut paths: Vec<&str> = members
        .iter()
        .filter_map(|m| m.get("local_path").and_then(|v| v.as_str()))
        .collect();
    paths.sort_unstable();
    paths.dedup();
    assert_eq!(
        paths.len(),
        2,
        "same-named empty members must land in distinct folders: {members:?}"
    );

    // --- 5. Empty members never pollute the catalog ---
    let resp = call_tool(&mut stream, 5, "catalog.list", json!({})).await;
    let projects = resp
        .pointer("/result/structuredContent/projects")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        projects.is_empty(),
        "add_empty_member must not add catalog rows, got {projects:?}",
    );

    // --- 6. Blank name is rejected as an error ---
    let resp = call_tool(
        &mut stream,
        6,
        "solutions.add_empty_member",
        json!({"solution_id": solution_id, "name": "   "}),
    )
    .await;
    let is_error = resp
        .pointer("/result/isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(is_error, "blank name should be rejected: {resp}");
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
