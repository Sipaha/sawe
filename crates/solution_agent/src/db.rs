use std::sync::Arc;

use anyhow::{Result, anyhow};
use futures::{FutureExt, future::Shared};
use gpui::{App, BackgroundExecutor, Global, Task};
use indoc::indoc;
use parking_lot::Mutex;
use solutions::SolutionId;
use sqlez::connection::Connection;

use crate::model::SolutionSessionId;

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

        Self::open_connection(executor, connection)
    }

    /// Open the agent DB at an explicit file path, bypassing both the data-dir
    /// lookup and the in-memory swap `open` performs under test cfgs. The
    /// identity-migration rehearsal (`crates/solutions/tests`) uses this to run
    /// the real schema setup + `migrate_identity` against a *copy* of the
    /// operator's production database.
    pub fn open_at_path(executor: BackgroundExecutor, path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::open_connection(executor, Connection::open_file(&path.to_string_lossy()))
    }

    fn open_connection(executor: BackgroundExecutor, connection: Connection) -> Result<Self> {
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
        // Phase 1 (rename/identity): the session's project, as a fact rather
        // than an inference. NULL = the session runs at the solution root (the
        // "ROOT" label). Previously the project was derived by comparing `cwd`
        // to each member's `local_path` with exact equality, so any path drift
        // silently degraded the label to ROOT. No FK: `solution_agent.db` is a
        // different file from the solutions DB, so a dangling member_id degrades
        // to "unknown project" instead of corrupting the row.
        apply_idempotent_add_column(&connection, "member_id INTEGER");

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
        apply_idempotent_add_column_to(&connection, "supervisor_state", "next_eligible_ms INTEGER");

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

    pub fn delete_for_solution(&self, solution_id: SolutionId) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            delete_by_solution(&connection, solution_id)
        })
    }

    /// Rewrite `solution_sessions.cwd` for every session of `solution_id` whose
    /// cwd sits at or under `rewrite.old`, moving it under `rewrite.new`. Mirrors
    /// the cold-reconcile `rewrite_agent_db` step but scoped to one solution and
    /// run **hot**, on a rename's `PathsMoved`. Covers the COLD (un-hydrated)
    /// sessions the in-memory rewrite can't reach, so a same-process solution
    /// reopen re-hydrates a valid cwd instead of a stale one that
    /// `gc_orphan_members` would purge.
    pub fn rewrite_session_cwds(
        &self,
        solution_id: SolutionId,
        rewrite: solutions::path_migrations::PathRewrite,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            rewrite_session_cwds_by_solution(&connection, solution_id, &rewrite)
        })
    }

    /// Rewrite `solution_sessions.solution_id` / `solution_session_attachment
    /// .solution_id` from the pre-identity TEXT slug to the numeric counter id
    /// the solutions DB now uses, and bind each session to the member whose
    /// `local_path` equals its `cwd`.
    ///
    /// Idempotent: rows whose `solution_id` already parses as an integer are
    /// skipped, and `member_id` is only written where it is still NULL.
    ///
    /// `legacy_solution_ids` is `(old_slug, new_id)` from
    /// `SolutionsDb::load_solution_legacy_ids`; `members` is
    /// `(member_id, solution_id, local_path)`.
    pub fn migrate_identity(
        &self,
        legacy_solution_ids: Vec<(String, i64)>,
        members: Vec<(i64, i64, String)>,
    ) -> Task<Result<IdentityMigrationReport>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            migrate_identity_fn(&connection, &legacy_solution_ids, &members)
        })
    }

    pub fn set_session_member(
        &self,
        id: SolutionSessionId,
        member_id: Option<solutions::MemberId>,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            let mut update = connection.exec_bound::<(Option<i64>, String)>(indoc! {"
                UPDATE solution_sessions SET member_id = ?1 WHERE id = ?2
            "})?;
            update((member_id.map(|m| m.0), id.to_string()))?;
            Ok(())
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityMigrationReport {
    pub sessions_total: i64,
    pub sessions_remapped: i64,
    /// Slugs that had no entry in the solutions DB's legacy map — their rows are
    /// left untouched so the operator can inspect them. Non-empty means a
    /// solution was deleted from `solutions.db` while its sessions survived.
    pub sessions_unmapped: Vec<String>,
    pub member_ids_backfilled: i64,
}

fn migrate_identity_fn(
    connection: &Connection,
    legacy_solution_ids: &[(String, i64)],
    members: &[(i64, i64, String)],
) -> Result<IdentityMigrationReport> {
    let sessions = connection.select::<(String, String, Option<String>, Option<i64>)>(indoc! {"
        SELECT id, solution_id, cwd, member_id FROM solution_sessions
    "})?()?;

    let mut report = IdentityMigrationReport {
        sessions_total: sessions.len() as i64,
        ..Default::default()
    };

    let mut remap = connection.exec_bound::<(String, String)>(indoc! {"
        UPDATE solution_sessions SET solution_id = ?1 WHERE id = ?2
    "})?;
    let mut remap_attachments = connection.exec_bound::<(String, String)>(indoc! {"
        UPDATE solution_session_attachment SET solution_id = ?1 WHERE solution_id = ?2
    "})?;
    let mut bind_member = connection.exec_bound::<(i64, String)>(indoc! {"
        UPDATE solution_sessions SET member_id = ?1 WHERE id = ?2
    "})?;

    let mut unmapped: Vec<String> = Vec::new();
    for (session_id, solution_id, cwd, member_id) in &sessions {
        // Already-numeric rows were migrated by an earlier run.
        let numeric_solution: i64 = match solution_id.parse::<i64>() {
            Ok(numeric) => numeric,
            Err(_) => {
                let Some((_, new_id)) =
                    legacy_solution_ids.iter().find(|(old, _)| old == solution_id)
                else {
                    if !unmapped.contains(solution_id) {
                        unmapped.push(solution_id.clone());
                    }
                    continue;
                };
                remap((new_id.to_string(), session_id.clone()))?;
                remap_attachments((new_id.to_string(), solution_id.clone()))?;
                report.sessions_remapped += 1;
                *new_id
            }
        };

        if member_id.is_some() {
            continue;
        }
        let Some(cwd) = cwd.as_deref().filter(|c| !c.is_empty()) else {
            continue;
        };
        // Exact match only: `cwd` is the member root at spawn time. A cwd that is
        // the solution root (or anything else) stays NULL — that IS the ROOT label.
        let Some((matched_member, _, _)) = members
            .iter()
            .find(|(_, solution, path)| *solution == numeric_solution && path == cwd)
        else {
            continue;
        };
        bind_member((*matched_member, session_id.clone()))?;
        report.member_ids_backfilled += 1;
    }

    report.sessions_unmapped = unmapped;
    Ok(report)
}

/// Apply `ALTER TABLE solution_sessions ADD COLUMN <column_def>` and
/// silently swallow only the *expected* "duplicate column name" error
/// (the marker that the column has already been added on an earlier
/// run). Every other error — prepare-time *or* run-time — gets logged
/// at `warn` so a busted migration leaves a breadcrumb instead of
/// silently leaving the schema half-applied.
pub(crate) fn apply_idempotent_add_column(connection: &Connection, column_def: &str) {
    apply_idempotent_add_column_to(connection, "solution_sessions", column_def);
}

/// Generalized form of [apply_idempotent_add_column] that targets an arbitrary
/// `table`. `table` is always an internal constant (never user input), so
/// inlining it into the DDL carries no injection risk.
pub(crate) fn apply_idempotent_add_column_to(connection: &Connection, table: &str, column_def: &str) {
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
pub(crate) fn column_exists(connection: &Connection, table: &str, column: &str) -> bool {
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

fn delete_by_solution(connection: &Connection, solution_id: SolutionId) -> Result<()> {
    // The `solution_id` column is TEXT (it held slugs before the identity
    // migration); the migration rewrites it to the *decimal text* of the numeric
    // id rather than retyping the column, so every read/write of it binds the
    // stringified counter.
    let solution_id = solution_id.0.to_string();
    // One savepoint so the solution can't be left half-purged. Only
    // `solution_sessions` and `solution_session_attachment` carry a
    // `solution_id` column; the per-session child tables (entries + the two
    // background tables key on `session_id`/`solution_session_id`, and
    // `supervisor_state` keys on `session_id`) have no `solution_id`, so they
    // are swept via a subselect over the solution's sessions. The subselect
    // statements MUST run BEFORE the `solution_sessions` delete, otherwise the
    // subselect would already be empty.
    let tx = connection.with_savepoint("delete_by_solution", || {
        for sql in [
            "DELETE FROM solution_session_entries
             WHERE session_id IN (SELECT id FROM solution_sessions WHERE solution_id = ?)",
            "DELETE FROM solution_session_background_agent
             WHERE solution_session_id IN (SELECT id FROM solution_sessions WHERE solution_id = ?)",
            "DELETE FROM solution_session_background_shell
             WHERE solution_session_id IN (SELECT id FROM solution_sessions WHERE solution_id = ?)",
            "DELETE FROM supervisor_state
             WHERE session_id IN (SELECT id FROM solution_sessions WHERE solution_id = ?)",
            // Attachment rows carry the solution_id directly (callers delete the
            // files themselves via `attachment_paths_for_solution` before this,
            // while the paths are still queryable).
            "DELETE FROM solution_session_attachment WHERE solution_id = ?",
            // The parent rows last, so the subselects above still resolve.
            "DELETE FROM solution_sessions WHERE solution_id = ?",
        ] {
            let mut stmt = connection.exec_bound::<String>(sql)?;
            stmt(solution_id.clone())?;
        }
        Ok(())
    });
    tx.map_err(|e| anyhow!("delete_by_solution failed: {e}"))
}

fn rewrite_session_cwds_by_solution(
    connection: &Connection,
    solution_id: SolutionId,
    rewrite: &solutions::path_migrations::PathRewrite,
) -> Result<()> {
    // `solution_id` is bound as the numeric counter — SQLite applies the
    // column's TEXT affinity to the integer, so it matches the stored decimal
    // text (same convention as `select_open_tabs`). `cwd` is plain TEXT.
    let mut select = connection.select_bound::<i64, (String, String)>(
        "SELECT id, cwd FROM solution_sessions WHERE solution_id = ? AND cwd IS NOT NULL",
    )?;
    let rows = select(solution_id.0)?;
    let mut update = connection
        .exec_bound::<(String, String)>("UPDATE solution_sessions SET cwd = ?1 WHERE id = ?2")?;
    for (id, cwd) in rows {
        if let Some(rewritten) = rewrite.apply_str(&cwd) {
            update((rewritten, id))?;
        }
    }
    Ok(())
}

mod attachments;
mod background;
mod entries;
mod sessions;
mod supervisor;

#[cfg(test)]
mod tests;
