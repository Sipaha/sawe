use anyhow::Result;
use gpui::Task;
use indoc::indoc;
use sqlez::connection::Connection;

use crate::db::{BackgroundAgentRow, BackgroundShellRow, SolutionAgentDb};

impl SolutionAgentDb {
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
}

pub(crate) fn insert_or_update_background_agent(
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

pub(crate) fn select_background_agents_for_session(
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

pub(crate) fn delete_background_agent_by_id(
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

pub(crate) fn insert_or_update_background_shell(
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

pub(crate) fn select_background_shells_for_session(
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

pub(crate) fn delete_background_shell_by_id(
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

pub(crate) fn delete_background_shells_for_session(
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
