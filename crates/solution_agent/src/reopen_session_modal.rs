//! Modal that reopens a closed Solution chat session.
//!
//! Closing a chat tab fully closes the session: its transcript is flushed
//! to disk and the row is marked `closed_at` (see
//! [`SolutionAgentStore::close_session`]). This modal lists the active
//! solution's *closed* sessions straight from the DB — each row showing its
//! context size (cumulative tokens) and last-activity time, most-recent
//! first — and reopens the selected one via
//! [`SolutionAgentStore::reopen_closed_session`], which clears the close
//! marker, re-hydrates the transcript, and pins it back into the strip.

use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, Styled, WeakEntity, Window, div, rems,
};
use ui::prelude::*;
use ui::{Label, LabelSize};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::model::{SolutionSessionId, SolutionSessionMetadata};
use crate::status_row::{format_tokens_compact, relative_time_short};
use crate::store::SolutionAgentStore;
use solutions::SolutionId;

/// A closed session offered for reopening: id + solution it belongs to,
/// display title, and the metadata shown per row (cumulative context tokens
/// and last-activity time).
#[derive(Clone)]
pub struct ReopenableSession {
    pub id: SolutionSessionId,
    pub solution_id: SolutionId,
    pub title: SharedString,
    pub total_tokens: Option<u64>,
    pub last_activity_at: chrono::DateTime<chrono::Utc>,
}

impl ReopenableSession {
    /// Build a row from a DB metadata record.
    pub fn from_metadata(meta: &SolutionSessionMetadata) -> Self {
        Self {
            id: meta.id,
            solution_id: meta.solution_id,
            title: meta.title.clone(),
            total_tokens: meta.total_tokens,
            last_activity_at: meta.last_activity_at,
        }
    }
}

/// Workspaces whose closed-session query is still in flight.
///
/// The picker cannot open synchronously — its rows come from a DB round trip —
/// so a second click landing while the first query runs used to reach
/// `toggle_modal` a second time and TOGGLE the just-opened picker back shut.
/// A double click on the button therefore opened and immediately closed it.
/// Keyed by workspace so two windows can each open their own picker.
#[derive(Default)]
struct ReopenQueryInFlight(std::collections::HashSet<gpui::EntityId>);

impl gpui::Global for ReopenQueryInFlight {}

/// Open the reopen-a-closed-chat picker over `weak_workspace`.
///
/// Lives here, next to the modal, rather than on whatever surface offers the
/// entry point: it used to be a `ConsolePanel` method, but the console
/// panel's `+` no longer offers AI-session entries at all (AI sessions live
/// on the status-bar session tab strip), and `solution_agent` cannot depend
/// on `console_panel` to call back into it — that edge already runs the other
/// way. Closed sessions live only on disk, so the list is queried
/// asynchronously and the modal is opened once it resolves.
///
/// Re-entrancy: a call made while this workspace's query is still running, or
/// while its picker is already up, is a no-op. Both halves are needed — the
/// flag covers the in-flight window, and the already-open check (made inside
/// the continuation, where the workspace is legitimately borrowed) covers a
/// click after the picker has painted. Dismissing it stays the modal's own job
/// (Esc / click-away), which is why neither path toggles.
pub fn open_reopen_session_modal(
    weak_workspace: &WeakEntity<Workspace>,
    solution_id: SolutionId,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = weak_workspace.upgrade() else {
        return;
    };
    let workspace_id = workspace.entity_id();
    if !cx
        .default_global::<ReopenQueryInFlight>()
        .0
        .insert(workspace_id)
    {
        return;
    }
    // The query already returns top-level closed rows ordered
    // most-recently-active first, each carrying the token total +
    // last-activity time the rows display.
    let store = SolutionAgentStore::global(cx);
    let closed = store.update(cx, |store, cx| store.list_closed_sessions(solution_id, cx));
    window
        .spawn(cx, async move |cx| {
            let metas = closed.await.log_err().unwrap_or_default();
            let sessions: Vec<ReopenableSession> =
                metas.iter().map(ReopenableSession::from_metadata).collect();
            let opened = workspace.update_in(cx, |workspace, window, cx| {
                cx.default_global::<ReopenQueryInFlight>()
                    .0
                    .remove(&workspace_id);
                if workspace.active_modal::<ReopenSessionModal>(cx).is_some() {
                    return;
                }
                workspace.toggle_modal(window, cx, move |window, cx| {
                    ReopenSessionModal::new(sessions, window, cx)
                });
            });
            if opened.log_err().is_none() {
                // The workspace went away mid-query; drop the guard anyway so a
                // dead entity id can't accumulate in the global.
                cx.update(|_, cx| {
                    cx.default_global::<ReopenQueryInFlight>()
                        .0
                        .remove(&workspace_id);
                })
                .log_err();
            }
        })
        .detach();
}

pub struct ReopenSessionModal {
    sessions: Vec<ReopenableSession>,
    focus_handle: FocusHandle,
}

impl ReopenSessionModal {
    pub fn new(
        sessions: Vec<ReopenableSession>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            sessions,
            focus_handle: cx.focus_handle(),
        }
    }

    fn reopen(&mut self, id: SolutionSessionId, solution_id: SolutionId, cx: &mut Context<Self>) {
        let store = SolutionAgentStore::global(cx);
        store
            .update(cx, |store, cx| {
                store.reopen_closed_session(id, solution_id, cx)
            })
            .detach_and_log_err(cx);
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for ReopenSessionModal {}

impl Focusable for ReopenSessionModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for ReopenSessionModal {
    fn debug_kind(&self) -> &'static str {
        "ReopenSession"
    }
}

impl Render for ReopenSessionModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut container = div()
            .key_context("ReopenSessionModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .flex()
            .flex_col()
            .gap_2()
            .w(rems(30.))
            .p_4()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(Label::new("Reopen Closed Chat").size(LabelSize::Large));

        if self.sessions.is_empty() {
            return container.child(
                Label::new("No closed chats in this solution.")
                    .size(LabelSize::Default)
                    .color(Color::Muted),
            );
        }

        let mut list = v_flex()
            .id("reopen-session-list")
            .gap_px()
            .max_h(rems(20.))
            .overflow_y_scroll();
        let now = chrono::Utc::now();
        for session in self.sessions.clone() {
            let id = session.id;
            let solution_id = session.solution_id;
            // Secondary line: "128.4k ctx · 3h ago" (token half omitted when
            // the session never reported a usage). Lets the user pick a heavy
            // or recently-touched session without opening each one.
            let activity = relative_time_short(session.last_activity_at, now);
            let meta_text: SharedString = match session.total_tokens {
                Some(tokens) => {
                    format!("{} ctx · {activity}", format_tokens_compact(tokens)).into()
                }
                None => activity.into(),
            };
            list = list.child(
                ui::ListItem::new(SharedString::from(id.to_string()))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(Icon::new(IconName::Sparkle).size(IconSize::Small))
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(Label::new(session.title.clone()).truncate())
                                    .child(
                                        Label::new(meta_text)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.reopen(id, solution_id, cx))),
            );
        }
        container = container.child(list);
        container
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Double-clicking the entry point must leave the picker OPEN.
    ///
    /// The rows come from a DB round trip, so the modal is opened from an
    /// async continuation. With a plain `toggle_modal` there, the second
    /// click's continuation toggled the picker the first one had just opened
    /// straight back shut — the button looked like it did nothing.
    #[gpui::test]
    async fn a_double_click_leaves_the_picker_open(cx: &mut gpui::TestAppContext) {
        let (solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            let registry = std::sync::Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });

        let workspace_window =
            cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let weak = cx.update(|cx| {
            workspace_window
                .root(cx)
                .expect("workspace window alive")
                .downgrade()
        });

        // Both clicks land before the first query resolves — the second must
        // be a no-op rather than a toggle.
        workspace_window
            .update(cx, |_workspace, window, cx| {
                open_reopen_session_modal(&weak, solution_id, window, cx);
                open_reopen_session_modal(&weak, solution_id, window, cx);
            })
            .expect("workspace update");
        cx.run_until_parked();

        workspace_window
            .update(cx, |workspace, _window, cx| {
                assert!(
                    workspace.active_modal::<ReopenSessionModal>(cx).is_some(),
                    "the picker must still be up after a double click"
                );
            })
            .expect("workspace update");

        // A third click with the picker already up is also a no-op (it must
        // not close what the user can already see).
        workspace_window
            .update(cx, |_workspace, window, cx| {
                open_reopen_session_modal(&weak, solution_id, window, cx);
            })
            .expect("workspace update");
        cx.run_until_parked();
        workspace_window
            .update(cx, |workspace, _window, cx| {
                assert!(
                    workspace.active_modal::<ReopenSessionModal>(cx).is_some(),
                    "a click while the picker is open must not dismiss it"
                );
            })
            .expect("workspace update");
    }
}
