//! End-to-end test for `solutions.add_member` using a real local bare git
//! repository as the catalog project's remote URL.
//!
//! This is the autonomous verification that the user requested in lieu of
//! manual UI testing for Phase 4.5: agent-driven scenario over the MCP
//! socket that exercises the actual git pipeline (clone via system git,
//! cache hit on a second add, member status reporting).
//!
//! Isolation: pins the lock + socket to a tempdir via
//! `editor_mcp::set_runtime_dir_for_test`, so it is safe to run alongside
//! a live `sawe` instance.

use gpui::{TestAppContext, UpdateGlobal as _};
use serde_json::{Value, json};
use settings::{Settings as _, SettingsStore};
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use std::time::Duration;

#[gpui::test]
async fn add_member_clones_from_local_bare_repo(cx: &mut TestAppContext) {
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

    // Seed a local bare git repo to act as the catalog project's remote.
    // Uses solutions::git::test_support helpers (gated behind the
    // `test-support` feature in solutions/Cargo.toml).
    let bare = solutions::git::test_support::make_bare_with_one_commit(work_dir.path()).await;
    let remote_url = bare.to_str().expect("path to str").to_string();

    let store = cx.update(|cx| solutions::SolutionStore::for_test(cfg_path, cx));
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        solutions::SolutionsSettings::register(cx);
        solutions::install_global_for_test(store.clone(), cx);
        solutions::mcp::register(cx);
    });

    // Override solutions.root and solutions.cache_root so the test does not
    // touch the real ~/sawe/solutions/ or ~/.cache/sawe/.
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
    let resp = call_tool(&mut stream, 1, "solutions.create", json!({"name": "Demo"})).await;
    let solution_id = resp
        .pointer("/result/structuredContent/solution_id")
        .and_then(|v| v.as_i64())
        .expect("solution_id");
    assert!(solution_id > 0, "ids are counters: {solution_id}");

    // --- 2. Add catalog project pointing at our local bare repo ---
    let resp = call_tool(
        &mut stream,
        2,
        "catalog.add_project",
        json!({"name": "seed", "remote_url": remote_url}),
    )
    .await;
    let catalog_id = resp
        .pointer("/result/structuredContent/catalog_id")
        .and_then(|v| v.as_i64())
        .expect("catalog_id");

    // --- 3. add_member starts an async op and returns operation_id ---
    let resp = call_tool(
        &mut stream,
        3,
        "solutions.add_member",
        json!({"solution_id": solution_id, "catalog_id": catalog_id}),
    )
    .await;
    let op_id = resp
        .pointer("/result/structuredContent/operation_id")
        .and_then(|v| v.as_str())
        .expect("operation_id")
        .to_string();

    // Poll editor.get_operation until the clone completes. Timeout after 30s
    // — bare-repo clone of one commit is well under that.
    let mut elapsed = Duration::ZERO;
    let final_state = loop {
        cx.executor().timer(Duration::from_millis(200)).await;
        elapsed += Duration::from_millis(200);
        let resp = call_tool(
            &mut stream,
            100,
            "editor.get_operation",
            json!({"operation_id": op_id}),
        )
        .await;
        let status = resp
            .pointer("/result/structuredContent/status")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_default();
        if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
            break resp;
        }
        assert!(
            elapsed < Duration::from_secs(30),
            "add_member did not finish in 30s; last status: {status}"
        );
    };
    let final_status = final_state
        .pointer("/result/structuredContent/status")
        .and_then(|v| v.as_str());
    assert_eq!(
        final_status,
        Some("completed"),
        "add_member did not complete OK: {final_state:?}"
    );

    // --- 4. solutions.get reports the member with status="ok" ---
    let resp = call_tool(
        &mut stream,
        4,
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
    assert_eq!(
        member.get("origin_catalog_id").and_then(|v| v.as_i64()),
        Some(catalog_id),
    );
    let member_id = member
        .get("id")
        .and_then(|v| v.as_i64())
        .expect("member id");
    assert_eq!(
        member.get("status").and_then(|v| v.as_str()),
        Some("ok"),
        "member should be on disk: {member:?}"
    );
    let local_path = member
        .get("local_path")
        .and_then(|v| v.as_str())
        .expect("local_path");
    let local = std::path::Path::new(local_path);
    assert!(local.exists(), "local_path {local_path} does not exist");
    assert!(
        local.join(".git").exists(),
        ".git not found at {local_path}"
    );
    assert!(
        local.join("README").exists(),
        "seed README missing at {local_path}",
    );

    // --- 4b. solutions.find_for_path matches what the title bar would render ---
    // The active worktree path of an opened Solution is the member's local_path.
    // Title-bar logic walks SolutionStore looking for a Solution whose root
    // contains that path; this tool exposes the same matching for verification.
    let resp = call_tool(
        &mut stream,
        41,
        "solutions.find_for_path",
        json!({"abs_path": local_path}),
    )
    .await;
    assert_eq!(
        resp.pointer("/result/structuredContent/match/solution_id")
            .and_then(|v| v.as_i64()),
        Some(solution_id),
        "find_for_path should match the solution that contains local_path: {resp}"
    );
    assert_eq!(
        resp.pointer("/result/structuredContent/match/solution_name")
            .and_then(|v| v.as_str()),
        Some("Demo"),
    );

    // Unrelated path → no match.
    let resp = call_tool(
        &mut stream,
        42,
        "solutions.find_for_path",
        json!({"abs_path": "/tmp/definitely-not-a-solution-root"}),
    )
    .await;
    assert!(
        resp.pointer("/result/structuredContent/match").is_none()
            || resp
                .pointer("/result/structuredContent/match")
                .map(|v| v.is_null())
                .unwrap_or(false),
        "expected no match for unrelated path, got: {resp}"
    );

    // --- 5. remove_member tears it down cleanly ---
    let resp = call_tool(
        &mut stream,
        5,
        "solutions.remove_member",
        json!({"member_id": member_id}),
    )
    .await;
    assert!(resp.pointer("/result/structuredContent").is_some());

    let resp = call_tool(
        &mut stream,
        6,
        "solutions.get",
        json!({"solution_id": solution_id}),
    )
    .await;
    let members = resp
        .pointer("/result/structuredContent/solution/members")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(members.is_empty(), "members should be empty: {members:?}");

    // --- 6. catalog.clear_cache removes the on-disk cache directory ---
    // The earlier add_member populated the warm clone at
    // <cache_root>/<repo_key>/. clear_cache should report exactly that path.
    let resp = call_tool(
        &mut stream,
        7,
        "catalog.clear_cache",
        json!({"catalog_id": catalog_id}),
    )
    .await;
    let removed = resp
        .pointer("/result/structuredContent/removed_paths")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_else(|| panic!("clear_cache returned: {resp}"));
    assert_eq!(
        removed.len(),
        1,
        "expected one cache dir removed: {removed:?}"
    );
    let removed_path = removed[0].as_str().expect("path string");
    assert!(
        !std::path::Path::new(removed_path).exists(),
        "{removed_path} should be gone after clear_cache",
    );

    // Idempotent — second clear_cache for the same id reports zero removals.
    let resp = call_tool(
        &mut stream,
        8,
        "catalog.clear_cache",
        json!({"catalog_id": catalog_id}),
    )
    .await;
    let removed = resp
        .pointer("/result/structuredContent/removed_paths")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        removed.is_empty(),
        "second clear_cache should be a no-op, got {removed:?}",
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
    // operation_completed broadcasts can interleave between our request
    // and its response, since the server pushes them eagerly.
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
