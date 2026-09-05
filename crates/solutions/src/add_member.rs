//! In-flight tracking, progress streaming, and the async clone pipeline behind `SolutionStore::add_member`. Extracted from store.rs to keep the latter focused on persistence + lifecycle plumbing.

use crate::cache;
use crate::git::{self, GitProgress};
use crate::model::{CatalogId, MemberId, SolutionId, SolutionMember};
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

/// Delete the checkout an abandoned add cloned, so a cancel does not leave a
/// full working copy under the solution root for the next add to trip over.
///
/// Taken under `fs_lock`, and only after re-checking that nothing has claimed
/// the folder in the meantime: cancelling releases the reservation
/// immediately, so a second add may already have reserved this exact
/// directory and cloned into it — deleting it then would be precisely the
/// data loss the reservation exists to prevent. Best-effort by design; a
/// leftover directory is reclaimed by the next add of the same project.
async fn discard_abandoned_clone(
    weak: &gpui::WeakEntity<SolutionStore>,
    cx: &mut AsyncApp,
    lock: &Arc<smol::lock::Mutex<()>>,
    solution_id: SolutionId,
    catalog_id: CatalogId,
    folder: &str,
    target: &std::path::Path,
) {
    let _guard = lock.lock().await;
    let claimed = weak
        .update(cx, |store, _| {
            store.folder_claimed_by_other(solution_id, catalog_id, folder)
        })
        .unwrap_or(true);
    if claimed || !target.exists() {
        return;
    }
    smol::unblock({
        let target = target.to_path_buf();
        move || std::fs::remove_dir_all(&target)
    })
    .await
    .log_err();
}

/// Internal record for an in-flight `add_member` call. The UI reads a
/// snapshot via [`SolutionStore::pending_adds_for`] and reacts to
/// [`SolutionStoreEvent::MemberAddProgress`] / `MemberAddCompleted`.
pub(crate) struct InFlightAdd {
    pub(crate) catalog_name: String,
    /// The folder under the solution root this add reserved when it was
    /// spawned. It is what makes the reservation account for adds that are
    /// still running: without it two adds whose catalog names derive to the
    /// same folder ("Update Deps" and "Update-Deps") both computed the same
    /// `target`, and the second one's "wipe the stale target" step deleted
    /// the first one's freshly-cloned checkout.
    pub(crate) folder: String,
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
                return cx
                    .background_spawn(async move { bail!("solution not found: {solution_id}") });
            }
        };
        let cat = match self.config.catalog.iter().find(|c| c.id == catalog_id) {
            Some(c) => c.clone(),
            None => {
                return cx.background_spawn(async move {
                    bail!("catalog project not found: {catalog_id}")
                });
            }
        };
        if sol
            .members
            .iter()
            .any(|m| m.origin_catalog_id == Some(catalog_id))
        {
            let sol_name = sol.name;
            let cat_name = cat.name;
            return cx.background_spawn(async move {
                bail!("solution {sol_name} already contains {cat_name}")
            });
        }

        let key = (solution_id, catalog_id);
        // Reject overlapping calls so two pickers can't race for the same
        // (solution, catalog) and double-clone into the target directory.
        if self.in_flight_adds.contains_key(&key) {
            let sol_name = sol.name;
            let cat_name = cat.name;
            return cx.background_spawn(async move {
                bail!("add already in progress for {cat_name} in {sol_name}")
            });
        }

        // The clone folder is derived from the catalog project's NAME (it used
        // to be derived from the catalog id, which WAS the slug of the name —
        // same string, different source), by the same rule creation and rename
        // use.
        //
        // Uniquified against the solution's existing member folders AND the
        // folders every still-running add of this solution reserved, but
        // deliberately NOT against disk: the clone below wipes a stale target
        // left by a cancelled or failed add *of this same catalog project*,
        // and a disk check would step around that garbage into `-2` instead
        // of reclaiming it. The in-memory check is what keeps two *live*
        // members from sharing a directory — without the in-flight half, two
        // adds whose names derive to one folder both computed the same target
        // and the second one's wipe deleted the first member's checkout.
        //
        // The reservation is made HERE, in the same synchronous block that
        // inserts the in-flight entry, rather than under `fs_lock` inside the
        // spawned task: `add_member` runs on the foreground thread, so
        // derive-uniquify-insert is already atomic against every other
        // `add_member` call, while `fs_lock` is only taken once the task is
        // polled — by which time a second call has long since picked its
        // folder.
        let derived = match crate::folder_name::derive(&cat.name) {
            Ok(folder) => folder,
            Err(err) => return cx.background_spawn(async move { Err(anyhow::Error::new(err)) }),
        };
        let mut taken: Vec<String> = sol
            .members
            .iter()
            .filter_map(|m| Some(m.local_path.file_name()?.to_string_lossy().into_owned()))
            .collect();
        taken.extend(
            self.in_flight_adds
                .iter()
                .filter(|((in_flight_solution, _), _)| *in_flight_solution == solution_id)
                .map(|(_, entry)| entry.folder.clone()),
        );
        let Some(folder) = crate::folder_name::uniquify(&derived, |candidate| {
            !taken
                .iter()
                .any(|t| crate::folder_name::same_folder_name(t, candidate))
        }) else {
            let cat_name = cat.name;
            return cx.background_spawn(async move {
                bail!("no free directory name left for {cat_name}")
            });
        };
        let target = sol.root.join(&folder);
        let remote_url = cat.remote_url.clone();
        let default_branch = cat.default_branch.clone();
        let lock = Arc::clone(&self.fs_lock);
        let cancel_flag = Arc::new(AtomicBool::new(false));

        self.in_flight_adds.insert(
            key,
            InFlightAdd {
                catalog_name: cat.name,
                folder: folder.clone(),
                stage: "queued".into(),
                percent: Some(0),
                error: None,
                cancel_flag: Arc::clone(&cancel_flag),
            },
        );
        cx.emit(SolutionStoreEvent::MemberAddProgress {
            solution: solution_id,
            catalog: catalog_id,
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
                    async move |cx: &mut AsyncApp| {
                        while let Ok(p) = rx.recv().await {
                            let stage_for_event = p.stage.clone();
                            let percent_for_event = p.percent;
                            weak.update(cx, |store, cx| {
                                if let Some(entry) =
                                    store.in_flight_adds.get_mut(&(solution_id, catalog_id))
                                {
                                    entry.stage = p.stage.clone();
                                    entry.percent = p.percent;
                                }
                                cx.emit(SolutionStoreEvent::MemberAddProgress {
                                    solution: solution_id,
                                    catalog: catalog_id,
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
                        let cancelled = cancel_flag.load(Ordering::SeqCst);
                        let landed = weak.update(cx, |store, cx| {
                            store.land_added_member(
                                solution_id,
                                catalog_id,
                                &folder,
                                &target,
                                cancelled,
                                cx,
                            )
                        })??;
                        if !landed {
                            discard_abandoned_clone(
                                &weak,
                                cx,
                                &lock,
                                solution_id,
                                catalog_id,
                                &folder,
                                &target,
                            )
                            .await;
                            if cancelled {
                                bail!("cancelled");
                            }
                            bail!("add abandoned: no in-flight entry left for {catalog_id}");
                        }
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
                            if let Some(entry) =
                                store.in_flight_adds.get_mut(&(solution_id, catalog_id))
                            {
                                entry.stage = "failed".into();
                                entry.percent = None;
                                entry.error = Some(err_text.clone());
                                cx.emit(SolutionStoreEvent::MemberAddCompleted {
                                    solution: solution_id,
                                    catalog: catalog_id,
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

    /// Publish a finished clone as a solution member — unless the add was
    /// abandoned while the last git steps ran, in which case nothing is
    /// mutated and `false` is returned so the caller can clean the checkout
    /// up.
    ///
    /// Two ways to be abandoned, both of which used to land a member anyway:
    /// the user pressed Cancel (`cancel_flag`; `set_remote_url` + `checkout`
    /// are seconds of work on a large repo, and the UI row disappeared the
    /// moment they clicked), or the in-flight entry is gone for some other
    /// reason — `remove_catalog_project_cascade` drops the entries of a
    /// catalog row that was deleted mid-clone, and a member whose
    /// `origin_catalog_id` points at nothing is not a member anyone asked
    /// for. Landing one anyway also armed the *next* add of the same project
    /// to wipe the checkout it had just published.
    pub(crate) fn land_added_member(
        &mut self,
        solution_id: SolutionId,
        catalog_id: CatalogId,
        folder: &str,
        target: &std::path::Path,
        cancelled: bool,
        cx: &mut gpui::Context<Self>,
    ) -> Result<bool> {
        if cancelled || !self.in_flight_adds.contains_key(&(solution_id, catalog_id)) {
            return Ok(false);
        }
        let position = self
            .config
            .solutions
            .iter()
            .find(|s| s.id == solution_id)
            .map(|sol| sol.members.len() as i32);
        if let Some(position) = position {
            // Allocate the member id through the DB so the row and the
            // in-memory member agree; `for_test` stores with no DB fall back
            // to the shared in-memory counter.
            let member_id = match self.db.as_ref() {
                Some(db) => MemberId(gpui::block_on(db.insert_solution_member(
                    solution_id.0,
                    folder.to_string(),
                    target.to_string_lossy().into_owned(),
                    position,
                    Some(catalog_id.0),
                ))?),
                None => MemberId(self.next_id_without_db()),
            };
            let member = SolutionMember {
                id: member_id,
                name: folder.to_string(),
                local_path: target.to_path_buf(),
                origin_catalog_id: Some(catalog_id),
            };
            if let Some(sol) = self
                .config
                .solutions
                .iter_mut()
                .find(|s| s.id == solution_id)
            {
                sol.members.push(member);
            }
        }
        self.in_flight_adds.remove(&(solution_id, catalog_id));
        // First project in the solution → make it the active member so panels
        // and new AI sessions scope to it instead of the solution root. No-op
        // when a member is already active. See the matching note in
        // `add_empty_member`.
        self.seed_active_member_if_unset(solution_id, cx);
        cx.emit(SolutionStoreEvent::MemberAddCompleted {
            solution: solution_id,
            catalog: catalog_id,
            error: None,
        });
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(true)
    }

    /// Is `folder` under `solution`'s root spoken for by a landed member, or
    /// reserved by an in-flight add other than `catalog`'s? Guards the
    /// cleanup of an abandoned clone: a cancel frees the reservation
    /// immediately (the UI row has to go at once), so by the time the
    /// abandoned task gets around to deleting its directory another add may
    /// legitimately own it.
    pub(crate) fn folder_claimed_by_other(
        &self,
        solution: SolutionId,
        catalog: CatalogId,
        folder: &str,
    ) -> bool {
        let members = self
            .config
            .solutions
            .iter()
            .find(|s| s.id == solution)
            .map(|s| s.members.as_slice())
            .unwrap_or_default();
        members.iter().any(|member| {
            member.local_path.file_name().is_some_and(|name| {
                crate::folder_name::same_folder_name(&name.to_string_lossy(), folder)
            })
        }) || self.in_flight_adds.iter().any(|((sol, cat), entry)| {
            *sol == solution
                && *cat != catalog
                && crate::folder_name::same_folder_name(&entry.folder, folder)
        })
    }

    /// Create a member that has no catalog backing — the user wanted a
    /// fresh empty project that lives only inside this solution. Spec D4:
    /// solutions are built only from catalog clones or empty projects;
    /// external folders are not addable. The new member's folder name is a
    /// slug derived from `project_name` and uniquified against the solution's
    /// existing member names; nothing is inserted into `catalog_projects`. The
    /// directory `solution.root/<slug>` is created (incl. parents) and
    /// `git init`-ed with no remote, so the new project tracks history from
    /// the start and can be pushed somewhere later via the normal git UI. It
    /// never enters the catalog (which requires a `remote_url`), so a
    /// remote-less local project is not offered in the project picker when
    /// creating or editing other solutions.
    pub fn add_empty_member(
        &mut self,
        solution_id: SolutionId,
        project_name: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<MemberId> {
        let trimmed = project_name.trim();
        if trimmed.is_empty() {
            bail!("empty project name");
        }
        let sol = self.find_solution(solution_id)?;
        let folder = crate::folder_name::derive(trimmed)?;
        // Uniquify against the sibling members' *folders* (not their display
        // names, which a rename lets drift away from the folder) and against
        // disk, so a leftover directory is never adopted as a new project.
        let taken: Vec<crate::rename::TakenFolder> = sol
            .members
            .iter()
            .filter_map(|member| {
                Some(crate::rename::TakenFolder {
                    folder: member
                        .local_path
                        .file_name()?
                        .to_string_lossy()
                        .into_owned(),
                    owner: member.name.clone(),
                })
            })
            .collect();
        let local_path = crate::rename::first_available_folder(&sol.root, &folder, &taken)?;
        let folder = local_path
            .file_name()
            .context("member path has no file name")?
            .to_string_lossy()
            .into_owned();
        let position = sol.members.len() as i32;
        std::fs::create_dir_all(&local_path)
            .with_context(|| format!("creating {}", local_path.display()))?;
        init_empty_git_repo(&local_path).log_err();

        let member_id = match self.db.as_ref() {
            Some(db) => MemberId(gpui::block_on(db.insert_solution_member(
                solution_id.0,
                folder.clone(),
                local_path.to_string_lossy().into_owned(),
                position,
                None,
            ))?),
            None => MemberId(self.next_id_without_db()),
        };
        let sol = self.find_solution_mut(solution_id)?;
        sol.members.push(SolutionMember {
            id: member_id,
            name: folder,
            local_path,
            origin_catalog_id: None,
        });
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
        Ok(member_id)
    }

    /// Snapshot of every in-flight or failed add for a solution. The dock
    /// panel renders these as ghost rows with spinners or error messages.
    pub fn pending_adds_for(&self, sol_id: SolutionId) -> Vec<PendingAddView> {
        self.in_flight_adds
            .iter()
            .filter(|((s, _), _)| *s == sol_id)
            .map(|((_, cat_id), entry)| PendingAddView {
                catalog_id: *cat_id,
                // Prefer the catalog's CURRENT name over the one snapshotted
                // when the add started: the recovery path for a failed add is
                // "edit the project, then retry", and a tab still labelled with
                // the old typo'd name after the edit reads as a second stuck
                // entry. Falls back to the snapshot if the catalog row is gone.
                catalog_name: self
                    .config
                    .catalog
                    .iter()
                    .find(|c| c.id == *cat_id)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| entry.catalog_name.clone()),
                stage: entry.stage.clone(),
                percent: entry.percent,
                error: entry.error.clone(),
            })
            .collect()
    }

    /// Soft-cancel the in-flight add. The UI row is removed immediately and
    /// the spawned task bails at the next git boundary check — including the
    /// one *after* the last git step, so a cancel that lands during
    /// `set_remote_url` / `checkout` no longer publishes a member anyway.
    /// A task that got far enough to produce a checkout deletes it on its way
    /// out ([`discard_abandoned_clone`]); a directory left half-written by an
    /// earlier bail is wiped by the next add for the same
    /// `(solution, catalog)`.
    pub fn cancel_add_member(
        &mut self,
        solution_id: SolutionId,
        catalog_id: CatalogId,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(entry) = self.in_flight_adds.remove(&(solution_id, catalog_id)) else {
            return;
        };
        entry.cancel_flag.store(true, Ordering::SeqCst);
        cx.emit(SolutionStoreEvent::MemberAddCompleted {
            solution: solution_id,
            catalog: catalog_id,
            error: Some("cancelled".into()),
        });
        cx.notify();
    }

    /// Drop a failed in-flight entry so its row disappears from the panel.
    /// No-op if the entry is still in progress (use `cancel_add_member` for
    /// that) or already gone.
    pub fn clear_failed_add(
        &mut self,
        solution_id: SolutionId,
        catalog_id: CatalogId,
        cx: &mut gpui::Context<Self>,
    ) {
        let key = (solution_id, catalog_id);
        let drop_it = self
            .in_flight_adds
            .get(&key)
            .is_some_and(|e| e.error.is_some());
        if drop_it {
            self.in_flight_adds.remove(&key);
            cx.notify();
        }
    }

    /// Clear a failed add and immediately start it again. Retrying is a
    /// separate entry point rather than "dismiss, then add from the picker"
    /// because [`add_member`] refuses to start while an entry for the same
    /// `(solution, catalog)` is still in the map — a caller that forgot the
    /// clear would get a confusing "add already in progress" instead of a
    /// retry. Errors (unknown ids, a genuinely in-flight entry) surface
    /// through the returned task exactly as they do for `add_member`.
    pub fn retry_failed_add(
        &mut self,
        solution_id: SolutionId,
        catalog_id: CatalogId,
        cache_root: PathBuf,
        cx: &mut gpui::Context<Self>,
    ) -> Task<Result<()>> {
        self.clear_failed_add(solution_id, catalog_id, cx);
        self.add_member(solution_id, catalog_id, cache_root, cx)
    }

    /// Is this catalog project safe to delete along with a failed add — i.e.
    /// is the failed clone the ONLY thing referencing it? Drives whether the
    /// failed tab offers "Remove Project from Catalog": a project that other
    /// Solutions already cloned is not junk left over from this typo, and
    /// [`remove_catalog_project`](Self::remove_catalog_project) would refuse
    /// it anyway.
    pub fn catalog_project_is_unreferenced(&self, catalog_id: CatalogId) -> bool {
        self.config.catalog.iter().any(|c| c.id == catalog_id)
            && !self
                .config
                .solutions
                .iter()
                .flat_map(|s| s.members.iter())
                .any(|m| m.origin_catalog_id == Some(catalog_id))
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

        let task = store.update(cx, |s, cx| s.add_member(sol_id, cat_id, cache_root, cx));
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

        let task = store.update(cx, |s, cx| s.add_member(sol_id, cat_id, cache_root, cx));

        // The store inserts the in-flight entry synchronously before the
        // spawned task takes its first poll, so the UI can render the row
        // immediately. Without this, "Add looks frozen for 2 minutes" is
        // exactly what you'd see in the UI.
        let pending = store.read_with(cx, |s, _| s.pending_adds_for(sol_id));
        assert_eq!(pending.len(), 1);
        assert!(pending[0].error.is_none());
        assert_eq!(pending[0].catalog_id, cat_id);

        task.await.expect("add_member success");

        let pending = store.read_with(cx, |s, _| s.pending_adds_for(sol_id));
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

        let task = store.update(cx, |s, cx| s.add_member(sol_id, cat_id, cache_root, cx));
        let result = task.await;
        assert!(result.is_err(), "expected failure for non-existent source");

        let pending = store.read_with(cx, |s, _| s.pending_adds_for(sol_id));
        assert_eq!(pending.len(), 1, "failed entry must persist as a row");
        assert!(pending[0].error.is_some());
        assert_eq!(pending[0].catalog_id, cat_id);

        // Clearing the failed entry removes the row.
        store.update(cx, |s, cx| s.clear_failed_add(sol_id, cat_id, cx));
        let pending = store.read_with(cx, |s, _| s.pending_adds_for(sol_id));
        assert!(pending.is_empty());
    }

    /// The whole escape path from the reported dead end: a mistyped remote
    /// fails the clone, the user edits the catalog entry to fix the URL, and
    /// retries from the failed tab. `retry_failed_add` exists because
    /// `add_member` refuses to start while the failed entry is still in the
    /// map, so "just add it again" is not the same thing.
    #[gpui::test]
    async fn retry_failed_add_clears_the_failure_and_clones_the_fixed_url(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempdir().expect("tempdir");
        let bare = test_support::make_bare_with_one_commit(dir.path()).await;
        let cache_root = dir.path().join("cache");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        std::fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        let bogus = dir.path().join("does-not-exist.git");
        let cat_id = store
            .update(cx, |s, cx| {
                s.add_catalog_project("Typo", bogus.to_str().expect("path str"), None, cx)
            })
            .expect("add catalog");
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("S", solutions_root, cx))
            .expect("create solution");

        let task = store.update(cx, |s, cx| {
            s.add_member(sol_id, cat_id, cache_root.clone(), cx)
        });
        assert!(task.await.is_err(), "the bogus remote must fail to clone");
        assert!(
            store.read_with(cx, |s, _| s.pending_adds_for(sol_id))[0]
                .error
                .is_some(),
            "the failure must be parked as a row for the user to act on"
        );

        store
            .update(cx, |s, cx| {
                s.edit_catalog_project(
                    cat_id,
                    Some("Fixed".into()),
                    Some("master".into()),
                    Some(bare.to_string_lossy().into_owned()),
                    cx,
                )
            })
            .expect("edit catalog");
        // The parked row picks up the corrected name straight away, so the
        // user is not staring at the typo they just fixed.
        assert_eq!(
            store.read_with(cx, |s, _| s.pending_adds_for(sol_id))[0].catalog_name,
            "Fixed",
        );

        let task = store.update(cx, |s, cx| {
            s.retry_failed_add(sol_id, cat_id, cache_root, cx)
        });
        task.await.expect("retry must clone the fixed URL");

        assert!(
            store
                .read_with(cx, |s, _| s.pending_adds_for(sol_id))
                .is_empty(),
            "a successful retry leaves no pending row behind"
        );
        let target = store.read_with(cx, |s, _| {
            s.find_solution(sol_id).expect("solution").members[0]
                .local_path
                .clone()
        });
        assert!(target.join(".git").exists(), "the member must be cloned");
    }

    /// Drives whether the failed tab offers "Remove Project from Catalog".
    #[gpui::test]
    async fn catalog_project_is_unreferenced_flips_once_a_member_exists(cx: &mut TestAppContext) {
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

        assert!(
            store.read_with(cx, |s, _| s.catalog_project_is_unreferenced(cat_id)),
            "a project nothing has cloned yet is removable"
        );
        assert!(
            !store.read_with(cx, |s, _| s
                .catalog_project_is_unreferenced(CatalogId(9999))),
            "an id that is not in the catalog at all is not 'removable'"
        );

        let task = store.update(cx, |s, cx| s.add_member(sol_id, cat_id, cache_root, cx));
        task.await.expect("add_member");
        assert!(
            !store.read_with(cx, |s, _| s.catalog_project_is_unreferenced(cat_id)),
            "once a Solution has cloned it, deleting the catalog row is not cleanup"
        );
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
        let _task = store.update(cx, |s, cx| s.add_member(sol_id, cat_id, cache_root, cx));

        assert_eq!(
            store.read_with(cx, |s, _| s.pending_adds_for(sol_id).len()),
            1
        );
        store.update(cx, |s, cx| s.cancel_add_member(sol_id, cat_id, cx));
        assert_eq!(
            store.read_with(cx, |s, _| s.pending_adds_for(sol_id).len()),
            0,
            "UI row must disappear synchronously on cancel"
        );
    }

    /// `make_bare_with_one_commit` always writes `seed.git` into the
    /// directory it is handed, so a test that needs two distinct remotes
    /// (the catalog refuses two rows pointing at the same repository) gives
    /// each one its own parent.
    async fn bare_repo(root: &std::path::Path, name: &str) -> PathBuf {
        let parent = root.join(name);
        std::fs::create_dir_all(&parent).expect("mkdir bare parent");
        test_support::make_bare_with_one_commit(&parent).await
    }

    /// Two adds started before either one runs, whose catalog names derive to
    /// the SAME folder. The folder reservation used to be taken against
    /// `sol.members` only — and an in-flight add is not a member — so both
    /// computed `<root>/Update-Deps`, and the second one's "wipe the stale
    /// target" step did a `remove_dir_all` on the first member's freshly
    /// cloned checkout and then pushed a second member on the same path.
    #[gpui::test]
    async fn concurrent_adds_deriving_one_folder_get_separate_directories(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempdir().expect("tempdir");
        let first_bare = bare_repo(dir.path(), "first").await;
        let second_bare = bare_repo(dir.path(), "second").await;
        let cache_root = dir.path().join("cache");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        std::fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        let first_cat = store
            .update(cx, |s, cx| {
                s.add_catalog_project(
                    "Update Deps",
                    first_bare.to_str().expect("path str"),
                    Some("master".into()),
                    cx,
                )
            })
            .expect("first catalog");
        let second_cat = store
            .update(cx, |s, cx| {
                s.add_catalog_project(
                    "Update-Deps",
                    second_bare.to_str().expect("path str"),
                    Some("master".into()),
                    cx,
                )
            })
            .expect("second catalog");
        assert_eq!(
            crate::folder_name::derive("Update Deps").as_deref(),
            crate::folder_name::derive("Update-Deps").as_deref(),
            "the premise of this test: both names derive to one folder",
        );
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("S", solutions_root, cx))
            .expect("create solution");

        let first = store.update(cx, |s, cx| {
            s.add_member(sol_id, first_cat, cache_root.clone(), cx)
        });
        let second = store.update(cx, |s, cx| s.add_member(sol_id, second_cat, cache_root, cx));
        first.await.expect("first add");
        second.await.expect("second add");

        let members = store.read_with(cx, |s, _| {
            s.find_solution(sol_id).expect("solution").members.clone()
        });
        assert_eq!(members.len(), 2, "both adds must land");
        assert_ne!(
            members[0].local_path, members[1].local_path,
            "two live members must never share a directory",
        );
        for member in &members {
            assert!(
                member.local_path.join(".git").exists(),
                "{} must still hold its checkout",
                member.local_path.display(),
            );
        }
    }

    /// The same race, but the two derived folders differ only in NON-ASCII
    /// case. The reservation's `eq_ignore_ascii_case` called them two
    /// folders while `rename`'s Unicode fold called them one, so on a
    /// case-insensitive filesystem the second clone landed on top of the
    /// first member's directory. Asserted through the shared predicate
    /// rather than through path inequality, because on a case-sensitive
    /// filesystem (this test's) the buggy pair really are two directories —
    /// the damage only shows up on macOS/Windows.
    #[gpui::test]
    async fn concurrent_adds_differing_only_in_unicode_case_get_separate_folders(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let dir = tempdir().expect("tempdir");
        let first_bare = bare_repo(dir.path(), "first").await;
        let second_bare = bare_repo(dir.path(), "second").await;
        let cache_root = dir.path().join("cache");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        std::fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        let first_cat = store
            .update(cx, |s, cx| {
                s.add_catalog_project(
                    "Проект Один",
                    first_bare.to_str().expect("path str"),
                    Some("master".into()),
                    cx,
                )
            })
            .expect("first catalog");
        let second_cat = store
            .update(cx, |s, cx| {
                s.add_catalog_project(
                    "проект-Один",
                    second_bare.to_str().expect("path str"),
                    Some("master".into()),
                    cx,
                )
            })
            .expect("second catalog");
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("S", solutions_root, cx))
            .expect("create solution");

        let first = store.update(cx, |s, cx| {
            s.add_member(sol_id, first_cat, cache_root.clone(), cx)
        });
        let second = store.update(cx, |s, cx| s.add_member(sol_id, second_cat, cache_root, cx));
        first.await.expect("first add");
        second.await.expect("second add");

        let folders: Vec<String> = store.read_with(cx, |s, _| {
            s.find_solution(sol_id)
                .expect("solution")
                .members
                .iter()
                .filter_map(|m| Some(m.local_path.file_name()?.to_string_lossy().into_owned()))
                .collect()
        });
        assert_eq!(folders.len(), 2, "both adds must land");
        assert!(
            !crate::folder_name::same_folder_name(&folders[0], &folders[1]),
            "two members must not resolve to one directory on a case-insensitive \
             filesystem, got {folders:?}",
        );
    }

    /// The publish gate. Both refusals used to be missing: the `Ok` arm
    /// pushed a member no matter what, so a Cancel that arrived during
    /// `set_remote_url` / `git checkout` produced a `cancelled` completion
    /// event AND a landed member on a path the next add would wipe.
    #[gpui::test]
    async fn an_abandoned_add_does_not_land_a_member(cx: &mut TestAppContext) {
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
            .update(cx, |s, cx| {
                s.create_solution("S", solutions_root.clone(), cx)
            })
            .expect("create solution");

        // Hold the task so the in-flight entry stays put while we drive the
        // gate by hand — racing a real Cancel against a real `git checkout`
        // on a one-commit repo is not something a test can time.
        let _task = store.update(cx, |s, cx| s.add_member(sol_id, cat_id, cache_root, cx));
        let target = store
            .read_with(cx, |s, _| {
                s.find_solution(sol_id).map(|sol| sol.root.clone())
            })
            .expect("solution")
            .join("Bare");
        std::fs::create_dir_all(&target).expect("mkdir target");

        let landed = store
            .update(cx, |s, cx| {
                s.land_added_member(sol_id, cat_id, "Bare", &target, true, cx)
            })
            .expect("gate");
        assert!(!landed, "a cancelled add must not publish a member");

        // The other half: nothing was cancelled, but the in-flight entry is
        // gone — `remove_catalog_project_cascade` drops it when the catalog
        // row is deleted mid-clone.
        store.update(cx, |s, cx| s.cancel_add_member(sol_id, cat_id, cx));
        let landed = store
            .update(cx, |s, cx| {
                s.land_added_member(sol_id, cat_id, "Bare", &target, false, cx)
            })
            .expect("gate");
        assert!(
            !landed,
            "an add whose in-flight entry vanished must not publish a member"
        );
        assert!(
            store.read_with(cx, |s, _| s
                .find_solution(sol_id)
                .expect("solution")
                .members
                .is_empty()),
            "no member may exist for an abandoned add"
        );

        // …and the checkout it left behind is cleaned up rather than parked
        // under the solution root for the next add to wipe.
        let lock = store.read_with(cx, |s, _| Arc::clone(&s.fs_lock));
        let weak = store.downgrade();
        let mut async_cx = cx.to_async();
        discard_abandoned_clone(&weak, &mut async_cx, &lock, sol_id, cat_id, "Bare", &target).await;
        assert!(
            !target.exists(),
            "the abandoned clone must not be left on disk"
        );
    }

    /// The guard that keeps the cleanup above from becoming the very data
    /// loss it is cleaning up after: cancelling frees the reservation at
    /// once, so another add may legitimately own the directory by the time
    /// the abandoned task gets to delete it.
    #[gpui::test]
    async fn an_abandoned_clone_is_not_deleted_once_someone_else_owns_the_folder(
        cx: &mut TestAppContext,
    ) {
        let dir = tempdir().expect("tempdir");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        std::fs::create_dir_all(&solutions_root).expect("mkdir solutions");

        let store = cx.update(|cx| SolutionStore::for_test(cfg_path, cx));
        let sol_id = store
            .update(cx, |s, cx| s.create_solution("S", solutions_root, cx))
            .expect("create solution");
        store
            .update(cx, |s, cx| s.add_empty_member(sol_id, "Frontend", cx))
            .expect("add empty member");
        let target = store.read_with(cx, |s, _| {
            s.find_solution(sol_id).expect("solution").members[0]
                .local_path
                .clone()
        });

        store.read_with(cx, |s, _| {
            assert!(
                s.folder_claimed_by_other(sol_id, CatalogId(1), "Frontend"),
                "a landed member owns its folder"
            );
            assert!(
                s.folder_claimed_by_other(sol_id, CatalogId(1), "frontend"),
                "ownership is decided by the shared case-insensitive predicate"
            );
            assert!(
                !s.folder_claimed_by_other(sol_id, CatalogId(1), "Backend"),
                "an unrelated folder is free"
            );
        });

        let lock = store.read_with(cx, |s, _| Arc::clone(&s.fs_lock));
        let weak = store.downgrade();
        let mut async_cx = cx.to_async();
        discard_abandoned_clone(
            &weak,
            &mut async_cx,
            &lock,
            sol_id,
            CatalogId(1),
            "Frontend",
            &target,
        )
        .await;
        assert!(
            target.is_dir(),
            "a live member's checkout must survive another add's cleanup"
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

        let member_id = store
            .update(cx, |s, cx| s.add_empty_member(sol_id, "Frontend", cx))
            .expect("add_empty_member");

        let (member_path, stored_member_id, origin) = store.read_with(cx, |s, _| {
            let sol = s
                .solutions()
                .iter()
                .find(|x| x.id == sol_id)
                .expect("solution");
            let m = sol.members.first().expect("member");
            (m.local_path.clone(), m.id, m.origin_catalog_id)
        });

        assert_eq!(stored_member_id, member_id);
        assert_eq!(origin, None, "an empty member has no catalog provenance");
        assert!(member_path.is_dir(), "directory must exist on disk");
        assert!(
            member_path.starts_with(&solutions_root),
            "must live inside solution.root"
        );
        assert_eq!(
            member_path.file_name().and_then(|n| n.to_str()),
            Some("Frontend"),
            "folder derived from the name, case preserved"
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
            store.read_with(cx, |s, _| s.active_member(sol_id)),
            None,
            "no active member before any project"
        );
        let first = store
            .update(cx, |s, cx| s.add_empty_member(sol_id, "Frontend", cx))
            .expect("first add");
        assert_eq!(
            store.read_with(cx, |s, _| s.active_member(sol_id)),
            Some(first),
            "first project must become the active member"
        );
        // A second project must not steal the active selection.
        let second = store
            .update(cx, |s, cx| s.add_empty_member(sol_id, "Backend", cx))
            .expect("second add");
        assert_eq!(
            store.read_with(cx, |s, _| s.active_member(sol_id)),
            Some(first),
            "adding a second project must not change the active member"
        );
        // Make a NON-first member active, then add a third. This discriminates
        // the `contains_key` guard in `seed_active_member_if_unset`: re-seeding
        // would pick `members.first()` (= the first project), so without the
        // guard the active member would be reset to `first` here. The assertion
        // that it stays on `second` only holds because the guard short-circuits.
        store.update(cx, |s, cx| s.set_active_member(sol_id, second, cx));
        store
            .update(cx, |s, cx| s.add_empty_member(sol_id, "Infra", cx))
            .expect("third add");
        assert_eq!(
            store.read_with(cx, |s, _| s.active_member(sol_id)),
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
            .update(cx, |s, cx| s.add_empty_member(sol_id, "Frontend", cx))
            .expect("first add");
        let id2 = store
            .update(cx, |s, cx| s.add_empty_member(sol_id, "Frontend", cx))
            .expect("second add — must not collide");
        assert_ne!(
            id1, id2,
            "two empty members from the same name must get distinct ids"
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
            .update(cx, |s, cx| s.add_empty_member(sol_id, "Frontend", cx))
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
            s.add_member_with_progress(sol_id, cat_id, cache_root, cb, cx)
        });
        task.await.expect("add_member success");

        let pending = store.read_with(cx, |s, _| s.pending_adds_for(sol_id));
        assert!(pending.is_empty());
    }
}
