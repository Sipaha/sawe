//! The compaction ladder: what a judge's `compact` verdict actually does to a
//! session, and when.
//!
//! Dropping the compaction prompt into a working session the moment the verdict
//! arrives lands it mid-step, so the request escalates instead: ask the agent to
//! hand off itself, ask once more after [`crate::supervisor::COMPACT_ESCALATION_SECS`],
//! then send it. The rungs are counted per CONTEXT (reset on rotation) and
//! advanced by `tick_supervisor`, not by the next verdict — a judge reviewing
//! running work is on an hourly cadence and wakes with no memory of having
//! asked, which is the same reason `continue_guard` lives in the editor.

use gpui::{App, Context};

use crate::model::SolutionSessionId;
use crate::store::SolutionAgentStore;
use crate::supervisor::COMPACT_ESCALATION_SECS;

impl SolutionAgentStore {
    /// True while the observer is mid-ladder for this session — it has asked the
    /// agent to hand off and the transcript has not rotated yet. Read by
    /// `compact::start_compact_for_session` to answer "who is this compaction
    /// really coming from": a self-compaction the agent performs because the
    /// observer asked is an OBSERVER compaction wearing the agent's clothes, and
    /// must not wipe the observer's memory the way a human `/compact` does.
    pub(crate) fn observer_requested_compaction(&self, id: SolutionSessionId) -> bool {
        self.supervisor_states
            .get(&id)
            .is_some_and(|state| state.compact_requests > 0)
    }

    /// Arm the ladder as if the observer had already asked once. Test-only: the
    /// production path arms it through a `compact` verdict, which needs a live
    /// judge, and what this fixture exercises is the consequence of an armed
    /// ladder rather than how it got armed.
    #[cfg(test)]
    pub(crate) fn arm_compaction_ladder_for_test(&mut self, id: SolutionSessionId) {
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            state.compact_requests = 1;
            state.last_compact_request_ms = Some(chrono::Utc::now().timestamp_millis());
        }
    }

    /// Forget the compaction ladder for a session. Called when the transcript
    /// actually rotates (or is cleared): a fresh context has never been asked to
    /// hand off, so the next `compact` verdict starts at "ask", not at "force".
    pub(crate) fn reset_compaction_ladder(&mut self, id: SolutionSessionId) {
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            state.compact_requests = 0;
            state.last_compact_request_ms = None;
            state.last_force_ms = None;
            // The note is scoped to the context it was written about. Left
            // standing, a later ask in a FRESH context would resurrect it and
            // tell the agent to preserve something from a transcript that no
            // longer exists.
            state.compact_request_note = None;
        }
    }

    /// Which rung of the compaction ladder a `compact` verdict lands on for this
    /// session, or `None` when the session is already compacting (a request is
    /// queued and waiting) — in which case the verdict has nothing left to do.
    pub(crate) fn compaction_request_step(
        &self,
        id: SolutionSessionId,
        cx: &App,
    ) -> Option<crate::supervisor::CompactStep> {
        let session = self.session(id)?;
        let busy;
        {
            let session = session.read(cx);
            if session.is_compaction_pending() {
                return None;
            }
            // The ladder exists to let an agent finish what it is holding. A
            // session that is holding nothing — no turn running, no background
            // agent or shell still working for it — has nothing to finish, and
            // "wrap up, then hand off" addressed to a paused session is a
            // message nobody will act on until the user types again. It is
            // compacted now (below, once the refusal back-off has had its say),
            // which is also what the operator would do by hand.
            busy = matches!(session.state, crate::model::SessionState::Running { .. })
                || session.has_live_background_work(chrono::Utc::now());
        }
        let state = self.supervisor_states.get(&id)?;
        let now = chrono::Utc::now().timestamp_millis();
        // A force that was REFUSED changes nothing else the ladder reads, so
        // this is the only thing standing between a permanent refusal (an
        // unanswered permission prompt, no headroom left) and a retry every
        // five seconds for as long as it stands. Checked before the not-busy
        // short-circuit below, which would otherwise skip straight past it.
        if state
            .last_force_ms
            .is_some_and(|at| now.saturating_sub(at) < (COMPACT_ESCALATION_SECS as i64) * 1000)
        {
            return Some(crate::supervisor::CompactStep::TooSoon);
        }
        if !busy {
            return Some(crate::supervisor::CompactStep::Force);
        }
        let since = state
            .last_compact_request_ms
            .map(|at| now.saturating_sub(at));
        Some(crate::supervisor::compact_guard(
            state.compact_requests,
            since,
        ))
    }

    /// Send the compaction request itself, on the observer's behalf — the
    /// ladder's last rung, reached either from a `compact` verdict whose asks
    /// are spent or from the escalation tick.
    pub(crate) fn run_observer_compaction(
        &mut self,
        id: SolutionSessionId,
        note: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // `start_compact_for_session` re-acquires the global
        // `SolutionAgentStore` and `read_with`s it — but `apply_verdict`
        // runs INSIDE the MCP tool's `store.update(...)` lease (mcp.rs
        // `SupervisorVerdictTool::run`), so calling it inline reads the
        // store entity while it is `&mut`-borrowed → `double_lease_panic`
        // ("cannot read SolutionAgentStore while it is already being
        // updated"). Every supervisor "compact" verdict crashed the
        // editor this way. Defer the call past the current update so the
        // lease is released first (decision: never read an entity during
        // its own mutation — snapshot before, or defer).
        cx.defer(move |cx| {
                let outcome = crate::compact::start_compact_for_session(
                    id,
                    crate::compact::CompactInitiator::Observer,
                    note.as_deref(),
                    cx,
                );
                // A compact can be SILENTLY refused (session busy, conversation
                // too short, no headroom) and the refusal never reaches the
                // judge — so a cap-EXEMPT `compact` verdict can loop every idle
                // tick. Record the refusal in the observer's diary (which the
                // judge reads each wake-up) so the "don't re-issue compact if
                // the transcript didn't rotate" prompt rule can actually fire
                // (finding #10).
                let note = match &outcome {
                    Err(err) => Some(format!("compact verdict could not run: {err}")),
                    Ok(o) if !o.queued => {
                        let reason = o.reason.as_deref().unwrap_or("unknown reason");
                        // "session is busy" is a transient race-window refusal
                        // (the agent started a turn between the verdict and the
                        // deferred compact) — it self-resolves and has nothing
                        // to do with transcript rotation, so don't mislead the
                        // judge into deferring compaction. Only diary the
                        // rotation-relevant refusals (too short / no headroom).
                        if reason.starts_with("session is busy") {
                            None
                        } else {
                            Some(format!(
                                "compact verdict REFUSED ({reason}); do not re-issue compact until the transcript rotates"
                            ))
                        }
                    }
                    Ok(_) => None,
                };
                if let Some(note) = note {
                    log::warn!("apply_verdict compact({id}): {note}");
                    SolutionAgentStore::global(cx).update(cx, |store, cx| {
                        store.append_supervisor_diary_note(id, &note, cx);
                    });
                }
            });
    }

    /// Move the compaction ladder for `id` one rung, and report whether the
    /// caller should now run the compaction itself (`true` = the Force rung).
    ///
    /// Shared by the two things that can move it: a judge's `compact` verdict,
    /// which ARMS it and supplies the handoff note, and the supervisor tick,
    /// which advances it on the clock. The clock is what makes this a ladder
    /// rather than a coincidence — a running agent may not be judged again for
    /// an hour, and "ask, then ask again in fifteen minutes" must not depend on
    /// the judge happening to wake.
    pub(crate) fn advance_compaction_ladder(
        &mut self,
        id: SolutionSessionId,
        note: Option<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(step) = self.compaction_request_step(id, cx) else {
            return false;
        };
        match step {
            crate::supervisor::CompactStep::TooSoon => false,
            crate::supervisor::CompactStep::Force => {
                // Stamp the attempt, because a forced compaction can be REFUSED
                // — an unresolved permission prompt, no headroom left — and a
                // refusal changes nothing the guard reads. Unstamped, every tick
                // would retry it: a diary note and a warn line every few seconds
                // for as long as the refusal stands. Stamped, a refused force
                // backs off exactly like an unanswered ask.
                if let Some(state) = self.supervisor_states.get_mut(&id) {
                    state.last_force_ms = Some(chrono::Utc::now().timestamp_millis());
                }
                true
            }
            step => {
                // An ask issued while the human is typing is PARKED, not
                // delivered, and a genuine user send then discards it. Counting
                // it as a spent rung would march the ladder toward a forced
                // handoff on the strength of a message the agent never saw —
                // and the next ask would open with "the earlier handoff request
                // is still open", naming it. Let the tick come back instead.
                if self.user_is_composing(id, chrono::Utc::now().timestamp_millis()) {
                    return false;
                }
                let again = matches!(step, crate::supervisor::CompactStep::AskAgain);
                let note = note.or_else(|| {
                    self.supervisor_states
                        .get(&id)
                        .and_then(|state| state.compact_request_note.clone())
                });
                let ask = self.compaction_request_message(id, again, note.clone(), cx);
                if let Some(state) = self.supervisor_states.get_mut(&id) {
                    state.compact_requests = state.compact_requests.saturating_add(1);
                    state.last_compact_request_ms = Some(chrono::Utc::now().timestamp_millis());
                    if note.is_some() {
                        state.compact_request_note = note;
                    }
                }
                self.send_supervisor_nudge(id, ask, cx).detach();
                false
            }
        }
    }

    /// Per-tick escalation for a session that was asked to hand off and has not.
    /// Runs only while the ladder is armed, so an unsupervised or
    /// never-asked session costs one map lookup.
    pub(crate) fn tick_compaction_ladder(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) {
        let armed = self
            .supervisor_states
            .get(&id)
            .is_some_and(|state| state.enabled && state.compact_requests > 0);
        if !armed {
            return;
        }
        if self.advance_compaction_ladder(id, None, cx) {
            // Asking is spent. Run the compaction the way the verdict would
            // have, carrying the note from the verdict that armed the ladder.
            let note = self
                .supervisor_states
                .get(&id)
                .and_then(|state| state.compact_request_note.clone());
            self.run_observer_compaction(id, note, cx);
        }
    }

    /// The text the agent is asked to act on. It names the exact tool, because
    /// "compact when convenient" with no verb is how an agent acknowledges a
    /// request and does nothing, and it states that the editor will do it
    /// anyway — the deadline is real, and hiding it would make the eventual
    /// forced compaction look arbitrary.
    pub(crate) fn compaction_request_message(
        &self,
        id: SolutionSessionId,
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
        let opening = match (again, fullness) {
            (false, Some(pct)) => format!("Your context is {pct}% full."),
            (false, None) => "Your context is getting large.".to_string(),
            (true, Some(pct)) => {
                format!(
                    "Your context is {pct}% full and the earlier handoff request is still open."
                )
            }
            (true, None) => {
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
             handoff instructions to follow. {closing}"
        );
        if let Some(note) = note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            message.push_str("\n\nWhat this handoff must not lose: ");
            message.push_str(note);
        }
        message
    }
}
