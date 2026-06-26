//! `solution_agent`'s own transcript entry — the single type that is both
//! rendered (Phase 2) and serialized to per-entry DB rows + the mobile wire
//! (Phase 3-5). Content is held as markdown source strings + structured
//! metadata, the shape the renderer already projects to, so a cold/synced
//! entry paints identically to a live one. Replaces the lossy
//! `cold_persistence::PersistedEntryV2` once Phases 2-3 land.

use agent_client_protocol::schema as acp;
use gpui::SharedString;
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema as acp;

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

    #[test]
    fn session_entry_serde_round_trips() {
        let entry = sample_tool();
        let json = serde_json::to_string(&entry).unwrap();
        let back: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.created_ms, entry.created_ms);
        assert_eq!(back.mod_seq, entry.mod_seq);
        assert_eq!(back.subagent_id, entry.subagent_id);
        match back.kind {
            SessionEntryKind::ToolCall { status, status_started_at, .. } => {
                assert!(matches!(status, ToolStatus::InProgress));
                assert_eq!(status_started_at, Some(1_700_000_000_500));
            }
            _ => panic!("variant changed across round-trip"),
        }
    }
}
