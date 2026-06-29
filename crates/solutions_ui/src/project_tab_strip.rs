//! Horizontal project-tab strip: one `ProjectTab` per member of the
//! *active* solution, plus a trailing `+` button that opens
//! [`AddProjectPicker`] for that solution.
//!
//! Source of truth:
//!   * `MultiWorkspace::workspace()` for the active workspace, whose
//!     first solution-mapped worktree resolves to the active
//!     `SolutionId` via `SolutionStore::solution_for_path` (the same
//!     lookup the solution strip uses to find the active solution).
//!   * `Solution::members` (already in `position` order) for the tab
//!     list, and `SolutionStore::active_member` for the highlight.
//!
//! Re-render triggers (registered in [`ProjectTabStrip::new`]):
//!   * `SolutionStoreEvent` — covers member add/remove/reorder
//!     (`Changed`) and active-member switches (`ActiveMemberChanged`).
//!   * `cx.observe(&multi_workspace)` — covers active-workspace switch,
//!     since `MultiWorkspace` calls `cx.notify()` on that transition.
//!
//! Overflow: a fixed maximum of [`MAX_VISIBLE_TABS`] tabs render inline;
//! any remaining members spill into a trailing `more` `PopoverMenu`
//! whose rows switch the active member on click.

use gpui::{
    Entity, IntoElement, ParentElement, Render, Styled, Subscription, WeakEntity, Window, div, px,
};
use solutions::{
    CatalogId, Solution, SolutionId, SolutionMember, SolutionStore, SolutionStoreEvent,
};
use ui::{ContextMenu, IconButton, IconName, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace};

use crate::AddProjectPicker;
use crate::project_tab::{DraggedProjectTab, PendingProjectTab, ProjectTab, move_to_end};

/// How many project tabs render inline before the rest spill into the
/// `more` popover. A simple fixed cap — the strip lives in the title bar
/// where horizontal space is tight, and pixel-measured overflow isn't
/// worth the complexity here.
const MAX_VISIBLE_TABS: usize = 6;

pub struct ProjectTabStrip {
    multi_workspace: WeakEntity<MultiWorkspace>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectTabStrip {
    pub fn new(
        _workspace: WeakEntity<Workspace>,
        multi_workspace: WeakEntity<MultiWorkspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();

        let store = SolutionStore::global(cx);
        subscriptions.push(cx.subscribe(&store, |_, _, _: &SolutionStoreEvent, cx| {
            cx.notify();
        }));

        if let Some(mw) = multi_workspace.upgrade() {
            subscriptions.push(cx.observe(&mw, |_, _, cx| cx.notify()));
        }

        Self {
            multi_workspace,
            _subscriptions: subscriptions,
        }
    }
}

/// Walk a `Workspace`'s worktrees and return the first one that maps to
/// a registered Solution. Mirrors `solution_id_for_workspace` in the
/// solution tab strip.
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

impl Render for ProjectTabStrip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(mw) = self.multi_workspace.upgrade() else {
            return h_flex().h_full().into_any_element();
        };

        let store = SolutionStore::global(cx);
        let active_workspace = mw.read(cx).workspace().clone();
        let active_solution_id =
            solution_id_for_workspace(&active_workspace, store.read(cx), cx);

        let Some(solution_id) = active_solution_id else {
            // No active solution in this window — nothing to render.
            return h_flex().h_full().into_any_element();
        };

        // Seed a default active member for a freshly-opened solution, then
        // snapshot the (catalog_id, display name) list in member order and
        // the active member for the highlight. Done inside one `update` so
        // the borrow doesn't span the mutating `ensure_active_member` call.
        let (members, active_member): (Vec<(CatalogId, SharedString)>, Option<CatalogId>) = store
            .update(cx, |store, cx| {
                let Some(solution) = store
                    .solutions()
                    .iter()
                    .find(|s: &&Solution| s.id == solution_id)
                    .cloned()
                else {
                    return (Vec::new(), None);
                };
                store.ensure_active_member(&solution.id, &solution.members, cx);
                let active = store.active_member(&solution.id).cloned();
                let entries = solution
                    .members
                    .iter()
                    .map(|member: &SolutionMember| {
                        let name = store
                            .catalog()
                            .iter()
                            .find(|c| c.id == member.catalog_id)
                            .map(|c| c.name.clone())
                            .unwrap_or_else(|| member.catalog_id.0.clone());
                        (member.catalog_id.clone(), SharedString::from(name))
                    })
                    .collect();
                (entries, active)
            });

        let order: Vec<CatalogId> = members.iter().map(|(id, _)| id.clone()).collect();

        // Ghost tabs for in-flight (or just-failed) `add_member` clones that
        // haven't landed as real members yet. Skip any whose catalog id is
        // already a member — that's the brief window between the clone task
        // recording the member and removing the in-flight entry. This is the
        // surface the clone spinner belongs on (the project being cloned),
        // not the owning solution tab.
        let pending_tabs = store
            .read(cx)
            .pending_adds_for(&solution_id)
            .into_iter()
            .filter(|p| !order.contains(&p.catalog_id))
            .map(|p| {
                PendingProjectTab::new(
                    p.catalog_id,
                    SharedString::from(p.catalog_name),
                    SharedString::from(p.stage),
                    p.percent,
                    p.error.map(SharedString::from),
                )
            });

        let (visible, overflow): (&[(CatalogId, SharedString)], &[(CatalogId, SharedString)]) =
            if members.len() > MAX_VISIBLE_TABS {
                members.split_at(MAX_VISIBLE_TABS)
            } else {
                (members.as_slice(), &[])
            };

        let tabs = visible.iter().map(|(catalog_id, name)| {
            let is_active = active_member.as_ref() == Some(catalog_id);
            ProjectTab::new(
                solution_id.clone(),
                catalog_id.clone(),
                name.clone(),
                is_active,
                order.clone(),
            )
        });

        // Trailing `more` popover for the members that didn't fit inline.
        let overflow_popover = (!overflow.is_empty()).then(|| {
            let overflow_entries: Vec<(SolutionId, CatalogId, SharedString)> = overflow
                .iter()
                .map(|(catalog_id, name)| {
                    (solution_id.clone(), catalog_id.clone(), name.clone())
                })
                .collect();
            let more_button = IconButton::new("project-tab-strip-more", IconName::Ellipsis)
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("More projects"));
            PopoverMenu::new("project-tab-strip-more-popover")
                .trigger(more_button)
                .menu(move |window, cx| {
                    let overflow_entries = overflow_entries.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                        for (solution_id, catalog_id, name) in overflow_entries {
                            menu = menu.entry(name.clone(), None, move |_window, cx| {
                                let solution = solution_id.clone();
                                let catalog = catalog_id.clone();
                                SolutionStore::global(cx).update(cx, |store, cx| {
                                    store.set_active_member(solution, catalog, cx);
                                });
                            });
                        }
                        menu
                    }))
                })
        });

        // Trailing `+` button → AddProjectPicker for the active solution.
        let picker_solution_id = solution_id.clone();
        let plus_button = IconButton::new("project-tab-strip-plus", IconName::Plus)
            .icon_size(IconSize::Small)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text("Add project to this solution"));
        let plus_popover = PopoverMenu::new("project-tab-strip-plus-popover")
            .trigger(plus_button)
            .menu(move |window, cx| {
                let solution_id = picker_solution_id.clone();
                Some(cx.new(|cx| AddProjectPicker::new(solution_id, window, cx)))
            });

        // Trailing drop zone: dropping a dragged tab here moves it to the
        // very end of the member order — a position no per-tab drop target
        // can express (each tab inserts the dragged member *before* itself).
        // Only meaningful with at least two members. `flex_1` lets it absorb
        // any slack to the right of the last tab as a generous catch area;
        // `min_w` keeps it hittable even when the tabs already fill the strip.
        // Only present while a tab drag is in flight — otherwise this empty
        // catch area just reads as dead space to the right of the tabs.
        let end_drop = (members.len() > 1 && cx.has_active_drag()).then(|| {
            let solution_id = solution_id.clone();
            let order = order.clone();
            div()
                .id("project-tab-strip-end-drop")
                .h_full()
                // Fixed-width catch area right after the last tab — NOT
                // `flex_1`, which would stretch to fill the strip and shove
                // the trailing `+`/overflow buttons to the far edge.
                .w(px(40.))
                .drag_over::<DraggedProjectTab>(|style, _dragged, _window, cx| {
                    style.bg(cx.theme().colors().drop_target_background)
                })
                .on_drop(move |dragged: &DraggedProjectTab, _window, cx| {
                    let new_order = move_to_end(&order, &dragged.catalog_id);
                    SolutionStore::global(cx)
                        .update(cx, |store, cx| {
                            store.reorder_members(&solution_id, new_order, cx)
                        })
                        .log_err();
                })
        });

        h_flex()
            .id("project-tab-strip")
            .h_full()
            .overflow_x_scroll()
            .children(tabs)
            .children(pending_tabs)
            .when_some(end_drop, |this, zone| this.child(zone))
            .when_some(overflow_popover, |this, popover| {
                this.child(div().px_1().child(popover))
            })
            // Delimiter separating the tab zone from the trailing `+`.
            .child(
                div()
                    .w(px(1.))
                    .h(px(16.))
                    .mx_1()
                    .bg(cx.theme().colors().border_variant),
            )
            .child(div().px_1().child(plus_popover))
            .into_any_element()
    }
}
