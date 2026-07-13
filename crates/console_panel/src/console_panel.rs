//! Unified bottom-dock panel hosting terminal + AI-chat tabs.

mod actions;
mod chat_provider;
mod console_panel_settings;
mod panel;
mod terminal_provider;

pub use actions::{NewChat, NewTerminal, ShowSession, ToggleFocus};
pub use chat_provider::{ChatProvider, ChatProviderEvent};
pub use console_panel_settings::ConsolePanelSettings;
pub use panel::{ConsolePanel, ConsoleTab};
pub use terminal_provider::TerminalProvider;

pub fn init(cx: &mut gpui::App) {
    use settings::Settings;
    ConsolePanelSettings::register(cx);

    cx.observe_new(|workspace: &mut workspace::Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &NewTerminal, window, cx| {
            // No project directory to run in (an empty solution has 0
            // worktrees) → refuse. A non-empty solution or a plain folder has a
            // worktree and is allowed.
            if !panel::workspace_has_worktree(workspace, cx) {
                return;
            }
            if let Some(panel) = workspace.panel::<ConsolePanel>(cx) {
                panel.update(cx, |panel, cx| panel.add_terminal_tab(None, window, cx));
            }
        });
        workspace.register_action(|workspace, _: &NewChat, window, cx| {
            // Same gate as `NewTerminal`: an empty solution has 0 worktrees, so
            // there is no directory for the agent to work in. Without this the
            // keybinding walked straight past the menu entry's disabled state.
            if !panel::workspace_has_worktree(workspace, cx) {
                return;
            }
            let Some(solution_id) = panel::active_solution_id_for_workspace(workspace, cx) else {
                return;
            };
            // Read the project from the `&mut Workspace` we already hold — NOT
            // from the `Workspace` entity inside `add_chat_tab`. This handler
            // runs while the `Workspace` is mutably leased, so re-reading the
            // entity would `double_lease_panic` (fixed regression).
            let project = workspace.project().clone();
            if let Some(panel) = workspace.panel::<ConsolePanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.add_chat_tab(solution_id, project, window, cx)
                });
            }
        });
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ConsolePanel>(window, cx);
        });
        workspace.register_action(|workspace, action: &ShowSession, window, cx| {
            let Ok(session_id) = solution_agent::SolutionSessionId::parse(&action.session_id)
            else {
                return;
            };
            if let Some(panel) = workspace.panel::<ConsolePanel>(cx) {
                panel.update(cx, |panel, cx| panel.show_session(session_id, window, cx));
            }
        });
        workspace.register_action(ConsolePanel::handle_new_terminal);
    })
    .detach();
}
