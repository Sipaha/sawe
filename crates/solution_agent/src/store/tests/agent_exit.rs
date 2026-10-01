//! Agent-process death: the session must park at Idle with a visible note,
//! the dead connection must leave the pool, and the next send must
//! transparently respawn + resume instead of failing on the corpse.
//!
//! The death signal itself (`AcpThreadEvent::LoadError(LoadError::Exited)`)
//! is what `agent_servers::acp`'s process-exit watcher emits to every
//! session on the connection — the tests emit it straight onto the thread,
//! which is exactly what the store subscription ingests.

#![allow(unused_imports)]

use super::common::*;
use crate::adapter::AdapterRegistry;
use crate::model::SessionState;
use crate::store::*;
use crate::test_support::{MockAgentServer, PromptGate};
use gpui::{Entity, SharedString, TestAppContext};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Killed-by-SIGKILL shape on unix; a real one-shot process elsewhere. Only
/// the Display of the status matters (log line + LoadError's own rendering) —
/// never an assertion — so the platform difference is inert.
#[cfg(unix)]
fn exited_status() -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;
    std::process::ExitStatus::from_raw(9)
}

#[cfg(not(unix))]
fn exited_status() -> std::process::ExitStatus {
    std::process::Command::new("cmd")
        .args(["/c", "exit", "1"])
        .status()
        .expect("probe exit status")
}

fn emit_process_exit(thread: &Entity<acp_thread::AcpThread>, cx: &mut gpui::App) {
    thread.update(cx, |thread, cx| {
        thread.emit_load_error(
            acp_thread::LoadError::Exited {
                status: exited_status(),
                stderr: None,
            },
            cx,
        );
    });
}

fn session_snapshot(
    cx: &mut TestAppContext,
    session_id: crate::model::SolutionSessionId,
) -> (SessionState, bool, Option<(crate::session_entry::SystemEntryLevel, String)>) {
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let session = session.read(cx);
            let note = session.entries.last().and_then(|entry| match &entry.kind {
                crate::session_entry::SessionEntryKind::System { level, text_md } => {
                    Some((*level, text_md.clone()))
                }
                _ => None,
            });
            (session.state.clone(), session.is_cold(), note)
        })
    })
}

#[gpui::test]
async fn process_exit_parks_idle_with_note_and_clears_pool(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");
    let connect_count = std::sync::Arc::new(AtomicUsize::new(0));
    let server = Rc::new(MockAgentServer::configured(
        connect_count.clone(),
        None,
        None,
        None,
        true,
    ));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.register_agent_server(agent_id.clone(), server.clone());
        });
    });

    // Two co-tenant sessions collapse onto ONE pooled connection.
    let mut sessions = Vec::new();
    for _ in 0..2 {
        let session_id = cx
            .update(|cx| {
                let store = SolutionAgentStore::global(cx);
                store.update(cx, |store, cx| {
                    store.create_session(solution_id, agent_id.clone(), project.clone(), cx)
                })
            })
            .await
            .expect("create_session");
        sessions.push(session_id);
    }
    assert_eq!(
        connect_count.load(Ordering::SeqCst),
        1,
        "fixture: both sessions share one pooled connection"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| assert_eq!(store.pool_size(), 1));
    });

    // The process dies; the connection's exit watcher emits to every session.
    for &session_id in &sessions {
        let thread = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store
                .read(cx)
                .session(session_id)
                .expect("session exists")
                .read(cx)
                .acp_thread()
                .cloned()
                .expect("session is hot")
        });
        cx.update(|cx| emit_process_exit(&thread, cx));
    }
    cx.executor().run_until_parked();

    for &session_id in &sessions {
        let (state, cold, note) = session_snapshot(cx, session_id);
        assert!(matches!(state, SessionState::Idle), "state: {state:?}");
        assert!(cold, "the dead thread must be detached (session goes cold)");
        let (level, text) = note.expect("a system note must be appended to the transcript");
        assert_eq!(
            level,
            crate::session_entry::SystemEntryLevel::Error,
            "process death is an error-level note"
        );
        assert!(
            text.contains("agent process exited unexpectedly"),
            "note must explain what happened and that the next message restarts, got: {text}"
        );
        assert!(
            text.contains("next message"),
            "note must tell the user their next message restarts the agent, got: {text}"
        );
    }
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            assert_eq!(
                store.pool_size(),
                0,
                "a dead connection must never stay pooled — the next send must spawn fresh"
            );
        });
    });
}

#[gpui::test]
async fn next_send_after_process_exit_respawns_and_resumes(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    let (gate_tx, gate_rx) = async_channel::bounded::<()>(1);
    let connect_count = std::sync::Arc::new(AtomicUsize::new(0));
    let server = Rc::new(MockAgentServer::configured(
        connect_count.clone(),
        None,
        Some(PromptGate(gate_rx)),
        None,
        true,
    ));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.register_agent_server(agent_id.clone(), server.clone());
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
    let original_acp_session_id = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .acp_session_id
            .clone()
    });
    let first_thread = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .acp_thread()
            .cloned()
            .expect("session is hot")
    });

    // A turn is in flight when the process dies.
    let first_send = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.send_message(session_id, "one".into(), cx))
    });
    cx.executor().run_until_parked();
    {
        let (state, _, _) = session_snapshot(cx, session_id);
        assert!(
            matches!(state, SessionState::Running { .. }),
            "fixture: the first turn is in flight, state: {state:?}"
        );
    }

    cx.update(|cx| emit_process_exit(&first_thread, cx));
    cx.executor().run_until_parked();
    {
        let (state, cold, note) = session_snapshot(cx, session_id);
        assert!(
            matches!(state, SessionState::Idle),
            "a mid-turn process death parks the session at Idle, state: {state:?}"
        );
        assert!(cold);
        assert!(note.is_some(), "the transcript must explain the death");
    }

    // The dead turn's pending prompt future now resolves with an error; that
    // late error must NOT clobber the parked Idle state.
    gate_tx.close();
    cx.executor().run_until_parked();
    {
        let (state, cold, _) = session_snapshot(cx, session_id);
        assert!(
            matches!(state, SessionState::Idle),
            "the dying turn's late error must not flip the session back to Errored, \
             state: {state:?}"
        );
        assert!(cold, "no thread must have been grafted back by the late error");
    }

    // The next send transparently cold-wakes: fresh subprocess (second
    // connect), resume of the SAME ACP session id, and a working turn.
    let (gate2_tx, gate2_rx) = async_channel::bounded::<()>(1);
    server.set_prompt_gate(PromptGate(gate2_rx));
    let second_send = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.send_message(session_id, "two".into(), cx))
    });
    cx.executor().run_until_parked();
    {
        let (state, cold, _) = session_snapshot(cx, session_id);
        assert!(
            matches!(state, SessionState::Running { .. }),
            "the cold-wake re-entered send and started a new turn, state: {state:?}"
        );
        assert!(!cold, "resume must have grafted a live thread back");
        let acp_session_id = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store
                .read(cx)
                .session(session_id)
                .expect("session exists")
                .read(cx)
                .acp_session_id
                .clone()
        });
        assert_eq!(
            acp_session_id, original_acp_session_id,
            "session/resume must restore the SAME provider session (the agent keeps its \
             on-disk context), not mint a new one"
        );
        assert_eq!(
            connect_count.load(Ordering::SeqCst),
            2,
            "the pool must have spawned a fresh connection for the wake"
        );
    }

    gate2_tx.send(()).await.expect("release second prompt");
    second_send.await.expect("second send completes");
    cx.executor().run_until_parked();

    {
        let (state, cold, _) = session_snapshot(cx, session_id);
        assert!(matches!(state, SessionState::Idle), "turn completed: {state:?}");
        assert!(!cold);
    }
    // The second user message is a transcript entry (the send went through
    // the resumed connection, not into the void).
    let sent_two = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.read(cx).entries.iter().any(|entry| {
                matches!(
                    &entry.kind,
                    crate::session_entry::SessionEntryKind::UserMessage { content_md, .. }
                        if content_md.contains("two")
                )
            })
        })
    });
    assert!(sent_two, "the post-recovery message must land in the transcript");

    // The abandoned first turn never left a phantom user bubble unanswered:
    // message "one" sits in the transcript too (pushed optimistically at send
    // time), followed by the process-exit note.
    drop(first_send);
}

/// A process exit arriving at an ALREADY-cold session (a reconnect cold-ize
/// raced the exit event) must not double-release the pool slot or clobber
/// the recovery's transient state — the reconnect machinery owns both.
#[gpui::test]
async fn process_exit_on_already_cold_session_is_a_noop(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");
    let connect_count = std::sync::Arc::new(AtomicUsize::new(0));
    let server = Rc::new(MockAgentServer::configured(
        connect_count.clone(),
        None,
        None,
        None,
        true,
    ));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.register_agent_server(agent_id.clone(), server.clone());
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
    let thread = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .acp_thread()
            .cloned()
            .expect("session is hot")
    });

    // Simulate the reconnect cold-ize racing the exit event: detach first
    // (without the pool bookkeeping the real reconnect does — the invariant
    // under test is that the exit handler leaves BOTH alone when cold).
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, cx| {
                s.set_acp_thread(None, cx);
            });
        });
    });
    cx.update(|cx| emit_process_exit(&thread, cx));
    cx.executor().run_until_parked();

    let (state, cold, note) = session_snapshot(cx, session_id);
    assert!(
        matches!(state, SessionState::Idle),
        "no state churn on an already-cold session, state: {state:?}"
    );
    assert!(cold);
    assert!(
        note.is_none(),
        "no note either — the racing recovery writes its own breadcrumbs, got: {note:?}"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            assert_eq!(
                store.pool_size(),
                1,
                "the exit handler must not release or remove a slot it does not own"
            );
        });
    });
}

