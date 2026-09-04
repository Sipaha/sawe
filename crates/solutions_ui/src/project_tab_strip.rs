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
//! Overflow: the painted tabs are the LEADING members of the stored order,
//! as many as the strip's MEASURED width can hold; the rest spill into a
//! trailing `more` `PopoverMenu` whose rows can be DRAGGED out onto the strip
//! to reorder.
//!
//! The split is a function of exactly two things — the stored order and the
//! available width — and of nothing else. In particular the ACTIVE member has
//! no influence on it: the strip is the arrangement the user built, so the
//! first tabs are the projects they put first and the `…` holds the ones they
//! did not, and neither moves on its own. Two earlier attempts got this wrong,
//! both by making a selection change the strip: `cc05f6ef6d` promoted a picked
//! project to the head of the member order, and `f3ef02f0f2` reserved the
//! active member's width out of the budget, which pushed a tab at the fold
//! into the `…` instead. Both are spontaneous jumps; neither is here now.
//!
//! Clicking a row of the `…` therefore only ACTIVATES that project and leaves
//! the strip exactly as it was. Because that means the active project can be
//! invisible, the feedback moves onto the `…` BUTTON, which wears the same
//! accent marker the fork uses elsewhere for "the thing you are on is in
//! here" — and the menu's own check mark, unreachable while the reservation
//! existed, is live again.
//!
//! The strip used to cap the visible tabs at a fixed count (six) regardless
//! of how wide the window was, which on a 1920px window left ~790px — 41% of
//! the row — empty to the right of the `…` while half the projects sat
//! hidden behind it. The budget below replaces that count: `available_width`
//! is measured from a `canvas` covering the strip's own box, each tab's
//! natural width is derived from its shaped label via
//! `project_tab::tab_width_for_label`, and tabs are taken greedily until the
//! budget (minus the trailing controls) runs out.

use gpui::{
    Bounds, Entity, IntoElement, ParentElement, Pixels, Render, Styled, Subscription, TextRun,
    WeakEntity, Window, canvas, div, px,
};
use solutions::{
    MemberId, Solution, SolutionId, SolutionMember, SolutionStore, SolutionStoreEvent,
};
use ui::{ContextMenu, IconButton, IconName, Indicator, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace};

use crate::AddProjectPicker;
use crate::project_tab::{
    DraggedProjectTab, PENDING_TAB_TRAILING_WIDTH, PendingProjectTab, ProjectTab, move_to_end,
    tab_width_for_label,
};

/// How many tabs render before the strip has ever been measured — the very
/// first frame, where `available_width` is still `None`. Deliberately
/// generous: the measurement lands on the next frame, and briefly showing
/// too many tabs (which merely scroll) reads better than briefly hiding
/// projects that do fit.
const UNMEASURED_VISIBLE_TABS: usize = 8;

/// Width the trailing `…` overflow button occupies: `px_1` on each side of an
/// `IconButton` at `IconSize::Small`.
const MORE_BUTTON_WIDTH: Pixels = px(33.0);
/// Width of the rule between the tabs and the trailing `+`: a 1px rule plus
/// its 2px right margin.
const PLUS_DIVIDER_WIDTH: Pixels = px(3.0);
/// Slack kept free at the right end of the budget. Shaped label widths and
/// the flexbox's own rounding disagree by a fraction of a pixel per tab, and
/// over-filling by one pixel silently starts a horizontal scroll; under-
/// filling by three is invisible.
const BUDGET_SAFETY_MARGIN: Pixels = px(4.0);

/// Paint selector for the strip's own box — the width budget itself, so a test
/// can compare where the tabs actually landed against the space they were
/// given rather than against a number copied out of this file.
pub(crate) const STRIP_SELECTOR: &str = "PROJECT-TAB-STRIP";

pub struct ProjectTabStrip {
    multi_workspace: WeakEntity<MultiWorkspace>,
    /// The strip's own painted box, measured by the `canvas` in `render`.
    /// `None` until the first frame has been laid out.
    ///
    /// This is only safe to feed back into `render` because the measured
    /// quantity does not depend on the decision it drives: the strip is a
    /// `flex_1` child of the project toolbar, so its width is "whatever the
    /// row has left" no matter how many tabs are inside it. A content-sized
    /// strip would oscillate here.
    measured_bounds: Option<Bounds<Pixels>>,
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
            measured_bounds: None,
            _subscriptions: subscriptions,
        }
    }
}

/// The width `label` will shape to in the tab's own font — the UI font at
/// `LabelSize::Default`, which is what `ui::Label` renders with.
fn shaped_label_width(label: &SharedString, window: &Window, cx: &App) -> Pixels {
    let font = theme::theme_settings(cx).ui_font(cx);
    let font_size = ui::TextSize::Default.rems(cx).to_pixels(window.rem_size());
    let run = TextRun {
        len: label.len(),
        font: font.clone(),
        color: cx.theme().colors().text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(label.clone(), font_size, &[run], None)
        .width
}

/// Paint selector for one row of the overflow menu. The active/inactive state
/// is in the NAME, not in a separate marker element: the row's check differs
/// between the two states only by `Visibility`, and `debug_bounds` is recorded
/// before `Style::visibility` is consulted, so an invisible check still has
/// bounds and a state-blind selector would pass for both.
pub(crate) fn overflow_menu_row_selector(member_id: MemberId, is_active: bool) -> String {
    let state = if is_active { "ACTIVE" } else { "INACTIVE" };
    format!("PROJECT-OVERFLOW-{state}-{}", member_id.0)
}

/// Paint selector for the `…` button's cell, with the marker state in the NAME
/// for the same reason the row's is: the marked and unmarked buttons differ
/// only in the icon's colour and in a dot that `IconWithIndicator` positions
/// absolutely, neither of which changes the button's geometry, so a state-blind
/// selector would pass for both and `ICON-Ellipsis` (which `IconButton`
/// registers for itself) already is that state-blind selector.
pub(crate) fn overflow_more_selector(active_in_overflow: bool) -> &'static str {
    if active_in_overflow {
        "PROJECT-OVERFLOW-MORE-ACTIVE"
    } else {
        "PROJECT-OVERFLOW-MORE-INACTIVE"
    }
}

/// One row of the `…` menu: the active-project check, the project name, and
/// the drag handle that lets the row be pulled out onto the strip.
///
/// Rendered through `ContextMenu::custom_entry`, which wraps this in its own
/// `ListItem` (inset, hover highlight, click routing) — so this only supplies
/// what a `toggleable_entry` would have supplied, plus `on_drag`.
fn overflow_menu_row(
    member_id: MemberId,
    name: &SharedString,
    is_active: bool,
) -> gpui::AnyElement {
    h_flex()
        .id(SharedString::from(format!(
            "project-overflow-row-{}",
            member_id.0
        )))
        .debug_selector(move || overflow_menu_row_selector(member_id, is_active))
        .w_full()
        .gap_1p5()
        .child(
            // Kept in the tree (just invisible) when inactive so every label
            // starts at the same x — the same trick `ContextMenu`'s own
            // toggle slot uses.
            div()
                .flex_none()
                .child(
                    Icon::new(IconName::Check)
                        .size(IconSize::Small)
                        .color(Color::Accent),
                )
                .when(!is_active, |slot| slot.invisible()),
        )
        .child(Label::new(name.clone()))
        .on_drag(
            DraggedProjectTab::new(member_id, name.clone()),
            |dragged, _offset, _window, cx| cx.new(|_| dragged.clone()),
        )
        .into_any_element()
}

/// How many of `widths` (in member order) fit inline before the rest have to
/// spill into the `…`.
///
/// A greedy PREFIX and nothing more. The signature is the rule: there is no
/// `active` parameter, so no amount of navigating can change the answer, and
/// the painted tabs are always the members the user put first. `f3ef02f0f2`
/// took an `active: Option<usize>` here and reserved that member's width up
/// front; the cost was a tab at the fold silently dropping into the `…` the
/// moment an overflow project was selected, which is the spontaneous jump this
/// signature now makes unrepresentable.
///
/// A pure function over plain numbers so the boundary cases — everything fits,
/// nothing fits, the `…` itself being what pushes the last tab out — are
/// unit-testable without a rendered frame.
fn fit_count(widths: &[Pixels], budget: Pixels, more_button: Pixels) -> usize {
    let total: Pixels = widths.iter().copied().fold(px(0.0), |a, b| a + b);
    if total <= budget {
        return widths.len();
    }
    // Something has to spill, so the `…` is going to be painted and its width
    // comes out of the budget before any tab is placed.
    let budget = budget - more_button;

    let mut count = 0;
    let mut used = px(0.0);
    for width in widths {
        if used + *width > budget {
            break;
        }
        used += *width;
        count += 1;
    }
    // Never hide every project: a strip narrower than one tab still shows the
    // first one (it scrolls) rather than collapsing to a bare `…`.
    count.max(1).min(widths.len())
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
            .map(|sol| sol.id)
    })
}

impl Render for ProjectTabStrip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(mw) = self.multi_workspace.upgrade() else {
            return h_flex().h_full().into_any_element();
        };

        let store = SolutionStore::global(cx);
        let active_workspace = mw.read(cx).workspace().clone();
        let active_solution_id = solution_id_for_workspace(&active_workspace, store.read(cx), cx);

        let Some(solution_id) = active_solution_id else {
            // No active solution in this window — nothing to render.
            return h_flex().h_full().into_any_element();
        };

        // Seed a default active member for a freshly-opened solution, then
        // snapshot the (member_id, display name) list in member order and
        // the active member for the highlight. Done inside one `update` so
        // the borrow doesn't span the mutating `ensure_active_member` call.
        // The tab label is `member.name` — the member owns its own name now,
        // so there is no catalog lookup (and no slug fallback) left to do.
        let (members, active_member): (Vec<(MemberId, SharedString)>, Option<MemberId>) = store
            .update(cx, |store, cx| {
                let Some(solution) = store
                    .solutions()
                    .iter()
                    .find(|s: &&Solution| s.id == solution_id)
                    .cloned()
                else {
                    return (Vec::new(), None);
                };
                store.ensure_active_member(solution.id, &solution.members, cx);
                let active = store.active_member(solution.id);
                let entries = solution
                    .members
                    .iter()
                    .map(|member: &SolutionMember| {
                        (member.id, SharedString::from(member.name.clone()))
                    })
                    .collect();
                (entries, active)
            });

        let order: Vec<MemberId> = members.iter().map(|(id, _)| *id).collect();

        // Ghost tabs for in-flight (or just-failed) `add_member` clones that
        // haven't landed as real members yet. Skip any whose catalog project
        // already has a member instantiated from it — that's the brief window
        // between the clone task recording the member and removing the
        // in-flight entry. This is the surface the clone spinner belongs on
        // (the project being cloned), not the owning solution tab.
        let landed: Vec<solutions::CatalogId> = store
            .read(cx)
            .find_solution(solution_id)
            .map(|sol| {
                sol.members
                    .iter()
                    .filter_map(|m| m.origin_catalog_id)
                    .collect()
            })
            .unwrap_or_default();
        // One borrow of the store covers both the pending list and the
        // per-entry "is the catalog row unreferenced" answer that decides
        // whether the failed tab may offer to delete it — the answer depends
        // on every Solution's members, not just this one's.
        let (pending, catalog_removable): (Vec<solutions::PendingAddView>, Vec<bool>) = {
            let store = store.read(cx);
            let pending: Vec<solutions::PendingAddView> = store
                .pending_adds_for(solution_id)
                .into_iter()
                .filter(|p| !landed.contains(&p.catalog_id))
                .collect();
            let removable = pending
                .iter()
                .map(|p| store.catalog_project_is_unreferenced(p.catalog_id))
                .collect();
            (pending, removable)
        };
        let pending_tabs: Vec<PendingProjectTab> = pending
            .into_iter()
            .zip(catalog_removable)
            .map(|(p, catalog_removable)| {
                PendingProjectTab::new(
                    solution_id,
                    p.catalog_id,
                    SharedString::from(p.catalog_name),
                    SharedString::from(p.stage),
                    p.percent,
                    p.error.map(SharedString::from),
                    catalog_removable,
                )
            })
            .collect();

        // Width budget. The strip is a `flex_1` child of the project toolbar,
        // so `measured_bounds` is the width the row actually has left for it —
        // not the width its own content happens to want. Everything that is
        // painted inside the strip but is not a member tab comes off the top:
        // the ghost tabs of in-flight clones (they are never hidden — a failed
        // add has to stay reachable), the `+` cell (a square whose side is the
        // strip height) and the rule before it.
        let pending_width: Pixels = pending_tabs
            .iter()
            .map(|tab| {
                tab_width_for_label(
                    shaped_label_width(tab.name(), window, cx) + PENDING_TAB_TRAILING_WIDTH,
                )
            })
            .fold(px(0.0), |a, b| a + b);
        // Which member is active is NOT an input here. The split is the stored
        // order cut at whatever the width allows, so activating a project that
        // lives past the cut leaves every painted tab exactly where it was —
        // the `…` button below is where that selection shows up instead.
        let visible_count = match self.measured_bounds {
            Some(bounds) => {
                let widths: Vec<Pixels> = members
                    .iter()
                    .map(|(_, name)| tab_width_for_label(shaped_label_width(name, window, cx)))
                    .collect();
                let plus_cell = bounds.size.height;
                let budget = bounds.size.width
                    - pending_width
                    - plus_cell
                    - PLUS_DIVIDER_WIDTH
                    - BUDGET_SAFETY_MARGIN;
                fit_count(&widths, budget, MORE_BUTTON_WIDTH)
            }
            // Pre-measurement frame: a generous prefix. The measurement lands
            // on the next frame.
            None => UNMEASURED_VISIBLE_TABS.min(members.len()),
        };
        let (visible, overflow) = members.split_at(visible_count.min(members.len()));
        let visible = visible.to_vec();
        let overflow = overflow.to_vec();

        let tabs = visible.iter().map(|(member_id, name)| {
            let is_active = active_member == Some(*member_id);
            ProjectTab::new(
                solution_id,
                *member_id,
                name.clone(),
                is_active,
                order.clone(),
            )
        });

        // Trailing `more` popover for the members that didn't fit inline.
        //
        // Each row carries the same leading check the rest of this fork's
        // `ContextMenu` lists use for "this is the one you're on"
        // (`workspace::multi_workspace`, `dock`, `quick_action_bar`, …) —
        // `Color::Accent`, invisible rather than absent when inactive, so the
        // labels stay aligned. It is a hand-built `custom_entry` rather than
        // `toggleable_entry` only because a row has to be DRAGGABLE (below);
        // `ContextMenu` still supplies the `ListItem` chrome, hover highlight
        // and click routing around it, so it reads as an ordinary entry.
        //
        // The check is fully reachable again: the split ignores which member is
        // active, so the project the user is on can and does sit down here, and
        // its row is the marked one.
        //
        // A row can be DRAGGED out of the menu onto the strip, which is the
        // gesture the maintainer asked for. This works — verified live — and it
        // is worth saying why, because the obvious expectation is that it
        // cannot: GPUI menus do dismiss on a mouse-down outside themselves, but
        // this drag *starts inside* the menu, and `active_drag` lives on the
        // `App`, not in the menu's element tree. So the payload outlives the
        // menu's dismissal and lands on whichever tab (or the trailing
        // end-drop zone) it is released over, reusing the drop handlers the
        // visible tabs already have. Nothing about the menu had to change.
        //
        // CLICKING a row only activates the project — nothing about the strip
        // moves, including the strip's own contents. Two earlier versions of
        // this handler did move something (promote to the head; reserve a slot
        // in the budget) and both were the same mistake: a navigation gesture
        // rearranging the layout the user built. `reorder_members` is what the
        // DRAG is for.
        //
        // Which is why the `…` BUTTON carries the feedback instead: with no
        // reservation, activating an overflow project leaves nothing
        // highlighted anywhere on the strip, so the button says the project you
        // are on is behind it. The vocabulary is the fork's existing one for
        // exactly that — an accent dot indicator over the glyph (as
        // `workspace::status_bar` marks the sidebar toggle) plus the accent
        // colour the menu's own check uses. The dot is absolutely positioned
        // inside `IconWithIndicator`, so it costs no layout width and
        // `MORE_BUTTON_WIDTH` stays honest.
        let active_in_overflow = active_member
            .is_some_and(|active| overflow.iter().any(|(member_id, _)| *member_id == active));
        let overflow_popover = (!overflow.is_empty()).then(|| {
            let overflow_entries: Vec<(MemberId, SharedString, bool)> = overflow
                .iter()
                .map(|(member_id, name)| {
                    (
                        *member_id,
                        name.clone(),
                        active_member == Some(*member_id),
                    )
                })
                .collect();
            let indicator_border = cx.theme().colors().title_bar_background;
            let more_button = IconButton::new("project-tab-strip-more", IconName::Ellipsis)
                .icon_size(IconSize::Small)
                .icon_color(if active_in_overflow {
                    Color::Accent
                } else {
                    Color::Muted
                })
                .when(active_in_overflow, |this| {
                    this.indicator(Indicator::dot().color(Color::Accent))
                        .indicator_border_color(Some(indicator_border))
                })
                .tooltip(Tooltip::text(if active_in_overflow {
                    "More projects — the active project is in here"
                } else {
                    "More projects"
                }));
            PopoverMenu::new("project-tab-strip-more-popover")
                .trigger(more_button)
                .menu(move |window, cx| {
                    let overflow_entries = overflow_entries.clone();
                    Some(ContextMenu::build(
                        window,
                        cx,
                        move |mut menu, _window, _cx| {
                            for (member_id, name, is_active) in overflow_entries {
                                let row_name = name.clone();
                                menu = menu.custom_entry(
                                    move |_window, _cx| {
                                        overflow_menu_row(member_id, &row_name, is_active)
                                    },
                                    move |_window, cx| {
                                        SolutionStore::global(cx).update(cx, |store, cx| {
                                            store.set_active_member(solution_id, member_id, cx);
                                        });
                                    },
                                );
                            }
                            menu
                        },
                    ))
                })
        });

        // Trailing `+` button → AddProjectPicker for the active solution.
        let picker_solution_id = solution_id;
        let plus_button = IconButton::new("project-tab-strip-plus", IconName::Plus)
            .icon_size(IconSize::Small)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text("Add project to this solution"));
        let plus_popover = PopoverMenu::new("project-tab-strip-plus-popover")
            .trigger(plus_button)
            .menu(move |window, cx| {
                Some(cx.new(|cx| AddProjectPicker::new(picker_solution_id, window, cx)))
            });

        // Trailing drop zone: dropping a dragged tab here moves it to the
        // very end of the member order — a position no per-tab drop target
        // can express (each tab inserts the dragged member *before* itself).
        // Only meaningful with at least two members. `flex_1` lets it absorb
        // any slack to the right of the last tab as a generous catch area;
        // `min_w` keeps it hittable even when the tabs already fill the strip.
        // Only present while a PROJECT-TAB drag is in flight — gating on
        // `has_active_drag()` (any drag) made this empty catch area appear on
        // unrelated drags too (e.g. resizing a panel), reading as dead space.
        let is_tab_drag = cx.active_drag_is::<DraggedProjectTab>();
        let end_drop = (members.len() > 1 && is_tab_drag).then(|| {
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
                    let new_order = move_to_end(&order, dragged.member_id);
                    SolutionStore::global(cx)
                        .update(cx, |store, cx| {
                            store.reorder_members(solution_id, new_order, cx)
                        })
                        .log_err();
                })
        });

        // Measures the box the toolbar row actually gives the strip. It has to
        // sit OUTSIDE the `overflow_x_scroll` container below: inside one, an
        // absolutely-positioned child is laid out against the scrollable
        // content, so it would report the width of the tabs rather than the
        // width available to them — the exact number this is here to learn.
        let measure = canvas(
            cx.processor(|this: &mut Self, bounds: Bounds<Pixels>, _window, cx| {
                if this.measured_bounds == Some(bounds) {
                    return;
                }
                this.measured_bounds = Some(bounds);
                // `Window::invalidate_view` throws away a notify raised during
                // a draw, so this has to hop out of the frame. Guarded by the
                // change check above: an unconditional notify here re-renders,
                // re-prepaints and notifies again forever.
                let this = cx.entity();
                cx.defer(move |cx| this.update(cx, |_, cx| cx.notify()));
            }),
            |_bounds, _state, _window, _cx| {},
        )
        .size_full()
        .absolute()
        .top_0()
        .left_0();

        let strip = h_flex()
            .id("project-tab-strip")
            .h_full()
            .w_full()
            .overflow_x_scroll()
            .children(tabs)
            .children(pending_tabs)
            .when_some(end_drop, |this, zone| this.child(zone))
            .when_some(overflow_popover, |this, popover| {
                this.child(
                    div()
                        .debug_selector(move || overflow_more_selector(active_in_overflow).to_string())
                        .px_1()
                        .child(popover),
                )
            })
            // Trailing `+`, hidden while a project tab of THIS strip is being
            // dragged (the drop affordances own the trailing space then, and a
            // stray `+` under the drag ghost reads as clutter). A subtle
            // divider separates it from the tabs; the `+` sits in a square cell
            // (side == strip height) snug after the tabs, so its centre is
            // equidistant from the strip's top/bottom and from the last tab.
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
                        // Square cell whose width is DERIVED from the strip
                        // height (aspect 1:1), so the `+` stays centred in a
                        // square if the row height ever changes — no hardcoded
                        // side length coupled to a magic 30px.
                        .aspect_square()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(plus_popover),
                )
            });

        div()
            .relative()
            .w_full()
            .h_full()
            .debug_selector(|| STRIP_SELECTOR.to_string())
            .child(measure)
            .child(strip)
            .into_any_element()
    }
}

#[cfg(test)]
mod fit_tests {
    use super::*;

    #[test]
    fn everything_fitting_costs_no_overflow_button() {
        let widths = [px(100.), px(100.), px(100.)];
        // Exactly enough room, and the `…` is not subtracted because nothing
        // spills — the case the old fixed cap could never express.
        assert_eq!(fit_count(&widths, px(300.), px(33.)), 3);
    }

    #[test]
    fn the_overflow_button_comes_out_of_the_budget_before_any_tab() {
        let widths = [px(100.), px(100.), px(100.)];
        // 299px cannot hold all three, so the `…` is paid for first and only
        // (299 - 33) / 100 = 2 tabs remain.
        assert_eq!(fit_count(&widths, px(299.), px(33.)), 2);
    }

    #[test]
    fn a_strip_narrower_than_one_tab_still_shows_one() {
        let widths = [px(120.), px(120.)];
        assert_eq!(fit_count(&widths, px(40.), px(33.)), 1);
    }

    #[test]
    fn no_members_fit_nothing() {
        assert_eq!(fit_count(&[], px(500.), px(33.)), 0);
    }

    /// Was `the_active_member_is_reserved_a_slot_past_the_fold`, which asserted
    /// that an active member past the fold displaced a leading tab. That rule
    /// is gone; what survives from it is the arithmetic it was built on — a
    /// budget that holds exactly two of four tabs holds the LEADING two, and
    /// the number is decided by the width alone.
    #[test]
    fn the_split_is_the_leading_run_that_the_width_pays_for() {
        let widths = [px(100.), px(100.), px(100.), px(100.)];
        assert_eq!(fit_count(&widths, px(233.), px(33.)), 2);
        // One pixel less than the third tab needs is still two tabs; one pixel
        // more is three. Nothing else is an input.
        assert_eq!(fit_count(&widths, px(332.), px(33.)), 2);
        assert_eq!(fit_count(&widths, px(333.), px(33.)), 3);
    }

    /// Was `an_active_member_wider_than_the_whole_budget_is_still_painted`,
    /// which pinned an over-wide ACTIVE tab onto the strip. The over-wide case
    /// still matters, but the member it protects is the FIRST one: a strip too
    /// narrow for even the leading tab shows it anyway rather than collapsing
    /// to a bare `…`.
    #[test]
    fn a_leading_tab_wider_than_the_whole_budget_is_still_painted() {
        let widths = [px(200.), px(100.), px(100.)];
        assert_eq!(fit_count(&widths, px(150.), px(33.)), 1);
    }

    // `an_active_member_that_already_fits_changes_nothing` and
    // `an_out_of_range_active_index_is_ignored` are not rewritten here: both
    // tested edge cases of the `active` parameter, and the parameter is gone.
    // What they were approximating — "which member is active does not move a
    // tab" — is now a property of the signature rather than of the body, so a
    // unit test of it would be a tautology. It is asserted where it is actually
    // observable, against a painted frame:
    // `activating_a_project_in_the_overflow_changes_nothing_on_the_strip`.
}

/// Paint tests for the width budget and the overflow menu.
///
/// These assert against the geometry a real frame produced (`debug_bounds`),
/// not against the predicate the strip computed — asserting the predicate is
/// the repeat defect this repo has a finding about
/// (`docs/findings/2026-09-02-paint-tests-with-debug-bounds.md`). In
/// particular, `fit_count` returning 9 proves nothing about whether nine tabs
/// were painted, or whether they fit.
#[cfg(test)]
mod paint_tests {
    use super::*;
    use crate::project_tab::{project_tab_selector, project_tab_state_selector};
    use gpui::{Bounds, TestAppContext, VisualTestContext};

    /// Hosts a `ProjectTabStrip` at an exact width. The strip's whole contract
    /// is "fill the box you are given", so a test host with a pinned width
    /// exercises the budget honestly without standing up the project toolbar's
    /// flex row (whose own job — handing the strip the row's slack — is a
    /// separate assertion, in `title_bar`).
    struct StripHost {
        strip: Entity<ProjectTabStrip>,
        width: Pixels,
    }

    impl Render for StripHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(self.width)
                .h(px(28.))
                .flex()
                .child(self.strip.clone())
        }
    }

    /// `debug_bounds` wants a `&'static str`; the selectors here are built per
    /// member id. Leaking a handful of short strings inside a test is the
    /// cheapest way across that gap.
    fn bounds_of(cx: &mut VisualTestContext, selector: String) -> Option<Bounds<Pixels>> {
        cx.debug_bounds(String::leak(selector))
    }

    const MEMBER_NAMES: [&str; 12] = [
        "ecos-base",
        "ecos-records",
        "citeck-community",
        "ecos-webapp",
        "ecos-model",
        "ecos-uiserv",
        "ecos-apps",
        "ecos-integrations",
        "ecos-notifications",
        "ecos-process",
        "ecos-history",
        "ecos-bpmn",
    ];

    /// A window hosting a real `ProjectTabStrip` over a Solution with
    /// `MEMBER_NAMES` as its members, laid out at exactly `width`.
    async fn strip_at_width(
        width: Pixels,
        cx: &mut TestAppContext,
    ) -> (Vec<MemberId>, SolutionId, &mut VisualTestContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let (solution_id, member_ids, member_paths) = cx.update(|cx| {
            let store = SolutionStore::for_test(std::path::PathBuf::new(), cx);
            let out = store.update(cx, |store, _cx| {
                let solution_id = store.create_for_test_minimal("overflow-probe", _cx);
                let root = store
                    .solutions()
                    .last()
                    .expect("solution was just created")
                    .root
                    .clone();
                let mut ids = Vec::new();
                let mut paths = Vec::new();
                for name in MEMBER_NAMES {
                    let path = root.join(name);
                    ids.push(store.test_add_member_with_path(solution_id, name, path.clone()));
                    paths.push(path);
                }
                (solution_id, ids, paths)
            });
            solutions::install_global_for_test(store, cx);
            out
        });

        let fs = fs::FakeFs::new(cx.executor());
        for path in &member_paths {
            fs.insert_tree(path, serde_json::json!({ "a.txt": "" })).await;
        }
        let project = project::Project::test(
            fs.clone(),
            member_paths.iter().map(|p| p.as_path()),
            cx,
        )
        .await;
        cx.run_until_parked();

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let strip = cx.update(|_window, cx| {
            cx.new(|cx| {
                ProjectTabStrip::new(workspace.downgrade(), multi_workspace.downgrade(), cx)
            })
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| StripHost { strip, width });
        // Two frames: the first lays the strip out and measures its box, the
        // second is the one that renders the tab split that measurement
        // decided. A single frame would still be showing
        // `UNMEASURED_VISIBLE_TABS`.
        cx.run_until_parked();
        redraw(cx);

        (member_ids, solution_id, cx)
    }

    /// Force another frame. The strip decides its split from the box it
    /// measured on the PREVIOUS frame, so every assertion here needs at least
    /// two draws — one that measures, one that renders what the measurement
    /// decided.
    fn redraw(cx: &mut VisualTestContext) {
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();
    }

    /// Every member tab that actually landed in the last painted frame, in
    /// LEFT-TO-RIGHT order. Sorted by the painted x, not by member order: the
    /// two differ the moment anything is reordered, and "which tab is leftmost"
    /// is the question these tests ask.
    fn painted_tabs(
        member_ids: &[MemberId],
        cx: &mut VisualTestContext,
    ) -> Vec<(MemberId, Bounds<Pixels>)> {
        let mut painted: Vec<(MemberId, Bounds<Pixels>)> = member_ids
            .iter()
            .filter_map(|id| bounds_of(cx, project_tab_selector(*id)).map(|b| (*id, b)))
            .collect();
        painted.sort_by(|(_, a), (_, b)| {
            a.left()
                .partial_cmp(&b.left())
                .expect("painted bounds are finite")
        });
        painted
    }

    /// The member order as the STORE holds it — the thing a selection must
    /// never touch. Read back from the store rather than inferred from the
    /// paint, because "the strip shows it in place" and "the order was
    /// rewritten and it happens to look the same" are different bugs.
    fn stored_order(solution_id: SolutionId, cx: &mut VisualTestContext) -> Vec<MemberId> {
        cx.update(|_window, cx| {
            SolutionStore::global(cx)
                .read(cx)
                .find_solution(solution_id)
                .expect("the probe solution exists")
                .members
                .iter()
                .map(|member| member.id)
                .collect()
        })
    }

    /// The painted tabs, left to right, must be the stored order with the
    /// spilled members deleted — a subsequence, never a permutation.
    fn assert_keeps_relative_order(painted: &[(MemberId, Bounds<Pixels>)], order: &[MemberId]) {
        let painted_ids: Vec<MemberId> = painted.iter().map(|(id, _)| *id).collect();
        let expected: Vec<MemberId> = order
            .iter()
            .copied()
            .filter(|id| painted_ids.contains(id))
            .collect();
        assert_eq!(
            painted_ids, expected,
            "the visible tabs must keep their stored relative order"
        );
    }

    /// The `…` menu's rows for the members that are currently spilled. Opens
    /// the popover; the caller asserts against the row selectors.
    fn open_overflow_menu(cx: &mut VisualTestContext) {
        let more = cx
            .debug_bounds("ICON-Ellipsis")
            .expect("the overflow button must paint");
        cx.simulate_mouse_move(more.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_click(more.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    /// The width the next member would need if it were promoted onto the
    /// strip — used to assert that a spill really was forced by the width and
    /// not by a leftover cap.
    fn predicted_width(name: &str, cx: &mut VisualTestContext) -> Pixels {
        cx.update(|window, cx| {
            tab_width_for_label(shaped_label_width(&SharedString::from(name), window, cx))
        })
    }

    /// Asserts the strip is honestly full: either every project is on it, or
    /// the first one that is not could not have fitted in what is left.
    fn assert_no_room_was_wasted(
        painted: &[(MemberId, Bounds<Pixels>)],
        strip: Bounds<Pixels>,
        cx: &mut VisualTestContext,
    ) {
        if painted.len() == MEMBER_NAMES.len() {
            return;
        }
        let rightmost = painted
            .iter()
            .map(|(_, b)| b.right())
            .fold(px(0.), |a, b| if b > a { b } else { a });
        let leftover = strip.right() - rightmost;
        let next = predicted_width(MEMBER_NAMES[painted.len()], cx);
        assert!(
            leftover < next,
            "the strip spilled {} project(s) while {leftover:?} was still free and \
             the next tab only needed {next:?} — that is the fixed-cap bug",
            MEMBER_NAMES.len() - painted.len()
        );
    }

    /// The regression this whole change exists for: a wide strip must use the
    /// width it has. Asserted against the painted geometry — how many tabs
    /// landed, and where their right edge sits relative to the strip's own box.
    #[gpui::test]
    async fn a_wide_strip_paints_more_than_the_old_fixed_cap_and_still_fits(
        cx: &mut TestAppContext,
    ) {
        let (member_ids, _solution_id, cx) = strip_at_width(px(1580.), cx).await;

        let strip = cx
            .debug_bounds(STRIP_SELECTOR)
            .expect("the strip must paint");
        let painted = painted_tabs(&member_ids, cx);

        // The old behaviour, stated as the thing that must not come back: this
        // is a width at which the strip painted exactly six tabs and left the
        // rest of the row empty, whatever the font.
        assert!(
            painted.len() > 6,
            "a 1580px strip must paint more than the old fixed cap of six tabs, \
             painted {}",
            painted.len()
        );

        let rightmost = painted
            .iter()
            .map(|(_, b)| b.right())
            .fold(px(0.), |a, b| if b > a { b } else { a });
        assert!(
            rightmost <= strip.right(),
            "no tab may be painted past the strip's own box: last tab ends at \
             {rightmost:?}, strip ends at {:?}",
            strip.right()
        );
        assert_eq!(
            cx.debug_bounds("ICON-Ellipsis").is_some(),
            painted.len() < MEMBER_NAMES.len(),
            "the overflow button must be painted exactly when something spilled"
        );
        assert_no_room_was_wasted(&painted, strip, cx);
    }

    /// The other side: the same projects in a strip that genuinely cannot hold
    /// them paint fewer tabs, still inside the box, and do surface the `…`.
    #[gpui::test]
    async fn a_narrow_strip_paints_fewer_tabs_and_surfaces_the_overflow_button(
        cx: &mut TestAppContext,
    ) {
        let (member_ids, _solution_id, cx) = strip_at_width(px(660.), cx).await;

        let strip = cx
            .debug_bounds(STRIP_SELECTOR)
            .expect("the strip must paint");
        let painted = painted_tabs(&member_ids, cx);

        assert!(
            !painted.is_empty() && painted.len() < MEMBER_NAMES.len(),
            "660px cannot hold twelve projects, but must hold some: painted {}",
            painted.len()
        );
        let rightmost = painted
            .iter()
            .map(|(_, b)| b.right())
            .fold(px(0.), |a, b| if b > a { b } else { a });
        assert!(
            rightmost <= strip.right(),
            "even when it has to spill, the strip must not paint a tab past its \
             own box: {rightmost:?} vs {:?}",
            strip.right()
        );
        assert!(
            cx.debug_bounds("ICON-Ellipsis").is_some(),
            "something spilled, so the overflow button must be painted"
        );
        // The tabs it did paint are the leading ones, in member order — the
        // split is a prefix, not an arbitrary subset.
        let painted_ids: Vec<MemberId> = painted.iter().map(|(id, _)| *id).collect();
        assert_eq!(painted_ids, member_ids[..painted.len()].to_vec());
        // …and it spilled because it had to, not because of a leftover cap.
        assert_no_room_was_wasted(&painted, strip, cx);
    }

    /// The width budget is only honest if a tab really lays out at the width
    /// `tab_width_for_label` predicts. Pins the analytic model to the geometry
    /// a real frame produced, so a change to the tab's padding that forgets the
    /// constants fails here instead of silently making the budget lie.
    #[gpui::test]
    async fn tabs_lay_out_at_their_predicted_width(cx: &mut TestAppContext) {
        let (member_ids, _solution_id, cx) = strip_at_width(px(1580.), cx).await;
        let painted = painted_tabs(&member_ids, cx);
        assert!(painted.len() > 6, "need several tabs to compare");

        for ((_, bounds), name) in painted.iter().zip(MEMBER_NAMES) {
            let predicted = predicted_width(name, cx);
            let actual = bounds.size.width;
            assert!(
                (actual - predicted).abs() <= px(1.0),
                "{name}: predicted {predicted:?}, painted {actual:?} — the strip's \
                 width budget and the tab's own styling have drifted apart"
            );
        }
    }

    /// The rule this change exists for: activating a project that lives in the
    /// `…` changes NOTHING about the strip. Same tabs, same order, same x, same
    /// stored member order — the only thing that moves is the `…` button's own
    /// marker.
    ///
    /// This replaces `the_active_project_is_painted_in_place_even_when_it_sits_
    /// past_the_fold`, which asserted the opposite: that the budget reserved the
    /// active member a slot and therefore pushed a tab at the fold into the `…`.
    /// That reservation was the second of two spontaneous jumps (the first being
    /// `cc05f6ef6d`'s promote-to-head) and is what this test now forbids.
    #[gpui::test]
    async fn activating_a_project_in_the_overflow_changes_nothing_on_the_strip(
        cx: &mut TestAppContext,
    ) {
        let (member_ids, solution_id, cx) = strip_at_width(px(660.), cx).await;
        let before = painted_tabs(&member_ids, cx);
        assert!(
            before.len() < member_ids.len(),
            "precondition: something must have spilled"
        );
        let order_before = stored_order(solution_id, cx);
        // Deliberately the LAST member — the furthest possible from the fold,
        // so nothing here can pass by accident of being one slot over.
        let active = *member_ids.last().expect("twelve members exist");
        assert!(
            bounds_of(cx, project_tab_selector(active)).is_none(),
            "precondition: the last project starts off the strip"
        );
        assert!(
            cx.debug_bounds(overflow_more_selector(false)).is_some(),
            "precondition: the `…` starts unmarked — the active project is on \
             the strip"
        );

        cx.update(|_window, cx| {
            SolutionStore::global(cx).update(cx, |store, cx| {
                store.set_active_member(solution_id, active, cx);
            })
        });
        cx.run_until_parked();
        redraw(cx);

        let after = painted_tabs(&member_ids, cx);
        // The whole rule, in one assertion: same members, same left-to-right
        // order, same boxes down to the pixel.
        assert_eq!(
            after, before,
            "activating an overflow project must leave every painted tab \
             exactly where it was"
        );
        assert!(
            bounds_of(cx, project_tab_selector(active)).is_none(),
            "the activated project must NOT have been pulled onto the strip"
        );
        assert_eq!(
            stored_order(solution_id, cx),
            order_before,
            "activating a project must not rewrite the member order"
        );
        // Nor may some other tab quietly take the highlight.
        for (member_id, _) in &after {
            assert!(
                bounds_of(cx, project_tab_state_selector(*member_id, true)).is_none(),
                "no tab may claim to be active while the active project is in \
                 the overflow"
            );
        }

        // …and the feedback that replaces the reservation: the `…` says the
        // project you are on is behind it, and its row inside carries the check.
        assert!(
            cx.debug_bounds(overflow_more_selector(true)).is_some(),
            "the `…` must be marked when the active project is inside it"
        );
        assert!(
            cx.debug_bounds(overflow_more_selector(false)).is_none(),
            "…and must not also paint unmarked"
        );
        open_overflow_menu(cx);
        assert!(
            bounds_of(cx, overflow_menu_row_selector(active, true)).is_some(),
            "the active project's row in the menu must paint marked — the check \
             is reachable again now that the strip does not pull it out"
        );
        let still_hidden = *member_ids
            .iter()
            .find(|id| **id != active && bounds_of(cx, project_tab_selector(**id)).is_none())
            .expect("more than one project is still spilled at 660px");
        assert!(
            bounds_of(cx, overflow_menu_row_selector(still_hidden, false)).is_some(),
            "a hidden project that is not active must have an unmarked row"
        );
    }

    /// The other side of the `…` marker: while the active project is one of the
    /// painted tabs, the button is plain and the highlight is on the tab.
    #[gpui::test]
    async fn the_overflow_button_is_unmarked_while_the_active_project_is_on_the_strip(
        cx: &mut TestAppContext,
    ) {
        let (member_ids, solution_id, cx) = strip_at_width(px(660.), cx).await;
        let painted = painted_tabs(&member_ids, cx);
        assert!(
            painted.len() < member_ids.len(),
            "precondition: the `…` must exist at all"
        );
        // Move the active member around WITHIN the painted run: still no mark.
        let on_strip = painted.last().map(|(id, _)| *id).expect("tabs are painted");
        cx.update(|_window, cx| {
            SolutionStore::global(cx).update(cx, |store, cx| {
                store.set_active_member(solution_id, on_strip, cx);
            })
        });
        cx.run_until_parked();
        redraw(cx);

        assert!(
            bounds_of(cx, project_tab_state_selector(on_strip, true)).is_some(),
            "a project that is on the strip takes the highlight there"
        );
        assert!(
            cx.debug_bounds(overflow_more_selector(false)).is_some(),
            "the `…` must be unmarked while the active project is visible"
        );
        assert!(
            cx.debug_bounds(overflow_more_selector(true)).is_none(),
            "…and must not claim to hold the active project"
        );
    }

    /// The overflow row's "this is the one you're on" check, isolated. The
    /// state is reachable through the real menu again (the strip no longer
    /// pulls the active member out of the overflow) and
    /// `activating_a_project_in_the_overflow_changes_nothing_on_the_strip`
    /// exercises it end to end; these two keep the row itself covered on BOTH
    /// sides without standing up a whole strip to get there.
    struct RowHost {
        member_id: MemberId,
        name: SharedString,
        is_active: bool,
    }

    impl Render for RowHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(240.))
                .h(px(24.))
                .child(overflow_menu_row(self.member_id, &self.name, self.is_active))
        }
    }

    async fn row_painted_as(is_active: bool, cx: &mut TestAppContext) -> (MemberId, &mut VisualTestContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let member_id = MemberId(7);
        let (_host, cx) = cx.add_window_view(|_window, _cx| RowHost {
            member_id,
            name: SharedString::from("ecos-base"),
            is_active,
        });
        cx.run_until_parked();
        (member_id, cx)
    }

    #[gpui::test]
    async fn an_overflow_row_paints_the_active_mark(cx: &mut TestAppContext) {
        let (member_id, cx) = row_painted_as(true, cx).await;
        assert!(
            bounds_of(cx, overflow_menu_row_selector(member_id, true)).is_some(),
            "an active row must paint marked"
        );
        assert!(
            bounds_of(cx, overflow_menu_row_selector(member_id, false)).is_none(),
            "…and must not also paint unmarked"
        );
    }

    #[gpui::test]
    async fn an_overflow_row_paints_unmarked_when_it_is_not_active(cx: &mut TestAppContext) {
        let (member_id, cx) = row_painted_as(false, cx).await;
        assert!(
            bounds_of(cx, overflow_menu_row_selector(member_id, false)).is_some(),
            "an inactive row must paint unmarked"
        );
        assert!(
            bounds_of(cx, overflow_menu_row_selector(member_id, true)).is_none(),
            "…and must not be marked as active"
        );
    }

    /// Problem 2, the gesture as asked for: drag a row out of the `…` menu and
    /// drop it on a visible tab.
    ///
    /// The obvious expectation is that this cannot work — a GPUI `ContextMenu`
    /// dismisses on a mouse-down outside itself. It works because the drag
    /// STARTS inside the menu and `active_drag` lives on the `App`, not in the
    /// menu's element tree, so the payload outlives the dismissal and lands on
    /// the tab's ordinary `on_drop`.
    #[gpui::test]
    async fn a_project_can_be_dragged_out_of_the_overflow_menu_onto_the_strip(
        cx: &mut TestAppContext,
    ) {
        let (member_ids, solution_id, cx) = strip_at_width(px(660.), cx).await;
        let painted = painted_tabs(&member_ids, cx);
        let target = painted
            .get(1)
            .map(|(id, bounds)| (*id, *bounds))
            .expect("two tabs must be on the strip to drop onto the second");
        let hidden = *member_ids
            .get(painted.len())
            .expect("some project must have spilled");
        let order_before = stored_order(solution_id, cx);

        open_overflow_menu(cx);

        let row = bounds_of(cx, overflow_menu_row_selector(hidden, false))
            .expect("the hidden project must have a row in the menu");

        // GPUI only arms a drag from a mouse-down on an already-hovered hitbox,
        // and only promotes it to a drag once the pointer has moved with the
        // button held.
        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_down(row.center(), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        for step in 1..=6 {
            let t = step as f32 / 6.0;
            let position = row.center() + (target.1.center() - row.center()) * t;
            cx.simulate_mouse_move(
                position,
                gpui::MouseButton::Left,
                gpui::Modifiers::none(),
            );
            cx.run_until_parked();
        }
        cx.simulate_mouse_up(
            target.1.center(),
            gpui::MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();
        redraw(cx);

        assert!(
            bounds_of(cx, project_tab_selector(hidden)).is_some(),
            "the dragged project must land on the strip"
        );
        let painted = painted_tabs(&member_ids, cx);
        let positions: Vec<MemberId> = painted.iter().map(|(id, _)| *id).collect();
        let dropped_at = positions
            .iter()
            .position(|id| *id == hidden)
            .expect("the dragged project is on the strip");
        let target_at = positions
            .iter()
            .position(|id| *id == target.0)
            .expect("the drop target is still on the strip");
        assert_eq!(
            dropped_at + 1,
            target_at,
            "a project dropped on a tab takes that tab's slot, exactly as a \
             tab-to-tab reorder does: {positions:?}"
        );

        // Dragging is the gesture that DOES rewrite the order — the whole
        // reason clicking must not. Asserted against the store, exactly as the
        // click test asserts the opposite.
        let mut expected: Vec<MemberId> =
            order_before.iter().copied().filter(|id| *id != hidden).collect();
        let target_slot = expected
            .iter()
            .position(|id| *id == target.0)
            .expect("the drop target is still a member");
        expected.insert(target_slot, hidden);
        let order_after = stored_order(solution_id, cx);
        assert_ne!(
            order_after, order_before,
            "a drag must persist a new member order"
        );
        assert_eq!(
            order_after, expected,
            "the dragged member takes the drop target's slot in the stored order"
        );
        // And the strip repaints as a prefix of the NEW order — a drag changes
        // what the split is taken from, never how it is taken.
        assert_keeps_relative_order(&painted, &order_after);
    }

    /// Picking a project from the `…` menu ACTIVATES it and does NOTHING else.
    ///
    /// Two behaviours have been removed from this handler. `cc05f6ef6d` moved
    /// the picked project to the head of the member order; `f3ef02f0f2` stopped
    /// doing that but made the budget reserve the active member's width, so the
    /// pick still dragged a tab at the fold into the `…`. Both are asserted
    /// against here: the stored order is byte-for-byte unchanged AND the painted
    /// tabs are byte-for-byte unchanged.
    #[gpui::test]
    async fn picking_a_project_from_the_overflow_menu_activates_it_and_moves_nothing(
        cx: &mut TestAppContext,
    ) {
        let (member_ids, solution_id, cx) = strip_at_width(px(660.), cx).await;
        let painted_before = painted_tabs(&member_ids, cx);
        let hidden = *member_ids
            .get(painted_before.len())
            .expect("some project must have spilled");
        let order_before = stored_order(solution_id, cx);
        let previously_active = cx
            .update(|_window, cx| SolutionStore::global(cx).read(cx).active_member(solution_id))
            .expect("the strip seeds an active member on its first frame");
        assert!(
            bounds_of(cx, project_tab_selector(hidden)).is_none(),
            "precondition: the project starts off the strip"
        );
        assert!(
            bounds_of(cx, project_tab_state_selector(previously_active, true)).is_some(),
            "precondition: some other project starts out highlighted"
        );

        open_overflow_menu(cx);
        let row = bounds_of(cx, overflow_menu_row_selector(hidden, false))
            .expect("the hidden project must have a row in the menu");
        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        redraw(cx);

        // The regression that shipped in cc05f6ef6d: selecting rewrote the
        // order. It must now be untouched, exactly. Asserted FIRST so a
        // reintroduced promotion fails here, on its own name, rather than
        // downstream on some consequence of it.
        assert_eq!(
            stored_order(solution_id, cx),
            order_before,
            "selecting a project from the overflow menu must not reorder the \
             members — dragging is the gesture that reorders"
        );
        // The regression that shipped in f3ef02f0f2: selecting spilled a tab at
        // the fold to make room. The painted frame must be identical.
        assert_eq!(
            painted_tabs(&member_ids, cx),
            painted_before,
            "selecting a project from the overflow menu must not move, add or \
             drop a single tab"
        );

        // It really did activate: the tab we came from has given up the
        // highlight, and nothing on the strip took it.
        assert!(
            bounds_of(cx, project_tab_state_selector(previously_active, false)).is_some(),
            "the project we came from must still be painted, unhighlighted"
        );
        assert!(
            bounds_of(cx, project_tab_state_selector(previously_active, true)).is_none(),
            "…and must not still claim to be active"
        );
        assert_eq!(
            cx.update(|_window, cx| SolutionStore::global(cx).read(cx).active_member(solution_id)),
            Some(hidden),
            "the picked project must be the active member"
        );
        assert!(
            cx.debug_bounds(overflow_more_selector(true)).is_some(),
            "…which the `…` button reports, since the tab cannot"
        );
    }
}
