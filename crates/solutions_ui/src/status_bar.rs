use gpui::{Render, Subscription, WeakEntity};
use solutions::{SolutionStore, SolutionStoreEvent};
use ui::prelude::*;
use workspace::{StatusItemView, Workspace, item::ItemHandle};

use crate::actions::OpenSolution;

pub struct SolutionsStatusItem {
    workspace: WeakEntity<Workspace>,
    _store_subscription: Subscription,
}

impl SolutionsStatusItem {
    pub fn new(workspace: &Workspace, cx: &mut gpui::Context<Self>) -> Self {
        let weak = workspace.weak_handle();
        let store = SolutionStore::global(cx);
        let store_subscription = cx.subscribe(&store, |_, _, _e: &SolutionStoreEvent, cx| {
            cx.notify();
        });
        Self {
            workspace: weak,
            _store_subscription: store_subscription,
        }
    }

    fn current_solution(&self, cx: &App) -> Option<(String, usize)> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().clone();
        let store = SolutionStore::try_global(cx)?;
        for tree in project.read(cx).worktrees(cx) {
            let path = tree.read(cx).abs_path();
            let found = store.read_with(cx, |s, _| {
                s.solution_for_path(&path)
                    .map(|sol| (sol.name.clone(), sol.members.len()))
            });
            if found.is_some() {
                return found;
            }
        }
        None
    }
}

impl Render for SolutionsStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let Some((name, member_count)) = self.current_solution(cx) else {
            return div().into_any_element();
        };
        Button::new(
            "solutions-status-item",
            format!("● {name} · {member_count} projects"),
        )
        .label_size(LabelSize::Small)
        .style(ButtonStyle::Subtle)
        .on_click(|_, window, cx| {
            window.dispatch_action(Box::new(OpenSolution), cx);
        })
        .into_any_element()
    }
}

impl StatusItemView for SolutionsStatusItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        cx.notify();
    }
}
