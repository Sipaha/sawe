use super::*;
use gpui::TestAppContext;
use language::Buffer;
use project::{FakeFs, Project};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use workspace::{AppState, MultiWorkspace};

/// A single-point match range at `offset`, sufficient for exercising `MatchList` grouping without
/// depending on the real search backend to produce ranges.
fn anchor_range(buffer: &Entity<Buffer>, cx: &App) -> Range<Anchor> {
    let buffer = buffer.read(cx);
    buffer.anchor_before(0)..buffer.anchor_after(0)
}

#[gpui::test]
async fn test_toggle_opens_modal(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "hello world\n" }))
        .await;
    let project = Project::test(fs, ["/root".as_ref()], cx).await;
    // Dispatch routes through `MultiWorkspace::actions`, which is the only
    // place workspace-registered `on_action` listeners get attached to the
    // render tree — a bare `Workspace` root view never sees them.
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    cx.dispatch_action(Toggle::default());
    workspace.update(cx, |workspace, cx| {
        assert!(
            workspace.active_modal::<FindInPath>(cx).is_some(),
            "Toggle should open the FindInPath modal"
        );
    });
}

#[gpui::test]
async fn test_query_edit_drives_search(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/root",
        json!({
            "a.txt": "token\n",
            "b.txt": "nothing here\n",
        }),
    )
    .await;
    let project = Project::test(fs, ["/root".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    cx.dispatch_action(Toggle::default());
    let find_in_path = workspace.update(cx, |workspace, cx| {
        workspace
            .active_modal::<FindInPath>(cx)
            .expect("Toggle should open the FindInPath modal")
    });

    // Editing the query editor should fire `EditorEvent::Edited`, which the modal's
    // subscription turns into `update_search` -> `build_query` -> `spawn_search`.
    cx.update(|window, cx| {
        find_in_path.update(cx, |find_in_path, cx| {
            find_in_path.query_editor.update(cx, |editor, cx| {
                editor.set_text("token", window, cx);
            });
        });
    });

    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    find_in_path.read_with(cx, |find_in_path, _cx| {
        assert!(
            find_in_path.results.total_matches() > 0,
            "editing the query editor should drive build_query -> spawn_search end-to-end"
        );
    });
}

#[gpui::test]
async fn test_toggle_regex_action_flips_search_options(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "hello\n" })).await;
    let project = Project::test(fs, ["/root".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    cx.dispatch_action(Toggle::default());
    let find_in_path = workspace.update(cx, |workspace, cx| {
        workspace
            .active_modal::<FindInPath>(cx)
            .expect("Toggle should open the FindInPath modal")
    });

    let was_regex = find_in_path.read_with(cx, |find_in_path, _| {
        find_in_path.search_options.contains(SearchOptions::REGEX)
    });

    // `ToggleRegex` dispatches to the currently focused node (the query editor, focused by the
    // modal layer on open) and bubbles up to the `on_action` handler registered on the modal root.
    cx.dispatch_action(ToggleRegex);

    find_in_path.read_with(cx, |find_in_path, _| {
        assert_eq!(
            find_in_path.search_options.contains(SearchOptions::REGEX),
            !was_regex,
            "dispatching ToggleRegex to the focused modal should flip the REGEX bit"
        );
    });
}

fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
    cx.update(|cx| {
        let state = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        super::init(cx);
        state
    })
}

#[gpui::test]
async fn test_include_patterns_for_scope_multi_worktree(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/alpha", json!({ "src": { "a.rs": "" } }))
        .await;
    fs.insert_tree("/beta", json!({ "b.rs": "" })).await;
    let project = Project::test(fs, ["/alpha".as_ref(), "/beta".as_ref()], cx).await;

    project.read_with(cx, |_project, cx| {
        assert_eq!(
            super::include_patterns_for_scope(&Scope::Solution, None, &project, cx),
            Vec::<String>::new(),
            "Scope::Solution should not restrict the search"
        );

        assert_eq!(
            super::include_patterns_for_scope(
                &Scope::Directory(PathBuf::from("/alpha/src")),
                None,
                &project,
                cx
            ),
            vec!["alpha/src/**".to_string()],
            "Directory scope should be root-name prefixed when the project has multiple worktrees"
        );

        assert_eq!(
            super::include_patterns_for_scope(
                &Scope::Project,
                Some(&PathBuf::from("/alpha")),
                &project,
                cx
            ),
            vec!["alpha/**".to_string()],
            "Project scope should restrict to the active member's worktree root"
        );

        assert_eq!(
            super::include_patterns_for_scope(&Scope::Project, None, &project, cx),
            vec!["alpha/**".to_string()],
            "Project scope with no member_root falls back to the first visible worktree"
        );
    });
}

#[gpui::test]
async fn test_include_patterns_for_scope_single_worktree(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "src": { "a.rs": "" } }))
        .await;
    let project = Project::test(fs, ["/root".as_ref()], cx).await;

    project.read_with(cx, |_project, cx| {
        assert_eq!(
            super::include_patterns_for_scope(
                &Scope::Directory(PathBuf::from("/root/src")),
                None,
                &project,
                cx
            ),
            vec!["src/**".to_string()],
            "Single-worktree projects should not be root-name prefixed"
        );

        assert_eq!(
            super::include_patterns_for_scope(&Scope::Project, None, &project, cx),
            vec!["**".to_string()],
            "Project scope on the sole worktree covers the whole worktree root"
        );
    });
}

#[gpui::test]
async fn test_build_query_restricts_to_project_scope(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/alpha", json!({ "x.rs": "" })).await;
    fs.insert_tree("/beta", json!({ "x.rs": "" })).await;
    let project = Project::test(fs, ["/alpha".as_ref(), "/beta".as_ref()], cx).await;

    project.read_with(cx, |_project, cx| {
        let query = super::build_query(
            "needle",
            SearchOptions::NONE,
            "",
            "",
            &Scope::Project,
            Some(&PathBuf::from("/alpha")),
            &project,
            cx,
        )
        .expect("non-empty query text with a valid scope should build a query");

        let include = query.as_inner().files_to_include();
        assert!(
            include.is_match_std_path(std::path::Path::new("alpha/x.rs")),
            "In-Project scope should match files under the active member's worktree"
        );
        assert!(
            !include.is_match_std_path(std::path::Path::new("beta/x.rs")),
            "In-Project scope should reject files under a different worktree"
        );

        assert!(
            super::build_query(
                "",
                SearchOptions::NONE,
                "",
                "",
                &Scope::Solution,
                None,
                &project,
                cx,
            )
            .is_none(),
            "an empty query string should not build a query"
        );
    });
}

#[gpui::test]
async fn test_matchlist_groups_and_flattens(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "foo\nfoo\n", "b.txt": "foo\n" }))
        .await;
    let project = Project::test(fs, ["/root".as_ref()], cx).await;
    let buffer_a = project
        .update(cx, |p, cx| p.open_local_buffer("/root/a.txt", cx))
        .await
        .unwrap();
    let buffer_b = project
        .update(cx, |p, cx| p.open_local_buffer("/root/b.txt", cx))
        .await
        .unwrap();

    let mut list = MatchList::default();
    cx.update(|cx| {
        list.push_result(
            buffer_a.clone(),
            vec![anchor_range(&buffer_a, cx), anchor_range(&buffer_a, cx)],
            cx,
        );
        list.push_result(buffer_b.clone(), vec![anchor_range(&buffer_b, cx)], cx);
        list.rebuild_rows();
    });

    assert_eq!(list.file_count(), 2);
    assert_eq!(list.total_matches(), 3);
    assert_eq!(list.rows.len(), 5);
    assert!(matches!(list.rows[0], Row::Header(0)));
    assert!(matches!(list.rows[1], Row::Match(0, 0)));
    assert!(matches!(list.rows[2], Row::Match(0, 1)));
    assert!(matches!(list.rows[3], Row::Header(1)));
    assert!(matches!(list.rows[4], Row::Match(1, 0)));
}

#[gpui::test]
async fn test_matchlist_push_result_ignores_empty_ranges(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "foo\n" })).await;
    let project = Project::test(fs, ["/root".as_ref()], cx).await;
    let buffer_a = project
        .update(cx, |p, cx| p.open_local_buffer("/root/a.txt", cx))
        .await
        .unwrap();

    let mut list = MatchList::default();
    cx.update(|cx| {
        list.push_result(buffer_a.clone(), Vec::new(), cx);
        list.rebuild_rows();
    });

    assert_eq!(
        list.file_count(),
        0,
        "an empty ranges batch should not create a group"
    );
    assert_eq!(list.rows.len(), 0);
}

#[gpui::test]
async fn test_spawn_search_streams_grouped_results(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/root",
        json!({
            "a.txt": "token\ntoken\n",
            "b.txt": "token\n",
            "c.txt": "nothing here\n",
        }),
    )
    .await;
    let project = Project::test(fs, ["/root".as_ref()], cx).await;
    // Dispatch through `MultiWorkspace` (see `test_toggle_opens_modal`) so the modal is
    // constructed via `FindInPath::toggle`, which is what wires `self.project`.
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    cx.dispatch_action(Toggle::default());
    let find_in_path = workspace.update(cx, |workspace, cx| {
        workspace
            .active_modal::<FindInPath>(cx)
            .expect("Toggle should open the FindInPath modal")
    });

    let query = project.read_with(cx, |_project, cx| {
        super::build_query(
            "token",
            SearchOptions::NONE,
            "",
            "",
            &Scope::Solution,
            None,
            &project,
            cx,
        )
        .expect("non-empty query text with a valid scope should build a query")
    });

    find_in_path.update(cx, |find_in_path, cx| {
        find_in_path.spawn_search(query, cx);
    });

    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    find_in_path.read_with(cx, |find_in_path, _cx| {
        assert_eq!(
            find_in_path.results.file_count(),
            2,
            "a.txt and b.txt both contain 'token'; c.txt should not match"
        );
        assert_eq!(find_in_path.results.total_matches(), 3);
        assert_eq!(find_in_path.status, SearchStatus::Done);
        assert!(find_in_path.search_task.is_none());
    });
}
