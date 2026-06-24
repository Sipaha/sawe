use gpui::{
    App, Context, Empty, EventEmitter, IntoElement, ParentElement, Render, SharedString,
    WeakEntity, Window,
};
use project::Project;
use settings::Settings;
use ui::{Button, Tooltip, prelude::*};
use util::paths::PathStyle;

use crate::{
    HideStatusItem, StatusItemView, Workspace, item::ItemHandle,
    workspace_settings::StatusBarSettings,
};

pub struct ActiveFileName {
    /// Path shown in the status bar — the active file's path relative to
    /// its worktree, prefixed with the worktree's root name (so it's
    /// unambiguous across the multiple worktrees of a Solution).
    display_path: Option<SharedString>,
    full_path: Option<SharedString>,
    project: WeakEntity<Project>,
}

impl ActiveFileName {
    pub fn new(workspace: &Workspace) -> Self {
        Self {
            display_path: None,
            full_path: None,
            project: workspace.project().downgrade(),
        }
    }
}

impl Render for ActiveFileName {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !StatusBarSettings::get_global(cx).show_active_file {
            return Empty.into_any_element();
        }

        let Some(display_path) = self.display_path.clone() else {
            return Empty.into_any_element();
        };

        let tooltip_text = self
            .full_path
            .clone()
            .unwrap_or_else(|| display_path.clone());

        div()
            .child(
                Button::new("active-file-name-button", display_path)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(tooltip_text)),
            )
            .into_any_element()
    }
}

impl EventEmitter<crate::ToolbarItemEvent> for ActiveFileName {}

impl StatusItemView for ActiveFileName {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(item) = active_pane_item {
            self.display_path = item.project_path(cx).map(|project_path| {
                let relative = project_path.path.display(PathStyle::local());
                // `set_active_pane_item` runs inside a `Workspace` update, so
                // we can't touch the workspace entity here — read the project
                // (not mid-update during pane focus) to resolve the worktree
                // name, and fall back to the bare relative path otherwise.
                let root_name = self
                    .project
                    .read_with(cx, |project, cx| {
                        project
                            .worktree_for_id(project_path.worktree_id, cx)
                            .map(|worktree| worktree.read(cx).root_name_str().to_string())
                    })
                    .ok()
                    .flatten();
                match root_name {
                    Some(root_name) => format!("{root_name}/{relative}").into(),
                    None => relative.into_owned().into(),
                }
            });
            self.full_path = item.tab_tooltip_text(cx);
        } else {
            self.display_path = None;
            self.full_path = None;
        }
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|settings| {
            settings.status_bar.get_or_insert_default().show_active_file = Some(false);
        }))
    }
}
