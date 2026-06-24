//! Review modal for AI-suggested merge resolutions.
//!
//! Renders the suggestion produced by [`crate::ai_suggest::suggest_merge`]
//! in a read-only editor with a per-line diff summary (additions /
//! deletions vs. the current Result buffer). The user explicitly picks
//! Apply (writes into the Result buffer) or Cancel (dismiss). We never
//! auto-apply: the spec for S-AI-CFL is unambiguous on that point.

use editor::{Editor, MinimapVisibility};
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, WeakEntity,
    Window, div, rems,
};
use language::Buffer;
use ui::prelude::*;
use ui::{Button, ButtonStyle, Color, Label, LabelSize};
use workspace::ModalView;

use crate::resolver_view::ConflictResolverView;

pub(crate) struct AiSuggestModal {
    suggestion: SharedString,
    diff_summary: SharedString,
    suggestion_editor: Entity<Editor>,
    resolver: WeakEntity<ConflictResolverView>,
    focus_handle: FocusHandle,
}

impl AiSuggestModal {
    pub fn new(
        resolver: WeakEntity<ConflictResolverView>,
        current_result: String,
        suggestion: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let diff_summary = summarise_diff(&current_result, &suggestion);
        let suggestion_editor = cx.new(|cx| {
            let buffer = cx.new(|cx| Buffer::local(suggestion.clone(), cx));
            let mut editor = Editor::for_buffer(buffer, None, window, cx);
            editor.set_read_only(true);
            editor.disable_inline_diagnostics();
            editor.disable_diagnostics(cx);
            editor.set_minimap_visibility(MinimapVisibility::Disabled, window, cx);
            editor
        });
        let focus_handle = cx.focus_handle();

        Self {
            suggestion: suggestion.into(),
            diff_summary: diff_summary.into(),
            suggestion_editor,
            resolver,
            focus_handle,
        }
    }

    fn apply(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let suggestion = self.suggestion.clone();
        if let Some(resolver) = self.resolver.upgrade() {
            resolver.update(cx, |resolver, cx| {
                resolver.replace_result_with(suggestion.as_ref(), window, cx);
            });
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for AiSuggestModal {}

impl Focusable for AiSuggestModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for AiSuggestModal {
    fn debug_kind(&self) -> &'static str {
        "AiMergeSuggestion"
    }

    fn dismiss_on_overlay_click(&self) -> bool {
        // The modal owns user-reviewable AI output that took an
        // ephemeral subprocess turn to produce. A stray click outside
        // shouldn't throw it away.
        false
    }
}

impl Render for AiSuggestModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("AiSuggestModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::apply))
            .on_action(cx.listener(Self::cancel))
            .flex()
            .flex_col()
            .gap_3()
            .w(rems(60.))
            .h(rems(40.))
            .p_4()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(Label::new("AI Merge Suggestion").size(LabelSize::Large))
                    .child(
                        Label::new(self.diff_summary.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_sm()
                    .child(self.suggestion_editor.clone()),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("ai-suggest-cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("ai-suggest-apply", "Apply")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}

/// Produce a one-line "+N / -M lines" summary comparing the current
/// Result content to the proposed suggestion. Cheap line-count delta
/// only — a real per-line diff is overkill for a top-of-modal status
/// strip and would require pulling in a full diff renderer.
fn summarise_diff(current: &str, suggestion: &str) -> String {
    let current_lines = current.lines().count();
    let suggestion_lines = suggestion.lines().count();
    let added = suggestion_lines.saturating_sub(current_lines);
    let removed = current_lines.saturating_sub(suggestion_lines);
    let suggestion_bytes = suggestion.len();
    format!(
        "{suggestion_lines} lines, {suggestion_bytes} bytes \
         (\u{0394} +{added} / -{removed} vs current Result)"
    )
}

#[cfg(test)]
mod tests {
    use super::summarise_diff;

    #[test]
    fn summarise_diff_reports_growth() {
        let current = "a\nb\nc\n";
        let suggestion = "a\nb\nc\nd\ne\n";
        let s = summarise_diff(current, suggestion);
        assert!(s.contains("+2"));
        assert!(s.contains("-0"));
    }

    #[test]
    fn summarise_diff_reports_shrink() {
        let current = "a\nb\nc\nd\ne\n";
        let suggestion = "a\nb\n";
        let s = summarise_diff(current, suggestion);
        assert!(s.contains("+0"));
        assert!(s.contains("-3"));
    }

    #[test]
    fn summarise_diff_reports_no_change_when_lines_match() {
        let same = "x\ny\n";
        let s = summarise_diff(same, same);
        assert!(s.contains("+0"));
        assert!(s.contains("-0"));
    }
}
