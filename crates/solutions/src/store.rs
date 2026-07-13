use crate::add_member::InFlightAdd;
use crate::db::SolutionsDb;
use crate::model::{CatalogId, CatalogProject, Solution, SolutionId, SolutionMember};
use crate::persistence::{CURRENT_VERSION, SolutionsConfig};
use crate::tabs_snapshot::{SolutionTabsSnapshot, TabSnapshots};
use collections::{HashMap, HashSet};
use gpui::{App, AppContext as _, Entity, EventEmitter, Global};
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(any(test, feature = "test-support"))]
use gpui::Context;

mod catalog;
mod lifecycle;
mod members;

pub struct SolutionStore {
    pub(crate) config: SolutionsConfig,
    /// `Some` for stores hydrated from `SolutionsDb` (production via
    /// `init_global` and tests via `init_global_for_test`); `None` for
    /// `for_test` stores that exercise mutations without a DB.
    pub(crate) db: Option<SolutionsDb>,
    pub(crate) fs_lock: Arc<smol::lock::Mutex<()>>,
    pub(crate) in_flight_adds: HashMap<(SolutionId, CatalogId), InFlightAdd>,
    /// Per-Solution open-tab snapshots, populated by the in-place
    /// switch orchestrator on switch-out and replayed on switch-in.
    /// Runtime-only; not persisted to disk — losing the snapshot
    /// after an editor restart is acceptable (user can re-open the
    /// tabs themselves), and persistence would mean keeping
    /// `solutions.json` in sync with potentially-stale path lists.
    pub(crate) tab_snapshots: TabSnapshots,
    /// Solution-wide active catalog member selection. Hydrated from the
    /// `active_member` DB table at init time and updated through
    /// `set_active_member`. The cache exists so callers don't round-trip
    /// through SQL on every render.
    pub(crate) active_member: HashMap<SolutionId, CatalogId>,
    /// Runtime-only set of solutions whose desktop window is currently open.
    /// Populated by `event_sources::install` from MultiWorkspace lifecycle
    /// (observe_new fires on window creation; observe_release fires on close).
    /// Tests use `mark_open` directly to fake an open window.
    /// Not persisted — every process restart starts empty and reconciles via
    /// the `observe_new::<MultiWorkspace>` hook.
    pub(crate) open_solutions: HashSet<SolutionId>,
}

#[derive(Clone, Debug)]
pub enum SolutionStoreEvent {
    Changed,
    /// Emitted by `touch_last_opened` whenever a Solution is opened
    /// (or switched to). Subscribers that only need to react to "the
    /// active Solution flipped" — e.g. fork panels refreshing their
    /// content for a new Solution scope — should listen to this
    /// instead of `Changed`, which fires on every store mutation
    /// including non-active edits (catalog adds, member moves, …).
    /// The id IS always Some — `None` would mean "no Solution is
    /// active", which we model as "no event fired".
    ActiveSolutionChanged(SolutionId),
    MemberAddProgress {
        solution: SolutionId,
        catalog: CatalogId,
        stage: String,
        percent: Option<u8>,
    },
    MemberAddCompleted {
        solution: SolutionId,
        catalog: CatalogId,
        /// `None` on success; `Some(msg)` on failure or cancellation.
        error: Option<String>,
    },
    /// Emitted when the solution-wide active-member selection changes —
    /// either repointed to a concrete member (`Some`) or cleared because the
    /// solution has no members left (`None`). UI listeners react to this so a
    /// programmatic selection change in one window is reflected in every other
    /// window showing the same Solution. Member-scoped panels (project_panel,
    /// git_panel, console_panel, title bar, run-config) rebuild on this event
    /// regardless of the payload, so the `None` case refreshes them off the
    /// just-removed project — `Changed` alone does not drive a panel rebuild.
    ActiveMemberChanged {
        solution: SolutionId,
        catalog: Option<CatalogId>,
    },
    /// Emitted when a Solution is removed from the store. Carries the
    /// solution's `root` path captured *before* removal, because the
    /// in-store mapping is gone by the time subscribers run — window
    /// reconciliation (closing the deleted solution's workspace and
    /// activating a sibling) must match worktrees by path, not by a
    /// store lookup that would now return nothing.
    Deleted {
        id: SolutionId,
        root: std::path::PathBuf,
    },
    /// Emitted by `mark_closed`. Distinct from `Changed` so UI subscribers
    /// can drive workspace-tab close-out from a single seam regardless of
    /// who triggered the close (desktop UI button vs. wire-side
    /// `workspace.close_solution` from the mobile client). Without this
    /// the wire path only flipped the `open` flag + emitted the mobile
    /// notification, leaving the corresponding desktop workspace tabs
    /// open until the user closed them manually.
    Closed {
        id: SolutionId,
    },
    /// Emitted by `mark_open`. Distinct from `Changed` so cross-crate
    /// observers (notably `workspace_events`, which can see both
    /// `SolutionStore` and `SolutionAgentStore`) can react with side
    /// effects this crate can't do itself — most importantly fanning
    /// out one `workspace.session_opened` notification per tab-pinned
    /// session for the just-opened solution. Without that fan-out the
    /// mobile mirror sees the solution row appear with `sessions: []`
    /// even when the persisted tab strip has entries.
    Opened {
        id: SolutionId,
    },
}

impl EventEmitter<SolutionStoreEvent> for SolutionStore {}

impl SolutionStore {
    pub fn init_global(cx: &mut App) {
        let db = SolutionsDb::global(cx);
        Self::init_with_db(db, cx);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn init_global_for_test(db: SolutionsDb, cx: &mut App) {
        Self::init_with_db(db, cx);
    }

    fn init_with_db(db: SolutionsDb, cx: &mut App) {
        let json_path = paths::config_dir().join("solutions.json");
        if let Err(err) = crate::migrate::run_one_time_migration(&db, &json_path) {
            log::error!("solutions::store: legacy import failed: {err}. Continuing with empty DB.");
        }
        let config = match Self::load_from_db_blocking(&db) {
            Ok(cfg) => cfg,
            Err(err) => {
                log::error!("solutions::store: failed to hydrate from DB: {err}");
                SolutionsConfig {
                    version: CURRENT_VERSION,
                    ..Default::default()
                }
            }
        };
        let active_member_rows = match gpui::block_on(db.load_all_active_members()) {
            Ok(rows) => rows,
            Err(err) => {
                log::error!("solutions::store: failed to load active_member: {err}");
                Vec::new()
            }
        };
        let mut active_member: HashMap<SolutionId, CatalogId> = HashMap::default();
        for (sid, cid) in active_member_rows {
            active_member.insert(SolutionId(sid), CatalogId(cid));
        }
        let store = cx.new(|_| SolutionStore {
            config,
            db: Some(db),
            fs_lock: Arc::new(smol::lock::Mutex::new(())),
            in_flight_adds: HashMap::default(),
            tab_snapshots: TabSnapshots::default(),
            active_member,
            open_solutions: HashSet::default(),
        });
        cx.set_global(GlobalSolutionStore(store));
    }

    fn load_from_db_blocking(db: &SolutionsDb) -> anyhow::Result<SolutionsConfig> {
        let catalog_rows = gpui::block_on(db.load_all_catalog_projects())?;
        let catalog: Vec<CatalogProject> = catalog_rows
            .into_iter()
            .map(|(id, name, remote_url, default_branch)| CatalogProject {
                id: CatalogId(id),
                name,
                remote_url,
                default_branch,
            })
            .collect();

        let solution_rows = gpui::block_on(db.load_all_solutions_with_members())?;
        let mut by_id: collections::HashMap<String, Solution> = collections::HashMap::default();
        let mut order: Vec<String> = Vec::new();
        for (sid, sname, sroot, last_opened_at, catalog_id, local_path, _position) in solution_rows
        {
            let entry = by_id.entry(sid.clone()).or_insert_with(|| {
                order.push(sid.clone());
                Solution {
                    id: SolutionId(sid.clone()),
                    name: sname,
                    root: PathBuf::from(sroot),
                    members: vec![],
                    last_opened_at: last_opened_at
                        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis),
                }
            });
            if !catalog_id.is_empty() {
                entry.members.push(SolutionMember {
                    catalog_id: CatalogId(catalog_id),
                    local_path: PathBuf::from(local_path),
                });
            }
        }
        let solutions: Vec<Solution> = order.into_iter().filter_map(|k| by_id.remove(&k)).collect();

        Ok(SolutionsConfig {
            version: CURRENT_VERSION,
            catalog,
            solutions,
        })
    }

    pub fn global(cx: &App) -> Entity<SolutionStore> {
        cx.global::<GlobalSolutionStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<SolutionStore>> {
        cx.try_global::<GlobalSolutionStore>().map(|g| g.0.clone())
    }

    /// Build an in-memory `SolutionStore` for tests. The `_config_path`
    /// argument is retained only to avoid churning the ~38 call sites
    /// across the workspace; the JSON write path was removed in Task 11
    /// (Phase 1) so the path is no longer used for anything.
    pub fn for_test(_config_path: PathBuf, cx: &mut App) -> Entity<SolutionStore> {
        cx.new(|_| SolutionStore {
            config: SolutionsConfig {
                version: CURRENT_VERSION,
                ..Default::default()
            },
            db: None,
            fs_lock: Arc::new(smol::lock::Mutex::new(())),
            in_flight_adds: HashMap::default(),
            tab_snapshots: TabSnapshots::default(),
            active_member: HashMap::default(),
            open_solutions: HashSet::default(),
        })
    }

    /// Read the open-tab snapshot (if any) for a given Solution.
    /// Empty / missing entries return `None`. Used by the in-place
    /// switch orchestrator after worktrees have been swapped to find
    /// out which buffers to re-open.
    pub fn tab_snapshot(&self, id: &SolutionId) -> Option<&SolutionTabsSnapshot> {
        self.tab_snapshots.get(id)
    }

    /// Write the open-tab snapshot for a given Solution. An empty
    /// `snapshot` (no paths and no active path) **evicts** the entry
    /// instead of storing an empty record — keeps the in-memory map
    /// trim and matches the contract that `tab_snapshot` only ever
    /// returns `Some` when there's something worth restoring.
    /// Emits `Changed` (not `ActiveSolutionChanged` — the active id
    /// itself didn't move, only the saved shape).
    pub fn store_tab_snapshot(
        &mut self,
        id: SolutionId,
        snapshot: SolutionTabsSnapshot,
        cx: &mut gpui::Context<Self>,
    ) {
        if snapshot.is_empty() {
            self.tab_snapshots.remove(&id);
        } else {
            self.tab_snapshots.insert(id, snapshot);
        }
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
    }

    pub fn catalog(&self) -> &[CatalogProject] {
        &self.config.catalog
    }

    pub fn solutions(&self) -> &[Solution] {
        &self.config.solutions
    }

    /// First Solution whose `root` is an ancestor of (or equal to) `path`.
    /// Used by the title bar to determine which Solution segment to render
    /// for the active worktree, and by tests to assert the same matching
    /// without going through the rendered UI.
    pub fn solution_for_path(&self, path: &std::path::Path) -> Option<&Solution> {
        self.config
            .solutions
            .iter()
            .find(|sol| path.starts_with(&sol.root))
    }

    pub fn fs_lock(&self) -> Arc<smol::lock::Mutex<()>> {
        Arc::clone(&self.fs_lock)
    }

    /// Insert a minimal Solution (name + temp root, no members, no DB write)
    /// for use in unit tests. Returns the generated `SolutionId`.
    /// Only available in test builds.
    #[cfg(any(test, feature = "test-support"))]
    pub fn create_for_test_minimal(&mut self, name: &str, cx: &mut Context<Self>) -> SolutionId {
        let taken: Vec<String> = self
            .config
            .solutions
            .iter()
            .map(|s| s.id.0.clone())
            .collect();
        let slug = crate::slug::unique_slug(name, &taken);
        let id = SolutionId(slug.clone());
        let root = std::env::temp_dir().join("spke-test-solutions").join(&slug);
        self.config.solutions.push(Solution {
            id: id.clone(),
            name: name.into(),
            root,
            members: vec![],
            last_opened_at: None,
        });
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        id
    }

    #[cfg(test)]
    pub fn test_force_add_member(&mut self, sid: &SolutionId, cid: &CatalogId) {
        let sol = self
            .config
            .solutions
            .iter_mut()
            .find(|s| s.id == *sid)
            .expect("test_force_add_member: solution not found");
        sol.members.push(SolutionMember {
            catalog_id: cid.clone(),
            local_path: sol.root.join(&cid.0),
        });
    }

    /// Push a member with an explicit `local_path` onto a solution, bypassing
    /// catalog resolution and DB writes. Lets downstream crates (e.g.
    /// `solution_agent`) set up member directories for orphan-GC tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_add_member_with_path(
        &mut self,
        sid: &SolutionId,
        cid: &CatalogId,
        local_path: PathBuf,
    ) {
        let sol = self
            .config
            .solutions
            .iter_mut()
            .find(|s| s.id == *sid)
            .expect("test_add_member_with_path: solution not found");
        sol.members.push(SolutionMember {
            catalog_id: cid.clone(),
            local_path,
        });
    }
}

pub(crate) struct GlobalSolutionStore(Entity<SolutionStore>);

impl Global for GlobalSolutionStore {}

pub fn install_global_for_test(entity: Entity<SolutionStore>, cx: &mut App) {
    cx.set_global(GlobalSolutionStore(entity));
}

// =====================================================================
// S-SOL-PRT — process-global cache of the active branch-protection
// snapshot. Background tasks holding only `&Path` (e.g. handler paths
// in `git_ui`, MCP tool dispatch) can call
// [`active_branch_protection_snapshot`] without entering GPUI. The
// settings half is refreshed by `SolutionsSettings::from_settings`
// (called by the settings store on every reload); the active-Solution
// half is maintained by [`refresh_active_solution_for_branch_protection`].
// =====================================================================

use crate::settings::BranchProtectionSettings;
use std::sync::Mutex;

static BRANCH_PROTECTION_CACHE: Mutex<Option<BranchProtectionCache>> = Mutex::new(None);

#[derive(Clone, Default)]
struct BranchProtectionCache {
    settings: BranchProtectionSettings,
    active_solution: Option<Solution>,
}

pub(crate) fn set_branch_protection_settings(settings: BranchProtectionSettings) {
    let Ok(mut guard) = BRANCH_PROTECTION_CACHE.lock() else {
        log::warn!("solutions::store: branch-protection cache mutex poisoned");
        return;
    };
    let entry = guard.get_or_insert_with(BranchProtectionCache::default);
    entry.settings = settings;
}

pub(crate) fn set_active_solution_for_branch_protection(solution: Option<Solution>) {
    let Ok(mut guard) = BRANCH_PROTECTION_CACHE.lock() else {
        log::warn!("solutions::store: branch-protection cache mutex poisoned");
        return;
    };
    let entry = guard.get_or_insert_with(BranchProtectionCache::default);
    entry.active_solution = solution;
}

/// Returns `(settings, active_solution)` for non-GPUI callers (the
/// branch-protection check). `None` when the settings half hasn't been
/// installed yet (e.g. during early init or in unit tests outside
/// `TestAppContext`).
pub(crate) fn active_branch_protection_snapshot()
-> Option<(BranchProtectionSettings, Option<Solution>)> {
    let guard = BRANCH_PROTECTION_CACHE.lock().ok()?;
    let entry = guard.as_ref()?;
    Some((entry.settings.clone(), entry.active_solution.clone()))
}

/// Refresh the active Solution stored in the global branch-protection
/// cache. Called by `solutions::init` whenever `ActiveSolutionChanged`
/// fires. Idempotent and side-effect-free apart from the Mutex write.
pub fn refresh_active_solution_for_branch_protection(cx: &App) {
    let Some(store) = SolutionStore::try_global(cx) else {
        set_active_solution_for_branch_protection(None);
        return;
    };
    let store = store.read(cx);
    // Pick the most-recently-opened Solution as "active", matching the
    // heuristic used by the title bar / aggregated log MCP tool.
    let active = store
        .solutions()
        .iter()
        .filter(|s| s.last_opened_at.is_some())
        .max_by_key(|s| s.last_opened_at)
        .or_else(|| store.solutions().first())
        .cloned();
    set_active_solution_for_branch_protection(active);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support;
    use gpui::TestAppContext;
    use std::time::Duration;
    use tempfile::tempdir;

    /// Name and remote are both unique keys. A clash on either is an error —
    /// NOT a silent `-2` slug (which produced two picker rows with the identical
    /// label and URL) and NOT a silent reuse of the existing entry (which would
    /// hand the caller back someone else's project). Both are the "hidden magic"
    /// the registry must not do.
    #[gpui::test]
    async fn add_catalog_project_rejects_duplicate_name_and_remote(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let cfg_path = dir.path().join("solutions.json");
        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));

        let id1 = store
            .update(cx, |s, cx| {
                s.add_catalog_project("Foo", "git@x:foo.git", None, cx)
            })
            .expect("first add");
        assert_eq!(id1.as_str(), "foo");

        let same_name = store.update(cx, |s, cx| {
            s.add_catalog_project("Foo", "git@x:other-foo.git", None, cx)
        });
        assert!(
            same_name
                .unwrap_err()
                .to_string()
                .contains("duplicate_name"),
            "a second project under an existing name must be refused"
        );

        // Same repo, different label, and the `.git` suffix dropped — still the
        // same remote.
        let same_remote = store.update(cx, |s, cx| {
            s.add_catalog_project("Foo Clone", "git@x:foo", None, cx)
        });
        assert!(
            same_remote
                .unwrap_err()
                .to_string()
                .contains("duplicate_remote"),
            "re-registering a repository already in the catalog must be refused"
        );

        let count = store.read_with(cx, |s, _| s.catalog().len());
        assert_eq!(count, 1, "no rejected add may have landed a row");
    }

    #[gpui::test]
    async fn edit_catalog_project_rejects_duplicate_name(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let cfg_path = dir.path().join("solutions.json");
        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));

        store
            .update(cx, |s, cx| {
                s.add_catalog_project("Foo", "git@x:foo.git", None, cx)
            })
            .expect("add foo");
        let bar = store
            .update(cx, |s, cx| {
                s.add_catalog_project("Bar", "git@x:bar.git", None, cx)
            })
            .expect("add bar");

        let clash = store.update(cx, |s, cx| {
            s.edit_catalog_project(&bar, Some("foo".into()), None, None, cx)
        });
        assert!(
            clash.unwrap_err().to_string().contains("duplicate_name"),
            "Edit must not be a back door for the duplicates Add refuses"
        );
        // The rejected edit must not have half-written the new name.
        let still_bar = store.read_with(cx, |s, _| {
            s.catalog()
                .iter()
                .find(|c| c.id == bar)
                .map(|c| c.name.clone())
        });
        assert_eq!(still_bar.as_deref(), Some("Bar"));

        // A no-op self-rename is still allowed.
        store
            .update(cx, |s, cx| {
                s.edit_catalog_project(&bar, Some("Bar".into()), None, None, cx)
            })
            .expect("self-rename must be allowed");
    }

    #[gpui::test]
    async fn remove_catalog_refuses_when_referenced(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let cfg_path = dir.path().join("solutions.json");
        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));

        let cat_id = store
            .update(cx, |s, cx| {
                s.add_catalog_project("Foo", "git@x:foo.git", None, cx)
            })
            .expect("add catalog");
        let solutions_root = std::env::temp_dir().join("spke-test-solutions");
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("Sol", solutions_root, cx))
            .expect("create solution");
        store.update(cx, |s, _| {
            s.test_force_add_member(&sol_id, &cat_id);
        });

        let result = store.update(cx, |s, cx| s.remove_catalog_project(&cat_id, cx));
        assert!(result.is_err(), "expected refusal");
    }

    #[gpui::test]
    async fn remove_active_member_repoints_to_remaining(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("Sol", dir.path().to_path_buf(), cx))
            .expect("create");
        let a = CatalogId("alpha".into());
        let b = CatalogId("beta".into());
        store.update(cx, |s, _| {
            s.test_force_add_member(&sol_id, &a);
            s.test_force_add_member(&sol_id, &b);
        });
        // Make `alpha` the active member, then remove it.
        store.update(cx, |s, cx| s.set_active_member(sol_id.clone(), a.clone(), cx));
        store
            .update(cx, |s, cx| s.remove_member(&sol_id, &a, cx))
            .expect("remove active member");
        // Active member must follow to the surviving member, not go `None`
        // (which would make the project panel fall back to "show all").
        assert_eq!(
            store.read_with(cx, |s, _| s.active_member(&sol_id).cloned()),
            Some(b),
        );
    }

    #[gpui::test]
    async fn remove_last_member_clears_active(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("Sol", dir.path().to_path_buf(), cx))
            .expect("create");
        let only = CatalogId("only".into());
        store.update(cx, |s, _| s.test_force_add_member(&sol_id, &only));
        store.update(cx, |s, cx| s.set_active_member(sol_id.clone(), only.clone(), cx));

        // Collect events so we can assert the cleared-member notification
        // actually fires — member-scoped panels rebuild on `ActiveMemberChanged`
        // (not `Changed`), so without the `{ catalog: None }` emit a stale tree
        // would stay on screen after the last member is removed.
        let events: std::sync::Arc<std::sync::Mutex<Vec<SolutionStoreEvent>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let _sub = cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&store, move |_store, ev: &SolutionStoreEvent, _cx| {
                events.lock().expect("events lock").push(ev.clone());
            })
        });

        store
            .update(cx, |s, cx| s.remove_member(&sol_id, &only, cx))
            .expect("remove last member");
        cx.run_until_parked();
        assert_eq!(
            store.read_with(cx, |s, _| s.active_member(&sol_id).cloned()),
            None,
        );
        let events = events.lock().expect("events lock");
        assert!(
            events.iter().any(|e| matches!(e,
                SolutionStoreEvent::ActiveMemberChanged { solution, catalog }
                    if *solution == sol_id && catalog.is_none())),
            "expected ActiveMemberChanged {{ catalog: None }} on last-member removal; got: {events:?}"
        );
    }

    #[gpui::test]
    async fn solution_for_path_matches_root_and_descendants(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        let root_base = dir.path().join("alpha-root");
        std::fs::create_dir_all(&root_base).expect("mkdir sol root");
        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("Alpha", root_base.clone(), cx)
            })
            .expect("create solution");
        // create_solution joins the slug onto root_base — fetch the real root.
        let actual_root = store
            .read_with(cx, |s, _| {
                s.solutions()
                    .iter()
                    .find(|x| x.id == sol_id)
                    .map(|x| x.root.clone())
            })
            .expect("solution exists");

        store.read_with(cx, |s, _| {
            // Exact match on the stored root.
            assert_eq!(
                s.solution_for_path(&actual_root).map(|x| x.id.clone()),
                Some(sol_id.clone()),
            );
            // Descendant.
            assert_eq!(
                s.solution_for_path(&actual_root.join("nested/file.rs"))
                    .map(|x| x.id.clone()),
                Some(sol_id.clone()),
            );
            // Sibling at the same parent — not under actual_root.
            let sibling = root_base.join("not-alpha");
            assert!(s.solution_for_path(&sibling).is_none());
            // Path above the root → not contained.
            assert!(s.solution_for_path(&root_base).is_none());
            // Unrelated path.
            assert!(
                s.solution_for_path(std::path::Path::new("/tmp/elsewhere"))
                    .is_none(),
            );
        });
    }

    #[gpui::test]
    async fn solution_for_path_returns_none_when_no_solutions(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        store.read_with(cx, |s, _| {
            assert!(s.solution_for_path(dir.path()).is_none());
        });
    }

    #[gpui::test]
    async fn remove_catalog_project_cascade_drops_from_solutions(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let cfg_path = dir.path().join("solutions.json");
        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));

        let cat_a = store
            .update(cx, |s, cx| s.add_catalog_project("A", "git@x:a", None, cx))
            .expect("add A");
        let cat_b = store
            .update(cx, |s, cx| s.add_catalog_project("B", "git@x:b", None, cx))
            .expect("add B");
        let sol_one = store
            .update(cx, |s, cx| {
                s.create_solution("One", dir.path().to_path_buf(), cx)
            })
            .expect("sol One");
        let sol_two = store
            .update(cx, |s, cx| {
                s.create_solution("Two", dir.path().to_path_buf(), cx)
            })
            .expect("sol Two");
        store.update(cx, |s, _| {
            s.test_force_add_member(&sol_one, &cat_a);
            s.test_force_add_member(&sol_one, &cat_b);
            s.test_force_add_member(&sol_two, &cat_a);
        });

        // Removing A cascades into both solutions; B is untouched.
        let dropped_paths = store
            .update(cx, |s, cx| s.remove_catalog_project_cascade(&cat_a, cx))
            .expect("cascade remove");
        // Cascade returns the local paths the caller now owns the
        // responsibility of wiping. test_force_add_member assigns them
        // synthetically; we just confirm the count is right.
        assert_eq!(dropped_paths.len(), 2, "two members of A were dropped");

        store.read_with(cx, |s, _| {
            assert!(
                s.catalog().iter().all(|c| c.id != cat_a),
                "catalog entry must be gone"
            );
            assert!(s.catalog().iter().any(|c| c.id == cat_b), "B preserved");
            let one = s.solutions().iter().find(|x| x.id == sol_one).unwrap();
            assert_eq!(one.members.len(), 1, "One keeps only B");
            assert_eq!(one.members[0].catalog_id, cat_b);
            let two = s.solutions().iter().find(|x| x.id == sol_two).unwrap();
            assert!(two.members.is_empty(), "Two had only A so it ends up empty");
        });
    }

    #[gpui::test]
    async fn remove_catalog_project_cascade_errors_for_unknown_id(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("solutions.json"), cx));
        let result = store.update(cx, |s, cx| {
            s.remove_catalog_project_cascade(&CatalogId("ghost".into()), cx)
        });
        assert!(result.is_err());
    }

    #[gpui::test]
    async fn solutions_referencing_lists_consumers(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("solutions.json"), cx));
        let cat = store
            .update(cx, |s, cx| s.add_catalog_project("X", "git@x:x", None, cx))
            .expect("add X");
        let sol_a = store
            .update(cx, |s, cx| {
                s.create_solution("A", dir.path().to_path_buf(), cx)
            })
            .expect("sol A");
        let sol_b = store
            .update(cx, |s, cx| {
                s.create_solution("B", dir.path().to_path_buf(), cx)
            })
            .expect("sol B");
        store.update(cx, |s, _| {
            s.test_force_add_member(&sol_a, &cat);
            s.test_force_add_member(&sol_b, &cat);
        });
        let mut refs = store.read_with(cx, |s, _| s.solutions_referencing(&cat));
        refs.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].0, sol_a);
        assert_eq!(refs[1].0, sol_b);
    }

    #[gpui::test]
    async fn edit_catalog_url_rewrites_member_origin(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempdir().expect("tempdir");
        let bare = test_support::make_bare_with_one_commit(dir.path()).await;
        let cache_root = dir.path().join("cache");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        std::fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        let original_url = bare.to_str().expect("path str").to_string();
        let cat_id = store
            .update(cx, |s, cx| {
                s.add_catalog_project("Bare", &original_url, Some("master".into()), cx)
            })
            .expect("add catalog");
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("S", solutions_root, cx))
            .expect("create solution");
        let task = store.update(cx, |s, cx| {
            s.add_member(sol_id.clone(), cat_id.clone(), cache_root, cx)
        });
        task.await.expect("add_member success");

        let new_url = format!("{original_url}-renamed");
        store
            .update(cx, |s, cx| {
                s.edit_catalog_project(&cat_id, None, None, Some(new_url.clone()), cx)
            })
            .expect("edit catalog");

        let local_path = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|x| x.id == sol_id)
                .unwrap()
                .members[0]
                .local_path
                .clone()
        });
        // The URL update spawns a foreground task that shells out to
        // `git remote set-url`. Drive the executor until the new URL
        // shows up in `.git/config`. We poll instead of asserting after
        // a single `run_until_parked` because the spawned `git` child
        // process awaits real I/O outside the GPUI scheduler — one
        // pump cycle isn't enough.
        let config_path = local_path.join(".git/config");
        let mut attempts = 0u32;
        let observed = loop {
            cx.run_until_parked();
            cx.background_executor
                .timer(Duration::from_millis(50))
                .await;
            let text = std::fs::read_to_string(&config_path).expect("read .git/config");
            let url = text
                .lines()
                .map(str::trim)
                .find(|line| line.starts_with("url ="))
                .map(|line| line.trim_start_matches("url =").trim().to_string());
            if url.as_deref() == Some(new_url.as_str()) {
                break url.unwrap();
            }
            attempts += 1;
            assert!(
                attempts < 100,
                "origin URL never updated; last seen {:?}",
                url
            );
        };
        assert_eq!(observed, new_url);
    }

    #[gpui::test]
    async fn paths_for_open_returns_member_paths_in_order(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("S", dir.path().to_path_buf(), cx)
            })
            .expect("create solution");
        let cat_a = store
            .update(cx, |s, cx| s.add_catalog_project("A", "git@x:a", None, cx))
            .expect("add A");
        let cat_b = store
            .update(cx, |s, cx| s.add_catalog_project("B", "git@x:b", None, cx))
            .expect("add B");
        store.update(cx, |s, _| {
            s.test_force_add_member(&sol_id, &cat_a);
            s.test_force_add_member(&sol_id, &cat_b);
        });
        let paths = store.read_with(cx, |s, _| s.paths_for_open(&sol_id).expect("paths"));
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("a"));
        assert!(paths[1].ends_with("b"));
    }

    #[gpui::test]
    async fn store_tab_snapshot_round_trips(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("s.json"), cx));
        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("S", dir.path().to_path_buf(), cx)
            })
            .expect("create solution");
        let snapshot = SolutionTabsSnapshot {
            open_paths: vec![PathBuf::from("/x"), PathBuf::from("/y")],
            active_path: Some(PathBuf::from("/y")),
        };
        store.update(cx, |s, cx| {
            s.store_tab_snapshot(sol_id.clone(), snapshot.clone(), cx);
        });
        let recovered = store.read_with(cx, |s, _| s.tab_snapshot(&sol_id).cloned());
        assert_eq!(recovered, Some(snapshot));
    }

    #[gpui::test]
    async fn store_tab_snapshot_empty_evicts(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("s.json"), cx));
        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("S", dir.path().to_path_buf(), cx)
            })
            .expect("create solution");
        store.update(cx, |s, cx| {
            s.store_tab_snapshot(
                sol_id.clone(),
                SolutionTabsSnapshot {
                    open_paths: vec![PathBuf::from("/x")],
                    active_path: None,
                },
                cx,
            );
            s.store_tab_snapshot(sol_id.clone(), SolutionTabsSnapshot::default(), cx);
        });
        let still = store.read_with(cx, |s, _| s.tab_snapshot(&sol_id).cloned());
        assert!(
            still.is_none(),
            "default (empty) snapshot must evict the entry; got {still:?}"
        );
    }

    #[gpui::test]
    async fn set_active_member_emits(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("s.json"), cx));
        let sol = SolutionId("s1".into());
        let cat = CatalogId("cat-a".into());
        let events: std::sync::Arc<std::sync::Mutex<Vec<SolutionStoreEvent>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let _sub = cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&store, move |_store, ev: &SolutionStoreEvent, _cx| {
                events.lock().expect("events lock").push(ev.clone());
            })
        });
        store.update(cx, |s, cx| s.set_active_member(sol.clone(), cat.clone(), cx));
        cx.run_until_parked();
        assert_eq!(
            store.read_with(cx, |s, _| s.active_member(&sol).cloned()),
            Some(cat.clone())
        );
        let events = events.lock().expect("events lock");
        assert!(
            events.iter().any(|e| matches!(e,
                SolutionStoreEvent::ActiveMemberChanged { solution, catalog }
                    if *solution == sol && catalog.as_ref() == Some(&cat))),
            "expected ActiveMemberChanged event; got: {events:?}"
        );
    }

    #[gpui::test]
    async fn ensure_active_member_seeds_first_when_absent(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("s.json"), cx));
        let sol = SolutionId("s1".into());
        let cat_a = CatalogId("cat-a".into());
        let cat_b = CatalogId("cat-b".into());
        let members = vec![
            SolutionMember {
                catalog_id: cat_a.clone(),
                local_path: std::path::PathBuf::from("/tmp/a"),
            },
            SolutionMember {
                catalog_id: cat_b.clone(),
                local_path: std::path::PathBuf::from("/tmp/b"),
            },
        ];
        // No selection yet → seeds first member.
        let result = store.update(cx, |s, cx| s.ensure_active_member(&sol, &members, cx));
        assert_eq!(result, Some(cat_a.clone()));
        assert_eq!(
            store.read_with(cx, |s, _| s.active_member(&sol).cloned()),
            Some(cat_a.clone())
        );
        // Already set to a member → returns existing without change.
        let result2 = store.update(cx, |s, cx| s.ensure_active_member(&sol, &members, cx));
        assert_eq!(result2, Some(cat_a));
        // Existing selection removed from members → reseeds to first remaining.
        let members2 = vec![SolutionMember {
            catalog_id: cat_b.clone(),
            local_path: std::path::PathBuf::from("/tmp/b"),
        }];
        let result3 = store.update(cx, |s, cx| s.ensure_active_member(&sol, &members2, cx));
        assert_eq!(result3, Some(cat_b.clone()));
        assert_eq!(
            store.read_with(cx, |s, _| s.active_member(&sol).cloned()),
            Some(cat_b)
        );
        // Empty members → returns None.
        let result4 = store.update(cx, |s, cx| s.ensure_active_member(&sol, &[], cx));
        assert_eq!(result4, None);
    }

    #[gpui::test]
    async fn touch_last_opened_emits_active_solution_changed(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("s.json"), cx));
        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("S", dir.path().to_path_buf(), cx)
            })
            .expect("create solution");
        let events: std::sync::Arc<std::sync::Mutex<Vec<SolutionStoreEvent>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let _sub = cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&store, move |_store, ev: &SolutionStoreEvent, _cx| {
                events.lock().expect("events lock").push(ev.clone());
            })
        });
        store
            .update(cx, |s, cx| s.touch_last_opened(&sol_id, cx))
            .expect("touch");
        cx.run_until_parked();
        let events = events.lock().expect("events lock");
        let active_changes: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                SolutionStoreEvent::ActiveSolutionChanged(id) => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            active_changes,
            vec![sol_id],
            "expected exactly one ActiveSolutionChanged for the touched id; got events: {:?}",
            events
        );
    }
}
