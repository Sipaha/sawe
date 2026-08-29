use std::sync::Arc;

use anyhow::{Context as _, Result};
use console_panel::ConsolePanel;
use dap::adapters::DebugTaskDefinition;
use dap::client::DebugAdapterClient;
use gpui::{AppContext as _, Entity, TestAppContext, WindowHandle};
use project::{Project, debugger::session::Session};
use settings::SettingsStore;
use solution_agent::solution_band::SolutionBand;
use task::SharedTaskContext;
use workspace::{MultiWorkspace, UtilityKind};

use crate::{
    debugger_panel::{DebugPanel, debug_panel_for_workspace},
    session::DebugSession,
};

/// The `DebugPanel` of a test window, resolved the way production does —
/// out of the Solution band's utility slot (phase 2b task 5), not out of a
/// dock. Takes the `MultiWorkspace` because that is what
/// `WindowHandle::update` hands the test closures.
///
/// `#[cfg(test)]`, not just `#[cfg(any(test, feature = "test-support"))]`
/// like the module around it: every caller is one of the `#[cfg(test)]`
/// submodules below, so in a `test-support`-without-`cfg(test)` build (what
/// `./script/clippy` compiles, and what external crates get from this
/// crate's `test-support` feature) this would be genuinely dead code. The
/// `pub` helpers above it are the ones other crates call and stay ungated.
#[cfg(test)]
#[track_caller]
pub(crate) fn debug_panel(multi_workspace: &MultiWorkspace, cx: &gpui::App) -> Entity<DebugPanel> {
    debug_panel_for_workspace(multi_workspace.workspace().read(cx))
        .expect("debug panel installed in the solution band utility slot")
}

/// Focus the debug panel. It no longer lives in a dock (phase 2b task 5), so
/// `Workspace::focus_panel::<DebugPanel>` cannot find it; focus its handle
/// directly, which is what `SolutionBand::toggle_utility_focus` does in
/// production. `#[cfg(test)]` for the same reason as `debug_panel` above.
#[cfg(test)]
pub(crate) fn focus_debug_panel(
    multi_workspace: &mut MultiWorkspace,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<MultiWorkspace>,
) {
    multi_workspace.workspace().update(cx, |workspace, cx| {
        crate::debugger_panel::reveal_debug_panel(workspace, cx);
        let Some(panel) = debug_panel_for_workspace(workspace) else {
            return;
        };
        gpui::Focusable::focus_handle(panel.read(cx), cx).focus(window, cx);
    });
}

#[cfg(test)]
mod attach_modal;
#[cfg(test)]
mod console;
#[cfg(test)]
mod dap_logger;
#[cfg(test)]
mod debugger_panel;
#[cfg(test)]
mod inline_values;
#[cfg(test)]
mod module_list;
#[cfg(test)]
mod new_process_modal;
#[cfg(test)]
mod persistence;
#[cfg(test)]
mod stack_frame_list;
#[cfg(test)]
mod variable_list;

pub fn init_test(cx: &mut gpui::TestAppContext) {
    #[cfg(test)]
    zlog::init_test();

    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        terminal_view::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        crate::init(cx);
        dap_adapters::init(cx);

        // ConsolePanel::load now requires a global SolutionAgentStore so the
        // panel can wire up chat tabs. The debugger tests don't use chat, but
        // the load() path reads the store unconditionally — set up an empty
        // one rather than special-case the loader.
        let registry = std::sync::Arc::new(solution_agent::adapter::AdapterRegistry::new());
        solution_agent::store::SolutionAgentStore::init_global(cx, registry);
    });
}

pub async fn init_test_workspace(
    project: &Entity<Project>,
    cx: &mut TestAppContext,
) -> WindowHandle<MultiWorkspace> {
    let workspace_handle =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

    let debugger_panel = workspace_handle
        .update(cx, |multi, window, cx| {
            multi.workspace().update(cx, |_workspace, cx| {
                cx.spawn_in(window, async move |this, cx| {
                    DebugPanel::load(this, cx).await
                })
            })
        })
        .unwrap()
        .await
        .expect("Failed to load debug panel");

    let terminal_panel = workspace_handle
        .update(cx, |multi, window, cx| {
            let weak_workspace = multi.workspace().downgrade();
            cx.spawn_in(window, async move |_, cx| {
                ConsolePanel::load(weak_workspace, cx.clone()).await
            })
        })
        .unwrap()
        .await
        .expect("Failed to load console panel");

    workspace_handle
        .update(cx, |multi, window, cx| {
            multi.workspace().update(cx, |workspace, cx| {
                // The band is what actually renders the debugger now, so a
                // test workspace needs one or the panel is never drawn (and
                // `reveal_debug_panel` — the replacement for
                // `Workspace::open_panel` on a breakpoint hit — is a no-op).
                // Same construction as `zed::initialize_panels`.
                let band = cx.new(|cx| {
                    SolutionBand::new(workspace.weak_handle(), workspace.project().clone(), cx)
                });
                workspace.set_solution_band_item(band.into(), window, cx);
                // Neither panel is a dock panel any more — the Solution
                // band's utility section hosts them (phase 2a task 6 for the
                // terminal, phase 2b task 5 for the debugger). Install both
                // into the same type-erased slot `zed.rs` uses in production
                // so tests exercise the real lookup paths
                // (`console_panel_for_workspace`, `debug_panel_for_workspace`).
                workspace.set_solution_band_utility_item(
                    UtilityKind::Debug,
                    debugger_panel.into(),
                    window,
                    cx,
                );
                workspace.set_solution_band_utility_item(
                    UtilityKind::Terminal,
                    terminal_panel.into(),
                    window,
                    cx,
                );
            });
        })
        .unwrap();
    workspace_handle
}

#[track_caller]
pub fn active_debug_session_panel(
    workspace: WindowHandle<MultiWorkspace>,
    cx: &mut TestAppContext,
) -> Entity<DebugSession> {
    workspace
        .update(cx, |multi, _window, cx| {
            multi.workspace().update(cx, |workspace, cx| {
                let debug_panel = debug_panel_for_workspace(workspace).unwrap();
                debug_panel
                    .update(cx, |this, _| this.active_session())
                    .unwrap()
            })
        })
        .unwrap()
}

pub fn start_debug_session_with<T: Fn(&Arc<DebugAdapterClient>) + 'static>(
    workspace: &WindowHandle<MultiWorkspace>,
    cx: &mut gpui::TestAppContext,
    config: DebugTaskDefinition,
    configure: T,
) -> Result<Entity<Session>> {
    let _subscription = project::debugger::test::intercept_debug_sessions(cx, configure);
    workspace.update(cx, |multi, window, cx| {
        multi.workspace().update(cx, |workspace, cx| {
            workspace.start_debug_session(
                config.to_scenario(),
                SharedTaskContext::default(),
                None,
                None,
                window,
                cx,
            )
        })
    })?;
    cx.run_until_parked();
    let session = workspace.read_with(cx, |workspace, cx| {
        debug_panel_for_workspace(workspace.workspace().read(cx))
            .and_then(|panel| {
                panel
                    .read(cx)
                    .sessions_with_children
                    .keys()
                    .max_by_key(|session| session.read(cx).session_id(cx))
            })
            .map(|session| session.read(cx).running_state().read(cx).session())
            .cloned()
            .context("Failed to get active session")
    })??;

    Ok(session)
}

pub fn start_debug_session<T: Fn(&Arc<DebugAdapterClient>) + 'static>(
    workspace: &WindowHandle<MultiWorkspace>,
    cx: &mut gpui::TestAppContext,
    configure: T,
) -> Result<Entity<Session>> {
    use serde_json::json;

    start_debug_session_with(
        workspace,
        cx,
        DebugTaskDefinition {
            adapter: "fake-adapter".into(),
            label: "test".into(),
            config: json!({
                "request": "launch"
            }),
            tcp_connection: None,
        },
        configure,
    )
}
