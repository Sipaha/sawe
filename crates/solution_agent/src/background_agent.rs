//! Tracking surface for Claude Code's built-in **Managed Agents** —
//! the async sub-agents that the parent claude dispatches via the
//! `Agent` tool. Unlike inline `Task` subagents whose transcript
//! is interleaved into the parent's `AcpThread.entries`, a managed
//! agent gets its own JSONL file at
//! `~/.claude/projects/<encoded-cwd>/<session-id>/subagents/agent-<id>.jsonl`
//! and runs autonomously until it emits a terminal `stop_reason`.
//!
//! This module owns:
//!
//! - [`BackgroundAgentId`] — newtype around the hex id Claude Code
//!   prints in the tool output.
//! - [`BackgroundAgent`] + [`BackgroundAgentSnapshot`] — in-memory
//!   tracking state per agent.
//! - [`parse_managed_agent_announcement`] — the regex parser run on
//!   completed `Agent`-tool_call `raw_output`.
//! - JSONL tail / convert helpers (added in later tasks).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use acp_thread::{
    AgentThreadEntry, AssistantMessage, AssistantMessageChunk, ContentBlock, ToolCall,
    ToolCallStatus, UserMessage,
};
use agent_client_protocol::schema as acp;
use chrono::{DateTime, Utc};
use gpui::{App, AppContext, SharedString};
use markdown::Markdown;

use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
use regex::Regex;
use serde_json::Value;

static AGENT_ID_RE: OnceLock<Regex> = OnceLock::new();
static OUTPUT_FILE_RE: OnceLock<Regex> = OnceLock::new();

fn agent_id_re() -> &'static Regex {
    AGENT_ID_RE.get_or_init(|| {
        Regex::new(r"agentId:\s+([0-9a-f]{16,32})\b").expect("static regex compiles")
    })
}

fn output_file_re() -> &'static Regex {
    OUTPUT_FILE_RE.get_or_init(|| {
        Regex::new(r"output_file:\s+(\S+\.output)\b").expect("static regex compiles")
    })
}

/// Best-effort parse of an `Agent`-tool_call's `raw_output`. Returns
/// `Some((agent_id, output_file_path))` when both markers are present
/// AND the id is 16–32 hex chars AND the path ends `.output`.
/// `None` otherwise — caller silently skips registration so a future
/// claude version that reshapes the output doesn't spam the log.
///
/// Path is returned as-is (often a symlink under `/tmp/claude-<uid>/`);
/// caller resolves via `read_link` to the canonical JSONL location.
pub fn parse_managed_agent_announcement(raw_output: &str) -> Option<(String, PathBuf)> {
    let id = agent_id_re()
        .captures(raw_output)?
        .get(1)?
        .as_str()
        .to_string();
    let path = output_file_re().captures(raw_output)?.get(1)?.as_str();
    Some((id, PathBuf::from(path)))
}

/// Recover a terminal `Agent` tool call's managed-agent announcement, looking
/// in `raw_output` first then the tool call's rendered `content`.
///
/// For an async `Agent` launch claude emits the announcement (`agentId:` +
/// `output_file:`) in the tool_result BODY — which the native adapter surfaces
/// as the tool call's content — and leaves `raw_output` null. Parsing only
/// `raw_output` therefore never registers the background-agent strip pill, so
/// an actively-streaming teammate shows no tab and its parent-thread-tagged
/// output leaks into Main. `raw_output` is still tried first so a future
/// dispatcher that stashes the announcement there keeps working.
pub fn managed_agent_announcement(
    raw_output: Option<&str>,
    content: Option<&str>,
) -> Option<(String, PathBuf)> {
    raw_output
        .and_then(parse_managed_agent_announcement)
        .or_else(|| content.and_then(parse_managed_agent_announcement))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BackgroundAgentId(SharedString);

impl BackgroundAgentId {
    pub fn new(id: impl Into<SharedString>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }

    /// First 6 hex chars — what the pill renders so the user has
    /// something glanceable instead of the full 17-32 char id.
    pub fn short(&self) -> String {
        self.0.chars().take(6).collect()
    }
}

impl std::fmt::Display for BackgroundAgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_ref())
    }
}

#[derive(Clone, Debug)]
pub struct BackgroundAgent {
    pub id: BackgroundAgentId,
    /// Canonical (symlink-resolved) JSONL path on disk.
    pub jsonl_path: PathBuf,
    pub registered_at: DateTime<Utc>,
    pub latest: Option<BackgroundAgentSnapshot>,
    /// Byte offset past the last bytes that `refresh_background_agent_snapshot`
    /// successfully tailed. Carried across fs-watch events so each refresh
    /// only re-reads the new bytes (capped at `JSONL_LINE_CAP`) instead of
    /// the whole transcript. Reset to 0 by `tail_jsonl` when the file
    /// shrinks (truncation / replacement), so a rotated JSONL re-reads
    /// from the beginning rather than getting stuck past EOF.
    pub last_offset: u64,
    /// The parent `Agent` spawn tool-call's tool_use id — the key of this
    /// teammate's demux `Teammate` stream (StreamId::Teammate). Captured at
    /// live registration so the stream can be auto-closed on the agent's real
    /// terminal `stop_reason`. `None` when unknown (a DB cold-restore does not
    /// persist it — those streams are hydration orphans, already Main-only).
    pub parent_tool_use_id: Option<SharedString>,
    /// `change_seq`-axis stamp for the folded-pill `stream_entry`'s `mod_seq`
    /// (bumped in `refresh_background_agent_snapshot` each time `latest`
    /// advances). MUST be on the same monotonic axis as demux entries: the
    /// folded pill and a later demux stream can share one `Teammate(toolu)` id,
    /// so a mtime-millis stamp (~1.7e12) here would collapse the stream's wire
    /// `seq` when demux takes over and strand the mobile delta cursor.
    pub latest_seq: u64,
    /// The owning `claude` subprocess died / was replaced (watchdog reconnect,
    /// manual reconnect, crash) while this agent was still running. A Managed
    /// Agent is a CHILD of that subprocess, so it went down with it: it did not
    /// reach a `stop_reason` and it never will — no further JSONL line can ever
    /// arrive. This is a distinct terminal outcome from a genuine `stop_reason`
    /// completion, and the UI must say so rather than keep painting a "running"
    /// teammate tab for work that is gone (or, worse, silently drop the tab and
    /// imply it finished).
    pub killed: bool,
}

/// Close reason recorded on a killed agent's teammate stream. Deliberately not
/// "done": the agent was reaped mid-flight.
pub const KILLED_REASON: SharedString = SharedString::new_static("killed");

/// Close reason recorded on a teammate that hit a claude usage/session-limit
/// wall. Distinct from "done" (didn't finish) and "killed" (subprocess
/// replaced): the work is paused at the wall, awaiting the reset.
pub const USAGE_LIMIT_REASON: SharedString = SharedString::new_static("limit reached");

impl BackgroundAgent {
    /// True while the managed agent is still running — no terminal
    /// `stop_reason` has been observed in its JSONL yet (or it has only
    /// just registered, before any snapshot) AND its parent subprocess is
    /// still the one that spawned it. Since phase 6d-tail this only
    /// feeds the supervisor's `has_live_background_work` gate (the compose row
    /// no longer branches on it — async agents render as view-only `Task`
    /// teammate tabs). A `killed` agent is NOT live work: counting it there
    /// would suppress the stuck-session watchdog forever after a reconnect.
    pub fn is_messageable(&self) -> bool {
        !self.killed
            && !self.hit_usage_limit()
            && self
                .latest
                .as_ref()
                .map_or(true, |snapshot| snapshot.stop_reason.is_none())
    }

    /// True once this agent's last snapshot is a claude usage-limit wall — a
    /// distinct terminal outcome (see [`BackgroundAgentSnapshot::usage_limited`]).
    pub fn hit_usage_limit(&self) -> bool {
        self.latest.as_ref().is_some_and(|s| s.usage_limited)
    }

    /// Whether this agent still contributes a derived teammate stream to the
    /// mirror. A running agent obviously does; a KILLED one keeps its tab (in
    /// the terminal `Done { killed }` state) until the reaper ages it out, so
    /// the user sees *why* the work stopped. An agent that reached a genuine
    /// `stop_reason` is dropped straight away (`tick_background_agents` removes
    /// it from the map on the next pass) — its transcript ended normally.
    pub fn renders_stream(&self) -> bool {
        self.killed || self.hit_usage_limit() || self.is_messageable()
    }

    /// Render state of this agent's derived teammate stream.
    pub fn stream_state(&self) -> crate::stream::StreamState {
        if self.killed {
            crate::stream::StreamState::Done {
                reason: KILLED_REASON,
            }
        } else if self.hit_usage_limit() {
            crate::stream::StreamState::Done {
                reason: USAGE_LIMIT_REASON,
            }
        } else {
            crate::stream::StreamState::Live
        }
    }

    /// Convert this agent's last-observed snapshot into the single
    /// [`SessionEntry`] body of its derived `StreamId::Teammate` stream — the
    /// analogue of `BackgroundShell::stream_entry` (phase 6d-A). Plain data (no
    /// `Markdown` entity) so it can run inside the `cx`-free
    /// `SolutionSession::rebuild_streams`. The full JSONL transcript lives in
    /// `jsonl_path` and is intentionally NOT inlined — only the tailed one-line
    /// `activity_label` snapshot is shown, mirroring the pill's status.
    pub fn stream_entry(&self, now: DateTime<Utc>) -> SessionEntry {
        let (activity, observed, done) = match &self.latest {
            Some(snap) => (
                snap.activity_label.to_string(),
                agent_relative_time(snap.mtime, now),
                snap.stop_reason.is_some(),
            ),
            None => (
                "Starting…".to_string(),
                "no output yet".to_string(),
                false,
            ),
        };
        let state_label = if self.killed {
            "killed"
        } else if self.hit_usage_limit() {
            "limit reached"
        } else if done {
            "done"
        } else {
            "running"
        };
        let header = format!(
            "Agent {} · {} · {}",
            self.id.short(),
            state_label,
            observed
        );
        let body = if self.killed {
            format!(
                "{activity}\n\nKilled: the parent claude process was restarted \
                 (reconnect). This background agent did NOT finish its work."
            )
        } else if self.hit_usage_limit() {
            format!(
                "{activity}\n\nЛимит claude достигнут — этот фоновый агент \
                 остановлен и не завершил работу. Наблюдатель перезапустит \
                 после сброса лимита."
            )
        } else {
            activity
        };
        let text = format!("{header}\n\n{body}");
        let created_ms = self
            .latest
            .as_ref()
            .and_then(|snap| snap.mtime.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|dur| dur.as_millis() as i64)
            .unwrap_or(0);
        SessionEntry {
            created_ms,
            // `mod_seq` rides the `change_seq` axis (NOT the mtime), so the
            // folded pill's wire `seq` stays compatible with a demux stream that
            // may later claim the same `Teammate(toolu)` id.
            mod_seq: self.latest_seq,
            subagent_id: None,
            kind: SessionEntryKind::AssistantMessage {
                chunks: vec![AssistantChunk::Message(text)],
            },
        }
    }
}

/// "X ago" formatter for a background-agent snapshot's `SystemTime` mtime.
/// Mirrors `background_shell`'s private helper; kept `cx`-free so
/// `stream_entry` can run inside `SolutionSession::rebuild_streams`.
fn agent_relative_time(mtime: SystemTime, now: DateTime<Utc>) -> String {
    let secs = match mtime.duration_since(std::time::UNIX_EPOCH) {
        Ok(dur) => now.timestamp().saturating_sub(dur.as_secs() as i64).max(0),
        Err(_) => return "just now".to_string(),
    };
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[derive(Clone, Debug)]
pub struct BackgroundAgentSnapshot {
    pub mtime: SystemTime,
    pub activity_label: SharedString,
    pub stop_reason: Option<SharedString>,
    /// The last assistant message is a claude usage/session-limit wall
    /// ("You've hit your session limit · resets 12:50pm"). The subagent is
    /// dead-in-place: it hit the wall and will emit no terminal `stop_reason`
    /// (the wall isn't `end_turn`), so without recognising it here the teammate
    /// tab spins "Thinking…" until the 2-minute stale reaper — 30 minutes of a
    /// visibly-dead tab in practice. Treated as a distinct terminal outcome
    /// (like `killed`): the tab immediately goes `Done { limit }`, spinner off.
    pub usage_limited: bool,
}

/// 64 KiB cap on individual JSONL line size. A claude tool_use entry
/// is well under 4 KiB in practice; an entry past this cap is treated
/// as `Generating…` so a pathological line can't blow our memory.
const JSONL_LINE_CAP: usize = 64 * 1024;

/// Whether an assistant message's `stop_reason` means the agent is **DONE**,
/// as opposed to pausing mid-loop.
///
/// This is load-bearing: a `stop_reason` of `tool_use` (the reason on EVERY
/// assistant message that invokes a tool — i.e. almost every message a working
/// agent emits) means the agent loop CONTINUES. Treating it as terminal reaps a
/// live agent on its very first tool call, which killed the strip pill after
/// ~15-30s, dropped the `background_agents` entry, and lied to the supervisor's
/// `has_live_background_work` gate.
///
/// Allow-list rather than deny-list on purpose: an unrecognised reason is read
/// as NON-terminal, so the worst case is a pill that lingers until the
/// `MANAGED_AGENT_STALE_TIMEOUT_SECS` mtime reap (a self-healing backstop). The
/// inverse mistake — reading an unknown reason as terminal — silently kills a
/// running agent's tab with no backstop at all.
fn is_terminal_stop_reason(reason: &str) -> bool {
    matches!(
        reason,
        "end_turn" | "max_tokens" | "stop_sequence" | "refusal"
    )
}

/// Pure JSON → snapshot. Public so the watcher (Task 7) can feed it
/// arbitrary strings; never panics, returns `Generating…` for any
/// shape it doesn't recognise.
pub fn parse_jsonl_snapshot(line: &str) -> BackgroundAgentSnapshot {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return generating_snapshot(),
    };
    let typ = value.get("type").and_then(Value::as_str).unwrap_or("");
    match typ {
        "system" => {
            let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
            if subtype == "init" {
                BackgroundAgentSnapshot {
                    mtime: SystemTime::now(),
                    activity_label: SharedString::new_static("Starting…"),
                    stop_reason: None,
                    usage_limited: false,
                }
            } else {
                generating_snapshot()
            }
        }
        "assistant" => {
            let message = value.get("message").cloned().unwrap_or(Value::Null);
            let stop_reason = message
                .get("stop_reason")
                .and_then(Value::as_str)
                .filter(|s| is_terminal_stop_reason(s))
                .map(SharedString::from);
            // A usage-limit wall arrives as an ordinary assistant TEXT message
            // (no terminal `stop_reason`), so classify it from the message text.
            let usage_limited = assistant_text(&message)
                .is_some_and(|text| crate::supervisor::is_usage_limit_error(&text));
            let label = if usage_limited {
                SharedString::new_static("Достигнут лимит claude")
            } else {
                derive_assistant_label(&message)
            };
            BackgroundAgentSnapshot {
                mtime: SystemTime::now(),
                activity_label: label,
                stop_reason,
                usage_limited,
            }
        }
        _ => generating_snapshot(),
    }
}

fn generating_snapshot() -> BackgroundAgentSnapshot {
    BackgroundAgentSnapshot {
        mtime: SystemTime::now(),
        activity_label: SharedString::new_static("Generating…"),
        stop_reason: None,
        usage_limited: false,
    }
}

/// Concatenate the `text` blocks of an assistant message (usage-limit walls
/// arrive as plain text, not `tool_use`). `None` when there is no text.
fn assistant_text(message: &Value) -> Option<String> {
    let content = message.get("content").and_then(Value::as_array)?;
    let mut out = String::new();
    for block in content {
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = block.get("text").and_then(Value::as_str)
        {
            out.push_str(text);
            out.push('\n');
        }
    }
    (!out.trim().is_empty()).then_some(out)
}

fn derive_assistant_label(message: &Value) -> SharedString {
    let content = message
        .get("content")
        .and_then(Value::as_array)
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    for block in content {
        let typ = block.get("type").and_then(Value::as_str).unwrap_or("");
        if typ == "tool_use" {
            let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
            let input_preview = block
                .get("input")
                .and_then(|v| v.as_object())
                .and_then(|m| m.values().next())
                .and_then(Value::as_str)
                .unwrap_or("");
            const ARG_BUDGET: usize = 30;
            let truncated = if input_preview.chars().count() > ARG_BUDGET {
                let head: String = input_preview.chars().take(ARG_BUDGET).collect();
                format!("{name}: {head}…")
            } else if input_preview.is_empty() {
                name.to_string()
            } else {
                format!("{name}: {input_preview}")
            };
            return SharedString::from(truncated);
        }
    }
    SharedString::new_static("Generating…")
}

#[derive(Debug, Clone)]
pub struct Tail {
    /// Last non-empty, in-cap line of the file. `None` when:
    ///   * file is empty
    ///   * all lines past `since_offset` are blank
    ///   * the last line exceeds [`JSONL_LINE_CAP`]
    pub last_line: Option<String>,
    /// Offset just past EOF after the read; pass back as
    /// `since_offset` on the next call for incremental tails.
    pub new_offset: u64,
    pub mtime: SystemTime,
}

/// Seek a JSONL file to `since_offset`, read to EOF, return the last
/// non-empty line within the cap. Never loads more than
/// [`JSONL_LINE_CAP`] bytes for the final-line slice — earlier lines
/// in the read window are ignored, since only the latest one drives
/// the snapshot.
///
/// `since_offset` MUST be either `0` or a `new_offset` value returned
/// by a previous call. Passing an arbitrary byte offset that falls
/// mid-line will cause `last_line` to contain a JSON fragment, which
/// `parse_jsonl_snapshot` silently discards as malformed — masking the
/// real most-recent snapshot.
pub fn tail_jsonl(path: &Path, since_offset: u64) -> std::io::Result<Tail> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let metadata = f.metadata()?;
    let mtime = metadata.modified()?;
    let len = metadata.len();
    // Truncation / replacement: caller's stored offset points past EOF.
    // Re-read from byte 0 so a rotated JSONL surfaces its current tail
    // instead of getting stuck on the stale offset.
    let since_offset = if since_offset > len { 0 } else { since_offset };
    if since_offset == len {
        return Ok(Tail {
            last_line: None,
            new_offset: len,
            mtime,
        });
    }
    // Read tail up to JSONL_LINE_CAP + some slack so we can locate
    // line boundaries. If the final line is larger than the cap,
    // we'll detect that and drop it.
    let slack = JSONL_LINE_CAP + 4096;
    let read_start = std::cmp::max(since_offset, len.saturating_sub(slack as u64));
    f.seek(SeekFrom::Start(read_start))?;
    let mut buf = String::new();
    f.take(len - read_start).read_to_string(&mut buf)?;
    let last = buf
        .split('\n')
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|s| s.to_string());
    let last_line = match last {
        Some(l) if l.len() > JSONL_LINE_CAP => None,
        other => other,
    };
    Ok(Tail {
        last_line,
        new_offset: len,
        mtime,
    })
}

/// Lossy V1 converter from a managed-agent JSONL transcript into a
/// list of [`AgentThreadEntry`] for cold rendering in the strip.
///
/// Mapping:
///   * `system` rows → skipped.
///   * `user.message.content` text blocks → one
///     [`AgentThreadEntry::UserMessage`] per non-empty text block.
///     `tool_result` blocks are NOT promoted to entries — they only
///     drive [`ToolCallStatus`] of the paired `tool_use` (see below).
///   * `assistant.message.content` text blocks → concatenated, then
///     emitted as one [`AgentThreadEntry::AssistantMessage`]. A
///     `tool_use` block flushes the pending text first, then emits
///     [`AgentThreadEntry::ToolCall`] with status
///     [`ToolCallStatus::Completed`] if some later `user.tool_result`
///     references it by `tool_use_id`, else
///     [`ToolCallStatus::Pending`].
///   * Malformed JSON rows are silently skipped.
///
/// Two passes: first collects each `tool_use_id` that has a paired
/// `tool_result`, carrying the result's text + error flag so the
/// second pass can stamp the matching [`ToolCall`] with its content
/// and a [`ToolCallStatus::Failed`] when claude flagged `is_error`.
pub fn jsonl_to_entries<S: AsRef<str>>(lines: &[S], cx: &mut App) -> Vec<AgentThreadEntry> {
    let mut paired: HashMap<String, ToolResultInfo> = HashMap::new();
    for line in lines {
        let trimmed = line.as_ref().trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(content) = value
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                paired.insert(
                    id.to_string(),
                    ToolResultInfo {
                        is_error: block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        content_text: tool_result_content_text(block),
                    },
                );
            }
        }
    }

    let mut entries: Vec<AgentThreadEntry> = Vec::new();
    for line in lines {
        let trimmed = line.as_ref().trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "system" => continue,
            "user" => jsonl_user_to_entries(&value, &mut entries, cx),
            "assistant" => jsonl_assistant_to_entries(&value, &paired, &mut entries, cx),
            _ => continue,
        }
    }
    entries
}

fn jsonl_user_to_entries(value: &Value, out: &mut Vec<AgentThreadEntry>, cx: &mut App) {
    let Some(content) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in content {
        let typ = block.get("type").and_then(Value::as_str).unwrap_or("");
        if typ != "text" {
            continue;
        }
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        out.push(AgentThreadEntry::UserMessage(UserMessage {
            id: None,
            content: ContentBlock::Markdown {
                markdown: cx.new(|cx| Markdown::new(text.to_string().into(), None, None, cx)),
            },
            chunks: Vec::new(),
            checkpoint: None,
            indented: false,
        }));
    }
}

struct ToolResultInfo {
    is_error: bool,
    content_text: String,
}

/// Concatenate every `text`-typed block under a `tool_result`'s
/// `content` array (separated by `\n`). Non-text blocks (images,
/// resources) are silently dropped — V2 lossy.
fn tool_result_content_text(block: &Value) -> String {
    let mut out = String::new();
    let Some(content) = block.get("content").and_then(Value::as_array) else {
        return out;
    };
    for b in content {
        if b.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(t) = b.get("text").and_then(Value::as_str) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    out
}

fn jsonl_assistant_to_entries(
    value: &Value,
    paired: &HashMap<String, ToolResultInfo>,
    out: &mut Vec<AgentThreadEntry>,
    cx: &mut App,
) {
    let Some(content) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    let mut pending_text = String::new();
    for block in content {
        let typ = block.get("type").and_then(Value::as_str).unwrap_or("");
        match typ {
            "text" => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    pending_text.push_str(text);
                }
            }
            "thinking" => {
                // Render claude's reasoning trace as a folded `Thought`
                // chunk, the same shape `cold_persistence::from_persisted`
                // produces. Flush pending text first so order is
                // preserved across mixed text/thinking blocks.
                if let Some(thought) = block.get("thinking").and_then(Value::as_str) {
                    flush_pending_assistant_text(&mut pending_text, out, cx);
                    out.push(AgentThreadEntry::AssistantMessage(AssistantMessage {
                        chunks: vec![AssistantMessageChunk::Thought {
                            block: ContentBlock::Markdown {
                                markdown: cx
                                    .new(|cx| Markdown::new(thought.into(), None, None, cx)),
                            },
                        }],
                        indented: false,
                        is_subagent_output: false,
                        subagent_id: None,
                    }));
                }
            }
            "tool_use" => {
                flush_pending_assistant_text(&mut pending_text, out, cx);
                let Some(tool_use_id) = block.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let name =
                    SharedString::from(block.get("name").and_then(Value::as_str).unwrap_or("tool"));
                let result = paired.get(tool_use_id);
                let raw_input = block.get("input").cloned();
                let status = match result {
                    None => ToolCallStatus::Pending,
                    Some(info) if info.is_error => ToolCallStatus::Failed,
                    Some(_) => ToolCallStatus::Completed,
                };
                let content: Vec<acp_thread::ToolCallContent> = result
                    .filter(|info| !info.content_text.is_empty())
                    .map(|info| {
                        let md = cx.new(|cx| {
                            Markdown::new(info.content_text.clone().into(), None, None, cx)
                        });
                        vec![acp_thread::ToolCallContent::ContentBlock(
                            ContentBlock::Markdown { markdown: md },
                        )]
                    })
                    .unwrap_or_default();
                out.push(AgentThreadEntry::ToolCall(ToolCall {
                    id: acp::ToolCallId::new(format!("background:{tool_use_id}")),
                    label: cx.new(|cx| Markdown::new(name.clone(), None, None, cx)),
                    kind: acp::ToolKind::Other,
                    content,
                    status,
                    locations: Vec::new(),
                    resolved_locations: Vec::new(),
                    raw_input,
                    raw_input_markdown: None,
                    raw_output: None,
                    tool_name: Some(name),
                    subagent_session_info: None,
                    subagent_id: None,
                    sandbox_authorization_details: None,
                    status_started_at: None,
                }));
            }
            _ => continue,
        }
    }
    flush_pending_assistant_text(&mut pending_text, out, cx);
}

fn flush_pending_assistant_text(
    pending: &mut String,
    out: &mut Vec<AgentThreadEntry>,
    cx: &mut App,
) {
    if pending.is_empty() {
        return;
    }
    let text = std::mem::take(pending);
    out.push(AgentThreadEntry::AssistantMessage(AssistantMessage {
        chunks: vec![AssistantMessageChunk::Message {
            block: ContentBlock::Markdown {
                markdown: cx.new(|cx| Markdown::new(text.into(), None, None, cx)),
            },
        }],
        indented: false,
        is_subagent_output: false,
        subagent_id: None,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_agent_id_short_returns_first_six_chars() {
        let id = BackgroundAgentId::new("a30f92a688e431edc");
        assert_eq!(id.short(), "a30f92");
    }

    #[test]
    fn background_agent_id_short_handles_id_shorter_than_six() {
        let id = BackgroundAgentId::new("abc");
        assert_eq!(id.short(), "abc");
    }

    #[test]
    fn parse_managed_agent_announcement_happy_path() {
        let raw = "Async agent launched successfully.\n\
                   agentId: a30f92a688e431edc (internal ID)\n\
                   output_file: /tmp/claude-1000/x/abc/tasks/a30f92a688e431edc.output";
        let parsed = parse_managed_agent_announcement(raw);
        assert!(parsed.is_some());
        let (id, path) = parsed.unwrap();
        assert_eq!(id, "a30f92a688e431edc");
        assert_eq!(
            path,
            PathBuf::from("/tmp/claude-1000/x/abc/tasks/a30f92a688e431edc.output")
        );
    }

    #[test]
    fn managed_agent_announcement_falls_back_to_content_when_raw_output_null() {
        // The real shape claude emits today: `raw_output` is null and the
        // announcement rides in the tool_result body (the tool call's content).
        let content = "````\n\
             Async agent launched successfully. (This tool result is internal metadata.)\n\
             agentId: a874596024f50661f (internal ID - do not mention to user.)\n\
             The agent is working in the background.\n\
             output_file: /tmp/claude-1000/-home-x/b618b048/tasks/a874596024f50661f.output\n\
             ````";
        let parsed = managed_agent_announcement(None, Some(content));
        assert!(parsed.is_some(), "must parse the announcement out of content");
        let (id, path) = parsed.unwrap();
        assert_eq!(id, "a874596024f50661f");
        assert_eq!(
            path,
            PathBuf::from("/tmp/claude-1000/-home-x/b618b048/tasks/a874596024f50661f.output")
        );
    }

    #[test]
    fn managed_agent_announcement_prefers_raw_output_over_content() {
        let raw = "agentId: a30f92a688e431edc\noutput_file: /tmp/raw/a30f92a688e431edc.output";
        let content = "agentId: b111111111111111b\noutput_file: /tmp/content/b.output";
        let (id, _) = managed_agent_announcement(Some(raw), Some(content)).unwrap();
        assert_eq!(id, "a30f92a688e431edc", "raw_output wins when both present");
    }

    #[test]
    fn managed_agent_announcement_none_when_neither_has_markers() {
        assert!(managed_agent_announcement(None, None).is_none());
        assert!(managed_agent_announcement(Some("no markers"), Some("also none")).is_none());
    }

    #[test]
    fn parse_managed_agent_announcement_missing_agent_id_returns_none() {
        let raw = "output_file: /tmp/x/y.output";
        assert!(parse_managed_agent_announcement(raw).is_none());
    }

    #[test]
    fn parse_managed_agent_announcement_missing_output_file_returns_none() {
        let raw = "agentId: a30f92a688e431edc";
        assert!(parse_managed_agent_announcement(raw).is_none());
    }

    #[test]
    fn parse_managed_agent_announcement_ignores_surrounding_text() {
        let raw = "Random words.\n\
                   Do not duplicate this agent's work.\n\
                   agentId:    a30f92a688e431edc\n\
                   More noise. \n\
                   output_file:    /tmp/x/foo.output\n\
                   Trailing line.";
        let parsed = parse_managed_agent_announcement(raw);
        assert!(parsed.is_some());
        let (id, path) = parsed.unwrap();
        assert_eq!(id, "a30f92a688e431edc");
        assert_eq!(path, PathBuf::from("/tmp/x/foo.output"));
    }

    #[test]
    fn parse_managed_agent_announcement_rejects_non_hex_id() {
        let raw = "agentId: NOT-HEX-ID\noutput_file: /tmp/x.output";
        assert!(parse_managed_agent_announcement(raw).is_none());
    }

    #[test]
    fn parse_managed_agent_announcement_rejects_short_id() {
        let raw = "agentId: abcd\noutput_file: /tmp/x.output";
        assert!(parse_managed_agent_announcement(raw).is_none());
    }

    #[test]
    fn parse_managed_agent_announcement_rejects_fifteen_char_hex_id() {
        let raw = "agentId: a30f92a688e431e\noutput_file: /tmp/x.output";
        assert!(parse_managed_agent_announcement(raw).is_none());
    }

    #[test]
    fn parse_managed_agent_announcement_accepts_sixteen_char_hex_id() {
        let raw = "agentId: a30f92a688e431ed\noutput_file: /tmp/x.output";
        let parsed = parse_managed_agent_announcement(raw);
        assert!(parsed.is_some());
        let (id, path) = parsed.unwrap();
        assert_eq!(id, "a30f92a688e431ed");
        assert_eq!(path, std::path::PathBuf::from("/tmp/x.output"));
    }

    #[test]
    fn parse_managed_agent_announcement_requires_dot_output_suffix() {
        let raw = "agentId: a30f92a688e431edc\noutput_file: /tmp/x.jsonl";
        assert!(parse_managed_agent_announcement(raw).is_none());
    }

    #[test]
    fn parse_jsonl_snapshot_tool_use() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test --release"}}]}}"#;
        let snap = parse_jsonl_snapshot(line);
        assert_eq!(snap.activity_label.as_ref(), "Bash: cargo test --release");
        assert!(snap.stop_reason.is_none());
    }

    #[test]
    fn parse_jsonl_snapshot_tool_use_truncates_long_args() {
        let long = "x".repeat(200);
        let line = format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","name":"Bash","input":{{"command":"{long}"}}}}]}}}}"#
        );
        let snap = parse_jsonl_snapshot(&line);
        let label = snap.activity_label.as_ref();
        assert!(label.starts_with("Bash: "));
        assert!(label.ends_with('…'), "expected ellipsis, got: {label:?}");
        assert!(label.len() <= 40, "label too long: {} chars", label.len());
    }

    #[test]
    fn parse_jsonl_snapshot_assistant_text_without_stop_reason() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Sure, let me…"}]}}"#;
        let snap = parse_jsonl_snapshot(line);
        assert_eq!(snap.activity_label.as_ref(), "Generating…");
        assert!(snap.stop_reason.is_none());
    }

    #[test]
    fn parse_jsonl_snapshot_terminal_stop_reason_end_turn() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn"}}"#;
        let snap = parse_jsonl_snapshot(line);
        assert_eq!(snap.stop_reason.as_deref(), Some("end_turn"));
    }

    /// The regression that killed a live agent's pill ~15-30s in: an assistant
    /// message that invokes a tool carries `stop_reason: "tool_use"`, which is a
    /// mid-loop pause, NOT the end of the agent's work.
    #[test]
    fn parse_jsonl_snapshot_tool_use_stop_reason_is_not_terminal() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo build"}}],"stop_reason":"tool_use"}}"#;
        let snap = parse_jsonl_snapshot(line);
        assert_eq!(snap.activity_label.as_ref(), "Bash: cargo build");
        assert!(
            snap.stop_reason.is_none(),
            "a tool_use stop means the agent loop continues"
        );
    }

    /// A usage/session-limit wall arrives as a plain assistant TEXT message
    /// with no terminal `stop_reason`. It must be flagged `usage_limited` so the
    /// teammate tab goes terminal (`Done { limit reached }`, spinner off) at
    /// once instead of spinning "Thinking…" until the 2-minute stale reaper.
    #[test]
    fn parse_jsonl_snapshot_usage_limit_wall_is_flagged() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"You've hit your session limit · resets 12:50pm (Asia/Novosibirsk)"}]}}"#;
        let snap = parse_jsonl_snapshot(line);
        assert!(snap.usage_limited, "the session-limit wall must be recognised");
        assert!(
            snap.stop_reason.is_none(),
            "the wall carries no terminal stop_reason — usage_limited is the signal"
        );
    }

    #[test]
    fn ordinary_assistant_text_is_not_usage_limited() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"added rate limit handling to the client"}]}}"#;
        let snap = parse_jsonl_snapshot(line);
        assert!(
            !snap.usage_limited,
            "prose mentioning rate limits must not trip the wall detector"
        );
    }

    #[test]
    fn usage_limited_agent_is_terminal_not_live() {
        let mut agent = BackgroundAgent {
            id: BackgroundAgentId::new("a30f92a688e431edc"),
            jsonl_path: PathBuf::from("/tmp/x.jsonl"),
            registered_at: Utc::now(),
            latest: None,
            last_offset: 0,
            parent_tool_use_id: Some(SharedString::new_static("toolu_1")),
            latest_seq: 0,
            killed: false,
        };
        agent.latest = Some(parse_jsonl_snapshot(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"You've hit your session limit · resets 12:50pm"}]}}"#,
        ));
        assert!(agent.hit_usage_limit());
        assert!(!agent.is_messageable(), "a walled agent is not live work");
        assert!(agent.renders_stream(), "but its tab stays visible with the reason");
        assert_eq!(
            agent.stream_state(),
            crate::stream::StreamState::Done {
                reason: USAGE_LIMIT_REASON,
            },
        );
    }

    #[test]
    fn parse_jsonl_snapshot_pause_turn_and_unknown_stops_are_not_terminal() {
        for reason in ["pause_turn", "something_new"] {
            let line = format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"…"}}],"stop_reason":"{reason}"}}}}"#
            );
            let snap = parse_jsonl_snapshot(&line);
            assert!(
                snap.stop_reason.is_none(),
                "{reason} must not reap a live agent (the stale-mtime reap is the backstop)"
            );
        }
    }

    #[test]
    fn parse_jsonl_snapshot_other_terminal_stops_are_terminal() {
        for reason in ["max_tokens", "stop_sequence", "refusal"] {
            let line = format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"…"}}],"stop_reason":"{reason}"}}}}"#
            );
            let snap = parse_jsonl_snapshot(&line);
            assert_eq!(snap.stop_reason.as_deref(), Some(reason));
        }
    }

    #[test]
    fn parse_jsonl_snapshot_system_init() {
        let line = r#"{"type":"system","subtype":"init","cwd":"/x","tools":[]}"#;
        let snap = parse_jsonl_snapshot(line);
        assert_eq!(snap.activity_label.as_ref(), "Starting…");
        assert!(snap.stop_reason.is_none());
    }

    #[test]
    fn parse_jsonl_snapshot_malformed_returns_unknown() {
        let snap = parse_jsonl_snapshot("not json at all");
        assert_eq!(snap.activity_label.as_ref(), "Generating…");
        assert!(snap.stop_reason.is_none());
    }

    #[test]
    fn tail_jsonl_reads_last_nonempty_line() -> std::io::Result<()> {
        use std::io::Write;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("agent.jsonl");
        let mut f = std::fs::File::create(&path)?;
        writeln!(f, r#"{{"type":"system","subtype":"init"}}"#)?;
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"hi"}}]}}}}"#
        )?;
        f.write_all(b"\n")?; // trailing blank line
        let tail = tail_jsonl(&path, 0)?;
        assert!(tail.last_line.is_some());
        let last = tail.last_line.unwrap();
        assert!(last.contains(r#""type":"assistant""#));
        assert!(tail.new_offset > 0);
        Ok(())
    }

    #[gpui::test]
    async fn jsonl_to_entries_basic_round(cx: &mut gpui::TestAppContext) {
        let lines = vec![
            r#"{"type":"system","subtype":"init"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"world"}]}}"#,
        ];
        let entries = cx.update(|cx| jsonl_to_entries(&lines, cx));
        assert_eq!(entries.len(), 2, "system row should be skipped");
        let roles: Vec<_> = entries
            .iter()
            .map(|e| match e {
                acp_thread::AgentThreadEntry::UserMessage(_) => "user",
                acp_thread::AgentThreadEntry::AssistantMessage(_) => "assistant",
                acp_thread::AgentThreadEntry::ToolCall(_) => "tool_call",
                acp_thread::AgentThreadEntry::CompletedPlan(_) => "plan",
                acp_thread::AgentThreadEntry::ContextCompaction(_) => "compaction",
                acp_thread::AgentThreadEntry::SystemNote(_) => "system",
            })
            .collect();
        assert_eq!(roles, vec!["user", "assistant"]);
    }

    #[gpui::test]
    async fn jsonl_to_entries_skips_malformed_rows(cx: &mut gpui::TestAppContext) {
        let lines = vec![
            "not json",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"only valid"}]}}"#,
        ];
        let entries = cx.update(|cx| jsonl_to_entries(&lines, cx));
        assert_eq!(entries.len(), 1);
    }

    #[gpui::test]
    async fn jsonl_to_entries_renders_thinking_as_thought_chunk(cx: &mut gpui::TestAppContext) {
        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"weighing the trade-offs"},{"type":"text","text":"answer"}]}}"#,
        ];
        let entries = cx.update(|cx| jsonl_to_entries(&lines, cx));
        // Thinking flushes pending text first (empty here), emits its
        // own AssistantMessage, then the trailing text emits another.
        assert_eq!(entries.len(), 2, "thinking + text = two AssistantMessages");
        let acp_thread::AgentThreadEntry::AssistantMessage(thought_msg) = &entries[0] else {
            panic!("expected AssistantMessage at index 0, got {:?}", entries[0]);
        };
        assert!(
            matches!(
                thought_msg.chunks[0],
                acp_thread::AssistantMessageChunk::Thought { .. }
            ),
            "first chunk must be Thought, got {:?}",
            thought_msg.chunks[0],
        );
        let acp_thread::AgentThreadEntry::AssistantMessage(text_msg) = &entries[1] else {
            panic!("expected AssistantMessage at index 1, got {:?}", entries[1]);
        };
        assert!(
            matches!(
                text_msg.chunks[0],
                acp_thread::AssistantMessageChunk::Message { .. }
            ),
            "trailing text must be Message chunk, got {:?}",
            text_msg.chunks[0],
        );
    }

    #[gpui::test]
    async fn jsonl_to_entries_lifts_tool_result_text_into_content(cx: &mut gpui::TestAppContext) {
        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":[{"type":"text","text":"src/\ntarget/"}]}]}}"#,
        ];
        let entries = cx.update(|cx| jsonl_to_entries(&lines, cx));
        let acp_thread::AgentThreadEntry::ToolCall(tc) = &entries[0] else {
            panic!("expected ToolCall, got {:?}", entries[0]);
        };
        assert_eq!(
            tc.content.len(),
            1,
            "result text should land as one ContentBlock"
        );
        assert!(matches!(
            tc.content[0],
            acp_thread::ToolCallContent::ContentBlock(acp_thread::ContentBlock::Markdown { .. })
        ));
    }

    #[gpui::test]
    async fn jsonl_to_entries_tool_result_is_error_lands_failed(cx: &mut gpui::TestAppContext) {
        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_3","name":"Bash","input":{"command":"bad"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_3","is_error":true,"content":[{"type":"text","text":"command not found"}]}]}}"#,
        ];
        let entries = cx.update(|cx| jsonl_to_entries(&lines, cx));
        let acp_thread::AgentThreadEntry::ToolCall(tc) = &entries[0] else {
            panic!("expected ToolCall, got {:?}", entries[0]);
        };
        assert!(
            matches!(tc.status, acp_thread::ToolCallStatus::Failed),
            "is_error=true should land Failed, got {:?}",
            tc.status,
        );
        assert_eq!(tc.content.len(), 1, "error text still lifted into content");
    }

    #[gpui::test]
    async fn jsonl_to_entries_unpaired_tool_use_has_empty_content(cx: &mut gpui::TestAppContext) {
        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_4","name":"Bash","input":{"command":"ls"}}]}}"#,
        ];
        let entries = cx.update(|cx| jsonl_to_entries(&lines, cx));
        let acp_thread::AgentThreadEntry::ToolCall(tc) = &entries[0] else {
            panic!("expected ToolCall, got {:?}", entries[0]);
        };
        assert!(matches!(tc.status, acp_thread::ToolCallStatus::Pending));
        assert!(
            tc.content.is_empty(),
            "unpaired tool_use should have no content"
        );
    }

    #[gpui::test]
    async fn jsonl_to_entries_pairs_tool_use_with_tool_result(cx: &mut gpui::TestAppContext) {
        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"foo bar"}]}]}}"#,
        ];
        let entries = cx.update(|cx| jsonl_to_entries(&lines, cx));
        let tool_call_count = entries
            .iter()
            .filter(|e| matches!(e, acp_thread::AgentThreadEntry::ToolCall(_)))
            .count();
        assert_eq!(tool_call_count, 1);
        let acp_thread::AgentThreadEntry::ToolCall(tc) = &entries[0] else {
            panic!("expected ToolCall at index 0, got {:?}", entries[0]);
        };
        assert!(
            matches!(tc.status, acp_thread::ToolCallStatus::Completed),
            "paired tool_use should land Completed, got {:?}",
            tc.status,
        );
    }

    #[test]
    fn tail_jsonl_caps_oversize_last_line() -> std::io::Result<()> {
        use std::io::Write;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("huge.jsonl");
        let mut f = std::fs::File::create(&path)?;
        // 80 KiB single line — past the 64 KiB cap.
        let huge = "x".repeat(80 * 1024);
        writeln!(f, "{}", huge)?;
        let tail = tail_jsonl(&path, 0)?;
        // Cap behaviour: last_line is None when the line exceeds the cap.
        assert!(tail.last_line.is_none(), "oversize line should be dropped");
        Ok(())
    }

    #[test]
    fn tail_jsonl_resumes_from_offset_skipping_old_lines() -> std::io::Result<()> {
        use std::io::Write;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("agent.jsonl");
        let mut f = std::fs::File::create(&path)?;
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"old"}}]}}}}"#
        )?;
        let first = tail_jsonl(&path, 0)?;
        assert!(
            first
                .last_line
                .as_deref()
                .is_some_and(|s| s.contains("\"old\""))
        );
        // Resume from the offset — no new bytes, no new line.
        let second = tail_jsonl(&path, first.new_offset)?;
        assert!(second.last_line.is_none(), "no new bytes → no new line");
        assert_eq!(second.new_offset, first.new_offset);
        // Append a fresh line; resume should surface only it.
        let mut f = std::fs::OpenOptions::new().append(true).open(&path)?;
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"new"}}]}}}}"#
        )?;
        let third = tail_jsonl(&path, second.new_offset)?;
        let line = third.last_line.expect("appended line should surface");
        assert!(line.contains("\"new\""));
        assert!(
            !line.contains("\"old\""),
            "incremental tail must not re-read pre-offset bytes"
        );
        Ok(())
    }

    #[test]
    fn tail_jsonl_resets_offset_when_file_shrinks() -> std::io::Result<()> {
        use std::io::Write;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("rotated.jsonl");
        // Original large content; caller stores an offset past current EOF.
        {
            let mut f = std::fs::File::create(&path)?;
            writeln!(
                f,
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"original padded line"}}]}}}}"#
            )?;
        }
        let original = tail_jsonl(&path, 0)?;
        // File rotated: truncated then a small fresh line written.
        std::fs::File::create(&path)?;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path)?;
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"fresh"}}]}}}}"#
        )?;
        // Caller passes the stale offset that now points past the new EOF.
        // tail_jsonl must reset to 0 and surface the fresh line, not return empty.
        let after = tail_jsonl(&path, original.new_offset)?;
        let line = after
            .last_line
            .expect("post-truncation tail should re-read from start");
        assert!(line.contains("\"fresh\""));
        Ok(())
    }
}
