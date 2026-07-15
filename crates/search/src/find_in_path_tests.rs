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

#[gpui::test]
async fn test_replace_all(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "foo foo\n" }))
        .await;
    let project = Project::test(fs, ["/root".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    cx.dispatch_action(Toggle {
        replace_enabled: true,
    });
    let find_in_path = workspace.update(cx, |workspace, cx| {
        workspace
            .active_modal::<FindInPath>(cx)
            .expect("Toggle should open the FindInPath modal")
    });

    cx.update(|window, cx| {
        find_in_path.update(cx, |find_in_path, cx| {
            find_in_path.query_editor.update(cx, |editor, cx| {
                editor.set_text("foo", window, cx);
            });
            find_in_path.replace_editor.update(cx, |editor, cx| {
                editor.set_text("bar", window, cx);
            });
        });
    });
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    cx.update(|window, cx| {
        find_in_path.update(cx, |find_in_path, cx| {
            find_in_path.replace_all(&ReplaceAll, window, cx);
        });
    });
    cx.run_until_parked();

    let buffer = project
        .update(cx, |p, cx| p.open_local_buffer("/root/a.txt", cx))
        .await
        .unwrap();
    buffer.read_with(cx, |buffer, _cx| {
        assert_eq!(buffer.text(), "bar bar\n");
    });
}

#[gpui::test]
async fn test_replace_next_replaces_only_selected_match(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/root",
        json!({
            "a.txt": "foo\n",
            "b.txt": "foo\n",
        }),
    )
    .await;
    let project = Project::test(fs, ["/root".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    cx.dispatch_action(Toggle {
        replace_enabled: true,
    });
    let find_in_path = workspace.update(cx, |workspace, cx| {
        workspace
            .active_modal::<FindInPath>(cx)
            .expect("Toggle should open the FindInPath modal")
    });

    cx.update(|window, cx| {
        find_in_path.update(cx, |find_in_path, cx| {
            find_in_path.query_editor.update(cx, |editor, cx| {
                editor.set_text("foo", window, cx);
            });
            find_in_path.replace_editor.update(cx, |editor, cx| {
                editor.set_text("bar", window, cx);
            });
        });
    });
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    // Two files, one match each: the modal auto-selects the first Match row (row 1, since row 0
    // is always that group's Header) — see `test_selection_lands_on_first_match_...`.
    let selected_path = find_in_path.read_with(cx, |find_in_path, _cx| {
        assert_eq!(find_in_path.results.total_matches(), 2);
        let Row::Match(group_index, _) = find_in_path.results.rows[find_in_path.selected_row]
        else {
            panic!("selected_row should be a Match row after streaming results in");
        };
        find_in_path.results.groups[group_index].path.clone()
    });

    cx.update(|window, cx| {
        find_in_path.update(cx, |find_in_path, cx| {
            find_in_path.replace_next(&ReplaceNext, window, cx);
        });
    });
    cx.run_until_parked();

    let buffer_a = project
        .update(cx, |p, cx| p.open_local_buffer("/root/a.txt", cx))
        .await
        .unwrap();
    let buffer_b = project
        .update(cx, |p, cx| p.open_local_buffer("/root/b.txt", cx))
        .await
        .unwrap();

    let (replaced_text, untouched_text) =
        if selected_path.as_ref() == std::path::Path::new("root/a.txt") {
            (buffer_a.read_with(cx, |b, _| b.text()), buffer_b.read_with(cx, |b, _| b.text()))
        } else {
            (buffer_b.read_with(cx, |b, _| b.text()), buffer_a.read_with(cx, |b, _| b.text()))
        };
    assert_eq!(
        replaced_text, "bar\n",
        "replace_next should replace the selected match"
    );
    assert_eq!(
        untouched_text, "foo\n",
        "replace_next should leave the other file's match untouched"
    );
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

#[gpui::test]
async fn test_selection_lands_on_first_match_and_select_next_skips_header(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/root",
        json!({
            "a.txt": "token\ntoken\n",
            "b.txt": "token\n",
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

    let first_selected_row = find_in_path.read_with(cx, |find_in_path, _cx| {
        assert_eq!(find_in_path.results.file_count(), 2);
        // `rebuild_rows` always emits `Header(0)` first, so the earliest possible `Match` row
        // (whichever group streamed in first) is row 1.
        assert_eq!(
            find_in_path.selected_row, 1,
            "selection should reset onto the first Match row (row 0 is always a Header)"
        );
        assert!(
            matches!(
                find_in_path.results.rows.get(find_in_path.selected_row),
                Some(Row::Match(_, _))
            ),
            "selected_row should land on a Match row, not a Header, after streaming results in"
        );
        find_in_path.selected_row
    });

    // `menu::SelectNext` dispatches to the currently focused node (the query editor, focused by
    // the modal layer on open) and bubbles up to the `on_action` handler on the modal root.
    cx.dispatch_action(menu::SelectNext);

    find_in_path.read_with(cx, |find_in_path, _cx| {
        assert!(
            find_in_path.selected_row > first_selected_row,
            "SelectNext should move the selection forward"
        );
        assert!(
            matches!(
                find_in_path.results.rows.get(find_in_path.selected_row),
                Some(Row::Match(_, _))
            ),
            "SelectNext should land on a Match row, skipping any intervening Header row"
        );
    });
}

#[gpui::test]
async fn test_selecting_match_builds_and_reuses_preview_editor(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/root",
        json!({
            "a.txt": "token\ntoken\n",
            "b.txt": "token\n",
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

    // Streaming results auto-select the first Match row, but that happens inside `spawn_search`'s
    // async task (no `&mut Window` available there) so it only sets `preview_dirty`; dispatching
    // `SelectNext` (which has a real `Window`) drives `update_preview` directly and gives us
    // something deterministic to assert on without depending on whether a `render` pass already
    // drained the flag.
    cx.dispatch_action(menu::SelectNext);

    let (first_selected_buffer_id, first_preview_editor_id) =
        find_in_path.read_with(cx, |find_in_path, _cx| {
            let Row::Match(group_index, _) = find_in_path.results.rows[find_in_path.selected_row]
            else {
                panic!("selected_row should be a Match row after SelectNext");
            };
            let buffer_id = find_in_path.results.groups[group_index].buffer.entity_id();
            let (preview_buffer_id, preview_editor) = find_in_path
                .preview_editor
                .as_ref()
                .expect("selecting a match should build a preview editor");
            assert_eq!(
                *preview_buffer_id, buffer_id,
                "the preview editor should be keyed by the selected match's buffer id"
            );
            (buffer_id, preview_editor.entity_id())
        });

    // a.txt has two matches in a row, so a second `SelectNext` (assuming the first landed on
    // a.txt's first match) stays within the same file: `update_preview` should reuse the existing
    // editor entity rather than rebuilding it.
    cx.dispatch_action(menu::SelectNext);
    find_in_path.read_with(cx, |find_in_path, _cx| {
        let Row::Match(group_index, _) = find_in_path.results.rows[find_in_path.selected_row]
        else {
            panic!("selected_row should be a Match row after a second SelectNext");
        };
        let buffer_id = find_in_path.results.groups[group_index].buffer.entity_id();
        let (preview_buffer_id, preview_editor) = find_in_path
            .preview_editor
            .as_ref()
            .expect("preview editor should still be present");
        if buffer_id == first_selected_buffer_id {
            assert_eq!(
                preview_editor.entity_id(),
                first_preview_editor_id,
                "staying within the same file should reuse the existing preview editor"
            );
        }
        assert_eq!(*preview_buffer_id, buffer_id);
    });

    // Jump to the last match (b.txt, a different file) and confirm the preview editor rebuilds
    // against the new buffer.
    cx.dispatch_action(menu::SelectLast);
    find_in_path.read_with(cx, |find_in_path, _cx| {
        let Row::Match(group_index, _) = find_in_path.results.rows[find_in_path.selected_row]
        else {
            panic!("selected_row should be a Match row after SelectLast");
        };
        let buffer_id = find_in_path.results.groups[group_index].buffer.entity_id();
        let (preview_buffer_id, preview_editor) = find_in_path
            .preview_editor
            .as_ref()
            .expect("preview editor should still be present");
        assert_eq!(*preview_buffer_id, buffer_id);
        assert_ne!(
            buffer_id, first_selected_buffer_id,
            "SelectLast should land on a different file's match given two files with matches"
        );
        assert_ne!(
            preview_editor.entity_id(),
            first_preview_editor_id,
            "selecting a match in a different file should rebuild the preview editor"
        );
    });
}

#[gpui::test]
async fn test_streaming_batch_without_selection_change_does_not_redirty_preview(
    cx: &mut TestAppContext,
) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/root",
        json!({
            "a.txt": "token\ntoken\n",
            "b.txt": "token\n",
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

    // `spawn_search`'s batch handler already ran `clamp_selection` + set `preview_dirty` for the
    // batch that first landed the selection on a match, and `run_until_parked` drove a `render`
    // pass that consumed the flag via `update_preview`. Capture the resulting `previewed_match`
    // identity.
    let target_after_first_batch = find_in_path.read_with(cx, |find_in_path, _cx| {
        assert!(
            !find_in_path.preview_dirty,
            "the render pass driven by run_until_parked should have consumed preview_dirty"
        );
        find_in_path
            .previewed_match
            .clone()
            .expect("update_preview should have recorded the displayed match")
    });

    // Simulate a later streaming batch that grows `results.rows` without moving
    // `selected_row` — this is exactly what `clamp_selection` does when the current selection is
    // already a valid `Row::Match`, the case the fix targets.
    find_in_path.update(cx, |find_in_path, _cx| {
        let changed = find_in_path.clamp_selection();
        assert!(
            !changed,
            "clamp_selection should report no change when selected_row already points at a valid match"
        );
    });

    // The batch handler only sets `preview_dirty` when `clamp_selection` reports a change, so a
    // no-op reclamp must leave it false — no re-render-triggered `update_preview` call, so the
    // preview's scroll position is left alone instead of snapping back to center.
    find_in_path.read_with(cx, |find_in_path, _cx| {
        assert!(
            !find_in_path.preview_dirty,
            "a streaming batch that doesn't move the selection must not re-dirty the preview"
        );
    });

    // And even if `update_preview` were invoked again anyway (e.g. a stray render pass), it must
    // be a no-op against the same match: `previewed_match` stays exactly as it was.
    cx.update(|window, cx| {
        find_in_path.update(cx, |find_in_path, cx| {
            find_in_path.update_preview(window, cx);
            assert_eq!(
                find_in_path.previewed_match,
                Some(target_after_first_batch),
                "update_preview should be idempotent when the selected match hasn't changed"
            );
        });
    });
}

#[gpui::test]
async fn test_set_scope_directory_restricts_results(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/alpha", json!({ "x.rs": "token\n" })).await;
    fs.insert_tree("/beta", json!({ "x.rs": "token\n" })).await;
    let project = Project::test(fs, ["/alpha".as_ref(), "/beta".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    cx.dispatch_action(Toggle::default());
    let find_in_path = workspace.update(cx, |workspace, cx| {
        workspace
            .active_modal::<FindInPath>(cx)
            .expect("Toggle should open the FindInPath modal")
    });

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
        assert_eq!(
            find_in_path.results.file_count(),
            2,
            "Scope::Solution (the default) should search both worktrees"
        );
    });

    cx.update(|_window, cx| {
        find_in_path.update(cx, |find_in_path, cx| {
            find_in_path.set_scope(Scope::Directory(PathBuf::from("/alpha")), cx);
        });
    });
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    find_in_path.read_with(cx, |find_in_path, _cx| {
        assert_eq!(
            find_in_path.results.file_count(),
            1,
            "Scope::Directory(\"/alpha\") should restrict the search to that worktree only"
        );
        assert_eq!(
            find_in_path.results.groups[0].path.as_ref(),
            std::path::Path::new("alpha/x.rs"),
            "the sole remaining result should be alpha's x.rs, not beta's"
        );
    });
}

#[gpui::test]
async fn test_set_scope_project_falls_back_to_first_worktree(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/alpha", json!({ "x.rs": "token\n" })).await;
    fs.insert_tree("/beta", json!({ "x.rs": "token\n" })).await;
    let project = Project::test(fs, ["/alpha".as_ref(), "/beta".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    cx.dispatch_action(Toggle::default());
    let find_in_path = workspace.update(cx, |workspace, cx| {
        workspace
            .active_modal::<FindInPath>(cx)
            .expect("Toggle should open the FindInPath modal")
    });

    cx.update(|window, cx| {
        find_in_path.update(cx, |find_in_path, cx| {
            find_in_path.query_editor.update(cx, |editor, cx| {
                editor.set_text("token", window, cx);
            });
        });
    });
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    cx.update(|_window, cx| {
        find_in_path.update(cx, |find_in_path, cx| {
            // `init_test` never installs a `SolutionStore` global, so `member_root` is `None` and
            // `include_patterns_for_scope` falls back to the first visible worktree ("alpha").
            find_in_path.set_scope(Scope::Project, cx);
        });
    });
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    find_in_path.read_with(cx, |find_in_path, _cx| {
        assert_eq!(
            find_in_path.results.file_count(),
            1,
            "Scope::Project with no active member should restrict to the first worktree"
        );
        assert_eq!(
            find_in_path.results.groups[0].path.as_ref(),
            std::path::Path::new("alpha/x.rs"),
        );
    });
}

#[gpui::test]
async fn test_build_query_empty_or_unresolved_directory_scope_yields_none(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/alpha", json!({ "x.rs": "token\n" })).await;
    let project = Project::test(fs, ["/alpha".as_ref()], cx).await;

    project.read_with(cx, |_project, cx| {
        assert!(
            super::build_query(
                "token",
                SearchOptions::NONE,
                "",
                "",
                &Scope::Directory(PathBuf::from("")),
                None,
                &project,
                cx,
            )
            .is_none(),
            "an empty Scope::Directory path should build no query rather than falling back to \
             an unrestricted (Solution-wide) search"
        );

        assert!(
            super::build_query(
                "token",
                SearchOptions::NONE,
                "",
                "",
                &Scope::Directory(PathBuf::from("/nonexistent")),
                None,
                &project,
                cx,
            )
            .is_none(),
            "a Scope::Directory path that matches no visible worktree should build no query"
        );

        assert!(
            super::build_query(
                "token",
                SearchOptions::NONE,
                "",
                "",
                &Scope::Directory(PathBuf::from("/alpha")),
                None,
                &project,
                cx,
            )
            .is_some(),
            "a Scope::Directory path that resolves to a real worktree should still build a query"
        );
    });
}

#[gpui::test]
async fn test_set_scope_empty_directory_shows_no_results(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/alpha", json!({ "x.rs": "token\n" })).await;
    fs.insert_tree("/beta", json!({ "x.rs": "token\n" })).await;
    let project = Project::test(fs, ["/alpha".as_ref(), "/beta".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    cx.dispatch_action(Toggle::default());
    let find_in_path = workspace.update(cx, |workspace, cx| {
        workspace
            .active_modal::<FindInPath>(cx)
            .expect("Toggle should open the FindInPath modal")
    });

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
        assert_eq!(
            find_in_path.results.file_count(),
            2,
            "Scope::Solution (the default) should search both worktrees"
        );
    });

    // Simulates a user clicking the "Directory" tab, whose path field starts out empty.
    cx.update(|_window, cx| {
        find_in_path.update(cx, |find_in_path, cx| {
            find_in_path.set_scope(Scope::Directory(PathBuf::from("")), cx);
        });
    });
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    find_in_path.read_with(cx, |find_in_path, _cx| {
        assert_eq!(
            find_in_path.results.file_count(),
            0,
            "an empty Scope::Directory path must show no results, not silently widen to the \
             whole Solution"
        );
        assert_eq!(find_in_path.status, SearchStatus::Idle);
    });

    // Typing a path that matches no visible worktree should behave the same way.
    cx.update(|_window, cx| {
        find_in_path.update(cx, |find_in_path, cx| {
            find_in_path.set_scope(Scope::Directory(PathBuf::from("/nonexistent")), cx);
        });
    });
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    find_in_path.read_with(cx, |find_in_path, _cx| {
        assert_eq!(
            find_in_path.results.file_count(),
            0,
            "a Scope::Directory path outside every visible worktree must show no results"
        );
        assert_eq!(find_in_path.status, SearchStatus::Idle);
    });

    // Recovering to a valid directory should restore restricted results.
    cx.update(|_window, cx| {
        find_in_path.update(cx, |find_in_path, cx| {
            find_in_path.set_scope(Scope::Directory(PathBuf::from("/alpha")), cx);
        });
    });
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    find_in_path.read_with(cx, |find_in_path, _cx| {
        assert_eq!(
            find_in_path.results.file_count(),
            1,
            "typing a valid directory afterward should resume restricting results to it"
        );
    });
}

#[gpui::test]
async fn test_include_mask_filters_results(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/root",
        json!({
            "match.rs": "token\n",
            "match.txt": "token\n",
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
        assert_eq!(
            find_in_path.results.file_count(),
            2,
            "with no mask, both match.rs and match.txt should be found"
        );
    });

    cx.update(|window, cx| {
        find_in_path.update(cx, |find_in_path, cx| {
            find_in_path.included_files_editor.update(cx, |editor, cx| {
                editor.set_text("*.rs", window, cx);
            });
        });
    });
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    find_in_path.read_with(cx, |find_in_path, _cx| {
        assert_eq!(
            find_in_path.results.file_count(),
            1,
            "a '*.rs' include mask should filter out match.txt"
        );
        assert_eq!(
            find_in_path.results.groups[0].path.as_ref(),
            std::path::Path::new("root/match.rs"),
        );
    });
}
