//! Unified bottom-dock panel hosting terminal tabs. AI-chat sessions used to
//! live here too; phase 2a task 5 moved them to the Solution band + the
//! status-bar `solution_agent::session_tab_strip` — `NewChat` and
//! `ShowSession` below now drive that shared per-solution selection
//! (`SolutionAgentStore::{active_dialog_session,set_active_dialog_session}`)
//! instead of touching `ConsolePanel`.

mod actions;
mod console_panel_settings;
mod panel;
mod terminal_provider;

use gpui::{Context, SharedString, TaskExt as _, Window};
use solution_agent::SolutionSessionId;
use solution_agent::claude_adapter::CLAUDE_ACP_AGENT_ID;
use solution_agent::store::SolutionAgentStore;
use workspace::Workspace;

pub use actions::{NewChat, NewTerminal, ShowSession, ToggleFocus};
pub use console_panel_settings::ConsolePanelSettings;
pub use panel::{ConsolePanel, ConsoleTab};
pub use terminal_provider::TerminalProvider;

pub fn init(cx: &mut gpui::App) {
    use settings::Settings;
    ConsolePanelSettings::register(cx);

    cx.observe_new(|workspace: &mut workspace::Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &NewTerminal, window, cx| {
            // No project directory to run in (an empty solution has 0 member
            // projects) → refuse. A non-empty solution or a plain folder with a
            // visible worktree is allowed.
            if !panel::workspace_has_project(workspace, cx) {
                return;
            }
            if let Some(panel) = workspace.panel::<ConsolePanel>(cx) {
                panel.update(cx, |panel, cx| panel.add_terminal_tab(None, window, cx));
            }
        });
        workspace.register_action(handle_new_chat);
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ConsolePanel>(window, cx);
        });
        workspace.register_action(handle_show_session);
        workspace.register_action(ConsolePanel::handle_new_terminal);
    })
    .detach();
}

/// `NewChat` handler: creates a session under the workspace's active
/// solution and selects it as the Solution band's active dialog. This
/// handler holds `&mut Workspace` (it runs inside
/// `workspace.register_action`), so it reads the project straight off that
/// reference rather than re-acquiring the `Workspace` entity's lease
/// through a weak handle — doing the latter is the `double_lease_panic`
/// that bit this exact action twice before it was routed through
/// `ConsolePanel` (`ConsolePanel::add_chat_tab`'s history, when this action
/// used to go through a panel lookup). Guarded here by
/// `panel::tests::new_chat_action_does_not_double_lease_the_workspace`.
fn handle_new_chat(
    workspace: &mut Workspace,
    _: &NewChat,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(solution_id) = panel::active_solution_id_for_workspace(workspace, cx) else {
        return;
    };
    let project = workspace.project().clone();
    let store = SolutionAgentStore::global(cx);
    // A chat is always solution-scoped and rooted at `solution.root`
    // (`cwd: None`) — never the active member's folder. This used to be a
    // shared `new_chat_cwd` helper both creation entry points routed
    // through (Critical 1, 2026-08-26 final review); now there is only one
    // entry point (this action; the "+" popover dispatches it too), so
    // there is no second call site left to diverge.
    let task = store.update(cx, |store, cx| {
        store.create_session_with_cwd(
            solution_id,
            SharedString::from(CLAUDE_ACP_AGENT_ID),
            project,
            None,
            None,
            None,
            cx,
        )
    });
    cx.spawn(async move |_workspace, cx| {
        let session_id = task.await?;
        cx.update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_active_dialog_session(solution_id, Some(session_id), cx);
            });
        });
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// `ShowSession` handler: selects `action.session_id` as its own solution's
/// active dialog. Looks the solution up from the session itself (not from
/// the workspace's active member) so it works regardless of which project a
/// Solution's workspace currently has selected. The notification-click path
/// (FORK.md #36, `crates/zed/src/notification_focus.rs`) does not dispatch
/// this action — a background notification click has no reliable focused
/// view to dispatch from — it instead calls
/// `SolutionAgentStore::set_active_dialog_session` directly after activating
/// the right workspace; the logic here is the same two calls for the
/// in-app/MCP dispatch path.
fn handle_show_session(
    _workspace: &mut Workspace,
    action: &ShowSession,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Ok(session_id) = SolutionSessionId::parse(&action.session_id) else {
        return;
    };
    let Some(solution_id) = SolutionAgentStore::global(cx)
        .read(cx)
        .session(session_id)
        .map(|session| session.read(cx).solution_id)
    else {
        return;
    };
    SolutionAgentStore::global(cx).update(cx, |store, cx| {
        store.set_active_dialog_session(solution_id, Some(session_id), cx);
    });
}
