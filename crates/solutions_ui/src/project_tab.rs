//! One project (catalog member) tab in the project tab strip.
//!
//! Click → make this member the solution-wide active project via
//! [`SolutionStore::set_active_member`]. Drag-to-reorder mirrors
//! [`crate::solution_tab::SolutionTab`] but moves the member within
//! `solution.members` through [`SolutionStore::reorder_members`].
//!
//! Visuals: deterministic colour dot derived from the `MemberId`
//! (shared FNV-1a helper with the solution tabs), the member's name
//! (truncated), and an active-tab highlight. The clone-progress
//! spinner for an in-flight `add_member` renders here too, as a
//! non-interactive [`PendingProjectTab`] ghost — the project being
//! cloned is exactly the member-scoped surface it belongs on (it used
//! to wrongly live on the owning solution tab).

use gpui::{
    App, ClickEvent, Context, ElementId, Hsla, IntoElement, Render, RenderOnce, SharedString,
    Window, div, px,
};
use solutions::{CatalogId, MemberId, SolutionId, SolutionStore};
use std::cell::RefCell;
use ui::{ContextMenu, Indicator, Tooltip, prelude::*, right_click_menu};
use util::ResultExt as _;

use crate::actions::{RemoveMember, RenameMember};
use crate::solution_tab::dot_color_for_id;

#[derive(IntoElement)]
pub struct ProjectTab {
    solution_id: SolutionId,
    member_id: MemberId,
    name: SharedString,
    is_active: bool,
    /// Full member order (member ids) at render time. The drop handler
    /// rebuilds this list with the dragged member moved to the drop
    /// target's slot and hands the result to
    /// [`SolutionStore::reorder_members`], which takes the whole new
    /// order rather than a (from, to) pair.
    order: Vec<MemberId>,
}

/// Drag payload for reordering project tabs. Carries the dragged
/// member's `MemberId` (the drop target uses it to recompute the
/// member order) plus the colour-dot + name so the drag preview looks
/// like the tab being dragged.
#[derive(Clone)]
pub struct DraggedProjectTab {
    pub(crate) member_id: MemberId,
    name: SharedString,
    dot: Hsla,
}

impl Render for DraggedProjectTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h_8()
            .items_center()
            .gap_2()
            .px_3()
            .bg(cx.theme().colors().tab_active_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(div().w(px(8.0)).h(px(8.0)).rounded_full().bg(self.dot))
            .child(Label::new(self.name.clone()))
    }
}

impl ProjectTab {
    pub fn new(
        solution_id: SolutionId,
        member_id: MemberId,
        name: SharedString,
        is_active: bool,
        order: Vec<MemberId>,
    ) -> Self {
        Self {
            solution_id,
            member_id,
            name,
            is_active,
            order,
        }
    }
}

impl RenderOnce for ProjectTab {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let dot = dot_color_for_id(self.member_id.0);
        // Per-item ElementId derived from the member id so clicks/drags
        // route to the right tab (a constant literal reused per list item
        // would misroute).
        let row_id = ElementId::from(SharedString::from(format!(
            "project-tab-{}",
            self.member_id.0
        )));
        let active_bg = if self.is_active {
            Some(cx.theme().colors().tab_active_background)
        } else {
            None
        };
        let active_border = cx.theme().colors().border_focused;
        let inactive_border = cx.theme().colors().border_transparent;

        let solution_for_click = self.solution_id;
        let member_for_click = self.member_id;
        // Captured up front (the chain below partially moves `self.order`).
        let menu_id = ElementId::from(SharedString::from(format!(
            "project-tab-menu-{}",
            self.member_id.0
        )));
        let member_for_menu = self.member_id.0;

        let row = h_flex()
            .id(row_id)
            .h_full()
            .px_3()
            .gap_2()
            .min_w(px(80.0))
            .max_w(px(200.0))
            .items_center()
            .when_some(active_bg, |this, bg| this.bg(bg))
            .border_b_2()
            .border_color(if self.is_active {
                active_border
            } else {
                inactive_border
            })
            .cursor_pointer()
            .child(div().w(px(8.0)).h(px(8.0)).rounded_full().bg(dot))
            .child(
                Label::new(self.name.clone())
                    .truncate()
                    .color(if self.is_active {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .on_click({
                move |_event: &ClickEvent, _window, cx| {
                    SolutionStore::global(cx).update(cx, |store, cx| {
                        store.set_active_member(solution_for_click, member_for_click, cx);
                    });
                }
            })
            // Drag-and-drop reorder. `on_drag` only fires once the pointer
            // crosses GPUI's movement threshold, so a plain click still
            // reaches `on_click` above and switches the active member.
            .on_drag(
                DraggedProjectTab {
                    member_id: self.member_id,
                    name: self.name.clone(),
                    dot,
                },
                |dragged, _offset, _window, cx| cx.new(|_| dragged.clone()),
            )
            .drag_over::<DraggedProjectTab>(|style, _dragged, _window, cx| {
                style.bg(cx.theme().colors().drop_target_background)
            })
            .on_drop({
                let solution_id = self.solution_id;
                let target = self.member_id;
                let order = self.order;
                move |dragged: &DraggedProjectTab, _window, cx| {
                    let new_order = reorder_to(&order, dragged.member_id, target);
                    SolutionStore::global(cx)
                        .update(cx, |store, cx| {
                            store.reorder_members(solution_id, new_order, cx)
                        })
                        .log_err();
                }
            });

        // Right-click menu. At minimum a destructive "remove from solution"
        // entry, mirroring the solution tab's menu. `RemoveMember` opens the
        // confirmation modal that, on confirm, calls
        // `SolutionStore::remove_member` and rm-rfs the member's folder — the
        // handler is registered as a workspace action in `solutions_ui`. The
        // `RefCell` take-once dance matches `solution_tab`: `right_click_menu`
        // wants an `Fn` trigger but the row element can only be consumed once.
        let row_cell = RefCell::new(Some(row.into_any_element()));
        right_click_menu(menu_id)
            .trigger(move |_, _, _| {
                row_cell
                    .borrow_mut()
                    .take()
                    .unwrap_or_else(|| div().into_any_element())
            })
            .menu(move |window, cx| {
                ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.action(
                        "Rename…",
                        Box::new(RenameMember {
                            member_id: member_for_menu,
                        }),
                    )
                    .separator()
                    .action(
                        "Remove from Solution…",
                        Box::new(RemoveMember {
                            member_id: member_for_menu,
                        }),
                    )
                })
            })
            .into_any_element()
    }
}

/// A ghost project tab for an `add_member` clone that hasn't landed as a
/// real member yet. Non-interactive (no click / drag / context menu): it
/// only shows the project being cloned with a spinning progress indicator
/// while in flight, or an error icon if the clone failed. It renders in
/// the project tab strip — the member-scoped surface — rather than on the
/// owning solution tab, so the spinner points at the project actually
/// being cloned.
#[derive(IntoElement)]
pub struct PendingProjectTab {
    catalog_id: CatalogId,
    name: SharedString,
    /// Human-readable clone stage (e.g. `cloning`, `45%`) surfaced as a
    /// tooltip so a slow clone is legible without widening the tab.
    stage: SharedString,
    percent: Option<u8>,
    /// `Some(_)` once the add failed and is waiting on the user to retry
    /// or dismiss it; flips the spinner to an error glyph.
    error: Option<SharedString>,
}

impl PendingProjectTab {
    pub fn new(
        catalog_id: CatalogId,
        name: SharedString,
        stage: SharedString,
        percent: Option<u8>,
        error: Option<SharedString>,
    ) -> Self {
        Self {
            catalog_id,
            name,
            stage,
            percent,
            error,
        }
    }
}

impl RenderOnce for PendingProjectTab {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let dot = dot_color_for_id(self.catalog_id.0);
        let row_id = ElementId::from(SharedString::from(format!(
            "project-tab-pending-{}",
            self.catalog_id.0
        )));
        let tooltip_text: SharedString = match &self.error {
            Some(err) => SharedString::from(format!("Clone failed: {err}")),
            None => match self.percent {
                Some(p) => SharedString::from(format!("{} — {p}%", self.stage)),
                None => self.stage.clone(),
            },
        };
        let trailing = if self.error.is_some() {
            Icon::new(IconName::Warning)
                .size(IconSize::XSmall)
                .color(Color::Error)
                .into_any_element()
        } else {
            Indicator::icon(Icon::new(IconName::ArrowCircle))
                .color(Color::Accent)
                .into_any_element()
        };

        div()
            .id(row_id)
            .child(
                h_flex()
                    .h_full()
                    .px_3()
                    .gap_2()
                    .min_w(px(80.0))
                    .max_w(px(200.0))
                    .items_center()
                    .border_b_2()
                    .border_color(cx.theme().colors().border_transparent)
                    .child(div().w(px(8.0)).h(px(8.0)).rounded_full().bg(dot))
                    .child(Label::new(self.name).truncate().color(Color::Muted))
                    .child(trailing),
            )
            .tooltip(Tooltip::text(tooltip_text))
    }
}

/// Move `from` to the very end of the order, preserving the relative
/// order of the remaining members. Used by the trailing drop zone in the
/// strip so a tab can be dropped past the last tab to become last — a
/// position no per-tab drop target can express (each tab inserts *before*
/// itself). Returns the original order unchanged when `from` is missing.
pub(crate) fn move_to_end(order: &[MemberId], from: MemberId) -> Vec<MemberId> {
    if !order.contains(&from) {
        return order.to_vec();
    }
    let mut remaining: Vec<MemberId> = order.iter().copied().filter(|m| *m != from).collect();
    remaining.push(from);
    remaining
}

/// Move `from` so it lands at the slot currently occupied by `target`,
/// preserving the order of the remaining members. Returns the original
/// order unchanged when either id is missing.
fn reorder_to(order: &[MemberId], from: MemberId, target: MemberId) -> Vec<MemberId> {
    if from == target || !order.contains(&from) || !order.contains(&target) {
        return order.to_vec();
    }
    let mut remaining: Vec<MemberId> = order.iter().copied().filter(|m| *m != from).collect();
    let insert_at = remaining
        .iter()
        .position(|m| *m == target)
        .unwrap_or(remaining.len());
    remaining.insert(insert_at, from);
    remaining
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solution_tab::dot_color_for_str;

    fn id(n: i64) -> MemberId {
        MemberId(n)
    }

    #[test]
    fn dot_color_for_str_is_stable() {
        assert_eq!(dot_color_for_str("ecos-base"), dot_color_for_str("ecos-base"));
    }

    #[test]
    fn reorder_moves_member_to_target_slot() {
        let order = vec![id(1), id(2), id(3)];
        assert_eq!(reorder_to(&order, id(3), id(1)), vec![id(3), id(1), id(2)]);
        assert_eq!(reorder_to(&order, id(1), id(3)), vec![id(2), id(1), id(3)]);
    }

    #[test]
    fn reorder_is_noop_for_unknown_or_same_ids() {
        let order = vec![id(1), id(2)];
        assert_eq!(reorder_to(&order, id(1), id(1)), order);
        assert_eq!(reorder_to(&order, id(99), id(1)), order);
    }

    #[test]
    fn move_to_end_appends_dragged_member() {
        let order = vec![id(1), id(2), id(3)];
        // Front tab to the end.
        assert_eq!(move_to_end(&order, id(1)), vec![id(2), id(3), id(1)]);
        // Middle tab to the end.
        assert_eq!(move_to_end(&order, id(2)), vec![id(1), id(3), id(2)]);
        // Last tab to the end is a no-op (order unchanged).
        assert_eq!(move_to_end(&order, id(3)), order);
        // Unknown id is a no-op.
        assert_eq!(move_to_end(&order, id(99)), order);
    }
}
