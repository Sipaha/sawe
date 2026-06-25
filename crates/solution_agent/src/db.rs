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

        connection.exec(indoc! {"
            CREATE INDEX IF NOT EXISTS idx_session_by_solution
                ON solution_sessions (solution_id, last_activity_at DESC)
        "})?()
        .map_err(|e| anyhow!("Failed to create idx_session_by_solution: {}", e))?;

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
    // Pre-check via `PRAGMA table_info`. The sqlez wrapper surfaces
    // duplicate-column errors as opaque "Prepare call failed for
    // query: …" without the underlying SQLite text, so the old
    // substring filter no longer matched on a re-run — every restart
    // logged one WARN per already-applied migration. Inspecting the
    // catalog up front avoids both the noise AND the prepare attempt.
    let column_name = column_def.split_whitespace().next().unwrap_or(column_def);
    if column_exists(connection, "solution_sessions", column_name) {
        return;
    }
    let ddl = format!("ALTER TABLE solution_sessions ADD COLUMN {column_def}");
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
    // `preview` and `total_tokens` use COALESCE so a metadata write that
    // doesn't have those fields populated yet (e.g. fresh-session insert at
    // create time) doesn't clobber values an event-driven update wrote
    // earlier in the same session.
    //
    // Nested tuple shape because `sqlez::Bind` only implements tuples up
    // to size 10; we have 12 columns now that `parent_session_id` is
    // persisted (5 + 7).
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
    )>(indoc! {"
        INSERT INTO solution_sessions (
            id, solution_id, agent_id, acp_session_id, title,
            created_at, last_activity_at, preview, total_tokens,
            context_count, cwd, parent_session_id
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
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
            parent_session_id  = COALESCE(excluded.parent_session_id, parent_session_id)
    "})?;

    let cwd_str = if meta.cwd.as_os_str().is_empty() {
        None
    } else {
        Some(meta.cwd.to_string_lossy().into_owned())
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

fn delete_by_solution(connection: &Connection, solution_id: &SolutionId) -> Result<()> {
    let mut delete = connection.exec_bound::<String>(indoc! {"
        DELETE FROM solution_sessions WHERE solution_id = ?
    "})?;
    delete(solution_id.0.clone())?;
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
    // implements tuples up to size 10; we have 12 columns now.
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
    )>(indoc! {"
        SELECT id, solution_id, agent_id, acp_session_id, title,
               created_at, last_activity_at, preview, total_tokens,
               context_count, cwd, parent_session_id
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
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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
}
