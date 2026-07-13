use editor::{Editor, EditorEvent};
use gpui::{
    AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    WeakEntity,
};
use solutions::{SolutionId, SolutionStore};
use ui::prelude::*;
use workspace::{ModalView, Workspace};

pub struct AddCatalogProjectModal {
    name_editor: Entity<Editor>,
    url_editor: Entity<Editor>,
    branch_editor: Entity<Editor>,
    /// The Solution whose `+` opened this modal, if any — the new project is
    /// added to it as a member (cloning in the background, shown as a pending
    /// row with a spinner) once it's in the catalog. `None` = catalog-only.
    solution_id: Option<SolutionId>,
    /// Rejection from the last Confirm (duplicate name / duplicate remote).
    /// Rendered inline above the buttons and the modal STAYS open — the store
    /// enforces both as hard errors, and silently dismissing on a rejected add
    /// would look exactly like a successful one.
    error: Option<SharedString>,
    _workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    _url_subscription: Subscription,
}

impl AddCatalogProjectModal {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        solution_id: Option<SolutionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = cx.new(|cx| Editor::single_line(window, cx));
        name_editor.update(cx, |editor, cx| {
            editor.set_placeholder_text("Project name (e.g. ECOS Records)", window, cx);
        });
        let url_editor = cx.new(|cx| Editor::single_line(window, cx));
        url_editor.update(cx, |editor, cx| {
            editor.set_placeholder_text(
                "Remote URL (e.g. git@example.com:org/repo.git)",
                window,
                cx,
            );
        });
        let branch_editor = cx.new(|cx| Editor::single_line(window, cx));
        branch_editor.update(cx, |editor, cx| {
            editor.set_placeholder_text("Default branch (optional)", window, cx);
        });
        let focus_handle = cx.focus_handle();

        // Auto-fill the Name field from the Remote URL while the user types,
        // unless they have already put something in Name themselves. We treat
        // a manually-cleared name as "empty" too — typing in URL again will
        // refill, which is the simpler / less surprising rule than tracking a
        // sticky "user-modified" bit.
        let url_subscription = cx.subscribe_in(
            &url_editor,
            window,
            |this, url_editor, event, window, cx| {
                if !matches!(
                    event,
                    EditorEvent::Edited { .. } | EditorEvent::BufferEdited
                ) {
                    return;
                }
                let current_name = this.name_editor.read(cx).text(cx);
                if !current_name.trim().is_empty() {
                    return;
                }
                let url = url_editor.read(cx).text(cx);
                let derived = derive_project_name_from_url(&url);
                if derived.is_empty() {
                    return;
                }
                this.name_editor.update(cx, |editor, cx| {
                    editor.set_text(derived, window, cx);
                });
            },
        );

        Self {
            name_editor,
            url_editor,
            branch_editor,
            solution_id,
            error: None,
            _workspace: workspace,
            focus_handle,
            _url_subscription: url_subscription,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_editor.read(cx).text(cx).trim().to_string();
        let url = self.url_editor.read(cx).text(cx).trim().to_string();
        let branch = self.branch_editor.read(cx).text(cx).trim().to_string();
        if name.is_empty() || url.is_empty() {
            return;
        }
        let default_branch = if branch.is_empty() {
            None
        } else {
            Some(branch)
        };
        let solution_id = self.solution_id;
        let store = SolutionStore::global(cx);
        let catalog_id = match store.update(cx, |s, cx| {
            s.add_catalog_project(&name, &url, default_branch, cx)
        }) {
            Ok(id) => Some(id),
            Err(error) => {
                self.error = Some(humanize_catalog_error(&error).into());
                cx.notify();
                return;
            }
        };
        // If opened from a Solution's `+`, immediately add the new project as a
        // member of that Solution. The clone runs in the background; the project
        // strip shows it as a pending row with a spinner until it completes.
        if let (Some(solution_id), Some(catalog_id)) = (solution_id, catalog_id) {
            let cache_root = solutions::default_cache_root();
            let task = store.update(cx, |s, cx| {
                s.add_member(solution_id, catalog_id, cache_root, cx)
            });
            cx.spawn(async move |_, _| task.await).detach_and_log_err(cx);
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

/// Extracts a project name from a remote URL. Handles the three forms users
/// usually paste:
///
/// - `git@host:org/repo.git` → `repo`
/// - `https://host/org/repo.git` → `repo`
/// - `https://host/org/repo` → `repo`
fn derive_project_name_from_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or("");
    last.trim_end_matches(".git").to_string()
}

/// Turn the store's machine-tagged rejection into a sentence for the modal.
/// The `duplicate_name:` / `duplicate_remote:` prefixes exist so MCP callers can
/// branch on them; a human just wants to be told what to change.
pub(super) fn humanize_catalog_error(error: &anyhow::Error) -> String {
    let text = error.to_string();
    match text.split_once(": ") {
        Some((tag, rest)) if tag == "duplicate_name" || tag == "duplicate_remote" => {
            rest.to_string()
        }
        _ => text,
    }
}

impl EventEmitter<DismissEvent> for AddCatalogProjectModal {}

impl Focusable for AddCatalogProjectModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // URL first: it's what the user pastes, and the Name field auto-derives
        // from it.
        self.url_editor.focus_handle(cx)
    }
}

impl ModalView for AddCatalogProjectModal {
    fn debug_kind(&self) -> &'static str {
        "AddCatalogProject"
    }

    /// Don't fall over for a stray click on the overlay — the user is
    /// in the middle of typing project metadata. Dismiss only via the
    /// explicit "Cancel" button or the Escape action.
    fn dismiss_on_overlay_click(&self) -> bool {
        false
    }
}

impl Render for AddCatalogProjectModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("AddCatalogProjectModal")
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
            .child(Label::new("Add Project to Catalog").size(LabelSize::Large))
            .child(
                Label::new("Remote URL")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(self.url_editor.clone())
            .child(
                Label::new("Name")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(self.name_editor.clone())
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
                        Button::new("add", "Add")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}
