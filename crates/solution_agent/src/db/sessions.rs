use std::sync::Arc;

use agent_client_protocol::schema as acp;
use anyhow::anyhow;
use anyhow::Result;
use chrono::{DateTime, Utc};
use gpui::{SharedString, Task};
use indoc::indoc;
use solutions::SolutionId;
use sqlez::connection::Connection;

use crate::db::SolutionAgentDb;
use crate::model::{SolutionSessionId, SolutionSessionMetadata};

impl SolutionAgentDb {
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

    /// Ids of sessions the user soft-closed (`closed_at IS NOT NULL`) more than
    /// `cutoff_ms` ago — the TTL reaper hard-purges these. `reopen_session`
    /// clears `closed_at`, so a restored-then-reclosed session restarts the
    /// clock from its NEW close (its old, long-past `closed_at` is gone).
    pub fn list_sessions_closed_before(
        &self,
        solution_id: SolutionId,
        cutoff_ms: i64,
    ) -> Task<Result<Vec<SolutionSessionId>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_sessions_closed_before(&connection, &solution_id, cutoff_ms)
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

pub(crate) fn insert_or_update_metadata(
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

pub(crate) fn update_blob(connection: &Connection, id: SolutionSessionId, blob: &[u8]) -> Result<()> {
    let mut update = connection.exec_bound::<(Vec<u8>, String)>(indoc! {"
        UPDATE solution_sessions SET acp_thread_blob = ?1 WHERE id = ?2
    "})?;
    update((blob.to_vec(), id.to_string()))?;
    Ok(())
}

pub(crate) fn select_blob(connection: &Connection, id: SolutionSessionId) -> Result<Option<Vec<u8>>> {
    let mut select = connection.select_bound::<String, Option<Vec<u8>>>(indoc! {"
        SELECT acp_thread_blob FROM solution_sessions WHERE id = ? LIMIT 1
    "})?;
    let rows = select(id.to_string())?;
    Ok(rows.into_iter().next().flatten())
}

pub(crate) fn mark_closed_by_id(
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

pub(crate) fn reopen_session_by_id(connection: &Connection, id: SolutionSessionId) -> Result<()> {
    let mut update = connection.exec_bound::<String>(indoc! {"
        UPDATE solution_sessions SET closed_at = NULL, tab_order = NULL WHERE id = ?
    "})?;
    update(id.to_string())?;
    Ok(())
}

pub(crate) fn select_closed_at(
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

pub(crate) fn delete_by_id(connection: &Connection, id: SolutionSessionId) -> Result<()> {
    let mut delete = connection.exec_bound::<String>(indoc! {"
        DELETE FROM solution_sessions WHERE id = ?
    "})?;
    delete(id.to_string())?;
    Ok(())
}

pub(crate) fn update_epoch(connection: &Connection, session_id: &str, epoch: i64) -> Result<()> {
    let mut stmt = connection.exec_bound::<(i64, String)>(indoc! {"
        UPDATE solution_sessions SET epoch = ?1 WHERE id = ?2
    "})?;
    stmt((epoch, session_id.to_string()))?;
    Ok(())
}

pub(crate) fn select_epoch(connection: &Connection, session_id: &str) -> Result<Option<i64>> {
    let mut stmt = connection.select_bound::<String, Option<i64>>(indoc! {"
        SELECT epoch FROM solution_sessions WHERE id = ? LIMIT 1
    "})?;
    let rows = stmt(session_id.to_string())?;
    Ok(rows.into_iter().next().flatten())
}

pub(crate) fn update_change_seq(connection: &Connection, session_id: &str, change_seq: i64) -> Result<()> {
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

pub(crate) fn select_change_seq(connection: &Connection, session_id: &str) -> Result<Option<i64>> {
    let mut stmt = connection.select_bound::<String, Option<i64>>(indoc! {"
        SELECT change_seq FROM solution_sessions WHERE id = ? LIMIT 1
    "})?;
    let rows = stmt(session_id.to_string())?;
    Ok(rows.into_iter().next().flatten())
}

pub(crate) fn apply_tab_orders(
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

pub(crate) fn select_open_tabs(
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
pub(crate) fn select_open_session_ids(
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
pub(crate) fn select_closed_session_ids(
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

pub(crate) fn select_sessions_closed_before(
    connection: &Connection,
    solution_id: &SolutionId,
    cutoff_ms: i64,
) -> Result<Vec<SolutionSessionId>> {
    let mut select = connection.select_bound::<(String, i64), String>(indoc! {"
        SELECT id FROM solution_sessions
        WHERE solution_id = ? AND closed_at IS NOT NULL AND closed_at < ?
    "})?;
    let rows = select((solution_id.0.clone(), cutoff_ms))?;
    let mut out = Vec::with_capacity(rows.len());
    for id in rows {
        out.push(
            SolutionSessionId::parse(&id)
                .map_err(|e| anyhow!("invalid SolutionSessionId in closed-session row: {e}"))?,
        );
    }
    Ok(out)
}

pub(crate) fn select_metadata_for_solution(
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
