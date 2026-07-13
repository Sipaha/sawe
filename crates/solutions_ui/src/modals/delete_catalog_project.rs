use gpui::{DismissEvent, EventEmitter, FocusHandle, Focusable};
use solutions::{CatalogId, SolutionId, SolutionStore};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::ModalView;

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
    pub(crate) fn new(
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
        let id = self.catalog_id;
        // The store's cascade returns the list of clone directories
        // that need to be wiped from disk. Spawning the rm-rf here
        // (instead of inside the store) mirrors `DeleteSolutionModal`'s
        // pattern and keeps `SolutionStore` blocking-thread-free for
        // the GPUI test scheduler.
        let clone_paths = store
            .update(cx, |s, cx| s.remove_catalog_project_cascade(id, cx))
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
