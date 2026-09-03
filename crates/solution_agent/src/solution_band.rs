//! `SolutionBand`: the full-width dialog region rendered between the
//! project zone and the status bar (`Workspace::solution_band_item`, phase
//! 2a task 1). Shows the `SolutionSessionView` for
//! `SolutionAgentStore::active_dialog_session` beside the utility section
//! (the terminal panel, phase 2a task 6) — `None` dialog AND a hidden
//! utility section together collapse the band to a zero-height `div` so the
//! project zone reclaims the space. When both are shown a draggable divider
//! sits between them; its position, the utility section's visibility, and
//! the active dialog are one persisted `BandState` row per Solution
//! (task 7), so the band reopens the way the user left it. The band's
//! height is the same kind of persisted geometry: `render` paints the root
//! at `effective_band_height(state.height, viewport_height)` — the stored
//! height clamped against the live window so the band can never eat the
//! whole viewport — and a draggable top-edge handle (mirroring the
//! divider's own drag/double-click-reset pattern) lets the user resize it.
//! The handle paints no visible line of its own; the band's row boundary
//! above it is what a user currently has to find the edge by.
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
//! a type-erased `AnyView` slot keyed by `UtilityKind` and set by `zed.rs`
//! — NOT a typed `Entity<ConsolePanel>` field on this struct, because
//! `console_panel` already depends on `solution_agent` (for
//! `SolutionAgentStore`); the reverse dependency this struct would
//! otherwise need would cycle. This band reads the slot fresh every render
//! instead of caching a copy, so it never goes stale relative to whatever
//! `zed.rs` last installed there. As of phase 2b task 1 the slot holds one
//! entry per kind (terminal / git graph / debug); `render` asks for
//! whichever kind `BandState::utility_kind` (task 2) holds. The buttons that
//! let a user pick a kind arrive in a later task; until then the only writer
//! is `debugger_ui` (task 5), whose `debug_panel::ToggleFocus` and
//! breakpoint-hit reveal call `set_utility_kind` because "open my dock and
//! activate me" has no other translation into the band.
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
    FocusHandle, Focusable as _, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window, deferred, div, px,
};
use project::Project;
use solutions::mcp::{StructureSlot, VisualNode, register_structure_provider};
use solutions::{SolutionId, SolutionStore};
use ui::prelude::*;
use ui::{Color, Label, LabelSize, h_flex, v_flex};
use workspace::{UtilityKind, Workspace};

use crate::model::{
    BandState, DEFAULT_BAND_HEIGHT, DEFAULT_DIVIDER_RATIO, SolutionSessionId, clamp_band_height,
    clamp_divider_ratio, effective_band_height,
};
use crate::session_view::SolutionSessionView;
use crate::store::{SolutionAgentStore, SolutionAgentStoreEvent};

/// Drag payload for the band's divider. Carries nothing — the position comes
/// from `DragMoveEvent::event.position` relative to the container's bounds.
#[derive(Debug, Clone)]
struct DraggedBandDivider;

/// Drag payload for the band's top-edge resize handle. Carries nothing, same
/// as `DraggedBandDivider` — the height comes from `DragMoveEvent::bounds`
/// (the band root's hitbox) and the cursor's `event.position`.
#[derive(Debug, Clone)]
struct DraggedBandEdge;

/// Half-width of the divider's grab area, in logical pixels either side of
/// the 1px line it paints. Widening the hitbox without widening the paint is
/// what makes a hairline divider actually draggable.
const DIVIDER_HIT_SLOP: f32 = 3.;

/// Whether the status-bar button for `kind` renders selected. A hidden
/// utility section has **no** selected button even though `utility_kind`
/// still remembers what it was showing (spec §3) — that memory is what makes
/// re-showing return to the same content, and surfacing it as a lit button
/// over an invisible section would be a lie.
pub fn utility_button_selected(kind: UtilityKind, state: &BandState) -> bool {
    state.utility_visible && state.utility_kind == kind
}

/// What clicking the utility button for `kind` must produce, as a pure
/// function of the current band state: `(utility_kind, utility_visible)`.
///
/// Clicking the **selected** button hides the section and deliberately
/// leaves `utility_kind` at its current value, so the next reveal — by
/// button, by `ctrl-\``, or by a task that reveals the terminal — returns to
/// the same content. Clicking any other button switches to it and shows the
/// section, which also covers "the section is hidden, so nothing is
/// selected": every button then reads as inactive and one click shows it.
pub fn utility_button_click(kind: UtilityKind, state: &BandState) -> (UtilityKind, bool) {
    if utility_button_selected(kind, state) {
        (state.utility_kind, false)
    } else {
        (kind, true)
    }
}

/// The utility half when the selected kind has no registered occupant. See
/// the call site in `render` for why this exists rather than an empty half.
fn render_missing_occupant(kind: UtilityKind) -> gpui::AnyElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_1()
        .child(Label::new(format!("{} is unavailable", kind.label())).color(Color::Muted))
        .child(
            Label::new("It failed to load in this window.")
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .into_any_element()
}

/// Half-height of the top-edge handle's grab area, in logical pixels either
/// side of the band's top edge. The edge paints no line of its own (module
/// doc / band's own row boundary), so this only widens the hitbox.
const BAND_EDGE_HIT_SLOP: f32 = 3.;

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

    /// The utility section's content for `kind`, fresh off
    /// `Workspace::solution_band_utility_item` — see the module doc for why
    /// this isn't a typed field on `Self`. Reads the Workspace entity, so
    /// this is `render`-only.
    fn utility_panel(&self, kind: UtilityKind, cx: &App) -> Option<AnyView> {
        self.workspace
            .upgrade()?
            .read(cx)
            .solution_band_utility_item(kind)
    }

    /// Did `kind`'s occupant resolve with an error, as opposed to simply not
    /// having landed yet? The three load concurrently, so absence from the
    /// slot map alone cannot answer this — see `render` and
    /// `Workspace::solution_band_utility_unavailable`. Reads the Workspace
    /// entity, so this is `render`-only.
    fn utility_panel_unavailable(&self, kind: UtilityKind, cx: &App) -> bool {
        self.workspace
            .upgrade()
            .is_some_and(|workspace| workspace.read(cx).solution_band_utility_unavailable(kind))
    }

    /// This band's geometry: the owning Solution's persisted row, or the
    /// window-local fallback when there is no Solution. Reads the Project
    /// entity (never the Workspace), so it is safe under a live
    /// `&mut Workspace` borrow — see the module doc.
    pub fn band_state(&self, cx: &App) -> BandState {
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

    /// Which content the utility section is currently showing.
    pub fn utility_kind(&self, cx: &App) -> UtilityKind {
        self.band_state(cx).utility_kind
    }

    /// Choose which content the utility section shows. Callers are the two
    /// reveal/keybinding paths (`console_panel::handle_toggle_focus`,
    /// `debugger_ui::debugger_panel::{handle_toggle_focus, reveal_debug_panel}`,
    /// `console_panel::panel::reveal_utility_section`) and
    /// `activate_utility_kind` below, which is what the status-bar button
    /// group drives.
    pub fn set_utility_kind(&mut self, kind: UtilityKind, cx: &mut Context<Self>) {
        match self.solution_id(cx) {
            Some(solution_id) => SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_band_utility_kind(solution_id, kind, cx);
            }),
            None => {
                if self.local_state.utility_kind == kind {
                    return;
                }
                self.local_state.utility_kind = kind;
            }
        }
        cx.notify();
    }

    /// Apply [`utility_button_click`] — the status-bar button group's rule.
    /// Never touches focus: the keybindings (`ctrl-\``, `ctrl-shift-d`) are
    /// the focus path, a button click is only ever "show this content" or
    /// "hide the section". Both layers agree that *the active content* is
    /// `utility_kind` while `utility_visible`, so a button and its hotkey
    /// can never disagree about which content is current.
    ///
    /// That includes the case where the occupant being switched away from (or
    /// hidden) is the thing holding focus: it does not need releasing here.
    /// Unmounting it empties the rendered frame's focus path, which fires the
    /// window's focus-lost listeners, and `Workspace`'s re-focuses the active
    /// pane — the same target the dock path's `focus_or_unfocus_panel` used.
    /// Pinned by `console_panel`'s
    /// `switching_the_utility_kind_leaves_focus_on_the_centre_pane`, which
    /// also records why adding a release here would cost more than it buys.
    pub fn activate_utility_kind(&mut self, kind: UtilityKind, cx: &mut Context<Self>) {
        let (next_kind, next_visible) = utility_button_click(kind, &self.band_state(cx));
        // Order matters only for the switch case, and only cosmetically:
        // setting the kind first means a single frame never paints the old
        // content in a newly-shown section.
        self.set_utility_kind(next_kind, cx);
        self.set_utility_visible(next_visible, cx);
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

    /// Move the band's top edge. `solution_id` is passed in for the same
    /// reason as `set_divider_ratio`: the render pass that installed this
    /// callback already resolved it for the same frame.
    fn set_band_height(
        &mut self,
        solution_id: Option<SolutionId>,
        height: f32,
        cx: &mut Context<Self>,
    ) {
        match solution_id {
            Some(solution_id) => SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_band_height(solution_id, height, cx);
            }),
            None => {
                let height = clamp_band_height(height);
                if self.local_state.height == height {
                    return;
                }
                self.local_state.height = height;
            }
        }
        cx.notify();
    }

    /// Track the top edge under the cursor for the whole drag. Committed
    /// continuously here rather than on drop for the same reason as
    /// `on_divider_drag_move`: the handle sets `block_mouse_except_scroll`
    /// under `deferred()`, which truncates the hover stack so no ancestor's
    /// `on_drop` ever fires (FORK.md #84). Do not "fix" this back to
    /// `on_drop`.
    fn on_edge_drag_move(
        &mut self,
        solution_id: Option<SolutionId>,
        event: &DragMoveEvent<DraggedBandEdge>,
        cx: &mut Context<Self>,
    ) {
        // `event.bounds` is the band root's hitbox from the last paint. Its
        // BOTTOM edge is what's anchored (the status bar sits directly under
        // it), so the dragged height is measured from there up to the cursor
        // — measuring from the top edge would chase the value being changed.
        let height = event.bounds.bottom() - event.event.position.y;
        self.set_band_height(solution_id, f32::from(height), cx);
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

    /// Test-only mirror of the height `render` would use, without needing a
    /// live `Window` to drive a draw. See `active_view`'s doc for why this
    /// re-resolves fresh from the store rather than caching.
    #[cfg(test)]
    fn band_height(&self, cx: &App) -> f32 {
        self.band_state(cx).height
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

    /// The band's top-edge grab handle. Modelled on `render_divider`'s
    /// deferred grab area, but horizontal and along the band root's top
    /// edge rather than between the two halves. Paints no visible line of
    /// its own — see the module doc's note on this and the brief's caveat
    /// about verifying the edge is discoverable.
    fn render_top_edge_handle(
        &self,
        solution_id: Option<SolutionId>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        deferred(
            div()
                .id("solution-band-edge-handle")
                .absolute()
                .top(px(-BAND_EDGE_HIT_SLOP))
                .left_0()
                .right_0()
                .h(px(BAND_EDGE_HIT_SLOP * 2.))
                .cursor_row_resize()
                .block_mouse_except_scroll()
                .on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
                    if event.click_count() >= 2 {
                        this.set_band_height(solution_id, DEFAULT_BAND_HEIGHT, cx);
                    }
                    cx.stop_propagation();
                }))
                .on_drag(DraggedBandEdge, |_, _, _, cx| cx.new(|_| gpui::Empty)),
        )
    }
}

/// Teach `workspace.dump_visual_structure` about the band. Called from
/// `solution_agent::init`. The band reaches the dump this way rather than
/// the dump reaching the band directly: `solutions` owns the dump and
/// cannot depend on this crate (the edge runs the other way), and
/// `Workspace::solution_band_item` is an `AnyView`, so only this crate can
/// turn it back into something describable.
pub fn register_band_structure_provider(cx: &mut App) {
    register_structure_provider(cx, StructureSlot::SolutionBand, |workspace, window, cx| {
        let band = workspace
            .solution_band_item()?
            .downcast::<SolutionBand>()
            .ok()?;
        Some(band.read(cx).structure_node(workspace, window, cx))
    });
}

/// How the currently-selected utility content resolved this frame. Mirrors
/// the `match` at the top of [`SolutionBand::render`] exactly — the two must
/// agree or the dump describes a band the user is not looking at.
fn utility_occupant_state(workspace: &Workspace, kind: UtilityKind, visible: bool) -> &'static str {
    if !visible {
        "hidden"
    } else if workspace.solution_band_utility_item(kind).is_some() {
        "registered"
    } else if workspace.solution_band_utility_unavailable(kind) {
        "unavailable"
    } else {
        "pending"
    }
}

impl SolutionBand {
    /// The band's node for `workspace.dump_visual_structure`, so an agent can
    /// answer "is the band there, how tall, which half shows what" without a
    /// screenshot.
    ///
    /// `workspace` is the caller's already-borrowed `&Workspace` rather than
    /// `self.workspace.upgrade()`: the dump holds that borrow across this
    /// call, and re-reading the same entity here would be one more place to
    /// get the lease rules wrong for no gain.
    ///
    /// Deliberately reports no focus and no contents for the utility half.
    /// The occupant is an `AnyView` (`Workspace::solution_band_utility_item`),
    /// so this crate has neither its `FocusHandle` nor its element tree —
    /// `occupant_introspectable: false` says that in the payload instead of
    /// guessing.
    pub fn structure_node(&self, workspace: &Workspace, window: &Window, cx: &App) -> VisualNode {
        let solution_id = self.solution_id(cx);
        let state = self.band_state(cx);
        let store = SolutionAgentStore::global(cx);

        // Mirrors `render`: an `active_dialog_session` naming a session that
        // is already gone paints no half, so it must not read as one here.
        let dialog_session = state.active_dialog_session.filter(|session_id| {
            self.views.contains_key(session_id) || store.read(cx).session(*session_id).is_some()
        });
        let dialog_node = {
            let mut node = VisualNode::new("BandDialog").with_visible(dialog_session.is_some());
            if let Some(session_id) = dialog_session {
                node = node.with_attribute("session_id", session_id.as_str());
                if let Some(session) = store.read(cx).session(session_id) {
                    node = node.with_label(session.read(cx).title.to_string());
                }
                // Only a session whose view has actually been built can be
                // asked about focus; an un-rendered band has no view yet and
                // therefore holds no focus either, which `false` states
                // correctly.
                if let Some(view) = self.views.get(&session_id) {
                    node = node
                        .with_focused(view.read(cx).focus_handle(cx).contains_focused(window, cx));
                }
            }
            node
        };

        let occupant = utility_occupant_state(workspace, state.utility_kind, state.utility_visible);
        let utility_painted = matches!(occupant, "registered" | "unavailable");
        let utility_node = VisualNode::new(format!("BandUtility({})", state.utility_kind.as_str()))
            .with_label(state.utility_kind.label())
            .with_visible(utility_painted)
            .with_attribute("utility_kind", state.utility_kind.as_str())
            // The persisted toggle, which is what the status-bar buttons and
            // `ctrl-`` flip. It can be `true` while `visible` is `false`: the
            // occupant for this kind may still be loading, or have failed.
            .with_attribute("requested_visible", state.utility_visible)
            .with_attribute("occupant", occupant)
            .with_attribute("occupant_introspectable", false);

        let split = dialog_session.is_some() && utility_painted;

        let mut node = VisualNode::new("SolutionBand")
            .with_visible(dialog_session.is_some() || utility_painted)
            .with_attribute("height", state.height)
            .with_attribute(
                "effective_height",
                effective_band_height(state.height, f32::from(window.viewport_size().height)),
            )
            .with_attribute("divider_ratio", state.divider_ratio)
            .with_attribute("split", split)
            .with_attribute(
                "state_source",
                match solution_id {
                    Some(_) => "solution",
                    None => "window_local",
                },
            );
        if let Some(solution_id) = solution_id {
            node = node.with_attribute("solution_id", solution_id.0);
        }
        node.with_children(vec![
            dialog_node,
            VisualNode::new("BandDivider").with_visible(split),
            utility_node,
        ])
    }
}

impl Render for SolutionBand {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let solution_id = self.solution_id(cx);
        let state = self.band_state(cx);
        // A shown section whose kind has no registered occupant used to
        // render as *nothing at all* — the utility half simply vanished, and
        // with no dialog either the whole band collapsed, leaving a lit
        // status-bar button pointing at empty space. Now that three kinds
        // exist and any of them can fail to load (`add_utility_item_when_ready`
        // logs and gives up), the section says so instead. Falling back to a
        // kind that *is* registered was the alternative and is worse: it
        // would silently rewrite the user's persisted choice and desynchronise
        // the button group from `utility_kind`.
        let utility_panel = if state.utility_visible {
            match self.utility_panel(state.utility_kind, cx) {
                Some(view) => Some(view.into_any_element()),
                // Only once the install has actually FAILED. The three
                // occupants load concurrently, so "not in the slot map" alone
                // would make every window open flash "…is unavailable" for
                // whichever kind happened to resolve last — a false statement
                // shown to the user, not merely an ugly one.
                None if self.utility_panel_unavailable(state.utility_kind, cx) => {
                    Some(render_missing_occupant(state.utility_kind))
                }
                None => None,
            }
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
        // Every occupant (terminal, git graph, debugger — all three have
        // landed) inherits this instead of painting its own copy — neither
        // `Workspace::render` nor this element's parent paints a background,
        // so an occupant whose own content is empty (e.g. `ConsolePanel` with
        // no terminal in scope for the active member) must still read as an
        // opaque half rather than a transparent slab over whatever is behind
        // the band. Read out of `cx` before the closure below borrows it
        // mutably via `cx.listener` further down in this render call.
        let half_background = cx.theme().colors().panel_background;
        // `flex_basis` fractions only when there are two halves to divide;
        // a lone half takes the whole band regardless of the stored ratio, so
        // hiding the other side never leaves a dead gutter.
        let half = |content: gpui::AnyElement, fraction: f32| {
            let half = div().min_w_0().overflow_hidden().bg(half_background);
            if split {
                half.flex_shrink_1()
                    .flex_basis(DefiniteLength::Fraction(fraction))
            } else {
                half.flex_1()
            }
            .child(content)
        };

        let height = effective_band_height(state.height, f32::from(window.viewport_size().height));

        h_flex()
            .id("solution-band")
            .relative()
            .w_full()
            .h(px(height))
            // Shrinkable, NOT `flex_none`. `effective_band_height` reserves a
            // fixed slice of the viewport for the chrome, but it cannot know
            // the project zone's *content* minimum (tab bars, toolbars, panel
            // headers — measured at ~124px in a normal window and not a
            // constant). When the column overflows anyway, a `flex_none` band
            // keeps its full height and the deficit lands on the status bar,
            // which is squeezed and then pushed out of the window entirely.
            // Shrinking here puts the deficit on the band instead: the project
            // zone has `flex_basis: 0`, so its scaled shrink factor is 0 and
            // the band absorbs all of it. `min_h_0` is required — the default
            // automatic minimum size would otherwise refuse to shrink the band
            // below its own content.
            .flex_shrink(1.)
            .min_h_0()
            // `h_flex` centres its children; the halves and the divider must
            // instead fill the band's height.
            .items_stretch()
            .on_drag_move::<DraggedBandDivider>(cx.listener(
                move |this, event: &DragMoveEvent<DraggedBandDivider>, _window, cx| {
                    this.on_divider_drag_move(solution_id, event, cx);
                },
            ))
            .on_drag_move::<DraggedBandEdge>(cx.listener(
                move |this, event: &DragMoveEvent<DraggedBandEdge>, _window, cx| {
                    this.on_edge_drag_move(solution_id, event, cx);
                },
            ))
            .child(self.render_top_edge_handle(solution_id, cx))
            .children(dialog.map(|view| half(view.into_any_element(), state.divider_ratio)))
            .children(split.then(|| self.render_divider(solution_id, cx).into_any_element()))
            .children(utility_panel.map(|panel| half(panel, 1.0 - state.divider_ratio)))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SolutionAgentDb;
    use crate::model::{
        BAND_RESERVED_HEIGHT, DEFAULT_BAND_HEIGHT, MAX_BAND_HEIGHT, MAX_DIVIDER_RATIO,
        MIN_BAND_HEIGHT, MIN_DIVIDER_RATIO, band_reserved_height_terms, clamp_band_height,
        effective_band_height,
    };
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

    #[test]
    fn band_height_is_clamped_to_a_usable_range() {
        assert_eq!(clamp_band_height(-1.0), MIN_BAND_HEIGHT);
        assert_eq!(clamp_band_height(1_000_000.0), MAX_BAND_HEIGHT);
        assert_eq!(clamp_band_height(500.0), 500.0);
        assert_eq!(
            clamp_band_height(f32::NAN),
            DEFAULT_BAND_HEIGHT,
            "NaN survives `f32::clamp`, so it must be folded to the default \
             before it can reach a layout height"
        );
    }

    #[test]
    fn effective_band_height_caps_against_the_live_viewport() {
        assert_eq!(
            effective_band_height(600.0, 400.0),
            247.0,
            "a stored height above the ceiling of a small viewport is capped at \
             render, without touching the stored value"
        );
        assert_eq!(
            effective_band_height(320.0, 4000.0),
            320.0,
            "under a large viewport the stored value is returned unchanged"
        );
        assert_eq!(
            effective_band_height(300.0, 10.0),
            MIN_BAND_HEIGHT,
            "an absurdly short viewport must still leave room for the compose \
             box and status row — the ceiling never drops below MIN_BAND_HEIGHT"
        );
    }

    /// The reserve is hand-derived from a constant that lives in another crate,
    /// and the assertions below pin `effective_band_height`'s *arithmetic*, not
    /// that derivation — so a future change to `workspace::STATUS_BAR_HEIGHT`
    /// would leave every one of them green while the reserve silently stopped
    /// describing the real chrome. This is the assertion that fails instead.
    #[test]
    fn the_band_reserve_is_derived_from_the_live_status_bar_height() {
        let (chrome_above, status_bar, project_zone_floor) = band_reserved_height_terms();
        assert_eq!(
            status_bar,
            f32::from(workspace::STATUS_BAR_HEIGHT),
            "the middle term must BE the status bar's height, not a copy of its \
             current value"
        );
        assert_eq!(
            BAND_RESERVED_HEIGHT,
            chrome_above + status_bar + project_zone_floor,
            "BAND_RESERVED_HEIGHT is 61px of title bar + member tab row, plus the \
             status bar, plus 59px so the project zone is still an editor. If the \
             status bar's height changed, this constant and the four ceiling \
             assertions below move with it."
        );
    }

    #[test]
    fn effective_band_height_reserves_room_for_the_workspace_chrome() {
        // A 1366×768 laptop with the window tiled to the top half. The 0.8
        // fraction alone would allow 307.2px, which plus the 33px status bar
        // overflows the 322px left under the title bar / toolbar / borders —
        // zeroing the project zone and squeezing the status bar to 15px.
        assert_eq!(
            effective_band_height(f32::MAX, 384.0),
            231.0,
            "the reserve, not the fraction, is what binds on a short window"
        );
        assert_eq!(
            effective_band_height(f32::MAX, 765.0),
            612.0,
            "at the crossover the two ceilings agree (0.8 × 765 = 765 − 153)"
        );
        assert_eq!(
            effective_band_height(f32::MAX, 800.0),
            640.0,
            "just above the crossover the fraction is the binding ceiling again"
        );
        assert_eq!(
            effective_band_height(f32::MAX, 700.0),
            547.0,
            "just below the crossover the reserve is the binding ceiling"
        );
        assert_eq!(
            effective_band_height(f32::MAX, 200.0),
            MIN_BAND_HEIGHT,
            "below ~290px of window the reserve and MIN_BAND_HEIGHT cannot both \
             hold; the floor deliberately wins and the band stays usable"
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

    /// `set_band_utility_kind` round-trips immediately, like
    /// `set_band_utility_visible` (switching content is a discrete click, not
    /// a drag — see the setter's doc comment), without disturbing the row's
    /// other fields.
    #[gpui::test]
    async fn band_utility_kind_round_trips_and_leaves_other_fields_alone(cx: &mut TestAppContext) {
        let (store, db, _tmp) = store_with_persistence(cx).await;
        let solution_id = SolutionId(1);
        let dialog = SolutionSessionId::new();

        store.update(cx, |store, cx| {
            store.set_active_dialog_session(solution_id, Some(dialog), cx);
            store.set_band_utility_visible(solution_id, true, cx);
            store.set_band_divider_ratio(solution_id, 0.7, cx);
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();

        store.update(cx, |store, cx| {
            store.set_band_utility_kind(solution_id, UtilityKind::GitGraph, cx)
        });
        cx.run_until_parked();

        let written = persisted(&db, solution_id)
            .await
            .expect("the immediate write lands without waiting on a debounce");
        assert_eq!(written.utility_kind, UtilityKind::GitGraph);
        assert_eq!(
            written.divider_ratio, 0.7,
            "the kind write must not clobber the divider ratio set earlier"
        );
        assert!(written.utility_visible);
        assert_eq!(written.active_dialog_session, Some(dialog));
    }

    /// `set_band_height` round-trips through the debounce like the divider
    /// ratio does, without disturbing the row's other fields.
    #[gpui::test]
    async fn band_height_round_trips_and_leaves_other_fields_alone(cx: &mut TestAppContext) {
        let (store, db, _tmp) = store_with_persistence(cx).await;
        let solution_id = SolutionId(1);
        let dialog = SolutionSessionId::new();

        store.update(cx, |store, cx| {
            store.set_active_dialog_session(solution_id, Some(dialog), cx);
            store.set_band_utility_visible(solution_id, true, cx);
            store.set_band_divider_ratio(solution_id, 0.7, cx);
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();

        store.update(cx, |store, cx| {
            store.set_band_height(solution_id, 600.0, cx)
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();

        let written = db
            .load_band_states()
            .await
            .expect("load band states")
            .into_iter()
            .find(|(id, _)| *id == solution_id)
            .map(|(_, state)| state)
            .expect("row exists");
        assert_eq!(written.height, 600.0);
        assert_eq!(
            written.divider_ratio, 0.7,
            "the height write must not clobber the divider ratio set earlier"
        );
        assert!(written.utility_visible);
        assert_eq!(written.active_dialog_session, Some(dialog));
    }

    /// Ruling-5 invariant (a), for height: the write is cancel-on-replace,
    /// not one queued write per `on_drag_move`. Mirrors
    /// `a_replaced_divider_drag_cancels_the_write_it_supersedes`.
    #[gpui::test]
    async fn a_replaced_height_drag_cancels_the_write_it_supersedes(cx: &mut TestAppContext) {
        let (store, db, _tmp) = store_with_persistence(cx).await;
        let solution_id = SolutionId(1);

        // Seed a row so "nothing written yet" is distinguishable from "wrote
        // the new height".
        store.update(cx, |store, cx| {
            store.set_band_utility_visible(solution_id, true, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            persisted(&db, solution_id).await.map(|s| s.height),
            Some(DEFAULT_BAND_HEIGHT)
        );

        store.update(cx, |store, cx| {
            store.set_band_height(solution_id, 400.0, cx)
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        store.update(cx, |store, cx| {
            store.set_band_height(solution_id, 500.0, cx)
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();

        assert_eq!(
            persisted(&db, solution_id).await.map(|s| s.height),
            Some(DEFAULT_BAND_HEIGHT),
            "600ms after the first drag but only 300ms after the second, nothing \
             may have been written yet: the first drag's timer must have been \
             cancelled rather than left to fire at its own 400ms mark"
        );

        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        assert_eq!(
            persisted(&db, solution_id).await.map(|s| s.height),
            Some(500.0),
            "and the surviving timer writes the height the drag ended on"
        );
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
        let saved = BandState {
            divider_ratio: 0.7,
            utility_visible: false,
            utility_kind: UtilityKind::GitGraph,
            active_dialog_session: Some(SolutionSessionId::new()),
            height: 500.0,
        };
        store_with_this_saved_row_and_no_persistence_yet(cx, saved).await
    }

    /// As above, for the tests that need a specific persisted value to
    /// distinguish from the in-memory default.
    async fn store_with_this_saved_row_and_no_persistence_yet(
        cx: &mut TestAppContext,
        saved: BandState,
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

    /// Field-wise touched-mask discipline for `height` specifically: a
    /// pre-hydration `set_band_height` must win for `height` while the
    /// persisted row's `divider_ratio` (also non-default here) survives
    /// untouched, proving the overlay is per-field and not all-or-nothing.
    #[gpui::test]
    async fn a_height_touch_before_the_db_opens_keeps_the_persisted_ratio(cx: &mut TestAppContext) {
        let (store, db, saved, _tmp) = store_with_a_saved_row_and_no_persistence_yet(cx).await;
        let solution_id = SolutionId(1);
        assert_ne!(
            saved.divider_ratio, DEFAULT_DIVIDER_RATIO,
            "the persisted ratio must be distinguishable from the default for \
             this test to prove anything"
        );
        assert_ne!(
            saved.height, DEFAULT_BAND_HEIGHT,
            "same for height, or a bug that always fell back to the default \
             would pass unnoticed"
        );

        store.update(cx, |store, cx| {
            store.set_band_height(solution_id, 900.0, cx)
        });
        store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));
        cx.run_until_parked();

        let expected = BandState {
            height: 900.0,
            ..saved
        };
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.band_state(solution_id),
                expected,
                "height is the live value the user set; divider_ratio is still \
                 whatever the persisted row held"
            );
        });
        assert_eq!(persisted(&db, solution_id).await, Some(expected));
    }

    /// Field-wise touched-mask discipline for `utility_kind` specifically: a
    /// pre-hydration `set_band_utility_kind` must win for `utility_kind`
    /// while the persisted row's `divider_ratio` (also non-default here)
    /// survives untouched, proving the overlay is per-field and not
    /// all-or-nothing.
    #[gpui::test]
    async fn a_utility_kind_touch_before_the_db_opens_keeps_the_persisted_ratio(
        cx: &mut TestAppContext,
    ) {
        let (store, db, saved, _tmp) = store_with_a_saved_row_and_no_persistence_yet(cx).await;
        let solution_id = SolutionId(1);
        assert_ne!(
            saved.divider_ratio, DEFAULT_DIVIDER_RATIO,
            "the persisted ratio must be distinguishable from the default for \
             this test to prove anything"
        );
        assert_ne!(
            saved.utility_kind,
            UtilityKind::Terminal,
            "same for utility_kind, or a bug that always fell back to the \
             default would pass unnoticed"
        );

        store.update(cx, |store, cx| {
            store.set_band_utility_kind(solution_id, UtilityKind::Debug, cx)
        });
        store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));
        cx.run_until_parked();

        let expected = BandState {
            utility_kind: UtilityKind::Debug,
            ..saved
        };
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.band_state(solution_id),
                expected,
                "utility_kind is the live value the user set; divider_ratio is \
                 still whatever the persisted row held"
            );
        });
        assert_eq!(persisted(&db, solution_id).await, Some(expected));
    }

    /// The touched mask records the user's *request*, not a change of value.
    /// Before hydration the in-memory value IS the default, so a request for
    /// the default — `ctrl-\`` asking for `UtilityKind::Terminal`, the
    /// default kind — is indistinguishable from never having asked, and the
    /// setters' no-op check returns before any bookkeeping. Marking the field
    /// after that check (as it used to) let hydration's overlay pull
    /// `utility_kind` back off disk: `ctrl-\`` pressed inside the DB-open
    /// window reopened the band on the *persisted* content while focus had
    /// already been sent to the unrendered terminal — the exact failure the
    /// task-5 fix round removed from the post-hydration path.
    #[gpui::test]
    async fn asking_for_the_default_utility_kind_before_the_db_opens_still_wins(
        cx: &mut TestAppContext,
    ) {
        let saved = BandState {
            divider_ratio: 0.7,
            utility_visible: false,
            utility_kind: UtilityKind::Debug,
            active_dialog_session: Some(SolutionSessionId::new()),
            height: 500.0,
        };
        let (store, db, saved, _tmp) =
            store_with_this_saved_row_and_no_persistence_yet(cx, saved).await;
        let solution_id = SolutionId(1);
        let default_kind = BandState::default().utility_kind;
        assert_ne!(
            saved.utility_kind, default_kind,
            "the persisted kind must differ from the default the store starts \
             on, or the request below could not be a no-op against memory"
        );

        store.update(cx, |store, cx| {
            store.set_band_utility_kind(solution_id, default_kind, cx)
        });
        store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));
        cx.run_until_parked();

        let expected = BandState {
            utility_kind: default_kind,
            ..saved
        };
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.band_state(solution_id),
                expected,
                "the user asked for the terminal; hydration must not hand them \
                 back the debugger the row remembered"
            );
        });
        assert_eq!(persisted(&db, solution_id).await, Some(expected));
    }

    /// The same hazard on `utility_visible`, whose default (`false`) a user
    /// asks for every time they click the lit utility button or the
    /// debugger's Close Panel. `divider_ratio` and `height` have the same
    /// reachable path (double-clicking either handle resets to the default),
    /// which is why all four setters mark before their no-op check.
    #[gpui::test]
    async fn hiding_the_section_before_the_db_opens_survives_hydration(cx: &mut TestAppContext) {
        let saved = BandState {
            divider_ratio: 0.7,
            utility_visible: true,
            utility_kind: UtilityKind::GitGraph,
            active_dialog_session: Some(SolutionSessionId::new()),
            height: 500.0,
        };
        let (store, db, saved, _tmp) =
            store_with_this_saved_row_and_no_persistence_yet(cx, saved).await;
        let solution_id = SolutionId(1);

        store.update(cx, |store, cx| {
            store.set_band_utility_visible(solution_id, false, cx)
        });
        store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));
        cx.run_until_parked();

        let expected = BandState {
            utility_visible: false,
            ..saved
        };
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.band_state(solution_id),
                expected,
                "the user hid the section; hydration must not re-show it"
            );
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

    /// Locate a child of the band node by kind prefix — `BandUtility(...)`
    /// carries the live kind in its own `kind` string, so an exact match
    /// would have to be re-spelled per assertion.
    fn child_starting_with<'a>(node: &'a VisualNode, prefix: &str) -> &'a VisualNode {
        node.children
            .iter()
            .find(|child| child.kind.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix:?} child in {:?}", node.children))
    }

    fn attribute(node: &VisualNode, key: &str) -> serde_json::Value {
        node.attributes
            .get(key)
            .cloned()
            .unwrap_or_else(|| panic!("no {key:?} attribute on {:?}", node.kind))
    }

    /// The dump's answers to "is the band there, how tall, where is the
    /// divider, which half shows what" for a Solution window with a
    /// registered utility occupant.
    #[gpui::test]
    async fn the_band_structure_node_reports_geometry_and_the_utility_half(
        cx: &mut TestAppContext,
    ) {
        let (solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;

        cx.update(|cx| {
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

        workspace.update_in(cx, |workspace, window, cx| {
            let occupant = cx.new(|_| gpui::Empty);
            workspace.set_solution_band_utility_item(
                UtilityKind::GitGraph,
                occupant.into(),
                window,
                cx,
            );
        });
        cx.update(|_window, cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_band_height(solution_id, 412.0, cx);
                store.set_band_divider_ratio(solution_id, 0.25, cx);
                store.set_band_utility_kind(solution_id, UtilityKind::GitGraph, cx);
                store.set_band_utility_visible(solution_id, true, cx);
            });
        });

        let node = workspace.update_in(cx, |workspace, window, cx| {
            band.read(cx).structure_node(workspace, window, cx)
        });

        assert_eq!(node.kind, "SolutionBand");
        assert!(
            node.visible,
            "a shown utility half makes the band non-empty"
        );
        assert_eq!(attribute(&node, "height"), serde_json::json!(412.0));
        assert_eq!(attribute(&node, "divider_ratio"), serde_json::json!(0.25));
        assert_eq!(
            attribute(&node, "state_source"),
            serde_json::json!("solution")
        );
        assert_eq!(
            attribute(&node, "solution_id"),
            serde_json::json!(solution_id.0)
        );
        assert_eq!(
            attribute(&node, "split"),
            serde_json::json!(false),
            "no dialog session is active, so the utility half is alone in the band"
        );

        let dialog = child_starting_with(&node, "BandDialog");
        assert!(!dialog.visible);
        assert!(dialog.label.is_none());

        let utility = child_starting_with(&node, "BandUtility");
        assert_eq!(utility.kind, "BandUtility(git_graph)");
        assert_eq!(utility.label.as_deref(), Some("Git Graph"));
        assert!(utility.visible);
        assert_eq!(
            attribute(&utility, "occupant"),
            serde_json::json!("registered")
        );
        assert_eq!(
            attribute(&utility, "requested_visible"),
            serde_json::json!(true)
        );
        assert_eq!(
            attribute(&utility, "occupant_introspectable"),
            serde_json::json!(false),
            "the occupant is an AnyView; the dump must say it cannot see inside \
             rather than imply it looked"
        );

        assert!(!child_starting_with(&node, "BandDivider").visible);
    }

    /// Hiding the utility half must not erase the remembered kind (spec §3),
    /// and the dump has to keep the two apart: `visible` is what is painted,
    /// `requested_visible` is the persisted toggle the buttons flip.
    #[gpui::test]
    async fn a_hidden_utility_half_still_reports_its_remembered_kind(cx: &mut TestAppContext) {
        let (solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;

        cx.update(|cx| {
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

        workspace.update_in(cx, |workspace, window, cx| {
            let occupant = cx.new(|_| gpui::Empty);
            workspace.set_solution_band_utility_item(
                UtilityKind::Debug,
                occupant.into(),
                window,
                cx,
            );
        });
        cx.update(|_window, cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_band_utility_kind(solution_id, UtilityKind::Debug, cx);
                store.set_band_utility_visible(solution_id, false, cx);
            });
        });

        let node = workspace.update_in(cx, |workspace, window, cx| {
            band.read(cx).structure_node(workspace, window, cx)
        });

        assert!(
            !node.visible,
            "neither half has anything to paint, so the band collapses"
        );
        let utility = child_starting_with(&node, "BandUtility");
        assert_eq!(utility.kind, "BandUtility(debug)");
        assert!(!utility.visible);
        assert_eq!(
            attribute(&utility, "requested_visible"),
            serde_json::json!(false)
        );
        assert_eq!(attribute(&utility, "occupant"), serde_json::json!("hidden"));
    }

    /// A kind whose occupant has not landed yet reads as `pending`, not as
    /// a painted half — the same distinction `render` draws before it decides
    /// whether to paint the "is unavailable" placeholder.
    #[gpui::test]
    async fn a_pending_occupant_is_not_reported_as_a_painted_half(cx: &mut TestAppContext) {
        let (solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;

        cx.update(|cx| {
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

        cx.update(|_window, cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_band_utility_visible(solution_id, true, cx);
            });
        });

        let pending = workspace.update_in(cx, |workspace, window, cx| {
            band.read(cx).structure_node(workspace, window, cx)
        });
        let utility = child_starting_with(&pending, "BandUtility");
        assert_eq!(
            attribute(&utility, "occupant"),
            serde_json::json!("pending")
        );
        assert!(!utility.visible);

        workspace.update_in(cx, |workspace, _window, cx| {
            workspace.mark_solution_band_utility_unavailable(UtilityKind::Terminal, cx);
        });

        let failed = workspace.update_in(cx, |workspace, window, cx| {
            band.read(cx).structure_node(workspace, window, cx)
        });
        let utility = child_starting_with(&failed, "BandUtility");
        assert_eq!(
            attribute(&utility, "occupant"),
            serde_json::json!("unavailable")
        );
        assert!(
            utility.visible,
            "the failure placeholder is a painted half, so the band is not empty"
        );
    }

    /// A plain-folder window has no persisted row, and the dump must say so
    /// rather than attributing the geometry to a Solution.
    #[gpui::test]
    async fn a_window_with_no_solution_reports_window_local_geometry(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });

        let plain_dir = tempfile::tempdir().expect("plain-folder tempdir");
        let fs = fs::FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(plain_dir.path(), serde_json::json!({ ".keep": "" }))
            .await;
        let project = project::Project::test(fs, [plain_dir.path()], cx).await;

        cx.update(|cx| {
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
        band.update(cx, |band, cx| band.set_band_height(None, 500.0, cx));

        let node = workspace.update_in(cx, |workspace, window, cx| {
            band.read(cx).structure_node(workspace, window, cx)
        });

        assert_eq!(
            attribute(&node, "state_source"),
            serde_json::json!("window_local")
        );
        assert!(
            !node.attributes.contains_key("solution_id"),
            "there is no Solution to name"
        );
        assert_eq!(attribute(&node, "height"), serde_json::json!(500.0));
    }

    /// The band's own view of its height, for a Solution window, comes from
    /// the store rather than `local_state` — mirrors how `active_view` reads
    /// through `band_state` rather than a cached field.
    #[gpui::test]
    async fn band_height_for_a_solution_window_comes_from_the_store(cx: &mut TestAppContext) {
        let (solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;

        cx.update(|cx| {
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

        cx.update(|_window, cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_band_height(solution_id, 777.0, cx);
            });
        });

        assert_eq!(
            band.read_with(cx, |band, cx| band.band_height(cx)),
            777.0,
            "the band's view of its height comes from the store, not local_state"
        );
    }

    /// A plain-folder window that resolves to no Solution is a supported
    /// case (module doc) — its height must round-trip through `local_state`
    /// and never reach the DB. Mirrors console_panel's
    /// `toggle_focus_works_in_a_workspace_with_no_solution` regression, one
    /// crate over, for `utility_visible`.
    #[gpui::test]
    async fn band_height_round_trips_through_local_state_for_a_window_with_no_solution(
        cx: &mut TestAppContext,
    ) {
        let (_store, db, _tmp) = store_with_persistence(cx).await;
        let solution_id = SolutionId(1);

        let plain_dir = tempfile::tempdir().expect("plain-folder tempdir");
        let fs = fs::FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(plain_dir.path(), serde_json::json!({ ".keep": "" }))
            .await;
        let project = project::Project::test(fs, [plain_dir.path()], cx).await;

        cx.update(|cx| theme_settings::init(theme::LoadThemes::JustBase, cx));

        let (workspace, cx) =
            cx.add_window_view(|window, cx| workspace::Workspace::test_new(project, window, cx));

        let band = workspace.update_in(cx, |workspace, _window, cx| {
            let project = workspace.project().clone();
            cx.new(|cx| SolutionBand::new(workspace.weak_handle(), project, cx))
        });

        band.update(cx, |band, cx| {
            assert_eq!(
                band.solution_id(cx),
                None,
                "the plain-folder worktree must not resolve to the Solution \
                 `store_with_persistence` set up"
            );
            band.set_band_height(None, 500.0, cx);
        });
        assert_eq!(
            band.read_with(cx, |band, cx| band.band_height(cx)),
            500.0,
            "the height lands in the window-local fallback"
        );

        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(
            persisted(&db, solution_id).await,
            None,
            "a window with no Solution must never write the band_state row — \
             set_band_height(None, ..) stays entirely in local_state"
        );
    }

    /// The status-bar button group's rule end-to-end through the *persisted*
    /// layer (spec §3): switch, hide-on-re-click, and — the load-bearing
    /// half — a hide that leaves `utility_kind` alone so the next click
    /// returns to the same content rather than to the default.
    #[gpui::test]
    async fn activating_a_utility_kind_switches_shows_and_hides(cx: &mut TestAppContext) {
        let (solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;

        cx.update(|cx| {
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
            let state = band.band_state(cx);
            assert!(
                !state.utility_visible,
                "the default band starts with the section hidden"
            );
            for kind in UtilityKind::ALL {
                assert!(
                    !utility_button_selected(kind, &state),
                    "{kind:?} must render unselected while the section is hidden"
                );
            }
        });

        band.update(cx, |band, cx| {
            band.activate_utility_kind(UtilityKind::GitGraph, cx)
        });
        band.read_with(cx, |band, cx| {
            assert_eq!(band.utility_kind(cx), UtilityKind::GitGraph);
            assert!(band.utility_visible(cx), "an inactive button reveals");
        });

        band.update(cx, |band, cx| {
            band.activate_utility_kind(UtilityKind::Debug, cx)
        });
        band.read_with(cx, |band, cx| {
            let state = band.band_state(cx);
            assert_eq!(state.utility_kind, UtilityKind::Debug);
            assert!(state.utility_visible);
            assert!(!utility_button_selected(UtilityKind::GitGraph, &state));
        });

        band.update(cx, |band, cx| {
            band.activate_utility_kind(UtilityKind::Debug, cx)
        });
        band.read_with(cx, |band, cx| {
            assert!(!band.utility_visible(cx), "the active button hides");
            assert_eq!(
                band.utility_kind(cx),
                UtilityKind::Debug,
                "hiding must leave the remembered kind untouched"
            );
        });

        band.update(cx, |band, cx| {
            band.activate_utility_kind(UtilityKind::Debug, cx)
        });
        band.read_with(cx, |band, cx| {
            assert!(band.utility_visible(cx));
            assert_eq!(
                band.utility_kind(cx),
                UtilityKind::Debug,
                "re-showing comes back to the same content"
            );
        });

        cx.update(|_window, cx| {
            assert_eq!(
                SolutionAgentStore::global(cx)
                    .read(cx)
                    .band_state(solution_id)
                    .utility_kind,
                UtilityKind::Debug,
                "a Solution window's buttons write the persisted row, not \
                 local_state"
            );
        });
    }

    /// The same rule for a plain-folder window that resolves to no Solution.
    /// It has no `BandState` row to write, so the whole switch/hide cycle has
    /// to round-trip through `local_state` — the case a button group wired
    /// straight to `SolutionAgentStore` would have left inert.
    #[gpui::test]
    async fn activating_a_utility_kind_round_trips_through_local_state_with_no_solution(
        cx: &mut TestAppContext,
    ) {
        let (_store, db, _tmp) = store_with_persistence(cx).await;
        let solution_id = SolutionId(1);

        let plain_dir = tempfile::tempdir().expect("plain-folder tempdir");
        let fs = fs::FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(plain_dir.path(), serde_json::json!({ ".keep": "" }))
            .await;
        let project = project::Project::test(fs, [plain_dir.path()], cx).await;

        cx.update(|cx| theme_settings::init(theme::LoadThemes::JustBase, cx));

        let (workspace, cx) =
            cx.add_window_view(|window, cx| workspace::Workspace::test_new(project, window, cx));

        let band = workspace.update_in(cx, |workspace, _window, cx| {
            let project = workspace.project().clone();
            cx.new(|cx| SolutionBand::new(workspace.weak_handle(), project, cx))
        });

        band.update(cx, |band, cx| {
            assert_eq!(band.solution_id(cx), None);
            band.activate_utility_kind(UtilityKind::GitGraph, cx);
        });
        band.read_with(cx, |band, cx| {
            assert_eq!(band.utility_kind(cx), UtilityKind::GitGraph);
            assert!(band.utility_visible(cx));
        });

        band.update(cx, |band, cx| {
            band.activate_utility_kind(UtilityKind::GitGraph, cx)
        });
        band.read_with(cx, |band, cx| {
            assert!(!band.utility_visible(cx));
            assert_eq!(
                band.utility_kind(cx),
                UtilityKind::GitGraph,
                "the window-local fallback remembers the kind across a hide too"
            );
        });

        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(
            persisted(&db, solution_id).await,
            None,
            "a window with no Solution must never write the band_state row"
        );
    }

    /// Double-lease guard for the new setter, in the shape of
    /// `console_panel`'s `toggle_focus_action_does_not_double_lease_the_workspace`:
    /// call `set_band_height` while a `&mut Workspace` borrow is live (the
    /// shape a real drag callback runs under once GPUI's dispatch reaches
    /// through an action handler), proving it never resolves
    /// `self.workspace.upgrade()?.read(cx)` the way `utility_panel` does.
    #[gpui::test]
    async fn set_band_height_does_not_double_lease_the_workspace(cx: &mut TestAppContext) {
        let (solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;

        cx.update(|cx| {
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

        // `workspace.update`'s closure leases the root view (`Workspace`)
        // exactly the way real action dispatch does; calling into the band
        // from inside it is what would panic if `set_band_height` ever
        // upgraded and read `self.workspace`.
        workspace.update(cx, |_workspace, cx| {
            band.update(cx, |band, cx| {
                band.set_band_height(Some(solution_id), 900.0, cx);
            });
        });

        assert_eq!(
            band.read_with(cx, |band, cx| band.band_height(cx)),
            900.0,
            "the height landed even though the setter ran under a live \
             &mut Workspace borrow"
        );
    }

    /// The band's placeholder gate. A kind whose occupant has simply not
    /// landed yet must NOT read as unavailable — `zed.rs` loads the three
    /// concurrently, so inferring failure from an empty slot would paint
    /// "…is unavailable" over a kind that was still loading.
    #[gpui::test]
    async fn a_kind_reads_unavailable_only_once_its_install_has_failed(cx: &mut TestAppContext) {
        let (_solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;

        cx.update(|cx| {
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
            for kind in UtilityKind::ALL {
                assert!(
                    band.utility_panel(kind, cx).is_none(),
                    "precondition: nothing is installed in this test workspace"
                );
                assert!(
                    !band.utility_panel_unavailable(kind, cx),
                    "…and an uninstalled kind is still not *unavailable* ({kind:?})"
                );
            }
        });

        workspace.update_in(cx, |workspace, _window, cx| {
            workspace.mark_solution_band_utility_unavailable(UtilityKind::GitGraph, cx);
        });

        band.read_with(cx, |band, cx| {
            assert!(band.utility_panel_unavailable(UtilityKind::GitGraph, cx));
            assert!(
                !band.utility_panel_unavailable(UtilityKind::Terminal, cx),
                "a sibling's failure must not make the terminal render as broken"
            );
        });
    }

    /// Same guard for the button group's entry point. A status item's click
    /// handler must be assumed to run under a live `&mut Workspace` borrow,
    /// and `activate_utility_kind` reads `band_state` (which resolves the
    /// Solution) before writing — the exact shape that would panic if any of
    /// it went through `self.workspace`.
    #[gpui::test]
    async fn activate_utility_kind_does_not_double_lease_the_workspace(cx: &mut TestAppContext) {
        let (_solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;

        cx.update(|cx| {
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

        workspace.update(cx, |_workspace, cx| {
            band.update(cx, |band, cx| {
                band.activate_utility_kind(UtilityKind::GitGraph, cx);
            });
        });

        band.read_with(cx, |band, cx| {
            assert_eq!(band.utility_kind(cx), UtilityKind::GitGraph);
            assert!(band.utility_visible(cx));
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
