//! The conversation's one floating preview window — images and full tool
//! arguments both land here.
//!
//! Two things it exists to guarantee, both of them user-visible:
//!
//! * **There is exactly one.** Every click used to call `cx.open_window`, so
//!   walking a conversation with screenshots in it buried the desktop under a
//!   stack of "Image preview" windows the user then had to close one by one.
//!   Opening a second preview now retargets the first and raises it.
//! * **It is a real window, not a modal.** The full shell command used to open
//!   in a workspace modal, which cannot be moved, cannot be resized, and blocks
//!   the pane behind it — the worst surface for the one case that needs room
//!   (a heredoc, a long pipeline). A window can be dragged aside and kept open
//!   next to the conversation it came from.

use std::sync::Arc;

use gpui::AnyElement;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, Global, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Render, SharedString, Styled, Window, WindowHandle,
    div, px,
};
use ui::prelude::*;
use ui::{CopyButton, IconButton, IconName, Label, LabelSize, Tooltip};

/// What the preview window is showing. One window, two kinds of content —
/// keeping them in one view is what lets a click on an image retarget a window
/// that is currently showing a shell command, and the other way round.
pub(crate) enum PreviewContent {
    Image(Arc<gpui::Image>),
    /// A tool call's full argument: the whole command, path or pattern behind
    /// the clipped preview row in the conversation.
    Text {
        title: SharedString,
        body: SharedString,
    },
}

impl PreviewContent {
    fn window_title(&self) -> SharedString {
        match self {
            PreviewContent::Image(_) => "Image preview".into(),
            PreviewContent::Text { title, .. } => title.clone(),
        }
    }
}

/// The open window, if there is one. `WindowHandle::update` fails once the
/// window is gone, which is the liveness check — there is no "was it closed"
/// event to subscribe to, and a handle to a closed window is indistinguishable
/// from a live one until you try to use it.
#[derive(Default)]
struct OpenPreview(Option<WindowHandle<PreviewWindow>>);

impl Global for OpenPreview {}

/// Show `content` in the preview window, opening it if it is not already up.
pub(crate) fn open_preview(content: PreviewContent, window: &mut Window, cx: &mut App) {
    let title = content.window_title();
    let existing = cx.try_global::<OpenPreview>().and_then(|g| g.0);

    // `update` moves its closure, so a failed call would take `content` with
    // it. Hand the closure an `Option` to `take` instead, and read afterwards
    // whether it actually ran.
    let mut pending = Some(content);
    if let Some(handle) = existing {
        let retargeted = handle.update(cx, |preview, window, cx| {
            if let Some(content) = pending.take() {
                preview.set_content(content, window, cx);
            }
            window.set_window_title(&title);
            window.activate_window();
            cx.notify();
        });
        if retargeted.is_ok() {
            return;
        }
        // The window was closed behind our back; fall through and open a new
        // one with the content the closure never got to consume.
        cx.set_global(OpenPreview(None));
    }
    let content = match pending {
        Some(content) => content,
        // Unreachable: `take` only runs inside a closure whose `Ok` returns
        // above. Nothing to show rather than a panic in a click handler.
        None => return,
    };

    let display_size = window
        .display(cx)
        .or_else(|| cx.primary_display())
        .map(|d| d.bounds().size)
        .unwrap_or(gpui::Size {
            width: px(800.0),
            height: px(600.0),
        });
    let size = gpui::Size {
        width: display_size.width * 0.6,
        height: display_size.height * 0.7,
    };
    let bounds = gpui::WindowBounds::centered(size, cx);
    let opened = cx.open_window(
        gpui::WindowOptions {
            titlebar: Some(gpui::TitlebarOptions {
                title: Some(title),
                appears_transparent: false,
                traffic_light_position: None,
            }),
            window_bounds: Some(bounds),
            is_resizable: true,
            is_minimizable: true,
            kind: gpui::WindowKind::Normal,
            ..Default::default()
        },
        move |window, cx| {
            window.activate_window();
            cx.new(|cx| PreviewWindow::new(content, window, cx))
        },
    );
    match opened {
        Ok(handle) => cx.set_global(OpenPreview(Some(handle))),
        Err(err) => log::error!("failed to open the preview window: {err:?}"),
    }
}

pub(crate) struct PreviewWindow {
    content: PreviewContent,
    /// Present only while `content` is `Text`. A read-only editor rather than a
    /// label so the text can be selected piecewise — copying the whole thing is
    /// one button, but "just the file path out of the command" is not.
    editor: Option<Entity<editor::Editor>>,
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
        self.editor = match &content {
            PreviewContent::Image(_) => None,
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
            None => self.focus_handle.clone(),
        }
    }
}

impl Render for PreviewWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.content.window_title();
        let copyable = match &self.content {
            PreviewContent::Text { body, .. } => Some(body.clone()),
            PreviewContent::Image(_) => None,
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
            // The window may be drawn without server-side decorations, in which
            // case there is no titlebar to grab; this row is the drag handle
            // either way, so "move it aside" never depends on the window
            // manager's choice.
            .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
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
                            .on_click(|_, window, _| window.remove_window()),
                    ),
            );

        let body: AnyElement = match (&self.content, &self.editor) {
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
        };

        div()
            .key_context("PreviewWindow")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().colors().editor_background)
            .on_action(|_: &menu::Cancel, window, _| window.remove_window())
            .child(header)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn an_image() -> Arc<gpui::Image> {
        // Never rendered by these tests, so the bytes do not have to decode —
        // what is under test is which window the content lands in.
        Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, Vec::new()))
    }

    /// The whole point of the rewrite: walking a conversation full of
    /// screenshots used to bury the desktop under one OS window per click.
    #[gpui::test]
    async fn every_preview_lands_in_the_same_window(cx: &mut TestAppContext) {
        let (_solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| workspace::Workspace::test_new(project, window, cx));

        let before = cx.update(|_, cx| cx.windows().len());

        workspace.update_in(cx, |_, window, cx| {
            open_preview(PreviewContent::Image(an_image()), window, cx);
        });
        cx.run_until_parked();
        let after_first = cx.update(|_, cx| cx.windows().len());
        assert_eq!(
            after_first,
            before + 1,
            "the first preview opens the window"
        );
        let first = cx
            .update(|_, cx| cx.global::<OpenPreview>().0)
            .expect("the window handle is remembered");

        workspace.update_in(cx, |_, window, cx| {
            open_preview(PreviewContent::Image(an_image()), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| cx.windows().len()),
            after_first,
            "a second image must retarget the open window, not stack another \
             one on top of it"
        );
        assert_eq!(
            cx.update(|_, cx| cx.global::<OpenPreview>().0),
            Some(first),
            "and it must be the SAME window, not a replacement"
        );

        // The tool argument is the other half of the ask: it used to be a
        // workspace modal, and it has to land in this same window.
        workspace.update_in(cx, |_, window, cx| {
            open_preview(
                PreviewContent::Text {
                    title: "Bash".into(),
                    body: "echo hi".into(),
                },
                window,
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| cx.windows().len()),
            after_first,
            "a tool argument opens no window of its own"
        );
        assert_eq!(cx.update(|_, cx| cx.global::<OpenPreview>().0), Some(first));
        first
            .update(cx, |preview, _, _| {
                assert!(
                    matches!(preview.content, PreviewContent::Text { .. }),
                    "the window is showing the argument now, not the stale image"
                );
                assert!(
                    preview.editor.is_some(),
                    "text is shown in a selectable read-only editor"
                );
            })
            .expect("the window is still open");
    }

    /// A handle to a closed window is indistinguishable from a live one until
    /// you use it, so the reopen path is only reachable through a failed
    /// `update` — and a preview that silently stopped opening after the user
    /// closed it once is the obvious way to get this wrong.
    #[gpui::test]
    async fn closing_the_window_does_not_stop_the_next_preview(cx: &mut TestAppContext) {
        let (_solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| workspace::Workspace::test_new(project, window, cx));

        workspace.update_in(cx, |_, window, cx| {
            open_preview(PreviewContent::Image(an_image()), window, cx);
        });
        cx.run_until_parked();
        let first = cx
            .update(|_, cx| cx.global::<OpenPreview>().0)
            .expect("handle");
        let with_preview = cx.update(|_, cx| cx.windows().len());

        first
            .update(cx, |_, window, _| window.remove_window())
            .expect("close the preview");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| cx.windows().len()),
            with_preview - 1,
            "precondition: the window is really gone"
        );

        workspace.update_in(cx, |_, window, cx| {
            open_preview(PreviewContent::Image(an_image()), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| cx.windows().len()),
            with_preview,
            "the next click has to open a fresh window"
        );
        assert_ne!(
            cx.update(|_, cx| cx.global::<OpenPreview>().0),
            Some(first),
            "and remember the new one, or the click after it reuses a corpse"
        );
    }
}
