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

}
