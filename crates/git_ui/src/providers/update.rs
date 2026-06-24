//! Provider trait for solution-wide "Update Project" (fetch + pull all members).
//!
//! Implemented in `solution_git::update::SolutionUpdateOrchestrator`.
//! `git_ui` calls into the trait through the registry in
//! [`crate::providers`]; the trait stays narrow so `git_ui` doesn't pull
//! `solutions` / `solution_git` types into its dep graph (P-9).

use gpui::{App, WeakEntity};
use workspace::Workspace;

pub trait SolutionUpdateProvider: Send + Sync {
    fn is_active(&self) -> bool;

    /// Fetch + pull every git member of the active Solution. Resolves the
    /// active Solution itself; the workspace handle is the launching surface
    /// (for toasts). Runs async; surfaces a summary toast.
    fn update_solution(&self, workspace: WeakEntity<Workspace>, cx: &mut App);
}
