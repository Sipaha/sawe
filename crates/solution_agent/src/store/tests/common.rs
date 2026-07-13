#![allow(unused_imports)]

use crate::adapter::AdapterRegistry;
use crate::model::SessionState;
use crate::store::*;
use crate::test_support::{MockAgentServer, MockConnection};
use chrono::Utc;
use gpui::{Entity, SharedString, TestAppContext};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Insert a minimal cold session (no `acp_thread`) directly into the store
/// for tests that need a pre-existing session without going through the full
/// `create_session` → ACP-handshake flow.
pub(crate) fn insert_cold_session(
    session_id: crate::model::SolutionSessionId,
    solution_id: solutions::SolutionId,
    agent_id: gpui::SharedString,
    cached_total_tokens: Option<u64>,
    project: Option<Entity<project::Project>>,
    store: &mut SolutionAgentStore,
    cx: &mut gpui::Context<SolutionAgentStore>,
) -> Entity<crate::model::SolutionSession> {
    let session = cx.new(|_| {
        let mut s = crate::model::SolutionSession::new_idle(
            session_id,
            solution_id,
            agent_id,
            agent_client_protocol::schema::SessionId::new("acp-cold"),
        );
        s.title = SharedString::from("Cold");
        s.project = project;
        s.cached_total_tokens = cached_total_tokens;
        s
    });
    store.sessions.insert(session_id, session.clone());
    store
        .by_solution
        .entry(solution_id)
        .or_default()
        .push(session_id);
    session
}

/// Set up SolutionStore with one Solution rooted at a tempdir, plus
/// a `Project::test` whose worktree is that root. Returns
/// (`SolutionId`, `tempdir`, `Project`). Hold the tempdir for the
/// lifetime of the test — `create_solution` writes to it.
pub(crate) async fn setup_solution_and_project(
    cx: &mut TestAppContext,
) -> (
    SolutionId,
    tempfile::TempDir,
    gpui::Entity<project::Project>,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg_path = dir.path().join("solutions.json");
    let solutions_root = dir.path().join("solutions");
    std::fs::create_dir_all(&solutions_root).expect("solutions root");
    let store = cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
        let store = solutions::SolutionStore::for_test(cfg_path, cx);
        solutions::install_global_for_test(store.clone(), cx);
        store
    });
    let solution_id = store
        .update(cx, |store, cx| {
            store.create_solution("Sol", solutions_root.clone(), cx)
        })
        .expect("create_solution");
    let solution_root: PathBuf = store.read_with(cx, |store, _| {
        store
            .solutions()
            .iter()
            .find(|s| s.id == solution_id)
            .map(|s| s.root.clone())
            .expect("solution exists")
    });

    let fs = fs::FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(solution_root.clone(), serde_json::json!({ ".keep": "" }))
        .await;
    let project = project::Project::test(fs, [solution_root.as_path()], cx).await;

    (solution_id, dir, project)
}

/// Create a real session (via `create_session`) backed by `MockAgentServer`/
/// `MockConnection`, then return both its id and a clone of the underlying
/// `Entity<AcpThread>` so tests can emit synthetic `AcpThreadEvent`s.
pub(crate) async fn create_session_with_thread(
    cx: &mut TestAppContext,
) -> (
    SolutionSessionId,
    gpui::Entity<acp_thread::AcpThread>,
    tempfile::TempDir,
) {
    let (solution_id, tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    let connect_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::new(connect_count.clone())),
            );
        });
    });

    let session_id = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session(solution_id, agent_id.clone(), project.clone(), cx)
            })
        })
        .await
        .expect("create_session");

    let acp_thread = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .acp_thread()
            .cloned()
            .expect("acp_thread populated")
    });

    (session_id, acp_thread, tmp)
}

// ---------------------------------------------------------------------------
// Etap 3: Subagent-tab lifecycle (`teammate_labels`). Since wire v5 the durable
// friendly label rides `teammate_labels`; a teammate's rendered pill + wire label
// come from `Stream.label` (enriched from that map at `rebuild_streams`).
// These exercise `SolutionAgentStore::apply_subagent_lifecycle` through the
// real `AcpThreadEvent::NewEntry` / `EntryUpdated` plumbing — by upserting
// `acp::ToolCall` shapes directly on a live `AcpThread` and asserting how
// the per-session map and the `SessionSubagentsChanged` event stream react.
// ---------------------------------------------------------------------------

/// Build an `acp::ToolCall` for a Task/Agent subagent dispatch with the
/// programmatic name carried in `_meta.tool_name` (the convention shared by
/// `claude_native::translate_assistant` and consumed by
/// `apply_subagent_lifecycle`). Optional `description` populates
/// `raw_input["description"]` so the label-fallback chain can be exercised.
pub(crate) fn make_task_tool_call(
    id: &str,
    tool_name: &str,
    status: agent_client_protocol::schema::ToolCallStatus,
    description: Option<&str>,
    subagent_type: Option<&str>,
) -> agent_client_protocol::schema::ToolCall {
    use agent_client_protocol::schema as acp;
    let mut raw_input = serde_json::Map::new();
    if let Some(d) = description {
        raw_input.insert("description".into(), serde_json::Value::String(d.into()));
    }
    if let Some(s) = subagent_type {
        raw_input.insert("subagent_type".into(), serde_json::Value::String(s.into()));
    }
    let mut call = acp::ToolCall::new(acp::ToolCallId::new(id.to_string()), tool_name.to_string())
        .kind(acp::ToolKind::Think)
        .status(status)
        .meta(Some(acp_thread::meta_with_tool_name(tool_name)));
    if !raw_input.is_empty() {
        call = call.raw_input(serde_json::Value::Object(raw_input));
    }
    call
}
