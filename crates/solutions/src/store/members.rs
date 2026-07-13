use super::{SolutionStore, SolutionStoreEvent};
use crate::model::{MemberId, Solution, SolutionId, SolutionMember};
use anyhow::{Context as _, Result, bail};
use gpui::{App, AppContext as _, Context, Entity};
use project::WorktreeId;
use util::ResultExt as _;

impl SolutionStore {
    pub fn find_solution(&self, id: SolutionId) -> Result<&Solution> {
        self.config
            .solutions
            .iter()
            .find(|s| s.id == id)
            .with_context(|| format!("solution not found: {id}"))
    }

    pub fn find_member(&self, id: MemberId) -> Result<&SolutionMember> {
        self.config
            .solutions
            .iter()
            .flat_map(|s| s.members.iter())
            .find(|m| m.id == id)
            .with_context(|| format!("member not found: {id}"))
    }

    /// The solution a member belongs to.
    pub fn member_of(&self, id: MemberId) -> Option<SolutionId> {
        self.config
            .solutions
            .iter()
            .find(|s| s.members.iter().any(|m| m.id == id))
            .map(|s| s.id)
    }

    /// Read the solution-wide active member, or `None` if nothing has been
    /// recorded yet. Backed by an in-memory cache hydrated from the
    /// `active_member` DB table at init.
    pub fn active_member(&self, solution: SolutionId) -> Option<MemberId> {
        self.active_member.get(&solution).copied()
    }

    /// Path of the solution's active member, falling back to the solution root
    /// when no member is selected. The one place that answers "where do new
    /// terminals / chats start".
    pub fn active_member_path(&self, solution: SolutionId) -> Option<std::path::PathBuf> {
        let sol = self.find_solution(solution).ok()?;
        if let Some(member) = self.active_member(solution).and_then(|id| sol.member(id)) {
            return Some(member.local_path.clone());
        }
        Some(sol.root.clone())
    }

    /// Persist the solution-wide active member through the DB and update the
    /// in-memory cache. Emits `ActiveMemberChanged` so other windows observing
    /// the store can mirror the change. No-op if the given member is already
    /// the active one for this solution.
    pub fn set_active_member(
        &mut self,
        solution: SolutionId,
        member: MemberId,
        cx: &mut Context<Self>,
    ) {
        if self.active_member.get(&solution) == Some(&member) {
            return;
        }
        self.active_member.insert(solution, member);
        if let Some(db) = self.db.clone() {
            let (sid, mid) = (solution.0, member.0);
            cx.background_spawn(async move {
                db.set_active_member(sid, mid).await.log_err();
            })
            .detach();
        }
        cx.emit(SolutionStoreEvent::ActiveMemberChanged {
            solution,
            member: Some(member),
        });
        cx.notify();
    }

    /// Return the active member for the solution, seeding it to the first
    /// member in `members` if no selection has been recorded yet (or the
    /// recorded one is no longer a member). Returns `None` if `members` is
    /// empty.
    pub fn ensure_active_member(
        &mut self,
        solution: SolutionId,
        members: &[SolutionMember],
        cx: &mut Context<Self>,
    ) -> Option<MemberId> {
        if let Some(existing) = self.active_member.get(&solution).copied()
            && members.iter().any(|m| m.id == existing)
        {
            return Some(existing);
        }
        let first = members.first()?.id;
        self.set_active_member(solution, first, cx);
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
        solution: SolutionId,
        cx: &mut Context<Self>,
    ) {
        if self.active_member.contains_key(&solution) {
            return;
        }
        let first = self
            .find_solution(solution)
            .ok()
            .and_then(|s| s.members.first())
            .map(|m| m.id);
        if let Some(first) = first {
            self.set_active_member(solution, first, cx);
        }
    }

    /// Resolve the active member for `solution` to the matching worktree in
    /// `project`, using prefix matching on `member.local_path`. Returns `None`
    /// if no active member is set, the member is not found in the solution, or
    /// no worktree's `abs_path` starts with the member's `local_path`.
    pub fn active_member_worktree(
        &self,
        solution: &Solution,
        project: &Entity<project::Project>,
        cx: &App,
    ) -> Option<(MemberId, WorktreeId)> {
        let member_id = self.active_member(solution.id)?;
        let member = solution.member(member_id)?;
        let worktree = project
            .read(cx)
            .worktrees(cx)
            .find(|w| w.read(cx).abs_path().starts_with(&member.local_path))?;
        Some((member_id, worktree.read(cx).id()))
    }

    pub fn remove_member(
        &mut self,
        member_id: MemberId,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let Some(solution_id) = self.member_of(member_id) else {
            bail!("member not in any solution: {member_id}");
        };
        let sol = self.find_solution_mut(solution_id)?;
        sol.members.retain(|m| m.id != member_id);
        self.db_delete_member(member_id)?;
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
        if self.active_member.get(&solution_id) == Some(&member_id) {
            let next = self
                .find_solution(solution_id)
                .ok()
                .and_then(|s| s.members.first())
                .map(|m| m.id);
            match next {
                Some(next) => self.set_active_member(solution_id, next, cx),
                None => {
                    self.active_member.remove(&solution_id);
                    if let Some(db) = self.db.clone() {
                        let sid = solution_id.0;
                        cx.background_spawn(async move {
                            db.clear_active_member(sid).await.log_err();
                        })
                        .detach();
                    }
                    cx.emit(SolutionStoreEvent::ActiveMemberChanged {
                        solution: solution_id,
                        member: None,
                    });
                }
            }
        }
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(())
    }

    /// Rename a member project **and its directory**. Live sessions and
    /// terminals are deliberately left alone: their cwd is an inode, so a
    /// same-filesystem `rename(2)` does not disturb them, and the compat
    /// symlink keeps the path *strings* they still hold valid until the cold
    /// reconcile runs.
    pub fn rename_member(
        &mut self,
        id: MemberId,
        new_name: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let folder = crate::folder_name::derive(new_name)?;

        let (solution_index, member_index) = self
            .config
            .solutions
            .iter()
            .enumerate()
            .find_map(|(solution_index, solution)| {
                solution
                    .members
                    .iter()
                    .position(|member| member.id == id)
                    .map(|member_index| (solution_index, member_index))
            })
            .with_context(|| format!("member not found: {id}"))?;

        let solution = &self.config.solutions[solution_index];
        let old_path = solution.members[member_index].local_path.clone();
        let parent = old_path
            .parent()
            .context("member path has no parent")?
            .to_path_buf();

        let taken: Vec<crate::rename::TakenFolder> = solution
            .members
            .iter()
            .filter(|member| member.id != id)
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

        let new_path =
            crate::rename::ensure_folder_available(&parent, &folder, Some(&old_path), &taken)?;

        if old_path == new_path {
            // Display-name-only change (the folder already has this name).
            let member = &mut self.config.solutions[solution_index].members[member_index];
            member.name = new_name.to_string();
            let member = member.clone();
            self.db_update_member(&member)?;
            cx.emit(SolutionStoreEvent::Changed);
            cx.notify();
            return Ok(());
        }

        anyhow::ensure!(
            crate::rename::same_filesystem(&old_path, &parent)?,
            "{} and {} are on different filesystems — a cross-device move would orphan every running process in this project",
            old_path.display(),
            parent.display(),
        );

        crate::rename::move_dir_with_compat_link(&old_path, &new_path)?;

        let member = &mut self.config.solutions[solution_index].members[member_index];
        member.name = new_name.to_string();
        member.local_path = new_path.clone();
        let member = member.clone();
        self.db_update_member(&member)?;
        self.db_insert_pending_path_migration(&old_path, &new_path)?;

        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(())
    }

    /// Plain UPDATE — `INSERT OR REPLACE` on a row whose children cascade is
    /// how a rename wiped a solution's members once already
    /// (docs/findings/2026-07-13-rename-solution-cascade-data-loss.md).
    pub(crate) fn db_update_member(&self, member: &SolutionMember) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.update_member_row(
            member.id.0,
            member.name.clone(),
            member.local_path.to_string_lossy().into_owned(),
        ))
    }

    /// Record the move for the cold reconcile, which rewrites every other
    /// path-bearing row (workspaces, terminals, transcripts, …) on the next
    /// start, when nothing is live.
    pub(crate) fn db_insert_pending_path_migration(
        &self,
        old_path: &std::path::Path,
        new_path: &std::path::Path,
    ) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.insert_pending_path_migration(
            old_path.to_string_lossy().into_owned(),
            new_path.to_string_lossy().into_owned(),
            chrono::Utc::now().timestamp_millis(),
        ))
    }

    pub fn reorder_members(
        &mut self,
        solution_id: SolutionId,
        new_order: Vec<MemberId>,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let sol = self.find_solution_mut(solution_id)?;
        let mut by_id: collections::HashMap<MemberId, SolutionMember> =
            sol.members.drain(..).map(|m| (m.id, m)).collect();
        for id in &new_order {
            if let Some(m) = by_id.remove(id) {
                sol.members.push(m);
            }
        }
        // Members the caller didn't name keep a deterministic tail order.
        let mut leftovers: Vec<SolutionMember> = by_id.into_values().collect();
        leftovers.sort_by_key(|m| m.id);
        sol.members.append(&mut leftovers);
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
        sol_id: SolutionId,
        m: &SolutionMember,
        position: i32,
    ) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.set_solution_member(
            m.id.0,
            sol_id.0,
            m.name.clone(),
            m.local_path.to_string_lossy().into_owned(),
            position,
            m.origin_catalog_id.map(|c| c.0),
        ))
    }

    pub(crate) fn db_delete_member(&self, member_id: MemberId) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.delete_solution_member(member_id.0))
    }

    pub(crate) fn find_solution_mut(&mut self, id: SolutionId) -> Result<&mut Solution> {
        self.config
            .solutions
            .iter_mut()
            .find(|s| s.id == id)
            .with_context(|| format!("solution not found: {id}"))
    }
}

#[cfg(test)]
mod tests {
    #[gpui::test]
    async fn rename_member_moves_the_folder_and_leaves_a_link(cx: &mut gpui::TestAppContext) {
        let root = tempfile::tempdir().expect("tempdir");
        let solution_root = root.path().join("my-solution");
        let member_path = solution_root.join("old-project");
        std::fs::create_dir_all(&member_path).expect("mkdir member");
        std::fs::write(member_path.join("marker.txt"), b"m").expect("write marker");

        let store =
            cx.update(|cx| crate::store::for_test_with_solution(cx, &solution_root, &member_path));

        let member_id = store.read_with(cx, |store, _| store.solutions()[0].members[0].id);

        store
            .update(cx, |store, cx| {
                store.rename_member(member_id, "New Project", cx)
            })
            .expect("rename");

        let new_path = solution_root.join("New-Project");
        assert!(new_path.join("marker.txt").is_file(), "folder moved");
        assert!(
            std::fs::symlink_metadata(&member_path)
                .expect("stat old path")
                .file_type()
                .is_symlink(),
            "compat symlink left behind"
        );

        store.read_with(cx, |store, _| {
            let member = store.find_member(member_id).expect("member");
            assert_eq!(member.name, "New Project");
            assert_eq!(member.local_path, new_path);
        });
    }

    #[gpui::test]
    async fn rename_member_rejects_a_sibling_collision(cx: &mut gpui::TestAppContext) {
        let root = tempfile::tempdir().expect("tempdir");
        let solution_root = root.path().join("my-solution");
        let member_path = solution_root.join("old-project");
        std::fs::create_dir_all(&member_path).expect("mkdir member");
        std::fs::create_dir_all(solution_root.join("Taken")).expect("mkdir sibling");

        let store =
            cx.update(|cx| crate::store::for_test_with_solution(cx, &solution_root, &member_path));
        let member_id = store.read_with(cx, |store, _| store.solutions()[0].members[0].id);

        // `ensure_folder_available` matches the *DB* list case-insensitively but
        // probes the disk at the exact derived path, so the on-disk sibling is
        // named exactly as the derivation of "Taken".
        let err = store
            .update(cx, |store, cx| store.rename_member(member_id, "Taken", cx))
            .expect_err("collides");
        assert!(
            err.to_string()
                .contains("already exists on disk (not owned by any solution)"),
            "{err}"
        );
        // Nothing moved: the member is still a real directory at its old path
        // (not a compat symlink), and the sibling was not clobbered.
        assert!(member_path.is_dir());
        assert!(
            !std::fs::symlink_metadata(&member_path)
                .expect("stat member")
                .file_type()
                .is_symlink()
        );
        assert!(
            std::fs::read_dir(solution_root.join("Taken"))
                .expect("read sibling")
                .next()
                .is_none()
        );

        store.read_with(cx, |store, _| {
            let member = store.find_member(member_id).expect("member");
            assert_eq!(member.name, "Old Project");
            assert_eq!(member.local_path, member_path);
        });
    }
}
