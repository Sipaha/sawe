//! `remote.*` allow-list: maps WS-side method names to upstream MCP tool
//! names, and filters which `editor/notification` kinds get forwarded out
//! to the WebSocket client.
//!
//! Method allow-list is the only authorisation gate — the post-HMAC
//! WebSocket session has carte blanche on whatever methods this module
//! says yes to. Everything else returns JSON-RPC -32601 ("method not
//! found"), so reconnaissance can't distinguish "banned" from "typo".
//!
//! Notification filter is a BLOCK-list at the fan-out layer: the upstream
//! server gladly fires every kind the client subscribed to, and we drop
//! the disallowed ones before they hit the WS write. This keeps the
//! upstream `editor.subscribe` protocol untouched and the filter
//! self-contained.

/// Translate a `remote.*` method name to the bare upstream tool name.
/// Returns `None` for any method outside the allow-list — the caller
/// reports -32601.
pub fn translate(method: &str) -> Option<&'static str> {
    match method {
        "remote.editor.capabilities" => Some("editor.capabilities"),
        "remote.editor.subscribe" => Some("editor.subscribe"),
        "remote.editor.unsubscribe" => Some("editor.unsubscribe"),
        "remote.editor.list_subscriptions" => Some("editor.list_subscriptions"),
        "remote.solutions.list" => Some("solutions.list"),
        "remote.solutions.get" => Some("solutions.get"),
        "remote.solutions.open" => Some("solutions.open"),
        "remote.solutions.create" => Some("solutions.create"),
        "remote.solutions.delete" => Some("solutions.delete"),
        "remote.solutions.add_member" => Some("solutions.add_member"),
        "remote.solutions.add_empty_member" => Some("solutions.add_empty_member"),
        "remote.solutions.remove_member" => Some("solutions.remove_member"),
        "remote.catalog.list" => Some("catalog.list"),
        "remote.catalog.remove_project" => Some("catalog.remove_project"),
        "remote.solution_agent.list_agents" => Some("solution_agent.list_agents"),
        "remote.solution_agent.list_sessions" => Some("solution_agent.list_sessions"),
        "remote.solution_agent.get_session" => Some("solution_agent.get_session"),
        "remote.solution_agent.get_session_changes" => {
            Some("solution_agent.get_session_changes")
        }
        "remote.solution_agent.get_session_entry" => Some("solution_agent.get_session_entry"),
        "remote.solution_agent.create_session" => Some("solution_agent.create_session"),
        "remote.solution_agent.delete_session" => Some("solution_agent.delete_session"),
        "remote.solution_agent.send_message" => Some("solution_agent.send_message"),
        "remote.solution_agent.send_message_blocks" => Some("solution_agent.send_message_blocks"),
        "remote.solution_agent.cancel_turn" => Some("solution_agent.cancel_turn"),
        "remote.solution_agent.authorize_tool_call" => Some("solution_agent.authorize_tool_call"),
        "remote.solution_agent.get_session_children" => Some("solution_agent.get_session_children"),
        "remote.solution_agent.get_session_background_shells" => {
            Some("solution_agent.get_session_background_shells")
        }
        "remote.solution_agent.get_session_background_agents" => {
            Some("solution_agent.get_session_background_agents")
        }
        "remote.solution_agent.rename_session" => Some("solution_agent.rename_session"),
        "remote.solution_agent.restart_agent" => Some("solution_agent.restart_agent"),
        "remote.solution_agent.reset_context" => Some("solution_agent.reset_context"),
        "remote.solution_agent.start_compact" => Some("solution_agent.start_compact"),
        "remote.solution_agent.upload_init" => Some("solution_agent.upload_init"),
        "remote.solution_agent.upload_status" => Some("solution_agent.upload_status"),
        "remote.solution_agent.upload_finish" => Some("solution_agent.upload_finish"),
        "remote.solution_agent.upload_abort" => Some("solution_agent.upload_abort"),
        // Unified open-workspace (wire schema v2). The bulk read +
        // closed-list picker query + four lifecycle tools that drive
        // the mobile WorkspaceScreen. Non-destructive — these don't
        // expose file or project surfaces, only solution / session
        // open/close + the corresponding seq-ack.
        "remote.workspace.snapshot" => Some("workspace.snapshot"),
        "remote.workspace.list_solutions" => Some("workspace.list_solutions"),
        "remote.workspace.open_solution" => Some("workspace.open_solution"),
        "remote.workspace.close_solution" => Some("workspace.close_solution"),
        "remote.workspace.open_session" => Some("workspace.open_session"),
        "remote.workspace.close_session" => Some("workspace.close_session"),
        _ => None,
    }
}

/// Forward `agent_session_*` events to the WS client, drop everything
/// else. The `kind` lives at `params.kind` on the upstream notification
/// frame — see `crates/editor_mcp/src/notifications.rs::emit` and
/// `crates/editor_mcp/tests/notifications_e2e_test.rs` for the on-wire
/// shape.
///
/// Block-list rationale: buffer/LSP/diagnostic local-state events leak
/// filesystem and project detail we don't want the Android client poking
/// at. The agent-session events are exactly what an Android pager-like
/// client needs to stream a turn live; the solution member-add + change
/// events drive the mobile project-registry UI (ghost rows + list refresh).
pub fn should_forward_event(kind: &str) -> bool {
    // `agent_session_*` covers per-turn streaming; `upload_*` covers
    // chunked-upload progress / errors emitted by the binary-frame path
    // in `listener.rs`. The `solution_member_add_*` + `solution_changed`
    // kinds let the mobile client render clone progress and refresh the
    // member list after an add. Mobile subscribes to all of these.
    kind.starts_with("agent_session_")
        || kind.starts_with("upload_")
        || kind.starts_with("workspace.")
        || kind == "solution_member_add_progress"
        || kind == "solution_member_add_completed"
        || kind == "solution_changed"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_list_round_trip() {
        // Every documented allow-listed method translates to its bare
        // counterpart. The strings are paired by hand; if you add an
        // entry to `translate`, mirror it here.
        let cases = &[
            ("remote.editor.capabilities", "editor.capabilities"),
            ("remote.editor.subscribe", "editor.subscribe"),
            ("remote.editor.unsubscribe", "editor.unsubscribe"),
            (
                "remote.editor.list_subscriptions",
                "editor.list_subscriptions",
            ),
            ("remote.solutions.list", "solutions.list"),
            ("remote.solutions.get", "solutions.get"),
            ("remote.solutions.open", "solutions.open"),
            ("remote.solutions.create", "solutions.create"),
            ("remote.solutions.delete", "solutions.delete"),
            ("remote.solutions.add_member", "solutions.add_member"),
            (
                "remote.solutions.add_empty_member",
                "solutions.add_empty_member",
            ),
            ("remote.solutions.remove_member", "solutions.remove_member"),
            ("remote.catalog.list", "catalog.list"),
            ("remote.catalog.remove_project", "catalog.remove_project"),
            (
                "remote.solution_agent.list_agents",
                "solution_agent.list_agents",
            ),
            (
                "remote.solution_agent.list_sessions",
                "solution_agent.list_sessions",
            ),
            (
                "remote.solution_agent.get_session",
                "solution_agent.get_session",
            ),
            (
                "remote.solution_agent.get_session_changes",
                "solution_agent.get_session_changes",
            ),
            (
                "remote.solution_agent.get_session_entry",
                "solution_agent.get_session_entry",
            ),
            (
                "remote.solution_agent.create_session",
                "solution_agent.create_session",
            ),
            (
                "remote.solution_agent.delete_session",
                "solution_agent.delete_session",
            ),
            (
                "remote.solution_agent.send_message",
                "solution_agent.send_message",
            ),
            (
                "remote.solution_agent.send_message_blocks",
                "solution_agent.send_message_blocks",
            ),
            (
                "remote.solution_agent.cancel_turn",
                "solution_agent.cancel_turn",
            ),
            (
                "remote.solution_agent.authorize_tool_call",
                "solution_agent.authorize_tool_call",
            ),
            (
                "remote.solution_agent.get_session_children",
                "solution_agent.get_session_children",
            ),
            (
                "remote.solution_agent.get_session_background_shells",
                "solution_agent.get_session_background_shells",
            ),
            (
                "remote.solution_agent.get_session_background_agents",
                "solution_agent.get_session_background_agents",
            ),
            (
                "remote.solution_agent.rename_session",
                "solution_agent.rename_session",
            ),
            (
                "remote.solution_agent.restart_agent",
                "solution_agent.restart_agent",
            ),
            (
                "remote.solution_agent.reset_context",
                "solution_agent.reset_context",
            ),
            (
                "remote.solution_agent.start_compact",
                "solution_agent.start_compact",
            ),
            (
                "remote.solution_agent.upload_init",
                "solution_agent.upload_init",
            ),
            (
                "remote.solution_agent.upload_status",
                "solution_agent.upload_status",
            ),
            (
                "remote.solution_agent.upload_finish",
                "solution_agent.upload_finish",
            ),
            (
                "remote.solution_agent.upload_abort",
                "solution_agent.upload_abort",
            ),
            ("remote.workspace.snapshot", "workspace.snapshot"),
            (
                "remote.workspace.list_solutions",
                "workspace.list_solutions",
            ),
            ("remote.workspace.open_solution", "workspace.open_solution"),
            (
                "remote.workspace.close_solution",
                "workspace.close_solution",
            ),
            ("remote.workspace.open_session", "workspace.open_session"),
            ("remote.workspace.close_session", "workspace.close_session"),
        ];
        for (wire, bare) in cases {
            assert_eq!(translate(wire), Some(*bare), "for {wire}");
        }
    }

    #[test]
    fn banned_methods_return_none() {
        // File CRUD, project ops, full workspace dumps — explicitly NOT
        // exposed to remote clients per ADR-0003 § "How to apply".
        let banned = &[
            "remote.lsp.start",
            "remote.project.open_file",
            "remote.project.delete_file",
            "remote.workspace.screenshot",
            "remote.windows.send_keystroke",
            "remote.editor.handle_cli_args",
            "editor.capabilities", // bare name without `remote.` prefix
            "remote.catalog.add_project",
        ];
        for method in banned {
            assert_eq!(translate(method), None, "for {method}");
        }
    }

    #[test]
    fn unknown_method_returns_none() {
        assert_eq!(translate(""), None);
        assert_eq!(translate("garbage"), None);
        assert_eq!(translate("remote."), None);
    }

    #[test]
    fn agent_session_kinds_forward() {
        assert!(should_forward_event("agent_session_created"));
        assert!(should_forward_event("agent_session_closed"));
        assert!(should_forward_event("agent_session_state_changed"));
        assert!(should_forward_event("agent_session_title_changed"));
        assert!(should_forward_event("agent_session_message_appended"));
        assert!(should_forward_event("agent_session_notification_sent"));
        assert!(should_forward_event(
            "agent_session_background_shells_changed"
        ));
        assert!(should_forward_event(
            "agent_session_background_agents_changed"
        ));
    }

    #[test]
    fn upload_kinds_forward() {
        // Chunked-upload progress / error events flow back to the mobile
        // client over the same notification pipe — see
        // `docs/plans/2026-05-19-chunked-upload-binary-frames.md`.
        assert!(should_forward_event("upload_chunk_acked"));
        assert!(should_forward_event("upload_chunk_error"));
        // Bare `upload` (no underscore) does NOT match — the prefix
        // gate is exact, same as agent_session_.
        assert!(!should_forward_event("uploadqueued"));
    }

    #[test]
    fn solution_member_and_change_kinds_forward() {
        // The mobile project-registry UI needs these: progress drives the
        // ghost member rows, completed clears them, and `solution_changed`
        // triggers a `solutions.get` refresh after an add.
        assert!(should_forward_event("solution_member_add_progress"));
        assert!(should_forward_event("solution_member_add_completed"));
        assert!(should_forward_event("solution_changed"));
    }

    #[test]
    fn local_state_kinds_are_blocked() {
        assert!(!should_forward_event("solution_active_changed"));
        assert!(!should_forward_event("buffer_opened"));
        assert!(!should_forward_event("buffer_saved"));
        assert!(!should_forward_event("lsp_started"));
        assert!(!should_forward_event("lsp_stopped"));
        assert!(!should_forward_event("diagnostic_updated"));
        assert!(!should_forward_event("operation_progress"));
        assert!(!should_forward_event("operation_completed"));
        assert!(!should_forward_event("window_focused"));
        assert!(!should_forward_event(""));
        // A typo'd `agentsession_` (missing underscore) must NOT match —
        // the prefix is exact, not fuzzy.
        assert!(!should_forward_event("agentsession_created"));
    }
}
