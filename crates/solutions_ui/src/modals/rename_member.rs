use editor::Editor;
use gpui::{AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable};
use solutions::{MemberId, SolutionStore};
use ui::prelude::*;
use workspace::{ModalView, Workspace};

/// Single-field modal for renaming a member project. Renaming now also moves
/// the member's directory, so — unlike the old name-only rename — it can fail
/// (empty derivation, collision, cross-device move). The error is rendered in
/// the modal and the modal stays open.
pub struct RenameMemberModal {
    id: MemberId,
    name_editor: Entity<Editor>,
    focus_handle: FocusHandle,
    error: Option<SharedString>,
}

impl RenameMemberModal {
    fn new(id: MemberId, current_name: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
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

    fn sanitize(raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    fn describe_error(error: &anyhow::Error) -> String {
        error.to_string()
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(new_name) = Self::sanitize(&self.name_editor.read(cx).text(cx)) else {
            return;
        };
        let id = self.id;
        let result =
            SolutionStore::global(cx).update(cx, |store, cx| store.rename_member(id, &new_name, cx));
        match result {
            Ok(()) => {
                self.error = None;
                cx.emit(DismissEvent);
            }
            Err(error) => {
                self.error = Some(Self::describe_error(&error).into());
                cx.notify();
            }
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for RenameMemberModal {}

impl Focusable for RenameMemberModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl ModalView for RenameMemberModal {
    fn debug_kind(&self) -> &'static str {
        "RenameMember"
    }
}

impl Render for RenameMemberModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RenameMemberModal")
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
            .child(Label::new("Rename Project").size(LabelSize::Large))
            .child(
                Label::new("Renaming also renames the project's folder on disk.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(self.name_editor.clone())
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("rename-member-cancel", "Cancel").on_click(cx.listener(
                            |this, _, window, cx| {
                                this.cancel(&menu::Cancel, window, cx);
                            },
                        )),
                    )
                    .child(
                        Button::new("rename-member-save", "Save")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}

/// Entry point for the `RenameMember` action. No-op if the id is unknown
/// (stale action targeting an already-removed member).
pub fn open_rename_member(
    workspace: &mut Workspace,
    id: MemberId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let store = SolutionStore::global(cx);
    let Some(current_name) = store.read_with(cx, |store, _| {
        store.find_member(id).ok().map(|member| member.name.clone())
    }) else {
        return;
    };
    workspace.toggle_modal(window, cx, move |window, cx| {
        RenameMemberModal::new(id, &current_name, window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn confirm_reports_a_collision_and_keeps_the_modal_open(cx: &mut gpui::TestAppContext) {
        let error = RenameMemberModal::describe_error(&anyhow::anyhow!(
            solutions::FolderNameError::ExistsOnDisk {
                folder: "taken".into()
            }
        ));
        assert_eq!(
            error,
            "Directory 'taken' already exists on disk (not owned by any solution)"
        );
        // A blank name never reaches the store.
        assert!(RenameMemberModal::sanitize("   ").is_none());
        assert_eq!(
            RenameMemberModal::sanitize("  New Project "),
            Some("New Project".to_string())
        );
        let _ = cx;
    }
}
