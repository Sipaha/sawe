use anyhow::Result;
use gpui::Task;
use indoc::indoc;
use sqlez::connection::Connection;

use crate::db::{EntryRow, SolutionAgentDb};
use crate::model::SolutionSessionId;

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
    let mut stmt =
        connection.exec_bound::<(String, i64, i64, i64, Option<String>, Vec<u8>)>(indoc! {"
        INSERT INTO solution_session_entries
            (session_id, idx, mod_seq, created_ms, subagent_id, payload)
        VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(session_id, idx) DO UPDATE SET
            mod_seq     = excluded.mod_seq,
            created_ms  = excluded.created_ms,
            subagent_id = excluded.subagent_id,
            payload     = excluded.payload
    "})?;
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

pub(crate) fn select_entries_for_session(connection: &Connection, session_id: &str) -> Result<Vec<EntryRow>> {
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

pub(crate) fn delete_entries_from_idx(connection: &Connection, session_id: &str, from_idx: i64) -> Result<()> {
    let mut stmt = connection.exec_bound::<(String, i64)>(indoc! {"
        DELETE FROM solution_session_entries
        WHERE session_id = ? AND idx >= ?
    "})?;
    stmt((session_id.to_string(), from_idx))?;
    Ok(())
}

pub(crate) fn delete_all_entries_for_session(connection: &Connection, session_id: &str) -> Result<()> {
    let mut stmt = connection.exec_bound::<String>(indoc! {"
        DELETE FROM solution_session_entries
        WHERE session_id = ?
    "})?;
    stmt(session_id.to_string())?;
    Ok(())
}
