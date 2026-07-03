use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{Context, Task, Window};
use solutions::{CatalogId, SolutionId, SolutionStoreEvent};
use workspace::{DockStructure, OpenOptions, OpenVisible, SaveIntent, Workspace};

type MemberKey = (SolutionId, CatalogId);

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

/// Snapshot the outgoing active member's layout and apply the incoming
/// member's, if it has one. `catalog == None` means the solution lost its
/// last member: clear tracking without applying. First visit to a member
/// (no stored snapshot) leaves the current layout intact.
pub(crate) fn apply_active_member_change(
    state: &Rc<RefCell<MemberLayoutState>>,
    workspace: &mut Workspace,
    solution: SolutionId,
    catalog: Option<CatalogId>,
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
        if let Some(prev) = st.current.clone() {
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
    let Some(catalog) = catalog else {
        let mut st = state.borrow_mut();
        st.current = None;
        st.apply_task = None;
        return;
    };
    let key = (solution, catalog);

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

    // Center pane: close all editor items, reopen the snapshot paths, activate
    // the previously-active one. Async (open/close are Tasks); stored in
    // `apply_task` so a rapid re-switch cancels this one.
    let task = cx.spawn_in(window, async move |workspace, cx| {
        // Close the outgoing tabs as ONE batch: `close_items` removes every
        // matching item in a single task. Awaiting each close in turn (the old
        // `close_item_by_id` loop) spread the removals across frames, so tabs
        // visibly vanished one by one; a single batched close collapses that to
        // one repaint.
        let close = workspace.update_in(cx, |ws, window, cx| {
            let pane = ws.active_pane().clone();
            pane.update(cx, |pane, cx| {
                pane.close_items(window, cx, SaveIntent::Skip, &|_| true)
            })
        });
        if let Ok(close) = close {
            let _ = close.await;
        }

        // Reopen the snapshot's tabs IN ORDER (order matters, so sequential).
        // Reveal is suppressed, so these do not scroll the tree.
        for path in &layout.open_paths {
            if let Ok(open) = workspace.update_in(cx, |ws, window, cx| {
                let mut options = OpenOptions::default();
                options.visible = Some(OpenVisible::None);
                ws.open_abs_path(path.clone(), options, window, cx)
            }) {
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
        if let SolutionStoreEvent::ActiveMemberChanged { solution, catalog } = event {
            apply_active_member_change(
                &state,
                workspace,
                solution.clone(),
                catalog.clone(),
                window,
                cx,
            );
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
    use solutions::{CatalogId, SolutionId};
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
        s: &str,
        c: Option<&str>,
    ) {
        let (state, catalog) = (state.clone(), c.map(|c| CatalogId(c.into())));
        workspace.update_in(cx, |ws, window, cx| {
            apply_active_member_change(&state, ws, SolutionId(s.into()), catalog, window, cx);
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
        switch(&state, &workspace, cx, "sol", Some("A"));
        open(&workspace, cx, path!("/sol/a.txt")).await;

        // Switch to B (first visit): snapshots A={a.txt}, inherits a.txt open.
        switch(&state, &workspace, cx, "sol", Some("B"));
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
        switch(&state, &workspace, cx, "sol", Some("A"));
        let paths = open_paths(&workspace, cx);
        assert_eq!(paths.len(), 1, "A restores exactly its own tab: {paths:?}");
        assert!(paths[0].ends_with("a.txt"), "A restores a.txt: {paths:?}");

        // Back to B: apply B={b.txt}.
        switch(&state, &workspace, cx, "sol", Some("B"));
        let paths = open_paths(&workspace, cx);
        assert_eq!(paths.len(), 1, "B restores exactly its own tab: {paths:?}");
        assert!(paths[0].ends_with("b.txt"), "B restores b.txt: {paths:?}");
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

        switch(&state, &workspace, cx, "sol", Some("A"));
        open(&workspace, cx, path!("/sol/a.txt")).await;
        switch(&state, &workspace, cx, "sol", Some("B"));
        open(&workspace, cx, path!("/sol/b.txt")).await;
        // Back to A: a real close+reopen swap runs.
        switch(&state, &workspace, cx, "sol", Some("A"));

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

        switch(&state, &workspace, cx, "sol", Some("A"));
        open(&workspace, cx, path!("/sol/a.txt")).await;
        // First visit to B must NOT blank the editor (inherit A's view).
        switch(&state, &workspace, cx, "sol", Some("B"));
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

        switch(&state, &workspace, cx, "sol", Some("A"));
        open(&workspace, cx, path!("/sol/a.txt")).await;
        switch(&state, &workspace, cx, "sol", None); // member removed
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

        switch(&state, &workspace, cx, "sol", Some("A"));
        // A: open the left dock.
        workspace.update_in(cx, |ws, window, cx| {
            ws.left_dock().update(cx, |d, cx| d.set_open(true, window, cx));
        });
        cx.run_until_parked();
        assert!(left_dock_open(&workspace, cx), "A has left dock open");

        // B (first visit): inherits open; then close it for B.
        switch(&state, &workspace, cx, "sol", Some("B"));
        workspace.update_in(cx, |ws, window, cx| {
            ws.left_dock().update(cx, |d, cx| d.set_open(false, window, cx));
        });
        cx.run_until_parked();

        // Back to A: left dock re-opens.
        switch(&state, &workspace, cx, "sol", Some("A"));
        assert!(left_dock_open(&workspace, cx), "A restores left dock open");

        // Back to B: left dock closed.
        switch(&state, &workspace, cx, "sol", Some("B"));
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
        switch(&state, &workspace, cx, "A", Some("X"));
        open(&workspace, cx, path!("/sol/a.txt")).await;

        // 2. Switch to A, member X2 (first visit): snapshots (A,X)={a.txt};
        //    current=(A,X2); no apply, workspace still shows a.txt.
        switch(&state, &workspace, cx, "A", Some("X2"));
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
        switch(&state, &workspace, cx, "B", Some("Y"));

        // 5. (A, X2) must not have been corrupted with B's file.
        let key_x2 = (SolutionId("A".into()), CatalogId("X2".into()));
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
        let key_x = (SolutionId("A".into()), CatalogId("X".into()));
        let layout_x = st.layouts.get(&key_x).expect("(A, X) snapshot must still exist");
        assert_eq!(layout_x.open_paths.len(), 1, "{:?}", layout_x.open_paths);
        assert!(layout_x.open_paths[0].to_string_lossy().ends_with("a.txt"));
    }
}
