//! Modal that renames an open Solution session tab. Triggered from the tab right-click menu in the chat panel.

use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Window, div,
    rems,
};
use ui::prelude::*;
use ui::{Button, ButtonStyle, Label, LabelSize};
use util::ResultExt as _;
use workspace::ModalView;

use crate::model::SolutionSessionId;
use crate::store::SolutionAgentStore;

/// Single-line popup for renaming a session tab. Replaces the previous
/// in-tab inline editor — the popup makes the rename action discoverable
/// from the right-click menu and keeps the tab strip visually compact.
pub struct RenameSessionModal {
    session_id: SolutionSessionId,
    name_editor: Entity<editor::Editor>,
    focus_handle: FocusHandle,
}

impl RenameSessionModal {
    pub fn new(
        session_id: SolutionSessionId,
        current_title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = cx.new(|cx| {
            let mut e = editor::Editor::single_line(window, cx);
            e.set_text(current_title, window, cx);
            // Pre-select so a fresh keystroke replaces the title (Chrome
            // rename behaviour). Without this the cursor parks at the
            // end and a typed character appends.
            e.select_all(&editor::actions::SelectAll, window, cx);
            e
        });
        let focus_handle = cx.focus_handle();
        Self {
            session_id,
            name_editor,
            focus_handle,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let new_title = self.name_editor.read(cx).text(cx);
        let new_title = new_title.trim();
        if !new_title.is_empty() {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store
                    .rename_session(self.session_id, SharedString::from(new_title), cx)
                    .log_err();
            });
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for RenameSessionModal {}

impl Focusable for RenameSessionModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl ModalView for RenameSessionModal {
    fn debug_kind(&self) -> &'static str {
        "RenameSession"
    }
}

impl Render for RenameSessionModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("RenameSessionModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .flex()
            .flex_col()
            .gap_3()
            .w(rems(28.))
            .p_4()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(Label::new("Rename Session").size(LabelSize::Large))
            .child(self.name_editor.clone())
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("rename-cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("rename-save", "Save")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}
