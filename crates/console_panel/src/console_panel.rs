//! Terminal tab strip hosted in the Solution band's utility section
//! (`solution_agent::solution_band`, phase 2a task 6) — NOT a dock panel;
//! `ConsolePanel` keeps `Render`/`Focusable` but has no `Panel` impl.
//! `Workspace::solution_band_utility_item` is the type-erased slot `zed.rs`
//! installs it into and the one other crates resolve it back out of via
//! `console_panel_for_workspace` (decision 91 in `FORK.md`). AI-chat
//! sessions used to live here too; phase 2a task 5 moved them to the
//! Solution band's dialog half + the status-bar
//! `solution_agent::session_tab_strip` — `NewChat` and `ShowSession` below
//! now drive that shared per-solution selection
//! (`SolutionAgentStore::{active_dialog_session,set_active_dialog_session}`)
//! instead of touching `ConsolePanel`.

mod actions;
mod panel;
mod terminal_provider;

use gpui::{Context, Focusable as _, SharedString, TaskExt as _, Window};
use solution_agent::SolutionSessionId;
use solution_agent::claude_adapter::CLAUDE_ACP_AGENT_ID;
use solution_agent::solution_band::SolutionBand;
use solution_agent::store::SolutionAgentStore;
use workspace::Workspace;

pub use actions::{NewChat, NewTerminal, ShowSession, ToggleFocus};
pub use panel::{ConsolePanel, ConsoleTab, console_panel_for_workspace};
pub use terminal_provider::TerminalProvider;

pub fn init(cx: &mut gpui::App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &NewTerminal, window, cx| {
            // No project directory to run in (an empty solution has 0 member
            // projects) → refuse. A non-empty solution or a plain folder with a
            // visible worktree is allowed.
            if !panel::workspace_has_project(workspace, cx) {
                return;
            }
            if let Some(panel) = console_panel_for_workspace(workspace) {
                panel.update(cx, |panel, cx| panel.add_terminal_tab(None, window, cx));
            }
        });
        workspace.register_action(handle_new_chat);
        workspace.register_action(handle_toggle_focus);
        workspace.register_action(handle_show_session);
        workspace.register_action(ConsolePanel::handle_new_terminal);
    })
    .detach();
}

/// `ToggleFocus` (`ctrl-\``) handler. `ConsolePanel` no longer lives in a
/// dock (phase 2a task 6), so this can't go through
/// `Workspace::toggle_panel_focus` any more — it shows/hides/focuses the
/// Solution band's utility section instead. Resolves both the concrete
/// panel (for its `FocusHandle` — the band only holds an `AnyView`, see
/// `solution_band`'s module doc) and the band itself from `Workspace`'s
/// type-erased slots; a no-op if either hasn't been installed (e.g. a
/// workspace with no Solution).
fn handle_toggle_focus(
    workspace: &mut Workspace,
    _: &ToggleFocus,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(console_panel) = console_panel_for_workspace(workspace) else {
        return;
    };
    let Some(band) = workspace
        .solution_band_item()
        .and_then(|item| item.downcast::<SolutionBand>().ok())
    else {
        return;
    };
    let focus_handle = console_panel.focus_handle(cx);
    band.update(cx, |band, cx| {
        band.toggle_utility_focus(&focus_handle, window, cx);
    });
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
    let Some(session) = SolutionAgentStore::global(cx).read(cx).session(session_id) else {
        return;
    };
    // The band would happily paint any session in the store, but the strip
    // only builds tabs for the ones `can_be_active_dialog` admits — an
    // ephemeral helper or a session with no `tab_order` would leave the user a
    // dialog with no tab, and no way to deselect it. Persisted, so it survives
    // a restart; an MCP-driven `ShowSession` must not be able to leave the
    // band reopening on a tab-less sub-agent dialog.
    let (solution_id, selectable) = session.read_with(cx, |session, _| {
        (session.solution_id, session.can_be_active_dialog())
    });
    if !selectable {
        return;
    }
    SolutionAgentStore::global(cx).update(cx, |store, cx| {
        store.set_active_dialog_session(solution_id, Some(session_id), cx);
    });
}
