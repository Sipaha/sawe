//! `solution_agent`'s own transcript entry — the single type that is both
//! rendered (Phase 2) and serialized to per-entry DB rows + the mobile wire
//! (Phase 3-5). Content is held as markdown source strings + structured
//! metadata, the shape the renderer already projects to, so a cold/synced
//! entry paints identically to a live one. Replaces the lossy
//! `cold_persistence::PersistedEntryV2` once Phases 2-3 land.

use acp_thread::{AgentThreadEntry, AssistantMessageChunk, UserMessageId};
use agent_client_protocol::schema as acp;
use gpui::{App, AppContext as _, SharedString};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionEntry {
    /// Unix-millis creation time. 0 = unknown (replayed gap / pre-feature).
    /// Stamped by the store, not the converter.
    pub created_ms: i64,
    /// `change_seq` value at this entry's last create/mutate — the delta
    /// watermark. Stamped by the store (Phase 4); the converter leaves it 0.
    pub mod_seq: u64,
    /// None = main agent; Some = the sub-agent that produced this entry.
    pub subagent_id: Option<SharedString>,
    pub kind: SessionEntryKind,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum SessionEntryKind {
    UserMessage {
        id: Option<String>,
        content_md: String,
        chunks: Vec<acp::ContentBlock>,
    },
    AssistantMessage {
        chunks: Vec<AssistantChunk>,
    },
    ToolCall {
        id: String,
        label_md: String,
        kind: acp::ToolKind,
        status: ToolStatus,
        content_md: Vec<String>,
        raw_input: Option<serde_json::Value>,
        raw_output: Option<serde_json::Value>,
        tool_name: Option<String>,
        locations: Vec<acp::ToolCallLocation>,
        /// Unix-ms when the call entered InProgress; drives the elapsed badge.
        status_started_at: Option<i64>,
    },
    Plan(Vec<PlanItem>),
    ContextCompaction {
        id: String,
        status: CompactionStatus,
        summary_md: Option<String>,
    },
    /// Editor-originated annotation (watchdog / usage-limit notices, supervisor
    /// activity), rendered distinctly per [`SystemEntryLevel`]. Not part of the
    /// agent's transcript — persisted only in the session's own entry rows.
    System {
        level: SystemEntryLevel,
        text_md: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SystemEntryLevel {
    Info,
    Error,
    Observer,
}

impl From<acp_thread::SystemNoteLevel> for SystemEntryLevel {
    fn from(level: acp_thread::SystemNoteLevel) -> Self {
        match level {
            acp_thread::SystemNoteLevel::Info => Self::Info,
            acp_thread::SystemNoteLevel::Error => Self::Error,
            acp_thread::SystemNoteLevel::Observer => Self::Observer,
        }
    }
}

impl From<SystemEntryLevel> for acp_thread::SystemNoteLevel {
    fn from(level: SystemEntryLevel) -> Self {
        match level {
            SystemEntryLevel::Info => Self::Info,
            SystemEntryLevel::Error => Self::Error,
            SystemEntryLevel::Observer => Self::Observer,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum AssistantChunk {
    Message(String),
    Thought(String),
}

/// Full tool-call lifecycle — unlike `PersistedEntryV2`'s terminal-only
/// status, the owned model keeps in-flight states so the live render and the
/// mobile delta show "running" tool calls. The live authorization channel for
/// `WaitingForConfirmation` is NOT stored here (it's a side map keyed by tool
/// id — Phase 4/5).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum ToolStatus {
    Pending,
    WaitingForConfirmation,
    InProgress,
    Completed,
    Failed,
    Rejected,
    Canceled,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlanItem {
    pub content_md: String,
    pub priority: acp::PlanEntryPriority,
    pub status: acp::PlanEntryStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum CompactionStatus {
    InProgress,
    Completed,
    Canceled,
}

/// Project a live `AgentThreadEntry` onto the owned `SessionEntry`. The ONLY
/// place `acp_thread::AgentThreadEntry` is read. Total (never drops): unlike
/// `cold_persistence::to_persisted` it keeps in-flight tool calls and
/// context-compaction. `created_ms` / `mod_seq` are left 0 for the store to
/// stamp; `subagent_id` is lifted from the entry where present.
pub fn to_session_entry(entry: &AgentThreadEntry, cx: &App) -> SessionEntry {
    let (subagent_id, kind) = match entry {
        AgentThreadEntry::UserMessage(msg) => (
            None,
            SessionEntryKind::UserMessage {
                id: msg.id.as_ref().map(user_message_id_to_string),
                content_md: msg.content.to_markdown(cx).to_string(),
                chunks: msg.chunks.clone(),
            },
        ),
        AgentThreadEntry::AssistantMessage(msg) => {
            let chunks = msg
                .chunks
                .iter()
                .map(|chunk| match chunk {
                    AssistantMessageChunk::Message { block } => {
                        AssistantChunk::Message(block.to_markdown(cx).to_string())
                    }
                    AssistantMessageChunk::Thought { block } => {
                        AssistantChunk::Thought(block.to_markdown(cx).to_string())
                    }
                })
                .collect();
            (
                msg.subagent_id.clone(),
                SessionEntryKind::AssistantMessage { chunks },
            )
        }
        AgentThreadEntry::ToolCall(call) => {
            let status = match call.status {
                acp_thread::ToolCallStatus::Pending => ToolStatus::Pending,
                acp_thread::ToolCallStatus::WaitingForConfirmation { .. } => {
                    ToolStatus::WaitingForConfirmation
                }
                acp_thread::ToolCallStatus::InProgress => ToolStatus::InProgress,
                acp_thread::ToolCallStatus::Completed => ToolStatus::Completed,
                acp_thread::ToolCallStatus::Failed => ToolStatus::Failed,
                acp_thread::ToolCallStatus::Rejected => ToolStatus::Rejected,
                acp_thread::ToolCallStatus::Canceled => ToolStatus::Canceled,
            };
            let content_md = call
                .content
                .iter()
                .map(|content| {
                    crate::conversation_render::tool_call_content_summary(call, content, cx)
                })
                .collect();
            (
                call.subagent_id.clone(),
                SessionEntryKind::ToolCall {
                    id: call.id.0.to_string(),
                    label_md: call.label.read(cx).source().to_string(),
                    kind: call.kind,
                    status,
                    content_md,
                    raw_input: call.raw_input.clone(),
                    raw_output: call.raw_output.clone(),
                    tool_name: call.tool_name.as_ref().map(|s| s.to_string()),
                    locations: call.locations.clone(),
                    status_started_at: call.status_started_at.map(|t| t.timestamp_millis()),
                },
            )
        }
        AgentThreadEntry::CompletedPlan(entries) => (
            None,
            SessionEntryKind::Plan(
                entries
                    .iter()
                    .map(|e| PlanItem {
                        content_md: e.content.read(cx).source().to_string(),
                        priority: e.priority.clone(),
                        status: e.status.clone(),
                    })
                    .collect(),
            ),
        ),
        AgentThreadEntry::ContextCompaction(c) => (
            None,
            SessionEntryKind::ContextCompaction {
                id: c.id.0.to_string(),
                status: match c.status {
                    acp_thread::ContextCompactionStatus::InProgress => CompactionStatus::InProgress,
                    acp_thread::ContextCompactionStatus::Completed => CompactionStatus::Completed,
                    acp_thread::ContextCompactionStatus::Canceled => CompactionStatus::Canceled,
                },
                summary_md: c.summary.as_ref().map(|m| m.read(cx).source().to_string()),
            },
        ),
        AgentThreadEntry::SystemNote(note) => (
            None,
            SessionEntryKind::System {
                level: note.level.into(),
                text_md: note.text.to_string(),
            },
        ),
    };
    SessionEntry {
        created_ms: 0,
        mod_seq: 0,
        subagent_id,
        kind,
    }
}

/// Rebuild the entry list from a cold prefix for a cold restore.
///
/// Converts `cold` in order via [`to_session_entry`], stamping each result's
/// `created_ms` from the index-aligned `created_ms` slice (0 when absent) and
/// its `mod_seq` as `base_seq + 1 + index` so that cold-restored entries carry
/// ascending, non-zero sequence numbers. The caller is responsible for calling
/// `init_change_seq_from_entries` after `set_entries` so that the first live
/// append continues monotonically from `n+1`.
///
/// `live` is accepted for API compatibility but must always be empty; live
/// entries are appended by the store's `NewEntry` handler.
pub fn rebuild_entries(
    cold: &[AgentThreadEntry],
    live: &[AgentThreadEntry],
    created_ms: &[i64],
    base_seq: u64,
    cx: &App,
) -> Vec<SessionEntry> {
    let mut entries = Vec::with_capacity(cold.len() + live.len());
    for (global_idx, entry) in cold.iter().chain(live.iter()).enumerate() {
        let mut session_entry = to_session_entry(entry, cx);
        session_entry.created_ms = created_ms.get(global_idx).copied().unwrap_or(0);
        session_entry.mod_seq = base_seq + 1 + global_idx as u64;
        entries.push(session_entry);
    }
    entries
}

impl SessionEntry {
    /// Encode the entry's `kind` as a JSON blob for storage in the
    /// `solution_session_entries.payload` column. `SessionEntryKind` is a
    /// derived-`Serialize` enum whose variants all contain only JSON-compatible
    /// types, so serialisation cannot fail in practice; `.unwrap_or_default()`
    /// preserves the no-panic contract while keeping the return type simple.
    pub fn to_payload(&self) -> Vec<u8> {
        serde_json::to_vec(&self.kind).unwrap_or_default()
    }
}

/// Decode a `payload` blob back into a `SessionEntryKind`. Returns an error
/// on malformed JSON or unrecognised variant tags.
pub fn kind_from_payload(bytes: &[u8]) -> anyhow::Result<SessionEntryKind> {
    serde_json::from_slice(bytes).map_err(Into::into)
}

fn user_message_id_to_string(id: &UserMessageId) -> String {
    serde_json::to_value(id)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema as acp;
    use gpui::{AppContext as _, TestAppContext};

    fn sample_tool() -> SessionEntry {
        SessionEntry {
            created_ms: 1_700_000_000_000,
            mod_seq: 42,
            subagent_id: Some("toolu_abc".into()),
            kind: SessionEntryKind::ToolCall {
                id: "tc_1".into(),
                label_md: "Run tests".into(),
                kind: acp::ToolKind::Execute,
                status: ToolStatus::InProgress,
                content_md: vec!["```\nok\n```".into()],
                raw_input: Some(serde_json::json!({"cmd": "cargo test"})),
                raw_output: None,
                tool_name: Some("bash".into()),
                locations: Vec::new(),
                status_started_at: Some(1_700_000_000_500),
            },
        }
    }

    #[gpui::test]
    fn converts_user_and_assistant_messages(cx: &mut TestAppContext) {
        use acp_thread::{AgentThreadEntry, AssistantMessage, AssistantMessageChunk, ContentBlock};
        cx.update(|cx| {
            let user = AgentThreadEntry::UserMessage(acp_thread::UserMessage {
                id: None,
                content: ContentBlock::Markdown {
                    markdown: cx.new(|cx| markdown::Markdown::new("hello".into(), None, None, cx)),
                },
                chunks: Vec::new(),
                checkpoint: None,
                indented: false,
            });
            let entry = to_session_entry(&user, cx);
            match entry.kind {
                SessionEntryKind::UserMessage { content_md, .. } => assert_eq!(content_md, "hello"),
                _ => panic!("expected UserMessage"),
            }

            let assistant = AgentThreadEntry::AssistantMessage(AssistantMessage {
                chunks: vec![AssistantMessageChunk::Message {
                    block: ContentBlock::Markdown {
                        markdown: cx
                            .new(|cx| markdown::Markdown::new("hi there".into(), None, None, cx)),
                    },
                }],
                indented: false,
                is_subagent_output: false,
                subagent_id: Some("toolu_x".into()),
            });
            let entry = to_session_entry(&assistant, cx);
            assert_eq!(entry.subagent_id.as_deref(), Some("toolu_x"));
            match entry.kind {
                SessionEntryKind::AssistantMessage { chunks } => {
                    assert!(matches!(&chunks[0], AssistantChunk::Message(m) if m == "hi there"));
                }
                _ => panic!("expected AssistantMessage"),
            }
        });
    }

    #[gpui::test]
    fn converts_in_flight_tool_call(cx: &mut TestAppContext) {
        use acp_thread::{AgentThreadEntry, ToolCall, ToolCallStatus};
        cx.update(|cx| {
            let call = AgentThreadEntry::ToolCall(ToolCall {
                id: acp::ToolCallId::new("tc_9".to_string()),
                label: cx.new(|cx| markdown::Markdown::new("Edit file".into(), None, None, cx)),
                kind: acp::ToolKind::Edit,
                content: Vec::new(),
                status: ToolCallStatus::InProgress,
                locations: Vec::new(),
                resolved_locations: Vec::new(),
                raw_input: None,
                raw_input_markdown: None,
                raw_output: None,
                tool_name: Some("edit".into()),
                subagent_session_info: None,
                subagent_id: Some("toolu_p".into()),
                sandbox_authorization_details: None,
                status_started_at: None,
            });
            let entry = to_session_entry(&call, cx);
            assert_eq!(entry.subagent_id.as_deref(), Some("toolu_p"));
            match entry.kind {
                SessionEntryKind::ToolCall {
                    status, id, kind, ..
                } => {
                    assert!(matches!(status, ToolStatus::InProgress));
                    assert_eq!(id, "tc_9");
                    assert!(matches!(kind, acp::ToolKind::Edit));
                }
                _ => panic!("expected ToolCall"),
            }
        });
    }

    #[gpui::test]
    fn converts_plan_and_compaction(cx: &mut TestAppContext) {
        use acp_thread::{
            AgentThreadEntry, ContextCompaction, ContextCompactionId, ContextCompactionStatus,
            PlanEntry,
        };
        cx.update(|cx| {
            let plan = AgentThreadEntry::CompletedPlan(vec![PlanEntry {
                content: cx.new(|cx| markdown::Markdown::new("step one".into(), None, None, cx)),
                priority: acp::PlanEntryPriority::Medium,
                status: acp::PlanEntryStatus::Completed,
            }]);
            match to_session_entry(&plan, cx).kind {
                SessionEntryKind::Plan(items) => assert_eq!(items[0].content_md, "step one"),
                _ => panic!("expected Plan"),
            }

            let compaction = AgentThreadEntry::ContextCompaction(ContextCompaction {
                id: ContextCompactionId("cc_1".into()),
                status: ContextCompactionStatus::Completed,
                summary: Some(
                    cx.new(|cx| markdown::Markdown::new("summary".into(), None, None, cx)),
                ),
            });
            match to_session_entry(&compaction, cx).kind {
                SessionEntryKind::ContextCompaction {
                    status, summary_md, ..
                } => {
                    assert!(matches!(status, CompactionStatus::Completed));
                    assert_eq!(summary_md.as_deref(), Some("summary"));
                }
                _ => panic!("expected ContextCompaction"),
            }
        });
    }

    #[gpui::test]
    fn assistant_markdown_matches_persisted(cx: &mut TestAppContext) {
        use acp_thread::{AgentThreadEntry, AssistantMessage, AssistantMessageChunk, ContentBlock};
        cx.update(|cx| {
            let entry = AgentThreadEntry::AssistantMessage(AssistantMessage {
                chunks: vec![AssistantMessageChunk::Message {
                    block: ContentBlock::Markdown {
                        markdown: cx
                            .new(|cx| markdown::Markdown::new("**bold**".into(), None, None, cx)),
                    },
                }],
                indented: false,
                is_subagent_output: false,
                subagent_id: None,
            });
            let sawe = to_session_entry(&entry, cx);
            let persisted = crate::cold_persistence::to_persisted(&entry, cx).unwrap();
            let sawe_md = match sawe.kind {
                SessionEntryKind::AssistantMessage { chunks } => match &chunks[0] {
                    AssistantChunk::Message(m) => m.clone(),
                    _ => panic!(),
                },
                _ => panic!(),
            };
            let persisted_md = match persisted {
                crate::cold_persistence::PersistedEntryV2::Assistant(a) => match &a.chunks[0] {
                    crate::cold_persistence::PersistedAssistantChunk::Message(m) => m.clone(),
                    _ => panic!(),
                },
                _ => panic!(),
            };
            assert_eq!(sawe_md, persisted_md);
        });
    }

    #[test]
    fn session_entry_serde_round_trips() {
        let entry = sample_tool();
        let json = serde_json::to_string(&entry).unwrap();
        let back: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.created_ms, entry.created_ms);
        assert_eq!(back.mod_seq, entry.mod_seq);
        assert_eq!(back.subagent_id, entry.subagent_id);
        match back.kind {
            SessionEntryKind::ToolCall {
                status,
                status_started_at,
                ..
            } => {
                assert!(matches!(status, ToolStatus::InProgress));
                assert_eq!(status_started_at, Some(1_700_000_000_500));
            }
            _ => panic!("variant changed across round-trip"),
        }
    }

    #[test]
    fn payload_codec_round_trips_kind() {
        let entry = sample_tool();
        let bytes = entry.to_payload();
        assert!(!bytes.is_empty());
        let decoded = kind_from_payload(&bytes).expect("kind_from_payload should succeed");
        assert_eq!(decoded, entry.kind);
    }
}
