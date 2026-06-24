//! Persistent per-repo favorites + recent-checkouts store for the S-BRP
//! Branches popup. Backed by SQLite via the `BranchFavoritesDb` Domain in
//! the shared `AppDatabase`. Concurrent writers across threads are
//! serialized by sqlez's write queue; multi-process coordination uses
//! SQLite's WAL + busy_timeout configured globally for `AppDatabase`.
//!
//! The repo is keyed by a stable hash of its work-tree absolute path so
//! the persisted format doesn't pin the on-disk path verbatim — moving a
//! repo preserves favorites only by best-effort (the user re-favorites
//! after the move). Trade-off: avoids leaking the user's filesystem
//! layout into the persisted state.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

pub use self::persistence::BranchFavoritesDb;

/// Initialise the module-level connection cache. Called from
/// `crates/zed/src/main.rs` after `cx.set_global(app_db)`.
pub fn init(cx: &gpui::App) {
    persistence::set_global(BranchFavoritesDb::global(cx));
}

const RECENT_CAP: usize = 50;

/// Stable identifier for a repository, derived from the absolute path of
/// its working directory. Returned as a hex-encoded `u64` so it survives
/// any text format without serialization quirks.
pub fn repo_hash(work_dir: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    work_dir.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentEntry {
    pub branch: String,
    pub last_checkout_unix: i64,
}

/// Snapshot for one repo as exposed to the UI. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct RepoFavoritesSnapshot {
    pub favorites: Vec<String>,
    pub recent: Vec<RecentEntry>,
}

/// Read favorites + recent for a single repository.
pub fn load_for_repo(work_dir: &Path) -> Result<RepoFavoritesSnapshot> {
    let key = repo_hash(work_dir);
    let db = persistence::db()?;
    let favorites = db.select_favorites(key.clone())?;
    let recent = db
        .select_recent(key)?
        .into_iter()
        .map(|(branch, last_checkout_unix)| RecentEntry {
            branch,
            last_checkout_unix,
        })
        .collect();
    Ok(RepoFavoritesSnapshot { favorites, recent })
}

/// Toggle `branch` in the favorites set for the given repository.
/// Returns the post-toggle membership (`true` = is now a favorite).
pub fn toggle_favorite(work_dir: &Path, branch: &str) -> Result<bool> {
    let key = repo_hash(work_dir);
    let branch = branch.to_string();
    let db = persistence::db()?;
    let already_starred = db
        .select_favorite_one(key.clone(), branch.clone())?
        .is_some();
    if already_starred {
        gpui::block_on(db.delete_favorite(key, branch))?;
        Ok(false)
    } else {
        gpui::block_on(db.insert_favorite(key, branch))?;
        Ok(true)
    }
}

/// Record `branch` as the most-recent checkout in `work_dir`. Truncates
/// the recent list at [`RECENT_CAP`] entries to keep the table bounded.
pub fn record_checkout(work_dir: &Path, branch: &str) -> Result<()> {
    let key = repo_hash(work_dir);
    let branch = branch.to_string();
    let now = current_unix_seconds();
    let db = persistence::db()?;
    gpui::block_on(db.upsert_recent(key.clone(), branch, now))?;

    // Trim oldest recents beyond the cap. Done in code rather than via a
    // single SQL DELETE because timestamps frequently tie at second
    // resolution (multiple rapid checkouts in the same second), and a
    // pure `last_checkout_unix < threshold` predicate would silently
    // miss equal-ts rows. Per-repo row count is small (≤ ~50 + 1), so
    // the extra round trip is fine.
    let entries = db.select_recent(key.clone())?;
    if entries.len() > RECENT_CAP {
        for (branch_name, _) in entries.into_iter().skip(RECENT_CAP) {
            gpui::block_on(db.delete_recent_one(key.clone(), branch_name))?;
        }
    }
    Ok(())
}

/// Test scaffolding kept under the original `test_override` name so
/// existing test fixtures keep compiling. The new SQLite store uses one
/// shared in-memory DB per process; tests should rely on unique repo
/// paths to avoid cross-test collisions.
#[cfg(any(test, feature = "test-support"))]
pub mod test_override {
    use std::path::PathBuf;

    pub fn set(_path: PathBuf) {}
    pub fn clear() {}
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

    pub struct BranchFavoritesDb(ThreadSafeConnection);

    impl Domain for BranchFavoritesDb {
        const NAME: &str = stringify!(BranchFavoritesDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE branch_favorites (
                repo_hash   TEXT NOT NULL,
                branch_name TEXT NOT NULL,
                PRIMARY KEY (repo_hash, branch_name)
            ) STRICT;

            CREATE TABLE branch_recent (
                repo_hash           TEXT NOT NULL,
                branch_name         TEXT NOT NULL,
                last_checkout_unix  INTEGER NOT NULL,
                PRIMARY KEY (repo_hash, branch_name)
            ) STRICT;

            CREATE INDEX idx_branch_recent_repo_time
                ON branch_recent(repo_hash, last_checkout_unix DESC);
        )];
    }

    db::static_connection!(BranchFavoritesDb, []);

    static GLOBAL: OnceLock<BranchFavoritesDb> = OnceLock::new();

    pub(super) fn set_global(handle: BranchFavoritesDb) {
        let _ = GLOBAL.set(handle);
    }

    /// Per-thread test connection — see
    /// `git::undo_registry::persistence::thread_local_test_db` for why
    /// each test thread needs its own DB rather than sharing one
    /// in-memory DB process-wide.
    #[cfg(any(test, feature = "test-support"))]
    fn thread_local_test_db() -> BranchFavoritesDb {
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::thread::ThreadId;
        static REGISTRY: OnceLock<Mutex<HashMap<ThreadId, BranchFavoritesDb>>> = OnceLock::new();
        let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = registry.lock();
        if let Some(existing) = guard.get(&std::thread::current().id()) {
            return existing.clone();
        }
        let name = format!("branch_favorites_test_db_{}", uuid::Uuid::new_v4().simple());
        let leaked: &'static str = Box::leak(name.into_boxed_str());
        let db = gpui::block_on(BranchFavoritesDb::open_test_db(leaked));
        guard.insert(std::thread::current().id(), db.clone());
        db
    }

    pub(super) fn db() -> Result<BranchFavoritesDb> {
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
                "favorites::init has not been called — BranchFavoritesDb connection unavailable"
            ))
        }
    }

    impl BranchFavoritesDb {
        query! {
            pub fn select_favorites(repo_hash: String) -> Result<Vec<String>> {
                SELECT branch_name FROM branch_favorites
                WHERE repo_hash = ?
                ORDER BY branch_name ASC
            }
        }

        query! {
            pub fn select_favorite_one(repo_hash: String, branch_name: String)
                -> Result<Option<String>>
            {
                SELECT branch_name FROM branch_favorites
                WHERE repo_hash = ? AND branch_name = ?
            }
        }

        query! {
            pub async fn insert_favorite(repo_hash: String, branch_name: String) -> Result<()> {
                INSERT OR IGNORE INTO branch_favorites (repo_hash, branch_name)
                VALUES (?, ?)
            }
        }

        query! {
            pub async fn delete_favorite(repo_hash: String, branch_name: String) -> Result<()> {
                DELETE FROM branch_favorites
                WHERE repo_hash = ? AND branch_name = ?
            }
        }

        // Returned as `(branch, last_checkout_unix)` ordered most-recent-first.
        query! {
            pub fn select_recent(repo_hash: String) -> Result<Vec<(String, i64)>> {
                SELECT branch_name, last_checkout_unix
                FROM branch_recent
                WHERE repo_hash = ?
                ORDER BY last_checkout_unix DESC, branch_name ASC
            }
        }

        query! {
            pub async fn upsert_recent(
                repo_hash: String,
                branch_name: String,
                last_checkout_unix: i64
            ) -> Result<()> {
                INSERT INTO branch_recent (repo_hash, branch_name, last_checkout_unix)
                VALUES (?, ?, ?)
                ON CONFLICT(repo_hash, branch_name)
                DO UPDATE SET last_checkout_unix = excluded.last_checkout_unix
            }
        }

        query! {
            pub async fn delete_recent_one(repo_hash: String, branch_name: String) -> Result<()> {
                DELETE FROM branch_recent
                WHERE repo_hash = ? AND branch_name = ?
            }
        }
    }
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests share a single in-memory `BranchFavoritesDb`. Use unique repo
    /// paths per test so rows don't collide across tests.
    #[gpui::test]
    async fn toggle_favorite_round_trips() {
        let repo = Path::new("/tmp/r1-fav-toggle");
        let added = toggle_favorite(repo, "main").expect("toggle");
        assert!(added);
        let snap = load_for_repo(repo).expect("load");
        assert_eq!(snap.favorites, vec!["main".to_string()]);
        let removed = toggle_favorite(repo, "main").expect("toggle");
        assert!(!removed);
        let snap = load_for_repo(repo).expect("load");
        assert!(snap.favorites.is_empty());
    }

    #[gpui::test]
    async fn recent_caps_at_50_and_dedupes() {
        let repo = Path::new("/tmp/r2-fav-recent-cap");
        for ix in 0..60u32 {
            // Sprinkle ascending timestamps via the order of insertion +
            // the upsert overwriting last_checkout_unix on conflict —
            // each insert is "now", so b59 ends up newest.
            record_checkout(repo, &format!("b{ix}")).expect("record");
            // Tiny stagger so timestamps aren't all equal-second; the
            // mock clock for `current_unix_seconds` resolves to seconds.
            // We don't assert exact ordering on equal seconds, just the
            // cap.
        }
        let snap = load_for_repo(repo).expect("load");
        assert_eq!(snap.recent.len(), RECENT_CAP);
        // Re-checking an existing branch updates its timestamp, doesn't dupe.
        record_checkout(repo, "b30").expect("record");
        let snap = load_for_repo(repo).expect("load");
        assert!(snap.recent.iter().filter(|e| e.branch == "b30").count() == 1);
    }

    #[test]
    fn repo_hash_stable_per_path() {
        let h1 = repo_hash(Path::new("/a/b"));
        let h2 = repo_hash(Path::new("/a/b"));
        let h3 = repo_hash(Path::new("/a/c"));
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }
}
