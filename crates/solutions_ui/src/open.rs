//! Shared "open this Solution" entry point used by the welcome page,
//! the title-bar tab strip and the picker. Centralises three concerns
//! that were previously copy-pasted:
//!
//! 1. Reuse an already-open window for the Solution if one exists.
//! 2. Decide between replacing the current window's workspace
//!    (`OpenIntent::SameWindow`, default for left-click) and opening a
//!    fresh window (`OpenIntent::NewWindow`, middle-click).
//! 3. Append the `EmptySolutionPage` CTA when the Solution has no members.

use std::path::PathBuf;

use anyhow::anyhow;
use gpui::{App, AppContext, WindowHandle};
use solutions::{SolutionId, SolutionStore};
use util::ResultExt as _;
use workspace::{AppState, MultiWorkspace, OpenMode, OpenOptions, OpenVisible};

/// Skips the currently-active window when iterating other windows.
/// `cx.read_window` on the window whose event we're handling panics with
/// "attempted to read a window that is already on the stack" because GPUI
/// has temporarily moved it out of the registry.
fn skip_window_id(cx: &App) -> Option<gpui::WindowId> {
    cx.active_window().map(|w| w.window_id())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OpenIntent {
    /// Replace the current window's workspace with the Solution. If the
    /// Solution is already open in any window, that window is focused
    /// instead.
    SameWindow,
    /// Always open the Solution in a new window.
    NewWindow,
}

pub fn open_solution(
    sol_id: SolutionId,
    source_window: Option<WindowHandle<MultiWorkspace>>,
    intent: OpenIntent,
    cx: &mut App,
) {
    // Focus an already-open window for this Solution, except when the
    // user explicitly asked for a new window via middle-click.
    let source_window_id = source_window.as_ref().map(|w| w.window_id());
    if intent == OpenIntent::SameWindow
        && let Some(existing) = find_window_for_solution(&sol_id, source_window_id, cx)
    {
        existing
            .update(cx, |_, window, _| window.activate_window())
            .log_err();
        if let Some(store) = SolutionStore::try_global(cx) {
            store
                .update(cx, |s, cx| s.touch_last_opened(&sol_id, cx))
                .log_err();
        }
        // If the click came from a different window (e.g. the welcome
        // launcher), retire it now that the user has chosen a target.
        if let Some(src) = source_window
            && src.window_id() != existing.window_id()
        {
            src.update(cx, |_, window, _| window.remove_window())
                .log_err();
        }
        return;
    }

    // SameWindow + target not in another window: bring the Solution
    // into this `MultiWorkspace`. With Phase 2's tab strip, each open
    // Solution is its own retained `Workspace`, so:
    //
    //   * If the target is already a workspace tab in this window,
    //     just activate that tab — no worktree swap, no Workspace
    //     teardown. (Handles tab-strip clicks and re-opening a
    //     Solution from the picker that's still up here.)
    //   * Else, defer-call `open_solution_as_new_workspace`, which
    //     loads a fresh `Workspace` for the target, retains the
    //     currently-active one, and activates the new one.
    //
    // Both branches run inside `cx.defer` so the click handler's
    // window-dispatch frame finishes first — reading or updating a
    // window inline from within `Window::dispatch_event` panics with
    // "attempted to read a window that is already on the stack."
    if intent == OpenIntent::SameWindow
        && let Some(src) = source_window
    {
        let target = sol_id;
        cx.defer(move |cx| {
            let already_open_here = src
                .read_with(cx, |multi_workspace, cx| {
                    multi_workspace
                        .workspaces()
                        .find(|ws| workspace_has_solution(ws, &target, cx))
                        .cloned()
                })
                .ok()
                .flatten();
            if let Some(target_workspace) = already_open_here {
                if let Some(store) = SolutionStore::try_global(cx) {
                    store
                        .update(cx, |s, cx| s.touch_last_opened(&target, cx))
                        .log_err();
                }
                src.update(cx, |multi_workspace, window, cx| {
                    multi_workspace.activate(target_workspace, None, window, cx);
                })
                .log_err();
            } else {
                open_solution_as_new_workspace(target, Some(src), cx);
            }
        });
        return;
    }

    open_solution_as_new_workspace(sol_id, source_window, cx);
}

/// Loads a fresh `Workspace` for `sol_id` via `OpenMode::Add` (when
/// `source_window` is `Some`) or `OpenMode::NewWindow` (when it's
/// `None`). For the `OpenMode::Add` path, retains the source MW's
/// previously-active workspace before activating the freshly-loaded
/// one — so the previous Solution stays available as a tab. For empty
/// solutions, mounts an `EmptySolutionPage` placeholder and hides the
/// solution.root worktree from the panel.
fn open_solution_as_new_workspace(
    sol_id: SolutionId,
    source_window: Option<WindowHandle<MultiWorkspace>>,
    cx: &mut App,
) {
    let Some(store) = SolutionStore::try_global(cx) else {
        return;
    };

    struct OpenInfo {
        paths: Vec<PathBuf>,
        name: String,
        is_empty: bool,
    }
    let info = match store.read_with(cx, |s, _| -> anyhow::Result<OpenInfo> {
        let solution = s
            .solutions()
            .iter()
            .find(|sol| sol.id == sol_id)
            .ok_or_else(|| anyhow!("solution not found: {}", sol_id.as_str()))?;
        let is_empty = solution.members.is_empty();
        let name = solution.name.clone();
        let paths = if is_empty {
            vec![solution.root.clone()]
        } else {
            s.paths_for_open(&sol_id)?
        };
        Ok(OpenInfo {
            paths,
            name,
            is_empty,
        })
    }) {
        Ok(info) => info,
        Err(err) => {
            log::error!(
                "solutions_ui: resolving paths for {} failed: {err}",
                sol_id.as_str()
            );
            return;
        }
    };

    store
        .update(cx, |s, cx| s.touch_last_opened(&sol_id, cx))
        .log_err();

    let app_state = AppState::global(cx);
    let mut options = OpenOptions::default();
    if info.is_empty {
        options.visible = Some(OpenVisible::None);
    }
    let add_to_existing = source_window.is_some();
    if add_to_existing {
        options.open_mode = OpenMode::Add;
        options.requesting_window = source_window;
    } else {
        options.open_mode = OpenMode::NewWindow;
        options.requesting_window = None;
    }
    // Retain the current active workspace BEFORE the new solution's workspace
    // is added. `open_paths`' Add mode pushes the newcomer onto the retained
    // list immediately; the leaving workspace was previously only retained
    // afterwards (in the spawn below, via `retain_active_workspace`), so it
    // landed AFTER the newcomer — putting the freshly-opened tab at the FRONT
    // of the strip. Retaining it here keeps the new tab at the end.
    if let Some(source) = source_window {
        source
            .update(cx, |multi_workspace, _window, cx| {
                multi_workspace.retain_active_workspace(cx);
            })
            .log_err();
    }
    let task = workspace::open_paths(&info.paths, app_state, options, cx);

    // Capture the launcher window (if any) so we can retire it after
    // the new workspace window appears.
    let welcome_window = workspace::welcome::find_existing(cx);

    let sol_id_for_page = sol_id.clone();
    let sol_id_for_lookup = sol_id;
    cx.spawn(async move |cx| {
        let Some(opened) = task.await.log_err() else {
            return;
        };
        if add_to_existing {
            let new_workspace = opened.workspace.clone();
            let target_sol_id = sol_id_for_lookup.clone();
            cx.update(|cx| {
                opened
                    .window
                    .update(cx, |multi_workspace, window, cx| {
                        let existing = multi_workspace
                            .workspaces()
                            .find(|ws| {
                                ws != &&new_workspace
                                    && workspace_has_solution(ws, &target_sol_id, cx)
                            })
                            .cloned();
                        multi_workspace.retain_active_workspace(cx);
                        let to_activate = existing.unwrap_or(new_workspace);
                        multi_workspace.activate(to_activate, None, window, cx);
                    })
                    .log_err();
            });
        }
        // event_sources.rs::observe_new only fires for FRESH MultiWorkspace
        // entities, so adding a solution into an existing window
        // (OpenMode::Add) leaves `open_solutions: HashSet` un-flipped — no
        // workspace.solution_opened delta reaches the mobile client and the
        // mark_closed release observer never gets registered either, so
        // closing the window later won't drop it from mobile's strip.
        // mark_open is idempotent on the HashSet — for the NewWindow case
        // observe_new still races us and the second insert no-ops.
        cx.update(|cx| {
            if let Some(store) = SolutionStore::try_global(cx) {
                store.update(cx, |s, cx| s.mark_open(sol_id_for_lookup.clone(), cx));
            }
        });
        if info.is_empty {
            let sol_id_for_page = sol_id_for_page.clone();
            let name_for_page = info.name.clone();
            cx.update(|cx| {
                opened
                    .window
                    .update(cx, |multi_workspace, window, cx| {
                        let workspace = multi_workspace.workspace().clone();
                        let weak_workspace = workspace.downgrade();
                        workspace.update(cx, |ws, cx| {
                            let page = cx.new(|cx| {
                                crate::empty_solution_page::EmptySolutionPage::new(
                                    sol_id_for_page,
                                    name_for_page,
                                    weak_workspace,
                                    cx,
                                )
                            });
                            ws.add_item_to_active_pane(Box::new(page), None, true, window, cx);
                        });
                    })
                    .log_err();
            });
        }
        if let Some(welcome) = welcome_window {
            cx.update(|cx| {
                welcome
                    .update(cx, |_, window, _| window.remove_window())
                    .log_err();
            });
        }
    })
    .detach();
}

pub(crate) fn workspace_has_solution(
    workspace: &gpui::Entity<workspace::Workspace>,
    sol_id: &SolutionId,
    cx: &App,
) -> bool {
    let project = workspace.read(cx).project().clone();
    let Some(store) = SolutionStore::try_global(cx) else {
        return false;
    };
    let store_read = store.read(cx);
    project.read(cx).worktrees(cx).any(|tree| {
        store_read
            .solution_for_path(&tree.read(cx).abs_path())
            .is_some_and(|sol| &sol.id == sol_id)
    })
}

fn find_window_for_solution(
    sol_id: &SolutionId,
    skip_extra: Option<gpui::WindowId>,
    cx: &App,
) -> Option<WindowHandle<MultiWorkspace>> {
    // `cx.active_window()` returns None during some dispatch frames
    // (e.g. when called from a click handler whose window has been
    // moved off the registry). The caller passes its source window id
    // as a belt-and-suspenders skip so we never re-enter the window
    // we're already inside.
    let skip = skip_window_id(cx);
    for handle in cx.windows() {
        let id = handle.window_id();
        if Some(id) == skip || Some(id) == skip_extra {
            continue;
        }
        let Some(mw_handle) = handle.downcast::<MultiWorkspace>() else {
            continue;
        };
        // Iterate active + retained workspaces — a retained workspace
        // for this solution counts as "already open" because the user's
        // sessions are still alive in it.
        let matches = mw_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspaces()
                    .any(|workspace| workspace_has_solution(workspace, sol_id, cx))
            })
            .ok()
            .unwrap_or(false);
        if matches {
            return Some(mw_handle);
        }
    }
    None
}
