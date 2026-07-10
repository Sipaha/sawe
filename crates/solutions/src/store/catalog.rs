use super::{SolutionStore, SolutionStoreEvent};
use crate::git;
use crate::model::{CatalogId, CatalogProject, SolutionId};
use crate::slug::unique_slug;
use anyhow::{Context as _, Result, bail};
use std::path::PathBuf;
use util::ResultExt as _;

impl SolutionStore {
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
}
