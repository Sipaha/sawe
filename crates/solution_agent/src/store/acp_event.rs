//! ACP thread-event handler: the single `handle_acp_event` state machine that
//! ingests `acp_thread::AcpThreadEvent`s from a live session subscription and
//! folds them into the Store's per-session entry model (index arithmetic over
//! `global_entry_index` / `mod_seq` / `live_base` / `cold_count`), persistence,
//! and delta-sync. Relocated VERBATIM from `store.rs` (Tier-4 god-object
//! refactor B4) — an `impl SolutionAgentStore` method that still owns
//! `&mut SolutionAgentStore` / `Context<Self>`; this split moves *source text*,
//! not state ownership. It is the nexus of hardening #35 (mod_seq end-of-turn
//! tail-flush) and #44 (self-resume rearm hook); its match arms and index
//! arithmetic are preserved byte-for-byte and must not be decomposed.

use super::*;

impl SolutionAgentStore {
    pub(super) fn handle_acp_event(
        &mut self,
        session_id: SolutionSessionId,
        event: &acp_thread::AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(session_entity) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        match event {
            acp_thread::AcpThreadEvent::NewEntry => {
                // An editor-injected `SystemNote` is not agent activity — it
                // must NOT flip an Idle session to Running (that would make the
                // stuck-session watchdog and the status row think a turn is in
                // flight) nor reset the silence clock. Still convert + persist +
                // delta-sync it below so it shows in the conversation.
                let is_system_note = session_entity
                    .read(cx)
                    .acp_thread()
                    .map(|t| {
                        matches!(
                            t.read(cx).entries().last(),
                            Some(acp_thread::AgentThreadEntry::SystemNote(_))
                        )
                    })
                    .unwrap_or(false);
                if !is_system_note {
                    self.mutate_state(
                        session_id,
                        |state| {
                            // Also clears a latched `Errored` — see
                            // `SessionState::resume_on_activity` (bug #5).
                            state.resume_on_activity();
                        },
                        cx,
                    );
                    if let Some(s) = self.sessions.get(&session_id).cloned() {
                        s.update(cx, |s, _| s.last_activity_at = Utc::now());
                    }
                    // Genuinely-new agent activity (NOT a system note) on a
                    // session parked in `WaitingUser`/`Stopped(Done)` means it
                    // resumed on its own — re-arm supervision so the status stops
                    // hanging at "waiting for user" while the agent works again.
                    self.rearm_supervisor_on_self_activity(session_id, cx);
                }
                // First user message appends a NewEntry — refresh DB so the
                // History popover preview stops being NULL.
                self.persist_session_row(session_id, cx);
                // `entry_index` on AcpThreadEvent is LOCAL to the live thread's
                // entries vector. The global index counts the cold prefix first.
                // Without the offset, the first live entry after a cold→live
                // transition would overwrite a cold entry in `session.entries`.
                let (cold_count, live_last_local) = {
                    let session = session_entity.read(cx);
                    let cold = session.live_base;
                    let live_last = session
                        .acp_thread()
                        .map(|thread| thread.read(cx).entries().len().saturating_sub(1))
                        .unwrap_or(0);
                    (cold, live_last)
                };
                let global_entry_index = cold_count + live_last_local;
                // Incremental NewEntry: convert just the new live entry and push
                // it onto `session.entries`, stamping `created_ms = now_ms`.
                // Fill any gap below the new global index with sentinel entries
                // (a resumed pre-feature session whose cold timestamps were never
                // captured). The genuinely-new entry at `global_entry_index`
                // gets `now_ms`; entries already present are left untouched.
                let now_ms = Utc::now().timestamp_millis();
                let new_entries = {
                    let s = session_entity.read(cx);
                    let live = s.acp_thread().map(|t| t.read(cx).entries()).unwrap_or(&[]);
                    // Gap entries: existing cold entries beyond what `entries` already
                    // holds (pre-feature sessions restored without timestamps).
                    let current_len = s.entries.len();
                    let mut additions: Vec<crate::session_entry::SessionEntry> = Vec::new();
                    // Fill any gap between the current entries length and the new index.
                    // After unification (Phase 2), cold restore guarantees entries.len() ==
                    // live_base, so gap indices are always in live space — the cold branch
                    // below is unreachable and has been removed to prevent accidental
                    // duplication of cold entries.
                    let live_base = s.live_base;
                    for gap_idx in current_len..global_entry_index {
                        // These are pre-existing entries whose creation time was never
                        // captured; convert from live and stamp with the sentinel.
                        let local = gap_idx - live_base;
                        let entry = {
                            let Some(e) = live.get(local) else {
                                log::warn!(
                                    "solution_agent NewEntry gap-fill: live entry at local idx {} missing (live.len={})",
                                    local,
                                    live.len(),
                                );
                                continue;
                            };
                            crate::session_entry::to_session_entry(e, cx)
                        };
                        let mut gap_entry = entry;
                        gap_entry.created_ms = crate::model::NO_TIMESTAMP_MS;
                        additions.push(gap_entry);
                    }
                    // The new entry at global_entry_index, stamped with now_ms.
                    if current_len + additions.len() == global_entry_index {
                        let local = global_entry_index - s.live_base;
                        if let Some(live_entry) = live.get(local) {
                            let mut new_entry =
                                crate::session_entry::to_session_entry(live_entry, cx);
                            new_entry.created_ms = now_ms;
                            additions.push(new_entry);
                        }
                    }
                    additions
                };
                // Pre-extend length is the first index the newly-stamped entries
                // begin at; captured before the closure so we can stamp exactly
                // the appended entries' `mod_seq`.
                let first_new = session_entity.read(cx).entries.len();
                session_entity.update(cx, |s, cx| {
                    s.entries.extend(new_entries);
                    let new_count = s.entries.len() - first_new;
                    let seqs: Vec<u64> = (0..new_count).map(|_| s.bump_change_seq()).collect();
                    for (entry, seq) in s.entries[first_new..].iter_mut().zip(seqs) {
                        entry.mod_seq = seq;
                    }
                    s.rebuild_streams();
                    cx.notify();
                });
                // Persist authority is `streams[Main]` (phase 6b): flush the Main
                // stream incrementally after the rebuild so the coalesced,
                // Main-local rows land — NOT the (possibly torn) flat entries.
                self.persist_main_stream(session_id, cx);
                cx.emit(SolutionAgentStoreEvent::SessionMessageAppended(
                    session_id,
                    global_entry_index,
                ));
                // Subagent-tab lifecycle: a brand-new Task/Agent ToolCall in
                // InProgress is a spawn signal. The `local_entry_index` here is
                // the live thread's local index (entries.len() - 1), which is
                // what `apply_subagent_lifecycle` needs to look up the entry.
                let local_entry_index = session_entity
                    .read(cx)
                    .acp_thread()
                    .map(|thread| thread.read(cx).entries().len().saturating_sub(1));
                if let Some(idx) = local_entry_index {
                    self.apply_subagent_lifecycle(session_id, idx, cx);
                    // A `<task-notification>` completion block arrives as a
                    // user-role message, which `apply_subagent_lifecycle`
                    // ignores (non-ToolCall). Scan the same entry separately so
                    // a finished `Bash(bg)` shell flips to `Exited(code)`.
                    self.observe_task_notification(session_id, idx, cx);
                }
            }
            acp_thread::AcpThreadEvent::Stopped(_) => {
                // A turn that runs to `Stopped` is proof the agent responded —
                // cancel any pending usage-limit / backoff resume gate so the
                // session isn't kept waiting (and a stale wake timer doesn't
                // fire a redundant judge) after the wall has cleared (#7). A
                // re-hit of the wall arrives as `Error`, not `Stopped`, so the
                // gate survives that case.
                self.clear_resume_gate_on_agent_response(session_id, cx);
                // A completed turn is genuinely-new state: cancel any parked
                // one-shot `wait`. Otherwise, if the agent self-resumed and
                // FINISHED before the wait deadline, the mechanism would still
                // wake it at the deadline ("the task you were waiting on should be
                // done — check it") minutes after it already did exactly that
                // (finding #8). A user message already clears this in the send
                // funnel; an agent-side completion must too.
                if self
                    .supervisor_states
                    .get(&session_id)
                    .is_some_and(|s| s.wait_until_ms.is_some())
                {
                    if let Some(state) = self.supervisor_states.get_mut(&session_id) {
                        state.wait_until_ms = None;
                    }
                    self.persist_supervisor_state(session_id, cx);
                }
                // Snapshot the Running turn's elapsed time BEFORE the
                // state flip — `mutate_state` overwrites `started_at`
                // with `SessionState::Idle` so we can't recover it
                // after. Stamped onto the session for the status row's
                // "Done in Xs" indicator (cleared on the next Running).
                let elapsed = self.sessions.get(&session_id).and_then(|entity| {
                    if let SessionState::Running { started_at, .. } = &entity.read(cx).state {
                        Some(started_at.elapsed())
                    } else {
                        None
                    }
                });
                // Flush any pending end-of-turn entry-update debounce SYNCHRONOUSLY.
                //
                // The last assistant text of a turn arrives via `EntryUpdated`,
                // whose `SessionMessageAppended` emit (and thus its
                // `agent_session_dirty` re-poll signal) is debounced 500 ms / 2 s
                // to coalesce a streaming burst. At turn end that pending debounce
                // task is the ONLY append signal carrying the final flushed tail,
                // and it is fragile: if `Stopped` does not change the state
                // discriminant (e.g. the session was already Idle / Stopping, so
                // `mark_state_changed` below emits no dirty), or the debounce task
                // is dropped before it fires, the final entry's append notification
                // never reaches the client. The mobile then keeps showing the
                // turn WITHOUT its last message until the next client→server
                // interaction re-polls — the bug this flush fixes. Emitting the
                // queued append here (and clearing the slot so it can't double-fire
                // when its timer elapses) guarantees the final entry's
                // `SessionMessageAppended` + `agent_session_dirty` ride out
                // immediately on the turn-completion tick.
                self.flush_pending_entry_appends(session_id, cx);
                self.mutate_state(session_id, |state| *state = SessionState::Idle, cx);
                if let Some(s) = self.sessions.get(&session_id).cloned() {
                    s.update(cx, |s, _| {
                        s.last_activity_at = Utc::now();
                        if let Some(d) = elapsed {
                            s.last_turn_duration = Some(d);
                        }
                    });
                    // Emit a metrics notification on turn completion so the
                    // mobile client sees an updated last_activity_at without
                    // waiting for the next TokenUsageUpdated. Throttled
                    // (2 s window) and non-sequenced per spec.
                    let (last_activity_at, total_tokens, max_tokens) = {
                        let r = s.read(cx);
                        (
                            r.last_activity_at,
                            r.cached_total_tokens,
                            r.cached_max_tokens,
                        )
                    };
                    self.metrics_emitter.emit_if_ready(
                        cx,
                        &session_id,
                        serde_json::json!({
                            "session_id": session_id.to_string(),
                            "last_activity_at": last_activity_at,
                            "total_tokens": total_tokens,
                            "max_tokens": max_tokens,
                        }),
                    );
                }
                // Token usage is finalised on turn completion — refresh DB
                // so the History popover token column reflects the latest.
                self.persist_session_row(session_id, cx);
                // Flush queued follow-ups (if any). All pending entries
                // are drained and concatenated into ONE send — the user
                // typed them as a fast-fire stream while the agent was
                // working, so it's their joint intent for the next turn
                // rather than N independent prompts. A Cancelled stop
                // (user pressed Stop) is treated as "abandon what I
                // queued too": the queue is cleared without sending.
                if let acp_thread::AcpThreadEvent::Stopped(reason) = event {
                    // `flush_after_cancel` (set by `interrupt_and_flush_pending`)
                    // flips Cancelled's default semantics from "abandon the
                    // queue too" to "cancel the current turn but immediately
                    // start the next one with the queued follow-ups". One-
                    // shot — clear the flag whether or not the queue had
                    // anything left to send.
                    let flush_after_cancel = self
                        .sessions
                        .get(&session_id)
                        .map(|s| {
                            s.update(cx, |s, _| {
                                let was = s.flush_after_cancel;
                                s.flush_after_cancel = false;
                                was
                            })
                        })
                        .unwrap_or(false);
                    let cancelled =
                        matches!(reason, agent_client_protocol::schema::StopReason::Cancelled);
                    if cancelled && !flush_after_cancel {
                        // Silent-drop path: user pressed Stop, queue
                        // gets discarded without surfacing what was in
                        // it. Log the dropped bundles BEFORE the clear
                        // so post-mortem of "where did my queued
                        // message go?" can reconstruct it from the
                        // log line. WARN level (not INFO) — this is
                        // user-typed content vanishing without a
                        // trace, which is exactly the failure mode we
                        // want to be able to grep for.
                        let had_pending = if let Some(s) = self.sessions.get(&session_id).cloned() {
                            s.update(cx, |s, _| {
                                let dropped = s.pending_messages.len();
                                if dropped > 0 {
                                    let previews: Vec<String> = s
                                        .pending_messages
                                        .iter()
                                        .map(|bundle| {
                                            queue::summarize_blocks_for_log(&bundle.blocks)
                                        })
                                        .collect();
                                    log::warn!(
                                        target: "solution_agent::queue",
                                        "session={session_id} dropped {dropped} queued bundle(s) on Cancelled stop \
                                         (no flush_after_cancel) — content: [{}]",
                                        previews.join(" | "),
                                    );
                                }
                                s.pending_messages.clear();
                                dropped > 0
                            })
                        } else {
                            false
                        };
                        if had_pending {
                            self.mark_queue_changed(session_id, cx);
                        }
                    } else {
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
                                            crate::model::QueueTarget::Main => {
                                                main.extend(bundle.blocks)
                                            }
                                            crate::model::QueueTarget::Subagent(_) => {
                                                dropped.push(bundle)
                                            }
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
                            with_hint.push(acp::ContentBlock::Text(acp::TextContent::new(
                                format!("{}\n\n", queue::QUEUE_HINT_LINE),
                            )));
                            with_hint.extend(main_blocks);
                            self.send_message_blocks(session_id, with_hint, cx).detach();
                        }
                    }
                }
            }
            acp_thread::AcpThreadEvent::TokenUsageUpdated => {
                // claude-acp ships incremental usage during a turn, not
                // just at the end. Persist on every update so a session
                // closed mid-turn (or right before `Stopped` fires)
                // resumes with the correct meter — without this the DB
                // value lags behind the live meter and a resume drops
                // back to whatever the previous Stopped wrote.
                // Also mirror the new total onto `cached_total_tokens`
                // so the next cold-restore (or any read of the session
                // entity bypassing the live thread) sees the latest
                // figure without the meter regressing to zero.
                if let Some(s) = self.sessions.get(&session_id).cloned() {
                    let usage = s
                        .read(cx)
                        .acp_thread()
                        .and_then(|t| t.read(cx).token_usage().cloned());
                    let total = usage.as_ref().map(|u| u.used_tokens);
                    // `max_tokens == 0` is the "agent didn't fill it in"
                    // sentinel claude-acp ships under some beta paths.
                    // Treat that as None so MCP consumers can fall back
                    // to `DEFAULT_CONTEXT_WINDOW` instead of rendering
                    // "X / 0" on the meter.
                    let max = usage.as_ref().map(|u| u.max_tokens).filter(|m| *m > 0);
                    s.update(cx, |s, _| {
                        s.cached_total_tokens = total;
                        s.cached_max_tokens = max;
                    });
                    // The initialize response (carrying `models`) only lands after the
                    // first turn, so the first TokenUsageUpdated is the earliest capture.
                    let live_models = s.read(cx).acp_thread().and_then(|t| {
                        let t = t.read(cx);
                        t.connection()
                            .clone()
                            .downcast::<claude_native::ClaudeNativeConnection>()
                            .map(|c| c.available_models(t.session_id()))
                    });
                    if let Some(models) = live_models {
                        if !models.is_empty() {
                            let agent_id = s.read(cx).agent_id.clone();
                            self.model_catalog.set_models(agent_id, models.clone());
                            if s.read(cx).cached_models != models {
                                s.update(cx, |s, _| s.cached_models = models);
                                self.persist_session_row(session_id, cx);
                            }
                        }
                    }
                    // Throttled non-sequenced notification — at most one
                    // emit per 2 s per session. The client treats a
                    // missed metric notify as "check on next snapshot
                    // resync"; no gap-detection or seq field needed.
                    let (last_activity_at, total_tokens, max_tokens) = {
                        let r = s.read(cx);
                        (
                            r.last_activity_at,
                            r.cached_total_tokens,
                            r.cached_max_tokens,
                        )
                    };
                    self.metrics_emitter.emit_if_ready(
                        cx,
                        &session_id,
                        serde_json::json!({
                            "session_id": session_id.to_string(),
                            "last_activity_at": last_activity_at,
                            "total_tokens": total_tokens,
                            "max_tokens": max_tokens,
                        }),
                    );
                }
                self.persist_session_row(session_id, cx);
            }
            acp_thread::AcpThreadEvent::Error => {
                // Symmetric with the `Stopped` arm: flush any pending end-of-turn
                // entry-append throttle synchronously so the final entry's
                // `SessionMessageAppended` (+ `agent_session_dirty`) rides out on
                // the turn-error tick rather than depending on the 500 ms timer —
                // which, if `Running→Errored` doesn't change the state
                // discriminant (already Errored), would be the only remaining
                // dirty signal.
                self.flush_pending_entry_appends(session_id, cx);
                // A provider usage/session-limit wall can arrive as a fast `Error`
                // (not only as the silent stall the stuck-turn watchdog catches):
                // the worker's fast-error path lands here before the watchdog's
                // silence window elapses. The generic "agent error" string would
                // then bury the reset time. Classify the wall from the session's
                // own last assistant message and, for a SUPERVISED session, hand
                // off to quota recovery so the observer schedules an auto-resume at
                // the reset (mirroring the stuck-watchdog wall branch). For an
                // unsupervised session, at least surface the wall text so the user
                // sees when it resets. `apply_usage_limit_stop` is intentionally
                // NOT called in the unsupervised case: it would leave a diary
                // breadcrumb for a session that has no observer.
                match self.session_wall_message(session_id, cx) {
                    Some(wall) => {
                        let supervised = self
                            .supervisor_states
                            .get(&session_id)
                            .is_some_and(|s| s.enabled);
                        self.mutate_state(
                            session_id,
                            |state| {
                                *state = SessionState::Errored(SharedString::from(wall.clone()))
                            },
                            cx,
                        );
                        if supervised {
                            self.push_system_note(
                                session_id,
                                acp_thread::SystemNoteLevel::Error,
                                "Достигнут лимит claude — текущий ход остановлен.",
                                cx,
                            );
                            self.apply_usage_limit_stop(session_id, &wall, cx);
                        }
                    }
                    None => {
                        self.mutate_state(
                            session_id,
                            |state| {
                                *state = SessionState::Errored(SharedString::from("agent error"))
                            },
                            cx,
                        );
                    }
                }
            }
            acp_thread::AcpThreadEvent::LoadError(_) => {
                // A thread load/reconnect failure — distinct from a turn wall, so
                // no wall-classification here (a stale prior wall in the transcript
                // must not schedule a spurious resume). Flush + generic error,
                // same as before.
                self.flush_pending_entry_appends(session_id, cx);
                self.mutate_state(
                    session_id,
                    |state| *state = SessionState::Errored(SharedString::from("agent error")),
                    cx,
                );
            }
            acp_thread::AcpThreadEvent::ToolAuthorizationRequested(_) => {
                self.mutate_state(session_id, |state| *state = SessionState::AwaitingInput, cx);
            }
            acp_thread::AcpThreadEvent::ToolAuthorizationReceived(_) => {
                self.mutate_state(
                    session_id,
                    |state| {
                        if matches!(state, SessionState::AwaitingInput) {
                            *state = SessionState::Running {
                                started_at: std::time::Instant::now(),
                                notified: false,
                            };
                        }
                    },
                    cx,
                );
            }
            acp_thread::AcpThreadEvent::TitleUpdated => {
                let new_title = session_entity
                    .read(cx)
                    .acp_thread()
                    .and_then(|t| t.read(cx).title())
                    .unwrap_or_default();
                session_entity.update(cx, |s, _| s.title = new_title);
                cx.emit(SolutionAgentStoreEvent::SessionTitleChanged(session_id));
            }
            acp_thread::AcpThreadEvent::EntriesRemoved(range) => {
                // Truncate `session.entries` to match: a rewind removes all
                // entries from `range.start` onward (in global index space).
                // Cold entries are never removed by a live-thread rewind, so
                // the truncation point is `live_base + range.start`.
                let cold_count = session_entity.read(cx).live_base;
                let global_truncate = cold_count + range.start;
                session_entity.update(cx, |s, cx| {
                    s.entries.truncate(global_truncate);
                    // Rebuild the streams FIRST so `streams[Main]` reflects the
                    // truncated, re-coalesced transcript, THEN re-stamp on Main.
                    s.rebuild_streams();
                    // Decision #11 re-homed onto the Main stream (phase 6b): a
                    // truncate that splits a coalesced same-source assistant group
                    // leaves the Main-stream survivor's content changed (a fragment
                    // removed) but its first-fragment mod_seq unchanged — possibly
                    // BELOW a delta client's cursor or `persisted_main_seq` — while
                    // the stream's `total_count` is unchanged (the removed fragment
                    // was coalesced INTO the survivor, not a separate stream entry).
                    // Since the per-stream wire delta AND `persist_main_stream` now
                    // both key on the Main stream (`entry.mod_seq > watermark`), the
                    // re-stamp must land on the Main stream's boundary entry, not
                    // the flat one. Bump the surviving Main entry's mod_seq to a
                    // fresh change_seq and lift `streams[Main].seq` to it so the
                    // next delta re-delivers the now-shorter entry and
                    // `persist_main_stream` re-upserts its row.
                    let seq = s.bump_change_seq();
                    if let Some(main) = s.streams.get_mut(&crate::stream::StreamId::Main)
                        && let Some(last) = main.entries.last_mut()
                    {
                        last.mod_seq = seq;
                        main.seq = seq;
                    }
                    cx.notify();
                });
                // Persist authority is `streams[Main]` (phase 6b): a rewind drops
                // the removed rows and shrinks the coalesce survivor.
                // `persist_main_stream` trims via `delete_entries_from(main_len)`
                // AND re-upserts the re-stamped survivor (its mod_seq now exceeds
                // `persisted_main_seq`), keeping the persisted transcript in lockstep
                // so a stale idx>=len row can't corrupt the next cold load.
                self.persist_main_stream(session_id, cx);
                // The user-facing `/clear` does NOT reach this branch:
                // it's intercepted client-side and routed through
                // `reset_context` (which spawns a brand-new `AcpThread`
                // and never emits `EntriesRemoved`); the corresponding
                // token-meter reset lives at the swap site in
                // `reset_context` / `rotate_context`.
                //
                // What this branch covers is a thread-local truncation
                // that happens to remove every entry — today the only
                // in-tree producer is `acp_thread::rewind` /
                // refusal-truncate (`acp_thread.rs:2369`, `:2491`)
                // when rewinding to before the very first user message.
                // The post-event `entries().is_empty()` check
                // discriminates this "rewind to zero" case from a
                // partial rewind: the latter leaves a surviving
                // prefix whose token usage is still meaningful, and
                // the agent will emit a fresh `TokenUsageUpdated`
                // against that prefix on the next turn — so we MUST
                // NOT preemptively wipe state in the partial case.
                let thread = session_entity.read(cx).acp_thread().cloned();
                let cleared = thread
                    .as_ref()
                    .map(|t| t.read(cx).entries().is_empty())
                    .unwrap_or(false);
                if cleared {
                    if let Some(t) = thread {
                        t.update(cx, |t, cx| t.update_token_usage(None, cx));
                    }
                    session_entity.update(cx, |s, _| {
                        s.cached_total_tokens = None;
                        s.last_turn_duration = None;
                    });
                    self.persist_session_row(session_id, cx);
                }
            }
            acp_thread::AcpThreadEvent::EntryUpdated(idx) => {
                // A streaming update on a non-system entry (assistant-text
                // chunk, tool-status transition) is proof the agent is live, so
                // clear a latched `Errored` — the visible "Error while
                // streaming" symptom is EntryUpdated-driven (bug #5). Mirror the
                // NewEntry arm's SystemNote guard so an injected note can't flip
                // state.
                let updated_is_system_note = self
                    .sessions
                    .get(&session_id)
                    .and_then(|s| s.read(cx).acp_thread().cloned())
                    .map(|t| {
                        matches!(
                            t.read(cx).entries().get(*idx),
                            Some(acp_thread::AgentThreadEntry::SystemNote(_))
                        )
                    })
                    .unwrap_or(false);
                if !updated_is_system_note {
                    self.mutate_state(
                        session_id,
                        |state| {
                            state.clear_error_on_activity();
                        },
                        cx,
                    );
                    // A streaming chunk or a tool-status transition is agent
                    // activity too, so it must reset the silence clock the
                    // stuck-session watchdog reads — exactly like `NewEntry`
                    // does above. Without this, a long silent FOREGROUND command
                    // (one `NewEntry` at tool start, then minutes blocked while
                    // streaming nothing, then a terminal-status `EntryUpdated`
                    // when it finishes) leaves `last_activity_at` frozen at
                    // tool-start: the instant the tool leaves `InProgress` the
                    // watchdog's `TOOL_STUCK_SECS` shield drops while `silent_secs`
                    // is already >= `STUCK_TURN_SECS`, so a perfectly-alive agent
                    // is falsely declared wedged and reconnected the moment its
                    // command completes.
                    if let Some(s) = self.sessions.get(&session_id).cloned() {
                        s.update(cx, |s, _| s.last_activity_at = Utc::now());
                    }
                }
                // Subagent-tab lifecycle: a tracked Task/Agent ToolCall that
                // just flipped to a terminal status is a finish signal. We
                // run this BEFORE the EntryUpdated throttle plumbing so the
                // `SessionSubagentsChanged` emit happens on the same tick
                // the parent thread's `EntryUpdated` is observed, without
                // waiting for the 500 ms debounce that gates
                // `SessionMessageAppended`.
                self.apply_subagent_lifecycle(session_id, *idx, cx);
                // A `<task-notification>` can also surface via an in-place
                // EntryUpdated (a user message whose text streams in); scan it
                // here too. `observe_task_notification` is idempotent — the
                // `mark_background_shell_state` no-op guard rejects a re-observed
                // terminal state, so a NewEntry + EntryUpdated pair on the same
                // notification flips the shell exactly once.
                self.observe_task_notification(session_id, *idx, cx);
                // Tool-call arg deltas, assistant-text chunks, and tool-
                // status transitions on an existing entry all surface
                // here. The pre-fix behaviour fell through to the
                // `_ => {}` catch-all, so external MCP consumers (the
                // Android client) never learned the entry changed and
                // displayed only the initial empty `args_preview = "{}"`
                // for a tool call or the first preview snapshot of a
                // streaming assistant reply.
                //
                // Coalesced via a trailing-edge debounce: a 500 ms quiet
                // window collapses a token-by-token streaming burst
                // into roughly 2 emits/sec, and a 2 s max-stale guard
                // forces an emit when an entry is continuously dirty so
                // the consumer doesn't starve. Replacing an entry in
                // `entry_update_throttles` drops the previous `Task`,
                // which cancels its inflight timer → only the latest
                // debounce window's task survives to fire.
                let key = (session_id, *idx);
                let now = std::time::Instant::now();
                let existing_first_dirty_at = self
                    .entry_update_throttles
                    .get(&key)
                    .map(|t| t.first_dirty_at);
                let max_stale_breached = existing_first_dirty_at
                    .map(|t| {
                        now.saturating_duration_since(t) >= std::time::Duration::from_millis(2000)
                    })
                    .unwrap_or(false);
                if max_stale_breached {
                    self.entry_update_throttles.remove(&key);
                    cx.emit(SolutionAgentStoreEvent::SessionMessageAppended(
                        session_id, *idx,
                    ));
                } else {
                    let first_dirty_at = existing_first_dirty_at.unwrap_or(now);
                    let entry_index = *idx;
                    let task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(500))
                            .await;
                        this.update(cx, |this, cx| {
                            if this.entry_update_throttles.remove(&key).is_some() {
                                cx.emit(SolutionAgentStoreEvent::SessionMessageAppended(
                                    session_id,
                                    entry_index,
                                ));
                            }
                        })
                        .ok();
                    });
                    self.entry_update_throttles.insert(
                        key,
                        EntryUpdateThrottle {
                            first_dirty_at,
                            _task: task,
                        },
                    );
                }
                // Incremental EntryUpdated: reconvert only the changed entry and
                // replace it in `session.entries`, preserving its `created_ms`
                // (no restamp — the creation time is fixed at first append).
                let cold_count = session_entity.read(cx).live_base;
                let global_idx = cold_count + *idx;
                let updated_entry = {
                    let s = session_entity.read(cx);
                    let live = s.acp_thread().map(|t| t.read(cx).entries()).unwrap_or(&[]);
                    live.get(*idx).map(|live_entry| {
                        let mut entry = crate::session_entry::to_session_entry(live_entry, cx);
                        // Preserve the creation time stamped at first append.
                        entry.created_ms = s
                            .entries
                            .get(global_idx)
                            .map(|e| e.created_ms)
                            .unwrap_or(crate::model::NO_TIMESTAMP_MS);
                        entry
                    })
                };
                if let Some(entry) = updated_entry {
                    session_entity.update(cx, |s, cx| {
                        let seq = s.bump_change_seq();
                        if let Some(slot) = s.entries.get_mut(global_idx) {
                            *slot = entry;
                            slot.mod_seq = seq;
                        }
                        s.rebuild_streams();
                        cx.notify();
                    });
                    // Row upsert happens unconditionally on the in-memory update;
                    // the 500ms/2s throttle above governs only the MCP
                    // `SessionMessageAppended` emit, NOT this persist. Persist
                    // authority is `streams[Main]` (phase 6b): flush the Main
                    // stream incrementally after the rebuild so the coalesced
                    // Main-local row lands (the edited flat entry may map to a
                    // coalesced Main entry at a different index).
                    self.persist_main_stream(session_id, cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }
}
