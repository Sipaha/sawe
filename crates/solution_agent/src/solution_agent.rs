//! Solution-scoped AI sessions: N parallel Claude Code-style chats per Solution,
//! multiplexed onto a shared subprocess per (solution, agent) pair.
//!
//! See `docs/superpowers/specs/2026-04-26-solution-scoped-ai-sessions-design.md`
//! for the design rationale.

pub mod actions;
pub mod adapter;
pub mod agent_settings;
pub mod background_agent;
pub mod background_shell;
pub mod claude_adapter;
pub(crate) mod cold_persistence;
pub(crate) mod compact;
pub(crate) mod conversation_render;
pub(crate) mod db;
pub mod event_sources;
pub(crate) mod expanded_compose;
pub mod mcp;
pub mod message_generator;
pub(crate) mod metrics_emitter;
pub mod model;
pub mod notifier;
pub(crate) mod pool;
pub mod rename_session_modal;
pub mod reopen_session_modal;
pub mod session_entry;
pub mod session_view;
pub(crate) mod slash_commands;
pub mod status_item;
pub(crate) mod status_row;
pub mod store;
pub mod stream;
pub mod supervisor;
pub mod supervisor_instruction_modal;
pub mod upload;

pub use claude_native::ModelInfo;
pub use metrics_emitter::MetricsEmitter;

#[cfg(any(feature = "test-support", test))]
pub mod test_support;

pub use background_agent::{BackgroundAgent, BackgroundAgentId, BackgroundAgentSnapshot};
pub use background_shell::{
    BackgroundShell, BackgroundShellId, BackgroundShellSnapshot, ShellRuntimeState,
};
pub use model::{
    AgentServerId, SessionState, SolutionSession, SolutionSessionId, SolutionSessionMetadata,
};
pub use store::EFFORT_LEVELS;

use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{App, AppContext, AsyncApp, SharedString};
use project::agent_server_store::AgentId;

pub fn init(cx: &mut App) {
    use ::settings::Settings as _;
    agent_settings::SolutionAgentSettings::register(cx);

    let mut adapters = adapter::AdapterRegistry::new();
    adapters.register(Arc::new(claude_adapter::ClaudeAcpAdapter));
    let adapters = Arc::new(adapters);

    store::SolutionAgentStore::init_global(cx, adapters);

    // Register the AgentServer instance for `claude-acp`. The native Rust
    // stream-json backend (`claude_native::ClaudeNativeAgentServer`) spawns the
    // `claude` binary directly — no node wrapper. The legacy
    // `@agentclientprotocol/claude-agent-acp` path was retired in commit
    // history; revert via git if it ever needs to come back.
    let claude_id = AgentId(SharedString::from(claude_adapter::CLAUDE_ACP_AGENT_ID));
    let claude_server: Rc<dyn agent_servers::AgentServer> =
        Rc::new(claude_native::ClaudeNativeAgentServer::new(claude_id));
    store::SolutionAgentStore::global(cx).update(cx, |store, _cx| {
        store.register_agent_server(
            SharedString::from(claude_adapter::CLAUDE_ACP_AGENT_ID),
            claude_server,
        );
    });

    // Connect the persistence DB asynchronously and wire it into the store
    // once it's ready. Failure to open the DB is logged but non-fatal — the
    // store falls back to in-memory state.
    let db_task = db::SolutionAgentDb::connect(cx);
    cx.spawn(async move |cx: &mut AsyncApp| match db_task.await {
        Ok(db) => {
            cx.update(|cx| {
                let store = store::SolutionAgentStore::global(cx);
                store.update(cx, |store, cx| store.set_persistence(db, cx));
            });
        }
        Err(err) => {
            log::error!("solution_agent: failed to open persistence DB: {err}");
        }
    })
    .detach();

    mcp::register(cx);
    event_sources::install(cx);

    // Chunked-upload manager: shared between the listener (pure tokio
    // binary-frame handler) and the `solution_agent.upload_*` MCP tools
    // (GPUI context). Bytes land under `<editor_mcp::runtime_dir>/uploads/`
    // so they share the lifetime of the editor's runtime root — tests can
    // pin this via `editor_mcp::set_runtime_dir_for_test`.
    let tmp_root = editor_mcp::runtime_dir().join("uploads");
    match upload::UploadManager::new(tmp_root) {
        Ok(manager) => {
            let handle = Arc::new(Mutex::new(manager));
            upload::install(handle);
            spawn_upload_ack_drainer(cx);
            spawn_upload_gc(cx);
        }
        Err(err) => {
            log::error!("solution_agent: failed to init upload manager: {err}");
        }
    }

    // Workspace hook for the status-bar item. The standalone
    // `SolutionSessionsNavigator` dock was removed when ConsolePanel
    // took over chat hosting — `FocusNavigator` is now a no-op until
    // B10 rewires it to focus the ConsolePanel's chat tab. The action
    // is kept registered so the keybind still resolves (instead of
    // surfacing as "no handler").
    cx.observe_new::<workspace::Workspace>(|workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        // TODO(B10): focus the active ConsolePanel chat tab here once
        // ConsolePanel's focus API is settled.
        workspace.register_action(|_workspace, _: &actions::FocusNavigator, _window, _cx| {
            log::debug!("FocusNavigator dispatched: no-op until B10 wires ConsolePanel focus");
        });

        let status_item = cx.new(|cx| status_item::SolutionAgentStatusItem::new(cx));
        workspace.status_bar().update(cx, |bar, cx| {
            bar.add_right_item(status_item, window, cx);
        });
    })
    .detach();
}

/// Drain queued chunk-ack events from the `UploadManager` and broadcast each
/// one as an `upload_chunk_acked` MCP notification. The listener (pure tokio)
/// can't call `editor_mcp::emit_notification` directly because the underlying
/// `McpServer` uses `RefCell` and must be touched from the GPUI thread, so the
/// ack queue inside `UploadManager` is the cross-thread hand-off.
///
/// 100ms tick is fast enough that mobile progress bars feel live but slow
/// enough that an idle editor isn't waking up for nothing. The drainer only
/// emits when the queue has acks — empty drains are a single Vec::take + early
/// continue.
fn spawn_upload_ack_drainer(cx: &mut App) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            // `AsyncApp::update` panics if the App was dropped — the task
            // is detached, so the panic is contained to this task (matches
            // every other detached `cx.spawn` site in the crate).
            cx.update(|cx| {
                let acks = upload::with_manager(|m| m.drain_acks()).unwrap_or_default();
                for ack in acks {
                    log::info!(
                        target: "solution_agent::upload",
                        "drainer emit upload_chunk_acked: upload_id={} received={}",
                        ack.upload_id,
                        ack.received_bytes,
                    );
                    let payload = serde_json::json!({
                        "upload_id": ack.upload_id,
                        "received_bytes": ack.received_bytes,
                    });
                    editor_mcp::emit_notification(cx, "upload_chunk_acked", payload);
                }
            });
        }
    })
    .detach();
}

/// Reap stale uploads every 5 minutes. An attacker who could exhaust disk by
/// init-ing thousands of uploads + never finishing is bounded by the
/// per-session cap inside `UploadManager`, but the periodic GC catches the
/// "legitimate client uploaded and crashed" case too.
fn spawn_upload_gc(cx: &mut App) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            cx.background_executor()
                .timer(Duration::from_secs(5 * 60))
                .await;
            cx.update(|_cx| {
                upload::with_manager(|m| {
                    let n = m.gc(std::time::Instant::now(), upload::UPLOAD_TTL);
                    if n > 0 {
                        log::info!("upload::gc: reaped {n} expired entries");
                    }
                });
            });
        }
    })
    .detach();
}
