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
    ///
    /// The shipped mobile gate is an EQUALITY gate wired straight to a terminal
    /// "incompatible server" screen with no retry ladder, so a bump is a hard
    /// cutover that bricks every phone in the field the moment the desktop
    /// updates. Additive, both-peers-must-agree wire behaviours therefore ride
    /// [`Capabilities::wire_features`] instead — only a genuinely BREAKING DTO
    /// change may bump this number.
    pub wire_schema_version: u32,
    /// Additive feature tokens for wire behaviours that need BOTH peers to
    /// have been built with them. A client must not send a gated request
    /// parameter unless the corresponding token was present in the last
    /// successful `editor.capabilities` response on the CURRENT connection.
    ///
    /// ALWAYS serialised (empty vec when nothing is on): the client reads
    /// ABSENCE as "server predates feature negotiation", which is a different
    /// fact from "server has no features on". Every other new field on this
    /// struct follows the ordinary absent-means-old-peer rule; this one is the
    /// deliberate exception.
    pub wire_features: Vec<String>,
    /// Fresh UUIDv4 minted once per editor process. A client that observed a
    /// different value than the one in force when it dispatched an ambiguous
    /// send must NOT rely on `csid_dedupe` for that send — the dedupe table is
    /// in-memory and died with the old process.
    pub server_instance_id: String,
    /// How long a `spk_client_send_id` stays deduplicated, in milliseconds.
    /// Present only when `csid_dedupe` is advertised; derived from
    /// [`CSID_DEDUPE_WINDOW`], never written as a second literal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csid_dedupe_window_ms: Option<u64>,
}

/// The chat-wire schema version this build speaks. See
/// [`Capabilities::wire_schema_version`] for why it is not a knob.
pub const WIRE_SCHEMA_VERSION: u32 = 6;

/// N-29: `known_entries` on `solution_agent.get_session_changes` plus the
/// `markdown_len` / `markdown_prefix_len` / `markdown_tail` response fields.
pub const FEATURE_ENTRY_BODY_DELTA: &str = "entry_body_delta";
/// N-37: the `omit_preview_when_markdown` request parameter.
pub const FEATURE_OMIT_PREVIEW: &str = "omit_preview";
/// N-05: `spk_client_send_id` idempotency on `send_message_blocks` plus the
/// `delivery` field on its result.
pub const FEATURE_CSID_DEDUPE: &str = "csid_dedupe";
/// N-34 (narrow slice): the `suppress_kinds` parameter on `editor.subscribe`,
/// enforced per connection in the remote-control proxy.
pub const FEATURE_QUIET_MESSAGE_APPENDED: &str = "quiet_message_appended";

/// How long a `spk_client_send_id` stays deduplicated. 24 h — exactly the
/// mobile offline queue's TTL, which is the longest a message can sit on the
/// phone's disk and still be replayed; the two MUST stay equal.
///
/// Lives here rather than next to the dedupe table in `solution_agent` because
/// `editor_mcp` is the lower crate in the dependency order and this is the
/// value that goes on the wire. `solution_agent::store` re-exports it, so there
/// is exactly one literal.
pub const CSID_DEDUPE_WINDOW: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// The single source of truth for what this build advertises. Tests assert
/// membership against the consts above, never against string literals.
pub fn wire_features() -> Vec<String> {
    vec![
        FEATURE_ENTRY_BODY_DELTA.to_string(),
        FEATURE_OMIT_PREVIEW.to_string(),
        FEATURE_CSID_DEDUPE.to_string(),
        FEATURE_QUIET_MESSAGE_APPENDED.to_string(),
    ]
}

/// Stable-for-the-process identity of this editor. Regenerated on every
/// restart on purpose: it is what tells a client that the in-memory
/// `spk_client_send_id` dedupe table it was relying on no longer exists.
pub fn server_instance_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::new_v4().to_string())
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
        let features = wire_features();
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
            // v6 (numeric identity — HARD CUTOVER): the identity migration made
            // Solution / member / catalog ids surrogate counters, so every wire
            // field carrying one is now a JSON **number** (`i64`), not a quoted
            // string: `SolutionSummary.id`, `SessionSummary.solution_id` /
            // `member_id`, `WorkspaceSolution.id`, the `workspace.*` payloads'
            // `solution_id`, `catalog_id` everywhere, and the `solution_id`
            // *parameter* of the workspace lifecycle tools. Session ids and
            // agent ids stay strings. This shape shipped with the migration but
            // was not versioned then; v6 makes the break explicit so an
            // un-migrated client gates instead of crash-decoding a number into a
            // string field.
            wire_schema_version: WIRE_SCHEMA_VERSION,
            wire_features: features.clone(),
            server_instance_id: server_instance_id().to_string(),
            csid_dedupe_window_ms: features
                .iter()
                .any(|f| f == FEATURE_CSID_DEDUPE)
                .then(|| CSID_DEDUPE_WINDOW.as_millis() as u64),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A reflexive bump would brick every phone already in the field: the
    /// shipped client's gate is `serverWire > SUPPORTED` → terminal screen +
    /// teardown, with no retry ladder. Additive behaviours ride
    /// `wire_features`; only a breaking DTO change may touch this number, and
    /// then only as a coordinated hard cutover.
    #[test]
    fn wire_schema_version_is_still_six() {
        assert_eq!(
            6, WIRE_SCHEMA_VERSION,
            "wire_schema_version is an equality gate on the client — do not bump it \
             for an additive feature; add a `wire_features` token instead"
        );
    }

    #[test]
    fn wire_features_always_serialised() {
        let mut caps = sample_capabilities();
        caps.wire_features.clear();
        caps.csid_dedupe_window_ms = None;
        let json = serde_json::to_value(&caps).expect("serialise capabilities");
        let object = json.as_object().expect("capabilities is an object");
        assert!(
            object.contains_key("wire_features"),
            "an EMPTY feature list must still be emitted — its absence is the \
             client's signal that the server predates feature negotiation"
        );
        assert_eq!(object["wire_features"], serde_json::json!([]));
        assert!(
            !object.contains_key("csid_dedupe_window_ms"),
            "the window is emitted only alongside the csid_dedupe token"
        );
    }

    #[test]
    fn server_instance_id_is_stable_within_the_process() {
        let first = server_instance_id();
        let second = server_instance_id();
        assert!(!first.is_empty());
        assert_eq!(first, second, "one uuid per editor process, not per call");
    }

    #[test]
    fn advertised_features_are_the_named_constants() {
        let features = wire_features();
        for token in [
            FEATURE_ENTRY_BODY_DELTA,
            FEATURE_OMIT_PREVIEW,
            FEATURE_CSID_DEDUPE,
            FEATURE_QUIET_MESSAGE_APPENDED,
        ] {
            assert!(
                features.iter().any(|f| f == token),
                "{token} must be advertised"
            );
        }
        let mut sorted = features.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), features.len(), "duplicates are not permitted");
    }

    /// The tokens are a wire contract shared byte-for-byte with the mobile
    /// client's `WireFeature` object. Asserting the literals here is what makes
    /// a rename on either side a failing test rather than a silently disabled
    /// feature.
    #[test]
    fn feature_token_spellings_are_frozen() {
        assert_eq!(FEATURE_ENTRY_BODY_DELTA, "entry_body_delta");
        assert_eq!(FEATURE_OMIT_PREVIEW, "omit_preview");
        assert_eq!(FEATURE_CSID_DEDUPE, "csid_dedupe");
        assert_eq!(FEATURE_QUIET_MESSAGE_APPENDED, "quiet_message_appended");
    }

    #[test]
    fn csid_dedupe_window_matches_constant() {
        let caps = sample_capabilities();
        assert_eq!(
            caps.csid_dedupe_window_ms,
            Some(CSID_DEDUPE_WINDOW.as_millis() as u64),
            "the advertised window must be DERIVED from the constant the store \
             evicts by, never written as a second literal"
        );
        assert_eq!(caps.csid_dedupe_window_ms, Some(86_400_000));
    }

    fn sample_capabilities() -> Capabilities {
        let features = wire_features();
        Capabilities {
            protocol_version: "2024-11-05".to_string(),
            editor_mcp_version: env!("CARGO_PKG_VERSION").to_string(),
            supported_event_kinds: Vec::new(),
            experiments: Vec::new(),
            binary_path: "<test>".to_string(),
            binary_built_at: "<test>".to_string(),
            wire_schema_version: WIRE_SCHEMA_VERSION,
            csid_dedupe_window_ms: features
                .iter()
                .any(|f| f == FEATURE_CSID_DEDUPE)
                .then(|| CSID_DEDUPE_WINDOW.as_millis() as u64),
            wire_features: features,
            server_instance_id: server_instance_id().to_string(),
        }
    }
}
