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
        /// Opens a new AI-chat tab in the console panel.
        NewChat,
    ]
);

/// Shows (and activates) the console-panel chat tab for a specific AI
/// session, spawning the tab if it isn't in the strip yet. This is the
/// deterministic "bring session N into view" seam that the live
/// `ChatProviderEvent::TabsChanged` path can't guarantee (a session pinned
/// out-of-band — e.g. via the `workspace.open_session` RPC — may never
/// surface a desktop tab). Primarily for MCP-driven UI verification: dispatch
/// via `windows.dispatch_action` with `{"session_id": "…"}` then
/// `windows.screenshot`. `session_id` is a `SolutionSessionId` string (as
/// returned by `solution_agent.list_sessions`).
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
