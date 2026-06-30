use std::sync::Arc;

use agent_client_protocol::schema as acp;
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use futures::{FutureExt, future::Shared};
use gpui::{App, BackgroundExecutor, Global, SharedString, Task};
use indoc::indoc;
use parking_lot::Mutex;
use solutions::SolutionId;
use sqlez::connection::Connection;

use crate::model::{SolutionSessionId, SolutionSessionMetadata};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundAgentRow {
    pub solution_session_id: String,
    pub agent_id: String,
    pub jsonl_path: String,
    pub registered_at_ms: i64,
    pub last_seen_label: Option<String>,
    pub last_mtime_ms: Option<i64>,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntryRow {
    pub idx: i64,
    pub mod_seq: i64,
    pub created_ms: i64,
    pub subagent_id: Option<String>,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundShellRow {
    pub solution_session_id: String,
    pub shell_id: String,
    pub command: String,
    pub output_path: String,
    pub registered_at_ms: i64,
    pub last_tail: Option<String>,
    pub last_mtime_ms: Option<i64>,
    /// Serialized runtime state: `"running"`, `"exited:N"`, or `"killed"`.
    pub state_text: String,
}

pub struct SolutionAgentDb {
    executor: BackgroundExecutor,
    connection: Arc<Mutex<Connection>>,
}

struct GlobalSolutionAgentDb(Shared<Task<Result<Arc<SolutionAgentDb>, Arc<anyhow::Error>>>>);

impl Global for GlobalSolutionAgentDb {}

impl SolutionAgentDb {
    pub fn connect(cx: &mut App) -> Shared<Task<Result<Arc<SolutionAgentDb>, Arc<anyhow::Error>>>> {
        if cx.has_global::<GlobalSolutionAgentDb>() {
            return cx.global::<GlobalSolutionAgentDb>().0.clone();
        }
        let executor = cx.background_executor().clone();
        let task = executor
            .spawn({
                let executor = executor.clone();
                async move {
                    match Self::open(executor) {
                        Ok(db) => Ok(Arc::new(db)),
                        Err(err) => Err(Arc::new(err)),
                    }
                }
            })
            .shared();
        cx.set_global(GlobalSolutionAgentDb(task.clone()));
        task
    }

    pub fn open(executor: BackgroundExecutor) -> Result<Self> {
        let connection = if cfg!(any(feature = "test-support", test)) {
            let thread = std::thread::current();
            Connection::open_memory(Some(&format!(
                "SOLUTION_AGENT_TEST_{}",
                thread.name().unwrap_or_default()
            )))
        } else {
            let dir = paths::data_dir().join("solution_agent");
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("solution_agent.db");
            Connection::open_file(&path.to_string_lossy())
        };

        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS solution_sessions (
                id                TEXT PRIMARY KEY,
                solution_id       TEXT NOT NULL,
                agent_id          TEXT NOT NULL,
                acp_session_id    TEXT NOT NULL,
                title             TEXT NOT NULL,
                created_at        INTEGER NOT NULL,
                last_activity_at  INTEGER NOT NULL,
                acp_thread_blob   BLOB
            )
        "})?()
        .map_err(|e| anyhow!("Failed to create solution_sessions table: {}", e))?;

        // Idempotent ALTERs for columns added across the project's
        // history. SQLite has no `ADD COLUMN IF NOT EXISTS`, so we
        // detect the already-applied case by matching the
        // "duplicate column name" error and surface every other
        // failure as a `log::warn` instead of swallowing it.
        //
        // Background: the previous shape was
        //
        //   if let Ok(mut run) = connection.exec(ddl) {
        //       let _ = run();
        //   }
        //
        // — which silently dropped two failure modes (prepare-time
        // `Err` and run-time `Err`). At least one user wound up
        // with a DB that had the first four ALTERs applied but
        // `cwd` and `tab_order` still missing, with no breadcrumb
        // explaining why. Subsequent INSERT/SELECT calls
        // referencing those columns failed (no such column), but
        // the failure was several layers away from the migration
        // that should have added them, so the root cause was
        // invisible. Logging the first time a real error happens
        // beats hunting it down via DB forensics later.
        apply_idempotent_add_column(&connection, "preview TEXT");
        apply_idempotent_add_column(&connection, "total_tokens INTEGER");
        apply_idempotent_add_column(&connection, "closed_at INTEGER");
        apply_idempotent_add_column(&connection, "context_count INTEGER NOT NULL DEFAULT 1");
        // `cwd` is the working directory the session was created
        // against. NULL for rows written before this column
        // existed — the resume path falls back to `solution.root`
        // in that case.
        apply_idempotent_add_column(&connection, "cwd TEXT");
        // `tab_order` drives per-solution open-tab strip ordering
        // for the `SolutionSessionsNavigator`. NULL = closed
        // (visible only via History); INTEGER = open at that
        // 0-indexed position. Updated as a batch under
        // `update_tab_orders` whenever the user reorders, opens,
        // or closes a tab.
        apply_idempotent_add_column(&connection, "tab_order INTEGER");
        // F: sub-agent parent reference. NULL = top-level session;
        // non-NULL = child of the referenced session id, surfaced in
        // the session-view "sub-agents" strip. No FK constraint so a
        // dangling pointer (parent later deleted) degrades to "looks
        // like top-level" instead of corrupting the row — the in-
        // memory store + the create_session validation enforce the
        // same-solution constraint at write time.
        apply_idempotent_add_column(&connection, "parent_session_id TEXT");
        // Phase 4: epoch counter — monotonically-increasing generation that the
        // mobile delta-sync protocol uses to detect a full-transcript reset
        // (e.g. context compaction that discards old entries). NULL = pre-phase-4
        // rows; Tasks 2-5 populate it.
        apply_idempotent_add_column(&connection, "epoch INTEGER");
        // Phase 5 Task 5.1b: persist the session's `change_seq` cursor so it is
        // monotonic across desktop restarts. Section bumps advance change_seq
        // above max(mod_seq) WITHOUT creating an entry, and the mobile delta
        // hands the client `current_seq = change_seq` as its `since_seq` cursor;
        // if change_seq reseated to max(mod_seq) on cold load it would drop below
        // a cursor already issued, silently losing new entries. NULL = legacy
        // rows pre-dating this feature (restore falls back to max(mod_seq)).
        apply_idempotent_add_column(&connection, "change_seq INTEGER");
        // Phase 4 Task 3a: persist model/effort/cached_models as columns so they
        // survive the removal of the transcript blob (Task 5). NULL = not set or
        // pre-Task-3a rows; read column-first then blob-fallback in restore_open_tabs.
        apply_idempotent_add_column(&connection, "desired_model TEXT");
        apply_idempotent_add_column(&connection, "desired_effort TEXT");
        apply_idempotent_add_column(&connection, "cached_models TEXT");

        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS solution_session_background_agent (
                solution_session_id TEXT NOT NULL,
                agent_id            TEXT NOT NULL,
                jsonl_path          TEXT NOT NULL,
                registered_at_ms    INTEGER NOT NULL,
                last_seen_label     TEXT,
                last_mtime_ms       INTEGER,
                stop_reason         TEXT,
                PRIMARY KEY (solution_session_id, agent_id)
            )
        "})?()
        .map_err(|e| {
            anyhow!(
                "Failed to create solution_session_background_agent table: {}",
                e
            )
        })?;

        connection.exec(indoc! {"
            CREATE INDEX IF NOT EXISTS idx_bg_agent_by_session
                ON solution_session_background_agent (solution_session_id)
        "})?()
        .map_err(|e| anyhow!("Failed to create idx_bg_agent_by_session: {}", e))?;

        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS solution_session_background_shell (
                solution_session_id TEXT NOT NULL,
                shell_id            TEXT NOT NULL,
                command             TEXT NOT NULL,
                output_path         TEXT NOT NULL,
                registered_at_ms    INTEGER NOT NULL,
                last_tail           TEXT,
                last_mtime_ms       INTEGER,
                state_text          TEXT NOT NULL,
                PRIMARY KEY (solution_session_id, shell_id)
            )
        "})?()
        .map_err(|e| {
            anyhow!(
                "Failed to create solution_session_background_shell table: {}",
                e
            )
        })?;

        connection.exec(indoc! {"
            CREATE INDEX IF NOT EXISTS idx_bg_shell_by_session
                ON solution_session_background_shell (solution_session_id)
        "})?()
        .map_err(|e| anyhow!("Failed to create idx_bg_shell_by_session: {}", e))?;

        // Phase 4: per-entry transcript rows. Each row stores one
        // `SessionEntry` kind as a JSON blob; `mod_seq` drives
        // mobile-delta queries (Tasks 2-5 consume this table).
        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS solution_session_entries (
                session_id   TEXT    NOT NULL,
                idx          INTEGER NOT NULL,
                mod_seq      INTEGER NOT NULL,
                created_ms   INTEGER NOT NULL,
                subagent_id  TEXT,
                payload      BLOB    NOT NULL,
                PRIMARY KEY (session_id, idx)
            )
        "})?()
        .map_err(|e| anyhow!("Failed to create solution_session_entries table: {}", e))?;

        connection.exec(indoc! {"
            CREATE INDEX IF NOT EXISTS idx_session_entry_modseq
                ON solution_session_entries (session_id, mod_seq)
        "})?()
        .map_err(|e| anyhow!("Failed to create idx_session_entry_modseq: {}", e))?;

        connection.exec(indoc! {"
            CREATE INDEX IF NOT EXISTS idx_session_by_solution
                ON solution_sessions (solution_id, last_activity_at DESC)
        "})?()
        .map_err(|e| anyhow!("Failed to create idx_session_by_solution: {}", e))?;

        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS supervisor_state (
                session_id            TEXT PRIMARY KEY,
                enabled               INTEGER NOT NULL,
                custom_prompt         TEXT,
                consecutive_continues INTEGER NOT NULL,
                backoff_attempt       INTEGER NOT NULL,
                last_fired_at         INTEGER,
                status                TEXT NOT NULL,
                next_eligible_ms      INTEGER
            )
        "})?()
        .map_err(|e| anyhow!("Failed to create supervisor_state table: {}", e))?;

        // `next_eligible_ms` gates the watchdog from re-firing a judge
        // during a transient-failure backoff. Added idempotently because the
        // table may already exist (from before this column) in a user DB.
        apply_idempotent_add_column_to(
            &connection,
            "supervisor_state",
            "next_eligible_ms INTEGER",
        );

        // Count of supervisor firings since last (re)enable, surfaced next to
        // the status-row icon. Defaults 0 for pre-existing rows.
        apply_idempotent_add_column_to(
            &connection,
            "supervisor_state",
            "trigger_count INTEGER NOT NULL DEFAULT 0",
        );

        // Per-session chat attachments (images written to the session inbox so
        // the agent can `Read` them mid-turn). Bound to the session AND its
        // solution so they can be cascade-deleted on cleanup (context
        // compaction / clear / session close / solution delete) instead of
        // accumulating on disk for the editor's lifetime. `path` is the
        // absolute inbox file path.
        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS solution_session_attachment (
                session_id    TEXT NOT NULL,
                solution_id   TEXT NOT NULL,
                path          TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                PRIMARY KEY (session_id, path)
            )
        "})?()
        .map_err(|e| anyhow!("Failed to create solution_session_attachment table: {}", e))?;

        connection.exec(indoc! {"
            CREATE INDEX IF NOT EXISTS idx_attachment_by_session
                ON solution_session_attachment (session_id)
        "})?()
        .map_err(|e| anyhow!("Failed to create idx_attachment_by_session: {}", e))?;

        connection.exec(indoc! {"
            CREATE INDEX IF NOT EXISTS idx_attachment_by_solution
                ON solution_session_attachment (solution_id)
        "})?()
        .map_err(|e| anyhow!("Failed to create idx_attachment_by_solution: {}", e))?;

        Ok(Self {
            executor,
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn save_metadata(&self, meta: SolutionSessionMetadata) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            insert_or_update_metadata(&connection, &meta)
        })
    }

    pub fn list_for_solution(
        &self,
        solution_id: SolutionId,
    ) -> Task<Result<Vec<SolutionSessionMetadata>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_metadata_for_solution(&connection, &solution_id)
        })
    }

    pub fn save_blob(&self, id: SolutionSessionId, blob: Vec<u8>) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            update_blob(&connection, id, &blob)
        })
    }

    pub fn load_blob(&self, id: SolutionSessionId) -> Task<Result<Option<Vec<u8>>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_blob(&connection, id)
        })
    }

    pub fn delete(&self, id: SolutionSessionId) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            delete_by_id(&connection, id)
        })
    }

    /// Hard-delete every persisted trace of a session across all six tables
    /// (`solution_sessions`, `solution_session_entries`,
    /// `solution_session_attachment`, `solution_session_background_agent`,
    /// `solution_session_background_shell`, `supervisor_state`) in a single
    /// transaction. Unlike [`mark_closed`], the row is gone for good — used by
    /// the member-removal GC path, where the session's directory no longer
    /// exists so there is nothing to reopen.
    pub fn purge_session(&self, id: SolutionSessionId) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            purge_session_fn(&connection, id)
        })
    }

    pub fn save_background_agent(&self, row: BackgroundAgentRow) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            insert_or_update_background_agent(&connection, &row)
        })
    }

    pub fn load_background_agents(
        &self,
        solution_session_id: String,
    ) -> Task<Result<Vec<BackgroundAgentRow>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_background_agents_for_session(&connection, &solution_session_id)
        })
    }

    pub fn delete_background_agent(
        &self,
        solution_session_id: String,
        agent_id: String,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            delete_background_agent_by_id(&connection, &solution_session_id, &agent_id)
        })
    }

    pub fn save_background_shell(&self, row: BackgroundShellRow) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            insert_or_update_background_shell(&connection, &row)
        })
    }

    /// Load persisted background-shell rows for a session. NOTE: no hydrate /
    /// resume path calls this in production — background shells are ephemeral
    /// (their `/tmp` `.output` files and the claude subprocess both die across
    /// an editor restart), so persisted rows are *deleted* on resume rather
    /// than restored (see `delete_background_shells_for_session`). This loader
    /// exists for round-trip test symmetry with the background-agent table and
    /// as a building block should crash-recovery visibility be wanted later.
    pub fn load_background_shells(
        &self,
        solution_session_id: String,
    ) -> Task<Result<Vec<BackgroundShellRow>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_background_shells_for_session(&connection, &solution_session_id)
        })
    }

    pub fn delete_background_shell(
        &self,
        solution_session_id: String,
        shell_id: String,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            delete_background_shell_by_id(&connection, &solution_session_id, &shell_id)
        })
    }

    pub fn delete_background_shells_for_session(
        &self,
        solution_session_id: String,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            delete_background_shells_for_session(&connection, &solution_session_id)
        })
    }

    pub fn record_attachment(
        &self,
        session_id: String,
        solution_id: String,
        path: String,
        created_at_ms: i64,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            insert_attachment(&connection, &session_id, &solution_id, &path, created_at_ms)
        })
    }

    pub fn attachment_paths_for_session(&self, session_id: String) -> Task<Result<Vec<String>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_attachment_paths_for_session(&connection, &session_id)
        })
    }

    pub fn attachment_paths_for_solution(&self, solution_id: String) -> Task<Result<Vec<String>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_attachment_paths_for_solution(&connection, &solution_id)
        })
    }

    pub fn delete_attachments_for_session(&self, session_id: String) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            delete_attachments_for_session_fn(&connection, &session_id)
        })
    }

    pub fn upsert_entry(
        &self,
        session_id: SolutionSessionId,
        idx: i64,
        mod_seq: i64,
        created_ms: i64,
        subagent_id: Option<String>,
        payload: Vec<u8>,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            insert_or_update_entry(&connection, &session_id.to_string(), idx, mod_seq, created_ms, subagent_id, payload)
        })
    }

    pub fn load_entries(&self, session_id: SolutionSessionId) -> Task<Result<Vec<EntryRow>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_entries_for_session(&connection, &session_id.to_string())
        })
    }

    pub fn delete_entries_from(
        &self,
        session_id: SolutionSessionId,
        from_idx: i64,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            delete_entries_from_idx(&connection, &session_id.to_string(), from_idx)
        })
    }

    pub fn delete_entries_for_session(&self, session_id: SolutionSessionId) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            delete_all_entries_for_session(&connection, &session_id.to_string())
        })
    }

    pub fn save_epoch(&self, session_id: SolutionSessionId, epoch: i64) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            update_epoch(&connection, &session_id.to_string(), epoch)
        })
    }

    pub fn load_epoch(&self, session_id: SolutionSessionId) -> Task<Result<Option<i64>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_epoch(&connection, &session_id.to_string())
        })
    }

    pub fn save_change_seq(
        &self,
        session_id: SolutionSessionId,
        change_seq: i64,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            update_change_seq(&connection, &session_id.to_string(), change_seq)
        })
    }

    pub fn load_change_seq(&self, session_id: SolutionSessionId) -> Task<Result<Option<i64>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_change_seq(&connection, &session_id.to_string())
        })
    }

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

    pub fn load_supervisor_states(
        &self,
    ) -> Task<Result<Vec<crate::supervisor::SupervisorState>>> {
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
                out.push(crate::supervisor::SupervisorState {
                    session_id,
                    enabled: enabled != 0,
                    custom_prompt,
                    consecutive_continues: cont.max(0) as u32,
                    backoff_attempt: backoff.max(0) as u32,
                    last_fired_at: last_fired,
                    next_eligible_ms: next_eligible,
                    status: crate::supervisor::SupervisorStatus::parse_db_string(&status),
                    trigger_count: trigger_count.max(0) as u32,
                    // Transient (not persisted): a cold-loaded session has no
                    // in-flight draft to protect from a supervisor nudge.
                    last_user_input_ms: None,
                    // Transient: no in-flight judge exists for a cold-loaded row.
                    judge_superseded: false,
                });
            }
            Ok(out)
        })
    }

    /// Soft-close: mark the row's `closed_at` so MCP / UI can distinguish
    /// archived sessions from live ones, but keep `acp_thread_blob` so
    /// downstream tooling can still read the conversation transcript
    /// after the user closes the tab.
    ///
    /// Pass `None` to clear the marker (called from resume_session) so a
    /// previously-closed session that the user reopened is reported as
    /// live again.
    pub fn mark_closed(
        &self,
        id: SolutionSessionId,
        closed_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            mark_closed_by_id(&connection, id, closed_at)
        })
    }

    /// Reopen a previously-closed session: clear `closed_at` (it's live
    /// again) AND `tab_order` (detach it from any stale strip slot).
    ///
    /// Clearing `tab_order` is the load-bearing half. [`close_session`]
    /// marks `closed_at` but leaves `tab_order` intact, so a closed row
    /// keeps a dangling strip position. If reopen only cleared `closed_at`,
    /// the row would immediately satisfy [`select_open_tabs`]
    /// (`tab_order IS NOT NULL AND closed_at IS NULL`), so
    /// `hydrate_all_for_solution` would re-stamp the stale `tab_order` onto
    /// the session — and `open_session_in_strip` would then early-return on
    /// its `already_pinned` guard without ever emitting `TabsChanged`,
    /// leaving the user with a reopened-but-invisible tab. Nulling
    /// `tab_order` here forces a fresh pin.
    ///
    /// [`close_session`]: crate::store::SolutionAgentStore::close_session
    pub fn reopen_session(&self, id: SolutionSessionId) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            reopen_session_by_id(&connection, id)
        })
    }

    /// Looks up the closed_at timestamp for `id`. `None` means the row
    /// is live (or doesn't exist — the two are indistinguishable here;
    /// callers that care about distinguishing should also check the
    /// in-memory store).
    pub fn closed_at(
        &self,
        id: SolutionSessionId,
    ) -> Task<Result<Option<chrono::DateTime<chrono::Utc>>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_closed_at(&connection, id)
        })
    }

    pub fn delete_for_solution(&self, solution_id: SolutionId) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            delete_by_solution(&connection, &solution_id)
        })
    }

    /// Sets `tab_order = 0..N` on the sessions in `ordered_ids` for
    /// `solution_id` and clears `tab_order` (sets to NULL) on every
    /// other session belonging to that solution. Run inside a single
    /// transaction so the strip never sees an intermediate split state
    /// (e.g. after a reorder, a History query won't briefly see an
    /// open tab counted as both open and closed).
    pub fn update_tab_orders(
        &self,
        solution_id: SolutionId,
        ordered_ids: Vec<SolutionSessionId>,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            apply_tab_orders(&connection, &solution_id, &ordered_ids)
        })
    }

    /// Returns session ids with `tab_order IS NOT NULL` for
    /// `solution_id`, sorted by `tab_order ASC`. Used by the navigator
    /// at panel-open time to restore the strip without spawning agents.
    pub fn list_open_tabs(&self, solution_id: SolutionId) -> Task<Result<Vec<SolutionSessionId>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_open_tabs(&connection, &solution_id)
        })
    }

    /// Returns ids of every session for `solution_id` whose
    /// `closed_at IS NULL` (i.e. the user hasn't explicitly closed
    /// the session via the desktop's close-tab affordance).
    /// Driven by `hydrate_all_for_solution` so MCP-only consumers
    /// see open-but-not-currently-tabbed sessions WITHOUT also
    /// resurrecting ones the user explicitly archived.
    pub fn list_open_session_ids(
        &self,
        solution_id: SolutionId,
    ) -> Task<Result<Vec<SolutionSessionId>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_open_session_ids(&connection, &solution_id)
        })
    }

    /// Ids of the solution's *closed* sessions — `closed_at IS NOT NULL`,
    /// i.e. the ones the user explicitly closed via the desktop's close-tab
    /// affordance. Drives the "Reopen Closed Chat" picker, which reads each
    /// row's metadata (title / tokens / last activity) to let the user pick
    /// one to bring back.
    pub fn list_closed_session_ids(
        &self,
        solution_id: SolutionId,
    ) -> Task<Result<Vec<SolutionSessionId>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_closed_session_ids(&connection, &solution_id)
        })
    }
}

/// Apply `ALTER TABLE solution_sessions ADD COLUMN <column_def>` and
/// silently swallow only the *expected* "duplicate column name" error
/// (the marker that the column has already been added on an earlier
/// run). Every other error — prepare-time *or* run-time — gets logged
/// at `warn` so a busted migration leaves a breadcrumb instead of
/// silently leaving the schema half-applied.
fn apply_idempotent_add_column(connection: &Connection, column_def: &str) {
    apply_idempotent_add_column_to(connection, "solution_sessions", column_def);
}

/// Generalized form of [apply_idempotent_add_column] that targets an arbitrary
/// `table`. `table` is always an internal constant (never user input), so
/// inlining it into the DDL carries no injection risk.
fn apply_idempotent_add_column_to(connection: &Connection, table: &str, column_def: &str) {
    // Pre-check via `PRAGMA table_info`. The sqlez wrapper surfaces
    // duplicate-column errors as opaque "Prepare call failed for
    // query: …" without the underlying SQLite text, so the old
    // substring filter no longer matched on a re-run — every restart
    // logged one WARN per already-applied migration. Inspecting the
    // catalog up front avoids both the noise AND the prepare attempt.
    let column_name = column_def.split_whitespace().next().unwrap_or(column_def);
    if column_exists(connection, table, column_name) {
        return;
    }
    let ddl = format!("ALTER TABLE {table} ADD COLUMN {column_def}");
    let mut run = match connection.exec(&ddl) {
        Ok(run) => run,
        Err(err) => {
            // `Statement::prepare` doesn't (today) emit a duplicate-
            // column error — that comes from the run step — but
            // future SQLite versions might do early validation, so
            // mirror the run-side filter to stay future-proof.
            let msg = err.to_string();
            if !msg.contains("duplicate column name") {
                log::warn!(
                    target: "solution_agent::db",
                    "migration prepare failed for {column_def:?}: {msg}",
                );
            }
            return;
        }
    };
    if let Err(err) = run() {
        let msg = err.to_string();
        if !msg.contains("duplicate column name") {
            log::warn!(
                target: "solution_agent::db",
                "migration run failed for {column_def:?}: {msg}",
            );
        }
    }
}

/// True if [table] already has a column named [column] (case-
/// insensitive per SQLite catalog rules). Used by
/// [apply_idempotent_add_column] to skip migrations whose DDL would
/// have triggered a duplicate-column SQLite error. Errors from the
/// PRAGMA itself fall back to "assume missing" — the ALTER will then
/// either succeed or hit the substring-filtered duplicate-name path,
/// so the worst case is one harmless log line.
fn column_exists(connection: &Connection, table: &str, column: &str) -> bool {
    // Use the table-valued `pragma_table_info(...)` function (SQLite ≥
    // 3.16) and project `name` explicitly so the single-column
    // `select::<String>` reads the column NAME. The bare
    // `PRAGMA table_info(...)` form yields six columns
    // (cid, name, type, notnull, dflt_value, pk) and `select::<String>`
    // reads column 0 — the integer `cid`, NOT `name` — so the existence
    // check compared cid strings ("0", "1", …) against the column name,
    // never matched, and let every already-applied migration re-run its
    // ALTER and log a spurious "prepare failed" WARN on each restart.
    // `table` is an internal constant, so inlining it as a quoted
    // literal carries no injection risk.
    let ddl = format!("SELECT name FROM pragma_table_info('{table}')");
    let prepared = connection.select::<String>(&ddl);
    let mut selector = match prepared {
        Ok(s) => s,
        Err(_) => return false,
    };
    let rows = match selector() {
        Ok(r) => r,
        Err(_) => return false,
    };
    rows.iter().any(|name| name.eq_ignore_ascii_case(column))
}

fn insert_or_update_metadata(
    connection: &Connection,
    meta: &SolutionSessionMetadata,
) -> Result<()> {
    // `preview`, `total_tokens`, `cwd`, `parent_session_id`, `desired_model`,
    // `desired_effort`, `cached_models`, and `tab_order` use COALESCE so a
    // metadata write that doesn't carry a value (e.g. a fresh-session insert at
    // create time) doesn't clobber values an event-driven update wrote earlier
    // in the same session.
    //
    // `tab_order` in particular guards a lost-update race at create time:
    // `create_session_with_parent` issues this metadata INSERT and a separate
    // `update_tab_orders` UPDATE with no happens-before, both contending on one
    // `Arc<Mutex<Connection>>`. If the UPDATE wins and runs FIRST it no-ops (no
    // row yet); without COALESCE this INSERT would then create the row with
    // `tab_order = NULL`, so `select_open_tabs` (and the restore-on-restart
    // path) never re-hydrates the session -> "unknown session" on send. The
    // existing-row column is qualified as `solution_sessions.tab_order` so
    // SQLite resolves the conflict-target row rather than the bare name.
    //
    // Nested tuple shape because `sqlez::Bind` only implements tuples up to
    // size 10; we have 16 columns now (5 + 7 + 4).
    let mut insert = connection.exec_bound::<(
        (String, String, String, Arc<str>, String),
        (
            i64,
            i64,
            Option<String>,
            Option<i64>,
            i64,
            Option<String>,
            Option<String>,
        ),
        (Option<String>, Option<String>, Option<String>, Option<i64>),
    )>(indoc! {"
        INSERT INTO solution_sessions (
            id, solution_id, agent_id, acp_session_id, title,
            created_at, last_activity_at, preview, total_tokens,
            context_count, cwd, parent_session_id,
            desired_model, desired_effort, cached_models, tab_order
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
        ON CONFLICT(id) DO UPDATE SET
            solution_id        = excluded.solution_id,
            agent_id           = excluded.agent_id,
            acp_session_id     = excluded.acp_session_id,
            title              = excluded.title,
            created_at         = excluded.created_at,
            last_activity_at   = excluded.last_activity_at,
            preview            = COALESCE(excluded.preview, preview),
            total_tokens       = COALESCE(excluded.total_tokens, total_tokens),
            context_count      = excluded.context_count,
            cwd                = COALESCE(excluded.cwd, cwd),
            parent_session_id  = COALESCE(excluded.parent_session_id, parent_session_id),
            desired_model      = COALESCE(excluded.desired_model, desired_model),
            desired_effort     = COALESCE(excluded.desired_effort, desired_effort),
            cached_models      = COALESCE(excluded.cached_models, cached_models),
            tab_order          = COALESCE(excluded.tab_order, solution_sessions.tab_order)
    "})?;

    let cwd_str = if meta.cwd.as_os_str().is_empty() {
        None
    } else {
        Some(meta.cwd.to_string_lossy().into_owned())
    };
    // Serialize cached_models as None when empty so COALESCE preserves the
    // last non-empty list on a write that doesn't carry a fresh model list.
    let cached_models_json = if meta.cached_models.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&meta.cached_models)?)
    };
    insert((
        (
            meta.id.to_string(),
            meta.solution_id.0.clone(),
            meta.agent_id.to_string(),
            meta.acp_session_id.0.clone(),
            meta.title.to_string(),
        ),
        (
            meta.created_at.timestamp_millis(),
            meta.last_activity_at.timestamp_millis(),
            meta.preview.as_ref().map(|s| s.to_string()),
            meta.total_tokens.map(|t| t as i64),
            meta.context_count as i64,
            cwd_str,
            meta.parent_session_id.map(|id| id.to_string()),
        ),
        (
            meta.desired_model.clone(),
            meta.desired_effort.clone(),
            cached_models_json,
            meta.tab_order,
        ),
    ))?;

    Ok(())
}

fn update_blob(connection: &Connection, id: SolutionSessionId, blob: &[u8]) -> Result<()> {
    let mut update = connection.exec_bound::<(Vec<u8>, String)>(indoc! {"
        UPDATE solution_sessions SET acp_thread_blob = ?1 WHERE id = ?2
    "})?;
    update((blob.to_vec(), id.to_string()))?;
    Ok(())
}

fn select_blob(connection: &Connection, id: SolutionSessionId) -> Result<Option<Vec<u8>>> {
    let mut select = connection.select_bound::<String, Option<Vec<u8>>>(indoc! {"
        SELECT acp_thread_blob FROM solution_sessions WHERE id = ? LIMIT 1
    "})?;
    let rows = select(id.to_string())?;
    Ok(rows.into_iter().next().flatten())
}

fn mark_closed_by_id(
    connection: &Connection,
    id: SolutionSessionId,
    closed_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<()> {
    let mut update = connection.exec_bound::<(Option<i64>, String)>(indoc! {"
        UPDATE solution_sessions SET closed_at = ?1 WHERE id = ?2
    "})?;
    update((closed_at.map(|ts| ts.timestamp_millis()), id.to_string()))?;
    Ok(())
}

fn reopen_session_by_id(connection: &Connection, id: SolutionSessionId) -> Result<()> {
    let mut update = connection.exec_bound::<String>(indoc! {"
        UPDATE solution_sessions SET closed_at = NULL, tab_order = NULL WHERE id = ?
    "})?;
    update(id.to_string())?;
    Ok(())
}

fn select_closed_at(
    connection: &Connection,
    id: SolutionSessionId,
) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    let mut select = connection.select_bound::<String, Option<i64>>(indoc! {"
        SELECT closed_at FROM solution_sessions WHERE id = ? LIMIT 1
    "})?;
    let rows = select(id.to_string())?;
    Ok(rows
        .into_iter()
        .next()
        .flatten()
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis))
}

fn delete_by_id(connection: &Connection, id: SolutionSessionId) -> Result<()> {
    let mut delete = connection.exec_bound::<String>(indoc! {"
        DELETE FROM solution_sessions WHERE id = ?
    "})?;
    delete(id.to_string())?;
    Ok(())
}

fn purge_session_fn(connection: &Connection, id: SolutionSessionId) -> Result<()> {
    let id = id.to_string();
    // One savepoint so a partial failure can't leave the session half-deleted
    // (e.g. the row gone but its attachment/supervisor rows still dangling).
    // The two background tables key on `solution_session_id`; the rest key on
    // `session_id` — both verified against the CREATE TABLE statements above.
    let tx = connection.with_savepoint("purge_session", || {
        for sql in [
            "DELETE FROM solution_sessions WHERE id = ?",
            "DELETE FROM solution_session_entries WHERE session_id = ?",
            "DELETE FROM solution_session_attachment WHERE session_id = ?",
            "DELETE FROM solution_session_background_agent WHERE solution_session_id = ?",
            "DELETE FROM solution_session_background_shell WHERE solution_session_id = ?",
            "DELETE FROM supervisor_state WHERE session_id = ?",
        ] {
            let mut stmt = connection.exec_bound::<String>(sql)?;
            stmt(id.clone())?;
        }
        Ok(())
    });
    tx.map_err(|e| anyhow!("purge_session failed: {e}"))
}

fn insert_or_update_background_agent(
    connection: &Connection,
    row: &BackgroundAgentRow,
) -> Result<()> {
    let mut stmt = connection.exec_bound::<(
        String,
        String,
        String,
        i64,
        Option<String>,
        Option<i64>,
        Option<String>,
    )>(indoc! {"
        INSERT INTO solution_session_background_agent
            (solution_session_id, agent_id, jsonl_path, registered_at_ms,
             last_seen_label, last_mtime_ms, stop_reason)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(solution_session_id, agent_id) DO UPDATE SET
            jsonl_path       = excluded.jsonl_path,
            last_seen_label  = excluded.last_seen_label,
            last_mtime_ms    = excluded.last_mtime_ms,
            stop_reason      = excluded.stop_reason
    "})?;
    stmt((
        row.solution_session_id.clone(),
        row.agent_id.clone(),
        row.jsonl_path.clone(),
        row.registered_at_ms,
        row.last_seen_label.clone(),
        row.last_mtime_ms,
        row.stop_reason.clone(),
    ))?;
    Ok(())
}

fn select_background_agents_for_session(
    connection: &Connection,
    solution_session_id: &str,
) -> Result<Vec<BackgroundAgentRow>> {
    let mut stmt = connection.select_bound::<String, (
        String,
        String,
        String,
        i64,
        Option<String>,
        Option<i64>,
        Option<String>,
    )>(indoc! {"
        SELECT solution_session_id, agent_id, jsonl_path,
               registered_at_ms, last_seen_label,
               last_mtime_ms, stop_reason
        FROM   solution_session_background_agent
        WHERE  solution_session_id = ?
    "})?;
    let rows = stmt(solution_session_id.to_string())?;
    Ok(rows
        .into_iter()
        .map(|(sid, aid, p, r, l, m, sr)| BackgroundAgentRow {
            solution_session_id: sid,
            agent_id: aid,
            jsonl_path: p,
            registered_at_ms: r,
            last_seen_label: l,
            last_mtime_ms: m,
            stop_reason: sr,
        })
        .collect())
}

fn delete_background_agent_by_id(
    connection: &Connection,
    solution_session_id: &str,
    agent_id: &str,
) -> Result<()> {
    let mut stmt = connection.exec_bound::<(String, String)>(indoc! {"
        DELETE FROM solution_session_background_agent
        WHERE solution_session_id = ? AND agent_id = ?
    "})?;
    stmt((solution_session_id.to_string(), agent_id.to_string()))?;
    Ok(())
}

fn insert_or_update_background_shell(
    connection: &Connection,
    row: &BackgroundShellRow,
) -> Result<()> {
    let mut stmt = connection.exec_bound::<(
        String,
        String,
        String,
        String,
        i64,
        Option<String>,
        Option<i64>,
        String,
    )>(indoc! {"
        INSERT INTO solution_session_background_shell
            (solution_session_id, shell_id, command, output_path, registered_at_ms,
             last_tail, last_mtime_ms, state_text)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(solution_session_id, shell_id) DO UPDATE SET
            command          = excluded.command,
            output_path      = excluded.output_path,
            last_tail        = excluded.last_tail,
            last_mtime_ms    = excluded.last_mtime_ms,
            state_text       = excluded.state_text
    "})?;
    stmt((
        row.solution_session_id.clone(),
        row.shell_id.clone(),
        row.command.clone(),
        row.output_path.clone(),
        row.registered_at_ms,
        row.last_tail.clone(),
        row.last_mtime_ms,
        row.state_text.clone(),
    ))?;
    Ok(())
}

fn select_background_shells_for_session(
    connection: &Connection,
    solution_session_id: &str,
) -> Result<Vec<BackgroundShellRow>> {
    let mut stmt = connection.select_bound::<String, (
        String,
        String,
        String,
        String,
        i64,
        Option<String>,
        Option<i64>,
        String,
    )>(indoc! {"
        SELECT solution_session_id, shell_id, command, output_path,
               registered_at_ms, last_tail, last_mtime_ms, state_text
        FROM   solution_session_background_shell
        WHERE  solution_session_id = ?
    "})?;
    let rows = stmt(solution_session_id.to_string())?;
    Ok(rows
        .into_iter()
        .map(
            |(sid, shell_id, command, output_path, r, lt, m, st)| BackgroundShellRow {
                solution_session_id: sid,
                shell_id,
                command,
                output_path,
                registered_at_ms: r,
                last_tail: lt,
                last_mtime_ms: m,
                state_text: st,
            },
        )
        .collect())
}

fn delete_background_shell_by_id(
    connection: &Connection,
    solution_session_id: &str,
    shell_id: &str,
) -> Result<()> {
    let mut stmt = connection.exec_bound::<(String, String)>(indoc! {"
        DELETE FROM solution_session_background_shell
        WHERE solution_session_id = ? AND shell_id = ?
    "})?;
    stmt((solution_session_id.to_string(), shell_id.to_string()))?;
    Ok(())
}

fn delete_background_shells_for_session(
    connection: &Connection,
    solution_session_id: &str,
) -> Result<()> {
    let mut stmt = connection.exec_bound::<String>(indoc! {"
        DELETE FROM solution_session_background_shell
        WHERE solution_session_id = ?
    "})?;
    stmt(solution_session_id.to_string())?;
    Ok(())
}

fn insert_attachment(
    connection: &Connection,
    session_id: &str,
    solution_id: &str,
    path: &str,
    created_at_ms: i64,
) -> Result<()> {
    let mut stmt = connection.exec_bound::<(String, String, String, i64)>(indoc! {"
        INSERT INTO solution_session_attachment
            (session_id, solution_id, path, created_at_ms)
        VALUES (?, ?, ?, ?)
        ON CONFLICT(session_id, path) DO UPDATE SET
            solution_id   = excluded.solution_id,
            created_at_ms = excluded.created_at_ms
    "})?;
    stmt((
        session_id.to_string(),
        solution_id.to_string(),
        path.to_string(),
        created_at_ms,
    ))?;
    Ok(())
}

fn select_attachment_paths_for_session(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<String>> {
    let mut stmt = connection.select_bound::<String, String>(indoc! {"
        SELECT path FROM solution_session_attachment WHERE session_id = ?
    "})?;
    stmt(session_id.to_string())
}

fn select_attachment_paths_for_solution(
    connection: &Connection,
    solution_id: &str,
) -> Result<Vec<String>> {
    let mut stmt = connection.select_bound::<String, String>(indoc! {"
        SELECT path FROM solution_session_attachment WHERE solution_id = ?
    "})?;
    stmt(solution_id.to_string())
}

fn delete_attachments_for_session_fn(connection: &Connection, session_id: &str) -> Result<()> {
    let mut stmt = connection.exec_bound::<String>(indoc! {"
        DELETE FROM solution_session_attachment WHERE session_id = ?
    "})?;
    stmt(session_id.to_string())?;
    Ok(())
}

fn delete_attachments_for_solution_fn(connection: &Connection, solution_id: &str) -> Result<()> {
    let mut stmt = connection.exec_bound::<String>(indoc! {"
        DELETE FROM solution_session_attachment WHERE solution_id = ?
    "})?;
    stmt(solution_id.to_string())?;
    Ok(())
}

fn insert_or_update_entry(
    connection: &Connection,
    session_id: &str,
    idx: i64,
    mod_seq: i64,
    created_ms: i64,
    subagent_id: Option<String>,
    payload: Vec<u8>,
) -> Result<()> {
    let mut stmt = connection.exec_bound::<(String, i64, i64, i64, Option<String>, Vec<u8>)>(indoc! {"
        INSERT INTO solution_session_entries
            (session_id, idx, mod_seq, created_ms, subagent_id, payload)
        VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(session_id, idx) DO UPDATE SET
            mod_seq     = excluded.mod_seq,
            created_ms  = excluded.created_ms,
            subagent_id = excluded.subagent_id,
            payload     = excluded.payload
    "})?;
    stmt((session_id.to_string(), idx, mod_seq, created_ms, subagent_id, payload))?;
    Ok(())
}

fn select_entries_for_session(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<EntryRow>> {
    let mut stmt = connection.select_bound::<String, (i64, i64, i64, Option<String>, Vec<u8>)>(indoc! {"
        SELECT idx, mod_seq, created_ms, subagent_id, payload
        FROM   solution_session_entries
        WHERE  session_id = ?
        ORDER BY idx ASC
    "})?;
    let rows = stmt(session_id.to_string())?;
    Ok(rows
        .into_iter()
        .map(|(idx, mod_seq, created_ms, subagent_id, payload)| EntryRow {
            idx,
            mod_seq,
            created_ms,
            subagent_id,
            payload,
        })
        .collect())
}

fn delete_entries_from_idx(
    connection: &Connection,
    session_id: &str,
    from_idx: i64,
) -> Result<()> {
    let mut stmt = connection.exec_bound::<(String, i64)>(indoc! {"
        DELETE FROM solution_session_entries
        WHERE session_id = ? AND idx >= ?
    "})?;
    stmt((session_id.to_string(), from_idx))?;
    Ok(())
}

fn delete_all_entries_for_session(connection: &Connection, session_id: &str) -> Result<()> {
    let mut stmt = connection.exec_bound::<String>(indoc! {"
        DELETE FROM solution_session_entries
        WHERE session_id = ?
    "})?;
    stmt(session_id.to_string())?;
    Ok(())
}

fn update_epoch(connection: &Connection, session_id: &str, epoch: i64) -> Result<()> {
    let mut stmt = connection.exec_bound::<(i64, String)>(indoc! {"
        UPDATE solution_sessions SET epoch = ?1 WHERE id = ?2
    "})?;
    stmt((epoch, session_id.to_string()))?;
    Ok(())
}

fn select_epoch(connection: &Connection, session_id: &str) -> Result<Option<i64>> {
    let mut stmt = connection.select_bound::<String, Option<i64>>(indoc! {"
        SELECT epoch FROM solution_sessions WHERE id = ? LIMIT 1
    "})?;
    let rows = stmt(session_id.to_string())?;
    Ok(rows.into_iter().next().flatten())
}

fn update_change_seq(connection: &Connection, session_id: &str, change_seq: i64) -> Result<()> {
    // `max(COALESCE(...), ?1)` keeps the durable value monotonic regardless of
    // the order detached background writes land in: a lower write can never
    // overwrite a higher one. This protects the Task 5.1b invariant (durable
    // change_seq >= every issued cursor) against write reordering. COALESCE is
    // required because SQLite's scalar `max(X, Y)` returns NULL if either arg is
    // NULL (the legacy / first-write case).
    let mut stmt = connection.exec_bound::<(i64, String)>(indoc! {"
        UPDATE solution_sessions
        SET change_seq = max(COALESCE(change_seq, 0), ?1)
        WHERE id = ?2
    "})?;
    stmt((change_seq, session_id.to_string()))?;
    Ok(())
}

fn select_change_seq(connection: &Connection, session_id: &str) -> Result<Option<i64>> {
    let mut stmt = connection.select_bound::<String, Option<i64>>(indoc! {"
        SELECT change_seq FROM solution_sessions WHERE id = ? LIMIT 1
    "})?;
    let rows = stmt(session_id.to_string())?;
    Ok(rows.into_iter().next().flatten())
}

fn delete_by_solution(connection: &Connection, solution_id: &SolutionId) -> Result<()> {
    let mut delete = connection.exec_bound::<String>(indoc! {"
        DELETE FROM solution_sessions WHERE solution_id = ?
    "})?;
    delete(solution_id.0.clone())?;
    // Cascade: drop attachment rows for the solution (callers delete the files
    // themselves via `attachment_paths_for_solution` before this, while the
    // paths are still queryable).
    delete_attachments_for_solution_fn(connection, &solution_id.0)?;
    Ok(())
}

fn apply_tab_orders(
    connection: &Connection,
    solution_id: &SolutionId,
    ordered_ids: &[SolutionSessionId],
) -> Result<()> {
    // Single transaction: clear all tab_order for the solution, then
    // set the new positions in order. Two-step (instead of one
    // `CASE WHEN id IN (…) THEN …`) so the SQL stays trivial and we
    // don't have to bind a variable-length IN list with sqlez.
    let tx = connection.with_savepoint("apply_tab_orders", || {
        let mut clear = connection.exec_bound::<String>(
            "UPDATE solution_sessions SET tab_order = NULL WHERE solution_id = ?",
        )?;
        clear(solution_id.0.clone())?;
        let mut set = connection.exec_bound::<(i64, String, String)>(
            "UPDATE solution_sessions SET tab_order = ?1 WHERE id = ?2 AND solution_id = ?3",
        )?;
        for (idx, id) in ordered_ids.iter().enumerate() {
            set((idx as i64, id.to_string(), solution_id.0.clone()))?;
        }
        Ok(())
    });
    tx.map_err(|e| anyhow!("apply_tab_orders failed: {e}"))
}

fn select_open_tabs(
    connection: &Connection,
    solution_id: &SolutionId,
) -> Result<Vec<SolutionSessionId>> {
    // `closed_at IS NULL` filters out soft-closed sessions: when the
    // user closes a tab via `close_session`, we keep the row (and its
    // `tab_order`) so the persisted transcript stays readable, but the
    // restore-on-open path must not re-hydrate it as a live tab — the
    // user closed it, they expect it to stay closed across restarts.
    let mut select = connection.select_bound::<String, String>(indoc! {"
        SELECT id FROM solution_sessions
        WHERE solution_id = ? AND tab_order IS NOT NULL AND closed_at IS NULL
        ORDER BY tab_order ASC
    "})?;
    let rows = select(solution_id.0.clone())?;
    let mut out = Vec::with_capacity(rows.len());
    for id in rows {
        let parsed = SolutionSessionId::parse(&id)
            .map_err(|e| anyhow!("invalid SolutionSessionId in tab_order row: {e}"))?;
        out.push(parsed);
    }
    Ok(out)
}

/// Sibling of [`select_open_tabs`] for the MCP-driven phone hydration
/// path. Drops the `tab_order IS NOT NULL` requirement so closed-tab-
/// but-not-explicitly-closed sessions surface, while still excluding
/// rows whose `closed_at` is set (those were soft-deleted on close_session
/// and re-hydrating them would resurrect the conversation the user
/// just dismissed).
fn select_open_session_ids(
    connection: &Connection,
    solution_id: &SolutionId,
) -> Result<Vec<SolutionSessionId>> {
    let mut select = connection.select_bound::<String, String>(indoc! {"
        SELECT id FROM solution_sessions
        WHERE solution_id = ? AND closed_at IS NULL
    "})?;
    let rows = select(solution_id.0.clone())?;
    let mut out = Vec::with_capacity(rows.len());
    for id in rows {
        let parsed = SolutionSessionId::parse(&id)
            .map_err(|e| anyhow!("invalid SolutionSessionId in open-session row: {e}"))?;
        out.push(parsed);
    }
    Ok(out)
}

/// Sibling of [`select_open_session_ids`] for the reopen picker: the
/// explicitly-closed sessions (`closed_at IS NOT NULL`).
fn select_closed_session_ids(
    connection: &Connection,
    solution_id: &SolutionId,
) -> Result<Vec<SolutionSessionId>> {
    let mut select = connection.select_bound::<String, String>(indoc! {"
        SELECT id FROM solution_sessions
        WHERE solution_id = ? AND closed_at IS NOT NULL
    "})?;
    let rows = select(solution_id.0.clone())?;
    let mut out = Vec::with_capacity(rows.len());
    for id in rows {
        let parsed = SolutionSessionId::parse(&id)
            .map_err(|e| anyhow!("invalid SolutionSessionId in closed-session row: {e}"))?;
        out.push(parsed);
    }
    Ok(out)
}

fn select_metadata_for_solution(
    connection: &Connection,
    solution_id: &SolutionId,
) -> Result<Vec<SolutionSessionMetadata>> {
    // Same nested-tuple shape as the INSERT side — `sqlez::Column` only
    // implements tuples up to size 10; we have 16 columns now (5 + 7 + 4).
    let mut select = connection.select_bound::<String, (
        (String, String, String, Arc<str>, String),
        (
            i64,
            i64,
            Option<String>,
            Option<i64>,
            i64,
            Option<String>,
            Option<String>,
        ),
        (Option<String>, Option<String>, Option<String>, Option<i64>),
    )>(indoc! {"
        SELECT id, solution_id, agent_id, acp_session_id, title,
               created_at, last_activity_at, preview, total_tokens,
               context_count, cwd, parent_session_id,
               desired_model, desired_effort, cached_models, tab_order
        FROM solution_sessions
        WHERE solution_id = ?
        ORDER BY last_activity_at DESC
    "})?;

    let rows = select(solution_id.0.clone())?;
    let mut out = Vec::with_capacity(rows.len());
    for (
        (id, solution_id, agent_id, acp_session_id, title),
        (
            created_at,
            last_activity_at,
            preview,
            total_tokens,
            context_count,
            cwd,
            parent_session_id,
        ),
        (desired_model, desired_effort, cached_models_json, tab_order),
    ) in rows
    {
        let id = SolutionSessionId::parse(&id)
            .map_err(|e| anyhow!("invalid SolutionSessionId in db: {e}"))?;
        let created_at = DateTime::<Utc>::from_timestamp_millis(created_at)
            .ok_or_else(|| anyhow!("invalid created_at timestamp: {created_at}"))?;
        let last_activity_at = DateTime::<Utc>::from_timestamp_millis(last_activity_at)
            .ok_or_else(|| anyhow!("invalid last_activity_at timestamp: {last_activity_at}"))?;
        // A dangling `parent_session_id` (parent later deleted) parses
        // fine here — the dangling pointer is silently treated as
        // top-level by the surfaces that read it (status row, MCP).
        let parent_session_id = match parent_session_id {
            Some(raw) => Some(
                SolutionSessionId::parse(&raw)
                    .map_err(|e| anyhow!("invalid parent_session_id in db: {e}"))?,
            ),
            None => None,
        };
        // Parse cached_models tolerantly: a corrupt JSON cell logs a warning
        // and falls back to empty rather than failing the whole listing.
        let cached_models: Vec<claude_native::ModelInfo> = cached_models_json
            .and_then(|s| match serde_json::from_str(&s) {
                Ok(v) => Some(v),
                Err(e) => {
                    log::warn!("invalid cached_models json for {id}: {e}");
                    None
                }
            })
            .unwrap_or_default();

        out.push(SolutionSessionMetadata {
            id,
            solution_id: SolutionId(solution_id),
            agent_id: SharedString::from(agent_id),
            acp_session_id: acp::SessionId::new(acp_session_id),
            title: SharedString::from(title),
            created_at,
            last_activity_at,
            preview: preview.map(SharedString::from),
            total_tokens: total_tokens.map(|t| t as u64),
            context_count: context_count.max(1) as u32,
            cwd: cwd.map(std::path::PathBuf::from).unwrap_or_default(),
            parent_session_id,
            desired_model,
            desired_effort,
            cached_models,
            tab_order,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[gpui::test]
    async fn supervisor_state_roundtrips(cx: &mut gpui::TestAppContext) {
        use crate::supervisor::{StoppedReason, SupervisorState, SupervisorStatus};
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let id = crate::model::SolutionSessionId::parse("zzzz9999").unwrap();
        let mut st = SupervisorState::new(id);
        st.enabled = true;
        st.custom_prompt = Some("don't stop before tests pass".into());
        st.consecutive_continues = 3;
        st.next_eligible_ms = Some(1_700_000_000_000);
        st.status = SupervisorStatus::Watching;
        db.save_supervisor_state(st.clone()).await.unwrap();

        // overwrite (upsert)
        st.consecutive_continues = 4;
        st.next_eligible_ms = Some(1_700_000_999_000);
        st.status = SupervisorStatus::Stopped(StoppedReason::Quota);
        db.save_supervisor_state(st).await.unwrap();

        let all = db.load_supervisor_states().await.unwrap();
        let got = all.iter().find(|s| s.session_id == id).unwrap();
        assert!(got.enabled);
        assert_eq!(got.consecutive_continues, 4);
        assert_eq!(got.custom_prompt.as_deref(), Some("don't stop before tests pass"));
        assert_eq!(got.status, SupervisorStatus::Stopped(StoppedReason::Quota));
        assert_eq!(got.next_eligible_ms, Some(1_700_000_999_000));
    }

    fn make_meta(seq: u32, sol: &str) -> SolutionSessionMetadata {
        SolutionSessionMetadata {
            id: SolutionSessionId::new(),
            solution_id: SolutionId(sol.into()),
            agent_id: SharedString::from("claude-acp"),
            acp_session_id: acp::SessionId::new(format!("acp-{seq}")),
            title: SharedString::from(format!("session {seq}")),
            created_at: Utc
                .timestamp_millis_opt(1_700_000_000_000 + seq as i64 * 1000)
                .unwrap(),
            last_activity_at: Utc
                .timestamp_millis_opt(1_700_000_000_000 + seq as i64 * 1000)
                .unwrap(),
            preview: None,
            total_tokens: None,
            context_count: 1,
            cwd: std::path::PathBuf::new(),
            parent_session_id: None,
            desired_model: None,
            desired_effort: None,
            cached_models: vec![],
            tab_order: None,
        }
    }

    #[gpui::test]
    async fn save_then_list_returns_inserted_rows(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        db.save_metadata(make_meta(1, "sol-a")).await.unwrap();
        db.save_metadata(make_meta(2, "sol-a")).await.unwrap();
        db.save_metadata(make_meta(3, "sol-b")).await.unwrap();

        let in_a = db
            .list_for_solution(SolutionId("sol-a".into()))
            .await
            .unwrap();
        assert_eq!(in_a.len(), 2);
        let in_b = db
            .list_for_solution(SolutionId("sol-b".into()))
            .await
            .unwrap();
        assert_eq!(in_b.len(), 1);
        let in_c = db
            .list_for_solution(SolutionId("sol-c".into()))
            .await
            .unwrap();
        assert_eq!(in_c.len(), 0);
    }

    #[gpui::test]
    async fn cwd_roundtrips_through_save_and_list(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let mut with_cwd = make_meta(1, "sol-a");
        with_cwd.cwd = std::path::PathBuf::from("/tmp/sol-a/member-x");
        let without_cwd = make_meta(2, "sol-a"); // empty PathBuf — legacy row

        db.save_metadata(with_cwd.clone()).await.unwrap();
        db.save_metadata(without_cwd.clone()).await.unwrap();

        let listed = db
            .list_for_solution(SolutionId("sol-a".into()))
            .await
            .unwrap();
        let by_id = |id| listed.iter().find(|m| m.id == id).expect("row present");
        assert_eq!(by_id(with_cwd.id).cwd, with_cwd.cwd);
        assert_eq!(by_id(without_cwd.id).cwd, std::path::PathBuf::new());
    }

    #[gpui::test]
    async fn save_blob_then_load_roundtrips(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let meta = make_meta(1, "sol-a");
        db.save_metadata(meta.clone()).await.unwrap();
        let blob = b"\x01\x02\x03 example payload".to_vec();
        db.save_blob(meta.id, blob.clone()).await.unwrap();

        let loaded = db.load_blob(meta.id).await.unwrap();
        assert_eq!(loaded, Some(blob));
    }

    #[gpui::test]
    async fn delete_removes_row(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        let meta = make_meta(1, "sol-a");
        db.save_metadata(meta.clone()).await.unwrap();

        db.delete(meta.id).await.unwrap();

        let listing = db
            .list_for_solution(meta.solution_id.clone())
            .await
            .unwrap();
        assert!(listing.is_empty());
    }

    #[gpui::test]
    async fn tab_order_roundtrips_per_solution(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let m1 = make_meta(1, "sol-a");
        let m2 = make_meta(2, "sol-a");
        let m3 = make_meta(3, "sol-a");
        let other = make_meta(4, "sol-b");
        for m in [&m1, &m2, &m3, &other] {
            db.save_metadata(m.clone()).await.unwrap();
        }

        db.update_tab_orders(SolutionId("sol-a".into()), vec![m2.id, m3.id, m1.id])
            .await
            .unwrap();
        db.update_tab_orders(SolutionId("sol-b".into()), vec![other.id])
            .await
            .unwrap();

        let in_a = db.list_open_tabs(SolutionId("sol-a".into())).await.unwrap();
        assert_eq!(in_a, vec![m2.id, m3.id, m1.id]);
        let in_b = db.list_open_tabs(SolutionId("sol-b".into())).await.unwrap();
        assert_eq!(in_b, vec![other.id]);
    }

    #[gpui::test]
    async fn update_tab_orders_clears_omitted_sessions(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let m1 = make_meta(1, "sol-a");
        let m2 = make_meta(2, "sol-a");
        let m3 = make_meta(3, "sol-a");
        for m in [&m1, &m2, &m3] {
            db.save_metadata(m.clone()).await.unwrap();
        }

        db.update_tab_orders(SolutionId("sol-a".into()), vec![m1.id, m2.id, m3.id])
            .await
            .unwrap();
        assert_eq!(
            db.list_open_tabs(SolutionId("sol-a".into())).await.unwrap(),
            vec![m1.id, m2.id, m3.id]
        );

        db.update_tab_orders(SolutionId("sol-a".into()), vec![m2.id])
            .await
            .unwrap();
        assert_eq!(
            db.list_open_tabs(SolutionId("sol-a".into())).await.unwrap(),
            vec![m2.id]
        );
    }

    #[gpui::test]
    async fn tab_order_survives_update_before_insert(cx: &mut gpui::TestAppContext) {
        // Lost-update race at create time: `create_session_with_parent` writes
        // the metadata row (`save_metadata`) and the strip position
        // (`update_tab_orders`) as two independent detached DB writes with no
        // happens-before. `update_tab_orders` is an UPDATE-only path, so if it
        // wins the race it no-ops (the metadata row doesn't exist yet) — the
        // strip position can only survive if the metadata write itself carries
        // the tab_order. The store fixes this by re-persisting the row AFTER
        // pinning, so the `save_metadata` here carries `Some(0)`.
        //
        // This test exercises the DB contract that makes that durable: a
        // metadata INSERT carrying a concrete tab_order lands it, and the
        // outcome does not depend on whether the bare UPDATE ran first or never
        // matched a row.
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let mut m = make_meta(1, "sol-a");

        // UPDATE first against a non-existent row: a genuine no-op, mirroring
        // the metadata-INSERT-loses-the-race ordering.
        db.update_tab_orders(SolutionId("sol-a".into()), vec![m.id])
            .await
            .unwrap();
        // INSERT second, carrying the real tab_order the store stamped in
        // memory before re-persisting. The row must end up pinned regardless of
        // the lost UPDATE above.
        m.tab_order = Some(0);
        db.save_metadata(m.clone()).await.unwrap();

        assert_eq!(
            db.list_open_tabs(SolutionId("sol-a".into())).await.unwrap(),
            vec![m.id],
            "a metadata INSERT carrying tab_order must persist it even when a \
             prior UPDATE found no row"
        );
    }

    #[gpui::test]
    async fn tab_order_set_after_insert_still_works(cx: &mut gpui::TestAppContext) {
        // The benign order (INSERT then UPDATE) must keep working unchanged.
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let m = make_meta(1, "sol-a");
        db.save_metadata(m.clone()).await.unwrap();
        db.update_tab_orders(SolutionId("sol-a".into()), vec![m.id])
            .await
            .unwrap();

        assert_eq!(
            db.list_open_tabs(SolutionId("sol-a".into())).await.unwrap(),
            vec![m.id]
        );
    }

    #[gpui::test]
    async fn save_metadata_does_not_wipe_existing_tab_order(cx: &mut gpui::TestAppContext) {
        // A follow-up `save_metadata` (e.g. a token/preview update) carries
        // tab_order None, but must NOT clear a tab_order a prior
        // `update_tab_orders` legitimately set — the ON CONFLICT clause
        // COALESCE(excluded.tab_order=NULL, solution_sessions.tab_order) keeps it.
        // This is the load-bearing half of the order-independent fix: even if a
        // late metadata write lands after the strip position is durable, it
        // never reverts the row to NULL.
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let m = make_meta(1, "sol-a");
        db.save_metadata(m.clone()).await.unwrap();
        db.update_tab_orders(SolutionId("sol-a".into()), vec![m.id])
            .await
            .unwrap();
        // Re-save metadata (tab_order None) as the live store would on a later
        // activity-driven update.
        db.save_metadata(m.clone()).await.unwrap();

        assert_eq!(
            db.list_open_tabs(SolutionId("sol-a".into())).await.unwrap(),
            vec![m.id],
            "a follow-up save_metadata(None) must not clear an existing tab_order"
        );
    }

    #[gpui::test]
    async fn reopen_session_clears_stale_tab_order(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let m = make_meta(1, "sol-a");
        db.save_metadata(m.clone()).await.unwrap();

        // Pin the session into the strip, then close it. `close_session` marks
        // `closed_at` but deliberately leaves `tab_order` set, so a closed row
        // keeps a dangling strip slot — reproduced here directly.
        db.update_tab_orders(SolutionId("sol-a".into()), vec![m.id])
            .await
            .unwrap();
        let closed_at = Utc.timestamp_millis_opt(1_700_000_500_000).unwrap();
        db.mark_closed(m.id, Some(closed_at)).await.unwrap();

        // While closed it is excluded from the open-tab strip (the closed_at
        // filter), even though its tab_order is still set.
        assert!(
            db.list_open_tabs(SolutionId("sol-a".into()))
                .await
                .unwrap()
                .is_empty()
        );

        // Reopen. This must clear `closed_at` (live again) AND the stale
        // `tab_order`. If it only cleared `closed_at`, the row would
        // immediately satisfy `list_open_tabs` (`tab_order IS NOT NULL AND
        // closed_at IS NULL`); hydration would re-stamp the stale order, and
        // `open_session_in_strip` would early-return on its `already_pinned`
        // guard without emitting `TabsChanged` — the reopened-but-invisible
        // tab bug.
        db.reopen_session(m.id).await.unwrap();

        // Live again:
        assert_eq!(
            db.list_open_session_ids(SolutionId("sol-a".into()))
                .await
                .unwrap(),
            vec![m.id]
        );
        // …but NOT pinned: the strip set is empty, so the reopen path re-pins
        // it fresh via `open_session_in_strip` and emits `TabsChanged`.
        assert!(
            db.list_open_tabs(SolutionId("sol-a".into()))
                .await
                .unwrap()
                .is_empty(),
            "reopen must clear the stale tab_order so the session re-pins fresh"
        );
    }

    #[gpui::test]
    async fn background_agent_round_trip(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        let row = BackgroundAgentRow {
            solution_session_id: "ses-1".into(),
            agent_id: "a30f92a688e431edc".into(),
            jsonl_path: "/tmp/x.jsonl".into(),
            registered_at_ms: 1_700_000_000_000,
            last_seen_label: Some("Bash: ls".into()),
            last_mtime_ms: Some(1_700_000_001_000),
            stop_reason: None,
        };
        db.save_background_agent(row.clone()).await.unwrap();
        let loaded = db.load_background_agents("ses-1".into()).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], row);
    }

    #[gpui::test]
    async fn background_agent_delete(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        let row = BackgroundAgentRow {
            solution_session_id: "ses-1".into(),
            agent_id: "a30f92a688e431edc".into(),
            jsonl_path: "/tmp/x.jsonl".into(),
            registered_at_ms: 1_700_000_000_000,
            last_seen_label: None,
            last_mtime_ms: None,
            stop_reason: None,
        };
        db.save_background_agent(row).await.unwrap();
        db.delete_background_agent("ses-1".into(), "a30f92a688e431edc".into())
            .await
            .unwrap();
        let loaded = db.load_background_agents("ses-1".into()).await.unwrap();
        assert!(loaded.is_empty());
    }

    #[gpui::test]
    async fn background_shell_round_trip(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        let row = BackgroundShellRow {
            solution_session_id: "ses-1".into(),
            shell_id: "bvb4ful1z".into(),
            command: "npm run watch".into(),
            output_path: "/tmp/bvb4ful1z.output".into(),
            registered_at_ms: 1_700_000_000_000,
            last_tail: Some("Watching for changes...".into()),
            last_mtime_ms: Some(1_700_000_001_000),
            state_text: "running".into(),
        };
        db.save_background_shell(row.clone()).await.unwrap();
        let loaded = db.load_background_shells("ses-1".into()).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], row);

        // Verify None variants for optional fields also round-trip.
        let row_no_opts = BackgroundShellRow {
            solution_session_id: "ses-1".into(),
            shell_id: "xyz123".into(),
            command: "sleep 60".into(),
            output_path: "/tmp/xyz123.output".into(),
            registered_at_ms: 1_700_000_002_000,
            last_tail: None,
            last_mtime_ms: None,
            state_text: "exited:0".into(),
        };
        db.save_background_shell(row_no_opts.clone()).await.unwrap();
        let loaded = db.load_background_shells("ses-1".into()).await.unwrap();
        assert_eq!(loaded.len(), 2);
        let found = loaded.iter().find(|r| r.shell_id == "xyz123").unwrap();
        assert_eq!(found, &row_no_opts);
    }

    #[gpui::test]
    async fn background_shell_delete_by_id(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        let row = BackgroundShellRow {
            solution_session_id: "ses-1".into(),
            shell_id: "bvb4ful1z".into(),
            command: "npm run watch".into(),
            output_path: "/tmp/bvb4ful1z.output".into(),
            registered_at_ms: 1_700_000_000_000,
            last_tail: None,
            last_mtime_ms: None,
            state_text: "running".into(),
        };
        db.save_background_shell(row).await.unwrap();
        db.delete_background_shell("ses-1".into(), "bvb4ful1z".into())
            .await
            .unwrap();
        let loaded = db.load_background_shells("ses-1".into()).await.unwrap();
        assert!(loaded.is_empty());
    }

    #[gpui::test]
    async fn background_shell_delete_for_session_only_removes_that_session(
        cx: &mut gpui::TestAppContext,
    ) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let make_row = |session: &str, shell: &str| BackgroundShellRow {
            solution_session_id: session.into(),
            shell_id: shell.into(),
            command: "echo hi".into(),
            output_path: format!("/tmp/{shell}.output"),
            registered_at_ms: 1_700_000_000_000,
            last_tail: None,
            last_mtime_ms: None,
            state_text: "killed".into(),
        };

        db.save_background_shell(make_row("ses-1", "shell-a"))
            .await
            .unwrap();
        db.save_background_shell(make_row("ses-1", "shell-b"))
            .await
            .unwrap();
        db.save_background_shell(make_row("ses-2", "shell-c"))
            .await
            .unwrap();

        db.delete_background_shells_for_session("ses-1".into())
            .await
            .unwrap();

        let ses1 = db.load_background_shells("ses-1".into()).await.unwrap();
        assert!(ses1.is_empty());
        let ses2 = db.load_background_shells("ses-2".into()).await.unwrap();
        assert_eq!(ses2.len(), 1);
        assert_eq!(ses2[0].shell_id, "shell-c");
    }

    #[gpui::test]
    async fn attachments_round_trip_and_cascade(cx: &mut gpui::TestAppContext) {
        let db = SolutionAgentDb::open(cx.executor()).unwrap();
        db.record_attachment("ses-1".into(), "sol-a".into(), "/inbox/a.png".into(), 1)
            .await
            .unwrap();
        db.record_attachment("ses-1".into(), "sol-a".into(), "/inbox/b.png".into(), 2)
            .await
            .unwrap();
        db.record_attachment("ses-2".into(), "sol-a".into(), "/inbox/c.png".into(), 3)
            .await
            .unwrap();
        db.record_attachment("ses-3".into(), "sol-b".into(), "/inbox/d.png".into(), 4)
            .await
            .unwrap();

        let mut by_ses1 = db.attachment_paths_for_session("ses-1".into()).await.unwrap();
        by_ses1.sort();
        assert_eq!(by_ses1, vec!["/inbox/a.png", "/inbox/b.png"]);

        let by_sol_a = db.attachment_paths_for_solution("sol-a".into()).await.unwrap();
        assert_eq!(by_sol_a.len(), 3);

        // Delete by session removes only that session's rows.
        db.delete_attachments_for_session("ses-1".into()).await.unwrap();
        assert!(
            db.attachment_paths_for_session("ses-1".into())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.attachment_paths_for_session("ses-2".into())
                .await
                .unwrap()
                .len(),
            1
        );

        // delete_for_solution cascades to attachment rows for that solution only.
        db.delete_for_solution(SolutionId("sol-a".into())).await.unwrap();
        assert!(
            db.attachment_paths_for_solution("sol-a".into())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.attachment_paths_for_solution("sol-b".into())
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[gpui::test]
    async fn solution_session_entries_table_and_index_exist(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        let connection = db.connection.lock();

        // Check table exists
        let mut tables = connection
            .select::<String>(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='solution_session_entries'",
            )
            .unwrap();
        let table_names = tables().unwrap();
        assert!(
            table_names.iter().any(|n| n == "solution_session_entries"),
            "solution_session_entries table must exist"
        );

        // Check index exists
        let mut indexes = connection
            .select::<String>(
                "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_session_entry_modseq'",
            )
            .unwrap();
        let index_names = indexes().unwrap();
        assert!(
            index_names.iter().any(|n| n == "idx_session_entry_modseq"),
            "idx_session_entry_modseq index must exist"
        );

        // Check epoch column exists on solution_sessions
        assert!(
            column_exists(&connection, "solution_sessions", "epoch"),
            "epoch column must exist on solution_sessions"
        );
    }

    #[gpui::test]
    async fn delete_for_solution_cascades(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        db.save_metadata(make_meta(1, "sol-a")).await.unwrap();
        db.save_metadata(make_meta(2, "sol-a")).await.unwrap();
        db.save_metadata(make_meta(3, "sol-b")).await.unwrap();

        db.delete_for_solution(SolutionId("sol-a".into()))
            .await
            .unwrap();

        assert!(
            db.list_for_solution(SolutionId("sol-a".into()))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.list_for_solution(SolutionId("sol-b".into()))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[gpui::test]
    async fn entry_upsert_and_load_ordered_by_idx(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let session_id = SolutionSessionId::new();
        db.upsert_entry(session_id, 1, 10, 1_000, None, b"second".to_vec())
            .await
            .unwrap();
        db.upsert_entry(session_id, 0, 5, 500, Some("agent-a".into()), b"first".to_vec())
            .await
            .unwrap();

        let rows = db.load_entries(session_id).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].idx, 0);
        assert_eq!(rows[0].payload, b"first".to_vec());
        assert_eq!(rows[0].subagent_id, Some("agent-a".into()));
        assert_eq!(rows[1].idx, 1);
        assert_eq!(rows[1].payload, b"second".to_vec());
        assert_eq!(rows[1].subagent_id, None);
    }

    #[gpui::test]
    async fn entry_upsert_same_idx_updates_in_place(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let session_id = SolutionSessionId::new();
        db.upsert_entry(session_id, 0, 1, 100, None, b"original".to_vec())
            .await
            .unwrap();
        db.upsert_entry(session_id, 0, 2, 200, Some("sub".into()), b"updated".to_vec())
            .await
            .unwrap();

        let rows = db.load_entries(session_id).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].idx, 0);
        assert_eq!(rows[0].mod_seq, 2);
        assert_eq!(rows[0].created_ms, 200);
        assert_eq!(rows[0].subagent_id, Some("sub".into()));
        assert_eq!(rows[0].payload, b"updated".to_vec());
    }

    #[gpui::test]
    async fn delete_entries_from_leaves_earlier_rows(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let session_id = SolutionSessionId::new();
        for i in 0i64..3 {
            db.upsert_entry(session_id, i, i, i * 100, None, vec![i as u8])
                .await
                .unwrap();
        }

        db.delete_entries_from(session_id, 1).await.unwrap();

        let rows = db.load_entries(session_id).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].idx, 0);
    }

    #[gpui::test]
    async fn save_and_load_epoch_roundtrips(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        // epoch lives on solution_sessions, so the row must exist first.
        let meta = make_meta(1, "sol-epoch");
        db.save_metadata(meta.clone()).await.unwrap();

        // Before setting it, load_epoch returns None.
        let before = db.load_epoch(meta.id).await.unwrap();
        assert_eq!(before, None);

        db.save_epoch(meta.id, 42).await.unwrap();
        let after = db.load_epoch(meta.id).await.unwrap();
        assert_eq!(after, Some(42));

        // Update to a new value.
        db.save_epoch(meta.id, 99).await.unwrap();
        let updated = db.load_epoch(meta.id).await.unwrap();
        assert_eq!(updated, Some(99));
    }

    #[gpui::test]
    async fn save_and_load_change_seq_roundtrips(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        // change_seq lives on solution_sessions, so the row must exist first.
        let meta = make_meta(1, "sol-change-seq");
        db.save_metadata(meta.clone()).await.unwrap();

        // Before setting it, load_change_seq returns None (legacy/unset).
        let before = db.load_change_seq(meta.id).await.unwrap();
        assert_eq!(before, None);

        db.save_change_seq(meta.id, 7).await.unwrap();
        let after = db.load_change_seq(meta.id).await.unwrap();
        assert_eq!(after, Some(7));

        // Update to a new (higher) value.
        db.save_change_seq(meta.id, 42).await.unwrap();
        let updated = db.load_change_seq(meta.id).await.unwrap();
        assert_eq!(updated, Some(42));

        // The UPDATE is `max`-guarded: a stale lower write (e.g. a detached
        // background flush that lands out of order) must NOT roll the durable
        // value back below an already-issued cursor.
        db.save_change_seq(meta.id, 10).await.unwrap();
        let guarded = db.load_change_seq(meta.id).await.unwrap();
        assert_eq!(guarded, Some(42), "a lower write must not overwrite a higher durable change_seq");
    }

    #[gpui::test]
    async fn delete_entries_for_session_removes_all_rows(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let session_a = SolutionSessionId::new();
        let session_b = SolutionSessionId::new();

        for i in 0i64..3 {
            db.upsert_entry(session_a, i, i, i * 100, None, vec![i as u8])
                .await
                .unwrap();
        }
        db.upsert_entry(session_b, 0, 0, 0, None, b"keep".to_vec())
            .await
            .unwrap();

        db.delete_entries_for_session(session_a).await.unwrap();

        let rows_a = db.load_entries(session_a).await.unwrap();
        assert!(rows_a.is_empty());
        let rows_b = db.load_entries(session_b).await.unwrap();
        assert_eq!(rows_b.len(), 1);
    }

    #[gpui::test]
    async fn purge_session_removes_rows_from_all_six_tables(cx: &mut gpui::TestAppContext) {
        use crate::supervisor::SupervisorState;

        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        // The session to purge, plus a sibling whose rows must survive.
        let meta = make_meta(1, "sol-purge");
        let target = meta.id;
        db.save_metadata(meta).await.unwrap();

        let sibling_meta = make_meta(2, "sol-purge");
        let sibling = sibling_meta.id;
        db.save_metadata(sibling_meta).await.unwrap();

        // Populate all six tables for both sessions, to prove purge is scoped.
        for (id, tag) in [(target, "x"), (sibling, "y")] {
            db.upsert_entry(id, 0, 0, 0, None, tag.as_bytes().to_vec())
                .await
                .unwrap();
            db.record_attachment(
                id.to_string(),
                "sol-purge".into(),
                format!("/inbox/{tag}.png"),
                1,
            )
            .await
            .unwrap();
            db.save_background_agent(BackgroundAgentRow {
                solution_session_id: id.to_string(),
                agent_id: format!("agent-{tag}"),
                jsonl_path: format!("/tmp/{tag}.jsonl"),
                registered_at_ms: 1,
                last_seen_label: None,
                last_mtime_ms: None,
                stop_reason: None,
            })
            .await
            .unwrap();
            db.save_background_shell(BackgroundShellRow {
                solution_session_id: id.to_string(),
                shell_id: format!("shell-{tag}"),
                command: "echo hi".into(),
                output_path: format!("/tmp/shell-{tag}.output"),
                registered_at_ms: 1,
                last_tail: None,
                last_mtime_ms: None,
                state_text: "running".into(),
            })
            .await
            .unwrap();
            db.save_supervisor_state(SupervisorState::new(id))
                .await
                .unwrap();
        }

        db.purge_session(target).await.unwrap();

        // Every table is empty for `target`.
        assert!(db.load_entries(target).await.unwrap().is_empty());
        assert!(db
            .attachment_paths_for_session(target.to_string())
            .await
            .unwrap()
            .is_empty());
        assert!(db
            .load_background_agents(target.to_string())
            .await
            .unwrap()
            .is_empty());
        assert!(db
            .load_background_shells(target.to_string())
            .await
            .unwrap()
            .is_empty());
        assert!(db
            .load_supervisor_states()
            .await
            .unwrap()
            .iter()
            .all(|s| s.session_id != target));
        let listed = db
            .list_for_solution(SolutionId("sol-purge".into()))
            .await
            .unwrap();
        assert!(listed.iter().all(|m| m.id != target));

        // The sibling's rows all survive.
        assert_eq!(db.load_entries(sibling).await.unwrap().len(), 1);
        assert_eq!(
            db.attachment_paths_for_session(sibling.to_string())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.load_background_agents(sibling.to_string())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.load_background_shells(sibling.to_string())
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(db
            .load_supervisor_states()
            .await
            .unwrap()
            .iter()
            .any(|s| s.session_id == sibling));
        assert!(listed.iter().any(|m| m.id == sibling));
    }

    // ── Task 3a: session model/effort/cached_models columns ──────────────────

    /// Helper: build a `ModelInfo` for use in tests.
    fn make_model_info(value: &str) -> claude_native::ModelInfo {
        claude_native::ModelInfo {
            value: value.into(),
            display_name: format!("{value} Display"),
            description: format!("{value} description"),
        }
    }

    /// (a) Round-trip: fields written in save_metadata come back from
    /// list_for_solution intact.
    /// (b) COALESCE: a second save with all-None/empty doesn't clobber the
    /// values from the first.
    /// (c) cached_models JSON serialises and deserialises without data loss.
    #[gpui::test]
    async fn session_settings_roundtrip_and_coalesce(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        // (a) round-trip
        let mut meta = make_meta(1, "sol-settings");
        meta.desired_model = Some("claude-opus-4-5".into());
        meta.desired_effort = Some("high".into());
        meta.cached_models = vec![
            make_model_info("claude-opus-4-5"),
            make_model_info("claude-sonnet-4-5"),
        ];
        db.save_metadata(meta.clone()).await.unwrap();

        let listed = db
            .list_for_solution(SolutionId("sol-settings".into()))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        let loaded = &listed[0];
        assert_eq!(loaded.desired_model, Some("claude-opus-4-5".into()));
        assert_eq!(loaded.desired_effort, Some("high".into()));
        assert_eq!(loaded.cached_models, meta.cached_models);

        // (b) COALESCE: second write with None/empty must not clobber
        let mut meta_nones = make_meta(1, "sol-settings");
        // Override id to match the existing row so ON CONFLICT fires.
        meta_nones.id = meta.id;
        meta_nones.desired_model = None;
        meta_nones.desired_effort = None;
        meta_nones.cached_models = vec![];
        db.save_metadata(meta_nones).await.unwrap();

        let listed2 = db
            .list_for_solution(SolutionId("sol-settings".into()))
            .await
            .unwrap();
        assert_eq!(listed2.len(), 1);
        let loaded2 = &listed2[0];
        // Original values must still be present.
        assert_eq!(
            loaded2.desired_model,
            Some("claude-opus-4-5".into()),
            "COALESCE must not clobber desired_model"
        );
        assert_eq!(
            loaded2.desired_effort,
            Some("high".into()),
            "COALESCE must not clobber desired_effort"
        );
        assert_eq!(
            loaded2.cached_models, meta.cached_models,
            "COALESCE must not clobber cached_models"
        );

        // (c) cached_models JSON round-trip: a write with ≥1 model and then a
        // read must preserve all fields of ModelInfo.
        let mut meta2 = make_meta(2, "sol-settings");
        meta2.cached_models = vec![make_model_info("model-x")];
        db.save_metadata(meta2.clone()).await.unwrap();

        let listed3 = db
            .list_for_solution(SolutionId("sol-settings".into()))
            .await
            .unwrap();
        let loaded3 = listed3
            .iter()
            .find(|m| m.id == meta2.id)
            .expect("row must be present");
        assert_eq!(loaded3.cached_models.len(), 1);
        assert_eq!(loaded3.cached_models[0].value, "model-x");
        assert_eq!(loaded3.cached_models[0].display_name, "model-x Display");
        assert_eq!(loaded3.cached_models[0].description, "model-x description");
    }
}
