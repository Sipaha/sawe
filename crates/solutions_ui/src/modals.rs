use editor::{Editor, EditorEvent};
use gpui::{
    AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    WeakEntity, actions,
};
use settings::Settings as _;
use solutions::{CatalogId, SolutionId, SolutionStore, SolutionsSettings};
use std::path::PathBuf;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::actions::{
    CreateNewProjectInSolution, DeleteCatalogProject, DeleteSolution, EditCatalogProject,
    NewSolution,
};

actions!(
    solutions,
    [
        /// Open the modal to add a new project to the catalog.
        AddCatalogProject,
    ]
);

pub fn register(workspace: &mut Workspace, _: Option<&mut Window>, _: &mut Context<Workspace>) {
    workspace.register_action(|workspace, _: &NewSolution, window, cx| {
        let weak = cx.weak_entity();
        workspace.toggle_modal(window, cx, |window, cx| {
            NewSolutionModal::new(weak, window, cx)
        });
    });
    workspace.register_action(|workspace, _: &AddCatalogProject, window, cx| {
        let weak = cx.weak_entity();
        workspace.toggle_modal(window, cx, |window, cx| {
            AddCatalogProjectModal::new(weak, window, cx)
        });
    });
    workspace.register_action(|workspace, action: &EditCatalogProject, window, cx| {
        let id = CatalogId(action.id.clone());
        let store = SolutionStore::global(cx);
        let Some(prefill) = store.read_with(cx, |s, _| {
            s.catalog()
                .iter()
                .find(|p| p.id == id)
                .map(|p| EditCatalogPrefill {
                    name: p.name.clone(),
                    remote_url: p.remote_url.clone(),
                    default_branch: p.default_branch.clone().unwrap_or_default(),
                })
        }) else {
            return;
        };
        let weak = cx.weak_entity();
        workspace.toggle_modal(window, cx, move |window, cx| {
            EditCatalogProjectModal::new(weak, id, prefill, window, cx)
        });
    });
    workspace.register_action(|workspace, action: &DeleteCatalogProject, window, cx| {
        let id = CatalogId(action.id.clone());
        let store = SolutionStore::global(cx);
        let Some((name, references)) = store.read_with(cx, |s, _| {
            let project = s.catalog().iter().find(|p| p.id == id)?;
            Some((project.name.clone(), s.solutions_referencing(&id)))
        }) else {
            return;
        };
        workspace.toggle_modal(window, cx, move |_window, cx| {
            DeleteCatalogProjectModal::new(id, name, references, cx)
        });
    });
    workspace.register_action(|workspace, action: &DeleteSolution, window, cx| {
        let id = SolutionId(action.id.clone());
        let store = SolutionStore::global(cx);
        // Look up the solution's display name + root for the modal copy.
        // If the id is unknown (stale action / already-deleted), do nothing.
        let Some((name, root)) = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|sol| sol.id == id)
                .map(|sol| (sol.name.clone(), sol.root.clone()))
        }) else {
            return;
        };
        workspace.toggle_modal(window, cx, |_window, cx| {
            DeleteSolutionModal::new(id, name, root, cx)
        });
    });
    workspace.register_action(
        |workspace, action: &CreateNewProjectInSolution, window, cx| {
            let id = SolutionId(action.solution_id.clone());
            open_new_project_in_solution(workspace, id, window, cx);
        },
    );
}

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

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_editor.read(cx).text(cx);
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let root = SolutionsSettings::get_global(cx).root.clone();
        let store = SolutionStore::global(cx);
        store
            .update(cx, |s, cx| s.create_solution(name, root, cx))
            .log_err();
        cx.emit(DismissEvent);
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

pub struct AddCatalogProjectModal {
    name_editor: Entity<Editor>,
    url_editor: Entity<Editor>,
    branch_editor: Entity<Editor>,
    _workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    _url_subscription: Subscription,
}

impl AddCatalogProjectModal {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        let store = SolutionStore::global(cx);
        store
            .update(cx, |s, cx| {
                s.add_catalog_project(&name, &url, default_branch, cx)
            })
            .log_err();
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

impl EventEmitter<DismissEvent> for AddCatalogProjectModal {}

impl Focusable for AddCatalogProjectModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.focus_handle(cx)
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

pub struct DeleteSolutionModal {
    id: SolutionId,
    name: String,
    root: PathBuf,
    focus_handle: FocusHandle,
}

impl DeleteSolutionModal {
    fn new(id: SolutionId, name: String, root: PathBuf, cx: &mut Context<Self>) -> Self {
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

/// Initial values handed to [`EditCatalogProjectModal::new`]. Snapshotted
/// at action-dispatch time so the modal does not need a borrow into the
/// store after the action handler returns.
struct EditCatalogPrefill {
    name: String,
    remote_url: String,
    default_branch: String,
}

pub struct EditCatalogProjectModal {
    catalog_id: CatalogId,
    name_editor: Entity<Editor>,
    url_editor: Entity<Editor>,
    branch_editor: Entity<Editor>,
    _workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
}

impl EditCatalogProjectModal {
    fn new(
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
        store
            .update(cx, |s, cx| {
                s.edit_catalog_project(&id, Some(name), new_branch, Some(url), cx)
            })
            .log_err();
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

/// Confirmation modal for `DeleteCatalogProject`. Surfaces the cascade
/// impact (every solution that references the project will lose its
/// member, and each member's local clone directory will be wiped) so
/// the user can opt out before pulling the trigger.
pub struct DeleteCatalogProjectModal {
    catalog_id: CatalogId,
    project_name: String,
    references: Vec<(SolutionId, String)>,
    focus_handle: FocusHandle,
}

impl DeleteCatalogProjectModal {
    fn new(
        catalog_id: CatalogId,
        project_name: String,
        references: Vec<(SolutionId, String)>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            catalog_id,
            project_name,
            references,
            focus_handle: cx.focus_handle(),
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let store = SolutionStore::global(cx);
        let id = self.catalog_id.clone();
        // The store's cascade returns the list of clone directories
        // that need to be wiped from disk. Spawning the rm-rf here
        // (instead of inside the store) mirrors `DeleteSolutionModal`'s
        // pattern and keeps `SolutionStore` blocking-thread-free for
        // the GPUI test scheduler.
        let clone_paths = store
            .update(cx, |s, cx| s.remove_catalog_project_cascade(&id, cx))
            .log_err()
            .unwrap_or_default();
        if !clone_paths.is_empty() {
            cx.background_spawn(async move {
                for path in clone_paths {
                    let path_for_log = path.clone();
                    let result: std::io::Result<()> =
                        smol::unblock(move || std::fs::remove_dir_all(&path)).await;
                    if let Err(err) = result {
                        if err.kind() != std::io::ErrorKind::NotFound {
                            log::warn!(
                                "DeleteCatalogProjectModal: removing {} failed: {err} (orphan files left on disk)",
                                path_for_log.display(),
                            );
                        }
                    }
                }
            })
            .detach();
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for DeleteCatalogProjectModal {}

impl Focusable for DeleteCatalogProjectModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for DeleteCatalogProjectModal {
    fn debug_kind(&self) -> &'static str {
        "DeleteCatalogProject"
    }
}

impl Render for DeleteCatalogProjectModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body_text = if self.references.is_empty() {
            format!(
                "\"{}\" will be removed from the catalog. No solution currently uses it.",
                self.project_name,
            )
        } else if self.references.len() == 1 {
            format!(
                "\"{}\" will be removed from the catalog AND from {} (its local clone is deleted from disk).",
                self.project_name, self.references[0].1,
            )
        } else {
            format!(
                "\"{}\" will be removed from the catalog AND from {} solutions (each one's local clone is deleted from disk):",
                self.project_name,
                self.references.len(),
            )
        };

        let mut content = v_flex()
            .key_context("DeleteCatalogProjectModal")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .track_focus(&self.focus_handle)
            .w(rems(34.))
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
                    .child(Label::new("Delete Catalog Project").size(LabelSize::Large)),
            )
            .child(Label::new(body_text).color(Color::Default));

        // Render the per-solution list when there is more than one — for
        // a single reference the body sentence already names it.
        if self.references.len() > 1 {
            let mut list = v_flex().gap_0p5().pl_3();
            for (_, name) in &self.references {
                list = list.child(
                    Label::new(format!("• {name}"))
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                );
            }
            content = content.child(list);
        }

        content
            .child(Label::new("This action cannot be undone.").color(Color::Warning))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("delete-cat-cancel", "Cancel").on_click(cx.listener(
                            |this, _, window, cx| {
                                this.cancel(&menu::Cancel, window, cx);
                            },
                        )),
                    )
                    .child(
                        Button::new("delete-cat-confirm", "Delete")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}

/// Single-field modal for renaming a solution. Used by the title-bar tab
/// strip's right-click menu (Rename…) and the welcome list's pencil icon.
/// The (retired-in-Phase-2) dock panel handled rename inline within its
/// row; this modal replaces that path so rename keeps working once the
/// panel is gone.
pub struct RenameSolutionModal {
    id: SolutionId,
    name_editor: Entity<Editor>,
    focus_handle: FocusHandle,
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
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let new_name = self.name_editor.read(cx).text(cx).trim().to_string();
        if !new_name.is_empty() {
            SolutionStore::global(cx)
                .update(cx, |s, cx| s.rename_solution(&self.id, &new_name, cx))
                .log_err();
        }
        cx.emit(DismissEvent);
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
    let Some(current_name) = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .find(|sol| sol.id == id)
            .map(|sol| sol.name.clone())
    }) else {
        return;
    };
    workspace.toggle_modal(window, cx, move |window, cx| {
        RenameSolutionModal::new(id, &current_name, window, cx)
    });
}

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
