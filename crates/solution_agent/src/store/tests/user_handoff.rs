//! The handoff ladder: signal the agent to stop at the next natural work
//! boundary, signal again, then send the compaction prompt — started by the
//! user's "Compact context" on a running session, or by auto-compaction when
//! the context crosses its threshold.

use super::in_place_rotation::{create_gated_session, start_turn};
use crate::compact::{CompactInitiator, request_compact_for_session, start_compact_for_session};
use crate::store::*;
use agent_client_protocol::schema::v1 as acp;
use gpui::{Entity, TestAppContext};

const ESCALATION_MS: i64 = (crate::supervisor::COMPACT_ESCALATION_SECS as i64) * 1000;

fn fill_context(thread: &Entity<acp_thread::AcpThread>, cx: &mut TestAppContext) {
    cx.update(|cx| {
        thread.update(cx, |thread, cx| {
            thread.update_token_usage(
                Some(acp_thread::TokenUsage {
                    used_tokens: 250_000,
                    max_tokens: 1_000_000,
                    ..Default::default()
                }),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();
}

fn request(session_id: SolutionSessionId, cx: &mut TestAppContext) -> bool {
    let outcome = cx
        .update(|cx| request_compact_for_session(session_id, CompactInitiator::User, None, cx))
        .expect("request dispatches");
    cx.executor().run_until_parked();
    outcome.queued
}

fn queued_texts(session_id: SolutionSessionId, cx: &mut TestAppContext) -> Vec<String> {
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        session
            .read(cx)
            .pending_messages
            .iter()
            .flat_map(|bundle| bundle.blocks.iter())
            .filter_map(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    })
}

/// Stand-in for the queue reaching the agent: the mock does not steer, so an
/// ask sits in the queue until the turn ends.
fn deliver_queue(session_id: SolutionSessionId, cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        session.update(cx, |s, _| s.pending_messages.clear());
    });
}

fn handoff(
    session_id: SolutionSessionId,
    cx: &mut TestAppContext,
) -> Option<crate::model::HandoffLadder> {
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        session.read(cx).handoff_ladder.clone()
    })
}

fn age_last_ask(session_id: SolutionSessionId, cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        session.update(cx, |s, _| {
            let handoff = s.handoff_ladder.as_mut().expect("ladder armed");
            let last = handoff.last_ask_ms.expect("asked");
            handoff.last_ask_ms = Some(last - ESCALATION_MS - 1000);
        });
    });
}

fn tick(cx: &mut TestAppContext) {
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| store.tick_context_handoffs(cx));
    });
    cx.executor().run_until_parked();
}

fn compaction_state(session_id: SolutionSessionId, cx: &mut TestAppContext) -> (bool, bool) {
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        (s.is_compaction_pending(), s.compact_reset_observer_memory)
    })
}

fn has_compaction_prompt(texts: &[String]) -> bool {
    texts
        .iter()
        .any(|text| text.starts_with(crate::compact::COMPACT_PROMPT_HEADING))
}

/// Signal, signal again after the escalation window, then send the prompt — and
/// the prompt carries the user's authority, as a direct Compact would.
#[gpui::test]
async fn a_running_session_is_asked_twice_then_compacted(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    fill_context(&thread, cx);
    start_turn(session_id, cx);

    assert!(request(session_id, cx));
    let texts = queued_texts(session_id, cx);
    assert!(!has_compaction_prompt(&texts), "the first request only asks");
    assert!(
        texts.iter().any(|t| t.starts_with("The user asked for a context handoff")
            && t.contains("next natural boundary")
            && t.contains("does not change or shorten")
            && t.contains("Do not call a compaction tool")),
        "the signal preserves the active phase and names its natural boundary: {texts:?}"
    );
    assert!(
        texts.iter().all(|t| !t.contains("solution_agent.start_compact")),
        "an early signal must not tell the agent to interrupt itself: {texts:?}"
    );
    assert_eq!(handoff(session_id, cx).map(|h| h.asks), Some(1));
    deliver_queue(session_id, cx);

    tick(cx);
    assert!(queued_texts(session_id, cx).is_empty(), "too soon to ask again");

    age_last_ask(session_id, cx);
    tick(cx);
    let texts = queued_texts(session_id, cx);
    assert!(
        texts.iter().any(|t| t.starts_with("The user's context handoff request is still open")
            && t.contains("next natural boundary")
            && t.contains("does not change or shorten")
            && t.contains("force the handoff as the final escalation")),
        "second ask: {texts:?}"
    );
    assert!(
        texts.iter().all(|t| !t.contains("solution_agent.start_compact")),
        "the second signal still must not tell the agent to interrupt itself: {texts:?}"
    );
    assert!(!has_compaction_prompt(&texts));
    assert_eq!(handoff(session_id, cx).map(|h| h.asks), Some(2));
    deliver_queue(session_id, cx);

    age_last_ask(session_id, cx);
    tick(cx);
    assert!(
        has_compaction_prompt(&queued_texts(session_id, cx)),
        "asking is spent: the editor sends the prompt itself"
    );
    assert!(handoff(session_id, cx).is_none(), "queuing the prompt ends the ladder");
    assert_eq!(compaction_state(session_id, cx), (true, true));
}

/// A turn that ends without a handoff has nothing left to finish; waiting out
/// the window would only leave an idle agent unasked.
#[gpui::test]
async fn a_turn_ending_without_a_handoff_is_compacted_at_once(cx: &mut TestAppContext) {
    let (session_id, thread, gate, _tmp) = create_gated_session(cx).await;
    fill_context(&thread, cx);
    start_turn(session_id, cx);
    assert!(request(session_id, cx));
    deliver_queue(session_id, cx);

    gate.send(()).await.expect("release the turn");
    cx.executor().run_until_parked();
    tick(cx);

    assert!(handoff(session_id, cx).is_none());
    assert!(compaction_state(session_id, cx).0, "the prompt went out");
}

/// Asking again while the ladder runs means "now".
#[gpui::test]
async fn a_second_request_compacts_now(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    fill_context(&thread, cx);
    start_turn(session_id, cx);
    assert!(request(session_id, cx));
    deliver_queue(session_id, cx);

    assert!(request(session_id, cx));
    assert!(has_compaction_prompt(&queued_texts(session_id, cx)));
    assert!(handoff(session_id, cx).is_none());
}

/// If the agent independently starts a handoff while the user's request is
/// armed, it still inherits the user's authority.
#[gpui::test]
async fn the_agents_own_handoff_inherits_the_users_request(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    fill_context(&thread, cx);
    start_turn(session_id, cx);
    assert!(request(session_id, cx));
    deliver_queue(session_id, cx);

    let queued = cx
        .update(|cx| start_compact_for_session(session_id, CompactInitiator::Agent, None, cx))
        .expect("agent compacts")
        .queued;
    cx.executor().run_until_parked();
    assert!(queued);
    assert!(handoff(session_id, cx).is_none());
    assert_eq!(compaction_state(session_id, cx), (true, true));
}

/// The user's Stop cuts the request off with the turn.
#[gpui::test]
async fn stopping_the_turn_drops_the_request(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    fill_context(&thread, cx);
    start_turn(session_id, cx);
    assert!(request(session_id, cx));

    thread.update(cx, |_, cx| {
        cx.emit(acp_thread::AcpThreadEvent::Stopped(acp::StopReason::Cancelled))
    });
    cx.executor().run_until_parked();
    tick(cx);

    assert!(handoff(session_id, cx).is_none());
    assert!(!compaction_state(session_id, cx).0, "nothing is compacted after a Stop");
}

/// An idle session has no step to finish: Compact works as it always did.
#[gpui::test]
async fn an_idle_session_is_compacted_directly(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    fill_context(&thread, cx);

    assert!(request(session_id, cx));
    assert!(handoff(session_id, cx).is_none());
    assert!(compaction_state(session_id, cx).0);
}

fn set_usage(thread: &Entity<acp_thread::AcpThread>, used: u64, cx: &mut TestAppContext) {
    cx.update(|cx| {
        thread.update(cx, |thread, cx| {
            thread.update_token_usage(
                Some(acp_thread::TokenUsage {
                    used_tokens: used,
                    max_tokens: 1_000_000,
                    ..Default::default()
                }),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();
}

fn set_auto_compact(
    session_id: SolutionSessionId,
    setting: crate::model::AutoCompactSetting,
    cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .update(cx, |store, cx| store.set_auto_compact(session_id, setting, cx));
    });
}

fn system_notes(session_id: SolutionSessionId, needle: &str, cx: &mut TestAppContext) -> usize {
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let thread = session.read(cx).acp_thread().cloned().expect("live thread");
        thread
            .read(cx)
            .entries()
            .iter()
            .filter(|entry| {
                matches!(entry, acp_thread::AgentThreadEntry::SystemNote(note) if note.text.contains(needle))
            })
            .count()
    })
}

/// Crossing the threshold while the agent works signals it to stop before the
/// next phase, in the editor's voice and marked as the editor's — once, not
/// every tick.
#[gpui::test]
async fn auto_compaction_asks_when_the_context_crosses_the_threshold(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    set_usage(&thread, 390_000, cx);
    start_turn(session_id, cx);
    tick(cx);
    assert!(handoff(session_id, cx).is_none(), "39% is below the 40% default");

    set_usage(&thread, 410_000, cx);
    tick(cx);
    let ladder = handoff(session_id, cx).expect("armed at the threshold");
    assert_eq!(
        (ladder.asker, ladder.asks, ladder.claims_user),
        (crate::model::HandoffAsker::Auto, 1, false)
    );
    let texts = queued_texts(session_id, cx);
    assert!(
        texts.iter().any(|t| t.starts_with("Your context is 41% full.")
            && t.contains("Keep working normally on the phase already in progress")
            && t.contains("pause before beginning another phase or independent task")
            && t.contains("Do not call a compaction tool")),
        "the editor's wording: {texts:?}"
    );
    assert!(
        texts.iter().all(|t| !t.contains("solution_agent.start_compact")),
        "the threshold is a signal inside the turn, not a self-interrupt command: {texts:?}"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let bundle = session.read(cx).pending_messages.front().cloned().expect("queued");
        assert!(
            acp_thread::is_editor_recovery_blocks(&bundle.blocks),
            "marked as the editor's, not the user's"
        );
    });

    tick(cx);
    assert_eq!(handoff(session_id, cx).map(|l| l.asks), Some(1), "one ask per rung");
}

/// A provider failure releases the prompt future, but does not finish the
/// user's work. The 40% warning must not become an immediate handoff at 41%.
#[gpui::test]
async fn auto_compaction_does_not_force_after_a_failed_turn(cx: &mut TestAppContext) {
    let (session_id, thread, gate, _tmp) = create_gated_session(cx).await;
    set_usage(&thread, 410_000, cx);
    start_turn(session_id, cx);
    tick(cx);
    assert_eq!(handoff(session_id, cx).map(|l| l.asks), Some(1));
    deliver_queue(session_id, cx);
    drop(gate);
    cx.executor().run_until_parked();

    for _ in 0..3 { tick(cx); }
    assert!(!compaction_state(session_id, cx).0,
        "a failed provider turn is not the natural work boundary");
    assert!(!has_compaction_prompt(&queued_texts(session_id, cx)));
    assert_eq!(handoff(session_id, cx).map(|l| l.asks), Some(1));
    // Background/side-channel activity can label the session Running even
    // though the failed main turn is gone. It must not start another prompt.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| { s.state.resume_on_activity(); });
    });
    age_last_ask(session_id, cx);
    tick(cx);
    assert_eq!(handoff(session_id, cx).unwrap().asks, 1);
    assert!(!compaction_state(session_id, cx).0);
}

#[gpui::test]
async fn automatic_handoff_follows_a_successful_end_turn(cx: &mut TestAppContext) {
    let (session_id, thread, gate, _tmp) = create_gated_session(cx).await;
    set_usage(&thread, 410_000, cx);
    start_turn(session_id, cx);
    tick(cx);
    deliver_queue(session_id, cx);
    gate.send(()).await.unwrap();
    cx.executor().run_until_parked();
    assert!(handoff(session_id, cx).unwrap().end_turn_observed);
    tick(cx);
    assert!(compaction_state(session_id, cx).0);
}

#[gpui::test]
async fn a_new_failed_turn_cannot_reuse_an_old_successful_boundary(cx: &mut TestAppContext) {
    let (session_id, thread, gate, _tmp) = create_gated_session(cx).await;
    set_usage(&thread, 410_000, cx);
    start_turn(session_id, cx);
    tick(cx);
    deliver_queue(session_id, cx);
    gate.send(()).await.unwrap();
    cx.executor().run_until_parked();
    assert!(handoff(session_id, cx).unwrap().end_turn_observed);
    start_turn(session_id, cx);
    assert!(!handoff(session_id, cx).unwrap().end_turn_observed);
    drop(gate);
    cx.executor().run_until_parked();
    age_last_ask(session_id, cx);
    tick(cx);
    assert!(!compaction_state(session_id, cx).0);
    assert_eq!(handoff(session_id, cx).unwrap().asks, 1);
}

#[gpui::test]
async fn automatic_forced_handoff_still_obeys_both_escalation_windows(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    set_usage(&thread, 410_000, cx);
    start_turn(session_id, cx);
    tick(cx);
    deliver_queue(session_id, cx);
    tick(cx);
    assert_eq!(handoff(session_id, cx).unwrap().asks, 1);
    age_last_ask(session_id, cx);
    tick(cx);
    assert_eq!(handoff(session_id, cx).unwrap().asks, 2);
    deliver_queue(session_id, cx);
    tick(cx);
    assert!(!compaction_state(session_id, cx).0);
    age_last_ask(session_id, cx);
    tick(cx);
    assert!(compaction_state(session_id, cx).0);
}

/// Once per context: a user who stops the handoff is not asked again in the
/// same context.
#[gpui::test]
async fn auto_compaction_fires_once_per_context(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    set_usage(&thread, 500_000, cx);
    start_turn(session_id, cx);
    tick(cx);
    assert!(handoff(session_id, cx).is_some());

    thread.update(cx, |_, cx| {
        cx.emit(acp_thread::AcpThreadEvent::Stopped(acp::StopReason::Cancelled))
    });
    cx.executor().run_until_parked();
    assert!(handoff(session_id, cx).is_none(), "the Stop dropped it");
    start_turn(session_id, cx);
    tick(cx);
    assert!(handoff(session_id, cx).is_none(), "not re-armed in the same context");
}

/// Off means off; and an idle session above its threshold (say, restored so)
/// is left alone — only a working agent is asked.
#[gpui::test]
async fn auto_compaction_waits_for_work_and_respects_off(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    set_usage(&thread, 500_000, cx);
    tick(cx);
    assert!(handoff(session_id, cx).is_none(), "idle: nothing to hand off from");

    set_auto_compact(
        session_id,
        crate::model::AutoCompactSetting {
            enabled: false,
            threshold_pct: None,
        },
        cx,
    );
    start_turn(session_id, cx);
    tick(cx);
    assert!(handoff(session_id, cx).is_none(), "switched off");

    set_auto_compact(session_id, crate::model::AutoCompactSetting::default(), cx);
    tick(cx);
    assert!(handoff(session_id, cx).is_some(), "switched back on");
    set_auto_compact(
        session_id,
        crate::model::AutoCompactSetting {
            enabled: false,
            threshold_pct: None,
        },
        cx,
    );
    tick(cx);
    assert!(
        handoff(session_id, cx).is_none(),
        "switching off stands down a handoff it started"
    );
}

/// The user's threshold replaces the window default.
#[gpui::test]
async fn auto_compaction_follows_the_chosen_threshold(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    set_usage(&thread, 300_000, cx);
    start_turn(session_id, cx);
    tick(cx);
    assert!(handoff(session_id, cx).is_none(), "30% is below the 40% default");

    set_auto_compact(
        session_id,
        crate::model::AutoCompactSetting {
            enabled: true,
            threshold_pct: Some(30),
        },
        cx,
    );
    tick(cx);
    assert!(handoff(session_id, cx).is_some(), "at the chosen 30%");
}

/// A refused force (no headroom left) is reported once and retried only after
/// the back-off, not every tick.
#[gpui::test]
async fn a_refused_force_backs_off_instead_of_retrying_every_tick(cx: &mut TestAppContext) {
    let (session_id, thread, gate, _tmp) = create_gated_session(cx).await;
    set_usage(&thread, 500_000, cx);
    start_turn(session_id, cx);
    tick(cx);
    deliver_queue(session_id, cx);
    set_usage(&thread, 999_000, cx);
    gate.send(()).await.expect("release the turn");
    cx.executor().run_until_parked();

    const REFUSED: &str = "Context compaction could not start";
    tick(cx);
    assert_eq!(system_notes(session_id, REFUSED, cx), 1, "refused, and said so");
    for _ in 0..3 {
        tick(cx);
    }
    assert_eq!(system_notes(session_id, REFUSED, cx), 1, "not retried every tick");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        session.update(cx, |s, _| {
            let ladder = s.handoff_ladder.as_mut().expect("still armed");
            ladder.last_force_ms = ladder.last_force_ms.map(|at| at - ESCALATION_MS - 1000);
        });
    });
    tick(cx);
    assert_eq!(system_notes(session_id, REFUSED, cx), 2, "tried again after the back-off");
}

/// The setting survives a restart: stored when it differs from the default,
/// and a return to the default removes the row.
#[gpui::test]
async fn the_auto_compaction_setting_is_persisted(cx: &mut TestAppContext) {
    let db = crate::db::SolutionAgentDb::open(cx.executor()).expect("open db");
    let id = SolutionSessionId::new();
    let custom = crate::model::AutoCompactSetting {
        enabled: false,
        threshold_pct: Some(60),
    };
    db.set_auto_compact(id, custom).expect("write");
    assert_eq!(db.load_auto_compact().expect("load"), vec![(id, custom)]);
    db.set_auto_compact(id, crate::model::AutoCompactSetting::default())
        .expect("write default");
    assert!(db.load_auto_compact().expect("load").is_empty());
}

/// One ladder per session: the user's request takes over an automatic one from
/// the first rung (the user has not asked yet), in the user's voice.
#[gpui::test]
async fn the_users_request_takes_over_an_automatic_ladder(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    set_usage(&thread, 500_000, cx);
    start_turn(session_id, cx);
    tick(cx);
    let ladder = handoff(session_id, cx).expect("auto-compaction armed the ladder");
    assert_eq!(
        (ladder.asker, ladder.asks),
        (crate::model::HandoffAsker::Auto, 1)
    );
    deliver_queue(session_id, cx);

    assert!(request(session_id, cx));
    let ladder = handoff(session_id, cx).expect("still climbing");
    assert_eq!(
        (ladder.asker, ladder.asks, ladder.claims_user),
        (crate::model::HandoffAsker::User(CompactInitiator::User), 1, true)
    );
    assert!(
        queued_texts(session_id, cx)
            .iter()
            .any(|t| t.starts_with("The user asked for a context handoff")),
        "the user's first ask goes out at once"
    );
}

/// Auto-compaction does not reword or re-time a ladder the user started.
#[gpui::test]
async fn auto_compaction_leaves_the_users_ladder_alone(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    fill_context(&thread, cx);
    start_turn(session_id, cx);
    assert!(request(session_id, cx));
    deliver_queue(session_id, cx);

    set_usage(&thread, 500_000, cx);
    tick(cx);
    let ladder = handoff(session_id, cx).expect("still the user's");
    assert_eq!(
        (ladder.asker, ladder.asks),
        (crate::model::HandoffAsker::User(CompactInitiator::User), 1)
    );
    assert!(queued_texts(session_id, cx).is_empty(), "nothing new was sent");
}

/// A judge already reading the transcript when the user asks for a handoff is
/// ruling on a conversation about to be replaced: it is torn down, and its
/// verdict would be dropped as stale.
#[gpui::test]
async fn the_users_request_supersedes_an_in_flight_judge(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    fill_context(&thread, cx);
    start_turn(session_id, cx);
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_supervision_enabled(session_id, true, cx);
            store
                .supervisor_states
                .get_mut(&session_id)
                .expect("supervised")
                .status = crate::supervisor::SupervisorStatus::Judging;
            store.judge_sessions.insert(
                session_id,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    nonce: String::new(),
                    _task: gpui::Task::ready(()),
                },
            );
        })
    });

    assert!(request(session_id, cx));
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx).read(cx);
        assert!(!store.judge_sessions.contains_key(&session_id));
        assert!(store.supervisor_states[&session_id].judge_superseded);
        assert_eq!(
            store.supervisor_states[&session_id].status,
            crate::supervisor::SupervisorStatus::Watching
        );
    });
}
