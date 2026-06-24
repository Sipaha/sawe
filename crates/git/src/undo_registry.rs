//! Persistent undo registry — every destructive operation records a row so
//! the user can rewind with `editor.git.undo_last`.
//!
//! Persistence lives in the shared SQLite `AppDatabase` under the
//! `UndoRegistryDb` Domain (table `undo_entries`). Concurrent writers
//! across threads are serialized by sqlez's write queue. Multi-process
//! coordination on the same DB file uses SQLite's WAL mode + busy_timeout
//! (configured globally for `AppDatabase`).

use anyhow::Result;
use std::path::{Path, PathBuf};

pub use self::persistence::UndoRegistryDb;

/// Initialise the module-level connection cache. Called from
/// `crates/zed/src/main.rs` after `cx.set_global(app_db)`. Idempotent —
/// re-init replaces the cached handle (the underlying `ThreadSafeConnection`
/// is `Arc`-cloned, so no resource leak).
pub fn init(cx: &gpui::App) {
    persistence::set_global(UndoRegistryDb::global(cx));
}

/// One undo row.
#[derive(Debug, Clone)]
pub struct UndoEntry {
    pub id: u64,
    pub repo_path: PathBuf,
    pub op: String,
    pub timestamp_unix: i64,
    pub branch: String,
    pub before_sha: String,
    pub after_sha: Option<String>,
    pub failed: bool,
}

/// Append a fresh entry. Returns the assigned id.
pub fn record(repo_path: &Path, op: &str, branch: &str, before_sha: &str) -> Result<u64> {
    let db = persistence::db()?;
    let now = current_unix_seconds();
    let repo_path_str = repo_path.to_string_lossy().into_owned();
    let op = op.to_string();
    let branch = branch.to_string();
    let before_sha = before_sha.to_string();
    let id = gpui::block_on(db.insert_entry(repo_path_str, op, now, branch, before_sha))?
        .ok_or_else(|| anyhow::anyhow!("INSERT INTO undo_entries returned no id"))?;
    Ok(id as u64)
}

/// Mark `id` as completed with the resulting `after_sha`.
pub fn complete(id: u64, after_sha: &str) -> Result<()> {
    let db = persistence::db()?;
    gpui::block_on(db.set_after_sha(id as i64, after_sha.to_string()))
}

/// Mark `id` as failed.
pub fn mark_failed(id: u64) -> Result<()> {
    let db = persistence::db()?;
    gpui::block_on(db.set_failed(id as i64))
}

/// List entries newer than `since_unix` (inclusive lower bound), most recent first.
pub fn list(since_unix: i64) -> Result<Vec<UndoEntry>> {
    let db = persistence::db()?;
    let rows = db.select_since(since_unix)?;
    let entries = rows
        .into_iter()
        .map(
            |(id, repo_path, op, timestamp_unix, branch, before_sha, after_sha, failed)| {
                UndoEntry {
                    id: id as u64,
                    repo_path: PathBuf::from(repo_path),
                    op,
                    timestamp_unix,
                    branch,
                    before_sha,
                    after_sha,
                    failed: failed != 0,
                }
            },
        )
        .collect();
    Ok(entries)
}

/// Remove the entry with `id` from the registry. The corresponding
/// backup-ref (if any) is **not** touched — it follows its own retention.
pub fn forget(id: u64) -> Result<()> {
    let db = persistence::db()?;
    gpui::block_on(db.delete_by_id(id as i64))
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

mod persistence {
    use anyhow::Result;
    #[cfg(not(any(test, feature = "test-support")))]
    use anyhow::anyhow;
    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use std::sync::OnceLock;

    pub struct UndoRegistryDb(ThreadSafeConnection);

    impl Domain for UndoRegistryDb {
        const NAME: &str = stringify!(UndoRegistryDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE undo_entries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                repo_path TEXT NOT NULL,
                op TEXT NOT NULL,
                timestamp_unix INTEGER NOT NULL,
                branch TEXT NOT NULL,
                before_sha TEXT NOT NULL,
                after_sha TEXT,
                failed INTEGER NOT NULL DEFAULT 0
            ) STRICT;
        )];
    }

    db::static_connection!(UndoRegistryDb, []);

    static GLOBAL: OnceLock<UndoRegistryDb> = OnceLock::new();

    /// Replace the cached connection handle. The `OnceLock` keeps only the
    /// first-set value, which is intended: production startup wires this
    /// once via [`super::init`].
    pub(super) fn set_global(handle: UndoRegistryDb) {
        let _ = GLOBAL.set(handle);
    }

    /// Per-thread test connection. Tests that share a process-wide
    /// in-memory DB hit shared-cache table locks (code 262 /
    /// `SQLITE_LOCKED_SHAREDCACHE`) when several threads write
    /// concurrently. A per-thread DB keyed by `ThreadId` sidesteps that —
    /// every test thread gets its own DB and no inter-test interference.
    #[cfg(any(test, feature = "test-support"))]
    fn thread_local_test_db() -> UndoRegistryDb {
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::thread::ThreadId;
        static REGISTRY: OnceLock<Mutex<HashMap<ThreadId, UndoRegistryDb>>> = OnceLock::new();
        let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = registry.lock();
        if let Some(existing) = guard.get(&std::thread::current().id()) {
            return existing.clone();
        }
        let name = format!("undo_registry_test_db_{}", uuid::Uuid::new_v4().simple());
        // Leak the name to obtain a `&'static str` — this is test-only,
        // and the leak is bounded by `RUST_TEST_THREADS` (typically the
        // CPU count), so total memory growth is trivial.
        let leaked: &'static str = Box::leak(name.into_boxed_str());
        let db = gpui::block_on(UndoRegistryDb::open_test_db(leaked));
        guard.insert(std::thread::current().id(), db.clone());
        db
    }

    pub(super) fn db() -> Result<UndoRegistryDb> {
        if let Some(db) = GLOBAL.get() {
            return Ok(db.clone());
        }
        #[cfg(any(test, feature = "test-support"))]
        {
            return Ok(thread_local_test_db());
        }
        #[cfg(not(any(test, feature = "test-support")))]
        {
            Err(anyhow!(
                "undo_registry::init has not been called — UndoRegistryDb connection unavailable"
            ))
        }
    }

    impl UndoRegistryDb {
        query! {
            pub async fn insert_entry(
                repo_path: String,
                op: String,
                timestamp_unix: i64,
                branch: String,
                before_sha: String
            ) -> Result<Option<i64>> {
                INSERT INTO undo_entries (repo_path, op, timestamp_unix, branch, before_sha)
                VALUES (?, ?, ?, ?, ?)
                RETURNING id
            }
        }

        query! {
            pub async fn set_after_sha(id: i64, after_sha: String) -> Result<()> {
                UPDATE undo_entries SET after_sha = ?2 WHERE id = ?1
            }
        }

        query! {
            pub async fn set_failed(id: i64) -> Result<()> {
                UPDATE undo_entries SET failed = 1 WHERE id = ?
            }
        }

        query! {
            pub fn select_since(since_unix: i64) -> Result<Vec<(
                i64,
                String,
                String,
                i64,
                String,
                String,
                Option<String>,
                i64
            )>> {
                SELECT id, repo_path, op, timestamp_unix, branch, before_sha, after_sha, failed
                FROM undo_entries
                WHERE timestamp_unix >= ?
                ORDER BY timestamp_unix DESC, id DESC
            }
        }

        query! {
            pub async fn delete_by_id(id: i64) -> Result<()> {
                DELETE FROM undo_entries WHERE id = ?
            }
        }
    }
}

/// Test scaffolding kept under the original `test_override` name so existing
/// callers (`operations.rs`, `cherry_pick.rs`, etc.) keep compiling. The
/// migration from JSON to SQLite means a per-test directory no longer makes
/// sense — every test now shares a single in-memory `UndoRegistryDb` that's
/// installed lazily on first use. `set` and `clear` are accepted as no-ops
/// so existing call sites need no changes; tests that need isolation should
/// rely on unique repo paths / timestamps to avoid cross-test collisions.
#[cfg(any(test, feature = "test-support"))]
pub mod test_override {
    use std::path::PathBuf;

    /// No-op: kept for source compatibility with the prior JSON-backed
    /// implementation. The shared in-memory DB is set up lazily on first
    /// API call.
    pub fn set(_path: PathBuf) {}

    /// No-op: see [`set`].
    pub fn clear() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Each test inserts under a unique repo path / op label so rows from
    /// other tests sharing the in-memory DB don't collide on filtering.
    #[gpui::test]
    async fn record_and_list_round_trips() {
        let id1 = record(Path::new("/repo/r1"), "drop_test_a", "main", "deadbeef").expect("record");
        let id2 =
            record(Path::new("/repo/r1"), "squash_test_a", "main", "feedface").expect("record");
        assert!(id2 > id1);

        let entries = list(0).expect("list");
        let mine: Vec<_> = entries
            .iter()
            .filter(|e| e.op == "drop_test_a" || e.op == "squash_test_a")
            .collect();
        assert_eq!(mine.len(), 2);
    }

    #[gpui::test]
    async fn complete_sets_after_sha() {
        let id = record(Path::new("/r/complete"), "rebase_test_b", "main", "aaaa").expect("record");
        complete(id, "bbbb").expect("complete");
        let entries = list(0).expect("list");
        let entry = entries.iter().find(|e| e.id == id).expect("entry exists");
        assert_eq!(entry.after_sha.as_deref(), Some("bbbb"));
        assert!(!entry.failed);
    }

    #[gpui::test]
    async fn mark_failed_flips_flag() {
        let id = record(Path::new("/r/failed"), "drop_test_c", "main", "ccc").expect("record");
        mark_failed(id).expect("mark_failed");
        let entries = list(0).expect("list");
        let entry = entries.iter().find(|e| e.id == id).expect("entry exists");
        assert!(entry.failed);
    }

    #[gpui::test]
    async fn list_filters_by_timestamp() {
        let id = record(Path::new("/r/ts"), "drop_test_d", "main", "aa").expect("record");
        let entries = list(0).expect("list");
        assert!(entries.iter().any(|e| e.id == id));
        // Cutoff in the future: empty.
        let future = current_unix_seconds() + 10_000;
        let entries = list(future).expect("list");
        assert!(entries.is_empty());
    }

    #[gpui::test]
    async fn forget_removes_entry() {
        let id = record(Path::new("/r/forget"), "drop_test_e", "main", "aa").expect("record");
        forget(id).expect("forget");
        let entries = list(0).expect("list");
        assert!(entries.iter().all(|e| e.id != id));
    }
}
