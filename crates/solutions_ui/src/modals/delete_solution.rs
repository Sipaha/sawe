use gpui::{DismissEvent, EventEmitter, FocusHandle, Focusable};
use solutions::SolutionId;
use std::path::PathBuf;
use ui::prelude::*;
use workspace::ModalView;

pub struct DeleteSolutionModal {
    id: SolutionId,
    name: String,
    root: PathBuf,
    focus_handle: FocusHandle,
}

impl DeleteSolutionModal {
    pub(crate) fn new(id: SolutionId, name: String, root: PathBuf, cx: &mut Context<Self>) -> Self {
        Self {
            id,
            name,
            root,
            focus_handle: cx.focus_handle(),
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        // Disk cleanup is best-effort and async — the directory can be
        // huge (worktrees with full git histories), so we don't want to
        // block the UI thread. Failures are logged but not surfaced: by
        // this point the metadata entry is gone, so the user has
        // effectively forgotten the solution either way.
        crate::delete_solution_with_cleanup(self.id.clone(), self.root.clone(), cx);
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for DeleteSolutionModal {}

impl Focusable for DeleteSolutionModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for DeleteSolutionModal {
    fn debug_kind(&self) -> &'static str {
        "DeleteSolution"
    }
}

impl Render for DeleteSolutionModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let path_str = self.root.display().to_string();
        v_flex()
            .key_context("DeleteSolutionModal")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .track_focus(&self.focus_handle)
            .w(rems(32.))
            .p_4()
            .gap_3()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(IconName::Warning)
                            .color(Color::Warning)
                            .size(IconSize::Medium),
                    )
                    .child(Label::new("Delete Solution").size(LabelSize::Large)),
            )
            .child(
                Label::new(format!(
                    "\"{}\" will be removed from the launcher.",
                    self.name
                ))
                .color(Color::Default),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new(
                            "All files under this directory will be permanently deleted from disk:",
                        )
                        .color(Color::Muted),
                    )
                    .child(Label::new(path_str).color(Color::Muted)),
            )
            .child(Label::new("This action cannot be undone.").color(Color::Warning))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("cancel", "Cancel").on_click(cx.listener(
                        |this, _, window, cx| {
                            this.cancel(&menu::Cancel, window, cx);
                        },
                    )))
                    .child(
                        Button::new("delete", "Delete")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}
