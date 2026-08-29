use crate::tests::{init_test, init_test_workspace, start_debug_session};
use gpui::{BackgroundExecutor, BorrowAppContext as _, TestAppContext, VisualTestContext};
use project::{FakeFs, Project};
use serde_json::json;
use settings::SettingsStore;
use util::path;

/// The debugger's orientation is fixed by `BAND_DOCK_POSITION`, not by
/// `DebuggerSettings::dock`: it occupies the Solution band's utility half, a
/// wide, short region, and a session built with the horizontal (side-dock)
/// axis would stack its panes vertically and paint sideways in there. The
/// setting still exists — removing it is a settings migration this plan does
/// not want — so this pins that it no longer steers anything: set it to
/// `Right` and a freshly started session still serialises `Axis::Vertical`,
/// the axis `DockPosition::Bottom` yields.
#[gpui::test]
async fn test_new_sessions_ignore_the_debugger_dock_setting(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.debugger.get_or_insert_default().dock =
                    Some(settings::DockPosition::Right);
            });
        });
    });

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({
            "main.rs": "fn main() {}",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let cx = &mut VisualTestContext::from_window(*workspace, cx);

    start_debug_session(&workspace, cx, |_| {}).unwrap();
    cx.run_until_parked();

    let debug_panel = workspace
        .update(cx, |workspace, _window, cx| {
            crate::tests::debug_panel(workspace, cx)
        })
        .unwrap();

    let dock_axis = debug_panel
        .read_with(cx, |panel, cx| {
            panel
                .active_session()
                .unwrap()
                .read(cx)
                .running_state()
                .read(cx)
                .serialized_layout(cx)
        })
        .dock_axis;

    assert_eq!(
        dock_axis,
        gpui::Axis::Vertical,
        "a session started while `debugger.dock` says Right must still be \
         built with the band's bottom-dock-shaped axis"
    );
}
