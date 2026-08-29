use gpui::Action;
use schemars::JsonSchema;
use serde::Deserialize;

gpui::actions!(
    console_panel,
    [
        /// Toggles focus on the console panel.
        ToggleFocus,
        /// Opens a new terminal tab in the console panel.
        NewTerminal,
        /// Creates a new AI-chat session and shows it in the Solution band.
        NewChat,
        /// Toggles the Solution band's dialog half (`ctrl-shift-a`).
        /// Collapses it if a session is currently showing
        /// (`SolutionAgentStore::set_active_dialog_session(solution_id, None,
        /// …)`); if collapsed, reopens on
        /// `SolutionAgentStore::last_dialog_session` (the last session shown
        /// this run), falling back to the first session in `tab_order`, and
        /// doing nothing if the solution has no sessions at all. See
        /// `SolutionAgentStore::toggle_dialog_session` for the full
        /// precedence and `handle_toggle_dialog` for the handler.
        ///
        /// Bound only in the `"Workspace"` keymap context, deliberately NOT
        /// overriding the more specific `"Terminal"` context (`ctrl-shift-a`
        /// already means `editor::SelectAll` there) or, on macOS, the broad
        /// `"Editor"` context (`ctrl-shift-a` already means
        /// `editor::SelectToBeginningOfLine` there). Consequence: on macOS
        /// this hotkey does not fire while any editor — including the
        /// dialog's own compose box — has focus; the user must click out of
        /// it first. Accepted per the phase-2b task-8 ruling rather than
        /// breaking `SelectToBeginningOfLine` for every macOS editor.
        ToggleDialog,
    ]
);

/// Selects a specific AI session as the Solution band's active dialog
/// (`SolutionAgentStore::set_active_dialog_session`), the deterministic
/// "bring session N into view" seam a session pinned out-of-band — e.g. via
/// the `workspace.open_session` RPC — cannot otherwise guarantee. Primarily
/// for MCP-driven UI verification: dispatch via `windows.dispatch_action`
/// with `{"session_id": "…"}` then `windows.screenshot`. `session_id` is a
/// `SolutionSessionId` string (as returned by `solution_agent.list_sessions`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = console_panel)]
#[serde(deny_unknown_fields)]
pub struct ShowSession {
    pub session_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // `windows.dispatch_action` builds this action from a JSON `args` blob via
    // `cx.build_action`, so the serde contract IS the MCP surface. Guard it:
    // the field must deserialize by name, and unknown keys must be rejected so
    // a typo'd param fails loudly (build_action errors) instead of silently
    // showing nothing.
    #[test]
    fn show_session_deserializes_session_id() {
        let action: ShowSession =
            serde_json::from_value(serde_json::json!({ "session_id": "abc123" })).unwrap();
        assert_eq!(action.session_id, "abc123");

        assert!(
            serde_json::from_value::<ShowSession>(
                serde_json::json!({ "session_id": "x", "oops": 1 })
            )
            .is_err(),
            "unknown fields must be rejected (deny_unknown_fields)"
        );
    }
}
