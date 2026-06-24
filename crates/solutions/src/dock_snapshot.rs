//! Per-Solution snapshot of dock (panel) layout, recorded when leaving
//! a Solution and replayed when returning. The in-place Solution-switch
//! orchestrator (`solutions_ui::switch`) populates `SolutionStore`'s map
//! via `set_dock_snapshot` before swapping worktrees, then reads the
//! target Solution's saved snapshot via `dock_snapshot` after the swap
//! and re-applies open/closed state, active panel index, and active
//! panel size to each side's `Dock`.
//!
//! This mirrors `tabs_snapshot` (the per-Solution open-tab list) and
//! exists for the same reason: the in-place switch keeps ONE `Workspace`
//! (and its three `Dock` entities) alive across switches, so without an
//! explicit per-Solution capture/replay the docks would be SHARED across
//! every Solution shown in that window. Capturing on switch-out and
//! replaying on switch-in gives each Solution its own dock layout.
//!
//! Why store the full `PanelSizeState` rather than a bare `Pixels`: a
//! horizontal dock's active panel can be sized by `flex` instead of an
//! absolute `size` (see `Dock::resize_panel_entry`), so collapsing to a
//! single pixel value would drop the flex layout for those panels.

use workspace::dock::PanelSizeState;

use crate::SolutionId;

/// Dock layout snapshot for a single Solution: one entry per side.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SolutionDockSnapshot {
    pub left: DockSideSnapshot,
    pub right: DockSideSnapshot,
    pub bottom: DockSideSnapshot,
}

/// Captured state for a single dock side.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DockSideSnapshot {
    /// Whether the dock was open.
    pub is_open: bool,
    /// Index of the active panel within the dock, if any. Panel order is
    /// deterministic across the (single, shared) workspace, so the index
    /// maps back to the same panel type on restore.
    pub active_panel_index: Option<usize>,
    /// Size of the active panel at capture time. `None` means "no active
    /// panel / dock closed" — leave the side's default size on restore.
    pub size: Option<PanelSizeState>,
}

/// Identifier-keyed shorthand used by `SolutionStore` so callers don't
/// have to spell out the full `HashMap` type.
pub type DockSnapshots = std::collections::HashMap<SolutionId, SolutionDockSnapshot>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_side_is_closed_with_no_active_panel() {
        let side = DockSideSnapshot::default();
        assert!(!side.is_open);
        assert_eq!(side.active_panel_index, None);
        assert_eq!(side.size, None);
    }
}
