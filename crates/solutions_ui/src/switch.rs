//! In-place Solution switch within a single open window.
//!
//! The earlier path (`open::open_solution` with `OpenIntent::SameWindow`)
//! built a *new* `Workspace` for every Solution-switch, retained the
//! old one, and let `MultiWorkspace::activate` flip between them. That
//! preserves the old Solution's panel state because the entity stays
//! alive — but only at the cost of visible UI churn on every switch:
//! the new Workspace mounts with default panels, then the user has to
//! re-establish their layout / re-find their tabs.
//!
//! `switch_active_solution_in_place` flips this around: keep the same
//! `Workspace`/`Project`/dock entities alive, swap *worktrees* inside
//! the existing `Project` to match the target Solution's members
//! (`Workspace::swap_worktrees_to`), and snapshot/replay the per-
//! Solution open-tab list through `SolutionStore::tab_snapshots`.
//! Upstream panels (`ProjectPanel`, `OutlinePanel`, …) react to
//! `project::Event::WorktreeAdded`/`Removed` automatically; fork
//! panels listen to `SolutionStoreEvent::ActiveSolutionChanged`.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp, Entity, Task, WeakEntity, Window};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use solutions::{
    DockSideSnapshot, SolutionDockSnapshot, SolutionId, SolutionStore, SolutionTabsSnapshot,
};
use util::ResultExt as _;
use workspace::dock::Dock;
use workspace::{MultiWorkspace, OpenOptions, OpenVisible, SaveIntent, Workspace};

/// Run an in-place Solution switch on the given `Workspace`. Steps:
///
/// 1. Identify the *previous* Solution by scanning the workspace's
///    visible worktrees through `SolutionStore::solution_for_path`.
///    If found, snapshot its open tabs into
///    `SolutionStore::tab_snapshots` so a future switch back can
///    restore them.
/// 2. `touch_last_opened(target_id)` — bumps the activity stamp and
///    fires `Changed` + `ActiveSolutionChanged(target_id)` so fork
///    panels can refresh their content.
/// 3. Resolve `target_id`'s member paths and call
///    `Workspace::swap_worktrees_to` to reconcile.
/// 4. Replay `target_id`'s saved open-tab snapshot (if any) by
///    closing every currently-open editor and re-opening the
///    snapshot's `open_paths` in order, activating `active_path`
///    last.
///
/// Snapshot save failures (step 1) are logged-and-continued — the
/// user wants to *get to* the new Solution; losing one tab list is
/// recoverable. A worktree-swap failure (step 3) is propagated as
/// `Err` so callers can surface it via toast / status.
pub fn switch_active_solution_in_place(
    workspace: WeakEntity<Workspace>,
    target_id: SolutionId,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
    window.spawn(cx, async move |cx| {
        let workspace = workspace.upgrade().context("workspace dropped")?;

        // Step 1: snapshot current.
        let prev_id = previous_solution_id(&workspace, cx)?;
        if let Some(prev_id) = prev_id.clone() {
            // `Entity::update` on `AsyncWindowContext` returns `R`
            // directly (not `Result<R>`); the entity-released case
            // surfaces only when calling through `update_in`. Plain
            // reads here can't observe a dropped entity (we just
            // upgraded the Weak).
            let snapshot = workspace.update(cx, |workspace, cx| {
                let app: &App = cx;
                SolutionTabsSnapshot {
                    open_paths: workspace.open_item_abs_paths(app),
                    active_path: workspace
                        .active_item(app)
                        .and_then(|item| item.project_path(app))
                        .and_then(|pp| workspace.project().read(app).absolute_path(&pp, app)),
                }
            });
            // Capture the leaving Solution's dock layout in the same
            // breath as its tabs. Both are per-Solution state that would
            // otherwise be SHARED, because the in-place switch keeps the
            // one Workspace (and its three Docks) alive across switches.
            let dock_snapshot =
                workspace.update(cx, |workspace, cx| capture_dock_snapshot(workspace, cx));
            cx.update(|_, cx| {
                if let Some(store) = SolutionStore::try_global(cx) {
                    store.update(cx, |store, cx| {
                        store.store_tab_snapshot(prev_id.clone(), snapshot, cx);
                        store.set_dock_snapshot(prev_id, dock_snapshot, cx);
                    });
                }
            })
            .ok();
        }

        // Step 2: bump active id (also fires ActiveSolutionChanged).
        cx.update(|_, cx| {
            if let Some(store) = SolutionStore::try_global(cx) {
                store
                    .update(cx, |s, cx| s.touch_last_opened(&target_id, cx))
                    .log_err();
            }
        })
        .ok();

        // Step 3: resolve + swap worktrees.
        let target_paths: Vec<PathBuf> = cx
            .update(|_, cx| {
                SolutionStore::try_global(cx)
                    .and_then(|store| {
                        store.read_with(cx, |s, _| s.paths_for_open(&target_id).log_err())
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        if target_paths.is_empty() {
            // Target Solution has no members yet (newly-created or a
            // legacy row with empty member list). Nothing to swap to —
            // we still keep `active` flipped so the panel chrome
            // reflects the new selection, but don't tear down the
            // existing worktrees because that'd leave the user
            // staring at a panel-less editor.
            return Ok(());
        }
        let swap_task = workspace.update_in(cx, |workspace, window, cx| {
            workspace.swap_worktrees_to(target_paths, window, cx)
        })?;
        swap_task.await?;

        // Step 4: close all currently-open editor items. Their
        // ProjectPaths point at the *previous* Solution's worktrees,
        // which we just demounted; leaving them in place produces
        // stale tabs whose buffers' worktrees no longer exist
        // (visible to MCP / panels as ghost entries that read garbage
        // when activated). This step runs whether or not the target
        // Solution has a saved snapshot — a "first visit" target
        // ends with an empty pane, matching the user's mental model
        // of "I never had any tabs in this Solution yet."
        close_all_editor_items(&workspace, cx).await?;

        // Step 5: replay target Solution's snapshot, if any.
        let snapshot = cx
            .update(|_, cx| {
                SolutionStore::try_global(cx).and_then(|store| {
                    store.read_with(cx, |s, _| s.tab_snapshot(&target_id).cloned())
                })
            })
            .ok()
            .flatten();
        if let Some(snapshot) = snapshot {
            for path in &snapshot.open_paths {
                let task = workspace.update_in(cx, |workspace, window, cx| {
                    let mut options = OpenOptions::default();
                    options.visible = Some(OpenVisible::None);
                    workspace.open_abs_path(path.clone(), options, window, cx)
                })?;
                let _ = task.await;
            }
            if let Some(active) = snapshot.active_path {
                let task = workspace.update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(active, OpenOptions::default(), window, cx)
                })?;
                let _ = task.await;
            }
        }

        // Step 6: restore target Solution's dock layout, if a snapshot
        // exists. ABSENT (first time this Solution is shown in this
        // session) → do nothing: the Solution inherits whatever docks
        // are currently mounted and starts remembering its own changes
        // from here on. This toggling is the INTENDED per-Solution
        // restore, keyed by solution and only applied when a snapshot
        // exists — NOT a splash-robustness band-aid.
        let dock_snapshot = cx
            .update(|_, cx| {
                SolutionStore::try_global(cx).and_then(|store| {
                    store.read_with(cx, |s, _| s.dock_snapshot(&target_id).cloned())
                })
            })
            .ok()
            .flatten();
        if let Some(dock_snapshot) = dock_snapshot {
            workspace.update_in(cx, |workspace, window, cx| {
                apply_dock_snapshot(workspace, &dock_snapshot, window, cx);
            })?;
        }
        Ok(())
    })
}

/// Capture the current dock layout (open/closed, active panel index, and
/// active-panel size per side) of the given workspace into a
/// `SolutionDockSnapshot`. Mirrors `MultiWorkspace::activate`'s old
/// snapshot helper but also records the active-panel size so a switch
/// back restores the exact width/height the user dragged to.
fn capture_dock_snapshot(workspace: &Workspace, cx: &App) -> SolutionDockSnapshot {
    let side = |dock: &Entity<Dock>| {
        let dock = dock.read(cx);
        DockSideSnapshot {
            is_open: dock.is_open(),
            active_panel_index: dock.active_panel_index(),
            size: dock.active_panel_size(),
        }
    };
    SolutionDockSnapshot {
        left: side(workspace.left_dock()),
        right: side(workspace.right_dock()),
        bottom: side(workspace.bottom_dock()),
    }
}

/// Apply a previously-captured `SolutionDockSnapshot` to the workspace's
/// docks. Safe-apply throughout: `activate_panel`/`set_open` are no-ops
/// when already in the target state, the active-panel index is only
/// applied when the dock actually has that panel, and the size is only
/// pushed onto the side's active panel.
fn apply_dock_snapshot(
    workspace: &Workspace,
    snapshot: &SolutionDockSnapshot,
    window: &mut Window,
    cx: &mut App,
) {
    for (dock, side) in [
        (workspace.left_dock(), &snapshot.left),
        (workspace.right_dock(), &snapshot.right),
        (workspace.bottom_dock(), &snapshot.bottom),
    ] {
        dock.update(cx, |dock, cx| {
            if let Some(index) = side.active_panel_index
                && index < dock.panels_len()
            {
                dock.activate_panel(index, window, cx);
            }
            dock.set_open(side.is_open, window, cx);
            // Size applies to whichever panel is now active. Skip when
            // the side is closed or has no active panel — there's
            // nothing visible to size.
            if let (Some(size_state), Some(active_panel)) =
                (side.size, dock.active_panel().cloned())
            {
                dock.set_panel_size_state(active_panel.as_ref(), size_state, cx);
            }
        });
    }
}

fn previous_solution_id(
    workspace: &Entity<Workspace>,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<Option<SolutionId>> {
    cx.update(|_, cx| {
        let Some(store) = SolutionStore::try_global(cx) else {
            return None;
        };
        let store_read = store.read(cx);
        let project = workspace.read(cx).project().clone();
        project
            .read(cx)
            .visible_worktrees(cx)
            .find_map(|wt| store_read.solution_for_path(&wt.read(cx).abs_path()))
            .map(|sol| sol.id.clone())
    })
}

// =====================================================================
// MCP tool: solutions.switch
// =====================================================================
//
// Switch the *active* Solution shown in a given window without
// recreating its `Workspace`. Wraps `switch_active_solution_in_place`
// for autonomous agents driving the editor over the MCP socket; the
// `solutions.open` tool stays as the "open in a new window" path.
//
// Errors mirror what the orchestrator surfaces:
//   - `window_not_found`        — `window_id` doesn't match any open window
//   - `window_not_multi_workspace` — window isn't a `MultiWorkspace`
//   - `solution_not_found`      — `solution_id` not registered in the store
// All other errors propagate as the orchestrator's `Result<()>` text.

/// Switch the active Solution within an existing window in-place.
/// Keeps the same `Workspace`/`Project`/dock entities, swaps worktrees
/// inside the existing `Project`, and replays per-Solution open-tabs
/// from `SolutionStore::tab_snapshots`. See module docs for full
/// behaviour.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SwitchSolutionParams {
    pub window_id: String,
    pub solution_id: String,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SwitchSolutionResult {
    pub window_id: String,
    pub solution_id: String,
}

#[derive(Clone)]
pub struct SwitchSolutionTool;

impl McpServerTool for SwitchSolutionTool {
    type Input = SwitchSolutionParams;
    type Output = SwitchSolutionResult;
    const NAME: &'static str = "solutions.switch";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.window_id.is_empty(),
            "invalid_params: window_id is required"
        );
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        let target_id = SolutionId(input.solution_id.clone());
        let window_id_str = input.window_id.clone();

        // Resolve the window + active workspace, then schedule the
        // switch on that window's foreground tick. Orchestrator
        // returns `Task<Result<()>>` which we await.
        let task: Task<anyhow::Result<()>> =
            cx.update(|cx| -> anyhow::Result<Task<anyhow::Result<()>>> {
                anyhow::ensure!(
                    SolutionStore::try_global(cx)
                        .map(|store| {
                            store.read(cx).solutions().iter().any(|s| s.id == target_id)
                        })
                        .unwrap_or(false),
                    "solution_not_found: {}",
                    target_id.0,
                );
                let handle = cx
                    .windows()
                    .into_iter()
                    .find(|h| editor_mcp::format_window_id(h.window_id()) == window_id_str)
                    .with_context(|| format!("window_not_found: {window_id_str}"))?;
                let multi = handle
                    .downcast::<MultiWorkspace>()
                    .with_context(|| format!("window_not_multi_workspace: {window_id_str}"))?;
                multi.update(cx, |multi_workspace, window, cx| {
                    let workspace = multi_workspace.workspace().clone().downgrade();
                    switch_active_solution_in_place(workspace, target_id.clone(), window, cx)
                })
            })?;
        task.await?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("switched to {} in {}", input.solution_id, input.window_id),
            }],
            structured_content: SwitchSolutionResult {
                window_id: input.window_id,
                solution_id: input.solution_id,
            },
        })
    }
}

pub fn register_mcp(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SwitchSolutionTool);
    });
}

async fn close_all_editor_items(
    workspace: &Entity<Workspace>,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<()> {
    let item_ids: Vec<_> = workspace.update(cx, |workspace, cx| {
        let app: &App = cx;
        workspace
            .items(app)
            .map(|item| item.item_id())
            .collect::<Vec<_>>()
    });
    for id in item_ids {
        let close_task = workspace.update_in(cx, |workspace, window, cx| {
            let active_pane = workspace.active_pane().clone();
            active_pane.update(cx, |pane, cx| {
                pane.close_item_by_id(id, SaveIntent::Skip, window, cx)
            })
        })?;
        let _ = close_task.await;
    }
    Ok(())
}

// =====================================================================
// Keyboard cycle actions: SwitchToNext/PrevSolution
// =====================================================================

/// Cycle to the next or previous open Solution within the active window.
/// Direction: `1` for next, `-1` for previous. Wraps at boundaries using
/// modular arithmetic: `(((idx + dir) % n) + n) % n` handles negative wraps.
///
/// With multi-tab, each open Solution lives in its own retained
/// `Workspace`, so cycling means activating a different existing
/// workspace via [`MultiWorkspace::activate`] — the same primitive the
/// title-bar tab strip uses on click. The earlier implementation called
/// `switch_active_solution_in_place`, which swapped worktrees inside
/// the active workspace and is now reserved for the MCP `solutions.switch`
/// tool (autonomous-agent driving where the agent declared "switch this
/// window's active solution to X without spawning a tab").
pub fn cycle_solution(direction: i32, window: &mut Window, cx: &mut App) {
    // Defer the swap so the source `Workspace::update` whose action
    // handler reached us has released its lease. Reading workspaces
    // inline here — even just to enumerate `(SolutionId, Workspace)`
    // pairs — panics with "cannot read workspace::Workspace while it
    // is already being updated" when the iteration hits the active
    // one. By the time the deferred closure runs, the window is also
    // off the dispatch stack so the registry-based `WindowHandle::update`
    // works without the "window not found" error we'd otherwise hit
    // inline.
    let Some(mw_handle) = window.window_handle().downcast::<MultiWorkspace>() else {
        return;
    };
    cx.defer(move |cx| {
        mw_handle
            .update(cx, |mw, window, cx| {
                let pairs = solution_workspace_pairs(mw, cx);
                if pairs.len() < 2 {
                    return;
                }
                let Some(active_id) = active_solution_id_in(mw, cx) else {
                    return;
                };
                let cur_idx = pairs
                    .iter()
                    .position(|(id, _)| id == &active_id)
                    .unwrap_or(0);
                let n = pairs.len() as i32;
                let next_idx = (((cur_idx as i32 + direction) % n) + n) % n;
                let target_workspace = pairs[next_idx as usize].1.clone();
                mw.activate(target_workspace, None, window, cx);
            })
            .ok();
    });
}

fn active_solution_id_in(mw: &MultiWorkspace, cx: &App) -> Option<SolutionId> {
    let active_workspace = mw.workspace().clone();
    let store = SolutionStore::global(cx);
    let store_read = store.read(cx);
    let project = active_workspace.read(cx).project().clone();
    project.read(cx).worktrees(cx).find_map(|tree| {
        store_read
            .solution_for_path(&tree.read(cx).abs_path())
            .map(|sol| sol.id.clone())
    })
}

/// Snapshot the open `(SolutionId, Workspace)` pairs in this window in
/// the order they appear in the tab strip. Each solution is reported
/// once even if it happens to be open in multiple workspaces — the
/// first workspace mapping to a solution wins, mirroring the dedupe
/// in `SolutionTabStrip::render`.
fn solution_workspace_pairs(mw: &MultiWorkspace, cx: &App) -> Vec<(SolutionId, Entity<Workspace>)> {
    let store = SolutionStore::global(cx);
    let store_read = store.read(cx);
    let mut pairs: Vec<(SolutionId, Entity<Workspace>)> = Vec::new();
    for ws in mw.workspaces() {
        let project = ws.read(cx).project().clone();
        if let Some(sol_id) = project.read(cx).worktrees(cx).find_map(|tree| {
            store_read
                .solution_for_path(&tree.read(cx).abs_path())
                .map(|sol| sol.id.clone())
        }) {
            if !pairs.iter().any(|(existing, _)| existing == &sol_id) {
                pairs.push((sol_id, ws.clone()));
            }
        }
    }
    pairs
}
