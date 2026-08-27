//! `GitGraphPanel` — the commit-log graph as an occupant of the Solution
//! band's utility section (phase 2b task 4), NOT a dock panel: it keeps
//! `Render`/`Focusable` but has no `Panel` impl, exactly like
//! `console_panel::ConsolePanel` (the worked example). `zed.rs` installs it
//! into `Workspace::solution_band_utility_item` under
//! `UtilityKind::GitGraph`; `SolutionBand::render` reads that slot for
//! whichever kind the Solution's persisted `utility_kind` names.
//!
//! It hosts an inner [`GitGraph`] view for the workspace's active
//! repository and re-creates it when the active repo changes. The graph is
//! independently openable as a pane item (`git_ui::git_panel::Open`,
//! `git::FileHistory` → `open_or_reuse_graph` in `git_graph.rs`); that path
//! never went through this type and is unaffected.

use anyhow::Result;
use gpui::{
    App, AsyncWindowContext, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement,
    Render, Styled, Subscription, WeakEntity, Window, div,
};
use project::git_store::{GitStore, GitStoreEvent, RepositoryId};
use ui::prelude::*;
use workspace::Workspace;

use crate::GitGraph;

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
        let mut subscriptions =
            vec![
                cx.subscribe_in(&git_store, window, |this, _git_store, event, window, cx| {
                    if let GitStoreEvent::ActiveRepositoryChanged(_) = event {
                        this.refresh_active_repo(window, cx);
                    }
                }),
            ];
        // In a multi-member Solution all members share ONE `Project`, so
        // `ActiveRepositoryChanged` alone tracks the last-focused editor's
        // repo — NOT the project the user selected in the tab strip. Re-target
        // the graph when the active member flips too (mirrors git_panel /
        // title-bar / branch-picker, which all scope to the active member).
        if let Some(store) = solutions::SolutionStore::try_global(cx) {
            subscriptions.push(cx.subscribe_in(
                &store,
                window,
                |this, _store, event, window, cx| {
                    if matches!(
                        event,
                        solutions::SolutionStoreEvent::ActiveMemberChanged { .. }
                    ) {
                        this.refresh_active_repo(window, cx);
                    }
                },
            ));
        }
        let this = Self {
            workspace,
            git_store,
            graph: None,
            active_repo_id: None,
            focus_handle,
            _subscriptions: subscriptions,
        };
        // The initial resolve must run AFTER this constructor returns. `new`
        // executes inside the `workspace.update_in` that creates the panel, so
        // the Workspace entity is still mutably leased — and `resolve_active_repo_id`
        // reads it (`self.workspace.upgrade()?.read(cx)`), which would
        // double-lease-panic. Defer to the next effect cycle, once the
        // construction lease is released. (Subscription-driven refreshes already
        // run outside any Workspace update, so they don't need this.)
        cx.defer_in(window, |this, window, cx| {
            this.refresh_active_repo(window, cx)
        });
        this
    }

    /// Recompute which repository the graph should track and re-point the
    /// inner [`GitGraph`] if it changed. Prefers the active Solution member's
    /// repo; falls back to the project's `active_repository` outside a
    /// Solution.
    fn refresh_active_repo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repo_id = self.resolve_active_repo_id(cx);
        self.set_active_repo(repo_id, window, cx);
    }

    fn resolve_active_repo_id(&self, cx: &App) -> Option<RepositoryId> {
        let project = self.workspace.upgrade()?.read(cx).project().clone();
        let repo = solutions::active_member_repository(&project, cx)
            .or_else(|| project.read(cx).active_repository(cx))?;
        Some(repo.read(cx).id)
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
