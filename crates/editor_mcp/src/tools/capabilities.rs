//! `editor.capabilities` MCP tool — protocol probe for clients.
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::AsyncApp;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

/// Editor MCP capability probe — returns protocol version, server version,
/// supported event kinds, and any experimental flags currently enabled.
#[derive(Debug, Clone, Default, JsonSchema)]
pub struct CapabilitiesParams {}

// Custom deserializer accepts JSON null, missing, or `{}` — all valid forms
// for a tool whose input schema declares no required fields. Without this,
// `serde_json::from_value(Value::Null)` rejects the unit-style struct, so
// MCP clients that omit `arguments` (the dispatcher routes that to `Null`)
// would fail before reaching `run`.
impl<'de> Deserialize<'de> for CapabilitiesParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let _ = serde::de::IgnoredAny::deserialize(de)?;
        Ok(CapabilitiesParams {})
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Capabilities {
    pub protocol_version: String,
    pub editor_mcp_version: String,
    pub supported_event_kinds: Vec<String>,
    pub experiments: Vec<String>,
    /// Absolute path of the running editor binary (`std::env::current_exe`).
    pub binary_path: String,
    /// Local-time mtime of that binary file, i.e. when this build was
    /// written to disk. Lets a client confirm the *running* process is the
    /// freshly-built binary rather than a stale one, without trusting the
    /// operator's memory of whether they restarted. `<unknown>` if the
    /// path / metadata can't be read.
    pub binary_built_at: String,
    /// Monotonic chat-wire schema version. Bumped on every breaking change to
    /// the session/entry wire DTOs. Clients refuse to operate against a server
    /// whose value exceeds what they support, prompting the user to update.
    pub wire_schema_version: u32,
}

/// Resolve `(path, mtime-as-local-time-string)` for the running binary.
/// Returns `<unknown>` placeholders rather than failing — the probe is a
/// best-effort diagnostic, not a critical path.
fn running_binary_build_info() -> (String, String) {
    let unknown = || ("<unknown>".to_string(), "<unknown>".to_string());
    let Ok(path) = std::env::current_exe() else {
        return unknown();
    };
    let built_at = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .map(|mtime| {
            let dt: chrono::DateTime<chrono::Local> = mtime.into();
            dt.format("%Y-%m-%d %H:%M:%S %:z").to_string()
        })
        .unwrap_or_else(|_| "<unknown>".to_string());
    (path.display().to_string(), built_at)
}

#[derive(Clone)]
pub struct CapabilitiesTool;

impl McpServerTool for CapabilitiesTool {
    type Input = CapabilitiesParams;
    type Output = Capabilities;
    const NAME: &'static str = "editor.capabilities";

    async fn run(
        &self,
        _input: Self::Input,
        _cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let (binary_path, binary_built_at) = running_binary_build_info();
        let caps = Capabilities {
            protocol_version: "2024-11-05".to_string(),
            editor_mcp_version: env!("CARGO_PKG_VERSION").to_string(),
            supported_event_kinds: SUPPORTED_EVENT_KINDS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            experiments: vec![],
            binary_path,
            binary_built_at,
            // v2: added `workspace.*` MCP namespace; renamed `SolutionSummary.window_open`
            // to `open` and `solution_agent.close_session` to `solution_agent.delete_session`.
            // v3 (per-source streams, HARD CUTOVER): `solution_agent.get_session` /
            // `get_session_changes` dropped the flat `active_subagents` +
            // `subagent_filter` model for the per-stream `streams` descriptors +
            // `stream_id` selector; `entries` / `changed_entries` `index` is now
            // STREAM-LOCAL and the delta cursor is per-stream `seq`.
            // v4 (per-source streams — shells + background-agents folded onto
            // `streams`): background shells now ride the wire as `kind: shell`
            // streams; background agents render as their `kind: teammate` demux
            // stream; the separate `get_session_background_{shells,agents}` tools
            // are removed. HARD CUTOVER.
            // v5 (per-source streams — labels on the stream):
            // `SessionSummary.active_subagents` removed; a teammate stream's
            // friendly label now rides `StreamDto.label`; the
            // `agent_session_active_subagents_changed` notification is a bare
            // `{session_id}` dirty-poke. HARD CUTOVER.
            wire_schema_version: 5,
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "editor_mcp v{} · binary built {}",
                    caps.editor_mcp_version, caps.binary_built_at
                ),
            }],
            structured_content: caps,
        })
    }
}

pub(crate) const SUPPORTED_EVENT_KINDS: &[&str] = &[
    "operation_progress",
    "operation_completed",
    "buffer_opened",
    "buffer_closed",
    "buffer_saved",
    "buffer_dirty_changed",
    "selection_changed",
    "diagnostic_updated",
    "solution_changed",
    "solution_active_changed",
    "solution_active_member_changed",
    "window_focused",
    "lsp_started",
    "lsp_stopped",
    "cli_args_received",
    "server_shutting_down",
    "agent_session_created",
    "agent_session_closed",
    "agent_session_context_reset",
    "agent_session_state_changed",
    "agent_session_title_changed",
    "agent_session_message_appended",
    "agent_session_notification_sent",
    "agent_session_queue_changed",
    // Bare `{ session_id }` dirty-poke (post-6d-tail-2 it no longer carries a
    // subagent list — the mobile just re-polls `streams` on it).
    "agent_session_active_subagents_changed",
    // Content-free, coalesced "transcript advanced — re-poll" signal. Carries
    // `{ session_id, current_seq }`; the mobile polls `get_session_changes` to
    // convergence (cursor >= current_seq) on it, so a single delivered dirty
    // heals a view left short by lost per-entry pokes.
    "agent_session_dirty",
];
