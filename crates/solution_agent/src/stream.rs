//! The per-source stream model (design: docs/superpowers/specs/2026-07-06-per-source-streams-design.md).
//! A `Stream` is a single-source ordered slice of a session's transcript: the
//! parent agent (`Main`), one async `Agent` teammate, or one background shell.
//! Because a stream is single-source, coalescing consecutive assistant messages
//! is a plain `last()` merge — later phases delete the `AcpThread` backward-scan
//! and the Main render filter that only existed to compensate for the old flat,
//! interleaved list.

use crate::background_shell::BackgroundShellId;
use crate::session_entry::{SessionEntry, SessionEntryKind};
use gpui::SharedString;
use std::path::PathBuf;

/// Which stream an entry belongs to. `Teammate` carries the parent `Agent`
/// tool_use id (`toolu_…`) that all of that teammate's entries are tagged with.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StreamId {
    Main,
    Teammate(SharedString),
    Shell(BackgroundShellId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Main,
    Teammate,
    Shell,
}

/// A secondary stream auto-closes on `Done`; `Main` is always `Live`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamState {
    Live,
    Done { reason: SharedString },
}

/// Where a stream's entries come from. Main + teammates are demultiplexed from
/// the parent `AcpThread`; a shell tails its `.output` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamSource {
    ParentThreadDemux,
    FileTail(PathBuf),
}

#[derive(Clone, Debug)]
pub struct Stream {
    pub id: StreamId,
    pub kind: StreamKind,
    pub label: SharedString,
    pub entries: Vec<SessionEntry>,
    /// Per-stream delta watermark (replaces the old global `mod_seq` churn).
    pub seq: u64,
    pub state: StreamState,
    pub source: StreamSource,
}

impl Stream {
    pub fn main() -> Self {
        Stream {
            id: StreamId::Main,
            kind: StreamKind::Main,
            label: SharedString::new_static("Main"),
            entries: Vec::new(),
            seq: 0,
            state: StreamState::Live,
            source: StreamSource::ParentThreadDemux,
        }
    }

    pub fn teammate(id: SharedString) -> Self {
        Stream {
            id: StreamId::Teammate(id.clone()),
            kind: StreamKind::Teammate,
            label: id,
            entries: Vec::new(),
            seq: 0,
            state: StreamState::Live,
            source: StreamSource::ParentThreadDemux,
        }
    }

    /// Append `entry`, merging it into the previous entry when both are
    /// `AssistantMessage`. A stream is single-source, so a plain `last()` merge
    /// is correct: interleaving from *other* sources lives in *other* streams,
    /// and a tool call (or any non-message entry) between two messages sits
    /// between them and so is a natural boundary.
    pub fn push_coalesced(&mut self, entry: SessionEntry) {
        if let SessionEntryKind::AssistantMessage { chunks: incoming } = &entry.kind
            && let Some(last) = self.entries.last_mut()
            && let SessionEntryKind::AssistantMessage { chunks } = &mut last.kind
        {
            chunks.extend(incoming.iter().cloned());
            return;
        }
        self.entries.push(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};

    fn assistant(text: &str) -> SessionEntry {
        SessionEntry {
            created_ms: 0,
            mod_seq: 0,
            subagent_id: None,
            kind: SessionEntryKind::AssistantMessage {
                chunks: vec![AssistantChunk::Message(text.to_string())],
            },
        }
    }

    fn tool_call(id: &str) -> SessionEntry {
        SessionEntry {
            created_ms: 0,
            mod_seq: 0,
            subagent_id: None,
            kind: SessionEntryKind::ToolCall {
                id: id.to_string(),
                label_md: "Bash".to_string(),
                kind: agent_client_protocol::schema::ToolKind::Execute,
                status: crate::session_entry::ToolStatus::Completed,
                content_md: Vec::new(),
                raw_input: None,
                raw_output: None,
                tool_name: Some("Bash".to_string()),
                locations: Vec::new(),
                status_started_at: None,
            },
        }
    }

    #[test]
    fn main_stream_is_empty_live_parent() {
        let s = Stream::main();
        assert_eq!(s.id, StreamId::Main);
        assert_eq!(s.kind, StreamKind::Main);
        assert_eq!(s.label.as_ref(), "Main");
        assert!(s.entries.is_empty());
        assert_eq!(s.seq, 0);
        assert_eq!(s.state, StreamState::Live);
        assert_eq!(s.source, StreamSource::ParentThreadDemux);
    }

    #[test]
    fn consecutive_assistant_messages_coalesce_into_one_entry() {
        let mut s = Stream::main();
        s.push_coalesced(assistant("Three "));
        s.push_coalesced(assistant("scouts"));
        assert_eq!(s.entries.len(), 1, "same-source assistant messages must merge");
        let SessionEntryKind::AssistantMessage { chunks } = &s.entries[0].kind else {
            panic!("expected AssistantMessage");
        };
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn a_tool_call_between_messages_is_a_boundary() {
        let mut s = Stream::main();
        s.push_coalesced(assistant("before"));
        s.push_coalesced(tool_call("tc1"));
        s.push_coalesced(assistant("after"));
        assert_eq!(
            s.entries.len(),
            3,
            "a tool call between two messages must NOT be coalesced away"
        );
    }

    #[test]
    fn non_assistant_entries_never_merge() {
        let mut s = Stream::main();
        s.push_coalesced(tool_call("tc1"));
        s.push_coalesced(tool_call("tc2"));
        assert_eq!(s.entries.len(), 2);
    }

    #[test]
    fn teammate_stream_is_empty_live_teammate() {
        let s = Stream::teammate(SharedString::from("toolu_abc"));
        assert_eq!(s.id, StreamId::Teammate(SharedString::from("toolu_abc")));
        assert_eq!(s.kind, StreamKind::Teammate);
        assert_eq!(s.label.as_ref(), "toolu_abc");
        assert!(s.entries.is_empty());
        assert_eq!(s.state, StreamState::Live);
        assert_eq!(s.source, StreamSource::ParentThreadDemux);
    }
}
