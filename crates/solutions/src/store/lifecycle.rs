use super::{SolutionStore, SolutionStoreEvent};
use crate::model::{Solution, SolutionId};
use crate::slug::unique_slug;
use anyhow::{Context as _, Result, bail};
use chrono::Utc;
use gpui::Context;
use std::path::PathBuf;

impl SolutionStore {
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

    fn db_update_last_opened(&self, id: &SolutionId, ts_ms: i64) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.update_last_opened(id.0.clone(), ts_ms))
    }
}
