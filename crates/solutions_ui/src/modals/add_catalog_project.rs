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
        if url.is_empty() {
            return;
        }
        // Report a bad URL BEFORE the empty-name guard. The name auto-fill
        // deliberately derives nothing from a URL the store would reject, so
        // checking the name first would make Confirm a silent no-op on exactly
        // the input the user most needs to be told about.
        if let Err(error) = solutions::normalize_remote_url(&url) {
            self.error = Some(humanize_catalog_error(&error).into());
            cx.notify();
            return;
        }
        if name.is_empty() {
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
            cx.spawn(async move |_, _| task.await)
                .detach_and_log_err(cx);
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
///
/// Derives from the NORMALISED URL, and derives NOTHING at all from a URL the
/// store would reject. Two reasons: a typo the store strips (a trailing `#`)
/// must not get baked into the project NAME, where nothing would ever remove
/// it; and a pasted browse URL would otherwise name the project after its
/// last path segment (`master`), which then blocks the auto-fill from
/// correcting itself once the user fixes the URL.
fn derive_project_name_from_url(url: &str) -> String {
    let Ok(normalized) = solutions::normalize_remote_url(url) else {
        return String::new();
    };
    let trimmed = normalized.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or("");
    last.trim_end_matches(".git").to_string()
}

/// Turn the store's machine-tagged rejection into a sentence for the modal.
/// The `duplicate_name:` / `duplicate_remote:` / `invalid_remote:` prefixes exist
/// so MCP callers can branch on them; a human just wants to be told what to
/// change.
pub(super) fn humanize_catalog_error(error: &anyhow::Error) -> String {
    let text = error.to_string();
    match text.split_once(": ") {
        Some((tag, rest))
            if tag == "duplicate_name" || tag == "duplicate_remote" || tag == "invalid_remote" =>
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use tempfile::TempDir;

    #[test]
    fn derives_the_repository_name_from_every_form_users_paste() {
        assert_eq!(
            derive_project_name_from_url("git@host.ru:org/repo.git"),
            "repo"
        );
        assert_eq!(
            derive_project_name_from_url("https://host.ru/org/repo.git"),
            "repo"
        );
        assert_eq!(
            derive_project_name_from_url("https://host.ru/org/repo"),
            "repo"
        );
        assert_eq!(
            derive_project_name_from_url("https://host.ru/org/repo/"),
            "repo"
        );
    }

    #[test]
    fn a_typo_never_reaches_the_project_name() {
        // The reported bug: `citeck-hazelcast#` became both the remote AND the
        // catalog project's name, and only the remote was ever fixable.
        assert_eq!(
            derive_project_name_from_url("https://host.ru/org/citeck-hazelcast#"),
            "citeck-hazelcast",
        );
        // A browse URL is refused by the store, so it must not seed a name
        // either — otherwise Name is stuck on `master` and no longer
        // auto-corrects when the user fixes the URL.
        assert_eq!(
            derive_project_name_from_url("https://host.ru/org/repo/-/tree/master"),
            "",
        );
    }

    fn build_modal<'a>(
        cx: &'a mut TestAppContext,
    ) -> (
        gpui::Entity<AddCatalogProjectModal>,
        TempDir,
        &'a mut VisualTestContext,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            let store = SolutionStore::for_test(dir.path().join("catalog.json"), cx);
            solutions::install_global_for_test(store, cx);
        });
        let (modal, cx) = cx.add_window_view(|window, cx| {
            AddCatalogProjectModal::new(gpui::WeakEntity::new_invalid(), None, window, cx)
        });
        (modal, dir, cx)
    }

    /// A URL the store refuses must be REPORTED, not swallowed. The name
    /// auto-fill deliberately leaves Name empty for such a URL, so an
    /// empty-name guard ahead of the URL check would turn Confirm into a
    /// silent no-op — which is how the original bug felt to the user.
    #[gpui::test]
    async fn confirm_reports_a_refused_url_even_with_an_empty_name(cx: &mut TestAppContext) {
        let (modal, _dir, cx) = build_modal(cx);

        modal.update_in(cx, |modal, window, cx| {
            modal.url_editor.update(cx, |editor, cx| {
                editor.set_text("https://host.example/org/repo/-/tree/main", window, cx);
            });
        });
        cx.run_until_parked();

        modal.update_in(cx, |modal, window, cx| {
            assert!(
                modal.name_editor.read(cx).text(cx).is_empty(),
                "a refused URL must not seed the project name"
            );
            modal.confirm(&menu::Confirm, window, cx);
        });

        modal.read_with(cx, |modal, _| {
            let error = modal.error.clone().expect("the refusal must be surfaced");
            assert!(
                error.contains("web page URL") && error.contains("https://host.example/org/repo"),
                "got: {error}"
            );
            assert!(
                !error.starts_with("invalid_remote"),
                "the machine tag must be stripped for the human, got: {error}"
            );
        });
    }

    /// The other side: a URL that only needs normalising is accepted, the
    /// name is derived from the CLEANED url, and the catalog row stores it.
    #[gpui::test]
    async fn confirm_accepts_a_url_that_only_needs_normalising(cx: &mut TestAppContext) {
        let (modal, _dir, cx) = build_modal(cx);

        modal.update_in(cx, |modal, window, cx| {
            modal.url_editor.update(cx, |editor, cx| {
                editor.set_text("https://host.example/org/citeck-hazelcast#", window, cx);
            });
        });
        cx.run_until_parked();

        modal.update_in(cx, |modal, window, cx| {
            assert_eq!(modal.name_editor.read(cx).text(cx), "citeck-hazelcast");
            modal.confirm(&menu::Confirm, window, cx);
        });

        modal.read_with(cx, |modal, _| assert_eq!(modal.error, None));
        cx.update(|_, cx| {
            let store = SolutionStore::global(cx);
            let catalog = store.read(cx).catalog().to_vec();
            assert_eq!(catalog.len(), 1);
            assert_eq!(catalog[0].name, "citeck-hazelcast");
            assert_eq!(
                catalog[0].remote_url,
                "https://host.example/org/citeck-hazelcast",
            );
        });
    }
}
