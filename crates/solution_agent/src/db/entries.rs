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
    /// separately-awaited trim. Between the last upsert and the trim the table
    /// holds the new rows AND the stale tail beyond them — a fresh head followed
    /// by a stale tail, which cold load accepts as authoritative under EITHER
    /// branch of `model::hydrate_streams_main_only`'s `entries.len() == main_len`
    /// check, so neither branch rescues the reader:
    ///
    /// - the stale tail is normally untagged too (the write that left it was
    ///   itself a Main-local flush), so `stream::demux` routes every row into
    ///   Main and the counts MATCH — the layout is not read as legacy at all,
    ///   the watermark is seeded from the spliced transcript, no realign is
    ///   armed, and the splice is authoritative from then on;
    /// - if `Stream::push_coalesced` does merge across the head/tail seam the
    ///   counts differ and legacy IS detected — which only means
    ///   `persisted_main_seq` is seeded to 0, so the full rewrite that arms
    ///   writes the spliced transcript back permanently.
    ///
    /// The order within the closure is the order the callers issued it in:
    /// upserts first, trim last.
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
                false,
            )
        })
    }

    /// [`Self::upsert_entries_and_trim`] for a CONTEXT WIPE (`/clear`,
    /// `/compact`), which additionally drops the legacy `acp_thread_blob`.
    ///
    /// Why it has to be the same savepoint: the store consults the blob exactly
    /// when a session has no entry rows, so "rows deleted, blob kept" is the one
    /// intermediate state that resurrects the transcript the user just wiped.
    /// Rows-and-blob or neither; a failure part-way through must leave the
    /// pre-wipe state intact rather than half of it. (Which readers do that is
    /// the store's business and is not enumerated here — see
    /// `store::hydration::is_wiped_row_native`.)
    pub fn upsert_entries_trim_and_clear_blob(
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
                true,
            )
        })
    }

    /// Rename `solution_session_entries` away so every subsequent entry write FAILS.
    ///
    /// Test-only, and the only way to observe the "an epoch must never outrun
    /// the row write it describes" contract in `persist_all_rows_inner` /
    /// `persist_main_stream`: on an in-memory sqlite there is otherwise no way
    /// to make a write fail on demand, and the state a violation manufactures
    /// (no rows + `epoch > 0`) is exactly what `is_wiped_row_native` reads as a
    /// wipe — i.e. it would make a genuinely un-migrated blob unreadable.
    /// `solution_sessions` is left intact so the epoch is still observable.
    ///
    /// Blast radius is per TEST THREAD, not per handle: under `test`/
    /// `test-support`, `SolutionAgentDb::open` names its in-memory database
    /// `SOLUTION_AGENT_TEST_<thread name>`, so every handle opened on the same
    /// thread shares one database and sees the table go missing. A later `open`
    /// on that thread would also re-create an EMPTY `solution_session_entries`
    /// (`open_connection` issues `CREATE TABLE IF NOT EXISTS`), which both
    /// un-breaks writes behind the test's back and makes
    /// [`Self::restore_entry_writes_for_test`] fail. Call it once, after every
    /// handle the test needs, and do not use it in a test that reopens.
    ///
    /// Renames rather than drops, so the failure is REVERSIBLE and the existing
    /// rows survive it — a test can model a transient I/O error and then assert
    /// what the recovered flush wrote, which is the only way to observe that a
    /// failed write did not permanently truncate the transcript.
    #[cfg(any(test, feature = "test-support"))]
    pub fn break_entry_writes_for_test(&self) -> Result<()> {
        let connection = self.connection.lock();
        connection.exec(
            "ALTER TABLE solution_session_entries RENAME TO solution_session_entries_broken",
        )?()?;
        Ok(())
    }

    /// Undo [`Self::break_entry_writes_for_test`], rows and indexes intact.
    #[cfg(any(test, feature = "test-support"))]
    pub fn restore_entry_writes_for_test(&self) -> Result<()> {
        let connection = self.connection.lock();
        connection.exec(
            "ALTER TABLE solution_session_entries_broken RENAME TO solution_session_entries",
        )?()?;
        Ok(())
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
        #[cfg(any(test, feature = "test-support"))]
        let entry_load_count = self.entry_load_count.clone();
        self.executor.spawn(async move {
            #[cfg(any(test, feature = "test-support"))]
            entry_load_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let connection = connection.lock();
            select_entries_for_session(&connection, &session_id.to_string())
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
    clear_legacy_blob: bool,
) -> Result<()> {
    // A row at or past the trim would be written and then deleted again by the
    // closure below. Both persist sites derive `idx` from an `enumerate()` over
    // the very slice whose length they pass as `trim_from_idx`, so this holds by
    // construction — and it is the sole reason upsert-before-trim is the safe
    // order. Reversed, such a row would SURVIVE the trim as an unreachable stale
    // tail instead of being swallowed.
    debug_assert!(
        rows.iter().all(|row| row.idx < trim_from_idx),
        "upsert_entries_and_trim would swallow its own row: max idx {:?} >= trim {trim_from_idx}",
        rows.iter().map(|row| row.idx).max(),
    );
    // One savepoint so a failure part-way through cannot leave the session with
    // some of the flush applied. Growing, that would merely look like a shorter
    // transcript on the next cold load. Shrinking, it is the fresh-head-over-
    // stale-tail splice the folded trim exists to prevent — so the upserts and
    // the trim have to stand or fall together.
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
        delete_entries_from_idx(connection, session_id, trim_from_idx)?;
        if clear_legacy_blob {
            super::sessions::clear_blob_by_id(connection, session_id)?;
        }
        Ok(())
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

/// The trim half of a flush. Reachable only from
/// [`SolutionAgentDb::upsert_entries_and_trim`]: a standalone `delete_entries_from`
/// task existed until the trim was folded into the batched write, and it is not
/// coming back — a trim awaited separately from the upserts that shrink the row
/// set is exactly the torn write that batching removed.
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
