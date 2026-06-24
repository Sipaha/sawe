//! Helpers for locating Solution-bearing windows. Extracted from the
//! retired left-dock panel; reused by the title-bar tab strip and the
//! Welcome trigger.

use gpui::{App, WindowHandle};
use solutions::{SolutionId, SolutionStore};
use workspace::{MultiWorkspace, Workspace};

use crate::open::workspace_has_solution;

/// Returns the window handle that currently has `sol_id` as one of its
/// open solutions, or `None` if no such window exists.
///
/// Uses [`WindowHandle::read`] rather than [`WindowHandle::read_with`]:
/// `read_with` goes through `App::read_window`, which panics with
/// "attempted to read a window that is already on the stack" when the
/// window's slot is taken (i.e. that window is currently in the middle
/// of an `update` — including its own render frame). The render of any
/// `MultiWorkspace` window iterates `cx.windows()`, which includes its
/// own handle, so `read_with` would always panic in that path. `read`
/// returns `Err("window not found")` instead, and we treat that as
/// "skip — can't determine right now". As a consequence the **calling
/// window is excluded from this iteration**: callers that care whether
/// `sol_id` is open in their own window must determine that locally
/// (e.g. by inspecting their own `MultiWorkspace::workspaces()`).
pub fn find_window_for_solution(
    sol_id: &SolutionId,
    cx: &App,
) -> Option<WindowHandle<MultiWorkspace>> {
    cx.windows().into_iter().find_map(|handle| {
        let mw_handle = handle.downcast::<MultiWorkspace>()?;
        let mw = mw_handle.read(cx).ok()?;
        mw.workspaces()
            .any(|ws| workspace_has_solution(ws, sol_id, cx))
            .then_some(mw_handle)
    })
}

pub fn is_solution_open_anywhere(sol_id: &SolutionId, cx: &App) -> bool {
    find_window_for_solution(sol_id, cx).is_some()
}

/// First solution in the registry whose `root` is an ancestor of any
/// worktree in `workspace`'s project (visible OR hidden). Mirrors the
/// behaviour the title-bar tab strip uses to highlight the active
/// tab; tabs and panel selectors must agree on which solution is
/// active. Hidden worktrees are included because empty solutions are
/// opened with `OpenVisible::None` — without considering hidden
/// worktrees, an empty solution's panel selector would show
/// "No solution" even though the solution clearly is the active one.
pub fn active_solution_in_workspace(workspace: &Workspace, cx: &App) -> Option<SolutionId> {
    let store = SolutionStore::try_global(cx)?;
    let store = store.read(cx);
    let project = workspace.project().read(cx);
    let paths = project
        .worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path());
    active_solution_for_paths(store, paths)
}

/// Inner pure-data helper: walks `paths` and returns the id of the first
/// solution whose root is an ancestor of any path. Kept private so tests
/// can drive it directly without needing a full workspace harness.
fn active_solution_for_paths<P>(
    store: &SolutionStore,
    paths: impl IntoIterator<Item = P>,
) -> Option<SolutionId>
where
    P: AsRef<std::path::Path>,
{
    for path in paths {
        if let Some(sol) = store.solution_for_path(path.as_ref()) {
            return Some(sol.id.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use solutions::SolutionStore;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[gpui::test]
    async fn active_solution_for_paths_matches_first_worktree(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");

        let store = cx.update(|cx| SolutionStore::for_test(PathBuf::new(), cx));
        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("S", dir.path().to_path_buf(), cx)
            })
            .expect("create");

        // `create_solution` appends the slug ("s") to the base, so the actual
        // root is `dir/s`. A path under that root must match.
        let worktree_path = dir.path().join("s").join("some-project");
        let result = store.read_with(cx, |store, _cx| {
            active_solution_for_paths(store, [worktree_path])
        });
        assert_eq!(result, Some(sol_id));
    }

    #[gpui::test]
    async fn active_solution_for_paths_returns_none_when_no_match(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");

        let store = cx.update(|cx| SolutionStore::for_test(PathBuf::new(), cx));
        store
            .update(cx, |s, cx| {
                s.create_solution("S", dir.path().to_path_buf(), cx)
            })
            .expect("create");

        // A path outside the solution root should not match.
        let other_dir = tempdir().expect("tempdir");
        let other_path = other_dir.path().join("file.txt");
        let result = store.read_with(cx, |store, _cx| {
            active_solution_for_paths(store, [other_path])
        });
        assert_eq!(result, None);
    }

    #[gpui::test]
    async fn active_solution_for_paths_returns_none_on_empty_paths(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");

        let store = cx.update(|cx| SolutionStore::for_test(PathBuf::new(), cx));
        store
            .update(cx, |s, cx| {
                s.create_solution("S", dir.path().to_path_buf(), cx)
            })
            .expect("create");

        let result = store.read_with(cx, |store, _cx| {
            active_solution_for_paths(store, Vec::<PathBuf>::new())
        });
        assert_eq!(result, None);
    }
}
