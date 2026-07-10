use anyhow::Result;
use gpui::Task;
use indoc::indoc;
use sqlez::connection::Connection;

use crate::db::SolutionAgentDb;

impl SolutionAgentDb {
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
}

pub(crate) fn insert_attachment(
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

pub(crate) fn select_attachment_paths_for_session(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<String>> {
    let mut stmt = connection.select_bound::<String, String>(indoc! {"
        SELECT path FROM solution_session_attachment WHERE session_id = ?
    "})?;
    stmt(session_id.to_string())
}

pub(crate) fn select_attachment_paths_for_solution(
    connection: &Connection,
    solution_id: &str,
) -> Result<Vec<String>> {
    let mut stmt = connection.select_bound::<String, String>(indoc! {"
        SELECT path FROM solution_session_attachment WHERE solution_id = ?
    "})?;
    stmt(solution_id.to_string())
}

pub(crate) fn delete_attachments_for_session_fn(connection: &Connection, session_id: &str) -> Result<()> {
    let mut stmt = connection.exec_bound::<String>(indoc! {"
        DELETE FROM solution_session_attachment WHERE session_id = ?
    "})?;
    stmt(session_id.to_string())?;
    Ok(())
}
