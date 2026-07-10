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
fn visible_session_count_excludes_live_judges(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sol = SolutionId("sol-a".into());
            let agent = SharedString::from("claude-acp");

            // Two ordinary user sessions + one judge child session.
            let supervised = SolutionSessionId::new();
            let other = SolutionSessionId::new();
            let judge = SolutionSessionId::new();
            for id in [supervised, other, judge] {
                insert_cold_session(id, sol.clone(), agent.clone(), None, None, store, cx);
            }

            // Before the judge is registered, all three count.
            assert_eq!(store.sessions_for(&sol).len(), 3);
            assert_eq!(store.visible_session_count(&sol), 3);

            // Register the judge handle (as `spawn_judge` does once create
            // resolves): supervised_id -> JudgeHandle { judge_id }.
            store.judge_sessions.insert(
                supervised,
                JudgeHandle {
                    judge_id: Some(judge),
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    nonce: String::new(),
                    _task: Task::ready(()),
                },
            );

            // The live judge is now excluded from the badge count, but the
            // raw session list is unchanged (judge is still reachable).
            assert_eq!(store.sessions_for(&sol).len(), 3);
            assert_eq!(store.visible_session_count(&sol), 2);

            // A judge handle whose create has not resolved yet (judge_id None)
            // excludes nothing.
            store.judge_sessions.remove(&supervised);
            store.judge_sessions.insert(
                supervised,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    nonce: String::new(),
                    _task: Task::ready(()),
                },
            );
            assert_eq!(store.visible_session_count(&sol), 3);
        });
    });
}

/// Bug #1: a HUMAN reply into a session whose supervisor is mid-`Judging`
/// supersedes the in-flight judge — its verdict is now stale (the user took
/// over direction), so it must NOT push an Observer nudge afterwards. The
/// reply tears the judge down and returns supervision to `Watching`; a verdict
/// that still races in is dropped by `apply_verdict`'s staleness guard.
#[gpui::test]
fn user_reply_supersedes_in_flight_judge(cx: &mut TestAppContext) {
    use crate::supervisor::{SupervisorState, SupervisorStatus, VerdictAction};
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sol = SolutionId("sol-a".into());
            let agent = SharedString::from("claude-acp");
            let id = SolutionSessionId::new();
            insert_cold_session(id, sol.clone(), agent.clone(), None, None, store, cx);

            // Supervisor is mid-Judging with a live judge handle (judge_id None
            // so finish_judge has no child session to close).
            let mut st = SupervisorState::new(id);
            st.enabled = true;
            st.status = SupervisorStatus::Judging;
            store.supervisor_states.insert(id, st);
            store.judge_sessions.insert(
                id,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    nonce: String::new(),
                    _task: Task::ready(()),
                },
            );

            // The human replies -> supersede the judge.
            store.supersede_judge_on_user_reply(id, cx);
            assert!(
                !store.judge_sessions.contains_key(&id),
                "in-flight judge must be torn down on a user reply"
            );
            assert!(
                matches!(
                    store.supervisor_states[&id].status,
                    SupervisorStatus::Watching
                ),
                "supervisor returns to Watching after the reply, got {:?}",
                store.supervisor_states[&id].status
            );

            // A verdict that races in AFTER the teardown is stale: it must NOT
            // act (no nudge, no continue-counter increment).
            store.apply_verdict(
                id,
                VerdictAction::Continue,
                "looks unfinished".into(),
                Some("Keep going.".into()),
                None,
                None,
                None,
                cx,
            );
            assert_eq!(
                store.supervisor_states[&id].consecutive_continues, 0,
                "a superseded verdict must be suppressed (no nudge / no counter bump)"
            );
        });
    });
}

/// Control for bug #1: when the judge is STILL in flight (no user reply), a
/// `Continue` verdict applies normally — it increments the continue counter and
/// nudges. Proves the staleness guard discriminates on the judge handle rather
/// than suppressing every verdict.
#[gpui::test]
fn verdict_applies_while_judge_in_flight(cx: &mut TestAppContext) {
    use crate::supervisor::{SupervisorState, SupervisorStatus, VerdictAction};
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sol = SolutionId("sol-a".into());
            let agent = SharedString::from("claude-acp");
            let id = SolutionSessionId::new();
            insert_cold_session(id, sol.clone(), agent.clone(), None, None, store, cx);

            let mut st = SupervisorState::new(id);
            st.enabled = true;
            st.status = SupervisorStatus::Judging;
            store.supervisor_states.insert(id, st);
            store.judge_sessions.insert(
                id,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    nonce: String::new(),
                    _task: Task::ready(()),
                },
            );

            store.apply_verdict(
                id,
                VerdictAction::Continue,
                "looks unfinished".into(),
                Some("Keep going.".into()),
                None,
                None,
                None,
                cx,
            );
            assert_eq!(
                store.supervisor_states[&id].consecutive_continues, 1,
                "a live verdict must apply (counter bumped)"
            );
        });
    });
}

/// The stuck watchdog must NOT treat a usage/session-limit wall as a hang. A
/// turn that hits the limit prints the wall as its last assistant message and
/// then stalls (Running, silent past STUCK_TURN_SECS, no in-progress tool) —
/// which looks wedged. Reconnecting + "carry on" there just re-hits the wall
/// and burns quota (the reported loop). Instead the session is stopped with
/// the wall message (NOT `reconnecting…`) and the supervisor is parked at
/// `Stopped(Quota)` (no parseable reset in this message).
#[gpui::test]
async fn stuck_usage_limit_wall_stops_without_reconnect(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // The turn's last assistant message is the limit wall.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(
                        "You've hit your session limit".to_string(),
                    ),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Force the wedged shape: Running, last activity well past STUCK_TURN_SECS
    // (5 min), no in-progress tool. Set the stale timestamp AFTER the push
    // (which bumps last_activity to now). Supervision on, to observe the stop.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.set_supervision_enabled(session_id, true, cx);
            let session = store.session(session_id).unwrap();
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
                s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(6 * 60);
            });
            store.tick_stuck_sessions(cx);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let state = store.session(session_id).unwrap().read(cx).state.clone();
            match state {
                SessionState::Errored(msg) => {
                    assert!(
                        msg.contains("session limit"),
                        "must stop with the wall message, got {msg:?}"
                    );
                    assert!(
                        !msg.contains("reconnect"),
                        "must NOT take the reconnect path, got {msg:?}"
                    );
                }
                other => panic!("expected Errored(wall), got {other:?}"),
            }
            let st = store.supervisor_state(session_id).unwrap();
            assert_eq!(
                st.status,
                crate::supervisor::SupervisorStatus::Stopped(
                    crate::supervisor::StoppedReason::Quota
                ),
                "supervisor parked at Stopped(Quota) for a no-reset limit"
            );
            assert!(!st.enabled);
        });
    });
}

/// MEDIUM-hardening #1: a usage/session-limit wall can arrive as a fast
/// `AcpThreadEvent::Error` (the worker's fast-error path), not only as the
/// silent stall the stuck-turn watchdog catches. For a SUPERVISED session the
/// Error arm must classify the wall from the session's own last assistant
/// message and hand off to quota recovery — scheduling an auto-resume at the
/// reset — instead of latching the generic "agent error" and losing the reset.
#[gpui::test]
async fn error_arm_supervised_wall_schedules_resume(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.set_supervision_enabled(session_id, true, cx)
        });
    });

    // The turn's last assistant message is a limit wall WITH a parseable reset.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(
                        "You've hit your session limit · resets 8:20pm (Asia/Novosibirsk)"
                            .to_string(),
                    ),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // The fast-error wall (not a silent stall).
    cx.update(|cx| acp_thread.update(cx, |_t, cx| cx.emit(acp_thread::AcpThreadEvent::Error)));
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, cx| {
            let state = store.session(session_id).unwrap().read(cx).state.clone();
            match state {
                SessionState::Errored(msg) => assert!(
                    msg.contains("session limit"),
                    "Error arm must surface the wall text, got {msg:?}"
                ),
                other => panic!("expected Errored(wall), got {other:?}"),
            }
            let st = store.supervisor_state(session_id).unwrap();
            assert_eq!(
                st.status,
                crate::supervisor::SupervisorStatus::Watching,
                "a parseable-reset wall parks Watching (not Stopped)"
            );
            assert!(
                st.next_eligible_ms.is_some(),
                "an auto-resume must be scheduled at the reset"
            );
            assert!(
                store.backoff_timers.contains_key(&session_id),
                "the resume wake timer must be armed"
            );
        });
    });
}

/// The same fast-error wall on an UNSUPERVISED session must still surface the
/// wall text (so the user sees when it resets) rather than the generic "agent
/// error" — but must NOT fabricate a supervisor row or schedule a resume.
#[gpui::test]
async fn error_arm_unsupervised_wall_surfaces_text_without_resume(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(
                        "You've hit your session limit".to_string(),
                    ),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| acp_thread.update(cx, |_t, cx| cx.emit(acp_thread::AcpThreadEvent::Error)));
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, cx| {
            let state = store.session(session_id).unwrap().read(cx).state.clone();
            match state {
                SessionState::Errored(msg) => {
                    assert!(
                        msg.contains("session limit"),
                        "must surface the wall text, got {msg:?}"
                    );
                    assert_ne!(msg.as_ref(), "agent error");
                }
                other => panic!("expected Errored(wall), got {other:?}"),
            }
            assert!(
                store.supervisor_states.get(&session_id).is_none(),
                "no supervisor row may be fabricated for an unsupervised session",
            );
        });
    });
}

/// Regression for the turn-boundary anchor in `session_wall_message`: after a
/// wall latches, the observer sends a `continue` user message and the resumed
/// turn fast-errors BEFORE streaming any assistant chunk (dead subprocess /
/// network drop on wake). The stale prior-turn wall is still the transcript's
/// last assistant message — but the scan must stop at the intervening user
/// message and classify this as a plain transient error ("agent error"), NOT a
/// fresh wall. Otherwise supervision is parked on a bogus ~24h resume.
#[gpui::test]
async fn error_arm_stale_wall_behind_user_message_is_not_reclassified(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.set_supervision_enabled(session_id, true, cx)
        });
    });

    // A prior turn's wall, then the observer's `continue` nudge as a fresh user
    // message opening a new (failing) turn that streams nothing.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(
                        "You've hit your session limit · resets 8:20pm (Asia/Novosibirsk)"
                            .to_string(),
                    ),
                ),
                false,
                cx,
            );
            t.push_user_content_block(
                None,
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("continue".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // The resumed turn fast-errors with no assistant output this turn.
    cx.update(|cx| acp_thread.update(cx, |_t, cx| cx.emit(acp_thread::AcpThreadEvent::Error)));
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, cx| {
            let state = store.session(session_id).unwrap().read(cx).state.clone();
            match state {
                SessionState::Errored(msg) => assert_eq!(
                    msg.as_ref(),
                    "agent error",
                    "a stale wall behind a user message must NOT reclassify a transient error"
                ),
                other => panic!("expected Errored(agent error), got {other:?}"),
            }
            let st = store.supervisor_state(session_id).unwrap();
            assert!(
                st.next_eligible_ms.is_none(),
                "no bogus resume may be scheduled from a stale wall"
            );
            assert!(
                !store.backoff_timers.contains_key(&session_id),
                "no resume wake timer may be armed from a stale wall"
            );
        });
    });
}

/// MEDIUM-hardening #3: the judge submits its verdict via the solution-scoped
/// `supervisor_verdict` tool, which is served ONLY on the per-solution socket —
/// the editor-global socket doesn't carry it. If the per-solution socket can't
/// be resolved (startup race / socket not opened — which is the case in this
/// headless test, where no MCP `ActiveServer` global exists), spawning a judge
/// briefed with the global socket would guarantee a JUDGE_TIMEOUT → bogus
/// backoff spiral. Instead the spawn is skipped and the supervisor reverts
/// `Judging → Watching` so the next tick retries once the socket is up.
#[gpui::test]
async fn judge_spawn_skipped_when_solution_socket_unresolvable(cx: &mut TestAppContext) {
    let (session_id, _acp_thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.set_supervision_enabled(session_id, true, cx);
            // The caller (tick_supervisor) flips to Judging before spawn_judge.
            store.supervisor_states.get_mut(&session_id).unwrap().status =
                crate::supervisor::SupervisorStatus::Judging;
            store.spawn_judge(session_id, cx);
        });
    });
    cx.executor().run_until_parked();
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, _| {
            assert!(
                !store.judge_sessions.contains_key(&session_id),
                "no judge handle when the per-solution socket can't be resolved"
            );
            let st = store.supervisor_state(session_id).unwrap();
            assert_eq!(
                st.status,
                crate::supervisor::SupervisorStatus::Watching,
                "a skipped judge spawn reverts Judging → Watching so the next tick retries"
            );
            assert!(
                st.next_eligible_ms.is_some(),
                "the retry is gated so the 1 Hz tick doesn't re-fire→re-skip every second"
            );
        });
    });
}

/// LOW-hardening #8: the verdict tool passes no token figure (always `None` in
/// production), so `VerdictRecord.tokens` — and the `total_tokens` stat — read 0.
/// `apply_verdict` now fills it from the live judge session's own usage.
#[gpui::test]
async fn acted_verdict_records_judge_tokens(cx: &mut TestAppContext) {
    let (supervised_id, _thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.set_supervision_enabled(supervised_id, true, cx);
            store.supervisor_states.get_mut(&supervised_id).unwrap().status =
                crate::supervisor::SupervisorStatus::Judging;
            // Idle so the verdict is ACTED (not dropped by the send-time gate).
            store.session(supervised_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Idle;
            });
            let (solution_id, agent_id) = {
                let s = store.session(supervised_id).unwrap();
                let s = s.read(cx);
                (s.solution_id.clone(), s.agent_id.clone())
            };
            // A judge session carrying a known token figure.
            let judge_id = crate::model::SolutionSessionId::new();
            let judge = crate::store::tests::insert_cold_session(
                judge_id,
                solution_id,
                agent_id,
                Some(4321),
                None,
                store,
                cx,
            );
            judge.update(cx, |j, _| j.cached_total_tokens = Some(4321));
            store.judge_sessions.insert(
                supervised_id,
                JudgeHandle {
                    judge_id: Some(judge_id),
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    nonce: String::new(),
                    _task: Task::ready(()),
                },
            );
            store.apply_verdict(
                supervised_id,
                crate::supervisor::VerdictAction::Continue,
                "keep going".into(),
                None,
                None,
                // Production passes None here; the judge-session fill supplies it.
                None,
                None,
                cx,
            );
        });
    });
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, cx| {
            let root = store.solution_root_for_app(supervised_id, cx).expect("root");
            let dir = crate::supervisor::supervisor_dir(&root, supervised_id);
            let recs = crate::supervisor::read_verdicts(&dir);
            assert_eq!(recs.len(), 1);
            assert!(!recs[0].dropped, "verdict must be acted for this assertion");
            assert_eq!(
                recs[0].tokens,
                Some(4321),
                "the acted verdict records the judge session's token usage"
            );
            assert_eq!(
                crate::supervisor::verdict_stats(&recs).total_tokens,
                4321,
                "total_tokens is no longer always 0"
            );
        });
    });
}

/// MEDIUM-hardening #5: the judge-stuck watchdog measures wall-clock from the
/// fire, but a thorough judge (reading files, running read-only Bash) can run
/// past `JUDGE_TIMEOUT_SECS` while still streaming. Once the wall-clock timeout
/// is crossed, a judge whose OWN session is still active (recent
/// `last_activity_at`) must be EXTENDED, not killed mid-verdict.
#[gpui::test]
async fn judge_past_timeout_but_streaming_is_not_killed(cx: &mut TestAppContext) {
    let (supervised_id, _thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let now_ms = chrono::Utc::now().timestamp_millis();
            store.set_supervision_enabled(supervised_id, true, cx);
            let st = store.supervisor_states.get_mut(&supervised_id).unwrap();
            st.status = crate::supervisor::SupervisorStatus::Judging;
            st.last_fired_at =
                Some(now_ms - (crate::supervisor::JUDGE_TIMEOUT_SECS as i64 + 60) * 1000);
            let (solution_id, agent_id) = {
                let s = store.session(supervised_id).unwrap();
                let s = s.read(cx);
                (s.solution_id.clone(), s.agent_id.clone())
            };
            // A judge session that streamed just now → still alive.
            let judge_id = crate::model::SolutionSessionId::new();
            let judge =
                crate::store::tests::insert_cold_session(judge_id, solution_id, agent_id, None, None, store, cx);
            judge.update(cx, |j, _| j.last_activity_at = chrono::Utc::now());
            store.judge_sessions.insert(
                supervised_id,
                JudgeHandle {
                    judge_id: Some(judge_id),
                    started_ms: now_ms,
                    nonce: String::new(),
                    _task: Task::ready(()),
                },
            );
            store.tick_supervisor(cx);
        });
    });
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.supervisor_state(supervised_id).unwrap().status,
                crate::supervisor::SupervisorStatus::Judging,
                "a still-streaming judge past the wall-clock timeout must not be killed"
            );
            assert!(
                store.judge_sessions.contains_key(&supervised_id),
                "the judge handle is retained while it's alive"
            );
        });
    });
}

/// Contrast to the liveness test: a judge past the wall-clock timeout whose own
/// session has gone SILENT (no streaming) longer than `JUDGE_LIVENESS_SILENCE_SECS`
/// is genuinely stuck → killed as a transient failure (backoff → Watching).
#[gpui::test]
async fn judge_past_timeout_and_silent_is_killed(cx: &mut TestAppContext) {
    let (supervised_id, _thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let now_ms = chrono::Utc::now().timestamp_millis();
            store.set_supervision_enabled(supervised_id, true, cx);
            let st = store.supervisor_states.get_mut(&supervised_id).unwrap();
            st.status = crate::supervisor::SupervisorStatus::Judging;
            st.last_fired_at =
                Some(now_ms - (crate::supervisor::JUDGE_TIMEOUT_SECS as i64 + 60) * 1000);
            let (solution_id, agent_id) = {
                let s = store.session(supervised_id).unwrap();
                let s = s.read(cx);
                (s.solution_id.clone(), s.agent_id.clone())
            };
            // A judge session silent well past the liveness window → dead.
            let judge_id = crate::model::SolutionSessionId::new();
            let judge =
                crate::store::tests::insert_cold_session(judge_id, solution_id, agent_id, None, None, store, cx);
            judge.update(cx, |j, _| {
                j.last_activity_at = chrono::Utc::now()
                    - chrono::Duration::seconds(
                        crate::supervisor::JUDGE_LIVENESS_SILENCE_SECS as i64 + 30,
                    );
            });
            store.judge_sessions.insert(
                supervised_id,
                JudgeHandle {
                    judge_id: Some(judge_id),
                    started_ms: now_ms,
                    nonce: String::new(),
                    _task: Task::ready(()),
                },
            );
            store.tick_supervisor(cx);
        });
    });
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, _| {
            assert_ne!(
                store.supervisor_state(supervised_id).unwrap().status,
                crate::supervisor::SupervisorStatus::Judging,
                "a silent (dead) judge past the timeout must be un-wedged"
            );
            assert!(
                !store.judge_sessions.contains_key(&supervised_id),
                "the dead judge handle is reaped"
            );
        });
    });
}

/// Verify that persisted supervisor states are merged into the store map as
/// soon as `set_persistence` is called — not lazily deferred to per-solution
/// hydration. This is the "restart" path: supervisor toggles written to the
/// DB in a previous run must be visible in the fresh store after init.
#[gpui::test]
async fn supervisor_states_loaded_at_persistence_init(cx: &mut gpui::TestAppContext) {
    // Open a DB and write a supervisor state row directly, simulating work
    // done in a previous session. The in-memory DB is keyed by thread name,
    // so a second `open` call below shares the same data.
    let session_id = crate::model::SolutionSessionId::new();
    let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
    let state = crate::supervisor::SupervisorState {
        session_id,
        enabled: true,
        custom_prompt: Some("reload me".into()),
        consecutive_continues: 0,
        backoff_attempt: 0,
        last_fired_at: None,
        next_eligible_ms: None,
        status: crate::supervisor::SupervisorStatus::Watching,
        trigger_count: 0,
        last_user_input_ms: None,
        judge_superseded: false,
        held_by_done: false,
        pending_nudge: None,
        wait_until_ms: None,
        watch_started_ms: None,
    };
    db.save_supervisor_state(state.clone())
        .await
        .expect("save_supervisor_state");

    // Create a fresh store (no prior in-memory state) and attach persistence.
    let registry = Arc::new(AdapterRegistry::new());
    let store: gpui::Entity<SolutionAgentStore> =
        cx.update(|cx| cx.new(|cx| SolutionAgentStore::new_in_app(registry, cx)));

    // Pre-condition: the fresh store has no supervisor state for our session.
    let pre = store.read_with(cx, |s, _| s.supervisor_state(session_id));
    assert!(
        pre.is_none(),
        "fresh store must not have supervisor state yet"
    );

    // Open the same shared in-memory DB and call set_persistence — this
    // must fire the one-time load.
    let db2 = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db again"));
    store.update(cx, |store, cx| {
        store.set_persistence(db2, cx);
    });

    // Pump the executor so the spawned load task runs to completion.
    cx.run_until_parked();

    // Post-condition: the loaded row is now in the map.
    let loaded = store
        .read_with(cx, |s, _| s.supervisor_state(session_id))
        .expect("supervisor state must be present after persistence init");
    assert!(loaded.enabled, "enabled must be restored from DB");
    assert_eq!(
        loaded.custom_prompt.as_deref(),
        Some("reload me"),
        "custom_prompt must be restored from DB"
    );
    assert_eq!(loaded.status, crate::supervisor::SupervisorStatus::Watching);
    // The restart/load path stamps a fresh watch baseline so an inherited idle
    // session doesn't fire a judge the instant the editor reopens.
    assert!(
        loaded.watch_started_ms.is_some(),
        "load must anchor the idle clock to process start"
    );
}

/// Regression for "reopened the editor and the observer fired on a parked
/// session by itself": after a restart the operator resumes each session by
/// hand — the supervisor must NOT auto-resume a session that was already idle
/// before the restart. A restored row carries a `watch_started_ms` baseline;
/// `tick_supervisor` fires only once the session produces genuinely-new
/// activity THIS process (its `last_activity_at` moves past the baseline, e.g.
/// a manual kick starts a turn). Three cases pin the whole gate.
#[gpui::test]
async fn restart_leaves_inherited_idle_until_fresh_activity(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    let now_ms = chrono::Utc::now().timestamp_millis();

    // Case A — inherited idle (activity predates the watch baseline): the
    // session was parked before the restart, so it stays untouched even though
    // it is idle and silent well past the threshold.
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id, true, cx);
        // Restart/load path: we started watching just now, AFTER the last activity.
        store
            .supervisor_states
            .get_mut(&id)
            .unwrap()
            .watch_started_ms = Some(now_ms);
        store.tick_supervisor(cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Watching,
        "a session parked before the restart must NOT auto-resume on reopen"
    );
    assert!(st.last_fired_at.is_none(), "no judge should have fired");

    // Case B — genuinely-new activity under our watch (a manual kick's turn
    // completed: last_activity is now AFTER the baseline, and silent past the
    // threshold): the normal idle-nudge cycle re-engages and fires.
    store.update(cx, |store, cx| {
        // Baseline sits 5 min back; the fresh activity landed 2 min ago.
        let st = store.supervisor_states.get_mut(&id).unwrap();
        st.watch_started_ms = Some(now_ms - 300_000);
        store.tick_supervisor(cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Judging,
        "once the session has fresh activity under our watch, the cycle re-engages"
    );

    // Case C — a FRESH in-session enable (no baseline) is always eligible: its
    // idle arose under our watch, so immediate-idle semantics are unchanged.
    let (store2, id2, _tmp2) = crate::store::test_support::seed_store_with_session(cx).await;
    store2.update(cx, |store, cx| {
        let session = store.session(id2).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id2, true, cx);
        assert!(
            store
                .supervisor_states
                .get(&id2)
                .unwrap()
                .watch_started_ms
                .is_none()
        );
        store.tick_supervisor(cx);
    });
    let st2 = store2
        .read_with(cx, |store, _| store.supervisor_state(id2))
        .unwrap();
    assert_eq!(
        st2.status,
        crate::supervisor::SupervisorStatus::Judging,
        "a fresh in-session enable fires on idle as before (no baseline gate)"
    );
}

#[gpui::test]
async fn toggle_supervision_persists_and_reads_back(cx: &mut gpui::TestAppContext) {
    let (store, session_id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;

    store.update(cx, |store, cx| {
        store.set_supervision_enabled(session_id, true, cx);
        store.set_supervisor_prompt(session_id, Some("verify FORK.md updated".into()), cx);
    });

    let st = store
        .read_with(cx, |store, _| store.supervisor_state(session_id))
        .unwrap();
    assert!(st.enabled);
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Watching);
    assert_eq!(st.custom_prompt.as_deref(), Some("verify FORK.md updated"));

    store.update(cx, |store, cx| {
        store.set_supervision_enabled(session_id, false, cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(session_id))
        .unwrap();
    assert!(!st.enabled);
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Disabled);
}

#[gpui::test]
async fn tick_fires_judge_after_threshold(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    // Force the session Idle with last_activity 2 minutes ago.
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id, true, cx);
    });

    store.update(cx, |store, cx| store.tick_supervisor(cx));

    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Judging);
    assert!(st.last_fired_at.is_some());

    // Second tick must NOT re-fire (already Judging).
    store.update(cx, |store, cx| {
        if let Some(s) = store.supervisor_states.get(&id) {
            assert_eq!(s.status, crate::supervisor::SupervisorStatus::Judging);
        }
        store.tick_supervisor(cx);
    });
    let st2 = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st2.last_fired_at, st.last_fired_at,
        "must not re-fire while judging"
    );
}

/// Manual user-stop parks the supervisor in `Held`: even an idle, long-silent
/// session never fires a judge while held, and the next human message re-arms it
/// to `Watching`. This is the "I stopped the agent myself; don't let the
/// observer drag it back to work until I say so" guarantee.
#[gpui::test]
async fn manual_stop_holds_supervisor_then_message_rearms(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id, true, cx);
        // The user manually stops the agent.
        store.hold_supervisor(id, cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Held,
        "manual stop parks the supervisor on hold"
    );
    assert!(
        st.enabled,
        "Held keeps supervision enabled (it's a pause, not a disable)"
    );

    // A tick while Held must NOT spawn a judge, despite the idle, silent session.
    store.update(cx, |store, cx| store.tick_supervisor(cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Held,
        "a held supervisor never fires on the current dialog state"
    );

    // The user sends a new message → re-arm to Watching.
    store.update(cx, |store, cx| {
        store.reset_supervisor_continue_counter(id, cx)
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Watching,
        "the next human message re-arms the held supervisor"
    );
}

/// Live typing defers the watchdog: a fresh keystroke (`note_user_input`) makes
/// the supervisor treat the session as active, so an otherwise-fireable idle
/// session does NOT get a judge until the typing grace elapses. Prevents the
/// observer firing its own nudge while the user is mid-message.
#[gpui::test]
async fn typing_defers_supervisor_tick(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id, true, cx);
        // The user is mid-message: a keystroke just landed.
        store.note_user_input(id);
    });

    store.update(cx, |store, cx| store.tick_supervisor(cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Watching,
        "a recent keystroke defers the supervisor tick — no judge mid-typing"
    );

    // Clear the typing marker (grace window elapsed) → the tick now fires.
    store.update(cx, |store, _| {
        if let Some(s) = store.supervisor_states.get_mut(&id) {
            s.last_user_input_ms = None;
        }
    });
    store.update(cx, |store, cx| store.tick_supervisor(cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Judging,
        "with no recent typing the idle, silent session fires a judge"
    );
}

/// Hold-on-typing: a judge that fired while the user was idle but FINISHED
/// after the user started composing must NOT drop its nudge into the middle of
/// the user's message. The verdict is accepted (the continue-counter bumps) but
/// its nudge is parked in `pending_nudge`; once the user goes quiet for the idle
/// window, `tick_supervisor` flushes it (without firing a fresh judge).
#[gpui::test]
async fn observer_nudge_held_while_typing_then_flushed(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::{SupervisorStatus, VerdictAction};
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id, true, cx);
        // Simulate a judge that already fired and is mid-review.
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(st) = store.supervisor_states.get_mut(&id) {
            st.status = SupervisorStatus::Judging;
            st.last_fired_at = Some(now);
        }
        store.judge_sessions.insert(
            id,
            JudgeHandle {
                judge_id: None,
                started_ms: now,
                nonce: String::new(),
                _task: Task::ready(()),
            },
        );
        // The user starts typing WHILE the judge is running.
        store.note_user_input(id);
        // The judge delivers a Continue verdict — but the user is mid-message.
        store.apply_verdict(
            id,
            VerdictAction::Continue,
            "keep going".into(),
            Some("Continue please.".into()),
            None,
            None,
            None,
            cx,
        );
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.consecutive_continues, 1,
        "the verdict is accepted (counter bumps) even though its nudge is held"
    );
    assert_eq!(
        st.pending_nudge.as_deref(),
        Some("Continue please."),
        "the nudge is parked, not delivered, while the user is typing"
    );

    // The user changes their mind and goes quiet past the idle window.
    store.update(cx, |store, cx| {
        let quiet = chrono::Utc::now().timestamp_millis()
            - (crate::supervisor::IDLE_THRESHOLD_SECS as i64 + 5) * 1000;
        if let Some(st) = store.supervisor_states.get_mut(&id) {
            st.last_user_input_ms = Some(quiet);
        }
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.tick_supervisor(cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.pending_nudge, None,
        "once the user is quiet for the idle window, the held nudge flushes"
    );
    assert_eq!(
        st.status,
        SupervisorStatus::Watching,
        "flushing a held nudge must not fire a fresh judge"
    );
}

/// Stuck-watchdog decision (#7): a session silent past `STUCK_TURN_SECS` is
/// wedged when no tool is running (hung between steps), or when its in-progress
/// tool has BOTH exceeded `TOOL_STUCK_SECS` AND stopped showing liveness. A long
/// foreground build that keeps streaming output (`shows_liveness == true`) is
/// never wedged, so it isn't reconnected out from under itself.
#[test]
fn turn_wedged_decision_gates_on_tool_liveness() {
    // No in-progress tool: claude hung between steps → wedged.
    assert!(turn_is_wedged(None));
    // A young tool (under the backstop) is never wedged, live or not.
    assert!(!turn_is_wedged(Some((60, false))));
    assert!(!turn_is_wedged(Some((60, true))));
    // Past the backstop but STILL streaming output (live build) → NOT wedged.
    assert!(!turn_is_wedged(Some((TOOL_STUCK_SECS as i64 + 1, true))));
    // Past the backstop AND no liveness (truly hung command) → wedged.
    assert!(turn_is_wedged(Some((TOOL_STUCK_SECS as i64 + 1, false))));
    // Exactly at the backstop with no liveness → wedged (the bound is `>=`).
    assert!(turn_is_wedged(Some((TOOL_STUCK_SECS as i64, false))));
    // At the backstop but live → not wedged.
    assert!(!turn_is_wedged(Some((TOOL_STUCK_SECS as i64, true))));
}

/// Verdict authentication (#6): a `supervisor_verdict` call is honoured only
/// when its nonce matches the in-flight judge's briefing nonce. A wrong nonce is
/// rejected without touching state (the real judge can still submit); a matching
/// nonce applies the verdict AND reaps the judge handle, so a duplicate re-submit
/// (bridge-EOF retry) then finds no in-flight judge and is an idempotent no-op —
/// no second nudge, no double counter bump.
#[gpui::test]
async fn verdict_nonce_authenticates_and_dedups(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::{SupervisorStatus, VerdictAction};
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id, true, cx);
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(st) = store.supervisor_states.get_mut(&id) {
            st.status = SupervisorStatus::Judging;
            st.last_fired_at = Some(now);
        }
        store.judge_sessions.insert(
            id,
            JudgeHandle {
                judge_id: None,
                started_ms: now,
                nonce: "goodnonce".into(),
                _task: Task::ready(()),
            },
        );
        // Park the delivered nudge (user "typing") so the accepted verdict bumps
        // the counter without needing a live thread to deliver into.
        store.note_user_input(id);

        // Wrong nonce → rejected, no state change, judge still in flight.
        let bad = store.apply_verdict_authenticated(
            id,
            "wrongnonce",
            VerdictAction::Continue,
            "forged".into(),
            Some("Continue please.".into()),
            None,
            None,
            cx,
        );
        assert!(
            matches!(bad, VerdictAuth::Unauthorized),
            "a mismatched nonce is rejected"
        );
        assert!(
            store.judge_sessions.contains_key(&id),
            "a rejected verdict must NOT reap the judge handle"
        );
        assert_eq!(
            store.supervisor_state(id).unwrap().consecutive_continues,
            0,
            "a rejected verdict does not act"
        );

        // Correct nonce → applied (counter bumps, nudge parked) and handle reaped.
        let ok = store.apply_verdict_authenticated(
            id,
            "goodnonce",
            VerdictAction::Continue,
            "keep going".into(),
            Some("Continue please.".into()),
            None,
            None,
            cx,
        );
        assert!(matches!(ok, VerdictAuth::Applied), "matching nonce applies");
        assert!(
            !store.judge_sessions.contains_key(&id),
            "applying a verdict reaps the judge handle"
        );
        let st = store.supervisor_state(id).unwrap();
        assert_eq!(st.consecutive_continues, 1, "the verdict acted");
        assert_eq!(st.pending_nudge.as_deref(), Some("Continue please."));

        // Duplicate re-submit (bridge-EOF retry) → no in-flight judge → no-op.
        let dup = store.apply_verdict_authenticated(
            id,
            "goodnonce",
            VerdictAction::Continue,
            "retry".into(),
            Some("Continue please.".into()),
            None,
            None,
            cx,
        );
        assert!(
            matches!(dup, VerdictAuth::NoInFlight),
            "a re-submit after the handle is reaped is a no-op, not a second act"
        );
        assert_eq!(
            store.supervisor_state(id).unwrap().consecutive_continues,
            1,
            "the duplicate did NOT bump the counter again"
        );
    });
}

/// Audit-verdict authentication (#6): the meta-auditor path enforces the same
/// nonce + in-flight-handle gate, keyed on `auditor_sessions`.
#[gpui::test]
async fn audit_verdict_nonce_authenticates_and_dedups(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        let now = chrono::Utc::now().timestamp_millis();
        store.auditor_sessions.insert(
            id,
            JudgeHandle {
                judge_id: None,
                started_ms: now,
                nonce: "auditnonce".into(),
                _task: Task::ready(()),
            },
        );

        // Wrong nonce → rejected, auditor handle preserved.
        let bad =
            store.apply_audit_verdict_authenticated(id, "nope", false, true, "forged".into(), cx);
        assert!(matches!(bad, VerdictAuth::Unauthorized));
        assert!(
            store.auditor_sessions.contains_key(&id),
            "a rejected audit verdict must NOT reap the auditor handle"
        );

        // Correct nonce → applied, handle reaped.
        let ok = store.apply_audit_verdict_authenticated(
            id,
            "auditnonce",
            true,
            false,
            "healthy".into(),
            cx,
        );
        assert!(matches!(ok, VerdictAuth::Applied));
        assert!(!store.auditor_sessions.contains_key(&id));

        // Re-submit → no in-flight auditor → idempotent no-op.
        let dup = store.apply_audit_verdict_authenticated(
            id,
            "auditnonce",
            true,
            false,
            "retry".into(),
            cx,
        );
        assert!(matches!(dup, VerdictAuth::NoInFlight));
    });
}

/// Audit / interrupt: turning supervision OFF while a judge is mid-review tears
/// the judge down immediately, discards any held nudge, and a verdict that still
/// races out of the torn-down judge is dropped by the send-time gate (no nudge,
/// no counter bump). Previously disabling did NOT stop a running observer.
#[gpui::test]
async fn disabling_supervision_interrupts_running_judge(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::{SupervisorStatus, VerdictAction};
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(st) = store.supervisor_states.get_mut(&id) {
            st.status = SupervisorStatus::Judging;
            st.last_fired_at = Some(now);
            st.pending_nudge = Some("held".into());
        }
        store.judge_sessions.insert(
            id,
            JudgeHandle {
                judge_id: None,
                started_ms: now,
                nonce: String::new(),
                _task: Task::ready(()),
            },
        );
        store.set_supervision_enabled(id, false, cx);
    });
    store.read_with(cx, |store, _| {
        assert!(
            !store.judge_sessions.contains_key(&id),
            "disabling supervision must tear down the in-flight judge"
        );
        let st = store.supervisor_state(id).unwrap();
        assert!(matches!(st.status, SupervisorStatus::Disabled));
        assert_eq!(st.pending_nudge, None, "disabling discards a held nudge");
    });
    // A verdict racing out of the torn-down judge must be dropped.
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            VerdictAction::Continue,
            "keep going".into(),
            Some("Continue.".into()),
            None,
            None,
            None,
            cx,
        );
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.consecutive_continues, 0,
        "a verdict from a disabled supervisor is dropped (no nudge, no counter bump)"
    );
}

/// Audit: a verdict that arrives after the user hit Stop (supervision parked in
/// `Held`) is dropped by the send-time gate. `hold_supervisor` already tore the
/// judge down; this proves the racing-verdict backstop (previously a verdict
/// racing in after a manual Stop still nudged the agent).
#[gpui::test]
async fn held_supervisor_drops_racing_verdict(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::{SupervisorStatus, VerdictAction};
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(st) = store.supervisor_states.get_mut(&id) {
            st.status = SupervisorStatus::Judging;
            st.last_fired_at = Some(now);
        }
        store.judge_sessions.insert(
            id,
            JudgeHandle {
                judge_id: None,
                started_ms: now,
                nonce: String::new(),
                _task: Task::ready(()),
            },
        );
        store.hold_supervisor(id, cx);
    });
    store.read_with(cx, |store, _| {
        assert!(!store.judge_sessions.contains_key(&id));
        assert!(matches!(
            store.supervisor_state(id).unwrap().status,
            SupervisorStatus::Held
        ));
    });
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            VerdictAction::Continue,
            "keep going".into(),
            Some("Continue.".into()),
            None,
            None,
            None,
            cx,
        );
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.consecutive_continues, 0,
        "a verdict arriving after a manual Stop (Held) is dropped"
    );
    // The dropped verdict is still logged, but flagged `dropped:true` so the
    // meta-auditor / `verdict_stats` don't miscount it as an acted nudge.
    store.read_with(cx, |store, cx| {
        let root = store.solution_root_for_app(id, cx).expect("solution root");
        let dir = crate::supervisor::supervisor_dir(&root, id);
        let recs = crate::supervisor::read_verdicts(&dir);
        assert_eq!(recs.len(), 1, "the dropped verdict is still recorded");
        assert!(recs[0].dropped, "a gated verdict must be marked dropped");
        let stats = crate::supervisor::verdict_stats(&recs);
        assert_eq!(stats.total, 0, "verdict_stats excludes the dropped verdict");
    });
}

/// The user sending their own message forgets a nudge that was parked waiting
/// for them to stop typing. The clear happens in the single user-send funnel
/// unconditionally, because the judge is already gone once a nudge is held.
#[gpui::test]
async fn user_send_discards_held_nudge(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        if let Some(st) = store.supervisor_states.get_mut(&id) {
            st.pending_nudge = Some("stale observer nudge".into());
        }
    });
    // The user sends their own reply through the single funnel (from_user).
    store.update(cx, |store, cx| {
        let blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
            "my own reply".to_string(),
        ))];
        store
            .send_message_blocks_targeted(id, blocks, crate::model::QueueTarget::Main, true, cx)
            .detach();
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.pending_nudge, None,
        "the user's own message forgets the held observer nudge"
    );
}

/// Interrupt: changing the supervisor's instruction while a judge is mid-review
/// tears that judge down (its verdict is stale — it reviewed under the old
/// instruction) and returns to `Watching` so the next tick re-fires under the
/// new instruction. A verdict racing out of the old judge is then dropped.
#[gpui::test]
async fn changing_instruction_interrupts_running_judge(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::{SupervisorStatus, VerdictAction};
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(st) = store.supervisor_states.get_mut(&id) {
            st.status = SupervisorStatus::Judging;
            st.last_fired_at = Some(now);
        }
        store.judge_sessions.insert(
            id,
            JudgeHandle {
                judge_id: None,
                started_ms: now,
                nonce: String::new(),
                _task: Task::ready(()),
            },
        );
        store.set_supervisor_prompt(id, Some("review only the tests".into()), cx);
    });
    store.read_with(cx, |store, _| {
        assert!(
            !store.judge_sessions.contains_key(&id),
            "changing the instruction tears down the stale in-flight judge"
        );
        assert!(matches!(
            store.supervisor_state(id).unwrap().status,
            SupervisorStatus::Watching
        ));
    });
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            VerdictAction::Continue,
            "keep going".into(),
            Some("Continue.".into()),
            None,
            None,
            None,
            cx,
        );
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.consecutive_continues, 0,
        "a verdict from a judge interrupted by an instruction change is dropped"
    );
}

/// A session sitting idle OVER a running background command must not wake the
/// supervisor — the agent is legitimately waiting on that work, and hung
/// background commands are watched elsewhere. Once the shell exits, the idle
/// session fires normally.
#[gpui::test]
async fn background_command_suppresses_supervisor_tick(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::SupervisorStatus;
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
            let shell_id = crate::background_shell::BackgroundShellId::new("shell-1");
            s.background_shells.insert(
                shell_id.clone(),
                crate::background_shell::BackgroundShell {
                    id: shell_id,
                    command: "sleep 100".into(),
                    output_path: std::path::PathBuf::from("/tmp/x.output"),
                    registered_at: chrono::Utc::now(),
                    latest: None,
                    last_offset: 0,
                    state: crate::background_shell::ShellRuntimeState::Running,
                },
            );
        });
        store.set_supervision_enabled(id, true, cx);
        store.tick_supervisor(cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        SupervisorStatus::Watching,
        "a running background command keeps the supervisor quiet"
    );

    // The shell finishes → the idle session now fires.
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            for shell in s.background_shells.values_mut() {
                shell.state = crate::background_shell::ShellRuntimeState::Exited(Some(0));
            }
        });
        store.tick_supervisor(cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        SupervisorStatus::Judging,
        "once the background command exits, the idle session fires a judge"
    );
}

/// A `wait` verdict is one-shot: the mechanism honors the full timeout without
/// re-judging (no fresh judge even though the session is silent past the idle
/// threshold), then wakes the agent itself when the deadline elapses.
#[gpui::test]
async fn wait_is_one_shot_no_rejudge_until_deadline(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::{SupervisorStatus, VerdictAction};
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(600);
        });
        store.set_supervision_enabled(id, true, cx);
        store.supervisor_states.get_mut(&id).unwrap().status = SupervisorStatus::Judging;
        store.apply_verdict(
            id,
            VerdictAction::Wait,
            "waiting on the background build".into(),
            None,
            None,
            None,
            Some(300),
            cx,
        );
    });
    // Parked with a future deadline: a tick must NOT fire a fresh judge despite
    // the session being silent for 10 minutes.
    store.update(cx, |store, cx| store.tick_supervisor(cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        SupervisorStatus::Watching,
        "a parked wait must not re-fire a judge"
    );
    assert!(
        st.wait_until_ms.is_some(),
        "the wait deadline is still pending"
    );

    // Force the deadline into the past → the mechanism wakes the agent itself
    // and clears the wait; it does NOT spawn a judge (status stays Watching).
    store.update(cx, |store, cx| {
        store.supervisor_states.get_mut(&id).unwrap().wait_until_ms =
            Some(chrono::Utc::now().timestamp_millis() - 1_000);
        store.tick_supervisor(cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.wait_until_ms, None, "the elapsed wait is cleared");
    assert_eq!(
        st.status,
        SupervisorStatus::Watching,
        "waking the agent at the deadline does not spawn a judge"
    );
}

/// The status-icon firing counter: `trigger_count` increments each time the
/// supervisor fires a judge and resets to 0 on every enable/disable toggle.
#[gpui::test]
async fn trigger_count_increments_on_fire_and_resets_on_toggle(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id, true, cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.trigger_count, 0,
        "a fresh enable starts the counter at 0"
    );

    // One fire → count 1.
    store.update(cx, |store, cx| store.tick_supervisor(cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.trigger_count, 1, "firing a judge increments the counter");

    // Toggle off then on → counter cleared both times.
    store.update(cx, |store, cx| store.set_supervision_enabled(id, false, cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.trigger_count, 0, "disabling clears the counter");

    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.trigger_count, 0, "re-enabling keeps the counter at 0");
}

#[gpui::test]
async fn tick_sweeps_stuck_auditor(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));

    // Register a STALE auditor handle (spawned past the timeout) and a FRESH
    // one for an unrelated supervised id. The stale handle models an auditor
    // that errored / ended without calling `supervisor_audit_verdict` while the
    // supervised session sat in `Watching` — the judge-stuck path never sees it.
    let fresh_id = SolutionSessionId::new();
    let timeout_ms = (crate::supervisor::AUDITOR_TIMEOUT_SECS as i64) * 1000;
    let now = chrono::Utc::now().timestamp_millis();
    store.update(cx, |store, _| {
        store.auditor_sessions.insert(
            id,
            JudgeHandle {
                judge_id: None,
                started_ms: now - timeout_ms - 1_000,
                nonce: String::new(),
                _task: Task::ready(()),
            },
        );
        store.auditor_sessions.insert(
            fresh_id,
            JudgeHandle {
                judge_id: None,
                started_ms: now,
                nonce: String::new(),
                _task: Task::ready(()),
            },
        );
    });

    store.update(cx, |store, cx| store.tick_supervisor(cx));

    store.read_with(cx, |store, _| {
        assert!(
            !store.auditor_sessions.contains_key(&id),
            "stale auditor handle must be swept by the auditor-stuck sweep"
        );
        assert!(
            store.auditor_sessions.contains_key(&fresh_id),
            "a fresh auditor handle must survive the sweep"
        );
    });

    // The sweep is a pure cleanup — it must NOT pause/disable supervision
    // (an auditor failing is not the judge failing).
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert!(st.enabled, "auditor timeout must not disable supervision");
}

#[gpui::test]
async fn apply_continue_nudges_and_increments(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.supervisor_states.get_mut(&id).unwrap().status =
            crate::supervisor::SupervisorStatus::Judging;
    });
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::Continue,
            "two items left".into(),
            None,
            None,
            Some(500),
            None,
            cx,
        );
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.consecutive_continues, 1);
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Watching);

    // Verdict was recorded to disk — acted, so NOT flagged dropped and counted.
    let root = crate::store::test_support::session_solution_root(&store, id, cx);
    let dir = crate::supervisor::supervisor_dir(&root, id);
    let recs = crate::supervisor::read_verdicts(&dir);
    assert_eq!(recs.len(), 1);
    assert!(!recs[0].dropped, "an acted verdict must NOT be marked dropped");
    assert_eq!(
        crate::supervisor::verdict_stats(&recs).total,
        1,
        "verdict_stats counts an acted verdict"
    );
}

#[gpui::test]
async fn apply_wait_sleeps_without_nudging(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.supervisor_states.get_mut(&id).unwrap().status =
            crate::supervisor::SupervisorStatus::Judging;
    });
    let before = chrono::Utc::now().timestamp_millis();
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::Wait,
            "agent is waiting on the background build".into(),
            None,
            None,
            None,
            Some(90),
            cx,
        );
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    // Wait must NOT count toward the consecutive-continue guard.
    assert_eq!(st.consecutive_continues, 0);
    // Stays Watching (the one-shot wait handler is gated on it).
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Watching);
    // The one-shot wait commits a single wake deadline ~90s out (slack for
    // scheduling); the mechanism honors it in full without re-judging.
    let wake = st.wait_until_ms.expect("wait sets wait_until_ms");
    assert!(
        wake >= before + 80_000,
        "wait wake ~90s out: {wake} vs {before}"
    );
    assert!(wake <= before + 100_000, "wait wake within clamp: {wake}");

    // The wait verdict was recorded to disk.
    let root = crate::store::test_support::session_solution_root(&store, id, cx);
    let dir = crate::supervisor::supervisor_dir(&root, id);
    assert_eq!(crate::supervisor::read_verdicts(&dir).len(), 1);
}

#[gpui::test]
async fn apply_ask_agent_increments_and_records(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.supervisor_states.get_mut(&id).unwrap().status =
            crate::supervisor::SupervisorStatus::Judging;
    });
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::AskAgent,
            "unclear whether tests were actually run".into(),
            None,
            Some("Did you run the full test suite, and did it pass?".into()),
            None,
            None,
            cx,
        );
    });
    // ask_agent behaves like continue for the guard: counts toward the cap and
    // returns the session to Watching (it sent the question to the agent).
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.consecutive_continues, 1);
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Watching);

    // The verdict was logged with action == AskAgent.
    let root = crate::store::test_support::session_solution_root(&store, id, cx);
    let dir = crate::supervisor::supervisor_dir(&root, id);
    let recs = crate::supervisor::read_verdicts(&dir);
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].action,
        Some(crate::supervisor::VerdictAction::AskAgent)
    );
}

#[gpui::test]
async fn fifteen_continues_force_ask(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));

    for _ in 0..15 {
        store.update(cx, |store, cx| {
            // A judge only ever fires while the session is idle (`should_fire`),
            // and `apply_verdict` now re-checks that at delivery. A real
            // continue cycle returns to idle between nudges; this test fires
            // them back-to-back with no live turn, so re-assert the idle
            // premise each round (the previous nudge left the session Running).
            if let Some(s) = store.session(id) {
                s.update(cx, |s, _| s.state = crate::model::SessionState::Idle);
            }
            store.supervisor_states.get_mut(&id).unwrap().status =
                crate::supervisor::SupervisorStatus::Judging;
            store.apply_verdict(
                id,
                crate::supervisor::VerdictAction::Continue,
                "still going".into(),
                None,
                None,
                None,
                None,
                cx,
            );
        });
    }

    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::WaitingUser);

    // User replies → counter resets, back to watching.
    store.update(cx, |store, cx| {
        store.reset_supervisor_continue_counter(id, cx)
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.consecutive_continues, 0);
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Watching);
}

/// A session parked by the supervisor that resumes ON ITS OWN (the agent's
/// self-scheduled monitor fires fresh activity, not a human message) must
/// re-arm to `Watching` so the status stops hanging. This covers the two
/// supervisor-parked states — `WaitingUser` (after `ask`) and `Held` with
/// `held_by_done` (after `done`) — but a `Held` from a MANUAL user stop
/// (`held_by_done == false`) must NOT re-arm on self-activity.
#[gpui::test]
async fn self_resume_rearms_parked_supervisor(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::SupervisorStatus;
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));

    // WaitingUser (ask escalation) → self-resume re-arms to Watching.
    store.update(cx, |store, cx| {
        store.supervisor_states.get_mut(&id).unwrap().status = SupervisorStatus::WaitingUser;
        store.rearm_supervisor_on_self_activity(id, cx);
    });
    assert_eq!(
        store
            .read_with(cx, |store, _| store.supervisor_state(id))
            .unwrap()
            .status,
        SupervisorStatus::Watching,
    );

    // Held from a `done` verdict (held_by_done) → self-resume re-arms to Watching
    // and clears the flag.
    store.update(cx, |store, cx| {
        {
            let st = store.supervisor_states.get_mut(&id).unwrap();
            st.status = SupervisorStatus::Held;
            st.held_by_done = true;
        }
        store.rearm_supervisor_on_self_activity(id, cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.status, SupervisorStatus::Watching);
    assert!(!st.held_by_done);

    // Held from a MANUAL user stop (held_by_done == false) → self-resume must NOT
    // re-arm; the session stays On hold until the user sends a message.
    store.update(cx, |store, cx| {
        {
            let st = store.supervisor_states.get_mut(&id).unwrap();
            st.status = SupervisorStatus::Held;
            st.held_by_done = false;
        }
        store.rearm_supervisor_on_self_activity(id, cx);
    });
    assert_eq!(
        store
            .read_with(cx, |store, _| store.supervisor_state(id))
            .unwrap()
            .status,
        SupervisorStatus::Held,
    );
}

/// A soft/cold close evicts the in-memory supervisor row; reopening the session
/// IN-PROCESS must reload it from the DB, so supervision resumes (and doesn't
/// silently surprise-resurrect on the next restart) — finding #5.
#[gpui::test]
async fn reopen_reloads_evicted_supervisor_state(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    // Enable supervision and let the row persist to the DB.
    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));
    cx.run_until_parked();

    // Simulate a soft close: evict the in-memory supervisor state (the DB row
    // survives, as it does for a real soft close).
    store.update(cx, |store, _| {
        store.supervisor_states.remove(&id);
    });
    assert!(
        store
            .read_with(cx, |store, _| store.supervisor_state(id))
            .is_none(),
        "precondition: the in-memory state was evicted"
    );

    // Reopen (the hydrate path) reloads it.
    store.update(cx, |store, cx| store.reload_supervisor_state_for(id, cx));
    cx.run_until_parked();

    let st = store.read_with(cx, |store, _| store.supervisor_state(id));
    assert!(
        st.as_ref().is_some_and(|s| s.enabled),
        "reopen must reload the evicted supervisor row with enabled preserved"
    );
}

/// A phantom `Judging` (the fire set `Judging` but the judge SPAWN early-returned
/// because the cold session has no project → no judge handle registered) must
/// un-wedge to `Watching` WITHOUT being charged as a judge failure — otherwise
/// repeated phantoms spiral to a false `Stopped(ProviderError)` that silently
/// kills supervision (finding #2).
#[gpui::test]
async fn phantom_judging_unwedges_without_false_provider_error(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::SupervisorStatus;
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        if let Some(s) = store.session(id) {
            s.update(cx, |s, _| s.state = crate::model::SessionState::Idle);
        }
        store.set_supervision_enabled(id, true, cx);
        // Phantom: Judging, fired past the timeout, but NO judge handle exists.
        let st = store.supervisor_states.get_mut(&id).unwrap();
        st.status = SupervisorStatus::Judging;
        st.last_fired_at = Some(
            chrono::Utc::now().timestamp_millis()
                - (crate::supervisor::JUDGE_TIMEOUT_SECS as i64 + 10) * 1000,
        );
        assert!(
            !store.judge_sessions.contains_key(&id),
            "precondition: no real judge handle (the spawn early-returned)"
        );
        store.tick_supervisor(cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        SupervisorStatus::Watching,
        "phantom Judging must un-wedge to Watching, not die as ProviderError"
    );
    assert_eq!(
        st.backoff_attempt, 0,
        "a phantom judge must NOT be charged as a transient failure"
    );
    assert!(st.enabled, "supervision must stay enabled");
}

/// A meta-auditor spawns while the session is `Watching` and runs for minutes;
/// if the user manually stops the agent (`Held`) meanwhile, a late audit
/// `escalate` must NOT force `WaitingUser` — that would override the manual-stop
/// rule (a `WaitingUser` is re-armed by self-activity). It must still fire on an
/// actively-supervised session.
#[gpui::test]
async fn late_audit_escalate_respects_manual_stop(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::SupervisorStatus;
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));

    // Manual stop → Held; a racing auditor escalate must be dropped.
    store.update(cx, |store, cx| {
        {
            let st = store.supervisor_states.get_mut(&id).unwrap();
            st.status = SupervisorStatus::Held;
            st.held_by_done = false;
        }
        store.apply_audit_verdict(id, /* ok */ false, /* escalate */ true, "loop?".into(), cx);
    });
    assert_eq!(
        store
            .read_with(cx, |store, _| store.supervisor_state(id))
            .unwrap()
            .status,
        SupervisorStatus::Held,
        "a late audit escalate must not override a manual stop",
    );

    // Disabled → also dropped (no spurious WaitingUser / toast on a session the
    // user switched off).
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, false, cx);
        store.apply_audit_verdict(id, false, true, "loop?".into(), cx);
    });
    assert!(
        !matches!(
            store
                .read_with(cx, |store, _| store.supervisor_state(id))
                .unwrap()
                .status,
            SupervisorStatus::WaitingUser
        ),
        "a late audit escalate must not escalate a disabled session",
    );

    // On an actively-Watching session the escalate DOES fire → WaitingUser.
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.supervisor_states.get_mut(&id).unwrap().status = SupervisorStatus::Watching;
        store.apply_audit_verdict(id, false, true, "loop?".into(), cx);
    });
    assert_eq!(
        store
            .read_with(cx, |store, _| store.supervisor_state(id))
            .unwrap()
            .status,
        SupervisorStatus::WaitingUser,
        "an audit escalate on an actively-supervised session must fire",
    );

    // Each `apply_audit_verdict` logs a `VerdictRecord`; the two gated escalates
    // (Held, Disabled) must be flagged `dropped`, only the actively-Watching one
    // is acted — so `verdict_stats` counts exactly one.
    store.read_with(cx, |store, cx| {
        let root = store.solution_root_for_app(id, cx).expect("solution root");
        let dir = crate::supervisor::supervisor_dir(&root, id);
        let recs = crate::supervisor::read_verdicts(&dir);
        assert_eq!(recs.len(), 3, "all three audit verdicts are logged");
        assert!(recs[0].dropped, "the Held-gated escalate is marked dropped");
        assert!(recs[1].dropped, "the Disabled-gated escalate is marked dropped");
        assert!(!recs[2].dropped, "the Watching escalate is acted, not dropped");
        assert_eq!(
            crate::supervisor::verdict_stats(&recs).total,
            1,
            "verdict_stats counts only the acted audit escalate"
        );
    });
}

/// Send-time session-state re-check: a judge fires only while the session is
/// idle, but its turn runs seconds→minutes and the agent can resume ON ITS
/// OWN in the meantime (a `Bash(run_in_background)` continuation lands as an
/// orphan result and flips the session back to `Running`). A `Continue`
/// verdict delivered against a now-`Running` session must be DROPPED — not
/// nudged/queued behind the live turn (the reported "supervisor reacted while
/// the agent was still alive and the message got queued").
#[gpui::test]
async fn continue_verdict_dropped_when_agent_already_running(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));

    store.update(cx, |store, cx| {
        // The judge fired while idle; by delivery the agent has resumed.
        if let Some(s) = store.session(id) {
            s.update(cx, |s, _| {
                s.state = crate::model::SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        }
        store.supervisor_states.get_mut(&id).unwrap().status =
            crate::supervisor::SupervisorStatus::Judging;
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::Continue,
            "keep going".into(),
            None,
            None,
            None,
            None,
            cx,
        );
    });

    // The verdict is dropped: the continue counter never advanced (the drop
    // returns before the Continue arm), and no nudge was queued behind the
    // live turn.
    let st = store.read_with(cx, |store, _| store.supervisor_state(id)).unwrap();
    assert_eq!(
        st.consecutive_continues, 0,
        "a verdict delivered while the agent is Running must be dropped, not acted on"
    );
    let queued = store.read_with(cx, |store, cx| {
        store.session(id).unwrap().read(cx).pending_messages.len()
    });
    assert_eq!(queued, 0, "no spurious supervisor nudge queued behind the live turn");
    // And the drop must return the supervisor to `Watching` — NOT leave it
    // pinned in `Judging` with no live judge, which the stuck-watchdog would
    // later mistake for a crashed judge and charge a bogus backoff (audit #1).
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Watching,
        "a dropped verdict must un-pin the status from Judging back to Watching"
    );
}

/// Audit #3: a nudge parked for the "user stopped typing" flush must be DROPPED
/// (not delivered) if the session moved to a paused state (`Held` etc.) after
/// it was parked — otherwise a held nudge drags the agent back to work the user
/// explicitly stopped. Mirrors the wait-wake path's `Watching` gate.
#[gpui::test]
async fn pending_nudge_dropped_when_paused_before_flush(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(300);
        });
        // A nudge was held (user was typing when the judge finished), then the
        // user hit Stop → Held. `hold_supervisor` doesn't clear the parked nudge.
        let st = store.supervisor_states.get_mut(&id).unwrap();
        st.pending_nudge = Some("continue where you left off".into());
        st.status = crate::supervisor::SupervisorStatus::Held;
        store.tick_supervisor(cx);
    });

    let st = store.read_with(cx, |store, _| store.supervisor_state(id)).unwrap();
    assert!(
        st.pending_nudge.is_none(),
        "a parked nudge must be dropped once the session is paused (Held), not delivered"
    );
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Held,
        "the pause state is preserved — the observer stays Held"
    );
    let queued = store.read_with(cx, |store, cx| {
        store.session(id).unwrap().read(cx).pending_messages.len()
    });
    assert_eq!(queued, 0, "the stale nudge must not be delivered/queued after a Stop");
}

#[gpui::test]
async fn audit_escalate_pauses_supervision(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.apply_audit_verdict(id, false, true, "supervisor is looping".into(), cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::WaitingUser);

    // An Audit-kind record must be on disk.
    let root = crate::store::test_support::session_solution_root(&store, id, cx);
    let dir = crate::supervisor::supervisor_dir(&root, id);
    let recs = crate::supervisor::read_verdicts(&dir);
    assert!(
        recs.iter()
            .any(|r| matches!(r.kind, crate::supervisor::VerdictKind::Audit)),
        "expected an Audit-kind verdict record"
    );
    assert!(
        recs.iter()
            .any(|r| matches!(r.kind, crate::supervisor::VerdictKind::Audit)
                && r.audit_ok == Some(false)),
        "Audit record must carry audit_ok = Some(false)"
    );
}

#[gpui::test]
fn visible_session_count_excludes_live_auditors(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sol = SolutionId("sol-a".into());
            let agent = SharedString::from("claude-acp");

            // Two ordinary user sessions + one auditor child session.
            let supervised = SolutionSessionId::new();
            let other = SolutionSessionId::new();
            let auditor = SolutionSessionId::new();
            for id in [supervised, other, auditor] {
                insert_cold_session(id, sol.clone(), agent.clone(), None, None, store, cx);
            }

            assert_eq!(store.visible_session_count(&sol), 3);

            // Register a live AUDITOR handle (separate map from judges); it
            // must be excluded from the badge count exactly like a judge.
            store.auditor_sessions.insert(
                supervised,
                JudgeHandle {
                    judge_id: Some(auditor),
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    nonce: String::new(),
                    _task: Task::ready(()),
                },
            );
            assert_eq!(store.visible_session_count(&sol), 2);
            assert!(store.live_supervisor_session_ids().contains(&auditor));

            // Auditor whose create hasn't resolved (judge_id None) excludes nothing.
            store.auditor_sessions.remove(&supervised);
            store.auditor_sessions.insert(
                supervised,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    nonce: String::new(),
                    _task: Task::ready(()),
                },
            );
            assert_eq!(store.visible_session_count(&sol), 3);
        });
    });
}

#[gpui::test]
async fn escalate_sets_marker_and_waiting(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.escalate_to_user(id, "Which API did you mean?".into(), cx);
    });
    let (status, q) = store.read_with(cx, |store, cx| {
        let st = store.supervisor_state(id).unwrap();
        let q = store
            .session(id)
            .unwrap()
            .read(cx)
            .supervisor_question
            .clone();
        (st.status, q)
    });
    assert_eq!(status, crate::supervisor::SupervisorStatus::WaitingUser);
    assert_eq!(q.as_deref(), Some("Which API did you mean?"));
}

/// Regression: a human reply into a supervised session that is paused in
/// `WaitingUser` (after an `ask`) must RESUME supervision (→ `Watching`) and
/// clear the question banner — but ONLY for a genuine user send (`from_user:
/// true`), never for a supervisor nudge (`from_user: false`). The desktop
/// compose row funnels through `send_message_blocks_targeted`, so the resume
/// must live there, not only in the MCP `send_message` path (the bug: a chat
/// reply left supervision stuck `WaitingUser` forever).
#[gpui::test]
async fn user_reply_resumes_waiting_user_supervision(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.escalate_to_user(id, "Restart the stand yourself?".into(), cx);
    });

    let msg = |text: &str| {
        vec![acp::ContentBlock::Text(acp::TextContent::new(
            text.to_string(),
        ))]
    };

    // A supervisor nudge (from_user: false) must NOT resume — still WaitingUser.
    store.update(cx, |store, cx| {
        store
            .send_message_blocks_targeted(
                id,
                msg("supervisor nudge"),
                crate::model::QueueTarget::Main,
                false,
                cx,
            )
            .detach();
    });
    let (status, q) = store.read_with(cx, |store, cx| {
        (
            store.supervisor_state(id).unwrap().status,
            store
                .session(id)
                .unwrap()
                .read(cx)
                .supervisor_question
                .clone(),
        )
    });
    assert_eq!(
        status,
        crate::supervisor::SupervisorStatus::WaitingUser,
        "a non-user send must not resume a WaitingUser pause"
    );
    assert!(q.is_some(), "question stays set after a non-user send");

    // A genuine user reply (from_user: true) resumes supervision + clears banner.
    store.update(cx, |store, cx| {
        store
            .send_message_blocks_targeted(
                id,
                msg("готово, перезапустил"),
                crate::model::QueueTarget::Main,
                true,
                cx,
            )
            .detach();
    });
    let (status, q) = store.read_with(cx, |store, cx| {
        (
            store.supervisor_state(id).unwrap().status,
            store
                .session(id)
                .unwrap()
                .read(cx)
                .supervisor_question
                .clone(),
        )
    });
    assert_eq!(
        status,
        crate::supervisor::SupervisorStatus::Watching,
        "a user reply must resume supervision to Watching"
    );
    assert!(q.is_none(), "user reply must clear the supervisor question");
}

/// A `Done` verdict parks supervision in `Held` (standby) WITHOUT disabling it —
/// the same pause the user's manual Stop uses. When the user then continues the
/// work with a new message, supervision re-arms to `Watching` (the task is
/// evidently not done anymore).
#[gpui::test]
async fn user_reply_rearms_supervision_after_done(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::Done,
            "task complete".into(),
            None,
            None,
            None,
            None,
            cx,
        );
    });
    let st = store.read_with(cx, |store, _| store.supervisor_state(id).unwrap());
    assert!(
        st.enabled,
        "Done parks in Held, it does NOT disable supervision"
    );
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Held,
        "Done parks the supervisor on hold"
    );

    // User continues the work — supervision must re-arm.
    store.update(cx, |store, cx| {
        store
            .send_message_blocks_targeted(
                id,
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "ещё не всё, продолжаем".to_string(),
                ))],
                crate::model::QueueTarget::Main,
                true,
                cx,
            )
            .detach();
    });
    let st = store.read_with(cx, |store, _| store.supervisor_state(id).unwrap());
    assert!(
        st.enabled,
        "a user reply after Done must re-enable supervision"
    );
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Watching,
        "re-armed supervision returns to Watching"
    );
}

/// Regression: a `Compact` verdict re-entered the store. `apply_verdict` runs
/// inside the MCP tool's `store.update(...)` lease, and the Compact arm called
/// `compact::start_compact_for_session`, which re-`read_with`s the global store
/// → `double_lease_panic` ("cannot read SolutionAgentStore while it is already
/// being updated"). Every supervisor "compact" verdict crashed the live editor.
/// The fix defers the compact past the update lease; this test drives the exact
/// path (apply_verdict(Compact) inside `store.update`, then pump deferred work)
/// and must NOT panic.
#[gpui::test]
async fn compact_verdict_does_not_reenter_store(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        // Pre-fix this call panicked synchronously inside the update.
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::Compact,
            "context is large; compact".into(),
            None,
            None,
            None,
            None,
            cx,
        );
    });
    // The deferred compact runs here (post-lease). It may decline (no context to
    // compact on the seeded session) — that's fine; the point is no panic.
    cx.executor().run_until_parked();
    let st = store.read_with(cx, |store, _| store.supervisor_state(id).unwrap());
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Watching,
        "a Compact verdict leaves supervision Watching"
    );
}

/// A parked one-shot `wait` must be cancelled when the agent's own turn
/// completes (`Stopped`) — otherwise, if the agent self-resumed and FINISHED
/// before the wait deadline, the mechanism would still wake it at the deadline
/// to "check the task you were waiting on" minutes after it already did it
/// (finding #8). A user message already clears the wait; an agent completion
/// must too.
#[gpui::test]
async fn agent_completion_clears_parked_wait(cx: &mut gpui::TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.set_supervision_enabled(session_id, true, cx);
            // A one-shot wait is parked, deadline well in the future.
            store
                .supervisor_states
                .get_mut(&session_id)
                .unwrap()
                .wait_until_ms = Some(chrono::Utc::now().timestamp_millis() + 600_000);
        });
    });
    // The agent self-resumed and its turn ran to completion before the deadline.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        let thread = session.read(cx).acp_thread().cloned().unwrap();
        thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                agent_client_protocol::schema::StopReason::EndTurn,
            ));
        });
    });
    cx.executor().run_until_parked();
    let wait = cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .read(cx)
            .supervisor_state(session_id)
            .unwrap()
            .wait_until_ms
    });
    assert!(
        wait.is_none(),
        "a completed turn must cancel the parked one-shot wait (finding #8)"
    );
}

#[gpui::test]
async fn done_verdict_clears_pending_question(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;

    // Enable supervision and escalate a question — the banner should be set.
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.escalate_to_user(id, "Q?".into(), cx);
    });
    let (status, q) = store.read_with(cx, |store, cx| {
        let st = store.supervisor_state(id).unwrap();
        let q = store
            .session(id)
            .unwrap()
            .read(cx)
            .supervisor_question
            .clone();
        (st.status, q)
    });
    assert_eq!(status, crate::supervisor::SupervisorStatus::WaitingUser);
    assert!(q.is_some(), "question must be set after escalate_to_user");

    // A Done verdict fires before the user replies — banner must be cleared.
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::Done,
            "all done".into(),
            None,
            None,
            None,
            None,
            cx,
        );
    });
    let q_after = store.read_with(cx, |store, cx| {
        store
            .session(id)
            .unwrap()
            .read(cx)
            .supervisor_question
            .clone()
    });
    assert!(
        q_after.is_none(),
        "supervisor_question must be cleared after Done verdict"
    );

    // Verify the supervision state is parked on hold (Done → Held).
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Held,
        "a Done verdict parks supervision on hold"
    );

    // Also verify that set_supervision_enabled(false) clears a pending question.
    let (store2, id2, _tmp2) = crate::store::test_support::seed_store_with_session(cx).await;
    store2.update(cx, |store, cx| {
        store.set_supervision_enabled(id2, true, cx);
        store.escalate_to_user(id2, "still waiting?".into(), cx);
    });
    let q_before_disable = store2.read_with(cx, |store, cx| {
        store
            .session(id2)
            .unwrap()
            .read(cx)
            .supervisor_question
            .clone()
    });
    assert!(
        q_before_disable.is_some(),
        "question must be set before disable"
    );

    store2.update(cx, |store, cx| {
        store.set_supervision_enabled(id2, false, cx);
    });
    let q_after_disable = store2.read_with(cx, |store, cx| {
        store
            .session(id2)
            .unwrap()
            .read(cx)
            .supervisor_question
            .clone()
    });
    assert!(
        q_after_disable.is_none(),
        "supervisor_question must be cleared when supervision is disabled"
    );
}

/// Arm a usage-limit / backoff resume gate on `session_id` exactly like
/// `on_judge_failed`'s Quota branch: `next_eligible_ms` in the future plus a
/// live `backoff_timers` wake task.
fn arm_resume_gate(store: &mut SolutionAgentStore, session_id: SolutionSessionId) {
    let future_ms = chrono::Utc::now().timestamp_millis() + 60 * 60 * 1000;
    let st = store
        .supervisor_states
        .entry(session_id)
        .or_insert_with(|| crate::supervisor::SupervisorState::new(session_id));
    st.enabled = true;
    st.status = crate::supervisor::SupervisorStatus::Watching;
    st.next_eligible_ms = Some(future_ms);
    store.backoff_timers.insert(session_id, Task::ready(()));
}

#[gpui::test]
async fn successful_turn_clears_pending_usage_limit_resume_gate(cx: &mut TestAppContext) {
    // #7: a usage-limit auto-resume gate (`next_eligible_ms` + its
    // `backoff_timers` wake task) must be cancelled once the worker actually
    // responds — a successful turn (`Stopped`) proves the wall is gone.
    // Otherwise the session stays gated until the stale reset time and the
    // timer fires a redundant judge.
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| arm_resume_gate(store, session_id));
    });

    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                acp::StopReason::EndTurn,
            ));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, _| {
            let st = store.supervisor_states.get(&session_id).expect("state");
            assert_eq!(
                st.next_eligible_ms, None,
                "resume gate must be cleared on a successful turn",
            );
            assert!(
                !store.backoff_timers.contains_key(&session_id),
                "resume wake timer must be removed on a successful turn",
            );
        });
    });
}

#[gpui::test]
async fn rehit_error_keeps_pending_usage_limit_resume_gate(cx: &mut TestAppContext) {
    // #7 (the other half): a NEW user message that re-hits the wall surfaces as
    // `AcpThreadEvent::Error` (not `Stopped`), so the pending resume gate must
    // SURVIVE — we keep waiting for the reset.
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| arm_resume_gate(store, session_id));
    });

    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Error);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, _| {
            let st = store.supervisor_states.get(&session_id).expect("state");
            assert!(
                st.next_eligible_ms.is_some(),
                "resume gate must survive a re-hit (Error), so we keep waiting",
            );
            assert!(
                store.backoff_timers.contains_key(&session_id),
                "resume wake timer must survive a re-hit (Error)",
            );
        });
    });
}
