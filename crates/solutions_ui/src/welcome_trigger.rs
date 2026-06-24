//! When the last open solution in a window is closed, open the
//! launcher (`WelcomeWindow`) — the same view shown at startup with no
//! solutions open. Wired into the `close_solution` flow in
//! [`crate::solutions_ui`].
//!
//! Note on the "empty list" condition. `MultiWorkspace::close_workspace`
//! always provides a fallback workspace when the active one is being
//! closed, so the literal `workspaces()` list rarely goes to zero —
//! instead we usually end up with one workspace whose project has no
//! solution-bearing worktree. So "empty" here means "no remaining
//! workspace hosts any solution", which matches the user-visible
//! intent of "the last solution in this window just closed".

use gpui::{App, Context, Window};
use solutions::SolutionStore;
use workspace::{MultiWorkspace, welcome::ShowWelcome};

/// Call after a successful `close_solution` once the in-flight close
/// tasks have completed. If the window's `MultiWorkspace` no longer
/// hosts any solution, opens the launcher (`WelcomeWindow`) via the
/// `ShowWelcome` action — the same primitive used by the menu and by
/// the onboarding entry point — and retires the now-empty `MultiWorkspace`
/// window. The fallback workspace MW spawns to keep the window alive
/// has no solution and no path list, so leaving it on screen alongside
/// the launcher just shows the user two windows when one would do.
pub fn open_welcome_if_window_empty(
    multi_workspace: &MultiWorkspace,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) {
    if has_any_solution(multi_workspace, cx) {
        return;
    }
    cx.dispatch_action(&ShowWelcome);
    window.remove_window();
}

fn has_any_solution(multi_workspace: &MultiWorkspace, cx: &App) -> bool {
    let Some(store) = SolutionStore::try_global(cx) else {
        return false;
    };
    let store_read = store.read(cx);
    multi_workspace.workspaces().any(|workspace| {
        let project = workspace.read(cx).project().clone();
        project.read(cx).worktrees(cx).any(|tree| {
            store_read
                .solution_for_path(&tree.read(cx).abs_path())
                .is_some()
        })
    })
}
