//! End-to-end test that `solution_agent.create_session` reports a
//! structured tool error when no workspace is open for the named
//! Solution.
//!
//! Production callers must `solutions.open` a Solution before they can
//! create an agent session against it; this test exercises the negative
//! branch so the error surfaces via the standard `result.isError` channel
//! and not as a JSON-RPC protocol error or a swallowed panic.
//!
//! Isolation: pins the lock + socket to a tempdir via
//! `editor_mcp::set_runtime_dir_for_test`. Lives in its own `tests/*.rs`
//! file (= separate test binary) because `start_server` binds a single
//! Unix socket and `set_runtime_dir_for_test` is a process-global
//! `OnceLock` — running multiple `#[gpui::test]`s in the same file would
//! race them through the same socket.

use std::sync::Arc;
use std::time::Duration;

use gpui::TestAppContext;
use serde_json::json;
use settings::SettingsStore;
use smol::net::unix::UnixStream;

mod support;

#[gpui::test]
async fn create_session_without_workspace_errors(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    let socket_path = runtime_dir.path().join("mcp.sock");
    editor_mcp::set_runtime_dir_for_test(runtime_dir.path().to_path_buf());

    let work_dir = tempfile::tempdir().expect("work tempdir");
    let cfg_path = work_dir.path().join("solutions.json");

    cx.update(|cx| {
        editor_mcp::init(cx);

        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        <solutions::SolutionsSettings as settings::Settings>::register(cx);
        let store = solutions::SolutionStore::for_test(cfg_path, cx);
        solutions::install_global_for_test(store, cx);

        let registry = Arc::new(solution_agent::adapter::AdapterRegistry::new());
        solution_agent::store::SolutionAgentStore::init_global(cx, registry);
        solution_agent::mcp::register(cx);
        solution_agent::event_sources::install(cx);
    });

    let start_result = cx.update(|cx| editor_mcp::start_server(cx));
    assert!(
        start_result.is_ok(),
        "start_server: {:?}",
        start_result.err()
    );

    assert!(
        support::wait_for_socket(&socket_path, Duration::from_secs(10)).await,
        "mcp.sock did not appear within 10s at {}",
        socket_path.display()
    );

    let mut stream = UnixStream::connect(&socket_path)
        .await
        .expect("connect to socket");

    let resp = support::call_tool(
        &mut stream,
        1,
        "solution_agent.create_session",
        json!({
            "solution_id": "nonexistent",
            "agent_id": "claude-acp",
        }),
    )
    .await;

    // The MCP tool framework reports tool-side errors via `result.isError`
    // (with a textual `content[0].text`) rather than the JSON-RPC `error`
    // field — the latter is reserved for protocol-level failures (bad
    // params, unknown method, etc.). Accept either.
    let is_jsonrpc_error = resp.get("error").is_some();
    let is_tool_error = resp
        .pointer("/result/isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(
        is_jsonrpc_error || is_tool_error,
        "expected an error response, got: {resp}"
    );

    drop(runtime_dir);
    drop(work_dir);
}
