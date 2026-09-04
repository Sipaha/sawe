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
//! [`PendingProjectTab`] ghost — the project being cloned is exactly
//! the member-scoped surface it belongs on (it used to wrongly live on
//! the owning solution tab). That ghost has its own right-click menu
//! (cancel while cloning; retry / edit / dismiss / drop-from-catalog
//! once it failed), which is the only way out of a failed add.

use gpui::{
    App, ClickEvent, Context, ElementId, Hsla, IntoElement, Render, RenderOnce, SharedString,
    Window, div, px,
};
use solutions::{CatalogId, MemberId, SolutionId, SolutionStore};
use std::cell::RefCell;
use ui::{ContextMenu, Indicator, Tooltip, prelude::*, right_click_menu};
use util::ResultExt as _;

use crate::actions::{EditCatalogProject, RemoveMember, RenameMember};
use crate::solution_tab::dot_color_for_id;

/// Horizontal padding on each side of a tab's content (`px_3`).
pub(crate) const TAB_PADDING_X: gpui::Pixels = px(12.0);
/// Side of the square colour dot that leads every tab.
pub(crate) const TAB_DOT_SIZE: gpui::Pixels = px(8.0);
/// Gap between the colour dot and the label (`gap_2`).
pub(crate) const TAB_GAP: gpui::Pixels = px(8.0);
/// Narrowest a tab may render, so a one-character project name still gives
/// the pointer something to hit.
pub(crate) const TAB_MIN_WIDTH: gpui::Pixels = px(80.0);
/// Widest a tab may render; past this the label truncates with an ellipsis.
pub(crate) const TAB_MAX_WIDTH: gpui::Pixels = px(200.0);

/// The width a tab will lay out at, given the shaped width of its label.
///
/// This is the strip's width budget talking to the tab's own styling, so both
/// sides read the constants above rather than repeating literals — a tab whose
/// padding changed without this following would silently make the budget lie.
/// `project_tab_strip::tabs_lay_out_at_their_predicted_width` pins the two
/// together against the geometry a real frame paints.
pub(crate) fn tab_width_for_label(label_width: gpui::Pixels) -> gpui::Pixels {
    let natural = TAB_PADDING_X + TAB_DOT_SIZE + TAB_GAP + label_width + TAB_PADDING_X;
    natural.clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH)
}

/// What a [`PendingProjectTab`] adds on top of a plain tab: one more `TAB_GAP`
/// and the trailing spinner / warning glyph (`IconSize::XSmall`, 12px).
pub(crate) const PENDING_TAB_TRAILING_WIDTH: gpui::Pixels = px(20.0);

/// Paint selector for a real (landed) project tab. Named per member so a test
/// can ask which members actually made it onto the strip rather than trusting
/// the split the strip computed — see
/// `docs/findings/2026-09-02-paint-tests-with-debug-bounds.md`.
pub(crate) fn project_tab_selector(member_id: MemberId) -> String {
    format!("PROJECT-TAB-{}", member_id.0)
}

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

impl DraggedProjectTab {
    /// EXPERIMENT (drag-out-of-overflow-menu): lets the overflow menu build
    /// the same drag payload a real tab does.
    pub(crate) fn new(member_id: MemberId, name: SharedString) -> Self {
        Self {
            member_id,
            name,
            dot: dot_color_for_id(member_id.0),
        }
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
            .debug_selector({
                let member_id = self.member_id;
                move || project_tab_selector(member_id)
            })
            .h_full()
            .px(TAB_PADDING_X)
            .gap(TAB_GAP)
            .min_w(TAB_MIN_WIDTH)
            .max_w(TAB_MAX_WIDTH)
            .items_center()
            .when_some(active_bg, |this, bg| this.bg(bg))
            .border_b_2()
            .border_color(if self.is_active {
                active_border
            } else {
                inactive_border
            })
            .cursor_pointer()
            .child(
                div()
                    .w(TAB_DOT_SIZE)
                    .h(TAB_DOT_SIZE)
                    .rounded_full()
                    .bg(dot),
            )
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
/// real member yet. It shows the project being cloned with a spinning
/// progress indicator while in flight, or a warning glyph once the clone
/// failed. It renders in the project tab strip — the member-scoped surface
/// — rather than on the owning solution tab, so the spinner points at the
/// project actually being cloned.
///
/// Left click is inert (there is no member to activate yet), but right
/// click opens a menu: `Cancel Clone` while in flight, and the full escape
/// hatch once it failed. Before that menu existed a failed add was a dead
/// end — the ghost tab was completely non-interactive, so a mistyped remote
/// URL left a warning-triangle tab that could only be cleared by closing and
/// reopening the whole Solution.
///
/// Right click rather than an always-visible `×`: this fork has been moving
/// tab affordances INTO the context menu (the AI session tabs lost their
/// close cross for exactly that menu), and an error tab is not special
/// enough to reverse that — it is transient, it already draws a warning
/// glyph, and the gesture the user reaches for on a stuck tab is the one
/// every other tab here answers. The discoverability gap is closed in the
/// tooltip instead, which now says so.
#[derive(IntoElement)]
pub struct PendingProjectTab {
    solution_id: SolutionId,
    catalog_id: CatalogId,
    name: SharedString,
    /// Human-readable clone stage (e.g. `cloning`, `45%`) surfaced as a
    /// tooltip so a slow clone is legible without widening the tab.
    stage: SharedString,
    percent: Option<u8>,
    /// `Some(_)` once the add failed and is waiting on the user to retry
    /// or dismiss it; flips the spinner to an error glyph.
    error: Option<SharedString>,
    /// Whether the backing catalog project exists and no Solution has a
    /// member cloned from it — i.e. whether "Remove Project from Catalog"
    /// would actually be a cleanup rather than a surprise. See
    /// [`SolutionStore::catalog_project_is_unreferenced`].
    catalog_removable: bool,
}

impl PendingProjectTab {
    pub fn new(
        solution_id: SolutionId,
        catalog_id: CatalogId,
        name: SharedString,
        stage: SharedString,
        percent: Option<u8>,
        error: Option<SharedString>,
        catalog_removable: bool,
    ) -> Self {
        Self {
            solution_id,
            catalog_id,
            name,
            stage,
            percent,
            error,
            catalog_removable,
        }
    }

    /// The label this ghost tab paints, so the strip can budget its width
    /// before consuming the tab into the element tree.
    pub(crate) fn name(&self) -> &SharedString {
        &self.name
    }
}

/// Paint selector for a pending tab whose clone FAILED. Explicit rather
/// than relying on the glyph, because `Icon` (unlike `IconButton`) registers
/// no selector of its own and the two states differ only by which icon they
/// draw — see `docs/findings/2026-09-02-paint-tests-with-debug-bounds.md`.
pub(crate) const FAILED_TAB_SELECTOR: &str = "PENDING-PROJECT-TAB-FAILED";
/// Paint selector for a pending tab whose clone is still running.
pub(crate) const CLONING_TAB_SELECTOR: &str = "PENDING-PROJECT-TAB-CLONING";

impl RenderOnce for PendingProjectTab {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let dot = dot_color_for_id(self.catalog_id.0);
        let row_id = ElementId::from(SharedString::from(format!(
            "project-tab-pending-{}",
            self.catalog_id.0
        )));
        let menu_id = ElementId::from(SharedString::from(format!(
            "project-tab-pending-menu-{}",
            self.catalog_id.0
        )));
        let failed = self.error.is_some();
        let selector = if failed {
            FAILED_TAB_SELECTOR
        } else {
            CLONING_TAB_SELECTOR
        };
        let tooltip_title: SharedString = match &self.error {
            Some(err) => SharedString::from(format!("Clone failed: {err}")),
            None => match self.percent {
                Some(p) => SharedString::from(format!("{} — {p}%", self.stage)),
                None => self.stage.clone(),
            },
        };
        let tooltip_meta: SharedString = if failed {
            "Right-click to retry, edit or dismiss".into()
        } else {
            "Right-click to cancel".into()
        };
        let trailing = if failed {
            Icon::new(IconName::Warning)
                .size(IconSize::XSmall)
                .color(Color::Error)
                .into_any_element()
        } else {
            Indicator::icon(Icon::new(IconName::ArrowCircle))
                .color(Color::Accent)
                .into_any_element()
        };

        let solution_id = self.solution_id;
        let catalog_id = self.catalog_id;
        let catalog_removable = self.catalog_removable;

        let row = div()
            .id(row_id)
            .debug_selector(move || selector.to_string())
            .child(
                h_flex()
                    .h_full()
                    .px(TAB_PADDING_X)
                    .gap(TAB_GAP)
                    .min_w(TAB_MIN_WIDTH)
                    .max_w(TAB_MAX_WIDTH)
                    .items_center()
                    .border_b_2()
                    .border_color(cx.theme().colors().border_transparent)
                    .child(
                        div()
                            .w(TAB_DOT_SIZE)
                            .h(TAB_DOT_SIZE)
                            .rounded_full()
                            .bg(dot),
                    )
                    .child(Label::new(self.name).truncate().color(Color::Muted))
                    .child(trailing),
            )
            .tooltip(move |_window, cx| {
                Tooltip::with_meta(tooltip_title.clone(), None, tooltip_meta.clone(), cx)
            });

        // Same `RefCell` take-once dance as `ProjectTab` above:
        // `right_click_menu` wants an `Fn` trigger, the row can only be
        // consumed once.
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
                    if !failed {
                        return menu.entry("Cancel Clone", None, move |_window, cx| {
                            SolutionStore::global(cx).update(cx, |store, cx| {
                                store.cancel_add_member(solution_id, catalog_id, cx);
                            });
                        });
                    }
                    menu.entry("Retry Clone", None, move |_window, cx| {
                        let cache_root = solutions::default_cache_root();
                        let task = SolutionStore::global(cx).update(cx, |store, cx| {
                            store.retry_failed_add(solution_id, catalog_id, cache_root, cx)
                        });
                        task.detach_and_log_err(cx);
                    })
                    // The overwhelmingly common cause of a failed first clone
                    // is a wrong remote URL, so the fix-it path is one row
                    // below the retry that will use it.
                    .action(
                        "Edit Project…",
                        Box::new(EditCatalogProject { id: catalog_id.0 }),
                    )
                    .separator()
                    // Dismiss keeps the catalog entry: the project may be
                    // perfectly fine and just unreachable right now.
                    .entry("Dismiss", None, move |_window, cx| {
                        SolutionStore::global(cx).update(cx, |store, cx| {
                            store.clear_failed_add(solution_id, catalog_id, cx);
                        });
                    })
                    // …and when the catalog row exists only because of this
                    // failed add, offer to take it away in the same gesture.
                    // Without this the typo'd project stays in the catalog
                    // forever and is offered again by the add picker, with no
                    // UI anywhere that can delete it.
                    .when(catalog_removable, |menu| {
                        menu.entry("Remove Project from Catalog", None, move |_window, cx| {
                            SolutionStore::global(cx).update(cx, |store, cx| {
                                store.clear_failed_add(solution_id, catalog_id, cx);
                                store.remove_catalog_project(catalog_id, cx).log_err();
                            });
                        })
                    })
                })
            })
            .into_any_element()
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
        assert_eq!(
            dot_color_for_str("ecos-base"),
            dot_color_for_str("ecos-base")
        );
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

#[cfg(test)]
mod pending_tab_paint_tests {
    use super::*;
    use gpui::{Entity, Modifiers, MouseButton, TestAppContext, VisualTestContext};

    /// Hosts a single [`PendingProjectTab`] so the paint assertions below see
    /// exactly the element the strip renders, without standing up a whole
    /// workspace + MultiWorkspace to get at the strip.
    struct PendingTabHost {
        error: Option<SharedString>,
        catalog_removable: bool,
    }

    impl Render for PendingTabHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            PendingProjectTab::new(
                SolutionId(1),
                CatalogId(7),
                "citeck-hazelcast".into(),
                "failed".into(),
                None,
                self.error.clone(),
                self.catalog_removable,
            )
        }
    }

    fn host<'a>(
        error: Option<&str>,
        catalog_removable: bool,
        cx: &'a mut TestAppContext,
    ) -> (Entity<PendingTabHost>, &'a mut VisualTestContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let error = error.map(SharedString::from);
        let (view, cx) = cx.add_window_view(|_window, _cx| PendingTabHost {
            error,
            catalog_removable,
        });
        cx.run_until_parked();
        (view, cx)
    }

    /// Right-click the pending tab. `right_click_menu` only opens when its
    /// hitbox is hovered, so the pointer has to be rested on the tab (and a
    /// frame drawn) before the button goes down — a bare mouse-down opens
    /// nothing and the assertion that follows would be a false negative.
    fn right_click(selector: &'static str, cx: &mut VisualTestContext) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} was never painted"));
        cx.simulate_mouse_move(bounds.center(), None, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_down(bounds.center(), MouseButton::Right, Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn a_failed_add_paints_a_failed_tab_whose_menu_is_the_way_out(cx: &mut TestAppContext) {
        let (_view, cx) = host(Some("redirect: .../users/sign_in (exit 128)"), true, cx);

        assert!(
            cx.debug_bounds(FAILED_TAB_SELECTOR).is_some(),
            "a pending add carrying an error must paint as the failed tab"
        );
        assert!(
            cx.debug_bounds(CLONING_TAB_SELECTOR).is_none(),
            "…and must not also paint as a still-cloning tab"
        );

        right_click(FAILED_TAB_SELECTOR, cx);

        assert!(
            cx.debug_bounds("MENU_ITEM-Retry Clone").is_some(),
            "right-clicking a failed tab must offer a retry"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Edit Project…").is_some(),
            "…and the fix-the-URL path the retry will use"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Dismiss").is_some(),
            "…and a way to drop the failed row and keep the catalog entry"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Remove Project from Catalog")
                .is_some(),
            "…and, for a catalog row nothing references, a full cleanup"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Cancel Clone").is_none(),
            "there is nothing left to cancel once the clone has failed"
        );
    }

    #[gpui::test]
    async fn a_clone_still_running_paints_a_cloning_tab_that_can_only_be_cancelled(
        cx: &mut TestAppContext,
    ) {
        let (_view, cx) = host(None, true, cx);

        assert!(
            cx.debug_bounds(CLONING_TAB_SELECTOR).is_some(),
            "a pending add with no error must paint as the cloning tab"
        );
        assert!(
            cx.debug_bounds(FAILED_TAB_SELECTOR).is_none(),
            "…and must not paint the failure variant"
        );

        right_click(CLONING_TAB_SELECTOR, cx);

        assert!(
            cx.debug_bounds("MENU_ITEM-Cancel Clone").is_some(),
            "an in-flight clone must be cancellable from the tab"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Retry Clone").is_none(),
            "retry is meaningless while the first attempt is still running"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Remove Project from Catalog")
                .is_none(),
            "and the catalog must not be editable out from under a running clone"
        );
    }

    #[gpui::test]
    async fn a_referenced_catalog_project_is_not_offered_for_deletion(cx: &mut TestAppContext) {
        let (_view, cx) = host(Some("clone failed"), false, cx);

        right_click(FAILED_TAB_SELECTOR, cx);

        assert!(
            cx.debug_bounds("MENU_ITEM-Dismiss").is_some(),
            "the failed row is still dismissable"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Remove Project from Catalog")
                .is_none(),
            "a catalog project another Solution already cloned must not be offered for deletion"
        );
    }
}
