use editor::Editor;
use gpui::{AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable};
use solutions::{SolutionId, SolutionStore};
use ui::prelude::*;
use workspace::{ModalView, Workspace};

/// Single-field modal for renaming a solution. Used by the title-bar tab
/// strip's right-click menu (Rename…) and the welcome list's pencil icon.
/// The (retired-in-Phase-2) dock panel handled rename inline within its
/// row; this modal replaces that path so rename keeps working once the
/// panel is gone.
pub struct RenameSolutionModal {
    id: SolutionId,
    name_editor: Entity<Editor>,
    focus_handle: FocusHandle,
    error: Option<SharedString>,
}

impl RenameSolutionModal {
    fn new(
        id: SolutionId,
        current_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = cx.new(|cx| Editor::single_line(window, cx));
        name_editor.update(cx, |editor, cx| {
            editor.set_text(current_name, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
        });
        let focus_handle = cx.focus_handle();
        Self {
            id,
            name_editor,
            focus_handle,
            error: None,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let new_name = self.name_editor.read(cx).text(cx).trim().to_string();
        if new_name.is_empty() {
            return;
        }
        let id = self.id;
        let result = SolutionStore::global(cx)
            .update(cx, |store, cx| store.rename_solution(id, &new_name, cx));
        match result {
            Ok(()) => {
                self.error = None;
                cx.emit(DismissEvent);
            }
            // A rename now moves the folder, so it can fail (bad name,
            // collision, cross-device move) — show it and keep the modal open
            // instead of dismissing on a rename that never happened.
            Err(error) => {
                self.error = Some(error.to_string().into());
                cx.notify();
            }
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for RenameSolutionModal {}

impl Focusable for RenameSolutionModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl ModalView for RenameSolutionModal {
    fn debug_kind(&self) -> &'static str {
        "RenameSolution"
    }
}

impl Render for RenameSolutionModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RenameSolutionModal")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .track_focus(&self.focus_handle)
            .w(rems(28.))
            .p_4()
            .gap_3()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(Label::new("Rename Solution").size(LabelSize::Large))
            .child(self.name_editor.clone())
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("rename-cancel", "Cancel").on_click(cx.listener(
                        |this, _, window, cx| {
                            this.cancel(&menu::Cancel, window, cx);
                        },
                    )))
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

/// Convenience entry point used by `RenameSolution` action handlers. Looks
/// up the solution's current name in the store and opens
/// [`RenameSolutionModal`]; no-op if the id is unknown (stale action
/// targeting an already-deleted solution).
pub fn open_rename_solution(
    workspace: &mut Workspace,
    id: SolutionId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let store = SolutionStore::global(cx);
    let Some(current_name) = store.read_with(cx, |store, _| {
        store
            .find_solution(id)
            .ok()
            .map(|solution| solution.name.clone())
    }) else {
        return;
    };
    workspace.toggle_modal(window, cx, move |window, cx| {
        RenameSolutionModal::new(id, &current_name, window, cx)
    });
}
