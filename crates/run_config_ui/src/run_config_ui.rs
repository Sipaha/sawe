pub mod actions;
mod edit_modal;
mod run_controller;
mod schema_form;
mod status_item;
mod toolbar_strip;

pub use run_controller::{ActiveRun, RunController, RunControllerEvent};

use gpui::App;
use workspace::Workspace;

pub fn init(cx: &mut App) {
    actions::init(cx);

    // Let MCP tools in `run_config` reach a window's `RunController` without a
    // `run_config -> run_config_ui` dependency: install a sink on the store.
    if let Some(store) = run_config::RunConfigStore::try_global(cx) {
        store.update(cx, |store, _| {
            store.set_command_sink(std::sync::Arc::new(|command, cx| {
                toolbar_strip::dispatch_run_command(command, cx);
            }));
        });
    }

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };

        let project = workspace.project().clone();
        let fs = project.read(cx).fs().clone();
        if let Some(store) = run_config::RunConfigStore::try_global(cx) {
            store.update(cx, |store, cx| store.watch_project(project, fs, cx));
        }

        toolbar_strip::install(workspace, window, cx);
    })
    .detach();
}
