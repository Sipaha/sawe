//! Provider trait for solution-wide push dialog.
//!
//! Implemented in `solution_git::push::SolutionPushOrchestrator` (S-SOL-PSH).
//! `git_ui` calls into the trait through the registry in
//! [`crate::providers`]; the trait stays narrow so `git_ui` doesn't pull
//! `solutions` / `solution_git` types into its dep graph.

use gpui::{App, WeakEntity};
use workspace::Workspace;

pub trait SolutionPushProvider: Send + Sync {
    /// True if Solution-wide push UI should replace the per-repo push for
    /// the currently-open Solution. Used by `git_panel` (and the
    /// command-palette wiring) to decide whether to surface the
    /// `solution: git push all` action.
    fn is_active(&self) -> bool;

    /// Open the per-Solution push dialog as a workspace pane item (or
    /// modal — implementation choice). The orchestrator is responsible
    /// for resolving the active Solution itself; the workspace handle is
    /// only a launching surface.
    fn open_solution_push_dialog(&self, workspace: WeakEntity<Workspace>, cx: &mut App);
}
