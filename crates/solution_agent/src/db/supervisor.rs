use anyhow::{Result, anyhow};
use gpui::Task;
use indoc::indoc;

use crate::db::SolutionAgentDb;

impl SolutionAgentDb {
    pub fn save_supervisor_state(
        &self,
        state: crate::supervisor::SupervisorState,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            let mut insert = connection.exec_bound::<(
                String,
                i64,
                Option<String>,
                i64,
                i64,
                Option<i64>,
                String,
                Option<i64>,
                i64,
            )>(indoc! {"
                INSERT INTO supervisor_state (
                    session_id, enabled, custom_prompt, consecutive_continues,
                    backoff_attempt, last_fired_at, status, next_eligible_ms, trigger_count
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(session_id) DO UPDATE SET
                    enabled               = excluded.enabled,
                    custom_prompt         = excluded.custom_prompt,
                    consecutive_continues = excluded.consecutive_continues,
                    backoff_attempt       = excluded.backoff_attempt,
                    last_fired_at         = excluded.last_fired_at,
                    status                = excluded.status,
                    next_eligible_ms      = excluded.next_eligible_ms,
                    trigger_count         = excluded.trigger_count
            "})?;
            insert((
                state.session_id.to_string(),
                state.enabled as i64,
                state.custom_prompt.clone(),
                state.consecutive_continues as i64,
                state.backoff_attempt as i64,
                state.last_fired_at,
                state.status.to_db_string(),
                state.next_eligible_ms,
                state.trigger_count as i64,
            ))?;
            Ok(())
        })
    }

    pub fn load_supervisor_states(&self) -> Task<Result<Vec<crate::supervisor::SupervisorState>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            let mut select = connection.select::<(
                String,
                i64,
                Option<String>,
                i64,
                i64,
                Option<i64>,
                String,
                Option<i64>,
                i64,
            )>(indoc! {"
                SELECT session_id, enabled, custom_prompt, consecutive_continues,
                       backoff_attempt, last_fired_at, status, next_eligible_ms, trigger_count
                FROM supervisor_state
            "})?;
            let rows = select()?;
            let mut out = Vec::with_capacity(rows.len());
            for (
                session_id,
                enabled,
                custom_prompt,
                cont,
                backoff,
                last_fired,
                status,
                next_eligible,
                trigger_count,
            ) in rows
            {
                let session_id = crate::model::SolutionSessionId::parse(&session_id)
                    .map_err(|e| anyhow!("invalid session id in supervisor_state: {e}"))?;
                // A judge is in-flight state that lives only in the transient
                // `judge_sessions` map — it never survives a restart. A row
                // persisted mid-`Judging` therefore restores as a PHANTOM: no
                // judge is actually running, so `supersede_judge_on_user_reply`
                // (gated on `judge_sessions`) can't clear it on a user reply and
                // the stuck-watchdog only fires if the persisted `last_fired_at`
                // is already stale — the status row would show "reviewing"
                // indefinitely. Coerce it back to `Watching` (and drop the stale
                // `last_fired_at`) on load so a cold session resumes cleanly.
                let mut status = crate::supervisor::SupervisorStatus::parse_db_string(&status);
                let mut last_fired = last_fired;
                if matches!(status, crate::supervisor::SupervisorStatus::Judging) {
                    status = crate::supervisor::SupervisorStatus::Watching;
                    last_fired = None;
                }
                out.push(crate::supervisor::SupervisorState {
                    session_id,
                    enabled: enabled != 0,
                    custom_prompt,
                    consecutive_continues: cont.max(0) as u32,
                    backoff_attempt: backoff.max(0) as u32,
                    last_fired_at: last_fired,
                    next_eligible_ms: next_eligible,
                    status,
                    trigger_count: trigger_count.max(0) as u32,
                    // Transient (not persisted): a cold-loaded session has no
                    // in-flight draft to protect from a supervisor nudge.
                    last_user_input_ms: None,
                    // Transient: no in-flight judge exists for a cold-loaded row.
                    judge_superseded: false,
                    // Transient: a cold-loaded `Held` row is treated as a manual
                    // stop (won't self-resume) — the conservative default.
                    held_by_done: false,
                    // Transient: a held nudge does not survive a restart.
                    pending_nudge: None,
                    // Transient: a parked wait does not survive a restart.
                    wait_until_ms: None,
                    // Transient: the per-process watch baseline is established
                    // on the first tick, not restored from disk.
                    watch_started_ms: None,
                });
            }
            Ok(out)
        })
    }
}
