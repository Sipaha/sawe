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
use solution_agent::solution_band::SolutionBand;
use ui::prelude::*;
use workspace::{UtilityKind, Workspace};

use crate::{GitGraph, ToggleFocus};

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
        // Re-pointing the graph drops the `Entity<GitGraph>` that owns the
        // focused handle, so that handle leaves the dispatch tree: the
        // window's focus points at a dead id, `render`'s `is_focused` guard
        // is false so the redirect cannot fire, and `Workspace`'s focus-lost
        // listener then yanks focus out to the centre pane. The user's next
        // arrow key would scroll a buffer, and `contains_focused` would be
        // false, costing the tri-state an extra press. Re-home onto the
        // panel's own handle rather than the new graph's so `render` stays
        // the single place that decides what inside the panel holds focus
        // (with no repository there is nothing to redirect into, and focus
        // deliberately rests on the tracked container). Same shape, same
        // reason as `console_panel::ConsolePanel::close_tab`.
        let held_focus = self.focus_handle.contains_focused(window, cx);
        self.active_repo_id = repo_id;
        self.graph = repo_id.map(|id| {
            let git_store = self.git_store.clone();
            let workspace = self.workspace.clone();
            cx.new(|cx| GitGraph::new(id, git_store, workspace, None, window, cx))
        });
        if held_focus {
            self.focus_handle.focus(window, cx);
        }
        cx.notify();
    }
}

impl Focusable for GitGraphPanel {
    /// The panel's own handle, never the inner graph's — even though the
    /// graph is the only thing in here that handles keys. `Focusable` is what
    /// `SolutionBand::toggle_utility_focus`'s tri-state asks
    /// `contains_focused` on, and that predicate is only true of an ANCESTOR
    /// of the focused handle: handing out the graph's handle would make the
    /// "visible but unfocused" and "visible and focused" legs
    /// indistinguishable, and would answer `false` outright whenever there is
    /// no repository and hence no graph to hand out. `render` redirects focus
    /// that stops here down into the graph instead — the same split
    /// `ConsolePanel` uses, for the same reason.
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitGraphPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Focus that stopped on this container is a mis-aimed focus: the
        // container carries no key context, so the keystroke after
        // `ctrl-alt-\`` would go nowhere. Hand it down to the graph, which
        // does. Done here rather than from a `cx.on_focus` subscription
        // because `ctrl-alt-\`` on a hidden section shows the panel and
        // focuses it within one effect cycle, so a subscription installed on
        // the first render would miss exactly the frame that matters
        // (`ConsolePanel::focus_active_terminal` records the full argument).
        // With no repository there is nothing to redirect to and focus rests
        // on the tracked container, which keeps `contains_focused` true so
        // the tri-state can still hide the section.
        if self.focus_handle.is_focused(window)
            && let Some(graph) = self.graph.as_ref()
        {
            graph.focus_handle(cx).focus(window, cx);
        }

        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(match &self.graph {
                Some(graph) => graph.clone().into_any_element(),
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Label::new("No active repository").color(Color::Muted))
                    .into_any_element(),
            })
    }
}

/// Resolve the concrete `Entity<GitGraphPanel>` from the type-erased
/// `Workspace::solution_band_utility_item` slot `zed.rs` installs at startup.
/// `None` means either the panel-init task (`initialize_panels`) hasn't
/// finished yet, or this workspace has no Solution band at all (headless /
/// test workspaces that skip it). Mirrors
/// `console_panel::console_panel_for_workspace` and
/// `debugger_ui::debugger_panel::debug_panel_for_workspace`, including taking
/// `&Workspace` rather than the entity: callers already hold the workspace
/// leased, and re-reading the entity there is GPUI's double-lease panic.
pub(crate) fn git_graph_panel_for_workspace(
    workspace: &Workspace,
) -> Option<Entity<GitGraphPanel>> {
    workspace
        .solution_band_utility_item(UtilityKind::GitGraph)?
        .downcast::<GitGraphPanel>()
        .ok()
}

fn solution_band(workspace: &Workspace) -> Option<Entity<SolutionBand>> {
    workspace
        .solution_band_item()
        .and_then(|item| item.downcast::<SolutionBand>().ok())
}

/// `git_graph::ToggleFocus`'s (`ctrl-alt-\``) handler. The graph is not a
/// dock panel (see the module doc), so this cannot go through
/// `Workspace::toggle_panel_focus`; it drives the Solution band's utility
/// section instead, exactly as `console_panel::handle_toggle_focus` and
/// `debugger_ui::debugger_panel::handle_toggle_focus` do for their own
/// occupants. A no-op if either the panel or the band hasn't been installed.
///
/// The "showing another kind" arm is not optional. `utility_kind` is
/// persisted per Solution, so without selecting `GitGraph` here a user who
/// last used the terminal would reopen the band **on the terminal** while
/// focus went to an unrendered graph, leaving the graph unreachable by its
/// own keybinding for the rest of that Solution's life.
pub(crate) fn handle_toggle_focus(
    workspace: &mut Workspace,
    _: &ToggleFocus,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = git_graph_panel_for_workspace(workspace) else {
        return;
    };
    let Some(band) = solution_band(workspace) else {
        return;
    };
    let focus_handle = panel.focus_handle(cx);
    band.update(cx, |band, cx| {
        if band.utility_kind(cx) != UtilityKind::GitGraph {
            band.set_utility_kind(UtilityKind::GitGraph, cx);
            band.set_utility_visible(true, cx);
            focus_handle.focus(window, cx);
        } else {
            band.toggle_utility_focus(&focus_handle, window, cx);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Action as _, TestAppContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use solution_agent::adapter::AdapterRegistry;
    use solution_agent::store::SolutionAgentStore;

    /// The status-bar button's tooltip looks its hotkey up by name
    /// (`utility_buttons::toggle_action_name`), so a rename here silently
    /// downgrades that tooltip to a keybinding-less one — the lookup logs and
    /// falls back rather than failing. `console_panel` and `debugger_ui` pin
    /// their own entry the same way.
    #[test]
    fn toggle_focus_action_matches_the_utility_button_tooltip_lookup() {
        assert_eq!(
            solution_agent::utility_buttons::toggle_action_name(UtilityKind::GitGraph),
            Some(ToggleFocus.name())
        );
    }

    /// Nothing in-tree resolves the shipped keymaps against the registered
    /// action set, which is how four dead `git_graph::*` bindings survived
    /// until `18eaec348f` deleted them. This is deliberately not that
    /// resolution test — it cannot see an unregistered action or a wrong
    /// context — but it does pin the exact name all three platform keymaps
    /// spell against the action's real one, so a rename cannot silently turn
    /// `ctrl-alt-\`` into a binding that is dropped at load, and cannot be
    /// applied to one keymap while the other two are forgotten. Matching a
    /// whole line rather than a formatted substring keeps it robust to
    /// re-indentation while still pinning chord and action *together*.
    #[test]
    fn every_platform_keymap_binds_the_chord_to_the_real_action_name() {
        let action_name = ToggleFocus.name();
        for (platform, keymap) in [
            (
                "linux",
                include_str!("../../../assets/keymaps/default-linux.json"),
            ),
            (
                "macos",
                include_str!("../../../assets/keymaps/default-macos.json"),
            ),
            (
                "windows",
                include_str!("../../../assets/keymaps/default-windows.json"),
            ),
        ] {
            assert!(
                keymap
                    .lines()
                    .any(|line| { line.contains("\"ctrl-alt-`\"") && line.contains(action_name) }),
                "default-{platform}.json must bind ctrl-alt-` to {action_name} \
                 on one line; a rename that misses a keymap leaves a binding \
                 that resolves to nothing"
            );
        }
    }

    /// The whole tri-state, on a workspace with **no** repository — the case
    /// that used to be unreachable. The panel hands out a handle that only
    /// `render`'s `track_focus` puts in the dispatch tree, so before that
    /// existed the first press focused a handle no frame contained,
    /// `Workspace`'s focus-lost listener yanked focus to the centre pane, and
    /// `contains_focused` stayed false forever: every later press re-showed
    /// the section instead of hiding it.
    #[gpui::test]
    async fn toggle_focus_shows_focuses_then_hides(cx: &mut TestAppContext) {
        let (window, panel, band) = bootstrap(cx).await;

        assert!(
            !band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "precondition: the utility section starts hidden"
        );

        toggle(&window, cx);
        assert_eq!(
            band.read_with(cx, |band, cx| band.utility_kind(cx)),
            UtilityKind::GitGraph
        );
        assert!(band.read_with(cx, |band, cx| band.utility_visible(cx)));
        window
            .update(cx, |_workspace, window, cx| {
                assert!(
                    panel.focus_handle(cx).contains_focused(window, cx),
                    "the first press must leave focus inside the graph panel, \
                     or the tri-state loses its 'visible and focused' leg"
                );
            })
            .expect("window is open");

        toggle(&window, cx);
        assert!(
            !band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "a second press on the focused graph hides the section"
        );
        assert_eq!(
            band.read_with(cx, |band, cx| band.utility_kind(cx)),
            UtilityKind::GitGraph,
            "hiding must not rewrite the remembered kind"
        );
    }

    /// The section already open on another occupant is not "visible" as far
    /// as the graph is concerned: the press has to switch the kind, not fall
    /// into the band's tri-state and hide the terminal.
    #[gpui::test]
    async fn toggle_focus_switches_from_another_kind(cx: &mut TestAppContext) {
        let (window, panel, band) = bootstrap(cx).await;
        band.update(cx, |band, cx| {
            band.set_utility_kind(UtilityKind::Terminal, cx);
            band.set_utility_visible(true, cx);
        });

        toggle(&window, cx);

        assert_eq!(
            band.read_with(cx, |band, cx| band.utility_kind(cx)),
            UtilityKind::GitGraph
        );
        assert!(
            band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "switching occupants must leave the section open, not toggle it shut"
        );
        window
            .update(cx, |_workspace, window, cx| {
                assert!(panel.focus_handle(cx).contains_focused(window, cx));
            })
            .expect("window is open");
    }

    /// `ctrl-alt-\`` has to leave focus in the GRAPH, not on the panel
    /// container around it — the container handles no keys, so every
    /// keystroke after the hotkey would go nowhere. The two tri-state tests
    /// above cannot see this: they bootstrap without a repository, so
    /// `self.graph` is `None`, `render`'s redirect early-returns, and their
    /// `contains_focused` assertions are satisfied by
    /// `DispatchTree::focus_contains`'s `parent == child` short-circuit
    /// rather than by any panel→graph ancestry.
    ///
    /// The `is_action_available` assertion is the one that speaks about
    /// keystrokes rather than focus bookkeeping: it walks only the dispatch
    /// path to the focused node in the RENDERED frame, and
    /// `git_graph::OpenCommitView` is registered on `GitGraph`'s own element
    /// and nowhere else in this workspace, so its reachability means a key
    /// event dispatched now really traverses the graph. With focus resting
    /// on the panel root that path stops above the graph and the action is
    /// unreachable.
    #[gpui::test]
    async fn toggle_focus_lands_in_the_graph(cx: &mut TestAppContext) {
        let (window, panel, band) = bootstrap_with_repositories(cx, 1).await;
        let graph = panel
            .read_with(cx, |panel, _cx| panel.graph.clone())
            .expect("a worktree with a .git resolves one repository");

        toggle(&window, cx);

        window
            .update(cx, |_workspace, window, cx| {
                assert!(
                    graph.focus_handle(cx).is_focused(window),
                    "ctrl-alt-` must focus the graph itself, not the panel around it"
                );
                assert!(
                    panel.focus_handle(cx).contains_focused(window, cx),
                    "and the panel must still count as focused, or the band's \
                     tri-state loses its 'visible and focused' leg"
                );
                assert!(
                    window.is_action_available(&crate::OpenCommitView, cx),
                    "a keystroke dispatched now must travel through the graph's \
                     element"
                );
            })
            .expect("window is open");

        toggle(&window, cx);
        assert!(
            !band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "a second press on the focused graph still hides the section"
        );
    }

    /// Re-pointing the panel at another repository while it holds focus must
    /// not eject focus. The old `Entity<GitGraph>` is dropped, so its handle
    /// leaves the dispatch tree; without the re-home in `set_active_repo` the
    /// window's focus points at a dead id, `Workspace`'s focus-lost listener
    /// yanks focus to the centre pane, and the tri-state — which keys on
    /// `contains_focused` — silently costs an extra press.
    ///
    /// The realistic trigger is a two-member Solution: press ctrl-alt-`,
    /// arrow through commits, switch the active member
    /// (`SolutionStoreEvent::ActiveMemberChanged` → `refresh_active_repo`),
    /// and the next arrow key scrolls a buffer instead of the graph.
    #[gpui::test]
    async fn switching_repository_keeps_focus_in_the_graph(cx: &mut TestAppContext) {
        let (window, panel, _band) = bootstrap_with_repositories(cx, 2).await;

        toggle(&window, cx);
        let first_graph = panel
            .read_with(cx, |panel, _cx| panel.graph.clone())
            .expect("two worktrees with a .git resolve a repository apiece");
        window
            .update(cx, |_workspace, window, cx| {
                assert!(
                    first_graph.focus_handle(cx).is_focused(window),
                    "precondition: the hotkey put focus in the first graph"
                );
            })
            .expect("window is open");

        let other_repo_id = panel.read_with(cx, |panel, cx| {
            panel
                .git_store
                .read(cx)
                .repositories()
                .keys()
                .copied()
                .find(|id| Some(*id) != panel.active_repo_id)
                .expect("the second worktree's repository")
        });
        window
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.set_active_repo(Some(other_repo_id), window, cx)
                });
            })
            .expect("window is open");
        cx.run_until_parked();

        let second_graph = panel
            .read_with(cx, |panel, _cx| panel.graph.clone())
            .expect("the panel re-pointed at the other repository");
        assert_ne!(
            second_graph.entity_id(),
            first_graph.entity_id(),
            "precondition: switching repository re-creates the inner graph"
        );
        window
            .update(cx, |workspace, window, cx| {
                assert!(
                    second_graph.focus_handle(cx).is_focused(window),
                    "focus must follow the graph across the switch"
                );
                assert!(
                    !workspace.active_pane().focus_handle(cx).is_focused(window),
                    "and must not have been ejected to the centre pane, where the \
                     next arrow key would scroll a buffer"
                );
                assert!(
                    panel.focus_handle(cx).contains_focused(window, cx),
                    "or the next ctrl-alt-` focuses the graph instead of hiding it"
                );
            })
            .expect("window is open");
    }

    fn toggle(window: &gpui::WindowHandle<Workspace>, cx: &mut TestAppContext) {
        window
            .update(cx, |workspace, window, cx| {
                handle_toggle_focus(workspace, &ToggleFocus, window, cx);
            })
            .expect("window is open");
        cx.run_until_parked();
    }

    async fn bootstrap(
        cx: &mut TestAppContext,
    ) -> (
        gpui::WindowHandle<Workspace>,
        Entity<GitGraphPanel>,
        Entity<SolutionBand>,
    ) {
        bootstrap_with_repositories(cx, 0).await
    }

    /// One worktree per requested repository, each carrying a `.git` so the
    /// project resolves a `Repository` for it. `0` gives a single plain
    /// worktree and hence no repository at all — the state in which
    /// `GitGraphPanel::graph` stays `None` and `render`'s redirect has
    /// nothing to hand focus to.
    async fn bootstrap_with_repositories(
        cx: &mut TestAppContext,
        repository_count: usize,
    ) -> (
        gpui::WindowHandle<Workspace>,
        Entity<GitGraphPanel>,
        Entity<SolutionBand>,
    ) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            SolutionAgentStore::init_global(cx, std::sync::Arc::new(AdapterRegistry::new()));
        });

        let fs = FakeFs::new(cx.executor());
        let mut roots = Vec::new();
        if repository_count == 0 {
            fs.insert_tree("/root", serde_json::json!({})).await;
            roots.push(std::path::PathBuf::from("/root"));
        } else {
            for index in 0..repository_count {
                let root = std::path::PathBuf::from(format!("/repo-{index}"));
                fs.insert_tree(
                    &root,
                    serde_json::json!({".git": {}, "file.txt": "content"}),
                )
                .await;
                roots.push(root);
            }
        }
        let root_paths = roots
            .iter()
            .map(|root| root.as_path())
            .collect::<Vec<&std::path::Path>>();
        let project = Project::test(fs, root_paths, cx).await;
        cx.run_until_parked();

        let window = cx.add_window(|window, cx| Workspace::test_new(project, window, cx));
        let (panel, band) = window
            .update(cx, |workspace, window, cx| {
                let panel = cx.new(|cx| {
                    GitGraphPanel::new(
                        workspace.weak_handle(),
                        workspace.project().read(cx).git_store().clone(),
                        window,
                        cx,
                    )
                });
                let band = cx.new(|cx| {
                    SolutionBand::new(workspace.weak_handle(), workspace.project().clone(), cx)
                });
                workspace.set_solution_band_item(band.clone().into(), window, cx);
                workspace.set_solution_band_utility_item(
                    UtilityKind::GitGraph,
                    panel.clone().into(),
                    window,
                    cx,
                );
                (panel, band)
            })
            .expect("window is open");
        cx.run_until_parked();
        (window, panel, band)
    }
}
