//! Horizontal solution-tab strip rendered in the title bar after the
//! hamburger. Hosts the open solutions as `SolutionTab` children, plus
//! a trailing `+` button that opens [`SolutionPickerDropdown`] (or
//! dispatches [`crate::actions::NewSolution`] directly when the catalog
//! has no closed solutions to pick from — in that case the dropdown
//! would only show the "Create new solution…" entry, so we skip it).
//!
//! Source of truth:
//!   * `MultiWorkspace::workspaces()` for the list of open workspaces in
//!     this window (the active one plus retained ones), each mapped to a
//!     `SolutionId` via the `solutions::SolutionStore` worktree lookup.
//!   * `MultiWorkspace::workspace()` for the active workspace, whose
//!     solution is highlighted as the active tab.
//!   * `SolutionStore::solutions()` for the displayed name and to count
//!     closed solutions for the `+` button branching.
//!   * `SolutionAgentStore::visible_session_count(&id)` for the live AI
//!     session count badge on each tab (excludes ephemeral supervisor
//!     judge sessions so they never tick the badge during an idle wake-up).
//!
//! Re-render triggers (registered in [`SolutionTabStrip::new`]):
//!   * `SolutionStoreEvent` — covers solution add/remove/rename and
//!     pending-add stage transitions.
//!   * `SolutionAgentStoreEvent` — covers session create/close so the
//!     AI badge stays in sync.
//!   * `cx.observe(&multi_workspace)` — covers active-workspace switch
//!     and retained-workspace open/close, since `MultiWorkspace` calls
//!     `cx.notify()` on each of those transitions and `observe` fires
//!     on every notify.

use gpui::{
    Entity, IntoElement, ParentElement, Render, Styled, Subscription, WeakEntity, Window, div, px,
};
use solution_agent::store::{SolutionAgentStore, SolutionAgentStoreEvent};
use solutions::{Solution, SolutionId, SolutionStore, SolutionStoreEvent};
use ui::{IconButton, IconName, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace};

use crate::solution_picker_dropdown::SolutionPickerDropdown;
use crate::solution_tab::{DraggedSolutionTab, SolutionTab};
use crate::window_helpers::is_solution_open_anywhere;

pub struct SolutionTabStrip {
    workspace: WeakEntity<Workspace>,
    multi_workspace: WeakEntity<MultiWorkspace>,
    _subscriptions: Vec<Subscription>,
}

impl SolutionTabStrip {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        multi_workspace: WeakEntity<MultiWorkspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();

        let store = SolutionStore::global(cx);
        subscriptions.push(cx.subscribe(&store, |_, _, _: &SolutionStoreEvent, cx| {
            cx.notify();
        }));

        // Agent store may not be initialised in headless / test contexts;
        // only subscribe when present.
        if let Some(agent_store) = SolutionAgentStore::try_global(cx) {
            subscriptions.push(cx.subscribe(
                &agent_store,
                |_, _, _: &SolutionAgentStoreEvent, cx| {
                    cx.notify();
                },
            ));
        }

        // Re-render whenever the multi-workspace's open list or active
        // workspace changes. `MultiWorkspace::activate` /
        // `retain_active_workspace` / `close_workspace` all call
        // `cx.notify()`, so a plain `observe` is enough — no event types
        // to filter on.
        if let Some(mw) = multi_workspace.upgrade() {
            subscriptions.push(cx.observe(&mw, |_, _, cx| cx.notify()));
        }

        Self {
            workspace,
            multi_workspace,
            _subscriptions: subscriptions,
        }
    }
}

/// Walk a `Workspace`'s worktrees and return the first one that maps to
/// a registered Solution. Mirrors the logic inside
/// `workspace_has_solution` — extracted here so we can build the
/// (id, name, badges) tuple list in a single pass.
fn solution_id_for_workspace(
    workspace: &Entity<Workspace>,
    store: &SolutionStore,
    cx: &App,
) -> Option<SolutionId> {
    let project = workspace.read(cx).project().clone();
    project.read(cx).worktrees(cx).find_map(|tree| {
        store
            .solution_for_path(&tree.read(cx).abs_path())
            .map(|sol| sol.id.clone())
    })
}

impl Render for SolutionTabStrip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(mw) = self.multi_workspace.upgrade() else {
            return h_flex().h_full().into_any_element();
        };

        let store = SolutionStore::global(cx);
        let agent_store = SolutionAgentStore::try_global(cx);

        // Snapshot the data each tab needs in one pass so we don't borrow
        // across mutating callbacks. Each entry is (id, name, is_active,
        // ai_count). `SolutionTab` is a `RenderOnce` that takes the values
        // by move, so we don't need to keep `&Solution` references alive
        // past this map. The clone-in-flight spinner is intentionally NOT
        // surfaced here — it renders on the project tab being cloned, in
        // the project tab strip, not on the owning solution tab.
        let active_workspace = mw.read(cx).workspace().clone();
        let store_read = store.read(cx);
        let active_solution_id = solution_id_for_workspace(&active_workspace, store_read, cx);

        let mut seen_ids: Vec<SolutionId> = Vec::new();
        let mut tabs: Vec<(SolutionId, SharedString, bool, usize)> = Vec::new();
        for ws in mw.read(cx).workspaces() {
            let Some(sol_id) = solution_id_for_workspace(ws, store_read, cx) else {
                continue;
            };
            // A retained workspace and the active workspace can map to
            // the same Solution; avoid duplicating the tab in that case.
            if seen_ids.iter().any(|id| id == &sol_id) {
                continue;
            }
            let Some(sol) = store_read
                .solutions()
                .iter()
                .find(|s: &&Solution| s.id == sol_id)
            else {
                continue;
            };
            let is_active = active_solution_id.as_ref() == Some(&sol_id);
            let ai_count = agent_store
                .as_ref()
                .map(|s| s.read(cx).visible_session_count(&sol_id))
                .unwrap_or(0);
            tabs.push((
                sol_id.clone(),
                SharedString::from(sol.name.clone()),
                is_active,
                ai_count,
            ));
            seen_ids.push(sol_id);
        }

        // Tooltip text reflects whether the picker has anything other
        // than "Create new solution…" to offer. The picker itself
        // handles the empty-list case by rendering a "No closed
        // solutions" hint above the create row, so we don't need to
        // skip the popover.
        let any_closed = store_read
            .solutions()
            .iter()
            .any(|s| !seen_ids.contains(&s.id) && !is_solution_open_anywhere(&s.id, cx));

        let weak_workspace = self.workspace.clone();
        let weak_multi_workspace = self.multi_workspace.clone();
        let plus_button = IconButton::new("solution-tab-strip-plus", IconName::Plus)
            .icon_size(IconSize::Small)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text(if any_closed {
                "Open or create a solution"
            } else {
                "Create new solution"
            }));

        let picker_workspace = self.workspace.clone();
        let picker_mw = self.multi_workspace.clone();
        let plus_popover =
            PopoverMenu::new("solution-tab-strip-plus-popover")
                .trigger(plus_button)
                .menu(move |window, cx| {
                    let picker_workspace = picker_workspace.clone();
                    let picker_mw = picker_mw.clone();
                    Some(cx.new(|cx| {
                        SolutionPickerDropdown::new(picker_workspace, picker_mw, window, cx)
                    }))
                });

        // Trailing drop zone that moves a dragged tab to the very end of the
        // strip. Dropping onto the last tab only lands *after* it when the
        // drag started left of it, and the empty space past the last tab is
        // otherwise inert — so this explicit zone makes "move to end" a
        // reliable, discoverable target. The end slot is the last tab's
        // index (`tab_count - 1`), matching what the last tab's own drop uses.
        let tab_count = tabs.len();
        // Only present while a SOLUTION-TAB drag is in flight — gating on
        // `has_active_drag()` (any drag) made this empty catch area appear on
        // unrelated drags too (e.g. resizing a panel), reading as dead space.
        let is_tab_drag = cx.active_drag_is::<DraggedSolutionTab>();
        let end_drop = (tab_count > 1 && is_tab_drag).then(|| {
            let multi_workspace = weak_multi_workspace.clone();
            let target = tab_count - 1;
            div()
                .id("solution-tab-strip-end-drop")
                .h_full()
                // Fixed-width catch area right after the last tab — NOT
                // `flex_1`, which would stretch to fill the strip and shove
                // the trailing `+` button to the far edge.
                .w(px(40.))
                .drag_over::<DraggedSolutionTab>(|style, _dragged, _window, cx| {
                    style.bg(cx.theme().colors().drop_target_background)
                })
                .on_drop(move |dragged: &DraggedSolutionTab, _window, cx| {
                    let from = dragged.index;
                    multi_workspace
                        .update(cx, |mw, cx| mw.reorder_workspaces(from, target, cx))
                        .log_err();
                })
        });

        h_flex()
            .id("solution-tab-strip")
            .h_full()
            .overflow_x_scroll()
            .children(tabs.into_iter().enumerate().map(
                |(index, (id, name, is_active, ai_count))| {
                    SolutionTab::new(
                        id,
                        name,
                        is_active,
                        ai_count,
                        index,
                        weak_multi_workspace.clone(),
                        weak_workspace.clone(),
                    )
                },
            ))
            .when_some(end_drop, |this, zone| this.child(zone))
            // Trailing `+`, hidden while a solution tab of THIS strip is being
            // dragged (the drop affordances own the trailing space then). A
            // subtle divider separates it from the tabs; the `+` sits in a
            // square cell (side == strip height) snug after the tabs, so its
            // centre is equidistant from the strip's top/bottom and the last tab.
            .when(!is_tab_drag, |this| {
                this.child(
                    div()
                        .w(px(1.))
                        .h(px(16.))
                        .mr(px(2.))
                        .bg(cx.theme().colors().border_variant),
                )
                .child(
                    div()
                        .h_full()
                        .w(px(30.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(plus_popover),
                )
            })
            .into_any_element()
    }
}
