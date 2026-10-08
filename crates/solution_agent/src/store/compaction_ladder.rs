//! The compaction ladder: how a request to hand the context off actually
//! reaches a working session, and when.
//!
//! Dropping the compaction prompt into a working session the moment it is
//! requested lands it mid-step, so the request escalates instead: signal that
//! no new work should start, repeat the signal after [`COMPACT_ESCALATION_SECS`],
//! then send the prompt for a manual request. Automatic requests can force an
//! active turn only at 80% context usage; below that, elapsed time only permits
//! a notice every thirty minutes. Both paths can hand off at a successful turn boundary.
//! The same ladder serves auto-compaction and the user's Compact button
//! ([`HandoffAsker`]). The rungs are counted per CONTEXT (reset on rotation) and
//! advanced on the editor's own clock by `tick_supervisor`: nobody re-clicks a
//! button to move a ladder along.

use gpui::{App, Context, TaskExt as _};

use crate::compact::CompactInitiator;
use crate::model::{HandoffAsker, HandoffLadder, SolutionSessionId};
use crate::store::SolutionAgentStore;
use crate::supervisor::{COMPACT_ESCALATION_SECS, CompactStep};

/// Automatic notices repeat while work continues; they never consume a
/// finite escalation budget or force a handoff because time elapsed.
const AUTO_COMPACT_REMINDER_SECS: i64 = 30 * 60;

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
    /// Arm the ladder as if auto-compaction had already asked once. Test-only:
    /// what this fixture exercises is the consequence of an armed ladder rather
    /// than how it got armed.
    #[cfg(test)]
    pub(crate) fn arm_compaction_ladder_for_test(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.session(id) {
            session.update(cx, |session, _| {
                session.handoff_ladder = Some(HandoffLadder {
                    asker: HandoffAsker::Auto,
                    end_turn_observed: false,
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

    /// A new request to hand off — the context crossing the auto-compaction
    /// threshold, or the user's Compact. Arms the ladder if nothing is climbing
    /// it yet, and moves it one rung.
    ///
    /// A user asking again means "now". A user's request takes over an
    /// automatic ladder from the first rung — the user has not asked yet — and
    /// auto-compaction leaves a user's ladder alone. No observer judge runs
    /// while a handoff is under way (see `tick_supervisor`), so one already
    /// reading the transcript is dropped here: it would rule on a conversation
    /// about to be replaced.
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
            (Some(HandoffAsker::User(_)), HandoffAsker::Auto) => {
                return HandoffRequest::Handled;
            }
            (Some(HandoffAsker::Auto), HandoffAsker::Auto) => set_note(note, cx),
            (None, _) | (Some(HandoffAsker::Auto), HandoffAsker::User(_)) => {
                self.supersede_judge(id, cx);
                session.update(cx, |s, cx| {
                    s.handoff_ladder = Some(HandoffLadder {
                        asker,
                        end_turn_observed: false,
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

    /// Per-tick escalation for every session climbing the ladder. An automatic
    /// ladder whose session had auto-compaction switched off since is dropped;
    /// the user's always climbs.
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
            if asker == HandoffAsker::Auto && !self.auto_compact_setting(id).enabled {
                self.reset_compaction_ladder(id, cx);
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
        let (ladder, busy, auto_force_due) = {
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
            // A user can compact an idle session immediately. An automatic
            // request must distinguish successful completion from a provider
            // failure that merely releases the prompt future (often to Idle).
            let main_active = self.main_turn_in_flight(id, cx);
            let busy = matches!(s.state, crate::model::SessionState::Running { .. })
                || main_active
                || s.has_live_background_work(chrono::Utc::now());
            if !main_active && ladder.asker == HandoffAsker::Auto && !ladder.end_turn_observed {
                return false;
            }
            // Elapsed time is not context pressure. Below 80%, an automatic
            // request may signal the agent but cannot interrupt active work.
            // Compare exact counts, not the rounded status-row percentage.
            let auto_force_due = ladder.asker == HandoffAsker::Auto
                && crate::model::session_context_usage(s, cx).is_some_and(|(used, max)| {
                    max > 0 && (used as u128) * 100 >= (max as u128) * 80
                });
            (ladder, busy, auto_force_due)
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
        let step = if !busy || auto_force_due {
            CompactStep::Force
        } else if ladder.asker == HandoffAsker::Auto {
            if ladder.asks == 0 {
                CompactStep::Ask
            } else if ladder.last_ask_ms.is_none_or(|at| {
                now.saturating_sub(at) >= AUTO_COMPACT_REMINDER_SECS * 1000
            }) {
                CompactStep::AskAgain
            } else {
                CompactStep::TooSoon
            }
        } else {
            crate::supervisor::compact_guard(
                ladder.asks,
                ladder.last_ask_ms.map(|at| now.saturating_sub(at)),
            )
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
                // Steered into the running turn where the agent supports that,
                // queued for the turn's end where it does not. The user's own
                // request travels as the user's message; the editor's carries
                // the editor marker, so neither the agent nor the reader takes
                // it for something the user typed.
                let text = agent_client_protocol::schema::v1::TextContent::new(ask);
                let (text, from_user) = match ladder.asker {
                    HandoffAsker::Auto => {
                        (text.meta(Some(acp_thread::meta_with_editor_recovery())), false)
                    }
                    HandoffAsker::User(_) => (text, true),
                };
                self.send_message_blocks_targeted(
                    id,
                    vec![agent_client_protocol::schema::v1::ContentBlock::Text(text)],
                    crate::model::QueueTarget::Main,
                    from_user,
                    cx,
                )
                .detach_and_log_err(cx);
                false
            }
        }
    }

    /// The ladder's last rung: send the compaction prompt on the asker's behalf.
    ///
    /// Deferred: `start_compact_for_session` re-acquires the global store and
    /// `read_with`s it, and this runs inside a store update (the tick) — calling
    /// it inline panicked the editor ("cannot read SolutionAgentStore while it
    /// is already being updated").
    pub(crate) fn run_ladder_compaction(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) {
        let Some(ladder) = self
            .session(id)
            .and_then(|session| session.read(cx).handoff_ladder.clone())
        else {
            return;
        };
        let initiator = match ladder.asker {
            HandoffAsker::Auto => CompactInitiator::Auto,
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
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.push_system_note(
                    id,
                    acp_thread::SystemNoteLevel::Info,
                    format!("Context compaction could not start: {reason}."),
                    cx,
                );
            });
        });
    }

    /// The early notice sent before the force rung. It deliberately does not
    /// ask the agent to compact itself: that instruction made the notice
    /// interrupt the active task by causing the agent to abandon the rest of
    /// its work and start a handoff. The editor owns the handoff and starts it
    /// naturally when the turn ends. Automatic force requires 80% context
    /// usage; only manual requests can force after the notices expire.
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
        let instruction = if again {
            "Keep working normally on the phase that was already active when the first notice \
             arrived. This notice does not change or shorten that phase. At its next natural \
             boundary — after the active phase and its verification are fully complete — pause \
             before beginning another phase or independent task. Do not call a compaction tool \
             because of this notice. The editor will start the handoff when the current turn \
             ends. If the same phase is still active at the next check, the editor will force \
             the handoff as the final escalation."
        } else {
            "This is an early signal, not a request to stop the current work. Keep working \
             normally on the phase already in progress; this notice does not change or shorten \
             it. At the next natural boundary — after the active phase and its verification are \
             fully complete — pause before beginning another phase or independent task. Keep \
             waiting for and integrating work already underway when that belongs to the active \
             phase. Do not call a compaction tool because of this notice; the editor will start \
             the handoff when the current turn ends."
        };
        let instruction = if again && !user {
            "Keep working normally on the phase that was already active when the first notice \
             arrived. This notice does not change or shorten that phase. At its next natural \
             boundary — after the active phase and its verification are fully complete — pause \
             before beginning another phase or independent task. Do not call a compaction tool \
             because of this notice. The editor will start the handoff when the current turn \
             ends. While work remains active, automatic forced handoff requires at least 80% \
             context usage; elapsed time alone will not force it. Until then, the editor may \
             repeat this reminder every thirty minutes."
        } else {
            instruction
        };
        let mut message = format!("{opening} {instruction}");
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
