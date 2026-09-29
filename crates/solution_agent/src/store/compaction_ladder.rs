//! The compaction ladder: how a request to hand the context off actually
//! reaches a working session, and when.
//!
//! Dropping the compaction prompt into a working session the moment it is
//! requested lands it mid-step, so the request escalates instead: ask the agent
//! to hand off itself, ask once more after [`COMPACT_ESCALATION_SECS`], then
//! send the prompt. The same ladder serves the observer's `compact` verdict and
//! the user's Compact button; only who is asking differs ([`HandoffAsker`]). The
//! rungs are counted per CONTEXT (reset on rotation) and advanced on the
//! editor's own clock by `tick_supervisor` — a judge reviewing running work is
//! on an hourly cadence and wakes with no memory of having asked, and nobody
//! re-clicks a button to move a ladder along.

use gpui::{App, Context, TaskExt as _};

use crate::compact::CompactInitiator;
use crate::model::{HandoffAsker, HandoffLadder, SolutionSessionId};
use crate::store::SolutionAgentStore;
use crate::supervisor::{COMPACT_ESCALATION_SECS, CompactStep};

/// What the caller of [`SolutionAgentStore::request_handoff`] still has to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandoffRequest {
    /// The ladder has it: an ask went out, or it is not yet time for one.
    Handled,
    /// Send the compaction prompt now — nothing left to ask for (the session is
    /// idle, or the asks are spent), or the user asked again, which means "now".
    CompactNow,
}

impl SolutionAgentStore {
    /// Arm the ladder as if the observer had already asked once. Test-only: the
    /// production path arms it through a `compact` verdict, which needs a live
    /// judge, and what this fixture exercises is the consequence of an armed
    /// ladder rather than how it got armed.
    #[cfg(test)]
    pub(crate) fn arm_compaction_ladder_for_test(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.session(id) {
            session.update(cx, |session, _| {
                session.handoff_ladder = Some(HandoffLadder {
                    asker: HandoffAsker::Observer,
                    asks: 1,
                    last_ask_ms: Some(chrono::Utc::now().timestamp_millis()),
                    last_force_ms: None,
                    note: None,
                    claims_user: false,
                });
            });
        }
    }

    /// Forget the compaction ladder for a session. Called when the transcript
    /// actually rotates (or is cleared): a fresh context has never been asked to
    /// hand off, so the next request starts at "ask", not at "force".
    pub(crate) fn reset_compaction_ladder(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) {
        if let Some(session) = self.session(id) {
            session.update(cx, |session, cx| {
                if session.handoff_ladder.take().is_some() {
                    cx.notify();
                }
            });
        }
    }

    /// A new request to hand off — a `compact` verdict, or the user's Compact.
    /// Arms the ladder if nothing is climbing it yet, and moves it one rung.
    ///
    /// A repeat from the same asker is a clock tick with a fresh note: the
    /// judge wakes with no memory of having asked, so its next verdict must not
    /// restart the count. A user asking again means "now". A user's request
    /// takes over an observer's ladder from the first rung — the user has not
    /// asked yet — and an observer's verdict leaves a user's ladder alone.
    pub(crate) fn request_handoff(
        &mut self,
        id: SolutionSessionId,
        asker: HandoffAsker,
        claims_user: bool,
        note: Option<String>,
        cx: &mut Context<Self>,
    ) -> HandoffRequest {
        let Some(session) = self.session(id) else {
            return HandoffRequest::Handled;
        };
        if session.read(cx).is_compaction_pending() {
            return HandoffRequest::Handled;
        }
        let note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
        let existing = session.read(cx).handoff_ladder.as_ref().map(|l| l.asker);
        let set_note = |note: Option<String>, cx: &mut Context<Self>| {
            if note.is_some() {
                session.update(cx, |s, _| {
                    if let Some(ladder) = s.handoff_ladder.as_mut() {
                        ladder.note = note;
                    }
                });
            }
        };
        match (existing, asker) {
            (Some(HandoffAsker::User(_)), HandoffAsker::User(_)) => {
                set_note(note, cx);
                return HandoffRequest::CompactNow;
            }
            (Some(HandoffAsker::User(_)), HandoffAsker::Observer) => {
                return HandoffRequest::Handled;
            }
            (Some(HandoffAsker::Observer), HandoffAsker::Observer) => set_note(note, cx),
            (None, _) | (Some(HandoffAsker::Observer), HandoffAsker::User(_)) => {
                // The observer's own request comes from the verdict of the
                // judge being applied right now; anything else outdates it.
                if asker != HandoffAsker::Observer {
                    self.supersede_judge(id, cx);
                }
                session.update(cx, |s, cx| {
                    s.handoff_ladder = Some(HandoffLadder {
                        asker,
                        asks: 0,
                        last_ask_ms: None,
                        last_force_ms: None,
                        note,
                        claims_user,
                    });
                    cx.notify();
                });
            }
        }
        if self.climb_compaction_ladder(id, cx) {
            HandoffRequest::CompactNow
        } else {
            HandoffRequest::Handled
        }
    }

    /// Per-tick escalation for every session climbing the ladder. An observer's
    /// ladder only moves while the session is supervised; the user's always.
    pub(crate) fn tick_compaction_ladders(&mut self, cx: &mut Context<Self>) {
        let climbing: Vec<(SolutionSessionId, HandoffAsker)> = self
            .sessions
            .iter()
            .filter_map(|(id, session)| {
                session
                    .read(cx)
                    .handoff_ladder
                    .as_ref()
                    .map(|ladder| (*id, ladder.asker))
            })
            .collect();
        for (id, asker) in climbing {
            if asker == HandoffAsker::Observer
                && !self.supervisor_states.get(&id).is_some_and(|s| s.enabled)
            {
                continue;
            }
            if self.climb_compaction_ladder(id, cx) {
                self.run_ladder_compaction(id, cx);
            }
        }
    }

    /// Move `id`'s ladder one rung, if it is time: ask (or ask again), or report
    /// that the prompt should be sent now (`true`, the Force rung — stamped, so a
    /// refused force backs off instead of retrying every tick).
    fn climb_compaction_ladder(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.session(id) else {
            return false;
        };
        let now = chrono::Utc::now().timestamp_millis();
        let (ladder, busy) = {
            let s = session.read(cx);
            let Some(ladder) = s.handoff_ladder.clone() else {
                return false;
            };
            // Already on its way (the agent started its own handoff), or waiting
            // on the user's answer to a permission prompt, which the compaction
            // prompt could not get past anyway.
            if s.is_compaction_pending() || crate::compact::has_pending_compact_approval(s, cx) {
                return false;
            }
            // The ladder exists to let an agent finish what it is holding. A
            // session holding nothing — no turn running, no background agent or
            // shell still working for it — has nothing to finish, and "wrap up,
            // then hand off" addressed to it is a message nobody acts on until
            // the user types again. It is compacted now instead.
            let busy = matches!(s.state, crate::model::SessionState::Running { .. })
                || self.main_turn_in_flight(id, cx)
                || s.has_live_background_work(chrono::Utc::now());
            (ladder, busy)
        };
        // Checked before the not-busy short-circuit, which would otherwise skip
        // straight past it: a permanent refusal (no headroom left) would be
        // retried every five seconds for as long as it stands.
        if ladder
            .last_force_ms
            .is_some_and(|at| now.saturating_sub(at) < (COMPACT_ESCALATION_SECS as i64) * 1000)
        {
            return false;
        }
        let step = if busy {
            crate::supervisor::compact_guard(
                ladder.asks,
                ladder.last_ask_ms.map(|at| now.saturating_sub(at)),
            )
        } else {
            CompactStep::Force
        };
        match step {
            CompactStep::TooSoon => false,
            CompactStep::Force => {
                session.update(cx, |s, _| {
                    if let Some(ladder) = s.handoff_ladder.as_mut() {
                        ladder.last_force_ms = Some(now);
                    }
                });
                true
            }
            CompactStep::Ask | CompactStep::AskAgain => {
                // An ask issued while the human is typing is PARKED, not
                // delivered, and a genuine user send then discards it. Counting
                // it as a spent rung would march the ladder toward a forced
                // handoff on the strength of a message the agent never saw.
                if self.user_is_composing(id, now) {
                    return false;
                }
                let again = step == CompactStep::AskAgain;
                let ask =
                    self.compaction_request_message(id, ladder.asker, again, ladder.note.clone(), cx);
                session.update(cx, |s, cx| {
                    if let Some(ladder) = s.handoff_ladder.as_mut() {
                        ladder.asks = ladder.asks.saturating_add(1);
                        ladder.last_ask_ms = Some(now);
                    }
                    cx.notify();
                });
                match ladder.asker {
                    HandoffAsker::Observer => self.send_supervisor_nudge(id, ask, cx).detach(),
                    // The user's own request, so it travels as the user's
                    // message: steered into the running turn where the agent
                    // supports that, queued for the turn's end where it does not.
                    HandoffAsker::User(_) => {
                        let blocks = vec![agent_client_protocol::schema::v1::ContentBlock::Text(
                            agent_client_protocol::schema::v1::TextContent::new(ask),
                        )];
                        self.send_message_blocks_targeted(
                            id,
                            blocks,
                            crate::model::QueueTarget::Main,
                            true,
                            cx,
                        )
                        .detach_and_log_err(cx);
                    }
                }
                false
            }
        }
    }

    /// The ladder's last rung: send the compaction prompt on the asker's behalf.
    ///
    /// Deferred: `start_compact_for_session` re-acquires the global store and
    /// `read_with`s it, and this runs inside a store update (the verdict's MCP
    /// lease, the tick) — calling it inline panicked the editor on every
    /// observer `compact` verdict ("cannot read SolutionAgentStore while it is
    /// already being updated").
    pub(crate) fn run_ladder_compaction(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) {
        let Some(ladder) = self
            .session(id)
            .and_then(|session| session.read(cx).handoff_ladder.clone())
        else {
            return;
        };
        let initiator = match ladder.asker {
            HandoffAsker::Observer => CompactInitiator::Observer,
            HandoffAsker::User(initiator) => initiator,
        };
        cx.defer(move |cx| {
            let reason = match crate::compact::start_compact_for_session(
                id,
                initiator,
                ladder.note.as_deref(),
                cx,
            ) {
                Ok(outcome) if outcome.queued => return,
                Ok(outcome) => outcome.reason.unwrap_or_else(|| "unknown reason".to_string()),
                Err(err) => format!("could not run: {err}"),
            };
            // A busy session, or sub-agents still running on an agent that
            // cannot rotate in place, clear by themselves; the back-off retries.
            if reason.starts_with("session is busy")
                || reason.starts_with(crate::compact::BACKGROUND_AGENTS_RUNNING)
            {
                return;
            }
            log::warn!(
                target: "solution_agent::store",
                "session={id} handoff ladder ({:?}): compaction refused: {reason}",
                ladder.asker
            );
            SolutionAgentStore::global(cx).update(cx, |store, cx| match ladder.asker {
                // Where the judge reads it each wake-up, so the "don't re-issue
                // compact until the transcript rotates" rule can fire.
                HandoffAsker::Observer => store.append_supervisor_diary_note(
                    id,
                    &format!(
                        "compact verdict REFUSED ({reason}); do not re-issue compact until the transcript rotates"
                    ),
                    cx,
                ),
                HandoffAsker::User(_) => store.push_system_note(
                    id,
                    acp_thread::SystemNoteLevel::Info,
                    format!("Context compaction could not start: {reason}."),
                    cx,
                ),
            });
        });
    }

    /// The text the agent is asked to act on. It names the exact tool, because
    /// "compact when convenient" with no verb is how an agent acknowledges a
    /// request and does nothing, and it states that the editor will do it
    /// anyway — the deadline is real, and hiding it would make the eventual
    /// forced compaction look arbitrary.
    pub(crate) fn compaction_request_message(
        &self,
        id: SolutionSessionId,
        asker: HandoffAsker,
        again: bool,
        note: Option<String>,
        cx: &App,
    ) -> String {
        // Same reading the judge's briefing quotes (`session_context_usage`), so
        // the two cannot tell the agent two different numbers in the same
        // minute — the ask is a follow-up to that briefing.
        let fullness = self
            .session(id)
            .and_then(|session| crate::model::session_context_usage(&session.read(cx), cx))
            .map(|(used, max)| ((used as f64 / max as f64) * 100.0).round() as u64);
        let user = matches!(asker, HandoffAsker::User(_));
        let opening = match (user, again, fullness) {
            (true, false, Some(pct)) => {
                format!("The user asked for a context handoff (your context is {pct}% full).")
            }
            (true, false, None) => "The user asked for a context handoff.".to_string(),
            (true, true, _) => "The user's context handoff request is still open.".to_string(),
            (false, false, Some(pct)) => format!("Your context is {pct}% full."),
            (false, false, None) => "Your context is getting large.".to_string(),
            (false, true, Some(pct)) => {
                format!(
                    "Your context is {pct}% full and the earlier handoff request is still open."
                )
            }
            (false, true, None) => {
                "The earlier handoff request is still open and your context keeps growing."
                    .to_string()
            }
        };
        let closing = if again {
            "If it is still open at the next check, the editor will start the handoff for \
             you, wherever you happen to be."
        } else {
            "If nothing happens, the editor will start it for you."
        };
        let mut message = format!(
            "{opening} Finish the step you are on — do not start new work — then hand off: \
             call the `solution_agent.start_compact` tool on the `sawe` MCP server with \
             {{\"session_id\": \"{id}\", \"initiator\": \"agent\"}}. It gives you the standard \
             handoff instructions to follow. Sub-agents still running keep running across the \
             handoff and report into the next context, so you need not wait for them. \
             {closing}"
        );
        if let Some(note) = note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            message.push_str(if user {
                "\n\nThe user's note for this handoff: "
            } else {
                "\n\nWhat this handoff must not lose: "
            });
            message.push_str(note);
        }
        message
    }
}
