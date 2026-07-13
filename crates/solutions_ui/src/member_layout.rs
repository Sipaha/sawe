use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{Context, Task, Window};
use solutions::{MemberId, SolutionId, SolutionStoreEvent};
use workspace::{DockStructure, OpenOptions, OpenVisible, SaveIntent, Workspace};

type MemberKey = (SolutionId, MemberId);

#[derive(Clone, Default)]
struct MemberLayout {
    open_paths: Vec<PathBuf>,
    active_path: Option<PathBuf>,
    docks: Option<DockStructure>,
}

#[derive(Default)]
pub(crate) struct MemberLayoutState {
    current: Option<MemberKey>,
    layouts: HashMap<MemberKey, MemberLayout>,
    apply_task: Option<Task<()>>,
}

struct ItemReconcilePlan {
    /// Currently-open file paths not wanted by the target member.
    to_close: Vec<PathBuf>,
    /// Target paths not currently open, in target order.
    to_open: Vec<PathBuf>,
}

/// Diff the currently-open file paths against the target member's open paths.
/// Overlapping paths appear in neither list (left untouched — no close/reopen
/// churn). If the two sets are identical, both lists are empty (zero churn).
fn plan_item_reconcile(current: &[PathBuf], target: &[PathBuf]) -> ItemReconcilePlan {
    let target_set: std::collections::HashSet<&PathBuf> = target.iter().collect();
    let current_set: std::collections::HashSet<&PathBuf> = current.iter().collect();
    ItemReconcilePlan {
        to_close: current
            .iter()
            .filter(|p| !target_set.contains(*p))
            .cloned()
            .collect(),
        to_open: target
            .iter()
            .filter(|p| !current_set.contains(*p))
            .cloned()
            .collect(),
    }
}

/// Snapshot the outgoing active member's layout and apply the incoming
/// member's, if it has one. `member == None` means the solution lost its
/// last member: clear tracking without applying. First visit to a member
/// (no stored snapshot) leaves the current layout intact.
pub(crate) fn apply_active_member_change(
    state: &Rc<RefCell<MemberLayoutState>>,
    workspace: &mut Workspace,
    solution: SolutionId,
    member: Option<MemberId>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // Reset any stale reveal suppression from a prior swap whose task was
    // cancelled by a rapid re-switch (the task clears it on completion, but a
    // cancelled one never got there). Every entry starts clean; the task below
    // re-arms it around its own close/open batch.
    workspace.set_active_entry_reveal_suppressed(false);

    // 1. Snapshot the outgoing member from the live workspace.
    {
        let mut st = state.borrow_mut();
        if let Some(prev) = st.current {
            // Only snapshot when the solution itself hasn't changed. An
            // in-place solution switch (switch::switch_active_solution_in_place)
            // swaps worktrees and replays the new solution's tabs on this
            // same Workspace WITHOUT firing ActiveMemberChanged, so `prev`
            // can point at a member of a solution that is no longer active.
            // In that case the live workspace content belongs to the new
            // solution, not to `prev` — snapshotting it would silently
            // overwrite `prev`'s saved layout with another solution's files.
            if prev.0 == solution {
                let active_path = workspace
                    .active_item(cx)
                    .and_then(|item| item.project_path(cx))
                    .and_then(|pp| workspace.project().read(cx).absolute_path(&pp, cx));
                let layout = MemberLayout {
                    open_paths: workspace.open_item_abs_paths(cx),
                    active_path,
                    docks: Some(workspace.capture_dock_state(window, cx)),
                };
                st.layouts.insert(prev, layout);
            }
        }
    }

    // 2. Resolve the incoming key. No member => stop tracking, no apply.
    let Some(member) = member else {
        let mut st = state.borrow_mut();
        st.current = None;
        st.apply_task = None;
        return;
    };
    let key = (solution, member);

    // 3. Apply the incoming member's snapshot (first visit => skip).
    let to_apply = {
        let mut st = state.borrow_mut();
        let layout = st.layouts.get(&key).cloned();
        st.current = Some(key);
        layout
    };
    let Some(layout) = to_apply else { return };

    if let Some(docks) = layout.docks.clone() {
        workspace.set_dock_structure(docks, window, cx);
    }

    // Suppress the project panel's per-tab reveal for the whole swap: closing
    // and reopening a batch of tabs re-points the active entry many times, and
    // revealing on each one scrolls the tree over and over (the jank the user
    // sees — "the tree jumps on every tab"). We drop the flag just before
    // re-activating the final file so the tree reveals exactly once, at the end.
    workspace.set_active_entry_reveal_suppressed(true);

    // Center pane: reconcile the currently-open editor items against the
    // target member's snapshot. Overlapping files are left untouched (no
    // close/reopen churn); only non-target files are closed and only the
    // missing target files are opened. Async (open/close are Tasks); stored in
    // `apply_task` so a rapid re-switch cancels this one.
    // Scope `current` to the ACTIVE pane — the same scope the close/open
    // enactment below operates on. `open_item_abs_paths` is workspace-wide (all
    // panes), which mis-plans under a split: a target file open only in a
    // non-active pane would count as "already open" and never get opened into
    // the active pane, and a stale file living only in a non-active pane would
    // never match `close_ids` (leaking across switches). Deriving the paths via
    // the SAME `project_path` → `absolute_path` chain as `close_ids` also keeps
    // the two path representations identical. Target (`layout.open_paths`) stays
    // workspace-wide, so `to_open` collapses the full snapshot into the active
    // pane exactly as the old close-all/reopen-all did.
    let current_paths: Vec<PathBuf> = {
        let project = workspace.project().clone();
        workspace
            .active_pane()
            .read(cx)
            .items()
            .filter_map(|item| {
                item.project_path(cx)
                    .and_then(|pp| project.read(cx).absolute_path(&pp, cx))
            })
            .collect()
    };
    let plan = plan_item_reconcile(&current_paths, &layout.open_paths);
    let task = cx.spawn_in(window, async move |workspace, cx| {
        // Close ONLY the outgoing tabs the target doesn't want, as ONE batch:
        // compute their item ids first, then `close_items` removes every
        // matching item in a single task (one repaint). Items with no abs path
        // (ProjectDiff, settings, welcome) are never matched, so they persist
        // across the swap instead of being torn down and rebuilt.
        let close = workspace.update_in(cx, |ws, window, cx| {
            let project = ws.project().clone();
            let pane = ws.active_pane().clone();
            let to_close: std::collections::HashSet<PathBuf> =
                plan.to_close.iter().cloned().collect();
            let close_ids: std::collections::HashSet<gpui::EntityId> = pane
                .read(cx)
                .items()
                .filter_map(|item| {
                    let abs = item
                        .project_path(cx)
                        .and_then(|pp| project.read(cx).absolute_path(&pp, cx));
                    abs.filter(|p| to_close.contains(p)).map(|_| item.item_id())
                })
                .collect();
            pane.update(cx, |pane, cx| {
                pane.close_items(window, cx, SaveIntent::Skip, &move |id| {
                    close_ids.contains(&id)
                })
            })
        });
        if let Ok(close) = close {
            let _ = close.await;
        }

        // Open ONLY the missing target tabs, as ONE batch. `open_paths` opens
        // the whole set in a single coordinated task and emits the project-panel
        // reveal exactly ONCE (for its "winner" path), instead of the old
        // per-path loop that fired a reveal per file — so the tree no longer
        // jumps/highlights one file at a time. `focus: false` keeps the restore
        // from stealing keyboard focus per file; the final active tab is focused
        // once below. Reveal is also suppressed for the whole batch, so the tree
        // does not scroll here.
        if !plan.to_open.is_empty() {
            let open = workspace.update_in(cx, |ws, window, cx| {
                let mut options = OpenOptions::default();
                options.visible = Some(OpenVisible::None);
                options.focus = Some(false);
                ws.open_paths(plan.to_open.clone(), options, None, window, cx)
            });
            if let Ok(open) = open {
                let _ = open.await;
            }
        }

        // Drop the suppression, then re-activate the previously-active file so
        // the tree scrolls to it exactly once. With no active file, just clear
        // the flag (nothing to reveal).
        let final_open = workspace.update_in(cx, |ws, window, cx| {
            ws.set_active_entry_reveal_suppressed(false);
            layout
                .active_path
                .clone()
                .map(|active| ws.open_abs_path(active, OpenOptions::default(), window, cx))
        });
        if let Ok(Some(open)) = final_open {
            let _ = open.await;
        }
    });
    state.borrow_mut().apply_task = Some(task);
}

/// One per `Workspace`: subscribe to `ActiveMemberChanged` and drive the
/// per-member layout swap. State lives in an `Rc<RefCell<..>>` captured by
/// the subscription closure, which is owned by the Workspace (so the state
/// and its in-flight apply task die with the window). No-op outside a
/// Solution (the event simply never fires for a plain project).
pub(crate) fn register_member_layout_controller(
    _workspace: &mut Workspace,
    window: Option<&mut Window>,
    cx: &mut Context<Workspace>,
) {
    let Some(window) = window else { return };
    let Some(store) = solutions::SolutionStore::try_global(cx) else {
        return;
    };
    let state = Rc::new(RefCell::new(MemberLayoutState::default()));
    cx.subscribe_in(&store, window, move |workspace, _store, event, window, cx| {
        if let SolutionStoreEvent::ActiveMemberChanged { solution, member } = event {
            apply_active_member_change(&state, workspace, *solution, *member, window, cx);
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{AppContext as _, TestAppContext};
    use project::Project;
    use serde_json::json;
    use solutions::{MemberId, SolutionId};
    use settings::{Settings as _, SettingsStore};
    use std::cell::RefCell;
    use std::rc::Rc;
    use theme::LoadThemes;
    use util::path;
    use workspace::dock::DockPosition;
    use workspace::dock::test::TestPanel;
    use workspace::{OpenOptions, Workspace};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(LoadThemes::JustBase, cx);
            project::project_settings::ProjectSettings::register(cx);
            project::WorktreeSettings::register(cx);
            workspace::WorkspaceSettings::register(cx);
            language::language_settings::AllLanguageSettings::register(cx);
            editor::init(cx);
        });
    }

    async fn open(workspace: &gpui::Entity<Workspace>, cx: &mut gpui::VisualTestContext, p: &str) {
        let task = workspace.update_in(cx, |ws, window, cx| {
            ws.open_abs_path(std::path::PathBuf::from(p), OpenOptions::default(), window, cx)
        });
        let _ = task.await;
        cx.run_until_parked();
    }

    fn switch(
        state: &Rc<RefCell<MemberLayoutState>>,
        workspace: &gpui::Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
        solution: i64,
        member: Option<i64>,
    ) {
        let (state, member) = (state.clone(), member.map(MemberId));
        workspace.update_in(cx, |ws, window, cx| {
            apply_active_member_change(&state, ws, SolutionId(solution), member, window, cx);
        });
        cx.run_until_parked();
    }

    fn open_paths(workspace: &gpui::Entity<Workspace>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
        workspace.update(cx, |ws, cx| {
            ws.open_item_abs_paths(cx)
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        })
    }

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn plan_item_reconcile_identical_sets_is_zero_churn() {
        let current = [pb("/a"), pb("/b"), pb("/c")];
        let target = [pb("/a"), pb("/b"), pb("/c")];
        let plan = plan_item_reconcile(&current, &target);
        assert!(plan.to_close.is_empty(), "{:?}", plan.to_close);
        assert!(plan.to_open.is_empty(), "{:?}", plan.to_open);
    }

    #[test]
    fn plan_item_reconcile_drop_one_add_one() {
        // current = {a, b}, target = {a, c}: b dropped, c added, a untouched.
        let current = [pb("/a"), pb("/b")];
        let target = [pb("/a"), pb("/c")];
        let plan = plan_item_reconcile(&current, &target);
        assert_eq!(plan.to_close, vec![pb("/b")]);
        assert_eq!(plan.to_open, vec![pb("/c")]);
    }

    #[test]
    fn plan_item_reconcile_disjoint_sets() {
        let current = [pb("/a"), pb("/b")];
        let target = [pb("/c"), pb("/d")];
        let plan = plan_item_reconcile(&current, &target);
        assert_eq!(plan.to_close, vec![pb("/a"), pb("/b")]);
        assert_eq!(plan.to_open, vec![pb("/c"), pb("/d")]);
    }

    #[test]
    fn plan_item_reconcile_to_open_preserves_target_order() {
        // None of the target files are currently open: to_open must mirror
        // target order exactly.
        let current: [PathBuf; 0] = [];
        let target = [pb("/z"), pb("/a"), pb("/m")];
        let plan = plan_item_reconcile(&current, &target);
        assert_eq!(plan.to_open, vec![pb("/z"), pb("/a"), pb("/m")]);
        assert!(plan.to_close.is_empty());
    }

    #[gpui::test]
    async fn member_switch_swaps_open_files(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/sol"), json!({ "a.txt": "", "b.txt": "" })).await;
        let project = Project::test(fs.clone(), [path!("/sol").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let state = Rc::new(RefCell::new(MemberLayoutState::default()));

        // First visit to member A: no apply (inherit current empty view).
        switch(&state, &workspace, cx, 1, Some(10));
        open(&workspace, cx, path!("/sol/a.txt")).await;

        // Switch to B (first visit): snapshots A={a.txt}, inherits a.txt open.
        switch(&state, &workspace, cx, 1, Some(20));
        // Establish B's own view: close a.txt, open b.txt.
        let a_id = workspace.update(cx, |ws, cx| {
            ws.items(cx).next().map(|i| i.item_id())
        });
        if let Some(id) = a_id {
            let t = workspace.update_in(cx, |ws, window, cx| {
                let pane = ws.active_pane().clone();
                pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(id, workspace::SaveIntent::Skip, window, cx)
                })
            });
            let _ = t.await;
        }
        open(&workspace, cx, path!("/sol/b.txt")).await;

        // Back to A: apply A={a.txt} → center shows only a.txt.
        switch(&state, &workspace, cx, 1, Some(10));
        let paths = open_paths(&workspace, cx);
        assert_eq!(paths.len(), 1, "A restores exactly its own tab: {paths:?}");
        assert!(paths[0].ends_with("a.txt"), "A restores a.txt: {paths:?}");

        // Back to B: apply B={b.txt}.
        switch(&state, &workspace, cx, 1, Some(20));
        let paths = open_paths(&workspace, cx);
        assert_eq!(paths.len(), 1, "B restores exactly its own tab: {paths:?}");
        assert!(paths[0].ends_with("b.txt"), "B restores b.txt: {paths:?}");
    }

    fn pane_abs_paths(
        pane: &gpui::Entity<workspace::pane::Pane>,
        workspace: &gpui::Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
    ) -> Vec<String> {
        workspace.update(cx, |ws, cx| {
            let project = ws.project().clone();
            pane.read(cx)
                .items()
                .filter_map(|item| {
                    item.project_path(cx)
                        .and_then(|pp| project.read(cx).absolute_path(&pp, cx))
                        .map(|p| p.to_string_lossy().into_owned())
                })
                .collect()
        })
    }

    /// Regression guard for the multi-pane scope of the reconcile plan.
    /// `plan_item_reconcile`'s `current` and the close/open enactment must both
    /// be scoped to the ACTIVE pane. If `current` were workspace-wide (all
    /// panes), a target file open only in a NON-active pane would count as
    /// "already open" and never get opened into the active pane, blanking it.
    ///
    /// Design note: the missing target file (x.txt) must NOT be the snapshot's
    /// *active* path — otherwise the final `active_path` re-open (which always
    /// targets the active pane) would open it regardless, masking the scope
    /// bug. Here the active file is z.txt (already in the active pane) and the
    /// file that must be *opened via `to_open`* is x.txt (present only in the
    /// non-active pane), so the assertion truly exercises the plan's scope.
    #[gpui::test]
    async fn member_switch_reconciles_active_pane_under_split(cx: &mut TestAppContext) {
        use workspace::SplitDirection;

        fn sorted(mut v: Vec<String>) -> Vec<String> {
            v.sort();
            v
        }

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/sol"),
            json!({ "x.txt": "", "y.txt": "", "z.txt": "" }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/sol").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let state = Rc::new(RefCell::new(MemberLayoutState::default()));

        // Member A, first visit: open x.txt then z.txt in the single pane, so
        // z.txt is the active item.
        switch(&state, &workspace, cx, 1, Some(10));
        open(&workspace, cx, path!("/sol/x.txt")).await;
        open(&workspace, cx, path!("/sol/z.txt")).await;
        let first_pane = workspace.update(cx, |ws, _| ws.active_pane().clone());

        // Switch to B: snapshots A workspace-wide = {x.txt, z.txt}, active=z.txt.
        switch(&state, &workspace, cx, 1, Some(20));

        // Build B's split layout:
        //   non-active pane  = {x.txt}
        //   active pane      = {y.txt, z.txt}, active item z.txt
        // so x.txt (a target file) lives ONLY in the non-active pane, and y.txt
        // (a non-target file) sits in the active pane.
        let second_pane = workspace.update_in(cx, |ws, window, cx| {
            let active = ws.active_pane().clone();
            ws.split_pane(active, SplitDirection::Right, window, cx)
        });
        cx.run_until_parked();
        // Remove z.txt from the (now non-active) first pane, leaving it {x.txt}.
        let z_in_first = first_pane.read_with(cx, |pane, cx| {
            pane.items().find_map(|item| {
                let abs = item
                    .project_path(cx)
                    .and_then(|pp| project.read(cx).absolute_path(&pp, cx))?;
                abs.to_string_lossy().ends_with("z.txt").then(|| item.item_id())
            })
        });
        if let Some(id) = z_in_first {
            let t = workspace.update_in(cx, |_ws, window, cx| {
                first_pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(id, workspace::SaveIntent::Skip, window, cx)
                })
            });
            let _ = t.await;
        }
        open(&workspace, cx, path!("/sol/y.txt")).await;
        open(&workspace, cx, path!("/sol/z.txt")).await;

        // Sanity on the constructed split.
        assert!(
            workspace.update(cx, |ws, _| ws.active_pane().entity_id() == second_pane.entity_id()),
            "the split's new pane must be active"
        );
        assert_eq!(
            pane_abs_paths(&first_pane, &workspace, cx),
            vec![path!("/sol/x.txt").to_string()],
            "non-active pane holds only x.txt before the switch"
        );
        assert_eq!(
            sorted(pane_abs_paths(&second_pane, &workspace, cx)),
            sorted(vec![path!("/sol/y.txt").to_string(), path!("/sol/z.txt").to_string()]),
            "active pane holds y.txt + z.txt before the switch"
        );

        // Back to A (target snapshot = {x.txt, z.txt}, active=z.txt). The
        // scope-correct plan, using ACTIVE-pane current = {y.txt, z.txt}, must
        // OPEN x.txt into the active pane (it lives only in the non-active pane)
        // and CLOSE y.txt there. A workspace-wide `current` would see x.txt as
        // "already open" and never open it — leaving the active pane without it.
        switch(&state, &workspace, cx, 1, Some(10));

        let active_paths = sorted(pane_abs_paths(
            &workspace.update(cx, |ws, _| ws.active_pane().clone()),
            &workspace,
            cx,
        ));
        assert_eq!(
            active_paths,
            sorted(vec![path!("/sol/x.txt").to_string(), path!("/sol/z.txt").to_string()]),
            "active pane must reconcile to {{x.txt, z.txt}}: x.txt opened despite \
             living only in a non-active pane, y.txt closed: {active_paths:?}"
        );

        // The non-active first pane is left as it was: still x.txt, untouched.
        assert_eq!(
            pane_abs_paths(&first_pane, &workspace, cx),
            vec![path!("/sol/x.txt").to_string()],
            "the non-active pane must be left untouched by the swap"
        );
    }

    /// The swap suppresses the project-panel reveal while it closes/reopens
    /// tabs; it MUST clear that suppression when done, or user-driven reveals
    /// stay broken. Guards against a stuck flag (e.g. a cancelled apply task).
    #[gpui::test]
    async fn swap_clears_reveal_suppression_when_done(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/sol"), json!({ "a.txt": "", "b.txt": "" })).await;
        let project = Project::test(fs.clone(), [path!("/sol").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let state = Rc::new(RefCell::new(MemberLayoutState::default()));

        switch(&state, &workspace, cx, 1, Some(10));
        open(&workspace, cx, path!("/sol/a.txt")).await;
        switch(&state, &workspace, cx, 1, Some(20));
        open(&workspace, cx, path!("/sol/b.txt")).await;
        // Back to A: a real close+reopen swap runs.
        switch(&state, &workspace, cx, 1, Some(10));

        let suppressed = workspace.update(cx, |ws, _| ws.active_entry_reveal_suppressed());
        assert!(
            !suppressed,
            "reveal suppression must be cleared once the swap settles"
        );
    }

    #[gpui::test]
    async fn first_visit_leaves_current_layout(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/sol"), json!({ "a.txt": "" })).await;
        let project = Project::test(fs.clone(), [path!("/sol").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let state = Rc::new(RefCell::new(MemberLayoutState::default()));

        switch(&state, &workspace, cx, 1, Some(10));
        open(&workspace, cx, path!("/sol/a.txt")).await;
        // First visit to B must NOT blank the editor (inherit A's view).
        switch(&state, &workspace, cx, 1, Some(20));
        let paths = open_paths(&workspace, cx);
        assert_eq!(paths.len(), 1, "first visit inherits current tabs: {paths:?}");
        assert!(paths[0].ends_with("a.txt"));
    }

    #[gpui::test]
    async fn none_catalog_clears_current_without_apply(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/sol"), json!({ "a.txt": "" })).await;
        let project = Project::test(fs.clone(), [path!("/sol").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let state = Rc::new(RefCell::new(MemberLayoutState::default()));

        switch(&state, &workspace, cx, 1, Some(10));
        open(&workspace, cx, path!("/sol/a.txt")).await;
        switch(&state, &workspace, cx, 1, None); // member removed
        assert!(state.borrow().current.is_none(), "current cleared on None catalog");
        // The just-open file is left as-is (no forced blank).
        let paths = open_paths(&workspace, cx);
        assert_eq!(paths.len(), 1, "None catalog does not clear the editor: {paths:?}");
    }

    fn left_dock_open(workspace: &gpui::Entity<Workspace>, cx: &mut gpui::VisualTestContext) -> bool {
        workspace.update(cx, |ws, cx| ws.left_dock().read(cx).is_open())
    }

    #[gpui::test]
    async fn member_switch_swaps_dock_open_state(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/sol"), json!({ "a.txt": "" })).await;
        let project = Project::test(fs.clone(), [path!("/sol").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        // Register a left-dock panel so the dock can be opened/closed.
        workspace.update_in(cx, |ws, window, cx| {
            let panel = cx.new(|cx| TestPanel::new(DockPosition::Left, 100, cx));
            ws.add_panel(panel, window, cx);
        });
        let state = Rc::new(RefCell::new(MemberLayoutState::default()));

        switch(&state, &workspace, cx, 1, Some(10));
        // A: open the left dock.
        workspace.update_in(cx, |ws, window, cx| {
            ws.left_dock().update(cx, |d, cx| d.set_open(true, window, cx));
        });
        cx.run_until_parked();
        assert!(left_dock_open(&workspace, cx), "A has left dock open");

        // B (first visit): inherits open; then close it for B.
        switch(&state, &workspace, cx, 1, Some(20));
        workspace.update_in(cx, |ws, window, cx| {
            ws.left_dock().update(cx, |d, cx| d.set_open(false, window, cx));
        });
        cx.run_until_parked();

        // Back to A: left dock re-opens.
        switch(&state, &workspace, cx, 1, Some(10));
        assert!(left_dock_open(&workspace, cx), "A restores left dock open");

        // Back to B: left dock closed.
        switch(&state, &workspace, cx, 1, Some(20));
        assert!(!left_dock_open(&workspace, cx), "B restores left dock closed");
    }

    #[gpui::test]
    async fn solution_switch_does_not_corrupt_outgoing_member_snapshot(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/sol"),
            json!({ "a.txt": "", "bfile.txt": "" }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/sol").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let state = Rc::new(RefCell::new(MemberLayoutState::default()));

        // 1. Switch to solution A, member X; open a.txt.
        switch(&state, &workspace, cx, 1, Some(100));
        open(&workspace, cx, path!("/sol/a.txt")).await;

        // 2. Switch to A, member X2 (first visit): snapshots (A,X)={a.txt};
        //    current=(A,X2); no apply, workspace still shows a.txt.
        switch(&state, &workspace, cx, 1, Some(102));
        let paths = open_paths(&workspace, cx);
        assert_eq!(paths.len(), 1, "X2 first visit inherits current tabs: {paths:?}");
        assert!(paths[0].ends_with("a.txt"));

        // 3. Simulate an in-place solution swap (switch.rs) replaying B's
        //    tabs on the SAME workspace with no ActiveMemberChanged event:
        //    close everything and open a different file.
        let item_ids: Vec<_> =
            workspace.update(cx, |ws, cx| ws.items(cx).map(|i| i.item_id()).collect::<Vec<_>>());
        for id in item_ids {
            let t = workspace.update_in(cx, |ws, window, cx| {
                let pane = ws.active_pane().clone();
                pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(id, workspace::SaveIntent::Skip, window, cx)
                })
            });
            let _ = t.await;
        }
        open(&workspace, cx, path!("/sol/bfile.txt")).await;

        // 4. Switch to solution B, member Y. `state.current` still says
        //    (A, X2), which is stale relative to the workspace's real
        //    content (B's tabs) — the snapshot step must not fire.
        switch(&state, &workspace, cx, 2, Some(200));

        // 5. (A, X2) must not have been corrupted with B's file.
        let key_x2 = (SolutionId(1), MemberId(102));
        let st = state.borrow();
        if let Some(layout) = st.layouts.get(&key_x2) {
            assert!(
                !layout
                    .open_paths
                    .iter()
                    .any(|p| p.to_string_lossy().ends_with("bfile.txt")),
                "X2's snapshot must not be corrupted with B's file: {:?}",
                layout.open_paths
            );
        }
        // (A, X) must still hold its original snapshot of a.txt.
        let key_x = (SolutionId(1), MemberId(100));
        let layout_x = st.layouts.get(&key_x).expect("(A, X) snapshot must still exist");
        assert_eq!(layout_x.open_paths.len(), 1, "{:?}", layout_x.open_paths);
        assert!(layout_x.open_paths[0].to_string_lossy().ends_with("a.txt"));
    }
}
