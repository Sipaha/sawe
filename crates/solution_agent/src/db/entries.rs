use anyhow::{Result, anyhow};
use gpui::Task;
use indoc::indoc;
use sqlez::connection::Connection;

use crate::db::{EntryRow, SolutionAgentDb};
use crate::model::SolutionSessionId;

/// Shared by the per-row and the batched writer so the two can never disagree
/// about what an entry upsert means.
const UPSERT_ENTRY_SQL: &str = indoc! {"
    INSERT INTO solution_session_entries
        (session_id, idx, mod_seq, created_ms, subagent_id, payload)
    VALUES (?, ?, ?, ?, ?, ?)
    ON CONFLICT(session_id, idx) DO UPDATE SET
        mod_seq     = excluded.mod_seq,
        created_ms  = excluded.created_ms,
        subagent_id = excluded.subagent_id,
        payload     = excluded.payload
"};

impl SolutionAgentDb {
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
            insert_or_update_entry(
                &connection,
                &session_id.to_string(),
                idx,
                mod_seq,
                created_ms,
                subagent_id,
                payload,
            )
        })
    }

    /// Write a whole persist flush — every row plus the trailing trim that ends
    /// it — under ONE acquisition of the connection.
    ///
    /// [`Self::upsert_entry`] costs an `executor.spawn` and a lock acquisition
    /// per row, so a 200-entry flush spent 200 background round trips with the
    /// connection released between each one. Every reader takes the same lock,
    /// so a `load_entries` from a concurrent reopen could land in any of those
    /// gaps and hydrate from a PREFIX of the flush; cold load then derives
    /// `persisted_main_seq` from the short read and the session's next persist
    /// trims the rest away. Doing the flush inside one closure leaves a reader
    /// only two states to see: the row set from before it, or the one after.
    ///
    /// `trim_from_idx` rides along for exactly that reason rather than staying a
    /// separately-awaited [`Self::delete_entries_from`]. Between the last upsert
    /// and the trim the table holds the new rows AND the stale tail beyond them
    /// — which is precisely the "flat mirror longer than Main" shape cold load
    /// reads as a legacy row layout (`model::hydrate_streams_main_only`). The
    /// order within the closure is the order the callers issued it in: upserts
    /// first, trim last.
    pub fn upsert_entries_and_trim(
        &self,
        session_id: SolutionSessionId,
        rows: Vec<EntryRow>,
        trim_from_idx: i64,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            insert_or_update_entries_and_trim(
                &connection,
                &session_id.to_string(),
                rows,
                trim_from_idx,
            )
        })
    }

    /// Read a session's rows on the CALLING thread instead of through the
    /// background executor. Test-only: a `load_entries` task needs an executor
    /// turn of its own, so it cannot sample the table *between* two turns —
    /// which is the whole measurement in the flush-atomicity tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn load_entries_blocking(&self, session_id: SolutionSessionId) -> Result<Vec<EntryRow>> {
        let connection = self.connection.lock();
        select_entries_for_session(&connection, &session_id.to_string())
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
}

pub(crate) fn insert_or_update_entry(
    connection: &Connection,
    session_id: &str,
    idx: i64,
    mod_seq: i64,
    created_ms: i64,
    subagent_id: Option<String>,
    payload: Vec<u8>,
) -> Result<()> {
    let mut stmt = connection
        .exec_bound::<(String, i64, i64, i64, Option<String>, Vec<u8>)>(UPSERT_ENTRY_SQL)?;
    stmt((
        session_id.to_string(),
        idx,
        mod_seq,
        created_ms,
        subagent_id,
        payload,
    ))?;
    Ok(())
}

pub(crate) fn insert_or_update_entries_and_trim(
    connection: &Connection,
    session_id: &str,
    rows: Vec<EntryRow>,
    trim_from_idx: i64,
) -> Result<()> {
    // One savepoint so a failure part-way through cannot leave the session with
    // some of the flush applied — a half-applied flush is indistinguishable on
    // the next cold load from a genuinely shorter transcript.
    let tx = connection.with_savepoint("upsert_entries_and_trim", || {
        {
            let mut upsert = connection
                .exec_bound::<(String, i64, i64, i64, Option<String>, Vec<u8>)>(UPSERT_ENTRY_SQL)?;
            for row in rows {
                upsert((
                    session_id.to_string(),
                    row.idx,
                    row.mod_seq,
                    row.created_ms,
                    row.subagent_id,
                    row.payload,
                ))?;
            }
        }
        delete_entries_from_idx(connection, session_id, trim_from_idx)
    });
    tx.map_err(|e| anyhow!("upsert_entries_and_trim failed: {e}"))
}

pub(crate) fn select_entries_for_session(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<EntryRow>> {
    let mut stmt =
        connection.select_bound::<String, (i64, i64, i64, Option<String>, Vec<u8>)>(indoc! {"
        SELECT idx, mod_seq, created_ms, subagent_id, payload
        FROM   solution_session_entries
        WHERE  session_id = ?
        ORDER BY idx ASC
    "})?;
    let rows = stmt(session_id.to_string())?;
    Ok(rows
        .into_iter()
        .map(
            |(idx, mod_seq, created_ms, subagent_id, payload)| EntryRow {
                idx,
                mod_seq,
                created_ms,
                subagent_id,
                payload,
            },
        )
        .collect())
}

pub(crate) fn delete_entries_from_idx(
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

pub(crate) fn delete_all_entries_for_session(
    connection: &Connection,
    session_id: &str,
) -> Result<()> {
    let mut stmt = connection.exec_bound::<String>(indoc! {"
        DELETE FROM solution_session_entries
        WHERE session_id = ?
    "})?;
    stmt(session_id.to_string())?;
    Ok(())
}
