use crate::add_member::InFlightAdd;
use crate::db::SolutionsDb;
use crate::git;
use crate::model::{CatalogId, CatalogProject, Solution, SolutionId, SolutionMember};
use crate::persistence::{CURRENT_VERSION, SolutionsConfig};
use crate::slug::unique_slug;
use crate::tabs_snapshot::{SolutionTabsSnapshot, TabSnapshots};
use anyhow::{Context as _, Result, bail};
use chrono::Utc;
use collections::{HashMap, HashSet};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global};
use project::WorktreeId;
use std::path::PathBuf;
use std::sync::Arc;
use util::ResultExt as _;

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

    /// Read the solution-wide active catalog member, or `None` if nothing
    /// has been recorded yet. Backed by an in-memory cache hydrated from
    /// the `active_member` DB table at init.
    pub fn active_member(&self, solution: &SolutionId) -> Option<&CatalogId> {
        self.active_member.get(solution)
    }

    /// Persist the solution-wide active catalog member through the DB and
    /// update the in-memory cache. Emits `ActiveMemberChanged` so other
    /// windows observing the store can mirror the change. No-op if the
    /// given catalog is already the active member for this solution.
    pub fn set_active_member(
        &mut self,
        solution: SolutionId,
        catalog: CatalogId,
        cx: &mut Context<Self>,
    ) {
        if self.active_member.get(&solution) == Some(&catalog) {
            return;
        }
        self.active_member.insert(solution.clone(), catalog.clone());
        if let Some(db) = self.db.clone() {
            let (sid, cid) = (solution.0.clone(), catalog.0.clone());
            cx.background_spawn(async move {
                db.set_active_member(sid, cid).await.log_err();
            })
            .detach();
        }
        cx.emit(SolutionStoreEvent::ActiveMemberChanged {
            solution,
            catalog: Some(catalog),
        });
        cx.notify();
    }

    /// Return the active catalog member for the solution, seeding it to
    /// the first member in `members` if no selection has been recorded
    /// yet (or the recorded one is no longer a member). Returns `None`
    /// if `members` is empty.
    pub fn ensure_active_member(
        &mut self,
        solution: &SolutionId,
        members: &[SolutionMember],
        cx: &mut Context<Self>,
    ) -> Option<CatalogId> {
        if let Some(existing) = self.active_member.get(solution) {
            if members.iter().any(|m| &m.catalog_id == existing) {
                return Some(existing.clone());
            }
        }
        let first = members.first()?.catalog_id.clone();
        self.set_active_member(solution.clone(), first.clone(), cx);
        Some(first)
    }

    /// Seed the solution-wide active member to the first member when none is
    /// recorded yet. No-op if an active member is already set or the solution
    /// has no members. Called from the member-add paths so the active member
    /// is valid the instant a solution gains its first project — panels and
    /// new AI / terminal sessions depend on it to scope to the project rather
    /// than falling back to the solution root.
    pub(crate) fn seed_active_member_if_unset(
        &mut self,
        solution: &SolutionId,
        cx: &mut Context<Self>,
    ) {
        if self.active_member.contains_key(solution) {
            return;
        }
        let first = self
            .config
            .solutions
            .iter()
            .find(|s| &s.id == solution)
            .and_then(|s| s.members.first())
            .map(|m| m.catalog_id.clone());
        if let Some(first) = first {
            self.set_active_member(solution.clone(), first, cx);
        }
    }

    /// Resolve the active catalog member for `solution` to the matching
    /// worktree in `project`, using prefix matching on `member.local_path`.
    /// Returns `None` if no active member is set, the member is not found
    /// in the solution, or no worktree's `abs_path` starts with the member's
    /// `local_path`.
    pub fn active_member_worktree(
        &self,
        solution: &Solution,
        project: &Entity<project::Project>,
        cx: &App,
    ) -> Option<(CatalogId, WorktreeId)> {
        let catalog = self.active_member.get(&solution.id)?;
        let member = solution.members.iter().find(|m| &m.catalog_id == catalog)?;
        let worktree = project
            .read(cx)
            .worktrees(cx)
            .find(|w| w.read(cx).abs_path().starts_with(&member.local_path))?;
        Some((catalog.clone(), worktree.read(cx).id()))
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

    pub fn add_catalog_project(
        &mut self,
        name: &str,
        remote_url: &str,
        default_branch: Option<String>,
        cx: &mut gpui::Context<Self>,
    ) -> Result<CatalogId> {
        let taken: Vec<String> = self.config.catalog.iter().map(|c| c.id.0.clone()).collect();
        let slug = unique_slug(name, &taken);
        let id = CatalogId(slug);
        self.config.catalog.push(CatalogProject {
            id: id.clone(),
            name: name.into(),
            remote_url: remote_url.into(),
            default_branch,
        });
        self.db_save_catalog(self.config.catalog.last().expect("just pushed"))?;
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(id)
    }

    pub fn edit_catalog_project(
        &mut self,
        id: &CatalogId,
        new_name: Option<String>,
        new_default_branch: Option<String>,
        new_remote_url: Option<String>,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let proj = self
            .config
            .catalog
            .iter_mut()
            .find(|p| p.id == *id)
            .with_context(|| format!("catalog_not_found: {}", id.0))?;
        if let Some(name) = new_name {
            proj.name = name;
        }
        if let Some(branch) = new_default_branch {
            proj.default_branch = Some(branch);
        }
        // Track whether the URL actually changed so we can propagate the
        // new value to existing solution-member clones below. Comparing
        // before reassigning avoids both no-op `git remote set-url`
        // round-trips and re-reading the freshly-written value out of
        // `proj` after the assignment.
        let url_change: Option<String> = new_remote_url.and_then(|url| {
            if proj.remote_url == url {
                None
            } else {
                proj.remote_url = url.clone();
                Some(url)
            }
        });
        let updated = self
            .config
            .catalog
            .iter()
            .find(|c| c.id == *id)
            .expect("just edited")
            .clone();
        self.db_save_catalog(&updated)?;
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();

        if let Some(new_url) = url_change {
            // For every existing clone that points at this catalog entry,
            // rewrite `.git/config`'s `origin` so the next pull / fetch
            // hits the new URL. The warm-cache key is hashed from the URL
            // (see `cache.rs`), so a stale `origin` plus a fresh cache
            // would eventually diverge — better to fix both halves
            // atomically. Fire-and-forget on the foreground executor (so
            // the GPUI test scheduler can pump it deterministically); a
            // failed `git remote set-url` is logged but not surfaced to
            // the user, since the worst case is "next fetch fails with
            // the old error" which is the pre-edit state anyway.
            let targets: Vec<PathBuf> = self
                .config
                .solutions
                .iter()
                .flat_map(|sol| sol.members.iter())
                .filter(|m| m.catalog_id == *id)
                .map(|m| m.local_path.clone())
                .collect();
            if !targets.is_empty() {
                cx.spawn(async move |_, _| {
                    for target in targets {
                        if let Err(err) = git::set_remote_url(&target, "origin", &new_url).await {
                            log::warn!(
                                "edit_catalog_project: git remote set-url failed for {}: {err}",
                                target.display(),
                            );
                        }
                    }
                })
                .detach();
            }
        }
        Ok(())
    }

    pub fn remove_catalog_project(
        &mut self,
        id: &CatalogId,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let referenced_by: Vec<String> = self
            .config
            .solutions
            .iter()
            .filter(|s| s.members.iter().any(|m| m.catalog_id == *id))
            .map(|s| s.name.clone())
            .collect();
        if !referenced_by.is_empty() {
            bail!(
                "catalog project {} is used by solution(s): {}",
                id.0,
                referenced_by.join(", ")
            );
        }
        self.config.catalog.retain(|c| c.id != *id);
        self.db_delete_catalog(id)?;
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(())
    }

    /// Snapshot of which solutions reference a given catalog entry. Used
    /// by the delete-confirmation modal to render "this will be removed
    /// from N solution(s):" before the user pulls the trigger.
    pub fn solutions_referencing(&self, id: &CatalogId) -> Vec<(SolutionId, String)> {
        self.config
            .solutions
            .iter()
            .filter(|s| s.members.iter().any(|m| m.catalog_id == *id))
            .map(|s| (s.id.clone(), s.name.clone()))
            .collect()
    }

    /// Remove a catalog entry, cascading the deletion to every solution
    /// that references it (drops the matching `SolutionMember` from
    /// each). Returns the list of clone directories that the caller
    /// should remove from disk — disk cleanup is the caller's
    /// responsibility (mirrors `delete_solution` which expects the
    /// `DeleteSolutionModal` to wipe `solution.root`). No-op + `bail!`
    /// if the id is not in the catalog.
    pub fn remove_catalog_project_cascade(
        &mut self,
        id: &CatalogId,
        cx: &mut gpui::Context<Self>,
    ) -> Result<Vec<PathBuf>> {
        if !self.config.catalog.iter().any(|c| c.id == *id) {
            bail!("catalog_not_found: {}", id.0);
        }
        let mut clone_paths: Vec<PathBuf> = Vec::new();
        for sol in self.config.solutions.iter_mut() {
            sol.members.retain(|m| {
                if m.catalog_id == *id {
                    clone_paths.push(m.local_path.clone());
                    false
                } else {
                    true
                }
            });
        }
        // Also drop any in-flight or failed `add_member` rows for this
        // catalog id so the panel doesn't paint orphan "Adding…" /
        // "Failed: …" rows after the catalog entry itself is gone.
        self.in_flight_adds.retain(|(_, cat), _| cat != id);
        self.config.catalog.retain(|c| c.id != *id);
        if let Some(db) = self.db.as_ref() {
            gpui::block_on(async {
                for sol in self.config.solutions.iter() {
                    db.delete_solution_member(sol.id.0.clone(), id.0.clone())
                        .await
                        .log_err();
                }
                db.delete_catalog_project(id.0.clone()).await
            })?;
        }
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(clone_paths)
    }

    pub fn create_solution(
        &mut self,
        name: &str,
        root_base: PathBuf,
        cx: &mut gpui::Context<Self>,
    ) -> Result<SolutionId> {
        let taken: Vec<String> = self
            .config
            .solutions
            .iter()
            .map(|s| s.id.0.clone())
            .collect();
        let slug = unique_slug(name, &taken);
        let id = SolutionId(slug.clone());
        let root = root_base.join(&slug);
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        self.config.solutions.push(Solution {
            id: id.clone(),
            name: name.into(),
            root,
            members: vec![],
            last_opened_at: None,
        });
        self.db_save_solution(self.config.solutions.last().expect("just pushed"))?;
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(id)
    }

    pub fn rename_solution(
        &mut self,
        id: &SolutionId,
        new_name: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let sol = self.find_solution_mut(id)?;
        sol.name = new_name.into();
        let updated = self
            .config
            .solutions
            .iter()
            .find(|s| s.id == *id)
            .expect("just renamed")
            .clone();
        self.db_save_solution(&updated)?;
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(())
    }

    pub fn delete_solution(&mut self, id: &SolutionId, cx: &mut gpui::Context<Self>) -> Result<()> {
        // Capture the root before removal so the `Deleted` event can carry
        // it — subscribers can no longer look the solution up by id once
        // it's gone from the store.
        let root = self
            .config
            .solutions
            .iter()
            .find(|s| s.id == *id)
            .map(|s| s.root.clone());
        let before = self.config.solutions.len();
        self.config.solutions.retain(|s| s.id != *id);
        if self.config.solutions.len() == before {
            bail!("solution not found: {}", id.0);
        }
        self.db_delete_solution(id)?;
        // DB row for this solution's active_member is removed by
        // `ON DELETE CASCADE`; mirror that on the in-memory cache so
        // stale entries don't leak past the deletion.
        self.active_member.remove(id);
        // Emit the sequenced workspace-level notification so the mobile snapshot
        // (and any other listener) updates regardless of who triggered the delete.
        // Reserve seq first, then drop the borrow, then emit — avoids holding
        // &WorkspaceEventCoordinator while also borrowing cx for emit_notification.
        let seq_opt = editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx)
            .map(|c| c.next_seq());
        if let Some(seq) = seq_opt {
            editor_mcp::emit_notification(
                cx,
                "workspace.solution_deleted",
                serde_json::json!({
                    "seq": seq,
                    "solution_id": id.as_str(),
                }),
            );
        }
        cx.emit(SolutionStoreEvent::Changed);
        if let Some(root) = root {
            cx.emit(SolutionStoreEvent::Deleted {
                id: id.clone(),
                root,
            });
        }
        cx.notify();
        Ok(())
    }

    pub fn touch_last_opened(
        &mut self,
        id: &SolutionId,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let sol = self.find_solution_mut(id)?;
        sol.last_opened_at = Some(Utc::now());
        let ts = sol.last_opened_at.expect("just set").timestamp_millis();
        self.db_update_last_opened(id, ts)?;
        // `Changed` first so listeners that watch the broad signal
        // see the broadcast in chronological order; the more specific
        // `ActiveSolutionChanged` follows so subscribers that only
        // care about the active-id-flipped case can ignore `Changed`.
        cx.emit(SolutionStoreEvent::Changed);
        cx.emit(SolutionStoreEvent::ActiveSolutionChanged(id.clone()));
        cx.notify();
        Ok(())
    }

    pub fn remove_member(
        &mut self,
        solution_id: &SolutionId,
        catalog_id: &CatalogId,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let sol = self.find_solution_mut(solution_id)?;
        let before = sol.members.len();
        sol.members.retain(|m| m.catalog_id != *catalog_id);
        if sol.members.len() == before {
            bail!("member not in solution");
        }
        self.db_delete_member(solution_id, catalog_id)?;
        // If the removed member was the solution-wide active one, repoint to a
        // remaining member or clear the selection. Panels scoped to the active
        // member (project_panel, git_panel, …) filter their visible worktree by
        // `active_member_path`; with no active member that filter falls back to
        // "show all worktrees", which keeps the just-removed project's now-empty
        // tree on screen. Both branches emit `ActiveMemberChanged` (the empty
        // case with `None`), which those panels subscribe to, so they rebuild
        // deterministically onto a surviving project — or off the removed one
        // when the solution is now empty — without depending on the caller's
        // worktree teardown (`Changed` alone does not drive a panel rebuild).
        if self.active_member.get(solution_id) == Some(catalog_id) {
            let next = self
                .config
                .solutions
                .iter()
                .find(|s| &s.id == solution_id)
                .and_then(|s| s.members.first())
                .map(|m| m.catalog_id.clone());
            match next {
                Some(next) => self.set_active_member(solution_id.clone(), next, cx),
                None => {
                    self.active_member.remove(solution_id);
                    if let Some(db) = self.db.clone() {
                        let sid = solution_id.0.clone();
                        cx.background_spawn(async move {
                            db.clear_active_member(sid).await.log_err();
                        })
                        .detach();
                    }
                    cx.emit(SolutionStoreEvent::ActiveMemberChanged {
                        solution: solution_id.clone(),
                        catalog: None,
                    });
                }
            }
        }
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(())
    }

    pub fn reorder_members(
        &mut self,
        solution_id: &SolutionId,
        new_order: Vec<CatalogId>,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let sol = self.find_solution_mut(solution_id)?;
        let mut by_id: collections::HashMap<CatalogId, SolutionMember> = sol
            .members
            .drain(..)
            .map(|m| (m.catalog_id.clone(), m))
            .collect();
        for id in &new_order {
            if let Some(m) = by_id.remove(id) {
                sol.members.push(m);
            }
        }
        for (_, m) in by_id {
            sol.members.push(m);
        }
        let snapshot: Vec<SolutionMember> = sol.members.clone();
        for (i, m) in snapshot.iter().enumerate() {
            self.db_set_member(solution_id, m, i as i32)?;
        }
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(())
    }

    /// Returns `true` if the solution's desktop window is currently tracked as open.
    pub fn is_open(&self, id: &SolutionId) -> bool {
        self.open_solutions.contains(id)
    }

    /// Mark a solution's window as open. Idempotent — repeat calls are no-ops.
    /// Emits `Changed` so existing MCP observers react automatically.
    /// Also emits a sequenced `workspace.solution_opened` notification so the
    /// mobile client updates regardless of who triggered the open.
    pub fn mark_open(&mut self, id: SolutionId, cx: &mut Context<Self>) {
        if !self.open_solutions.insert(id.clone()) {
            return; // already open — no-op
        }
        // Build a minimal summary inline (cannot call mcp::build_summary here
        // because that re-borrows SolutionStore via try_global, which would
        // panic while we are inside a mutable update of the same entity).
        // sessions is always empty: solutions does not depend on solution_agent
        // (cycle). The mobile client calls workspace.snapshot for full state.
        let solution_json = self
            .config
            .solutions
            .iter()
            .find(|s| s.id == id)
            .map(|sol| {
                serde_json::json!({
                    "id": sol.id.as_str(),
                    "name": sol.name,
                    "root": sol.root.to_string_lossy(),
                    "member_count": sol.members.len(),
                    "last_opened_at": sol.last_opened_at.map(|t| t.to_rfc3339()),
                    "open": true,
                    "main_window_id": serde_json::Value::Null,
                })
            });
        // Reserve seq first, then drop the borrow, then emit — avoids holding
        // &WorkspaceEventCoordinator while also borrowing cx for emit_notification.
        let seq_opt = editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx)
            .map(|c| c.next_seq());
        if let Some(seq) = seq_opt {
            editor_mcp::emit_notification(
                cx,
                "workspace.solution_opened",
                serde_json::json!({
                    "seq": seq,
                    "solution": solution_json,
                    "sessions": [],
                }),
            );
        }
        cx.emit(SolutionStoreEvent::Opened { id });
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
    }

    /// Mark a solution's window as closed. Idempotent — repeat calls are no-ops.
    /// Emits `Changed` so existing MCP observers react automatically.
    /// Also emits a sequenced `workspace.solution_closed` notification so the
    /// mobile client updates regardless of who triggered the close.
    pub fn mark_closed(&mut self, id: &SolutionId, cx: &mut Context<Self>) {
        if !self.open_solutions.remove(id) {
            return; // already closed — no-op
        }
        // Reserve seq first, then drop the borrow, then emit — avoids holding
        // &WorkspaceEventCoordinator while also borrowing cx for emit_notification.
        let seq_opt = editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx)
            .map(|c| c.next_seq());
        if let Some(seq) = seq_opt {
            editor_mcp::emit_notification(
                cx,
                "workspace.solution_closed",
                serde_json::json!({
                    "seq": seq,
                    "solution_id": id.as_str(),
                }),
            );
        }
        cx.emit(SolutionStoreEvent::Closed { id: id.clone() });
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
    }

    pub fn paths_for_open(&self, id: &SolutionId) -> Result<Vec<PathBuf>> {
        let sol = self
            .config
            .solutions
            .iter()
            .find(|s| s.id == *id)
            .with_context(|| format!("solution not found: {}", id.0))?;
        Ok(sol.members.iter().map(|m| m.local_path.clone()).collect())
    }

    fn db_save_catalog(&self, c: &CatalogProject) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.save_catalog_project(
            c.id.0.clone(),
            c.name.clone(),
            c.remote_url.clone(),
            c.default_branch.clone(),
        ))
    }

    fn db_delete_catalog(&self, id: &CatalogId) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.delete_catalog_project(id.0.clone()))
    }

    fn db_save_solution(&self, s: &Solution) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        let last_ms = s.last_opened_at.map(|t| t.timestamp_millis());
        gpui::block_on(db.save_solution(
            s.id.0.clone(),
            s.name.clone(),
            s.root.to_string_lossy().into_owned(),
            last_ms,
        ))
    }

    fn db_delete_solution(&self, id: &SolutionId) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.delete_solution_row(id.0.clone()))
    }

    pub(crate) fn db_set_member(
        &self,
        sol_id: &SolutionId,
        m: &SolutionMember,
        position: i32,
    ) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.set_solution_member(
            sol_id.0.clone(),
            m.catalog_id.0.clone(),
            m.local_path.to_string_lossy().into_owned(),
            position,
        ))
    }

    fn db_delete_member(&self, sol_id: &SolutionId, cat_id: &CatalogId) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.delete_solution_member(sol_id.0.clone(), cat_id.0.clone()))
    }

    fn db_update_last_opened(&self, id: &SolutionId, ts_ms: i64) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.update_last_opened(id.0.clone(), ts_ms))
    }

    pub(crate) fn find_solution_mut(&mut self, id: &SolutionId) -> Result<&mut Solution> {
        self.config
            .solutions
            .iter_mut()
            .find(|s| s.id == *id)
            .with_context(|| format!("solution not found: {}", id.0))
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

    #[gpui::test]
    async fn add_catalog_project_dedupes_slug(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let cfg_path = dir.path().join("solutions.json");
        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));

        let id1 = store
            .update(cx, |s, cx| {
                s.add_catalog_project("Foo", "git@x:foo.git", None, cx)
            })
            .expect("first add");
        let id2 = store
            .update(cx, |s, cx| {
                s.add_catalog_project("Foo", "git@x:other-foo.git", None, cx)
            })
            .expect("second add");
        assert_eq!(id1.as_str(), "foo");
        assert_eq!(id2.as_str(), "foo-2");
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
