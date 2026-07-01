use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{Context, Task, Window};
use solutions::{CatalogId, SolutionId};
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
    // 1. Snapshot the outgoing member from the live workspace.
    {
        let mut st = state.borrow_mut();
        if let Some(prev) = st.current.clone() {
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

    // Center pane: close all editor items, reopen the snapshot paths,
    // activate the previously-active one. Async (open/close are Tasks);
    // stored in `apply_task` so a rapid re-switch cancels this one.
    let task = cx.spawn_in(window, async move |workspace, cx| {
        let item_ids: Vec<_> = workspace
            .update(cx, |ws, cx| ws.items(cx).map(|i| i.item_id()).collect::<Vec<_>>())
            .unwrap_or_default();
        for id in item_ids {
            if let Ok(close) = workspace.update_in(cx, |ws, window, cx| {
                let pane = ws.active_pane().clone();
                pane.update(cx, |pane, cx| pane.close_item_by_id(id, SaveIntent::Skip, window, cx))
            }) {
                let _ = close.await;
            }
        }
        for path in &layout.open_paths {
            if let Ok(open) = workspace.update_in(cx, |ws, window, cx| {
                let mut options = OpenOptions::default();
                options.visible = Some(OpenVisible::None);
                ws.open_abs_path(path.clone(), options, window, cx)
            }) {
                let _ = open.await;
            }
        }
        if let Some(active) = layout.active_path {
            if let Ok(open) = workspace.update_in(cx, |ws, window, cx| {
                ws.open_abs_path(active, OpenOptions::default(), window, cx)
            }) {
                let _ = open.await;
            }
        }
    });
    state.borrow_mut().apply_task = Some(task);
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

    use workspace::dock::test::TestPanel;
    use workspace::dock::DockPosition;

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
}
