//! Modal for setting a per-chat supervisor instruction. Triggered from the
//! supervisor popover menu in the status row.

use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Styled, Window, div, rems,
};
use ui::prelude::*;
use ui::{Button, ButtonStyle, Label, LabelSize};
use workspace::ModalView;

use crate::model::SolutionSessionId;
use crate::store::SolutionAgentStore;

/// Single-line popup for setting the supervisor instruction for a session.
/// Mirrors `RenameSessionModal` — Editor-based single-line input, modal
/// dismiss on Confirm/Cancel, prefilled with the current prompt if one exists.
pub struct SupervisorInstructionModal {
    session_id: SolutionSessionId,
    instruction_editor: Entity<editor::Editor>,
    focus_handle: FocusHandle,
}

impl SupervisorInstructionModal {
    pub fn new(
        session_id: SolutionSessionId,
        current_instruction: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let instruction_editor = cx.new(|cx| {
            let mut e = editor::Editor::single_line(window, cx);
            if let Some(text) = current_instruction {
                e.set_text(text, window, cx);
                e.select_all(&editor::actions::SelectAll, window, cx);
            } else {
                e.set_placeholder_text(
                    "Supervisor instruction for this chat…",
                    window,
                    cx,
                );
            }
            e
        });
        let focus_handle = cx.focus_handle();
        Self {
            session_id,
            instruction_editor,
            focus_handle,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.instruction_editor.read(cx).text(cx);
        let trimmed = text.trim();
        let prompt = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_supervisor_prompt(self.session_id, prompt, cx);
        });
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for SupervisorInstructionModal {}

impl Focusable for SupervisorInstructionModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.instruction_editor.focus_handle(cx)
    }
}

impl ModalView for SupervisorInstructionModal {
    fn debug_kind(&self) -> &'static str {
        "SupervisorInstruction"
    }
}

impl Render for SupervisorInstructionModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("SupervisorInstructionModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .flex()
            .flex_col()
            .gap_3()
            .w(rems(40.))
            .p_4()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(Label::new("Supervisor Instruction").size(LabelSize::Large))
            .child(self.instruction_editor.clone())
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("supervisor-instruction-cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("supervisor-instruction-save", "Save")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}
