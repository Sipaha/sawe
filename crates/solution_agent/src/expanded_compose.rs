//! Detached OS popup window for composing long messages. Spawned from the in-panel compose row when the user clicks the expand affordance.

use gpui::{
    App, Context, FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement,
    Render, Styled, Window, div,
};
use ui::prelude::*;

use crate::session_view::SolutionSessionView;

/// Detached OS popup window for editing the chat draft as a long-form
/// document. Renders a roomy multi-line editor + Save / Cancel footer in
/// its own resizable + movable OS window so the user can keep reading the
/// AI conversation or browsing project files while the popup is open.
/// Save writes the popup text back to the original compose editor;
/// Cancel and the OS close button both discard the change.
pub(crate) struct ExpandedComposeWindowView {
    pub(crate) editor: gpui::Entity<editor::Editor>,
    target: gpui::WeakEntity<editor::Editor>,
    owner: gpui::WeakEntity<SolutionSessionView>,
}

pub(crate) const EXPANDED_COMPOSE_DEFAULT_W: f32 = 1080.0;
/// Fallback height when no display is available (off-screen / headless).
/// In normal usage we open at `EXPANDED_COMPOSE_HEIGHT_RATIO` of the
/// current display's height.
pub(crate) const EXPANDED_COMPOSE_DEFAULT_H: f32 = 720.0;
pub(crate) const EXPANDED_COMPOSE_HEIGHT_RATIO: f32 = 0.8;

impl ExpandedComposeWindowView {
    pub(crate) fn new(
        initial_text: String,
        target: gpui::WeakEntity<editor::Editor>,
        owner: gpui::WeakEntity<SolutionSessionView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut e = editor::Editor::multi_line(window, cx);
            e.set_text(initial_text, window, cx);
            e.set_show_gutter(false, cx);
            e.set_show_line_numbers(false, cx);
            e.set_show_scrollbars(true, cx);
            // Wrap at the window width (like the inline compose editor) so a
            // long prompt never needs horizontal scrolling.
            e.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            e
        });
        Self {
            editor,
            target,
            owner,
        }
    }

    pub(crate) fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx);
        if let Some(target) = self.target.upgrade() {
            target.update(cx, |editor, cx| {
                editor.set_text(text, window, cx);
            });
        }
        self.dismiss(window, cx);
    }

    pub(crate) fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Clear the parent view's handle so the inline compose row can
        // swap back to the editor and the next "expand" click opens a
        // fresh window instead of trying to revive this one.
        if let Some(owner) = self.owner.upgrade() {
            owner.update(cx, |owner, cx| {
                owner.expanded_window = None;
                cx.notify();
            });
        }
        window.remove_window();
    }
}

impl Focusable for ExpandedComposeWindowView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for ExpandedComposeWindowView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ExpandedComposeWindow")
            .size_full()
            .p_3()
            .gap_3()
            .bg(cx.theme().colors().elevated_surface_background)
            .child(
                div()
                    .id("expanded-compose-editor-frame")
                    .flex_1()
                    .min_h_0()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .p_2()
                    .overflow_hidden()
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        ui::Button::new("expanded-compose-cancel", "Cancel")
                            .style(ui::ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dismiss(window, cx);
                            })),
                    )
                    .child(
                        ui::Button::new("expanded-compose-save", "Save")
                            .style(ui::ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save(window, cx);
                            })),
                    ),
            )
    }
}
