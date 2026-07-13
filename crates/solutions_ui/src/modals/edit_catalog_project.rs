use editor::Editor;
use gpui::{
    AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, WeakEntity,
};
use solutions::{CatalogId, SolutionStore};
use ui::prelude::*;
use workspace::{ModalView, Workspace};

/// Initial values handed to [`EditCatalogProjectModal::new`]. Snapshotted
/// at action-dispatch time so the modal does not need a borrow into the
/// store after the action handler returns.
pub(crate) struct EditCatalogPrefill {
    pub(crate) name: String,
    pub(crate) remote_url: String,
    pub(crate) default_branch: String,
}

pub struct EditCatalogProjectModal {
    catalog_id: CatalogId,
    name_editor: Entity<Editor>,
    url_editor: Entity<Editor>,
    branch_editor: Entity<Editor>,
    /// Rejection from the last Confirm (duplicate name / duplicate remote),
    /// shown inline so a refused edit can't look like a successful one.
    error: Option<SharedString>,
    _workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
}

impl EditCatalogProjectModal {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        catalog_id: CatalogId,
        prefill: EditCatalogPrefill,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = cx.new(|cx| Editor::single_line(window, cx));
        name_editor.update(cx, |editor, cx| {
            editor.set_text(prefill.name.clone(), window, cx);
        });
        let url_editor = cx.new(|cx| Editor::single_line(window, cx));
        url_editor.update(cx, |editor, cx| {
            editor.set_text(prefill.remote_url.clone(), window, cx);
        });
        let branch_editor = cx.new(|cx| Editor::single_line(window, cx));
        branch_editor.update(cx, |editor, cx| {
            editor.set_text(prefill.default_branch.clone(), window, cx);
            editor.set_placeholder_text("Default branch (optional)", window, cx);
        });
        let focus_handle = cx.focus_handle();
        Self {
            catalog_id,
            name_editor,
            url_editor,
            branch_editor,
            error: None,
            _workspace: workspace,
            focus_handle,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_editor.read(cx).text(cx).trim().to_string();
        let url = self.url_editor.read(cx).text(cx).trim().to_string();
        let branch = self.branch_editor.read(cx).text(cx).trim().to_string();
        if name.is_empty() || url.is_empty() {
            return;
        }
        let store = SolutionStore::global(cx);
        let id = self.catalog_id.clone();
        let new_branch = if branch.is_empty() {
            None
        } else {
            Some(branch)
        };
        if let Err(error) = store.update(cx, |s, cx| {
            s.edit_catalog_project(&id, Some(name), new_branch, Some(url), cx)
        }) {
            // Name / remote uniqueness is enforced in the store. Keep the modal
            // open with the reason instead of dismissing as if it had worked.
            self.error = Some(super::humanize_catalog_error(&error).into());
            cx.notify();
            return;
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for EditCatalogProjectModal {}

impl Focusable for EditCatalogProjectModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl ModalView for EditCatalogProjectModal {
    fn debug_kind(&self) -> &'static str {
        "EditCatalogProject"
    }

    fn dismiss_on_overlay_click(&self) -> bool {
        false
    }
}

impl Render for EditCatalogProjectModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("EditCatalogProjectModal")
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
            .child(Label::new("Edit Catalog Project").size(LabelSize::Large))
            .child(
                Label::new("Name")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(self.name_editor.clone())
            .child(
                Label::new("Remote URL")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(self.url_editor.clone())
            .child(
                Label::new("Default branch")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(self.branch_editor.clone())
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
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
                        Button::new("save", "Save")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}
