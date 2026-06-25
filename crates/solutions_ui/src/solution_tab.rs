//! One open-solution tab in the title-bar strip.
//!
//! Click → switch to this Solution in the same window via
//! [`crate::open::open_solution`] (`OpenIntent::SameWindow`). Middle-click
//! is intentionally a no-op — closing a solution is explicit (right-click
//! → Close, or the picker dropdown). Right-click → context menu (Close,
//! Delete…, Reveal Solution Folder, Rename…).
//!
//! Visuals: deterministic colour dot derived from the id, name (truncated),
//! AI session badge (Sparkle + count) when there are live sessions for this
//! solution, clone-progress spinner while an `add_member` is in flight,
//! active-tab highlight (background + border accent).
//!
//! The badge counts and clone-in-flight flag are caller-driven (passed
//! into `new`) so this `RenderOnce` element doesn't read globals during
//! render — the `SolutionTabStrip` (Task 7) will subscribe to the store
//! events that change them and pass fresh values down on each rerender.

use gpui::{
    App, ClickEvent, Context, Hsla, IntoElement, Render, RenderOnce, SharedString, WeakEntity,
    Window, div, hsla, px,
};
use solutions::SolutionId;
use std::cell::RefCell;
use ui::{ContextMenu, Indicator, prelude::*, right_click_menu};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace};

use crate::actions::{
    CloseSolutionFromTabBar, DeleteSolutionFromTabBar, RenameSolution, RevealSolutionFolder,
};
use crate::open::{OpenIntent, open_solution};

#[derive(IntoElement)]
pub struct SolutionTab {
    id: SolutionId,
    name: SharedString,
    is_active: bool,
    ai_session_count: usize,
    clone_in_flight: bool,
    /// Position in the displayed tab order — the drag payload and drop target
    /// both key off it so `MultiWorkspace::reorder_workspaces` can move the
    /// dragged tab to land at this tab's slot.
    index: usize,
    /// Drop target for reorder — `on_drop` calls back into the multi-workspace
    /// to move the dragged tab into this tab's position.
    multi_workspace: WeakEntity<MultiWorkspace>,
    /// Held for parity with the spec and to give Task 7's tab-strip a
    /// stable handle path; currently unused inside `render` because the
    /// click handler dispatches through `cx.windows()` and the right-
    /// click context menu dispatches actions globally.
    #[allow(dead_code)]
    weak_workspace: WeakEntity<Workspace>,
}

/// Drag payload for reordering solution tabs. Mirrors
/// `console_panel::DraggedConsoleTab`: carries the source `index` (consumed by
/// the drop target's [`MultiWorkspace::reorder_workspaces`]) plus the
/// colour-dot + name so the drag preview looks like the tab being dragged.
#[derive(Clone)]
pub struct DraggedSolutionTab {
    pub(crate) index: usize,
    name: SharedString,
    dot: Hsla,
}

impl Render for DraggedSolutionTab {
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

impl SolutionTab {
    pub fn new(
        id: SolutionId,
        name: SharedString,
        is_active: bool,
        ai_session_count: usize,
        clone_in_flight: bool,
        index: usize,
        multi_workspace: WeakEntity<MultiWorkspace>,
        weak_workspace: WeakEntity<Workspace>,
    ) -> Self {
        Self {
            id,
            name,
            is_active,
            ai_session_count,
            clone_in_flight,
            index,
            multi_workspace,
            weak_workspace,
        }
    }
}

impl RenderOnce for SolutionTab {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let dot = dot_color(&self.id);
        let id_for_click = self.id.clone();
        let id_for_menu = self.id.clone();
        let active_bg = if self.is_active {
            Some(cx.theme().colors().tab_active_background)
        } else {
            None
        };
        let active_border = cx.theme().colors().border_focused;
        let inactive_border = cx.theme().colors().border_transparent;
        let row_id = SharedString::from(format!("solution-tab-{}", self.id.as_str()));
        let menu_id = SharedString::from(format!("solution-tab-menu-{}", self.id.as_str()));

        let row = h_flex()
            .id(row_id)
            .h_full()
            .px_3()
            .gap_2()
            // Match the Console panel chat-tab sizing (panel.rs render_tab_strip)
            // so the title-bar solution tabs read at the same width.
            .min_w(px(96.0))
            .max_w(px(220.0))
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
            .when(self.ai_session_count > 0, |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Sparkle)
                                .size(IconSize::XSmall)
                                .color(Color::Accent),
                        )
                        .child(
                            Label::new(self.ai_session_count.to_string())
                                .size(LabelSize::Small)
                                .color(Color::Accent),
                        ),
                )
            })
            .when(self.clone_in_flight, |this| {
                this.child(Indicator::icon(Icon::new(IconName::ArrowCircle)).color(Color::Accent))
            })
            .on_click({
                move |_event: &ClickEvent, window, cx| {
                    let id = id_for_click.clone();
                    let source = window.window_handle().downcast();
                    open_solution(id, source, OpenIntent::SameWindow, cx);
                }
            })
            // Drag-and-drop reorder. `on_drag` only fires once the pointer
            // crosses GPUI's movement threshold, so a plain click still
            // reaches `on_click` above and switches solutions.
            .on_drag(
                DraggedSolutionTab {
                    index: self.index,
                    name: self.name.clone(),
                    dot,
                },
                |dragged, _offset, _window, cx| cx.new(|_| dragged.clone()),
            )
            .drag_over::<DraggedSolutionTab>(|style, _dragged, _window, cx| {
                style.bg(cx.theme().colors().drop_target_background)
            })
            .on_drop({
                let multi_workspace = self.multi_workspace.clone();
                let target = self.index;
                move |dragged: &DraggedSolutionTab, _window, cx| {
                    let from = dragged.index;
                    multi_workspace
                        .update(cx, |mw, cx| mw.reorder_workspaces(from, target, cx))
                        .log_err();
                }
            });

        // Wrap the row in a `right_click_menu` so the user can reach
        // Close / Delete / Reveal / Rename. Each entry just dispatches
        // the matching action — the workspace-level handlers (registered
        // in `solutions_ui::register_tab_actions`) own the actual
        // behaviour, which keeps this element pure presentation. The
        // `RefCell` dance mirrors `solution_agent::conversation_render`:
        // `right_click_menu::trigger` takes an `Fn` closure but we want
        // to consume the row exactly once when the trigger fires.
        let row_cell = RefCell::new(Some(row.into_any_element()));
        right_click_menu(menu_id)
            .trigger(move |_, _, _| {
                row_cell
                    .borrow_mut()
                    .take()
                    .unwrap_or_else(|| div().into_any_element())
            })
            .menu(move |window, cx| {
                let id_str = id_for_menu.0.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.action(
                        "Close",
                        Box::new(CloseSolutionFromTabBar { id: id_str.clone() }),
                    )
                    .action(
                        "Delete…",
                        Box::new(DeleteSolutionFromTabBar { id: id_str.clone() }),
                    )
                    .separator()
                    .action(
                        "Reveal Solution Folder",
                        Box::new(RevealSolutionFolder { id: id_str.clone() }),
                    )
                    .action("Rename…", Box::new(RenameSolution { id: id_str }))
                })
            })
            .into_any_element()
    }
}

/// Deterministic hue derived from the solution's id. Stable across
/// restarts — keeps a tab visually identifiable even when its name
/// changes — and reasonably spread across the colour wheel for short
/// id strings (the persistence layer uses uuid-shaped ids).
fn dot_color(id: &SolutionId) -> Hsla {
    dot_color_for_str(id.as_str())
}

/// FNV-1a 32-bit hue derivation shared by the solution tabs and the
/// per-project tabs (which hash a `CatalogId` instead of a `SolutionId`).
/// Stable across Rust toolchain upgrades unlike `DefaultHasher` (whose
/// algorithm is explicitly unstable). The 360-mod means we don't care
/// about higher-quality hashing.
pub(crate) fn dot_color_for_str(s: &str) -> Hsla {
    let mut h: u32 = 2166136261;
    for byte in s.bytes() {
        h ^= byte as u32;
        h = h.wrapping_mul(16777619);
    }
    let hue = (h % 360) as f32;
    hsla(hue / 360.0, 0.55, 0.55, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_color_is_stable_for_same_id() {
        let id = SolutionId("abc-123".to_string());
        assert_eq!(dot_color(&id), dot_color(&id));
    }

    #[test]
    fn dot_color_differs_across_ids() {
        let a = dot_color(&SolutionId("a".to_string()));
        let b = dot_color(&SolutionId("b".to_string()));
        assert_ne!(a, b);
    }
}
