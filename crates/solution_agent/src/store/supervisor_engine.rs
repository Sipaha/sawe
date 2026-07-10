//! Observer/supervisor engine glue: the Store-side methods that spawn
//! ephemeral judge/auditor sessions, apply their verdicts, drive the
//! per-tick supervisor + stuck-session watchdogs, and manage supervision
//! state/nudges. Relocated verbatim from `store.rs` (Tier-3 god-object
//! refactor) — these are `impl SolutionAgentStore` methods that still own
//! `&mut SolutionAgentStore` / `Context<Self>`; this split moves *source
//! text*, not state ownership. The pure decision logic they call lives in
//! `crate::supervisor`.
//!
//! The #5–#9 watchdog hardening (usage-wall Error-vs-Stopped + judge
//! liveness, stuck-tool liveness gate, parent-liveness shell reap, verdict
//! nonce auth + idempotency, Compact double-lease defer) lives entirely in
//! these methods and is preserved byte-for-byte.

use super::*;

/// An in-flight ephemeral judge/auditor session. `judge_id` is `None` until
/// the async create resolves; `_task` drives create → send-briefing and is
/// dropped (cancelling the spawn) when the handle is removed by `finish_judge`.
pub(crate) struct JudgeHandle {
    pub(crate) judge_id: Option<SolutionSessionId>,
    /// Wall-clock spawn time (`chrono::Utc::now().timestamp_millis()`), set at
    /// insert in `spawn_ephemeral_supervisor_session`. Drives the auditor-stuck
    /// sweep in `tick_supervisor`: an auditor that errors/ends WITHOUT calling
    /// `supervisor_audit_verdict` spawns while the supervised session is
    /// `Watching` (not `Judging`), so the judge-stuck timeout never catches it;
    /// without this timestamp the handle would leak forever and meta-audit would
    /// be permanently disabled for that session.
    pub(crate) started_ms: i64,
    /// Single-use credential minted at spawn ([`crate::supervisor::new_verdict_nonce`])
    /// and baked into this session's briefing. The `supervisor_verdict` /
    /// `supervisor_audit_verdict` call must echo it or the store rejects the
    /// verdict as unauthorized — a client on the per-solution socket that never
    /// saw the briefing can't forge one. Removing the handle (`finish_judge` /
    /// `finish_auditor`) invalidates the nonce, which also makes a duplicate
    /// re-submit (bridge-EOF retry) a no-op instead of a second `apply_verdict`.
    pub(crate) nonce: String,
    pub(crate) _task: Task<()>,
}

/// Outcome of an authenticated verdict submission from the MCP boundary
/// (`apply_verdict_authenticated` / `apply_audit_verdict_authenticated`).
pub(crate) enum VerdictAuth {
    /// Nonce matched the in-flight judge/auditor; the verdict was applied.
    Applied,
    /// No in-flight judge/auditor for this session, so nothing was applied.
    /// This is the idempotent case: the first (successful OR gate-dropped)
    /// verdict already reaped the handle, so a bridge-EOF retry lands here and
    /// is a no-op. Reported to the caller as SUCCESS so a retrying judge stops
    /// re-submitting. A stray verdict for a session that never had a judge also
    /// lands here — harmless, since nothing is mutated.
    NoInFlight,
    /// A judge/auditor IS in flight but the supplied nonce is wrong — a forged
    /// or stale verdict. Rejected without touching any state (the real judge can
    /// still submit with the correct nonce).
    Unauthorized,
}

impl SolutionAgentStore {
    pub(crate) fn persist_supervisor_state(&self, id: SolutionSessionId, cx: &mut Context<Self>) {
        let (Some(db), Some(state)) = (self.persistence.clone(), self.supervisor_states.get(&id))
        else {
            return;
        };
        let state = state.clone();
        cx.background_spawn(async move {
            db.save_supervisor_state(state).await.log_err();
        })
        .detach();
    }

    /// Resolve the root directory of the solution that owns `id`, via the
    /// `SolutionStore` global (the compact.rs pattern). Returns `None` when the
    /// session is unknown, the `SolutionStore` global is absent (headless /
    /// test), or the solution isn't registered.
    pub(super) fn solution_root_for(&self, id: SolutionSessionId, cx: &App) -> Option<PathBuf> {
        let session = self.session(id)?;
        let solution_id = session.read(cx).solution_id.clone();
        SolutionStore::try_global(cx).and_then(|store| {
            store.read_with(cx, |s, _| {
                s.solutions()
                    .iter()
                    .find(|sol| sol.id == solution_id)
                    .map(|sol| sol.root.clone())
            })
        })
    }

    /// Public read-only variant of [`solution_root_for`] for use from rendering
    /// code (e.g. the status-row supervisor popover) that only has `&App`.
    pub fn solution_root_for_app(&self, id: SolutionSessionId, cx: &App) -> Option<PathBuf> {
        self.solution_root_for(id, cx)
    }

    /// Scan the CURRENT turn of `id`'s transcript for a provider usage/session-
    /// limit wall: the text of the latest assistant message, but only when that
    /// text matches [`is_usage_limit_error`]. Shared by the judge-wall probe and
    /// the fast-`Error` arm so both classify a wall by the exact same rule (the
    /// stuck-turn watchdog inlines the same scan because it runs mid-iteration
    /// over `self.sessions`, where a second `&self` borrow would be awkward).
    ///
    /// The reverse scan STOPS at the user message that opened the current turn:
    /// a bare `AcpThreadEvent::Error` carries no payload and fires for any failed
    /// turn (dead subprocess, network drop on resume, MaxTokens), so without this
    /// anchor a STALE prior-turn wall — still the transcript's last assistant
    /// message — would reclassify a later transient error as a fresh wall and
    /// park supervision on a bogus ~24h resume (or a permanent `Stopped(Quota)`
    /// for a no-reset phrasing). Only an assistant message NEWER than the last
    /// user message — i.e. produced by the turn that just errored — may classify.
    pub(super) fn session_wall_message(&self, id: SolutionSessionId, cx: &App) -> Option<String> {
        let session = self.session(id)?;
        let thread = session.read(cx).acp_thread()?.clone();
        let text = thread
            .read(cx)
            .entries()
            .iter()
            .rev()
            .find_map(|e| match e {
                acp_thread::AgentThreadEntry::AssistantMessage(m) => Some(Some(
                    m.chunks
                        .iter()
                        .map(|chunk| match chunk {
                            acp_thread::AssistantMessageChunk::Message { block }
                            | acp_thread::AssistantMessageChunk::Thought { block } => {
                                block.to_markdown(cx)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                )),
                // Turn boundary — an error before this turn streamed any
                // assistant chunk must not inherit a previous turn's wall.
                acp_thread::AgentThreadEntry::UserMessage(_) => Some(None),
                _ => None,
            })??;
        crate::supervisor::is_usage_limit_error(&text).then_some(text)
    }

    /// If session `id`'s in-flight JUDGE has itself stalled on a usage/session
    /// limit wall — its last assistant message IS the wall text — return that
    /// text. The judge is a claude session on the same account and hits the same
    /// wall in the same "print the wall, then stall without ending the turn"
    /// shape as the worker (FORK #34); it's exempt from `tick_stuck_sessions`, so
    /// the ONLY thing that notices is the judge timeout — which otherwise routes
    /// the generic timeout string to a *transient* failure that spirals to a
    /// false `Stopped(ProviderError)` instead of scheduling the reset-time resume
    /// (finding #3). Returning the wall text lets the timeout path route to quota
    /// recovery. Detection is anchored (`is_usage_limit_error`, finding #4) so the
    /// judge's own rate-limit-code *analysis* doesn't false-match.
    pub(super) fn judge_wall_message(&self, id: SolutionSessionId, cx: &App) -> Option<String> {
        let judge_id = self.judge_sessions.get(&id)?.judge_id?;
        self.session_wall_message(judge_id, cx)
    }

    /// Cumulative tokens the ephemeral judge/auditor session `judge_id` reported
    /// (live `TokenUsage.used_tokens`, else the cached mirror). Read at
    /// verdict/audit time — while the ephemeral session is still alive, before
    /// `finish_judge`/`finish_auditor` reaps it — so a `VerdictRecord` can record
    /// what the supervisor's own review turn cost (the verdict tool itself has no
    /// token figure to pass). `None` when the session is gone or has no usage yet.
    pub(super) fn ephemeral_session_tokens(&self, judge_id: Option<SolutionSessionId>, cx: &App) -> Option<u64> {
        let session = self.session(judge_id?)?;
        let session = session.read(cx);
        session
            .acp_thread()
            .and_then(|t| t.read(cx).token_usage().map(|u| u.used_tokens))
            .or(session.cached_total_tokens)
    }

    /// Best-effort append of a timestamped line to the session's supervisor
    /// `diary.md`. Used to leave a human-readable breadcrumb for failure /
    /// backoff events (the structured record stays in `verdicts.jsonl`). Silent
    /// no-op when the solution root can't be resolved (headless / test without a
    /// registered solution); write errors are logged, never propagated.
    pub(crate) fn append_supervisor_diary_note(
        &mut self,
        id: SolutionSessionId,
        note: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.solution_root_for(id, cx) else {
            return;
        };
        let dir = crate::supervisor::supervisor_dir(&root, id);
        let diary_path = crate::supervisor::diary_path(&dir);
        let line = format!("- {} {note}\n", chrono::Utc::now().to_rfc3339());
        (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&diary_path)?;
            std::io::Write::write_all(&mut file, line.as_bytes())
        })()
        .log_err();
        crate::supervisor::cap_log_tail(&diary_path, crate::supervisor::DIARY_LOG_MAX_BYTES);
    }

    /// Spawn an ephemeral judge for the supervised session `id`. Creates a
    /// throwaway session in the SAME solution+agent (cwd = solution root so the
    /// judge's file tools can read project files, `.agents` handoffs, and the
    /// diary), then delivers [`build_judge_briefing`] as its single user turn.
    /// The verdict tool calls [`finish_judge`] to tear the judge down
    /// once a verdict is recorded.
    ///
    /// The judge needs an `Entity<project::Project>`; the 1 Hz tick has no
    /// Window/Workspace, so we reuse the supervised session's CACHED project
    /// (the same source `restart_agent` uses). Prebuilt/cold sessions have no
    /// cached project — the judge is skipped for those (expected: this path is
    /// verified live, not in unit tests where the seed session is project-less).
    pub(crate) fn spawn_judge(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) {
        self.spawn_ephemeral_supervisor_session(id, false, cx);
    }

    /// Shared body for [`spawn_judge`] (`audit = false`) and [`spawn_auditor`]
    /// (`audit = true`). Both spawn a throwaway session in the SAME
    /// solution+agent (cwd = solution root so its file tools can read project
    /// files, `.agents` handoffs, the diary, and the verdict log), then deliver
    /// [`build_judge_briefing`] as the single user turn. The only differences
    /// are the briefing's `audit` flag and which in-flight map records the
    /// handle (`judge_sessions` vs `auditor_sessions`). Behaviour is otherwise
    /// identical to the judge spawn — including project
    /// resolution, the hidden parent-linked create, and the failure path.
    ///
    /// The session needs an `Entity<project::Project>`; the 1 Hz tick has no
    /// Window/Workspace, so we reuse the supervised session's CACHED project
    /// (the same source `restart_agent` uses). Prebuilt/cold sessions have no
    /// cached project — the spawn is skipped for those (expected: this path is
    /// verified live, not in unit tests where the seed session is project-less).
    fn spawn_ephemeral_supervisor_session(
        &mut self,
        id: SolutionSessionId,
        audit: bool,
        cx: &mut Context<Self>,
    ) {
        let kind = if audit {
            "spawn_auditor"
        } else {
            "spawn_judge"
        };
        // One judge AND one auditor at most per supervised session at a time;
        // each kind guards only its own map so a judge doesn't block an auditor.
        let already_running = if audit {
            self.auditor_sessions.contains_key(&id)
        } else {
            self.judge_sessions.contains_key(&id)
        };
        if already_running {
            return;
        }
        let Some(session) = self.session(id) else {
            return;
        };
        let (solution_id, agent_id, project, context_usage) = {
            let s = session.read(cx);
            let project = s
                .project
                .clone()
                .or_else(|| s.acp_thread().map(|t| t.read(cx).project().clone()));
            // Context-window fullness of the SUPERVISED session, so the judge
            // can weigh a `compact` verdict. Auditors review the supervisor's
            // own work, not the agent's context, so they get `None`.
            let context_usage = if audit {
                None
            } else {
                let used = s
                    .acp_thread()
                    .and_then(|t| t.read(cx).token_usage().map(|u| u.used_tokens))
                    .or(s.cached_total_tokens);
                let max = s.cached_max_tokens;
                match (used, max) {
                    (Some(used), Some(max)) if max > 0 => {
                        let pct = ((used as f64 / max as f64) * 100.0).round() as u64;
                        Some(format!("{used} / {max} tokens ({pct}%)"))
                    }
                    (Some(used), _) => {
                        Some(format!("{used} tokens used (context window size unknown)"))
                    }
                    _ => None,
                }
            };
            (
                s.solution_id.clone(),
                s.agent_id.clone(),
                project,
                context_usage,
            )
        };
        let Some(project) = project else {
            log::debug!("{kind}({id}): session has no cached project (prebuilt/cold); skipped");
            return;
        };
        let Some(solution_root) = self.solution_root_for(id, cx) else {
            log::warn!(
                "{kind}({id}): solution {:?} not registered; disabling supervision",
                solution_id.0
            );
            self.set_supervision_enabled(id, false, cx);
            return;
        };

        let dir = crate::supervisor::supervisor_dir(&solution_root, id);
        let custom_prompt = self
            .supervisor_states
            .get(&id)
            .and_then(|s| s.custom_prompt.clone());
        // The judge talks to the editor over the `--nc` socket bridge from
        // Bash (claude does NOT reliably register the editor's
        // `solution_agent.*` MCP tools — see supervisor instructions). It submits
        // its verdict via `supervisor_verdict` / `supervisor_audit_verdict`, which
        // are SOLUTION-SCOPED tools served ONLY on the per-solution socket — the
        // editor-global socket does not carry them. So resolve the per-solution
        // socket EXACTLY (no global fallback): briefing a judge with the global
        // socket would leave it unable to ever submit → JUDGE_TIMEOUT → a bogus
        // transient-failure backoff that, repeated, spirals to a false
        // `Stopped(ProviderError)` that silently kills supervision. If the socket
        // isn't up yet (startup race, solution socket not opened), skip this spawn
        // and stay `Watching` so the next tick retries once it's available.
        let bridge_bin = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "sawe".to_string());
        let Some(socket_path) = editor_mcp::solution_socket_for_path(cx, &solution_root) else {
            log::warn!(
                "{kind}({id}): per-solution MCP socket not resolvable; skipping spawn (will retry)"
            );
            self.append_supervisor_diary_note(
                id,
                "judge/auditor spawn skipped: per-solution MCP socket not up \
                 (verdict tool unreachable on the global socket); staying Watching, will retry",
                cx,
            );
            // The caller flipped a JUDGE to `Judging` before calling us; revert so
            // the judge-stuck watchdog doesn't later mistake a never-spawned judge
            // for a crashed one and charge a bogus backoff. (An AUDITOR is spawned
            // while `Watching`, so this guard is a no-op for it.) Gate the next
            // fire ~15s out instead of leaving immediate re-eligibility: the fire
            // path cleared `next_eligible_ms`, so without this the 1 Hz tick would
            // re-fire → re-skip every second for the whole outage, appending a
            // diary line + two DB writes per second and rotating the judge's real
            // diary memory out under `DIARY_LOG_MAX_BYTES`. The gate also preserves
            // the `next_eligible_ms`-is-some bypass of the inherited-idle guard so
            // a restart-scheduled resume racing the socket isn't silently dropped.
            if let Some(state) = self.supervisor_states.get_mut(&id)
                && matches!(state.status, crate::supervisor::SupervisorStatus::Judging)
            {
                state.status = crate::supervisor::SupervisorStatus::Watching;
                state.next_eligible_ms =
                    Some(chrono::Utc::now().timestamp_millis() + JUDGE_SPAWN_RETRY_MS);
                self.persist_supervisor_state(id, cx);
                cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
            }
            return;
        };
        let socket_path = socket_path.to_string_lossy().into_owned();
        // One-time credential for this briefing: the judge/auditor echoes it in
        // its verdict call and the store checks it against the handle's nonce
        // (see `apply_verdict_authenticated`). Minted before the briefing so it
        // rides into the template, and stored on the handle below.
        let nonce = crate::supervisor::new_verdict_nonce();
        let briefing =
            crate::supervisor::build_judge_briefing(&crate::supervisor::JudgeBriefingContext {
                supervised_session_id: id.to_string(),
                diary_path: crate::supervisor::diary_path(&dir)
                    .to_string_lossy()
                    .into_owned(),
                verdicts_path: crate::supervisor::verdicts_path(&dir)
                    .to_string_lossy()
                    .into_owned(),
                intent_path: crate::supervisor::intent_path(&dir)
                    .to_string_lossy()
                    .into_owned(),
                compact_dir: solution_root
                    .join(".agents")
                    .join(id.to_string())
                    .to_string_lossy()
                    .into_owned(),
                custom_prompt,
                context_usage,
                audit,
                bridge_bin,
                socket_path,
                nonce: nonce.clone(),
            });

        // Spawn the judge/auditor as a CHILD of the supervised session
        // (`parent_session_id = Some(id)`) instead of a top-level session.
        // `create_session_with_parent` skips `open_session_in_strip` for any
        // parent-linked session (no `tab_order`, no `TabsChanged { opened }`),
        // so the ephemeral session never flickers a visible tab in the
        // ConsolePanel strip on each idle wake-up. It does NOT produce a teammate
        // stream on the parent (teammate streams are driven only by Task/Agent
        // ToolCall events on the parent's ACP thread, never by this create), so
        // it also stays out of the subagent strip and the
        // `agent_session_active_subagents_changed` wire surface. The verdict /
        // audit-verdict tool closes it by its stored `judge_id`. The
        // `ephemeral_supervisor = true` arg stamps `is_supervisor_ephemeral` on
        // the new session entity so every enumeration surface (Sparkle badge,
        // subagent strip, `list_sessions`, `get_session_children`, the desktop
        // "AI: N" counter) filters it out, and so the create/close wire
        // notifications are suppressed at the store-event source.
        let create = self.create_session_with_parent(
            solution_id,
            agent_id,
            project,
            Some(solution_root),
            Some(id),
            None,
            None,
            true,
            // Not needed: the `parent_session_id = Some(id)` above already
            // skips the strip pin, and `ephemeral_supervisor = true` already
            // suppresses the `SessionCreated` emit.
            false,
            cx,
        );
        let task = cx.spawn(async move |this, cx| {
            let ephemeral_id = match create.await {
                Ok(ephemeral_id) => ephemeral_id,
                Err(err) => {
                    log::warn!("{kind}({id}): create failed: {err}");
                    this.update(cx, |this, cx| this.on_judge_failed(id, err.to_string(), cx))
                        .ok();
                    return;
                }
            };
            this.update(cx, |this, _| {
                let map = if audit {
                    &mut this.auditor_sessions
                } else {
                    &mut this.judge_sessions
                };
                if let Some(handle) = map.get_mut(&id) {
                    handle.judge_id = Some(ephemeral_id);
                }
            })
            .ok();
            // Deliver the briefing as the session's single user turn.
            let send = this.update(cx, |this, cx| this.send_message(ephemeral_id, briefing, cx));
            if let Ok(send) = send {
                if let Err(err) = send.await {
                    log::warn!("{kind}({id}): briefing send failed: {err}");
                    this.update(cx, |this, cx| this.on_judge_failed(id, err.to_string(), cx))
                        .ok();
                }
            }
            // On success the session runs until it calls its verdict tool
            // (`supervisor_verdict` for the judge, `supervisor_audit_verdict`
            // for the auditor), which tears it down. The ends-without-verdict
            // path (transient failure) is handled by the judge-stuck watchdog
            // in `tick_supervisor`, which applies backoff/retry on timeout.
        });
        let handle = JudgeHandle {
            judge_id: None,
            started_ms: chrono::Utc::now().timestamp_millis(),
            nonce,
            _task: task,
        };
        if audit {
            self.auditor_sessions.insert(id, handle);
        } else {
            self.judge_sessions.insert(id, handle);
        }
    }

    /// Tear down the ephemeral judge for supervised session `id` and clear its
    /// handle. Called by the verdict tool once a verdict is recorded+executed.
    /// Dropping the `JudgeHandle` also cancels the driving task if it's still
    /// in flight.
    pub(crate) fn finish_judge(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) {
        if let Some(handle) = self.judge_sessions.remove(&id) {
            if let Some(judge_id) = handle.judge_id {
                self.close_session(judge_id, cx).log_err();
            }
        }
    }

    /// Spawn the ephemeral meta-auditor for supervised session `id`. The
    /// auditor reviews the SUPERVISOR's own work (`verdicts.jsonl` + `diary.md`)
    /// rather than the agent dialogue, deciding whether the supervisor is making
    /// real progress or is looping. Triggered every 5 consecutive `continue`
    /// verdicts by [`apply_verdict`]. Shares its body with [`spawn_judge`] via
    /// [`spawn_ephemeral_supervisor_session`] (`audit = true`).
    pub(crate) fn spawn_auditor(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) {
        self.spawn_ephemeral_supervisor_session(id, true, cx);
    }

    /// Tear down the ephemeral auditor for supervised session `id` and clear its
    /// handle. Called by the audit-verdict tool once the auditor records a
    /// verdict. Dropping the `JudgeHandle` also cancels the driving task if it's
    /// still in flight.
    pub(crate) fn finish_auditor(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) {
        if let Some(handle) = self.auditor_sessions.remove(&id) {
            if let Some(auditor_id) = handle.judge_id {
                self.close_session(auditor_id, cx).log_err();
            }
        }
    }

    /// Record the meta-auditor's verdict for supervised session `id`, tear down
    /// the auditor session, and act on the result. Appends an `Audit`-kind
    /// `VerdictRecord` (carrying `audit_ok`) to `verdicts.jsonl`; when the audit
    /// fails (`!ok`) or explicitly escalates, pauses supervision in `WaitingUser`
    /// and routes the reasoning to the human via [`escalate_to_user`]. When the
    /// audit passes (`ok && !escalate`) supervision is left untouched — the
    /// 5-continue cadence already nudged the agent forward in `apply_verdict`.
    pub(crate) fn apply_audit_verdict(
        &mut self,
        id: SolutionSessionId,
        ok: bool,
        escalate: bool,
        reasoning: String,
        cx: &mut Context<Self>,
    ) {
        use crate::supervisor::{VerdictKind, VerdictRecord};

        // Send-time gate (mirrors `apply_verdict`, FORK #31): the auditor spawned
        // while the session was `Watching` and ran for minutes; if it has since
        // been manually stopped (`Held`), disabled, or walled (`Stopped`), a late
        // `escalate` must NOT force `WaitingUser` — that overrides the manual-stop
        // rule (a `WaitingUser` is re-armed by self-activity — FORK #44) and
        // toasts a session the user switched off. Computed BEFORE the append so the
        // record can carry `dropped` — a gated escalation is still logged for the
        // audit trail, just marked as not-acted so it isn't miscounted.
        let supervising = self.supervisor_states.get(&id).is_some_and(|s| {
            s.enabled
                && !matches!(
                    s.status,
                    crate::supervisor::SupervisorStatus::Held
                        | crate::supervisor::SupervisorStatus::Stopped(_)
                )
        });
        // This audit wanted an action (escalate, or a failed audit) but the gate
        // suppressed it. A clean pass (`ok && !escalate`) is NOT dropped — the
        // no-op IS its outcome.
        let audit_dropped = (escalate || !ok) && !supervising;

        if let Some(root) = self.solution_root_for(id, cx) {
            let dir = crate::supervisor::supervisor_dir(&root, id);
            let rec = VerdictRecord {
                ts_ms: chrono::Utc::now().timestamp_millis(),
                kind: VerdictKind::Audit,
                action: None,
                audit_ok: Some(ok),
                reasoning: reasoning.clone(),
                message: None,
                question: None,
                // Same as the verdict path: fill from the live auditor session's
                // usage (read before `finish_auditor` reaps it) so the audit's
                // cost is recorded rather than always `None`.
                tokens: self.ephemeral_session_tokens(
                    self.auditor_sessions.get(&id).and_then(|h| h.judge_id),
                    cx,
                ),
                dropped: audit_dropped,
            };
            crate::supervisor::append_verdict(&dir, &rec).log_err();
        }

        self.finish_auditor(id, cx);

        if (escalate || !ok) && supervising {
            self.escalate_to_user(id, format!("Supervisor meta-audit: {reasoning}"), cx);
        }

        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    /// Authenticated entry point for the `supervisor_verdict` MCP tool. Verifies
    /// the caller-supplied `nonce` against the in-flight judge's stored nonce
    /// before applying the verdict, so an arbitrary client on the per-solution
    /// socket can't forge one. The nonce check doubles as an idempotency guard:
    /// applying the first verdict reaps the judge handle (`finish_judge`), so a
    /// duplicate re-submit (the bridge exits on stdin EOF and the judge is told
    /// to retry on an empty reply) finds no in-flight judge and is a no-op.
    /// Internal/test callers use [`apply_verdict`](Self::apply_verdict) directly
    /// (trusted, un-authenticated).
    pub(crate) fn apply_verdict_authenticated(
        &mut self,
        id: SolutionSessionId,
        nonce: &str,
        action: crate::supervisor::VerdictAction,
        reasoning: String,
        message: Option<String>,
        question: Option<String>,
        wait_seconds: Option<u64>,
        cx: &mut Context<Self>,
    ) -> VerdictAuth {
        // Resolve the nonce match into a bool BEFORE the `&mut self` call so the
        // `&self.judge_sessions` borrow ends first.
        let matched = self
            .judge_sessions
            .get(&id)
            .map(|handle| crate::supervisor::verdict_nonce_matches(&handle.nonce, nonce));
        match matched {
            Some(true) => {
                self.apply_verdict(id, action, reasoning, message, question, None, wait_seconds, cx);
                VerdictAuth::Applied
            }
            Some(false) => VerdictAuth::Unauthorized,
            None => VerdictAuth::NoInFlight,
        }
    }

    /// Authenticated entry point for the `supervisor_audit_verdict` MCP tool.
    /// Same nonce + in-flight-handle gate as [`apply_verdict_authenticated`],
    /// keyed on `auditor_sessions`.
    pub(crate) fn apply_audit_verdict_authenticated(
        &mut self,
        id: SolutionSessionId,
        nonce: &str,
        ok: bool,
        escalate: bool,
        reasoning: String,
        cx: &mut Context<Self>,
    ) -> VerdictAuth {
        let matched = self
            .auditor_sessions
            .get(&id)
            .map(|handle| crate::supervisor::verdict_nonce_matches(&handle.nonce, nonce));
        match matched {
            Some(true) => {
                self.apply_audit_verdict(id, ok, escalate, reasoning, cx);
                VerdictAuth::Applied
            }
            Some(false) => VerdictAuth::Unauthorized,
            None => VerdictAuth::NoInFlight,
        }
    }

    /// Record the judge's verdict, tear down the judge session, and execute the
    /// verdict action. Orchestrates the three-step response to every judge turn:
    ///
    /// 1. Append a `VerdictRecord` to `verdicts.jsonl` (best-effort; never
    ///    blocks the action even when the write fails).
    /// 2. Call `finish_judge` to close the ephemeral judge session.
    /// 3. Dispatch the action: `Continue` nudges the supervised session and
    ///    increments the guard counter; `Compact` queues a compact prompt;
    ///    `Done` stops supervision; `Ask` escalates to the user.
    ///
    /// `Continue` runs through `continue_guard`: it nudges, or every 5th
    /// consecutive continue also spawns a meta-auditor, or at the 15-cap
    /// forces an `Ask` escalation instead of a 16th nudge.
    pub(crate) fn apply_verdict(
        &mut self,
        id: SolutionSessionId,
        action: crate::supervisor::VerdictAction,
        reasoning: String,
        message: Option<String>,
        question: Option<String>,
        tokens: Option<u64>,
        wait_seconds: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        use crate::supervisor::{SupervisorStatus, VerdictAction, VerdictKind, VerdictRecord};

        // Send-time state re-check (audit). Every eligibility condition that let
        // this judge fire was evaluated at START; between fire and now an
        // autonomous judge turn ran (seconds→minutes) during which the live
        // state can have changed out from under it — the user disabled
        // supervision, hit Stop (→ `Held`), the session hit a usage wall
        // (`Stopped`), or the session was torn down. Re-read the state and DROP
        // the verdict when the supervisor is no longer actively supervising this
        // session: supervision OFF (`!enabled`, i.e. `Disabled`), parked after a
        // manual Stop (`Held`), or dead (`Stopped`). Otherwise a verdict from a
        // run the user already cancelled would nudge the agent anyway. Combined
        // with `judge_superseded` (consumed here, set by
        // `supersede_judge_on_user_reply` / the prompt-change interrupt) this is
        // the "check at the end" half of the double-check — its "check at the
        // start" half is `should_fire`. `Watching` is allowed through so the
        // direct-`apply_verdict` paths (e.g. a `Done` verdict from `Watching`,
        // and the unit tests) still act. The verdict is still logged below for
        // audit — it just isn't acted on when dropped.
        let (verdict_superseded, supervising) = self
            .supervisor_states
            .get_mut(&id)
            .map(|s| {
                (
                    std::mem::take(&mut s.judge_superseded),
                    s.enabled
                        && !matches!(
                            s.status,
                            SupervisorStatus::Held | SupervisorStatus::Stopped(_)
                        ),
                )
            })
            .unwrap_or((false, false));
        // Send-time SESSION-state re-check: `should_fire` only lets a judge
        // fire while the session is idle/errored, but the judge turn runs
        // seconds→minutes and the agent can resume ON ITS OWN in the meantime
        // (a `Bash(run_in_background)` continuation lands as an orphan result
        // and flips the session back to `Running`; a tool-auth prompt →
        // `AwaitingInput`). Delivering a nudge now would just queue a spurious
        // extra turn behind the live one (the reported "supervisor reacted
        // while the agent was still alive and the message got queued"). Drop
        // the verdict unless the session is still genuinely idle/errored — the
        // same premise `should_fire` required at the start.
        let session_idle = self
            .session(id)
            .map(|s| {
                matches!(
                    s.read(cx).state,
                    SessionState::Idle | SessionState::Errored(_)
                )
            })
            .unwrap_or(true);
        let drop_verdict = verdict_superseded || !supervising || !session_idle;

        if let Some(root) = self.solution_root_for(id, cx) {
            let dir = crate::supervisor::supervisor_dir(&root, id);
            let rec = VerdictRecord {
                ts_ms: chrono::Utc::now().timestamp_millis(),
                kind: VerdictKind::Verdict,
                action: Some(action),
                audit_ok: None,
                reasoning: reasoning.clone(),
                message: message.clone(),
                question: question.clone(),
                // The verdict tool has no token figure to pass (`tokens` is
                // `None` in production); fill it from the live judge session's
                // own usage — read here while the judge is still alive, before
                // `finish_judge` below reaps it — so `total_tokens` reflects the
                // supervisor's real cost instead of always reading 0.
                tokens: tokens.or_else(|| {
                    self.ephemeral_session_tokens(
                        self.judge_sessions.get(&id).and_then(|h| h.judge_id),
                        cx,
                    )
                }),
                // Record whether this verdict is about to be dropped by the
                // send-time gate so the meta-auditor / `verdict_stats` don't
                // miscount a superseded verdict as an acted nudge.
                dropped: drop_verdict,
            };
            crate::supervisor::append_verdict(&dir, &rec).log_err();
        }

        self.finish_judge(id, cx);

        if drop_verdict {
            // `finish_judge` reaped the ephemeral judge but does NOT touch
            // `status`. If we got here still `Judging` (the `!session_idle`
            // drop — the agent resumed on its own mid-judge; the superseded /
            // `!supervising` drops already left `Watching`/`Held`/`Stopped`),
            // return to `Watching` so the status isn't pinned in `Judging` with
            // no live judge. Otherwise the judge-stuck watchdog would later
            // mistake it for a crashed judge and charge a bogus transient
            // backoff — compounding, over repeated benign self-resumes, to a
            // false `Stopped(ProviderError)` that silently kills supervision.
            if let Some(state) = self.supervisor_states.get_mut(&id)
                && matches!(state.status, SupervisorStatus::Judging)
            {
                state.status = SupervisorStatus::Watching;
                self.persist_supervisor_state(id, cx);
                cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
            }
            return;
        }

        // A real verdict means the judge succeeded: clear any transient-failure
        // backoff so a previously-flaky supervisor that recovered is not gated
        // on the next watchdog fire. Done here (covers Continue/Compact/Done/Ask
        // uniformly) rather than per-arm.
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            state.backoff_attempt = 0;
            state.next_eligible_ms = None;
        }
        self.backoff_timers.remove(&id);

        match action {
            VerdictAction::Continue => {
                let count = {
                    let state = self
                        .supervisor_states
                        .entry(id)
                        .or_insert_with(|| crate::supervisor::SupervisorState::new(id));
                    state.consecutive_continues += 1;
                    state.consecutive_continues
                };
                match crate::supervisor::continue_guard(count) {
                    crate::supervisor::ContinueGuard::Nudge => {
                        if let Some(state) = self.supervisor_states.get_mut(&id) {
                            state.status = SupervisorStatus::Watching;
                        }
                        self.persist_supervisor_state(id, cx);
                        let nudge = message.unwrap_or_else(|| "Continue.".into());
                        self.send_supervisor_nudge(id, nudge, cx).detach();
                    }
                    crate::supervisor::ContinueGuard::Audit => {
                        if let Some(state) = self.supervisor_states.get_mut(&id) {
                            state.status = SupervisorStatus::Watching;
                        }
                        self.persist_supervisor_state(id, cx);
                        let nudge = message.unwrap_or_else(|| "Continue.".into());
                        self.send_supervisor_nudge(id, nudge, cx).detach();
                        self.spawn_auditor(id, cx);
                    }
                    crate::supervisor::ContinueGuard::ForceAsk => {
                        let question = "The supervisor issued \"Continue\" 15 times in a row — \
                                        check whether the agent is stuck."
                            .to_string();
                        self.escalate_to_user(id, question, cx);
                    }
                }
            }
            VerdictAction::Compact => {
                if let Some(state) = self.supervisor_states.get_mut(&id) {
                    state.status = SupervisorStatus::Watching;
                }
                self.persist_supervisor_state(id, cx);
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
            VerdictAction::Done => {
                // The supervisor considers the work complete. Don't switch
                // supervision OFF — park it in `Held` (the same standby the
                // user's manual Stop uses): it stops acting on the current
                // dialog state but stays enabled, and the next human message
                // re-arms it to `Watching` (the user evidently has more work).
                // Mirrors the manual-stop pause so "done" and "I stopped it"
                // behave identically.
                if let Some(state) = self.supervisor_states.get_mut(&id) {
                    state.status = SupervisorStatus::Held;
                    // Mark this Held as done-sourced (NOT a manual stop) so a
                    // self-resume re-arms it — see `rearm_supervisor_on_self_activity`.
                    state.held_by_done = true;
                    state.next_eligible_ms = None;
                }
                self.backoff_timers.remove(&id);
                self.persist_supervisor_state(id, cx);
                self.clear_supervisor_question(id, cx);
                // Append the supervisor's summary to the cumulative session log,
                // labeling a PARK-awaiting-operator distinctly from a genuine
                // completion and keeping the raw `PARK:` marker out of the log
                // body (see `classify_done_reasoning`).
                let (is_park, done_body) = classify_done_reasoning(&reasoning);
                let done_header = if is_park {
                    "⏸ Session parked — awaiting operator (Supervisor)"
                } else {
                    "✓ Session complete (Supervisor)"
                };
                if let Some(root) = self.solution_root_for(id, cx) {
                    crate::supervisor::append_session_log(
                        &crate::supervisor::session_log_path(&root, id),
                        done_header,
                        done_body,
                        chrono::Utc::now().timestamp_millis(),
                    )
                    .log_err();
                }
                self.notify_supervisor_done(id, is_park, done_body, cx);
            }
            VerdictAction::Ask => {
                self.escalate_to_user(id, question.unwrap_or_default(), cx);
            }
            VerdictAction::AskAgent => {
                // Pose a clarifying question to the WORKING AGENT (not the
                // human). Mechanically like `Continue` — it sends a message
                // into the supervised session and counts toward the same
                // consecutive-nudge guards so it can't loop forever — but the
                // message is the judge's question, and the agent's answer lands
                // in the history for the next wake-up's verdict.
                let count = {
                    let state = self
                        .supervisor_states
                        .entry(id)
                        .or_insert_with(|| crate::supervisor::SupervisorState::new(id));
                    state.consecutive_continues += 1;
                    state.consecutive_continues
                };
                let agent_question = question.unwrap_or_else(|| {
                    "Is the task fully complete? If not, what concrete work remains?".into()
                });
                match crate::supervisor::continue_guard(count) {
                    crate::supervisor::ContinueGuard::Nudge => {
                        if let Some(state) = self.supervisor_states.get_mut(&id) {
                            state.status = SupervisorStatus::Watching;
                        }
                        self.persist_supervisor_state(id, cx);
                        self.send_supervisor_nudge(id, agent_question, cx).detach();
                    }
                    crate::supervisor::ContinueGuard::Audit => {
                        if let Some(state) = self.supervisor_states.get_mut(&id) {
                            state.status = SupervisorStatus::Watching;
                        }
                        self.persist_supervisor_state(id, cx);
                        self.send_supervisor_nudge(id, agent_question, cx).detach();
                        self.spawn_auditor(id, cx);
                    }
                    crate::supervisor::ContinueGuard::ForceAsk => {
                        let question = "The supervisor has nudged/queried the agent 15 times \
                                        in a row — check whether the agent is stuck."
                            .to_string();
                        self.escalate_to_user(id, question, cx);
                    }
                }
            }
            VerdictAction::Wait => {
                // The agent legitimately paused to wait on an async task it said
                // it would resume after. This is a ONE-SHOT decision: the judge
                // commits a single timeout (`wait_seconds`, clamped up to 30 min)
                // and the mechanism honors it in FULL via `wait_until_ms` — it
                // does NOT re-spawn a judge in between (re-judging an unchanged
                // wait is the wasteful poll we're eliminating). `tick_supervisor`
                // stays quiet until the deadline, then wakes the agent itself.
                // Stay `Watching` (the wait handler is gated on it). Wait does
                // NOT increment the consecutive-continue guard, so a long
                // legitimate wait can't trip the 15-nudge force-ask cap.
                let secs = crate::supervisor::clamp_wait_secs(wait_seconds);
                let wake_at = chrono::Utc::now().timestamp_millis() + (secs as i64) * 1000;
                if let Some(state) = self.supervisor_states.get_mut(&id) {
                    state.status = SupervisorStatus::Watching;
                    state.wait_until_ms = Some(wake_at);
                }
                self.persist_supervisor_state(id, cx);
            }
        }

        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    /// Handle a judge that failed to produce a verdict — either because the
    /// create/send errored, or because it ended/timed-out without calling its
    /// verdict tool. Tears down the judge handle, then classifies `message`:
    ///
    /// * [`JudgeFailure::Quota`] (usage/billing/rate-limit exhaustion) → stop
    ///   supervision immediately (`Stopped(Quota)`), no retries: more requests
    ///   would only burn into the same wall.
    /// * [`JudgeFailure::Transient`] → advance `backoff_attempt`; once it strictly
    ///   exceeds the [`BACKOFF_SCHEDULE_MINS`] length (i.e. on the 9th failure) give
    ///   up (`Stopped(ProviderError)`), otherwise gate the next watchdog fire
    ///   `BACKOFF_SCHEDULE_MINS[attempt-1]` minutes out (via `next_eligible_ms`)
    ///   and return to `Watching`. All 8 schedule entries (1,1,2,3,5,10,30,60 min)
    ///   are used before giving up.
    pub(crate) fn on_judge_failed(
        &mut self,
        id: SolutionSessionId,
        message: String,
        cx: &mut Context<Self>,
    ) {
        use crate::supervisor::{
            BACKOFF_SCHEDULE_MINS, JudgeFailure, StoppedReason, SupervisorState, SupervisorStatus,
        };
        self.finish_judge(id, cx);
        match crate::supervisor::classify_judge_error(&message) {
            JudgeFailure::Quota => {
                self.apply_usage_limit_stop(id, &message, cx);
            }
            JudgeFailure::Transient => {
                let attempt = {
                    let state = self
                        .supervisor_states
                        .entry(id)
                        .or_insert_with(|| SupervisorState::new(id));
                    state.backoff_attempt += 1;
                    state.backoff_attempt
                };
                // Give up once we've exceeded the schedule. With 8 entries
                // this means: attempts 1..=8 each schedule a retry (delays
                // index 0..=7), and the 9th transient failure exhausts.
                if attempt as usize > BACKOFF_SCHEDULE_MINS.len() {
                    if let Some(state) = self.supervisor_states.get_mut(&id) {
                        state.enabled = false;
                        state.status = SupervisorStatus::Stopped(StoppedReason::ProviderError);
                        state.next_eligible_ms = None;
                    }
                    self.backoff_timers.remove(&id);
                    self.append_supervisor_diary_note(
                        id,
                        "supervisor stopped: provider error, backoff schedule exhausted",
                        cx,
                    );
                    self.clear_supervisor_question(id, cx);
                } else {
                    let delay_mins = BACKOFF_SCHEDULE_MINS[(attempt - 1) as usize];
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    let next_eligible = now_ms + (delay_mins as i64) * 60_000;
                    if let Some(state) = self.supervisor_states.get_mut(&id) {
                        state.status = SupervisorStatus::Watching;
                        state.next_eligible_ms = Some(next_eligible);
                    }
                    self.append_supervisor_diary_note(
                        id,
                        &format!(
                            "judge transient failure (attempt {attempt}/{}): retry in {delay_mins} min",
                            BACKOFF_SCHEDULE_MINS.len()
                        ),
                        cx,
                    );
                    // Hold a live timer so the eligibility window is honoured by a
                    // wake-up even if no other tick churns; the gate itself is
                    // `next_eligible_ms`, checked in `tick_supervisor`.
                    let delay = std::time::Duration::from_secs(delay_mins * 60);
                    let task = cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(delay).await;
                        this.update(cx, |this, _cx| {
                            this.backoff_timers.remove(&id);
                        })
                        .ok();
                    });
                    self.backoff_timers.insert(id, task);
                }
                self.persist_supervisor_state(id, cx);
                cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
            }
        }
    }

    /// Handle a usage / session / weekly limit wall for supervised session
    /// `id`, described by `message`. A limit is a WALL, not a transient error
    /// to retry or a hung subprocess to reconnect: if the observer is still
    /// enabled AND `message` carries a parseable reset time, schedule an
    /// auto-resume — stay `Watching` with the watchdog gate set to
    /// `reset + jitter(2..=15min)`, so `tick_supervisor` re-fires a judge once
    /// the limit clears (which re-observes the idle/errored worker and nudges
    /// it to continue); the jitter avoids hammering the wall at the exact reset
    /// minute, and a live timer is held so the wake happens even if nothing
    /// else ticks. Otherwise (observer off, or no reset time) fall back to a
    /// terminal `Stopped(Quota)`. Shared by the judge-failure path
    /// (`on_judge_failed`) and the stuck-session watchdog (`tick_stuck_sessions`).
    pub(crate) fn apply_usage_limit_stop(
        &mut self,
        id: SolutionSessionId,
        message: &str,
        cx: &mut Context<Self>,
    ) {
        use crate::supervisor::{StoppedReason, SupervisorStatus};
        {
            use rand::Rng as _;
            let now_ms = chrono::Utc::now().timestamp_millis();
            let enabled = self
                .supervisor_states
                .get(&id)
                .map(|s| s.enabled)
                .unwrap_or(false);
            let resume_at_ms = if enabled {
                crate::supervisor::parse_usage_limit_reset_ms(message, now_ms).map(|reset| {
                    let jitter_ms = rand::rng().random_range(120_000i64..=900_000i64);
                    reset + jitter_ms
                })
            } else {
                None
            };
            match resume_at_ms {
                Some(resume_ms) => {
                    if let Some(state) = self.supervisor_states.get_mut(&id) {
                        state.status = SupervisorStatus::Watching;
                        state.next_eligible_ms = Some(resume_ms);
                        // Not a judge fault — don't count it toward the
                        // transient-failure backoff exhaustion.
                        state.backoff_attempt = 0;
                    }
                    let eta = chrono::DateTime::from_timestamp_millis(resume_ms)
                        .map(|dt| dt.with_timezone(&chrono::Local).format("%H:%M").to_string())
                        .unwrap_or_else(|| "?".into());
                    self.append_supervisor_diary_note(
                            id,
                            &format!(
                                "usage limit hit; auto-resume scheduled ~{eta} local (reset + 2-15min jitter)"
                            ),
                            cx,
                        );
                    self.push_system_note(
                            id,
                            acp_thread::SystemNoteLevel::Info,
                            format!(
                                "Достигнут лимит claude. Наблюдатель продолжит сессию автоматически примерно в {eta}."
                            ),
                            cx,
                        );
                    // Hold a live timer until the resume moment so a wake
                    // happens even if nothing else ticks the watchdog. The
                    // gate itself is `next_eligible_ms`, re-checked in
                    // `tick_supervisor`.
                    let delay =
                        std::time::Duration::from_millis((resume_ms - now_ms).max(0) as u64);
                    let task = cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(delay).await;
                        this.update(cx, |this, _cx| {
                            this.backoff_timers.remove(&id);
                        })
                        .ok();
                    });
                    self.backoff_timers.insert(id, task);
                }
                None => {
                    if let Some(state) = self.supervisor_states.get_mut(&id) {
                        state.enabled = false;
                        state.status = SupervisorStatus::Stopped(StoppedReason::Quota);
                        state.next_eligible_ms = None;
                    }
                    self.backoff_timers.remove(&id);
                    self.append_supervisor_diary_note(
                        id,
                        "supervisor stopped: provider quota / usage limit \
                             (no reset time parsed or observer disabled)",
                        cx,
                    );
                    self.clear_supervisor_question(id, cx);
                }
            }
        }
        self.persist_supervisor_state(id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

}
