use super::*;
use gpui::TestAppContext;
use project::{FakeFs, Project};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use workspace::{AppState, MultiWorkspace};

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
