//! Receipt-based active-turn steering. Reserved queue bundles stay visible
//! until acknowledgement and cannot merge with or consume newer submissions.
use super::*;
use crate::model::QueueTarget;
use std::collections::HashSet;

fn apply_receipt_to_queue(
    queue: &mut std::collections::VecDeque<crate::model::PendingBundle>,
    bundles: &HashSet<uuid::Uuid>,
    outcome: &codex_native::SteerOutcome,
) -> Vec<acp::ContentBlock> {
    if matches!(outcome, codex_native::SteerOutcome::Rejected(_)) {
        return Vec::new();
    }
    let mut delivered = Vec::new();
    queue.retain(|bundle| {
        if bundles.contains(&bundle.id) {
            delivered.extend(bundle.blocks.clone());
            false
        } else {
            true
        }
    });
    delivered
}

fn steerable_bundles(s: &crate::model::SolutionSession) -> HashSet<uuid::Uuid> {
    s.pending_messages
        .iter()
        .filter(|b| {
            b.target == QueueTarget::Main
                && (s.pending_compaction.is_none()
                    || crate::compact::is_compaction_blocks(&b.blocks))
        })
        .map(|b| b.id)
        .collect()
}

pub(super) struct PendingSteer {
    token: uuid::Uuid,
    pub bundles: HashSet<uuid::Uuid>,
}

impl SolutionAgentStore {
    pub(super) fn rotation_steering_ready(
        &self,
        id: SolutionSessionId,
        expected: &(u64, acp::SessionId, Option<u64>),
        cx: &App,
    ) -> Result<bool> {
        let session = self
            .session(id)
            .ok_or_else(|| anyhow!("Session closed during context rotation"))?;
        let s = session.read(cx);
        if (s.epoch, &s.acp_session_id, s.pending_compaction)
            != (expected.0, &expected.1, expected.2)
            || matches!(
                s.state,
                SessionState::Stopping { .. } | SessionState::Errored(_)
            )
        {
            return Err(anyhow!(
                "The session changed while context rotation was waiting"
            ));
        }
        Ok(!self.active_steers.contains_key(&id))
    }

    pub(crate) fn bundle_is_steering(
        &self,
        session: SolutionSessionId,
        bundle: uuid::Uuid,
    ) -> bool {
        self.active_steers
            .get(&session)
            .is_some_and(|pending| pending.bundles.contains(&bundle))
    }

    pub(super) fn try_steer_pending(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        if self.active_steers.contains_key(&session_id) {
            return;
        }
        let Some(session) = self.session(session_id) else {
            return;
        };
        let s = session.read(cx);
        if !matches!(s.state, SessionState::Running { .. }) {
            return;
        }
        let Some(thread) = s.acp_thread().cloned() else {
            return;
        };
        let Some(connection) = thread
            .read(cx)
            .connection()
            .clone()
            .downcast::<codex_native::CodexConnection>()
        else {
            return;
        };
        // Once the handoff request is in flight, newer user intent belongs in
        // the replacement context. A receipt only proves acceptance, not that
        // the worker incorporated it into the handoff file already written.
        let bundles = steerable_bundles(s);
        if bundles.is_empty() {
            return;
        }
        let blocks = s
            .pending_messages
            .iter()
            .filter(|b| bundles.contains(&b.id))
            .flat_map(|b| b.blocks.clone())
            .collect();
        let epoch = s.epoch;
        let acp_id = s.acp_session_id.clone();
        let token = uuid::Uuid::new_v4();
        let task = connection.steer(&acp_id, blocks, token.to_string(), cx);
        self.active_steers
            .insert(session_id, PendingSteer { token, bundles });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            // Reconnect can briefly detach the thread while preserving this
            // context. Keep the receipt reserved until its user entry can be
            // recorded on the replacement thread; never discard the text or
            // resend an accepted/ambiguous message during that gap.
            if !matches!(outcome, codex_native::SteerOutcome::Rejected(_)) {
                loop {
                    let wait_for_thread = this.update(cx, |store, cx| {
                        store.active_steers.get(&session_id).is_some_and(|pending|
                            pending.token == token && store.session(session_id).is_some_and(|session| {
                                let s = session.read(cx);
                                s.epoch == epoch && s.acp_session_id == acp_id && s.acp_thread().is_none()
                                    && s.pending_messages.iter().any(|b| pending.bundles.contains(&b.id))
                            }))
                    }).unwrap_or(false);
                    if !wait_for_thread { break; }
                    cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
                }
            }
            this.update(cx, |store, cx| {
                let Some(pending) = store.active_steers.get(&session_id) else { return; };
                if pending.token != token { return; }
                let pending = store.active_steers.remove(&session_id).unwrap();
                let Some(session) = store.session(session_id) else { return; };
                if session.read(cx).epoch != epoch || session.read(cx).acp_session_id != acp_id { return; }
                let retry = matches!(outcome, codex_native::SteerOutcome::Rejected(_));
                if !retry {
                    let delivered = session.update(cx, |s, _| apply_receipt_to_queue(&mut s.pending_messages, &pending.bundles, &outcome));
                    if !delivered.is_empty() {
                        if let Some(current_thread) = session.read(cx).acp_thread().cloned() {
                            current_thread.update(cx, |thread, cx| {thread.push_user_message_entry(None, delivered, cx);});
                        }
                        store.mark_queue_changed(session_id, cx);
                    }
                }
                match outcome {
                    codex_native::SteerOutcome::Accepted => {},
                    codex_native::SteerOutcome::Rejected(error) => log::info!("Agent follow-up remains queued: {error}"),
                    codex_native::SteerOutcome::Uncertain(error) => {
                        store.push_system_note(session_id, acp_thread::SystemNoteLevel::Error,
                            format!("Agent follow-up delivery could not be confirmed: {error}. It was not resent automatically; verify the conversation before retrying."), cx);
                    }
                }
                if matches!(session.read(cx).state, SessionState::Idle) {
                    let has_queued_compact = session.read(cx).pending_messages.iter().any(|bundle| crate::compact::is_compaction_blocks(&bundle.blocks));
                    if !has_queued_compact { session.update(cx, |s, _| {s.clear_compaction_request();}); }
                    store.flush_stopped_queue(session_id, false, cx);
                } else if !retry {
                    store.try_steer_pending(session_id, cx);
                }
                cx.notify();
            }).log_err();
        }).detach();
    }

    pub(super) fn flush_stopped_queue(
        &mut self,
        session_id: SolutionSessionId,
        flush_after_cancel: bool,
        cx: &mut Context<Self>,
    ) {
        // turn/completed may race ahead of the steer response. Only the
        // receipt may decide whether these bundles need a new turn.
        if self.active_steers.contains_key(&session_id) {
            return;
        }
        // Idle / flush-after-cancel. Deliver the MAIN-targeted
        // bundles as a new turn. Any Subagent-targeted leftover
        // belongs to a teammate that the now-ending parent turn
        // has finished — per design it is LOST (a follow-up for
        // teammate X is meaningless to the parent), so drop it
        // with a WARN rather than mis-route it to the main
        // thread. Partition the queue in one update.
        let (main_blocks, dropped_subagent) = self
            .sessions
            .get(&session_id)
            .cloned()
            .map(|s| {
                s.update(cx, |s, _| {
                    let mut main: Vec<acp::ContentBlock> = Vec::new();
                    let mut dropped: Vec<crate::model::PendingBundle> = Vec::new();
                    for bundle in s.pending_messages.drain(..) {
                        match bundle.target {
                            crate::model::QueueTarget::Main => main.extend(bundle.blocks),
                            crate::model::QueueTarget::Subagent(_) => dropped.push(bundle),
                        }
                    }
                    (main, dropped)
                })
            })
            .unwrap_or_default();
        if !dropped_subagent.is_empty() {
            let previews: Vec<String> = dropped_subagent
                .iter()
                .map(|b| {
                    let to = match &b.target {
                        crate::model::QueueTarget::Subagent(id) => id.as_ref(),
                        crate::model::QueueTarget::Main => "main",
                    };
                    format!("→{to}: {}", queue::summarize_blocks_for_log(&b.blocks))
                })
                .collect();
            log::warn!(
                target: "solution_agent::queue",
                "session={session_id} dropped {} subagent-targeted bundle(s) on turn end \
                 (addressee teammate finished without draining; no fallback to main) — content: [{}]",
                dropped_subagent.len(),
                previews.join(" | "),
            );
        }
        let had_pending = !main_blocks.is_empty() || !dropped_subagent.is_empty();
        if had_pending {
            self.mark_queue_changed(session_id, cx);
        }
        if !main_blocks.is_empty() {
            log::info!(
                target: "solution_agent::queue",
                "session={session_id} flushing {} Main block(s) \
                 (flush_after_cancel={flush_after_cancel}) preview={}",
                main_blocks.len(),
                queue::summarize_blocks_for_log(&main_blocks),
            );
            // Idle-flush is always end-of-turn: the agent
            // already produced a complete message, so prepend
            // the "not a reply" hint (stripped on render, like
            // the per-message timestamps already in the blocks).
            let mut with_hint = Vec::with_capacity(main_blocks.len() + 1);
            with_hint.push(acp::ContentBlock::Text(acp::TextContent::new(format!(
                "{}\n\n",
                queue::QUEUE_HINT_LINE
            ))));
            with_hint.extend(main_blocks);
            self.send_message_blocks(session_id, with_hint, cx).detach();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bundle(text: &str) -> crate::model::PendingBundle {
        crate::model::PendingBundle {
            id: uuid::Uuid::new_v4(),
            target: QueueTarget::Main,
            blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
        }
    }
    #[test]
    fn receipts_keep_rejections_and_remove_only_original_bundles() {
        let original = bundle("same text");
        let newer = bundle("same text");
        let reserved = HashSet::from([original.id]);
        let mut queue = std::collections::VecDeque::from([original, newer.clone()]);
        assert!(
            apply_receipt_to_queue(
                &mut queue,
                &reserved,
                &codex_native::SteerOutcome::Rejected(anyhow!("turn ended"))
            )
            .is_empty()
        );
        assert_eq!(queue.len(), 2);
        let delivered =
            apply_receipt_to_queue(&mut queue, &reserved, &codex_native::SteerOutcome::Accepted);
        assert_eq!(delivered.len(), 1);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id, newer.id);
        assert!(
            apply_receipt_to_queue(&mut queue, &reserved, &codex_native::SteerOutcome::Accepted)
                .is_empty()
        );
    }
    #[test]
    fn ambiguous_delivery_is_not_retried_and_clear_does_not_restore_it() {
        let original = bundle("followup");
        let reserved = HashSet::from([original.id]);
        let mut queue = std::collections::VecDeque::from([original]);
        let delivered = apply_receipt_to_queue(
            &mut queue,
            &reserved,
            &codex_native::SteerOutcome::Uncertain(anyhow!("timeout")),
        );
        assert_eq!(delivered.len(), 1);
        assert!(queue.is_empty());
        assert!(
            apply_receipt_to_queue(&mut queue, &reserved, &codex_native::SteerOutcome::Accepted)
                .is_empty()
        );
    }
    #[gpui::test]
    async fn newer_intent_stays_queued_while_handoff_is_pending(cx: &mut gpui::TestAppContext) {
        let (store, id, _tmp) = super::super::test_support::seed_store_with_session(cx).await;
        store.update(cx, |store, cx| {
            let session = store.session(id).unwrap();
            session.update(cx, |s, _| {
                s.begin_compaction_request();
                let compact = bundle(crate::compact::COMPACT_PROMPT_HEADING);
                let human = bundle("Newer user instruction");
                let compact_id = compact.id;
                let human_id = human.id;
                s.pending_messages.extend([compact, human]);
                assert_eq!(steerable_bundles(s), HashSet::from([compact_id]));
                s.clear_compaction_request();
                assert_eq!(steerable_bundles(s), HashSet::from([human_id]));
            });
        });
    }

    #[gpui::test]
    async fn stopped_flush_waits_for_inflight_receipt(cx: &mut gpui::TestAppContext) {
        let (store, id, _tmp) = super::super::test_support::seed_store_with_session(cx).await;
        store.update(cx, |store, cx| {
            let original = bundle("followup");
            let reserved = HashSet::from([original.id]);
            store
                .session(id)
                .unwrap()
                .update(cx, |s, _| s.pending_messages.push_back(original));
            store.active_steers.insert(
                id,
                PendingSteer {
                    token: uuid::Uuid::new_v4(),
                    bundles: reserved,
                },
            );
            let expected = {
                let session = store.session(id).unwrap();
                let s = session.read(cx);
                (s.epoch, s.acp_session_id.clone(), s.pending_compaction)
            };
            assert!(!store.rotation_steering_ready(id, &expected, cx).unwrap());
            store.flush_stopped_queue(id, false, cx);
            assert_eq!(
                store.session(id).unwrap().read(cx).pending_messages.len(),
                1
            );
            store.forget_client_send_ids(id);
            assert!(!store.active_steers.contains_key(&id));
            assert!(store.rotation_steering_ready(id, &expected, cx).unwrap());
            store.session(id).unwrap().update(cx, |s, _| s.bump_epoch());
            assert!(store.rotation_steering_ready(id, &expected, cx).is_err());
        });
    }
}
