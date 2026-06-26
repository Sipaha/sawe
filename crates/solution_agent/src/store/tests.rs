use super::*;
use crate::adapter::AdapterRegistry;
use crate::model::SessionState;
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
            solution_id.clone(),
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

#[gpui::test]
fn close_session_removes_from_indices(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let id = SolutionSessionId::new();
            let entity = cx.new(|_| {
                let mut s = SolutionSession::new_idle(
                    id,
                    SolutionId("sol-a".into()),
                    SharedString::from("claude-acp"),
                    agent_client_protocol::schema::SessionId::new("acp-1"),
                );
                s.title = SharedString::from("test");
                s
            });
            store.sessions.insert(id, entity);
            store
                .by_solution
                .entry(SolutionId("sol-a".into()))
                .or_default()
                .push(id);

            assert_eq!(store.sessions_for(&SolutionId("sol-a".into())).len(), 1);
            store.close_session(id, cx).expect("close_session");
            assert_eq!(store.sessions_for(&SolutionId("sol-a".into())).len(), 0);
            assert!(store.session(id).is_none());
        });
    });
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

#[gpui::test]
async fn pool_release_arms_60s_shutdown_then_drops(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let key = (SolutionId("sol-a".into()), SharedString::from("mock-agent"));

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
    let key = (SolutionId("sol-a".into()), SharedString::from("mock-agent"));

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
                store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
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
            store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
        })
    });
    let task2 = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
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
                store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
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
                store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
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
                store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
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
    assert!(text.contains("hello"), "delivers user content, got {text:?}");
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

#[test]
fn subagent_view_queue_target_only_background_is_a_subagent() {
    use crate::background_agent::BackgroundAgentId;
    use crate::background_shell::BackgroundShellId;
    use crate::model::QueueTarget;
    use crate::store::SubagentView;
    assert_eq!(SubagentView::Main.queue_target(), QueueTarget::Main);
    assert_eq!(
        SubagentView::Task(SharedString::from("toolu_1")).queue_target(),
        QueueTarget::Main
    );
    assert_eq!(
        SubagentView::Shell(BackgroundShellId::new("sh-1")).queue_target(),
        QueueTarget::Main
    );
    assert_eq!(
        SubagentView::Background(BackgroundAgentId::new("agent-1")).queue_target(),
        QueueTarget::Subagent(SharedString::from("agent-1"))
    );
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
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store.session(session_id).unwrap().read(cx).pending_messages.len(),
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
    assert!(!sub.contains("for main"), "must not leak the Main bundle, got {sub:?}");
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store.session(session_id).unwrap().read(cx).pending_messages.len(),
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
                store.session(session_id).unwrap().read(cx).pending_messages.len(),
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
                store.session(session_id).unwrap().read(cx).pending_messages.len(),
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
                store.session(session_id).unwrap().read(cx).pending_messages.len(),
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
                        acp::ContentBlock::Text(acp::TextContent::new(
                            "look at this".to_string(),
                        )),
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
    assert_eq!(written, b"data", "inbox file must hold the decoded image bytes");
    let _ = std::fs::remove_file(&path);

    // Queue is drained — nothing left for the idle-flush to re-send.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert!(
                store.session(session_id).unwrap().read(cx).pending_messages.is_empty(),
                "image bundle must be drained after mid-turn delivery"
            );
        });
    });
}

#[test]
fn stale_archive_dirs_gates_on_count_then_age() {
    let now = Utc::now();
    let root = std::path::Path::new("/sol/root");
    let make = |n: usize, days_ago: i64| crate::model::SolutionSessionMetadata {
        id: crate::model::SolutionSessionId::new(),
        solution_id: SolutionId("sol".into()),
        agent_id: SharedString::from("claude-acp"),
        acp_session_id: agent_client_protocol::schema::SessionId::new(format!("acp-{n}")),
        title: SharedString::from("s"),
        created_at: now,
        last_activity_at: now - chrono::Duration::days(days_ago),
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
    };

    // <= the min-session gate: keep everything, even ancient archives.
    let small: Vec<_> = (0..ARCHIVE_REAP_MIN_SESSIONS).map(|n| make(n, 999)).collect();
    assert!(
        stale_archive_dirs(root, &small, now).is_empty(),
        "small workspaces keep their full history"
    );

    // Over the gate: reap only the sessions inactive past the age cutoff.
    let recent: Vec<_> = (0..8).map(|n| make(n, 1)).collect();
    let stale: Vec<_> = (8..14)
        .map(|n| make(n, ARCHIVE_REAP_MAX_AGE_DAYS + 5))
        .collect();
    let mut metas = recent.clone();
    metas.extend(stale.iter().cloned());

    let reaped = stale_archive_dirs(root, &metas, now);
    assert_eq!(reaped.len(), stale.len(), "only the stale sessions are reaped");
    for m in &stale {
        assert!(
            reaped.contains(&root.join(".agents").join(m.id.to_string())),
            "stale session {} must be reaped",
            m.id
        );
    }
    for m in &recent {
        assert!(
            !reaped.contains(&root.join(".agents").join(m.id.to_string())),
            "recently-active session {} must be kept",
            m.id
        );
    }
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
async fn reset_context_clears_cold_entries(cx: &mut TestAppContext) {
    use crate::cold_persistence::{PersistedAssistantChunk, PersistedAssistantMessage, PersistedEntryV2};
    let (session_id, _old_thread, _tmp) = create_session_with_thread(cx).await;

    // Stamp a fake cold prefix so we can prove `reset_context` clears it.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, cx| {
                s.cold_persisted_v2.push(PersistedEntryV2::Assistant(PersistedAssistantMessage {
                    chunks: vec![PersistedAssistantChunk::Message("cold msg".into())],
                }));
                cx.notify();
            });
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
            let s = session.read(cx);
            assert!(
                s.cold_persisted_v2.is_empty(),
                "reset_context should drop cold_persisted_v2 (was {:?})",
                s.cold_persisted_v2.len()
            );
            assert!(s.entries.is_empty(), "reset_context should drop entries");
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
                store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
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

#[gpui::test]
async fn restore_open_tabs_hydrates_cold_sessions(cx: &mut TestAppContext) {
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.set_persistence(db.clone());
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let id_b = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = Utc::now();

    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("session A"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
    };
    let meta_b = crate::model::SolutionSessionMetadata {
        id: id_b,
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-b"),
        title: SharedString::from("session B"),
        ..meta_a.clone()
    };
    db.save_metadata(meta_a).await.expect("meta a");
    db.save_metadata(meta_b).await.expect("meta b");

    let blob_a = serde_json::to_vec(&PersistedSession {
        title: "session A".into(),
        entries: vec![PersistedEntry {
            role: PersistedRole::User,
            markdown: "first prompt".into(),
        }],
        entry_summaries: vec!["first prompt".into()],
        entries_v2: vec![],
        entry_created_ms: vec![],
        available_models: vec![],
        desired_model: None,
        desired_effort: None,
    })
    .unwrap();
    db.save_blob(id_a, blob_a).await.expect("blob a");

    db.update_tab_orders(solution_id.clone(), vec![id_b, id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("restore");
    assert_eq!(ordered, vec![id_b, id_a]);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session A restored");
            let sb = store.session(id_b).expect("session B restored");
            sa.read_with(cx, |s, _| {
                assert!(s.is_cold(), "restored session should be cold");
                assert_eq!(s.entries.len(), 1);
                // v1 blobs hydrate as Assistant-shaped legacy rows
                // (the old `role` field is no longer round-tripped —
                // structured v2 carries the real role per variant).
                assert!(matches!(
                    s.entries[0].kind,
                    crate::session_entry::SessionEntryKind::AssistantMessage { .. }
                ));
            });
            sb.read_with(cx, |s, _| {
                assert!(s.is_cold());
                // No blob saved for B → entries empty.
                assert!(s.entries.is_empty());
            });
            // sessions_for is what the navigator's reconcile path
            // reads; insertion order into `by_solution` must match
            // the `tab_order ASC` returned by the DB so the strip
            // ends up identical to what the user closed last time.
            let listed: Vec<_> = store
                .sessions_for(&solution_id)
                .into_iter()
                .map(|entity| entity.read(cx).id)
                .collect();
            assert_eq!(listed, vec![id_b, id_a]);
        });
    });
}

/// Regression for the close→reopen empty-history bug: the extracted
/// blob→cold_entries helper must produce exactly the same shape from
/// the same input regardless of which call site invokes it. Pre-fix,
/// the v2 reconstruction was inlined in `restore_open_tabs` only; the
/// `resume_session` ELSE branch silently created an empty
/// `cold_entries` because `claude --resume` doesn't re-emit the
/// transcript. This test pins the helper's contract: a structured v2
/// blob round-trips into a same-length `AgentThreadEntry` vector
/// (one entry per `PersistedEntryV2`) and a 1:1 `entry_created_ms`
/// vector — so a future inline-it-back regression in either call
/// site fails here, not silently in the UI.
#[gpui::test]
async fn cold_entries_from_persisted_v2_reconstructs_per_entry(cx: &mut TestAppContext) {
    use crate::cold_persistence::{
        PersistedAssistantChunk, PersistedAssistantMessage, PersistedEntryV2, PersistedUserMessage,
    };
    let persisted = PersistedSession {
        title: "demo".into(),
        entries: vec![],
        entry_summaries: vec![],
        entries_v2: vec![
            PersistedEntryV2::User(PersistedUserMessage {
                id: None,
                content_md: "first prompt".into(),
                chunks: vec![],
            }),
            PersistedEntryV2::Assistant(PersistedAssistantMessage {
                chunks: vec![PersistedAssistantChunk::Message("reply".into())],
            }),
        ],
        entry_created_ms: vec![1_700_000_000_000, 1_700_000_001_000],
        available_models: vec![],
        desired_model: None,
        desired_effort: None,
    };
    let (cold_entries, created_ms) =
        cx.update(|cx| crate::store::cold_entries_from_persisted(Some(persisted), cx));
    assert_eq!(cold_entries.len(), 2, "v2 reconstruction must be 1:1");
    assert_eq!(created_ms, vec![1_700_000_000_000, 1_700_000_001_000]);
    assert!(matches!(
        cold_entries[0],
        acp_thread::AgentThreadEntry::UserMessage(_)
    ));
    assert!(matches!(
        cold_entries[1],
        acp_thread::AgentThreadEntry::AssistantMessage(_)
    ));

    // None-blob path returns empty vectors (no panic, no garbage).
    let (cold_entries, created_ms) =
        cx.update(|cx| crate::store::cold_entries_from_persisted(None, cx));
    assert!(cold_entries.is_empty());
    assert!(created_ms.is_empty());
}

#[test]
fn persisted_session_roundtrips_with_structured_entries() {
    let original = PersistedSession {
        title: "demo".into(),
        entries: vec![
            PersistedEntry {
                role: PersistedRole::User,
                markdown: "Hello".into(),
            },
            PersistedEntry {
                role: PersistedRole::Assistant,
                markdown: "Hi there!".into(),
            },
            PersistedEntry {
                role: PersistedRole::Tool,
                markdown: "ran tool x".into(),
            },
        ],
        entry_summaries: vec!["Hello".into(), "Hi there!".into(), "ran tool x".into()],
        entries_v2: vec![],
        entry_created_ms: vec![],
        available_models: vec![],
        desired_model: None,
        desired_effort: None,
    };
    let bytes = serde_json::to_vec(&original).unwrap();
    let decoded: PersistedSession = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.title, original.title);
    assert_eq!(decoded.entries.len(), 3);
    assert!(matches!(decoded.entries[0].role, PersistedRole::User));
    assert!(matches!(decoded.entries[1].role, PersistedRole::Assistant));
    assert!(matches!(decoded.entries[2].role, PersistedRole::Tool));
    assert_eq!(decoded.entries[0].markdown, "Hello");
    assert_eq!(decoded.entry_summaries.len(), 3);
}

/// Task 5: cold-restored session exposes transcript via `entries` (not `cold_entries`).
/// `is_cold()` is true (no live thread) and `entries` is non-empty.
#[gpui::test]
async fn cold_restore_populates_entries_directly(cx: &mut TestAppContext) {
    use crate::cold_persistence::{
        PersistedAssistantChunk, PersistedAssistantMessage, PersistedEntryV2, PersistedUserMessage,
    };
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.set_persistence(db.clone());
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = Utc::now();

    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("session A"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
    };
    db.save_metadata(meta_a).await.expect("meta a");

    let blob_a = serde_json::to_vec(&PersistedSession {
        title: "session A".into(),
        entries: vec![],
        entry_summaries: vec![],
        entries_v2: vec![
            PersistedEntryV2::User(PersistedUserMessage {
                id: None,
                content_md: "first prompt".into(),
                chunks: vec![],
            }),
            PersistedEntryV2::Assistant(PersistedAssistantMessage {
                chunks: vec![PersistedAssistantChunk::Message("reply".into())],
            }),
        ],
        entry_created_ms: vec![1_700_000_000_000, 1_700_000_001_000],
        available_models: vec![],
        desired_model: None,
        desired_effort: None,
    })
    .unwrap();
    db.save_blob(id_a, blob_a).await.expect("blob a");
    db.update_tab_orders(solution_id.clone(), vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("restore");
    assert_eq!(ordered, vec![id_a]);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session A restored");
            sa.read_with(cx, |s, _| {
                assert!(s.is_cold(), "restored session should be cold");
                assert_eq!(s.entries.len(), 2, "entries must hold the 2 restored entries");
                assert_eq!(s.live_base, 0, "cold session has live_base = 0 (no live thread)");
                assert!(
                    matches!(
                        s.entries[0].kind,
                        crate::session_entry::SessionEntryKind::UserMessage { .. }
                    ),
                    "first entry must be UserMessage"
                );
                assert!(
                    matches!(
                        s.entries[1].kind,
                        crate::session_entry::SessionEntryKind::AssistantMessage { .. }
                    ),
                    "second entry must be AssistantMessage"
                );
                assert_eq!(s.entries[0].created_ms, 1_700_000_000_000);
                assert_eq!(s.entries[1].created_ms, 1_700_000_001_000);
            });
        });
    });
}

#[test]
fn persisted_session_legacy_blob_decodes_with_empty_entries() {
    let legacy_json = serde_json::json!({
        "title": "old session",
        "entry_summaries": ["one", "two"],
    });
    let bytes = serde_json::to_vec(&legacy_json).unwrap();
    let decoded: PersistedSession = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.title, "old session");
    assert!(
        decoded.entries.is_empty(),
        "legacy blobs have no entries field"
    );
    assert_eq!(
        decoded.entry_summaries,
        vec!["one".to_string(), "two".to_string()]
    );
}

/// Regression: restoring a legacy blob (entries_v2 empty, entry_summaries populated)
/// must populate cold_persisted_v2 so that serializable_snapshot preserves the cold
/// history in subsequent persists. Before the fix, cold_persisted_v2 was set from
/// persisted.entries_v2 (empty for legacy blobs), causing the cold prefix to be
/// silently dropped from the blob on the next save.
#[gpui::test]
async fn legacy_blob_cold_history_survives_snapshot(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    // Build a legacy-shaped PersistedSession: entries_v2 is empty, history lives in
    // entry_summaries (the older flat-markdown format).
    let legacy = PersistedSession {
        title: "legacy session".into(),
        entries: vec![],
        entry_summaries: vec![
            "user said hello".to_string(),
            "assistant replied hi".to_string(),
        ],
        entries_v2: vec![],
        entry_created_ms: vec![],
        available_models: vec![],
        desired_model: None,
        desired_effort: None,
    };

    // Simulate what each restore site does: call cold_entries_from_persisted, then
    // build cold_persisted_v2 from the resulting cold_entries via to_persisted.
    let (cold_entries, _created_ms) =
        cx.update(|cx| crate::store::cold_entries_from_persisted(Some(legacy), cx));
    assert_eq!(cold_entries.len(), 2, "legacy blob must restore 2 entries");

    // Build a minimal SolutionSession with cold_persisted_v2 populated from cold_entries,
    // mirroring the fixed restore-site pattern.
    let session_id = crate::model::SolutionSessionId::new();
    let (session_bytes, cold_v2_len) = cx.update(|cx| {
        let cold_persisted_v2: Vec<crate::cold_persistence::PersistedEntryV2> = cold_entries
            .iter()
            .filter_map(|e| crate::cold_persistence::to_persisted(e, cx))
            .collect();
        let cold_v2_len = cold_persisted_v2.len();
        // Rebuild session entries before moving cold_persisted_v2 and cold_entries.
        let session_entries =
            crate::session_entry::rebuild_entries(&cold_entries, &[], &[], 0, cx);
        let session = cx.new(|_| {
            let mut s = crate::model::SolutionSession::new_idle(
                session_id,
                solutions::SolutionId("sol-legacy".into()),
                SharedString::from("claude-acp"),
                agent_client_protocol::schema::SessionId::new("acp-legacy"),
            );
            s.title = SharedString::from("legacy session");
            s.cold_persisted_v2 = cold_persisted_v2;
            s.entries = session_entries;
            s
        });
        let bytes = session.read_with(cx, |s, cx| serializable_snapshot(s, cx));
        (bytes, cold_v2_len)
    });

    assert_eq!(
        cold_v2_len, 2,
        "cold_persisted_v2 must be populated from cold_entries, not from the empty entries_v2"
    );

    let decoded: PersistedSession = serde_json::from_slice(&session_bytes).unwrap();
    assert_eq!(
        decoded.entries_v2.len(),
        2,
        "serializable_snapshot must emit the legacy cold entries into entries_v2 (was 0 before fix)"
    );
    assert!(
        !decoded.entry_summaries.is_empty(),
        "entry_summaries must be populated for legacy entries"
    );
}

// ----- Task 4: row-based cold load + lazy blob→rows migration -----

/// `entries_from_rows` unit test: a corrupt-payload row is skipped (log::warn,
/// not a panic) while every well-formed row decodes IN ORDER, preserving the
/// per-row meta (mod_seq / created_ms / subagent_id).
#[test]
fn entries_from_rows_skips_corrupt_and_preserves_order() {
    let good_user = crate::session_entry::SessionEntryKind::UserMessage {
        id: None,
        content_md: "hello".into(),
        chunks: vec![],
    };
    let good_assistant = crate::session_entry::SessionEntryKind::AssistantMessage {
        chunks: vec![crate::session_entry::AssistantChunk::Message("hi".into())],
    };
    let rows = vec![
        crate::db::EntryRow {
            idx: 0,
            mod_seq: 1,
            created_ms: 1_700_000_000_000,
            subagent_id: None,
            payload: serde_json::to_vec(&good_user).unwrap(),
        },
        crate::db::EntryRow {
            idx: 1,
            mod_seq: 2,
            created_ms: 1_700_000_001_000,
            subagent_id: None,
            payload: b"{not valid json".to_vec(),
        },
        crate::db::EntryRow {
            idx: 2,
            mod_seq: 3,
            created_ms: 1_700_000_002_000,
            subagent_id: Some("sub-7".into()),
            payload: serde_json::to_vec(&good_assistant).unwrap(),
        },
    ];

    let entries = crate::store::entries_from_rows(rows);
    assert_eq!(entries.len(), 2, "the corrupt middle row must be dropped");
    assert!(matches!(
        entries[0].kind,
        crate::session_entry::SessionEntryKind::UserMessage { .. }
    ));
    assert_eq!(entries[0].mod_seq, 1);
    assert_eq!(entries[0].created_ms, 1_700_000_000_000);
    assert_eq!(entries[0].subagent_id, None);
    assert!(matches!(
        entries[1].kind,
        crate::session_entry::SessionEntryKind::AssistantMessage { .. }
    ));
    assert_eq!(entries[1].mod_seq, 3);
    assert_eq!(entries[1].created_ms, 1_700_000_002_000);
    assert_eq!(
        entries[1].subagent_id,
        Some(SharedString::from("sub-7")),
        "subagent_id column must carry over"
    );
}

/// (a) A session whose transcript is already stored as ROWS (no blob touched)
/// cold-restores from those rows and reads the persisted epoch verbatim
/// (NO bump — a restart loading the same transcript must not look like a new
/// generation to the mobile delta client).
#[gpui::test]
async fn cold_restore_loads_from_rows_and_reads_epoch(cx: &mut TestAppContext) {
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.set_persistence(db.clone());
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = Utc::now();
    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("session A"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
    };
    db.save_metadata(meta_a).await.expect("meta a");

    // Write rows directly (the row-native shape) + a non-trivial epoch.
    let user = crate::session_entry::SessionEntryKind::UserMessage {
        id: None,
        content_md: "first prompt".into(),
        chunks: vec![],
    };
    let assistant = crate::session_entry::SessionEntryKind::AssistantMessage {
        chunks: vec![crate::session_entry::AssistantChunk::Message("reply".into())],
    };
    db.upsert_entry(
        id_a,
        0,
        1,
        1_700_000_000_000,
        None,
        serde_json::to_vec(&user).unwrap(),
    )
    .await
    .expect("row 0");
    db.upsert_entry(
        id_a,
        1,
        2,
        1_700_000_001_000,
        None,
        serde_json::to_vec(&assistant).unwrap(),
    )
    .await
    .expect("row 1");
    db.save_epoch(id_a, 7).await.expect("epoch");
    db.update_tab_orders(solution_id.clone(), vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("restore");
    assert_eq!(ordered, vec![id_a]);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session A restored");
            sa.read_with(cx, |s, _| {
                assert_eq!(s.entries.len(), 2, "entries must come from the 2 rows");
                assert!(matches!(
                    s.entries[0].kind,
                    crate::session_entry::SessionEntryKind::UserMessage { .. }
                ));
                assert!(matches!(
                    s.entries[1].kind,
                    crate::session_entry::SessionEntryKind::AssistantMessage { .. }
                ));
                assert_eq!(s.entries[0].mod_seq, 1);
                assert_eq!(s.entries[1].mod_seq, 2);
                assert_eq!(s.entries[0].created_ms, 1_700_000_000_000);
                assert_eq!(s.entries[1].created_ms, 1_700_000_001_000);
                assert_eq!(
                    s.epoch, 7,
                    "rows branch must READ the persisted epoch, not bump it"
                );
            });
        });
    });
}

/// (b) A v2 blob with NO rows migrates to rows on cold-restore: `entries`
/// matches the blob, `db.load_entries` becomes non-empty, and a SECOND
/// cold-restore returns the same entries straight from rows (idempotent — no
/// double-migrate, the blob is preserved as the model/effort fallback).
#[gpui::test]
async fn v2_blob_migrates_to_rows_and_is_idempotent(cx: &mut TestAppContext) {
    use crate::cold_persistence::{
        PersistedAssistantChunk, PersistedAssistantMessage, PersistedEntryV2, PersistedUserMessage,
    };
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.set_persistence(db.clone());
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = Utc::now();
    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("session A"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
    };
    db.save_metadata(meta_a).await.expect("meta a");

    let blob_a = serde_json::to_vec(&PersistedSession {
        title: "session A".into(),
        entries: vec![],
        entry_summaries: vec![],
        entries_v2: vec![
            PersistedEntryV2::User(PersistedUserMessage {
                id: None,
                content_md: "first prompt".into(),
                chunks: vec![],
            }),
            PersistedEntryV2::Assistant(PersistedAssistantMessage {
                chunks: vec![PersistedAssistantChunk::Message("reply".into())],
            }),
        ],
        entry_created_ms: vec![1_700_000_000_000, 1_700_000_001_000],
        available_models: vec![],
        desired_model: None,
        desired_effort: None,
    })
    .unwrap();
    db.save_blob(id_a, blob_a).await.expect("blob a");
    // No rows written: this is the lazy-migration trigger.
    assert!(
        db.load_entries(id_a).await.expect("load rows").is_empty(),
        "precondition: no rows before migration"
    );
    db.update_tab_orders(solution_id.clone(), vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("restore");
    assert_eq!(ordered, vec![id_a]);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session A restored");
            sa.read_with(cx, |s, _| {
                assert_eq!(s.entries.len(), 2, "entries must match the v2 blob");
                assert!(matches!(
                    s.entries[0].kind,
                    crate::session_entry::SessionEntryKind::UserMessage { .. }
                ));
                assert!(matches!(
                    s.entries[1].kind,
                    crate::session_entry::SessionEntryKind::AssistantMessage { .. }
                ));
            });
        });
    });

    // The migration (persist_all_rows) is spawned + detached; let it land.
    cx.run_until_parked();

    let rows = db.load_entries(id_a).await.expect("load rows after migrate");
    assert_eq!(rows.len(), 2, "migration must have written rows");

    // Second cold-restore: drop the in-memory session, restore again — now the
    // rows branch must serve the same entries (idempotent, no double-migrate).
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.sessions.remove(&id_a);
            store.by_solution.remove(&solution_id);
        });
    });
    let ordered2 = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("restore 2");
    assert_eq!(ordered2, vec![id_a]);
    cx.run_until_parked();

    let rows_after = db
        .load_entries(id_a)
        .await
        .expect("load rows after 2nd restore");
    assert_eq!(
        rows_after.len(),
        2,
        "second restore must NOT double-migrate (still exactly 2 rows)"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session A re-restored");
            sa.read_with(cx, |s, _| {
                assert_eq!(s.entries.len(), 2, "2nd restore loads same entries from rows");
            });
        });
    });
}

/// Regression guard: a v2 blob with `desired_model` set migrates on the first
/// restore (MIGRATE branch recovers model from blob and writes rows). On the
/// SECOND cold-restore (ROWS branch — no blob deserialization), the session
/// must still carry the same `desired_model`. This proves that
/// `persist_session_row` is called during migration, flushing the recovered
/// model/effort to the metadata columns before the blob path is bypassed.
#[gpui::test]
async fn migrated_session_retains_model_on_second_restore(cx: &mut TestAppContext) {
    use crate::cold_persistence::{PersistedEntryV2, PersistedUserMessage};
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.set_persistence(db.clone());
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = chrono::Utc::now();
    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("model session"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: std::path::PathBuf::new(),
        parent_session_id: None,
        // No model in the DB metadata column yet — simulates pre-Task-3a row.
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
    };
    db.save_metadata(meta_a).await.expect("meta a");

    // Write a v2 blob that carries desired_model; no rows yet (migration trigger).
    let blob_a = serde_json::to_vec(&PersistedSession {
        title: "model session".into(),
        entries: vec![],
        entry_summaries: vec![],
        entries_v2: vec![PersistedEntryV2::User(PersistedUserMessage {
            id: None,
            content_md: "hello".into(),
            chunks: vec![],
        })],
        entry_created_ms: vec![1_700_000_000_000],
        available_models: vec![],
        desired_model: Some("some-model".into()),
        desired_effort: None,
    })
    .unwrap();
    db.save_blob(id_a, blob_a).await.expect("blob a");
    assert!(
        db.load_entries(id_a).await.expect("load rows").is_empty(),
        "precondition: no rows before first restore"
    );
    db.update_tab_orders(solution_id.clone(), vec![id_a])
        .await
        .expect("tab order");

    // First restore — MIGRATE branch: recovers desired_model from blob.
    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("first restore");
    assert_eq!(ordered, vec![id_a]);

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session restored");
            sa.read_with(cx, |s, _| {
                assert_eq!(
                    s.desired_model.as_deref(),
                    Some("some-model"),
                    "migrate branch must recover desired_model from blob"
                );
            });
        });
    });

    // Let persist_all_rows + persist_session_row tasks land.
    cx.run_until_parked();

    // Confirm rows were written and metadata column was backfilled.
    let rows = db.load_entries(id_a).await.expect("load rows after migrate");
    assert_eq!(rows.len(), 1, "migration must have written 1 row");
    let metas = db
        .list_for_solution(solution_id.clone())
        .await
        .expect("list metas");
    let db_meta = metas.iter().find(|m| m.id == id_a).expect("meta in db");
    assert_eq!(
        db_meta.desired_model.as_deref(),
        Some("some-model"),
        "persist_session_row must have written desired_model to the metadata column"
    );

    // Drop the in-memory session so the second restore starts cold.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.sessions.remove(&id_a);
            store.by_solution.remove(&solution_id);
        });
    });

    // Second restore — ROWS branch: no blob deserialization, reads columns only.
    let ordered2 = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("second restore");
    assert_eq!(ordered2, vec![id_a]);
    cx.run_until_parked();

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session re-restored");
            sa.read_with(cx, |s, _| {
                assert_eq!(
                    s.desired_model.as_deref(),
                    Some("some-model"),
                    "second restore (rows branch) must retain desired_model from column"
                );
            });
        });
    });
}

/// (c) MANDATORY Phase-2 regression guard: a LEGACY v1 blob (entries_v2 EMPTY,
/// entry_summaries populated) migrates losslessly — `entries` carries the
/// summary text (history NOT lost) and rows are written. This is the exact
/// regression Phase 2 fixed; it must stay fixed.
#[gpui::test]
async fn legacy_v1_blob_migrates_losslessly(cx: &mut TestAppContext) {
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.set_persistence(db.clone());
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = Utc::now();
    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("legacy session"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
    };
    db.save_metadata(meta_a).await.expect("meta a");

    // Legacy v1 shape: entries_v2 EMPTY, history only in entry_summaries.
    let blob_a = serde_json::to_vec(&PersistedSession {
        title: "legacy session".into(),
        entries: vec![],
        entry_summaries: vec![
            "user said hello".to_string(),
            "assistant replied hi".to_string(),
        ],
        entries_v2: vec![],
        entry_created_ms: vec![],
        available_models: vec![],
        desired_model: None,
        desired_effort: None,
    })
    .unwrap();
    db.save_blob(id_a, blob_a).await.expect("blob a");
    db.update_tab_orders(solution_id.clone(), vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("restore");
    assert_eq!(ordered, vec![id_a]);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("legacy session restored");
            sa.read_with(cx, |s, _| {
                assert_eq!(
                    s.entries.len(),
                    2,
                    "legacy v1 history must NOT be lost (Phase-2 regression guard)"
                );
                // Legacy summaries hydrate as Assistant-shaped entries carrying
                // the flat markdown text.
                let carries_text = s.entries.iter().any(|e| {
                    matches!(
                        &e.kind,
                        crate::session_entry::SessionEntryKind::AssistantMessage { chunks }
                            if chunks.iter().any(|c| matches!(
                                c,
                                crate::session_entry::AssistantChunk::Message(m)
                                    if m.contains("user said hello")
                            ))
                    )
                });
                assert!(carries_text, "summary text must survive into entries");
            });
        });
    });

    // Migration writes rows.
    cx.run_until_parked();
    let rows = db.load_entries(id_a).await.expect("load rows after migrate");
    assert_eq!(
        rows.len(),
        2,
        "legacy migration must write rows so the next restore is row-native"
    );
    // Blob must be PRESERVED (Task 5 owns blob removal + model/effort backfill).
    assert!(
        db.load_blob(id_a).await.expect("load blob").is_some(),
        "migration must NOT null the blob (model/effort fallback safety net)"
    );
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
    use solutions::{CatalogId, Solution, SolutionMember};

    let mut registry = AdapterRegistry::new();
    registry.register(Arc::new(ClaudeAcpAdapter));
    cx.update(|cx| SolutionAgentStore::init_global(cx, Arc::new(registry)));

    let solution = Solution {
        id: SolutionId("sol-meta".into()),
        name: "test-meta".into(),
        root: PathBuf::from("/tmp/sol-meta"),
        members: vec![SolutionMember {
            catalog_id: CatalogId("cat-foo".into()),
            local_path: PathBuf::from("/tmp/sol-meta/foo"),
        }],
        last_opened_at: Some(Utc::now()),
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
            let meta =
                store.build_session_meta(
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

    let orphan_solution_id = SolutionId("orphan-sol".into());
    let session_id = SolutionSessionId::new();
    let agent_id = SharedString::from("mock-agent");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            insert_cold_session(
                session_id,
                orphan_solution_id.clone(),
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
        let session = store
            .read(cx)
            .session(session_id)
            .expect("session exists");
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
        let session = store
            .read(cx)
            .session(session_id)
            .expect("session exists");
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
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.set_persistence(db.clone());
        });
    });

    // Assert the persisted rows match the in-memory entries exactly.
    async fn assert_rows_match(
        cx: &mut TestAppContext,
        db: &crate::db::SolutionAgentDb,
        session_id: SolutionSessionId,
    ) {
        let entries = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session exists");
            session.read(cx).entries.clone()
        });
        let rows = db.load_entries(session_id).await.expect("load entries");
        assert_eq!(
            rows.len(),
            entries.len(),
            "row count must match in-memory entries"
        );
        for (idx, (row, entry)) in rows.iter().zip(entries.iter()).enumerate() {
            assert_eq!(row.idx, idx as i64, "rows must be in ascending idx order");
            assert_eq!(
                row.mod_seq, entry.mod_seq as i64,
                "row mod_seq must mirror the entry's mod_seq"
            );
            let kind = crate::session_entry::kind_from_payload(&row.payload)
                .expect("payload decodes to a kind");
            assert_eq!(kind, entry.kind, "row payload must decode to the entry kind");
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
    assert_eq!(remaining.len(), 1, "EntriesRemoved must delete the trailing row");
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
        let session = store
            .read(cx)
            .session(session_id)
            .expect("session exists");
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
async fn entry_created_ms_survives_persist_roundtrip(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Append two entries (user + assistant) so the hot `entry_created_ms`
    // gets two stamps, index-aligned with the live thread entries.
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

    // (a) Roundtrip: produce the real persisted blob via the same path the
    // store writes, decode it, and assert the timestamps survive intact and
    // stay index-aligned with the persisted entries.
    let (original_stamp0, original_stamp1, decoded) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let session = session.read(cx);
        let stamp0 = session.entries[0].created_ms;
        let stamp1 = session.entries[1].created_ms;
        let bytes = serializable_snapshot(session, cx);
        let decoded: PersistedSession = serde_json::from_slice(&bytes).unwrap();
        (stamp0, stamp1, decoded)
    });
    assert_eq!(decoded.entry_created_ms.len(), 2, "two appends → two persisted stamps");
    assert_eq!(
        decoded.entry_created_ms[0], original_stamp0,
        "persisted stamp[0] must roundtrip unchanged"
    );
    assert_eq!(
        decoded.entry_created_ms[1], original_stamp1,
        "persisted stamp[1] must roundtrip unchanged"
    );
    assert_eq!(
        decoded.entry_created_ms.len(),
        decoded.entries_v2.len(),
        "timestamp vector must stay index-aligned with entries_v2"
    );

    // (b) Absent sentinel roundtrips: force the first entry's stamp to
    // NO_TIMESTAMP_MS (an entry whose creation time was never captured) and
    // confirm `serializable_snapshot` + serde preserves it rather than turning
    // it into 0 or dropping it, and that the vector stays index-aligned.
    use crate::model::NO_TIMESTAMP_MS;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        session.update(cx, |s, _| {
            if let Some(e) = s.entries.get_mut(0) {
                e.created_ms = NO_TIMESTAMP_MS;
            }
        });
    });
    let decoded_sentinel = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let session = session.read(cx);
        let bytes = serializable_snapshot(session, cx);
        serde_json::from_slice::<PersistedSession>(&bytes).unwrap()
    });
    assert_eq!(
        decoded_sentinel.entry_created_ms[0], NO_TIMESTAMP_MS,
        "absent sentinel must survive persist roundtrip, not become 0 or be dropped"
    );
    assert_eq!(
        decoded_sentinel.entry_created_ms.len(),
        decoded_sentinel.entries_v2.len(),
        "sentinel-bearing vector must stay index-aligned with entries_v2"
    );

    // (c) Legacy decode: a blob without the `entry_created_ms` key decodes to
    // an empty vec (proves `#[serde(default)]`).
    let legacy = serde_json::json!({
        "title": "t",
        "entry_summaries": [],
        "entries_v2": []
    });
    let decoded: PersistedSession = serde_json::from_value(legacy).unwrap();
    assert!(decoded.entry_created_ms.is_empty());
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
        let session = store
            .read(cx)
            .session(session_id)
            .expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.entries.len(), 2, "two appends → two entries before removal");
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
        let session = store
            .read(cx)
            .session(session_id)
            .expect("session exists");
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
                solution_id.clone(),
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
                .entry(solution_id.clone())
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
                solution_id.clone(),
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
                .entry(solution_id.clone())
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
    let sub_pull =
        native.invoke_store_pull_for_test(&acp_session_id, Some("sub-agent-1"), false, &mut async_cx);
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

// ---------------------------------------------------------------------------
// Etap 3: Subagent-tab lifecycle (`active_subagents` + insertion-order vec).
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
fn make_task_tool_call(
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

/// Helper to count `SessionSubagentsChanged` events for a given session.
/// Returns the counter handle plus the subscription (which must be held in
/// scope for the lifetime of the test).
fn subscribe_subagents_changed(
    session_id: SolutionSessionId,
    cx: &mut TestAppContext,
) -> (Rc<std::cell::RefCell<usize>>, gpui::Subscription) {
    let counter = Rc::new(std::cell::RefCell::new(0usize));
    let subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let counter = counter.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionSubagentsChanged(id) = event
                && *id == session_id
            {
                *counter.borrow_mut() += 1;
            }
        })
    });
    (counter, subscription)
}

#[gpui::test]
async fn subagent_inprogress_task_registers_tab(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_task_1",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Loop agent 1"),
                    Some("general-purpose"),
                ),
                cx,
            )
            .expect("upsert task");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.active_subagents.len(), 1, "one subagent tracked");
        assert_eq!(
            s.active_subagent_order.len(),
            1,
            "order vec parallel to map"
        );
        let key = SharedString::from("toolu_task_1");
        assert_eq!(s.active_subagent_order[0], key);
        let tab = s.active_subagents.get(&key).expect("tab present");
        assert_eq!(tab.label.as_ref(), "Loop agent 1");
    });
    assert_eq!(
        *changed_count.borrow(),
        1,
        "SessionSubagentsChanged emitted exactly once on first registration"
    );
}

#[gpui::test]
async fn subagent_terminal_status_removes_tab(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_task_2",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Worker A"),
                    None,
                ),
                cx,
            )
            .expect("upsert in progress");
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(*changed_count.borrow(), 1, "one add emit");

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_task_2",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::Completed,
                    Some("Worker A"),
                    None,
                ),
                cx,
            )
            .expect("upsert completed");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert!(s.active_subagents.is_empty(), "tab removed on Completed");
        assert!(s.active_subagent_order.is_empty(), "order vec drained");
    });
    assert_eq!(
        *changed_count.borrow(),
        2,
        "exactly two emits: add + remove"
    );
}

#[gpui::test]
async fn subagent_label_falls_back_to_subagent_type(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_long_abcd",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    None,
                    Some("general-purpose"),
                ),
                cx,
            )
            .expect("upsert");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let key = SharedString::from("toolu_long_abcd");
        let tab = s.active_subagents.get(&key).expect("tab present");
        assert_eq!(
            tab.label.as_ref(),
            "general-purpose#abcd",
            "fallback label is `subagent_type#<short-id>`"
        );
    });
}

#[gpui::test]
async fn subagent_label_defaults_to_agent_short_id(cx: &mut TestAppContext) {
    // No description, no subagent_type → final fallback: `Agent <short-id>`.
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_xy12",
                    "Agent",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    None,
                    None,
                ),
                cx,
            )
            .expect("upsert");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let key = SharedString::from("toolu_xy12");
        let tab = s.active_subagents.get(&key).expect("tab present");
        assert_eq!(tab.label.as_ref(), "Agent xy12");
    });
}

#[gpui::test]
async fn non_task_tool_call_does_not_register_tab(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_bash_1",
                    "Bash",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("ignored"),
                    None,
                ),
                cx,
            )
            .expect("upsert bash");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert!(s.active_subagents.is_empty(), "Bash is not a subagent");
        assert!(s.active_subagent_order.is_empty(), "order vec untouched");
    });
    assert_eq!(
        *changed_count.borrow(),
        0,
        "no SessionSubagentsChanged emission for non-subagent tools"
    );
}

#[gpui::test]
async fn subagent_insertion_order_preserved(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    for (id, label) in [
        ("toolu_a", "First"),
        ("toolu_b", "Second"),
        ("toolu_c", "Third"),
    ] {
        cx.update(|cx| {
            acp_thread.update(cx, |t, cx| {
                t.upsert_tool_call(
                    make_task_tool_call(
                        id,
                        "Task",
                        agent_client_protocol::schema::ToolCallStatus::InProgress,
                        Some(label),
                        None,
                    ),
                    cx,
                )
                .expect("upsert");
            });
        });
        cx.executor().run_until_parked();
    }

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let order: Vec<&str> = s.active_subagent_order.iter().map(|s| s.as_ref()).collect();
        assert_eq!(
            order,
            vec!["toolu_a", "toolu_b", "toolu_c"],
            "tabs render in spawn order, not hash order"
        );
        assert_eq!(s.active_subagents.len(), 3);
    });
}

#[gpui::test]
async fn duplicate_inprogress_does_not_re_register(cx: &mut TestAppContext) {
    // A streaming Task's raw_input arrives over several EntryUpdated events
    // before the status flips off InProgress; each one re-enters
    // `apply_subagent_lifecycle`. The first must register the tab, every
    // subsequent observation must be a no-op (no double-insert, no
    // SessionSubagentsChanged spam).
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_dup",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Original"),
                    None,
                ),
                cx,
            )
            .expect("upsert initial");
        });
    });
    cx.executor().run_until_parked();

    // Simulate a second EntryUpdated for the same id, still InProgress
    // (e.g. raw_input streamed in more keys). Even if the label would now
    // be different, the tab is already there.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_dup",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Renamed"),
                    None,
                ),
                cx,
            )
            .expect("upsert again");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.active_subagents.len(), 1, "single tab, not doubled");
        assert_eq!(s.active_subagent_order.len(), 1, "order vec single entry");
        let tab = s
            .active_subagents
            .get(&SharedString::from("toolu_dup"))
            .expect("tab");
        // Label is locked-in at first observation — the "Renamed" update is
        // ignored to preserve a stable user-facing pill across the streaming
        // raw_input chunks.
        assert_eq!(tab.label.as_ref(), "Original");
    });
    assert_eq!(
        *changed_count.borrow(),
        1,
        "duplicate InProgress must not re-emit SessionSubagentsChanged"
    );
}

#[gpui::test]
async fn subagent_failed_status_also_removes_tab(cx: &mut TestAppContext) {
    // Terminal-status coverage: Failed is just as final as Completed/Canceled.
    // A Task subagent that crashed mid-run still releases its tab.
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_fail",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Doomed"),
                    None,
                ),
                cx,
            )
            .expect("upsert");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_fail",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::Failed,
                    Some("Doomed"),
                    None,
                ),
                cx,
            )
            .expect("upsert failed");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        assert!(session.read(cx).active_subagents.is_empty());
    });
    assert_eq!(*changed_count.borrow(), 2, "add + remove on Failed");
}

/// Background-Agents-Strip Task 8: when an `Agent`-named ToolCall enters a
/// terminal status with a parseable `raw_output`, `apply_subagent_lifecycle`
/// registers a `BackgroundAgent` and emits `SessionBackgroundAgentsChanged`.
#[gpui::test]
async fn agent_terminal_with_parseable_raw_output_registers_background_agent(
    cx: &mut TestAppContext,
) {
    use agent_client_protocol::schema as acp;
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let bg_counter = Rc::new(std::cell::RefCell::new(0usize));
    let _bg_sub = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let counter = bg_counter.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(id) = event
                && *id == session_id
            {
                *counter.borrow_mut() += 1;
            }
        })
    });

    // InProgress → Completed with raw_output carrying the managed-agent
    // announcement. Mirrors how claude_code emits the Agent tool call.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            let call = acp::ToolCall::new(
                acp::ToolCallId::new("toolu_agent_1".to_string()),
                "Agent".to_string(),
            )
            .kind(acp::ToolKind::Think)
            .status(acp::ToolCallStatus::InProgress)
            .meta(Some(acp_thread::meta_with_tool_name("Agent")));
            t.upsert_tool_call(call, cx).expect("upsert in-progress");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            let raw = serde_json::Value::String(
                "agentId: a30f92a688e431edc\noutput_file: /tmp/agent-a30f92a688e431edc.output"
                    .to_string(),
            );
            let call = acp::ToolCall::new(
                acp::ToolCallId::new("toolu_agent_1".to_string()),
                "Agent".to_string(),
            )
            .kind(acp::ToolKind::Think)
            .status(acp::ToolCallStatus::Completed)
            .raw_output(raw)
            .meta(Some(acp_thread::meta_with_tool_name("Agent")));
            t.upsert_tool_call(call, cx).expect("upsert completed");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(
            s.background_agents.len(),
            1,
            "one background-agent registered on Agent-tool terminal"
        );
        assert_eq!(s.background_agent_order.len(), 1, "order vec parallel");
        let id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
        assert!(
            s.background_agents.contains_key(&id),
            "registration keyed on parsed agentId"
        );
        assert_eq!(s.background_agent_order[0], id);
    });
    assert_eq!(
        *bg_counter.borrow(),
        1,
        "SessionBackgroundAgentsChanged emitted exactly once on registration"
    );

    // Idempotency: a duplicate terminal-status update with the same
    // raw_output must NOT re-register or re-emit. Drives the
    // `contains_key` guard in apply_subagent_lifecycle.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            let raw = serde_json::Value::String(
                "agentId: a30f92a688e431edc\noutput_file: /tmp/agent-a30f92a688e431edc.output"
                    .to_string(),
            );
            let call = acp::ToolCall::new(
                acp::ToolCallId::new("toolu_agent_1".to_string()),
                "Agent".to_string(),
            )
            .kind(acp::ToolKind::Think)
            .status(acp::ToolCallStatus::Completed)
            .raw_output(raw)
            .meta(Some(acp_thread::meta_with_tool_name("Agent")));
            t.upsert_tool_call(call, cx).expect("upsert duplicate");
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(
        *bg_counter.borrow(),
        1,
        "duplicate Agent-terminal update is a no-op (idempotent registration)"
    );
}

/// Build a `Bash` ToolCall carrying a `run_in_background` flag in
/// `raw_input` and the launch announcement in `raw_output`. Mirrors how
/// claude_code shapes a backgrounded `Bash` call.
fn make_bash_bg_tool_call(
    id: &str,
    command: &str,
    run_in_background: bool,
    raw_output: Option<&str>,
) -> agent_client_protocol::schema::ToolCall {
    use agent_client_protocol::schema as acp;
    let mut raw_input = serde_json::Map::new();
    raw_input.insert("command".into(), serde_json::Value::String(command.into()));
    raw_input.insert(
        "run_in_background".into(),
        serde_json::Value::Bool(run_in_background),
    );
    let mut call = acp::ToolCall::new(acp::ToolCallId::new(id.to_string()), "Bash".to_string())
        .kind(acp::ToolKind::Execute)
        .status(acp::ToolCallStatus::Completed)
        .meta(Some(acp_thread::meta_with_tool_name("Bash")))
        .raw_input(serde_json::Value::Object(raw_input));
    if let Some(out) = raw_output {
        call = call.raw_output(serde_json::Value::String(out.to_string()));
    }
    call
}

/// Background-Shells-Strip Tasks 7 + 9: a terminal `Bash` tool call with
/// `run_in_background=true` and a parseable launch announcement registers a
/// `BackgroundShell` (before the `is_task_like` gate, since `Bash` is not a
/// task-like tool) and emits `SessionBackgroundShellsChanged`.
#[gpui::test]
async fn bash_run_in_background_terminal_registers_background_shell(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let bg_counter = Rc::new(std::cell::RefCell::new(0usize));
    let _bg_sub = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let counter = bg_counter.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionBackgroundShellsChanged(id) = event
                && *id == session_id
            {
                *counter.borrow_mut() += 1;
            }
        })
    });

    const ANNOUNCEMENT: &str = "Command running in background with ID: bvb4ful1z. Output is being written to: /tmp/claude-1000/-home-spk-proj/ses-x/tasks/bvb4ful1z.output. You will be notified when it completes.";

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_bash_bg_tool_call("toolu_bash_1", "sleep 60", true, Some(ANNOUNCEMENT)),
                cx,
            )
            .expect("upsert bash bg");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(
            s.background_shells.len(),
            1,
            "one background-shell registered on Bash(bg) terminal"
        );
        assert_eq!(s.background_shell_order.len(), 1, "order vec parallel");
        let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
        assert!(
            s.background_shells.contains_key(&id),
            "registration keyed on parsed shell id"
        );
        assert_eq!(s.background_shell_order[0], id);
        let shell = s.background_shells.get(&id).expect("shell present");
        assert_eq!(
            shell.output_path,
            PathBuf::from("/tmp/claude-1000/-home-spk-proj/ses-x/tasks/bvb4ful1z.output"),
            "output_path parsed from announcement"
        );
        assert_eq!(shell.command.as_ref(), "sleep 60", "command label captured");
        assert_eq!(
            shell.state,
            crate::background_shell::ShellRuntimeState::Running,
            "fresh shell is Running"
        );
    });
    assert_eq!(
        *bg_counter.borrow(),
        1,
        "SessionBackgroundShellsChanged emitted exactly once on registration"
    );
}

/// Tasks 7 + 9: a `Bash` tool call WITHOUT `run_in_background` (false) must
/// NOT register a background shell — the `run_in_background == Some(true)`
/// guard rejects it.
#[gpui::test]
async fn bash_without_run_in_background_registers_no_shell(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    const ANNOUNCEMENT: &str = "Command running in background with ID: bvb4ful1z. Output is being written to: /tmp/claude-1000/proj/tasks/bvb4ful1z.output. You will be notified.";

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                // run_in_background = false, but with an announcement-shaped
                // raw_output — the guard must still reject it.
                make_bash_bg_tool_call("toolu_bash_2", "echo hi", false, Some(ANNOUNCEMENT)),
                cx,
            )
            .expect("upsert non-bg bash");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert!(
            s.background_shells.is_empty(),
            "no shell registered for a non-background Bash call"
        );
        assert!(s.background_shell_order.is_empty());
    });
}

/// Tasks 7 + 9: after registration, `refresh_background_shell_snapshot`
/// live-tails the on-disk `.output` file into `BackgroundShell::latest`.
/// Drives the full launch→register→tail pipeline against a real temp file.
#[gpui::test]
async fn refresh_background_shell_snapshot_tails_output_file(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let output_path = dir.path().join("tasks").join("bvb4ful1z.output");
    std::fs::create_dir_all(output_path.parent().expect("parent")).expect("mkdir tasks");
    std::fs::write(&output_path, b"line one\nline two\n").expect("write output");

    let announcement = format!(
        "Command running in background with ID: bvb4ful1z. Output is being written to: {}. You will be notified.",
        output_path.display()
    );

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_bash_bg_tool_call("toolu_bash_3", "tail -f log", true, Some(&announcement)),
                cx,
            )
            .expect("upsert bash bg");
        });
    });
    cx.executor().run_until_parked();

    // The inline refresh in the registration branch already tailed the file
    // (the announcement path is a real temp file). Assert the snapshot.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
        let shell = s.background_shells.get(&id).expect("shell present");
        let latest = shell
            .latest
            .as_ref()
            .expect("inline refresh seeded a snapshot from the real file");
        assert!(
            latest.output_tail.contains("line one") && latest.output_tail.contains("line two"),
            "tail captured the file bytes, got: {:?}",
            latest.output_tail
        );
        assert_eq!(
            shell.last_offset, 18,
            "offset advanced past the 18 written bytes"
        );
    });
}

/// Test helper: register a `Running` background shell directly on a session,
/// bypassing the `Bash(bg)` launch-announcement parse path. Used by the Task 8
/// terminal-signal tests so they can assert the state transition in isolation.
fn register_background_shell(
    cx: &mut TestAppContext,
    session_id: SolutionSessionId,
    shell_id: &str,
) {
    let id = crate::background_shell::BackgroundShellId::new(shell_id.to_string());
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        session.update(cx, |s, _| {
            s.background_shells.insert(
                id.clone(),
                crate::background_shell::BackgroundShell {
                    id: id.clone(),
                    command: SharedString::from("sleep 60"),
                    output_path: PathBuf::from("/tmp/claude-1000/tasks")
                        .join(format!("{shell_id}.output")),
                    registered_at: chrono::Utc::now(),
                    latest: None,
                    last_offset: 0,
                    state: crate::background_shell::ShellRuntimeState::Running,
                },
            );
            s.background_shell_order.push(id);
        });
    });
}

/// Task 8: a terminal `KillShell` tool_call whose `raw_input` carries the
/// `shell_id` of a tracked background shell flips that shell to
/// `ShellRuntimeState::Killed` and emits `SessionBackgroundShellsChanged`.
#[gpui::test]
async fn kill_shell_terminal_marks_shell_killed(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    register_background_shell(cx, session_id, "bvb4ful1z");

    let bg_counter = Rc::new(std::cell::RefCell::new(0usize));
    let _bg_sub = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let counter = bg_counter.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionBackgroundShellsChanged(id) = event
                && *id == session_id
            {
                *counter.borrow_mut() += 1;
            }
        })
    });

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            use agent_client_protocol::schema as acp;
            let mut raw_input = serde_json::Map::new();
            raw_input.insert(
                "shell_id".into(),
                serde_json::Value::String("bvb4ful1z".into()),
            );
            let call = acp::ToolCall::new(
                acp::ToolCallId::new("toolu_kill_1".to_string()),
                "KillShell".to_string(),
            )
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Completed)
            .meta(Some(acp_thread::meta_with_tool_name("KillShell")))
            .raw_input(serde_json::Value::Object(raw_input));
            t.upsert_tool_call(call, cx).expect("upsert KillShell");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
        let shell = s.background_shells.get(&id).expect("shell still tracked");
        assert_eq!(
            shell.state,
            crate::background_shell::ShellRuntimeState::Killed,
            "KillShell tool_call flips the shell to Killed"
        );
    });
    assert_eq!(
        *bg_counter.borrow(),
        1,
        "SessionBackgroundShellsChanged emitted exactly once on the kill"
    );
}

/// Task 8: a `<task-notification>` user-role message whose `<task-id>` matches
/// a tracked background shell flips it to `Exited(Some(code))`. Drives the
/// real NewEntry wiring: `push_user_content_block` appends a `UserMessage`
/// entry → `AcpThreadEvent::NewEntry` → `observe_task_notification`.
#[gpui::test]
async fn task_notification_user_message_marks_shell_exited(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    register_background_shell(cx, session_id, "bvb4ful1z");

    const NOTIFICATION: &str = r#"<task-notification>
<task-id>bvb4ful1z</task-id>
<status>completed</status>
<summary>Background command "sleep 60" completed (exit code 0)</summary>
</task-notification>"#;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(NOTIFICATION.to_string()),
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
        let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
        let shell = s.background_shells.get(&id).expect("shell still tracked");
        assert_eq!(
            shell.state,
            crate::background_shell::ShellRuntimeState::Exited(Some(0)),
            "task-notification user message flips the shell to Exited(0)"
        );
    });
}

/// Task 8: a `<task-notification>` for an UNTRACKED shell id is a no-op — no
/// stray shell is registered and no tracked shell's state changes.
#[gpui::test]
async fn task_notification_unknown_shell_is_noop(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    register_background_shell(cx, session_id, "tracked123");

    const NOTIFICATION: &str = r#"<task-notification>
<task-id>unknown999</task-id>
<status>completed</status>
<summary>Background command "x" completed (exit code 0)</summary>
</task-notification>"#;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(NOTIFICATION.to_string()),
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
        assert_eq!(s.background_shells.len(), 1, "no stray shell registered");
        let tracked = crate::background_shell::BackgroundShellId::new("tracked123");
        assert_eq!(
            s.background_shells
                .get(&tracked)
                .expect("tracked present")
                .state,
            crate::background_shell::ShellRuntimeState::Running,
            "an unrelated notification leaves the tracked shell Running"
        );
    });
}

/// Task 9: a Managed Agent whose `latest.stop_reason` is `Some(...)` is
/// removed from the session on the next `tick_background_agents` pass,
/// and a `SessionBackgroundAgentsChanged` event is emitted.
#[gpui::test]
async fn done_agent_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: "/nonexistent".into(),
                    registered_at: chrono::Utc::now(),
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now(),
                        activity_label: SharedString::from("Done."),
                        stop_reason: Some(SharedString::from("end_turn")),
                    }),
                    last_offset: 0,
                },
            );
            s.background_agent_order.push(bg_id.clone());
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_agents(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.is_empty(),
            "done agent must be removed on tick"
        );
        assert!(
            session.read(cx).background_agent_order.is_empty(),
            "order vec must be pruned in lockstep"
        );
    });
}

/// Task 9: an agent with a stale snapshot beyond
/// `MANAGED_AGENT_STALE_TIMEOUT + MANAGED_AGENT_DEAD_LINGER`
/// (V1 hardcoded: 120s + 300s = 420s) is removed on tick.
#[gpui::test]
async fn stale_agent_lingers_briefly_then_removed(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: "/nonexistent".into(),
                    registered_at: chrono::Utc::now(),
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now() - std::time::Duration::from_secs(500),
                        activity_label: SharedString::from("Bash: x"),
                        stop_reason: None,
                    }),
                    last_offset: 0,
                },
            );
            s.background_agent_order.push(bg_id.clone());
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_agents(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.is_empty(),
            "stale-beyond-linger agent must be removed on tick"
        );
    });
}

/// Task 9: a fresh (non-terminal, recent mtime) Managed Agent must NOT
/// be removed by `tick_background_agents` — only done + long-dead are
/// candidates. Guards against the tick over-pruning live work.
#[gpui::test]
async fn fresh_agent_survives_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: "/nonexistent".into(),
                    registered_at: chrono::Utc::now(),
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now(),
                        activity_label: SharedString::from("Bash: x"),
                        stop_reason: None,
                    }),
                    last_offset: 0,
                },
            );
            s.background_agent_order.push(bg_id.clone());
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_agents(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.contains_key(&bg_id),
            "fresh agent must survive a tick"
        );
    });
}

/// Helper: insert one background shell into a session's tracking maps.
fn insert_test_background_shell(
    cx: &mut TestAppContext,
    session_id: crate::model::SolutionSessionId,
    shell_id: &crate::background_shell::BackgroundShellId,
    state: crate::background_shell::ShellRuntimeState,
    latest: Option<crate::background_shell::BackgroundShellSnapshot>,
    registered_at: chrono::DateTime<chrono::Utc>,
) {
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.background_shells.insert(
                shell_id.clone(),
                crate::background_shell::BackgroundShell {
                    id: shell_id.clone(),
                    command: SharedString::from("sleep 1"),
                    output_path: "/nonexistent".into(),
                    registered_at,
                    latest,
                    last_offset: 0,
                    state,
                },
            );
            s.background_shell_order.push(shell_id.clone());
        });
    });
}

/// Task 10: an `Exited(Some(0))` background shell is reaped on the next
/// `tick_background_shells` pass (terminal-state arm).
#[gpui::test]
async fn exited_shell_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Exited(Some(0)),
        Some(crate::background_shell::BackgroundShellSnapshot {
            // Fresh mtime: proves it's the terminal state, not staleness,
            // driving the removal.
            mtime: std::time::SystemTime::now(),
            output_tail: SharedString::from("done"),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.is_empty(),
            "Exited shell must be removed on tick"
        );
        assert!(
            session.read(cx).background_shell_order.is_empty(),
            "order vec must be pruned in lockstep"
        );
    });
}

/// Task 10: a `Killed` background shell is reaped on tick (terminal-state arm).
#[gpui::test]
async fn killed_shell_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Killed,
        Some(crate::background_shell::BackgroundShellSnapshot {
            mtime: std::time::SystemTime::now(),
            output_tail: SharedString::from("killed"),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.is_empty(),
            "Killed shell must be removed on tick"
        );
    });
}

/// Task 10 — THE leak-prevention case: a still-`Running` shell whose
/// `latest.mtime` is older than stale+linger (420s) is reaped on tick. In the
/// current build a completed shell almost never flips to `Exited` (the
/// `<task-notification>` signal is dormant), so without this staleness arm the
/// finished shell would leak as a "Running" pill forever.
#[gpui::test]
async fn stale_running_shell_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Running,
        Some(crate::background_shell::BackgroundShellSnapshot {
            mtime: std::time::SystemTime::now() - std::time::Duration::from_secs(10_000),
            output_tail: SharedString::from("...stalled output"),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.is_empty(),
            "stale-Running shell (mtime beyond stale+linger) must be reaped \
             on tick — else it leaks as a Running pill forever"
        );
    });
}

/// Task 10: a Running shell with NO snapshot but a stale `registered_at`
/// (zero output, long since launched) must still age out via the
/// registered_at fallback.
#[gpui::test]
async fn stale_running_shell_no_snapshot_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Running,
        None,
        chrono::Utc::now() - chrono::Duration::seconds(10_000),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.is_empty(),
            "stale Running shell with no snapshot must age out via registered_at"
        );
    });
}

/// Task 10: a fresh `Running` shell (recent mtime, just registered) must NOT
/// be removed by `tick_background_shells`. Guards against over-pruning live
/// shells.
#[gpui::test]
async fn fresh_running_shell_survives_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Running,
        Some(crate::background_shell::BackgroundShellSnapshot {
            mtime: std::time::SystemTime::now(),
            output_tail: SharedString::from("running..."),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.contains_key(&shell_id),
            "fresh Running shell must survive a tick"
        );
    });
}

/// Pin the error-string set that `resume_session` treats as "session gone,
/// try the next cwd candidate (and ultimately mint a new ACP session)".
///
/// claude-code-acp returns `No conversation found with session ID: …` as the
/// MESSAGE of a JSON-RPC `-32603` (Internal error). Before the fix, our
/// predicate only matched `Resource not found` / `-32002` (the spec's
/// "missing resource" code) — so the message-only error fell through, the
/// `resume_session` for-loop broke after the first attempt, the
/// `solution.root` fallback never fired, and the user saw a raw
/// "No conversation found with session ID: …" snackbar on the next editor
/// restart even though the jsonl was sitting under sanitize(solution.root)
/// the whole time.
///
/// If you change the predicate, update both the match set and this test —
/// dropping a marker silently reintroduces the snackbar.
#[test]
fn is_session_gone_error_matches_known_markers() {
    use crate::store::is_session_gone_error;
    assert!(is_session_gone_error(
        "No conversation found with session ID: 877b9e1b-ae75-448e-bcef-906058b156df"
    ));
    assert!(is_session_gone_error("Resource not found"));
    assert!(is_session_gone_error(
        "RPC error -32002: session id no longer known"
    ));
    // Non-recoverable transport/auth/allow-list errors stay opaque so we
    // don't pointlessly retry against another cwd.
    assert!(!is_session_gone_error("connection refused"));
    assert!(!is_session_gone_error("authentication required"));
    assert!(!is_session_gone_error("permission denied: /home/spk/.spk"));
}

/// Task 13: hydrate-side reconcile drops SQLite rows whose JSONL file
/// no longer exists on disk — without this the strip would render dead
/// pills for runs whose worker dirs were wiped while the editor was
/// closed.
#[gpui::test]
async fn reconciliation_drops_rows_with_missing_jsonl(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let rows = vec![crate::db::BackgroundAgentRow {
        solution_session_id: session_id.to_string(),
        agent_id: "missing000000000000".into(),
        jsonl_path: "/nonexistent/path/missing.jsonl".into(),
        registered_at_ms: 0,
        last_seen_label: None,
        last_mtime_ms: None,
        stop_reason: None,
    }];
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.reconcile_background_agents_for(session_id, rows, cx)
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.is_empty(),
            "row with missing JSONL file must not be registered"
        );
    });
}

/// Task 13: rows whose tail JSONL line carries a terminal `stop_reason`
/// are NOT re-registered — the worker already wrapped up while the
/// editor was closed, so re-adding it would resurrect a finished pill.
#[gpui::test]
async fn reconciliation_drops_done_rows(cx: &mut TestAppContext) {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("done.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"bye"}}],"stop_reason":"end_turn"}}}}"#
    )
    .unwrap();
    drop(f);

    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let rows = vec![crate::db::BackgroundAgentRow {
        solution_session_id: session_id.to_string(),
        agent_id: "done00000000000000".into(),
        jsonl_path: path.to_string_lossy().into_owned(),
        registered_at_ms: 0,
        last_seen_label: None,
        last_mtime_ms: None,
        stop_reason: None,
    }];
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.reconcile_background_agents_for(session_id, rows, cx)
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.is_empty(),
            "row with terminal stop_reason must not be registered"
        );
    });
}

/// Task 13: an alive row (file present, no terminal stop_reason on the
/// tail) is re-registered with a snapshot — the render-side classifier
/// then decides Running vs Dead based on mtime.
#[gpui::test]
async fn reconciliation_registers_alive_row(cx: &mut TestAppContext) {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alive.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"hi"}}]}}}}"#
    )
    .unwrap();
    drop(f);

    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let rows = vec![crate::db::BackgroundAgentRow {
        solution_session_id: session_id.to_string(),
        agent_id: "alive0000000000000".into(),
        jsonl_path: path.to_string_lossy().into_owned(),
        registered_at_ms: 0,
        last_seen_label: None,
        last_mtime_ms: None,
        stop_reason: None,
    }];
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.reconcile_background_agents_for(session_id, rows, cx)
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        let s = session.read(cx);
        assert_eq!(s.background_agents.len(), 1);
        assert!(
            s.background_agents
                .values()
                .next()
                .unwrap()
                .latest
                .is_some()
        );
    });
}

/// Removes a directory subtree on drop — used to clean up the
/// `~/.claude/projects/<encoded-cwd>/` dir the live-scan test must create at
/// the real (home-derived) path the resolver computes.
struct CleanupDir(PathBuf);
impl Drop for CleanupDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// End-to-end: a Running shell flips to `Exited(Some(0))` when a realistic
/// single-line `<task-notification>` JSON `user` message is appended to the
/// parent session JSONL and `scan_parent_jsonl_for_completions` runs. Also
/// asserts the forward-only offset: a second scan with no new bytes is a
/// no-op (the shell stays exactly where the first scan left it).
#[gpui::test]
fn scan_parent_jsonl_flips_running_shell_to_exited(cx: &mut TestAppContext) {
    use crate::background_shell::{BackgroundShell, BackgroundShellId, ShellRuntimeState};

    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    // Unique cwd so the home-derived JSONL path never collides with a real
    // session or a parallel test run.
    let unique = format!(
        "/tmp/spk-scan-test-{}-{}",
        std::process::id(),
        SolutionSessionId::new()
    );
    let cwd = PathBuf::from(&unique);
    let acp_id = "ses-scan-xyz";

    let jsonl = super::parent_session_jsonl_for(&cwd, acp_id).expect("home_dir resolves in test");
    let project_dir = jsonl.parent().expect("jsonl has parent").to_path_buf();
    let _cleanup = CleanupDir(project_dir.clone());
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    // Pre-existing historical content the scan must skip past (offset lazy-init
    // to current EOF on first sight).
    std::fs::write(&jsonl, b"{\"type\":\"system\",\"old\":true}\n").expect("seed jsonl");

    let session_id = SolutionSessionId::new();
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let entity = cx.new(|_| {
                let mut s = SolutionSession::new_idle(
                    session_id,
                    SolutionId("sol-scan".into()),
                    SharedString::from("claude-acp"),
                    agent_client_protocol::schema::SessionId::new(acp_id),
                );
                s.cwd = cwd.clone();
                let id = BackgroundShellId::new("bvb4ful1z");
                s.background_shells.insert(
                    id.clone(),
                    BackgroundShell {
                        id: id.clone(),
                        command: "sleep 60".into(),
                        output_path: PathBuf::from("/tmp/bvb4ful1z.output"),
                        registered_at: Utc::now(),
                        latest: None,
                        last_offset: 0,
                        state: ShellRuntimeState::Running,
                    },
                );
                s.background_shell_order.push(id);
                s
            });
            store.sessions.insert(session_id, entity);
            store
                .by_solution
                .entry(SolutionId("sol-scan".into()))
                .or_default()
                .push(session_id);
            // First scan: lazy-inits the offset to current EOF, flips nothing.
            store.scan_parent_jsonl_for_completions(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        let shell = session
            .read(cx)
            .background_shells
            .get(&BackgroundShellId::new("bvb4ful1z"))
            .cloned()
            .unwrap();
        assert_eq!(
            shell.state,
            ShellRuntimeState::Running,
            "historical content must not flip the shell"
        );
    });

    // Append the realistic single-line completion notification.
    {
        use std::io::Write;
        let line = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"<task-notification>\\n<task-id>bvb4ful1z</task-id>\\n<status>completed</status>\\n<summary>Background command \\\"sleep 60\\\" completed (exit code 0)</summary>\\n</task-notification>\"}]},\"uuid\":\"abc-123\"}\n";
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl)
            .expect("reopen jsonl");
        f.write_all(line.as_bytes()).expect("append notification");
    }

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.scan_parent_jsonl_for_completions(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        let shell = session
            .read(cx)
            .background_shells
            .get(&BackgroundShellId::new("bvb4ful1z"))
            .cloned()
            .unwrap();
        assert_eq!(
            shell.state,
            ShellRuntimeState::Exited(Some(0)),
            "completion notification must flip Running -> Exited(0)"
        );
    });

    // Second scan, no new bytes: idempotent no-op (state unchanged).
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.scan_parent_jsonl_for_completions(session_id, cx);
        });
        let session = store.read(cx).session(session_id).unwrap();
        let shell = session
            .read(cx)
            .background_shells
            .get(&BackgroundShellId::new("bvb4ful1z"))
            .cloned()
            .unwrap();
        assert_eq!(shell.state, ShellRuntimeState::Exited(Some(0)));
    });

    // Unknown-id notification flips nothing.
    {
        use std::io::Write;
        let line = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"<task-notification>\\n<task-id>unknownid99</task-id>\\n<status>completed</status>\\n<summary>done (exit code 0)</summary>\\n</task-notification>\"}]}}\n";
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl)
            .expect("reopen jsonl");
        f.write_all(line.as_bytes()).expect("append unknown");
    }
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.scan_parent_jsonl_for_completions(session_id, cx);
        });
        let session = store.read(cx).session(session_id).unwrap();
        assert_eq!(session.read(cx).background_shells.len(), 1);
    });
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
            s.solution_id.clone(),
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
            s.solution_id.clone(),
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
                    Some(parent),
                    None,
                    None,
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

#[test]
fn persisted_session_round_trips_models() {
    let p = PersistedSession {
        title: "t".into(),
        entries: vec![],
        entry_summaries: vec![],
        entries_v2: vec![],
        entry_created_ms: vec![],
        available_models: vec![claude_native::ModelInfo {
            value: "opus".into(),
            display_name: "Opus".into(),
            description: "".into(),
        }],
        desired_model: Some("opus".into()),
        desired_effort: Some("high".into()),
    };
    let bytes = serde_json::to_vec(&p).unwrap();
    let back: PersistedSession = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(back.available_models.len(), 1);
    assert_eq!(back.available_models[0].value, "opus");
    assert_eq!(back.desired_model.as_deref(), Some("opus"));
    assert_eq!(back.desired_effort.as_deref(), Some("high"));
    // Old blobs without the new fields still decode (serde default).
    let old = serde_json::json!({"title":"t","entry_summaries":[]});
    let back2: PersistedSession = serde_json::from_value(old).unwrap();
    assert!(back2.available_models.is_empty());
    assert!(back2.desired_model.is_none());
    assert!(
        back2.desired_effort.is_none(),
        "old blobs without desired_effort decode to None via serde default"
    );
}

/// Cold path (no live `acp_thread`): `select_model` records `desired_model`
/// and `selected_model` reflects it, without any live connection. This is the
/// primary user scenario — picking a model on a cold tab before waking it.
#[gpui::test]
fn select_model_on_cold_session_records_desired(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let id = SolutionSessionId::new();
            let session = insert_cold_session(
                id,
                SolutionId("sol-a".into()),
                SharedString::from("claude-acp"),
                None,
                None,
                store,
                cx,
            );
            session.update(cx, |s, _| {
                s.cached_models = vec![
                    claude_native::ModelInfo {
                        value: "opus".into(),
                        display_name: "Opus".into(),
                        description: "".into(),
                    },
                    claude_native::ModelInfo {
                        value: "sonnet".into(),
                        display_name: "Sonnet".into(),
                        description: "".into(),
                    },
                ];
            });

            let models = store.session_models(id, cx);
            assert_eq!(models.len(), 2);
            assert_eq!(models[0].value, "opus");
            assert_eq!(models[1].value, "sonnet");

            assert!(store.selected_model(id, cx).is_none());

            store.select_model(id, "sonnet".into(), cx);

            assert_eq!(
                session.read(cx).desired_model.as_deref(),
                Some("sonnet"),
                "select_model must record desired_model on the cold session"
            );
            assert_eq!(
                store.selected_model(id, cx).as_deref(),
                Some("sonnet"),
                "selected_model must reflect the explicit desired_model"
            );
        });
    });
}

/// Cold path (no live `acp_thread`): `select_effort` records `desired_effort`,
/// `selected_effort` reflects it, and the chosen effort survives a persist
/// round-trip (`serializable_snapshot` → decode). Mirrors the model cold test;
/// this is the primary scenario — picking an effort on a cold tab before it
/// wakes. Without a live connection the `apply_flag_settings` push is a no-op,
/// so only the persisted `desired_effort` is asserted here.
#[gpui::test]
fn select_effort_on_cold_session_records_and_persists(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let id = SolutionSessionId::new();
            let session = insert_cold_session(
                id,
                SolutionId("sol-a".into()),
                SharedString::from("claude-acp"),
                None,
                None,
                store,
                cx,
            );

            assert!(
                store.selected_effort(id, cx).is_none(),
                "a fresh cold session has no chosen effort"
            );

            store.select_effort(id, "high".into(), cx);

            assert_eq!(
                session.read(cx).desired_effort.as_deref(),
                Some("high"),
                "select_effort must record desired_effort on the cold session"
            );
            assert_eq!(
                store.selected_effort(id, cx).as_deref(),
                Some("high"),
                "selected_effort must reflect the explicit desired_effort"
            );

            // Persist round-trip: the chosen effort survives snapshot + decode,
            // exercising both the snapshot converter and the serde field.
            let bytes = serializable_snapshot(session.read(cx), cx);
            let decoded: PersistedSession = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                decoded.desired_effort.as_deref(),
                Some("high"),
                "desired_effort must survive a persist round-trip"
            );
        });
    });
}

/// `new_chat_model_options` derives the model list + default selection for a
/// brand-new session from the pair's most-recently-active existing session:
/// its captured `cached_models` and chosen `desired_model`.
#[gpui::test]
fn new_chat_model_options_from_latest_session(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let sol = SolutionId("sol-a".into());
    let agent: AgentServerId = SharedString::from("claude-acp");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            // No sessions yet for the pair → empty list + None.
            let (models, selected) = store.new_chat_model_options(&sol, &agent, cx);
            assert!(models.is_empty());
            assert!(selected.is_none());

            let id = SolutionSessionId::new();
            let session =
                insert_cold_session(id, sol.clone(), agent.clone(), None, None, store, cx);
            session.update(cx, |s, _| {
                s.cached_models = vec![
                    claude_native::ModelInfo {
                        value: "opus".into(),
                        display_name: "Opus".into(),
                        description: "".into(),
                    },
                    claude_native::ModelInfo {
                        value: "sonnet".into(),
                        display_name: "Sonnet".into(),
                        description: "".into(),
                    },
                ];
                s.desired_model = Some("opus".into());
            });

            let (models, selected) = store.new_chat_model_options(&sol, &agent, cx);
            assert_eq!(models.len(), 2);
            assert_eq!(models[0].value, "opus");
            assert_eq!(models[1].value, "sonnet");
            assert_eq!(selected.as_deref(), Some("opus"));
        });
    });
}

/// A FRESH session (no turn yet → empty `cached_models`) must still offer a
/// model picker by falling back to the GLOBAL per-agent cache (`agent_models`),
/// which is filled by the first live capture / create-time probe of any session
/// of that agent. Mirrors the live capture by seeding `agent_models` directly.
#[gpui::test]
fn session_models_falls_back_to_global_agent_cache(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let sol = SolutionId("sol-a".into());
    let agent: AgentServerId = SharedString::from("claude-acp");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            // Stand in for the first live capture: a sibling session of this
            // agent populated the global cache.
            store.agent_models.insert(
                agent.clone(),
                vec![
                    claude_native::ModelInfo {
                        value: "opus".into(),
                        display_name: "Opus".into(),
                        description: "".into(),
                    },
                    claude_native::ModelInfo {
                        value: "sonnet".into(),
                        display_name: "Sonnet".into(),
                        description: "".into(),
                    },
                ],
            );

            // A brand-new session with an empty per-session list.
            let fresh_id = SolutionSessionId::new();
            let fresh =
                insert_cold_session(fresh_id, sol.clone(), agent.clone(), None, None, store, cx);
            assert!(
                fresh.read(cx).cached_models.is_empty(),
                "precondition: fresh session has no per-session model list"
            );

            let models = store.session_models(fresh_id, cx);
            assert_eq!(
                models.len(),
                2,
                "fresh session must surface the global agent model list"
            );
            assert_eq!(models[0].value, "opus");
            assert_eq!(models[1].value, "sonnet");
        });
    });
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
        let session = store
            .read(cx)
            .session(session_id)
            .expect("session exists");
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
            matches!(s.entries.last().unwrap().kind, SessionEntryKind::AssistantMessage { .. }),
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
            s.entries[0].created_ms,
            original_entry0_created_ms,
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
        assert_eq!(
            s.entries[0].mod_seq, 1,
            "entry0.mod_seq must be 1 after first NewEntry"
        );
        assert_eq!(
            s.entries[1].mod_seq, 2,
            "entry1.mod_seq must be 2 after second NewEntry"
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
            s.entries[0].mod_seq, 3,
            "entry0.mod_seq must advance to 3 after EntryUpdated(0)"
        );
        assert_eq!(
            s.entries[1].mod_seq, 2,
            "entry1.mod_seq must remain 2 (unchanged by EntryUpdated on entry0)"
        );
        assert_eq!(
            s.entries[0].created_ms,
            original_entry0_created_ms,
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

/// Phase 3, Task 3: cold-restored entries get ascending `mod_seq` (1-based)
/// and `change_seq` is re-seated so the first live `NewEntry` stamps the next
/// monotonic value.
///
/// Setup: create a session with a live thread, then replace its entries with 2
/// cold entries built by `rebuild_entries(base_seq=0)` and seed `change_seq`
/// via `init_change_seq_from_entries`.  Re-attach the live thread so the store
/// observes `NewEntry`.  Fire one user message and assert the new entry's
/// `mod_seq` == 3.
#[gpui::test]
async fn cold_restore_stamps_mod_seq_and_reseats_change_seq(cx: &mut TestAppContext) {
    use crate::cold_persistence::{
        PersistedAssistantChunk, PersistedAssistantMessage, PersistedEntryV2, PersistedUserMessage,
    };
    use crate::session_entry::SessionEntryKind;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Build 2 cold AgentThreadEntry values via the persisted → cold pipeline,
    // then call `rebuild_entries` with `base_seq = 0` to get stamped SessionEntries.
    let (cold_entries, created_ms) = cx.update(|cx| {
        crate::store::cold_entries_from_persisted(
            Some(crate::store::PersistedSession {
                title: "test".into(),
                entries: vec![],
                entry_summaries: vec![],
                entries_v2: vec![
                    PersistedEntryV2::User(PersistedUserMessage {
                        id: None,
                        content_md: "cold user".into(),
                        chunks: vec![],
                    }),
                    PersistedEntryV2::Assistant(PersistedAssistantMessage {
                        chunks: vec![PersistedAssistantChunk::Message("cold reply".into())],
                    }),
                ],
                entry_created_ms: vec![1_700_000_000_000, 1_700_000_001_000],
                available_models: vec![],
                desired_model: None,
                desired_effort: None,
            }),
            cx,
        )
    });

    // Inject the cold entries and re-seat change_seq, then re-attach the live
    // thread so the store's observe_task_notification hook sees subsequent NewEntry.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, cx| {
                let stamped = crate::session_entry::rebuild_entries(&cold_entries, &[], &created_ms, 0, cx);
                s.set_entries(stamped, cx);
                s.init_change_seq_from_entries();
                // Re-attach the live thread so live_base = 2 and the store
                // resumes observing AcpThreadEvent notifications.
                s.set_acp_thread(Some(acp_thread.clone()), cx);
            });
        });
    });
    cx.executor().run_until_parked();

    // Assert: mod_seq stamped 1..=2, change_seq re-seated to 2.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert_eq!(s.entries.len(), 2, "expected 2 cold entries");
        assert_eq!(
            s.entries[0].mod_seq, 1,
            "cold entry[0].mod_seq must be 1"
        );
        assert_eq!(
            s.entries[1].mod_seq, 2,
            "cold entry[1].mod_seq must be 2"
        );
        assert_eq!(
            s.change_seq, 2,
            "change_seq must be re-seated to 2 after cold restore"
        );
    });

    // Fire one live NewEntry; it must receive mod_seq == 3.
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
        assert_eq!(s.entries.len(), 3, "entries must be cold(2) + live(1) = 3");
        assert_eq!(
            s.entries[2].mod_seq, 3,
            "first live NewEntry must stamp mod_seq == 3"
        );
        assert!(
            matches!(s.entries[2].kind, SessionEntryKind::UserMessage { .. }),
            "live entry must be UserMessage"
        );
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
        store.read(cx).session(session_id).expect("session").read(cx).epoch
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
        store.read(cx).session(session_id).expect("session").read(cx).epoch
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
        store.read(cx).session(session_id).expect("session").read(cx).epoch
    });
    assert_eq!(
        epoch_after_reset,
        epoch_before + 1,
        "reset_context must bump epoch by exactly 1 (was {}, got {})",
        epoch_before,
        epoch_after_reset
    );
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
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.set_persistence(db.clone());
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
    let rows_before = db.load_entries(session_id).await.expect("load entries before");
    assert_eq!(rows_before.len(), 2, "two appends → two persisted rows before clear");

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
    let rows_after = db.load_entries(session_id).await.expect("load entries after");
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
