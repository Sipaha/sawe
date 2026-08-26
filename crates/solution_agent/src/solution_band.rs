//! `SolutionBand`: the full-width dialog region rendered between the
//! project zone and the status bar (`Workspace::solution_band_item`, phase
//! 2a task 1). Shows the `SolutionSessionView` for
//! `SolutionAgentStore::active_dialog_session` beside the utility section
//! (the terminal panel, phase 2a task 6) — `None` dialog AND a hidden
//! utility section together collapse the band to a zero-height `div` so the
//! project zone reclaims the space. The draggable divider + persisted
//! width/collapse state land in task 7 — for now the two halves split the
//! band evenly when both are shown.
//!
//! The utility section's content comes from `Workspace::solution_band_utility_item`,
//! a type-erased `AnyView` slot set by `zed.rs` — NOT a typed
//! `Entity<ConsolePanel>` field on this struct, because `console_panel`
//! already depends on `solution_agent` (for `SolutionAgentStore`); the
//! reverse dependency this struct would otherwise need would cycle. This
//! band reads the slot fresh every render instead of caching a copy, so it
//! never goes stale relative to whatever `zed.rs` last installed there.
//!
//! Installed from `crates/zed/src/zed.rs` (NOT `title_bar`, which cannot
//! depend on `solution_agent` — see the `SessionTabStrip` install a few
//! lines above it for the same reasoning) inside the
//! `cx.observe_new(|workspace: &mut Workspace, window, cx| ...)` closure
//! that also installs `SessionTabStrip`. That closure already holds
//! `&mut Workspace`, so `SolutionBand::new` takes only a `WeakEntity<Workspace>`
//! and never reads the entity during construction — reading it there would
//! double-lease-panic (the same trap `ProjectToolbar::new` sidesteps by
//! taking `&Workspace` as a plain parameter instead of upgrading a weak
//! handle).

use std::collections::HashMap;

use gpui::{
    AnyView, App, AppContext as _, Context, Entity, FocusHandle, IntoElement, ParentElement,
    Render, Styled, Subscription, WeakEntity, Window, div,
};
use solutions::{SolutionId, SolutionStore};
use ui::h_flex;
use workspace::Workspace;

use crate::model::SolutionSessionId;
use crate::session_view::SolutionSessionView;
use crate::store::{SolutionAgentStore, SolutionAgentStoreEvent};

pub struct SolutionBand {
    workspace: WeakEntity<Workspace>,
    /// `SolutionSessionView::new` constructs a real `editor::Editor` for the
    /// compose box, so rebuilding one every paint is out of the question —
    /// cache by session id and reuse across renders. Evicted on
    /// `SessionClosed` so a closed session's view (and the subscriptions/
    /// editor entity it holds) doesn't linger past the session's lifetime.
    views: HashMap<SolutionSessionId, Entity<SolutionSessionView>>,
    /// Whether the utility section (terminal) is shown. Independent of
    /// whether a dialog is active — pressing `ctrl-\`` can reveal the
    /// terminal with no chat selected at all. Defaults to hidden, mirroring
    /// `Dock::new`'s `is_open: false` default that the console dock used to
    /// have before this task moved it out of the dock system.
    utility_visible: bool,
    _subscriptions: Vec<Subscription>,
}

impl SolutionBand {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let store = SolutionAgentStore::global(cx);
        let subscription = cx.subscribe(&store, |this, _store, event, cx| match event {
            SolutionAgentStoreEvent::ActiveDialogSessionChanged { .. } => cx.notify(),
            SolutionAgentStoreEvent::SessionClosed(id) => {
                if this.views.remove(id).is_some() {
                    cx.notify();
                }
            }
            _ => {}
        });

        Self {
            workspace,
            views: HashMap::new(),
            utility_visible: false,
            _subscriptions: vec![subscription],
        }
    }

    /// The utility section's content, fresh off `Workspace::solution_band_utility_item`
    /// — see the module doc for why this isn't a typed field on `Self`.
    fn utility_panel(&self, cx: &App) -> Option<AnyView> {
        self.workspace
            .upgrade()?
            .read(cx)
            .solution_band_utility_item()
    }

    /// Whether the utility section is currently shown, regardless of dialog
    /// state.
    pub fn utility_visible(&self) -> bool {
        self.utility_visible
    }

    pub fn set_utility_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.utility_visible == visible {
            return;
        }
        self.utility_visible = visible;
        cx.notify();
    }

    /// `console_panel::ToggleFocus`'s (`ctrl-\``) handler. Mirrors
    /// `Workspace::toggle_panel_focus`'s tri-state: hidden → show + focus;
    /// shown + already focused → hide; shown + unfocused → just focus.
    /// `focus_handle` is the caller's handle onto the concrete panel entity
    /// — this band only holds an `AnyView` for the utility section (see the
    /// module doc), so it cannot compute a `FocusHandle` itself.
    pub fn toggle_utility_focus(
        &mut self,
        focus_handle: &FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.utility_visible {
            self.utility_visible = true;
            focus_handle.focus(window, cx);
        } else if focus_handle.contains_focused(window, cx) {
            self.utility_visible = false;
        } else {
            focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    /// Walk the bound workspace's worktrees for the Solution that owns
    /// them. Mirrors `solutions_ui::project_tab_strip::solution_id_for_workspace`
    /// and `session_tab_strip::SessionTabStrip::active_solution_id` —
    /// duplicated rather than shared for the same cross-crate-cycle reason
    /// documented on the latter (`solution_agent` can't depend on
    /// `solutions_ui`). Safe to call from `render`/event callbacks (unlike
    /// from `new` — see the module doc comment): both run outside the
    /// `&mut Workspace` borrow that installs this band.
    fn solution_id(&self, cx: &App) -> Option<SolutionId> {
        let workspace = self.workspace.upgrade()?;
        let store = SolutionStore::global(cx);
        let store = store.read(cx);
        let project = workspace.read(cx).project().clone();
        project.read(cx).worktrees(cx).find_map(|tree| {
            store
                .solution_for_path(&tree.read(cx).abs_path())
                .map(|sol| sol.id)
        })
    }

    /// The session id whose dialog the band would show on the next render,
    /// resolved fresh from the store rather than cached — cheap (a worktree
    /// walk + two hashmap lookups) and avoids a second place that can go
    /// stale relative to `SolutionAgentStore::active_dialog_session`.
    fn resolve_active_session(&self, cx: &App) -> Option<SolutionSessionId> {
        let solution_id = self.solution_id(cx)?;
        SolutionAgentStore::global(cx)
            .read(cx)
            .active_dialog_session(solution_id)
    }

    /// Test-only mirror of what `render` would show, without needing a live
    /// `Window` to drive a draw. Production code always goes through
    /// `render`.
    #[cfg(test)]
    fn active_view(&self, cx: &App) -> Option<SolutionSessionId> {
        self.resolve_active_session(cx)
    }
}

impl Render for SolutionBand {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let session_id = self.resolve_active_session(cx);
        let utility_panel = if self.utility_visible {
            self.utility_panel(cx)
        } else {
            None
        };

        let dialog = session_id.and_then(|session_id| {
            if let Some(view) = self.views.get(&session_id) {
                return Some(view.clone());
            }
            let session = SolutionAgentStore::global(cx).read(cx).session(session_id)?;
            let view = cx.new(|cx| {
                SolutionSessionView::new(session_id, session, self.workspace.clone(), window, cx)
            });
            self.views.insert(session_id, view.clone());
            Some(view)
        });

        // Neither half has anything to show — collapse to zero height so the
        // project zone reclaims the space, same as the dialog-only behaviour
        // this replaces. A stale `session_id` racing a concurrent session
        // removal falls in here too: `clear_active_dialog_for_session` emits
        // `ActiveDialogSessionChanged` momentarily and this frame's absence
        // self-corrects.
        if dialog.is_none() && utility_panel.is_none() {
            return div().into_any_element();
        }

        // Fixed even split when both halves are present — the draggable
        // divider and persisted width are task 7.
        h_flex()
            .w_full()
            .children(dialog.map(|view| div().flex_1().min_w_0().child(view)))
            .children(utility_panel.map(|panel| div().flex_1().min_w_0().child(panel)))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{InteractiveElement, TestAppContext};
    use std::sync::Arc;

    #[gpui::test]
    async fn the_band_is_absent_when_no_dialog_is_active(cx: &mut TestAppContext) {
        let (_solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;

        cx.update(|cx| {
            // `Workspace::new` calls `theme_settings::track_window_appearance`,
            // which requires `GlobalSystemAppearance` to be initialized —
            // mirrors `compact::tests::cold_compact_queues_prompt_and_kicks_resume`.
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            let registry = Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });

        let (workspace, cx) =
            cx.add_window_view(|window, cx| workspace::Workspace::test_new(project, window, cx));

        let band = workspace.update_in(cx, |workspace, _window, cx| {
            cx.new(|cx| SolutionBand::new(workspace.weak_handle(), cx))
        });

        band.read_with(cx, |band, cx| {
            assert!(
                band.active_view(cx).is_none(),
                "a fresh Solution has no active dialog session, so the band must render nothing"
            );
        });
    }

    /// Bare view that tracks a `FocusHandle`, standing in for `ConsolePanel`'s
    /// own `.track_focus(&self.focus_handle)` (`crates/console_panel/src/panel.rs`).
    /// Also the window `SolutionBand` gets constructed against in the test
    /// below (via a nested `cx.new` inside this view's own `update_in`) —
    /// `Entity::update_in` resolves the window an entity was CREATED in
    /// (`with_window(entity_id, ..)`), not whichever `VisualTestContext`
    /// happens to be holding the call, so `band` must be built here rather
    /// than in `Workspace`'s window for its `window: &mut Window` to be
    /// *this* probe's window. That matters because `Workspace` runs its own
    /// focus-management on every redraw (e.g. refocusing its center pane
    /// when nothing else claims the window's focus) — sharing its window
    /// with the probe made the handle's focus bounce back within a frame.
    struct FocusProbeRoot(FocusHandle);

    impl Render for FocusProbeRoot {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().track_focus(&self.0).size_full()
        }
    }

    #[gpui::test]
    async fn toggle_utility_focus_shows_focuses_then_hides(cx: &mut TestAppContext) {
        let (_solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;

        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            let registry = Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });

        // `SolutionBand::new` only needs a `WeakEntity<Workspace>` for its
        // resolve-active-session path, which `toggle_utility_focus` never
        // touches — build the Workspace in its own window, purely to mint a
        // weak handle for `band`'s constructor (see `FocusProbeRoot`'s doc
        // comment for why `band` itself must NOT live in this window).
        let workspace_window =
            cx.add_window(|window, cx| workspace::Workspace::test_new(project, window, cx));
        let weak_workspace = workspace_window
            .update(cx, |workspace, _window, _cx| workspace.weak_handle())
            .unwrap();

        let (probe, cx) = cx.add_window_view(|_window, cx| FocusProbeRoot(cx.focus_handle()));
        let focus_handle = probe.read_with(cx, |probe, _| probe.0.clone());
        let band = probe
            .update_in(cx, |_probe, _window, cx| {
                cx.new(|cx| SolutionBand::new(weak_workspace, cx))
            });

        assert!(
            !band.read_with(cx, |band, _| band.utility_visible()),
            "the utility section starts hidden, mirroring Dock::new's is_open: false default"
        );

        band.update_in(cx, |band, window, cx| {
            band.toggle_utility_focus(&focus_handle, window, cx);
        });
        assert!(
            band.read_with(cx, |band, _| band.utility_visible()),
            "first toggle on a hidden section shows it"
        );
        band.update_in(cx, |_band, window, cx| {
            assert!(
                focus_handle.contains_focused(window, cx),
                "first toggle also focuses the handle, matching Workspace::toggle_panel_focus"
            );
        });

        band.update_in(cx, |band, window, cx| {
            band.toggle_utility_focus(&focus_handle, window, cx);
        });
        assert!(
            !band.read_with(cx, |band, _| band.utility_visible()),
            "second toggle, while already focused, hides the section again"
        );
    }
}
