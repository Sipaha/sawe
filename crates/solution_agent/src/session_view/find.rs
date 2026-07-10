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
use crate::conversation_render::{FindMatch, entry_text_spans, find_all};

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

    /// Scroll the conversation list so the currently-selected find match is
    /// in view. The match's `entry_idx` is a LIVE-thread index, but the
    /// virtualized `list_state` is sized over the cold+live concatenation, so
    /// offset by the cold-entry count — exactly as the render path and
    /// `on_thread_event` do. Without this, Enter / the ↑↓ buttons move the
    /// counter and the active-match highlight but never bring an off-screen
    /// match into view, so iterating "does nothing" visually.
    fn scroll_to_selected_match(&mut self, _cx: &mut Context<Self>) {
        let Some(entry_idx) = self.find.as_ref().and_then(|find| {
            let selected = find.selected?;
            find.matches.get(selected).map(|m| m.entry_idx)
        }) else {
            return;
        };
        // `entry_idx` is now the global index into `session.entries`
        // (which the virtualized list also indexes 1:1), so no cold offset.
        self.list_state.scroll_to_reveal_item(entry_idx);
    }

    pub(super) fn previous_match(
        &mut self,
        _: &FindPreviousMatch,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub(crate) fn recompute_matches(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let query = find.editor.read(cx).text(cx);
        if query.is_empty() {
            find.matches.clear();
            find.selected = None;
            return;
        }
        let query_lower = query.to_lowercase();
        let mut matches = Vec::new();
        let session = self.session.read(cx);
        // Iterate the selected parent-thread stream's entries so `entry_idx` is
        // the per-stream index (matching `markdown_for_render`'s keys and the
        // list dispatch). Read from `session.streams` directly, not the
        // render-frame field, so a `recompute_matches` fired from
        // `on_thread_event` reflects the just-mutated stream. Drill-in views
        // don't support find over the parent thread → no matches.
        let stream_entries: &[crate::session_entry::SessionEntry] = session
            .streams
            .get(&self.selected_stream)
            .map(|s| s.entries.as_slice())
            .unwrap_or_default();
        for (entry_idx, entry) in stream_entries.iter().enumerate() {
            for (span_idx, text) in entry_text_spans(entry).into_iter().enumerate() {
                find_all(&text, &query_lower, |range| {
                    matches.push(FindMatch {
                        entry_idx,
                        span_idx,
                        range,
                    });
                });
            }
        }
        find.selected = if matches.is_empty() { None } else { Some(0) };
        find.matches = matches;
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
