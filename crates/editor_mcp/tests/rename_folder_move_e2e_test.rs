//! End-to-end acceptance test for the folder-move rename, over the real MCP
//! socket: create a solution → add an empty member → rename both → restart the
//! store (which drains `pending_path_migrations`) → the solution still has its
//! member, at the new path, and the compat symlinks are gone.
//!
//! Lives in its own test binary (one `start_server` per process — the
//! `editor_mcp` server singleton cannot be re-bound within a process).
//!
//! Isolation:
//!   * `editor_mcp::set_runtime_dir_for_test` pins the lock + socket to a
//!     tempdir — mandatory, or this corrupts the live editor's socket (CLAUDE.md);
//!   * `paths::set_custom_data_dir` pins `config_dir()`/`data_dir()` to a tempdir,
//!     so the cold reconcile never reads the user's real config or agent DB;
//!   * the app database is `AppDatabase::test_new()` — in-memory, migrated, and
//!     installed as the global, so `SolutionsDb::global` and the reconcile's
//!     `AppDatabase::global` are the same connection.

use gpui::{TestAppContext, UpdateGlobal as _};
use serde_json::{Value, json};
use settings::{Settings as _, SettingsStore};
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use std::path::Path;
use std::time::Duration;

#[gpui::test]
async fn rename_solution_and_member_over_mcp_survives_a_restart(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    editor_mcp::set_runtime_dir_for_test(runtime_dir.path().to_path_buf());

    let state_dir = tempfile::tempdir().expect("state tempdir");
    paths::set_custom_data_dir(&state_dir.path().to_string_lossy());

    let work_dir = tempfile::tempdir().expect("work tempdir");
    let solutions_root = work_dir.path().join("sol-root");
    std::fs::create_dir_all(&solutions_root).expect("mkdir sol-root");

    cx.update(|cx| {
        editor_mcp::init(cx);
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        cx.set_global(db::AppDatabase::test_new());
        solutions::SolutionsSettings::register(cx);
    });

    let user_settings = json!({
        "solutions": {
            "root": solutions_root.to_string_lossy(),
            "cache_root": work_dir.path().join("cache").to_string_lossy(),
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

    // A DB-backed store (not `for_test`): the rename records a row in
    // `pending_path_migrations` that only the cold reconcile can drain.
    cx.update(|cx| {
        solutions::SolutionStore::init_global(cx);
        solutions::mcp::register(cx);
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

    // --- 1. A solution with one empty member ---
    let resp = call_tool(
        &mut stream,
        1,
        "solutions.create",
        json!({"name": "Old Solution"}),
    )
    .await;
    let solution_id = resp
        .pointer("/result/structuredContent/solution_id")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("solutions.create returned: {resp}"));
    // `solutions.create` slugifies the folder name (`old-solution`); only a
    // *rename* derives it with `folder_name::derive` (`New-Solution`). Read the
    // root back rather than assuming either spelling.
    let resp = call_tool(
        &mut stream,
        11,
        "solutions.get",
        json!({"solution_id": solution_id}),
    )
    .await;
    let old_root = resp
        .pointer("/result/structuredContent/solution/root")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("solutions.get returned: {resp}"))
        .to_string();
    let old_root = std::path::PathBuf::from(old_root);
    assert_eq!(old_root.parent(), Some(solutions_root.as_path()));
    assert!(old_root.is_dir(), "the solution root is created on disk");

    let resp = call_tool(
        &mut stream,
        2,
        "solutions.add_empty_member",
        json!({"solution_id": solution_id, "name": "Old Project"}),
    )
    .await;
    let member_id = resp
        .pointer("/result/structuredContent/member_id")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("solutions.add_empty_member returned: {resp}"));
    let resp = call_tool(
        &mut stream,
        12,
        "solutions.get",
        json!({"solution_id": solution_id}),
    )
    .await;
    let old_member = resp
        .pointer("/result/structuredContent/solution/members/0/local_path")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("solutions.get returned: {resp}"))
        .to_string();
    let old_member = std::path::PathBuf::from(old_member);
    assert!(old_member.is_dir(), "the member is created on disk");
    std::fs::write(old_member.join("marker.txt"), b"m").expect("write marker");

    // --- 2. Rename the member: the folder moves, a compat symlink stays ---
    let resp = call_tool(
        &mut stream,
        3,
        "solutions.rename_member",
        json!({"member_id": member_id, "new_name": "New Project"}),
    )
    .await;
    let member_path = resp
        .pointer("/result/structuredContent/local_path")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("solutions.rename_member returned: {resp}"))
        .to_string();
    assert_eq!(Path::new(&member_path), old_root.join("New-Project"));
    assert!(Path::new(&member_path).join("marker.txt").is_file());
    assert!(
        is_symlink(&old_member),
        "the hot rename leaves a compat symlink behind"
    );

    // --- 3. Rename the solution: the whole root moves ---
    let resp = call_tool(
        &mut stream,
        4,
        "solutions.rename",
        json!({"solution_id": solution_id, "new_name": "New Solution"}),
    )
    .await;
    let new_root = resp
        .pointer("/result/structuredContent/root")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("solutions.rename returned: {resp}"))
        .to_string();
    let new_root = Path::new(&new_root);
    assert_eq!(new_root, solutions_root.join("New-Solution"));
    assert!(new_root.join("New-Project/marker.txt").is_file());
    assert!(is_symlink(&old_root), "the old root is a compat symlink");

    // --- 4. Restart: `init_global` drains `pending_path_migrations` first ---
    cx.update(|cx| {
        solutions::SolutionStore::init_global(cx);
    });
    cx.run_until_parked();

    let resp = call_tool(
        &mut stream,
        5,
        "solutions.get",
        json!({"solution_id": solution_id}),
    )
    .await;
    let solution = resp
        .pointer("/result/structuredContent/solution")
        .cloned()
        .unwrap_or_else(|| panic!("solutions.get returned: {resp}"));
    assert_eq!(solution.get("name").and_then(Value::as_str), Some("New Solution"));
    assert_eq!(
        solution.get("root").and_then(Value::as_str),
        Some(new_root.to_string_lossy().as_ref()),
    );
    let members = solution
        .get("members")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        members.len(),
        1,
        "the member survives the rename + restart: {solution}"
    );
    assert_eq!(
        members[0].get("id").and_then(Value::as_i64),
        Some(member_id),
        "ids are stable across a rename"
    );
    assert_eq!(
        members[0].get("name").and_then(Value::as_str),
        Some("New Project")
    );
    assert_eq!(
        members[0].get("local_path").and_then(Value::as_str),
        Some(new_root.join("New-Project").to_string_lossy().as_ref()),
    );
    assert_eq!(
        members[0].get("status").and_then(Value::as_str),
        Some("ok"),
        "the member's new path is on disk: {members:?}"
    );

    // --- 5. The cold reconcile removed both compat symlinks ---
    assert!(!is_symlink(&old_root), "the root compat symlink is gone");
    let old_member_name = old_member.file_name().expect("member folder name");
    assert!(
        !is_symlink(&new_root.join(old_member_name)),
        "the member compat symlink is gone"
    );
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
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
    // Notifications interleave with responses on the same socket — skip every
    // frame that is not the response to this request.
    loop {
        let line = read_line(stream).await;
        assert!(
            !line.is_empty(),
            "socket closed while waiting for the response to {name}"
        );
        let frame: Value = serde_json::from_slice(&line).expect("parse frame");
        match frame.get("id").and_then(Value::as_u64) {
            Some(frame_id) if frame_id == id => return frame,
            _ => continue,
        }
    }
}

async fn read_line(stream: &mut UnixStream) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).await {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                buffer.push(byte[0]);
            }
            Err(_) => break,
        }
    }
    buffer
}
