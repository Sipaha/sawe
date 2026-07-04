//! Route a click on a "Sawe — …" desktop notification back to the session it
//! came from: raise the editor window, activate that session's Solution tab,
//! reveal + focus the ConsolePanel, and select the session's chat tab.
//!
//! The notifier (`solution_agent::notifier::dispatch`) tags each notification
//! with the id `dev.sawe.session-{session_id}` and a `default_action("open")`,
//! so the freedesktop portal fires an `ActionInvoked` signal when the body is
//! clicked. This module owns the single long-lived listener for that signal
//! (Linux/FreeBSD; other platforms have no portal backend yet) and the
//! main-thread focus routing. It lives in the `zed` crate because the routing
//! spans `solution_agent` + `solutions` + `workspace` + `console_panel`, which
//! only the top-level app crate may depend on together.

use console_panel::ConsolePanel;
use gpui::App;
use solution_agent::model::SolutionSessionId;
use solution_agent::store::SolutionAgentStore;
use solutions::SolutionStore;
use workspace::{MultiWorkspace, Workspace};

/// Prefix of the freedesktop notification id minted in
/// `solution_agent::notifier::dispatch`. Kept in sync with that call site.
const SESSION_NOTIFICATION_PREFIX: &str = "dev.sawe.session-";

/// Spawn the notification-click listener. Idempotent-safe to call once at
/// startup (after the stores + panels are initialised). No-op on platforms
/// without a freedesktop notification portal.
pub fn init(cx: &mut App) {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        use ashpd::desktop::notification::NotificationProxy;
        use futures::StreamExt as _;

        // A single proxy subscribed to `ActionInvoked` receives clicks for
        // every notification we sent, regardless of which per-dispatch proxy
        // sent it (the signal is on the shared portal object).
        let Ok(proxy) = NotificationProxy::new().await else {
            log::warn!("notification_focus: could not open the notification portal; clicks won't focus");
            return;
        };
        let Ok(mut actions) = proxy.receive_action_invoked().await else {
            log::warn!("notification_focus: could not subscribe to ActionInvoked");
            return;
        };
        while let Some(action) = actions.next().await {
            let Some(session_id) = session_id_from_notification_id(action.id()) else {
                continue;
            };
            cx.update(|cx| focus_session(session_id, cx));
        }
    })
    .detach();
    let _ = cx;
}

/// Parse a freedesktop notification id back into the `SolutionSessionId` it
/// encodes, or `None` when the id isn't one of ours (a foreign app's
/// notification click on the shared portal object). Inverse of the id minted in
/// `solution_agent::notifier::dispatch`.
fn session_id_from_notification_id(id: &str) -> Option<SolutionSessionId> {
    let raw = id.strip_prefix(SESSION_NOTIFICATION_PREFIX)?;
    SolutionSessionId::parse(raw).ok()
}

/// Bring the given session fully into view on the main thread: raise its
/// window, activate its Solution, reveal + focus the ConsolePanel, and select
/// its chat tab. Silent no-op if the session / its solution / its window can no
/// longer be resolved (closed since the notification fired).
pub fn focus_session(session_id: SolutionSessionId, cx: &mut App) {
    let Some(store) = SolutionAgentStore::try_global(cx) else {
        return;
    };
    let Some(session) = store.read(cx).session(session_id) else {
        return;
    };
    let solution_id = session.read(cx).solution_id.clone();

    // Each editor window's root is a `MultiWorkspace`; find the one holding the
    // session's Solution and drive the focus sequence inside it.
    let windows: Vec<_> = cx
        .windows()
        .into_iter()
        .filter_map(|w| w.downcast::<MultiWorkspace>())
        .collect();
    for handle in windows {
        let focused = handle
            .update(cx, |multi_workspace, window, cx| {
                let Some(target) = workspace_for_solution(multi_workspace, &solution_id, cx) else {
                    return false;
                };
                // 1. raise the OS window, 2. activate the Solution's tab.
                window.activate_window();
                multi_workspace.activate(target.clone(), None, window, cx);
                // 3. reveal + focus the console dock, 4. select the session tab.
                target.update(cx, |workspace, cx| {
                    workspace.focus_panel::<ConsolePanel>(window, cx);
                    if let Some(panel) = workspace.panel::<ConsolePanel>(cx) {
                        panel.update(cx, |panel, cx| panel.show_session(session_id, window, cx));
                    }
                });
                true
            })
            .unwrap_or(false);
        if focused {
            break;
        }
    }
}

/// Find the retained `Workspace` inside `multi_workspace` whose project holds a
/// worktree mapping to `solution_id`, mirroring `solutions_ui::switch`'s
/// worktree→solution resolution.
fn workspace_for_solution(
    multi_workspace: &MultiWorkspace,
    solution_id: &solutions::SolutionId,
    cx: &App,
) -> Option<gpui::Entity<Workspace>> {
    let store = SolutionStore::global(cx);
    let store = store.read(cx);
    multi_workspace
        .workspaces()
        .find(|workspace| {
            let project = workspace.read(cx).project().clone();
            project.read(cx).worktrees(cx).any(|tree| {
                store
                    .solution_for_path(&tree.read(cx).abs_path())
                    .is_some_and(|solution| &solution.id == solution_id)
            })
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_our_notification_id_and_ignores_others() {
        let sid = SolutionSessionId::new();
        let id = format!("{SESSION_NOTIFICATION_PREFIX}{sid}");
        assert_eq!(session_id_from_notification_id(&id), Some(sid));

        // Foreign / malformed ids are ignored (a click on another app's
        // notification lands on the same shared portal signal).
        assert_eq!(session_id_from_notification_id("org.gnome.Contrast"), None);
        assert_eq!(
            session_id_from_notification_id("dev.sawe.session-not-a-valid-id!!"),
            None
        );
        assert_eq!(session_id_from_notification_id(""), None);
    }
}
