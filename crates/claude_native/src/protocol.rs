//! Serde types for the `claude` stream-json protocol.
//!
//! Pure (no GPUI, no I/O). Only the fields the connection consumes are
//! modeled; every struct ignores unknown fields and an unknown top-level
//! message `type` parses to [`OutputMessage::Unknown`] rather than erroring,
//! so a future `claude` that adds message kinds (or emits a stray
//! `{"type":"ping"}`) does not break the reader.

use agent_client_protocol::schema as acp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputMessage {
    System(System),
    Assistant(ConversationMessage),
    User(ConversationMessage),
    StreamEvent(StreamEvent),
    Result(ResultMessage),
    ControlRequest(ControlRequestEnvelope),
    ControlResponse(ControlResponseEnvelope),
    #[serde(other)]
    Unknown,
}

impl OutputMessage {
    pub fn parse(line: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(line)?)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum System {
    Init {
        session_id: String,
        #[serde(default)]
        uuid: String,
    },
    SessionStateChanged {
        state: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct StreamEvent {
    pub event: serde_json::Value,
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ConversationMessage {
    pub message: serde_json::Value,
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
}

impl ConversationMessage {
    /// The Anthropic-API `usage` block attached to this assistant message,
    /// if any. Within a single user-perceived turn there can be several
    /// assistant messages (one per API roundtrip when tools fire); each
    /// carries its own `usage` whose `cache_read_input_tokens` reflects
    /// everything the model saw on THAT call. The terminal `result` event
    /// from the SDK only summarises the last sub-call, so it can drop to
    /// a tiny number once the cache is warm — drive the meter off
    /// assistant-message usage instead. Mirrors how Claude Code itself
    /// computes `context_window.current_usage` (cli.js `zD1`).
    pub fn usage(&self) -> Option<Usage> {
        let raw = self.message.get("usage")?;
        serde_json::from_value(raw.clone()).ok()
    }
}

#[derive(Debug, Deserialize)]
pub struct ResultMessage {
    pub subtype: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default, rename = "modelUsage")]
    pub model_usage: BTreeMap<String, ModelUsage>,
    #[serde(default)]
    pub usage: Option<Usage>,
    /// Cumulative session cost in USD reported by the SDK at turn end.
    /// Mirrors JS `acp-agent.js:632` (`message.total_cost_usd`). Optional
    /// because not every result carries it (local-only slash-command turns
    /// report `0` or omit the field).
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct ModelUsage {
    #[serde(rename = "contextWindow")]
    pub context_window: u64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Tokens read from the prompt cache this turn — i.e. the bulk of the
    /// previously-built conversation context that the model didn't have to
    /// re-process. Without this in the meter sum, a deep session shows a
    /// near-zero token count because `input_tokens` only reflects what's NEW
    /// this turn after the cache pre-loads the rest.
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    /// Tokens written to the cache this turn (new context entries that get
    /// reused on later turns). Counted toward the live context window for the
    /// same reason as cache reads.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

impl ResultMessage {
    /// The largest advertised context window across the models used this turn.
    /// `claude` reports per-model windows in `modelUsage`; the active model's is
    /// the relevant budget. When several appear (rare — e.g. a subagent on a
    /// different model), the max is the safe upper bound for the meter.
    pub fn context_window_for_active_model(&self) -> Option<u64> {
        self.model_usage.values().map(|m| m.context_window).max()
    }
}

#[derive(Debug, Deserialize)]
pub struct ControlRequestEnvelope {
    pub request_id: String,
    pub request: ControlRequestKind,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlRequestKind {
    CanUseTool {
        tool_name: String,
        tool_use_id: String,
        #[serde(default)]
        input: serde_json::Value,
        #[serde(default)]
        permission_suggestions: Vec<serde_json::Value>,
    },
    /// `PostToolUse` / `Stop` hook firing from the SDK. Sent between the
    /// previous tool's `tool_result` and the next assistant generation; our
    /// reply may include an `additionalContext` string that the agent reads
    /// in the SAME turn (the live-injection mechanism — no interrupt, no
    /// new turn, no broken tool).
    HookCallback {
        callback_id: String,
        #[serde(default)]
        tool_use_id: Option<String>,
        #[serde(default)]
        input: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

/// Wire shape claude actually sends for control_response:
///   {"type":"control_response","response":{"subtype":"success","request_id":"…","response":{…}}}
/// The `request_id` lives INSIDE the `response` object — the previous
/// deser put it at the outer level and silently rejected every claude
/// response with `missing field 'request_id'` (most visible: the
/// `initialize` reply carrying commands/skills/agents was lost on every
/// session start). `inner.response` (the doubly-nested field) is the
/// actual payload (success-body / hook-output / etc).
#[derive(Debug, Deserialize)]
pub struct ControlResponseEnvelope {
    #[serde(rename = "response")]
    pub inner: ControlResponseBody,
}

#[derive(Debug, Deserialize)]
pub struct ControlResponseBody {
    #[serde(default)]
    pub subtype: String,
    pub request_id: String,
    #[serde(default)]
    pub response: serde_json::Value,
}

impl ControlResponseEnvelope {
    pub fn request_id(&self) -> &str {
        &self.inner.request_id
    }
    pub fn into_response(self) -> serde_json::Value {
        self.inner.response
    }
}

/// A message written to `claude`'s stdin (NDJSON). Either a user turn or a
/// control request/response on the same stream.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputMessage {
    User {
        message: UserPayload,
    },
    ControlRequest {
        request_id: String,
        request: ControlRequestOut,
    },
    ControlResponse {
        request_id: String,
        response: serde_json::Value,
    },
}

#[derive(Debug, Serialize)]
pub struct UserPayload {
    pub role: &'static str,
    pub content: serde_json::Value,
}

/// Descriptor for one selectable model, mirroring the SDK `ModelInfo`
/// (`SDKControlInitializeResponse.models[]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Alias or full id passed to `set_model` / `--model` (SDK `value`).
    pub value: String,
    /// Human-facing label (SDK `displayName`).
    pub display_name: String,
    /// One-line description (SDK `description`).
    pub description: String,
}

/// One entry in the `hooks` map of an `initialize` control_request: a matcher
/// pattern (or `null` for "all"), the list of callback ids the SDK should
/// invoke when this event fires (we pick stable names like `pti`/`stop_inj`),
/// and a per-callback timeout in milliseconds.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookConfig {
    pub matcher: Option<String>,
    pub hook_callback_ids: Vec<String>,
    pub timeout: u32,
}

#[derive(Debug, Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlRequestOut {
    Interrupt,
    /// Outbound SDK handshake registering our hook callback ids. Sent
    /// fire-and-forget on session spawn so the agent will emit
    /// `hook_callback` control_requests at `PostToolUse` and `Stop` event
    /// boundaries. Keys of `hooks` are PascalCase Claude Code event names
    /// (`PostToolUse`, `Stop`) — leaving them as `BTreeMap<String, …>`
    /// keeps the wire literal and lets us add events later without an
    /// enum bump.
    Initialize {
        hooks: std::collections::BTreeMap<String, Vec<HookConfig>>,
    },
    /// Switch the session's model mid-conversation. Mirrors the SDK's
    /// `setModel()`; applied by `claude` on the next turn. Wire:
    /// `{"subtype":"set_model","model":"<value>"}`.
    SetModel { model: String },
    /// Apply runtime flag settings (effort level / ultracode). Mirrors the
    /// SDK's `applyFlagSettings`; applied by `claude` on the next turn. Wire:
    /// `{"subtype":"apply_flag_settings","settings":{...}}`.
    ApplyFlagSettings { settings: serde_json::Value },
}

impl InputMessage {
    /// A plain-text user turn: `content` is a bare string.
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::User {
            message: UserPayload {
                role: "user",
                content: serde_json::Value::String(text.into()),
            },
        }
    }

    /// A structured user turn (text + images). `content` is an array of
    /// Anthropic content blocks. ACP blocks the compose row can't express as
    /// text/image are skipped (the Foundation only produces those).
    pub fn user_blocks(blocks: &[acp::ContentBlock]) -> Self {
        let content: Vec<serde_json::Value> = blocks
            .iter()
            .filter_map(|block| match block {
                acp::ContentBlock::Text(t) => Some(serde_json::json!({
                    "type": "text",
                    "text": t.text,
                })),
                acp::ContentBlock::Image(img) => Some(serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": img.mime_type,
                        "data": img.data,
                    },
                })),
                _ => None,
            })
            .collect();
        Self::User {
            message: UserPayload {
                role: "user",
                content: serde_json::Value::Array(content),
            },
        }
    }

    /// The soft-interrupt control request (what the SDK's `query.interrupt()`
    /// writes): `claude` cancels the current turn and emits a `result`.
    pub fn interrupt(request_id: impl Into<String>) -> Self {
        Self::ControlRequest {
            request_id: request_id.into(),
            request: ControlRequestOut::Interrupt,
        }
    }

    /// Reply to a `can_use_tool` control request.
    pub fn permission_response(request_id: impl Into<String>, allow: bool) -> Self {
        Self::ControlResponse {
            request_id: request_id.into(),
            response: serde_json::json!({
                "behavior": if allow { "allow" } else { "deny" },
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_init_session_id() {
        let v = r#"{"type":"system","subtype":"init","session_id":"abc","uuid":"u1"}"#;
        match OutputMessage::parse(v).unwrap() {
            OutputMessage::System(System::Init { session_id, .. }) => assert_eq!(session_id, "abc"),
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn parses_text_delta() {
        let v = r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}},"uuid":"u","session_id":"s"}"#;
        let m = OutputMessage::parse(v).unwrap();
        assert!(matches!(m, OutputMessage::StreamEvent(_)));
    }
    #[test]
    fn parses_result_success() {
        let v = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"modelUsage":{"claude-x":{"contextWindow":1000000,"maxOutputTokens":64000,"inputTokens":1,"outputTokens":2,"cachedReadTokens":0,"cachedWriteTokens":0,"costUSD":0.0}},"uuid":"u","session_id":"s"}"#;
        match OutputMessage::parse(v).unwrap() {
            OutputMessage::Result(r) => {
                assert!(!r.is_error);
                assert_eq!(r.stop_reason.as_deref(), Some("end_turn"));
                assert_eq!(r.context_window_for_active_model(), Some(1_000_000));
            }
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn parses_error_result() {
        let v = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["boom"],"stop_reason":null,"uuid":"u","session_id":"s"}"#;
        match OutputMessage::parse(v).unwrap() {
            OutputMessage::Result(r) => {
                assert!(r.is_error);
                assert_eq!(r.errors, vec!["boom".to_string()]);
            }
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn parses_can_use_tool_control_request() {
        let v = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"t1","input":{"command":"ls"}}}"#;
        match OutputMessage::parse(v).unwrap() {
            OutputMessage::ControlRequest(env) => {
                assert_eq!(env.request_id, "r1");
                assert!(matches!(env.request, ControlRequestKind::CanUseTool { .. }));
            }
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn parses_assistant_with_parent_tool_use_id() {
        let v = r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"role":"assistant","content":[]},"uuid":"u","session_id":"s"}"#;
        match OutputMessage::parse(v).unwrap() {
            OutputMessage::Assistant(m) => {
                assert_eq!(m.parent_tool_use_id.as_deref(), Some("toolu_1"))
            }
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn unknown_type_is_unknown_not_error() {
        assert!(matches!(
            OutputMessage::parse(r#"{"type":"ping"}"#).unwrap(),
            OutputMessage::Unknown
        ));
        assert!(matches!(
            OutputMessage::parse(r#"{"type":"rate_limit_event","x":1}"#).unwrap(),
            OutputMessage::Unknown
        ));
    }

    #[test]
    fn serializes_user_text() {
        let s = serde_json::to_string(&InputMessage::user_text("hi")).unwrap();
        assert_eq!(
            s,
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#
        );
    }
    #[test]
    fn serializes_user_blocks_text_and_image() {
        let blocks = vec![
            acp::ContentBlock::Text(acp::TextContent::new("hi".to_string())),
            acp::ContentBlock::Image(acp::ImageContent::new(
                "BASE64".to_string(),
                "image/png".to_string(),
            )),
        ];
        let v: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&InputMessage::user_blocks(&blocks)).unwrap(),
        )
        .unwrap();
        let content = &v["message"]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hi");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "BASE64");
    }
    #[test]
    fn serializes_interrupt_control() {
        let s = serde_json::to_string(&InputMessage::interrupt("r1")).unwrap();
        assert!(s.contains(r#""type":"control_request""#));
        assert!(s.contains(r#""subtype":"interrupt""#));
        assert!(s.contains(r#""request_id":"r1""#));
    }
    #[test]
    fn parses_hook_callback_control_request() {
        let v = r#"{"type":"control_request","request_id":"hk1","request":{"subtype":"hook_callback","callback_id":"pti","tool_use_id":"t1","input":{"tool_name":"Bash"}}}"#;
        match OutputMessage::parse(v).unwrap() {
            OutputMessage::ControlRequest(env) => {
                assert_eq!(env.request_id, "hk1");
                match env.request {
                    ControlRequestKind::HookCallback {
                        callback_id,
                        tool_use_id,
                        ..
                    } => {
                        assert_eq!(callback_id, "pti");
                        assert_eq!(tool_use_id.as_deref(), Some("t1"));
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn serializes_initialize_control_request_with_hooks() {
        let mut hooks = std::collections::BTreeMap::new();
        hooks.insert(
            "PostToolUse".to_string(),
            vec![HookConfig {
                matcher: None,
                hook_callback_ids: vec!["pti".to_string()],
                timeout: 30000,
            }],
        );
        let s = serde_json::to_string(&InputMessage::ControlRequest {
            request_id: "init-1".to_string(),
            request: ControlRequestOut::Initialize { hooks },
        })
        .unwrap();
        assert!(s.contains(r#""type":"control_request""#), "{s}");
        assert!(s.contains(r#""subtype":"initialize""#), "{s}");
        assert!(s.contains(r#""PostToolUse""#), "{s}");
        // camelCase required by the SDK.
        assert!(s.contains(r#""hookCallbackIds":["pti"]"#), "{s}");
        assert!(s.contains(r#""timeout":30000"#), "{s}");
    }
    #[test]
    fn serializes_permission_response_allow_and_deny() {
        let allow = serde_json::to_string(&InputMessage::permission_response("r1", true)).unwrap();
        assert!(allow.contains(r#""type":"control_response""#));
        assert!(allow.contains(r#""behavior":"allow""#));
        let deny = serde_json::to_string(&InputMessage::permission_response("r1", false)).unwrap();
        assert!(deny.contains(r#""behavior":"deny""#));
    }
    #[test]
    fn set_model_control_request_wire_shape() {
        let msg = InputMessage::ControlRequest {
            request_id: "r1".into(),
            request: ControlRequestOut::SetModel { model: "sonnet".into() },
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "control_request");
        assert_eq!(v["request"]["subtype"], "set_model");
        assert_eq!(v["request"]["model"], "sonnet");
    }
    #[test]
    fn apply_flag_settings_control_request_wire_shape() {
        let msg = InputMessage::ControlRequest {
            request_id: "r1".into(),
            request: ControlRequestOut::ApplyFlagSettings {
                settings: serde_json::json!({"effortLevel":"high","ultracode":false}),
            },
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["request"]["subtype"], "apply_flag_settings");
        assert_eq!(v["request"]["settings"]["effortLevel"], "high");
        assert_eq!(v["request"]["settings"]["ultracode"], false);
    }
}
