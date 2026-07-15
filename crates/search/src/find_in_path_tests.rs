use super::*;
use gpui::TestAppContext;
use project::{FakeFs, Project};
use serde_json::json;
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
