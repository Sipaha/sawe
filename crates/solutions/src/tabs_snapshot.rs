//! Per-Solution snapshot of open editor tabs, recorded when leaving a
//! Solution and replayed when returning. The in-place Solution-switch
//! orchestrator (`solutions_ui::switch`) populates `SolutionStore`'s
//! map via `store_tab_snapshot` before swapping worktrees, then reads
//! the target Solution's saved snapshot via `tab_snapshot` after the
//! swap and re-opens the listed paths in order.
//!
//! Only the part reconstructable from absolute paths is captured — we
//! deliberately *don't* mirror upstream's full `PreviousWorkspaceState`
//! (`workspace::PreviousWorkspaceState`), because the in-place switch
//! keeps the same `Workspace`/`Project`/dock entities alive: dock
//! widths, open/closed flags, scroll positions, and panel-specific
//! state survive automatically. Re-applying a captured `DockStructure`
//! on top would be a no-op at best and a state-clobber at worst.

use std::path::PathBuf;

use crate::SolutionId;

/// Open-tab snapshot for a single Solution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SolutionTabsSnapshot {
    /// Absolute paths of items the user had open, in pane left-to-right
    /// order. Empty Vec means "no editor was open".
    pub open_paths: Vec<PathBuf>,
    /// Absolute path of the active item, if any. Must be a member of
    /// `open_paths` for `restore` to activate it; if it's not, the
    /// orchestrator falls back to leaving the most-recently opened
    /// item active.
    pub active_path: Option<PathBuf>,
}

impl SolutionTabsSnapshot {
    /// Empty snapshots are never persisted in the store map (see
    /// `SolutionStore::store_tab_snapshot`'s eviction rule). Exposed
    /// here so the orchestrator can decide whether to skip the save
    /// step before paying for a `cx.emit` round-trip.
    pub fn is_empty(&self) -> bool {
        self.open_paths.is_empty() && self.active_path.is_none()
    }
}

/// Identifier-keyed shorthand used by `SolutionStore` so callers don't
/// have to spell out the full `HashMap` type.
pub type TabSnapshots = std::collections::HashMap<SolutionId, SolutionTabsSnapshot>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_empty_default_snapshot() {
        assert!(SolutionTabsSnapshot::default().is_empty());
    }

    #[test]
    fn is_empty_false_with_paths() {
        let snapshot = SolutionTabsSnapshot {
            open_paths: vec![PathBuf::from("/a")],
            active_path: None,
        };
        assert!(!snapshot.is_empty());
    }

    #[test]
    fn is_empty_false_with_only_active() {
        let snapshot = SolutionTabsSnapshot {
            open_paths: Vec::new(),
            active_path: Some(PathBuf::from("/a")),
        };
        assert!(!snapshot.is_empty());
    }
}
