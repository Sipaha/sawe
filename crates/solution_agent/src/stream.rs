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
use indexmap::IndexMap;
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
            // The coalesced entry's `mod_seq` is the phase-4 wire delta key. A
            // merge keeps the FIRST fragment's chunks in place but MUST advance
            // the key so a client polling `entry.mod_seq > since_seq` sees the
            // update (decision #5 — otherwise a coalesced-message update is
            // silently missed). Stream entries are clones, so bumping this copy's
            // mod_seq is local to the stream mirror.
            last.mod_seq = last.mod_seq.max(entry.mod_seq);
            return;
        }
        self.entries.push(entry);
    }
}

/// Group a flat, interleaved entry list into per-source streams, coalescing
/// each stream's consecutive assistant messages. `Main` is always present
/// (inserted first, possibly empty); teammate streams appear in first-seen
/// order. Pure — a derived view over `session.entries`, not duplicated state.
/// Shell streams are not produced here (their content lives outside `entries`).
pub fn demux(entries: &[SessionEntry]) -> IndexMap<StreamId, Stream> {
    let mut streams: IndexMap<StreamId, Stream> = IndexMap::new();
    streams.insert(StreamId::Main, Stream::main());
    for entry in entries {
        let stream = match &entry.subagent_id {
            None => streams
                .get_mut(&StreamId::Main)
                .expect("Main is inserted above"),
            Some(toolu) => streams
                .entry(StreamId::Teammate(toolu.clone()))
                .or_insert_with(|| Stream::teammate(toolu.clone())),
        };
        stream.push_coalesced(entry.clone());
    }
    streams
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
    fn coalesce_merge_raises_merged_entry_mod_seq() {
        let mut s = Stream::main();
        let mut first = assistant("Three ");
        first.mod_seq = 1;
        let mut second = assistant("scouts");
        second.mod_seq = 4;
        s.push_coalesced(first);
        s.push_coalesced(second);
        assert_eq!(s.entries.len(), 1, "same-source assistant messages must merge");
        assert_eq!(
            s.entries[0].mod_seq, 4,
            "a coalesce-merge must advance the frozen first-fragment mod_seq to the incoming max"
        );
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

    fn assistant_tagged(text: &str, sub: Option<&str>) -> SessionEntry {
        SessionEntry {
            created_ms: 0,
            mod_seq: 0,
            subagent_id: sub.map(SharedString::from),
            kind: SessionEntryKind::AssistantMessage {
                chunks: vec![AssistantChunk::Message(text.to_string())],
            },
        }
    }

    #[test]
    fn demux_empty_yields_only_empty_main() {
        let streams = demux(&[]);
        assert_eq!(streams.len(), 1);
        assert!(streams.contains_key(&StreamId::Main));
        assert!(streams[&StreamId::Main].entries.is_empty());
    }

    #[test]
    fn demux_reunites_a_parent_message_split_by_an_interleaved_teammate() {
        // Flat, interleaved (as AcpThread would produce WITHOUT the backward-scan):
        // parent "Three ", teammate chunk, parent "scouts".
        let flat = vec![
            assistant_tagged("Three ", None),
            assistant_tagged("subagent noise", Some("T1")),
            assistant_tagged("scouts", None),
        ];
        let streams = demux(&flat);
        assert_eq!(streams.len(), 2, "Main + one teammate");
        // Main: the two parent fragments are now adjacent → coalesced to ONE entry.
        let main = &streams[&StreamId::Main];
        assert_eq!(main.entries.len(), 1, "parent message reunited");
        let SessionEntryKind::AssistantMessage { chunks } = &main.entries[0].kind else {
            panic!("expected AssistantMessage");
        };
        assert_eq!(chunks.len(), 2);
        // Teammate stream holds only its own entry.
        let t1 = &streams[&StreamId::Teammate(SharedString::from("T1"))];
        assert_eq!(t1.entries.len(), 1);
        assert_eq!(t1.kind, StreamKind::Teammate);
    }

    #[test]
    fn demux_orders_teammate_streams_by_first_appearance() {
        let flat = vec![
            assistant_tagged("m", None),
            assistant_tagged("b", Some("T2")),
            assistant_tagged("a", Some("T1")),
        ];
        let streams = demux(&flat);
        let ids: Vec<&StreamId> = streams.keys().collect();
        assert_eq!(ids[0], &StreamId::Main);
        assert_eq!(ids[1], &StreamId::Teammate(SharedString::from("T2")));
        assert_eq!(ids[2], &StreamId::Teammate(SharedString::from("T1")));
    }

    #[test]
    fn demux_with_no_parent_entries_still_has_empty_main() {
        let flat = vec![assistant_tagged("only sub", Some("T1"))];
        let streams = demux(&flat);
        assert!(streams[&StreamId::Main].entries.is_empty());
        assert_eq!(streams[&StreamId::Teammate(SharedString::from("T1"))].entries.len(), 1);
    }
}
