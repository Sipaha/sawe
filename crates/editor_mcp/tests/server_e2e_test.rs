//! End-to-end integration test for editor_mcp's registry → start_server →
//! socket → JSON-RPC dispatch chain.
//!
//! This test exercises the FULL wire layer:
//! 1. Boot a TestAppContext.
//! 2. Run `editor_mcp::init` (sets up Registry + global, registers built-in
//!    tools including `editor.capabilities`).
//! 3. Run `editor_mcp::start_server` (acquires the single-instance lock,
//!    creates an `McpServer` bound to a tempdir socket, and symlinks the
//!    well-known `paths::config_dir()/mcp.sock` to it).
//! 4. Wait briefly for the symlink to appear (the bind is async).
//! 5. Connect a raw JSON-RPC client to the well-known socket.
//! 6. Send `tools/list`, assert `editor.capabilities` is among the names.
//! 7. Send `tools/call` for `editor.capabilities`, assert
//!    `result.structuredContent.protocol_version == "2024-11-05"`.
//!
//! Isolation: the test pins the lock + socket directory to a tempdir via
//! `editor_mcp::set_runtime_dir_for_test` so it can run alongside a live
//! `spk-editor` instance without colliding on the user's real
//! `~/.config/spk-editor/mcp.{lock,sock}` files.

use gpui::TestAppContext;
use serde_json::json;
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use std::time::Duration;

#[gpui::test]
async fn end_to_end_capabilities_via_socket(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let runtime_dir = tempfile::tempdir().expect("tempdir");
    editor_mcp::set_runtime_dir_for_test(runtime_dir.path().to_path_buf());

    cx.update(|cx| {
        editor_mcp::init(cx);
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
    let interval = Duration::from_millis(100);
    while !socket_path.exists() && waited < timeout {
        cx.executor().timer(interval).await;
        waited += interval;
    }
    assert!(
        socket_path.exists(),
        "mcp.sock did not appear within {}s",
        timeout.as_secs()
    );

    let mut stream = UnixStream::connect(&socket_path)
        .await
        .expect("connect to socket");

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    });
    let mut bytes = serde_json::to_vec(&req).expect("serialize tools/list");
    bytes.push(b'\n');
    stream.write_all(&bytes).await.expect("write tools/list");

    let response = read_line(&mut stream).await;
    let parsed: serde_json::Value =
        serde_json::from_slice(&response).expect("parse tools/list response");
    let tools = parsed
        .pointer("/result/tools")
        .and_then(|v| v.as_array())
        .expect("tools array in response");
    let names: Vec<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    assert!(
        names.iter().any(|n| n == "editor.capabilities"),
        "tools/list missing editor.capabilities; got: {:?}",
        names
    );

    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "editor.capabilities",
            "arguments": {}
        }
    });
    let mut bytes = serde_json::to_vec(&req).expect("serialize tools/call");
    bytes.push(b'\n');
    stream.write_all(&bytes).await.expect("write tools/call");

    let response = read_line(&mut stream).await;
    let parsed: serde_json::Value =
        serde_json::from_slice(&response).expect("parse tools/call response");
    let caps = parsed
        .pointer("/result/structuredContent")
        .expect("structuredContent in response");
    let proto_version = caps
        .get("protocol_version")
        .and_then(|v| v.as_str())
        .expect("protocol_version field");
    assert_eq!(proto_version, "2024-11-05");
    let wire_schema_version = caps
        .get("wire_schema_version")
        .and_then(|v| v.as_u64())
        .expect("wire_schema_version field");
    assert!(
        wire_schema_version >= 2,
        "wire_schema_version should be >= 2 after workspace.* namespace + window_open/close_session renames; got {}",
        wire_schema_version
    );
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
