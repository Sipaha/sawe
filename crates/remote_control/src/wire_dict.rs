//! Preset DEFLATE dictionary for the wire codec — the server-side twin of the
//! mobile `core/.../WireDictionary.kt`. The bytes MUST stay byte-identical; the
//! `dictionary_adler32_matches_cross_language_pin` test on each side pins the
//! Adler-32 (the DICTID zlib stamps) so any drift fails a test on both sides.
//!
//! ASCII-only on purpose so the literal is trivially identical across Rust and
//! Kotlin. Most-frequent substrings sit at the END (shortest back-references).

/// Dictionary id for the proto-v1 preset dictionary. Bump (never mutate v1 in
/// place) if the vocabulary changes, so peers negotiate the id cleanly.
pub(crate) const WIRE_DICT_PROTO_V1: u8 = 1;

/// Dictionary id meaning "raw DEFLATE, no preset dictionary".
pub(crate) const WIRE_DICT_NONE: u8 = 0;

/// Adler-32 of [`WIRE_DICT_PROTO_V1_BYTES`]. Pinned identically in Kotlin.
/// Consumed only by the cross-language parity test.
#[allow(dead_code)]
pub(crate) const WIRE_DICT_PROTO_V1_ADLER32: u32 = 639723996;

pub(crate) const WIRE_DICT_PROTO_V1_BYTES: &[u8] = concat!(
    "remote.solution_agent.list_solutions remote.solution_agent.solution_details ",
    "remote.solution_agent.list_sessions remote.solution_agent.get_session ",
    "remote.solution_agent.read_session_history remote.solution_agent.send_message ",
    "remote.solution_agent.start_compact remote.solution_agent.reset_context ",
    "remote.solution_agent.rename_session remote.solution_agent.delete_session ",
    "remote.solution_agent.cancel_turn remote.solution_agent.authorize_tool_call ",
    "remote.solution_agent.get_session_background_shells ",
    "remote.solution_agent.get_session_background_agents ",
    "remote.solution_agent.upload_init remote.solution_agent.upload_chunk ",
    "remote.solution_agent.upload_finish remote.solution_agent.upload_status ",
    "session_state_changed session_entry_appended session_entry_updated ",
    "session_queue_changed session_created session_deleted agent_session_context_reset ",
    "workspace_session_metrics_changed upload_chunk_acked remote/notification ",
    "awaiting_input waiting_for_confirmation tool_status_started_at_ms ",
    "stopping running pending errored failed rejected canceled assistant ",
    "\"parent_session_id\":\"acp_session_id\":\"active_subagents\":[\"background_agents\"",
    "\"last_activity_at\":\"total_tokens\":\"max_tokens\":\"created_at\":\"context_count\":",
    "\"solution_id\":\"agent_id\":\"session_id\":\"display_name\":\"total_size\":",
    "\"received_bytes\":\"upload_id\":\"started_at_ms\":\"created_ms\":\"tool_call\":{",
    "\"entries\":[\"sessions\":[\"total_count\":\"title\":\"state\":{\"kind\":\"",
    "\"index\":\"role\":\"name\":\"status\":\"args\":\"text\":\"mime\":\"cwd\":\"",
    "tool_call user assistant idle done \"params\":{\"kind\":\"",
    "{\"jsonrpc\":\"2.0\",\"id\":,\"method\":\"remote/notification\",\"params\":{",
    "\"result\":{\"error\":{\"code\":,\"message\":\"",
)
.as_bytes();

/// Resolve the dictionary bytes for a negotiated id. `None` for the no-dict id.
pub(crate) fn dictionary_for(dict_id: u8) -> Option<&'static [u8]> {
    match dict_id {
        WIRE_DICT_NONE => None,
        WIRE_DICT_PROTO_V1 => Some(WIRE_DICT_PROTO_V1_BYTES),
        _ => None,
    }
}
