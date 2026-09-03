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

/// Open the reopen-a-closed-chat picker over `weak_workspace`.
///
/// Lives here, next to the modal, rather than on whatever surface offers the
/// entry point: it used to be a `ConsolePanel` method, but the console
/// panel's `+` no longer offers AI-session entries at all (AI sessions live
/// on the status-bar session tab strip), and `solution_agent` cannot depend
/// on `console_panel` to call back into it — that edge already runs the other
/// way. Closed sessions live only on disk, so the list is queried
/// asynchronously and the modal is toggled once it resolves.
pub fn open_reopen_session_modal(
    weak_workspace: &WeakEntity<Workspace>,
    solution_id: SolutionId,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = weak_workspace.upgrade() else {
        return;
    };
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
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.toggle_modal(window, cx, move |window, cx| {
                        ReopenSessionModal::new(sessions, window, cx)
                    });
                })
                .log_err();
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
