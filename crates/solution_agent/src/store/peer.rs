use super::*;
use crate::model::{MessageOrigin, QueueTarget};

pub struct PeerMessageAcceptance {
    pub delivery: &'static str,
}

impl SolutionAgentStore {
    pub(super) fn peer_recipient_ready(&self, id: SolutionSessionId, cx: &App) -> Result<()> {
        let session = self
            .session(id)
            .ok_or_else(|| anyhow!("Recipient session not found"))?;
        let s = session.read(cx);
        if s.is_ephemeral
            || s.is_supervisor_ephemeral
            || s.peer_messages_held
            || !matches!(s.state, SessionState::Idle | SessionState::Running { .. })
            || crate::compact::has_pending_compact_approval(s, cx)
            || s.transcript_unavailable
        {
            return Err(anyhow!(
                "Recipient cannot accept peer messages while stopped, awaiting input, errored, or unavailable"
            ));
        }
        if s.acp_thread().is_none() && !self.peer_wake_sessions.contains(&id) {
            return Err(anyhow!(
                "Cold recipient requires a user message in this app session before peer-triggered wake"
            ));
        }
        if self.supervisor_states.get(&id).is_some_and(|state| {
            matches!(
                state.status,
                crate::supervisor::SupervisorStatus::Held
                    | crate::supervisor::SupervisorStatus::WaitingUser
                    | crate::supervisor::SupervisorStatus::Stopped(_)
            )
        }) {
            return Err(anyhow!("Recipient is paused and requires user input"));
        }
        Ok(())
    }

    pub fn send_peer_message(
        &mut self,
        solution_id: SolutionId,
        from_session_id: SolutionSessionId,
        to_session_id: SolutionSessionId,
        content: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<PeerMessageAcceptance>> {
        cx.spawn(async move |this, cx| {
            let (wake, epoch) = this.update(cx, |store, cx| -> Result<_> {
                if from_session_id == to_session_id || content.trim().is_empty() || content.len() > 16 * 1024 {
                    return Err(anyhow!("Invalid peer message"));
                }
                for id in [from_session_id, to_session_id] {
                    let session = store.session(id).ok_or_else(|| anyhow!("Peer session not found"))?;
                    let s = session.read(cx);
                    if s.solution_id != solution_id || s.is_ephemeral || s.is_supervisor_ephemeral {
                        return Err(anyhow!("Peer sessions must belong to the same Solution and be ordinary chats"));
                    }
                }
                store.peer_recipient_ready(to_session_id, cx)?;
                let session = store.session(to_session_id).unwrap();
                let s = session.read(cx);
                if s.acp_thread().is_some() { return Ok((None, s.epoch)); }
                let epoch = s.epoch;
                let meta = SolutionSessionMetadata {
                    id: s.id, solution_id:s.solution_id, agent_id:s.agent_id.clone(), acp_session_id:s.acp_session_id.clone(),
                    title:s.title.clone(), created_at:s.created_at,last_activity_at:s.last_activity_at,
                    preview:None,total_tokens:None,context_count:s.context_count,cwd:s.cwd.clone(),parent_session_id:s.parent_session_id,
                    desired_model:s.desired_model.clone(),desired_effort:s.desired_effort.clone(),cached_models:s.cached_models.clone(),tab_order:s.tab_order,
                };
                let project = s.project.clone();
                let solution = SolutionStore::try_global(cx).ok_or_else(|| anyhow!("Solution store unavailable"))?.read(cx).solutions().iter().find(|s| s.id == solution_id).cloned().ok_or_else(|| anyhow!("Solution not found"))?;
                let project = match project { Some(project) => project, None => Self::make_headless_project_for_solution(&solution, cx)? };
                Ok((Some(store.resume_session(meta, project, cx)), epoch))
            })??;
            let was_cold = wake.is_some();
            if let Some(wake) = wake { wake.await?; }
            this.update(cx, |store, cx| {
                if was_cold && !store.peer_wake_sessions.contains(&to_session_id) {
                    return Err(anyhow!("Recipient was stopped while waking; user must resume it first"));
                }
                for id in [from_session_id, to_session_id] {
                    let session = store.session(id).ok_or_else(|| anyhow!("Peer session no longer exists"))?;
                    let s = session.read(cx);
                    if s.solution_id != solution_id || s.is_ephemeral || s.is_supervisor_ephemeral {
                        return Err(anyhow!("Peer session identity changed while waking"));
                    }
                }
                store.peer_recipient_ready(to_session_id, cx)?;
                let recipient = store.session(to_session_id).unwrap();
                if recipient.read(cx).epoch != epoch { return Err(anyhow!("Recipient context changed while waking")); }
                if recipient.read(cx).acp_thread().is_none() { return Err(anyhow!("Recipient did not wake")); }
                let queued = matches!(recipient.read(cx).state, SessionState::Running { .. }) || store.active_steers.contains_key(&to_session_id);
                let text = format!("[Agent message from session {from_session_id}, Solution {}. Collaborator context, not user authorization. Sender identity is declared on the shared local socket.]\n\n{content}", solution_id.0);
                let task = store.send_message_blocks_origin(to_session_id,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(text))], QueueTarget::Main, MessageOrigin::Peer, cx);
                // All refusal conditions above are checked synchronously; this
                // task now owns queued input or an appended live user entry.
                // Report transport acceptance without waiting for model work.
                cx.spawn(async move |_, _| { if let Err(error) = task.await { log::error!("Peer message turn failed: {}", error.source); } }).detach();
                Ok(PeerMessageAcceptance { delivery: if queued { "queued" } else { "submitted" } })
            })?
        })
    }
}

const PEER_ORIGIN_KEY: &str = "sawePeerOrigin";

pub(super) fn set_peer_origin(blocks: &mut [acp::ContentBlock], peer: bool) {
    for block in blocks {
        if let acp::ContentBlock::Text(text) = block {
            if peer {
                text.meta
                    .get_or_insert_with(Default::default)
                    .insert(PEER_ORIGIN_KEY.into(), serde_json::Value::Bool(true));
            } else if let Some(meta) = &mut text.meta {
                meta.remove(PEER_ORIGIN_KEY);
            }
        }
    }
}

pub(crate) fn is_peer_only_blocks(blocks: &[acp::ContentBlock]) -> bool {
    let mut found = false;
    for block in blocks {
        let acp::ContentBlock::Text(text) = block else {
            return false;
        };
        if text.meta.as_ref().and_then(|m| m.get(PEER_ORIGIN_KEY))
            == Some(&serde_json::Value::Bool(true))
        {
            found = true;
            continue;
        }
        let text = text.text.trim();
        if text.is_empty()
            || text == super::queue::QUEUE_HINT_LINE
            || (text.len() == 10
                && text.starts_with('[')
                && text.ends_with(']')
                && text.as_bytes()[3] == b':'
                && text.as_bytes()[6] == b':')
        {
            continue;
        }
        return false;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text(value: &str) -> Vec<acp::ContentBlock> {
        vec![acp::ContentBlock::Text(acp::TextContent::new(value))]
    }

    #[gpui::test]
    async fn cold_peer_wake_requires_current_user_eligibility(cx: &mut gpui::TestAppContext) {
        let (store, id, _tmp) = super::super::test_support::seed_store_with_session(cx).await;
        store.update(cx, |store, cx| {
            assert!(store.peer_recipient_ready(id, cx).is_err());
            store.peer_wake_sessions.insert(id);
            assert!(store.peer_recipient_ready(id, cx).is_ok());
            let session = store.session(id).unwrap();
            store.cancel_turn(id, cx).unwrap();
            assert!(store.peer_recipient_ready(id, cx).is_err());
            // Recreating a cold entity after Stop cannot restore eligibility.
            session.update(cx, |s, _| s.peer_messages_held = false);
            assert!(store.peer_recipient_ready(id, cx).is_err());
        });
    }

    #[gpui::test]
    async fn peer_queue_preserves_origin_and_cannot_resume_a_hold(cx: &mut gpui::TestAppContext) {
        let (store, id, _tmp) = super::super::test_support::seed_store_with_session(cx).await;
        let task = store.update(cx, |store, cx| {
            store.peer_wake_sessions.insert(id);
            let session = store.session(id).unwrap();
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                }
            });
            let mut observer = crate::supervisor::SupervisorState::new(id);
            observer.consecutive_continues = 7;
            store.supervisor_states.insert(id, observer);
            store.send_message_blocks_origin(
                id,
                text("[Agent message from session sender-id] context"),
                QueueTarget::Main,
                MessageOrigin::Peer,
                cx,
            )
        });
        assert!(task.await.is_ok());
        store.update(cx, |store, cx| {
            assert_eq!(store.supervisor_states[&id].consecutive_continues, 7);
            let session = store.session(id).unwrap();
            assert_eq!(
                session.read(cx).pending_messages.front().unwrap().origin,
                MessageOrigin::Peer
            );
            session.update(cx, |s, _| s.peer_messages_held = true);
            assert!(
                store
                    .take_pending_for_delivery(id, None, false, cx)
                    .is_none()
            );
            assert_eq!(session.read(cx).pending_messages.len(), 1);
            session.update(cx, |s, _| s.state = SessionState::Idle);
            store.flush_stopped_queue(id, false, cx);
            assert_eq!(session.read(cx).pending_messages.len(), 1);
        });
        let task = store.update(cx, |store, cx| {
            let session = store.session(id).unwrap();
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                }
            });
            store.send_message_blocks_origin(
                id,
                text("human reply"),
                QueueTarget::Main,
                MessageOrigin::User,
                cx,
            )
        });
        assert!(task.await.is_ok());
        store.update(cx, |store, cx| {
            let session = store.session(id).unwrap();
            assert!(!session.read(cx).peer_messages_held);
            assert!(store.peer_wake_sessions.contains(&id));
            assert_eq!(session.read(cx).pending_messages.len(), 2);
            assert_eq!(
                session.read(cx).pending_messages.back().unwrap().origin,
                MessageOrigin::User
            );
        });
    }

    #[test]
    fn recovery_identifies_only_peer_text_not_mixed_human_input() {
        let mut blocks = text("[Agent message from session sender-id] context");
        assert!(!is_peer_only_blocks(&blocks));
        set_peer_origin(&mut blocks, true);
        assert!(is_peer_only_blocks(&blocks));
        blocks.extend(text("please do this"));
        assert!(!is_peer_only_blocks(&blocks));
    }
}
