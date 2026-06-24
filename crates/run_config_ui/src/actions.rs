use gpui::{App, Window};
use workspace::Workspace;

gpui::actions!(
    run_config,
    [
        /// Run the selected run configuration.
        Run,
        /// Debug the selected run configuration.
        Debug,
        /// Stop the active run of the selected run configuration.
        Stop,
        /// Select the next run configuration in the list.
        SelectNextConfig,
        /// Open the Edit Configurations dialog.
        EditConfigurations,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(register).detach();
}

fn register(
    workspace: &mut Workspace,
    _window: Option<&mut Window>,
    _cx: &mut gpui::Context<Workspace>,
) {
    workspace
        .register_action(|workspace, _: &Run, window, cx| {
            crate::toolbar_strip::with_controller(workspace, cx, |controller, cx| {
                if let Some(id) = controller.selected_id().cloned() {
                    controller.run(id, run_config::Executor::Run, window, cx);
                }
            });
        })
        .register_action(|workspace, _: &Debug, window, cx| {
            crate::toolbar_strip::with_controller(workspace, cx, |controller, cx| {
                if let Some(id) = controller.selected_id().cloned() {
                    controller.run(id, run_config::Executor::Debug, window, cx);
                }
            });
        })
        .register_action(|workspace, _: &Stop, _window, cx| {
            crate::toolbar_strip::with_controller(workspace, cx, |controller, cx| {
                if let Some(id) = controller.selected_id().cloned() {
                    controller.stop(&id, cx);
                }
            });
        })
        .register_action(|workspace, _: &SelectNextConfig, _window, cx| {
            crate::toolbar_strip::with_controller(workspace, cx, |controller, cx| {
                controller.select_next(cx)
            });
        })
        .register_action(|workspace, _: &EditConfigurations, window, cx| {
            crate::edit_modal::EditConfigurationsModal::toggle(workspace, window, cx);
        });
}
