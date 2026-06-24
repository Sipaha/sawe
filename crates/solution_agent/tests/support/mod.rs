//! Shared helpers for `solution_agent` integration tests.
//!
//! Each `tests/*_e2e_test.rs` file declares `mod support;` at the top to
//! pull these in. The MCP wire format (newline-delimited JSON-RPC 2.0) and
//! the notification-skipping pattern are mirrored from
//! `crates/editor_mcp/tests/solutions_add_member_e2e_test.rs::call_tool`.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use smol::io::{AsyncReadExt, AsyncWriteExt};
use smol::net::unix::UnixStream;

/// Poll for the MCP socket to appear (the server creates it asynchronously
/// after `start_server`). Returns whether the socket existed before the
/// timeout elapsed.
///
/// `smol::Timer::after` is used (instead of GPUI's executor timer) because
/// the wait is for a filesystem-level event whose progress doesn't depend
/// on `run_until_parked`. Promoting this to the GPUI executor would just
/// add a dependency without changing observable behavior.
pub async fn wait_for_socket(path: &Path, timeout: Duration) -> bool {
    let mut waited = Duration::ZERO;
    let interval = Duration::from_millis(50);
    while !path.exists() && waited < timeout {
        #[allow(clippy::disallowed_methods)]
        {
            smol::Timer::after(interval).await;
        }
        waited += interval;
    }
    path.exists()
}

/// Read one newline-delimited frame from the socket. Returns the line
/// without the trailing `\n`. An empty `Vec` indicates EOF or read error.
pub async fn read_line(stream: &mut UnixStream) -> Vec<u8> {
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

/// Send a JSON-RPC `tools/call` request and return the matching response.
///
/// Skips `editor/notification` frames (no `id`) and frames whose `id` does
/// not match the request — the MCP server interleaves event broadcasts
/// (e.g. `agent_session_*`) with regular responses on the same socket, so
/// clients must filter by id.
pub async fn call_tool(stream: &mut UnixStream, id: u64, name: &str, args: Value) -> Value {
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": args }
    });
    let mut bytes = serde_json::to_vec(&req).expect("serialize request");
    bytes.push(b'\n');
    stream.write_all(&bytes).await.expect("write request");
    loop {
        let line = read_line(stream).await;
        if line.is_empty() {
            panic!("socket closed while waiting for response to id {id} ({name})");
        }
        let value: Value = serde_json::from_slice(&line).expect("parse JSON-RPC frame");
        if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
            return value;
        }
    }
}
