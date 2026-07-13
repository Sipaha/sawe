use super::{SolutionStore, SolutionStoreEvent};
use crate::model::{Solution, SolutionId};
use crate::slug::unique_slug;
use anyhow::{Context as _, Result, bail};
use gpui::Context;
use std::path::PathBuf;

impl SolutionStore {
    pub fn create_solution(
        &mut self,
        name: &str,
        root_base: PathBuf,
        cx: &mut gpui::Context<Self>,
    ) -> Result<SolutionId> {
        // The folder name is derived from the display name; the id is a counter
        // that has nothing to do with either, so uniquify against the folder
        // names already in use rather than against ids.
        let taken: Vec<String> = self
            .config
            .solutions
            .iter()
            .filter_map(|s| {
                s.root
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
            })
            .collect();
        let folder = unique_slug(name, &taken);
        let root = root_base.join(&folder);
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;

        let id = match self.db.as_ref() {
            Some(db) => SolutionId(gpui::block_on(db.insert_solution(
                name.to_string(),
                root.to_string_lossy().into_owned(),
                None,
            ))?),
            None => SolutionId(self.next_id_without_db()),
        };
        self.config.solutions.push(Solution {
            id,
            name: name.into(),
            root,
            members: vec![],
            last_opened_at: None,
        });
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(id)
    }

    /// Rename a solution **and move its directory**. One `rename(2)` of the
    /// root carries every member with it, and one compat symlink at the old
    /// root keeps every descendant path string resolving (live `claude`
    /// subprocesses, terminals, git `gitdir` pointers) until the cold
    /// reconcile removes it. Live sessions and terminals are not touched.
    pub fn rename_solution(
        &mut self,
        id: SolutionId,
        new_name: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let folder = crate::folder_name::derive(new_name)?;

        let index = self
            .config
            .solutions
            .iter()
            .position(|solution| solution.id == id)
            .with_context(|| format!("solution not found: {id}"))?;

        let old_root = self.config.solutions[index].root.clone();
        let parent = old_root
            .parent()
            .context("solution root has no parent")?
            .to_path_buf();

        let taken: Vec<crate::rename::TakenFolder> = self
            .config
            .solutions
            .iter()
            .filter(|solution| solution.id != id)
            .filter_map(|solution| {
                Some(crate::rename::TakenFolder {
                    folder: solution.root.file_name()?.to_string_lossy().into_owned(),
                    owner: solution.name.clone(),
                })
            })
            .collect();

        let new_root =
            crate::rename::ensure_folder_available(&parent, &folder, Some(&old_root), &taken)?;

        if old_root == new_root {
            // Display-name-only change (the folder already has this name).
            let solution = &mut self.config.solutions[index];
            solution.name = new_name.to_string();
            let solution = solution.clone();
            self.db_update_solution(&solution)?;
            cx.emit(SolutionStoreEvent::Changed);
            cx.notify();
            return Ok(());
        }

        anyhow::ensure!(
            crate::rename::same_filesystem(&old_root, &parent)?,
            "{} and {} are on different filesystems — a cross-device move would orphan every running process in this solution",
            old_root.display(),
            parent.display(),
        );

        crate::rename::move_dir_with_compat_link(&old_root, &new_root)?;

        // The one `rename(2)` above already moved every member on disk, so the
        // member paths are only *rewritten*, never moved individually — and a
        // single `pending_path_migrations` row (the root) covers all of them,
        // since the cold reconcile rewrites by path prefix.
        let solution = &mut self.config.solutions[index];
        solution.name = new_name.to_string();
        solution.root = new_root.clone();
        for member in solution.members.iter_mut() {
            if let Ok(relative) = member.local_path.strip_prefix(&old_root) {
                member.local_path = new_root.join(relative);
            }
        }
        let solution = solution.clone();

        self.db_update_solution(&solution)?;
        for member in &solution.members {
            self.db_update_member(member)?;
        }
        self.db_insert_pending_path_migration(&old_root, &new_root)?;

        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(())
    }

    pub fn delete_solution(&mut self, id: SolutionId, cx: &mut gpui::Context<Self>) -> Result<()> {
        // Capture the root before removal so the `Deleted` event can carry
        // it — subscribers can no longer look the solution up by id once
        // it's gone from the store.
        let root = self
            .config
            .solutions
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.root.clone());
        let before = self.config.solutions.len();
        self.config.solutions.retain(|s| s.id != id);
        if self.config.solutions.len() == before {
            bail!("solution not found: {id}");
        }
        self.db_delete_solution(id)?;
        // DB row for this solution's active_member is removed by
        // `ON DELETE CASCADE`; mirror that on the in-memory cache so
        // stale entries don't leak past the deletion.
        self.active_member.remove(&id);
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
                    "solution_id": id.0,
                }),
            );
        }
        cx.emit(SolutionStoreEvent::Changed);
        if let Some(root) = root {
            cx.emit(SolutionStoreEvent::Deleted { id, root });
        }
        cx.notify();
        Ok(())
    }

    pub fn touch_last_opened(
        &mut self,
        id: SolutionId,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let sol = self.find_solution_mut(id)?;
        sol.last_opened_at = Some(now_ms);
        self.db_update_last_opened(id, now_ms)?;
        // `Changed` first so listeners that watch the broad signal
        // see the broadcast in chronological order; the more specific
        // `ActiveSolutionChanged` follows so subscribers that only
        // care about the active-id-flipped case can ignore `Changed`.
        cx.emit(SolutionStoreEvent::Changed);
        cx.emit(SolutionStoreEvent::ActiveSolutionChanged(id));
        cx.notify();
        Ok(())
    }

    /// Returns `true` if the solution's desktop window is currently tracked as open.
    pub fn is_open(&self, id: SolutionId) -> bool {
        self.open_solutions.contains(&id)
    }

    /// Mark a solution's window as open. Idempotent — repeat calls are no-ops.
    /// Emits `Changed` so existing MCP observers react automatically.
    /// Also emits a sequenced `workspace.solution_opened` notification so the
    /// mobile client updates regardless of who triggered the open.
    pub fn mark_open(&mut self, id: SolutionId, cx: &mut Context<Self>) {
        if !self.open_solutions.insert(id) {
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
                    "id": sol.id.0,
                    "name": sol.name,
                    "root": sol.root.to_string_lossy(),
                    "member_count": sol.members.len(),
                    "last_opened_at": sol
                        .last_opened_at
                        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                        .map(|t| t.to_rfc3339()),
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
    pub fn mark_closed(&mut self, id: SolutionId, cx: &mut Context<Self>) {
        if !self.open_solutions.remove(&id) {
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
                    "solution_id": id.0,
                }),
            );
        }
        cx.emit(SolutionStoreEvent::Closed { id });
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
    }

    pub fn paths_for_open(&self, id: SolutionId) -> Result<Vec<PathBuf>> {
        let sol = self.find_solution(id)?;
        Ok(sol.members.iter().map(|m| m.local_path.clone()).collect())
    }

    /// Id allocation for `for_test` stores with no DB. Production stores always
    /// go through `INSERT … RETURNING id`. Shared across solutions, members and
    /// catalog rows so a test can't mint two entities with the same id.
    pub(crate) fn next_id_without_db(&self) -> i64 {
        let max_solution = self
            .config
            .solutions
            .iter()
            .map(|s| s.id.0)
            .max()
            .unwrap_or(0);
        let max_member = self
            .config
            .solutions
            .iter()
            .flat_map(|s| s.members.iter().map(|m| m.id.0))
            .max()
            .unwrap_or(0);
        let max_catalog = self
            .config
            .catalog
            .iter()
            .map(|c| c.id.0)
            .max()
            .unwrap_or(0);
        max_solution.max(max_member).max(max_catalog) + 1
    }

    /// Plain UPDATE of the name/root columns. `save_solution` is an
    /// `INSERT OR REPLACE`, which *deletes* the conflicting parent row first
    /// and cascades that delete into `solution_members` / `active_member` —
    /// exactly how a rename wiped every member the last time it shipped
    /// (docs/findings/2026-07-13-rename-solution-cascade-data-loss.md).
    pub(crate) fn db_update_solution(&self, s: &Solution) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.update_solution_row(
            s.id.0,
            s.name.clone(),
            s.root.to_string_lossy().into_owned(),
        ))
    }

    fn db_delete_solution(&self, id: SolutionId) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.delete_solution_row(id.0))
    }

    fn db_update_last_opened(&self, id: SolutionId, ts_ms: i64) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.update_last_opened(id.0, ts_ms))
    }
}

#[cfg(test)]
mod tests {
    #[gpui::test]
    async fn rename_solution_moves_the_root_and_rewrites_member_paths(
        cx: &mut gpui::TestAppContext,
    ) {
        let base = tempfile::tempdir().expect("tempdir");
        let old_root = base.path().join("spk-solutions");
        let member_path = old_root.join("sawe");
        std::fs::create_dir_all(&member_path).expect("mkdir member");
        std::fs::write(member_path.join("marker.txt"), b"m").expect("write marker");

        let store =
            cx.update(|cx| crate::store::for_test_with_solution(cx, &old_root, &member_path));
        let solution_id = store.read_with(cx, |store, _| store.solutions()[0].id);

        store
            .update(cx, |store, cx| {
                store.rename_solution(solution_id, "Sawe", cx)
            })
            .expect("rename");

        let new_root = base.path().join("Sawe");
        assert!(new_root.join("sawe/marker.txt").is_file(), "root moved");
        assert!(
            std::fs::symlink_metadata(&old_root)
                .expect("stat old root")
                .file_type()
                .is_symlink(),
            "compat symlink left at the old root"
        );
        // The single link at the root covers every descendant path.
        assert_eq!(
            std::fs::read(old_root.join("sawe/marker.txt")).expect("read through link"),
            b"m"
        );

        store.read_with(cx, |store, _| {
            let solution = store.find_solution(solution_id).expect("solution");
            assert_eq!(solution.name, "Sawe");
            assert_eq!(solution.root, new_root);
            assert_eq!(solution.members.len(), 1, "members survive the rename");
            assert_eq!(solution.members[0].local_path, new_root.join("sawe"));
        });
    }

    #[gpui::test]
    async fn rename_solution_rejects_a_name_taken_by_another_solution(
        cx: &mut gpui::TestAppContext,
    ) {
        let base = tempfile::tempdir().expect("tempdir");
        let old_root = base.path().join("spk-solutions");
        let member_path = old_root.join("sawe");
        std::fs::create_dir_all(&member_path).expect("mkdir member");

        let store =
            cx.update(|cx| crate::store::for_test_with_solution(cx, &old_root, &member_path));
        store.update(cx, |store, _| {
            store.config.solutions.push(crate::model::Solution {
                id: crate::model::SolutionId(2),
                name: "Citeck Forge".into(),
                root: base.path().join("Citeck-Forge"),
                members: vec![],
                last_opened_at: None,
            });
        });
        let solution_id = store.read_with(cx, |store, _| store.solutions()[0].id);

        let err = store
            .update(cx, |store, cx| {
                store.rename_solution(solution_id, "citeck forge", cx)
            })
            .expect_err("collides");
        assert_eq!(
            err.to_string(),
            "Directory 'citeck-forge' is already taken by solution 'Citeck Forge'"
        );
        // Nothing moved.
        assert!(member_path.is_dir());
        store.read_with(cx, |store, _| {
            let solution = store.find_solution(solution_id).expect("solution");
            assert_eq!(solution.name, "My Solution");
            assert_eq!(solution.root, old_root);
        });
    }
}
