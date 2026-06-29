//! In-flight tracking, progress streaming, and the async clone pipeline behind `SolutionStore::add_member`. Extracted from store.rs to keep the latter focused on persistence + lifecycle plumbing.

use crate::cache;
use crate::git::{self, GitProgress};
use crate::model::{CatalogId, SolutionId, SolutionMember};
use crate::store::{SolutionStore, SolutionStoreEvent};
use anyhow::{Context as _, Result, bail};
use gpui::{App, AppContext as _, AsyncApp, Task};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use util::ResultExt as _;

pub type AddProgressCallback = Box<dyn FnMut(&str, Option<u8>, &mut App) + 'static>;

/// `git init` a freshly-created empty member directory with no remote.
///
/// Run synchronously (blocking) via `std::process::Command`: the call site
/// [`SolutionStore::add_empty_member`] is sync and a local `git init` is a
/// sub-100ms one-shot, so spinning up the async clone machinery would be
/// overkill. Best-effort by design — the caller `.log_err()`s the result so
/// a missing/old `git` binary degrades to "plain folder, no VCS" rather
/// than failing project creation outright.
// Sync `std::process::Command` is deliberate — see the doc comment above: the
// call site is sync and `git init` is a sub-100ms one-shot, so the async
// `smol::process::Command` the lint suggests would be pure overhead here.
#[allow(clippy::disallowed_methods)]
fn init_empty_git_repo(local_path: &std::path::Path) -> Result<()> {
    let status = std::process::Command::new("git")
        .arg("init")
        .arg(local_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("spawning git init for {}", local_path.display()))?;
    anyhow::ensure!(
        status.success(),
        "git init for {} exited with {status}",
        local_path.display()
    );
    Ok(())
}

/// Internal record for an in-flight `add_member` call. The UI reads a
/// snapshot via [`SolutionStore::pending_adds_for`] and reacts to
/// [`SolutionStoreEvent::MemberAddProgress`] / `MemberAddCompleted`.
pub(crate) struct InFlightAdd {
    pub(crate) catalog_name: String,
    pub(crate) stage: String,
    pub(crate) percent: Option<u8>,
    /// `Some(_)` once the spawned task has completed with an error and is
    /// waiting for the user to either Retry or Dismiss the failure row.
    pub(crate) error: Option<String>,
    /// Soft-cancel signal: spawned task polls between git steps. We keep
    /// "soft" cancel because git child processes are not killable mid-step
    /// without losing the freshly-cloned `.git` directory in an inconsistent
    /// state — but the in-flight entry is removed from the map immediately
    /// in `cancel_add_member`, so the UI is free at once even if the
    /// background git keeps churning briefly.
    pub(crate) cancel_flag: Arc<AtomicBool>,
}

/// Public read-only view of an in-flight add for the UI panel.
#[derive(Clone, Debug)]
pub struct PendingAddView {
    pub catalog_id: CatalogId,
    pub catalog_name: String,
    pub stage: String,
    pub percent: Option<u8>,
    pub error: Option<String>,
}

impl SolutionStore {
    pub fn add_member(
        &mut self,
        solution_id: SolutionId,
        catalog_id: CatalogId,
        cache_root: PathBuf,
        cx: &mut gpui::Context<Self>,
    ) -> Task<Result<()>> {
        self.add_member_internal(solution_id, catalog_id, cache_root, None, cx)
    }

    /// Variant of [`add_member`] that also forwards every git progress tick
    /// to an external sink (used by the MCP `solutions.add_member` tool to
    /// drive `op_record_progress`). The callback runs on the foreground
    /// thread with `&mut App`.
    pub fn add_member_with_progress(
        &mut self,
        solution_id: SolutionId,
        catalog_id: CatalogId,
        cache_root: PathBuf,
        on_progress: AddProgressCallback,
        cx: &mut gpui::Context<Self>,
    ) -> Task<Result<()>> {
        self.add_member_internal(solution_id, catalog_id, cache_root, Some(on_progress), cx)
    }

    fn add_member_internal(
        &mut self,
        solution_id: SolutionId,
        catalog_id: CatalogId,
        cache_root: PathBuf,
        mut external_progress: Option<AddProgressCallback>,
        cx: &mut gpui::Context<Self>,
    ) -> Task<Result<()>> {
        let sol = match self.config.solutions.iter().find(|s| s.id == solution_id) {
            Some(s) => s.clone(),
            None => {
                let id = solution_id.0.clone();
                return cx.background_spawn(async move { bail!("solution not found: {id}") });
            }
        };
        let cat = match self.config.catalog.iter().find(|c| c.id == catalog_id) {
            Some(c) => c.clone(),
            None => {
                let id = catalog_id.0.clone();
                return cx
                    .background_spawn(async move { bail!("catalog project not found: {id}") });
            }
        };
        if sol.members.iter().any(|m| m.catalog_id == catalog_id) {
            let sol_name = sol.name;
            let cat_name = cat.name;
            return cx.background_spawn(async move {
                bail!("solution {sol_name} already contains {cat_name}")
            });
        }

        let key = (solution_id.clone(), catalog_id.clone());
        // Reject overlapping calls so two pickers can't race for the same
        // (solution, catalog) and double-clone into the target directory.
        if self.in_flight_adds.contains_key(&key) {
            let sol_name = sol.name;
            let cat_name = cat.name;
            return cx.background_spawn(async move {
                bail!("add already in progress for {cat_name} in {sol_name}")
            });
        }

        let target = sol.root.join(&catalog_id.0);
        let remote_url = cat.remote_url.clone();
        let default_branch = cat.default_branch.clone();
        let lock = Arc::clone(&self.fs_lock);
        let cancel_flag = Arc::new(AtomicBool::new(false));

        self.in_flight_adds.insert(
            key,
            InFlightAdd {
                catalog_name: cat.name,
                stage: "queued".into(),
                percent: Some(0),
                error: None,
                cancel_flag: Arc::clone(&cancel_flag),
            },
        );
        cx.emit(SolutionStoreEvent::MemberAddProgress {
            solution: solution_id.clone(),
            catalog: catalog_id.clone(),
            stage: "queued".into(),
            percent: Some(0),
        });
        cx.notify();

        cx.spawn(
            async move |weak: gpui::WeakEntity<Self>, cx: &mut AsyncApp| {
                let (tx, rx) = smol::channel::unbounded::<GitProgress>();

                // Pump git progress → in-flight entry update + Progress event +
                // optional external sink. Stops when `tx` is dropped at the end of
                // the `work` block. Awaited (not detached) before we continue, so
                // the final progress tick is observed before we mark the entry
                // complete or remove it.
                let pump = cx.spawn({
                    let weak = weak.clone();
                    let solution_id = solution_id.clone();
                    let catalog_id = catalog_id.clone();
                    async move |cx: &mut AsyncApp| {
                        while let Ok(p) = rx.recv().await {
                            let stage_for_event = p.stage.clone();
                            let percent_for_event = p.percent;
                            weak.update(cx, |store, cx| {
                                if let Some(entry) = store
                                    .in_flight_adds
                                    .get_mut(&(solution_id.clone(), catalog_id.clone()))
                                {
                                    entry.stage = p.stage.clone();
                                    entry.percent = p.percent;
                                }
                                cx.emit(SolutionStoreEvent::MemberAddProgress {
                                    solution: solution_id.clone(),
                                    catalog: catalog_id.clone(),
                                    stage: stage_for_event,
                                    percent: percent_for_event,
                                });
                                cx.notify();
                            })
                            .log_err();
                            if let Some(cb) = external_progress.as_mut() {
                                // `AsyncApp::update` is infallible (returns the
                                // closure's value directly, not a `Result`), so
                                // there's nothing to log here — `cb` itself is
                                // a no-result `FnMut`.
                                cx.update(|app| cb(&p.stage, p.percent, app));
                            }
                        }
                    }
                });

                let work_result: Result<()> = async {
                    let _guard = lock.lock().await;

                    // Forward the same `tx` into both git steps so progress lines
                    // from `git clone` (which is by far the longest step) reach
                    // the pump as they're produced.
                    let cache_tx = tx.clone();
                    let cache_path = cache::ensure_cache(&cache_root, &remote_url, move |p| {
                        let _ = cache_tx.try_send(p);
                    })
                    .await?;
                    if cancel_flag.load(Ordering::SeqCst) {
                        bail!("cancelled");
                    }

                    // Wipe any partial directory left behind by a previous
                    // cancelled / failed add — git refuses to clone into a
                    // non-empty directory.
                    if target.exists() {
                        smol::unblock({
                            let target = target.clone();
                            move || std::fs::remove_dir_all(&target)
                        })
                        .await
                        .with_context(|| format!("removing stale {}", target.display()))?;
                    }

                    let clone_tx = tx.clone();
                    git::clone_local(&cache_path, &target, move |p| {
                        let _ = clone_tx.try_send(p);
                    })
                    .await?;
                    if cancel_flag.load(Ordering::SeqCst) {
                        bail!("cancelled");
                    }

                    git::set_remote_url(&target, "origin", &remote_url).await?;
                    if let Some(branch) = default_branch.as_deref() {
                        git::checkout(&target, branch).await.ok();
                    }
                    Ok(())
                }
                .await;

                // Close the channel so the pump task drains and exits.
                drop(tx);
                pump.await;

                match work_result {
                    Ok(()) => {
                        weak.update(cx, |store, cx| {
                            let new_member_and_pos: Option<(SolutionMember, i32)> = store
                                .config
                                .solutions
                                .iter_mut()
                                .find(|s| s.id == solution_id)
                                .map(|sol| {
                                    sol.members.push(SolutionMember {
                                        catalog_id: catalog_id.clone(),
                                        local_path: target.clone(),
                                    });
                                    let new_member =
                                        sol.members.last().expect("just pushed").clone();
                                    let position = (sol.members.len() - 1) as i32;
                                    (new_member, position)
                                });
                            if let Some((new_member, position)) = new_member_and_pos {
                                store
                                    .db_set_member(&solution_id, &new_member, position)
                                    .log_err();
                            }
                            store
                                .in_flight_adds
                                .remove(&(solution_id.clone(), catalog_id.clone()));
                            // First project in the solution → make it the
                            // active member so panels and new AI sessions
                            // scope to it instead of the solution root. No-op
                            // when a member is already active. See the matching
                            // note in `add_empty_member`.
                            store.seed_active_member_if_unset(&solution_id, cx);
                            cx.emit(SolutionStoreEvent::MemberAddCompleted {
                                solution: solution_id.clone(),
                                catalog: catalog_id.clone(),
                                error: None,
                            });
                            cx.emit(SolutionStoreEvent::Changed);
                            cx.notify();
                        })?;
                        Ok(())
                    }
                    Err(err) => {
                        let err_text = err.to_string();
                        weak.update(cx, |store, cx| {
                            // If the user already pressed `cancel_add_member`,
                            // the entry is gone AND that path already emitted
                            // its own `MemberAddCompleted{ error: "cancelled" }`.
                            // Re-emitting here would double-fire the completion
                            // event for one user action. Gate the failure
                            // mutation + emit on the entry still being present.
                            if let Some(entry) = store
                                .in_flight_adds
                                .get_mut(&(solution_id.clone(), catalog_id.clone()))
                            {
                                entry.stage = "failed".into();
                                entry.percent = None;
                                entry.error = Some(err_text.clone());
                                cx.emit(SolutionStoreEvent::MemberAddCompleted {
                                    solution: solution_id.clone(),
                                    catalog: catalog_id.clone(),
                                    error: Some(err_text),
                                });
                                cx.notify();
                            }
                        })
                        .log_err();
                        Err(err)
                    }
                }
            },
        )
    }

    /// Create a member that has no catalog backing — the user wanted a
    /// fresh empty project that lives only inside this solution. Spec D4:
    /// solutions are built only from catalog clones or empty projects;
    /// external folders are not addable. The new member's `catalog_id` is a
    /// slug derived from `project_name` and uniquified against the
    /// solution's existing member catalog ids; nothing is inserted into
    /// `catalog_projects`. The directory `solution.root/<slug>` is created
    /// (incl. parents) and `git init`-ed with no remote, so the new project
    /// tracks history from the start and can be pushed somewhere later via
    /// the normal git UI. It never enters the catalog (which requires a
    /// `remote_url`), so a remote-less local project is not offered in the
    /// project picker when creating or editing other solutions. Display
    /// name in selectors comes from the path's last segment via the
    /// orphan-rendering rule.
    pub fn add_empty_member(
        &mut self,
        solution_id: &SolutionId,
        project_name: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<CatalogId> {
        let trimmed = project_name.trim();
        if trimmed.is_empty() {
            bail!("empty project name");
        }
        let sol = self.find_solution_mut(solution_id)?;
        let taken: Vec<String> = sol.members.iter().map(|m| m.catalog_id.0.clone()).collect();
        let slug = crate::slug::unique_slug(trimmed, &taken);
        let cat_id = CatalogId(slug.clone());
        let local_path = sol.root.join(&slug);
        std::fs::create_dir_all(&local_path)
            .with_context(|| format!("creating {}", local_path.display()))?;
        init_empty_git_repo(&local_path).log_err();
        sol.members.push(SolutionMember {
            catalog_id: cat_id.clone(),
            local_path,
        });
        let position = (sol.members.len() - 1) as i32;
        let new_member = sol.members.last().expect("just pushed").clone();
        self.db_set_member(solution_id, &new_member, position)
            .log_err();
        // Seed the solution-wide active member when this is the first
        // project, so panels and newly-started AI / terminal sessions scope
        // to it immediately. Without this, `active_member` stays `None` until
        // the project tab strip happens to render and seed it — and a session
        // started before that lands in the solution root ("ROOT") instead of
        // the project. `set_active_member` no-ops if one is already set, so
        // adding a second project never steals the active selection.
        self.seed_active_member_if_unset(solution_id, cx);
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(cat_id)
    }

    /// Snapshot of every in-flight or failed add for a solution. The dock
    /// panel renders these as ghost rows with spinners or error messages.
    pub fn pending_adds_for(&self, sol_id: &SolutionId) -> Vec<PendingAddView> {
        self.in_flight_adds
            .iter()
            .filter(|((s, _), _)| s == sol_id)
            .map(|((_, cat_id), entry)| PendingAddView {
                catalog_id: cat_id.clone(),
                catalog_name: entry.catalog_name.clone(),
                stage: entry.stage.clone(),
                percent: entry.percent,
                error: entry.error.clone(),
            })
            .collect()
    }

    /// Soft-cancel the in-flight add. The UI row is removed immediately and
    /// the spawned task bails at the next git boundary check. Any half-cloned
    /// directory under the solution root is left behind and will be wiped on
    /// the next successful add for the same `(solution, catalog)`.
    pub fn cancel_add_member(
        &mut self,
        solution_id: &SolutionId,
        catalog_id: &CatalogId,
        cx: &mut gpui::Context<Self>,
    ) {
        let key = (solution_id.clone(), catalog_id.clone());
        let Some(entry) = self.in_flight_adds.remove(&key) else {
            return;
        };
        entry.cancel_flag.store(true, Ordering::SeqCst);
        cx.emit(SolutionStoreEvent::MemberAddCompleted {
            solution: solution_id.clone(),
            catalog: catalog_id.clone(),
            error: Some("cancelled".into()),
        });
        cx.notify();
    }

    /// Drop a failed in-flight entry so its row disappears from the panel.
    /// No-op if the entry is still in progress (use `cancel_add_member` for
    /// that) or already gone.
    pub fn clear_failed_add(
        &mut self,
        solution_id: &SolutionId,
        catalog_id: &CatalogId,
        cx: &mut gpui::Context<Self>,
    ) {
        let key = (solution_id.clone(), catalog_id.clone());
        let drop_it = self
            .in_flight_adds
            .get(&key)
            .is_some_and(|e| e.error.is_some());
        if drop_it {
            self.in_flight_adds.remove(&key);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support;
    use gpui::TestAppContext;
    use tempfile::tempdir;

    #[gpui::test]
    async fn add_member_clones_and_records(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempdir().expect("tempdir");
        let bare = test_support::make_bare_with_one_commit(dir.path()).await;
        let cache_root = dir.path().join("cache");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        std::fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        let cat_id = store
            .update(cx, |s, cx| {
                s.add_catalog_project(
                    "Bare",
                    bare.to_str().expect("path str"),
                    Some("master".into()),
                    cx,
                )
            })
            .expect("add catalog");
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("S", solutions_root, cx))
            .expect("create solution");

        let task = store.update(cx, |s, cx| {
            s.add_member(sol_id.clone(), cat_id.clone(), cache_root, cx)
        });
        task.await.expect("add_member");

        let target = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|x| x.id == sol_id)
                .expect("solution exists")
                .members[0]
                .local_path
                .clone()
        });
        assert!(target.join(".git").exists());
    }

    #[gpui::test]
    async fn add_member_tracks_in_flight_and_clears_on_success(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempdir().expect("tempdir");
        let bare = test_support::make_bare_with_one_commit(dir.path()).await;
        let cache_root = dir.path().join("cache");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        std::fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        let cat_id = store
            .update(cx, |s, cx| {
                s.add_catalog_project(
                    "Bare",
                    bare.to_str().expect("path str"),
                    Some("master".into()),
                    cx,
                )
            })
            .expect("add catalog");
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("S", solutions_root, cx))
            .expect("create solution");

        let task = store.update(cx, |s, cx| {
            s.add_member(sol_id.clone(), cat_id.clone(), cache_root, cx)
        });

        // The store inserts the in-flight entry synchronously before the
        // spawned task takes its first poll, so the UI can render the row
        // immediately. Without this, "Add looks frozen for 2 minutes" is
        // exactly what you'd see in the UI.
        let pending = store.read_with(cx, |s, _| s.pending_adds_for(&sol_id));
        assert_eq!(pending.len(), 1);
        assert!(pending[0].error.is_none());
        assert_eq!(pending[0].catalog_id, cat_id);

        task.await.expect("add_member success");

        let pending = store.read_with(cx, |s, _| s.pending_adds_for(&sol_id));
        assert!(
            pending.is_empty(),
            "in-flight entry must be cleared on success"
        );
    }

    #[gpui::test]
    async fn add_member_records_failure_in_pending(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempdir().expect("tempdir");
        let cache_root = dir.path().join("cache");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        std::fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        // Point at a path that is not a git repo so `git clone` fails fast.
        let bogus = dir.path().join("does-not-exist.git");
        let cat_id = store
            .update(cx, |s, cx| {
                s.add_catalog_project("Bogus", bogus.to_str().expect("path str"), None, cx)
            })
            .expect("add catalog");
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("S", solutions_root, cx))
            .expect("create solution");

        let task = store.update(cx, |s, cx| {
            s.add_member(sol_id.clone(), cat_id.clone(), cache_root, cx)
        });
        let result = task.await;
        assert!(result.is_err(), "expected failure for non-existent source");

        let pending = store.read_with(cx, |s, _| s.pending_adds_for(&sol_id));
        assert_eq!(pending.len(), 1, "failed entry must persist as a row");
        assert!(pending[0].error.is_some());
        assert_eq!(pending[0].catalog_id, cat_id);

        // Clearing the failed entry removes the row.
        store.update(cx, |s, cx| s.clear_failed_add(&sol_id, &cat_id, cx));
        let pending = store.read_with(cx, |s, _| s.pending_adds_for(&sol_id));
        assert!(pending.is_empty());
    }

    #[gpui::test]
    async fn cancel_add_member_clears_in_flight_immediately(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempdir().expect("tempdir");
        let bare = test_support::make_bare_with_one_commit(dir.path()).await;
        let cache_root = dir.path().join("cache");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        std::fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        let cat_id = store
            .update(cx, |s, cx| {
                s.add_catalog_project(
                    "Bare",
                    bare.to_str().expect("path str"),
                    Some("master".into()),
                    cx,
                )
            })
            .expect("add catalog");
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("S", solutions_root, cx))
            .expect("create solution");

        // Hold the task so it doesn't get auto-dropped before we cancel —
        // we want to exercise `cancel_add_member` against an actively
        // running spawned future, mirroring what happens when the user
        // hits the Cancel button.
        let _task = store.update(cx, |s, cx| {
            s.add_member(sol_id.clone(), cat_id.clone(), cache_root, cx)
        });

        assert_eq!(
            store.read_with(cx, |s, _| s.pending_adds_for(&sol_id).len()),
            1
        );
        store.update(cx, |s, cx| s.cancel_add_member(&sol_id, &cat_id, cx));
        assert_eq!(
            store.read_with(cx, |s, _| s.pending_adds_for(&sol_id).len()),
            0,
            "UI row must disappear synchronously on cancel"
        );
    }

    #[gpui::test]
    async fn add_empty_member_creates_directory_and_member(cx: &mut TestAppContext) {
        use std::fs;
        let dir = tempdir().expect("tempdir");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("S", solutions_root.clone(), cx)
            })
            .expect("create solution");

        let cat_id = store
            .update(cx, |s, cx| s.add_empty_member(&sol_id, "Frontend", cx))
            .expect("add_empty_member");

        let (member_path, member_cat_id) = store.read_with(cx, |s, _| {
            let sol = s
                .solutions()
                .iter()
                .find(|x| x.id == sol_id)
                .expect("solution");
            let m = sol.members.first().expect("member");
            (m.local_path.clone(), m.catalog_id.clone())
        });

        assert_eq!(member_cat_id, cat_id);
        assert!(member_path.is_dir(), "directory must exist on disk");
        assert!(
            member_path.starts_with(&solutions_root),
            "must live inside solution.root"
        );
        assert_eq!(
            member_path.file_name().and_then(|n| n.to_str()),
            Some("frontend"),
            "slug from name"
        );
        assert!(
            member_path.join(".git").exists(),
            "empty member must be git-initialised (no remote)"
        );
    }

    #[gpui::test]
    async fn add_empty_member_seeds_active_member(cx: &mut TestAppContext) {
        use std::fs;
        let dir = tempdir().expect("tempdir");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("S", solutions_root, cx))
            .expect("create solution");
        assert_eq!(
            store.read_with(cx, |s, _| s.active_member(&sol_id).cloned()),
            None,
            "no active member before any project"
        );
        let first = store
            .update(cx, |s, cx| s.add_empty_member(&sol_id, "Frontend", cx))
            .expect("first add");
        assert_eq!(
            store.read_with(cx, |s, _| s.active_member(&sol_id).cloned()),
            Some(first.clone()),
            "first project must become the active member"
        );
        // A second project must not steal the active selection.
        let second = store
            .update(cx, |s, cx| s.add_empty_member(&sol_id, "Backend", cx))
            .expect("second add");
        assert_eq!(
            store.read_with(cx, |s, _| s.active_member(&sol_id).cloned()),
            Some(first.clone()),
            "adding a second project must not change the active member"
        );
        // Make a NON-first member active, then add a third. This discriminates
        // the `contains_key` guard in `seed_active_member_if_unset`: re-seeding
        // would pick `members.first()` (= the first project), so without the
        // guard the active member would be reset to `first` here. The assertion
        // that it stays on `second` only holds because the guard short-circuits.
        store.update(cx, |s, cx| {
            s.set_active_member(sol_id.clone(), second.clone(), cx)
        });
        store
            .update(cx, |s, cx| s.add_empty_member(&sol_id, "Infra", cx))
            .expect("third add");
        assert_eq!(
            store.read_with(cx, |s, _| s.active_member(&sol_id).cloned()),
            Some(second),
            "adding a project must not reset a non-first active member to the first"
        );
    }

    #[gpui::test]
    async fn add_empty_member_uniquifies_slug_within_solution(cx: &mut TestAppContext) {
        use std::fs;
        let dir = tempdir().expect("tempdir");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("S", solutions_root, cx))
            .expect("create solution");
        let id1 = store
            .update(cx, |s, cx| s.add_empty_member(&sol_id, "Frontend", cx))
            .expect("first add");
        let id2 = store
            .update(cx, |s, cx| s.add_empty_member(&sol_id, "Frontend", cx))
            .expect("second add — must not collide");
        assert_ne!(
            id1, id2,
            "two empty members from the same name must get distinct slugs"
        );
    }

    #[gpui::test]
    async fn add_empty_member_does_not_add_catalog_row(cx: &mut TestAppContext) {
        use std::fs;
        let dir = tempdir().expect("tempdir");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("S", solutions_root, cx))
            .expect("create solution");
        let _ = store
            .update(cx, |s, cx| s.add_empty_member(&sol_id, "Frontend", cx))
            .expect("add empty");
        store.read_with(cx, |s, _| {
            assert!(
                s.catalog().is_empty(),
                "empty members must not pollute the catalog"
            );
        });
    }

    #[gpui::test]
    async fn add_member_with_progress_runs_to_completion(cx: &mut TestAppContext) {
        // Verifies the with-progress entry point compiles and runs through to
        // a successful clone. We deliberately do NOT assert that the callback
        // fired: the callback is only invoked when `git --progress` emits a
        // line, and `git` is silent on tiny repos like the one this test
        // creates. Realistic-repo coverage of the streaming ticks lives in
        // `editor_mcp/tests/solutions_add_member_e2e_test.rs` (which clones
        // a real-sized repo over the MCP-driven path).
        cx.executor().allow_parking();
        let dir = tempdir().expect("tempdir");
        let bare = test_support::make_bare_with_one_commit(dir.path()).await;
        let cache_root = dir.path().join("cache");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        std::fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        let cat_id = store
            .update(cx, |s, cx| {
                s.add_catalog_project(
                    "Bare",
                    bare.to_str().expect("path str"),
                    Some("master".into()),
                    cx,
                )
            })
            .expect("add catalog");
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("S", solutions_root, cx))
            .expect("create solution");

        let cb: AddProgressCallback = Box::new(|_stage, _percent, _app| {});
        let task = store.update(cx, |s, cx| {
            s.add_member_with_progress(sol_id.clone(), cat_id.clone(), cache_root, cb, cx)
        });
        task.await.expect("add_member success");

        let pending = store.read_with(cx, |s, _| s.pending_adds_for(&sol_id));
        assert!(pending.is_empty());
    }
}
