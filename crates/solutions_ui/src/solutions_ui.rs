//! UI layer for the solutions crate: title-bar tab strip, picker,
//! modals, status-bar widget, welcome integration.

mod actions;
pub mod add_project_picker;
mod add_member_picker;
pub mod delete_confirm_modal;
mod empty_solution_page;
mod modals;
mod open;
mod picker;
pub mod project_tab;
pub mod project_tab_strip;
pub mod solution_picker_dropdown;
pub mod solution_tab;
pub mod solution_tab_strip;
mod status_bar;
mod switch;
mod welcome;
mod welcome_trigger;
pub mod window_helpers;

pub use add_project_picker::AddProjectPicker;
pub use empty_solution_page::EmptySolutionPage;
pub use open::{OpenIntent, open_solution};
pub use project_tab_strip::ProjectTabStrip;
pub use status_bar::SolutionsStatusItem;
pub use switch::switch_active_solution_in_place;

pub use actions::{DeleteSolution, NewSolution, OpenSolution, RefreshCacheForCurrent};

use gpui::{App, AppContext as _, TaskExt as _, Window};
use solutions::{SolutionId, SolutionStore, SolutionStoreEvent};
use std::path::PathBuf;
use ui::SharedString;
use util::ResultExt as _;
use workspace::Workspace;

use crate::actions::{
    CloseSolutionFromTabBar, DeleteSolutionFromTabBar, RemoveMember, RenameSolution,
    RevealSolutionFolder, SwitchToNextProjectInPanel, SwitchToNextSolution,
    SwitchToPrevProjectInPanel, SwitchToPrevSolution,
};

pub fn init(cx: &mut App) {
    cx.observe_new(picker::OpenSolutionModal::register).detach();
    cx.observe_new(modals::register).detach();
    cx.observe_new(register_tab_actions).detach();
    cx.observe_new(register_member_sync_observer).detach();
    cx.observe_new(register_solution_delete_observer).detach();
    cx.observe_new(register_solution_close_observer).detach();
    welcome::init(cx);
    switch::register_mcp(cx);
}

/// Remove a solution from the registry and (best-effort) wipe its
/// `root` folder from disk. Callers that already showed their own
/// confirmation modal should invoke this directly instead of
/// re-dispatching the `DeleteSolution` action — `cx.dispatch_action`
/// from inside a nested click/listener silently fails because the
/// active window is already taken from `App::windows`, so the
/// action never reaches the workspace handler that performs the
/// delete.
pub fn delete_solution_with_cleanup(id: SolutionId, root: PathBuf, cx: &mut App) {
    let store = SolutionStore::global(cx);
    store
        .update(cx, |s, cx| s.delete_solution(&id, cx))
        .log_err();
    cx.background_spawn(async move {
        let result: std::io::Result<()> =
            smol::unblock(move || std::fs::remove_dir_all(&root)).await;
        if let Err(err) = result {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "delete_solution: removing directory failed: {err} (orphaned files left in place)"
                );
            }
        }
    })
    .detach();
}

/// Each new `Workspace` subscribes to `SolutionStore` so that adding
/// or removing a member of an already-open solution mounts (or
/// unmounts) the corresponding worktree without requiring a close /
/// reopen. The panels keep their own subscription to the store for
/// their active-member filter; this observer's job is the
/// workspace-level reconciliation (`Workspace::swap_worktrees_to`) that
/// the panels can't do because they don't own a `Window`.
///
/// Both `add_member` (catalog clone) and `add_empty_member` (no
/// clone) emit `Changed`, while `MemberAddCompleted` is fired only
/// by the catalog-clone path. Subscribing to `Changed` covers both;
/// `swap_worktrees_to` is idempotent when the target set already
/// matches, so spurious fires from unrelated mutations are no-ops.
fn register_member_sync_observer(
    _workspace: &mut Workspace,
    window: Option<&mut Window>,
    cx: &mut gpui::Context<Workspace>,
) {
    let Some(window) = window else { return };
    let Some(store) = SolutionStore::try_global(cx) else {
        return;
    };
    cx.subscribe_in(&store, window, |workspace, store, event, window, cx| {
        match event {
            SolutionStoreEvent::Changed => {}
            SolutionStoreEvent::MemberAddCompleted { error: None, .. } => {}
            _ => return,
        }
        let store_read = store.read(cx);
        let project = workspace.project().clone();
        let hosted = project.read(cx).worktrees(cx).find_map(|tree| {
            store_read
                .solution_for_path(&tree.read(cx).abs_path())
                .map(|sol| sol.id.clone())
        });
        let Some(sol_id) = hosted else { return };
        let paths = match store_read.paths_for_open(&sol_id) {
            Ok(paths) => paths,
            Err(err) => {
                log::error!("solutions_ui: paths_for_open({:?}): {err}", sol_id);
                return;
            }
        };
        if paths.is_empty() {
            // Empty solutions keep their hidden placeholder worktree —
            // tearing it down here would orphan the EmptySolutionPage.
            return;
        }
        let visible: std::collections::HashSet<std::path::PathBuf> = project
            .read(cx)
            .visible_worktrees(cx)
            .map(|wt| wt.read(cx).abs_path().to_path_buf())
            .collect();
        let target: std::collections::HashSet<std::path::PathBuf> = paths.iter().cloned().collect();
        if visible == target {
            return;
        }
        workspace
            .swap_worktrees_to(paths, window, cx)
            .detach_and_log_err(cx);
    })
    .detach();
}

/// When a Solution is deleted from the store (from any source — the
/// desktop tab strip, the mobile remote, an MCP call), close the
/// workspace that was hosting it and let `MultiWorkspace` activate a
/// neighbouring solution tab in its place. Without this the deleting
/// caller (notably the mobile `solutions.delete` MCP path, which never
/// touches the desktop window) leaves the window showing an orphaned
/// project with no active solution tab.
///
/// Matches the hosting workspace by the deleted solution's `root` path
/// rather than a store lookup: by the time this fires the solution is
/// already gone from the store, so `solution_for_path` /
/// `workspace_has_solution` would report "not hosted" for the very
/// workspace we need to close.
fn register_solution_delete_observer(
    _workspace: &mut Workspace,
    window: Option<&mut Window>,
    cx: &mut gpui::Context<Workspace>,
) {
    use util::ResultExt as _;
    let Some(window) = window else { return };
    let Some(store) = SolutionStore::try_global(cx) else {
        return;
    };
    cx.subscribe_in(&store, window, |workspace, _store, event, window, cx| {
        let SolutionStoreEvent::Deleted { root, .. } = event else {
            return;
        };
        let Some(mw_weak) = workspace.multi_workspace().cloned() else {
            return;
        };
        let root = root.clone();
        // Defer: iterating / reading sibling workspaces must not run while
        // this Workspace entity is mid-update (mirrors `close_solution`).
        cx.spawn_in(window, async move |_, cx| {
            if let Some(mw) = cx.update(|_, _| mw_weak.upgrade()).ok().flatten() {
                mw.update_in(cx, |mw, window, cx| {
                    close_workspaces_under_root_in(mw, &root, window, cx);
                })
                .log_err();
            }
        })
        .detach();
    })
    .detach();
}

/// Mirror of [`register_solution_delete_observer`] for the non-destructive
/// close path. Listens for [`SolutionStoreEvent::Closed`] (emitted by
/// `SolutionStore::mark_closed`) and tears down every workspace tab in
/// every `MultiWorkspace` that hosts the closed solution. Without this,
/// a `workspace.close_solution` RPC from the mobile client only flipped
/// the `open` flag and emitted the mobile-facing wire notification, but
/// left the corresponding desktop workspace tabs visible until the user
/// closed them by hand. The desktop tab-bar "Close" action also calls
/// `mark_closed`, so both paths converge on the same teardown seam.
fn register_solution_close_observer(
    _workspace: &mut Workspace,
    window: Option<&mut Window>,
    cx: &mut gpui::Context<Workspace>,
) {
    use util::ResultExt as _;
    let Some(window) = window else { return };
    let Some(store) = SolutionStore::try_global(cx) else {
        return;
    };
    cx.subscribe_in(&store, window, |workspace, _store, event, window, cx| {
        let SolutionStoreEvent::Closed { id } = event else {
            return;
        };
        let Some(mw_weak) = workspace.multi_workspace().cloned() else {
            return;
        };
        let id = id.clone();
        cx.spawn_in(window, async move |_, cx| {
            if let Some(mw) = cx.update(|_, _| mw_weak.upgrade()).ok().flatten() {
                mw.update_in(cx, |mw, window, cx| {
                    close_solution_workspaces_in(mw, &id, window, cx);
                })
                .log_err();
            }
        })
        .detach();
    })
    .detach();
}

/// Close every workspace in `mw` whose project has a worktree under
/// `root` (i.e. it was hosting the just-deleted solution), then open the
/// launcher if the window is left empty. `remove_project_group` activates
/// a neighbouring group, so the user lands on another open solution
/// rather than a blank window. Idempotent: if several of the window's
/// workspaces fire this for the same delete, the first call removes the
/// group and the rest find nothing to close.
fn close_workspaces_under_root_in(
    mw: &mut workspace::MultiWorkspace,
    root: &std::path::Path,
    window: &mut Window,
    cx: &mut gpui::Context<workspace::MultiWorkspace>,
) {
    use util::ResultExt as _;
    let to_close: Vec<_> = mw
        .workspaces()
        .filter(|ws| {
            ws.read(cx)
                .project()
                .read(cx)
                .worktrees(cx)
                .any(|tree| tree.read(cx).abs_path().starts_with(root))
        })
        .map(|ws| (ws.read(cx).project_group_key(cx), ws.clone()))
        .collect();
    if to_close.is_empty() {
        return;
    }
    let close_tasks: Vec<_> = to_close
        .into_iter()
        .map(|(group_key, ws)| {
            // Same guard as `close_solution_workspaces_in`: a workspace with
            // only a hidden placeholder worktree has an empty visible
            // `ProjectGroupKey`, and `remove_project_group(empty)` would
            // over-match every other placeholder-only solution in the window.
            // Close those individually; reserve `remove_project_group` for
            // unique non-empty keys.
            if group_key.path_list().paths().is_empty() {
                mw.close_workspace(&ws, window, cx)
            } else {
                mw.remove_project_group(&group_key, window, cx)
            }
        })
        .collect();
    cx.spawn_in(window, async move |this, cx| {
        for task in close_tasks {
            task.await.log_err();
        }
        this.update_in(cx, |mw, window, cx| {
            crate::welcome_trigger::open_welcome_if_window_empty(mw, window, cx);
        })
        .log_err();
    })
    .detach();
}

fn register_tab_actions(
    workspace: &mut Workspace,
    _: Option<&mut Window>,
    _: &mut gpui::Context<Workspace>,
) {
    workspace.register_action(|workspace, action: &CloseSolutionFromTabBar, window, cx| {
        let id = SolutionId(action.id.clone());
        close_solution(workspace, id, window, cx);
    });
    workspace.register_action(|workspace, action: &DeleteSolutionFromTabBar, window, cx| {
        let id = SolutionId(action.id.clone());
        let store = SolutionStore::global(cx);
        let Some((name, root)) = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|sol| sol.id == id)
                .map(|sol| (sol.name.clone(), sol.root.clone()))
        }) else {
            return;
        };
        let folder_label = SharedString::from(format!("Folder {}", root.display()));
        let root_for_cleanup = root.clone();
        crate::delete_confirm_modal::open_delete_confirm(
            workspace,
            SharedString::from(format!("Delete solution \"{name}\"?")),
            "This will permanently delete:",
            vec![
                crate::delete_confirm_modal::DeleteConfirmItem {
                    label: "Registry entry".into(),
                    path: None,
                },
                crate::delete_confirm_modal::DeleteConfirmItem {
                    label: folder_label,
                    path: Some(root),
                },
            ],
            move |_window, cx| {
                delete_solution_with_cleanup(id, root_for_cleanup, cx);
            },
            window,
            cx,
        );
    });
    workspace.register_action(|_workspace, action: &RevealSolutionFolder, _window, cx| {
        let id = SolutionId(action.id.clone());
        let store = SolutionStore::global(cx);
        let Some(root) = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|sol| sol.id == id)
                .map(|sol| sol.root.clone())
        }) else {
            return;
        };
        cx.reveal_path(&root);
    });
    workspace.register_action(|workspace, action: &RenameSolution, window, cx| {
        let id = SolutionId(action.id.clone());
        crate::modals::open_rename_solution(workspace, id, window, cx);
    });
    workspace.register_action(|_workspace, _: &SwitchToNextSolution, window, cx| {
        crate::switch::cycle_solution(1, window, cx);
    });
    workspace.register_action(|_workspace, _: &SwitchToPrevSolution, window, cx| {
        crate::switch::cycle_solution(-1, window, cx);
    });
    workspace.register_action(|workspace, action: &SwitchToNextProjectInPanel, _, cx| {
        cycle_project_in_panel(workspace, &action.panel_kind, 1, cx);
    });
    workspace.register_action(|workspace, action: &SwitchToPrevProjectInPanel, _, cx| {
        cycle_project_in_panel(workspace, &action.panel_kind, -1, cx);
    });
    workspace.register_action(|workspace, _: &RefreshCacheForCurrent, _, cx| {
        refresh_cache_for_active_solution(workspace, cx);
    });
    workspace.register_action(|workspace, action: &RemoveMember, window, cx| {
        use util::ResultExt as _;

        let sol_id = SolutionId(action.solution_id.clone());
        let cat_id = solutions::CatalogId(action.catalog_id.clone());
        let store = SolutionStore::global(cx);
        let Some((sol_name, member_path, member_label)) = store.read_with(cx, |s, _| {
            let sol = s.solutions().iter().find(|sol| sol.id == sol_id)?;
            let m = sol.members.iter().find(|m| m.catalog_id == cat_id)?;
            let label = m
                .local_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| m.catalog_id.0.clone());
            Some((sol.name.clone(), m.local_path.clone(), label))
        }) else {
            return;
        };
        let folder_label = SharedString::from(format!("Folder {}", member_path.display()));
        let title = SharedString::from(format!(
            "Remove project \"{}\" from solution \"{}\"?",
            member_label, sol_name,
        ));
        let path_for_rm = member_path.clone();
        // Capture the triggering workspace's project so the removed member's
        // worktree can be detached on confirm — otherwise the now-deleted
        // folder lingers as a dangling (soon-empty) worktree in the project,
        // which the Project Panel keeps showing once the solution scopes to
        // "all worktrees" (e.g. after removing the last member).
        let project = workspace.project().clone();
        let worktree_path = member_path.clone();
        crate::delete_confirm_modal::open_delete_confirm(
            workspace,
            title,
            "This will permanently delete:",
            vec![
                crate::delete_confirm_modal::DeleteConfirmItem {
                    label: "Member entry from this solution".into(),
                    path: None,
                },
                crate::delete_confirm_modal::DeleteConfirmItem {
                    label: folder_label,
                    path: Some(member_path),
                },
            ],
            move |_window, cx| {
                let store = SolutionStore::global(cx);
                store
                    .update(cx, |s, cx| s.remove_member(&sol_id, &cat_id, cx))
                    .log_err();
                // Detach the removed member's worktree(s) from the project so
                // the Project Panel stops rendering its tree. Done before the
                // folder is rm-rf'd so the editor isn't watching a directory
                // that's about to vanish.
                project.update(cx, |project, cx| {
                    let stale: Vec<_> = project
                        .worktrees(cx)
                        .filter(|w| w.read(cx).abs_path().starts_with(&worktree_path))
                        .map(|w| w.read(cx).id())
                        .collect();
                    for id in stale {
                        project.remove_worktree(id, cx);
                    }
                });
                let path = path_for_rm.clone();
                cx.background_spawn(async move {
                    let result: std::io::Result<()> =
                        smol::unblock(move || std::fs::remove_dir_all(&path)).await;
                    if let Err(err) = result {
                        if err.kind() != std::io::ErrorKind::NotFound {
                            log::warn!(
                                "RemoveMember: removing {} failed: {err} (orphaned files left on disk)",
                                path_for_rm.display(),
                            );
                        }
                    }
                })
                .detach();
            },
            window,
            cx,
        );
    });
}

/// Close every workspace (active or retained) that hosts `sol_id` and
/// stop its AI sessions. Originally lived on the (retired) dock panel —
/// extracted so the title-bar tab strip can call it without a panel
/// dependency. Iterates every window: the caller's window via the
/// workspace's `MultiWorkspace` handle (so we don't double-lease the
/// in-flight window), then the rest by downcasting `cx.windows()`.
fn close_solution(
    workspace: &mut Workspace,
    sol_id: SolutionId,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    use solution_agent::store::SolutionAgentStore;
    use util::ResultExt as _;
    use workspace::MultiWorkspace;

    if let Some(agent_store) = SolutionAgentStore::try_global(cx) {
        agent_store.update(cx, |store, cx| {
            let session_ids: Vec<_> = store
                .sessions_for(&sol_id)
                .into_iter()
                .map(|session| session.read(cx).id)
                .collect();
            for id in session_ids {
                store.close_session(id, cx).log_err();
            }
        });
    }
    // Workspace iteration must run AFTER the action handler's
    // `workspace.update(...)` frame finishes — otherwise iterating
    // `mw.workspaces()` and reading each Workspace panics on the
    // currently-being-updated entity ("cannot read workspace::Workspace
    // while it is already being updated"). Defer via `cx.spawn_in` so
    // the closure runs on the next foreground turn with the lock
    // released.
    let mw_weak = workspace.multi_workspace().cloned();
    let skip_window_id = window.window_handle().window_id();
    let sol_id_for_defer = sol_id.clone();
    cx.spawn_in(window, async move |_, cx| {
        if let Some(mw_weak) = mw_weak
            && let Some(mw) = cx.update(|_, _| mw_weak.upgrade()).ok().flatten()
        {
            let sol_id = sol_id_for_defer.clone();
            mw.update_in(cx, |mw, window, cx| {
                close_solution_workspaces_in(mw, &sol_id, window, cx);
            })
            .log_err();
        }
        let other_windows: Vec<_> = cx
            .update(|_, cx| {
                cx.windows()
                    .into_iter()
                    .filter(|handle| handle.window_id() != skip_window_id)
                    .filter_map(|handle| handle.downcast::<MultiWorkspace>())
                    .collect()
            })
            .unwrap_or_default();
        for handle in other_windows {
            let sol_id = sol_id_for_defer.clone();
            handle
                .update(cx, move |mw, window, cx| {
                    close_solution_workspaces_in(mw, &sol_id, window, cx);
                })
                .log_err();
        }
        // Drive `mark_closed` from here too. The MultiWorkspace release
        // observer in `solutions::event_sources` only fires when the
        // entire window drops (every solution in it closed), so a
        // multi-solution window that closes ONE of N solutions never
        // fired `mark_closed` for that solution — the mobile client
        // missed the `workspace.solution_closed` notification and showed
        // a stale row. Calling it here covers the "still other
        // solutions in the window" case; `mark_closed` is idempotent on
        // an already-closed id, so the subsequent release-observer fire
        // on the eventual window drop is a safe no-op.
        cx.update(|_, cx| {
            if let Some(store) = solutions::SolutionStore::try_global(cx) {
                store.update(cx, |s, cx| s.mark_closed(&sol_id_for_defer, cx));
            }
        })
        .log_err();
    })
    .detach();
}

/// Advances or retreats the solution-wide active-member selection for
/// the active solution by `dir` steps (`+1` = next, `-1` = previous),
/// wrapping at both ends. No-op if the workspace has no active solution
/// or the solution has no members. The selection is now solution-wide
/// (shared across project_panel and git_panel), so `_panel_kind` is
/// retained only to keep the action payload stable for existing keymaps.
fn cycle_project_in_panel(workspace: &Workspace, _panel_kind: &str, dir: isize, cx: &mut gpui::App) {
    let Some(sol_id) = crate::window_helpers::active_solution_in_workspace(workspace, cx) else {
        return;
    };
    let store = SolutionStore::global(cx);
    let Some((members, current)) = store.read_with(cx, |s, _| {
        let sol = s.solutions().iter().find(|sol| sol.id == sol_id)?;
        let members: Vec<solutions::CatalogId> =
            sol.members.iter().map(|m| m.catalog_id.clone()).collect();
        if members.is_empty() {
            return None;
        }
        let current = s
            .active_member(&sol.id)
            .cloned()
            .unwrap_or_else(|| members[0].clone());
        Some((members, current))
    }) else {
        return;
    };
    let new_idx = cycle_index(
        members.iter().position(|c| *c == current).unwrap_or(0),
        members.len(),
        dir,
    );
    let new_catalog = members[new_idx].clone();
    store.update(cx, |s, cx| {
        s.set_active_member(sol_id, new_catalog, cx);
    });
}

fn refresh_cache_for_active_solution(workspace: &Workspace, cx: &mut gpui::App) {
    let Some(sol_id) = crate::window_helpers::active_solution_in_workspace(workspace, cx) else {
        log::info!("RefreshCacheForCurrent: no active solution in this workspace");
        return;
    };
    let store = SolutionStore::global(cx);
    let targets: Vec<(solutions::CatalogId, String)> = store.read_with(cx, |s, _| {
        let Some(sol) = s.solutions().iter().find(|sol| sol.id == sol_id) else {
            return Vec::new();
        };
        sol.members
            .iter()
            .filter_map(|m| {
                s.catalog()
                    .iter()
                    .find(|c| c.id == m.catalog_id)
                    .map(|c| (c.id.clone(), c.remote_url.clone()))
            })
            .collect()
    });
    if targets.is_empty() {
        log::info!(
            "RefreshCacheForCurrent: solution {sol_id:?} has no members with catalog entries"
        );
        return;
    }
    let cache_root = solutions::default_cache_root();
    log::info!(
        "RefreshCacheForCurrent: refreshing {} catalog entr{} for solution {sol_id:?}",
        targets.len(),
        if targets.len() == 1 { "y" } else { "ies" },
    );
    for (catalog_id, remote_url) in targets {
        let cache_root = cache_root.clone();
        cx.background_spawn(async move {
            match solutions::refresh_cache(&cache_root, &remote_url, |_| {}).await {
                Ok(_) => log::info!("RefreshCacheForCurrent: refreshed {catalog_id:?}"),
                Err(err) => {
                    log::warn!("RefreshCacheForCurrent: refresh of {catalog_id:?} failed: {err}")
                }
            }
        })
        .detach();
    }
}

fn cycle_index(cur: usize, len: usize, dir: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let n = len as isize;
    let new = (((cur as isize + dir) % n) + n) % n;
    new as usize
}

#[cfg(test)]
mod tests {
    use super::cycle_index;

    #[test]
    fn cycle_index_forward() {
        assert_eq!(cycle_index(0, 3, 1), 1);
    }

    #[test]
    fn cycle_index_wrap_forward() {
        assert_eq!(cycle_index(2, 3, 1), 0);
    }

    #[test]
    fn cycle_index_wrap_backward() {
        assert_eq!(cycle_index(0, 3, -1), 2);
    }

    #[test]
    fn cycle_index_single_element() {
        assert_eq!(cycle_index(0, 1, 1), 0);
        assert_eq!(cycle_index(0, 1, -1), 0);
    }
}

fn close_solution_workspaces_in(
    mw: &mut workspace::MultiWorkspace,
    sol_id: &SolutionId,
    window: &mut Window,
    cx: &mut gpui::Context<workspace::MultiWorkspace>,
) {
    use util::ResultExt as _;

    // Snapshot every workspace's project-group key alongside the
    // workspace itself. For a workspace that still has worktrees we close
    // via `remove_project_group` (instead of `close_workspace`) so the
    // lingering group entry doesn't survive the close — `remove_workspace`'s
    // fallback walks neighbouring groups and cheerfully respawns a workspace
    // from the previously-closed solution's path list, leaving the user with
    // a "ghost" tab for a solution they explicitly closed seconds ago.
    let to_close: Vec<_> = mw
        .workspaces()
        .filter(|ws| crate::open::workspace_has_solution(ws, sol_id, cx))
        .map(|ws| (ws.read(cx).project_group_key(cx), ws.clone()))
        .collect();
    if to_close.is_empty() {
        return;
    }
    let close_tasks: Vec<_> = to_close
        .into_iter()
        .map(|(group_key, ws)| {
            if group_key.path_list().paths().is_empty() {
                // A worktree-less workspace carries the shared empty/default
                // `ProjectGroupKey`. `remove_project_group(empty)` matches by
                // key and `workspaces_for_project_group(empty)` returns EVERY
                // worktree-less workspace in the window — so it would also
                // close any *other* empty solution's tab (the "closed one
                // solution and a neighbour closed too" bug). There is also no
                // project-group entry for an empty key, so the ghost-respawn
                // concern that motivates `remove_project_group` doesn't apply.
                // Close just this workspace.
                mw.close_workspace(&ws, window, cx)
            } else {
                // Non-empty keys are unique per solution (each member lives
                // under its own solution root / git identity), so removing the
                // whole group only closes this solution's workspaces while
                // clearing the lingering group entry.
                mw.remove_project_group(&group_key, window, cx)
            }
        })
        .collect();
    // Spawn one coordinator that awaits every close before checking
    // whether this window still hosts any solution. Awaiting
    // sequentially is fine — close ordering doesn't matter for the
    // emptiness check, and join_all would pull `futures` in just to
    // save microseconds on an action triggered by a human click.
    cx.spawn_in(window, async move |this, cx| {
        for task in close_tasks {
            task.await.log_err();
        }
        this.update_in(cx, |mw, window, cx| {
            crate::welcome_trigger::open_welcome_if_window_empty(mw, window, cx);
        })
        .log_err();
    })
    .detach();
}
