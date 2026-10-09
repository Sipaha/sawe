//! In-editor previews shared by conversation images, tool arguments and file links.

use std::sync::Arc;

use gpui::AnyElement;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Window, div, px,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::prelude::*;
use ui::utils::WithRemSize;
use ui::{CopyButton, IconButton, IconName, Label, LabelSize, Tooltip};

/// A report preview follows the Markdown preview font settings. Keep its type scale one step below the regular
/// preview while still following `markdown_preview.font_size` and zoom.
const DOCUMENT_FONT_SCALE: f32 = 0.875;

/// Content shown by the workspace preview modal.
pub(crate) enum PreviewContent {
    Image(Arc<gpui::Image>),
    FileInfo {
        title: SharedString,
        path: std::path::PathBuf,
        details: SharedString,
    },
    /// A tool call's full argument: the whole command, path or pattern behind
    /// the clipped preview row in the conversation.
    Text {
        title: SharedString,
        body: SharedString,
    },
    /// A markdown document, shown RENDERED. A report the agent wrote is
    /// written to be read as prose — headings, lists, links, fenced code — and
    /// showing it as its own source is showing the reader the thing they asked
    /// to be spared.
    Markdown {
        title: SharedString,
        source: SharedString,
        /// Directory the document was read from. Relative links inside a
        /// document point at its neighbours, not at the conversation's
        /// worktrees, so they are resolved from here.
        base_dir: Option<std::path::PathBuf>,
    },
}

impl PreviewContent {
    fn window_title(&self) -> SharedString {
        match self {
            PreviewContent::Image(_) => "Image preview".into(),
            PreviewContent::Text { title, .. }
            | PreviewContent::Markdown { title, .. }
            | PreviewContent::FileInfo { title, .. } => title.clone(),
        }
    }
}

/// Open or retarget a preview in the originating workspace, without creating an OS window.
pub(crate) fn open_preview(content: PreviewContent, window: &mut Window, cx: &mut App) {
    let workspace = window.root::<workspace::Workspace>().flatten().or_else(|| {
        window
            .root::<workspace::MultiWorkspace>()
            .flatten()
            .map(|multi| multi.read(cx).workspace().clone())
    });
    let Some(workspace) = workspace else {
        return;
    };
    // Link callbacks can run while the modal/workspace is borrowed. Retarget after that update.
    window.defer(cx, move |window, cx| {
        workspace.update(cx, |workspace, cx| {
            if let Some(preview) = workspace.active_modal::<PreviewWindow>(cx) {
                preview.update(cx, |preview, cx| {
                    preview.set_content(content, window, cx);
                    window.focus(&preview.focus_handle(cx), cx);
                });
            } else {
                workspace.toggle_modal(window, cx, move |window, cx| {
                    PreviewWindow::new(content, window, cx)
                });
            }
        });
    });
}

impl EventEmitter<DismissEvent> for PreviewWindow {}
impl workspace::ModalView for PreviewWindow {
    fn debug_kind(&self) -> &'static str {
        "ConversationPreview"
    }
    fn fade_out_background(&self) -> bool {
        true
    }
}

pub(crate) struct PreviewWindow {
    content: PreviewContent,
    /// Present only while `content` is `Text`. A read-only editor rather than a
    /// label so the text can be selected piecewise — copying the whole thing is
    /// one button, but "just the file path out of the command" is not.
    editor: Option<Entity<editor::Editor>>,
    /// Present only while `content` is `Markdown`. Parsing is the entity's job,
    /// so it is built once per content swap rather than per frame.
    markdown: Option<Entity<Markdown>>,
    focus_handle: FocusHandle,
}

impl PreviewWindow {
    fn new(content: PreviewContent, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            content: PreviewContent::Text {
                title: SharedString::default(),
                body: SharedString::default(),
            },
            editor: None,
            markdown: None,
            focus_handle: cx.focus_handle(),
        };
        this.set_content(content, window, cx);
        this
    }

    fn set_content(
        &mut self,
        content: PreviewContent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.markdown = match &content {
            PreviewContent::Markdown { source, .. } => {
                // `try_global`, not `global`: a fenced block loses only its
                // syntax colours without the registry, which is not worth
                // panicking a render pass over in a harness that has no
                // `AppState`.
                let languages =
                    workspace::AppState::try_global(cx).map(|state| state.languages.clone());
                Some(cx.new(|cx| Markdown::new(source.clone(), languages, None, cx)))
            }
            PreviewContent::Image(_)
            | PreviewContent::Text { .. }
            | PreviewContent::FileInfo { .. } => None,
        };
        self.editor = match &content {
            PreviewContent::Image(_)
            | PreviewContent::Markdown { .. }
            | PreviewContent::FileInfo { .. } => None,
            PreviewContent::Text { body, .. } => Some(cx.new(|cx| {
                let mut editor = editor::Editor::multi_line(window, cx);
                editor.set_show_gutter(false, cx);
                editor.set_show_line_numbers(false, cx);
                editor.set_show_vertical_scrollbar(true, cx);
                editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
                editor.set_text(body.clone(), window, cx);
                // `set_text` leaves the cursor at the end, which scrolls a long
                // command to its last line. The user clicked to read the
                // command, so start at the top.
                editor.move_to_beginning(&editor::actions::MoveToBeginning, window, cx);
                editor.set_read_only(true);
                editor
            })),
        };
        self.content = content;
        cx.notify();
    }
}

impl Focusable for PreviewWindow {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.editor {
            Some(editor) => editor.focus_handle(cx),
            // Markdown has no editor to focus, so the window keeps its own
            // handle — which also keeps Escape on the `PreviewWindow` key
            // context instead of the deeper `Editor` one.
            None => self.focus_handle.clone(),
        }
    }
}

impl Render for PreviewWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.content.window_title();
        let copyable = match &self.content {
            PreviewContent::Text { body, .. } => Some(body.clone()),
            // The markdown SOURCE, not the rendering: what a copy is for here
            // is pasting the document somewhere else that also renders it.
            PreviewContent::Markdown { source, .. } => Some(source.clone()),
            PreviewContent::Image(_) => None,
            PreviewContent::FileInfo { details, .. } => Some(details.clone()),
        };

        let header = h_flex()
            .id("preview-window-header")
            .flex_shrink_0()
            .w_full()
            .gap_2()
            .px_3()
            .py_1p5()
            .justify_between()
            .items_center()
            .bg(cx.theme().colors().title_bar_background)
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(title).size(LabelSize::Default).truncate())
            .child(
                h_flex()
                    .gap_1()
                    .when_some(copyable, |this, body| {
                        this.child(
                            CopyButton::new("preview-copy", body)
                                .tooltip_label("Copy the full text"),
                        )
                    })
                    .child(
                        IconButton::new("preview-close", IconName::Close)
                            .tooltip(Tooltip::text("Close"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            );

        let body: AnyElement = match (&self.content, &self.editor) {
            (PreviewContent::FileInfo { path, details, .. }, _) => {
                let path = path.clone();
                v_flex()
                    .id("preview-file-info")
                    .debug_selector(|| "PREVIEW-FILE-INFO".into())
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .gap_3()
                    .child(
                        div()
                            .id("preview-file-details")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(Label::new(details.clone())),
                    )
                    .child(
                        ui::Button::new("preview-reveal-file", "Open in File Manager")
                            .end_icon(Icon::new(IconName::Folder))
                            .on_click(move |_, _, cx| cx.reveal_path(&path)),
                    )
                    .into_any_element()
            }
            (PreviewContent::Image(image), _) => div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    gpui::img(image.clone())
                        .object_fit(gpui::ObjectFit::Contain)
                        .size_full(),
                )
                .into_any_element(),
            (PreviewContent::Text { .. }, Some(editor)) => div()
                .id("preview-text")
                .flex_1()
                .min_h_0()
                .p_3()
                .overflow_hidden()
                .child(editor.clone())
                .into_any_element(),
            // `set_content` builds the editor for every `Text`, so this is
            // unreachable; an empty body beats an `unwrap` in a render pass.
            (PreviewContent::Text { .. }, None) => div().flex_1().into_any_element(),
            (PreviewContent::Markdown { base_dir, .. }, _) => match &self.markdown {
                Some(markdown) => {
                    // `MarkdownFont::Preview` is the document face (the
                    // markdown-preview font settings), not the chat's — this
                    // window is showing a file, not a message.
                    let style = MarkdownStyle::themed(MarkdownFont::Preview, window, cx);
                    let preview_font_size = ThemeSettings::get_global(cx)
                        .markdown_preview_font_size(cx)
                        * DOCUMENT_FONT_SCALE;
                    let roots: Vec<std::path::PathBuf> = base_dir.iter().cloned().collect();
                    div()
                        .id("preview-markdown")
                        .flex_1()
                        .min_h_0()
                        .p_3()
                        .overflow_y_scroll()
                        .child(
                            // Preview typography is rem-based. The regular
                            // MarkdownPreviewView supplies this same local rem
                            // root; this compact modal uses a smaller root,
                            // but without one it silently inherits the larger
                            // UI size and ignores markdown_preview.font_size.
                            WithRemSize::new(preview_font_size).child(
                                MarkdownElement::new(markdown.clone(), style).on_url_click(
                                    move |url, window, cx| {
                                        // A relative link in a document points at
                                        // its neighbours, so the document's own
                                        // directory is the root it resolves
                                        // against; anything else falls through to
                                        // the browser.
                                        crate::conversation_render::link::open_link_within_preview(
                                            url.as_ref(),
                                            &roots,
                                            window,
                                            cx,
                                        );
                                    },
                                ),
                            ),
                        )
                        .into_any_element()
                }
                None => div().flex_1().into_any_element(),
            },
        };

        div()
            .key_context("PreviewWindow")
            .track_focus(&self.focus_handle)
            .w(px(
                (window.viewport_size().width.as_f32() * 0.85).min(1100.0)
            ))
            .h(px((window.viewport_size().height.as_f32() - 120.0)
                .max(120.0)
                .min(850.0)))
            .flex()
            .flex_col()
            .overflow_hidden()
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_lg()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(header)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal};

    #[gpui::test]
    async fn preview_retargets_and_dismisses_inside_the_workspace(cx: &mut TestAppContext) {
        let (_, _tmp, project) = crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| theme_settings::init(theme::LoadThemes::JustBase, cx));
        let (multi, cx) = cx
            .add_window_view(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
        let count = cx.update(|_, cx| cx.windows().len());
        workspace.update_in(cx, |_, window, cx| {
            open_preview(
                PreviewContent::Text {
                    title: "Command".into(),
                    body: "echo hello".into(),
                },
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let preview = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<PreviewWindow>(cx)
            })
            .unwrap();
        assert_eq!(cx.update(|_, cx| cx.windows().len()), count);
        assert!(preview.read_with(cx, |preview, _| preview.editor.is_some()));
        let dir = tempfile::tempdir().unwrap();
        let next = dir.path().join("next.md");
        std::fs::write(&next, "# Next document").unwrap();
        preview.update_in(cx, |_, window, cx| {
            crate::conversation_render::link::open_link_within_preview(
                "next.md",
                &[dir.path().into()],
                window,
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| workspace
                .active_modal::<PreviewWindow>(cx)),
            Some(preview.clone())
        );
        assert!(
            preview.read_with(cx, |preview, _| preview.markdown.is_some()
                && preview.editor.is_none())
        );
        preview.update(cx, |_, cx| cx.emit(DismissEvent));
        cx.run_until_parked();
        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<PreviewWindow>(cx))
                .is_none()
        );
        assert_eq!(
            cx.update(|_, cx| cx.windows().len()),
            count,
            "dismiss must not close the editor window"
        );
        workspace.update_in(cx, |_, window, cx| {
            open_preview(
                PreviewContent::FileInfo {
                    title: "archive.zip".into(),
                    path: next.clone(),
                    details: "Size: 10 bytes".into(),
                },
                window,
                cx,
            )
        });
        cx.run_until_parked();
        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<PreviewWindow>(cx))
                .is_some()
        );
        assert_eq!(cx.update(|_, cx| cx.windows().len()), count);
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        assert!(
            cx.debug_bounds("PREVIEW-FILE-INFO").is_some(),
            "file information must be painted inside the modal"
        );
    }
    #[gpui::test]
    async fn modal_markdown_preview_respects_preview_font_size(cx: &mut TestAppContext) {
        let (_solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.markdown_preview.get_or_insert_default().font_size = Some(10.0.into());
                });
            });
        });
        cx.run_until_parked();
        let (multi, cx) = cx
            .add_window_view(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());

        workspace.update_in(cx, |_, window, cx| {
            open_preview(
                PreviewContent::Markdown {
                    title: "TYPE-SCALE.md".into(),
                    source: "paragraph one\n\nparagraph two\n\nparagraph three".into(),
                    base_dir: None,
                },
                window,
                cx,
            );
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        let small_height = cx
            .debug_bounds("inner")
            .expect("the markdown root was drawn")
            .size
            .height;

        cx.update(|_, cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.markdown_preview.get_or_insert_default().font_size = Some(20.0.into());
                });
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        let large_height = cx
            .debug_bounds("inner")
            .expect("the markdown root was redrawn")
            .size
            .height;

        assert!(
            large_height > small_height * 1.5,
            "the preview setting must scale this window: {small_height:?} -> {large_height:?}"
        );
    }
}
