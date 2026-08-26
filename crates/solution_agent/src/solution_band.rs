//! `SolutionBand`: the full-width dialog region rendered between the
//! project zone and the status bar (`Workspace::solution_band_item`, phase
//! 2a task 1). Shows the `SolutionSessionView` for
//! `SolutionAgentStore::active_dialog_session` beside the utility section
//! (the terminal panel, phase 2a task 6) — `None` dialog AND a hidden
//! utility section together collapse the band to a zero-height `div` so the
//! project zone reclaims the space. When both are shown a draggable divider
//! sits between them; its position, the utility section's visibility, and
//! the active dialog are one persisted `BandState` row per Solution
//! (task 7), so the band reopens the way the user left it.
//!
//! **The band owns no geometry of its own for a Solution workspace** — it
//! reads and writes `SolutionAgentStore::band_state`. `local_state` is the
//! fallback for a plain-folder window that resolves to no Solution at all
//! (a supported case here: `console_panel::panel::workspace_has_project`
//! has an explicit non-Solution branch, and `ctrl-\`` must keep working
//! there). There is no key to persist that under, so it lives and dies with
//! the window.
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
//! `&mut Workspace`, so `SolutionBand::new` takes a `WeakEntity<Workspace>`
//! plus the already-borrowed `Entity<Project>` and never reads the
//! Workspace entity during construction — reading it there would
//! double-lease-panic (the same trap `ProjectToolbar::new` sidesteps by
//! taking `&Workspace` as a plain parameter instead of upgrading a weak
//! handle).
//!
//! The same trap is why `solution_id` resolves the Solution off the
//! **Project** entity rather than off the Workspace: `set_utility_visible`
//! and `toggle_utility_focus` are both called from inside a live
//! `&mut Workspace` borrow (`console_panel::handle_toggle_focus` runs under
//! `workspace.register_action`; `panel::reveal_utility_section` runs inside
//! `workspace.update_in`), so anything they reach must not touch the
//! Workspace entity. Project is safe there — `panel::active_solution_id_for_workspace`
//! already reads it under exactly that borrow. `utility_panel` still reads
//! the Workspace, and is therefore only ever called from `render`.

use std::collections::HashMap;

use gpui::{
    AnyView, App, AppContext as _, ClickEvent, Context, DefiniteLength, DragMoveEvent, Entity,
    FocusHandle, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window, deferred, div, px,
};
use project::Project;
use solutions::{SolutionId, SolutionStore};
use ui::h_flex;
use ui::prelude::ActiveTheme as _;
use workspace::Workspace;

use crate::model::{BandState, DEFAULT_DIVIDER_RATIO, SolutionSessionId, clamp_divider_ratio};
use crate::session_view::SolutionSessionView;
use crate::store::{SolutionAgentStore, SolutionAgentStoreEvent};

/// Drag payload for the band's divider. Carries nothing — the position comes
/// from `DragMoveEvent::event.position` relative to the container's bounds.
#[derive(Debug, Clone)]
struct DraggedBandDivider;

/// Half-width of the divider's grab area, in logical pixels either side of
/// the 1px line it paints. Widening the hitbox without widening the paint is
/// what makes a hairline divider actually draggable.
const DIVIDER_HIT_SLOP: f32 = 3.;

pub struct SolutionBand {
    workspace: WeakEntity<Workspace>,
    /// The workspace's project, handed in by the installer rather than read
    /// back off the Workspace entity — see the module doc's double-lease
    /// note. This is what `solution_id` walks.
    project: Entity<Project>,
    /// `SolutionSessionView::new` constructs a real `editor::Editor` for the
    /// compose box, so rebuilding one every paint is out of the question —
    /// cache by session id and reuse across renders. Evicted on
    /// `SessionClosed` so a closed session's view (and the subscriptions/
    /// editor entity it holds) doesn't linger past the session's lifetime.
    views: HashMap<SolutionSessionId, Entity<SolutionSessionView>>,
    /// Band geometry for a window that resolves to no Solution. See the
    /// module doc: it cannot be persisted (no key), and it is never consulted
    /// once `solution_id` resolves.
    local_state: BandState,
    _subscriptions: Vec<Subscription>,
}

impl SolutionBand {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = SolutionAgentStore::global(cx);
        let subscription = cx.subscribe(&store, |this, _store, event, cx| match event {
            SolutionAgentStoreEvent::ActiveDialogSessionChanged { .. }
            | SolutionAgentStoreEvent::BandStateChanged { .. } => cx.notify(),
            SolutionAgentStoreEvent::SessionClosed(id) => {
                if this.views.remove(id).is_some() {
                    cx.notify();
                }
            }
            _ => {}
        });

        Self {
            workspace,
            project,
            views: HashMap::new(),
            local_state: BandState::default(),
            _subscriptions: vec![subscription],
        }
    }

    /// The utility section's content, fresh off `Workspace::solution_band_utility_item`
    /// — see the module doc for why this isn't a typed field on `Self`.
    /// Reads the Workspace entity, so this is `render`-only.
    fn utility_panel(&self, cx: &App) -> Option<AnyView> {
        self.workspace
            .upgrade()?
            .read(cx)
            .solution_band_utility_item()
    }

    /// This band's geometry: the owning Solution's persisted row, or the
    /// window-local fallback when there is no Solution.
    fn band_state(&self, cx: &App) -> BandState {
        match self.solution_id(cx) {
            Some(solution_id) => SolutionAgentStore::global(cx)
                .read(cx)
                .band_state(solution_id),
            None => self.local_state,
        }
    }

    /// Whether the utility section is currently shown, regardless of dialog
    /// state.
    pub fn utility_visible(&self, cx: &App) -> bool {
        self.band_state(cx).utility_visible
    }

    pub fn set_utility_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        match self.solution_id(cx) {
            Some(solution_id) => SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_band_utility_visible(solution_id, visible, cx);
            }),
            None => {
                if self.local_state.utility_visible == visible {
                    return;
                }
                self.local_state.utility_visible = visible;
            }
        }
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
        if !self.utility_visible(cx) {
            self.set_utility_visible(true, cx);
            focus_handle.focus(window, cx);
        } else if focus_handle.contains_focused(window, cx) {
            self.set_utility_visible(false, cx);
        } else {
            focus_handle.focus(window, cx);
            cx.notify();
        }
    }

    /// Move the divider. `solution_id` is passed in rather than re-resolved
    /// because the render pass that installed this callback already resolved
    /// it for the same frame.
    fn set_divider_ratio(
        &mut self,
        solution_id: Option<SolutionId>,
        ratio: f32,
        cx: &mut Context<Self>,
    ) {
        match solution_id {
            Some(solution_id) => SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_band_divider_ratio(solution_id, ratio, cx);
            }),
            None => {
                let ratio = clamp_divider_ratio(ratio);
                if self.local_state.divider_ratio == ratio {
                    return;
                }
                self.local_state.divider_ratio = ratio;
            }
        }
        cx.notify();
    }

    /// Track the divider under the cursor for the whole drag. The ratio is
    /// committed continuously here rather than on drop because the handle
    /// sets `block_mouse_except_scroll` under `deferred()`, which truncates
    /// the hover stack so no ancestor's `on_drop` ever fires — the same trap
    /// `editor::split_editor_view` documents (FORK.md #84). Do not "fix"
    /// this back to `on_drop`.
    fn on_divider_drag_move(
        &mut self,
        solution_id: Option<SolutionId>,
        event: &DragMoveEvent<DraggedBandDivider>,
        cx: &mut Context<Self>,
    ) {
        let bounds = event.bounds;
        let width = bounds.right() - bounds.left();
        if width <= px(0.) {
            return;
        }
        let ratio = (event.event.position.x - bounds.left()) / width;
        self.set_divider_ratio(solution_id, ratio, cx);
    }

    /// Walk the project's worktrees for the Solution that owns them. Mirrors
    /// `solutions_ui::project_tab_strip::solution_id_for_workspace` and
    /// `session_tab_strip::SessionTabStrip::active_solution_id` — duplicated
    /// rather than shared for the same cross-crate-cycle reason documented on
    /// the latter (`solution_agent` can't depend on `solutions_ui`). Reads
    /// the Project entity, never the Workspace, so it is safe to call from
    /// the mutators that run under a live `&mut Workspace` borrow (module
    /// doc).
    fn solution_id(&self, cx: &App) -> Option<SolutionId> {
        let store = SolutionStore::try_global(cx)?;
        let store = store.read(cx);
        self.project.read(cx).worktrees(cx).find_map(|tree| {
            store
                .solution_for_path(&tree.read(cx).abs_path())
                .map(|sol| sol.id)
        })
    }

    /// Test-only mirror of what `render` would show, without needing a live
    /// `Window` to drive a draw. Resolved fresh from the store rather than
    /// cached, exactly as `render` does. Production code always goes through
    /// `render`.
    #[cfg(test)]
    fn active_view(&self, cx: &App) -> Option<SolutionSessionId> {
        self.band_state(cx).active_dialog_session
    }

    /// The 1px rule between the halves plus its oversized grab handle.
    /// Rendered only when both halves are present — with one half filling the
    /// band there is nothing to divide.
    fn render_divider(
        &self,
        solution_id: Option<SolutionId>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex_none()
            .self_stretch()
            .w(px(1.))
            .bg(cx.theme().colors().border)
            .relative()
            // Deferred so the grab handle's hitbox is painted above whichever
            // half is painted last, the same treatment the dock resize handle
            // and the split-diff divider get.
            .child(deferred(
                div()
                    .id("solution-band-divider-handle")
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(-DIVIDER_HIT_SLOP))
                    .w(px(DIVIDER_HIT_SLOP * 2. + 1.))
                    .cursor_col_resize()
                    .block_mouse_except_scroll()
                    .on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
                        if event.click_count() >= 2 {
                            this.set_divider_ratio(solution_id, DEFAULT_DIVIDER_RATIO, cx);
                        }
                        cx.stop_propagation();
                    }))
                    .on_drag(DraggedBandDivider, |_, _, _, cx| cx.new(|_| gpui::Empty)),
            ))
    }
}

impl Render for SolutionBand {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let solution_id = self.solution_id(cx);
        let state = self.band_state(cx);
        let utility_panel = if state.utility_visible {
            self.utility_panel(cx)
        } else {
            None
        };

        let dialog = state.active_dialog_session.and_then(|session_id| {
            if let Some(view) = self.views.get(&session_id) {
                return Some(view.clone());
            }
            let session = SolutionAgentStore::global(cx)
                .read(cx)
                .session(session_id)?;
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

        let split = dialog.is_some() && utility_panel.is_some();
        // `flex_basis` fractions only when there are two halves to divide;
        // a lone half takes the whole band regardless of the stored ratio, so
        // hiding the other side never leaves a dead gutter.
        let half = |content: AnyView, fraction: f32| {
            let half = div().min_w_0().overflow_hidden();
            if split {
                half.flex_shrink_1()
                    .flex_basis(DefiniteLength::Fraction(fraction))
            } else {
                half.flex_1()
            }
            .child(content)
        };

        h_flex()
            .id("solution-band")
            .w_full()
            // `h_flex` centres its children; the halves and the divider must
            // instead fill the band's height.
            .items_stretch()
            .on_drag_move::<DraggedBandDivider>(cx.listener(
                move |this, event: &DragMoveEvent<DraggedBandDivider>, _window, cx| {
                    this.on_divider_drag_move(solution_id, event, cx);
                },
            ))
            .children(dialog.map(|view| half(view.into(), state.divider_ratio)))
            .children(split.then(|| self.render_divider(solution_id, cx).into_any_element()))
            .children(utility_panel.map(|panel| half(panel, 1.0 - state.divider_ratio)))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SolutionAgentDb;
    use crate::model::{MAX_DIVIDER_RATIO, MIN_DIVIDER_RATIO};
    use gpui::TestAppContext;
    use std::sync::Arc;

    #[test]
    fn divider_ratio_is_clamped_to_a_usable_range() {
        assert_eq!(clamp_divider_ratio(-1.0), MIN_DIVIDER_RATIO);
        assert_eq!(clamp_divider_ratio(2.0), MAX_DIVIDER_RATIO);
        assert_eq!(clamp_divider_ratio(0.5), 0.5);
        assert_eq!(
            clamp_divider_ratio(f32::NAN),
            DEFAULT_DIVIDER_RATIO,
            "NaN survives `f32::clamp`, so it must be folded to the default \
             before it can reach a `flex_basis` fraction"
        );
    }

    /// Store + in-memory DB, wired together and settled. The tempdir must
    /// outlive the test: it holds the on-disk solution the store resolves
    /// paths under.
    async fn store_with_persistence(
        cx: &mut TestAppContext,
    ) -> (
        Entity<SolutionAgentStore>,
        Arc<SolutionAgentDb>,
        tempfile::TempDir,
    ) {
        let (_solution_id, tmp, _project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            let registry = Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });
        let store = cx.update(|cx| SolutionAgentStore::global(cx));
        let db = Arc::new(SolutionAgentDb::open(cx.executor()).expect("open db"));
        store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));
        cx.run_until_parked();
        (store, db, tmp)
    }

    /// What is actually on disk for `solution_id` right now, or `None` when
    /// no row has been written yet.
    async fn persisted(db: &Arc<SolutionAgentDb>, solution_id: SolutionId) -> Option<BandState> {
        db.load_band_states()
            .await
            .expect("load band states")
            .into_iter()
            .find(|(id, _)| *id == solution_id)
            .map(|(_, state)| state)
    }

    /// Ruling-5 invariant (a): the ratio's write is cancel-on-replace, not
    /// one queued write per `on_drag_move`. Proven by timing rather than by
    /// the stored value — the debounced write re-reads the state after its
    /// wait, so an uncancelled first write would store the *same* final ratio
    /// and be invisible in the row's contents. What it cannot fake is landing
    /// 400 ms after the FIRST call instead of 400 ms after the last one.
    #[gpui::test]
    async fn a_replaced_divider_drag_cancels_the_write_it_supersedes(cx: &mut TestAppContext) {
        let (store, db, _tmp) = store_with_persistence(cx).await;
        let solution_id = SolutionId(1);

        // Seed a row so "nothing written yet" is distinguishable from "wrote
        // the new ratio" — an absent row would make the assertion below pass
        // for the wrong reason.
        store.update(cx, |store, cx| {
            store.set_band_utility_visible(solution_id, true, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            persisted(&db, solution_id).await.map(|s| s.divider_ratio),
            Some(DEFAULT_DIVIDER_RATIO)
        );

        store.update(cx, |store, cx| {
            store.set_band_divider_ratio(solution_id, 0.7, cx)
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        store.update(cx, |store, cx| {
            store.set_band_divider_ratio(solution_id, 0.8, cx)
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();

        assert_eq!(
            persisted(&db, solution_id).await.map(|s| s.divider_ratio),
            Some(DEFAULT_DIVIDER_RATIO),
            "600ms after the first drag but only 300ms after the second, nothing \
             may have been written yet: the first drag's timer must have been \
             cancelled rather than left to fire at its own 400ms mark"
        );

        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        assert_eq!(
            persisted(&db, solution_id).await.map(|s| s.divider_ratio),
            Some(0.8),
            "and the surviving timer writes the position the drag ended on"
        );
    }

    /// Ruling-5 invariant (b): an immediate write (`utility_visible` /
    /// `active_dialog_session`) drops the pending ratio write, so it must
    /// carry that ratio itself. If it wrote a snapshot captured before the
    /// drag instead, dragging the divider and then hitting `ctrl-\`` would
    /// silently roll the divider back on the next restart.
    #[gpui::test]
    async fn an_immediate_band_write_carries_the_pending_divider_ratio(cx: &mut TestAppContext) {
        let (store, db, _tmp) = store_with_persistence(cx).await;
        let solution_id = SolutionId(1);

        store.update(cx, |store, cx| {
            store.set_band_divider_ratio(solution_id, 0.8, cx)
        });
        // No clock advance — the ratio's write is still parked when the
        // immediate write below drops it.
        store.update(cx, |store, cx| {
            store.set_band_utility_visible(solution_id, true, cx)
        });
        cx.run_until_parked();

        let written = persisted(&db, solution_id)
            .await
            .expect("the immediate write lands without waiting on the debounce");
        assert_eq!(
            written.divider_ratio, 0.8,
            "the immediate write must persist the CURRENT state, including the \
             ratio whose debounced write it just cancelled"
        );
        assert!(written.utility_visible);

        // The cancelled timer must stay cancelled — no late write resurrecting
        // a stale snapshot over the row that just landed.
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(persisted(&db, solution_id).await, Some(written));
    }

    /// Store + in-memory DB with persistence NOT yet wired, plus a band row
    /// already on disk — the state the app is in while
    /// `SolutionAgentDb::connect` and `run_identity_migration` are still
    /// awaiting and the user can already press `ctrl-\``.
    async fn store_with_a_saved_row_and_no_persistence_yet(
        cx: &mut TestAppContext,
    ) -> (
        Entity<SolutionAgentStore>,
        Arc<SolutionAgentDb>,
        BandState,
        tempfile::TempDir,
    ) {
        let (_solution_id, tmp, _project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            let registry = Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });
        let store = cx.update(|cx| SolutionAgentStore::global(cx));
        let db = Arc::new(SolutionAgentDb::open(cx.executor()).expect("open db"));
        let saved = BandState {
            divider_ratio: 0.7,
            utility_visible: false,
            active_dialog_session: Some(SolutionSessionId::new()),
        };
        db.save_band_state(SolutionId(1), saved)
            .await
            .expect("seed the saved row");
        (store, db, saved, tmp)
    }

    /// The pre-hydration mutation must not cost the user their saved geometry.
    /// `set_persistence` is gated behind two sequential DB awaits, and
    /// `ctrl-\`` inside that window used to occupy the map key with a
    /// defaults-seeded entry — after which the merge skipped the Solution and
    /// the divider position and selected chat were gone from memory and, on
    /// the next write, from disk.
    #[gpui::test]
    async fn a_band_touch_before_the_db_opens_keeps_the_persisted_row(cx: &mut TestAppContext) {
        let (store, db, saved, _tmp) = store_with_a_saved_row_and_no_persistence_yet(cx).await;
        let solution_id = SolutionId(1);

        store.update(cx, |store, cx| {
            store.set_band_utility_visible(solution_id, true, cx)
        });
        store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));
        cx.run_until_parked();

        let expected = BandState {
            utility_visible: true,
            ..saved
        };
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.band_state(solution_id),
                expected,
                "the fields the user did not touch must come from the saved row, \
                 and the one they did touch must survive the merge"
            );
        });
        assert_eq!(
            persisted(&db, solution_id).await,
            Some(expected),
            "and the row that lands on disk must not be a defaults-seeded one"
        );
    }

    /// The same hazard one step later: persistence is wired but the SELECT has
    /// not come back yet, so a write here would flatten the row the merge is
    /// about to read.
    #[gpui::test]
    async fn a_band_touch_while_hydration_is_in_flight_keeps_the_persisted_row(
        cx: &mut TestAppContext,
    ) {
        let (store, db, saved, _tmp) = store_with_a_saved_row_and_no_persistence_yet(cx).await;
        let solution_id = SolutionId(1);

        store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));
        // Deliberately no `run_until_parked` before the mutation.
        store.update(cx, |store, cx| {
            store.set_band_utility_visible(solution_id, true, cx)
        });
        cx.run_until_parked();

        let expected = BandState {
            utility_visible: true,
            ..saved
        };
        store.read_with(cx, |store, _| {
            assert_eq!(store.band_state(solution_id), expected);
        });
        assert_eq!(persisted(&db, solution_id).await, Some(expected));
    }

    #[gpui::test]
    async fn band_geometry_round_trips_per_solution(cx: &mut TestAppContext) {
        let (_solution_id, _tmp, _project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            let registry = Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });
        let store = cx.update(|cx| SolutionAgentStore::global(cx));
        let db = Arc::new(SolutionAgentDb::open(cx.executor()).expect("open db"));
        store.update(cx, |store, cx| store.set_persistence(db, cx));
        cx.run_until_parked();

        let solution_a = SolutionId(1);
        let solution_b = SolutionId(2);
        let dialog_a = SolutionSessionId::new();

        store.update(cx, |store, cx| {
            store.set_active_dialog_session(solution_a, Some(dialog_a), cx);
            store.set_band_utility_visible(solution_a, true, cx);
            store.set_active_dialog_session(solution_b, None, cx);
            store.set_band_utility_visible(solution_b, false, cx);
            // Last, so the debounced ratio write isn't cancelled by the
            // immediate writes above — this is the path the drag takes.
            store.set_band_divider_ratio(solution_a, 0.7, cx);
            store.set_band_divider_ratio(solution_b, 0.25, cx);
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();

        // Simulate a restart: drop the in-memory geometry and re-run the
        // hydration `set_persistence` performs at startup, against a fresh
        // handle onto the same database file.
        store.update(cx, |store, _cx| {
            store.forget_band_state(solution_a);
            store.forget_band_state(solution_b);
        });
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.band_state(solution_a),
                BandState::default(),
                "the cold-start precondition: without the reload below there is \
                 nothing left in memory, so the assertions after it can only \
                 pass by way of the persisted rows"
            );
        });
        let reopened = Arc::new(SolutionAgentDb::open(cx.executor()).expect("reopen db"));
        store.update(cx, |store, cx| store.set_persistence(reopened, cx));
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let a = store.band_state(solution_a);
            let b = store.band_state(solution_b);
            assert_eq!(a.divider_ratio, 0.7);
            assert!(a.utility_visible);
            assert_eq!(a.active_dialog_session, Some(dialog_a));
            assert_eq!(
                b.divider_ratio, 0.25,
                "solution B keeps its own divider position"
            );
            assert!(
                !b.utility_visible,
                "solution A's visible utility section must not leak into B"
            );
            assert_eq!(
                b.active_dialog_session, None,
                "solution A's active dialog must not leak into B"
            );
        });

        // A solution that was never touched still reads as the defaults
        // rather than inheriting whichever row happened to load last.
        store.read_with(cx, |store, _| {
            assert_eq!(store.band_state(SolutionId(3)), BandState::default());
        });
    }

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
            let project = workspace.project().clone();
            cx.new(|cx| SolutionBand::new(workspace.weak_handle(), project, cx))
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

        // `SolutionBand::new` only needs the `WeakEntity<Workspace>` for its
        // utility-panel slot lookup, which `toggle_utility_focus` never
        // touches — build the Workspace in its own window, purely to mint a
        // weak handle for `band`'s constructor (see `FocusProbeRoot`'s doc
        // comment for why `band` itself must NOT live in this window).
        let workspace_window =
            cx.add_window(|window, cx| workspace::Workspace::test_new(project, window, cx));
        let (weak_workspace, band_project) = workspace_window
            .update(cx, |workspace, _window, _cx| {
                (workspace.weak_handle(), workspace.project().clone())
            })
            .unwrap();

        let (probe, cx) = cx.add_window_view(|_window, cx| FocusProbeRoot(cx.focus_handle()));
        let focus_handle = probe.read_with(cx, |probe, _| probe.0.clone());
        let band = probe.update_in(cx, |_probe, _window, cx| {
            cx.new(|cx| SolutionBand::new(weak_workspace, band_project, cx))
        });

        assert!(
            !band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "the utility section starts hidden, mirroring Dock::new's is_open: false default"
        );

        band.update_in(cx, |band, window, cx| {
            band.toggle_utility_focus(&focus_handle, window, cx);
        });
        assert!(
            band.read_with(cx, |band, cx| band.utility_visible(cx)),
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
            !band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "second toggle, while already focused, hides the section again"
        );
    }
}
