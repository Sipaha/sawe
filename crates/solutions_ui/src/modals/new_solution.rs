use editor::Editor;
use gpui::{
    AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, WeakEntity,
};
use settings::Settings as _;
use solutions::{SolutionStore, SolutionsSettings};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::open::{OpenIntent, open_solution};

pub struct NewSolutionModal {
    name_editor: Entity<Editor>,
    _workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
}

impl NewSolutionModal {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = cx.new(|cx| Editor::single_line(window, cx));
        name_editor.update(cx, |editor, cx| {
            editor.set_placeholder_text("Solution name", window, cx);
        });
        let focus_handle = cx.focus_handle();
        Self {
            name_editor,
            _workspace: workspace,
            focus_handle,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_editor.read(cx).text(cx);
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let root = SolutionsSettings::get_global(cx).root.clone();
        let store = SolutionStore::global(cx);
        let created = store
            .update(cx, |s, cx| s.create_solution(name, root, cx))
            .log_err();
        cx.emit(DismissEvent);
        // Open the freshly-created solution right away — creating one and being
        // left on the previous solution is surprising.
        if let Some(id) = created {
            let source = window.window_handle().downcast();
            open_solution(id, source, OpenIntent::SameWindow, cx);
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for NewSolutionModal {}

impl Focusable for NewSolutionModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl ModalView for NewSolutionModal {
    fn debug_kind(&self) -> &'static str {
        "NewSolution"
    }
}

impl Render for NewSolutionModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("NewSolutionModal")
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
            .child(Label::new("New Solution").size(LabelSize::Large))
            .child(self.name_editor.clone())
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
                        Button::new("create", "Create")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}
