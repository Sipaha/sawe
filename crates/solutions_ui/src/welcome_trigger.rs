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
use util::ResultExt as _;
use workspace::{AppState, MultiWorkspace, welcome::WelcomeWindow};

/// Call after a successful `close_solution` once the in-flight close
/// tasks have completed. If the window's `MultiWorkspace` no longer
/// hosts any solution, opens the launcher (`WelcomeWindow`) and retires
/// the now-empty `MultiWorkspace` window. The fallback workspace MW spawns
/// to keep the window alive has no solution and no path list, so leaving it
/// on screen alongside the launcher just shows the user two windows when
/// one would do.
///
/// The launcher is opened **synchronously and directly** (not via the
/// deferred `ShowWelcome` action) BEFORE removing this window. Two reasons:
/// (1) `cx.dispatch_action` would queue the action on *this* window, which
/// is about to be removed — the queued action could be dropped with it; and
/// (2) the quit-on-last-window guard (`zed::no_main_windows_left`) fires from
/// `on_window_closed` the instant the MW is removed, so the `WelcomeWindow`
/// must already exist by then or the app quits instead of showing it.
pub fn open_welcome_if_window_empty(
    multi_workspace: &MultiWorkspace,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) {
    if has_any_solution(multi_workspace, cx) {
        return;
    }
    WelcomeWindow::open(AppState::global(cx), cx).log_err();
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
