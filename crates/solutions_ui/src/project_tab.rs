//! One project (catalog member) tab in the project tab strip.
//!
//! Click → make this member the solution-wide active project via
//! [`SolutionStore::set_active_member`]. Drag-to-reorder mirrors
//! [`crate::solution_tab::SolutionTab`] but moves the member within
//! `solution.members` through [`SolutionStore::reorder_members`].
//!
//! Visuals: deterministic colour dot derived from the `CatalogId`
//! (shared FNV-1a helper with the solution tabs), the catalog project
//! name (truncated), and an active-tab highlight. Unlike the solution
//! tab there is no AI-session badge or clone-in-flight spinner — those
//! are solution-scoped, not member-scoped.

use gpui::{
    App, ClickEvent, Context, ElementId, Hsla, IntoElement, Render, RenderOnce, SharedString,
    Window, div, px,
};
use solutions::{CatalogId, SolutionId, SolutionStore};
use ui::prelude::*;
use util::ResultExt as _;

use crate::solution_tab::dot_color_for_str;

#[derive(IntoElement)]
pub struct ProjectTab {
    solution_id: SolutionId,
    catalog_id: CatalogId,
    name: SharedString,
    is_active: bool,
    /// Full member order (catalog ids) at render time. The drop handler
    /// rebuilds this list with the dragged member moved to the drop
    /// target's slot and hands the result to
    /// [`SolutionStore::reorder_members`], which takes the whole new
    /// order rather than a (from, to) pair.
    order: Vec<CatalogId>,
}

/// Drag payload for reordering project tabs. Carries the dragged
/// member's `CatalogId` (the drop target uses it to recompute the
/// member order) plus the colour-dot + name so the drag preview looks
/// like the tab being dragged.
#[derive(Clone)]
pub struct DraggedProjectTab {
    pub(crate) catalog_id: CatalogId,
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
        catalog_id: CatalogId,
        name: SharedString,
        is_active: bool,
        order: Vec<CatalogId>,
    ) -> Self {
        Self {
            solution_id,
            catalog_id,
            name,
            is_active,
            order,
        }
    }
}

impl RenderOnce for ProjectTab {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let dot = dot_color_for_str(self.catalog_id.0.as_str());
        // Per-item ElementId derived from the catalog id so clicks/drags
        // route to the right tab (a constant literal reused per list item
        // would misroute).
        let row_id = ElementId::from(SharedString::from(format!(
            "project-tab-{}",
            self.catalog_id.0
        )));
        let active_bg = if self.is_active {
            Some(cx.theme().colors().tab_active_background)
        } else {
            None
        };
        let active_border = cx.theme().colors().border_focused;
        let inactive_border = cx.theme().colors().border_transparent;

        let solution_for_click = self.solution_id.clone();
        let catalog_for_click = self.catalog_id.clone();

        h_flex()
            .id(row_id)
            .h_full()
            .px_3()
            .gap_2()
            .min_w(px(120.0))
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
                    let solution = solution_for_click.clone();
                    let catalog = catalog_for_click.clone();
                    SolutionStore::global(cx).update(cx, |store, cx| {
                        store.set_active_member(solution, catalog, cx);
                    });
                }
            })
            // Drag-and-drop reorder. `on_drag` only fires once the pointer
            // crosses GPUI's movement threshold, so a plain click still
            // reaches `on_click` above and switches the active member.
            .on_drag(
                DraggedProjectTab {
                    catalog_id: self.catalog_id.clone(),
                    name: self.name.clone(),
                    dot,
                },
                |dragged, _offset, _window, cx| cx.new(|_| dragged.clone()),
            )
            .drag_over::<DraggedProjectTab>(|style, _dragged, _window, cx| {
                style.bg(cx.theme().colors().drop_target_background)
            })
            .on_drop({
                let solution_id = self.solution_id.clone();
                let target = self.catalog_id.clone();
                let order = self.order;
                move |dragged: &DraggedProjectTab, _window, cx| {
                    let new_order = reorder_to(&order, &dragged.catalog_id, &target);
                    SolutionStore::global(cx)
                        .update(cx, |store, cx| {
                            store.reorder_members(&solution_id, new_order, cx)
                        })
                        .log_err();
                }
            })
    }
}

/// Move `from` to the very end of the order, preserving the relative
/// order of the remaining members. Used by the trailing drop zone in the
/// strip so a tab can be dropped past the last tab to become last — a
/// position no per-tab drop target can express (each tab inserts *before*
/// itself). Returns the original order unchanged when `from` is missing.
pub(crate) fn move_to_end(order: &[CatalogId], from: &CatalogId) -> Vec<CatalogId> {
    if !order.contains(from) {
        return order.to_vec();
    }
    let mut remaining: Vec<CatalogId> = order.iter().filter(|c| *c != from).cloned().collect();
    remaining.push(from.clone());
    remaining
}

/// Move `from` so it lands at the slot currently occupied by `target`,
/// preserving the order of the remaining members. Returns the original
/// order unchanged when either id is missing.
fn reorder_to(order: &[CatalogId], from: &CatalogId, target: &CatalogId) -> Vec<CatalogId> {
    if from == target || !order.contains(from) || !order.contains(target) {
        return order.to_vec();
    }
    let mut remaining: Vec<CatalogId> = order.iter().filter(|c| *c != from).cloned().collect();
    let insert_at = remaining
        .iter()
        .position(|c| c == target)
        .unwrap_or(remaining.len());
    remaining.insert(insert_at, from.clone());
    remaining
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solution_tab::dot_color_for_str;

    fn id(s: &str) -> CatalogId {
        CatalogId(s.to_string())
    }

    #[test]
    fn dot_color_for_str_is_stable() {
        assert_eq!(dot_color_for_str("ecos-base"), dot_color_for_str("ecos-base"));
    }

    #[test]
    fn reorder_moves_member_to_target_slot() {
        let order = vec![id("a"), id("b"), id("c")];
        assert_eq!(
            reorder_to(&order, &id("c"), &id("a")),
            vec![id("c"), id("a"), id("b")]
        );
        assert_eq!(
            reorder_to(&order, &id("a"), &id("c")),
            vec![id("b"), id("a"), id("c")]
        );
    }

    #[test]
    fn reorder_is_noop_for_unknown_or_same_ids() {
        let order = vec![id("a"), id("b")];
        assert_eq!(reorder_to(&order, &id("a"), &id("a")), order);
        assert_eq!(reorder_to(&order, &id("z"), &id("a")), order);
    }

    #[test]
    fn move_to_end_appends_dragged_member() {
        let order = vec![id("a"), id("b"), id("c")];
        // Front tab to the end.
        assert_eq!(
            move_to_end(&order, &id("a")),
            vec![id("b"), id("c"), id("a")]
        );
        // Middle tab to the end.
        assert_eq!(
            move_to_end(&order, &id("b")),
            vec![id("a"), id("c"), id("b")]
        );
        // Last tab to the end is a no-op (order unchanged).
        assert_eq!(move_to_end(&order, &id("c")), order);
        // Unknown id is a no-op.
        assert_eq!(move_to_end(&order, &id("z")), order);
    }
}
