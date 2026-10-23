//! Find-in-session: the Ctrl+F search bar over the conversation transcript.
//! Splits the search cluster (`open_find`/`close_find`, match iteration,
//! `recompute_matches`, `render_find_bar`) out of the view root. `self` and
//! all fields stay owned by `SolutionSessionView`; these are `impl` methods
//! relocated verbatim.

use gpui::{AnyElement, Context, Focusable, IntoElement, Window, div};
use ui::prelude::*;
use ui::{IconButton, IconName, Label, Tooltip};

use super::{FindState, SolutionSessionView};
use crate::actions::{FindClose, FindInSession, FindNextMatch, FindPreviousMatch};
use crate::conversation_render::{FindMatch, find_all};

impl SolutionSessionView {
    pub(super) fn open_find(
        &mut self,
        _: &FindInSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(find) = self.find.as_ref() {
            // Already open — re-focus the input so a second Ctrl+F lands the
            // user back in the find bar after they've moved focus elsewhere
            // (e.g. clicked a tool-call body, then hit Ctrl+F again).
            let handle = find.editor.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
            return;
        }
        let editor = cx.new(|cx| {
            let mut e = editor::Editor::single_line(window, cx);
            e.set_placeholder_text("Find in session…", window, cx);
            e
        });
        let subscription = cx.subscribe(&editor, |this: &mut Self, _, event, cx| {
            if let editor::EditorEvent::BufferEdited = event {
                this.recompute_matches(cx);
                // As-you-type: follow the first hit into view. Only on a
                // query edit (this subscription) — NOT on the streaming
                // `recompute_matches` calls in `on_thread_event`, which would
                // yank the viewport to match #0 on every token mid-turn.
                this.scroll_to_selected_match(cx);
                cx.notify();
            }
        });
        let handle = editor.read(cx).focus_handle(cx);
        self.find = Some(FindState {
            editor,
            matches: Vec::new(),
            selected: None,
            _subscription: subscription,
        });
        self.recompute_matches(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    pub(super) fn close_find(
        &mut self,
        _: &FindClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.find.take().is_some() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    pub(super) fn next_match(
        &mut self,
        _: &FindNextMatch,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A streaming delta may have invalidated the match list since the last
        // render; navigate over the CURRENT matches, not the pre-delta ones.
        self.ensure_find_matches(cx);
        {
            let Some(find) = self.find.as_mut() else {
                return;
            };
            if find.matches.is_empty() {
                return;
            }
            let next = match find.selected {
                Some(i) => (i + 1) % find.matches.len(),
                None => 0,
            };
            find.selected = Some(next);
        }
        self.scroll_to_selected_match(cx);
        cx.notify();
    }

    /// Reveal the selected match's raw per-stream entry in the display rows.
    /// Search expands tool groups, but raw transcript indices remain separate
    /// from virtual row indices; explicit navigation also disarms tail following.
    fn scroll_to_selected_match(&mut self, cx: &mut Context<Self>) {
        let Some(entry_idx) = self.find.as_ref().and_then(|find| {
            let selected = find.selected?;
            find.matches.get(selected).map(|m| m.entry_idx)
        }) else {
            return;
        };
        self.reconcile_conversation_rows(cx);
        let row = self
            .conversation_rows
            .partition_point(|row| row.end <= entry_idx);
        // Explicit search navigation must disarm tail following, otherwise the
        // next layout immediately snaps the selected match back out of view.
        self.list_state.scroll_to(gpui::ListOffset {
            item_ix: row,
            offset_in_item: gpui::px(0.),
        });
    }

    pub(super) fn previous_match(
        &mut self,
        _: &FindPreviousMatch,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_find_matches(cx);
        {
            let Some(find) = self.find.as_mut() else {
                return;
            };
            if find.matches.is_empty() {
                return;
            }
            let len = find.matches.len();
            let prev = match find.selected {
                Some(0) => len - 1,
                Some(i) => i - 1,
                None => 0,
            };
            find.selected = Some(prev);
        }
        self.scroll_to_selected_match(cx);
        cx.notify();
    }

    /// Mark the find matches as needing a recompute. Cheap and idempotent —
    /// see [`SolutionSessionView::ensure_find_matches`] for why the recompute
    /// itself is deferred.
    pub(crate) fn invalidate_find_matches(&mut self) {
        if self.find.is_some() {
            self.find_dirty = true;
        }
    }

    /// Recompute the find matches iff something invalidated them since the
    /// last recompute. Every reader of `find.matches` (render, the find bar's
    /// counter, next/previous navigation) goes through here first, so
    /// deferring the work cannot serve a stale match list.
    pub(crate) fn ensure_find_matches(&mut self, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.find_dirty) {
            return;
        }
        if self.find.is_some() {
            self.recompute_matches(cx);
        }
    }

    pub(crate) fn recompute_matches(&mut self, cx: &mut Context<Self>) {
        self.find_dirty = false;
        let Some(find) = self.find.as_ref() else {
            return;
        };
        let query = find.editor.read(cx).text(cx);
        if query.is_empty() {
            if let Some(find) = self.find.as_mut() {
                find.matches.clear();
                find.selected = None;
            }
            return;
        }
        let query_lower = query.to_lowercase();
        // Search the SELECTED stream's entries so `entry_idx` is the
        // per-stream index (matching `markdown_for_render`'s keys and the list
        // dispatch). Sourced from `session.streams` (via
        // `selected_parent_stream_entries`), not the render-frame field, so a
        // recompute fired from an event reflects the just-mutated stream.
        // Drill-in views don't support find over the parent thread → no
        // matches. The span texts come from the shared per-entry cache the
        // render path fills, so an unchanged transcript costs no string
        // rebuilding here either.
        self.refresh_entry_texts_from_session(cx);
        let mut matches = Vec::new();
        for entry_idx in 0..self.entry_texts.len() {
            for (span_idx, text) in self.entry_spans(entry_idx).iter().enumerate() {
                find_all(text, &query_lower, |range| {
                    matches.push(FindMatch {
                        entry_idx,
                        span_idx,
                        range,
                    });
                });
            }
        }
        if let Some(find) = self.find.as_mut() {
            find.selected = if matches.is_empty() { None } else { Some(0) };
            find.matches = matches;
        }
    }

    pub(super) fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let find = self.find.as_ref()?;
        let total = find.matches.len();
        let pos_text = if total == 0 {
            "no results".to_string()
        } else {
            let i = find.selected.unwrap_or(0) + 1;
            format!("{i} of {total}")
        };
        Some(
            div()
                .key_context("SolutionSessionFindEditor")
                .track_focus(&find.editor.read(cx).focus_handle(cx))
                .flex()
                .h_8()
                .px_2()
                .gap_2()
                .items_center()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().colors().elevated_surface_background)
                .child(div().flex_1().child(find.editor.clone()))
                .child(
                    Label::new(pos_text)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    IconButton::new("solution-find-prev", IconName::ChevronUp)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Previous match"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.previous_match(&FindPreviousMatch, window, cx);
                        })),
                )
                .child(
                    IconButton::new("solution-find-next", IconName::ChevronDown)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Next match"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.next_match(&FindNextMatch, window, cx);
                        })),
                )
                .child(
                    IconButton::new("solution-find-close", IconName::Close)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Close (Esc)"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.close_find(&FindClose, window, cx);
                        })),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn explicit_find_navigation_disarms_tail_following(cx: &mut gpui::TestAppContext) {
        let (id, _thread, _tmp) = crate::store::tests::create_session_with_thread(cx).await;
        let session = cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::store::SolutionAgentStore::global(cx)
                .read(cx)
                .session(id)
                .unwrap()
        });
        let project = cx.update(|cx| session.read(cx).project.clone().unwrap());
        let workspace =
            cx.add_window(|window, cx| workspace::Workspace::test_new(project, window, cx));
        let weak = cx.update(|cx| workspace.root(cx).unwrap().downgrade());
        session.update(cx, |session, cx| {
            session.set_entries(
                [
                    "needle in earlier text".to_owned(),
                    "Later text\n".repeat(100),
                ]
                .into_iter()
                .map(|text| {
                    std::sync::Arc::new(crate::session_entry::SessionEntry {
                        created_ms: 0,
                        mod_seq: 0,
                        subagent_id: None,
                        kind: crate::session_entry::SessionEntryKind::AssistantMessage {
                            chunks: vec![crate::session_entry::AssistantChunk::Message(text)],
                        },
                    })
                })
                .collect::<Vec<_>>(),
                cx,
            );
        });
        let view = cx
            .add_window(|window, cx| SolutionSessionView::for_test(id, session, weak, window, cx));
        let vcx = &mut gpui::VisualTestContext::from_window(view.into(), cx);
        vcx.run_until_parked();
        view.update(vcx, |view, window, cx| {
            view.list_state.set_follow_mode(gpui::FollowMode::Tail);
            view.list_state.scroll_to_end();
            view.open_find(&FindInSession, window, cx);
            view.find
                .as_ref()
                .unwrap()
                .editor
                .update(cx, |editor, cx| editor.set_text("needle", window, cx));
        })
        .unwrap();
        vcx.run_until_parked();
        view.update(vcx, |view, _, _| {
            assert!(
                !view.list_state.is_following_tail(),
                "search must not snap back to the tail"
            );
            assert_eq!(view.list_state.logical_scroll_top().item_ix, 0);
            assert_eq!(view.find.as_ref().unwrap().matches.len(), 1);
        })
        .unwrap();
    }
}
