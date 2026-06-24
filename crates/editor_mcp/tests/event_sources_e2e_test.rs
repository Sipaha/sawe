//! End-to-end integration test for event sources.
//!
//! Validates that a client which calls `editor.subscribe` for `solution_changed`
//! receives a JSON-RPC notification when the in-process `SolutionStore` emits
//! its `Changed` event (via a real catalog mutation).
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
async fn solution_changed_notification_e2e(cx: &mut TestAppContext) {
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
        solutions::install_event_sources_for_test(cx);
    });

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

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "editor.subscribe",
            "arguments": { "kinds": ["solution_changed"] }
        }
    });
    let mut bytes = serde_json::to_vec(&req).expect("serialize subscribe");
    bytes.push(b'\n');
    stream.write_all(&bytes).await.expect("write subscribe");
    let _ack = read_line(&mut stream).await;

    // Trigger a real SolutionStore mutation. add_catalog_project emits
    // SolutionStoreEvent::Changed, which our coordinator translates into an
    // `editor/notification` with kind `solution_changed`.
    cx.update(|cx| {
        store.update(cx, |s, cx| {
            s.add_catalog_project("Demo", "git@example.com:demo.git", None, cx)
                .expect("add_catalog_project");
        });
    });

    cx.executor().timer(Duration::from_millis(100)).await;

    let line = read_line(&mut stream).await;
    let parsed: serde_json::Value = serde_json::from_slice(&line).expect("parse notification");
    assert_eq!(parsed.get("jsonrpc").and_then(|v| v.as_str()), Some("2.0"));
    assert!(
        parsed.get("id").is_none() || parsed.get("id").is_some_and(|v| v.is_null()),
        "notification should not carry an id, got: {parsed:?}"
    );
    assert_eq!(
        parsed.get("method").and_then(|v| v.as_str()),
        Some("editor/notification")
    );
    let kind = parsed
        .pointer("/params/kind")
        .and_then(|v| v.as_str())
        .expect("kind");
    assert_eq!(kind, "solution_changed");
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
