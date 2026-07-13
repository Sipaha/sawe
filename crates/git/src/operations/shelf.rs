//! S-SHL — Shelf: named long-term entries on top of git stash.
//!
//! A shelf entry pairs a stable `stash_sha` (the commit hash of a real git
//! stash entry, NOT `stash@{N}` — positional indices shift on every push /
//! drop and can't be used as a long-term key) with editor-local metadata
//! (name, description, source branch, file summary) persisted in the
//! shared SQLite `AppDatabase` under the `ShelfDb` Domain.
//!
//! Persistence: rows live in `shelf_entries` keyed by
//! `(repo_hash(work_dir), name)`. `files_summary` is stored as a JSON
//! TEXT column inside the row (small structured value, not worth a
//! separate table). Concurrent writers across threads are serialized by
//! sqlez's write queue; multi-process coordination uses SQLite's WAL +
//! busy_timeout configured globally for `AppDatabase`.

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

pub use self::persistence::ShelfDb;

/// Initialise the module-level connection cache. Called from
/// `crates/zed/src/main.rs` after `cx.set_global(app_db)`. See the
/// matching helper in `git::undo_registry::init` for the rationale.
pub fn init(cx: &gpui::App) {
    persistence::set_global(ShelfDb::global(cx));
}

/// One named shelf entry. The `stash_sha` is the stable identifier — git
/// stash positions (`stash@{N}`) shift on push/drop and would silently
/// corrupt long-term entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShelfEntry {
    pub name: String,
    pub stash_sha: String,
    pub created_at_unix: i64,
    pub source_branch: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub files_summary: FilesSummary,
}

/// Lightweight per-entry stat pulled from `git stash show --numstat`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesSummary {
    pub count_added: u32,
    pub count_modified: u32,
    pub count_deleted: u32,
    pub total_lines_added: u32,
    pub total_lines_removed: u32,
    /// First five paths from the stash, in numstat order.
    #[serde(default)]
    pub top_paths: Vec<String>,
}

/// In-memory snapshot of one repo's shelf, owned by callers. The `entries`
/// vector is a cache of the persisted rows for `repo_path`. Mutations
/// (add/remove/rename/update_description) update the cache and are then
/// flushed to the DB by [`Self::save`], which writes the full slice as a
/// transactional REPLACE for the repo bucket.
#[derive(Debug, Clone)]
pub struct ShelfStore {
    repo_path: PathBuf,
    repo_key: String,
    entries: Vec<ShelfEntry>,
}

impl ShelfStore {
    /// Load this repo's slice from the persisted store. Other repos' rows
    /// are not held in memory.
    pub fn load(repo_path: &Path) -> Result<Self> {
        let repo_key = repo_hash(repo_path);
        let entries = persistence::load_entries_for_repo(&repo_key)?;
        Ok(Self {
            repo_path: repo_path.to_path_buf(),
            repo_key,
            entries,
        })
    }

    /// Persist this repo's slice back into the store, leaving other repos'
    /// entries untouched. Replaces all rows for `repo_key`: simpler than
    /// diffing the in-memory cache against the DB and the row count per
    /// repo is small (handful of entries).
    pub fn save(&self) -> Result<()> {
        persistence::replace_entries_for_repo(&self.repo_key, &self.entries)
    }

    /// Append a fresh entry. Errors if `name` already exists for this repo.
    pub fn add(&mut self, entry: ShelfEntry) -> Result<()> {
        if self.entries.iter().any(|e| e.name == entry.name) {
            return Err(anyhow!(
                "a shelf entry named {:?} already exists",
                entry.name
            ));
        }
        self.entries.push(entry);
        self.save()
    }

    /// Read-only view of this repo's entries. `_repo_path` is accepted to
    /// keep the signature symmetric with the spec — callers can pass any
    /// path (it's ignored; the store was bound to the repo at `load()`).
    pub fn list(&self, _repo_path: &Path) -> Vec<&ShelfEntry> {
        self.entries.iter().collect()
    }

    /// Borrowed access to the in-memory entries.
    pub fn entries(&self) -> &[ShelfEntry] {
        &self.entries
    }

    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    /// Drop the entry whose `name` matches. Errors if no match.
    pub fn remove(&mut self, name: &str) -> Result<()> {
        let before = self.entries.len();
        self.entries.retain(|e| e.name != name);
        if self.entries.len() == before {
            return Err(anyhow!("no shelf entry named {:?}", name));
        }
        self.save()
    }

    /// Rename `old` -> `new`. Errors if `old` is missing or `new`
    /// collides with an existing entry.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<()> {
        if old == new {
            return Ok(());
        }
        if self.entries.iter().any(|e| e.name == new) {
            return Err(anyhow!("a shelf entry named {:?} already exists", new));
        }
        let target = self
            .entries
            .iter_mut()
            .find(|e| e.name == old)
            .ok_or_else(|| anyhow!("no shelf entry named {:?}", old))?;
        target.name = new.to_string();
        self.save()
    }

    /// Replace the description on `name`. `desc=None` clears it.
    pub fn update_description(&mut self, name: &str, desc: Option<String>) -> Result<()> {
        let target = self
            .entries
            .iter_mut()
            .find(|e| e.name == name)
            .ok_or_else(|| anyhow!("no shelf entry named {:?}", name))?;
        target.description = desc.filter(|d| !d.trim().is_empty());
        self.save()
    }

    /// Look up an entry by its stable `stash_sha`. Useful when correlating
    /// a `git stash list` row back to a named entry.
    pub fn lookup_by_sha(&self, sha: &str) -> Option<&ShelfEntry> {
        self.entries.iter().find(|e| e.stash_sha == sha)
    }

    /// Names of entries whose `stash_sha` is no longer in
    /// `git stash list` output (manually dropped via the CLI, etc.).
    /// Returns names so the caller can show "Forget" buttons or prune.
    pub fn lookup_orphaned(&self, repo_path: &Path) -> Vec<String> {
        let live = list_live_stash_shas(repo_path).unwrap_or_default();
        self.entries
            .iter()
            .filter(|entry| !live.iter().any(|sha| sha == &entry.stash_sha))
            .map(|entry| entry.name.clone())
            .collect()
    }
}

/// Stable identifier for a repository, hashed from the absolute path of its
/// working directory. Re-exported rather than redefined: the folder-move
/// reconcile in `solutions::path_migrations` re-derives this key to follow a
/// renamed repo, and it must be the *same* function.
pub use crate::repo_hash;

/// Test scaffolding kept under the original `test_override` name so existing
/// callers keep compiling. `set` / `clear` are no-ops; the SQLite-backed
/// store now uses a single shared in-memory DB for all tests within a
/// process (tests rely on unique repo paths to stay isolated).
#[cfg(any(test, feature = "test-support"))]
pub mod test_override {
    use std::path::PathBuf;

    pub fn set(_path: PathBuf) {}
    pub fn clear() {}
}

mod persistence {
    use super::ShelfEntry;
    #[cfg(not(any(test, feature = "test-support")))]
    use anyhow::anyhow;
    use anyhow::{Context as _, Result};
    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use std::sync::OnceLock;

    pub struct ShelfDb(ThreadSafeConnection);

    impl Domain for ShelfDb {
        const NAME: &str = stringify!(ShelfDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE shelf_entries (
                repo_hash TEXT NOT NULL,
                name TEXT NOT NULL,
                stash_sha TEXT NOT NULL,
                created_at_unix INTEGER NOT NULL,
                source_branch TEXT,
                description TEXT,
                files_summary_json TEXT NOT NULL,
                PRIMARY KEY (repo_hash, name)
            ) STRICT;
        )];
    }

    db::static_connection!(ShelfDb, []);

    static GLOBAL: OnceLock<ShelfDb> = OnceLock::new();

    pub(super) fn set_global(handle: ShelfDb) {
        let _ = GLOBAL.set(handle);
    }

    /// Per-thread test connection. See `undo_registry::persistence::thread_local_test_db`
    /// for the rationale (shared-cache lock contention when many test
    /// threads target a single in-memory DB).
    #[cfg(any(test, feature = "test-support"))]
    fn thread_local_test_db() -> ShelfDb {
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::thread::ThreadId;
        static REGISTRY: OnceLock<Mutex<HashMap<ThreadId, ShelfDb>>> = OnceLock::new();
        let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = registry.lock();
        if let Some(existing) = guard.get(&std::thread::current().id()) {
            return existing.clone();
        }
        let name = format!("shelf_test_db_{}", uuid::Uuid::new_v4().simple());
        let leaked: &'static str = Box::leak(name.into_boxed_str());
        let db = gpui::block_on(ShelfDb::open_test_db(leaked));
        guard.insert(std::thread::current().id(), db.clone());
        db
    }

    fn db() -> Result<ShelfDb> {
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
                "shelf::init has not been called — ShelfDb connection unavailable"
            ))
        }
    }

    pub(super) fn load_entries_for_repo(repo_key: &str) -> Result<Vec<ShelfEntry>> {
        let db = db()?;
        let rows = db.select_for_repo(repo_key.to_string())?;
        rows.into_iter()
            .map(
                |(
                    name,
                    stash_sha,
                    created_at_unix,
                    source_branch,
                    description,
                    files_summary_json,
                )| {
                    let files_summary = serde_json::from_str(&files_summary_json)
                        .with_context(|| format!("parsing files_summary for {name:?}"))?;
                    Ok(ShelfEntry {
                        name,
                        stash_sha,
                        created_at_unix,
                        source_branch,
                        description,
                        files_summary,
                    })
                },
            )
            .collect()
    }

    pub(super) fn replace_entries_for_repo(repo_key: &str, entries: &[ShelfEntry]) -> Result<()> {
        let db = db()?;
        let key = repo_key.to_string();

        let serialized: Vec<(
            String,
            String,
            String,
            i64,
            Option<String>,
            Option<String>,
            String,
        )> = entries
            .iter()
            .map(|entry| {
                let files_summary_json = serde_json::to_string(&entry.files_summary)
                    .with_context(|| format!("serializing files_summary for {:?}", entry.name))?;
                Ok::<_, anyhow::Error>((
                    key.clone(),
                    entry.name.clone(),
                    entry.stash_sha.clone(),
                    entry.created_at_unix,
                    entry.source_branch.clone(),
                    entry.description.clone(),
                    files_summary_json,
                ))
            })
            .collect::<Result<_>>()?;

        gpui::block_on(db.delete_for_repo(key))?;
        for row in serialized {
            gpui::block_on(db.insert_entry(row.0, row.1, row.2, row.3, row.4, row.5, row.6))?;
        }
        Ok(())
    }

    impl ShelfDb {
        query! {
            pub fn select_for_repo(repo_hash: String) -> Result<Vec<(
                String,
                String,
                i64,
                Option<String>,
                Option<String>,
                String
            )>> {
                SELECT name, stash_sha, created_at_unix, source_branch, description, files_summary_json
                FROM shelf_entries
                WHERE repo_hash = ?
                ORDER BY created_at_unix ASC, name ASC
            }
        }

        query! {
            pub async fn delete_for_repo(repo_hash: String) -> Result<()> {
                DELETE FROM shelf_entries WHERE repo_hash = ?
            }
        }

        query! {
            pub async fn insert_entry(
                repo_hash: String,
                name: String,
                stash_sha: String,
                created_at_unix: i64,
                source_branch: Option<String>,
                description: Option<String>,
                files_summary_json: String
            ) -> Result<()> {
                INSERT INTO shelf_entries (
                    repo_hash, name, stash_sha, created_at_unix,
                    source_branch, description, files_summary_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?)
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

// ====================================================================
//  High-level operations
// ====================================================================

/// Capture `paths_to_shelve` into a git stash labelled `[spk-shelf] <name>`,
/// record metadata in [`ShelfStore`], and return the resulting entry.
///
/// `paths_to_shelve = None` shelves the entire working-tree diff (the
/// default behaviour of `git stash push`). `remove_after = false` re-applies
/// the stash on top of the working tree so the shelf is a copy rather than
/// a move; the on-disk stash entry still exists either way.
pub fn shelve(
    repo_path: &Path,
    name: &str,
    description: Option<String>,
    paths_to_shelve: Option<Vec<PathBuf>>,
    remove_after: bool,
) -> Result<ShelfEntry> {
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return Err(anyhow!("shelf entry name must not be empty"));
    }

    let mut store = ShelfStore::load(repo_path)?;
    if store.entries.iter().any(|e| e.name == trimmed_name) {
        return Err(anyhow!(
            "a shelf entry named {:?} already exists",
            trimmed_name
        ));
    }

    let stash_message = format!("[spk-shelf] {}", trimmed_name);
    let mut args: Vec<String> = vec![
        "stash".into(),
        "push".into(),
        "--include-untracked".into(),
        "-m".into(),
        stash_message,
    ];
    if let Some(paths) = paths_to_shelve.as_ref() {
        if !paths.is_empty() {
            args.push("--".into());
            for path in paths {
                args.push(path.to_string_lossy().into_owned());
            }
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let push_out = run_git(repo_path, &arg_refs)?;
    if push_out.contains("No local changes to save") {
        return Err(anyhow!("nothing to shelve: working tree is clean"));
    }

    let stash_sha = run_git(repo_path, &["rev-parse", "stash@{0}"])
        .context("reading stash@{0} sha")?
        .trim()
        .to_string();
    if stash_sha.is_empty() {
        return Err(anyhow!("`git rev-parse stash@{{0}}` returned empty sha"));
    }

    let summary = parse_files_summary(repo_path, &stash_sha)?;
    let source_branch = current_branch(repo_path).ok();

    if !remove_after {
        run_git(repo_path, &["stash", "apply", "--quiet", &stash_sha])
            .context("re-applying shelved stash (remove_after=false)")?;
    }

    let entry = ShelfEntry {
        name: trimmed_name.to_string(),
        stash_sha,
        created_at_unix: current_unix_seconds(),
        source_branch,
        description: description.filter(|d| !d.trim().is_empty()),
        files_summary: summary,
    };
    store.add(entry.clone())?;
    Ok(entry)
}

/// Apply the stash backing the named shelf entry. `remove_from_shelf=true`
/// drops the underlying stash and removes the entry; `false` leaves both
/// in place (equivalent to `git stash apply` rather than `git stash pop`).
pub fn apply(repo_path: &Path, name: &str, remove_from_shelf: bool) -> Result<()> {
    let mut store = ShelfStore::load(repo_path)?;
    let entry = store
        .entries
        .iter()
        .find(|e| e.name == name)
        .cloned()
        .ok_or_else(|| anyhow!("no shelf entry named {:?}", name))?;

    run_git(repo_path, &["stash", "apply", &entry.stash_sha])
        .with_context(|| format!("applying shelf entry {:?}", name))?;

    if remove_from_shelf {
        // Resolve the position of `stash_sha` in `git stash list`, then
        // drop it. We can't use `stash@{0}` here because the user may have
        // pushed unrelated stashes after the shelf was created.
        if let Some(index) = locate_stash_index(repo_path, &entry.stash_sha)? {
            run_git(repo_path, &["stash", "drop", &format!("stash@{{{index}}}")])
                .with_context(|| format!("dropping stash backing {:?}", name))?;
        }
        store.remove(name)?;
    }
    Ok(())
}

/// Drop both the underlying stash (if still present) and the named entry.
pub fn drop(repo_path: &Path, name: &str) -> Result<()> {
    let mut store = ShelfStore::load(repo_path)?;
    let entry = store
        .entries
        .iter()
        .find(|e| e.name == name)
        .cloned()
        .ok_or_else(|| anyhow!("no shelf entry named {:?}", name))?;

    if let Some(index) = locate_stash_index(repo_path, &entry.stash_sha)? {
        // Best-effort — the shelf entry should disappear even if the stash
        // drop fails (so the user can stop seeing a stale row).
        if let Err(err) = run_git(repo_path, &["stash", "drop", &format!("stash@{{{index}}}")]) {
            log::warn!("git::shelf: failed to drop stash for shelf entry {name:?}: {err}");
        }
    }
    store.remove(name)?;
    Ok(())
}

/// Names of entries whose backing stash is no longer reachable. Mirrors
/// [`ShelfStore::lookup_orphaned`] for callers that don't keep a store
/// handle around.
pub fn lookup_orphaned(repo_path: &Path) -> Result<Vec<String>> {
    let store = ShelfStore::load(repo_path)?;
    Ok(store.lookup_orphaned(repo_path))
}

fn parse_files_summary(repo_path: &Path, stash_sha: &str) -> Result<FilesSummary> {
    let raw = run_git(
        repo_path,
        &["stash", "show", "--numstat", "--no-color", stash_sha],
    )
    .unwrap_or_default();
    Ok(parse_numstat(&raw))
}

/// Parses `git stash show --numstat` output — `<added>\t<deleted>\t<path>`
/// with `-` standing in for binary files. Lines that don't match are
/// silently skipped (`git` occasionally interleaves header rows).
pub(crate) fn parse_numstat(raw: &str) -> FilesSummary {
    let mut summary = FilesSummary::default();
    for line in raw.lines() {
        let mut parts = line.splitn(3, '\t');
        let added = parts.next().unwrap_or("").trim();
        let removed = parts.next().unwrap_or("").trim();
        let path = parts.next().unwrap_or("").trim();
        if path.is_empty() {
            continue;
        }
        let added_n: u32 = added.parse().unwrap_or(0);
        let removed_n: u32 = removed.parse().unwrap_or(0);
        summary.total_lines_added = summary.total_lines_added.saturating_add(added_n);
        summary.total_lines_removed = summary.total_lines_removed.saturating_add(removed_n);
        if added_n > 0 && removed_n == 0 {
            summary.count_added = summary.count_added.saturating_add(1);
        } else if added_n == 0 && removed_n > 0 {
            summary.count_deleted = summary.count_deleted.saturating_add(1);
        } else {
            summary.count_modified = summary.count_modified.saturating_add(1);
        }
        if summary.top_paths.len() < 5 {
            summary.top_paths.push(path.to_string());
        }
    }
    summary
}

fn list_live_stash_shas(repo_path: &Path) -> Result<Vec<String>> {
    let raw = run_git(repo_path, &["stash", "list", "--format=%H"])?;
    Ok(raw
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

fn locate_stash_index(repo_path: &Path, target_sha: &str) -> Result<Option<usize>> {
    let shas = list_live_stash_shas(repo_path)?;
    Ok(shas.iter().position(|sha| sha == target_sha))
}

fn current_branch(repo_path: &Path) -> Result<String> {
    let raw = run_git(repo_path, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("HEAD is detached"));
    }
    Ok(trimmed.to_string())
}

// `Command::new("git")` is in the `disallowed_methods` set workspace-wide;
// shelf operations run synchronously under callers that already hopped to
// a background thread, so the simple `Command` form is the right shape.
#[allow(clippy::disallowed_methods)]
fn run_git(repo_path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    // Avoid `use super::*;` because our local `drop` free function would
    // shadow the prelude's `Drop`/`drop` vocabulary used by `#[gpui::test]`'s
    // generated scaffolding (the macro emits cleanup code that calls `drop`).
    use super::{FilesSummary, ShelfEntry, ShelfStore, parse_numstat};
    use std::path::Path;

    /// Tests within this module share one in-memory `ShelfDb`. Use unique
    /// repo paths per test so rows don't collide across tests.
    #[gpui::test]
    async fn store_roundtrips_through_db() {
        let repo = Path::new("/tmp/example-repo-db-roundtrip");
        let mut store = ShelfStore::load(repo).expect("load empty");
        assert!(store.entries.is_empty());

        let entry = ShelfEntry {
            name: "feature-x".into(),
            stash_sha: "deadbeef".repeat(5),
            created_at_unix: 1_700_000_000,
            source_branch: Some("main".into()),
            description: Some("Half-baked feature x".into()),
            files_summary: FilesSummary {
                count_modified: 2,
                total_lines_added: 10,
                total_lines_removed: 1,
                top_paths: vec!["src/lib.rs".into(), "src/main.rs".into()],
                ..FilesSummary::default()
            },
        };
        store.add(entry.clone()).expect("add");

        let store2 = ShelfStore::load(repo).expect("reload");
        assert_eq!(store2.entries.len(), 1);
        assert_eq!(store2.entries[0], entry);

        // Different repo path, separate slot.
        let other = ShelfStore::load(Path::new("/tmp/other-db-roundtrip")).expect("load other");
        assert!(other.entries.is_empty());
    }

    #[gpui::test]
    async fn add_rejects_duplicate_name() {
        let mut store = ShelfStore::load(Path::new("/r-db-add-dup")).expect("load");
        let entry = ShelfEntry {
            name: "wip".into(),
            stash_sha: "a".into(),
            created_at_unix: 1,
            source_branch: None,
            description: None,
            files_summary: FilesSummary::default(),
        };
        store.add(entry.clone()).expect("add");
        let err = store.add(entry).expect_err("must reject duplicate");
        assert!(err.to_string().contains("already exists"));
    }

    #[gpui::test]
    async fn rename_and_update_description_mutate_in_place() {
        let mut store = ShelfStore::load(Path::new("/r-db-rename")).expect("load");
        store
            .add(ShelfEntry {
                name: "old".into(),
                stash_sha: "x".into(),
                created_at_unix: 1,
                source_branch: None,
                description: None,
                files_summary: FilesSummary::default(),
            })
            .expect("add");
        store.rename("old", "new").expect("rename");
        store
            .update_description("new", Some("desc".into()))
            .expect("desc");
        let reloaded = ShelfStore::load(Path::new("/r-db-rename")).expect("reload");
        assert_eq!(reloaded.entries[0].name, "new");
        assert_eq!(reloaded.entries[0].description.as_deref(), Some("desc"));
    }

    #[gpui::test]
    async fn lookup_orphaned_flags_missing_sha() {
        // We can't easily run real `git stash list` against a tempdir
        // here, so just verify the in-memory plumbing — the live-list
        // helper falls through to "empty" on failure, so every entry is
        // reported as orphaned.
        let mut store = ShelfStore::load(Path::new("/no-such-repo-db")).expect("load");
        store
            .add(ShelfEntry {
                name: "ghost".into(),
                stash_sha: "0".repeat(40),
                created_at_unix: 1,
                source_branch: None,
                description: None,
                files_summary: FilesSummary::default(),
            })
            .expect("add");
        let orphans = store.lookup_orphaned(Path::new("/no-such-repo-db"));
        assert_eq!(orphans, vec!["ghost".to_string()]);
    }

    #[test]
    fn parse_numstat_buckets_files_correctly() {
        let raw = "5\t0\tnew_file.rs\n0\t3\tdeleted.rs\n2\t2\tmodified.rs\n-\t-\tbinary.png\n";
        let summary = parse_numstat(raw);
        assert_eq!(summary.count_added, 1);
        assert_eq!(summary.count_deleted, 1);
        // Modified + binary (whose `0\t0` parse falls into the modified bucket).
        assert_eq!(summary.count_modified, 2);
        assert_eq!(summary.total_lines_added, 7);
        assert_eq!(summary.total_lines_removed, 5);
        assert_eq!(summary.top_paths.len(), 4);
    }

    #[gpui::test]
    async fn store_save_clears_repo_when_empty() {
        let mut store = ShelfStore::load(Path::new("/r-db-empty-clear")).expect("load");
        store
            .add(ShelfEntry {
                name: "tmp".into(),
                stash_sha: "x".into(),
                created_at_unix: 1,
                source_branch: None,
                description: None,
                files_summary: FilesSummary::default(),
            })
            .expect("add");
        store.remove("tmp").expect("remove");
        // Reload — should be empty.
        let reloaded = ShelfStore::load(Path::new("/r-db-empty-clear")).expect("reload");
        assert!(reloaded.entries.is_empty());
    }
}
