//! Modal showing a tool call's full argument — the whole shell command, path or
//! pattern behind the clipped preview row in the conversation.

use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Window, div, rems,
};
use ui::prelude::*;
use ui::{Button, ButtonStyle, CopyButton, Label, LabelSize};
use workspace::ModalView;

/// The tool header's preview row collapses newlines to `↵` and the label clips
/// to the container width, so a heredoc or a long pipeline is unreadable there
/// — and the raw command appears nowhere else in the UI. This modal paints it
/// verbatim in a read-only editor: soft-wrapped, scrollable, selectable, with a
/// copy button. Read-only rather than a plain label so the text can be selected
/// piecewise (copying the whole thing is one click, but "just the file path"
/// is not).
pub struct ToolArgumentModal {
    title: SharedString,
    argument: SharedString,
    argument_editor: Entity<editor::Editor>,
    focus_handle: FocusHandle,
}

impl ToolArgumentModal {
    pub fn new(
        title: SharedString,
        argument: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let argument = SharedString::from(argument);
        let argument_editor = cx.new(|cx| {
            let mut e = editor::Editor::multi_line(window, cx);
            e.set_show_gutter(false, cx);
            e.set_show_line_numbers(false, cx);
            e.set_show_vertical_scrollbar(true, cx);
            e.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            e.set_text(argument.clone(), window, cx);
            // After `set_text` the cursor sits at the end, which scrolls a long
            // command to its last line — the user asked to see the command, so
            // start at the top.
            e.move_to_beginning(&editor::actions::MoveToBeginning, window, cx);
            e.set_read_only(true);
            e
        });
        let focus_handle = cx.focus_handle();
        Self {
            title,
            argument,
            argument_editor,
            focus_handle,
        }
    }

    fn dismiss(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for ToolArgumentModal {}

impl Focusable for ToolArgumentModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.argument_editor.focus_handle(cx)
    }
}

impl ModalView for ToolArgumentModal {
    fn debug_kind(&self) -> &'static str {
        "ToolArgument"
    }
}

impl Render for ToolArgumentModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("ToolArgumentModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::confirm))
            .flex()
            .flex_col()
            .gap_2()
            .w(rems(58.))
            .p_4()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        Label::new(self.title.clone())
                            .size(LabelSize::Large)
                            .truncate(),
                    )
                    .child(
                        CopyButton::new("tool-argument-copy", self.argument.to_string())
                            .tooltip_label("Copy the full argument"),
                    ),
            )
            .child(
                div()
                    .id("tool-argument-editor-frame")
                    .h(rems(26.))
                    .min_h_0()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .p_2()
                    .overflow_hidden()
                    .child(self.argument_editor.clone()),
            )
            .child(
                div().flex().justify_end().child(
                    Button::new("tool-argument-close", "Close")
                        .style(ButtonStyle::Filled)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.dismiss(&menu::Cancel, window, cx)
                        })),
                ),
            )
    }
}
