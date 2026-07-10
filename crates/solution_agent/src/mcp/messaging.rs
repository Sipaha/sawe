//! Messaging `solution_agent` MCP tools. Relocated verbatim from the former
//! monolithic `mcp.rs`.
use agent_client_protocol::schema as acp;
use anyhow::{Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::model::SolutionSessionId;
use crate::store::SolutionAgentStore;
use gpui::SharedString;

/// Send a user message to an existing session. Fire-and-forget — the
/// returned `Task` is detached so the tool response returns immediately
/// once the prompt is enqueued. Use `solution_agent.get_session` to poll
/// for new entries, or subscribe to `solution_agent.*` events (deferred
/// to a later phase) for push notifications.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SendMessageParams {
    pub session_id: String,
    pub content: String,
}

impl<'de> Deserialize<'de> for SendMessageParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            content: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            content: inner.content,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SendMessageResult {}

#[derive(Clone)]
pub struct SendMessageTool;

impl McpServerTool for SendMessageTool {
    type Input = SendMessageParams;
    type Output = SendMessageResult;
    const NAME: &'static str = "solution_agent.send_message";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;
        let content = input.content;

        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.send_message(session_id, content, cx).detach();
            });
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "queued".to_string(),
            }],
            structured_content: SendMessageResult {},
        })
    }
}

// =====================================================================
// solution_agent.send_message_blocks
// =====================================================================

/// Send a structured user message composed of one or more ACP
/// `ContentBlock`s (text + images + resource links, etc). Mirrors
/// `SendMessageTool` but lets MCP consumers pass multi-modal payloads
/// — primarily the mobile client, which encodes picked images and
/// text-like files into `Image` / `Text` blocks. The bare
/// `send_message` text-only tool stays for callers that only have a
/// plain prompt.
///
/// Fire-and-forget — the returned `Task` from
/// `SolutionAgentStore::send_message_blocks` is detached so the tool
/// response returns immediately once the prompt is enqueued.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SendMessageBlocksParams {
    pub session_id: String,
    /// Each entry is serialised per the ACP `ContentBlock` schema
    /// (`{"type": "text", "text": "..."}` /
    /// `{"type": "image", "data": "<base64>", "mimeType": "image/png"}` /
    /// `{"type": "resource_link", "uri": "...", ...}` / etc).
    pub blocks: Vec<acp::ContentBlock>,
}

impl<'de> Deserialize<'de> for SendMessageBlocksParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            blocks: Vec<acp::ContentBlock>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            blocks: inner.blocks,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SendMessageBlocksResult {}

#[derive(Clone)]
pub struct SendMessageBlocksTool;

impl McpServerTool for SendMessageBlocksTool {
    type Input = SendMessageBlocksParams;
    type Output = SendMessageBlocksResult;
    const NAME: &'static str = "solution_agent.send_message_blocks";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        anyhow::ensure!(
            !input.blocks.is_empty(),
            "invalid_params: blocks must contain at least one item"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;
        // Swap any `spk-upload://<id>` ResourceLink for the inline
        // Image/Text the chunked-upload tmp file contains, BEFORE the
        // bundle reaches the store. Without this step the handle URI
        // would travel verbatim to claude-acp, which has no idea what
        // `spk-upload://` means — the attached image silently vanishes
        // and the agent sees only the accompanying text.
        let blocks = crate::upload::resolve_upload_handles(input.blocks)?;

        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.send_message_blocks(session_id, blocks, cx).detach();
            });
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "queued".to_string(),
            }],
            structured_content: SendMessageBlocksResult {},
        })
    }
}

// =====================================================================
// solution_agent.delete_session
// =====================================================================

/// Cancel the in-flight turn on `session_id`. Forwards to
/// `AgentConnection::cancel`; the session will eventually transition to
/// `Idle` (or `Errored`) via the regular `AcpThreadEvent` plumbing.
///
/// When `flush_pending` is true the call additionally sets the
/// session's `flush_after_cancel` flag so that the `pending_messages`
/// queue (filled while the agent was Running) gets flushed as one
/// merged follow-up turn the moment the cancel settles, instead of
/// being dropped. This is the wire path the mobile Force-flush
/// button uses to "stop and send my queued messages now".
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CancelTurnParams {
    pub session_id: String,
    #[serde(default)]
    pub flush_pending: bool,
}

impl<'de> Deserialize<'de> for CancelTurnParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            #[serde(default)]
            flush_pending: bool,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            flush_pending: inner.flush_pending,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CancelTurnResult {}

#[derive(Clone)]
pub struct CancelTurnTool;

impl McpServerTool for CancelTurnTool {
    type Input = CancelTurnParams;
    type Output = CancelTurnResult;
    const NAME: &'static str = "solution_agent.cancel_turn";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let flush_pending = input.flush_pending;
        cx.update(|cx| -> Result<()> {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                let result = if flush_pending {
                    // Best-effort: when there is nothing to flush
                    // `interrupt_and_flush_pending` errors with
                    // "no queued messages". Treat that as success
                    // here — the caller asked for "cancel + maybe
                    // flush", and the cancel half still makes sense.
                    match store.interrupt_and_flush_pending(session_id, cx) {
                        Ok(()) => Ok(()),
                        Err(_) => store.cancel_turn(session_id, cx),
                    }
                } else {
                    store.cancel_turn(session_id, cx)
                };
                // Human-initiated stop (desktop / mobile Stop) — park the
                // supervisor in `Held` so it doesn't re-engage until the user
                // sends the next message. (A `flush_pending` cancel re-sends the
                // queued user text, which re-arms via the send funnel anyway.)
                store.hold_supervisor(session_id, cx);
                result
            })?;
            Ok(())
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "cancelled".to_string(),
            }],
            structured_content: CancelTurnResult {},
        })
    }
}

// =====================================================================
// solution_agent.authorize_tool_call
// =====================================================================

/// Inject an in-conversation SystemNote breadcrumb into a live session.
/// `level` is one of `info` | `error` | `observer`. This exists primarily
/// so an agent driving the editor over MCP can exercise the SystemNote
/// rendering path (which otherwise only fires from supervisor / reconnect
/// internals). No-op on a cold session (no live `AcpThread`): create a
/// session and send it a message first so a thread exists.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct PushSystemNoteParams {
    pub session_id: String,
    /// `info` | `error` | `observer`. Defaults to `info` when empty.
    pub level: String,
    pub text: String,
}

impl<'de> Deserialize<'de> for PushSystemNoteParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            level: String,
            text: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            level: inner.level,
            text: inner.text,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct PushSystemNoteResult {}

#[derive(Clone)]
pub struct PushSystemNoteTool;

impl McpServerTool for PushSystemNoteTool {
    type Input = PushSystemNoteParams;
    type Output = PushSystemNoteResult;
    const NAME: &'static str = "solution_agent.push_system_note";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        anyhow::ensure!(!input.text.is_empty(), "invalid_params: text is required");
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;
        let level = match input.level.trim().to_ascii_lowercase().as_str() {
            "" | "info" => acp_thread::SystemNoteLevel::Info,
            "error" => acp_thread::SystemNoteLevel::Error,
            "observer" => acp_thread::SystemNoteLevel::Observer,
            other => {
                anyhow::bail!("invalid_params: level must be info|error|observer, got {other:?}")
            }
        };
        let text = SharedString::from(input.text);

        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.push_system_note(session_id, level, text, cx)
            });
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "pushed".to_string(),
            }],
            structured_content: PushSystemNoteResult {},
        })
    }
}

// =====================================================================
// solution_agent.restart_agent
// =====================================================================

pub(crate) fn register_messaging(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SendMessageTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SendMessageBlocksTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(CancelTurnTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(PushSystemNoteTool);
    });
}
