//! Session tab strip mounted in the status bar's left group: one tab per
//! non-ephemeral AI session of the *active* Solution, ordered by
//! `SolutionSession::tab_order`. Selecting a tab drives
//! `SolutionAgentStore::set_active_dialog_session`, the shared selection the
//! Solution band (phase 2a task 4) reads to decide which dialog to show.
//! Clicking the already-active tab collapses the band (sets the selection
//! back to `None` — spec 2026-08-26 §3).
//!
//! Structurally this mirrors `solutions_ui::project_tab_strip::ProjectTabStrip`
//! (fixed visible cap + trailing overflow popover + trailing `+`) — NOT
//! `solutions_ui::solution_tab::SolutionTabStrip`, which just scrolls and has
//! no overflow popover (see the phase-2a task-3 brief for why).
//!
//! Lives in `solution_agent` rather than `console_panel` (which owns the OLD
//! bottom-dock chat tab strip this one is replacing in phase 2a) because a
//! dependency the other way would cycle: `console_panel` already depends on
//! `solution_agent`. The trailing `+` therefore dispatches
//! `console_panel::NewChat` *dynamically by name* (`cx.build_action`) rather
//! than importing the action type — the same cross-crate action-dispatch
//! idiom already used by `git_ui::commit_context_menu` for
//! `solution_git::CrossCherryPick` / `git_graph::ShowAffectedPathsInLog`.
//! Reusing that action (instead of calling `SolutionAgentStore::create_session`
//! directly) keeps session creation on exactly one code path — two paths
//! disagreeing about the new session's cwd was the phase-1 Critical.
//!
//! Right-click a tab for Close / Rename Session / Restart Agent, and drag a
//! tab onto another to reorder (persisted via `SolutionAgentStore::
//! persist_tab_order`) — phase-2a task-5b restoring what the deleted
//! `ConsolePanel` chat-tab strip carried. See `reorder_to` and
//! `open_rename_session_modal` for the mechanics; the overflow popover's
//! rows are `submenu`s (not plain entries) so the same three actions stay
//! reachable for a tab that has spilled past `MAX_VISIBLE_TABS`. The trailing
//! `+` creates a session on a plain left click, and the history button beside
//! it opens the "Reopen Closed Chat…" picker — the reopen flow used to hang
//! off `ConsolePanel`'s `+`, which no longer offers AI-session entries at all;
//! see `render_reopen_button` for why it is a visible button rather than a
//! gesture on the `+`. There is no per-tab close cross — the right-click menu
//! is the only close affordance (maintainer request, 2026-09-03), which is
//! why that entry comes first.
//!
//! The strip closes itself off from the status bar's other left-hand items
//! with a vertical rule (`render_group_divider`), so the AI-dialog group reads
//! as one group rather than as the first few of a dozen unrelated widgets.

use std::cell::RefCell;

use gpui::{
    App, Context, ElementId, IntoElement, ParentElement, PromptLevel, Render, SharedString, Styled,
    Subscription, WeakEntity, Window, div,
};
use solutions::{SolutionId, SolutionStore};
use ui::{
    ContextMenu, Divider, DividerColor, Indicator, PopoverMenu, Tooltip, prelude::*,
    right_click_menu,
};
use util::ResultExt as _;
use workspace::item::ItemHandle;
use workspace::{HideStatusItem, MultiWorkspace, StatusItemView, Workspace};

use crate::model::{SessionState, SolutionSessionId};
use crate::rename_session_modal::RenameSessionModal;
use crate::reopen_session_modal::open_reopen_session_modal;
use crate::status_row::state_dot_color;
use crate::store::{SolutionAgentStore, SolutionAgentStoreEvent};

/// How many session tabs render inline before the rest spill into the
/// trailing `more` popover. Mirrors `project_tab_strip::MAX_VISIBLE_TABS` in
/// spirit; kept smaller because the status bar's left group already shares
/// space with several other items (search, LSP, diagnostics, file name, …),
/// unlike the title bar's project strip which owns a full-width row.
pub const MAX_VISIBLE_TABS: usize = 5;

/// The data one rendered tab needs, snapshotted from the live
/// `SolutionSession` entity so ordering/overflow can be decided as pure
/// functions over plain data (no GPUI entity access) — see
/// `split_visible_overflow` and its test.
struct TabCandidate {
    session_id: SolutionSessionId,
    tab_order: i64,
    title: SharedString,
    is_cold: bool,
    is_errored: bool,
    is_running: bool,
}

/// Split `entries` into (visible, overflow) at `MAX_VISIBLE_TABS`. A free
/// function — generic over the element type — so it is exercised directly by
/// a unit test without building a rendered tab (a `ConsoleTab::Chat`-style
/// entity needs a live `SolutionSessionView` embedding a real
/// `editor::Editor`, which test scaffolding cannot construct; extracting the
/// decision sidesteps that gap entirely rather than working around it).
fn split_visible_overflow<T>(entries: &[T]) -> (&[T], &[T]) {
    if entries.len() > MAX_VISIBLE_TABS {
        entries.split_at(MAX_VISIBLE_TABS)
    } else {
        (entries, &[])
    }
}

/// Decide the next `active_dialog_session` value for a tab click:
/// re-clicking the already-active tab collapses the selection (`None`),
/// any other click selects the clicked session. Extracted as a pure
/// function so the "click the active tab again" branch — the one most
/// likely to be gotten backwards — is covered by a unit test independent
/// of the click-driven store-mutation test.
fn toggle_selection(
    current: Option<SolutionSessionId>,
    clicked: SolutionSessionId,
) -> Option<SolutionSessionId> {
    if current == Some(clicked) {
        None
    } else {
        Some(clicked)
    }
}

/// Would closing `session_id` right now abandon an in-flight agent turn?
/// Extracted from [`SessionTabStrip::close_tab`] so the busy/idle split it
/// gates the confirmation prompt on is unit-testable without a live
/// `SolutionSession` entity. Deliberately includes `Stopping` — a cancel is
/// still winding down, so it is just as much a reason to confirm as
/// `Running` is. Does NOT match `status_row.rs`'s `is_running` (which feeds
/// the tab's status dot and excludes `Stopping`) — the two are different
/// questions answered from the same `SessionState`.
fn is_busy_state(state: &SessionState) -> bool {
    matches!(
        state,
        SessionState::Running { .. } | SessionState::Stopping { .. }
    )
}

/// The three visual signals that separate the selected session tab from the
/// unselected ones. Named as a value rather than inlined into `render_tab` so
/// the rule itself is unit-testable: `debug_bounds` can prove an element
/// painted but says nothing about its colours, so without this the house
/// style would only ever be checked by eye.
#[derive(Debug, PartialEq, Eq)]
struct TabSelectionStyle {
    /// Whether a background is painted at all. The unselected tab gets
    /// *none* — `tab_active_background` against `tab_inactive_background` is
    /// two adjacent neutral steps, which is what made the selection
    /// invisible before.
    filled: bool,
    /// Whether the 2px bottom border uses `border_focused` (vs
    /// `border_transparent`, which reserves the same space).
    accent_underline: bool,
    label: Color,
}

/// The fork's house style for a tab strip, matching
/// `solutions_ui::project_tab` / `solution_tab` — the two strips in the title
/// bar the user sees directly above this one.
fn tab_selection_style(is_active: bool) -> TabSelectionStyle {
    TabSelectionStyle {
        filled: is_active,
        accent_underline: is_active,
        label: if is_active {
            Color::Default
        } else {
            Color::Muted
        },
    }
}

/// Move `from` so it lands at the slot currently occupied by `target`,
/// preserving the relative order of every other session. Mirrors
/// `solutions_ui::project_tab::reorder_to` — the live precedent for
/// drag-reorder in a tab strip (this one, unlike the deleted
/// `ConsolePanel::reorder_tab`, is built for the same kind of container).
/// Returns the original order unchanged when either id is missing, or when
/// dropping a tab onto itself.
fn reorder_to(
    order: &[SolutionSessionId],
    from: SolutionSessionId,
    target: SolutionSessionId,
) -> Vec<SolutionSessionId> {
    if from == target || !order.contains(&from) || !order.contains(&target) {
        return order.to_vec();
    }
    let mut remaining: Vec<SolutionSessionId> =
        order.iter().copied().filter(|id| *id != from).collect();
    let insert_at = remaining
        .iter()
        .position(|id| *id == target)
        .unwrap_or(remaining.len());
    remaining.insert(insert_at, from);
    remaining
}

/// Drag payload for reordering session tabs, carrying enough to render a
/// drag preview that looks like the tab being dragged. Mirrors
/// `solutions_ui::project_tab::DraggedProjectTab`.
#[derive(Clone)]
struct DraggedSessionTab {
    session_id: SolutionSessionId,
    title: SharedString,
}

impl Render for DraggedSessionTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_1p5()
            .px_2()
            .h_7()
            .bg(cx.theme().colors().tab_active_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_sm()
            .child(Label::new(self.title.clone()).size(LabelSize::Small))
    }
}

/// Open the rename-session popup for `session_id`, seeded with its current
/// title. Free function (not a `SessionTabStrip` method) because it is
/// called from context-menu entry handlers, which only get `(Window, App)`
/// — no access to `Self` is needed here, only to the Solution's workspace
/// (to host the modal) and the store (to read the current title).
fn open_rename_session_modal(
    weak_workspace: &WeakEntity<Workspace>,
    session_id: SolutionSessionId,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = weak_workspace.upgrade() else {
        return;
    };
    let current_title = SolutionAgentStore::global(cx)
        .read(cx)
        .session(session_id)
        .map(|entity| entity.read(cx).title.to_string())
        .unwrap_or_default();
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, move |window, cx| {
            RenameSessionModal::new(session_id, current_title, window, cx)
        });
    });
}

/// The reopen button's tooltip — and, verbatim, the name the tab-close
/// confirmation prompt quotes back to the user. A const because those are two
/// files apart: the prompt used to name "Reopen Closed Chat" *in the console
/// panel*, which stopped being true the moment the flow moved here, and a
/// plain literal in two places is exactly how that went stale. Pinned by
/// `the_close_prompt_and_the_tooltip_point_at_the_same_affordance`.
const REOPEN_TOOLTIP: &str = "Reopen Closed Chat…";

/// The `+` button's tooltip. Just the action it performs: the reopen flow was
/// briefly a right-click on this same button, which forced the tooltip to
/// advertise a gesture; it is a button of its own now (maintainer request,
/// 2026-09-04 — the gesture was not discoverable), so there is nothing left
/// for this one to point at.
const PLUS_TOOLTIP: &str = "New AI Session";

/// `debug_selector` of the rule closing the AI group off from the status
/// bar's other left-hand items. Shared with the paint test so a rename cannot
/// leave the test asserting a selector nothing emits.
const GROUP_DIVIDER_SELECTOR: &str = "SESSION-STRIP-GROUP-DIVIDER";

/// Detail line of the tab-close confirmation prompt. Assembled from the same
/// const the reopen button's tooltip uses, so the promise it makes cannot
/// drift from the affordance it points at the way its predecessor did (that
/// one named "Reopen Closed Chat" *in the console panel*, and stayed wrong
/// for as long as nothing read it). A function, not a literal at the call
/// site, so the test can read exactly what the user is shown.
fn close_prompt_detail() -> String {
    format!(
        "The agent is still working. Closing interrupts the current turn — the tab can be \
         brought back with the session strip's \"{REOPEN_TOOLTIP}\" button, next to \"+\"."
    )
}

/// The vertical rule that closes the AI-dialog group off from the status
/// bar's other left-hand items.
///
/// Those items — search, LSP, diagnostics, file name, merge conflicts, the
/// activity indicator — sit to this strip's **right**: `zed::zed`'s
/// `initialize_workspace` registers the strip first in the left group, and
/// `StatusBar::render_left_tools` paints left items in registration order. So
/// the boundary the maintainer asked for ("between the AI tabs/buttons and
/// the rest of the buttons on the left") is this group's *trailing* edge.
///
/// `ui::Divider::vertical()` is `w_px().h_full()`, and `h_full` is inert
/// anywhere in this strip — no ancestor has a definite height, which is the
/// same reason the tab pills set `.h(ButtonSize::Default.rems())` explicitly.
/// Hosting it in a wrapper that *does* carry that height gives the rule the
/// exact vertical extent of the row's tabs and buttons, so it cannot grow the
/// status bar. The wrapper also carries the `debug_selector`: `Divider` is a
/// `RenderOnce` with no `InteractiveElement`, so it cannot carry one itself.
fn render_group_divider() -> impl IntoElement {
    div()
        .debug_selector(|| GROUP_DIVIDER_SELECTOR.into())
        .flex()
        .flex_none()
        .items_center()
        .h(ButtonSize::Default.rems())
        .px_1()
        .child(Divider::vertical().color(DividerColor::Border))
}

pub struct SessionTabStrip {
    multi_workspace: Option<WeakEntity<MultiWorkspace>>,
    _subscriptions: Vec<Subscription>,
}

impl SessionTabStrip {
    pub fn new(
        multi_workspace: Option<WeakEntity<MultiWorkspace>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();

        let store = SolutionAgentStore::global(cx);
        subscriptions.push(cx.subscribe(&store, |_, _, event, cx| {
            if matches!(
                event,
                SolutionAgentStoreEvent::SessionCreated { .. }
                    | SolutionAgentStoreEvent::SessionClosed(_)
                    | SolutionAgentStoreEvent::TabsChanged { .. }
                    | SolutionAgentStoreEvent::SessionStateChanged(_)
                    | SolutionAgentStoreEvent::SessionTitleChanged(_)
                    | SolutionAgentStoreEvent::ActiveDialogSessionChanged { .. }
            ) {
                cx.notify();
            }
        }));

        if let Some(mw) = multi_workspace.as_ref().and_then(|w| w.upgrade()) {
            subscriptions.push(cx.observe(&mw, |_, _, cx| cx.notify()));
        }

        Self {
            multi_workspace,
            _subscriptions: subscriptions,
        }
    }

    /// Resolve the Solution the active member workspace belongs to. Mirrors
    /// `project_tab_strip::solution_id_for_workspace` — same worktree→Solution
    /// lookup, duplicated rather than shared because `solution_agent` cannot
    /// depend on `solutions_ui` (which itself depends on `console_panel`,
    /// which depends on `solution_agent` — a cycle).
    fn active_solution_id(&self, cx: &App) -> Option<SolutionId> {
        let mw = self.multi_workspace.as_ref()?.upgrade()?;
        let workspace = mw.read(cx).workspace().clone();
        let store = SolutionStore::global(cx);
        let store = store.read(cx);
        let project = workspace.read(cx).project().clone();
        project.read(cx).worktrees(cx).find_map(|tree| {
            store
                .solution_for_path(&tree.read(cx).abs_path())
                .map(|sol| sol.id)
        })
    }

    /// Weak handle to the Solution's active workspace, used to host the
    /// rename-session modal. `MultiWorkspace::workspace()` is the same
    /// active-workspace lookup `active_solution_id` uses.
    fn workspace_weak(&self, cx: &App) -> Option<WeakEntity<Workspace>> {
        let mw = self.multi_workspace.as_ref()?.upgrade()?;
        Some(mw.read(cx).workspace().downgrade())
    }

    fn on_tab_clicked(
        &mut self,
        solution_id: SolutionId,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        let store = SolutionAgentStore::global(cx);
        let current = store.read(cx).active_dialog_session(solution_id);
        let next = toggle_selection(current, session_id);
        store.update(cx, |store, cx| {
            store.set_active_dialog_session(solution_id, next, cx);
        });
    }

    /// Close a tab's session, applying the same busy-state speed bump the old
    /// dock chat strip used before phase 2a task 5 removed it: a session
    /// that's still `Running`/`Stopping` gets a confirmation prompt (closing
    /// abandons in-flight agent work), a terminal-state session closes
    /// straight through. This is now the only surface that closes a chat
    /// session's tab, so there is nothing left to diverge from.
    fn close_tab(
        &mut self,
        session_id: SolutionSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = SolutionAgentStore::global(cx);
        let busy = store
            .read(cx)
            .session(session_id)
            .map(|session| is_busy_state(&session.read(cx).state))
            .unwrap_or(false);
        if !busy {
            store
                .update(cx, |store, cx| store.close_session(session_id, cx))
                .log_err();
            return;
        }
        let detail = close_prompt_detail();
        let answer = window.prompt(
            PromptLevel::Warning,
            "Close this AI session's tab?",
            Some(&detail),
            &["Close", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return;
            }
            this.update(cx, |_, cx| {
                SolutionAgentStore::global(cx)
                    .update(cx, |store, cx| store.close_session(session_id, cx))
                    .log_err();
            })
            .ok();
        })
        .detach();
    }

    fn candidates_for(&self, solution_id: SolutionId, cx: &App) -> Vec<TabCandidate> {
        let store = SolutionAgentStore::global(cx);
        let sessions = store.read(cx).sessions_for(&solution_id);
        let mut candidates: Vec<TabCandidate> = sessions
            .iter()
            .filter_map(|session| {
                let session = session.read(cx);
                if !session.can_be_active_dialog() {
                    return None;
                }
                // Re-asked only to extract the value the predicate above
                // already established is present.
                let tab_order = session.tab_order?;
                Some(TabCandidate {
                    session_id: session.id,
                    tab_order,
                    title: session.title.clone(),
                    is_cold: session.is_cold(),
                    is_errored: matches!(session.state, SessionState::Errored(_)),
                    // `Stopping` is deliberately NOT folded in here, even though
                    // `close_tab`'s busy-check below treats it the same as
                    // `Running` — the two questions are different ("is the agent
                    // doing something, for the dot" vs "would closing abandon
                    // work, for the confirm prompt"). `status_row.rs`'s own
                    // `is_running` (the thing `state_dot_color`'s other caller
                    // feeds) is `matches!(s.state, Running { .. }) && !is_resuming`
                    // — it does NOT include `Stopping` either. Matching that
                    // exactly is what keeps the two surfaces' dots from
                    // disagreeing while a cancelled turn winds down.
                    is_running: matches!(session.state, SessionState::Running { .. }),
                })
            })
            .collect();
        candidates.sort_by_key(|c| c.tab_order);
        candidates
    }

    fn render_tab(
        &self,
        solution_id: SolutionId,
        candidate: &TabCandidate,
        is_active: bool,
        ix: usize,
        order: Vec<SolutionSessionId>,
        weak_self: WeakEntity<Self>,
        weak_workspace: Option<WeakEntity<Workspace>>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let session_id = candidate.session_id;
        let dot_color = state_dot_color(
            candidate.is_errored,
            candidate.is_running,
            candidate.is_cold,
        );
        let title = if candidate.title.is_empty() {
            SharedString::from(session_id.to_string())
        } else {
            candidate.title.clone()
        };
        // Selected-vs-unselected styling follows the fork's house style for a
        // tab strip (`solutions_ui::project_tab` / `solution_tab`): the active
        // tab is the only one with a background, it carries a 2px accent
        // underline, and its label is `Default` against the others' `Muted`.
        // Three signals instead of the two adjacent neutral greys
        // (`tab_active_background` vs `tab_inactive_background`) this strip
        // shipped with, which read as the same dark pill either way.
        let style = tab_selection_style(is_active);
        let background = style
            .filled
            .then(|| cx.theme().colors().tab_active_background);
        let border = if style.accent_underline {
            cx.theme().colors().border_focused
        } else {
            cx.theme().colors().border_transparent
        };

        let row = div()
            .id(("session-tab-strip-tab", ix))
            // Selection state is in the selector because `debug_bounds` can
            // only answer "did this paint": a paint test asserts both the
            // active and the inactive row exist, which a single shared
            // selector could not distinguish.
            .debug_selector(|| {
                format!(
                    "SESSION-TAB-{}-{ix}",
                    if is_active { "ACTIVE" } else { "INACTIVE" }
                )
            })
            .flex()
            .flex_none()
            .items_center()
            // A definite height, taken from the same `ButtonSize` metric the
            // neighbouring `+`/overflow `IconButton`s size themselves with, so
            // the pill matches them and scales with the status bar's rem
            // override. It must be explicit: `h_full()` here was inert (no
            // ancestor has a definite height), which left the row's height an
            // accident of its tallest child — the close cross this strip no
            // longer has. Without it the label sits in a pill with no
            // vertical extent at all.
            .h(ButtonSize::Default.rems())
            .gap_1()
            .px_1p5()
            // In rems, not px: the status bar overrides the rem size for its
            // subtree (`workspace::status_bar::STATUS_BAR_UI_SCALE`), so a
            // fixed pixel width would hold the tab at its old size while its
            // label grew — i.e. truncate more text than before.
            .min_w(rems_from_px(90.))
            .max_w(rems_from_px(180.))
            .rounded_sm()
            .when_some(background, |this, bg| this.bg(bg))
            .border_b_2()
            .border_color(border)
            .cursor_pointer()
            .child(Indicator::dot().color(dot_color))
            .child(
                // Own flex row at full height so the label is optically
                // centred in the pill, mirroring `console_panel::panel`'s tab.
                // NB: no `LineHeightStyle::UiLabel` — it pins line-height to
                // 1.0×font-size and `.truncate()` adds `overflow: hidden`, so
                // descenders get clipped at the bottom edge.
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .h_full()
                    .child(
                        Label::new(title.clone())
                            .size(LabelSize::Small)
                            .color(style.label)
                            .truncate(),
                    ),
            )
            // No close cross: closing a session tab goes through the
            // right-click menu's "Close" below (and the overflow popover's own
            // "Close" for a tab past `MAX_VISIBLE_TABS`), which is the same
            // `close_tab` — busy-state confirmation included. The cross was
            // also the only thing giving this row a height; see the `.h(..)`
            // above.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.on_tab_clicked(solution_id, session_id, cx);
                }),
            )
            // Drag-and-drop reorder, mirroring `project_tab`'s idiom (the
            // live precedent — see `reorder_to`'s doc comment for why NOT
            // the deleted dock-strip machinery). `on_drag` only fires past
            // GPUI's movement threshold, so a plain click still reaches
            // `on_mouse_down` above and selects the tab.
            .on_drag(
                DraggedSessionTab { session_id, title },
                |dragged, _offset, _window, cx| cx.new(|_| dragged.clone()),
            )
            .drag_over::<DraggedSessionTab>(|style, _dragged, _window, cx| {
                style.bg(cx.theme().colors().drop_target_background)
            })
            .on_drop(move |dragged: &DraggedSessionTab, _window, cx| {
                let new_order = reorder_to(&order, dragged.session_id, session_id);
                SolutionAgentStore::global(cx).update(cx, |store, cx| {
                    store.persist_tab_order(solution_id, new_order, cx);
                });
            });

        // Right-click menu restoring the affordances the old dock chat-tab
        // strip carried (see the phase-2a task-5b brief): rename, restart
        // the agent subprocess (keeping the conversation), and close. With
        // the cross gone this "Close" is the tab's only close affordance, so
        // it leads the menu; it runs the busy-confirmation `close_tab`.
        let menu_id = ElementId::from(SharedString::from(format!(
            "session-tab-strip-menu-{session_id}"
        )));
        let row_cell = RefCell::new(Some(row.into_any_element()));
        right_click_menu(menu_id)
            .trigger(move |_, _, _| {
                row_cell
                    .borrow_mut()
                    .take()
                    .unwrap_or_else(|| div().into_any_element())
            })
            .menu(move |window, cx| {
                let weak_close = weak_self.clone();
                let weak_workspace = weak_workspace.clone();
                ContextMenu::build(window, cx, move |menu, _window, _cx| {
                    let weak_close = weak_close.clone();
                    let weak_workspace = weak_workspace.clone();
                    menu.entry("Close", None, move |window, cx| {
                        weak_close
                            .update(cx, |this, cx| this.close_tab(session_id, window, cx))
                            .log_err();
                    })
                    .entry("Rename Session", None, move |window, cx| {
                        if let Some(weak_workspace) = weak_workspace.as_ref() {
                            open_rename_session_modal(weak_workspace, session_id, window, cx);
                        }
                    })
                    .entry("Restart Agent", None, move |_window, cx| {
                        SolutionAgentStore::global(cx)
                            .update(cx, |store, cx| store.restart_agent(session_id, cx))
                            .detach_and_log_err(cx);
                    })
                })
            })
    }

    /// The strip's trailing `+`: one plain left click creates a session, and
    /// that is all it does. It briefly also carried "Reopen Closed Chat…" on
    /// a right click (10120c6a27); the maintainer ruled that undiscoverable
    /// on 2026-09-04, so the reopen flow became
    /// [`Self::render_reopen_button`] and the gesture was removed rather than
    /// kept alongside it — two paths to one action is how the tooltip, the
    /// menu entry and the close prompt started disagreeing in the first
    /// place.
    ///
    /// Same `IconButton` at the same `IconSize::Small` it has always been, so
    /// the strip's row height (`ButtonSize::Default.rems()`, shared with the
    /// tab pills) — and with it the status bar's height — is unchanged.
    fn render_plus_button(&self, _cx: &Context<Self>) -> impl IntoElement {
        IconButton::new("session-tab-strip-plus", IconName::Plus)
            .icon_size(IconSize::Small)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text(PLUS_TOOLTIP))
            .on_click(|_, window, cx| {
                // Dispatched by name (not imported) — see the module doc for why.
                match cx.build_action("console_panel::NewChat", None) {
                    Ok(action) => window.dispatch_action(action, cx),
                    Err(err) => {
                        log::error!("session_tab_strip: console_panel::NewChat unavailable: {err}")
                    }
                }
            })
    }

    /// The reopen-a-closed-chat button, immediately right of the `+`.
    ///
    /// It sits on the strip rather than in any menu because the state it
    /// serves is "I just closed my last chat": the Solution then has **zero**
    /// session tabs, so a per-tab context menu has nothing to hang off, and
    /// the strip's own buttons are the only thing still painted. It is a
    /// visible icon rather than a gesture on the `+` because the gesture was
    /// not discoverable — the tooltip was the only thing advertising it, and
    /// a tooltip you have to already hover the right pixel to read is not an
    /// affordance.
    ///
    /// `HistoryRerun` (a clock with a counter-clockwise arrow) is the fork's
    /// existing "go back through history" glyph — `file_finder` marks history
    /// matches with it, `git_ui::undo_modal` heads its list with it, and the
    /// debugger's back-in-history button *is* it. A second `+`-family icon
    /// would have read as "new", which is the neighbouring button's job.
    ///
    /// Sized exactly like the `+`, so it adds width to the strip and nothing
    /// to the status bar's height.
    fn render_reopen_button(
        &self,
        solution_id: SolutionId,
        weak_workspace: Option<WeakEntity<Workspace>>,
        _cx: &Context<Self>,
    ) -> impl IntoElement {
        IconButton::new("session-tab-strip-reopen", IconName::HistoryRerun)
            .icon_size(IconSize::Small)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text(REOPEN_TOOLTIP))
            .on_click(move |_, window, cx| {
                let Some(weak_workspace) = weak_workspace.as_ref() else {
                    // No hosting workspace means the strip is painted outside
                    // a Solution window, where there is no session to reopen.
                    return;
                };
                open_reopen_session_modal(weak_workspace, solution_id, window, cx);
            })
    }
}

impl Render for SessionTabStrip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(solution_id) = self.active_solution_id(cx) else {
            return div().h_full().into_any_element();
        };

        let store = SolutionAgentStore::global(cx);
        let active_session = store.read(cx).active_dialog_session(solution_id);
        let candidates = self.candidates_for(solution_id, cx);
        let (visible, overflow) = split_visible_overflow(&candidates);
        // Full order across visible AND overflow tabs — `persist_tab_order`
        // NULLs `tab_order` on every session of the solution that is absent
        // from what it's handed, so a drop handler built from `visible`
        // alone would silently evict every overflowing tab from the strip
        // on the next reorder.
        let order: Vec<SolutionSessionId> = candidates.iter().map(|c| c.session_id).collect();
        let weak_self = cx.weak_entity();
        let weak_workspace = self.workspace_weak(cx);

        let tabs = visible.iter().enumerate().map(|(ix, candidate)| {
            self.render_tab(
                solution_id,
                candidate,
                active_session == Some(candidate.session_id),
                ix,
                order.clone(),
                weak_self.clone(),
                weak_workspace.clone(),
                cx,
            )
        });

        let overflow_popover = (!overflow.is_empty()).then(|| {
            // Each overflowing tab still needs everything a visible tab's
            // right-click menu offers — otherwise a session that spills
            // past `MAX_VISIBLE_TABS` becomes unreachable for rename/
            // restart/close from the UI (constraint C, phase-2a task-5b
            // brief).
            //
            // Tried first: a `custom_row` (opts out of `ContextMenu`'s own
            // click/dismiss machinery, `selectable: false`) hosting the SAME
            // `right_click_menu`-wrapped row a visible tab uses, so the
            // whole strip would share one interaction model. It does not
            // work: `ContextMenu::build`/`new` unconditionally wires
            // `cx.on_blur(focus_handle, ...)` to `cancel()` → `emit(
            // DismissEvent)` the instant the menu's focus_handle loses
            // focus (context_menu.rs ~273-296), and `right_click_menu`'s own
            // right-click handler grabs focus for its freshly-built inner
            // `ContextMenu` two frames later — which blurs whatever
            // currently holds focus, here the "more" popover's OUTER
            // `ContextMenu`. That immediately dismisses the outer menu,
            // which tears down its whole deferred child tree — including
            // the row and the inner menu nested inside it — before the
            // inner menu ever gets to render. Confirmed live: a right-click
            // on an overflow row closes the entire popover, no nested menu
            // appears. The one escape hatch for this exact race
            // (`ignore_blur_until`, used internally for submenu-vs-parent
            // blur races) is a private field with no public setter beyond
            // wholesale-replacing the blur subscription — fixing this
            // properly means reworking shared `ui::ContextMenu` internals,
            // out of scope here. So: each overflow row is a `submenu`
            // instead. "Select" reproduces the old plain-entry click, the
            // rest mirror the visible tab's menu exactly. Submenus open on
            // hover per `ContextMenu`'s own idiom (see repo `.rules`) — a
            // second interaction model from the visible tabs' right-click,
            // but a working one.
            let overflow_entries: Vec<(SolutionSessionId, SharedString)> = overflow
                .iter()
                .map(|c| {
                    let title = if c.title.is_empty() {
                        SharedString::from(c.session_id.to_string())
                    } else {
                        c.title.clone()
                    };
                    (c.session_id, title)
                })
                .collect();
            let weak_self = weak_self.clone();
            let weak_workspace = weak_workspace.clone();
            let more_button = IconButton::new("session-tab-strip-more", IconName::Ellipsis)
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("More AI sessions"));
            PopoverMenu::new("session-tab-strip-more-popover")
                .trigger(more_button)
                .menu(move |window, cx| {
                    let overflow_entries = overflow_entries.clone();
                    let weak_self = weak_self.clone();
                    let weak_workspace = weak_workspace.clone();
                    Some(ContextMenu::build(
                        window,
                        cx,
                        move |mut menu, _window, _cx| {
                            for (session_id, title) in overflow_entries {
                                let weak_close = weak_self.clone();
                                let weak_workspace = weak_workspace.clone();
                                menu = menu.submenu(title, move |submenu, _window, _cx| {
                                    let weak_close = weak_close.clone();
                                    let weak_workspace = weak_workspace.clone();
                                    submenu
                                        .entry("Select", None, move |_window, cx| {
                                            SolutionAgentStore::global(cx).update(
                                                cx,
                                                |store, cx| {
                                                    let current =
                                                        store.active_dialog_session(solution_id);
                                                    let next =
                                                        toggle_selection(current, session_id);
                                                    store.set_active_dialog_session(
                                                        solution_id,
                                                        next,
                                                        cx,
                                                    );
                                                },
                                            );
                                        })
                                        .entry("Rename Session", None, move |window, cx| {
                                            if let Some(weak_workspace) = weak_workspace.as_ref() {
                                                open_rename_session_modal(
                                                    weak_workspace,
                                                    session_id,
                                                    window,
                                                    cx,
                                                );
                                            }
                                        })
                                        .entry("Restart Agent", None, move |_window, cx| {
                                            SolutionAgentStore::global(cx)
                                                .update(cx, |store, cx| {
                                                    store.restart_agent(session_id, cx)
                                                })
                                                .detach_and_log_err(cx);
                                        })
                                        .entry("Close", None, move |window, cx| {
                                            weak_close
                                                .update(cx, |this, cx| {
                                                    this.close_tab(session_id, window, cx)
                                                })
                                                .log_err();
                                        })
                                });
                            }
                            menu
                        },
                    ))
                })
        });

        let group = div()
            .id("session-tab-strip")
            .flex()
            .items_center()
            .min_w_0()
            .h_full()
            .gap_1()
            .overflow_x_scroll()
            .children(tabs)
            .when_some(overflow_popover, |this, popover| this.child(popover))
            .child(self.render_plus_button(cx))
            .child(self.render_reopen_button(solution_id, weak_workspace.clone(), cx));

        // The rule is a sibling of the scrolling group, not its last child:
        // inside `overflow_x_scroll` it would slide out of view as soon as
        // enough tabs were open, and a boundary marker that scrolls away is
        // worse than none. It only exists on this branch — the early return
        // above (no active Solution, so nothing AI-related paints at all)
        // leaves a bare `div`, because a rule with an empty group on one side
        // is chrome rather than structure.
        h_flex()
            .h_full()
            .min_w_0()
            .child(group)
            .child(render_group_divider())
            .into_any_element()
    }
}

impl StatusItemView for SessionTabStrip {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        // Self-hiding: renders empty outside a Solution window (no
        // active_solution_id), so a user-facing "hide this button" setting
        // would have nothing stable to toggle.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    /// Window root that paints a fixed set of [`SessionTabStrip::render_tab`]
    /// rows. The strip's own `Render` is unusable here — it resolves the
    /// Solution through a live `MultiWorkspace`, which this scaffolding
    /// cannot build — but `render_tab` is the element under test and takes
    /// its inputs as plain data, so driving it directly paints exactly the
    /// tree the status bar paints.
    struct TabPaintHarness {
        strip: Entity<SessionTabStrip>,
        solution_id: SolutionId,
        /// `(session id, title, is_active)` per tab, left to right.
        tabs: Vec<(SolutionSessionId, SharedString, bool)>,
    }

    impl Render for TabPaintHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let solution_id = self.solution_id;
            let tabs = self.tabs.clone();
            let strip = self.strip.clone();
            let rows = strip.update(cx, |strip, cx| {
                let weak_self = cx.weak_entity();
                let order: Vec<SolutionSessionId> = tabs.iter().map(|(id, _, _)| *id).collect();
                tabs.iter()
                    .enumerate()
                    .map(|(ix, (session_id, title, is_active))| {
                        let candidate = TabCandidate {
                            session_id: *session_id,
                            tab_order: ix as i64,
                            title: title.clone(),
                            is_cold: true,
                            is_errored: false,
                            is_running: false,
                        };
                        strip
                            .render_tab(
                                solution_id,
                                &candidate,
                                *is_active,
                                ix,
                                order.clone(),
                                weak_self.clone(),
                                None,
                                cx,
                            )
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            });
            div().flex().items_center().gap_1().children(rows)
        }
    }

    // The `+` button dispatches `console_panel::NewChat` *by name* (see the
    // module doc) rather than importing its type — a real dev-dependency on
    // `console_panel` here would create a Cargo dev-dependency CYCLE
    // (`console_panel` depends on `solution_agent` normally) that duplicates
    // this crate's own compilation for the test binary, which then panics
    // at startup on `inventory`-based action registration ("Action with name
    // `solution_agent::FindClose` already registered" — verified by trying
    // it). So the guard against a silent rename lives in
    // `console_panel::panel::tests::new_chat_action_matches_the_status_bar_strips_dispatch_string`
    // instead, where `console_panel` is linked exactly once and can assert
    // `NewChat.name()` directly (no registry round-trip needed).

    #[test]
    fn tabs_beyond_the_visible_cap_spill_into_the_overflow_list() {
        let ids: Vec<SolutionSessionId> = (0..MAX_VISIBLE_TABS + 3)
            .map(|_| SolutionSessionId::new())
            .collect();
        let (visible, overflow) = split_visible_overflow(&ids);
        assert_eq!(visible.len(), MAX_VISIBLE_TABS);
        assert_eq!(overflow.len(), 3);
        assert_eq!(visible, &ids[..MAX_VISIBLE_TABS]);
        assert_eq!(overflow, &ids[MAX_VISIBLE_TABS..]);
    }

    #[test]
    fn reorder_to_moves_the_dragged_tab_next_to_its_drop_target() {
        let ids: Vec<SolutionSessionId> = (0..4).map(|_| SolutionSessionId::new()).collect();
        let order = ids.clone();

        // Move right: dragging the first tab onto the third lands it
        // directly before the third.
        let moved_right = reorder_to(&order, ids[0], ids[2]);
        assert_eq!(moved_right, vec![ids[1], ids[0], ids[2], ids[3]]);

        // Move left: dragging the last tab onto the second lands it
        // directly before the second.
        let moved_left = reorder_to(&order, ids[3], ids[1]);
        assert_eq!(moved_left, vec![ids[0], ids[3], ids[1], ids[2]]);
    }

    #[test]
    fn reorder_to_is_a_noop_for_a_tab_dropped_onto_itself() {
        let ids: Vec<SolutionSessionId> = (0..3).map(|_| SolutionSessionId::new()).collect();
        assert_eq!(reorder_to(&ids, ids[1], ids[1]), ids);
    }

    #[test]
    fn reorder_to_is_a_noop_for_an_unknown_id() {
        let ids: Vec<SolutionSessionId> = (0..3).map(|_| SolutionSessionId::new()).collect();
        let unknown = SolutionSessionId::new();
        assert_eq!(reorder_to(&ids, unknown, ids[0]), ids);
        assert_eq!(reorder_to(&ids, ids[0], unknown), ids);
    }

    #[test]
    fn reorder_to_handles_the_boundary_positions() {
        let ids: Vec<SolutionSessionId> = (0..4).map(|_| SolutionSessionId::new()).collect();
        // Dragging the first tab all the way past every other tab lands it
        // just before the last one (there is no separate "append past the
        // last tab" drop zone in this strip — see the module's drag-reorder
        // notes in the phase-2a task-5b report).
        assert_eq!(
            reorder_to(&ids, ids[0], ids[3]),
            vec![ids[1], ids[2], ids[0], ids[3]]
        );
        // Dragging the last tab onto the first becomes the new first tab.
        assert_eq!(
            reorder_to(&ids, ids[3], ids[0]),
            vec![ids[3], ids[0], ids[1], ids[2]]
        );
    }

    #[test]
    fn is_busy_state_matches_running_and_stopping_only() {
        use crate::model::SessionState;
        use std::time::Instant;

        assert!(is_busy_state(&SessionState::Running {
            started_at: Instant::now(),
            notified: false,
        }));
        assert!(is_busy_state(&SessionState::Stopping {
            started_at: Instant::now(),
        }));
        assert!(!is_busy_state(&SessionState::Idle));
        assert!(!is_busy_state(&SessionState::AwaitingInput));
        assert!(!is_busy_state(&SessionState::Errored(SharedString::from(
            "boom"
        ))));
    }

    #[test]
    fn the_selected_tab_differs_from_an_unselected_one_on_every_signal() {
        let active = tab_selection_style(true);
        let inactive = tab_selection_style(false);

        assert_eq!(
            active,
            TabSelectionStyle {
                filled: true,
                accent_underline: true,
                label: Color::Default,
            }
        );
        assert_eq!(
            inactive,
            TabSelectionStyle {
                filled: false,
                accent_underline: false,
                label: Color::Muted,
            },
            "an unselected tab gets no background at all — the previous \
             `tab_inactive_background` pill was indistinguishable from the \
             active one"
        );
        assert_ne!(active.filled, inactive.filled);
        assert_ne!(active.accent_underline, inactive.accent_underline);
        assert_ne!(active.label, inactive.label);
    }

    /// The painted tree, not the predicate: that a tab row carries no close
    /// cross, that both selection states actually paint, and that the row has
    /// the `+` button's height rather than collapsing to its content now that
    /// the cross is gone.
    #[gpui::test]
    async fn a_painted_tab_has_no_close_cross_and_the_button_row_height(
        cx: &mut TestAppContext,
    ) {
        let (solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            let registry = Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });

        drop(project);
        let (_harness, cx) = cx.add_window_view(|_window, cx| TabPaintHarness {
            strip: cx.new(|cx| SessionTabStrip::new(None, cx)),
            solution_id,
            tabs: vec![
                (SolutionSessionId::new(), SharedString::from("first"), false),
                (SolutionSessionId::new(), SharedString::from("second"), true),
            ],
        });
        cx.run_until_parked();

        let inactive = cx
            .debug_bounds("SESSION-TAB-INACTIVE-0")
            .expect("the unselected tab must paint");
        let active = cx
            .debug_bounds("SESSION-TAB-ACTIVE-1")
            .expect("the selected tab must paint");
        assert!(
            cx.debug_bounds("ICON-Close").is_none(),
            "a session tab must carry no close cross — closing is right-click \
             only; this assertion is only meaningful because the two rows \
             above did paint"
        );

        // `ButtonSize::Default` at the test window's default 16px rem: the
        // metric the neighbouring `+` / overflow buttons use.
        assert_eq!(active.size.height, px(22.));
        assert_eq!(
            inactive.size.height,
            active.size.height,
            "selection must not change the row's height"
        );
    }

    /// Stand-in for `console_panel::NewChat`, registered under that exact
    /// wire name. The `+`'s left click builds the action *by name* because
    /// `solution_agent` cannot depend on `console_panel` (the edge runs the
    /// other way), which also means the real action is not linked into this
    /// crate's test binary — so without a stub, "left click creates a
    /// session" is unobservable and the click silently `log::error!`s. This
    /// is `#[cfg(test)]`, so it exists only in `solution_agent`'s own lib
    /// test binary, where `console_panel` is absent and there is nothing to
    /// collide with in the action registry. Paired with
    /// `console_panel::panel::tests::new_chat_action_matches_the_status_bar_strips_dispatch_string`,
    /// which pins the real action to the same string, this closes the loop.
    mod stub {
        gpui::actions!(
            console_panel,
            [
                /// Test stand-in for the real `console_panel::NewChat`.
                NewChat
            ]
        );
    }

    /// Window root hosting the strip's REAL `Render` (not `TabPaintHarness`,
    /// which paints hand-built rows) plus an action handler, so a click that
    /// reaches the `+` can be observed as a dispatch rather than inferred.
    struct StripHarness {
        strip: Entity<SessionTabStrip>,
        new_chat_dispatches: Rc<RefCell<usize>>,
        /// The harness must hold focus: `Window::dispatch_action` routes to
        /// the focused dispatch node and bubbles UP from there, so an
        /// `on_action` on an unfocused root is never in the path — the same
        /// place the real app puts this handler (`workspace.register_action`,
        /// an ancestor of whatever holds focus).
        focus_handle: gpui::FocusHandle,
    }

    impl Render for StripHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let dispatches = self.new_chat_dispatches.clone();
            div()
                .size_full()
                .track_focus(&self.focus_handle)
                .on_action(move |_: &stub::NewChat, _window, _cx| {
                    *dispatches.borrow_mut() += 1;
                })
                .child(self.strip.clone())
        }
    }

    /// Boot a Solution whose worktree the strip can resolve (a live
    /// `MultiWorkspace`, so the real `Render` reaches past its
    /// `active_solution_id` early return) with **zero** sessions, and paint
    /// it. Returns the `NewChat` dispatch counter, the `Workspace` the strip
    /// hosts its modals on, and the visual context.
    async fn paint_strip_with_no_tabs(
        cx: &mut TestAppContext,
    ) -> (
        Rc<RefCell<usize>>,
        Entity<Workspace>,
        gpui::VisualTestContext,
    ) {
        let (_solution_id, tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            let registry = Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let multi_workspace = multi_workspace
            .root(cx)
            .expect("the multi-workspace window's root");
        // The strip hosts the reopen picker on the Solution's active
        // workspace (`workspace_weak`), so that is where a click's modal
        // lands — not in the harness window the strip itself paints in.
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let new_chat_dispatches = Rc::new(RefCell::new(0usize));
        let window = cx.add_window(|_window, cx| StripHarness {
            strip: cx.new(|cx| SessionTabStrip::new(Some(multi_workspace.downgrade()), cx)),
            new_chat_dispatches: new_chat_dispatches.clone(),
            focus_handle: cx.focus_handle(),
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.run_until_parked();
        window
            .update(&mut visual, |harness, window, cx| {
                window.focus(&harness.focus_handle, cx);
            })
            .expect("focus the harness");
        visual.run_until_parked();

        assert!(
            visual.debug_bounds("SESSION-TAB-INACTIVE-0").is_none()
                && visual.debug_bounds("SESSION-TAB-ACTIVE-0").is_none(),
            "these tests are only meaningful with zero session tabs painted"
        );
        // The tempdir backs the Solution's on-disk root for the whole test.
        std::mem::forget(tmp);
        (new_chat_dispatches, workspace, visual)
    }

    /// The `+`'s plain left click must still create a session in one click —
    /// and must NOT open the reopen picker, which now lives on the button
    /// immediately next to it. Both sides asserted: a regression that wired
    /// the two buttons to each other's handler would otherwise show up as
    /// only one of "no dispatch" or "a modal appeared".
    #[gpui::test]
    async fn left_clicking_the_plus_creates_a_session_and_opens_no_picker(cx: &mut TestAppContext) {
        let (new_chat_dispatches, workspace, mut cx) = paint_strip_with_no_tabs(cx).await;

        let plus = cx
            .debug_bounds("ICON-Plus")
            .expect("the strip's `+` must paint even with no session tabs");
        // Height guard: the `+` must stay the same `ButtonSize::Default` row
        // the tab pills and the new reopen button use — growing it would grow
        // the status bar.
        assert_eq!(plus.size.height, px(22.));

        // Rest the cursor on the button first — the `windows.click_at` /
        // `hover_at` pairing the MCP layer uses, and what a real pointer does.
        cx.simulate_event(gpui::MouseMoveEvent {
            position: plus.center(),
            pressed_button: None,
            modifiers: gpui::Modifiers::default(),
        });
        cx.simulate_click(plus.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert_eq!(
            *new_chat_dispatches.borrow(),
            1,
            "a plain left click on `+` must dispatch console_panel::NewChat"
        );
        assert_eq!(
            workspace.read_with(&cx, |workspace, cx| workspace.active_modal_kind(cx)),
            None,
            "a left click on `+` must not open the reopen picker"
        );
    }

    /// The reopen-a-closed-chat flow is a **visible button** next to the `+`
    /// (maintainer request, 2026-09-04 — as a right-click gesture on the `+`
    /// it was not discoverable). The case that decides where it can live is a
    /// Solution with **zero** session tabs: a user who just closed their last
    /// chat has no tab to right-click and no tab strip to read, so the
    /// recovery path has to be one of the buttons that still paints. Clicks
    /// the button that actually painted and checks the picker opened — and
    /// that the neighbouring `+` did not also fire.
    #[gpui::test]
    async fn the_reopen_button_opens_the_picker_even_with_no_tabs(cx: &mut TestAppContext) {
        let (new_chat_dispatches, workspace, mut cx) = paint_strip_with_no_tabs(cx).await;

        let reopen = cx
            .debug_bounds("ICON-HistoryRerun")
            .expect("the strip's reopen button must paint even with no session tabs");
        // Same row metric as the `+` and the tab pills: this button is new, so
        // it is the one that could silently make the status bar taller.
        assert_eq!(reopen.size.height, px(22.));

        cx.simulate_event(gpui::MouseMoveEvent {
            position: reopen.center(),
            pressed_button: None,
            modifiers: gpui::Modifiers::default(),
        });
        cx.simulate_click(reopen.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert_eq!(
            workspace.read_with(&cx, |workspace, cx| workspace.active_modal_kind(cx)),
            Some("ReopenSession"),
            "clicking the reopen button must open the closed-chat picker"
        );
        assert_eq!(
            *new_chat_dispatches.borrow(),
            0,
            "the reopen button must not also create a session"
        );
    }

    /// The right-click gesture the `+` briefly carried (10120c6a27) is gone:
    /// the visible button above replaced it, and leaving both would be two
    /// paths to one action. Asserted on the painted tree, since "the menu
    /// builder was deleted" is exactly the kind of predicate that stays true
    /// while some other wrapper keeps opening a menu.
    #[gpui::test]
    async fn right_clicking_the_plus_opens_nothing(cx: &mut TestAppContext) {
        let (new_chat_dispatches, workspace, mut cx) = paint_strip_with_no_tabs(cx).await;

        let plus = cx
            .debug_bounds("ICON-Plus")
            .expect("the strip's `+` must paint even with no session tabs");
        let position = plus.center();
        cx.simulate_event(gpui::MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers: gpui::Modifiers::default(),
        });
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position,
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        cx.run_until_parked();

        // `debug_bounds` takes a `'static` selector; leaking a test-local
        // string keeps this derived from `REOPEN_TOOLTIP` instead of retyping
        // the label the const exists to stop people retyping.
        let reopen_menu_item: &'static str = format!("MENU_ITEM-{REOPEN_TOOLTIP}").leak();
        assert!(
            cx.debug_bounds(reopen_menu_item).is_none(),
            "the `+` must no longer carry a reopen menu — the button next to it is the one path"
        );
        assert_eq!(
            workspace.read_with(&cx, |workspace, cx| workspace.active_modal_kind(cx)),
            None,
            "a right click on `+` must not open the picker either"
        );
        assert_eq!(
            *new_chat_dispatches.borrow(),
            0,
            "a right click must not create a session"
        );
    }

    /// The close prompt promises a way to get the tab back; the reopen
    /// button's tooltip is what the user reads on the thing that keeps that
    /// promise. Its predecessor pointed at "Reopen Closed Chat" *in the
    /// console panel* and went stale the moment the flow moved, so both
    /// strings come off one const and this pins that they still agree — and
    /// that neither still advertises the right-click gesture that no longer
    /// exists.
    #[test]
    fn the_close_prompt_and_the_tooltip_point_at_the_same_affordance() {
        let detail = close_prompt_detail();
        assert!(
            detail.contains(REOPEN_TOOLTIP),
            "the close prompt must name the reopen button verbatim: {detail}"
        );
        assert!(
            !detail.contains("right-click") && !PLUS_TOOLTIP.contains("right-click"),
            "the reopen right-click on `+` is gone; neither string may still promise it: \
             {detail} / {PLUS_TOOLTIP}"
        );
        assert_ne!(
            PLUS_TOOLTIP, REOPEN_TOOLTIP,
            "two buttons, two names — a shared tooltip would make them indistinguishable"
        );
        assert!(
            !detail.contains("console panel"),
            "the console panel no longer hosts this flow: {detail}"
        );
    }

    /// The group rule paints beside a live AI group. Paired with
    /// [`the_group_divider_is_absent_when_the_strip_has_no_ai_group`] below,
    /// which covers the other side.
    #[gpui::test]
    async fn the_group_divider_paints_beside_the_ai_group(cx: &mut TestAppContext) {
        let (_new_chat_dispatches, _workspace, mut cx) = paint_strip_with_no_tabs(cx).await;

        let plus = cx
            .debug_bounds("ICON-Plus")
            .expect("the AI group must have content for the rule to close off");
        let divider = cx
            .debug_bounds(GROUP_DIVIDER_SELECTOR)
            .expect("the rule separating the AI group from the rest of the left items must paint");
        assert!(
            divider.origin.x > plus.origin.x,
            "the rule closes the group's *trailing* edge: the strip is the leftmost status-bar \
             item, so the other left-hand items are to its right ({divider:?} vs {plus:?})"
        );
        // The rule is exactly as tall as the row it bounds, so it cannot be
        // what makes the status bar grow.
        assert_eq!(divider.size.height, px(22.));
    }

    /// …and it is absent when there is no AI group at all. "Empty" here is
    /// the strip's `active_solution_id` early return — outside a Solution
    /// window nothing AI-related paints, and a rule with nothing on one side
    /// is chrome rather than structure. The `+` assertion is what makes this
    /// meaningful: it proves the empty branch is the one that rendered.
    #[gpui::test]
    async fn the_group_divider_is_absent_when_the_strip_has_no_ai_group(cx: &mut TestAppContext) {
        let (_solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            let registry = Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });
        drop(project);

        // No `MultiWorkspace`, so `active_solution_id` is `None` and `render`
        // takes its early return.
        let (_strip, cx) = cx.add_window_view(|_window, cx| SessionTabStrip::new(None, cx));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("ICON-Plus").is_none()
                && cx.debug_bounds("ICON-HistoryRerun").is_none(),
            "this test is only meaningful when the AI group painted nothing"
        );
        assert!(
            cx.debug_bounds(GROUP_DIVIDER_SELECTOR).is_none(),
            "the rule must not paint with an empty group on one side of it"
        );
    }

    #[test]
    fn all_tabs_are_visible_when_under_the_cap() {
        let ids: Vec<SolutionSessionId> = (0..2).map(|_| SolutionSessionId::new()).collect();
        let (visible, overflow) = split_visible_overflow(&ids);
        assert_eq!(visible.len(), 2);
        assert!(overflow.is_empty());
    }

    #[test]
    fn toggling_the_already_active_tab_collapses_the_selection() {
        let id = SolutionSessionId::new();
        let other = SolutionSessionId::new();
        assert_eq!(toggle_selection(None, id), Some(id));
        assert_eq!(toggle_selection(Some(id), id), None);
        assert_eq!(toggle_selection(Some(other), id), Some(id));
    }

    /// The restart path, end to end: rows on disk, an empty in-memory store,
    /// and `hydrate_all_for_solution` — the function `SolutionStoreEvent::Opened`
    /// actually runs — as the only thing that repopulates it. The strip has to
    /// come back with one tab per restored session, in `tab_order` order.
    ///
    /// The hydration tests used to drive `restore_open_tabs`, which no
    /// production code path called; that blind spot is why a restart shipped
    /// with the strip empty even though the transcripts, the tab order and the
    /// persisted active-dialog selection had all been restored. That function
    /// is gone and its tests were re-pointed here, but keep this one: it is the
    /// only one that asserts on the strip itself rather than on the store.
    #[gpui::test]
    async fn cold_hydrated_sessions_come_back_as_tabs(cx: &mut TestAppContext) {
        use crate::store::SolutionAgentStoreEvent;
        use chrono::Utc;
        use std::path::PathBuf;

        let (solution_id, _tmp, _project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            let registry = Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });
        let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
        cx.update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_persistence(db.clone(), cx);
            });
        });

        let id_a = SolutionSessionId::new();
        let id_b = SolutionSessionId::new();
        let id_child = SolutionSessionId::new();
        let now = Utc::now();
        let meta_a = crate::model::SolutionSessionMetadata {
            id: id_a,
            solution_id,
            agent_id: SharedString::from("claude-acp"),
            acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
            title: SharedString::from("session A"),
            created_at: now,
            last_activity_at: now,
            preview: None,
            total_tokens: None,
            context_count: 1,
            cwd: PathBuf::new(),
            parent_session_id: None,
            desired_model: None,
            desired_effort: None,
            cached_models: vec![],
            tab_order: None,
        };
        let meta_b = crate::model::SolutionSessionMetadata {
            id: id_b,
            acp_session_id: agent_client_protocol::schema::SessionId::new("acp-b"),
            title: SharedString::from("session B"),
            ..meta_a.clone()
        };
        // A sub-agent: persisted and hydrated, but never pinned into the strip.
        let meta_child = crate::model::SolutionSessionMetadata {
            id: id_child,
            acp_session_id: agent_client_protocol::schema::SessionId::new("acp-child"),
            title: SharedString::from("child of A"),
            parent_session_id: Some(id_a),
            ..meta_a.clone()
        };
        db.save_metadata(meta_a).await.expect("meta a");
        db.save_metadata(meta_b).await.expect("meta b");
        db.save_metadata(meta_child).await.expect("meta child");
        // B sits left of A, so a strip that merely preserved DB row order
        // would get this backwards.
        db.update_tab_orders(solution_id, vec![id_b, id_a])
            .await
            .expect("tab order");

        let tabs_opened = Rc::new(RefCell::new(Vec::<SolutionSessionId>::new()));
        let created = Rc::new(RefCell::new(Vec::<SolutionSessionId>::new()));
        let _subscription = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let tabs_opened = tabs_opened.clone();
            let created = created.clone();
            cx.subscribe(&store, move |_store, event, _cx| match event {
                SolutionAgentStoreEvent::TabsChanged { opened, .. } => {
                    tabs_opened.borrow_mut().extend(opened.iter().copied());
                }
                SolutionAgentStoreEvent::SessionCreated { id, .. } => {
                    created.borrow_mut().push(*id);
                }
                _ => {}
            })
        });

        cx.update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.hydrate_all_for_solution(solution_id, cx)
            })
        })
        .await
        .expect("hydrate");
        cx.run_until_parked();

        let strip = cx.update(|cx| cx.new(|cx| SessionTabStrip::new(None, cx)));
        let candidates = strip.read_with(cx, |strip, cx| strip.candidates_for(solution_id, cx));
        let tabbed: Vec<(SolutionSessionId, i64)> = candidates
            .iter()
            .map(|candidate| (candidate.session_id, candidate.tab_order))
            .collect();
        assert_eq!(
            tabbed,
            vec![(id_b, 0), (id_a, 1)],
            "restored tabs must appear in tab_order, and the un-pinned sub-agent must not"
        );

        // `by_solution` insertion order is the ordering contract
        // `hydrate_all_for_solution` documents: tab_order ASC, untabbed last.
        let indexed = cx.update(|cx| {
            SolutionAgentStore::global(cx).read_with(cx, |store, cx| {
                store
                    .sessions_for(&solution_id)
                    .into_iter()
                    .map(|session| session.read(cx).id)
                    .collect::<Vec<_>>()
            })
        });
        assert_eq!(indexed, vec![id_b, id_a, id_child]);

        // The band resolves its dialog through `store.session(..)`, which sees
        // all three; every path that SELECTS a dialog must instead ask
        // `can_be_active_dialog`, or it can persist a selection pointing at a
        // session the strip refuses to draw a tab for — leaving the user a
        // dialog they cannot leave, across restarts.
        let selectable = cx.update(|cx| {
            SolutionAgentStore::global(cx).read_with(cx, |store, cx| {
                [id_b, id_a, id_child]
                    .map(|id| {
                        store
                            .session(id)
                            .is_some_and(|session| session.read(cx).can_be_active_dialog())
                    })
                    .to_vec()
            })
        });
        assert_eq!(
            selectable,
            vec![true, true, false],
            "the dialog-selection predicate must admit exactly what `candidates_for` tabs"
        );

        assert_eq!(
            *tabs_opened.borrow(),
            vec![id_b, id_a],
            "the strip observes events, not the store entity — hydration must emit \
             TabsChanged for the pinned sessions or the restored tabs never paint"
        );
        assert!(
            created.borrow().is_empty(),
            "hydration must not masquerade as session creation: it already emits its own \
             workspace.session_opened deltas, so SessionCreated would double-announce"
        );
    }

    #[gpui::test]
    async fn clicking_a_session_tab_sets_the_active_dialog(cx: &mut TestAppContext) {
        let (solution_id, _tmp, _project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            let registry = Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });

        let (id_a, id_b) = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                let id_a = SolutionSessionId::new();
                crate::store::tests::insert_cold_session(
                    id_a,
                    solution_id,
                    SharedString::from("claude-acp"),
                    None,
                    None,
                    store,
                    cx,
                );
                let id_b = SolutionSessionId::new();
                crate::store::tests::insert_cold_session(
                    id_b,
                    solution_id,
                    SharedString::from("claude-acp"),
                    None,
                    None,
                    store,
                    cx,
                );
                (id_a, id_b)
            })
        });

        // Not rendered: `ConsoleTab::Chat` (and, once task 4 lands, the band
        // itself) needs a live `SolutionSessionView` embedding a real
        // `editor::Editor`, which this test scaffolding cannot construct
        // (see the module doc + phase-2a task-3 brief). Calling
        // `on_tab_clicked` directly exercises exactly what the tab's
        // `on_mouse_down` handler calls — mirrors how
        // `console_panel::panel::tests` exercises `activate_tab` /
        // `close_tab` directly rather than synthesizing a real click.
        let strip = cx.update(|cx| cx.new(|cx| SessionTabStrip::new(None, cx)));

        strip.update(cx, |strip, cx| {
            strip.on_tab_clicked(solution_id, id_b, cx);
        });
        let active = cx.update(|cx| {
            SolutionAgentStore::global(cx)
                .read(cx)
                .active_dialog_session(solution_id)
        });
        assert_eq!(active, Some(id_b));

        // Re-clicking the now-active tab collapses the selection.
        strip.update(cx, |strip, cx| {
            strip.on_tab_clicked(solution_id, id_b, cx);
        });
        let active = cx.update(|cx| {
            SolutionAgentStore::global(cx)
                .read(cx)
                .active_dialog_session(solution_id)
        });
        assert_eq!(active, None);

        // Clicking the other (inactive) tab selects it, not id_a==id_b confusion.
        strip.update(cx, |strip, cx| {
            strip.on_tab_clicked(solution_id, id_a, cx);
        });
        let active = cx.update(|cx| {
            SolutionAgentStore::global(cx)
                .read(cx)
                .active_dialog_session(solution_id)
        });
        assert_eq!(active, Some(id_a));
    }
}
