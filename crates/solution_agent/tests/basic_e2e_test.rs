//! End-to-end roundtrip test for `solution_agent.list_sessions` over the
//! real Unix-socket wire format.
//!
//! Verifies that the MCP layer correctly translates a JSON-RPC
//! `tools/call` frame into a `SolutionAgentStore` read and that the
//! response shape (`result.structuredContent.sessions: []`) matches what
//! external clients expect at startup, before any session has been
//! created.
//!
//! The full happy-path with a live workspace + real `MockAgentServer` is
//! covered by the in-crate unit tests; this wire-layer test exists solely
//! to catch regressions in tool registration, request dispatch, and
//! response shape.
//!
//! Isolation: pins the lock + socket to a tempdir via
//! `editor_mcp::set_runtime_dir_for_test` so it is safe to run alongside a
//! live `spk-editor` instance and never touches the user's real
//! `~/.config/spk-editor/mcp.{lock,sock}` files.
//!
//! One test per binary: cargo runs `#[gpui::test]`s in the same file in
//! parallel threads, but `set_runtime_dir_for_test` writes to a process-
//! global `OnceLock` and `start_server` binds a single Unix socket — so
//! splitting concerns into sibling `tests/*.rs` files (each a separate
//! binary) is the established pattern in `crates/editor_mcp/tests/`.

use std::sync::Arc;
use std::time::Duration;

use gpui::TestAppContext;
use serde_json::json;
use settings::SettingsStore;
use smol::net::unix::UnixStream;

mod support;

#[gpui::test]
async fn list_sessions_starts_empty(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    // Pin lock + socket to a tempdir BEFORE init — without this the test
    // would corrupt the user's real `~/.config/spk-editor/mcp.sock`.
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

    let resp = support::call_tool(&mut stream, 1, "solution_agent.list_sessions", json!({})).await;

    let sessions = resp
        .pointer("/result/structuredContent/sessions")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("missing sessions array; full response: {resp}"));
    assert_eq!(sessions.len(), 0, "expected empty session list at startup");

    // Hold the tempdirs alive until after assertions — `runtime_dir`
    // owns the directory containing `mcp.sock`; `work_dir` owns the
    // SolutionStore's config file path. Either being dropped early
    // would leave the assertions racing against directory cleanup.
    drop(runtime_dir);
    drop(work_dir);
}
