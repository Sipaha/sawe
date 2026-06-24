//! Structured persistence of `AgentThreadEntry` lists for cold tabs.
//!
//! `AcpThread` itself isn't directly serialisable — it owns
//! `Entity<Markdown>`, `Entity<Terminal>`, `Entity<Diff>` handles tied to
//! a live subprocess. To paint a restored conversation identically to
//! its live form without spawning the agent, we serialise the data that
//! drives the render (markdown source strings + raw ACP chunks +
//! terminal-statuses + tool-call status) and on cold restore rebuild
//! fresh `Markdown` widgets.
//!
//! The roundtrip is intentionally **lossy in one dimension**: tool
//! calls that were still in-flight at save time (`Pending`,
//! `WaitingForConfirmation`, `InProgress`) are dropped — re-rendering a
//! frozen "Running…" status across an editor restart is misleading,
//! and once the session resumes the agent replays the turn from its
//! own state anyway. Locations / resolved_locations on tool calls also
//! reset to empty: their `AgentLocation` carries `Entity<Buffer>`
//! references that are nonsense across an editor lifetime.

use acp_thread::{
    AgentThreadEntry, AssistantMessage, AssistantMessageChunk, ContentBlock, PlanEntry, ToolCall,
    ToolCallContent, ToolCallStatus, UserMessage, UserMessageId,
};
use agent_client_protocol::schema as acp;
use gpui::{App, AppContext, SharedString};
use markdown::Markdown;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub enum PersistedEntryV2 {
    User(PersistedUserMessage),
    Assistant(PersistedAssistantMessage),
    Tool(PersistedToolCall),
    Plan(Vec<PersistedPlanEntry>),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PersistedUserMessage {
    /// `UserMessageId` round-tripped through its `Serialize`/`Deserialize`
    /// impl — the inner `Arc<str>` field isn't `pub`, so this is the
    /// only way to pin a stable id across editor restarts. `None` when
    /// the live entry didn't carry one (e.g. `/clear` synthesised
    /// stub messages in older agents).
    pub id: Option<String>,
    /// Markdown source the live `ContentBlock` rendered to. We rebuild a
    /// fresh `Markdown` entity from this on cold restore — the live
    /// `Entity<Markdown>` itself is gpui-only and not serialisable.
    pub content_md: String,
    /// Original ACP chunks the user submitted (text + image data, etc).
    /// Preserved in raw form so image previews still work after restart
    /// (the spk-image:// scheme handler reads from here).
    pub chunks: Vec<acp::ContentBlock>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PersistedAssistantMessage {
    pub chunks: Vec<PersistedAssistantChunk>,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum PersistedAssistantChunk {
    Message(String),
    Thought(String),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PersistedToolCall {
    pub id: String,
    pub label_md: String,
    pub kind: acp::ToolKind,
    pub status: TerminalToolCallStatus,
    pub content: Vec<PersistedToolCallContent>,
    /// Pre-formatted markdown source for the tool's content — diff +
    /// terminal output also collapse to markdown here, so cold restore
    /// always reconstructs `ToolCallContent::ContentBlock` regardless
    /// of the live variant. Visual identity comes from the source
    /// having been formatted by the same pipeline at save time
    /// (`tool_call_content_summary`); the render path treats already-
    /// fenced markdown as a no-op so the second pass is idempotent.
    #[serde(default)]
    pub raw_input: Option<serde_json::Value>,
    #[serde(default)]
    pub raw_output: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub locations: Vec<acp::ToolCallLocation>,
}

/// Subset of `acp_thread::ToolCallStatus` covering only states a
/// completed turn can finish in. `Pending` / `WaitingForConfirmation` /
/// `InProgress` aren't representable here — those entries are dropped
/// at save time (see `to_persisted`) because rendering "still running"
/// post-restart would be a lie.
#[derive(Clone, Serialize, Deserialize)]
pub enum TerminalToolCallStatus {
    Completed,
    Failed,
    Rejected,
    Canceled,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PersistedToolCallContent {
    pub markdown: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PersistedPlanEntry {
    pub content_md: String,
    pub priority: acp::PlanEntryPriority,
    pub status: acp::PlanEntryStatus,
}

/// Build a `PersistedEntryV2` from a live entry, returning `None` when
/// the entry isn't safe to freeze (in-flight tool calls).
pub fn to_persisted(entry: &AgentThreadEntry, cx: &App) -> Option<PersistedEntryV2> {
    match entry {
        AgentThreadEntry::UserMessage(msg) => Some(PersistedEntryV2::User(PersistedUserMessage {
            id: msg.id.as_ref().map(user_message_id_to_string),
            content_md: msg.content.to_markdown(cx).to_string(),
            chunks: msg.chunks.clone(),
        })),
        AgentThreadEntry::AssistantMessage(msg) => {
            let chunks = msg
                .chunks
                .iter()
                .map(|chunk| match chunk {
                    AssistantMessageChunk::Message { block } => {
                        PersistedAssistantChunk::Message(block.to_markdown(cx).to_string())
                    }
                    AssistantMessageChunk::Thought { block } => {
                        PersistedAssistantChunk::Thought(block.to_markdown(cx).to_string())
                    }
                })
                .collect();
            Some(PersistedEntryV2::Assistant(PersistedAssistantMessage {
                chunks,
            }))
        }
        AgentThreadEntry::ToolCall(call) => {
            let status = match call.status {
                ToolCallStatus::Completed => TerminalToolCallStatus::Completed,
                ToolCallStatus::Failed => TerminalToolCallStatus::Failed,
                ToolCallStatus::Rejected => TerminalToolCallStatus::Rejected,
                ToolCallStatus::Canceled => TerminalToolCallStatus::Canceled,
                _ => return None,
            };
            let content: Vec<PersistedToolCallContent> = call
                .content
                .iter()
                .map(|content| PersistedToolCallContent {
                    markdown: crate::conversation_render::tool_call_content_summary(
                        call, content, cx,
                    ),
                })
                .collect();
            Some(PersistedEntryV2::Tool(PersistedToolCall {
                id: call.id.0.to_string(),
                label_md: call.label.read(cx).source().to_string(),
                kind: call.kind,
                status,
                content,
                raw_input: call.raw_input.clone(),
                raw_output: call.raw_output.clone(),
                tool_name: call.tool_name.as_ref().map(|s| s.to_string()),
                locations: call.locations.clone(),
            }))
        }
        AgentThreadEntry::CompletedPlan(entries) => {
            let entries: Vec<PersistedPlanEntry> = entries
                .iter()
                .map(|e| PersistedPlanEntry {
                    content_md: e.content.read(cx).source().to_string(),
                    priority: e.priority.clone(),
                    status: e.status.clone(),
                })
                .collect();
            Some(PersistedEntryV2::Plan(entries))
        }
        // Context-compaction markers are an in-session affordance (the model
        // summarizing its own history) and aren't part of the durable
        // transcript — skip them, like in-flight tool calls.
        AgentThreadEntry::ContextCompaction(_) => None,
    }
}

/// Reconstruct a fresh `AgentThreadEntry` from its persisted form.
/// Creates new `Markdown` widgets for every text block so the cold
/// render goes through the exact same widget pipeline as live.
pub fn from_persisted(persisted: PersistedEntryV2, cx: &mut App) -> AgentThreadEntry {
    match persisted {
        PersistedEntryV2::User(p) => AgentThreadEntry::UserMessage(UserMessage {
            id: p.id.as_deref().map(user_message_id_from_string),
            content: ContentBlock::Markdown {
                markdown: cx.new(|cx| Markdown::new(p.content_md.into(), None, None, cx)),
            },
            chunks: p.chunks,
            checkpoint: None,
            indented: false,
        }),
        PersistedEntryV2::Assistant(p) => {
            let chunks = p
                .chunks
                .into_iter()
                .map(|chunk| match chunk {
                    PersistedAssistantChunk::Message(md) => AssistantMessageChunk::Message {
                        block: ContentBlock::Markdown {
                            markdown: cx.new(|cx| Markdown::new(md.into(), None, None, cx)),
                        },
                    },
                    PersistedAssistantChunk::Thought(md) => AssistantMessageChunk::Thought {
                        block: ContentBlock::Markdown {
                            markdown: cx.new(|cx| Markdown::new(md.into(), None, None, cx)),
                        },
                    },
                })
                .collect();
            AgentThreadEntry::AssistantMessage(AssistantMessage {
                chunks,
                indented: false,
                is_subagent_output: false,
                subagent_id: None,
            })
        }
        PersistedEntryV2::Tool(p) => {
            let status = match p.status {
                TerminalToolCallStatus::Completed => ToolCallStatus::Completed,
                TerminalToolCallStatus::Failed => ToolCallStatus::Failed,
                TerminalToolCallStatus::Rejected => ToolCallStatus::Rejected,
                TerminalToolCallStatus::Canceled => ToolCallStatus::Canceled,
            };
            let raw_input_markdown = p.raw_input.as_ref().and_then(|input| {
                let pretty = serde_json::to_string_pretty(input).ok()?;
                if pretty.trim().is_empty() {
                    return None;
                }
                Some(cx.new(|cx| {
                    Markdown::new(format!("```json\n{pretty}\n```").into(), None, None, cx)
                }))
            });
            let content = p
                .content
                .into_iter()
                .map(|c| {
                    ToolCallContent::ContentBlock(ContentBlock::Markdown {
                        markdown: cx.new(|cx| Markdown::new(c.markdown.into(), None, None, cx)),
                    })
                })
                .collect();
            AgentThreadEntry::ToolCall(ToolCall {
                id: acp::ToolCallId::new(p.id),
                label: cx.new(|cx| Markdown::new(p.label_md.into(), None, None, cx)),
                kind: p.kind,
                content,
                status,
                locations: p.locations,
                resolved_locations: Vec::new(),
                raw_input: p.raw_input,
                raw_input_markdown,
                raw_output: p.raw_output,
                tool_name: p.tool_name.map(SharedString::from),
                subagent_session_info: None,
                subagent_id: None,
                sandbox_authorization_details: None,
                // Cold blobs only persist terminal statuses (see
                // `TerminalToolCallStatus`), so the rehydrated call is
                // never InProgress and therefore never needs a
                // start-of-InProgress timestamp.
                status_started_at: None,
            })
        }
        PersistedEntryV2::Plan(entries) => {
            let plan_entries = entries
                .into_iter()
                .map(|e| PlanEntry {
                    content: cx.new(|cx| Markdown::new(e.content_md.into(), None, None, cx)),
                    priority: e.priority,
                    status: e.status,
                })
                .collect();
            AgentThreadEntry::CompletedPlan(plan_entries)
        }
    }
}

fn user_message_id_to_string(id: &UserMessageId) -> String {
    serde_json::to_value(id)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

fn user_message_id_from_string(s: &str) -> UserMessageId {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .expect("UserMessageId deserializes from any string")
}
