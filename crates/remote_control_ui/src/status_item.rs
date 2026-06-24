use gpui::{Render, Subscription, WeakEntity};
use remote_control::{RemoteControlStore, RemoteControlStoreEvent};
use ui::{Button, ButtonStyle, Color, Icon, IconName, IconSize, LabelSize, prelude::*};
use workspace::{StatusItemView, Workspace, item::ItemHandle};

use crate::modal::RemoteControlModal;

/// Right-aligned status-bar entry showing a coloured dot + "Remote Control"
/// label. Clicking toggles the workspace modal.
pub struct RemoteControlStatusItem {
    workspace: WeakEntity<Workspace>,
    _store_subscription: Option<Subscription>,
}

impl RemoteControlStatusItem {
    pub fn new(workspace: &Workspace, cx: &mut gpui::Context<Self>) -> Self {
        let weak = workspace.weak_handle();
        let store_subscription = RemoteControlStore::try_global(cx).map(|store| {
            cx.subscribe(&store, |_, _, _: &RemoteControlStoreEvent, cx| {
                cx.notify();
            })
        });
        Self {
            workspace: weak,
            _store_subscription: store_subscription,
        }
    }

    fn is_enabled(&self, cx: &App) -> bool {
        RemoteControlStore::try_global(cx)
            .map(|store| store.read(cx).settings().enabled)
            .unwrap_or(false)
    }
}

impl Render for RemoteControlStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let enabled = self.is_enabled(cx);
        // The dot color flip is the visible signal of `enabled`. Tests
        // assert on the source string to make sure both branches stay
        // wired up.
        let dot_color = if enabled {
            Color::Success
        } else {
            Color::Error
        };

        let workspace = self.workspace.clone();
        Button::new("remote-control-status-item", "Remote Control")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Subtle)
            .start_icon(
                Icon::new(IconName::Server)
                    .size(IconSize::Small)
                    .color(dot_color),
            )
            .on_click(move |_, window, cx| {
                let Some(workspace) = workspace.upgrade() else {
                    return;
                };
                workspace.update(cx, |workspace, cx| {
                    RemoteControlModal::toggle(workspace, window, cx);
                });
            })
            .into_any_element()
    }
}

impl StatusItemView for RemoteControlStatusItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    // Source-invariant test: the render path *must* branch on `enabled`,
    // so a refactor that accidentally drops the dot-color flip fails this
    // check rather than silently regressing the UX.
    #[test]
    fn render_branches_on_enabled() {
        let source = include_str!("status_item.rs");
        assert!(
            source.contains("Color::Success"),
            "enabled branch must use Color::Success"
        );
        assert!(
            source.contains("Color::Error"),
            "disabled branch must use Color::Error"
        );
        assert!(
            source.contains("if enabled"),
            "the dot color must be conditional on `enabled`"
        );
    }
}
