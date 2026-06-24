//! `GitGraphPanel` — the commit-log graph as a bottom-docked panel
//! (IDEA-style "Git" tool window) with a toggle button in the left rail.
//!
//! It hosts an inner [`GitGraph`] view for the workspace's active
//! repository and re-creates it when the active repo changes. The graph
//! is still also openable as a pane item (file-history / open-at-commit
//! flows use it that way) — this is purely an additional, dock-anchored
//! way to get at it.

use anyhow::Result;
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Pixels, Render, Styled, Subscription, WeakEntity, Window, actions,
    div, px,
};
use project::git_store::{GitStore, GitStoreEvent, RepositoryId};
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::GitGraph;

actions!(
    git_graph,
    [
        /// Toggles focus on the bottom-docked Git Graph panel.
        ToggleGraphPanel,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleGraphPanel, window, cx| {
            workspace.toggle_panel_focus::<GitGraphPanel>(window, cx);
        });
    })
    .detach();
}

pub struct GitGraphPanel {
    workspace: WeakEntity<Workspace>,
    git_store: Entity<GitStore>,
    graph: Option<Entity<GitGraph>>,
    active_repo_id: Option<RepositoryId>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl GitGraphPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let git_store = workspace.project().read(cx).git_store().clone();
            let weak = workspace.weak_handle();
            cx.new(|cx| Self::new(weak, git_store, window, cx))
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        git_store: Entity<GitStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let subscriptions =
            vec![
                cx.subscribe_in(&git_store, window, |this, _git_store, event, window, cx| {
                    if let GitStoreEvent::ActiveRepositoryChanged(repo_id) = event {
                        this.set_active_repo(*repo_id, window, cx);
                    }
                }),
            ];
        let active = git_store
            .read(cx)
            .active_repository()
            .map(|r| r.read(cx).id);
        let mut this = Self {
            workspace,
            git_store,
            graph: None,
            active_repo_id: None,
            focus_handle,
            _subscriptions: subscriptions,
        };
        this.set_active_repo(active, window, cx);
        this
    }

    fn set_active_repo(
        &mut self,
        repo_id: Option<RepositoryId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_repo_id == repo_id {
            return;
        }
        self.active_repo_id = repo_id;
        self.graph = repo_id.map(|id| {
            let git_store = self.git_store.clone();
            let workspace = self.workspace.clone();
            cx.new(|cx| GitGraph::new(id, git_store, workspace, None, window, cx))
        });
        cx.notify();
    }
}

impl Focusable for GitGraphPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.graph
            .as_ref()
            .map(|graph| graph.focus_handle(cx))
            .unwrap_or_else(|| self.focus_handle.clone())
    }
}

impl EventEmitter<PanelEvent> for GitGraphPanel {}

impl Render for GitGraphPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match &self.graph {
            Some(graph) => div().size_full().child(graph.clone()).into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(Label::new("No active repository").color(Color::Muted))
                .into_any_element(),
        }
    }
}

impl Panel for GitGraphPanel {
    fn persistent_name() -> &'static str {
        "GitGraphPanel"
    }

    fn panel_key() -> &'static str {
        "GitGraphPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {
        // Not persisted — the graph panel defaults to the bottom dock.
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::GitGraph)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Git Graph")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleGraphPanel)
    }

    fn activation_priority(&self) -> u32 {
        // Must be unique across all panels (the dock asserts it). 1=project,
        // 2=terminal, 3=git, 6=outline, 7=debug, 0=agent — 4 sits next to git.
        4
    }
}
