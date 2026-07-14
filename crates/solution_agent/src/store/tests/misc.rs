#![allow(unused_imports)]

use super::common::*;
use crate::adapter::AdapterRegistry;
use crate::model::SessionState;
use crate::store::*;
use crate::test_support::{MockAgentServer, MockConnection};
use chrono::Utc;
use gpui::{Entity, SharedString, TestAppContext};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[gpui::test]
async fn pool_release_arms_60s_shutdown_then_drops(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let key = (SolutionId(1), SharedString::from("mock-agent"));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.pool_pretend_session_added(key.clone(), Rc::new(MockConnection::new()));
            assert_eq!(store.pool_size(), 1);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.pool_release_session(key.clone(), cx);
        });
    });

    cx.executor()
        .advance_clock(std::time::Duration::from_secs(30));
    cx.executor().run_until_parked();
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| assert_eq!(store.pool_size(), 1));
    });

    cx.executor()
        .advance_clock(std::time::Duration::from_secs(35));
    cx.executor().run_until_parked();
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| assert_eq!(store.pool_size(), 0));
    });
}

#[gpui::test]
async fn shutdown_cancels_when_session_re_added(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));
    let key = (SolutionId(1), SharedString::from("mock-agent"));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.pool_pretend_session_added(key.clone(), Rc::new(MockConnection::new()));
            store.pool_release_session(key.clone(), cx);
        });
    });

    cx.executor()
        .advance_clock(std::time::Duration::from_secs(30));
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.pool_pretend_session_added(key.clone(), Rc::new(MockConnection::new()));
        });
    });

    cx.executor()
        .advance_clock(std::time::Duration::from_secs(60));
    cx.executor().run_until_parked();
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| assert_eq!(store.pool_size(), 1));
    });
}

#[gpui::test]
async fn create_session_spawns_subprocess_once_per_pair(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
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

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            assert!(store.session(session_id).is_some());
            assert_eq!(store.pool_size(), 1);
        });
    });
    assert_eq!(connect_count.load(Ordering::SeqCst), 1);
}

#[gpui::test]
async fn parallel_create_session_for_same_pair_spawns_only_once(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    // Gate `connect()` until both create_session calls have observed the
    // pool entry — this guarantees the second call sees `Pending` and
    // doesn't race past into a fresh spawn before the first one inserts.
    let (gate_tx, gate_rx) = async_channel::bounded(1);
    let connect_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::with_gate(connect_count.clone(), gate_rx)),
            );
        });
    });

    let task1 = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.create_session(solution_id, agent_id.clone(), project.clone(), cx)
        })
    });
    let task2 = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.create_session(solution_id, agent_id.clone(), project.clone(), cx)
        })
    });

    // Pump scheduler so both tasks reach the await on `connect_task`.
    cx.executor().run_until_parked();
    // Now release the gate, letting connect() resolve.
    gate_tx.send(()).await.expect("gate send");
    gate_tx.close();

    let id1 = task1.await.expect("task1");
    let id2 = task2.await.expect("task2");
    assert_ne!(id1, id2);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            assert_eq!(store.pool_size(), 1);
            assert!(store.session(id1).is_some());
            assert!(store.session(id2).is_some());
        });
    });
    assert_eq!(connect_count.load(Ordering::SeqCst), 1);
}

/// Like `create_session_with_thread`, but backs the session with a
/// `MockConnection` whose `cancel()` increments the returned counter, so a
/// test can assert how many times the store forwarded a stop to the backend.
async fn create_session_with_cancel_counter(
    cx: &mut TestAppContext,
) -> (SolutionSessionId, Arc<AtomicUsize>, tempfile::TempDir) {
    let (solution_id, tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    let connect_count = Arc::new(AtomicUsize::new(0));
    let cancel_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::with_cancel_count(
                    connect_count.clone(),
                    cancel_count.clone(),
                )),
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

    (session_id, cancel_count, tmp)
}

#[gpui::test]
async fn cancel_turn_sets_stopping_and_is_idempotent(cx: &mut TestAppContext) {
    let (session_id, cancel_calls, _tmp) = create_session_with_cancel_counter(cx).await;

    // Put the session in Running so cancel has an in-flight turn to stop.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        });
    });

    let read_state = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store
                .read(cx)
                .session(session_id)
                .expect("session")
                .read(cx)
                .state
                .clone()
        })
    };

    // First cancel: Running -> Stopping, connection.cancel forwarded once.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.cancel_turn(session_id, cx))
    })
    .expect("cancel_turn");
    assert!(matches!(read_state(cx), SessionState::Stopping { .. }));
    assert_eq!(cancel_calls.load(Ordering::SeqCst), 1);

    // Second cancel while Stopping: no-op, no extra forward.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.cancel_turn(session_id, cx))
    })
    .expect("cancel_turn idempotent");
    assert!(matches!(read_state(cx), SessionState::Stopping { .. }));
    assert_eq!(cancel_calls.load(Ordering::SeqCst), 1);
}

/// Regression for the 2026-05-24 "Stopping forever" bug. The
/// `MockConnection::cancel` is a counter-only no-op (it does NOT fire
/// `AcpThreadEvent::Stopped`), so without the safety net the session
/// would sit in `Stopping` for the rest of the process lifetime. With
/// the net armed in `cancel_turn`, advancing the executor past
/// `STOPPING_SAFETY_NET` must force-flip the state back to `Idle`.
#[gpui::test]
async fn stopping_safety_net_force_flips_to_idle(cx: &mut TestAppContext) {
    let (session_id, cancel_calls, _tmp) = create_session_with_cancel_counter(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.cancel_turn(session_id, cx))
    })
    .expect("cancel_turn");

    // cancel forwarded once → Stopping → safety net armed.
    assert_eq!(cancel_calls.load(Ordering::SeqCst), 1);
    let state_after_cancel = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .state
            .clone()
    });
    assert!(
        matches!(state_after_cancel, SessionState::Stopping { .. }),
        "expected Stopping after cancel, got {state_after_cancel:?}"
    );

    // Net is 40s. Advance past it; the spawned timer fires and
    // mutate_state flips the session back to Idle.
    cx.executor().advance_clock(
        crate::store::queue::STOPPING_SAFETY_NET + std::time::Duration::from_secs(1),
    );
    cx.executor().run_until_parked();

    let state_after_net = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .state
            .clone()
    });
    assert!(
        matches!(state_after_net, SessionState::Idle),
        "expected Idle after safety net, got {state_after_net:?}"
    );
}

/// Regression (the "⚡ переводит в Stopping, потом снова Running" report):
/// `interrupt_and_flush_pending` arms `flush_after_cancel` and relies on the
/// backend's `Stopped(Cancelled)` to deliver the queue. When the backend never
/// emits it (the `cancel` no-prompt-tx race), the safety net force-flips to
/// `Idle` — and must ITSELF consume the one-shot flag and deliver the queued
/// follow-up. Otherwise the message limps in on a much later idle-flush and the
/// stale flag survives to flush a queue a subsequent Stop meant to abandon.
#[gpui::test]
async fn stopping_safety_net_flushes_pending_when_flush_after_cancel(cx: &mut TestAppContext) {
    let (session_id, _cancel_calls, _tmp) = create_session_with_cancel_counter(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        });
    });

    // Send while Running → enqueued (not delivered).
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store
                .send_message(session_id, "прерви и ответь".to_string(), cx)
                .detach();
        });
    });
    let queued = cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .pending_messages
            .len()
    });
    assert_eq!(queued, 1, "send while Running must enqueue");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.interrupt_and_flush_pending(session_id, cx)
        })
    })
    .expect("interrupt_and_flush_pending");

    // Backend never answers with `Stopped` — the net is the only thing that runs.
    cx.executor()
        .advance_clock(crate::store::queue::STOPPING_SAFETY_NET + std::time::Duration::from_secs(1));
    cx.executor().run_until_parked();

    let (pending, flush_flag) = cx.update(|cx| {
        let session = SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .expect("session");
        let s = session.read(cx);
        (s.pending_messages.len(), s.flush_after_cancel)
    });
    assert_eq!(
        pending, 0,
        "safety net must DELIVER the queued follow-up, not leave it parked"
    );
    assert!(
        !flush_flag,
        "flush_after_cancel is one-shot — the net must consume it, else a later \
         genuine Stop would flush an abandoned queue"
    );
}

/// Sibling regression: when the natural `Stopped` chain DOES fire
/// (the happy path), the safety net must NOT later overwrite a
/// legitimate new state. The `mutate_state` cleanup hook drops the
/// task on the `Stopping → Idle` transition; if that cleanup ever
/// regresses, a delayed timer would force a healthy `Running` turn
/// (started after the cancel) back to `Idle` spuriously.
#[gpui::test]
async fn stopping_safety_net_does_not_fire_after_natural_recovery(cx: &mut TestAppContext) {
    let (session_id, _acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.cancel_turn(session_id, cx))
    })
    .expect("cancel_turn");

    // Natural recovery: Stopped event fires (the way claude_native does
    // in the happy path). This must drop the safety-net task via
    // `mutate_state` cleanup.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let thread = session.read(cx).acp_thread().cloned().expect("live thread");
        thread.update(cx, |_thread, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                agent_client_protocol::schema::StopReason::Cancelled,
            ));
        });
    });
    cx.executor().run_until_parked();

    // Now flip back to Running (a follow-up turn the user kicked off)
    // and advance past the safety-net window. If the cleanup hook
    // dropped the task properly, state stays Running. If not, the
    // delayed timer would force it back to Idle.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.mutate_state(
                session_id,
                |state| {
                    *state = SessionState::Running {
                        started_at: std::time::Instant::now(),
                        notified: false,
                    }
                },
                cx,
            );
        });
    });
    cx.executor().advance_clock(
        crate::store::queue::STOPPING_SAFETY_NET + std::time::Duration::from_secs(1),
    );
    cx.executor().run_until_parked();

    let final_state = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .state
            .clone()
    });
    assert!(
        matches!(final_state, SessionState::Running { .. }),
        "safety-net must not fire after natural Stopped recovery; got {final_state:?}"
    );
}

#[gpui::test]
async fn turn_complete_event_transitions_running_to_idle(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        });
    });

    cx.update(|cx| {
        acp_thread.update(cx, |_thread, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                agent_client_protocol::schema::StopReason::EndTurn,
            ));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let state = session.read(cx).state.clone();
            assert!(
                matches!(state, SessionState::Idle),
                "expected Idle, got {:?}",
                state
            );
        });
    });
}

/// Regression: sending a message while a tool call is blocked
/// `WaitingForConfirmation` must NOT leave the turn stuck. The send path
/// declines the pending authorization (reject) to unblock the turn, then
/// the message rides the normal queue/flush. Here we assert the unblock:
/// the tool call leaves `WaitingForConfirmation` (becomes `Rejected`) and
/// the user's message is queued (not dropped) for the next turn.
#[gpui::test]
async fn send_while_waiting_for_confirmation_unblocks_the_turn(cx: &mut TestAppContext) {
    use acp_thread::{AgentThreadEntry, ToolCallStatus};
    use agent_client_protocol::schema as acp;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Stage a tool call sitting in WaitingForConfirmation with a flat
    // allow/reject option pair, and HOLD the returned auth task so the
    // oneshot the turn awaits stays alive (dropping it would itself flip
    // the call off WaitingForConfirmation and defeat the test).
    let _auth_task = cx.update(|cx| {
        acp_thread.update(cx, |thread, cx| {
            let update = acp::ToolCallUpdate::new(
                acp::ToolCallId::new("call-auth-1"),
                acp::ToolCallUpdateFields::new()
                    .kind(acp::ToolKind::Execute)
                    .title("Bash".to_string()),
            );
            let options = acp_thread::PermissionOptions::Flat(vec![
                acp::PermissionOption::new(
                    "opt-allow",
                    "Allow".to_string(),
                    acp::PermissionOptionKind::AllowOnce,
                ),
                acp::PermissionOption::new(
                    "opt-reject",
                    "Reject".to_string(),
                    acp::PermissionOptionKind::RejectOnce,
                ),
            ]);
            thread
                .request_tool_call_authorization(
                    update,
                    options,
                    acp_thread::AuthorizationKind::PermissionGrant,
                    cx,
                )
                .expect("stage waiting-for-confirmation")
        })
    });
    cx.executor().run_until_parked();

    // The turn is blocked → session is Running while it waits.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        });
    });

    // User types a new message instead of clicking a button.
    let send_task = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.send_message_blocks(
                session_id,
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "never mind, do this instead".to_string(),
                ))],
                cx,
            )
        })
    });
    send_task.await.expect("send_message_blocks");
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            // The pending authorization must be resolved: the tool call has
            // left WaitingForConfirmation (rejected), so the turn is no
            // longer blocked.
            let status_is_waiting = acp_thread.read(cx).entries().iter().any(|entry| {
                matches!(
                    entry,
                    AgentThreadEntry::ToolCall(call)
                        if matches!(call.status, ToolCallStatus::WaitingForConfirmation { .. })
                )
            });
            assert!(
                !status_is_waiting,
                "tool call should no longer be WaitingForConfirmation after the send"
            );
            let rejected = acp_thread.read(cx).entries().iter().any(|entry| {
                matches!(
                    entry,
                    AgentThreadEntry::ToolCall(call)
                        if matches!(call.status, ToolCallStatus::Rejected)
                )
            });
            assert!(rejected, "tool call should be Rejected (declined to unblock)");

            // The user's message must not be lost — it's queued for the
            // flush-on-Stopped that the now-unblocked turn will trigger.
            let session = store.session(session_id).expect("session exists");
            assert_eq!(
                session.read(cx).pending_messages.len(),
                1,
                "the user's message must be queued for delivery, not dropped"
            );
            // Fix 2 wired: the resolve branch set the one-shot
            // `flush_after_cancel` so the queue survives even if the agent
            // treats the rejection as a Cancelled stop rather than EndTurn.
            assert!(
                session.read(cx).flush_after_cancel,
                "flush_after_cancel must be set so a Cancelled-stop rejection still flushes the queue"
            );
        });
    });

    // Drive the now-unblocked turn to completion. On `Stopped(EndTurn)` the
    // store's queue handler drains `pending_messages` and re-sends them as
    // the next turn — end-to-end delivery, not just enqueue.
    cx.update(|cx| {
        acp_thread.update(cx, |_thread, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                agent_client_protocol::schema::StopReason::EndTurn,
            ));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            // The queued message was actually delivered: the flush drained
            // it (and re-sent it as the next turn), so the queue is empty.
            assert!(
                session.read(cx).pending_messages.is_empty(),
                "the queued message must be delivered (queue drained) after the turn stops, not dropped"
            );
        });
    });
}

#[gpui::test]
async fn error_event_transitions_to_errored_state(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        acp_thread.update(cx, |_thread, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Error);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let state = session.read(cx).state.clone();
            assert!(
                matches!(state, SessionState::Errored(_)),
                "expected Errored, got {:?}",
                state
            );
        });
    });
}

/// Bug #5: once `Error` latched the session to `Errored`, genuine agent
/// activity (a new entry, or a streaming in-place update) must clear it back
/// to `Running` so the status row stops showing red "Error: …" while text
/// streams. An editor-injected SystemNote must NOT clear it (it isn't agent
/// activity).
#[gpui::test]
async fn streaming_activity_clears_latched_errored_state(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let check = move |cx: &mut TestAppContext, want_running: bool, ctx: &str| {
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                let state = store
                    .session(session_id)
                    .expect("session exists")
                    .read(cx)
                    .state
                    .clone();
                if want_running {
                    assert!(
                        matches!(state, SessionState::Running { .. }),
                        "{ctx}: expected Running, got {state:?}"
                    );
                } else {
                    assert!(
                        matches!(state, SessionState::Errored(_)),
                        "{ctx}: expected Errored, got {state:?}"
                    );
                }
            });
        });
    };

    // 1. An agent error latches the session red.
    cx.update(|cx| acp_thread.update(cx, |_t, cx| cx.emit(acp_thread::AcpThreadEvent::Error)));
    cx.executor().run_until_parked();
    check(cx, false, "after Error");

    // 2. An Observer breadcrumb (SystemNote) is editor-originated, not agent
    //    activity — it must NOT clear the error.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_system_note(
                acp_thread::SystemNoteLevel::Observer,
                "Наблюдатель направил агента",
                cx,
            );
        })
    });
    cx.executor().run_until_parked();
    check(cx, false, "after SystemNote");

    // 3. A genuine new assistant entry (NewEntry) means the agent recovered —
    //    clears Errored -> Running.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("recovering".to_string()),
                ),
                false,
                cx,
            );
        })
    });
    cx.executor().run_until_parked();
    check(cx, true, "after recovered NewEntry");

    // 4. Re-latch Errored, then an in-place streaming EntryUpdated on the same
    //    assistant entry must also clear it (the visible "red while streaming"
    //    symptom is EntryUpdated-driven).
    cx.update(|cx| acp_thread.update(cx, |_t, cx| cx.emit(acp_thread::AcpThreadEvent::Error)));
    cx.executor().run_until_parked();
    check(cx, false, "after second Error");
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(" more".to_string()),
                ),
                false,
                cx,
            );
        })
    });
    cx.executor().run_until_parked();
    check(cx, true, "after streaming EntryUpdated");
}

#[gpui::test]
async fn tool_authorization_request_transitions_to_awaiting_input(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        acp_thread.update(cx, |_thread, cx| {
            cx.emit(acp_thread::AcpThreadEvent::ToolAuthorizationRequested(
                agent_client_protocol::schema::ToolCallId::new("test-tool"),
            ));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let state = session.read(cx).state.clone();
            assert!(
                matches!(state, SessionState::AwaitingInput),
                "expected AwaitingInput, got {:?}",
                state
            );
        });
    });
}

#[gpui::test]
async fn send_message_starts_running_state_immediately(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    // Use a prompt-gated MockConnection so prompt() stays pending until we
    // release the gate — this lets us observe the synchronous Running flip
    // before the underlying ACP turn completes.
    let (prompt_gate_tx, prompt_gate_rx) = async_channel::bounded::<()>(1);
    let connect_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::with_prompt_gate(
                    connect_count.clone(),
                    prompt_gate_rx,
                )),
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

    // Force `Idle` so we can prove `send_message` flips it to `Running`
    // synchronously rather than just observing pre-existing `Running`.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| s.state = SessionState::Idle);
        });
    });

    // Kick off the prompt. We deliberately don't await `task` here — we
    // want to read the state BEFORE the prompt resolves.
    let task = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.send_message(session_id, "hi".into(), cx)
        })
    });

    // Synchronous post-condition: state is already Running.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let state = session.read(cx).state.clone();
            assert!(
                matches!(state, SessionState::Running { .. }),
                "expected Running synchronously after send_message, got {:?}",
                state
            );
        });
    });

    // Now release the prompt gate so the spawned future resolves.
    prompt_gate_tx.send(()).await.expect("release prompt gate");
    prompt_gate_tx.close();
    task.await.expect("send_message task");
}

#[gpui::test]
async fn queued_messages_get_per_message_timestamp_prefix(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    // Force `Running` so send_message_blocks takes the queueing branch.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
                agent_client_protocol::schema::TextContent::new("first thought".to_string()),
            )];
            store
                .send_message_blocks(session_id, blocks, cx)
                .detach_and_log_err(cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            assert_eq!(s.pending_messages.len(), 1, "one queued bundle after first enqueue");
            let bundle = &s.pending_messages[0];
            let first = match &bundle.blocks[0] {
                agent_client_protocol::schema::ContentBlock::Text(t) => t.text.as_str(),
                other => panic!("first block must be Text, got {other:?}"),
            };
            assert!(first.starts_with('['), "first block starts with timestamp prefix, got {first:?}");
            let inner = first.strip_prefix('[').and_then(|s| s.split_once("] ")).map(|(ts, _)| ts);
            assert!(
                matches!(inner, Some(ts) if ts.len() == 8 && ts.as_bytes()[2] == b':' && ts.as_bytes()[5] == b':'),
                "expected [HH:MM:SS] prefix, got {first:?}"
            );
            let payload: String = bundle
                .blocks
                .iter()
                .filter_map(|b| match b {
                    agent_client_protocol::schema::ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            assert!(
                payload.contains("first thought"),
                "user content preserved after prefix, got {payload:?}"
            );
            assert!(
                !payload.contains("queued in advance"),
                "old verbose marker must not appear, got {payload:?}"
            );
        });
    });

    // Second enqueue while still Running — each follow-up gets its own timestamp prefix.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
                agent_client_protocol::schema::TextContent::new("follow-up".to_string()),
            )];
            store
                .send_message_blocks(session_id, blocks, cx)
                .detach_and_log_err(cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            assert_eq!(s.pending_messages.len(), 1, "still one bundle after second enqueue");
            let bundle = &s.pending_messages[0];
            let payload: String = bundle
                .blocks
                .iter()
                .filter_map(|b| match b {
                    agent_client_protocol::schema::ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            assert!(
                payload.contains("first thought") && payload.contains("follow-up"),
                "both messages preserved, got {payload:?}"
            );
            let stamp_count = bundle
                .blocks
                .iter()
                .filter(|b| matches!(b,
                    agent_client_protocol::schema::ContentBlock::Text(t)
                        if t.text.starts_with('[')
                            && t.text.strip_prefix('[')
                                .and_then(|s| s.split_once("] "))
                                .map(|(ts, _)| ts.len() == 8 && ts.as_bytes()[2] == b':' && ts.as_bytes()[5] == b':')
                                .unwrap_or(false)
                ))
                .count();
            assert_eq!(stamp_count, 2, "each follow-up gets its own timestamp prefix");
        });
    });
}

#[gpui::test]
async fn take_pending_for_delivery_drains_pushes_and_formats(cx: &mut TestAppContext) {
    let (session_id, thread, _tmp) = create_session_with_thread(cx).await;
    let entries_before = cx.update(|cx| thread.read(cx).entries().len());

    // Force Running so send_message enqueues, then queue one follow-up.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message(session_id, "hello".to_string(), cx)
                .detach_and_log_err(cx);
        });
    });

    // Mid-work delivery: text has the timestamp, NO hint; queue drained; one timeline entry pushed.
    let mid = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.take_pending_for_delivery(session_id, None, false, cx)
        })
    });
    let text = mid.expect("pending present");
    assert!(
        text.contains("hello"),
        "delivers user content, got {text:?}"
    );
    assert!(
        !text.contains(crate::store::queue::QUEUE_HINT_LINE),
        "no hint mid-work, got {text:?}"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                0,
                "queue drained"
            );
        });
    });
    assert_eq!(
        cx.update(|cx| thread.read(cx).entries().len()),
        entries_before + 1,
        "one user entry pushed"
    );

    // End-of-turn delivery carries the hint.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message(session_id, "again".to_string(), cx)
                .detach_and_log_err(cx);
        });
    });
    let end = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.take_pending_for_delivery(session_id, None, true, cx)
            })
        })
        .expect("pending present");
    assert!(
        end.contains(crate::store::queue::QUEUE_HINT_LINE),
        "Stop/idle delivery carries the hint, got {end:?}"
    );
}

/// Pure contract tests for the routing predicates — cheap, no GPUI needed.
#[test]
fn queue_target_matches_hook_routes_by_agent_id() {
    use crate::model::QueueTarget;
    // Main bundles drain on the main agent's hook (no agent_id), never on a
    // teammate's hook.
    assert!(QueueTarget::Main.matches_hook(None));
    assert!(!QueueTarget::Main.matches_hook(Some("agent-1")));
    // Subagent bundles drain ONLY on their own teammate's hook.
    let sub = QueueTarget::Subagent(SharedString::from("agent-1"));
    assert!(sub.matches_hook(Some("agent-1")));
    assert!(!sub.matches_hook(Some("agent-2")));
    assert!(!sub.matches_hook(None));
}

/// A `Subagent`-targeted follow-up drains only on the matching teammate's
/// hook; a non-matching teammate hook and the main agent's hook leave it
/// queued, and the main agent's own bundle is independent.
#[gpui::test]
async fn take_pending_routes_by_target(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    // Force Running, then queue one Main-targeted and one Subagent-targeted
    // follow-up (distinct addressees ⇒ two bundles, not a merge).
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message(session_id, "for main".to_string(), cx)
                .detach_and_log_err(cx);
            store
                .send_message_blocks_targeted(
                    session_id,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "for teammate".to_string(),
                    ))],
                    crate::model::QueueTarget::Subagent(SharedString::from("agent-1")),
                    true,
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                2,
                "distinct targets must NOT merge into one bundle"
            );
        });
    });

    // A non-matching teammate hook drains nothing and leaves both bundles.
    let miss = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.take_pending_for_delivery(session_id, Some("agent-2"), false, cx)
        })
    });
    assert!(miss.is_none(), "non-matching teammate hook drains nothing");

    // The matching teammate hook drains ONLY its own bundle (the Main bundle
    // stays queued).
    let sub = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.take_pending_for_delivery(session_id, Some("agent-1"), false, cx)
            })
        })
        .expect("teammate bundle delivered");
    assert!(sub.contains("for teammate"), "got {sub:?}");
    assert!(
        !sub.contains("for main"),
        "must not leak the Main bundle, got {sub:?}"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                1,
                "Main bundle stays queued after the teammate drains its own"
            );
        });
    });

    // The main agent's hook now drains the remaining Main bundle.
    let main = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.take_pending_for_delivery(session_id, None, false, cx)
            })
        })
        .expect("main bundle delivered");
    assert!(main.contains("for main"), "got {main:?}");
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                0,
                "queue empty after both addressees drained"
            );
        });
    });
}

/// A `Subagent`-targeted follow-up enqueued while running, then never drained
/// by its teammate (the teammate finishes), is DROPPED — not delivered — when
/// the parent turn ends. We can't easily fire the real `Stopped` event in a
/// unit test, so assert the inverse half of the contract directly: the main
/// agent's hook must never drain a subagent-targeted bundle.
#[gpui::test]
async fn main_hook_never_drains_subagent_bundle(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message_blocks_targeted(
                    session_id,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "orphan".to_string(),
                    ))],
                    crate::model::QueueTarget::Subagent(SharedString::from("ghost")),
                    true,
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });
    // Main agent's hook fires — must NOT swallow the teammate's bundle.
    let pulled = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.take_pending_for_delivery(session_id, None, true, cx)
        })
    });
    assert!(
        pulled.is_none(),
        "main hook must not drain a subagent-targeted bundle, got {pulled:?}"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                1,
                "subagent bundle stays queued for its own (now-gone) addressee"
            );
        });
    });
}

/// A queued bundle carrying an IMAGE must not be delivered via the mid-turn
/// hook (its `additionalContext` is text-only — the image bytes would be lost
/// and the agent would never see the screenshot). It stays queued for the
/// `Stopped` idle-flush, which re-sends the full content blocks.
#[gpui::test]
async fn take_pending_holds_image_bundle_for_idle_flush(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message_blocks(
                    session_id,
                    vec![
                        acp::ContentBlock::Text(acp::TextContent::new(
                            "look at [image #1]".to_string(),
                        )),
                        acp::ContentBlock::Image(acp::ImageContent::new(
                            "ZGF0YQ==".to_string(),
                            "image/png".to_string(),
                        )),
                    ],
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });
    // The main hook fires (even at end-of-turn) — the image bundle is NOT
    // drained text-only; it stays queued for the idle-flush.
    let pulled = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.take_pending_for_delivery(session_id, None, true, cx)
        })
    });
    assert!(
        pulled.is_none(),
        "image bundle must not be delivered via the text-only hook, got {pulled:?}"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                1,
                "image bundle stays queued for the idle-flush (full-content re-send)"
            );
        });
    });
}

/// MID-TURN (a `PostToolUse` hook, `is_end_of_turn = false`), a queued bundle
/// carrying an image IS delivered: the bytes are written to an inbox file and
/// the agent-facing text points at that path with a `Read`-tool instruction,
/// instead of being deferred to the next turn end. This is the fix for
/// "image follow-ups don't reach the agent until the (long) turn finishes".
#[gpui::test]
async fn take_pending_delivers_image_as_readable_path_mid_turn(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message_blocks(
                    session_id,
                    vec![
                        acp::ContentBlock::Text(acp::TextContent::new("look at this".to_string())),
                        // base64 "ZGF0YQ==" decodes to the bytes b"data".
                        acp::ContentBlock::Image(acp::ImageContent::new(
                            "ZGF0YQ==".to_string(),
                            "image/png".to_string(),
                        )),
                    ],
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });

    // Mid-turn hook (is_end_of_turn = false) — the image bundle is delivered.
    let pulled = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.take_pending_for_delivery(session_id, None, false, cx)
        })
    });
    let text = pulled.expect("image bundle must be delivered mid-turn as a path reference");
    assert!(
        text.contains("Use the Read tool"),
        "delivered text must instruct the agent to Read the saved image, got: {text:?}"
    );
    assert!(
        text.contains("look at this"),
        "delivered text must keep the user's words, got: {text:?}"
    );

    // The referenced file must exist on disk and hold the decoded bytes.
    let path_str = text
        .split("saved to ")
        .nth(1)
        .and_then(|rest| rest.split(". Use the Read tool").next())
        .expect("delivered text must carry the saved path")
        .to_string();
    let path = std::path::PathBuf::from(&path_str);
    assert!(
        path.extension().is_some_and(|e| e == "png"),
        "path {path:?} keeps the png extension"
    );
    let written = std::fs::read(&path).expect("inbox image file must exist");
    assert_eq!(
        written, b"data",
        "inbox file must hold the decoded image bytes"
    );
    let _ = std::fs::remove_file(&path);

    // Queue is drained — nothing left for the idle-flush to re-send.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .is_empty(),
                "image bundle must be drained after mid-turn delivery"
            );
        });
    });
}

#[gpui::test]
async fn reset_context_swaps_acp_thread_without_bumping_count(cx: &mut TestAppContext) {
    let (session_id, old_thread, _tmp) = create_session_with_thread(cx).await;

    // Snapshot pre-reset state. Bump context_count to 7 so we can prove the
    // reset path does NOT touch it (rotate_context, by contrast, would
    // increment to 8). Also stamp a fake usage onto the old thread so a
    // later "no usage on the new thread" assertion has signal.
    let (old_acp_session_id, old_thread_id) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| s.context_count = 7);
            let s = session.read(cx);
            (s.acp_session_id.clone(), old_thread.entity_id())
        })
    });

    let result = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| store.reset_context(session_id, cx))
        })
        .await
        .expect("reset_context");
    assert_eq!(
        result, session_id,
        "reset_context returns the same SolutionSessionId"
    );

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            let new_thread = s.acp_thread().cloned().expect("new acp_thread populated");
            assert_ne!(
                new_thread.entity_id(),
                old_thread_id,
                "reset_context swapped the AcpThread entity"
            );
            assert_ne!(
                s.acp_session_id, old_acp_session_id,
                "new acp_session_id differs from the old one"
            );
            assert_eq!(
                s.context_count, 7,
                "context_count unchanged (rotate_context would have bumped to 8)"
            );
            assert!(
                matches!(s.state, SessionState::Idle),
                "state is Idle, got {:?}",
                s.state
            );
            assert!(
                new_thread.read(cx).entries().is_empty(),
                "new thread has no entries"
            );
        });
    });
}

#[gpui::test]
async fn late_send_error_is_dropped_when_session_was_reset(cx: &mut TestAppContext) {
    // Race regression guard: `/clear` (reset_context) swapping the
    // AcpThread mid-turn must not let the OLD turn's late `Err`
    // clobber the freshly-Idle state with `Errored("...")`. Without
    // the `expected_acp_session_id` check in `send_message_blocks`,
    // this test fails — the dropped gate makes the mock prompt return
    // Err, which the spawn's Err branch unconditionally writes as
    // `SessionState::Errored(...)`.
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    let (prompt_gate_tx, prompt_gate_rx) = async_channel::bounded::<()>(1);
    let connect_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::with_prompt_gate(
                    connect_count.clone(),
                    prompt_gate_rx,
                )),
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

    // Send a message; the mock will park on the gate.
    let send_task = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.send_message(session_id, "hi".into(), cx)
        })
    });

    // Reset the session while the in-flight prompt is still parked. The
    // new ACP thread takes over; state should land on `Idle`.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            assert!(
                matches!(session.read(cx).state, SessionState::Idle),
                "post-reset state is Idle, got {:?}",
                session.read(cx).state
            );
        });
    });

    // Now release the OLD turn with an error (drop the sender without
    // sending) — without the rotation-race guard, this clobbers Idle
    // with `Errored`.
    prompt_gate_tx.close();
    drop(prompt_gate_tx);
    // The spawned send_task should now resolve to Err. We don't care
    // about its return value; we only care that the side-effect on
    // SessionState was suppressed because the acp_session_id changed.
    let _ = send_task.await;
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            assert!(
                matches!(session.read(cx).state, SessionState::Idle),
                "late Err on rotated session must NOT clobber Idle, got {:?}",
                session.read(cx).state
            );
        });
    });
}

/// `EntriesRemoved` covers thread-local truncation; the `cleared` arm
/// fires when `entries()` is empty after the event (the only in-tree
/// producer is rewind-to-zero from refusal-truncation). This test pins
/// that the live thread's `token_usage` and the session's
/// `cached_total_tokens`/`last_turn_duration` mirrors all reset on the
/// rewind-to-zero path; the partial-rewind sibling
/// (`entries_removed_partial_rewind_preserves_token_state`) pins the
/// negative case. The user-facing `/clear` flow is covered by
/// `reset_context_resets_token_meter`.
#[gpui::test]
async fn entries_removed_to_zero_resets_token_state(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Stamp pre-clear state on both the live thread (token_usage) and
    // the session (cached_total_tokens, last_turn_duration). All three
    // must be cleared on full /clear.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.update_token_usage(
                Some(acp_thread::TokenUsage {
                    used_tokens: 12_345,
                    max_tokens: 1_000_000,
                    ..Default::default()
                }),
                cx,
            );
        });
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.cached_total_tokens = Some(12_345);
                s.last_turn_duration = Some(std::time::Duration::from_secs(7));
            });
        });
    });

    // Emit EntriesRemoved. Range payload is informational; the handler
    // discriminates full clear vs partial rewind by checking
    // `entries().is_empty()` post-event. The mock thread starts empty
    // and we never appended, so this exercises the cleared-arm.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntriesRemoved(0..0));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            assert!(
                s.cached_total_tokens.is_none(),
                "cached_total_tokens reset, got {:?}",
                s.cached_total_tokens
            );
            assert!(
                s.last_turn_duration.is_none(),
                "last_turn_duration reset, got {:?}",
                s.last_turn_duration
            );
        });
        let usage = acp_thread.read(cx).token_usage().cloned();
        assert!(
            usage.is_none(),
            "live thread token_usage reset, got {usage:?}"
        );
    });
}

/// Sibling of `entries_removed_full_clear_resets_token_state`: when
/// `EntriesRemoved` fires but the live thread still has surviving
/// entries (a `rewind` to a specific user message rather than a full
/// `/clear`), the agent will emit a fresh `TokenUsageUpdated` reflecting
/// the surviving prefix's usage — so we must NOT preemptively wipe
/// token state. This pins the partial-rewind branch; the existence of
/// this test plus the full-clear sibling means a future "always reset
/// on EntriesRemoved" mutation breaks one of them.
#[gpui::test]
async fn entries_removed_partial_rewind_preserves_token_state(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Seed the live thread with a surviving user entry so the handler's
    // `entries().is_empty()` discriminator returns false.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                "survivor".into(),
                cx,
            );
            t.update_token_usage(
                Some(acp_thread::TokenUsage {
                    used_tokens: 9_999,
                    max_tokens: 1_000_000,
                    ..Default::default()
                }),
                cx,
            );
        });
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.cached_total_tokens = Some(9_999);
                s.last_turn_duration = Some(std::time::Duration::from_secs(11));
            });
        });
    });

    // Emit EntriesRemoved over an arbitrary range — what discriminates
    // partial-rewind from full-clear is `entries().is_empty()` on the
    // live thread, not the event payload. With one surviving entry,
    // the cleared arm must be skipped.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntriesRemoved(0..1));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            assert_eq!(
                s.cached_total_tokens,
                Some(9_999),
                "cached_total_tokens preserved on partial rewind",
            );
            assert_eq!(
                s.last_turn_duration,
                Some(std::time::Duration::from_secs(11)),
                "last_turn_duration preserved on partial rewind",
            );
        });
        let usage = acp_thread.read(cx).token_usage().cloned();
        assert!(
            usage.is_some_and(|u| u.used_tokens == 9_999),
            "live thread token_usage preserved on partial rewind",
        );
    });
}

/// Phase 4b regression: a rewind that splits a coalesced same-source assistant
/// group must re-stamp the surviving boundary entry so the per-stream delta
/// re-delivers it. Two consecutive parent `AssistantMessage`s coalesce into ONE
/// stream entry (keeping the FIRST fragment's mod_seq); removing the later
/// fragment shrinks that entry's content but leaves `total_count` unchanged, so
/// without the re-stamp a delta client caught up past the coalesced seq would
/// silently render stale text. The `EntriesRemoved` handler bumps the survivor's
/// mod_seq so the stream watermark rises above every issued cursor.
#[gpui::test]
async fn entries_removed_restamps_survivor_on_coalesce_split(cx: &mut TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let asst = |n: u64, text: &str| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: None,
        kind: SessionEntryKind::AssistantMessage {
            chunks: vec![AssistantChunk::Message(text.into())],
        },
    };
    let cursor_before = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.live_base = 0;
                s.entries = vec![
                    SessionEntry {
                        created_ms: 1,
                        mod_seq: 1,
                        subagent_id: None,
                        kind: SessionEntryKind::UserMessage {
                            id: None,
                            content_md: "u".into(),
                            chunks: vec![],
                        },
                    },
                    asst(7, "a1 "),
                    asst(8, "a2"),
                ];
                s.change_seq = 8;
                s.rebuild_streams();
            });
            let s = session.read(cx);
            let main = &s.streams[&crate::stream::StreamId::Main];
            assert_eq!(main.entries.len(), 2, "user + coalesced(a1+a2)");
            main.seq
        })
    });
    assert_eq!(cursor_before, 8, "coalesced entry carries the first fragment's seq");

    // Remove only the last fragment (a2) at global index 2 → splits the group.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntriesRemoved(2..3));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        let main = &s.streams[&crate::stream::StreamId::Main];
        assert_eq!(main.entries.len(), 2, "user + a1 (a2 removed)");
        assert!(
            main.seq > cursor_before,
            "survivor re-stamped so the delta re-delivers the shrunk entry (seq {} !> {cursor_before})",
            main.seq,
        );
    });
}

/// Phase 6b keystone: after the #3 revert `AcpThread` (and thus the flat
/// `session.entries` ingest mirror) may be TORN — a teammate chunk interleaved
/// between two parent deltas splits the parent into two flat entries. But the
/// PERSIST authority is now `streams[StreamId::Main]` (the coalesced demux), so
/// the demux re-unites the parent into ONE Main bubble and `persist_main_stream`
/// writes Main-LOCAL coalesced rows (subagent_id None), never the torn fragments.
/// This pins the whole coupling: torn flat entries → one Main bubble → coalesced
/// persisted rows → watermark advanced.
#[gpui::test]
async fn interleaved_flat_entries_persist_as_coalesced_main_rows(cx: &mut TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let asst = |n: u64, subagent: Option<&str>, text: &str| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: subagent.map(SharedString::from),
        kind: SessionEntryKind::AssistantMessage {
            chunks: vec![AssistantChunk::Message(text.into())],
        },
    };

    // Torn interleave: parent "Three ", teammate "noise", parent "scouts". In the
    // flat ingest buffer the parent is SPLIT across indices 0 and 2 (index 1 is
    // the teammate). The demux groups by source, so Main = coalesce(0, 2).
    let (main_len, main_seq) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, cx| {
                s.live_base = 0;
                s.change_seq = 3;
                s.set_entries(
                    vec![
                        asst(1, None, "Three "),
                        asst(2, Some("T1"), "noise"),
                        asst(3, None, "scouts"),
                    ],
                    cx,
                );
            });
            let s = session.read(cx);
            // Flat entries are TORN: three entries, the parent split at 0 and 2.
            assert_eq!(s.entries.len(), 3, "flat ingest mirror stays torn");
            assert_eq!(s.entries[0].subagent_id, None);
            assert_eq!(
                s.entries[1].subagent_id,
                Some(SharedString::from("T1")),
                "teammate chunk interleaves between the parent's two deltas"
            );
            assert_eq!(s.entries[2].subagent_id, None);
            // But the demux re-unites the parent into ONE Main bubble.
            let main = &s.streams[&crate::stream::StreamId::Main];
            assert_eq!(
                main.entries.len(),
                1,
                "the parent's two torn fragments coalesce into one Main bubble"
            );
            assert_eq!(main.entries[0].subagent_id, None);
            assert!(
                s.streams
                    .contains_key(&crate::stream::StreamId::Teammate("T1".into())),
                "the teammate gets its own stream, out of Main"
            );
            (main.entries.len(), main.seq)
        })
    });

    // Persist the Main stream and let the detached task land.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.persist_main_stream(session_id, cx);
        });
    });
    cx.executor().run_until_parked();

    // The persist watermark advanced to the Main stream's seq.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        assert_eq!(
            session.read(cx).persisted_main_seq,
            main_seq,
            "persisted_main_seq advanced to the Main stream watermark"
        );
    });

    // Persisted rows are the Main-LOCAL coalesced entries: exactly one row, no
    // teammate fragment, subagent_id None, and losslessly carrying both parent
    // deltas.
    let rows = db.load_entries(session_id).await.expect("load rows");
    assert_eq!(
        rows.len(),
        main_len,
        "row count == streams[Main].entries.len() (coalesced, no teammate row)"
    );
    assert_eq!(rows[0].idx, 0, "Main-local index");
    assert_eq!(
        rows[0].subagent_id, None,
        "persisted Main rows carry no subagent tag"
    );
    let kind = crate::session_entry::kind_from_payload(&rows[0].payload)
        .expect("row payload decodes");
    let crate::session_entry::SessionEntryKind::AssistantMessage { chunks } = kind else {
        panic!("expected an AssistantMessage row");
    };
    let text: String = chunks
        .iter()
        .filter_map(|c| match c {
            AssistantChunk::Message(m) => Some(m.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("Three ") && text.contains("scouts"),
        "coalesced Main row must preserve both parent deltas, got: {text:?}"
    );
    assert!(
        !text.contains("noise"),
        "the teammate fragment must NOT leak into the persisted Main row"
    );
}

/// User-facing `/clear` is intercepted client-side and routed through
/// `reset_context`, which spawns a brand-new `AcpThread` (the old one
/// is dropped without emitting any events). Without an explicit reset
/// at the swap site, `cached_total_tokens` / `last_turn_duration` on
/// the session entity persist across the swap and the status-row meter
/// keeps reading the pre-clear count (because the meter falls back to
/// `cached_total_tokens` when the live thread has no `token_usage`,
/// which it doesn't on a fresh thread). This test pins the reset at
/// the swap site — the actual user-visible bug.
#[gpui::test]
async fn reset_context_resets_token_meter(cx: &mut TestAppContext) {
    let (session_id, _old_thread, _tmp) = create_session_with_thread(cx).await;

    // Stamp pre-clear cached values on the session.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.cached_total_tokens = Some(33_333);
                s.last_turn_duration = Some(std::time::Duration::from_secs(13));
            });
        });
    });

    // Drive the actual `/clear` flow.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            assert!(
                s.cached_total_tokens.is_none(),
                "cached_total_tokens reset, got {:?}",
                s.cached_total_tokens
            );
            assert!(
                s.last_turn_duration.is_none(),
                "last_turn_duration reset, got {:?}",
                s.last_turn_duration
            );
            // Sanity: live thread is fresh and has no usage.
            let new_thread = s.acp_thread().cloned().expect("new thread populated");
            assert!(
                new_thread.read(cx).token_usage().is_none(),
                "fresh thread has no token_usage"
            );
        });
    });
}

/// Same invariant for `/compact` (rotate_context) — same swap pattern,
/// same risk of stale meter.
#[gpui::test]
async fn rotate_context_resets_token_meter(cx: &mut TestAppContext) {
    let (session_id, _old_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.cached_total_tokens = Some(44_444);
                s.last_turn_duration = Some(std::time::Duration::from_secs(17));
            });
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.rotate_context(session_id, cx))
    })
    .await
    .expect("rotate_context");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            assert!(
                s.cached_total_tokens.is_none(),
                "cached_total_tokens reset on rotate, got {:?}",
                s.cached_total_tokens
            );
            assert!(
                s.last_turn_duration.is_none(),
                "last_turn_duration reset on rotate, got {:?}",
                s.last_turn_duration
            );
        });
    });
}

/// `/clear` (reset_context) must emit `SessionContextReset` so remote
/// clients (mobile, WS proxy) learn the transcript was wiped — they
/// only get `SessionStateChanged` otherwise, which doesn't carry
/// "context gone" semantics, so their cached entry list goes stale
/// until a foreground/cold refresh.
#[gpui::test]
async fn reset_context_emits_session_context_reset(cx: &mut TestAppContext) {
    let (session_id, _old_thread, _tmp) = create_session_with_thread(cx).await;

    // Stamp a known context_count so the assertion below can verify it
    // is forwarded as-is (reset does NOT bump the counter).
    let stamped_count: crate::model::SessionContextCount = 4;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| s.context_count = stamped_count);
            cx.notify();
        });
    });

    let observed = Rc::new(std::cell::RefCell::new(Vec::<
        crate::model::SessionContextCount,
    >::new()));
    let _subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let observed = observed.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionContextReset { id, context_count } = event
                && *id == session_id
            {
                observed.borrow_mut().push(*context_count);
            }
        })
    });

    // Phase 5 Task 5.3 Part C: confirm /clear bumps the transcript epoch.
    // The `agent_session_context_reset` push (forwarded from this very
    // SessionContextReset event by `event_sources`) is what tells the
    // cache-first mobile client to full-reload — it must coincide with an
    // epoch bump so the client's `(epoch, current_seq)` cursor invalidates.
    let epoch_before = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        session.read(cx).epoch
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");
    cx.executor().run_until_parked();

    let collected = observed.borrow().clone();
    assert_eq!(
        collected,
        vec![stamped_count],
        "/clear must fire exactly one SessionContextReset with the unchanged context_count",
    );

    let epoch_after = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        session.read(cx).epoch
    });
    assert_eq!(
        epoch_after,
        epoch_before + 1,
        "/clear must bump the session epoch so the mobile delta cursor invalidates",
    );
}

/// `/compact` (rotate_context) must emit `SessionContextReset` and the
/// `context_count` carried on the event must be the POST-rotation
/// value (previous + 1) — clients use it to render the "now on
/// rotation #N" badge without a follow-up `get_session` round-trip.
#[gpui::test]
async fn rotate_context_emits_session_context_reset_with_incremented_count(
    cx: &mut TestAppContext,
) {
    let (session_id, _old_thread, _tmp) = create_session_with_thread(cx).await;

    let initial_count = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.read(cx).context_count
        })
    });

    let observed = Rc::new(std::cell::RefCell::new(Vec::<
        crate::model::SessionContextCount,
    >::new()));
    let _subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let observed = observed.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionContextReset { id, context_count } = event
                && *id == session_id
            {
                observed.borrow_mut().push(*context_count);
            }
        })
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.rotate_context(session_id, cx))
    })
    .await
    .expect("rotate_context");
    cx.executor().run_until_parked();

    let collected = observed.borrow().clone();
    assert_eq!(
        collected,
        vec![initial_count.saturating_add(1)],
        "/compact must fire exactly one SessionContextReset with the incremented context_count",
    );
}

/// Regression: `/compact` (rotate_context) must preserve the session's
/// `cwd`. Earlier the new ACP thread was always spawned with
/// `work_dirs = solution.root` regardless of which member sub-dir the
/// tab was bound to, so the agent's bash tool silently switched from
/// e.g. `voxelcraft/` to the solution root after the first compact and
/// then failed any command depending on `Cargo.toml` / `.git`.
#[gpui::test]
async fn rotate_context_preserves_session_cwd(cx: &mut TestAppContext) {
    let (session_id, _old_thread, _tmp) = create_session_with_thread(cx).await;

    // Stamp a non-root cwd onto the session — simulating "tab was
    // opened against the `member-x` subdir".
    let member_cwd = std::path::PathBuf::from("/tmp/sol-x/member-x");
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| s.cwd = member_cwd.clone());
            cx.notify();
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.rotate_context(session_id, cx))
    })
    .await
    .expect("rotate_context");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let new_thread = session
                .read(cx)
                .acp_thread()
                .cloned()
                .expect("new thread populated");
            let paths = new_thread
                .read(cx)
                .work_dirs()
                .expect("work_dirs propagated to AcpThread")
                .paths()
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert_eq!(
                paths,
                vec![member_cwd.to_string_lossy().into_owned()],
                "rotate_context must reuse session.cwd, not solution.root"
            );
            // session.cwd itself must also be unchanged.
            assert_eq!(session.read(cx).cwd, member_cwd);
        });
    });
}

/// Same regression for `/clear` (reset_context): a context wipe must
/// not reset the session's working directory away from the member
/// sub-dir.
#[gpui::test]
async fn reset_context_preserves_session_cwd(cx: &mut TestAppContext) {
    let (session_id, _old_thread, _tmp) = create_session_with_thread(cx).await;

    let member_cwd = std::path::PathBuf::from("/tmp/sol-x/member-x");
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| s.cwd = member_cwd.clone());
            cx.notify();
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let new_thread = session
                .read(cx)
                .acp_thread()
                .cloned()
                .expect("new thread populated");
            let paths = new_thread
                .read(cx)
                .work_dirs()
                .expect("work_dirs propagated to AcpThread")
                .paths()
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert_eq!(
                paths,
                vec![member_cwd.to_string_lossy().into_owned()],
                "reset_context must reuse session.cwd, not solution.root"
            );
            assert_eq!(session.read(cx).cwd, member_cwd);
        });
    });
}

/// `build_session_meta` shapes the system prompt into the exact JSON
/// envelope claude-agent-acp expects: `{ "systemPrompt": { "append": "<text>" } }`.
/// A wrong key name or nesting level silently drops the prompt — the
/// agent ignores unknown `_meta` keys per the ACP spec, so a typo here
/// would not surface as an error and the bug would only manifest as
/// "agent has no idea it's in a Solution". Pin the shape AND the empty-
/// prompt None path so future adapter changes can't regress either.
#[gpui::test]
fn build_session_meta_emits_correct_json_shape(cx: &mut TestAppContext) {
    use crate::claude_adapter::{CLAUDE_ACP_AGENT_ID, ClaudeAcpAdapter};
    use solutions::{MemberId, Solution, SolutionMember};

    let mut registry = AdapterRegistry::new();
    registry.register(Arc::new(ClaudeAcpAdapter));
    cx.update(|cx| SolutionAgentStore::init_global(cx, Arc::new(registry)));

    let solution = Solution {
        id: SolutionId(6),
        name: "test-meta".into(),
        root: PathBuf::from("/tmp/sol-meta"),
        members: vec![SolutionMember {
            id: MemberId(1),
            name: "foo".into(),
            local_path: PathBuf::from("/tmp/sol-meta/foo"),
            origin_catalog_id: None,
        }],
        last_opened_at: Some(Utc::now().timestamp_millis()),
    };

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let meta = store
                .build_session_meta(
                    &SharedString::from(CLAUDE_ACP_AGENT_ID),
                    &solution,
                    None,
                    None,
                    cx,
                )
                .expect("registered ClaudeAcpAdapter produces a non-empty prompt");
            let system_prompt = meta
                .get("systemPrompt")
                .expect("meta carries `systemPrompt` key (camelCase, not snake_case — claude-agent-acp matches exactly)")
                .as_object()
                .expect("`systemPrompt` is an object (not a bare string — agent reads `append` field)");
            let append = system_prompt
                .get("append")
                .expect("`append` key present (vs. replacing the preset entirely)")
                .as_str()
                .expect("`append` value is a string");
            assert!(
                append.contains("/tmp/sol-meta"),
                "prompt mentions solution root, got {append:?}"
            );
            assert!(
                append.contains("foo"),
                "prompt mentions member project, got {append:?}"
            );

            // Unknown agent → None (registry lookup fails)
            let none_meta =
                store.build_session_meta(
                    &SharedString::from("not-registered"),
                    &solution,
                    None,
                    None,
                    cx,
                );
            assert!(none_meta.is_none(), "unknown agent yields None");
        });
    });

    // Empty-prompt branch: a registered adapter that produces an empty
    // string must yield None so we don't ship a `_meta.systemPrompt:
    // {append: ""}` envelope (claude-agent-acp would then append nothing
    // to the preset and the round-trip wastes bandwidth + clutters the
    // request log).
    struct EmptyAdapter;
    impl crate::adapter::SolutionAgentAdapter for EmptyAdapter {
        fn agent_id(&self) -> AgentServerId {
            SharedString::from("empty-adapter")
        }
        fn display_name(&self) -> SharedString {
            SharedString::from("empty")
        }
        fn icon(&self) -> ui::IconName {
            ui::IconName::Sparkle
        }
        fn build_initial_system_prompt(&self, _: &Solution) -> String {
            String::new()
        }
    }
    cx.update(|cx| {
        let mut empty_registry = AdapterRegistry::new();
        empty_registry.register(Arc::new(EmptyAdapter));
        SolutionAgentStore::init_global(cx, Arc::new(empty_registry));
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let meta = store.build_session_meta(
                &SharedString::from("empty-adapter"),
                &solution,
                None,
                None,
                cx,
            );
            assert!(meta.is_none(), "empty prompt yields None");
        });
    });
}

// =====================================================================
// Auto-wake for MCP `send_message_blocks` on cold sessions
// =====================================================================

/// Sending to a cold session whose owning Solution is gone from
/// `SolutionStore` returns the structured `unknown_solution` error —
/// not the legacy "session has no ACP thread yet" — so MCP clients can
/// distinguish "the agent isn't running yet (we'll wake it)" from
/// "this session is orphaned (give up)".
#[gpui::test]
async fn cold_send_unknown_solution_returns_structured_error(cx: &mut TestAppContext) {
    // Use a SolutionId that won't be in SolutionStore. We still need
    // SolutionStore initialised because `SolutionAgentStore::init_global`
    // subscribes to it.
    let dir = tempfile::tempdir().expect("tempdir");
    cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
        let solutions_store =
            solutions::SolutionStore::for_test(dir.path().join("solutions.json"), cx);
        solutions::install_global_for_test(solutions_store, cx);
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
    });

    let orphan_solution_id = SolutionId(15);
    let session_id = SolutionSessionId::new();
    let agent_id = SharedString::from("mock-agent");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            insert_cold_session(
                session_id,
                orphan_solution_id,
                agent_id.clone(),
                None,
                None,
                store,
                cx,
            );
        });
    });

    let task = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
                agent_client_protocol::schema::TextContent::new("hello".to_string()),
            )];
            store.send_message_blocks(session_id, blocks, cx)
        })
    });

    let err = task.await.expect_err("orphan solution must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unknown_solution"),
        "expected structured 'unknown_solution' error, got {msg:?}"
    );
    assert!(
        !msg.contains("has no ACP thread yet"),
        "auto-wake should replace the legacy 'no ACP thread' error, got {msg:?}"
    );
}

/// Hot-path passthrough: when a session has a live `acp_thread`,
/// `send_message_blocks` flips the state to Running synchronously
/// without entering the wake path — the wake helper must not interfere
/// with already-attached sessions.
#[gpui::test]
async fn hot_send_does_not_enter_wake_path(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
                agent_client_protocol::schema::TextContent::new("hot path".to_string()),
            )];
            // Detach — we only care that the synchronous state flip
            // happened. The actual prompt path uses the MockConnection
            // which returns Err without a gate (see test_support); that
            // would arrive as `Errored` after the spawn resolves.
            store
                .send_message_blocks(session_id, blocks, cx)
                .detach_and_log_err(cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let state = session.read(cx).state.clone();
            assert!(
                matches!(state, SessionState::Running { .. }),
                "hot path should flip to Running synchronously, got {state:?}"
            );
        });
    });
}

#[gpui::test]
async fn append_stamps_entry_created_ms_once_per_index(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Append a user entry. `push_user_content_block` creates a new
    // UserMessage entry (no existing user message last), so `push_entry`
    // fires, which emits `AcpThreadEvent::NewEntry`.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Append an assistant entry. The thread's last entry is now UserMessage,
    // so `push_assistant_content_block` also calls `push_entry` → `NewEntry`.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    let (stamp0, stamp1) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.entries.len(), 2, "two appends → two entries");
        (s.entries[0].created_ms, s.entries[1].created_ms)
    });
    assert!(stamp0 > 0, "first stamp must be positive");
    assert!(stamp1 >= stamp0, "timestamps are non-decreasing");

    // Now drive an in-place EntryUpdated on the last entry (streaming more
    // text into the existing assistant message). `push_assistant_content_block`
    // with an existing assistant entry as the last entry emits `EntryUpdated`,
    // NOT `NewEntry` — so the entries count and stamps must NOT change.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(" more text".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.entries.len(), 2, "in-place update must not add an entry");
        assert_eq!(
            s.entries[1].created_ms, stamp1,
            "existing stamp must be unchanged after EntryUpdated"
        );
    });
}

/// Phase 4 Task 3: each transcript mutation must write the matching
/// `solution_session_entries` rows incrementally (not a wholesale blob).
/// Drives NewEntry×2, EntryUpdated(last), then EntriesRemoved and asserts
/// `db.load_entries(session_id)` mirrors `session.entries` after every step:
/// same count, ascending idx 0..n, mod_seq parity, and the payload decoding
/// back to the entry's kind.
#[gpui::test]
async fn transcript_mutations_persist_entry_rows(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    // Assert the persisted rows match the in-memory Main stream exactly. As of
    // phase 6b the persist authority is `streams[StreamId::Main]` (coalesced,
    // Main-local index), NOT the flat `entries` ingest buffer — a rewind that
    // splits a coalesce group re-stamps the Main survivor's mod_seq without
    // touching the flat entry, so the two mod_seqs legitimately diverge.
    async fn assert_rows_match(
        cx: &mut TestAppContext,
        db: &crate::db::SolutionAgentDb,
        session_id: SolutionSessionId,
    ) {
        let entries = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session exists");
            session
                .read(cx)
                .streams
                .get(&crate::stream::StreamId::Main)
                .map(|s| s.entries.clone())
                .unwrap_or_default()
        });
        let rows = db.load_entries(session_id).await.expect("load entries");
        assert_eq!(
            rows.len(),
            entries.len(),
            "row count must match the in-memory Main stream"
        );
        for (idx, (row, entry)) in rows.iter().zip(entries.iter()).enumerate() {
            assert_eq!(row.idx, idx as i64, "rows must be in ascending idx order");
            assert_eq!(
                row.mod_seq, entry.mod_seq as i64,
                "row mod_seq must mirror the Main-stream entry's mod_seq"
            );
            let kind = crate::session_entry::kind_from_payload(&row.payload)
                .expect("payload decodes to a kind");
            assert_eq!(
                kind, entry.kind,
                "row payload must decode to the entry kind"
            );
        }
    }

    // NewEntry #1 (user message).
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();
    assert_rows_match(cx, &db, session_id).await;

    // NewEntry #2 (assistant message — distinct entry).
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();
    assert_rows_match(cx, &db, session_id).await;

    // EntryUpdated on the last (assistant) entry — streaming more text into
    // the existing entry emits EntryUpdated, not NewEntry. The row at that
    // idx must be re-upserted with the new payload + bumped mod_seq.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(" more".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();
    assert_rows_match(cx, &db, session_id).await;

    // EntriesRemoved(1..2) — drop the last entry. The persisted rows must
    // shrink in lockstep so a stale idx-1 row can't corrupt a cold load.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntriesRemoved(1..2));
        });
    });
    cx.executor().run_until_parked();
    assert_rows_match(cx, &db, session_id).await;
    let remaining = db.load_entries(session_id).await.expect("load entries");
    assert_eq!(
        remaining.len(),
        1,
        "EntriesRemoved must delete the trailing row"
    );
}

/// Ephemeral supervisor judge/auditor sessions must leave NO durable trace: a
/// persisted row reloads (without the in-memory `is_supervisor_ephemeral` flag)
/// as a normal child session, leaking the judge's private reasoning as a
/// visible session chip after a restart. Every persist-to-DB path guards on the
/// flag, so an ephemeral session writes neither entry rows nor a
/// `solution_sessions` row, even when the entry-append + explicit flushes fire.
#[gpui::test]
async fn ephemeral_session_is_never_persisted(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));

    // Stamp the session ephemeral, then wire persistence.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| s.is_supervisor_ephemeral = true);
            store.set_persistence(db.clone(), cx);
        });
    });

    // Append an entry (NewEntry → the auto persist_upsert path) AND fire the
    // explicit row/all-rows flushes — every one must skip the ephemeral session.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(
                        "private supervisor reasoning".to_string(),
                    ),
                ),
                cx,
            );
        });
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.persist_session_row(session_id, cx);
            store.persist_all_rows(session_id, cx);
        });
    });
    cx.executor().run_until_parked();

    assert!(
        db.load_entries(session_id)
            .await
            .expect("load entries")
            .is_empty(),
        "ephemeral session entries must never be persisted"
    );
    assert!(
        db.load_change_seq(session_id)
            .await
            .expect("load change_seq")
            .is_none(),
        "ephemeral session must leave no solution_sessions row"
    );
}

#[gpui::test]
async fn append_after_resumed_unstamped_history_does_not_fabricate(cx: &mut TestAppContext) {
    use crate::model::NO_TIMESTAMP_MS;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Append two entries (user + assistant). These get real stamps on the
    // normal path.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Simulate a resumed pre-feature session: force the two existing entries'
    // created_ms to the absent sentinel — as if they were loaded from a legacy
    // blob that had no timestamps. The next NewEntry must NOT overwrite these
    // (no restamp on pre-existing entries) and must give the new entry a real
    // positive stamp.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        session.update(cx, |s, _| {
            for e in s.entries.iter_mut() {
                e.created_ms = NO_TIMESTAMP_MS;
            }
        });
    });

    // Now the user sends a new message → a genuinely-new entry arrives at
    // `global_entry_index == 2` (the thread already has 2 historical entries).
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("new".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);

        // All three entries must be present (2 historical + 1 new).
        assert_eq!(s.entries.len(), 3, "gap-fill must keep entries aligned");
        // The two historical (pre-existing) entries keep their sentinel.
        assert_eq!(
            s.entries[0].created_ms, NO_TIMESTAMP_MS,
            "historical entry must keep sentinel, not be restamped"
        );
        assert_eq!(
            s.entries[1].created_ms, NO_TIMESTAMP_MS,
            "historical entry must keep sentinel, not be restamped"
        );
        // Only the just-appended entry gets a real positive timestamp.
        assert!(
            s.entries[2].created_ms > 0,
            "the genuinely-new entry must hold a real positive timestamp, got {}",
            s.entries[2].created_ms
        );
    });
}

#[gpui::test]
async fn reset_context_clears_entries(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Append one user entry so `entries` is non-empty.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    let len_before = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .entries
            .len()
    });
    assert_eq!(len_before, 1, "one append → one entry before reset");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");

    let len_after = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .entries
            .len()
    });
    assert_eq!(len_after, 0, "reset_context clears entries");
}

/// `EntriesRemoved` must truncate `session.entries` at `cold_count + range.start`,
/// keeping the surviving prefix aligned with the surviving thread entries.
/// This exercises the actual truncation path on a populated entries list;
/// `entries_removed_partial_rewind_preserves_token_state` covers the token
/// state side independently.
#[gpui::test]
async fn entries_removed_truncates_entries(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Append two entries so entries.len() == 2.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("first".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("second".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    let stamp0 = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(
            s.entries.len(),
            2,
            "two appends → two entries before removal"
        );
        s.entries[0].created_ms
    });

    // Emit EntriesRemoved(1..2) — removes the last entry. The live thread
    // still has one surviving entry (the user message), so this is a
    // partial rewind: the handler truncates entries to length 1 (cold=0 +
    // range.start=1) but does NOT reset token state.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntriesRemoved(1..2));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(
            s.entries.len(),
            1,
            "EntriesRemoved(1..2) must truncate entries to length 1"
        );
        assert_eq!(
            s.entries[0].created_ms, stamp0,
            "surviving entry's created_ms must be unchanged"
        );
    });
}

/// `rotate_context` swaps the underlying ACP thread and clears `entries`.
/// Without this, entries from the old thread would bleed into the new context.
#[gpui::test]
async fn rotate_context_clears_entries(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Append one user entry so `entries` is non-empty before rotation.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    let len_before = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .entries
            .len()
    });
    assert_eq!(len_before, 1, "one append → one entry before rotation");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.rotate_context(session_id, cx))
    })
    .await
    .expect("rotate_context");

    let len_after = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .entries
            .len()
    });
    assert_eq!(len_after, 0, "rotate_context clears entries");
}

/// The `AcpThreadEvent::EntryUpdated` handler debounces re-emits of
/// `SessionMessageAppended`: a 500 ms quiet window collapses a streaming
/// burst into a single emit, and a 2 s max-stale guard forces an emit even
/// when an entry is continuously dirty so the consumer never starves.
///
/// We observe the emit count by subscribing to the store's
/// `SessionMessageAppended` events directly (no MCP socket needed) and
/// driving GPUI's test clock with `advance_clock`.
#[gpui::test]
async fn entry_updated_burst_coalesces_then_force_emits(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Seed one real entry so EntryUpdated(0) targets a live index.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("seed".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Count SessionMessageAppended emits for our index via a store
    // subscription. The subscription is held for the test's lifetime.
    let appended = Rc::new(std::cell::RefCell::new(0usize));
    let _subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let appended = appended.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionMessageAppended(id, idx) = event
                && *id == session_id
                && *idx == 0
            {
                *appended.borrow_mut() += 1;
            }
        })
    });

    // (a) Burst coalesces: fire several EntryUpdated(0) within the 500 ms
    // quiet window. Each replaces the prior debounce Task, cancelling its
    // timer, so only the final window's task survives.
    for _ in 0..5 {
        cx.update(|cx| {
            acp_thread.update(cx, |_t, cx| {
                cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(0));
            });
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(50));
    }
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        0,
        "no emit before the 500 ms quiet window elapses"
    );

    // Let the quiet window expire → exactly one coalesced emit.
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(600));
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        1,
        "burst of 5 updates coalesces into a single emit"
    );

    // (b) Continuous dirtying still force-emits within max-stale.
    //
    // HARNESS LIMITATION: the max-stale guard compares wall-clock
    // `std::time::Instant::now()` against `first_dirty_at`, and the GPUI
    // test executor's `advance_clock` only moves the *dispatcher* timer
    // (which drives the 500 ms debounce), NOT `Instant::now()`. So a
    // microsecond-fast burst can never accumulate 2 s of real staleness
    // and the force-emit branch can't be exercised by clock advancement.
    //
    // To assert the invariant deterministically we seed a throttle slot
    // whose `first_dirty_at` is already 2 s+ in the past (as a long
    // continuous stream would have left it), then fire one more
    // EntryUpdated(0): the handler must take the max-stale branch and emit
    // SYNCHRONOUSLY (no debounce wait), and clear the slot.
    *appended.borrow_mut() = 0;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _cx| {
            store.entry_update_throttles.insert(
                (session_id, 0),
                EntryUpdateThrottle {
                    first_dirty_at: std::time::Instant::now()
                        - std::time::Duration::from_millis(2_500),
                    _task: gpui::Task::ready(()),
                },
            );
        });
    });
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(0));
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        1,
        "max-stale breach must force a synchronous emit"
    );
    let still_throttled = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .entry_update_throttles
            .contains_key(&(session_id, 0))
    });
    assert!(
        !still_throttled,
        "force-emit must clear the throttle slot so the next window starts fresh"
    );
}

/// Reproduction for the "phone never shows the LAST message of a turn until the
/// next send" bug. Drives the real turn-end acp event sequence — a streamed
/// assistant reply whose trailing bytes are flushed at end-of-turn, then
/// `Stopped` — and asserts that, from a cursor captured BEFORE the turn ended, a
/// `get_session_changes` poll (what the mobile runs on `agent_session_dirty`)
/// returns the FINAL entry with its complete text and that the session's
/// `change_seq` advanced past it.
///
/// The wire path the mobile depends on at turn end is: the `Stopped` event flips
/// the session Idle, which advances `change_seq` and emits a `dirty` carrying
/// that fresh seq; the mobile converges on it. This test pins that the final
/// entry IS visible to that poll — without the fix it was not, because the only
/// `change_seq` bump after the final flush rode the debounced
/// `SessionMessageAppended` whose timer was cancelled by the session being torn
/// down / never drained, so the `Stopped` dirty reported a seq that did not yet
/// cover the flushed tail.
#[gpui::test]
async fn final_streamed_message_is_visible_to_delta_poll_after_stop(cx: &mut TestAppContext) {
    use agent_client_protocol::schema as acp;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // A user message (entry 0) then an assistant reply (entry 1). The assistant
    // entry starts with a first chunk so subsequent text streams into it.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                acp::ContentBlock::Text(acp::TextContent::new("question".to_string())),
                cx,
            );
            t.push_assistant_content_block(
                acp::ContentBlock::Text(acp::TextContent::new("Answer: ".to_string())),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Capture the cursor the mobile would hold mid-turn (before the final
    // streamed tail). This stands in for `openSeq` on the phone.
    let cursor_before_tail = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let s = store.read(cx).session(session_id).expect("session");
        s.read(cx).change_seq
    });

    // Force the session into Running so the upcoming Stopped flips the
    // discriminant (Running -> Idle) and therefore emits the state change /
    // dirty the mobile converges on — exactly what a real turn looks like.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.mutate_state(
                session_id,
                |state| {
                    *state = SessionState::Running {
                        started_at: std::time::Instant::now(),
                        notified: false,
                    };
                },
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Stream a long trailing tail into the assistant entry. It lands in the
    // streaming buffer and is revealed gradually — the final bytes only reach
    // the markdown when the buffer is flushed at end-of-turn.
    const TAIL: &str =
        "this is the final sentence of the assistant's reply that must reach the phone.";
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                acp::ContentBlock::Text(acp::TextContent::new(TAIL.to_string())),
                false,
                cx,
            );
        });
    });
    // Deliberately do NOT drain the reveal task fully — the tail is still
    // buffered. The turn-end flush + Stopped is what must surface it.

    // Now emit the exact end-of-turn sequence the run-turn completion code
    // produces (acp_thread.rs Ok path): cancel the reveal task, flush the
    // buffered tail into the markdown, signal the final EntryUpdated, then
    // Stopped. `cancel` flushes the streaming buffer for us.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            // Flush the buffered tail (mirrors `flush_streaming_text` at turn end).
            let _ = t.cancel(cx);
            let last = t.entries().len().saturating_sub(1);
            cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(last));
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                acp::StopReason::EndTurn,
            ));
        });
    });
    cx.executor().run_until_parked();

    // CRITICAL: WITHOUT advancing any debounce/reveal timers, the `dirty`
    // signal the editor pushed for the Stopped transition must already carry a
    // `current_seq` that covers the final entry. The mobile receives this dirty
    // and converges to it; if it does NOT cover the final entry, the phone's
    // convergence poll either early-returns (cursor already >= target) or fetches
    // a transcript that still lacks the flushed tail — the "last message missing
    // until next send" symptom. Reading the dirty payload here mirrors the exact
    // value `event_sources::build_session_dirty_payload` would have emitted on
    // the Stopped store event.
    let (immediate_dirty_seq, final_mod_seq) = cx.update(|cx| {
        let dirty = crate::event_sources::build_session_dirty_payload(session_id, cx);
        let seq = dirty
            .get("current_seq")
            .and_then(|v| v.as_u64())
            .expect("dirty payload carries current_seq");
        let store = SolutionAgentStore::global(cx);
        let s = store.read(cx).session(session_id).expect("session");
        let final_mod = s.read(cx).entries.last().expect("final entry").mod_seq;
        (seq, final_mod)
    });
    assert!(
        immediate_dirty_seq >= final_mod_seq,
        "the dirty pushed on the Stopped transition (current_seq={immediate_dirty_seq}) must already \
         cover the final entry's mod_seq ({final_mod_seq}) — without advancing any debounce timer; \
         otherwise the phone never converges to the final message until the next send"
    );

    // Drain any debounce / reveal timers so an honest "after everything settles"
    // state is observed (the bug is that the mobile must NOT need this).
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(2_500));
    cx.executor().run_until_parked();

    // The session must be Idle and its change_seq must have advanced past the
    // cursor the mobile held mid-turn.
    let (state_idle, change_seq_now, final_text) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let s = store.read(cx).session(session_id).expect("session");
        let s = s.read(cx);
        let idle = matches!(s.state, SessionState::Idle);
        let text = match &s.entries.last().expect("final entry").kind {
            crate::session_entry::SessionEntryKind::AssistantMessage { chunks } => chunks
                .iter()
                .map(|c| match c {
                    crate::session_entry::AssistantChunk::Message(m) => m.to_string(),
                    crate::session_entry::AssistantChunk::Thought(t) => t.to_string(),
                })
                .collect::<String>(),
            other => panic!("expected assistant final entry, got {other:?}"),
        };
        (idle, s.change_seq, text)
    });
    assert!(state_idle, "session must be Idle after Stopped");
    assert!(
        final_text.contains("final sentence"),
        "the flushed tail must be stored in the final entry: {final_text:?}"
    );
    assert!(
        change_seq_now > cursor_before_tail,
        "change_seq must advance past the mid-turn cursor ({change_seq_now} > {cursor_before_tail})"
    );

    // The decisive assertion: a delta poll from the mid-turn cursor (what the
    // mobile runs when it receives the `Stopped` dirty) must return the final
    // entry carrying the full flushed tail.
    use context_server::listener::McpServerTool as _;
    let delta = crate::mcp::GetSessionChangesTool
        .run(
            crate::mcp::GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: cursor_before_tail,
                known_epoch: 0,
                stream_id: None,
                include_images: false,
            },
            &mut cx.to_async(),
        )
        .await
        .expect("get_session_changes")
        .structured_content;
    assert!(!delta.reset, "no epoch rotation expected");
    let final_index = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let s = store.read(cx).session(session_id).expect("session");
        s.read(cx).entries.len() - 1
    });
    let returned_final = delta
        .changed_entries
        .iter()
        .find(|e| e.index == final_index)
        .unwrap_or_else(|| {
            panic!(
                "delta poll from mid-turn cursor must include the final entry (index {final_index}); \
                 got indices {:?}",
                delta.changed_entries.iter().map(|e| e.index).collect::<Vec<_>>()
            )
        });
    assert!(
        returned_final.preview.contains("final sentence"),
        "the delta-returned final entry must carry the flushed tail: {:?}",
        returned_final.preview
    );
}

/// Regression for the "final message strands until next send" bug, isolating the
/// fragile path: the final entry's append signal rides ONLY the `EntryUpdated`
/// debounce, and `Stopped` does NOT change the state discriminant (so the
/// `mark_state_changed` dirty does not bail us out). The turn-completion handler
/// must flush the pending debounce SYNCHRONOUSLY on `Stopped`, emitting the
/// final entry's `SessionMessageAppended` (→ `agent_session_dirty`) immediately —
/// WITHOUT waiting out the 500 ms / 2 s debounce window.
///
/// Without the fix the only append emit is the still-armed debounce task, so
/// this asserts zero appends right after `Stopped` and the test fails.
#[gpui::test]
async fn stopped_flushes_pending_entry_update_debounce_immediately(cx: &mut TestAppContext) {
    use agent_client_protocol::schema as acp;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // One assistant entry to stream into.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                acp::ContentBlock::Text(acp::TextContent::new("partial".to_string())),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Subscribe to SessionMessageAppended for the assistant entry (index 0).
    let appended = Rc::new(std::cell::RefCell::new(0usize));
    let _subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let appended = appended.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionMessageAppended(id, idx) = event
                && *id == session_id
                && *idx == 0
            {
                *appended.borrow_mut() += 1;
            }
        })
    });

    // Fire an EntryUpdated (streaming chunk). This arms the 500 ms debounce —
    // no append emit yet. Session stays Idle (never set Running), so the
    // upcoming Stopped will NOT change the state discriminant: the debounce
    // flush is the ONLY thing that can surface the final entry promptly.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(0));
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        0,
        "debounced EntryUpdated must not emit before the quiet window"
    );

    // Turn ends. The Stopped handler must flush the pending debounce slot
    // synchronously → exactly one append emit on this tick, with NO clock
    // advance.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                acp::StopReason::EndTurn,
            ));
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        1,
        "Stopped must flush the pending entry-update debounce immediately so the \
         final entry's append (and its agent_session_dirty) reaches the client \
         without waiting out the debounce window"
    );

    // The flushed slot must be gone so the debounce timer can't double-fire.
    let still_throttled = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .entry_update_throttles
            .contains_key(&(session_id, 0))
    });
    assert!(
        !still_throttled,
        "the flushed throttle slot must be cleared on Stopped"
    );

    // And the timer firing later must NOT produce a second append.
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(2_500));
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        1,
        "the cleared debounce timer must not double-emit after Stopped flushed it"
    );
}

/// Symmetric with `stopped_flushes_pending_entry_update_debounce_immediately`:
/// a turn that ends via `Error` must ALSO flush the pending entry-update
/// debounce synchronously, so the final entry's append (and its
/// `agent_session_dirty`) reaches the client without waiting out the 500 ms
/// window. Without the flush, a turn that errored while already `Errored`
/// (state discriminant unchanged → no state dirty) would leave the last
/// assistant bytes reachable only after the debounce timer — the tail-loss
/// class this fix closes.
#[gpui::test]
async fn errored_flushes_pending_entry_update_debounce_immediately(cx: &mut TestAppContext) {
    use agent_client_protocol::schema as acp;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                acp::ContentBlock::Text(acp::TextContent::new("partial".to_string())),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    let appended = Rc::new(std::cell::RefCell::new(0usize));
    let _subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let appended = appended.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionMessageAppended(id, idx) = event
                && *id == session_id
                && *idx == 0
            {
                *appended.borrow_mut() += 1;
            }
        })
    });

    // Arm the 500 ms debounce with a streaming EntryUpdated — no append yet.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(0));
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(*appended.borrow(), 0, "debounced update must not emit yet");

    // Turn errors. The Error handler must flush the pending slot synchronously.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Error);
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        1,
        "Error must flush the pending entry-update debounce immediately, like Stopped"
    );

    let still_throttled = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .entry_update_throttles
            .contains_key(&(session_id, 0))
    });
    assert!(!still_throttled, "the flushed throttle slot must be cleared on Error");

    cx.executor()
        .advance_clock(std::time::Duration::from_millis(2_500));
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        1,
        "the cleared debounce timer must not double-emit after Error flushed it"
    );
}

/// Path to the `claude_native` mock binary (a bash script) used by the
/// integration tests in `crates/claude_native/tests/`. Reuses the same fixture
/// here so a Phase-2 store-routing test can stand up a real
/// `ClaudeNativeConnection` without a system-installed `claude`.
fn native_mock_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("claude_native")
        .join("tests")
        .join("fixtures")
        .join("mock_claude.sh")
}

/// All mid-turn sends (including those backed by a native `ClaudeNativeConnection`)
/// now route through `pending_messages` — the inject branch has been removed.
/// In-turn delivery will be restored via a pull-closure in a later task.
#[gpui::test]
async fn send_during_running_on_native_connection_routes_to_queue(cx: &mut TestAppContext) {
    use acp_thread::AgentConnection;
    use agent_client_protocol::schema as acp;
    use agent_servers::{AgentServer, AgentServerDelegate};
    use claude_native::{ClaudeNativeAgentServer, ClaudeNativeConnection};
    use project::AgentId;

    let mock_binary = native_mock_binary();
    if !mock_binary.exists() {
        panic!(
            "mock claude binary missing at {} — tests/fixtures/mock_claude.sh not bundled?",
            mock_binary.display()
        );
    }
    cx.executor().allow_parking();

    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("claude-native");

    let server = Rc::new(ClaudeNativeAgentServer::with_binary(
        AgentId::new("claude-native"),
        mock_binary,
        Vec::new(),
    ));

    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(agent_id.clone(), server.clone());
        });
    });

    let connection: Rc<dyn acp_thread::AgentConnection> = cx
        .update(|cx| {
            let store = project.read(cx).agent_server_store().clone();
            let delegate = AgentServerDelegate::new(store, None, None);
            AgentServer::connect(server.as_ref(), delegate, project.clone(), cx)
        })
        .await
        .expect("native connect");
    let native = connection
        .clone()
        .downcast::<ClaudeNativeConnection>()
        .expect("downcast to ClaudeNativeConnection");

    let work_dirs = util::path_list::PathList::new(&[std::env::temp_dir().as_path()]);
    let acp_thread = cx
        .update(|cx| Rc::clone(&native).new_session(project.clone(), work_dirs, cx))
        .await
        .expect("new_session");

    let acp_session_id = acp_thread.read_with(cx, |t, _| t.session_id().clone());

    let session_id = SolutionSessionId::new();
    cx.update(|cx| {
        let session = cx.new(|_| {
            let mut s = crate::model::SolutionSession::new_idle(
                session_id,
                solution_id,
                agent_id.clone(),
                acp_session_id.clone(),
            );
            s.title = SharedString::from("native-test");
            s.project = Some(project.clone());
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            s
        });
        session.update(cx, |session, cx| {
            session.set_acp_thread(Some(acp_thread.clone()), cx);
        });
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _store_cx| {
            store.sessions.insert(session_id, session);
            store
                .by_solution
                .entry(solution_id)
                .or_default()
                .push(session_id);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store
                .send_message_blocks(
                    session_id,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "PURPLE_PINEAPPLE".to_string(),
                    ))],
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            // All running sends go through pending_messages now — native is no exception.
            assert_eq!(
                s.pending_messages.len(),
                1,
                "native-backed Running send must queue into pending_messages"
            );
            let payload: String = s.pending_messages[0]
                .blocks
                .iter()
                .filter_map(|b| match b {
                    acp::ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            assert!(
                payload.contains("PURPLE_PINEAPPLE"),
                "queued bundle must carry the sent text, got {payload:?}"
            );
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .update(cx, |store, cx| store.close_session(session_id, cx))
            .ok();
    });
    drop(acp_thread);
    drop(native);
    drop(server);
}

/// End-to-end of the pull side: after a native-backed session is wired through
/// `subscribe_to_session`, the connection holds a store pull-closure. Invoking
/// that closure (as the live pump does at each hook) must map the ACP session
/// id back to the `SolutionSessionId`, drain `pending_messages`, push a single
/// user entry onto the thread, and return the agent-facing text (no hint for a
/// mid-turn hook, hint prepended at end-of-turn).
#[gpui::test]
async fn registered_store_pull_drains_queue_and_returns_followup_text(cx: &mut TestAppContext) {
    use acp_thread::AgentConnection;
    use agent_client_protocol::schema as acp;
    use agent_servers::{AgentServer, AgentServerDelegate};
    use claude_native::ClaudeNativeConnection;
    use project::AgentId;

    let mock_binary = native_mock_binary();
    if !mock_binary.exists() {
        panic!(
            "mock claude binary missing at {} — tests/fixtures/mock_claude.sh not bundled?",
            mock_binary.display()
        );
    }
    cx.executor().allow_parking();

    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("claude-native");

    let server = Rc::new(claude_native::ClaudeNativeAgentServer::with_binary(
        AgentId::new("claude-native"),
        mock_binary,
        Vec::new(),
    ));

    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(agent_id.clone(), server.clone());
        });
    });

    let connection: Rc<dyn acp_thread::AgentConnection> = cx
        .update(|cx| {
            let store = project.read(cx).agent_server_store().clone();
            let delegate = AgentServerDelegate::new(store, None, None);
            AgentServer::connect(server.as_ref(), delegate, project.clone(), cx)
        })
        .await
        .expect("native connect");
    let native = connection
        .clone()
        .downcast::<ClaudeNativeConnection>()
        .expect("downcast to ClaudeNativeConnection");

    let work_dirs = util::path_list::PathList::new(&[std::env::temp_dir().as_path()]);
    let acp_thread = cx
        .update(|cx| Rc::clone(&native).new_session(project.clone(), work_dirs, cx))
        .await
        .expect("new_session");

    let acp_session_id = acp_thread.read_with(cx, |t, _| t.session_id().clone());

    // Insert the session AND wire it through `subscribe_to_session` (the single
    // attach choke point) so Part A's pull-registration runs.
    let session_id = SolutionSessionId::new();
    cx.update(|cx| {
        let session = cx.new(|_| {
            let mut s = crate::model::SolutionSession::new_idle(
                session_id,
                solution_id,
                agent_id.clone(),
                acp_session_id.clone(),
            );
            s.title = SharedString::from("native-pull-test");
            s.project = Some(project.clone());
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            s
        });
        session.update(cx, |session, cx| {
            session.set_acp_thread(Some(acp_thread.clone()), cx);
        });
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.sessions.insert(session_id, session.clone());
            store
                .by_solution
                .entry(solution_id)
                .or_default()
                .push(session_id);
            let sub = store.subscribe_to_session(session_id, acp_thread.clone(), cx);
            session.update(cx, |s, _| s._acp_subscription = Some(sub));
        });
    });

    // (1) Pull registered via `subscribe_to_session` ⇒ Part A ran.
    assert!(
        native.store_pull_registered_for_test(),
        "subscribe_to_session must register the native store pull"
    );

    // (2) Mid-turn send enqueues into pending_messages.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store
                .send_message_blocks(
                    session_id,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "FOLLOWUP_XYZ".to_string(),
                    ))],
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            assert_eq!(
                session.read(cx).pending_messages.len(),
                1,
                "running send must queue"
            );
        });
    });

    let entries_before = acp_thread.read_with(cx, |t, _| t.entries().len());

    // (3) Invoke the registered pull exactly as the pump would (mid-turn ⇒
    // is_end_of_turn = false). The closure runs `weak.update` itself, so it must
    // be called OUTSIDE any open store update.
    let mut async_cx = cx.to_async();
    // A subagent's hook (agent_id present) must NOT drain the main queue —
    // otherwise a running Agent Teams teammate swallows the follow-up. It
    // stays queued for the main agent's next hook.
    let sub_pull = native.invoke_store_pull_for_test(
        &acp_session_id,
        Some("sub-agent-1"),
        false,
        &mut async_cx,
    );
    assert!(
        sub_pull.is_none(),
        "a subagent hook must not drain the main agent's queue, got {sub_pull:?}"
    );
    // The main agent's hook (no agent_id) drains it.
    let pulled = native.invoke_store_pull_for_test(&acp_session_id, None, false, &mut async_cx);
    let pulled = pulled.expect("pull must return the queued follow-up text");
    assert!(
        pulled.contains("FOLLOWUP_XYZ"),
        "pulled text must carry the queued message, got {pulled:?}"
    );
    assert!(
        !pulled.contains(crate::store::queue::QUEUE_HINT_LINE),
        "mid-turn pull must NOT prepend the queue hint, got {pulled:?}"
    );

    // (4) Queue drained + the thread gained exactly one user entry ⇒ the
    // acp→solution id mapping ran and `take_pending_for_delivery` executed.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            assert_eq!(
                session.read(cx).pending_messages.len(),
                0,
                "pull must drain pending_messages"
            );
        });
    });
    let entries_after = acp_thread.read_with(cx, |t, _| t.entries().len());
    assert_eq!(
        entries_after,
        entries_before + 1,
        "delivery must push exactly one user entry onto the thread"
    );
    let last_is_user = acp_thread.read_with(cx, |t, _| {
        matches!(
            t.entries().last(),
            Some(acp_thread::AgentThreadEntry::UserMessage(_))
        )
    });
    assert!(last_is_user, "the appended entry must be a user message");

    // (5) End-of-turn pull prepends the hint.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store
                .send_message_blocks(
                    session_id,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "FOLLOWUP_EOT".to_string(),
                    ))],
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });
    let mut async_cx = cx.to_async();
    let pulled_eot = native
        .invoke_store_pull_for_test(&acp_session_id, None, true, &mut async_cx)
        .expect("end-of-turn pull must return text");
    assert!(
        pulled_eot.contains("FOLLOWUP_EOT"),
        "end-of-turn pull must carry the queued message, got {pulled_eot:?}"
    );
    assert!(
        pulled_eot.contains(crate::store::queue::QUEUE_HINT_LINE),
        "end-of-turn pull must prepend the queue hint, got {pulled_eot:?}"
    );

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .update(cx, |store, cx| store.close_session(session_id, cx))
            .ok();
    });
    drop(acp_thread);
    drop(native);
    drop(server);
}

/// The `subscribe_to_session` pull closure's authoritative-teammate-close
/// branch (`delivered.is_none() && is_end_of_turn && agent_id.is_some()` →
/// `close_teammate_on_stop`, `store.rs`). The pre-existing coverage above
/// exercises `Some(agent_id)+mid-turn` (must not drain) and `None+end-of-turn`
/// (drains the main queue) via `invoke_store_pull_for_test`, but never
/// `Some(agent_id)+end-of-turn` — the exact combo this branch adds: a
/// subagent's `Stop` hook with nothing left to deliver must close its
/// teammate stream.
#[gpui::test]
async fn native_pull_subagent_end_of_turn_closes_teammate_stream(cx: &mut TestAppContext) {
    use acp_thread::AgentConnection;
    use agent_servers::{AgentServer, AgentServerDelegate};
    use claude_native::ClaudeNativeConnection;
    use project::AgentId;

    let mock_binary = native_mock_binary();
    if !mock_binary.exists() {
        panic!(
            "mock claude binary missing at {} — tests/fixtures/mock_claude.sh not bundled?",
            mock_binary.display()
        );
    }
    cx.executor().allow_parking();

    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("claude-native");

    let server = Rc::new(claude_native::ClaudeNativeAgentServer::with_binary(
        AgentId::new("claude-native"),
        mock_binary,
        Vec::new(),
    ));

    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(agent_id.clone(), server.clone());
        });
    });

    let connection: Rc<dyn acp_thread::AgentConnection> = cx
        .update(|cx| {
            let store = project.read(cx).agent_server_store().clone();
            let delegate = AgentServerDelegate::new(store, None, None);
            AgentServer::connect(server.as_ref(), delegate, project.clone(), cx)
        })
        .await
        .expect("native connect");
    let native = connection
        .clone()
        .downcast::<ClaudeNativeConnection>()
        .expect("downcast to ClaudeNativeConnection");

    let work_dirs = util::path_list::PathList::new(&[std::env::temp_dir().as_path()]);
    let acp_thread = cx
        .update(|cx| Rc::clone(&native).new_session(project.clone(), work_dirs, cx))
        .await
        .expect("new_session");

    let acp_session_id = acp_thread.read_with(cx, |t, _| t.session_id().clone());

    let session_id = SolutionSessionId::new();
    cx.update(|cx| {
        let session = cx.new(|_| {
            let mut s = crate::model::SolutionSession::new_idle(
                session_id,
                solution_id,
                agent_id.clone(),
                acp_session_id.clone(),
            );
            s.title = SharedString::from("native-pull-subagent-stop-test");
            s.project = Some(project.clone());
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            s
        });
        session.update(cx, |session, cx| {
            session.set_acp_thread(Some(acp_thread.clone()), cx);
        });
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.sessions.insert(session_id, session.clone());
            store
                .by_solution
                .entry(solution_id)
                .or_default()
                .push(session_id);
            let sub = store.subscribe_to_session(session_id, acp_thread.clone(), cx);
            session.update(cx, |s, _| s._acp_subscription = Some(sub));
        });
    });
    assert!(
        native.store_pull_registered_for_test(),
        "subscribe_to_session must register the native store pull"
    );

    // A live teammate: parent-thread entries tagged with the spawn tool-call's
    // id (demux produces the `Teammate` stream) plus a registered
    // `BackgroundAgent` whose `parent_tool_use_id` maps back to it — the shape
    // `close_teammate_on_stop` expects.
    let bg_id = crate::background_agent::BackgroundAgentId::new("sub-agent-1");
    let parent_toolu = SharedString::from("toolu_sub_1");
    let teammate = crate::stream::StreamId::Teammate(parent_toolu.clone());
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.set_entries(
                vec![crate::session_entry::SessionEntry {
                    created_ms: 0,
                    mod_seq: 0,
                    subagent_id: Some(parent_toolu.clone()),
                    kind: crate::session_entry::SessionEntryKind::AssistantMessage {
                        chunks: vec![crate::session_entry::AssistantChunk::Message(
                            "streaming".to_string(),
                        )],
                    },
                }],
                cx,
            );
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: PathBuf::new(),
                    registered_at: chrono::Utc::now(),
                    latest: None,
                    last_offset: 0,
                    parent_tool_use_id: Some(parent_toolu.clone()),
                    latest_seq: 0,
                    killed: false,
                },
            );
            s.background_agent_order.push(bg_id.clone());
            assert!(s.streams.contains_key(&teammate), "teammate live before Stop");
        });
    });

    // Drive the REAL closure via `invoke_store_pull_for_test` with an empty
    // queue (⇒ `take_pending_for_delivery` returns `None`) and
    // `Some(agent_id) + is_end_of_turn=true` — the exact combo Task 1 adds.
    let mut async_cx = cx.to_async();
    let sub_pull =
        native.invoke_store_pull_for_test(&acp_session_id, Some("sub-agent-1"), true, &mut async_cx);
    assert!(
        sub_pull.is_none(),
        "empty queue ⇒ nothing to deliver, got {sub_pull:?}"
    );

    cx.update(|cx| {
        let session = SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                !s.streams.contains_key(&teammate),
                "end-of-turn subagent Stop with nothing to deliver must close the teammate stream"
            );
            assert!(
                s.closed_streams.contains_key(&teammate),
                "close reason recorded"
            );
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .update(cx, |store, cx| store.close_session(session_id, cx))
            .ok();
    });
    drop(acp_thread);
    drop(native);
    drop(server);
}

// =====================================================================
// Create-implies-open: a freshly-created top-level session must be
// pinned into its solution's tab strip (tab_order set) so it surfaces
// on every open-set surface (desktop ConsolePanel + mobile workspace
// mirror). Regression coverage for the "session created on mobile is
// invisible everywhere / appears to vanish" bug — the root cause was
// `create_session` writing the row without ever opening it.
// =====================================================================

/// Read a session's `tab_order` out of the store.
fn tab_order_of(cx: &mut TestAppContext, id: SolutionSessionId) -> Option<i64> {
    cx.read(|cx| {
        SolutionAgentStore::global(cx)
            .read(cx)
            .session(id)
            .expect("session exists")
            .read(cx)
            .tab_order
    })
}

#[gpui::test]
async fn create_session_pins_top_level_into_strip(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    assert_eq!(
        tab_order_of(cx, session_id),
        Some(0),
        "a freshly-created top-level session must be pinned into the strip (create implies open)"
    );
}

#[gpui::test]
async fn create_session_appends_subsequent_sessions_in_order(cx: &mut TestAppContext) {
    let (first, _thread, _tmp) = create_session_with_thread(cx).await;
    // A second top-level session in the SAME solution, via the store.
    let (solution_id, agent_id, project) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(first).expect("first session");
        let s = session.read(cx);
        (
            s.solution_id,
            s.agent_id.clone(),
            s.project.clone().expect("first session has project"),
        )
    });
    let second = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session(solution_id, agent_id, project, cx)
            })
        })
        .await
        .expect("create second session");

    assert_eq!(tab_order_of(cx, first), Some(0), "first stays at index 0");
    assert_eq!(
        tab_order_of(cx, second),
        Some(1),
        "second top-level session appends after the first"
    );
}

#[gpui::test]
async fn create_child_session_is_not_pinned(cx: &mut TestAppContext) {
    let (parent, _thread, _tmp) = create_session_with_thread(cx).await;
    let (solution_id, agent_id, project) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(parent).expect("parent session");
        let s = session.read(cx);
        (
            s.solution_id,
            s.agent_id.clone(),
            s.project.clone().expect("parent has project"),
        )
    });
    let child = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session_with_parent(
                    solution_id,
                    agent_id,
                    project,
                    None,
                    None,
                    Some(parent),
                    None,
                    None,
                    false,
                    false,
                    cx,
                )
            })
        })
        .await
        .expect("create child session");

    assert_eq!(tab_order_of(cx, parent), Some(0), "parent is pinned");
    assert_eq!(
        tab_order_of(cx, child),
        None,
        "a sub-agent (parent_session_id set) must NOT be pinned as a top-level tab"
    );
}

#[gpui::test]
async fn open_session_in_strip_is_idempotent(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    // Already pinned at create. A second open must not bump or duplicate
    // the order.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.open_session_in_strip(session_id, cx));
    });
    assert_eq!(
        tab_order_of(cx, session_id),
        Some(0),
        "re-opening an already-pinned session is a no-op"
    );
}

/// Idle-flush (queue drained on `Stopped`) must prepend the "not a reply"
/// hint so the agent knows the follow-up arrived after its last response.
#[gpui::test]
async fn idle_flush_prepends_not_a_reply_hint(cx: &mut TestAppContext) {
    let (session_id, thread, _tmp) = create_session_with_thread(cx).await;

    let entries_before = cx.update(|cx| thread.read(cx).entries().len());

    // Force Running so send_message takes the queueing branch.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message(session_id, "LATE_FOLLOWUP".to_string(), cx)
                .detach_and_log_err(cx);
        });
    });

    // Confirm the message is queued, not yet sent.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                1,
                "message queued while Running"
            );
        });
    });

    // Emit Stopped(EndTurn) — the Stopped handler transitions to Idle and
    // flushes the queue as a new turn WITH the not-a-reply hint prepended.
    cx.update(|cx| {
        thread.update(cx, |_thread, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                agent_client_protocol::schema::StopReason::EndTurn,
            ));
        });
    });
    cx.executor().run_until_parked();

    // Queue must be drained.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                0,
                "queue flushed after Stopped"
            );
        });
    });

    // The flushed turn must have pushed a new UserMessage entry.  Its
    // `chunks` carry the raw blocks we sent — verify the hint and the
    // follow-up text are both present.
    let found = cx.update(|cx| {
        thread.read(cx).entries()[entries_before..].iter().any(|e| {
            if let acp_thread::AgentThreadEntry::UserMessage(msg) = e {
                let text: String = msg
                    .chunks
                    .iter()
                    .filter_map(|b| {
                        if let agent_client_protocol::schema::ContentBlock::Text(t) = b {
                            Some(t.text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect();
                text.contains(crate::store::queue::QUEUE_HINT_LINE)
                    && text.contains("LATE_FOLLOWUP")
            } else {
                false
            }
        })
    });
    assert!(
        found,
        "idle-flush new turn must carry the not-a-reply hint and the follow-up text"
    );
}

/// Phase 2, Task 2: after a `NewEntry` event the session's `entries` list is
/// rebuilt and reflects the full cold+live entry count. Mirrors the harness
/// from `append_stamps_entry_created_ms_once_per_index`.
#[gpui::test]
async fn new_entry_rebuilds_session_entries(cx: &mut TestAppContext) {
    use crate::session_entry::SessionEntryKind;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Append a user entry → NewEntry fires → entries should be rebuilt.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Append an assistant entry → second NewEntry.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        // Cold prefix is empty for a fresh session; live has 2 entries → entries must be 2.
        assert_eq!(
            s.entries.len(),
            2,
            "entries must equal cold({}) + live(2) after two NewEntry events",
            s.live_base
        );
        // The last entry must be the assistant message we just appended.
        assert!(
            matches!(
                s.entries.last().unwrap().kind,
                SessionEntryKind::AssistantMessage { .. }
            ),
            "last entries element must be AssistantMessage, got {:?}",
            s.entries.last().unwrap().kind
        );
    });
}

/// Phase 2, Task 4: after two `NewEntry` events followed by an `EntryUpdated`
/// on the first entry, the first entry's `created_ms` must NOT change (no
/// restamp on in-place updates), and the second entry must still exist.
#[gpui::test]
async fn entry_updated_preserves_created_ms(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // First NewEntry: user message.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Second NewEntry: assistant message.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Capture the created_ms for entry 0 and verify both entries are present.
    let original_entry0_created_ms = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.entries.len(), 2, "two NewEntry events → two entries");
        assert!(
            s.entries[0].created_ms > 0,
            "entry 0 must have a real positive created_ms after NewEntry"
        );
        s.entries[0].created_ms
    });

    // EntryUpdated on entry 0 (the user message at local index 0 — emit directly).
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(0));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        // Both entries must still be present.
        assert_eq!(
            s.entries.len(),
            2,
            "EntryUpdated must not change entries count"
        );
        // Entry 0's created_ms must be unchanged (no restamp on update).
        assert_eq!(
            s.entries[0].created_ms, original_entry0_created_ms,
            "EntryUpdated must not restamp entry 0's created_ms"
        );
        // Entry 1 must still exist.
        assert!(
            s.entries[1].created_ms > 0,
            "entry 1 must retain its positive created_ms after update on entry 0"
        );
    });
}

/// Phase 3, Task 2: `mod_seq` is stamped on every live mutation.
/// After two `NewEntry` events, entry0.mod_seq==1 and entry1.mod_seq==2.
/// After `EntryUpdated(0)`, entry0.mod_seq==3 (advanced) and entry1.mod_seq==2
/// (unchanged). `entry0.created_ms` must be preserved across the update.
#[gpui::test]
async fn mod_seq_stamped_on_live_mutations(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // First NewEntry: user message.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Second NewEntry: assistant message.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // After two NewEntry events: entry0.mod_seq==1, entry1.mod_seq==2.
    let original_entry0_created_ms = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.entries.len(), 2, "two NewEntry events → two entries");
        // The first NewEntry flips Idle→Running through `mutate_state`, which now
        // consumes one `change_seq` for the `state_seq` watermark (decision 1:
        // entry mod_seq and section watermarks share one monotonic clock). So the
        // first entry stamps mod_seq 2 (seq 1 went to the state flip); the second
        // NewEntry finds the session already Running (no flip, no extra bump) and
        // stamps mod_seq 3.
        assert_eq!(
            s.entries[0].mod_seq, 2,
            "entry0.mod_seq must be 2 (seq 1 consumed by the Idle→Running state flip)"
        );
        assert_eq!(
            s.entries[1].mod_seq, 3,
            "entry1.mod_seq must be 3 after second NewEntry"
        );
        s.entries[0].created_ms
    });

    // EntryUpdated on entry 0 (local index 0).
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(0));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(
            s.entries.len(),
            2,
            "EntryUpdated must not change entries count"
        );
        assert_eq!(
            s.entries[0].mod_seq, 4,
            "entry0.mod_seq must advance to 4 after EntryUpdated(0)"
        );
        assert_eq!(
            s.entries[1].mod_seq, 3,
            "entry1.mod_seq must remain 3 (unchanged by EntryUpdated on entry0)"
        );
        assert_eq!(
            s.entries[0].created_ms, original_entry0_created_ms,
            "EntryUpdated must not restamp entry0.created_ms"
        );
    });
}

/// Fix 4: after cold-restore of a NON-EMPTY prefix, attaching a live thread
/// sets `live_base` to the prefix length. A subsequent `NewEntry` must land
/// at `entries[live_base]` (AFTER the cold prefix) and leave the cold prefix
/// entries unchanged.
#[gpui::test]
async fn new_entry_after_cold_prefix_lands_at_live_base(cx: &mut TestAppContext) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Inject 2 fake cold SessionEntries and re-attach the same acp_thread so
    // live_base is recomputed from entries.len() = 2.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, cx| {
                s.entries = vec![
                    SessionEntry {
                        created_ms: 1_700_000_000_000,
                        mod_seq: 0,
                        subagent_id: None,
                        kind: SessionEntryKind::UserMessage {
                            id: None,
                            content_md: "cold user".to_string(),
                            chunks: vec![],
                        },
                    },
                    SessionEntry {
                        created_ms: 1_700_000_001_000,
                        mod_seq: 0,
                        subagent_id: None,
                        kind: SessionEntryKind::AssistantMessage { chunks: vec![] },
                    },
                ];
                // Re-attach the same thread so live_base = entries.len() = 2.
                s.set_acp_thread(Some(acp_thread.clone()), cx);
            });
        });
    });
    cx.executor().run_until_parked();

    // Confirm live_base == 2 and is_cold() == false.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert_eq!(s.live_base, 2, "live_base must equal cold prefix length");
        assert!(!s.is_cold(), "session has a live thread");
    });

    // Fire a NewEntry via push_user_content_block.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("live msg".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        // Total entries = 2 cold + 1 live.
        assert_eq!(
            s.entries.len(),
            3,
            "entries must be cold(2) + live(1) = 3, got {}",
            s.entries.len()
        );
        // Cold prefix entries must be unchanged.
        assert_eq!(
            s.entries[0].created_ms, 1_700_000_000_000,
            "cold entry[0] created_ms must be unchanged"
        );
        assert_eq!(
            s.entries[1].created_ms, 1_700_000_001_000,
            "cold entry[1] created_ms must be unchanged"
        );
        // New entry must be at index live_base (= 2) with a real positive timestamp.
        assert!(
            s.entries[2].created_ms > 0,
            "live entry at entries[2] must have a positive created_ms, got {}",
            s.entries[2].created_ms
        );
        assert!(
            matches!(s.entries[2].kind, SessionEntryKind::UserMessage { .. }),
            "live entry at entries[2] must be UserMessage, got {:?}",
            s.entries[2].kind
        );
    });
}

/// Phase 5, Task 5.1: enqueuing a follow-up onto a Running session emits
/// `SessionQueueChanged` and must move `queue_seq` to the freshly-allocated
/// `change_seq` (and to a value > 0). Only the queue watermark advances — the
/// enqueue path emits a bare `SessionStateChanged` for the live Queued bubble
/// WITHOUT a `state_seq` bump (it does not route through `mark_state_changed`),
/// so the state watermark is intentionally left untouched here.
#[gpui::test]
async fn enqueue_bumps_queue_watermark(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            // Force Running so send_message_blocks takes the queueing branch
            // (the branch that emits SessionQueueChanged).
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            let queue_seq_before = session.read(cx).queue_seq;
            assert_eq!(queue_seq_before, 0, "fresh session starts with queue_seq 0");
            let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
                agent_client_protocol::schema::TextContent::new("follow-up".to_string()),
            )];
            store
                .send_message_blocks(session_id, blocks, cx)
                .detach_and_log_err(cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert_eq!(s.pending_messages.len(), 1, "message queued");
        assert!(s.queue_seq > 0, "queue_seq must advance off zero");
        assert_eq!(
            s.queue_seq, s.change_seq,
            "queue_seq must equal the freshly-allocated change_seq"
        );
    });
}

/// Phase 5, Task 5.1: registering a subagent tab emits `SessionSubagentsChanged`
/// and must move `subagents_seq` to the freshly-allocated `change_seq`.
#[gpui::test]
async fn subagent_spawn_bumps_subagents_watermark(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let subagents_seq_before = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .subagents_seq
    });
    assert_eq!(subagents_seq_before, 0, "fresh session starts at 0");

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_wm_1",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Worker"),
                    None,
                ),
                cx,
            )
            .expect("upsert task");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert_eq!(s.teammate_labels.len(), 1, "one teammate label captured");
        assert!(s.subagents_seq > 0, "subagents_seq must advance off zero");
        assert_eq!(
            s.subagents_seq, s.change_seq,
            "subagents_seq must equal the freshly-allocated change_seq"
        );
    });
}

/// The `→Idle` subagent-strip GC in `mutate_state` clears a stranded strip and
/// MUST move `subagents_seq` (and emit `SessionSubagentsChanged`), exactly like
/// an explicit removal — otherwise the mobile delta (and the desktop strip
/// view) never learns the strip emptied and a finished subagent tab strands.
#[gpui::test]
async fn idle_transition_gc_bumps_subagents_watermark(cx: &mut TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Strand a subagent pill: register an InProgress Task (non-terminal, so the
    // per-tool-call removal path never fires) and force the session Running.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_gc_1",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Worker"),
                    None,
                ),
                cx,
            )
            .expect("upsert task");
        });
    });
    cx.executor().run_until_parked();

    let subagents_seq_after_spawn = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
                // Seed a teammate-tagged entry so the demux produces a live
                // `Teammate` stream — since wire v5 the →Idle GC sources stranded
                // ids from `streams`, so a pill with no stream is not "stranded".
                s.entries = vec![SessionEntry {
                    created_ms: 1_700_000_000_000,
                    mod_seq: 1,
                    subagent_id: Some(SharedString::from("toolu_gc_1")),
                    kind: SessionEntryKind::AssistantMessage {
                        chunks: vec![AssistantChunk::Message("sub work".into())],
                    },
                }];
                s.rebuild_streams();
            });
            let seq = session.read(cx).subagents_seq;
            assert!(
                session.read(cx).teammate_labels.contains_key("toolu_gc_1"),
                "label captured"
            );
            assert!(
                session.read(cx).streams.contains_key(
                    &crate::stream::StreamId::Teammate(SharedString::from("toolu_gc_1"))
                ),
                "teammate stream stranded"
            );
            seq
        })
    });

    // Transition into Idle through `mutate_state` — the GC must fire.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.mutate_state(session_id, |state| *state = SessionState::Idle, cx);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert!(
            s.teammate_labels.is_empty()
                && !s.streams.contains_key(&crate::stream::StreamId::Teammate(
                    SharedString::from("toolu_gc_1")
                )),
            "→Idle GC must close the stranded stream + reclaim its label"
        );
        assert!(
            s.subagents_seq > subagents_seq_after_spawn,
            "the GC clear must advance subagents_seq ({} !> {})",
            s.subagents_seq,
            subagents_seq_after_spawn
        );
        assert_eq!(
            s.subagents_seq, s.change_seq,
            "subagents_seq must equal the freshly-allocated change_seq after GC"
        );
    });
}

/// Phase 6c regression: the `→Idle` subagent-strip GC sources stranded ids from
/// `session.streams` (the desktop snap-back `next_selection_after_change` also
/// watches `streams`), and MUST `close_stream` each stranded teammate — otherwise the stream
/// keeps re-demuxing Live from its still-tagged `entries` and the viewer strands on
/// a frozen, pill-less tab (the 14h-stuck-tab class this GC exists to prevent).
#[gpui::test]
async fn idle_transition_gc_closes_stranded_teammate_stream(cx: &mut TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Strand an inline-Task pill (non-terminal, so the per-tool-call removal never
    // fires) exactly like `idle_transition_gc_bumps_subagents_watermark`.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_gc_2",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Worker"),
                    None,
                ),
                cx,
            )
            .expect("upsert task");
        });
    });
    cx.executor().run_until_parked();

    let teammate = crate::stream::StreamId::Teammate(SharedString::from("toolu_gc_2"));
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
                // Seed a teammate-tagged entry so the demux produces a live
                // `Teammate` stream (the pill alone doesn't create one).
                s.entries = vec![SessionEntry {
                    created_ms: 1_700_000_000_000,
                    mod_seq: 1,
                    subagent_id: Some(SharedString::from("toolu_gc_2")),
                    kind: SessionEntryKind::AssistantMessage {
                        chunks: vec![AssistantChunk::Message("sub work".into())],
                    },
                }];
                s.rebuild_streams();
            });
            let s = session.read(cx);
            assert!(
                s.teammate_labels.contains_key("toolu_gc_2"),
                "label captured"
            );
            assert!(
                s.streams.contains_key(&teammate),
                "teammate stream must exist before the GC"
            );
        });
    });

    // Transition into Idle through `mutate_state` — the GC must fire.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.mutate_state(session_id, |state| *state = SessionState::Idle, cx);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert!(
            s.teammate_labels.is_empty(),
            "→Idle GC must reclaim the stranded label"
        );
        assert!(
            !s.streams.contains_key(&teammate),
            "→Idle GC must close the stranded teammate stream (still-tagged rows notwithstanding)"
        );
        assert!(
            s.streams.contains_key(&crate::stream::StreamId::Main),
            "Main stream survives the GC"
        );
    });
}

/// The →Idle GC must EXCLUDE a live async `Agent` teammate — its stream outlives
/// the parent turn (it closes on the real `stop_reason`, not on →Idle). Regression
/// guard for the `async_parents` exclusion set (`store.rs`, sourced from
/// `background_agents[*].parent_tool_use_id`): closing it here would suppress a
/// still-streaming async teammate (decision #5) and drop its label.
#[gpui::test]
async fn idle_transition_gc_excludes_live_async_agent_teammate(cx: &mut TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // An async `Agent` spawn tool-call reaches terminal (Completed) at spawn-ack
    // while the teammate keeps streaming — register it as an `Agent` (not `Task`)
    // so the terminal path leaves its stream open.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_async_gc",
                    "Agent",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Async worker"),
                    None,
                ),
                cx,
            )
            .expect("upsert agent");
        });
    });
    cx.executor().run_until_parked();

    let teammate = crate::stream::StreamId::Teammate(SharedString::from("toolu_async_gc"));
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
                // Live async teammate: a tagged entry (its demux stream) + a
                // `background_agents` registration whose `parent_tool_use_id` is
                // the teammate id — this is what marks it async-and-live for the
                // →Idle exclusion set.
                s.entries = vec![SessionEntry {
                    created_ms: 1_700_000_000_000,
                    mod_seq: 1,
                    subagent_id: Some(SharedString::from("toolu_async_gc")),
                    kind: SessionEntryKind::AssistantMessage {
                        chunks: vec![AssistantChunk::Message("async work in flight".into())],
                    },
                }];
                let bg_id = crate::background_agent::BackgroundAgentId::new("bg_async_gc");
                s.background_agents.insert(
                    bg_id.clone(),
                    crate::background_agent::BackgroundAgent {
                        id: bg_id.clone(),
                        jsonl_path: "/nonexistent".into(),
                        registered_at: chrono::Utc::now(),
                        // No terminal stop_reason → still running.
                        latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                            mtime: std::time::SystemTime::now(),
                            activity_label: SharedString::from("Bash: cargo test"),
                            stop_reason: None,
                        }),
                        last_offset: 0,
                        parent_tool_use_id: Some(SharedString::from("toolu_async_gc")),
                        latest_seq: 0,
                        killed: false,
                    },
                );
                s.background_agent_order.push(bg_id);
                s.rebuild_streams();
            });
            let s = session.read(cx);
            assert!(
                s.streams.contains_key(&teammate),
                "async teammate stream must exist before the GC"
            );
            assert!(
                s.teammate_labels.contains_key("toolu_async_gc"),
                "async teammate label captured at registration"
            );
        });
    });

    // →Idle: the GC fires, but the async teammate is excluded.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.mutate_state(session_id, |state| *state = SessionState::Idle, cx);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert!(
            s.streams.contains_key(&teammate),
            "→Idle GC must NOT close a live async agent's teammate stream"
        );
        assert!(
            s.teammate_labels.contains_key("toolu_async_gc"),
            "→Idle GC must NOT drop a live async agent's label"
        );
    });
}

/// The per-source-streams migration's stream-lifecycle (the →Idle strip GC +
/// `close_stream` + `rebuild_streams`) must NOT touch `last_activity_at` — it is
/// the supervisor's silence clock (`quiet_since_ms`), and a spurious bump on the
/// →Idle transition would push the judge's deadline forward every idle tick and
/// starve `should_fire` forever ("supervisor never fires" regression guard). The
/// GC runs INSIDE `mutate_state(Idle)`, which must leave the clock alone; only
/// the separate turn-completion path (a distinct statement) legitimately bumps it.
#[gpui::test]
async fn idle_transition_gc_does_not_bump_last_activity_at(cx: &mut TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // A stranded inline-Task teammate (non-terminal, so the per-tool-call removal
    // never fires) — exactly the input the →Idle GC closes.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_gc_clock",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Worker"),
                    None,
                ),
                cx,
            )
            .expect("upsert task");
        });
    });
    cx.executor().run_until_parked();

    // Pin the silence clock to a fixed instant in the past so any bump is visible.
    let t0 = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(1_700_000_000_000)
        .expect("valid timestamp");
    let teammate = crate::stream::StreamId::Teammate(SharedString::from("toolu_gc_clock"));
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
                s.entries = vec![SessionEntry {
                    created_ms: 1_700_000_000_000,
                    mod_seq: 1,
                    subagent_id: Some(SharedString::from("toolu_gc_clock")),
                    kind: SessionEntryKind::AssistantMessage {
                        chunks: vec![AssistantChunk::Message("sub work".into())],
                    },
                }];
                s.rebuild_streams();
                s.last_activity_at = t0;
            });
            assert!(
                session.read(cx).streams.contains_key(&teammate),
                "teammate stream must exist before the GC"
            );
        });
    });

    // →Idle: the GC (close_stream over stranded teammates + rebuild_streams) runs
    // inside this call.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.mutate_state(session_id, |state| *state = SessionState::Idle, cx);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert!(
            !s.streams.contains_key(&teammate),
            "→Idle GC must have closed the stranded teammate stream"
        );
        assert_eq!(
            s.last_activity_at, t0,
            "→Idle GC / close_stream / rebuild_streams must NOT bump last_activity_at \
             (it is the supervisor's silence clock — a bump here starves should_fire)"
        );
    });
}

/// Phase 5, Task 5.1: a discriminant-changing transition through `mutate_state`
/// (Idle → Running) emits `SessionStateChanged` and must move `state_seq` to the
/// freshly-allocated `change_seq`.
#[gpui::test]
async fn mutate_state_bumps_state_watermark(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    // Pin to Idle so the Idle → Running flip is a real discriminant change.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| s.state = SessionState::Idle);
            let state_seq_before = session.read(cx).state_seq;
            store.mutate_state(
                session_id,
                |state| {
                    *state = SessionState::Running {
                        started_at: std::time::Instant::now(),
                        notified: false,
                    };
                },
                cx,
            );
            let s = session.read(cx);
            assert!(
                s.state_seq > state_seq_before,
                "state_seq must advance on a discriminant-changing transition"
            );
            assert_eq!(
                s.state_seq, s.change_seq,
                "state_seq must equal the freshly-allocated change_seq"
            );
        });
    });
}

/// Regression (Chat Supervisor port, C1/I1): a state change on an
/// `is_supervisor_ephemeral` session (the hidden judge/auditor) must NOT emit a
/// sequenced `workspace.session_state_changed` notification — that event is
/// forwarded to mobile by `remote_control`'s allow-list, so an unfiltered emit
/// would leak the invisible judge's Idle→Running→Idle churn on every supervisor
/// wake-up. Asserted via the coordinator's seq counter: `emit_sequenced` advances
/// it, the suppressed path does not. A non-ephemeral control session in the same
/// test confirms the guard is specific to the ephemeral flag (and that internal
/// bookkeeping — `state_seq` — still advances for the judge).
#[gpui::test]
async fn ephemeral_session_state_change_does_not_emit_workspace_event(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(editor_mcp::workspace_seq::install);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            // Mark the session as the hidden ephemeral supervisor judge.
            session.update(cx, |s, _| {
                s.is_supervisor_ephemeral = true;
                s.state = SessionState::Idle;
            });

            let coord = editor_mcp::workspace_seq::WorkspaceEventCoordinator::global(cx);
            let seq_before = coord.current_seq();
            let state_seq_before = session.read(cx).state_seq;

            // Idle → Running is a real discriminant change, so the wire emit
            // would fire for a visible session.
            store.mutate_state(
                session_id,
                |state| {
                    *state = SessionState::Running {
                        started_at: std::time::Instant::now(),
                        notified: false,
                    };
                },
                cx,
            );

            let coord = editor_mcp::workspace_seq::WorkspaceEventCoordinator::global(cx);
            assert_eq!(
                coord.current_seq(),
                seq_before,
                "ephemeral judge state change must NOT advance the workspace seq \
                 (no workspace.session_state_changed emit leaks to mobile)"
            );
            // Internal bookkeeping must still run: the judge's own state_seq
            // advances so `message_generator`'s Idle/Errored await still resolves.
            assert!(
                session.read(cx).state_seq > state_seq_before,
                "internal state_seq must still advance for the ephemeral session"
            );
        });
    });

    // Control: a NON-ephemeral session in the same store DOES emit, proving the
    // guard keys on `is_supervisor_ephemeral` and not on some unrelated path.
    let (visible_id, _thread2, _tmp2) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(visible_id).expect("session");
            session.update(cx, |s, _| s.state = SessionState::Idle);
            let coord = editor_mcp::workspace_seq::WorkspaceEventCoordinator::global(cx);
            let seq_before = coord.current_seq();
            store.mutate_state(
                visible_id,
                |state| {
                    *state = SessionState::Running {
                        started_at: std::time::Instant::now(),
                        notified: false,
                    };
                },
                cx,
            );
            let coord = editor_mcp::workspace_seq::WorkspaceEventCoordinator::global(cx);
            assert!(
                coord.current_seq() > seq_before,
                "a visible session's state change MUST advance the workspace seq"
            );
        });
    });
}

/// Phase 3, Task 4: `reset_context` (/clear) must increment `epoch` by exactly 1,
/// because it replaces the transcript wholesale. A plain `NewEntry` must NOT bump
/// `epoch` (only `change_seq` / `mod_seq` advance on live appends).
#[gpui::test]
async fn reset_context_bumps_epoch(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Record the epoch before any reset.
    let epoch_before = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .epoch
    });

    // Fire a plain NewEntry — must NOT bump epoch.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    let epoch_after_entry = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .epoch
    });
    assert_eq!(
        epoch_after_entry, epoch_before,
        "a plain NewEntry must not bump epoch (got {} → {})",
        epoch_before, epoch_after_entry
    );

    // Now call reset_context — epoch must advance by exactly 1.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");

    let epoch_after_reset = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .epoch
    });
    assert_eq!(
        epoch_after_reset,
        epoch_before + 1,
        "reset_context must bump epoch by exactly 1 (was {}, got {})",
        epoch_before,
        epoch_after_reset
    );
}

/// Phase 5, Task 5.1: `/clear` (`reset_context`) with a non-empty queue must bump
/// `epoch` AND move the queue watermark, because the queued bundle is dropped —
/// a paired mobile holding the stale Queued bubble needs the section re-sent
/// (now empty). Mirrors `reset_context_bumps_epoch` but seeds a pending message.
#[gpui::test]
async fn reset_context_with_queue_bumps_epoch_and_queue_watermark(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    // Queue a follow-up onto a Running session so reset_context has pending
    // bundles to drop (the `had_pending` branch that moves the queue watermark).
    let (epoch_before, queue_seq_before) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
                agent_client_protocol::schema::TextContent::new("queued".to_string()),
            )];
            store
                .send_message_blocks(session_id, blocks, cx)
                .detach_and_log_err(cx);
            let s = session.read(cx);
            assert_eq!(
                s.pending_messages.len(),
                1,
                "one bundle queued before /clear"
            );
            (s.epoch, s.queue_seq)
        })
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert_eq!(
            s.epoch,
            epoch_before + 1,
            "reset_context must bump epoch by exactly 1"
        );
        assert!(
            s.pending_messages.is_empty(),
            "reset_context drops the queued bundle"
        );
        assert!(
            s.queue_seq > queue_seq_before,
            "reset_context must move the queue watermark when it drops a pending bundle \
             (was {queue_seq_before}, got {})",
            s.queue_seq
        );
    });
}

/// Phase 4 Task 3 (follow-up): `/clear` (reset_context) must not only wipe
/// in-memory entries and bump the in-memory epoch — it must also flush that
/// state to the DB so a cold-load after a /clear doesn't replay stale rows.
///
/// Uses the `reset_context` path (not the persist_all_rows fallback) because
/// the method is public and the test harness supports a live AcpThread session.
/// After the call, `db.load_entries` must be empty and `db.load_epoch` must be
/// strictly greater than the epoch that was written before the reset.
#[gpui::test]
async fn transcript_clear_resets_stale_rows_and_bumps_epoch(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    // Append two entries so the DB has rows to clear.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Sanity check: two rows must be persisted before the reset.
    let rows_before = db
        .load_entries(session_id)
        .await
        .expect("load entries before");
    assert_eq!(
        rows_before.len(),
        2,
        "two appends → two persisted rows before clear"
    );

    // Capture the epoch as written to the DB before the reset.
    let epoch_in_db_before = db
        .load_epoch(session_id)
        .await
        .expect("load epoch before")
        .unwrap_or(0);

    // Call reset_context — this clears entries, bumps in-memory epoch, then
    // calls persist_all_rows which deletes all rows and saves the new epoch.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");

    cx.executor().run_until_parked();

    // DB rows must be gone after the clear.
    let rows_after = db
        .load_entries(session_id)
        .await
        .expect("load entries after");
    assert_eq!(
        rows_after.len(),
        0,
        "reset_context must delete all persisted entry rows"
    );

    // DB epoch must have advanced so a cold-load sees the new epoch, not the
    // stale pre-clear one.
    let epoch_in_db_after = db
        .load_epoch(session_id)
        .await
        .expect("load epoch after")
        .unwrap_or(0);
    assert!(
        epoch_in_db_after > epoch_in_db_before,
        "reset_context must persist the bumped epoch to the DB (was {}, got {})",
        epoch_in_db_before,
        epoch_in_db_after
    );
}

#[gpui::test]
async fn quota_error_stops_immediately(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.supervisor_states.get_mut(&id).unwrap().status =
            crate::supervisor::SupervisorStatus::Judging;
        store.on_judge_failed(id, "Error: usage limit reached".into(), cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert!(!st.enabled);
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Stopped(crate::supervisor::StoppedReason::Quota)
    );
    // Quota never schedules a retry.
    assert_eq!(st.next_eligible_ms, None);
}

#[gpui::test]
async fn transient_error_advances_backoff_then_gives_up(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));
    // The spec requires all 8 delays to be waited: 1,1,2,3,5,10,30,60 minutes.
    // Failures 1..=8 must each leave status Watching with a retry gate; the 9th
    // failure exhausts the schedule and gives up (Stopped(ProviderError)).
    let mut prev_next_eligible: Option<i64> = None;
    for expected in 1..=9u32 {
        store.update(cx, |store, cx| {
            store.supervisor_states.get_mut(&id).unwrap().status =
                crate::supervisor::SupervisorStatus::Judging;
            store.on_judge_failed(id, "overloaded_error".into(), cx);
        });
        let st = store
            .read_with(cx, |store, _| store.supervisor_state(id))
            .unwrap();
        if expected <= 8 {
            assert_eq!(st.backoff_attempt, expected);
            assert_eq!(st.status, crate::supervisor::SupervisorStatus::Watching);
            // Each transient retry arms an eligibility gate in the future.
            let next = st.next_eligible_ms;
            assert!(
                next.is_some(),
                "attempt {expected} must schedule a retry window"
            );
            // Each successive delay must be >= the previous one (schedule is non-decreasing).
            if let (Some(prev), Some(cur)) = (prev_next_eligible, next) {
                assert!(
                    cur >= prev,
                    "attempt {expected}: next_eligible_ms should not decrease"
                );
            }
            // The 8th failure must schedule the 60-minute delay (last schedule entry).
            if expected == 8 {
                // 60 min = 3_600_000 ms; prev entry was 30 min = 1_800_000 ms.
                // Verify the 8th delay is strictly larger than the 7th.
                if let (Some(prev), Some(cur)) = (prev_next_eligible, next) {
                    assert!(
                        cur > prev,
                        "attempt 8 (60-min delay) must set a later gate than attempt 7 (30-min)"
                    );
                }
            }
            prev_next_eligible = next;
        }
    }
    // After the 9th transient failure the schedule is exhausted.
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert!(!st.enabled);
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Stopped(
            crate::supervisor::StoppedReason::ProviderError
        )
    );
    // Giving up must clear the gate (no dangling retry).
    assert_eq!(st.next_eligible_ms, None);
}

/// Helper: does the session's live thread contain a UserMessage carrying the
/// reconnect continuation prompt?
fn thread_has_continuation(
    store: &SolutionAgentStore,
    session_id: SolutionSessionId,
    cx: &gpui::App,
) -> bool {
    let Some(thread) = store
        .session(session_id)
        .and_then(|s| s.read(cx).acp_thread().cloned())
    else {
        return false;
    };
    thread.read(cx).entries().iter().any(|e| {
        if let acp_thread::AgentThreadEntry::UserMessage(msg) = e {
            let text: String = msg
                .chunks
                .iter()
                .filter_map(|b| match b {
                    acp::ContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect();
            text.contains("продолжай работу с того места")
        } else {
            false
        }
    })
}

/// Helper: does any UserMessage in the session's live thread contain `needle`?
fn last_user_text_contains(
    store: &SolutionAgentStore,
    session_id: SolutionSessionId,
    needle: &str,
    cx: &gpui::App,
) -> bool {
    let Some(thread) = store
        .session(session_id)
        .and_then(|s| s.read(cx).acp_thread().cloned())
    else {
        return false;
    };
    thread.read(cx).entries().iter().any(|e| {
        if let acp_thread::AgentThreadEntry::UserMessage(msg) = e {
            let text: String = msg
                .chunks
                .iter()
                .filter_map(|b| match b {
                    acp::ContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect();
            text.contains(needle)
        } else {
            false
        }
    })
}

#[gpui::test]
async fn reconnect_continues_a_wedged_running_session(cx: &mut TestAppContext) {
    // #6: the watchdog only reconnects sessions wedged mid-turn, so once the
    // session is back the agent must be re-engaged with a continuation prompt
    // rather than parking at Idle. (Driven through the post-resume hook
    // directly: the mock backend can't load/resume a session, so we exercise
    // `maybe_send_reconnect_continuation` — the real send + gate — against a
    // live thread, the state the genuine resume path lands in.)
    //
    // COVERAGE BOUNDARY: this verifies the gate (`was_running` -> send) and the
    // actual prompt send, NOT the `reconnect_agent` call site or its
    // `was_running` capture — those need a resume-capable backend the mock
    // lacks. If `reconnect_agent` were changed to always pass `was_running:
    // false`, this test would still pass; that wiring is currently unguarded by
    // a test (a MockConnection load/resume impl would close the gap).
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.maybe_send_reconnect_continuation(
                session_id,
                /* was_running */ true,
                /* tail_unanswered_user */ false,
                cx,
            )
        });
    });
    cx.executor().run_until_parked();

    let continued = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, cx| {
            thread_has_continuation(store, session_id, cx)
        })
    });
    assert!(
        continued,
        "a continuation prompt must be sent after reconnecting a wedged (Running) session",
    );
}

#[gpui::test]
async fn reconnect_idle_session_sends_no_continuation(cx: &mut TestAppContext) {
    // The gate: a reconnect of an already-idle session (e.g. a manual MCP
    // reconnect) must NOT inject a spurious "carry on" prompt — there was no
    // in-flight work to continue.
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.maybe_send_reconnect_continuation(
                session_id,
                /* was_running */ false,
                /* tail_unanswered_user */ false,
                cx,
            )
        });
    });
    cx.executor().run_until_parked();

    let continued = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, cx| {
            thread_has_continuation(store, session_id, cx)
        })
    });
    assert!(
        !continued,
        "no continuation prompt for a reconnect of an already-idle session",
    );
}

/// MEDIUM-hardening #4: a reconnect whose `resume_session` never completes
/// (dead subprocess / unsupported backend) must NOT strand the session at the
/// transient `Errored("reconnecting…")` forever. After one bounded retry it
/// surfaces the failure and lands a CLEAR terminal error the user can act on.
/// The mock backend can't load/resume, so `resume_session` fails fast twice —
/// exercising the retry-then-notify path without waiting on the timeout.
#[gpui::test]
async fn reconnect_resume_failure_surfaces_after_retry(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    // Look wedged mid-turn so `reconnect_agent` runs its normal path.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        });
    });

    let task = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reconnect_agent(session_id, cx))
    });
    let result = task.await;
    cx.executor().run_until_parked();

    assert!(
        result.is_err(),
        "a reconnect whose resume can't complete must report failure"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, cx| {
            let state = store.session(session_id).unwrap().read(cx).state.clone();
            match state {
                SessionState::Errored(msg) => {
                    assert!(
                        msg.contains("перезапустите"),
                        "terminal error must carry actionable guidance (not stuck at \
                         reconnecting…), got {msg:?}"
                    );
                    assert!(
                        !msg.contains("reconnecting"),
                        "must not be left in the transient reconnecting state, got {msg:?}"
                    );
                }
                other => panic!("expected Errored(actionable), got {other:?}"),
            }
        });
    });
}

/// The regression this session shipped: a wedge that happened on an UNANSWERED
/// user message must re-engage the fresh subprocess with the "you didn't answer
/// the user's last message" prompt, NOT the generic "carry on" — otherwise the
/// replayed human message is treated as already-handled and silently dropped.
#[gpui::test]
async fn reconnect_on_unanswered_user_message_points_at_it(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.maybe_send_reconnect_continuation(
                session_id,
                /* was_running */ true,
                /* tail_unanswered_user */ true,
                cx,
            )
        });
    });
    cx.executor().run_until_parked();

    let (has_unanswered_prompt, has_generic) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, cx| {
            let unanswered = last_user_text_contains(store, session_id, "НЕ считай его уже", cx);
            let generic = last_user_text_contains(store, session_id, "продолжай работу с того места", cx);
            (unanswered, generic)
        })
    });
    assert!(
        has_unanswered_prompt,
        "an unanswered-user wedge must send the 'you didn't answer the user' continuation",
    );
    assert!(
        !has_generic,
        "the generic 'carry on' prompt must NOT be used when the tail is an unanswered user message",
    );
}

#[test]
fn classify_done_reasoning_park_vs_completion() {
    use crate::store::classify_done_reasoning;
    // Genuine completion: not a park, body unchanged.
    assert_eq!(
        classify_done_reasoning("Shipped everything, all tests green."),
        (false, "Shipped everything, all tests green."),
    );
    // Park: flagged, and the `PARK:` token is stripped from the body.
    assert_eq!(
        classify_done_reasoning("PARK: blocked on the operator's branch decision"),
        (true, "blocked on the operator's branch decision"),
    );
    // Leading whitespace before the token is tolerated.
    assert_eq!(
        classify_done_reasoning("   PARK: awaiting go-ahead"),
        (true, "awaiting go-ahead"),
    );
    // The token must be a PREFIX — a mid-sentence "PARK" (even with a colon) is
    // a completion, body untouched.
    assert_eq!(
        classify_done_reasoning("Completed the PARK: feature end to end."),
        (false, "Completed the PARK: feature end to end."),
    );
}

#[test]
fn tail_unanswered_user_detection() {
    use crate::session_entry::{
        AssistantChunk, SessionEntry, SessionEntryKind, SystemEntryLevel,
    };
    use agent_client_protocol::schema as acp;
    let ent = |kind| SessionEntry {
        created_ms: 0,
        mod_seq: 0,
        subagent_id: None,
        kind,
    };
    let user = |chunks: Vec<acp::ContentBlock>| SessionEntryKind::UserMessage {
        id: None,
        content_md: "hi".into(),
        chunks,
    };
    let plain_user = || user(vec![]);
    let nudge_user = || {
        user(vec![acp::ContentBlock::Text(
            acp::TextContent::new("nudge").meta(Some(acp_thread::meta_with_observer_nudge())),
        )])
    };
    let recovery_user = || {
        user(vec![acp::ContentBlock::Text(
            acp::TextContent::new("your process hung, continue")
                .meta(Some(acp_thread::meta_with_editor_recovery())),
        )])
    };
    let system = || SessionEntryKind::System {
        level: SystemEntryLevel::Info,
        text_md: "note".into(),
    };
    let assistant = || SessionEntryKind::AssistantMessage {
        chunks: vec![AssistantChunk::Message("ok".into())],
    };

    use crate::store::tail_is_unanswered_user_message as tail;
    assert!(!tail(&[]), "empty transcript is not an unanswered-user tail");
    assert!(tail(&[ent(plain_user())]), "bare trailing user message");
    assert!(
        tail(&[ent(plain_user()), ent(system())]),
        "a trailing System note is skipped over",
    );
    assert!(
        !tail(&[ent(plain_user()), ent(assistant())]),
        "an assistant reply after the user message = answered",
    );
    assert!(
        !tail(&[ent(nudge_user())]),
        "an observer nudge is the supervisor's voice, not the human's",
    );
    assert!(
        !tail(&[ent(recovery_user())]),
        "an editor reconnect-recovery prompt is not an unanswered human message \
         (else a second consecutive hang points recovery at the editor's own prompt)",
    );
    assert!(
        !tail(&[ent(plain_user()), ent(nudge_user())]),
        "a nudge after the human message means the tail is not a bare unanswered human message",
    );
}

/// Regression (ghost console tabs): internal one-shot AI helpers
/// (`message_generator::run_ephemeral_task`) go through
/// `create_ephemeral_session`, which must NOT pin the session into the tab
/// strip and must emit NO `SessionCreated` / `TabsChanged { opened }` — its
/// lifetime is so brief that the console panel's async tab-add could lose the
/// race to the synchronous tab-remove and strand an orphaned tab. A normal
/// `create_session` DOES pin and DOES emit; this test asserts the contrast.
#[gpui::test]
async fn ephemeral_session_is_not_pinned_and_emits_no_session_created(
    cx: &mut TestAppContext,
) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
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

    let created = Rc::new(std::cell::RefCell::new(
        Vec::<crate::model::SolutionSessionId>::new(),
    ));
    let opened = Rc::new(std::cell::RefCell::new(
        Vec::<crate::model::SolutionSessionId>::new(),
    ));
    let closed = Rc::new(std::cell::RefCell::new(
        Vec::<crate::model::SolutionSessionId>::new(),
    ));
    let _subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let created = created.clone();
        let opened = opened.clone();
        let closed = closed.clone();
        cx.subscribe(&store, move |_store, event, _cx| match event {
            SolutionAgentStoreEvent::SessionCreated { id, .. } => {
                created.borrow_mut().push(*id);
            }
            SolutionAgentStoreEvent::TabsChanged {
                opened: opened_ids, ..
            } => {
                opened.borrow_mut().extend(opened_ids.iter().copied());
            }
            SolutionAgentStoreEvent::SessionClosed(id) => {
                closed.borrow_mut().push(*id);
            }
            _ => {}
        })
    });

    // Normal top-level session: pinned + SessionCreated + TabsChanged{opened}.
    let normal_id = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session(solution_id, agent_id.clone(), project.clone(), cx)
            })
        })
        .await
        .expect("create_session");
    cx.executor().run_until_parked();

    // Ephemeral one-shot session: NOT pinned, no SessionCreated.
    let ephemeral_id = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_ephemeral_session(
                    solution_id,
                    agent_id.clone(),
                    project.clone(),
                    cx,
                )
            })
        })
        .await
        .expect("create_ephemeral_session");
    cx.executor().run_until_parked();

    let (normal_tab_order, ephemeral_tab_order) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let store = store.read(cx);
        let normal = store
            .session(normal_id)
            .expect("normal session exists")
            .read(cx)
            .tab_order;
        let ephemeral = store
            .session(ephemeral_id)
            .expect("ephemeral session exists")
            .read(cx)
            .tab_order;
        (normal, ephemeral)
    });
    assert!(
        normal_tab_order.is_some(),
        "a normal top-level session must be pinned into the tab strip",
    );
    assert_eq!(
        ephemeral_tab_order, None,
        "an ephemeral one-shot session must never be pinned",
    );

    let created = created.borrow().clone();
    let opened = opened.borrow().clone();
    assert_eq!(
        created,
        vec![normal_id],
        "only the normal session may emit SessionCreated",
    );
    assert!(
        !created.contains(&ephemeral_id),
        "ephemeral session must emit no SessionCreated",
    );
    assert!(
        opened.contains(&normal_id),
        "normal session must appear in TabsChanged{{opened}}",
    );
    assert!(
        !opened.contains(&ephemeral_id),
        "ephemeral session must not appear in TabsChanged{{opened}}",
    );

    // Close side: closing a normal session emits `SessionClosed` (which
    // `finalize_session_teardown` also mirrors to `workspace.session_deleted`
    // via the same `was_ephemeral` gate); closing an ephemeral one-shot must
    // emit NOTHING, so a wire client never sees a close for a session it was
    // never told was created. Asserting the store `SessionClosed` event is the
    // testable proxy for the wire suppression — both share the `was_ephemeral`
    // gate, and the `WorkspaceEventCoordinator` isn't installed in this test.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.close_session(normal_id, cx).expect("close normal");
            store
                .close_session(ephemeral_id, cx)
                .expect("close ephemeral");
        });
    });
    cx.executor().run_until_parked();

    let closed = closed.borrow().clone();
    assert_eq!(
        closed,
        vec![normal_id],
        "only the normal session may emit SessionClosed; the ephemeral one is suppressed",
    );
    assert!(
        !closed.contains(&ephemeral_id),
        "ephemeral session must emit no SessionClosed",
    );
}

/// The project label is a stored fact (`member_id`), not a cwd comparison.
/// A session whose cwd has drifted from the member's `local_path` — exactly
/// what a folder rename produces — must still show its project.
#[gpui::test]
async fn project_label_reads_member_id_not_cwd(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = cx.update(|cx| solutions::SolutionStore::for_test(dir.path().join("s.json"), cx));
    cx.update(|cx| solutions::install_global_for_test(store.clone(), cx));

    let solution_id = store
        .update(cx, |s, cx| {
            s.create_solution("Sol", dir.path().to_path_buf(), cx)
        })
        .expect("create solution");
    let member_id = store.update(cx, |s, _| {
        s.test_add_member_with_path(solution_id, "sawe", dir.path().join("sol/sawe"))
    });
    let solution = store.read_with(cx, |s, _| {
        s.find_solution(solution_id).expect("solution").clone()
    });

    cx.update(|cx| {
        assert_eq!(
            crate::store::project_label(&solution, Some(member_id), cx).as_deref(),
            Some("sawe"),
            "the label comes from the member row"
        );
        assert_eq!(
            crate::store::project_label(&solution, None, cx),
            None,
            "no member = the solution root, which callers render as ROOT"
        );
        assert_eq!(
            crate::store::project_label(&solution, Some(solutions::MemberId(9999)), cx),
            None,
            "a dangling member_id degrades to ROOT rather than panicking"
        );
    });
}
