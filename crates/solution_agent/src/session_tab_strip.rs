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
//! `solution_agent`. The trailing `+` button therefore dispatches
//! `console_panel::NewChat` *dynamically by name* (`cx.build_action`) rather
//! than importing the action type — the same cross-crate action-dispatch
//! idiom already used by `git_ui::commit_context_menu` for
//! `solution_git::CrossCherryPick` / `git_graph::ShowAffectedPathsInLog`.
//! Reusing that action (instead of calling `SolutionAgentStore::create_session`
//! directly) keeps session creation on exactly one code path — two paths
//! disagreeing about the new session's cwd was the phase-1 Critical.

use gpui::{
    App, Context, IntoElement, ParentElement, PromptLevel, Render, SharedString, Styled,
    Subscription, WeakEntity, Window, div, px,
};
use solutions::{SolutionId, SolutionStore};
use ui::{ContextMenu, Indicator, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::item::ItemHandle;
use workspace::{HideStatusItem, MultiWorkspace, StatusItemView};

use crate::model::{SessionState, SolutionSessionId};
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
#[derive(Clone)]
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

    /// Close a tab's session, mirroring `console_panel::ConsolePanel::close_tab_at`'s
    /// busy-state speed bump: a session that's still `Running`/`Stopping` gets a
    /// confirmation prompt (closing abandons in-flight agent work), a terminal-state
    /// session closes straight through. Same underlying `close_session` call either
    /// way, so this surface and the old dock tab strip cannot diverge on what "close"
    /// means while both exist (phase 2a tasks 3–5).
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
            .map(|session| {
                matches!(
                    session.read(cx).state,
                    SessionState::Running { .. } | SessionState::Stopping { .. }
                )
            })
            .unwrap_or(false);
        if !busy {
            store
                .update(cx, |store, cx| store.close_session(session_id, cx))
                .log_err();
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Close this AI session's tab?",
            Some(
                "The agent is still working. Closing interrupts the current turn — the tab can \
                 be brought back via \"Reopen Closed Chat\" in the console panel.",
            ),
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
                if session.is_supervisor_ephemeral || session.is_ephemeral {
                    return None;
                }
                let tab_order = session.tab_order?;
                Some(TabCandidate {
                    session_id: session.id,
                    tab_order,
                    title: session.title.clone(),
                    is_cold: session.is_cold(),
                    is_errored: matches!(session.state, SessionState::Errored(_)),
                    is_running: matches!(
                        session.state,
                        SessionState::Running { .. } | SessionState::Stopping { .. }
                    ),
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
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let session_id = candidate.session_id;
        let dot_color = state_dot_color(candidate.is_errored, candidate.is_running, candidate.is_cold);
        let title = if candidate.title.is_empty() {
            SharedString::from(session_id.to_string())
        } else {
            candidate.title.clone()
        };
        let bg = if is_active {
            cx.theme().colors().tab_active_background
        } else {
            cx.theme().colors().tab_inactive_background
        };

        div()
            .id(("session-tab-strip-tab", ix))
            .flex()
            .flex_none()
            .items_center()
            .h_full()
            .gap_1()
            .px_1p5()
            .min_w(px(90.))
            .max_w(px(180.))
            .rounded_sm()
            .bg(bg)
            .cursor_pointer()
            .child(Indicator::dot().color(dot_color))
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(title)
                        .size(LabelSize::Small)
                        .truncate(),
                ),
            )
            .child(
                IconButton::new(("session-tab-strip-close", ix), IconName::Close)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.close_tab(session_id, window, cx);
                    })),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.on_tab_clicked(solution_id, session_id, cx);
                }),
            )
    }

    fn render_plus_button(&self, _cx: &Context<Self>) -> impl IntoElement {
        IconButton::new("session-tab-strip-plus", IconName::Plus)
            .icon_size(IconSize::Small)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text("New AI session"))
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

        let tabs = visible.iter().enumerate().map(|(ix, candidate)| {
            self.render_tab(
                solution_id,
                candidate,
                active_session == Some(candidate.session_id),
                ix,
                cx,
            )
        });

        let overflow_popover = (!overflow.is_empty()).then(|| {
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
            let more_button = IconButton::new("session-tab-strip-more", IconName::Ellipsis)
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("More AI sessions"));
            PopoverMenu::new("session-tab-strip-more-popover")
                .trigger(more_button)
                .menu(move |window, cx| {
                    let overflow_entries = overflow_entries.clone();
                    Some(ContextMenu::build(
                        window,
                        cx,
                        move |mut menu, _window, _cx| {
                            for (session_id, title) in overflow_entries {
                                menu = menu.entry(title, None, move |_window, cx| {
                                    SolutionAgentStore::global(cx).update(cx, |store, cx| {
                                        let current = store.active_dialog_session(solution_id);
                                        let next = toggle_selection(current, session_id);
                                        store.set_active_dialog_session(solution_id, next, cx);
                                    });
                                });
                            }
                            menu
                        },
                    ))
                })
        });

        div()
            .id("session-tab-strip")
            .flex()
            .items_center()
            .h_full()
            .gap_1()
            .overflow_x_scroll()
            .children(tabs)
            .when_some(overflow_popover, |this, popover| this.child(popover))
            .child(self.render_plus_button(cx))
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
        // Self-hiding, like `SolutionAgentStatusItem`: renders empty outside
        // a Solution window (no active_solution_id), so a user-facing "hide
        // this button" setting would have nothing stable to toggle.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::sync::Arc;

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
