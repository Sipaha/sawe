//! Cold reconcile of a folder move: the heavy path rewiring that the hot
//! rename deliberately skips. Runs at startup, before any window opens, when
//! nothing holds the old paths any more.
//!
//! Every step is idempotent — re-running a partially applied migration is a
//! no-op for the parts that already landed — so a crash mid-reconcile is
//! recovered by simply running it again on the next start.

use anyhow::{Context as _, Result};
use db::sqlez::connection::Connection;
use std::path::PathBuf;
use util::path_list::{PathList, SerializedPathList};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRewrite {
    pub old: PathBuf,
    pub new: PathBuf,
}

impl PathRewrite {
    /// `Some(rewritten)` when `value` is the old path or lives under it;
    /// `None` otherwise. A sibling with a longer name (`/base/older` vs
    /// `/base/old`) must not match — hence the explicit separator check.
    pub fn apply_str(&self, value: &str) -> Option<String> {
        let old = self.old.to_string_lossy();
        let new = self.new.to_string_lossy();
        if value == old {
            return Some(new.into_owned());
        }
        let suffix = value.strip_prefix(old.as_ref())?;
        let suffix = suffix.strip_prefix(std::path::MAIN_SEPARATOR)?;
        Some(format!("{new}{}{suffix}", std::path::MAIN_SEPARATOR))
    }

    /// The blob columns (`editors.path`, `terminals.working_directory`) hold
    /// raw OS-string bytes, which are not guaranteed to be UTF-8.
    pub fn apply_bytes(&self, value: &[u8]) -> Option<Vec<u8>> {
        let old = self.old.to_string_lossy();
        let new = self.new.to_string_lossy();
        let (old, new) = (old.as_bytes(), new.as_bytes());
        let separator = std::path::MAIN_SEPARATOR as u8;
        if value == old {
            return Some(new.to_vec());
        }
        if value.len() > old.len()
            && value.starts_with(old)
            && value.get(old.len()) == Some(&separator)
        {
            let mut out = new.to_vec();
            out.extend_from_slice(&value[old.len()..]);
            return Some(out);
        }
        None
    }
}

/// Rewrite every path-bearing row of the shared `AppDatabase` file
/// (`solutions`, `solution_members`, `workspaces`, `console_panel_state`,
/// `editors`, `terminals`, `breakpoints`, `bookmarks`, `trusted_worktrees`)
/// and drop the toolchain rows whose key is a stale path.
///
/// Tables are probed before they are touched: the domains that own them
/// (`WorkspaceDb`, `EditorDb`, `TerminalDb`) migrate lazily, so the reconcile
/// can legitimately run against a database where some of them are not there
/// yet. A missing table means "nothing to rewrite", not an error.
pub fn rewrite_app_db(connection: &Connection, rewrite: &PathRewrite) -> Result<()> {
    rewrite_keyed_text_column(connection, "solutions", "id", "root", rewrite)?;
    rewrite_keyed_text_column(connection, "solution_members", "id", "local_path", rewrite)?;
    rewrite_workspaces(connection, rewrite)?;
    rewrite_console_panel_state(connection, rewrite)?;
    rewrite_blob_column(connection, "editors", "path", rewrite)?;
    // `editors.buffer_path` is TEXT (the BLOB `path` is the authoritative one;
    // `buffer_path` is its `CAST(... AS TEXT)` twin) and `editors` is STRICT,
    // so it must be written as text, not as bytes.
    rewrite_text_column(connection, "editors", "buffer_path", rewrite)?;
    rewrite_blob_column(connection, "terminals", "working_directory", rewrite)?;
    rewrite_text_column(connection, "terminals", "working_directory_path", rewrite)?;
    rewrite_text_column(connection, "breakpoints", "path", rewrite)?;
    rewrite_text_column(connection, "bookmarks", "path", rewrite)?;
    rewrite_keyed_text_column(
        connection,
        "trusted_worktrees",
        "trust_id",
        "absolute_path",
        rewrite,
    )?;
    delete_stale_toolchains(connection, rewrite)?;
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
    let rows: Vec<i64> = connection
        .select_bound::<String, i64>(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .context("preparing table probe")?(table.to_string())
    .with_context(|| format!("probing for table {table}"))?;
    Ok(rows.first().copied().unwrap_or(0) > 0)
}

/// Table and column names are compile-time constants of this module, never user
/// input, so interpolating them into the pragma is not an injection surface.
fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    let names: Vec<String> = connection
        .select::<String>(&format!("SELECT name FROM pragma_table_info('{table}')"))
        .with_context(|| format!("preparing column probe on {table}"))?()
    .with_context(|| format!("probing columns of {table}"))?;
    Ok(names.iter().any(|name| name == column))
}

fn has_column(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(table_exists(connection, table)? && column_exists(connection, table, column)?)
}

/// For tables with a single INTEGER primary key, update row by row: matching on
/// the key rather than on the old value keeps the rewrite exact even when two
/// rows share a path.
fn rewrite_keyed_text_column(
    connection: &Connection,
    table: &str,
    key_column: &str,
    path_column: &str,
    rewrite: &PathRewrite,
) -> Result<()> {
    if !has_column(connection, table, path_column)? {
        return Ok(());
    }
    let rows: Vec<(i64, String)> = connection
        .select::<(i64, String)>(&format!(
            "SELECT {key_column}, {path_column} FROM {table} WHERE {path_column} IS NOT NULL"
        ))
        .with_context(|| format!("preparing select on {table}"))?()
    .with_context(|| format!("selecting from {table}"))?;

    let mut update = connection
        .exec_bound::<(String, i64)>(&format!(
            "UPDATE {table} SET {path_column} = ?1 WHERE {key_column} = ?2"
        ))
        .with_context(|| format!("preparing update on {table}"))?;
    for (key, value) in rows {
        if let Some(rewritten) = rewrite.apply_str(&value) {
            update((rewritten, key)).with_context(|| format!("updating {table}"))?;
        }
    }
    Ok(())
}

/// `breakpoints`, `bookmarks`, `editors.buffer_path` and
/// `terminals.working_directory_path` have no single-column key to address a
/// row by, so match on the old value itself. Safe because the value is an
/// absolute path that the rewrite has already made unreachable, and because
/// `apply_str` returns `None` for an already-rewritten value (idempotence).
fn rewrite_text_column(
    connection: &Connection,
    table: &str,
    path_column: &str,
    rewrite: &PathRewrite,
) -> Result<()> {
    if !has_column(connection, table, path_column)? {
        return Ok(());
    }
    let rows: Vec<String> = connection
        .select::<String>(&format!(
            "SELECT DISTINCT {path_column} FROM {table} WHERE {path_column} IS NOT NULL"
        ))
        .with_context(|| format!("preparing select on {table}"))?()
    .with_context(|| format!("selecting from {table}"))?;

    let mut update = connection
        .exec_bound::<(String, String)>(&format!(
            "UPDATE {table} SET {path_column} = ?1 WHERE {path_column} = ?2"
        ))
        .with_context(|| format!("preparing update on {table}"))?;
    for value in rows {
        if let Some(rewritten) = rewrite.apply_str(&value) {
            update((rewritten, value)).with_context(|| format!("updating {table}"))?;
        }
    }
    Ok(())
}

fn rewrite_blob_column(
    connection: &Connection,
    table: &str,
    path_column: &str,
    rewrite: &PathRewrite,
) -> Result<()> {
    if !has_column(connection, table, path_column)? {
        return Ok(());
    }
    let rows: Vec<Vec<u8>> = connection
        .select::<Vec<u8>>(&format!(
            "SELECT DISTINCT {path_column} FROM {table} WHERE {path_column} IS NOT NULL"
        ))
        .with_context(|| format!("preparing select on {table}"))?()
    .with_context(|| format!("selecting from {table}"))?;

    let mut update = connection
        .exec_bound::<(Vec<u8>, Vec<u8>)>(&format!(
            "UPDATE {table} SET {path_column} = ?1 WHERE {path_column} = ?2"
        ))
        .with_context(|| format!("preparing update on {table}"))?;
    for value in rows {
        if let Some(rewritten) = rewrite.apply_bytes(&value) {
            update((rewritten, value)).with_context(|| format!("updating {table}"))?;
        }
    }
    Ok(())
}

fn rewrite_console_panel_state(connection: &Connection, rewrite: &PathRewrite) -> Result<()> {
    if !has_column(connection, "console_panel_state", "cwd")? {
        return Ok(());
    }
    let rows: Vec<(i64, i64, String)> = connection
        .select::<(i64, i64, String)>(
            "SELECT workspace_id, tab_index, cwd FROM console_panel_state WHERE cwd IS NOT NULL",
        )
        .context("preparing select on console_panel_state")?()
    .context("selecting from console_panel_state")?;

    let mut update = connection
        .exec_bound::<(String, i64, i64)>(
            "UPDATE console_panel_state SET cwd = ?1 WHERE workspace_id = ?2 AND tab_index = ?3",
        )
        .context("preparing update on console_panel_state")?;
    for (workspace_id, tab_index, cwd) in rows {
        if let Some(rewritten) = rewrite.apply_str(&cwd) {
            update((rewritten, workspace_id, tab_index))
                .context("updating console_panel_state")?;
        }
    }
    Ok(())
}

/// `workspaces.paths` is the window's identity key (UNIQUE
/// `ix_workspaces_location`): every pane, tab, dock, editor, terminal and
/// console tab is FK'd on `workspace_id`, so losing the row loses the whole
/// layout. Three subtleties:
///   * `paths` is a `\n`-joined, *lexicographically sorted* list and
///     `paths_order` is the permutation back to the user's order — a rename
///     can change the sort, so round-trip through `PathList` rather than
///     patching the string.
///   * a row may already exist at the target path set (the user opened that
///     directory before). Blindly updating would violate the unique index, so
///     the squatter is deleted (its children cascade) and the *migrating* row
///     keeps its `workspace_id`.
///   * only local workspaces are rewritten: a remote workspace's paths live on
///     another machine and merely happen to be spelled the same.
fn rewrite_workspaces(connection: &Connection, rewrite: &PathRewrite) -> Result<()> {
    if !has_column(connection, "workspaces", "paths")? {
        return Ok(());
    }
    let has_identity = column_exists(connection, "workspaces", "identity_paths")?;
    type Row = (
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let select = if has_identity {
        "SELECT workspace_id, paths, paths_order, identity_paths, identity_paths_order
         FROM workspaces WHERE remote_connection_id IS NULL"
    } else {
        "SELECT workspace_id, paths, paths_order, NULL, NULL
         FROM workspaces WHERE remote_connection_id IS NULL"
    };
    let rows: Vec<Row> = connection
        .select::<Row>(select)
        .context("preparing select on workspaces")?()
    .context("selecting from workspaces")?;

    let mut delete_conflict = connection
        .exec_bound::<i64>("DELETE FROM workspaces WHERE workspace_id = ?")
        .context("preparing workspace delete")?;
    let mut select_conflict = connection
        .select_bound::<String, i64>(
            "SELECT workspace_id FROM workspaces
             WHERE paths = ? AND remote_connection_id IS NULL",
        )
        .context("preparing workspace conflict select")?;
    let mut update_paths = connection
        .exec_bound::<(Option<String>, Option<String>, i64)>(
            "UPDATE workspaces SET paths = ?1, paths_order = ?2 WHERE workspace_id = ?3",
        )
        .context("preparing workspace update")?;

    for (workspace_id, paths, paths_order, identity_paths, identity_paths_order) in rows {
        let rewritten = rewrite_path_list(paths.as_deref(), paths_order.as_deref(), rewrite);
        let rewritten_identity = rewrite_path_list(
            identity_paths.as_deref(),
            identity_paths_order.as_deref(),
            rewrite,
        );
        if rewritten.is_none() && rewritten_identity.is_none() {
            continue;
        }

        if let Some((new_paths, new_order)) = rewritten {
            for conflicting in select_conflict(new_paths.clone())
                .context("selecting conflicting workspace")?
            {
                if conflicting != workspace_id {
                    delete_conflict(conflicting).context("deleting conflicting workspace")?;
                }
            }
            update_paths((Some(new_paths), Some(new_order), workspace_id))
                .context("updating workspace paths")?;
        }

        if has_identity && let Some((new_paths, new_order)) = rewritten_identity {
            let mut update_identity = connection
                .exec_bound::<(Option<String>, Option<String>, i64)>(
                    "UPDATE workspaces
                     SET identity_paths = ?1, identity_paths_order = ?2
                     WHERE workspace_id = ?3",
                )
                .context("preparing workspace identity update")?;
            update_identity((Some(new_paths), Some(new_order), workspace_id))
                .context("updating workspace identity paths")?;
        }
    }
    Ok(())
}

/// `None` when nothing in the list is under the old path.
fn rewrite_path_list(
    paths: Option<&str>,
    order: Option<&str>,
    rewrite: &PathRewrite,
) -> Option<(String, String)> {
    let paths = paths?;
    if paths.is_empty() {
        return None;
    }
    let list = PathList::deserialize(&SerializedPathList {
        paths: paths.to_string(),
        order: order.unwrap_or_default().to_string(),
    });
    let mut changed = false;
    let rewritten: Vec<PathBuf> = list
        .ordered_paths()
        .map(
            |path| match rewrite.apply_str(&path.to_string_lossy()) {
                Some(new) => {
                    changed = true;
                    PathBuf::from(new)
                }
                None => path.clone(),
            },
        )
        .collect();
    if !changed {
        return None;
    }
    let serialized = PathList::new(&rewritten).serialize();
    Some((serialized.paths, serialized.order))
}

/// The worktree root path is part of the toolchain primary key and the
/// toolchain itself is re-detected on the next open, so a stale row is deleted
/// rather than rewritten.
fn delete_stale_toolchains(connection: &Connection, rewrite: &PathRewrite) -> Result<()> {
    let old = rewrite.old.to_string_lossy().into_owned();
    let prefix = format!("{old}{}", std::path::MAIN_SEPARATOR);
    // `LIKE` treats `%` and `_` as wildcards, and a real path may contain `_`.
    // Comparing the prefix with `substr` keeps the match exact.
    for table in ["toolchains", "user_toolchains"] {
        if !has_column(connection, table, "worktree_root_path")? {
            continue;
        }
        let mut delete = connection
            .exec_bound::<(String, String, i64)>(&format!(
                "DELETE FROM {table}
                 WHERE worktree_root_path = ?1 OR substr(worktree_root_path, 1, ?3) = ?2"
            ))
            .with_context(|| format!("preparing delete on {table}"))?;
        // `substr` counts *characters*, not bytes.
        let prefix_length = prefix.chars().count() as i64;
        delete((old.clone(), prefix.clone(), prefix_length))
            .with_context(|| format!("deleting stale rows from {table}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::sqlez::connection::Connection;

    /// Mirrors the *real* shipped schema of the shared `AppDatabase` file, not
    /// the one the plan sketched: the terminal table is `terminals` (the
    /// `terminals2` name was renamed away by a later upstream migration) and it
    /// carries both the BLOB `working_directory` and its TEXT twin
    /// `working_directory_path`; `editors.buffer_path` is TEXT, not BLOB.
    fn seed(connection: &Connection) {
        connection
            .exec(
                "CREATE TABLE solutions (id INTEGER PRIMARY KEY, name TEXT, root TEXT, last_opened_at INTEGER);
                 CREATE TABLE solution_members (id INTEGER PRIMARY KEY, solution_id INTEGER, name TEXT, local_path TEXT, position INTEGER, origin_catalog_id INTEGER);
                 CREATE TABLE workspaces (workspace_id INTEGER PRIMARY KEY, paths TEXT, paths_order TEXT, identity_paths TEXT, identity_paths_order TEXT, remote_connection_id INTEGER);
                 CREATE TABLE console_panel_state (workspace_id INTEGER, tab_index INTEGER, cwd TEXT);
                 CREATE TABLE editors (item_id INTEGER, workspace_id INTEGER, path BLOB, buffer_path TEXT);
                 CREATE TABLE terminals (workspace_id INTEGER, item_id INTEGER, working_directory BLOB, working_directory_path TEXT);
                 CREATE TABLE breakpoints (workspace_id INTEGER, path TEXT, breakpoint_location INTEGER);
                 CREATE TABLE bookmarks (workspace_id INTEGER, path TEXT, row INTEGER);
                 CREATE TABLE trusted_worktrees (trust_id INTEGER PRIMARY KEY, absolute_path TEXT, user_name TEXT, host_name TEXT);
                 CREATE TABLE toolchains (workspace_id INTEGER, worktree_root_path TEXT, language_name TEXT, name TEXT, path TEXT, raw_json TEXT, relative_worktree_path TEXT);
                 CREATE TABLE user_toolchains (remote_connection_id INTEGER, workspace_id INTEGER, worktree_root_path TEXT, relative_worktree_path TEXT, language_name TEXT, name TEXT, path TEXT, raw_json TEXT);",
            )
            .expect("prepare schema")()
        .expect("create schema");

        // A statement that depends on a table created by an earlier statement
        // cannot be prepared in the same batch (sqlez prepares them all up
        // front), so the index goes in its own call.
        connection
            .exec(
                "CREATE UNIQUE INDEX ix_workspaces_location
                     ON workspaces(remote_connection_id, paths);",
            )
            .expect("prepare index")()
        .expect("create index");

        connection
            .exec(
                "INSERT INTO solutions VALUES (1, 'Sol', '/base/old', NULL);
                 INSERT INTO solution_members VALUES (1, 1, 'Member', '/base/old/member', 0, NULL);
                 INSERT INTO workspaces VALUES (7, '/base/old/member', '0', '/base/old/member', '0', NULL);
                 INSERT INTO console_panel_state VALUES (7, 0, '/base/old/member');
                 INSERT INTO breakpoints VALUES (7, '/base/old/member/src/main.rs', 3);
                 INSERT INTO bookmarks VALUES (7, '/base/old/member/src/main.rs', 9);
                 INSERT INTO trusted_worktrees VALUES (1, '/base/old/member', NULL, NULL);
                 INSERT INTO toolchains VALUES (7, '/base/old/member', 'Rust', 'stable', '/usr/bin/cargo', '{}', '');
                 INSERT INTO user_toolchains VALUES (NULL, 7, '/base/old/member', '', 'Rust', 'stable', '/usr/bin/cargo', '{}');",
            )
            .expect("prepare seed")()
        .expect("seed rows");

        // Hostile neighbours: `/base/older` shares a *string* prefix with
        // `/base/old` but is a different directory and must never be touched.
        connection
            .exec(
                "INSERT INTO solutions VALUES (2, 'Other', '/base/older', NULL);
                 INSERT INTO solution_members VALUES (2, 2, 'Other', '/base/older/member', 0, NULL);
                 INSERT INTO workspaces VALUES (8, '/base/older/member', '0', NULL, NULL, NULL);
                 INSERT INTO console_panel_state VALUES (8, 0, '/base/older/member');
                 INSERT INTO breakpoints VALUES (8, '/base/older/member/src/main.rs', 3);
                 INSERT INTO bookmarks VALUES (8, '/base/older/member/src/main.rs', 9);
                 INSERT INTO trusted_worktrees VALUES (2, '/base/older/member', NULL, NULL);
                 INSERT INTO toolchains VALUES (8, '/base/older/member', 'Rust', 'stable', '/usr/bin/cargo', '{}', '');
                 INSERT INTO user_toolchains VALUES (NULL, 8, '/base/older/member', '', 'Rust', 'stable', '/usr/bin/cargo', '{}');",
            )
            .expect("prepare hostile seed")()
        .expect("seed hostile rows");

        let mut insert_editor = connection
            .exec_bound::<(i64, i64, Vec<u8>, String)>(
                "INSERT INTO editors (item_id, workspace_id, path, buffer_path) VALUES (?, ?, ?, ?)",
            )
            .expect("prepare editors");
        insert_editor((
            1,
            7,
            b"/base/old/member/src/main.rs".to_vec(),
            "/base/old/member/src/main.rs".to_string(),
        ))
        .expect("insert editor");
        insert_editor((
            2,
            8,
            b"/base/older/member/src/main.rs".to_vec(),
            "/base/older/member/src/main.rs".to_string(),
        ))
        .expect("insert hostile editor");

        let mut insert_terminal = connection
            .exec_bound::<(i64, i64, Vec<u8>, String)>(
                "INSERT INTO terminals (workspace_id, item_id, working_directory, working_directory_path)
                 VALUES (?, ?, ?, ?)",
            )
            .expect("prepare terminals");
        insert_terminal((
            7,
            1,
            b"/base/old/member".to_vec(),
            "/base/old/member".to_string(),
        ))
        .expect("insert terminal");
        insert_terminal((
            8,
            1,
            b"/base/older/member".to_vec(),
            "/base/older/member".to_string(),
        ))
        .expect("insert hostile terminal");
    }

    fn rewrite() -> PathRewrite {
        PathRewrite {
            old: PathBuf::from("/base/old"),
            new: PathBuf::from("/base/new"),
        }
    }

    fn text(connection: &Connection, query: &str) -> Vec<String> {
        connection.select::<String>(query).expect("prepare")().expect("select")
    }

    fn blobs(connection: &Connection, query: &str) -> Vec<Vec<u8>> {
        connection.select::<Vec<u8>>(query).expect("prepare")().expect("select")
    }

    fn counts(connection: &Connection, query: &str) -> Vec<i64> {
        connection.select::<i64>(query).expect("prepare")().expect("select")
    }

    #[test]
    fn apply_str_rewrites_the_path_and_its_descendants_only() {
        let rewrite = rewrite();
        assert_eq!(rewrite.apply_str("/base/old").as_deref(), Some("/base/new"));
        assert_eq!(
            rewrite.apply_str("/base/old/member/src/main.rs").as_deref(),
            Some("/base/new/member/src/main.rs")
        );
        assert_eq!(rewrite.apply_str("/base/older"), None);
        assert_eq!(rewrite.apply_str("/base/older/member"), None);
        assert_eq!(rewrite.apply_str("/base/other"), None);
        assert_eq!(rewrite.apply_str("/base/new/member"), None);
        assert_eq!(rewrite.apply_str(""), None);
    }

    #[test]
    fn apply_bytes_rewrites_the_path_and_its_descendants_only() {
        let rewrite = rewrite();
        assert_eq!(
            rewrite.apply_bytes(b"/base/old").as_deref(),
            Some(&b"/base/new"[..])
        );
        assert_eq!(
            rewrite.apply_bytes(b"/base/old/member").as_deref(),
            Some(&b"/base/new/member"[..])
        );
        assert_eq!(rewrite.apply_bytes(b"/base/older"), None);
        assert_eq!(rewrite.apply_bytes(b"/base/oldish/x"), None);
        assert_eq!(rewrite.apply_bytes(b"/base/new/member"), None);
    }

    #[test]
    fn rewrites_every_path_bearing_row() {
        let connection = Connection::open_memory(Some("rewrites_every_path_bearing_row"));
        seed(&connection);

        rewrite_app_db(&connection, &rewrite()).expect("rewrite");

        assert_eq!(
            text(&connection, "SELECT root FROM solutions WHERE id = 1"),
            vec!["/base/new"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT local_path FROM solution_members WHERE id = 1"
            ),
            vec!["/base/new/member"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT paths FROM workspaces WHERE workspace_id = 7"
            ),
            vec!["/base/new/member"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT identity_paths FROM workspaces WHERE workspace_id = 7"
            ),
            vec!["/base/new/member"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT cwd FROM console_panel_state WHERE workspace_id = 7"
            ),
            vec!["/base/new/member"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT path FROM breakpoints WHERE workspace_id = 7"
            ),
            vec!["/base/new/member/src/main.rs"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT path FROM bookmarks WHERE workspace_id = 7"
            ),
            vec!["/base/new/member/src/main.rs"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT absolute_path FROM trusted_worktrees WHERE trust_id = 1"
            ),
            vec!["/base/new/member"]
        );
        assert_eq!(
            blobs(&connection, "SELECT path FROM editors WHERE item_id = 1"),
            vec![b"/base/new/member/src/main.rs".to_vec()]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT buffer_path FROM editors WHERE item_id = 1"
            ),
            vec!["/base/new/member/src/main.rs"]
        );
        assert_eq!(
            blobs(
                &connection,
                "SELECT working_directory FROM terminals WHERE workspace_id = 7"
            ),
            vec![b"/base/new/member".to_vec()]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT working_directory_path FROM terminals WHERE workspace_id = 7"
            ),
            vec!["/base/new/member"]
        );

        assert_eq!(
            counts(
                &connection,
                "SELECT COUNT(*) FROM toolchains WHERE workspace_id = 7"
            ),
            vec![0],
            "stale toolchain rows are deleted, not rewritten"
        );
        assert_eq!(
            counts(
                &connection,
                "SELECT COUNT(*) FROM user_toolchains WHERE workspace_id = 7"
            ),
            vec![0]
        );
    }

    #[test]
    fn a_sibling_sharing_a_string_prefix_is_never_touched() {
        let connection = Connection::open_memory(Some("sibling_sharing_a_string_prefix"));
        seed(&connection);

        rewrite_app_db(&connection, &rewrite()).expect("rewrite");

        assert_eq!(
            text(&connection, "SELECT root FROM solutions WHERE id = 2"),
            vec!["/base/older"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT local_path FROM solution_members WHERE id = 2"
            ),
            vec!["/base/older/member"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT paths FROM workspaces WHERE workspace_id = 8"
            ),
            vec!["/base/older/member"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT cwd FROM console_panel_state WHERE workspace_id = 8"
            ),
            vec!["/base/older/member"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT path FROM breakpoints WHERE workspace_id = 8"
            ),
            vec!["/base/older/member/src/main.rs"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT path FROM bookmarks WHERE workspace_id = 8"
            ),
            vec!["/base/older/member/src/main.rs"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT absolute_path FROM trusted_worktrees WHERE trust_id = 2"
            ),
            vec!["/base/older/member"]
        );
        assert_eq!(
            blobs(&connection, "SELECT path FROM editors WHERE item_id = 2"),
            vec![b"/base/older/member/src/main.rs".to_vec()]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT buffer_path FROM editors WHERE item_id = 2"
            ),
            vec!["/base/older/member/src/main.rs"]
        );
        assert_eq!(
            blobs(
                &connection,
                "SELECT working_directory FROM terminals WHERE workspace_id = 8"
            ),
            vec![b"/base/older/member".to_vec()]
        );
        assert_eq!(
            counts(
                &connection,
                "SELECT COUNT(*) FROM toolchains WHERE workspace_id = 8"
            ),
            vec![1],
            "an unrelated toolchain row survives"
        );
        assert_eq!(
            counts(
                &connection,
                "SELECT COUNT(*) FROM user_toolchains WHERE workspace_id = 8"
            ),
            vec![1]
        );
    }

    #[test]
    fn workspace_identity_row_is_preserved_and_the_squatter_is_merged_away() {
        let connection =
            Connection::open_memory(Some("workspace_identity_row_is_preserved_and_merged"));
        seed(&connection);
        // A second workspace row already sits at the *target* path set — the
        // UNIQUE ix_workspaces_location would reject a blind UPDATE.
        connection
            .exec("INSERT INTO workspaces VALUES (9, '/base/new/member', '0', NULL, NULL, NULL);")
            .expect("prepare")()
        .expect("insert squatter");

        rewrite_app_db(&connection, &rewrite()).expect("rewrite");

        let ids: Vec<i64> = connection
            .select::<i64>("SELECT workspace_id FROM workspaces ORDER BY workspace_id")
            .expect("prepare")()
        .expect("select");
        assert_eq!(
            ids,
            vec![7, 8],
            "the migrating row keeps its workspace_id; the row already at the target is merged away"
        );
        assert_eq!(
            text(
                &connection,
                "SELECT paths FROM workspaces WHERE workspace_id = 7"
            ),
            vec!["/base/new/member"]
        );
    }

    #[test]
    fn rewriting_twice_is_a_no_op() {
        let connection = Connection::open_memory(Some("rewriting_twice_is_a_no_op"));
        seed(&connection);

        rewrite_app_db(&connection, &rewrite()).expect("first");
        rewrite_app_db(&connection, &rewrite()).expect("second");

        assert_eq!(
            text(
                &connection,
                "SELECT paths FROM workspaces WHERE workspace_id = 7"
            ),
            vec!["/base/new/member"]
        );
        assert_eq!(
            text(&connection, "SELECT root FROM solutions WHERE id = 1"),
            vec!["/base/new"]
        );
        assert_eq!(
            counts(&connection, "SELECT COUNT(*) FROM workspaces"),
            vec![2],
            "a second pass must not duplicate or drop rows"
        );
        assert_eq!(counts(&connection, "SELECT COUNT(*) FROM editors"), vec![2]);
    }

    #[test]
    fn a_missing_table_is_not_an_error() {
        // The reconcile runs against whatever schema version the app database
        // happens to be at; a table that does not exist yet is simply skipped.
        let connection = Connection::open_memory(Some("a_missing_table_is_not_an_error"));
        connection
            .exec("CREATE TABLE solutions (id INTEGER PRIMARY KEY, name TEXT, root TEXT);")
            .expect("prepare")()
        .expect("create");
        connection
            .exec("INSERT INTO solutions VALUES (1, 'Sol', '/base/old');")
            .expect("prepare")()
        .expect("insert");

        rewrite_app_db(&connection, &rewrite()).expect("rewrite");

        assert_eq!(
            text(&connection, "SELECT root FROM solutions"),
            vec!["/base/new"]
        );
    }
}
