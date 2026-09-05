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
    Render, Styled, Subscription, Task, WeakEntity, Window, div,
};
use project::git_store::{GitStore, GitStoreEvent, RepositoryId};
use solution_agent::solution_band::SolutionBand;
use std::time::Duration;
use ui::prelude::*;
use workspace::{UtilityKind, Workspace};

use crate::{GitGraph, GraphViewEvent, ToggleFocus};

/// How long the panel goes on painting the previous project's graph while the
/// incoming one's `git log` is still in flight.
///
/// Measured on this fork's own repository (79 531 commits reachable from all
/// refs) with a cold log cache: ~150 ms from the active-member change to the
/// first painted rows, of which the first ~145 ms had nothing to draw; a
/// 30-commit repository takes ~40 ms. So a normal switch never reaches this
/// cap — it exists because an *unbounded* hold would leave the wrong
/// project's history on screen indefinitely whenever the log never resolves
/// (a `git log` that hangs on a network filesystem, a repository that goes
/// away mid-switch). A stale log that looks current is worse than the blank
/// frame this whole mechanism removes, so past the cap the panel shows the
/// incoming graph's own honest "Loading" state instead.
const STALE_GRAPH_HOLD: Duration = Duration::from_millis(400);

/// A graph built for the repository the user has just switched to, kept off
/// screen while its `git log` is still in flight so the previous project's
/// rows stay painted instead of blanking out. Promoted by
/// [`GitGraphPanel::promote_pending`].
struct PendingGraph {
    repo_id: RepositoryId,
    graph: Entity<GitGraph>,
    /// Fires when the incoming log settles ([`GraphViewEvent::LoadSettled`]).
    _subscription: Subscription,
    /// Promotes the graph anyway once [`STALE_GRAPH_HOLD`] expires.
    _hold_expiry: Task<()>,
}

pub struct GitGraphPanel {
    workspace: WeakEntity<Workspace>,
    git_store: Entity<GitStore>,
    graph: Option<Entity<GitGraph>>,
    active_repo_id: Option<RepositoryId>,
    pending: Option<PendingGraph>,
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
            pending: None,
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

    /// Re-point the panel at `repo_id`.
    ///
    /// The incoming [`GitGraph`] starts its `git log` the moment it is
    /// constructed but is NOT shown until that log can paint something: until
    /// then the previous project's graph stays on screen. Swapping straight
    /// away is what made a project switch flash an empty panel — the new
    /// graph has no rows for as long as the log takes (~40 ms on a small
    /// repository, ~150 ms on a 79 531-commit one, several painted frames
    /// either way), so the user saw the old history disappear, a "Loading"
    /// placeholder, and only then the new history.
    fn set_active_repo(
        &mut self,
        repo_id: Option<RepositoryId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pending) = self.pending.as_ref()
            && Some(pending.repo_id) == repo_id
        {
            return;
        }
        if self.active_repo_id == repo_id {
            // Switched back to what is still on screen before the incoming log
            // resolved: drop the wait rather than promote a graph the user has
            // just navigated away from. What is on screen describes the
            // window's repository again, so it takes gestures again.
            if self.pending.take().is_some() {
                self.set_graph_selection_locked(false, cx);
                cx.notify();
            }
            return;
        }
        let Some(repo_id) = repo_id else {
            // No repository to load: "No active repository" is the final
            // answer, not an intermediate state worth holding the old graph
            // for.
            self.pending = None;
            self.install_graph(None, None, window, cx);
            return;
        };
        let graph = cx.new(|cx| {
            GitGraph::new(
                repo_id,
                self.git_store.clone(),
                self.workspace.clone(),
                None,
                window,
                cx,
            )
        });
        // Nothing on screen to protect, or the repository's log cache is
        // already warm (a switch back to a project visited earlier this
        // session): showing it now costs no blank frame, and waiting would
        // only delay it.
        if self.graph.is_none() || graph.read(cx).load_settled(cx) {
            self.pending = None;
            self.install_graph(Some(repo_id), Some(graph), window, cx);
            return;
        }
        let subscription = cx.subscribe_in(
            &graph,
            window,
            |this, _graph, _event: &GraphViewEvent, window, cx| {
                this.promote_pending(window, cx);
            },
        );
        let hold_expiry = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(STALE_GRAPH_HOLD).await;
            this.update_in(cx, |this, window, cx| this.promote_pending(window, cx))
                .ok();
        });
        // What stays painted for the length of the hold is a picture of a
        // repository the window has already left: the git panel beside it
        // describes the incoming one, so a click on these rows would point its
        // Commit tab at a commit from the project the user just navigated away
        // from. Keep it visible, but stop it acting.
        self.set_graph_selection_locked(true, cx);
        self.pending = Some(PendingGraph {
            repo_id,
            graph,
            _subscription: subscription,
            _hold_expiry: hold_expiry,
        });
    }

    fn set_graph_selection_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        if let Some(graph) = self.graph.as_ref() {
            graph.update(cx, |graph, _| graph.set_selection_locked(locked));
        }
    }

    /// Show the graph that has been waiting off screen. Called both when its
    /// log settles — including when it settles as an *error*, so a switch that
    /// fails replaces the old history with that error rather than going on
    /// showing another project's commits — and when [`STALE_GRAPH_HOLD`]
    /// expires.
    fn promote_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        self.install_graph(Some(pending.repo_id), Some(pending.graph), window, cx);
    }

    fn install_graph(
        &mut self,
        repo_id: Option<RepositoryId>,
        graph: Option<Entity<GitGraph>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        self.graph = graph;
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

    /// The intermediate frame is the whole point: a project switch used to
    /// blank the graph and only then fill it, so this drives the switch with
    /// the incoming `git log` deliberately held in flight and reads the
    /// PAINTED tree — asserting the predicate (`panel.active_repo_id`) would
    /// pass on exactly the behaviour being fixed, since the old code did
    /// re-point the panel correctly and merely painted nothing while doing it.
    ///
    /// All three sides are asserted, because each of them alone is satisfied
    /// by a broken panel: it must keep the previous rows while the log is in
    /// flight, it must swap to the new ones once the log lands, and it must
    /// not go on showing the previous project's history when the switch
    /// *fails*.
    #[gpui::test]
    async fn switching_projects_paints_the_old_graph_until_the_new_log_lands(
        cx: &mut TestAppContext,
    ) {
        let (window, panel, _band, fs) = bootstrap_with_logs(cx, &[1, 2]).await;
        toggle(&window, cx);
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        let first_row: &'static str = crate::commit_row_selector(0).leak();
        let second_row: &'static str = crate::commit_row_selector(1).leak();
        assert!(
            cx.debug_bounds(first_row).is_some(),
            "precondition: the first project's log is painted"
        );
        assert!(
            cx.debug_bounds(second_row).is_none(),
            "precondition: it is the ONE-commit repository, so a second row \
             on screen later can only have come from the other project"
        );

        // Hold the incoming `git log`, which is what a real switch spends its
        // blank on, and switch.
        fs.block_graph_load(std::path::Path::new("/repo-1/.git"));
        let second_repo = repository_other_than_active(&panel, cx);
        switch_to(&window, &panel, second_repo, cx);

        assert!(
            cx.debug_bounds(first_row).is_some(),
            "the previous project's rows must still be painted while the \
             incoming log is in flight — this is the blank the fix removes"
        );
        assert!(
            cx.debug_bounds(second_row).is_none(),
            "and they are the OLD rows: the incoming two-commit log has not \
             arrived yet"
        );
        assert!(
            cx.debug_bounds(crate::GRAPH_PLACEHOLDER_SELECTOR).is_none(),
            "so no 'Loading' placeholder is on screen either"
        );

        fs.release_graph_load(std::path::Path::new("/repo-1/.git"));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(second_row).is_some(),
            "once the log lands the panel must actually swap: holding the old \
             graph forever would be the worse bug"
        );
    }

    /// The failure side, split out because it needs a third repository: a
    /// switch whose `git log` errors must replace the previous project's
    /// history with the error, not keep painting commits from a project the
    /// user has navigated away from.
    #[gpui::test]
    async fn a_failed_switch_drops_the_previous_projects_rows(cx: &mut TestAppContext) {
        let (window, panel, _band, fs) = bootstrap_with_logs(cx, &[1, 2]).await;
        toggle(&window, cx);
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        let first_row: &'static str = crate::commit_row_selector(0).leak();
        assert!(
            cx.debug_bounds(first_row).is_some(),
            "precondition: the first project's log is painted"
        );

        let failing = std::path::Path::new("/repo-1/.git");
        fs.set_graph_error(failing, Some("fatal: bad default revision".into()));
        fs.block_graph_load(failing);
        let second_repo = repository_other_than_active(&panel, cx);
        switch_to(&window, &panel, second_repo, cx);
        assert!(
            cx.debug_bounds(first_row).is_some(),
            "precondition: the hold is in effect while the failing log runs"
        );

        fs.release_graph_load(failing);
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(first_row).is_none(),
            "a switch that errors must not leave the previous project's \
             commits on screen looking current"
        );
        assert!(
            cx.debug_bounds(crate::GRAPH_PLACEHOLDER_SELECTOR).is_some(),
            "the incoming graph's own error state is what replaces them"
        );
    }

    /// The hold is bounded. A `git log` that never resolves would otherwise
    /// leave another project's history on screen for the rest of the session,
    /// which is a worse lie than the blank frame the hold exists to remove.
    #[gpui::test]
    async fn a_log_that_never_resolves_gives_the_panel_back(cx: &mut TestAppContext) {
        let (window, panel, _band, fs) = bootstrap_with_logs(cx, &[1, 2]).await;
        toggle(&window, cx);
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        let first_row: &'static str = crate::commit_row_selector(0).leak();
        fs.block_graph_load(std::path::Path::new("/repo-1/.git"));
        let second_repo = repository_other_than_active(&panel, cx);
        switch_to(&window, &panel, second_repo, cx);
        assert!(
            cx.debug_bounds(first_row).is_some(),
            "precondition: the hold is in effect"
        );

        cx.executor()
            .advance_clock(STALE_GRAPH_HOLD + Duration::from_millis(1));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(first_row).is_none(),
            "past the cap the previous project's rows must be gone"
        );
        assert!(
            cx.debug_bounds(crate::GRAPH_PLACEHOLDER_SELECTOR).is_some(),
            "replaced by the incoming graph's own honest 'Loading' state"
        );
    }

    /// A switch back to a project whose log the repository has already cached
    /// must not wait on anything: there is no blank to protect against, and
    /// delaying it by even one event cycle would be a regression in the case
    /// that used to be instant.
    #[gpui::test]
    async fn a_warm_log_is_shown_without_waiting(cx: &mut TestAppContext) {
        let (window, panel, _band, _fs) = bootstrap_with_logs(cx, &[1, 2]).await;
        toggle(&window, cx);
        let second_repo = repository_other_than_active(&panel, cx);
        switch_to(&window, &panel, second_repo, cx);
        cx.run_until_parked();
        let first_repo = repository_other_than_active(&panel, cx);

        window
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.set_active_repo(Some(first_repo), window, cx);
                    assert!(
                        panel.pending.is_none(),
                        "a cached log has nothing to wait for"
                    );
                    assert_eq!(panel.active_repo_id, Some(first_repo));
                });
            })
            .expect("window is open");
    }

    /// Switching back to the project that is still on screen, before the
    /// incoming log resolves, must cancel the wait rather than promote a
    /// graph the user has navigated away from.
    #[gpui::test]
    async fn switching_back_mid_hold_cancels_the_pending_graph(cx: &mut TestAppContext) {
        let (window, panel, _band, fs) = bootstrap_with_logs(cx, &[1, 2]).await;
        toggle(&window, cx);
        fs.block_graph_load(std::path::Path::new("/repo-1/.git"));
        let first_repo = panel
            .read_with(cx, |panel, _cx| panel.active_repo_id)
            .expect("the bootstrap resolved a repository");
        let second_repo = repository_other_than_active(&panel, cx);
        switch_to(&window, &panel, second_repo, cx);

        window
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |panel, cx| {
                    assert!(panel.pending.is_some(), "precondition: the hold is on");
                    panel.set_active_repo(Some(first_repo), window, cx);
                    assert!(panel.pending.is_none(), "the wait is dropped");
                    assert_eq!(
                        panel.active_repo_id,
                        Some(first_repo),
                        "and the panel still tracks what it is painting"
                    );
                });
            })
            .expect("window is open");

        fs.release_graph_load(std::path::Path::new("/repo-1/.git"));
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _cx| panel.active_repo_id),
            Some(first_repo),
            "the abandoned log must not swap itself in when it finally lands"
        );
    }

    /// For the length of the hold the graph on screen describes a repository
    /// the window has already left, while the git panel next to it describes
    /// the incoming one. A click on those rows would point that panel's Commit
    /// tab at a commit from the project the user just navigated away from, so
    /// the held graph is a picture and not a surface until the switch resolves.
    #[gpui::test]
    async fn the_held_graph_stops_taking_selection_gestures(cx: &mut TestAppContext) {
        let (window, panel, _band, fs) = bootstrap_with_logs(cx, &[1, 2]).await;
        toggle(&window, cx);
        cx.run_until_parked();

        let held = panel
            .read_with(cx, |panel, _cx| panel.graph.clone())
            .expect("the bootstrap resolved a repository");
        assert!(
            !held.read_with(cx, |graph, _cx| graph.selection_locked_for_test()),
            "precondition: the graph on screen is the live one"
        );

        fs.block_graph_load(std::path::Path::new("/repo-1/.git"));
        let first_repo = panel
            .read_with(cx, |panel, _cx| panel.active_repo_id)
            .expect("the bootstrap resolved a repository");
        let second_repo = repository_other_than_active(&panel, cx);
        switch_to(&window, &panel, second_repo, cx);

        assert!(
            held.read_with(cx, |graph, _cx| graph.selection_locked_for_test()),
            "the previous project's rows are still painted, but they are stale"
        );

        // Switching back mid-hold makes them current again.
        window
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.set_active_repo(Some(first_repo), window, cx)
                });
            })
            .expect("window is open");
        assert!(
            !held.read_with(cx, |graph, _cx| graph.selection_locked_for_test()),
            "abandoning the switch must give the graph back its gestures"
        );

        fs.release_graph_load(std::path::Path::new("/repo-1/.git"));
        cx.run_until_parked();
    }

    /// The repository the panel is NOT currently pointed at.
    fn repository_other_than_active(
        panel: &Entity<GitGraphPanel>,
        cx: &mut TestAppContext,
    ) -> RepositoryId {
        panel.read_with(cx, |panel, cx| {
            panel
                .git_store
                .read(cx)
                .repositories()
                .keys()
                .copied()
                .find(|id| Some(*id) != panel.active_repo_id)
                .expect("the fixture has a second repository")
        })
    }

    /// Drive the switch the way `SolutionStoreEvent::ActiveMemberChanged`
    /// does, then let the panel react — but WITHOUT `run_until_parked`, which
    /// would also run the held `git log` to completion and skip past the
    /// frame under test.
    fn switch_to(
        window: &gpui::WindowHandle<Workspace>,
        panel: &Entity<GitGraphPanel>,
        repo_id: RepositoryId,
        cx: &mut TestAppContext,
    ) {
        window
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.set_active_repo(Some(repo_id), window, cx)
                });
            })
            .expect("window is open");
        // NOT `run_until_parked`'s job here: the held `git log` is parked on
        // its gate, so this only flushes effects and paints the frame the
        // assertions read.
        cx.run_until_parked();
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
        let (window, panel, band, _fs) = bootstrap_with_logs(cx, &vec![0; repository_count]).await;
        (window, panel, band)
    }

    /// As [`bootstrap_with_repositories`], but each repository's `git log`
    /// answers with the given number of commits, and the fake filesystem is
    /// handed back so a test can hold or fail one of those logs.
    async fn bootstrap_with_logs(
        cx: &mut TestAppContext,
        commit_counts: &[usize],
    ) -> (
        gpui::WindowHandle<Workspace>,
        Entity<GitGraphPanel>,
        Entity<SolutionBand>,
        std::sync::Arc<FakeFs>,
    ) {
        let repository_count = commit_counts.len();
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
        for (index, commit_count) in commit_counts.iter().enumerate() {
            fs.set_graph_commits(
                &std::path::PathBuf::from(format!("/repo-{index}/.git")),
                commit_chain(index, *commit_count),
            );
        }
        let root_paths = roots
            .iter()
            .map(|root| root.as_path())
            .collect::<Vec<&std::path::Path>>();
        let project = Project::test(fs.clone(), root_paths, cx).await;
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
        (window, panel, band, fs)
    }

    /// A linear history of `len` commits whose shas are unique to
    /// `repo_index`, so a row painted from one repository's log can never be
    /// mistaken for the other's.
    fn commit_chain(
        repo_index: usize,
        len: usize,
    ) -> Vec<std::sync::Arc<git::repository::InitialGraphCommitData>> {
        let sha = |commit_index: usize| {
            let mut bytes = [0u8; 20];
            bytes[0] = repo_index as u8 + 1;
            bytes[1] = commit_index as u8 + 1;
            git::Oid::from_bytes(&bytes).expect("20 bytes is a valid oid")
        };
        (0..len)
            .map(|commit_index| {
                std::sync::Arc::new(git::repository::InitialGraphCommitData {
                    sha: sha(commit_index),
                    parents: if commit_index + 1 < len {
                        smallvec::smallvec![sha(commit_index + 1)]
                    } else {
                        smallvec::smallvec![]
                    },
                    ref_names: Vec::new(),
                })
            })
            .collect()
    }
}
