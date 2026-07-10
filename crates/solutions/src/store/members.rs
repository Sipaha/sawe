use super::{SolutionStore, SolutionStoreEvent};
use crate::model::{CatalogId, Solution, SolutionId, SolutionMember};
use anyhow::{Context as _, Result, bail};
use gpui::{App, AppContext as _, Context, Entity};
use project::WorktreeId;
use util::ResultExt as _;

impl SolutionStore {
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

    pub(crate) fn find_solution_mut(&mut self, id: &SolutionId) -> Result<&mut Solution> {
        self.config
            .solutions
            .iter_mut()
            .find(|s| s.id == *id)
            .with_context(|| format!("solution not found: {}", id.0))
    }
}
