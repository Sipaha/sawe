//! Context rotation inside the running agent process (`/clear` in place): the
//! compaction path that keeps background sub-agents alive. Before it, every
//! rotation replaced the `claude` process, and six working sub-agents died with
//! one on 2026-09-28.

use super::common::*;
use crate::model::SessionState;
use crate::store::*;
use crate::adapter::AdapterRegistry;
use crate::test_support::{MockAgentServer, MockConnection, PromptGate};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use agent_client_protocol::schema::v1 as acp;
use gpui::{Entity, SharedString, TestAppContext};

fn mock_connection(thread: &Entity<acp_thread::AcpThread>, cx: &mut TestAppContext) -> std::rc::Rc<MockConnection> {
    thread.read_with(cx, |thread, _| {
        thread
            .connection()
            .clone()
            .into_any()
            .downcast::<MockConnection>()
            .expect("mock connection")
    })
}

fn add_running_background_agent(session_id: SolutionSessionId, cx: &mut TestAppContext) {
    let id = crate::background_agent::BackgroundAgentId::new("ab4ckgr0undw0rker");
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        session.update(cx, |s, _| {
            s.background_agents.insert(
                id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: id.clone(),
                    jsonl_path: "/nonexistent".into(),
                    registered_at: chrono::Utc::now(),
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now(),
                        activity_label: SharedString::from("Editing files"),
                        stop_reason: None,
                        usage_limited: false,
                    }),
                    last_offset: 0,
                    parent_tool_use_id: None,
                    latest_seq: 0,
                    killed: false,
                },
            );
        });
    });
}

/// A session whose agent turns hang until the test releases them — so the
/// thread is genuinely `Generating`, which is what an in-place rotation waits
/// out (the session's own `SessionState` is not: background activity flips it).
pub(super) async fn create_gated_session(
    cx: &mut TestAppContext,
) -> (
    SolutionSessionId,
    Entity<acp_thread::AcpThread>,
    async_channel::Sender<()>,
    tempfile::TempDir,
) {
    let (solution_id, tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");
    let (gate_tx, gate_rx) = async_channel::unbounded::<()>();
    cx.update(|cx| {
        SolutionAgentStore::init_global(cx, Arc::new(AdapterRegistry::new()));
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::configured(
                    Arc::new(AtomicUsize::new(0)),
                    None,
                    Some(PromptGate(gate_rx)),
                    None,
                    false,
                )),
            );
        });
    });
    let session_id = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.create_session(solution_id, agent_id.clone(), project.clone(), cx)
            })
        })
        .await
        .expect("create_session");
    let thread = cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .and_then(|s| s.read(cx).acp_thread().cloned())
            .expect("live thread")
    });
    (session_id, thread, gate_tx, tmp)
}

pub(super) fn start_turn(session_id: SolutionSessionId, cx: &mut TestAppContext) {
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.send_message(session_id, "work".into(), cx).detach();
        })
    });
    cx.executor().run_until_parked();
}

fn session_snapshot(
    session_id: SolutionSessionId,
    cx: &mut TestAppContext,
) -> (u32, acp::SessionId, usize, bool) {
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        (
            s.context_count,
            s.acp_session_id.clone(),
            s.running_background_agents(chrono::Utc::now()),
            s.pending_rotation.is_some(),
        )
    })
}

/// The rotation goes through `/clear` in the same process when the agent
/// offers it: the session moves to the new conversation id, nothing is closed,
/// and a working sub-agent is NOT marked killed.
#[gpui::test]
async fn rotation_clears_in_place_and_keeps_background_agents(cx: &mut TestAppContext) {
    let (session_id, old_thread, _tmp) = create_session_with_thread(cx).await;
    let connection = mock_connection(&old_thread, cx);
    connection.set_in_place_clear(true);
    add_running_background_agent(session_id, cx);
    let (count_before, old_id, _, _) = session_snapshot(session_id, cx);

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| store.rotate_context(session_id, cx))
    })
    .await
    .expect("rotate_context");
    cx.executor().run_until_parked();

    let (count_after, new_id, running, _) = session_snapshot(session_id, cx);
    assert_eq!(count_after, count_before + 1);
    assert_eq!(&*new_id.0, format!("{}-cleared", old_id.0).as_str());
    assert_eq!(connection.in_place_clears(), 1);
    assert_eq!(
        connection.live_sessions(),
        vec![new_id],
        "the process lives on under the new id; nothing was closed"
    );
    assert_eq!(running, 1, "the background agent must survive an in-place rotation");
}

/// The same `/clear`, reached from the user's Clear: it too keeps the process.
#[gpui::test]
async fn reset_context_clears_in_place_and_keeps_background_agents(cx: &mut TestAppContext) {
    let (session_id, old_thread, _tmp) = create_session_with_thread(cx).await;
    let connection = mock_connection(&old_thread, cx);
    connection.set_in_place_clear(true);
    add_running_background_agent(session_id, cx);

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");
    cx.executor().run_until_parked();

    let (_, new_id, running, _) = session_snapshot(session_id, cx);
    assert_eq!(connection.in_place_clears(), 1);
    assert_eq!(connection.live_sessions(), vec![new_id]);
    assert_eq!(running, 1);
}

/// Without in-place support the rotation still replaces the process — and
/// that one does kill the sub-agents, which is why the compaction guard
/// stays for it.
#[gpui::test]
async fn rotation_without_in_place_support_replaces_the_process(cx: &mut TestAppContext) {
    let (session_id, old_thread, _tmp) = create_session_with_thread(cx).await;
    let connection = mock_connection(&old_thread, cx);
    add_running_background_agent(session_id, cx);
    let (_, old_id, _, _) = session_snapshot(session_id, cx);

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| store.rotate_context(session_id, cx))
    })
    .await
    .expect("rotate_context");
    cx.executor().run_until_parked();

    let (_, new_id, running, _) = session_snapshot(session_id, cx);
    assert_eq!(connection.in_place_clears(), 0);
    assert!(!connection.live_sessions().contains(&old_id), "the old process was closed");
    assert!(connection.live_sessions().contains(&new_id));
    assert_eq!(running, 0, "a replaced process takes its sub-agents with it");
}

/// `compact_session` is called from INSIDE the agent's turn, and a process can
/// only clear itself between turns: the handoff waits for the turn's end, then
/// rotates.
#[gpui::test]
async fn a_handoff_accepted_mid_turn_rotates_when_the_turn_ends(cx: &mut TestAppContext) {
    let (session_id, thread, gate, _tmp) = create_gated_session(cx).await;
    let connection = mock_connection(&thread, cx);
    connection.set_in_place_clear(true);
    let (count_before, _, _, _) = session_snapshot(session_id, cx);
    start_turn(session_id, cx);

    let deferred = cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.rotate_and_continue(session_id, "continue from the handoff".into(), cx)
        })
    });
    assert!(deferred, "the agent's running turn defers the rotation");
    cx.executor().run_until_parked();
    let (count_mid, _, _, pending) = session_snapshot(session_id, cx);
    assert_eq!(count_mid, count_before, "nothing rotates while the turn runs");
    assert!(pending);

    // The turn ends the ordinary way: the prompt resolves and the thread
    // emits `Stopped(EndTurn)`.
    gate.send(()).await.expect("release the turn");
    cx.executor().run_until_parked();

    let (count_after, _, _, pending) = session_snapshot(session_id, cx);
    assert_eq!(count_after, count_before + 1, "the turn's end runs the rotation");
    assert!(!pending);
    assert_eq!(connection.in_place_clears(), 1);
}

/// Background activity marks the SESSION running while the agent itself is
/// idle; that must not defer (or refuse) the rotation — keeping that activity
/// alive is the point of rotating in place.
#[gpui::test]
async fn background_activity_does_not_hold_the_rotation(cx: &mut TestAppContext) {
    let (session_id, thread, _tmp) = create_session_with_thread(cx).await;
    let connection = mock_connection(&thread, cx);
    connection.set_in_place_clear(true);
    add_running_background_agent(session_id, cx);
    let (count_before, _, _, _) = session_snapshot(session_id, cx);

    let deferred = cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                }
            });
            store.rotate_and_continue(session_id, "continue".into(), cx)
        })
    });
    assert!(!deferred);
    cx.executor().run_until_parked();
    let (count_after, _, running, _) = session_snapshot(session_id, cx);
    assert_eq!(count_after, count_before + 1);
    assert_eq!(running, 1);
}

/// The user stopping the turn abandons the parked handoff instead of wiping
/// the context behind their back.
#[gpui::test]
async fn a_stopped_turn_abandons_the_parked_handoff(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    let connection = mock_connection(&thread, cx);
    connection.set_in_place_clear(true);
    let (count_before, _, _, _) = session_snapshot(session_id, cx);
    start_turn(session_id, cx);

    let deferred = cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.rotate_and_continue(session_id, "continue".into(), cx)
        })
    });
    assert!(deferred);
    thread.update(cx, |_, cx| {
        cx.emit(acp_thread::AcpThreadEvent::Stopped(acp::StopReason::Cancelled))
    });
    cx.executor().run_until_parked();

    let (count_after, _, _, pending) = session_snapshot(session_id, cx);
    assert_eq!(count_after, count_before);
    assert!(!pending);
    assert_eq!(connection.in_place_clears(), 0);
}
