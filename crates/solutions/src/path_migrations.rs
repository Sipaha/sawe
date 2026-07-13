//! Cold reconcile of a folder move: the heavy path rewiring that the hot
//! rename deliberately skips. Runs at startup, before any window opens, when
//! nothing holds the old paths any more.
//!
//! Every step is idempotent — re-running a partially applied migration is a
//! no-op for the parts that already landed — so a crash mid-reconcile is
//! recovered by simply running it again on the next start.

use anyhow::{Context as _, Result};
use db::sqlez::connection::Connection;
use db::sqlez::thread_safe_connection::ThreadSafeConnection;
use gpui::{App, Task};
use std::path::{Path, PathBuf};
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
/// `editors`, `terminals`, `breakpoints`, `bookmarks`, `file_folds`,
/// `vim_marks`, `vim_global_marks_paths`, `image_viewers`, `git_graphs`,
/// `undo_entries`, `trusted_worktrees`) and drop the toolchain rows whose key is
/// a stale path.
///
/// Deliberately *not* rewritten: `shelf_entries`, `branch_favorites`,
/// `branch_recent` and `pre_commit_configs` are keyed by `repo_hash` — a
/// `DefaultHasher` digest of the repo's absolute path, not the path itself — so
/// there is no prefix to rewrite; their rows are simply re-keyed (and orphaned)
/// by a move. The `agent_ui` thread tables hold paths too, but that panel is
/// disabled in this fork and never writes them.
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
    // Fold persistence is keyed by the *file* path (`PRIMARY KEY (workspace_id,
    // path, start)`), independently of the `editors` row — so a folder move
    // orphans it unless the path is rewritten here too.
    rewrite_text_column(connection, "file_folds", "path", rewrite)?;
    rewrite_blob_column(connection, "vim_marks", "path", rewrite)?;
    rewrite_blob_column(connection, "vim_global_marks_paths", "path", rewrite)?;
    rewrite_blob_column(connection, "image_viewers", "image_path", rewrite)?;
    rewrite_text_column(connection, "git_graphs", "repo_working_path", rewrite)?;
    rewrite_text_column(connection, "undo_entries", "repo_path", rewrite)?;
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

/// `breakpoints`, `bookmarks`, `file_folds`, `editors.buffer_path` and
/// `terminals.working_directory_path` have no single-column key to address a
/// row by, so match on the old value itself. Safe because the value is an
/// absolute path that the rewrite has already made unreachable, and because
/// `apply_str` returns `None` for an already-rewritten value (idempotence).
///
/// `OR REPLACE`, because in some of these tables the path is part of a unique
/// key (`file_folds` is `PRIMARY KEY (workspace_id, path, start)`) and a row may
/// already sit at the rewritten path — the user had that same file open under
/// the target directory before. Aborting the whole reconcile over a stale fold
/// row would be far worse than dropping it; the migrating row wins.
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
            "UPDATE OR REPLACE {table} SET {path_column} = ?1 WHERE {path_column} = ?2"
        ))
        .with_context(|| format!("preparing update on {table}"))?;
    for value in rows {
        if let Some(rewritten) = rewrite.apply_str(&value) {
            update((rewritten, value)).with_context(|| format!("updating {table}"))?;
        }
    }
    Ok(())
}

/// Same shape as `rewrite_text_column`, for the columns that hold raw OS-string
/// bytes (`editors.path`, `terminals.working_directory`, `vim_marks.path`,
/// `image_viewers.image_path`). `OR REPLACE` for the same reason: `vim_marks`
/// carries `UNIQUE (workspace_id, mark_name, path)`.
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
            "UPDATE OR REPLACE {table} SET {path_column} = ?1 WHERE {path_column} = ?2"
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

/// Rewrite the path-bearing rows of the `solution_agent` database, which is a
/// *separate* sqlite file from the shared `AppDatabase` one. The `solutions`
/// crate cannot depend on `solution_agent` (that would be a dependency cycle),
/// so the caller opens the file by path and hands the connection in.
pub fn rewrite_agent_db(connection: &Connection, rewrite: &PathRewrite) -> Result<()> {
    if has_column(connection, "solution_sessions", "cwd")? {
        let sessions: Vec<(String, String)> = connection
            .select::<(String, String)>("SELECT id, cwd FROM solution_sessions WHERE cwd IS NOT NULL")
            .context("preparing select on solution_sessions")?()
        .context("selecting from solution_sessions")?;
        let mut update_session = connection
            .exec_bound::<(String, String)>("UPDATE solution_sessions SET cwd = ?1 WHERE id = ?2")
            .context("preparing update on solution_sessions")?;
        for (id, cwd) in sessions {
            if let Some(rewritten) = rewrite.apply_str(&cwd) {
                update_session((rewritten, id)).context("updating solution_sessions")?;
            }
        }
    }

    if has_column(
        connection,
        "solution_session_background_agent",
        "jsonl_path",
    )? {
        let agents: Vec<(String, String, String)> = connection
            .select::<(String, String, String)>(
                "SELECT solution_session_id, agent_id, jsonl_path
                 FROM solution_session_background_agent
                 WHERE jsonl_path IS NOT NULL",
            )
            .context("preparing select on solution_session_background_agent")?()
        .context("selecting from solution_session_background_agent")?;
        let mut update_agent = connection
            .exec_bound::<(String, String, String)>(
                "UPDATE solution_session_background_agent SET jsonl_path = ?1
                 WHERE solution_session_id = ?2 AND agent_id = ?3",
            )
            .context("preparing update on solution_session_background_agent")?;
        for (session_id, agent_id, jsonl_path) in agents {
            if let Some(rewritten) = rewrite.apply_str(&jsonl_path) {
                update_agent((rewritten, session_id, agent_id))
                    .context("updating solution_session_background_agent")?;
            }
        }
    }

    // `path` is part of the attachment primary key, so an UPDATE would have to
    // move the key — delete + reinsert instead. `INSERT OR IGNORE` (not
    // `INSERT OR REPLACE`, which would delete a conflicting parent row) keeps a
    // row that already sits at the rewritten path.
    if has_column(connection, "solution_session_attachment", "path")? {
        let attachments: Vec<(String, String, String, i64)> = connection
            .select::<(String, String, String, i64)>(
                "SELECT session_id, solution_id, path, created_at_ms
                 FROM solution_session_attachment
                 WHERE path IS NOT NULL",
            )
            .context("preparing select on solution_session_attachment")?()
        .context("selecting from solution_session_attachment")?;
        let mut delete_attachment = connection
            .exec_bound::<(String, String)>(
                "DELETE FROM solution_session_attachment WHERE session_id = ?1 AND path = ?2",
            )
            .context("preparing delete on solution_session_attachment")?;
        let mut insert_attachment = connection
            .exec_bound::<(String, String, String, i64)>(
                "INSERT OR IGNORE INTO solution_session_attachment
                     (session_id, solution_id, path, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .context("preparing insert on solution_session_attachment")?;
        for (session_id, solution_id, path, created_at_ms) in attachments {
            if let Some(rewritten) = rewrite.apply_str(&path) {
                delete_attachment((session_id.clone(), path))
                    .context("deleting solution_session_attachment")?;
                insert_attachment((session_id, solution_id, rewritten, created_at_ms))
                    .context("reinserting solution_session_attachment")?;
            }
        }
    }
    Ok(())
}

/// claude keys its transcript bucket by the session cwd with every `/` and `.`
/// replaced by `-` (`<CLAUDE_CONFIG_DIR|~/.claude>/projects/<enc(cwd)>`).
/// Mirrors `solution_agent::store::teammate_reconciler::claude_project_dir_for`;
/// there is no setting that overrides the encoding.
pub fn encode_claude_bucket(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut encoded = String::with_capacity(raw.len());
    for character in raw.chars() {
        match character {
            '/' | '.' => encoded.push('-'),
            other => encoded.push(other),
        }
    }
    encoded
}

/// Move the transcript bucket of the moved directory to the bucket its new path
/// encodes to. When the target bucket already exists (the user had opened a
/// directory of that name before), the two are merged file-by-file instead of
/// renaming over it — a `rename(2)` onto a non-empty directory would fail, and
/// a forced one would destroy transcripts.
pub fn move_transcript_bucket(claude_projects_dir: &Path, rewrite: &PathRewrite) -> Result<()> {
    let source = claude_projects_dir.join(encode_claude_bucket(&rewrite.old));
    if !source.exists() {
        return Ok(());
    }
    let target = claude_projects_dir.join(encode_claude_bucket(&rewrite.new));
    if !target.exists() {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::rename(&source, &target)
            .with_context(|| format!("moving {} to {}", source.display(), target.display()))?;
        return Ok(());
    }
    merge_dir(&source, &target)?;
    std::fs::remove_dir_all(&source)
        .with_context(|| format!("removing drained bucket {}", source.display()))?;
    Ok(())
}

/// Copy every entry of `source` into `target`, never overwriting an existing
/// file. A transcript file we already have at the target is by definition the
/// same session (the session id is the file name), so keeping the target's copy
/// is safe and preserves anything written since the rename.
fn merge_dir(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    for entry in
        std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("reading an entry of {}", source.display()))?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat {}", from.display()))?;
        if file_type.is_dir() {
            merge_dir(&from, &to)?;
        } else if !to.exists() {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// A claude agent worktree is a *linked* git worktree: the tree lives in one
/// place, but its **admin directory always lives inside the member repo** at
/// `<member>/.git/worktrees/<name>/`, and the two point at each other with
/// **absolute** paths. So a rename breaks it in one of two directions:
///
///   * a **member** rename moves the repo (and with it the admin dir) — the
///     tree's `.git` file still names the old admin dir;
///   * a **solution** rename moves the *trees* — the admin dir's `gitdir` file
///     still names the old tree location. (Plan 3's `WorktreeCreate` hook
///     relocates new trees to `<solution_root>/.agents/worktrees/<member-dir>/<name>`;
///     legacy trees are still at `<member>/.claude/worktrees/<name>`. Both are
///     scanned here.)
///
/// `git -C <member> worktree repair <tree paths…>` fixes **both** directions —
/// but only when the moved tree paths are passed as **arguments**. A bare
/// `git worktree repair` can only repair trees it can still reach through the
/// (possibly stale) admin entries, so it silently misses the trees whose
/// location changed.
///
/// Best-effort: a missing/old `git` binary or an unrepairable tree is logged,
/// never fatal — the DB rewrites this reconcile also performs are the part that
/// must not be lost.
pub fn repair_git_worktrees(members: &[(PathBuf, PathBuf)], rewrite: &PathRewrite) -> Result<()> {
    for (member_root, solution_root) in members {
        // Candidate trees, both locations. The `<member-dir>` level under
        // `.agents/worktrees` is itself named after the member's *old* folder
        // after a member rename, so do NOT filter by that name — collect every
        // tree and decide ownership from the tree's own `.git` pointer.
        let mut candidates = Vec::new();
        collect_dirs(
            &member_root.join(".claude").join("worktrees"),
            &mut candidates,
        );
        let relocated = solution_root.join(".agents").join("worktrees");
        let mut member_dirs = Vec::new();
        collect_dirs(&relocated, &mut member_dirs);
        for member_dir in &member_dirs {
            collect_dirs(member_dir, &mut candidates);
        }

        let trees: Vec<PathBuf> = candidates
            .into_iter()
            .filter(|tree| owning_repo(tree, rewrite).as_deref() == Some(member_root.as_path()))
            .collect();
        if trees.is_empty() {
            continue;
        }
        run_worktree_repair(member_root, &trees);
    }
    Ok(())
}

// Sync `std::process::Command` is deliberate: the cold reconcile runs before any
// window exists, `git worktree repair` is a local sub-100ms one-shot, and the
// async `smol::process::Command` the lint suggests would force this whole
// module's sync API to become async for no gain.
#[allow(clippy::disallowed_methods)]
fn run_worktree_repair(member_root: &Path, trees: &[PathBuf]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(member_root)
        .arg("worktree")
        .arg("repair")
        .args(trees)
        .output();
    match output {
        Ok(output) if !output.status.success() => log::warn!(
            "path_migrations: `git worktree repair` in {} failed: {}",
            member_root.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ),
        Err(err) => log::warn!(
            "path_migrations: running `git worktree repair` in {} failed: {err}",
            member_root.display(),
        ),
        Ok(_) => {}
    }
}

fn collect_dirs(parent: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            out.push(entry.path());
        }
    }
}

/// A linked worktree's `.git` is a file reading
/// `gitdir: <repo>/.git/worktrees/<name>` — an **absolute** path that may still
/// name the pre-rename location, hence the rewrite before matching. Returns the
/// repo (member) the tree belongs to.
fn owning_repo(tree: &Path, rewrite: &PathRewrite) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(tree.join(".git")).ok()?;
    let pointer = contents.trim().strip_prefix("gitdir:")?.trim();
    let pointer = rewrite
        .apply_str(pointer)
        .unwrap_or_else(|| pointer.to_string());
    // <repo>/.git/worktrees/<name> → <repo>
    Path::new(&pointer).ancestors().nth(3).map(Path::to_path_buf)
}

pub struct ReconcileContext {
    pub app_db: ThreadSafeConnection,
    /// `None` when the agent DB file does not exist yet (fresh install).
    pub agent_db_path: Option<PathBuf>,
    /// `~/.claude/projects`. `None` when the home directory cannot be resolved.
    pub claude_projects_dir: Option<PathBuf>,
}

impl ReconcileContext {
    fn from_app(cx: &App) -> Self {
        Self {
            app_db: db::AppDatabase::global(cx).clone(),
            agent_db_path: Some(
                paths::data_dir()
                    .join("solution_agent")
                    .join("solution_agent.db"),
            ),
            // Mirrors `solution_agent::store::teammate_reconciler::claude_project_dir_for`,
            // which resolves the bucket root the same way — the two must agree
            // or the moved bucket lands where nothing reads it.
            claude_projects_dir: dirs::home_dir().map(|home| home.join(".claude").join("projects")),
        }
    }
}

/// Apply one recorded move. The app-database writes are funnelled through
/// `ThreadSafeConnection::write` because the thread-local connection a plain
/// deref hands out is deliberately **read-only** (`sqlez` serializes every write
/// onto one worker thread), so the whole sequence runs inside that callback.
pub fn apply_one(context: &ReconcileContext, rewrite: &PathRewrite) -> Result<()> {
    let agent = match context.agent_db_path.as_ref() {
        Some(path) if path.exists() => Some(Connection::open_file(&path.to_string_lossy())),
        _ => None,
    };
    let claude_projects_dir = context.claude_projects_dir.clone();
    let rewrite = rewrite.clone();
    gpui::block_on(context.app_db.write(move |connection| {
        apply_one_with_connections(
            connection,
            agent.as_ref(),
            claude_projects_dir.as_deref(),
            &rewrite,
        )
    }))
}

/// The whole per-migration sequence, on plain connections so it is testable
/// without an `App`. Every step is a no-op when it has already been applied,
/// which is what makes a crash mid-reconcile recoverable by re-running.
pub(crate) fn apply_one_with_connections(
    app_db: &Connection,
    agent_db: Option<&Connection>,
    claude_projects_dir: Option<&Path>,
    rewrite: &PathRewrite,
) -> Result<()> {
    rewrite_app_db(app_db, rewrite)?;
    if let Some(agent_db) = agent_db {
        rewrite_agent_db(agent_db, rewrite)?;
    }

    // `(member_root, solution_root)` for every member that now lives under the
    // moved path. The solution root is needed because the relocated agent
    // worktrees sit at `<solution_root>/.agents/worktrees/<member-dir>/*`,
    // outside the member itself.
    let members: Vec<(PathBuf, PathBuf)> = app_db
        .select::<(String, String)>(
            "SELECT solution_members.local_path, solutions.root
             FROM solution_members
             JOIN solutions ON solutions.id = solution_members.solution_id",
        )
        .context("preparing member select")?()
    .context("selecting members")?
    .into_iter()
    .map(|(member, solution)| (PathBuf::from(member), PathBuf::from(solution)))
    .filter(|(member, _)| member.starts_with(&rewrite.new))
    .collect();

    if let Some(projects) = claude_projects_dir {
        // A bucket is keyed by a *session's* cwd, so a solution rename has to
        // move the bucket of every cwd that lived under the moved root — the
        // members and the agent worktrees — not just the root's own bucket.
        for moved in moved_session_dirs(app_db, agent_db, &members, rewrite)? {
            move_transcript_bucket(projects, &moved)?;
        }
    }

    // Before the repair, not after: while the compat link still stands, the
    // `gitdir:` pointer inside a moved worktree keeps resolving, so git sees
    // nothing to fix and `worktree repair` leaves the stale absolute path in
    // place.
    remove_compat_link(&rewrite.old)?;

    repair_git_worktrees(&members, rewrite)?;
    Ok(())
}

/// Every directory whose transcript bucket the move invalidated: the moved path
/// itself, plus each member and each recorded session cwd that now lives under
/// it. Derived from the databases rather than by prefix-scanning the bucket
/// directory, because claude's encoding maps both `/` and `.` to `-` — so
/// `<old>.bak`'s bucket is indistinguishable from a bucket *under* `<old>` by
/// name alone, and prefix-scanning would move an unrelated project's transcripts.
fn moved_session_dirs(
    app_db: &Connection,
    agent_db: Option<&Connection>,
    members: &[(PathBuf, PathBuf)],
    rewrite: &PathRewrite,
) -> Result<Vec<PathRewrite>> {
    let mut new_paths: Vec<PathBuf> = vec![rewrite.new.clone()];
    new_paths.extend(members.iter().map(|(member, _)| member.clone()));

    if let Some(agent_db) = agent_db
        && has_column(agent_db, "solution_sessions", "cwd")?
    {
        let cwds: Vec<String> = agent_db
            .select::<String>("SELECT DISTINCT cwd FROM solution_sessions WHERE cwd IS NOT NULL")
            .context("preparing session cwd select")?()
        .context("selecting session cwds")?;
        new_paths.extend(cwds.into_iter().map(PathBuf::from));
    }
    // The solution roots the app DB knows about can also be session cwds.
    let roots: Vec<String> = app_db
        .select::<String>("SELECT root FROM solutions WHERE root IS NOT NULL")
        .context("preparing solution root select")?()
    .context("selecting solution roots")?;
    new_paths.extend(roots.into_iter().map(PathBuf::from));

    new_paths.retain(|path| path.starts_with(&rewrite.new));
    new_paths.sort();
    new_paths.dedup();

    Ok(new_paths
        .into_iter()
        .filter_map(|new| {
            let relative = new.strip_prefix(&rewrite.new).ok()?;
            // `join("")` would append a separator, and the bucket name encodes
            // every separator — so the moved path itself must be used verbatim.
            let old = if relative.as_os_str().is_empty() {
                rewrite.old.clone()
            } else {
                rewrite.old.join(relative)
            };
            Some(PathRewrite { old, new })
        })
        .collect())
}

/// Only ever removes a *symlink*. If the user re-created a real directory at the
/// old path after the rename, it is theirs and must survive.
fn remove_compat_link(old: &Path) -> Result<()> {
    match std::fs::symlink_metadata(old) {
        Err(_) => Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::remove_file(old)
            .with_context(|| format!("removing the compat link at {}", old.display())),
        Ok(_) => {
            log::warn!(
                "path_migrations: {} is a real directory, not our compat link — leaving it alone",
                old.display()
            );
            Ok(())
        }
    }
}

/// Drain `pending_path_migrations` and apply every recorded move. Called from
/// `SolutionStore::init_with_db` **before the store is hydrated and before any
/// window opens** — nothing is live, so this is the moment to rewrite the paths
/// that the hot rename deliberately left stale.
pub fn drain_and_apply(cx: &mut App) -> Task<Result<()>> {
    let db = crate::db::SolutionsDb::global(cx);
    drain_and_apply_with_db(&db, cx)
}

/// The work runs **synchronously on the caller's thread** and the result is
/// handed back as an already-resolved `Task`, rather than being spawned on the
/// background executor: `init_with_db` has to block on it either way (no window
/// may open on a stale path), and a `block_on` of a *background-spawned* task
/// deadlocks under GPUI's deterministic test executor, which only advances
/// background work when the test pumps it — and `init_global_for_test` runs
/// inside `cx.update`. The sqlite writes still happen on sqlez's own worker
/// thread, which is a real OS thread in both configurations.
pub(crate) fn drain_and_apply_with_db(
    db: &crate::db::SolutionsDb,
    cx: &mut App,
) -> Task<Result<()>> {
    Task::ready(drain_and_apply_blocking(
        db,
        &ReconcileContext::from_app(cx),
    ))
}

fn drain_and_apply_blocking(db: &crate::db::SolutionsDb, context: &ReconcileContext) -> Result<()> {
    let pending = db
        .load_pending_path_migrations()
        .context("loading pending_path_migrations")?;
    for (id, old_path, new_path) in pending {
        let rewrite = PathRewrite {
            old: PathBuf::from(old_path),
            new: PathBuf::from(new_path),
        };
        if let Err(err) = apply_one(context, &rewrite) {
            // Leave the row in place: the next start retries. Every step is
            // idempotent, so a partially applied migration resumes cleanly — and
            // one bad migration must never keep the editor from booting.
            log::error!(
                "path_migrations: reconciling {} → {} failed: {err:#}. Will retry on the next start.",
                rewrite.old.display(),
                rewrite.new.display(),
            );
            continue;
        }
        gpui::block_on(db.delete_pending_path_migration(id))
            .context("deleting the drained pending_path_migrations row")?;
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
                 CREATE TABLE user_toolchains (remote_connection_id INTEGER, workspace_id INTEGER, worktree_root_path TEXT, relative_worktree_path TEXT, language_name TEXT, name TEXT, path TEXT, raw_json TEXT);
                 CREATE TABLE file_folds (workspace_id INTEGER NOT NULL, path TEXT NOT NULL, start INTEGER NOT NULL, end INTEGER NOT NULL, start_fingerprint TEXT, end_fingerprint TEXT, PRIMARY KEY(workspace_id, path, start));
                 CREATE TABLE vim_marks (workspace_id INTEGER, mark_name TEXT, path BLOB, value TEXT);
                 CREATE TABLE vim_global_marks_paths (workspace_id INTEGER, mark_name TEXT, path BLOB);
                 CREATE TABLE image_viewers (workspace_id INTEGER, item_id INTEGER, image_path BLOB);
                 CREATE TABLE git_graphs (workspace_id INTEGER, item_id INTEGER, repo_working_path TEXT);
                 CREATE TABLE undo_entries (id INTEGER PRIMARY KEY, repo_path TEXT NOT NULL, op TEXT NOT NULL);",
            )
            .expect("prepare schema")()
        .expect("create schema");

        // A statement that depends on a table created by an earlier statement
        // cannot be prepared in the same batch (sqlez prepares them all up
        // front), so the indexes go in their own call.
        connection
            .exec(
                "CREATE UNIQUE INDEX ix_workspaces_location
                     ON workspaces(remote_connection_id, paths);",
            )
            .expect("prepare index")()
        .expect("create index");
        connection
            .exec("CREATE UNIQUE INDEX idx_vim_marks ON vim_marks (workspace_id, mark_name, path);")
            .expect("prepare vim index")()
        .expect("create vim index");

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
                 INSERT INTO user_toolchains VALUES (NULL, 7, '/base/old/member', '', 'Rust', 'stable', '/usr/bin/cargo', '{}');
                 INSERT INTO file_folds VALUES (7, '/base/old/member/src/main.rs', 10, 20, NULL, NULL);
                 INSERT INTO git_graphs VALUES (7, 1, '/base/old/member');
                 INSERT INTO undo_entries VALUES (1, '/base/old/member', 'commit');",
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
                 INSERT INTO user_toolchains VALUES (NULL, 8, '/base/older/member', '', 'Rust', 'stable', '/usr/bin/cargo', '{}');
                 INSERT INTO file_folds VALUES (8, '/base/older/member/src/main.rs', 10, 20, NULL, NULL);
                 INSERT INTO git_graphs VALUES (8, 1, '/base/older/member');
                 INSERT INTO undo_entries VALUES (2, '/base/older/member', 'commit');",
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

        let mut insert_mark = connection
            .exec_bound::<(i64, String, Vec<u8>, String)>(
                "INSERT INTO vim_marks (workspace_id, mark_name, path, value) VALUES (?, ?, ?, ?)",
            )
            .expect("prepare vim_marks");
        insert_mark((
            7,
            "a".to_string(),
            b"/base/old/member/src/main.rs".to_vec(),
            "1,1".to_string(),
        ))
        .expect("insert mark");
        insert_mark((
            8,
            "a".to_string(),
            b"/base/older/member/src/main.rs".to_vec(),
            "1,1".to_string(),
        ))
        .expect("insert hostile mark");

        let mut insert_global_mark = connection
            .exec_bound::<(i64, String, Vec<u8>)>(
                "INSERT INTO vim_global_marks_paths (workspace_id, mark_name, path) VALUES (?, ?, ?)",
            )
            .expect("prepare vim_global_marks_paths");
        insert_global_mark((7, "A".to_string(), b"/base/old/member/src/main.rs".to_vec()))
            .expect("insert global mark");
        insert_global_mark((
            8,
            "A".to_string(),
            b"/base/older/member/src/main.rs".to_vec(),
        ))
        .expect("insert hostile global mark");

        let mut insert_image = connection
            .exec_bound::<(i64, i64, Vec<u8>)>(
                "INSERT INTO image_viewers (workspace_id, item_id, image_path) VALUES (?, ?, ?)",
            )
            .expect("prepare image_viewers");
        insert_image((7, 1, b"/base/old/member/logo.png".to_vec())).expect("insert image");
        insert_image((8, 1, b"/base/older/member/logo.png".to_vec()))
            .expect("insert hostile image");
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
            text(
                &connection,
                "SELECT path FROM file_folds WHERE workspace_id = 7"
            ),
            vec!["/base/new/member/src/main.rs"]
        );
        assert_eq!(
            blobs(
                &connection,
                "SELECT path FROM vim_marks WHERE workspace_id = 7"
            ),
            vec![b"/base/new/member/src/main.rs".to_vec()]
        );
        assert_eq!(
            blobs(
                &connection,
                "SELECT path FROM vim_global_marks_paths WHERE workspace_id = 7"
            ),
            vec![b"/base/new/member/src/main.rs".to_vec()]
        );
        assert_eq!(
            blobs(
                &connection,
                "SELECT image_path FROM image_viewers WHERE workspace_id = 7"
            ),
            vec![b"/base/new/member/logo.png".to_vec()]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT repo_working_path FROM git_graphs WHERE workspace_id = 7"
            ),
            vec!["/base/new/member"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT repo_path FROM undo_entries WHERE id = 1"
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
            text(
                &connection,
                "SELECT path FROM file_folds WHERE workspace_id = 8"
            ),
            vec!["/base/older/member/src/main.rs"]
        );
        assert_eq!(
            blobs(
                &connection,
                "SELECT path FROM vim_marks WHERE workspace_id = 8"
            ),
            vec![b"/base/older/member/src/main.rs".to_vec()]
        );
        assert_eq!(
            blobs(
                &connection,
                "SELECT path FROM vim_global_marks_paths WHERE workspace_id = 8"
            ),
            vec![b"/base/older/member/src/main.rs".to_vec()]
        );
        assert_eq!(
            blobs(
                &connection,
                "SELECT image_path FROM image_viewers WHERE workspace_id = 8"
            ),
            vec![b"/base/older/member/logo.png".to_vec()]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT repo_working_path FROM git_graphs WHERE workspace_id = 8"
            ),
            vec!["/base/older/member"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT repo_path FROM undo_entries WHERE id = 2"
            ),
            vec!["/base/older/member"]
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
        assert_eq!(
            text(
                &connection,
                "SELECT path FROM file_folds WHERE workspace_id = 7"
            ),
            vec!["/base/new/member/src/main.rs"]
        );
        assert_eq!(
            counts(&connection, "SELECT COUNT(*) FROM file_folds"),
            vec![2],
            "a second pass over an already-migrated fold row is a no-op"
        );
    }

    #[test]
    fn a_fold_row_already_at_the_target_path_does_not_abort_the_rewrite() {
        let connection = Connection::open_memory(Some("a_fold_row_already_at_the_target_path"));
        seed(&connection);
        // The user had the same file open under the *target* directory before,
        // so a row already occupies `(workspace_id, path, start)` — a plain
        // UPDATE would trip the primary key and fail the whole reconcile.
        connection
            .exec(
                "INSERT INTO file_folds VALUES (7, '/base/new/member/src/main.rs', 10, 99, NULL, NULL);",
            )
            .expect("prepare")()
        .expect("insert squatting fold");

        rewrite_app_db(&connection, &rewrite()).expect("rewrite");

        assert_eq!(
            counts(
                &connection,
                "SELECT end FROM file_folds
                 WHERE workspace_id = 7 AND path = '/base/new/member/src/main.rs'"
            ),
            vec![20],
            "the migrating row wins over the stale row already at the target path"
        );
    }

    fn seed_agent_db(connection: &Connection) {
        connection
            .exec(
                "CREATE TABLE solution_sessions (id TEXT PRIMARY KEY, solution_id TEXT, cwd TEXT);
                 CREATE TABLE solution_session_background_agent (solution_session_id TEXT, agent_id TEXT, jsonl_path TEXT, PRIMARY KEY (solution_session_id, agent_id));
                 CREATE TABLE solution_session_attachment (session_id TEXT, solution_id TEXT, path TEXT, created_at_ms INTEGER, PRIMARY KEY (session_id, path));",
            )
            .expect("prepare schema")()
        .expect("create schema");
        connection
            .exec(
                "INSERT INTO solution_sessions VALUES ('s1', '1', '/base/old/member');
                 INSERT INTO solution_sessions VALUES ('s2', '2', '/base/older/member');
                 INSERT INTO solution_session_background_agent VALUES ('s1', 'a1', '/base/old/member/.claude/x.jsonl');
                 INSERT INTO solution_session_background_agent VALUES ('s2', 'a1', '/base/older/member/.claude/x.jsonl');
                 INSERT INTO solution_session_attachment VALUES ('s1', '1', '/base/old/member/inbox/a.png', 5);
                 INSERT INTO solution_session_attachment VALUES ('s2', '2', '/base/older/member/inbox/a.png', 5);",
            )
            .expect("prepare seed")()
        .expect("seed rows");
    }

    #[test]
    fn rewrites_agent_db_rows_including_the_pk_path() {
        let connection = Connection::open_memory(Some("rewrites_agent_db_rows"));
        seed_agent_db(&connection);

        rewrite_agent_db(&connection, &rewrite()).expect("rewrite");
        // Idempotent.
        rewrite_agent_db(&connection, &rewrite()).expect("rewrite again");

        assert_eq!(
            text(
                &connection,
                "SELECT cwd FROM solution_sessions WHERE id = 's1'"
            ),
            vec!["/base/new/member"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT jsonl_path FROM solution_session_background_agent WHERE solution_session_id = 's1'"
            ),
            vec!["/base/new/member/.claude/x.jsonl"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT path FROM solution_session_attachment WHERE session_id = 's1'"
            ),
            vec!["/base/new/member/inbox/a.png"]
        );
        assert_eq!(
            counts(&connection, "SELECT COUNT(*) FROM solution_session_attachment"),
            vec![2],
            "delete+reinsert must not duplicate the row"
        );

        // The `/base/older` sibling shares a string prefix and must be intact.
        assert_eq!(
            text(
                &connection,
                "SELECT cwd FROM solution_sessions WHERE id = 's2'"
            ),
            vec!["/base/older/member"]
        );
        assert_eq!(
            text(
                &connection,
                "SELECT path FROM solution_session_attachment WHERE session_id = 's2'"
            ),
            vec!["/base/older/member/inbox/a.png"]
        );
    }

    #[test]
    fn a_missing_agent_table_is_not_an_error() {
        let connection = Connection::open_memory(Some("a_missing_agent_table_is_not_an_error"));
        rewrite_agent_db(&connection, &rewrite()).expect("rewrite an empty agent db");
    }

    #[test]
    fn encodes_a_claude_bucket_name_like_claude_does() {
        assert_eq!(
            encode_claude_bucket(Path::new("/home/spk/.spk/sawe/ss/spk-solutions")),
            "-home-spk--spk-sawe-ss-spk-solutions"
        );
    }

    #[test]
    fn moves_the_transcript_bucket() {
        let projects = tempfile::tempdir().expect("tempdir");
        let rewrite = rewrite();
        let old_bucket = projects.path().join(encode_claude_bucket(&rewrite.old));
        std::fs::create_dir_all(&old_bucket).expect("mkdir bucket");
        std::fs::write(old_bucket.join("session.jsonl"), b"{}").expect("write");

        move_transcript_bucket(projects.path(), &rewrite).expect("move");

        let new_bucket = projects.path().join(encode_claude_bucket(&rewrite.new));
        assert!(new_bucket.join("session.jsonl").is_file());
        assert!(!old_bucket.exists());

        // Idempotent: a second run with no source bucket is a no-op.
        move_transcript_bucket(projects.path(), &rewrite).expect("move again");
        assert!(new_bucket.join("session.jsonl").is_file());
    }

    #[test]
    fn a_missing_transcript_bucket_is_a_no_op() {
        let projects = tempfile::tempdir().expect("tempdir");
        let rewrite = rewrite();

        move_transcript_bucket(projects.path(), &rewrite).expect("no source bucket");

        assert!(
            !projects
                .path()
                .join(encode_claude_bucket(&rewrite.new))
                .exists(),
            "an absent source bucket must not conjure an empty target bucket"
        );
    }

    #[test]
    fn merges_into_an_existing_transcript_bucket() {
        let projects = tempfile::tempdir().expect("tempdir");
        let rewrite = rewrite();
        let old_bucket = projects.path().join(encode_claude_bucket(&rewrite.old));
        let new_bucket = projects.path().join(encode_claude_bucket(&rewrite.new));
        std::fs::create_dir_all(old_bucket.join("subagents")).expect("mkdir old");
        std::fs::write(old_bucket.join("a.jsonl"), b"a").expect("write a");
        std::fs::write(old_bucket.join("subagents/s.jsonl"), b"s").expect("write s");
        std::fs::create_dir_all(&new_bucket).expect("mkdir new");
        std::fs::write(new_bucket.join("b.jsonl"), b"b").expect("write b");
        std::fs::write(new_bucket.join("a.jsonl"), b"keep").expect("write existing a");

        move_transcript_bucket(projects.path(), &rewrite).expect("merge");

        assert_eq!(std::fs::read(new_bucket.join("b.jsonl")).expect("b"), b"b");
        assert_eq!(
            std::fs::read(new_bucket.join("a.jsonl")).expect("a"),
            b"keep",
            "an existing file in the target bucket is never overwritten"
        );
        assert_eq!(
            std::fs::read(new_bucket.join("subagents/s.jsonl")).expect("s"),
            b"s"
        );
        assert!(
            !old_bucket.exists(),
            "the source bucket is drained and removed"
        );
    }

    #[allow(clippy::disallowed_methods)]
    fn git(args: &[&str], cwd: &Path) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("running git");
        assert!(
            output.status.success(),
            "git {args:?} in {} failed: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A member repo with one commit and one linked worktree at `tree`.
    fn repo_with_worktree(member: &Path, tree: &Path) {
        std::fs::create_dir_all(member).expect("mkdir member");
        git(&["init", "-q", "-b", "main", "."], member);
        git(&["config", "user.email", "t@example.com"], member);
        git(&["config", "user.name", "Test"], member);
        git(&["commit", "-q", "--allow-empty", "-m", "init"], member);
        std::fs::create_dir_all(tree.parent().expect("tree parent")).expect("mkdir tree parent");
        git(
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "agent",
                tree.to_str().expect("tree str"),
            ],
            member,
        );
    }

    /// After a solution rename the whole tree moved: both the tree's `.git`
    /// pointer and the admin dir's `gitdir` file name the old location.
    fn assert_worktree_is_healthy(tree: &Path) {
        let inside = git(&["rev-parse", "--is-inside-work-tree"], tree);
        assert_eq!(inside, "true", "worktree at {} is broken", tree.display());
    }

    #[test]
    fn repairs_a_legacy_agent_worktree_after_the_solution_moved() {
        let base = tempfile::tempdir().expect("tempdir");
        let old_root = base.path().join("old");
        let member = old_root.join("member");
        let tree = member.join(".claude").join("worktrees").join("agent-1");
        repo_with_worktree(&member, &tree);

        let new_root = base.path().join("new");
        std::fs::rename(&old_root, &new_root).expect("move the solution root");
        let rewrite = PathRewrite {
            old: old_root,
            new: new_root.clone(),
        };
        let new_member = new_root.join("member");
        let new_tree = new_member.join(".claude").join("worktrees").join("agent-1");

        repair_git_worktrees(&[(new_member.clone(), new_root.clone())], &rewrite)
            .expect("repair");

        assert_worktree_is_healthy(&new_tree);
        // Idempotent — a second pass on an already-healthy tree is harmless.
        repair_git_worktrees(&[(new_member, new_root)], &rewrite).expect("repair again");
        assert_worktree_is_healthy(&new_tree);
    }

    #[test]
    fn repairs_a_relocated_agent_worktree_after_the_solution_moved() {
        let base = tempfile::tempdir().expect("tempdir");
        let old_root = base.path().join("old");
        let member = old_root.join("member");
        // Plan 3's `WorktreeCreate` hook puts new trees here, *outside* the
        // member repo but inside the solution root.
        let tree = old_root
            .join(".agents")
            .join("worktrees")
            .join("member")
            .join("agent-1");
        repo_with_worktree(&member, &tree);

        let new_root = base.path().join("new");
        std::fs::rename(&old_root, &new_root).expect("move the solution root");
        let rewrite = PathRewrite {
            old: old_root,
            new: new_root.clone(),
        };
        let new_member = new_root.join("member");
        let new_tree = new_root
            .join(".agents")
            .join("worktrees")
            .join("member")
            .join("agent-1");

        repair_git_worktrees(&[(new_member, new_root)], &rewrite).expect("repair");

        assert_worktree_is_healthy(&new_tree);
    }

    #[test]
    fn worktree_repair_ignores_a_tree_owned_by_another_member() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        let mine = root.join("mine");
        let theirs = root.join("theirs");
        // Both members keep their trees under the shared relocated area; the
        // `<member-dir>` folder name is *not* trustworthy after a member
        // rename, so ownership must come from the tree's own `.git` pointer.
        let their_tree = root
            .join(".agents")
            .join("worktrees")
            .join("mine")
            .join("agent-1");
        repo_with_worktree(&mine, &root.join("unused-tree"));
        repo_with_worktree(&theirs, &their_tree);

        let rewrite = PathRewrite {
            old: base.path().join("nowhere"),
            new: base.path().join("elsewhere"),
        };
        // `mine` must not try to repair `theirs`'s tree: passing a foreign tree
        // to `git worktree repair` would be a no-op at best, so assert on the
        // pointer staying intact.
        repair_git_worktrees(&[(mine, root)], &rewrite).expect("repair");

        let pointer = std::fs::read_to_string(their_tree.join(".git")).expect("read .git");
        assert!(
            pointer.contains("theirs/.git/worktrees/"),
            "a foreign tree's pointer was rewritten: {pointer}"
        );
        assert_worktree_is_healthy(&their_tree);
    }

    #[test]
    fn a_missing_worktree_directory_is_not_an_error() {
        let base = tempfile::tempdir().expect("tempdir");
        let member = base.path().join("member");
        std::fs::create_dir_all(&member).expect("mkdir");
        repair_git_worktrees(&[(member, base.path().to_path_buf())], &rewrite())
            .expect("no worktrees, no error");
    }

    #[test]
    fn apply_one_removes_the_compat_symlink_and_is_crash_safe() {
        let base = tempfile::tempdir().expect("tempdir");
        let old_root = base.path().join("old");
        let new_root = base.path().join("new");
        std::fs::create_dir_all(&new_root).expect("mkdir new");
        std::os::unix::fs::symlink(&new_root, &old_root).expect("symlink");

        let projects = tempfile::tempdir().expect("projects tempdir");
        let rewrite = PathRewrite {
            old: old_root.clone(),
            new: new_root.clone(),
        };
        let old_bucket = projects.path().join(encode_claude_bucket(&old_root));
        std::fs::create_dir_all(&old_bucket).expect("mkdir bucket");
        std::fs::write(old_bucket.join("s.jsonl"), b"{}").expect("write");

        let app = Connection::open_memory(Some("apply_one_removes_the_compat_symlink"));
        seed(&app);
        let agent = Connection::open_memory(Some("apply_one_agent"));
        seed_agent_db(&agent);

        apply_one_with_connections(&app, Some(&agent), Some(projects.path()), &rewrite)
            .expect("apply");
        assert!(!old_root.exists(), "the compat symlink is gone");
        assert!(
            projects
                .path()
                .join(encode_claude_bucket(&new_root))
                .join("s.jsonl")
                .is_file()
        );

        // Crash-safe: re-running a fully applied migration is a clean no-op.
        apply_one_with_connections(&app, Some(&agent), Some(projects.path()), &rewrite)
            .expect("apply again");
        assert!(!old_root.exists());
    }

    #[gpui::test]
    async fn drain_and_apply_deletes_the_drained_row(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let base = tempfile::tempdir().expect("tempdir");
        let old_root = base.path().join("old");
        let new_root = base.path().join("new");
        std::fs::create_dir_all(&new_root).expect("mkdir new");
        std::os::unix::fs::symlink(&new_root, &old_root).expect("symlink");

        let db =
            crate::db::SolutionsDb::open_test_db("drain_and_apply_deletes_the_drained_row").await;
        db.insert_pending_path_migration(
            old_root.to_string_lossy().into_owned(),
            new_root.to_string_lossy().into_owned(),
            1,
        )
        .await
        .expect("insert pending row");

        cx.update(|cx| gpui::block_on(crate::path_migrations::drain_and_apply_with_db(&db, cx)))
            .expect("drain");

        assert!(
            db.load_pending_path_migrations().expect("load").is_empty(),
            "the drained row is deleted"
        );
        assert!(!old_root.exists(), "the compat symlink is gone");

        // A second drain over an empty table is a clean no-op.
        cx.update(|cx| gpui::block_on(crate::path_migrations::drain_and_apply_with_db(&db, cx)))
            .expect("drain again");
    }

    #[test]
    fn apply_one_refuses_to_delete_a_real_directory_at_the_old_path() {
        let base = tempfile::tempdir().expect("tempdir");
        let old_root = base.path().join("old");
        let new_root = base.path().join("new");
        std::fs::create_dir_all(&new_root).expect("mkdir new");
        // The user re-created a *real* directory at the old path after the
        // rename — it is not our symlink and must never be removed.
        std::fs::create_dir_all(&old_root).expect("mkdir old");
        std::fs::write(old_root.join("user-file.txt"), b"precious").expect("write");

        let app = Connection::open_memory(Some("apply_one_refuses_to_delete"));
        seed(&app);
        apply_one_with_connections(
            &app,
            None,
            None,
            &PathRewrite {
                old: old_root.clone(),
                new: new_root,
            },
        )
        .expect("apply");

        assert!(
            old_root.join("user-file.txt").is_file(),
            "a real directory at the old path is left alone"
        );
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
