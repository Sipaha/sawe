use super::{SolutionStore, SolutionStoreEvent};
use crate::git;
use crate::model::{CatalogId, CatalogProject, SolutionId, SolutionMember};
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
        // Both the name and the remote are unique keys, and a clash on either is
        // an ERROR, never a silent fixup. Registering a repo that was already in
        // the catalog used to mint a second row with a `-2` slug: the picker then
        // showed the identical project (same label, same URL) two or three times,
        // and `refresh_cache` fetched the same mirror once per row. Reusing the
        // existing entry instead would be just as bad in the other direction —
        // the caller asks to add a project and gets back someone else's id, with
        // its own name and default branch, which is exactly the hidden magic that
        // makes a registry untrustworthy. Say no and let the user decide.
        if let Some(clash) = self
            .config
            .catalog
            .iter()
            .find(|c| same_remote(&c.remote_url, remote_url))
        {
            bail!(
                "duplicate_remote: catalog already has \"{}\" pointing at {} — remove or edit \
                 that entry instead of registering the same repository twice",
                clash.name,
                clash.remote_url,
            );
        }
        // The picker shows the NAME as the row's primary label, so two rows
        // reading `citeck-ci` are indistinguishable at a glance no matter how
        // their URLs differ.
        if let Some(clash) = self.config.catalog.iter().find(|c| same_name(&c.name, name)) {
            bail!(
                "duplicate_name: catalog already has a project named \"{}\" ({}) — pick a \
                 different name",
                clash.name,
                clash.remote_url,
            );
        }
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
        // Same uniqueness contract as `add_catalog_project` — otherwise Edit is
        // a back door that recreates exactly the duplicates the add path now
        // refuses. Checked against every OTHER entry (a no-op self-rename must
        // still be allowed) and BEFORE any field is written, so a rejected edit
        // leaves the entry untouched.
        if let Some(name) = new_name.as_deref()
            && let Some(clash) = self
                .config
                .catalog
                .iter()
                .find(|c| c.id != *id && same_name(&c.name, name))
        {
            bail!(
                "duplicate_name: catalog already has a project named \"{}\" ({})",
                clash.name,
                clash.remote_url,
            );
        }
        if let Some(url) = new_remote_url.as_deref()
            && let Some(clash) = self
                .config
                .catalog
                .iter()
                .find(|c| c.id != *id && same_remote(&c.remote_url, url))
        {
            bail!(
                "duplicate_remote: catalog already has \"{}\" pointing at {}",
                clash.name,
                clash.remote_url,
            );
        }
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

    /// Fold the duplicate catalog entry `from` into the canonical `into`,
    /// repointing every solution member that referenced `from`, then deleting
    /// the `from` row. Returns how many members were repointed.
    ///
    /// This is the ONLY way to clean up duplicates that predate the uniqueness
    /// checks in `add_catalog_project`: both halves of such a pair are typically
    /// referenced by different solutions (the historical clones attached to
    /// whichever row existed at the time), so `remove_catalog_project` rightly
    /// refuses them and a raw delete would break those solutions.
    ///
    /// A member is `(catalog_id, local_path)`. Only `catalog_id` is rewritten —
    /// `local_path` and the member's position are preserved, so the checked-out
    /// working tree on disk is not touched, moved, or re-cloned.
    ///
    /// Refuses to merge entries that don't point at the same repository (this is
    /// deduplication, not a repoint tool), and refuses when a single solution
    /// holds BOTH entries — collapsing those two members into one would silently
    /// drop one of the two working trees, which is the user's call, not ours.
    pub fn merge_catalog_project(
        &mut self,
        from: &CatalogId,
        into: &CatalogId,
        cx: &mut gpui::Context<Self>,
    ) -> Result<usize> {
        if from == into {
            bail!("merge_self: {} is both source and target", from.0);
        }
        let from_url = self
            .config
            .catalog
            .iter()
            .find(|c| c.id == *from)
            .with_context(|| format!("catalog_not_found: {}", from.0))?
            .remote_url
            .clone();
        let into_url = self
            .config
            .catalog
            .iter()
            .find(|c| c.id == *into)
            .with_context(|| format!("catalog_not_found: {}", into.0))?
            .remote_url
            .clone();
        if !same_remote(&from_url, &into_url) {
            bail!(
                "not_duplicates: {} points at {from_url}, {} at {into_url} — merge only folds two \
                 entries for the SAME repository",
                from.0,
                into.0,
            );
        }
        if let Some(both) = self.config.solutions.iter().find(|s| {
            s.members.iter().any(|m| m.catalog_id == *from)
                && s.members.iter().any(|m| m.catalog_id == *into)
        }) {
            bail!(
                "solution_holds_both: solution \"{}\" has members for BOTH {} and {} — remove one \
                 of them yourself first (merging would drop a working tree)",
                both.name,
                from.0,
                into.0,
            );
        }

        // Collect the writes first; `db_set_member` / `db_delete_member` take
        // `&self`, so they can't run while `config.solutions` is borrowed mutably.
        let mut repointed: Vec<(SolutionId, SolutionMember, i32)> = Vec::new();
        for solution in self.config.solutions.iter_mut() {
            for (position, member) in solution.members.iter_mut().enumerate() {
                if member.catalog_id != *from {
                    continue;
                }
                member.catalog_id = into.clone();
                repointed.push((solution.id.clone(), member.clone(), position as i32));
            }
        }
        for (solution_id, member, position) in &repointed {
            // Order matters: the row is keyed by (solution_id, catalog_id), so
            // drop the old key before writing the new one — otherwise the stale
            // row survives and the member appears twice on next load.
            self.db_delete_member(solution_id, from)?;
            self.db_set_member(solution_id, member, *position)?;
        }
        // An active-member pointer at the dead id would leave the solution with
        // no resolvable active project.
        let stale_active: Vec<SolutionId> = self
            .active_member
            .iter()
            .filter(|(_, catalog)| *catalog == from)
            .map(|(solution, _)| solution.clone())
            .collect();
        for solution in stale_active {
            self.set_active_member(solution, into.clone(), cx);
        }

        self.config.catalog.retain(|c| c.id != *from);
        self.db_delete_catalog(from)?;
        log::info!(
            "solutions: merged catalog {} into {} ({} member(s) repointed)",
            from.0,
            into.0,
            repointed.len(),
        );
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(repointed.len())
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

/// Do two remote URLs name the same repository? Compared on a normalized form
/// (case-folded, trailing `/` and `.git` stripped) so `…/foo.git` and `…/foo`
/// — the two shapes a user copies out of GitLab — don't register as separate
/// catalog entries. Deliberately does NOT try to unify `git@host:path` with
/// `https://host/path`: those are different credentials/transport and the user
/// may genuinely want both.
fn same_remote(a: &str, b: &str) -> bool {
    fn normalize(url: &str) -> String {
        url.trim()
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .to_lowercase()
    }
    !a.trim().is_empty() && normalize(a) == normalize(b)
}

/// Do two catalog names collide? Case- and surrounding-whitespace-insensitive:
/// `citeck-ci` and `Citeck-CI ` read as the same row in the picker, so they must
/// not both be registrable.
fn same_name(a: &str, b: &str) -> bool {
    !a.trim().is_empty() && a.trim().to_lowercase() == b.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::{same_name, same_remote};

    #[test]
    fn same_remote_ignores_git_suffix_slash_and_case() {
        assert!(same_remote(
            "git@gitlab.citeck.ru:infrastructure/ci-cd/citeck-ci.git",
            "git@gitlab.citeck.ru:infrastructure/ci-cd/citeck-ci",
        ));
        assert!(same_remote("https://x/Foo.git", "https://x/foo/"));
    }

    #[test]
    fn same_remote_keeps_distinct_repos_and_transports_apart() {
        assert!(!same_remote("git@x:foo.git", "git@x:other-foo.git"));
        assert!(!same_remote("git@x:foo.git", "https://x/foo.git"));
        // An empty remote (a local-only catalog row) must never collapse onto
        // another empty one — every such project is its own thing.
        assert!(!same_remote("", ""));
    }

    #[test]
    fn same_name_folds_case_and_padding_but_not_empties() {
        assert!(same_name("citeck-ci", " Citeck-CI "));
        assert!(!same_name("citeck-ci", "citeck-ci-2"));
        assert!(!same_name("", ""));
    }
}
