//! The user's "Compact context" on a running session: ask the agent to hand
//! off, ask again, then send the compaction prompt — the observer's ladder,
//! driven by the user's request instead of a verdict.

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
        SolutionAgentStore::global(cx).update(cx, |store, cx| store.tick_supervisor(cx));
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

/// Ask, ask again after the escalation window, then send the prompt — and the
/// prompt carries the user's authority, as a direct Compact would.
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
            && t.contains("\"initiator\": \"agent\"")),
        "the ask names the user and the tool to call: {texts:?}"
    );
    assert_eq!(handoff(session_id, cx).map(|h| h.asks), Some(1));
    deliver_queue(session_id, cx);

    tick(cx);
    assert!(queued_texts(session_id, cx).is_empty(), "too soon to ask again");

    age_last_ask(session_id, cx);
    tick(cx);
    let texts = queued_texts(session_id, cx);
    assert!(
        texts.iter().any(|t| t.starts_with("The user's context handoff request is still open")),
        "second ask: {texts:?}"
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

/// The agent honouring the user's request compacts with the user's authority —
/// it is the user's compaction, only timed by the agent.
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

fn observer_requests(session_id: SolutionSessionId, cx: &mut TestAppContext) {
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.request_handoff(
                session_id,
                crate::model::HandoffAsker::Observer,
                false,
                Some("observer note".into()),
                cx,
            )
        })
    });
    cx.executor().run_until_parked();
}

/// One ladder per session: the user's request takes over an observer's from
/// the first rung (the user has not asked yet), in the user's voice.
#[gpui::test]
async fn the_users_request_takes_over_the_observers_ladder(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    fill_context(&thread, cx);
    start_turn(session_id, cx);
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_supervision_enabled(session_id, true, cx);
        })
    });
    observer_requests(session_id, cx);
    let ladder = handoff(session_id, cx).expect("observer armed the ladder");
    assert_eq!(
        (ladder.asker, ladder.asks),
        (crate::model::HandoffAsker::Observer, 1)
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

/// An observer verdict does not reword or re-time a ladder the user started.
#[gpui::test]
async fn an_observer_verdict_leaves_the_users_ladder_alone(cx: &mut TestAppContext) {
    let (session_id, thread, _gate, _tmp) = create_gated_session(cx).await;
    fill_context(&thread, cx);
    start_turn(session_id, cx);
    assert!(request(session_id, cx));
    deliver_queue(session_id, cx);

    observer_requests(session_id, cx);
    let ladder = handoff(session_id, cx).expect("still the user's");
    assert_eq!(
        (ladder.asker, ladder.asks, ladder.note),
        (crate::model::HandoffAsker::User(CompactInitiator::User), 1, None)
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
