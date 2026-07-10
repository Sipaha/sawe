use editor::Editor;
use gpui::{AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable};
use solutions::{SolutionId, SolutionStore};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

/// Single-field modal that creates a fresh empty member inside the
/// named solution. Calls `SolutionStore::add_empty_member` on confirm.
pub struct NewProjectInSolutionModal {
    solution_id: SolutionId,
    name_editor: Entity<Editor>,
    focus_handle: FocusHandle,
}

impl NewProjectInSolutionModal {
    fn new(solution_id: SolutionId, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name_editor = cx.new(|cx| Editor::single_line(window, cx));
        name_editor.update(cx, |editor, cx| {
            editor.set_placeholder_text("Project name", window, cx);
        });
        let focus_handle = cx.focus_handle();
        Self {
            solution_id,
            name_editor,
            focus_handle,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_editor.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            return;
        }
        let store = SolutionStore::global(cx);
        let id = self.solution_id.clone();
        store
            .update(cx, |s, cx| s.add_empty_member(&id, &name, cx))
            .log_err();
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for NewProjectInSolutionModal {}

impl Focusable for NewProjectInSolutionModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl ModalView for NewProjectInSolutionModal {
    fn debug_kind(&self) -> &'static str {
        "NewProjectInSolution"
    }
}

impl Render for NewProjectInSolutionModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("NewProjectInSolutionModal")
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
            .child(Label::new("New Project in Solution").size(LabelSize::Large))
            .child(self.name_editor.clone())
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("npis-cancel", "Cancel").on_click(cx.listener(
                        |this, _, window, cx| {
                            this.cancel(&menu::Cancel, window, cx);
                        },
                    )))
                    .child(
                        Button::new("npis-create", "Create")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}

/// Convenience entry point for the `CreateNewProjectInSolution` action.
/// Opens [`NewProjectInSolutionModal`] for the given solution so the user
/// can name a fresh empty project. Validation (empty name, slug
/// uniqueness) lives in `SolutionStore::add_empty_member`; this helper
/// just shows the modal.
pub fn open_new_project_in_solution(
    workspace: &mut Workspace,
    solution_id: SolutionId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.toggle_modal(window, cx, move |window, cx| {
        NewProjectInSolutionModal::new(solution_id, window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn new_project_in_solution_modal_constructs(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let _modal = cx.add_window(|window, cx| {
            NewProjectInSolutionModal::new(SolutionId("sol-1".into()), window, cx)
        });
    }
}
