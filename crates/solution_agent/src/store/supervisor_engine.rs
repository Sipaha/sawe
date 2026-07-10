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
    fn judge_wall_message(&self, id: SolutionSessionId, cx: &App) -> Option<String> {
        let judge_id = self.judge_sessions.get(&id)?.judge_id?;
        self.session_wall_message(judge_id, cx)
    }

    /// Cumulative tokens the ephemeral judge/auditor session `judge_id` reported
    /// (live `TokenUsage.used_tokens`, else the cached mirror). Read at
    /// verdict/audit time — while the ephemeral session is still alive, before
    /// `finish_judge`/`finish_auditor` reaps it — so a `VerdictRecord` can record
    /// what the supervisor's own review turn cost (the verdict tool itself has no
    /// token figure to pass). `None` when the session is gone or has no usage yet.
    fn ephemeral_session_tokens(&self, judge_id: Option<SolutionSessionId>, cx: &App) -> Option<u64> {
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

    /// Reload session `id`'s persisted supervisor row into the in-memory map when
    /// the session is reopened IN-PROCESS (a soft/cold close evicted the runtime
    /// state via `evict_session_runtime_maps`, but `load_supervisor_states` only
    /// runs once at startup). Without this a reopened session shows the observer
    /// OFF even though its persisted row says `enabled` — silent unsupervision —
    /// and the stale `enabled=true` row then RESURRECTS supervision on the NEXT
    /// editor restart, on a session the user believed unsupervised (finding #5).
    /// No-op if the state is already live (never evicted / re-enabled since the
    /// close) so an in-session toggle isn't clobbered. Anchors `watch_started_ms
    /// = now` like the startup load so a pre-close idle session isn't judged the
    /// instant it reopens.
    pub(super) fn reload_supervisor_state_for(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) {
        if self.supervisor_states.contains_key(&id) {
            return;
        }
        let Some(db) = self.persistence.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let states = db
                .load_supervisor_states()
                .await
                .log_err()
                .unwrap_or_default();
            this.update(cx, |this, _| {
                if this.supervisor_states.contains_key(&id) {
                    return;
                }
                if let Some(mut st) = states.into_iter().find(|s| s.session_id == id) {
                    st.watch_started_ms = Some(chrono::Utc::now().timestamp_millis());
                    this.supervisor_states.entry(id).or_insert(st);
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn supervisor_state(
        &self,
        id: SolutionSessionId,
    ) -> Option<crate::supervisor::SupervisorState> {
        self.supervisor_states.get(&id).cloned()
    }

    pub fn set_supervision_enabled(
        &mut self,
        id: SolutionSessionId,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let state = self
            .supervisor_states
            .entry(id)
            .or_insert_with(|| crate::supervisor::SupervisorState::new(id));
        if state.enabled == enabled {
            return;
        }
        state.enabled = enabled;
        // Every enable/disable toggle clears the activity counters (fire count +
        // consecutive-continue cap) — a fresh on/off starts the tally over.
        state.trigger_count = 0;
        state.consecutive_continues = 0;
        if enabled {
            state.status = crate::supervisor::SupervisorStatus::Watching;
            state.consecutive_continues = 0;
            state.backoff_attempt = 0;
            // A fresh enable must never inherit a stale backoff gate from a
            // previous transient-failure run, or the watchdog would refuse to
            // fire until the old delay elapsed.
            state.next_eligible_ms = None;
            // Nor inherit transient markers from a previous run: a leftover
            // supersede flag, a nudge held for a since-departed draft, or a
            // parked wait would otherwise leak into the first fresh cycle.
            state.judge_superseded = false;
            state.pending_nudge = None;
            state.wait_until_ms = None;
            self.backoff_timers.remove(&id);
        } else {
            state.status = crate::supervisor::SupervisorStatus::Disabled;
            // Turning supervision OFF must take effect immediately, including on
            // work already in flight: discard any nudge held for the user to
            // stop typing, and mark so a verdict already racing out of a judge
            // we're about to tear down is dropped by `apply_verdict`'s send-time
            // gate (belt-and-suspenders with the `!enabled` check there).
            state.pending_nudge = None;
            state.wait_until_ms = None;
            state.judge_superseded = true;
            // `state` borrow ends here — the `self.*` teardown calls below need
            // `&mut self`.
            self.backoff_timers.remove(&id);
            self.clear_supervisor_question(id, cx);
            // Interrupt an in-flight observer: a judge/auditor mid-run would
            // otherwise keep running and deliver a nudge after the user already
            // switched supervision off. `hold_supervisor` tears the judge down
            // for the manual-Stop path; disable must do the same (plus the
            // auditor). Without this, disabling did NOT stop a running observer.
            self.finish_judge(id, cx);
            self.finish_auditor(id, cx);
        }
        self.persist_supervisor_state(id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    pub fn set_supervisor_prompt(
        &mut self,
        id: SolutionSessionId,
        prompt: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let prompt = prompt.filter(|p| !p.trim().is_empty());
        let changed = {
            let state = self
                .supervisor_states
                .entry(id)
                .or_insert_with(|| crate::supervisor::SupervisorState::new(id));
            let changed = state.custom_prompt != prompt;
            state.custom_prompt = prompt;
            changed
        };
        // Changing the supervisor's instruction makes an IN-FLIGHT judge's
        // verdict useless — it reviewed the conversation under the OLD
        // instruction. Rather than let it run to completion and drop the stale
        // verdict at send time, interrupt it now and return to `Watching` so the
        // next tick re-fires a fresh judge under the new instruction. `superseded`
        // covers a verdict already racing out of the torn-down judge.
        if changed && self.judge_sessions.contains_key(&id) {
            self.finish_judge(id, cx);
            if let Some(state) = self.supervisor_states.get_mut(&id) {
                state.judge_superseded = true;
                if matches!(state.status, crate::supervisor::SupervisorStatus::Judging) {
                    state.status = crate::supervisor::SupervisorStatus::Watching;
                }
            }
        }
        self.persist_supervisor_state(id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    /// Wipe the observer's durable memory (diary, verdicts, user-intent) AND
    /// reset its in-memory reasoning cursor for `id`, on a HUMAN-initiated
    /// `/clear` or `/compact`. Gives the supervisor a clean slate so it doesn't
    /// re-read stale notes or re-litigate settled directives after the user
    /// reset the thread. NOT invoked on an observer-issued `compact` verdict
    /// (that path keeps `user_intent.md`). See
    /// [`crate::supervisor::wipe_supervisor_memory`].
    pub(crate) fn wipe_supervisor_memory(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(root) = self.solution_root_for(id, cx) {
            let dir = crate::supervisor::supervisor_dir(&root, id);
            crate::supervisor::wipe_supervisor_memory(&dir);
        }
        // Reset the in-memory reasoning cursor too: the continue-loop counter
        // and any parked/one-shot verdict state, so a stale nudge or wait can't
        // fire against the freshly-cleared thread and the continue cadence
        // restarts from zero. Leaves identity/config (enabled, status,
        // custom_prompt, trigger_count) intact.
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            state.consecutive_continues = 0;
            state.pending_nudge = None;
            state.judge_superseded = false;
            state.wait_until_ms = None;
        }
        self.persist_supervisor_state(id, cx);
    }

    /// Record `abandoned_acp_id` (the ACP session orphaned by a `/compact`
    /// rotation or `/clear`) for session `id` and delete claude's on-disk JSONL
    /// transcripts that fall outside the keep-window — the live transcript plus
    /// the last `KEEP_RAW_TRANSCRIPTS - 1` abandoned ones. Each ACP session
    /// leaves a `~/.claude/projects/<cwd>/<acp_session_id>.jsonl` (+ a
    /// `<acp_session_id>/` subagents dir) that claude never cleans up, so a
    /// multi-day session would otherwise accrue gigabytes of dead transcripts.
    /// Best-effort: any IO error is ignored. Keyed by our `SolutionSessionId`,
    /// so only THIS session's transcripts are ever deleted even when several
    /// sessions share the same project cwd.
    pub(super) fn prune_raw_transcripts(
        &mut self,
        id: SolutionSessionId,
        abandoned_acp_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(cwd) = self.sessions.get(&id).map(|s| s.read(cx).cwd.clone()) else {
            return;
        };
        if cwd.as_os_str().is_empty() {
            return;
        }
        let history = self.raw_transcript_history.entry(id).or_default();
        let evicted = push_and_evict_transcripts(history, abandoned_acp_id, KEEP_RAW_TRANSCRIPTS);
        for old in evicted {
            if let Some(jsonl) = parent_session_jsonl_for(&cwd, &old) {
                let _ = std::fs::remove_file(jsonl);
            }
            if let Some(proj) = claude_project_dir_for(&cwd) {
                let _ = std::fs::remove_dir_all(proj.join(&old));
            }
        }
    }

    /// Called when the HUMAN sends a message into a supervised session. Three
    /// effects, all keyed off the supervisor's current status:
    ///
    /// * resets the consecutive-continue counter (cap / audit cadence restart);
    /// * resumes a `WaitingUser` pause (the human answered the `ask`) → `Watching`;
    /// * **re-arms a `Stopped(Done)` session**: the supervisor had declared the
    ///   task complete and auto-disabled itself, but a new user message means the
    ///   work continues, so supervision re-enables → `Watching`. Re-arm is scoped
    ///   to `Done` only — a user-driven toggle-off (`Disabled`) stays off, and a
    ///   `Quota` / `ProviderError` stop is an infra wall we don't auto-retry here.
    pub(crate) fn reset_supervisor_continue_counter(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        use crate::supervisor::{StoppedReason, SupervisorStatus};
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            let was_done = matches!(state.status, SupervisorStatus::Stopped(StoppedReason::Done));
            let waiting = matches!(state.status, SupervisorStatus::WaitingUser);
            // A `Held` session (the user manually stopped the agent) re-arms on
            // the next human message — the user has decided to continue.
            let held = matches!(state.status, SupervisorStatus::Held);
            if state.consecutive_continues == 0 && !waiting && !was_done && !held {
                return;
            }
            state.consecutive_continues = 0;
            if held {
                // Leaving Held: clear any stale backoff so the watchdog can fire
                // on the next idle once the agent finishes the new turn.
                state.next_eligible_ms = None;
            }
            if was_done {
                // Re-arm: Done auto-disabled supervision; the user is continuing
                // the work, so restore their original enabled intent. Clear any
                // stale backoff so the watchdog can fire on the next idle.
                state.enabled = true;
                state.backoff_attempt = 0;
                state.next_eligible_ms = None;
            }
            if state.enabled {
                state.status = SupervisorStatus::Watching;
            }
            if was_done || held {
                self.backoff_timers.remove(&id);
            }
            self.persist_supervisor_state(id, cx);
            self.clear_supervisor_question(id, cx);
            cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
        }
    }

    /// Re-arm supervision when a PARKED session resumes **on its own** — the
    /// agent produced genuinely-new activity (a self-scheduled monitor /
    /// `ScheduleWakeup` fired and continued the work, or a background task the
    /// editor doesn't track came back) while the supervisor was parked. Two park
    /// states rest on the premise "nothing moves until the human acts" and are
    /// falsified by a self-resume, so they return to `Watching`:
    /// * `WaitingUser` — an `ask` escalation ("Waiting for you").
    /// * `Held` **when `held_by_done`** — a `done` verdict parked it ("On hold").
    ///
    /// A `Held` set by a MANUAL user Stop (`held_by_done == false`) is
    /// deliberately EXCLUDED: only a human message may resume that (the "don't
    /// drag it back before I decide" rule). This is the whole reason `held_by_done`
    /// exists — `done` and manual-stop share the `Held` status but must not share
    /// self-resume behaviour. No-op in any other state (already `Watching`,
    /// `Disabled`, `Quota`/`ProviderError`), so it's safe to call on every agent
    /// entry — the first self-resume entry re-arms and the rest early-return.
    ///
    /// Distinct from [`reset_supervisor_continue_counter`] (the HUMAN-message
    /// re-arm, which resumes ALL of `Held`/`WaitingUser`/`Done`): a self-resume is
    /// narrower — it must not resume a manual stop.
    pub(crate) fn rearm_supervisor_on_self_activity(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        use crate::supervisor::SupervisorStatus;
        {
            let Some(state) = self.supervisor_states.get_mut(&id) else {
                return;
            };
            let waiting = matches!(state.status, SupervisorStatus::WaitingUser);
            let done_hold =
                matches!(state.status, SupervisorStatus::Held) && state.held_by_done;
            if !waiting && !done_hold {
                return;
            }
            state.consecutive_continues = 0;
            state.next_eligible_ms = None;
            state.held_by_done = false;
            // Both park states keep `enabled == true`; if the user disabled
            // supervision the status would be `Disabled`, filtered out above. So
            // the session is always eligible to return to active watching.
            if state.enabled {
                state.status = SupervisorStatus::Watching;
            }
        }
        self.backoff_timers.remove(&id);
        self.persist_supervisor_state(id, cx);
        self.clear_supervisor_question(id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    /// Park the supervisor in `Held` because the HUMAN manually stopped the
    /// agent (Stop button / `cancel_turn`). Supervision stays enabled but must
    /// not re-engage on the current dialog state — no judge, no nudge — until the
    /// next human message re-arms it (`reset_supervisor_continue_counter`). This
    /// is the fix for "I stopped the agent myself; don't let the observer drag it
    /// back to work before I decide to continue." No-op unless supervision is
    /// enabled and currently `Watching`/`Judging`. Any in-flight judge is torn
    /// down so a verdict already in flight can't nudge the agent after the stop.
    pub(crate) fn hold_supervisor(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) {
        use crate::supervisor::SupervisorStatus;
        let should_hold = self.supervisor_states.get(&id).is_some_and(|s| {
            s.enabled
                && matches!(
                    s.status,
                    SupervisorStatus::Watching | SupervisorStatus::Judging
                )
        });
        if !should_hold {
            return;
        }
        // Tear down an in-flight judge AND a racing meta-auditor: at user-stop
        // time the session is usually Running (status Watching, no judge), but if
        // a judge/auditor had just spawned it would otherwise still deliver a
        // verdict and nudge/escalate the agent after the user stopped it. (The
        // auditor spawns while `Watching`, so `finish_judge` alone misses it — a
        // late audit `escalate` would force `WaitingUser`, which self-resumes,
        // dragging back the very session the user manually stopped.)
        self.finish_judge(id, cx);
        self.finish_auditor(id, cx);
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            state.status = SupervisorStatus::Held;
            // A MANUAL stop — NOT done-sourced. Only a human message may resume
            // it; a self-resume must NOT re-arm it (the "don't drag it back"
            // rule). Set explicitly so a stale `held_by_done` can't leak in.
            state.held_by_done = false;
            state.next_eligible_ms = None;
        }
        self.backoff_timers.remove(&id);
        self.persist_supervisor_state(id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    /// A HUMAN reply into a supervised session that is mid-`Judging` supersedes
    /// the in-flight judge: the user has taken over direction, so the judge's
    /// pending verdict is stale and must not nudge the agent afterwards. Tear
    /// the judge down (its verdict, if it still races in via the MCP tool, is
    /// then dropped by [`apply_verdict`]'s staleness guard) and return
    /// supervision to `Watching`. No-op unless a judge is actually in flight.
    ///
    /// Distinct from [`hold_supervisor`] (manual STOP → `Held`, supervision
    /// stands by): a reply means "keep working on what I just said", so normal
    /// watching resumes rather than parking. Called from the single user-send
    /// funnel ([`send_message_blocks_targeted`] with `from_user`), alongside
    /// `reset_supervisor_continue_counter` — separate because that reset
    /// early-returns when `consecutive_continues == 0`, which would skip the
    /// FIRST judge of a session.
    pub(crate) fn supersede_judge_on_user_reply(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        use crate::supervisor::SupervisorStatus;
        if !self.judge_sessions.contains_key(&id) {
            return;
        }
        self.finish_judge(id, cx);
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            // Mark so a verdict that already left the judge (racing the
            // teardown) is dropped by `apply_verdict`'s guard.
            state.judge_superseded = true;
            // The user answered for themselves, so any Observer nudge parked for
            // the "user stopped typing" flush is now stale — forget it, don't
            // deliver it after the user's own message.
            state.pending_nudge = None;
            if matches!(state.status, SupervisorStatus::Judging) {
                state.status = SupervisorStatus::Watching;
            }
        }
        self.persist_supervisor_state(id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    /// Note that the HUMAN is typing into `id`'s compose box. Pushes the
    /// supervisor's idle clock forward (transient `last_user_input_ms`), so the
    /// watchdog treats the session as active for another `IDLE_THRESHOLD_SECS`
    /// and never fires a nudge while the user is mid-message. Cheap + frequent
    /// (one per keystroke burst): in-memory only, no persist, no event.
    pub(crate) fn note_user_input(&mut self, id: SolutionSessionId) {
        if let Some(state) = self.supervisor_states.get_mut(&id)
            && state.enabled
        {
            state.last_user_input_ms = Some(chrono::Utc::now().timestamp_millis());
        }
    }

    /// Clear the pending supervisor question banner for session `id`. Emits
    /// `SessionStateChanged` only when the field was actually set (avoids a
    /// spurious notify when it was already `None`).
    pub(crate) fn clear_supervisor_question(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.session(id) {
            let was_set = session.read(cx).supervisor_question.is_some();
            if was_set {
                session.update(cx, |s, _| s.supervisor_question = None);
                cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
            }
        }
    }

    /// Escalate a supervisor question to the user: set `WaitingUser`, store
    /// the question on the session for the banner, surface it in-chat as an
    /// agent-invisible Observer bubble, fire a high-priority desktop
    /// notification, and emit `SessionStateChanged`.
    pub(crate) fn escalate_to_user(
        &mut self,
        id: SolutionSessionId,
        question: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            state.status = crate::supervisor::SupervisorStatus::WaitingUser;
        }
        self.persist_supervisor_state(id, cx);
        if let Some(session) = self.session(id) {
            session.update(cx, |s, _| {
                s.supervisor_question = Some(question.clone().into())
            });
        }
        // Surface the observer's question in the transcript as an Observer
        // bubble (FORK.md #29 render — eye badge, Accent). It is a `SystemNote`,
        // so it is DISPLAY-ONLY: the user reads it in-chat but the working agent
        // never sees it (not sent to the agent / not in its transcript). This
        // complements the persistent `supervisor_question` banner + the desktop
        // toast below, so the ask isn't lost when the toast is dismissed.
        self.push_system_note(
            id,
            acp_thread::SystemNoteLevel::Observer,
            question.clone(),
            cx,
        );
        let title = "Sawe — Supervisor".to_string();
        let body = format!("🛡 {question}");
        crate::notifier::dispatch_raw(
            id,
            crate::notifier::NotifyKind::AwaitingInput,
            &title,
            &body,
            cx,
        );
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    /// Notify the user that the supervisor concluded the turn — a genuine
    /// completion (`is_park == false`) or a PARK awaiting the operator
    /// (`is_park == true`). `reason` is the human-facing body with the internal
    /// `PARK:` marker already stripped (see `classify_done_reasoning`), so a park
    /// is never announced as "Work complete" and the raw token never leaks.
    pub(crate) fn notify_supervisor_done(
        &mut self,
        id: SolutionSessionId,
        is_park: bool,
        reason: &str,
        cx: &mut Context<Self>,
    ) {
        let label = if is_park {
            "⏸ Parked — awaiting you"
        } else {
            "✓ Work complete"
        };
        // In-chat Observer bubble (display-only, agent-invisible — see
        // `escalate_to_user`) so the verdict is visible in the transcript, not
        // just as a transient desktop toast.
        self.push_system_note(
            id,
            acp_thread::SystemNoteLevel::Observer,
            format!("{label}: {reason}"),
            cx,
        );
        let title = "Sawe — Supervisor".to_string();
        let body = format!("{label}: {reason}");
        // A park is "the agent is blocked on YOU" — the same attention class as
        // `AwaitingInput` (→ high-priority toast), not a genuine completion.
        let kind = if is_park {
            crate::notifier::NotifyKind::AwaitingInput
        } else {
            crate::notifier::NotifyKind::Completed
        };
        crate::notifier::dispatch_raw(id, kind, &title, &body, cx);
    }

    /// Send a supervisor-generated nudge message. Unlike the public
    /// [`send_message`](crate::store::queue) entry point, this does NOT reset
    /// the consecutive-continue counter — supervisor nudges must never clear
    /// the guard that was incremented just before the nudge was issued.
    fn send_supervisor_nudge(
        &mut self,
        id: SolutionSessionId,
        content: String,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        // "Hold on typing": if the human is composing a message RIGHT NOW (a
        // keystroke within `IDLE_THRESHOLD_SECS`), do not drop the nudge into
        // the conversation mid-sentence. The verdict has already been accepted
        // by `apply_verdict` (the continue-counter bumped); park its nudge in
        // `pending_nudge` and let `tick_supervisor` deliver it once the user has
        // gone quiet for the standard idle window — or a genuine user SEND
        // discards it (`supersede_judge_on_user_reply`). The start-time guard
        // (`should_fire`) only blocks a NEW judge from firing while the user
        // types; it cannot cover a judge that fired while the user was idle and
        // finished after the user began typing — this is that missing seam.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let composing = self
            .supervisor_states
            .get(&id)
            .and_then(|s| s.last_user_input_ms)
            .is_some_and(|t| {
                now_ms.saturating_sub(t) < (crate::supervisor::IDLE_THRESHOLD_SECS as i64) * 1000
            });
        if composing {
            if let Some(state) = self.supervisor_states.get_mut(&id) {
                state.pending_nudge = Some(content);
            }
            cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
            return gpui::Task::ready(Ok(()));
        }
        self.deliver_nudge_now(id, content, cx)
    }

    /// Deliver an Observer nudge into the supervised session's conversation
    /// unconditionally (the hold-on-typing decision lives in the callers:
    /// [`send_supervisor_nudge`] parks it when the user is mid-message,
    /// `tick_supervisor` flushes a parked nudge once the user is quiet).
    fn deliver_nudge_now(
        &mut self,
        id: SolutionSessionId,
        content: String,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        // The nudge is the SINGLE visible element: stamp it with the
        // `spk_observer_nudge` `_meta` marker so `conversation_render` shows it
        // as an OBSERVER comment (eye plaque) instead of a plain user bubble. We
        // no longer emit a separate "Наблюдатель направил агента: …" breadcrumb
        // note — the marked message itself carries the full instruction and the
        // observer attribution, so the old two-element layout (gist note + plain
        // bubble) is gone. The marker rides on `_meta`, invisible to the agent's
        // text.
        let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
            agent_client_protocol::schema::TextContent::new(content)
                .meta(Some(acp_thread::meta_with_observer_nudge())),
        )];
        // `from_user: false` — a supervisor nudge must NOT reset the
        // continue-cap counter (apply_verdict just incremented it) nor be
        // mistaken for a human reply that resumes a `WaitingUser` pause.
        self.send_message_blocks_targeted(id, blocks, crate::model::QueueTarget::Main, false, cx)
    }

    /// Recovery sweep for wedged sessions. A session stuck in `Running` with
    /// no streaming / tool activity for [`STUCK_TURN_SECS`] has a hung or dead
    /// claude subprocess: a healthy turn streams thinking / text / tool calls
    /// well within that window, each of which bumps `last_activity_at` (so the
    /// silence clock self-resets on any progress). A cleanly *exited*
    /// subprocess is already recovered by the connection's EOF path (it fails
    /// the pending prompt → `Errored`); this catches the harder hung-but-alive
    /// case the EOF path can't see.
    ///
    /// A genuinely-busy turn IS distinguishable from a hang: when claude blocks
    /// on a slow FOREGROUND command it leaves that tool call in
    /// [`ToolCallStatus::InProgress`] for the command's whole duration. So we
    /// only treat a silent turn as wedged when there is NO in-progress tool call.
    /// When there IS one we leave it alone until that single tool has both
    /// exceeded an unreasonable [`TOOL_STUCK_SECS`] AND stopped showing liveness
    /// — no output for [`TOOL_OUTPUT_SILENCE_SECS`] and no running OS process. A
    /// display-only command's output each bumps `last_activity_at` (it rides a
    /// `ToolCallUpdate`), so `silent_secs` already measures how long ago it last
    /// printed; a real client-side PTY's output does not, so that path is covered
    /// by [`acp_thread::Terminal::is_process_running`]. A build/deploy that keeps
    /// printing (or whose process is alive) is therefore never reconnected out
    /// from under itself (hardening #7); only one that truly hangs (silent, no
    /// live process) is. Background (`run_in_background`) commands don't block
    /// claude, so they keep streaming and never get here.
    ///
    /// Recovery is [`reconnect_agent`] — non-destructive: it respawns the
    /// subprocess and replays the same transcript, keeping the conversation.
    /// `reconnect_agent` synchronously flips the session out of `Running` (to
    /// `Errored("reconnecting…")`), so the next tick won't re-fire for it.
    pub(crate) fn tick_stuck_sessions(&mut self, cx: &mut Context<Self>) {
        use crate::model::SessionState;
        use acp_thread::{AgentThreadEntry, AssistantMessageChunk, ToolCallStatus};
        let now = Utc::now();
        // Each wedged session is tagged with `Some(limit_message)` when its
        // stall is actually a usage/session/weekly-limit wall rather than a hung
        // subprocess — those are recovered differently (see the loop below).
        let stuck: Vec<(SolutionSessionId, Option<String>)> = self
            .sessions
            .iter()
            .filter_map(|(id, session)| {
                let s = session.read(cx);
                // Live, project-backed, non-ephemeral sessions mid-turn only.
                // (Ephemeral judge/auditor sessions are short-lived and cold /
                // prebuilt sessions have nothing to reconnect.)
                if s.is_supervisor_ephemeral
                    || s.project.is_none()
                    || !matches!(s.state, SessionState::Running { .. })
                {
                    return None;
                }
                let thread = s.acp_thread()?;
                let silent_secs = now.signed_duration_since(s.last_activity_at).num_seconds();
                // Not silent long enough yet → claude is clearly alive.
                if silent_secs < STUCK_TURN_SECS as i64 {
                    return None;
                }
                let thread_ref = thread.read(cx);
                // The most-recent in-progress tool call as `(secs_running,
                // shows_liveness)`. `None` = no tool is executing right now. A
                // foreground build/deploy is "alive" — and so must NOT be
                // reconnected out from under itself (hardening #7) — when its OS
                // process is still running (real client-side PTY) OR it printed
                // within `TOOL_OUTPUT_SILENCE_SECS`. For the display-only path
                // (claude-acp) each output chunk bumps `last_activity_at`, so
                // `silent_secs` is exactly the time since the command last
                // printed; the real-PTY path's output does not, so it's covered
                // by the direct process check.
                let active_tool = thread_ref.entries().iter().rev().find_map(|e| match e {
                    AgentThreadEntry::ToolCall(tc)
                        if matches!(tc.status, ToolCallStatus::InProgress) =>
                    {
                        let since = tc.status_started_at.unwrap_or(s.last_activity_at);
                        let tool_secs = now.signed_duration_since(since).num_seconds();
                        let pty_running =
                            tc.terminals().any(|term| term.read(cx).is_process_running(cx));
                        let shows_liveness = pty_running
                            || silent_secs < TOOL_OUTPUT_SILENCE_SECS as i64;
                        Some((tool_secs, shows_liveness))
                    }
                    _ => None,
                });
                if !turn_is_wedged(active_tool) {
                    return None;
                }
                // Distinguish a usage/session-limit WALL from a genuine hang: a
                // turn that hit the limit prints the wall as its last assistant
                // message and then stalls (nothing ends the turn), so it looks
                // silent-and-wedged. Reconnecting + "carry on" there just re-hits
                // the wall and burns more quota (observed loop: repeated "You've
                // hit your session limit" + a spurious "your process hung,
                // continue" nudge). Detect it by scanning the latest assistant
                // message so the loop can route it to quota recovery instead.
                let limit_message = thread_ref
                    .entries()
                    .iter()
                    .rev()
                    .find_map(|e| match e {
                        AgentThreadEntry::AssistantMessage(m) => Some(
                            m.chunks
                                .iter()
                                .map(|chunk| match chunk {
                                    AssistantMessageChunk::Message { block }
                                    | AssistantMessageChunk::Thought { block } => {
                                        block.to_markdown(cx)
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(" "),
                        ),
                        _ => None,
                    })
                    .filter(|text| crate::supervisor::is_usage_limit_error(text));
                Some((*id, limit_message))
            })
            .collect();
        for (id, limit_message) in stuck {
            match limit_message {
                // Usage/session-limit wall, NOT a hang: stop the runaway turn and
                // hand off to the shared quota handler (auto-resume at the reset
                // time if supervised, else `Stopped(Quota)`) instead of
                // reconnecting + nudging "continue" (which re-hits the wall).
                Some(message) => {
                    log::warn!(
                        target: "solution_agent::store",
                        "session={id} stalled on a usage/session-limit wall (not a hang) — \
                         stopping turn + scheduling quota recovery, no reconnect"
                    );
                    self.append_supervisor_diary_note(
                        id,
                        "turn hit a usage/session limit while Running; stopped (no reconnect), \
                         quota recovery scheduled",
                        cx,
                    );
                    self.mutate_state(
                        id,
                        |st| *st = SessionState::Errored(SharedString::from(message.clone())),
                        cx,
                    );
                    self.push_system_note(
                        id,
                        acp_thread::SystemNoteLevel::Error,
                        "Достигнут лимит claude — текущий ход остановлен (без переподключения).",
                        cx,
                    );
                    self.apply_usage_limit_stop(id, &message, cx);
                }
                // Genuine hang: reconnect (respawn subprocess + replay transcript).
                None => {
                    log::warn!(
                        target: "solution_agent::store",
                        "session={id} wedged in Running (no progress {STUCK_TURN_SECS}s, no live \
                         tool — or a tool in-progress >{TOOL_STUCK_SECS}s with no output for \
                         >{TOOL_OUTPUT_SILENCE_SECS}s) — auto-reconnecting (respawn subprocess + \
                         replay transcript)"
                    );
                    self.append_supervisor_diary_note(
                        id,
                        "session wedged while Running (hung subprocess); auto-reconnect",
                        cx,
                    );
                    self.reconnect_agent(id, cx).detach_and_log_err(cx);
                }
            }
        }
    }

    pub(crate) fn tick_supervisor(&mut self, cx: &mut Context<Self>) {
        use crate::model::SessionState;
        let now_ms = chrono::Utc::now().timestamp_millis();

        // Auditor-stuck sweep: a meta-auditor spawns while the supervised
        // session is `Watching` (not `Judging`), so the judge-stuck timeout in
        // the per-session loop below never catches it. An auditor that
        // errors/ends WITHOUT calling `supervisor_audit_verdict` would leave its
        // `auditor_sessions` handle live forever and permanently disable
        // meta-audit for that session. Clean up any auditor older than the
        // timeout so the next audit cycle can spawn fresh. No supervision
        // backoff is applied — the auditor failing is not the judge failing.
        let stale_auditors: Vec<SolutionSessionId> = self
            .auditor_sessions
            .iter()
            .filter(|(_, handle)| {
                now_ms.saturating_sub(handle.started_ms)
                    >= (crate::supervisor::AUDITOR_TIMEOUT_SECS as i64) * 1000
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale_auditors {
            self.finish_auditor(id, cx);
            self.append_supervisor_diary_note(
                id,
                "meta-auditor timed out / ended without verdict; handle cleaned up",
                cx,
            );
        }

        let session_ids: Vec<SolutionSessionId> = self
            .supervisor_states
            .iter()
            .filter(|(_, st)| st.enabled)
            .map(|(id, _)| *id)
            .collect();
        for id in session_ids {
            let Some(state) = self.supervisor_states.get(&id) else {
                continue;
            };
            let enabled = state.enabled;
            let status = state.status.clone();
            let next_eligible_ms = state.next_eligible_ms;
            let last_fired_at = state.last_fired_at;
            let last_user_input_ms = state.last_user_input_ms;
            // Anchors the idle clock to "since this process started watching",
            // set only on the restart/load path (`set_persistence`). A session
            // whose supervision was enabled fresh THIS process leaves it `None`
            // (no baseline → normal immediate idle semantics).
            let watch_started_ms = state.watch_started_ms;

            // Judge-stuck watchdog: a judge that errored / ended WITHOUT calling
            // its verdict tool leaves the session pinned in `Judging` forever
            // (finish_judge never ran), so the watchdog would never re-fire.
            // Treat an over-long `Judging` window as a transient failure. This
            // single timeout uniformly catches crash, error, AND silent-end
            // without per-judge session-state subscriptions.
            if matches!(status, crate::supervisor::SupervisorStatus::Judging) {
                // A `Judging` status with no `last_fired_at` is a corrupt/phantom
                // wedge (currently unreachable — every fire sets it and the DB
                // load coerces `Judging`+`None` → `Watching`). Treat `None` as
                // "already stuck" (`0`, i.e. infinitely old) so it un-wedges
                // immediately rather than being pinned forever by `now_ms`.
                let fired_at = last_fired_at.unwrap_or(0);
                let stuck_ms = now_ms.saturating_sub(fired_at);
                if stuck_ms >= (crate::supervisor::JUDGE_TIMEOUT_SECS as i64) * 1000 {
                    // `Some(judge_id)` = a real judge handle is registered (the
                    // inner `judge_id` may still be `None` if its session hasn't
                    // been created yet); `None` = phantom (spawn early-returned).
                    if let Some(judge_id) = self.judge_sessions.get(&id).map(|h| h.judge_id) {
                        // LIVENESS (finding #5): the wall-clock timeout is crossed,
                        // but don't kill a judge that is still demonstrably working.
                        // Check the judge SESSION's own activity clock — a streaming
                        // judge bumps it on every thinking/text/tool event — and
                        // extend while it progresses, up to a hard cap that still
                        // catches a runaway (infinite-thinking) judge.
                        let judge_silent_ms = judge_id
                            .and_then(|jid| self.session(jid))
                            .map(|js| {
                                now_ms.saturating_sub(
                                    js.read(cx).last_activity_at.timestamp_millis(),
                                )
                            });
                        let judge_alive = judge_silent_ms.is_some_and(|ms| {
                            ms < (crate::supervisor::JUDGE_LIVENESS_SILENCE_SECS as i64) * 1000
                        });
                        let under_hard_cap =
                            stuck_ms < (crate::supervisor::JUDGE_HARD_TIMEOUT_SECS as i64) * 1000;
                        if judge_alive && under_hard_cap {
                            // Still streaming — leave it be; re-check next tick.
                            continue;
                        }
                        // A real judge that timed out / ended / went silent without
                        // a verdict. If it stalled on the usage wall (its own
                        // transcript shows it — the judge hits the same account wall
                        // as the worker), route to QUOTA recovery (schedule the
                        // reset-time resume) instead of a transient failure that
                        // spirals to a false `Stopped(ProviderError)` (finding #3).
                        // Otherwise it's a genuine timeout.
                        let message = self
                            .judge_wall_message(id, cx)
                            .unwrap_or_else(|| "judge timed out / ended without verdict".to_string());
                        self.on_judge_failed(id, message, cx);
                    } else {
                        // PHANTOM `Judging` (finding #2): the fire set `Judging` +
                        // `last_fired_at`, but the judge SPAWN early-returned (a
                        // cold session with no project / no live thread), so no
                        // judge handle was ever registered. This is NOT a timed-out
                        // judge — charging it as a transient failure would, over
                        // repeated phantoms, spiral to a FALSE
                        // `Stopped(ProviderError)` that silently kills supervision
                        // (and breaks the "продолжит автоматически" quota promise on
                        // a cold-restored tab). Un-wedge to `Watching` with NO
                        // penalty; the fire re-engages once the session warms up.
                        if let Some(st) = self.supervisor_states.get_mut(&id) {
                            st.status = crate::supervisor::SupervisorStatus::Watching;
                            st.last_fired_at = None;
                        }
                        self.persist_supervisor_state(id, cx);
                        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
                    }
                }
                continue;
            }

            let Some(session) = self.session(id) else {
                continue;
            };
            let (idle_or_errored, last_activity_ms, has_live_background_work) = {
                let s = session.read(cx);
                let idle_or_errored =
                    matches!(s.state, SessionState::Idle | SessionState::Errored(_));
                // A session sitting idle OVER a background command/agent it
                // launched is legitimately idle — the agent is waiting on that
                // work, so there is nothing for the supervisor to judge. Live =
                // any background shell still `Running`, or any managed agent that
                // has not hit a terminal stop.
                let has_live_background_work =
                    s.background_shells.values().any(|sh| {
                        matches!(
                            sh.state,
                            crate::background_shell::ShellRuntimeState::Running
                        )
                    }) || s.background_agents.values().any(|a| a.is_messageable());
                (
                    idle_or_errored,
                    s.last_activity_at.timestamp_millis(),
                    has_live_background_work,
                )
            };

            // Don't fire the supervisor while a background command/agent is
            // running: the agent's idleness is expected (it's waiting on that
            // work), and hung background work is already watched elsewhere (the
            // background-shell watcher + the `Running`-stuck watchdog). Stay
            // quiet like we do while the user types; the supervisor re-engages
            // once the work finishes and the agent goes genuinely idle. This is
            // what keeps the judge from firing a stream of `wait` verdicts over
            // a session parked on a long build/test.
            if has_live_background_work {
                continue;
            }
            // Treat live human typing as activity: the supervisor's idle clock
            // counts silence from the LATER of the session's last activity and
            // the user's last keystroke, so a nudge never fires while the user
            // is mid-message (note_user_input bumps `last_user_input_ms`).
            let quiet_since_ms = last_activity_ms.max(last_user_input_ms.unwrap_or(0));

            // Inherited idle after a restart is left ALONE until a manual kick.
            // `watch_started_ms` is stamped only on the restart/load path; a
            // session whose last activity predates it was already parked when the
            // editor closed, so the supervisor must NOT auto-resume it on reopen
            // — the operator resumes each session by hand, and only once it
            // produces genuinely-new activity THIS process (a manual kick starts
            // a turn, which bumps `last_activity_at` past the baseline) does the
            // normal idle-nudge cycle re-engage. `None` (a fresh in-session
            // enable) is always eligible — its idle arose under our watch.
            let eligible_for_watch = watch_started_ms
                .is_none_or(|baseline| last_activity_ms > baseline)
                // A session with a SCHEDULED fire (`next_eligible_ms` — a
                // usage-limit auto-resume or a transient-failure backoff) is not
                // plain inherited idle: the schedule is explicit intent. Honor
                // it across a restart, or `watch_started_ms` would gate it out
                // forever and silently break the "observer will auto-continue at
                // HH:MM" promise (the plain-idle "manual kick" rule still applies
                // to sessions with no schedule).
                || next_eligible_ms.is_some();

            // Flush a nudge that was held because the user was typing when the
            // judge finished (`send_supervisor_nudge`'s hold-on-typing). Deliver
            // it once the user has been quiet for the standard idle window and
            // the session is idle — the "user changed their mind and stopped
            // writing" case. A genuine user SEND would already have discarded it
            // via the `from_user` funnel. Never fire a FRESH judge while a nudge
            // is still parked (that would double up), so `continue` regardless.
            let has_pending = self
                .supervisor_states
                .get(&id)
                .is_some_and(|s| s.pending_nudge.is_some());
            if has_pending {
                // The held nudge only applies while we're still actively
                // `Watching`. If the session moved to a paused state since the
                // nudge was parked (user hit Stop → `Held`, supervisor
                // escalated → `WaitingUser`, quota → `Stopped`, disabled), it is
                // stale — DROP it rather than dragging the agent back to work
                // after the user paused it. (Mirrors the wait-wake `Watching`
                // gate below; the pause paths — `hold_supervisor` etc. — don't
                // all clear `pending_nudge` themselves.)
                if !matches!(status, crate::supervisor::SupervisorStatus::Watching) {
                    if let Some(st) = self.supervisor_states.get_mut(&id) {
                        st.pending_nudge = None;
                    }
                    continue;
                }
                let quiet_enough = now_ms.saturating_sub(quiet_since_ms)
                    >= (crate::supervisor::IDLE_THRESHOLD_SECS as i64) * 1000;
                if idle_or_errored && quiet_enough {
                    let pending = self
                        .supervisor_states
                        .get_mut(&id)
                        .and_then(|s| s.pending_nudge.take());
                    if let Some(content) = pending {
                        self.deliver_nudge_now(id, content, cx).detach();
                        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
                    }
                }
                continue;
            }

            // One-shot `wait`: a judge that decided "the agent is waiting on X,
            // park until here" committed a single timeout the mechanism honors
            // in full — no re-judging in between (re-deciding an unchanged wait
            // is the poll we're eliminating). While the deadline is in the
            // future, stay quiet. When it elapses, the mechanism itself wakes
            // the agent (a deterministic "check the result" nudge, only if it's
            // idle — if it already resumed we just drop the wait and let the
            // normal cycle judge the new state). Gated on `Watching` so a stale
            // deadline on a Held/WaitingUser session can't act.
            if matches!(status, crate::supervisor::SupervisorStatus::Watching)
                && let Some(wake_at) = self
                    .supervisor_states
                    .get(&id)
                    .and_then(|s| s.wait_until_ms)
            {
                if now_ms < wake_at {
                    continue;
                }
                if let Some(st) = self.supervisor_states.get_mut(&id) {
                    st.wait_until_ms = None;
                }
                if idle_or_errored {
                    self.deliver_nudge_now(
                        id,
                        "The task you were waiting on should be done by now — \
                         check the result and continue."
                            .to_string(),
                        cx,
                    )
                    .detach();
                }
                self.persist_supervisor_state(id, cx);
                cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
                continue;
            }

            if now_ms >= next_eligible_ms.unwrap_or(0)
                && eligible_for_watch
                && crate::supervisor::should_fire(
                    enabled,
                    &status,
                    idle_or_errored,
                    quiet_since_ms,
                    now_ms,
                    crate::supervisor::IDLE_THRESHOLD_SECS,
                )
            {
                if let Some(st) = self.supervisor_states.get_mut(&id) {
                    st.status = crate::supervisor::SupervisorStatus::Judging;
                    // Fresh judge cycle: clear any stale supersede marker from a
                    // prior reply whose judge never emitted, so this verdict
                    // isn't pre-suppressed (bug #1).
                    st.judge_superseded = false;
                    st.last_fired_at = Some(now_ms);
                    // One more supervisor firing — surfaced next to the status
                    // icon. Reset on enable/disable toggle.
                    st.trigger_count = st.trigger_count.saturating_add(1);
                    // We've consumed the backoff window; clear the gate so a stale
                    // value can't block a later eligible fire.
                    st.next_eligible_ms = None;
                }
                self.backoff_timers.remove(&id);
                self.persist_supervisor_state(id, cx);
                cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
                self.spawn_judge(id, cx);
            }
        }
    }

}
