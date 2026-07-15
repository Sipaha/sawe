use gpui::{
    actions, App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, Styled, Window,
};
use schemars::JsonSchema;
use serde::Deserialize;
use ui::prelude::*;
use workspace::{ModalView, Workspace};

#[cfg(test)]
#[path = "find_in_path_tests.rs"]
mod find_in_path_tests;

/// Opens the Find in Path modal (project-wide search overlay).
#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Default, gpui::Action)]
#[action(namespace = find_in_path)]
#[serde(deny_unknown_fields)]
pub struct Toggle {
    #[serde(default)]
    pub replace_enabled: bool,
}

actions!(
    find_in_path,
    [
        /// Opens the Find in Path modal with the replace field revealed.
        ToggleReplace
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(register).detach();
}

fn register(
    workspace: &mut Workspace,
    _window: Option<&mut Window>,
    _cx: &mut Context<Workspace>,
) {
    workspace.register_action(|workspace, action: &Toggle, window, cx| {
        FindInPath::toggle(workspace, action.replace_enabled, window, cx);
    });
    workspace.register_action(|workspace, _: &ToggleReplace, window, cx| {
        FindInPath::toggle(workspace, true, window, cx);
    });
}

pub struct FindInPath {
    focus_handle: FocusHandle,
    replace_enabled: bool,
}

impl FindInPath {
    fn toggle(
        workspace: &mut Workspace,
        replace_enabled: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(existing) = workspace.active_modal::<Self>(cx) {
            existing.update(cx, |this, cx| {
                this.replace_enabled |= replace_enabled;
                this.focus_handle.focus(window, cx);
                cx.notify();
            });
            return;
        }
        workspace.toggle_modal(window, cx, |_window, cx| Self {
            focus_handle: cx.focus_handle(),
            replace_enabled,
        });
    }
}

impl Focusable for FindInPath {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for FindInPath {}

impl ModalView for FindInPath {
    fn fade_out_background(&self) -> bool {
        true
    }

    fn debug_kind(&self) -> &'static str {
        "FindInPath"
    }
}

impl Render for FindInPath {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Placeholder shell — replaced in Task 4 with the real header/results/preview.
        v_flex()
            .key_context("FindInPath")
            .track_focus(&self.focus_handle)
            .w(rems(60.))
            .h(rems(30.))
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_lg()
            .child("Find in Path")
    }
}
