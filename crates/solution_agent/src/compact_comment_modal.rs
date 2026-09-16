//! Modal that collects an optional comment to send along with a "Compact
//! context" request. Triggered from the cleanup popover menu in the status row.

use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Styled, WeakEntity, Window, div, rems,
};
use ui::prelude::*;
use ui::{Button, ButtonStyle, Label, LabelSize};
use workspace::ModalView;

use crate::session_view::SolutionSessionView;

/// Modal shown before a compaction starts. The comment is optional — confirming
/// with an empty editor compacts exactly as the menu item used to, so the extra
/// step never blocks a plain "just compact".
///
/// A full multi-line editor (the same shape as
/// [`crate::supervisor_instruction_modal`]): `Enter` inserts a newline, confirm
/// via the button or `cmd-enter` / `ctrl-enter` (`menu::Confirm`), cancel via
/// the button or `escape` (`menu::Cancel`).
pub struct CompactCommentModal {
    view: WeakEntity<SolutionSessionView>,
    /// Cold sessions go through the wake path; the menu knows which one applies
    /// at click time, so the modal carries the answer rather than re-deriving it.
    is_cold: bool,
    comment_editor: Entity<editor::Editor>,
    focus_handle: FocusHandle,
}

impl CompactCommentModal {
    pub fn new(
        view: WeakEntity<SolutionSessionView>,
        is_cold: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let comment_editor = cx.new(|cx| {
            let mut e = editor::Editor::multi_line(window, cx);
            e.set_show_gutter(false, cx);
            e.set_show_line_numbers(false, cx);
            e.set_show_vertical_scrollbar(true, cx);
            e.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            e.set_placeholder_text(
                "Optional — what this handoff must not lose, what to do next…",
                window,
                cx,
            );
            e
        });
        let focus_handle = cx.focus_handle();
        Self {
            view,
            is_cold,
            comment_editor,
            focus_handle,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.comment_editor.read(cx).text(cx);
        let trimmed = text.trim();
        let note = (!trimmed.is_empty()).then(|| trimmed.to_string());
        if let Some(view) = self.view.upgrade() {
            view.update(cx, |view, cx| {
                if self.is_cold {
                    view.start_compact_from_cold(note, window, cx);
                } else {
                    view.start_compact(note, cx);
                }
            });
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for CompactCommentModal {}

impl Focusable for CompactCommentModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.comment_editor.focus_handle(cx)
    }
}

impl ModalView for CompactCommentModal {
    fn debug_kind(&self) -> &'static str {
        "CompactComment"
    }
}

impl Render for CompactCommentModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("CompactCommentModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .flex()
            .flex_col()
            .gap_2()
            .w(rems(46.))
            .p_4()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(Label::new("Compact context").size(LabelSize::Large))
            .child(
                Label::new(
                    "The agent dumps a handoff, then a fresh context continues. \
                     A comment here goes into that request — leave it empty to just compact.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                div()
                    .id("compact-comment-editor-frame")
                    .h(rems(9.))
                    .min_h_0()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .p_2()
                    .overflow_hidden()
                    .child(self.comment_editor.clone()),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("compact-comment-cancel", "Cancel").on_click(cx.listener(
                            |this, _, window, cx| this.cancel(&menu::Cancel, window, cx),
                        )),
                    )
                    .child(
                        Button::new("compact-comment-confirm", "Compact")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}
