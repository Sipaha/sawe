use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use agent_client_protocol::schema as acp;
use anyhow::{Result, anyhow};
use chrono::Utc;
use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EventEmitter, Global, SharedString, Subscription,
    Task, TaskExt as _, WeakEntity,
};
use solutions::{Solution, SolutionId, SolutionStore, SolutionStoreEvent};
use util::ResultExt;

use crate::adapter::AdapterRegistry;
use crate::db::SolutionAgentDb;
use crate::metrics_emitter::MetricsEmitter;
use crate::model::{
    AgentServerId, SessionContextCount, SessionState, SolutionSession, SolutionSessionId,
    SolutionSessionMetadata, SubagentTab,
};
use crate::notifier;
use crate::pool::SubprocessPool;

mod connection_pool;
mod queue;
#[cfg(test)]
pub(crate) mod tests;

pub(crate) use queue::{QUEUE_HINT_LINE, TS_PREFIX_CLOSE, TS_PREFIX_OPEN};

// Fork-local managed-agent lifecycle tunables. Upstream v1.7.2's resolved
// `AgentSettings` dropped these fields (they live only in `settings_content`
// as `Option<u64>`); since this crate may not edit those crates, we pin the
// historical defaults here. Stale = a session with no recent activity is a
// candidate for tear-down; dead-linger = grace period before reaping.
const MANAGED_AGENT_STALE_TIMEOUT_SECS: u64 = 120;
const MANAGED_AGENT_DEAD_LINGER_SECS: u64 = 300;

pub struct SolutionAgentStore {
    sessions: HashMap<SolutionSessionId, Entity<SolutionSession>>,
    by_solution: HashMap<SolutionId, Vec<SolutionSessionId>>,
    pool: parking_lot::Mutex<SubprocessPool>,
    persistence: Option<Arc<SolutionAgentDb>>,
    pub(crate) adapters: Arc<AdapterRegistry>,
    /// Map of `AgentServerId -> Rc<dyn AgentServer>`. Real `agent_servers`
    /// instances live per-Project (via `Project::agent_server_store`), but
    /// `SolutionAgentStore` is global-scoped — so we keep a fork-local lookup
    /// table that production wiring will populate at app init and tests
    /// populate manually. Held in an `Rc` because `dyn AgentServer` is `!Sync`.
    server_registry: HashMap<AgentServerId, Rc<dyn agent_servers::AgentServer>>,
    /// Last-known model list per agent, shared across that agent's sessions so
    /// a fresh session (no turn yet → empty per-session list) still offers a
    /// model picker. Filled on the first live capture and by a probe at create.
    agent_models: HashMap<AgentServerId, Vec<claude_native::ModelInfo>>,
    /// Agents with an in-flight `ensure_agent_models` probe (dedupe).
    agent_models_probing: HashSet<AgentServerId>,
    /// Set by the navigator (Phase 6) so `mutate_state` can ask "is this
    /// session currently focused in the UI?" before deciding whether to
    /// fire an OS notification. Stored as `Fn(&App) -> bool` rather than
    /// `Fn(&Context<Self>) -> bool` because `Context` is parameterised on
    /// `Self`, which makes the trait object generic and unstorable. `&App`
    /// is the strict supertype the resolver actually needs.
    pub focus_resolver: Option<Arc<dyn Fn(SolutionSessionId, &gpui::App) -> bool + Send + Sync>>,
    /// In-flight debounce slots for `AcpThreadEvent::EntryUpdated` events.
    /// Tool-call arg deltas, assistant-text chunks, and status flips on an
    /// existing entry all funnel through `EntryUpdated`; without this map
    /// they would either spam MCP notifications (one per token) or — as the
    /// pre-fix behaviour did — get dropped on the floor entirely because
    /// the catch-all match arm ignored them.
    ///
    /// Each key is a `(session_id, entry_index)` pair; the value is the
    /// pending trailing-edge `SessionMessageAppended` emit task. Updates
    /// while a task is in flight replace the entry (dropping the old `Task`
    /// cancels its `timer().await`), restarting the debounce window. The
    /// `first_dirty_at` field captures when the FIRST update for this
    /// debounce window arrived so we can force-emit on a max-stale
    /// breach — a continuously-streaming entry mustn't be able to starve
    /// the trailing-edge emit indefinitely.
    entry_update_throttles: HashMap<(SolutionSessionId, usize), EntryUpdateThrottle>,
    /// One per-session background-agent watcher task — alive as long as
    /// the session has >=1 registered `background_agents`. Stored as
    /// `Task<()>` so dropping kills the watcher cleanly. Populated by
    /// `ensure_background_agent_watcher` (called from the tool-call
    /// handler in a later task of the Background Agents Strip plan).
    background_agent_watchers: HashMap<SolutionSessionId, gpui::Task<()>>,
    /// One per-session background-shell watcher task — alive as long as the
    /// session has >=1 registered `background_shells`. Stored as `Task<()>`
    /// so dropping kills the watcher cleanly. Populated by
    /// `ensure_background_shell_watcher` (called from the tool-call handler
    /// when claude announces a `Bash(run_in_background=true)` launch). Keyed
    /// by `session_id` and structurally identical to
    /// `background_agent_watchers` (separate map so the two pipelines arm /
    /// cancel independently).
    background_shell_watchers: HashMap<SolutionSessionId, gpui::Task<()>>,
    /// Forward-only scan cursor into each session's PARENT session JSONL
    /// transcript, used by `scan_parent_jsonl_for_completions` to detect
    /// `<task-notification>` completion lines on the 1 Hz tick. Lazily
    /// initialised to the file's CURRENT length the first time a session is
    /// scanned — so we only observe completions FORWARD from editor launch
    /// and never re-flip shells off historical notifications. Cleared for a
    /// session once it has no `background_shells`, so a future shell re-arms
    /// from the then-current EOF.
    parent_jsonl_scan_offsets: HashMap<SolutionSessionId, u64>,
    /// Throttler for `workspace.session_metrics_changed` notifications.
    /// Caps emit rate at ~1 per 2 seconds per session so chatty fields
    /// (`last_activity_at`, `total_tokens`, `max_tokens`) don't flood
    /// the wire on every token-usage update. Non-sequenced: missed metric
    /// notifications do NOT trigger resync on the client.
    metrics_emitter: MetricsEmitter,
    _solution_subscription: Option<Subscription>,
    /// 1 Hz healthcheck loop that drives `tick_background_agents`.
    /// Held so the timer cancels when the store is dropped.
    _bg_agents_tick: Option<Task<()>>,
}

struct EntryUpdateThrottle {
    first_dirty_at: std::time::Instant,
    /// Stored only to keep the debounce timer alive: dropping this
    /// `Task` cancels its `timer().await` (the trailing-edge emit).
    /// Read implicitly via `Drop`, never by name.
    _task: Task<()>,
}

#[derive(Debug)]
pub enum SolutionAgentStoreEvent {
    /// A new session was registered in the store. `parent_session_id`
    /// is `Some` for sub-agent sessions (F: sub-agent indication) —
    /// the sub-agents-strip event coordinator forwards this through
    /// the wire payload so remote clients can update their tree
    /// without a follow-up `get_session_children` round-trip.
    SessionCreated {
        id: SolutionSessionId,
        parent_session_id: Option<SolutionSessionId>,
    },
    SessionClosed(SolutionSessionId),
    /// The set of sessions whose `tab_order IS NOT NULL` changed for
    /// `solution_id`. Emitted by `persist_tab_order` so that local UI
    /// consumers (notably `ConsolePanel`) can reactively add/remove the
    /// actual tabs in response to mutations driven from outside the
    /// panel — most importantly the wire-side
    /// `workspace.{open,close}_session` RPCs from the mobile client,
    /// which previously updated `tab_order` + the wire notification but
    /// left the desktop tab strip stale.
    ///
    /// `opened` and `closed` carry the diff against the pre-mutation
    /// set; both lists can be empty when `persist_tab_order` was called
    /// for a reorder-only change (same set, different order).
    TabsChanged {
        solution_id: SolutionId,
        opened: Vec<SolutionSessionId>,
        closed: Vec<SolutionSessionId>,
    },
    SessionStateChanged(SolutionSessionId),
    SessionTitleChanged(SolutionSessionId),
    /// Carries the entry index that was appended / updated so external
    /// MCP consumers (the WS proxy + Android client) can render the new
    /// entry without a follow-up `get_session` round-trip. The index is
    /// captured at emit time from the live `AcpThread.entries().len()
    /// - 1`, so a tight burst of appends can race — the consumer should
    /// treat the index as a hint and re-fetch the full session if the
    /// numbers don't line up.
    SessionMessageAppended(SolutionSessionId, usize),
    /// `pending_messages` on the session changed (push, drain, clear,
    /// or merge into back-of-queue). External MCP consumers use this
    /// to render server-side queued bundles as Queued bubbles in real
    /// time on every paired client — without it a desktop-typed
    /// follow-up while the agent is mid-turn stays invisible on the
    /// mobile until the eventual flush, and vice-versa.
    SessionQueueChanged(SolutionSessionId),
    SessionNotified(SolutionSessionId, notifier::NotifyKind),
    /// The session's [`SolutionSession::active_subagents`] map (and its
    /// parallel `active_subagent_order` vector) changed: a `Task` / `Agent`
    /// subagent was either spawned (parent ToolCall flipped to `InProgress`)
    /// or finished (parent ToolCall flipped to a terminal status). Emitted
    /// only when the map *actually* changed — a duplicate spawn event for a
    /// known id, or a terminal status on an unknown id, is silently
    /// ignored to keep the event stream debounce-friendly.
    ///
    /// Subscribers: the session_view's subagent-tabs strip (Etap 4) and the
    /// MCP wire's `session_active_subagents_changed` notification (Etap 5),
    /// so both desktop and mobile redraw without polling the session entity.
    SessionSubagentsChanged(SolutionSessionId),
    /// `SolutionSession::background_agents` changed — registration, snapshot
    /// update, dead-detection, or removal. Same debounce semantics as
    /// `SessionSubagentsChanged`: emitted only when the map actually changed.
    SessionBackgroundAgentsChanged(SolutionSessionId),
    /// `SolutionSession::background_shells` changed — registration, live-tail
    /// snapshot update, or terminal-state transition. Same debounce semantics
    /// as `SessionBackgroundAgentsChanged`: emitted only when the map actually
    /// changed (a re-tail of an unchanged `.output` file does NOT re-emit).
    SessionBackgroundShellsChanged(SolutionSessionId),
    /// Emitted when a session's conversation context has just been wiped
    /// in-place by `/clear` (`reset_context`) or `/compact`
    /// (`rotate_context`). Remote clients use this to drop their cached
    /// entry list for the session and re-fetch from scratch (the
    /// `session_id` is stable across the swap — only the transcript is
    /// gone). `context_count` is the post-operation value (incremented
    /// by `rotate_context`, left as-is by `reset_context`).
    SessionContextReset {
        id: SolutionSessionId,
        context_count: SessionContextCount,
    },
}

impl EventEmitter<SolutionAgentStoreEvent> for SolutionAgentStore {}

/// Which "view" of a session the user has selected — Main = parent
/// thread only, Task(id) = an in-flight inline Task subagent's
/// filtered slice, Background(id) = a Managed Agent's standalone
/// JSONL transcript. Replaces the older `Option<SharedString>` shape
/// where `None`=Main and `Some(id)` was ambiguously a Task id;
/// adding Background made an explicit sum-type necessary.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum SubagentView {
    #[default]
    Main,
    Task(SharedString),
    Background(crate::background_agent::BackgroundAgentId),
    /// A background shell's (`Bash(run_in_background=true)`) live-tailed
    /// output view. Like `Background`, it sources from disk rather than the
    /// parent thread, so it is neither a parent-thread view nor a
    /// parent-entry match. Drill-in body / pill rendering land in Tasks
    /// 12/13.
    Shell(crate::background_shell::BackgroundShellId),
}

impl SubagentView {
    /// True when the view sources its entries from the parent
    /// `AcpThread.entries` (Main + Task filter both do); false when
    /// the view sources from disk rather than the parent thread
    /// (`Background` tails a managed-agent JSONL; `Shell` tails a
    /// background-shell `.output` snapshot).
    pub fn is_parent_thread_view(&self) -> bool {
        matches!(self, Self::Main | Self::Task(_))
    }

    /// The follow-up [`crate::model::QueueTarget`] for a message typed on
    /// this tab. Only `Background` (an Agent Teams teammate) routes to a
    /// subagent — `Task` (an inline filtered slice of the parent thread)
    /// and `Shell` (a background shell) are not messageable, so they fall
    /// back to `Main` like the parent view does.
    pub fn queue_target(&self) -> crate::model::QueueTarget {
        match self {
            Self::Background(id) => {
                crate::model::QueueTarget::Subagent(SharedString::from(id.as_str().to_string()))
            }
            Self::Main | Self::Task(_) | Self::Shell(_) => crate::model::QueueTarget::Main,
        }
    }

    /// Predicate for parent-thread entry filtering. `Main` matches
    /// only entries with no `subagent_id`; `Task(id)` matches only
    /// entries stamped with exactly that id; `Background` matches
    /// nothing (it doesn't draw from parent entries).
    pub fn matches_parent_entry(&self, entry_subagent: Option<&SharedString>) -> bool {
        match (self, entry_subagent) {
            (Self::Main, None) => true,
            (Self::Task(sel), Some(eid)) => sel == eid,
            _ => false,
        }
    }
}

/// Compute the canonical subagents-dir path for a session. Mirrors
/// Anthropic's `~/.claude/projects/<encoded-cwd>/<session-id>/subagents/`
/// layout. `encoded-cwd` is "every char in `cwd.to_string_lossy()`
/// with `/` and `.` replaced by `-`". Returns `None` when `cwd` is
/// empty (legacy session) — those can't host managed agents anyway.
/// Case-insensitive match for the claude `Agent` tool name. Lives next
/// to `background_agent_dir_for` because both feed the managed-agent
/// registration path; keeping them adjacent makes the wiring obvious.
fn tool_name_is_agent(name: Option<&str>) -> bool {
    matches!(name, Some(n) if n.eq_ignore_ascii_case("agent"))
}

/// Don't GC session archives until a solution has more than this many sessions
/// (closed ones included) — small workspaces keep their full history on disk.
const ARCHIVE_REAP_MIN_SESSIONS: usize = 10;
/// A session's `.agents/<sid>/` archive is eligible for GC once its last
/// activity is older than this.
const ARCHIVE_REAP_MAX_AGE_DAYS: i64 = 30;

/// Pure half of [`SolutionAgentStore::reap_stale_session_archives`]: given a
/// solution `root` and the metadata for ALL its sessions (closed included),
/// return the `.agents/<sid>/` dirs eligible for reaping. Empty unless the
/// session count exceeds [`ARCHIVE_REAP_MIN_SESSIONS`]; then it's every session
/// whose `last_activity_at` predates the [`ARCHIVE_REAP_MAX_AGE_DAYS`] cutoff.
fn stale_archive_dirs(
    root: &std::path::Path,
    metas: &[SolutionSessionMetadata],
    now: chrono::DateTime<Utc>,
) -> Vec<PathBuf> {
    if metas.len() <= ARCHIVE_REAP_MIN_SESSIONS {
        return Vec::new();
    }
    let cutoff = now - chrono::Duration::days(ARCHIVE_REAP_MAX_AGE_DAYS);
    metas
        .iter()
        .filter(|m| m.last_activity_at < cutoff)
        .map(|m| root.join(".agents").join(m.id.to_string()))
        .collect()
}

/// `~/.claude/projects/<encoded-cwd>/` — the per-project root claude
/// writes session transcripts and subagent dirs under. `None` when `cwd`
/// is empty (legacy session) or `home_dir()` can't be resolved.
fn claude_project_dir_for(cwd: &std::path::Path) -> Option<PathBuf> {
    if cwd.as_os_str().is_empty() {
        return None;
    }
    let raw = cwd.to_string_lossy();
    let mut encoded = String::with_capacity(raw.len() + 1);
    for c in raw.chars() {
        match c {
            '/' | '.' => encoded.push('-'),
            other => encoded.push(other),
        }
    }
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("projects")
            .join(encoded),
    )
}

fn background_agent_dir_for(cwd: &std::path::Path, acp_session_id: &str) -> Option<PathBuf> {
    Some(
        claude_project_dir_for(cwd)?
            .join(acp_session_id)
            .join("subagents"),
    )
}

/// The PARENT session's on-disk JSONL transcript:
/// `~/.claude/projects/<encoded-cwd>/<acp_session_id>.jsonl`. claude
/// appends every parent-thread message (including the `<task-notification>`
/// user message a background shell emits on completion) to this file. Uses
/// the same cwd encoding as [`background_agent_dir_for`]. `None` under the
/// same conditions (empty cwd / unresolvable home).
fn parent_session_jsonl_for(cwd: &std::path::Path, acp_session_id: &str) -> Option<PathBuf> {
    Some(claude_project_dir_for(cwd)?.join(format!("{acp_session_id}.jsonl")))
}

/// Defensive per-tick read cap for the parent-JSONL scan. A single JSONL
/// message line is small; this only bounds a pathological burst.
const PARENT_JSONL_READ_CAP: u64 = 1024 * 1024;

/// Read `[offset, end)` of `path` and split it into COMPLETE lines (those
/// terminated by `\n`). Returns the complete lines plus the byte count
/// consumed (the offset of the byte just past the last `\n`), so a trailing
/// partial line is left unconsumed for the next tick. Returns `None` on any
/// IO error. The read is capped at [`PARENT_JSONL_READ_CAP`] bytes per call.
fn read_complete_lines_from(
    path: &std::path::Path,
    offset: u64,
    end: u64,
) -> Option<(Vec<String>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let to_read = std::cmp::min(end.saturating_sub(offset), PARENT_JSONL_READ_CAP);
    if to_read == 0 {
        return Some((Vec::new(), 0));
    }
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::with_capacity(to_read as usize);
    file.take(to_read).read_to_end(&mut buf).ok()?;
    // Consume up to and including the last newline; bytes after it are a
    // partial line we re-read next tick.
    let last_newline = buf.iter().rposition(|b| *b == b'\n');
    let Some(last_newline) = last_newline else {
        // No newline in the window. Two cases:
        //   - We filled the whole cap → a single line longer than the cap
        //     (e.g. a large inline `Read` result in the transcript). Pinning
        //     the offset here would WEDGE the scan forever (consumed=0 every
        //     tick), silently killing live completion detection for the
        //     session. Skip the oversized region by advancing past the cap;
        //     we may land mid-line but resync at the next newline (a fragment
        //     can't false-match the `<task-notification>` literal).
        //   - We read short of the cap → just a partial trailing line still
        //     being written. Wait (consume 0) for the newline to arrive.
        let consumed = if to_read == PARENT_JSONL_READ_CAP {
            to_read
        } else {
            0
        };
        return Some((Vec::new(), consumed));
    };
    let consumed = (last_newline + 1) as u64;
    let complete = &buf[..=last_newline];
    let lines = String::from_utf8_lossy(complete)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    Some((lines, consumed))
}

/// Pure scan: for each raw JSONL line, if it carries a `<task-notification>`
/// completion block whose `<task-id>` matches a tracked shell, emit the
/// `(id, terminal-state)` pair. The `<...>` tags and `(exit code N)` suffix
/// appear LITERALLY in the JSON string value (only newlines inside are
/// `\n`-escaped), so the existing regex-based `parse_task_notification`
/// matches the raw line directly — no JSON parse / unescape needed.
fn scan_lines_for_completions(
    lines: &[String],
    background_shells: &std::collections::HashMap<
        crate::background_shell::BackgroundShellId,
        crate::background_shell::BackgroundShell,
    >,
) -> Vec<(
    crate::background_shell::BackgroundShellId,
    crate::background_shell::ShellRuntimeState,
)> {
    let mut out = Vec::new();
    for line in lines {
        if !line.contains("<task-notification>") {
            continue;
        }
        if let Some(tn) = crate::background_shell::parse_task_notification(line) {
            if background_shells.contains_key(&tn.id) {
                out.push((tn.id, tn.status));
            }
        }
    }
    out
}

/// Last 4 chars of a `toolu_xxx` id, used as the short-id suffix in
/// fallback subagent tab labels (`general-purpose#a1b2`, `Agent #a1b2`).
/// Lower bound guarded: an id shorter than 4 chars (defensive — claude's
/// real ids are 24+ chars) falls back to the whole id rather than
/// panicking on the slice bound.
fn short_id_suffix(id: &str) -> &str {
    let len = id.len();
    if len <= 4 { id } else { &id[len - 4..] }
}

/// Subagent-tab label fallback chain when the parent ToolCall's
/// `raw_input["description"]` is missing.
///
///   1. `<subagent_type>#<short-id>` — e.g. `general-purpose#a1b2`. Used
///      when claude's `Task` SDK populated `subagent_type` but the agent
///      author didn't bother with a description.
///   2. `Agent <short-id>` — last-resort label, should only hit in
///      adversarial / malformed inputs since claude always ships at
///      least `subagent_type` for a real `Task` call.
fn label_fallback(id: &SharedString, subagent_type: Option<&str>) -> SharedString {
    let short = short_id_suffix(id.as_ref());
    match subagent_type {
        Some(stype) if !stype.is_empty() => SharedString::from(format!("{stype}#{short}")),
        _ => SharedString::from(format!("Agent {short}")),
    }
}

struct GlobalSolutionAgentStore(Entity<SolutionAgentStore>);
impl Global for GlobalSolutionAgentStore {}

/// Decode a persisted blob into `(cold_entries, entry_created_ms)`. Shared
/// by `restore_open_tabs` (editor startup) and `resume_session`'s
/// fresh-entity branch (close→reopen within the same editor session) —
/// without this in the latter, the visible conversation goes empty on
/// reopen because `claude --resume` does not re-emit the transcript
/// through stream-json and the blob is the only source of the prior
/// dialog. Prefers the structured v2 payload; legacy v1 / pre-v1 blobs
/// degrade to a single Assistant-shaped entry per row containing the
/// flat markdown summary (no per-role bubbles for archived sessions,
/// but the text shows up — not worth a migration round-trip).
pub(crate) fn cold_entries_from_persisted(
    persisted: Option<PersistedSession>,
    cx: &mut gpui::App,
) -> (Vec<acp_thread::AgentThreadEntry>, Vec<i64>) {
    let Some(persisted) = persisted else {
        return (Vec::new(), Vec::new());
    };
    // `entry_created_ms` is index-aligned with `entries_v2`; the v2 path
    // below maps every element 1:1 into `cold_entries`, so the restored
    // vectors stay aligned. Legacy blobs carry an empty timestamps vec.
    let restored_created_ms = persisted.entry_created_ms.clone();
    let cold_entries: Vec<acp_thread::AgentThreadEntry> = if !persisted.entries_v2.is_empty() {
        persisted
            .entries_v2
            .into_iter()
            .map(|p| crate::cold_persistence::from_persisted(p, cx))
            .collect()
    } else {
        let legacy_sources: Vec<String> = if !persisted.entry_summaries.is_empty() {
            persisted.entry_summaries
        } else {
            persisted.entries.into_iter().map(|e| e.markdown).collect()
        };
        legacy_sources
            .into_iter()
            .map(|md| {
                crate::cold_persistence::from_persisted(
                    crate::cold_persistence::PersistedEntryV2::Assistant(
                        crate::cold_persistence::PersistedAssistantMessage {
                            chunks: vec![
                                crate::cold_persistence::PersistedAssistantChunk::Message(md),
                            ],
                        },
                    ),
                    cx,
                )
            })
            .collect()
    };
    (cold_entries, restored_created_ms)
}

/// Decode per-entry DB rows (Phase 4 `solution_session_entries`) into the
/// store's `SessionEntry` shape. Rows arrive `ORDER BY idx`; each `payload`
/// is the JSON-encoded `SessionEntryKind` and the meta (`mod_seq`,
/// `created_ms`, `subagent_id`) comes straight from columns. A row whose
/// payload fails to decode is SKIPPED with a `log::warn` — a single corrupt
/// row must never blank the whole transcript.
pub(crate) fn entries_from_rows(
    rows: Vec<crate::db::EntryRow>,
) -> Vec<crate::session_entry::SessionEntry> {
    rows.into_iter()
        .filter_map(
            |r| match crate::session_entry::kind_from_payload(&r.payload) {
                Ok(kind) => Some(crate::session_entry::SessionEntry {
                    created_ms: r.created_ms,
                    mod_seq: r.mod_seq as u64,
                    subagent_id: r.subagent_id.map(SharedString::from),
                    kind,
                }),
                Err(e) => {
                    log::warn!(
                        target: "solution_agent::store",
                        "skipping undecodable entry row idx={}: {e}",
                        r.idx
                    );
                    None
                }
            },
        )
        .collect()
}

/// On-disk snapshot of a session. Persisted as a JSON blob in the
/// `acp_thread_blob` column so MCP / future archive UIs can rehydrate
/// the conversation transcript even after the session was closed.
///
/// Public so downstream tools (`solution_agent.read_session_history`)
/// can deserialize the same blob the store wrote.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct PersistedSession {
    pub title: String,
    /// Legacy v1 per-entry record (role + flat markdown summary). Kept
    /// for blobs written by builds before `entries_v2` landed — those
    /// are rendered through the simplified Archived path. New blobs
    /// populate `entries_v2` and leave this empty (`#[serde(default)]`
    /// on read accepts both shapes).
    #[serde(default)]
    pub entries: Vec<PersistedEntry>,
    /// Legacy flat markdown summaries — one string per thread entry.
    /// Kept populated alongside `entries` for backwards compat with the
    /// `solution_agent.read_session_history` MCP tool, which slices
    /// this list directly.
    pub entry_summaries: Vec<String>,
    /// Structured per-entry payload used to reconstruct the live
    /// conversation visually 1:1 after an editor restart. Each variant
    /// captures everything the render path reads (markdown sources,
    /// raw chunks for image previews, tool-call statuses + per-content
    /// markdown, plan entries, …). In-flight tool calls (`Pending` /
    /// `WaitingForConfirmation` / `InProgress`) are dropped at save
    /// time — see [`crate::cold_persistence::to_persisted`].
    #[serde(default)]
    pub entries_v2: Vec<crate::cold_persistence::PersistedEntryV2>,
    /// Unix-millis creation time per persisted entry, index-aligned with
    /// `entries_v2` (built with the same drop-in-flight-tool-calls filter).
    /// `#[serde(default)]` → blobs written before this feature decode to an
    /// empty vec, which the loader treats as "no captured times".
    #[serde(default)]
    pub entry_created_ms: Vec<i64>,
    /// Models advertised for this session (`ModelInfo`). `#[serde(default)]`
    /// → blobs written before this feature decode to an empty vec.
    #[serde(default)]
    pub available_models: Vec<claude_native::ModelInfo>,
    /// The session's chosen model (SDK `value`). `#[serde(default)]`.
    #[serde(default)]
    pub desired_model: Option<String>,
    /// The session's chosen effort level. `#[serde(default)]` → blobs written
    /// before this feature decode to `None` (claude's default).
    #[serde(default)]
    pub desired_effort: Option<String>,
}

pub use crate::model::{PersistedEntry, PersistedRole};
pub(crate) use queue::summarize_blocks_for_log;

/// First user prompt, normalised to a single line and truncated, for the
/// History popover label. Returns `None` if the thread has no user message
/// yet — caller's COALESCE keeps the previously-stored preview in that case.
fn extract_preview(entries: &[acp_thread::AgentThreadEntry]) -> Option<gpui::SharedString> {
    let first_user = entries.iter().find_map(|entry| match entry {
        acp_thread::AgentThreadEntry::UserMessage(msg) => Some(msg),
        _ => None,
    })?;
    // `chunks` is the raw ACP payload from the agent and contains the user's
    // typed text verbatim; `content` is the same data wrapped in a render-
    // ready `Markdown` entity that requires `&App` to read. We don't have
    // `cx` here (called from event-handler contexts that already hold a
    // mutable borrow of the store), so we walk chunks instead.
    let mut text = String::new();
    for chunk in &first_user.chunks {
        let chunk_text = match chunk {
            acp::ContentBlock::Text(t) => t.text.as_str(),
            _ => continue,
        };
        if !text.is_empty() && !text.ends_with(' ') {
            text.push(' ');
        }
        text.push_str(chunk_text);
        if text.len() >= 200 {
            break;
        }
    }
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let truncated = if collapsed.chars().count() > 80 {
        let mut s: String = collapsed.chars().take(77).collect();
        s.push('…');
        s
    } else {
        collapsed
    };
    Some(gpui::SharedString::from(truncated))
}

/// Placeholder title for a brand-new session, before claude-acp emits a
/// `TitleUpdated` describing the actual conversation. Keeps the tab
/// readable: 5 hex chars of the UUID is enough to disambiguate adjacent
/// tabs without smearing the entire UUID across the strip.
#[allow(dead_code)]
fn short_session_title(session_id: SolutionSessionId) -> SharedString {
    // SolutionSessionId is already 8 chars — no trimming needed; the
    // raw form is short enough to read at a glance and uniquely
    // identifies the session in `.agents/<id>/` paths.
    SharedString::from(session_id.to_string())
}

/// Returns `true` when the formatted error string from a `load_session` /
/// `resume_session` attempt indicates "the ACP server doesn't know about
/// this session id at this cwd" — as opposed to an auth/transport/allow-list
/// failure where retrying with a different cwd is pointless.
///
/// Match list is empirical because the wire shape of these errors isn't
/// part of the ACP contract:
///   - `Resource not found` / `-32002`: the canonical JSON-RPC code the
///     spec recommends for missing resources.
///   - `No conversation found`: claude-code-acp throws a plain `Error(...)`,
///     which marshals to `code: -32603 (Internal error)` with this text in
///     the message. Pre-fix, this string fell through the predicate and
///     `resume_session` broke out of the cwd-attempts loop on the first
///     failure, hiding the existing `solution.root` fallback (and the
///     `new_session` re-mint fallback below) and surfacing a raw
///     "No conversation found with session ID: …" snackbar on the user's
///     editor restart.
fn is_session_gone_error(err_str: &str) -> bool {
    err_str.contains("Resource not found")
        || err_str.contains("-32002")
        || err_str.contains("No conversation found")
}

/// Resolve the catalog project name for `cwd` if `cwd` matches one of
/// `solution.members`'s `local_path`s. Returns `None` for `solution.root`
/// (the "Solution root" choice in the New Session popover) and for any
/// path that doesn't map to a registered member — caller decides how to
/// label those (status row says "ROOT", title default uses
/// `solution.name`).
pub(crate) fn project_name_for_cwd(
    solution: &Solution,
    cwd: &std::path::Path,
    cx: &App,
) -> Option<SharedString> {
    if cwd.as_os_str().is_empty() || cwd == solution.root {
        return None;
    }
    let member = solution.members.iter().find(|m| m.local_path == cwd)?;
    let store = SolutionStore::try_global(cx)?;
    store.read_with(cx, |s, _| {
        s.catalog()
            .iter()
            .find(|c| c.id == member.catalog_id)
            .map(|c| SharedString::from(c.name.clone()))
    })
}

/// Pick a tab title that doesn't collide with any existing session in
/// the same Solution. First call returns `base`; subsequent collisions
/// get ` 2`, ` 3`, … appended (matching the "Untitled 2 / 3" convention
/// the rest of the editor uses for duplicate names). Caps at 1000 just
/// to avoid an infinite loop on a pathological state — practically
/// nobody opens 1000 sessions of the same project in one Solution.
fn unique_session_title(
    base: &str,
    store: &SolutionAgentStore,
    solution_id: &SolutionId,
    cx: &App,
) -> SharedString {
    let existing: std::collections::HashSet<String> = store
        .by_solution
        .get(solution_id)
        .into_iter()
        .flatten()
        .filter_map(|sid| store.sessions.get(sid))
        .map(|s| s.read(cx).title.to_string())
        .collect();
    if !existing.contains(base) {
        return SharedString::from(base.to_string());
    }
    for n in 2..1000 {
        let candidate = format!("{base} {n}");
        if !existing.contains(&candidate) {
            return SharedString::from(candidate);
        }
    }
    SharedString::from(base.to_string())
}

/// The fixed effort options offered in the UI (no per-agent list — these are
/// Claude Code's effort levels; `ultracode` = "xhigh + workflows").
pub const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultracode"];

impl SolutionAgentStore {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalSolutionAgentStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalSolutionAgentStore>()
            .map(|g| g.0.clone())
    }

    pub fn init_global(cx: &mut App, adapters: Arc<AdapterRegistry>) {
        let entity = cx.new(|cx| Self::new_in_app(adapters, cx));
        cx.set_global(GlobalSolutionAgentStore(entity));
    }

    fn new_in_app(adapters: Arc<AdapterRegistry>, cx: &mut Context<Self>) -> Self {
        // SolutionStore subscription is opt-in here: in tests SolutionStore
        // may not be initialised, so we tolerate its absence by checking
        // `try_global` (the public sentinel for "is solutions::init done?").
        let solution_subscription = SolutionStore::try_global(cx)
            .map(|store| cx.subscribe(&store, Self::on_solution_event));
        // 1 Hz background-agent healthcheck. Drops done agents and prunes
        // long-dead ones; rendering-side "dead" detection (orange pill) uses
        // `agent.managed_agent_stale_timeout_secs` directly off the snapshot mtime, so
        // the tick is only responsible for eventual cleanup, not the
        // first-observation transition.
        let bg_agents_tick = cx.spawn(async move |this, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.tick_background_agents(cx);
                        this.scan_parent_jsonls_for_completions(cx);
                        this.tick_background_shells(cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            sessions: HashMap::new(),
            by_solution: HashMap::new(),
            pool: parking_lot::Mutex::new(SubprocessPool::new()),
            persistence: None,
            adapters,
            server_registry: HashMap::new(),
            agent_models: HashMap::new(),
            agent_models_probing: HashSet::new(),
            focus_resolver: None,
            entry_update_throttles: HashMap::new(),
            background_agent_watchers: HashMap::new(),
            background_shell_watchers: HashMap::new(),
            parent_jsonl_scan_offsets: HashMap::new(),
            metrics_emitter: MetricsEmitter::new(),
            _solution_subscription: solution_subscription,
            _bg_agents_tick: Some(bg_agents_tick),
        }
    }

    /// Register an `AgentServer` instance under the given id so that
    /// `create_session` can look it up. Production wiring registers
    /// `CustomAgentServer::new(...)` for each known agent at app init;
    /// tests register a `MockAgentServer`.
    pub fn register_agent_server(
        &mut self,
        agent_id: AgentServerId,
        server: Rc<dyn agent_servers::AgentServer>,
    ) {
        self.server_registry.insert(agent_id, server);
    }

    pub fn registered_agent_server(
        &self,
        agent_id: &AgentServerId,
    ) -> Option<Rc<dyn agent_servers::AgentServer>> {
        self.server_registry.get(agent_id).cloned()
    }

    pub fn set_persistence(&mut self, db: Arc<SolutionAgentDb>) {
        self.persistence = Some(db);
    }

    /// Returns the database handle if set. Used by the navigator to list
    /// historic sessions (those persisted across editor restarts) for the
    /// "Resume" / "Continue last session" affordances.
    pub fn db(&self) -> Option<Arc<SolutionAgentDb>> {
        self.persistence.clone()
    }

    /// Create a new ACP session for `(solution_id, agent_id)`, multiplexed
    /// onto a shared subprocess via the pool. The caller passes the `project`
    /// to use for the session: production callers pass the active workspace's
    /// `Entity<Project>`; tests pass a `Project::test`-built entity.
    ///
    /// Synthetic single-worktree projects per session were considered (see
    /// `pool::make_production_project_for_solution`) but defer to a follow-up
    /// — the AgentServer's `connect()` path is tightly coupled to a
    /// per-Project `AgentServerStore`, so re-using the workspace project is
    /// the diff-minimal choice today.
    pub fn create_session(
        &mut self,
        solution_id: SolutionId,
        agent_id: AgentServerId,
        project: Entity<project::Project>,
        cx: &mut Context<Self>,
    ) -> Task<Result<SolutionSessionId>> {
        self.create_session_with_cwd(solution_id, agent_id, project, None, None, None, cx)
    }

    /// Same as `create_session`, but lets the caller pin the session's
    /// working directory to a specific path inside the solution (e.g.
    /// a member project root) instead of defaulting to `solution.root`.
    /// Pass `None` for the default behavior. `model` (when set) is the
    /// model the new session should start on — it's threaded into the
    /// `NewSessionRequest` `_meta` so `claude` launches with `--model` and
    /// is persisted as the session's `desired_model`.
    pub fn create_session_with_cwd(
        &mut self,
        solution_id: SolutionId,
        agent_id: AgentServerId,
        project: Entity<project::Project>,
        cwd: Option<PathBuf>,
        model: Option<String>,
        effort: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<SolutionSessionId>> {
        self.create_session_with_parent(
            solution_id,
            agent_id,
            project,
            cwd,
            None,
            model,
            effort,
            cx,
        )
    }

    /// Full variant. `parent_session_id` (F: sub-agent indication) marks
    /// the new session as a child of `parent_session_id` so the session-
    /// view's sub-agents strip renders it under its parent. The parent
    /// MUST already exist in the same solution — the caller is
    /// responsible for that validation; the in-process store accepts
    /// any value here and only writes it through.
    pub fn create_session_with_parent(
        &mut self,
        solution_id: SolutionId,
        agent_id: AgentServerId,
        project: Entity<project::Project>,
        cwd: Option<PathBuf>,
        parent_session_id: Option<SolutionSessionId>,
        model: Option<String>,
        effort: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<SolutionSessionId>> {
        let pair = (solution_id.clone(), agent_id.clone());

        cx.spawn(async move |this, cx: &mut AsyncApp| {
            // 1. Resolve the solution. Cloned out so we don't hold the store
            //    borrow across the connection await.
            let solution = cx.update(|cx| {
                SolutionStore::try_global(cx)
                    .ok_or_else(|| anyhow!("SolutionStore global is not initialised"))
                    .and_then(|store| {
                        store
                            .read(cx)
                            .solutions()
                            .iter()
                            .find(|s| s.id == solution_id)
                            .cloned()
                            .ok_or_else(|| anyhow!("solution {:?} not found", solution_id))
                    })
            })?;

            // 2. Get-or-spawn the pooled connection for (solution, agent).
            //    Build the session-prompt `_meta` here too: needs the live
            //    `adapters` registry on the store, and we already have the
            //    store borrow open.
            let (connection_task, acp_meta) = this.update(cx, |store, cx| {
                let task =
                    store.get_or_spawn_connection(pair.clone(), &solution, project.clone(), cx);
                // Brand-new session — no entity exists yet, so there is no
                // persisted `desired_model` to thread in. An explicit `model`
                // chosen in the new-chat row is passed as the override so
                // `claude` launches on it immediately.
                let meta = store.build_session_meta(&pair.1, &solution, None, model.clone(), cx);
                (task, meta)
            })?;
            let connection = connection_task.await?;

            // 3. Create an ACP session on that connection.
            let work_dir = cwd.unwrap_or_else(|| solution.root.clone());
            log::info!(
                target: "solution_agent::resume",
                "creating session in solution={:?} agent={} cwd={} (solution_root={})",
                solution_id,
                agent_id,
                work_dir.to_string_lossy(),
                solution.root.to_string_lossy(),
            );
            let work_dirs =
                util::path_list::PathList::new(&[work_dir.to_string_lossy().into_owned()]);
            let session_cwd = work_dir.clone();
            let acp_thread_task = cx.update(|cx| {
                connection
                    .clone()
                    .new_session_with_meta(project.clone(), work_dirs, acp_meta, cx)
            });
            let acp_thread = match acp_thread_task.await {
                Ok(thread) => thread,
                Err(err) => {
                    // Spawn succeeded but new_session failed — release our
                    // refcount on the pooled connection so it can debounce-
                    // close if no other sessions are active.
                    this.update(cx, |store, cx| {
                        store.pool_release_session(pair.clone(), cx);
                    })
                    .ok();
                    return Err(err);
                }
            };

            // 4. Register the session and emit `SessionCreated`.
            let session_id = this.update(cx, |store, cx| {
                let acp_session_id = acp_thread.read(cx).session_id().clone();
                // Apply the chosen effort to the now-live session. Unlike the
                // model (threaded through `acp_meta`'s `modelId` so claude
                // launches on it), effort has no spawn-time meta hook: the
                // create path can't seed the native respawn map before spawn.
                // So we push it to the live session directly — `set_desired_effort`
                // for any future respawn, `select_effort` (`apply_flag_settings`)
                // so the FIRST turn already uses it.
                if let Some(e) = effort.clone() {
                    if let Some(native) = acp_thread
                        .read(cx)
                        .connection()
                        .clone()
                        .downcast::<claude_native::ClaudeNativeConnection>()
                    {
                        native.set_desired_effort(&acp_session_id, Some(e.clone()));
                        native.select_effort(&acp_session_id, e);
                    }
                }
                let session_id = SolutionSessionId::new();
                // Default tab title = name of the project that's the
                // session's cwd: catalog name for a member, else the
                // Solution name (covers the "Solution root" choice).
                // Dedup'd against existing sessions in the same Solution
                // so successive same-cwd opens land as `name`, `name 2`,
                // `name 3`, …
                let title_base: SharedString = project_name_for_cwd(&solution, &session_cwd, cx)
                    .unwrap_or_else(|| SharedString::from(solution.name.clone()));
                let title = unique_session_title(&title_base, store, &solution_id, cx);
                let entity = cx.new(|cx| {
                    let mut s = SolutionSession::new_idle(
                        session_id,
                        solution_id.clone(),
                        agent_id.clone(),
                        acp_session_id,
                    );
                    s.title = title;
                    s.project = Some(project.clone());
                    s.cwd = session_cwd.clone();
                    s.parent_session_id = parent_session_id;
                    // Persist the model the session was created on so it shows
                    // as selected in the status row and survives a cold reload.
                    s.desired_model = model.clone();
                    // Same for the effort level chosen in the new-chat row.
                    s.desired_effort = effort.clone();
                    s.set_acp_thread(Some(acp_thread.clone()), cx);
                    s
                });
                store.sessions.insert(session_id, entity);
                let by_sol = store.by_solution.entry(solution_id.clone()).or_default();
                if !by_sol.contains(&session_id) {
                    by_sol.push(session_id);
                }
                let sub = store.subscribe_to_session(session_id, acp_thread, cx);
                store
                    .sessions
                    .get(&session_id)
                    .ok_or_else(|| anyhow!("session vanished after insert"))?
                    .update(cx, |s, _| s._acp_subscription = Some(sub));
                store.persist_session_row(session_id, cx);
                // Best-effort pre-turn probe so the new session's status-row /
                // new-chat picker has a model list before its first turn lands
                // the live `initialize` response. Deduped per agent, so this is
                // a no-op once any session of this agent has captured a list.
                store.ensure_agent_models(solution_id.clone(), agent_id.clone(), cx);
                cx.emit(SolutionAgentStoreEvent::SessionCreated {
                    id: session_id,
                    parent_session_id,
                });
                cx.notify();
                anyhow::Ok(session_id)
            })??;

            // Create implies open. Pin top-level sessions into their
            // solution's tab strip (the open-set) so they surface on BOTH
            // the desktop ConsolePanel and the mobile workspace mirror —
            // both render the `tab_order` open-set, so a session left
            // unpinned (tab_order NULL) is invisible everywhere despite
            // living on disk. This is the single definition of "a new
            // session is opened": every create path (desktop ChatProvider,
            // the wire `solution_agent.create_session` tool, restart_agent)
            // funnels here, so none of them can diverge into create-without-
            // open again. Sub-agents (parent_session_id set) live in the
            // subagent strip rather than as top-level tabs, so they are
            // intentionally NOT pinned.
            if parent_session_id.is_none() {
                this.update(cx, |store, cx| {
                    store.open_session_in_strip(session_id, cx);
                })?;
            }

            Ok(session_id)
        })
    }

    /// Build the `_meta` payload for a `NewSessionRequest` so the agent
    /// receives the solution-context system prompt. Wraps the adapter's
    /// `build_initial_system_prompt` output in the shape claude-agent-acp
    /// expects: `{ "systemPrompt": { "append": "<prompt>" } }`. The
    /// `append` form preserves Claude's default `claude_code` preset and
    /// concatenates our text after it (string-form would replace the
    /// preset entirely — wrong for our needs since we want the standard
    /// CLI behavior plus solution awareness).
    ///
    /// Returns `None` when no adapter is registered for `agent_id` or
    /// the adapter produced an empty prompt; ACP agents that don't
    /// understand `_meta.systemPrompt` ignore unknown keys per the
    /// protocol contract, so emitting it is safe even for non-Claude
    /// adapters.
    ///
    /// Called at every fresh-session site (`create_session`,
    /// `rotate_context` for `/compact`, `reset_context` for `/clear`)
    /// so the system prompt is re-asserted whenever the underlying ACP
    /// session is recreated — that's how it survives `/clear`.
    fn build_session_meta(
        &self,
        agent_id: &AgentServerId,
        solution: &Solution,
        session_id: Option<SolutionSessionId>,
        model_override: Option<String>,
        cx: &App,
    ) -> Option<acp::Meta> {
        let mut meta = acp::Meta::new();
        if let Some(adapter) = self.adapters.get(agent_id) {
            let prompt = adapter.build_initial_system_prompt(solution);
            if !prompt.is_empty() {
                meta.insert(
                    "systemPrompt".to_string(),
                    serde_json::json!({ "append": prompt }),
                );
            }
        }
        // The native side reads the chosen model from a TOP-LEVEL `"modelId"`
        // string key (`model_from_meta` → `meta.get("modelId")?.as_str()`), so
        // a session that picked a model while cold wakes onto it. An explicit
        // `model_override` (e.g. the model chosen in the new-chat row) wins;
        // otherwise fall back to the session's persisted `desired_model`.
        let desired_model = model_override.or_else(|| {
            session_id
                .and_then(|id| self.sessions.get(&id))
                .and_then(|session| session.read(cx).desired_model.clone())
        });
        if let Some(model) = desired_model {
            meta.insert("modelId".to_string(), serde_json::Value::String(model));
        }
        if meta.is_empty() {
            return None;
        }
        Some(meta)
    }

    /// Persist the row for `session_id` to the DB so the History popover and
    /// "Continue last session" CTA pick it up across editor restarts. No-op
    /// when persistence is disabled (test contexts).
    fn persist_session_row(&self, session_id: SolutionSessionId, cx: &mut Context<Self>) {
        let Some(db) = self.persistence.clone() else {
            return;
        };
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
        let s = session.read(cx);
        // Pull the dialog preview + total token count from the live thread.
        // Both are `None` until the user sends the first prompt and the agent
        // emits a usage update, respectively. The DB write uses COALESCE so a
        // None on a follow-up insert never clobbers a previously-stored value.
        let (preview, total_tokens) = s
            .acp_thread()
            .map(|thread| {
                let thread = thread.read(cx);
                // `used_tokens` is the cumulative context usage that
                // claude-acp reports via `SessionUpdate::UsageUpdate`
                // — same number the status-row meter shows live. We
                // used to persist `input_tokens + output_tokens`,
                // which only covers the LAST turn (gated by the
                // ACP-beta response.usage path), so a 33k-token
                // session resumed as 700 tokens. Saving used_tokens
                // keeps the persisted value aligned with the meter.
                (
                    extract_preview(thread.entries()),
                    thread.token_usage().map(|u| u.used_tokens),
                )
            })
            .unwrap_or((None, None));
        let meta = SolutionSessionMetadata {
            id: session_id,
            solution_id: s.solution_id.clone(),
            agent_id: s.agent_id.clone(),
            acp_session_id: s.acp_session_id.clone(),
            title: s.title.clone(),
            created_at: s.created_at,
            last_activity_at: s.last_activity_at,
            preview,
            total_tokens,
            context_count: s.context_count,
            cwd: s.cwd.clone(),
            parent_session_id: s.parent_session_id,
            desired_model: s.desired_model.clone(),
            desired_effort: s.desired_effort.clone(),
            cached_models: s.cached_models.clone(),
        };
        db.save_metadata(meta).detach_and_log_err(cx);
    }

    /// Resume a session from its persisted metadata: spawns / reuses the
    /// pooled connection and asks the agent to attach to the saved
    /// `acp_session_id`. Falls back to `resume_session` (history-less
    /// reattach) if `load_session` (full replay) isn't supported. If the
    /// metadata is already in-memory the existing session is returned.
    ///
    /// Returns the live `SolutionSessionId`. The caller can then look up
    /// the entity via `session(id)` and open it in the navigator.
    pub fn resume_session(
        &mut self,
        meta: SolutionSessionMetadata,
        project: Entity<project::Project>,
        cx: &mut Context<Self>,
    ) -> Task<Result<SolutionSessionId>> {
        // Already hot (`acp_thread` attached)? Return the existing
        // session id directly. A cold session — registered by
        // `restore_open_tabs` with `acp_thread: None` — falls through
        // and triggers the real spawn path so the user's pending Send
        // makes it to a live agent.
        if let Some(existing) = self
            .by_solution
            .get(&meta.solution_id)
            .into_iter()
            .flatten()
            .find(|sid| {
                self.sessions
                    .get(sid)
                    .map(|s| {
                        let s = s.read(cx);
                        s.acp_session_id == meta.acp_session_id && s.acp_thread().is_some()
                    })
                    .unwrap_or(false)
            })
            .cloned()
        {
            return Task::ready(Ok(existing));
        }

        let pair = (meta.solution_id.clone(), meta.agent_id.clone());

        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let solution = cx.update(|cx| {
                SolutionStore::try_global(cx)
                    .ok_or_else(|| anyhow!("SolutionStore global is not initialised"))
                    .and_then(|store| {
                        store
                            .read(cx)
                            .solutions()
                            .iter()
                            .find(|s| s.id == meta.solution_id)
                            .cloned()
                            .ok_or_else(|| anyhow!("solution {:?} not found", meta.solution_id))
                    })
            })?;

            let connection_task = this.update(cx, |store, cx| {
                store.get_or_spawn_connection(pair.clone(), &solution, project.clone(), cx)
            })?;
            let connection = connection_task.await?;

            // Empty `cwd` = legacy row written before the column existed —
            // fall back to `solution.root` (matches the pre-fix resume
            // behaviour, so already-broken sessions don't get any worse).
            let primary_cwd = if meta.cwd.as_os_str().is_empty() {
                solution.root.clone()
            } else {
                meta.cwd.clone()
            };
            let acp_session_id = meta.acp_session_id.clone();
            let title_for_load = Some(meta.title.clone());

            // Resume cwd resolution. claude code keys session JSONL files
            // by the cwd of its subprocess at session-creation time
            // (`~/.claude/projects/<sanitized cwd>/<id>.jsonl`). Since
            // `claude_native::open_session` spawns a fresh subprocess
            // PER ACP-session with `work_dir = work_dirs.first()`, the
            // JSONL lives under exactly the cwd that was passed in at
            // creation — which is what `primary_cwd` (`meta.cwd`) holds.
            //
            // Historical note: an earlier draft tried `solution.root`
            // FIRST on the theory that the connection pool unified all
            // subprocesses on solution.root. That theory was wrong — per
            // `connection.rs::open_session` each session spawns its own
            // subprocess — but the consequence was nasty: claude's
            // `--resume <id>` doesn't fail-fast when the JSONL is
            // missing. The spawn succeeds; the missing-conversation
            // error only surfaces inline on the FIRST PROMPT. So the
            // earlier attempts order would happily attach to a
            // solution-root subprocess, write `session.cwd =
            // solution.root` from the "success", and the user's first
            // turn would crash with "No conversation found" — with the
            // status row now mis-displaying ROOT.
            //
            // Always try the persisted `primary_cwd` first. Keep the
            // `solution.root` slot only as a fallback for legacy rows
            // whose `meta.cwd` was empty (treated as solution.root by
            // the `primary_cwd` initialiser above) — that branch is a
            // no-op, since the loop just runs the one candidate.
            let attempts: Vec<PathBuf> = if primary_cwd != solution.root {
                vec![primary_cwd.clone(), solution.root.clone()]
            } else {
                vec![primary_cwd.clone()]
            };
            log::info!(
                target: "solution_agent::resume",
                "session={} acp_session={} attempting resume with cwds={:?}",
                meta.id,
                acp_session_id.0,
                attempts
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            );
            // Seed the native connection's desired-model fallback before the
            // wake dispatch. `resume_session`/`load_session` thread no session
            // meta into `open_session`, so a model the user picked while this
            // session was cold would otherwise be lost — `open_session`
            // consults `desired_models` when the ACP meta has no `modelId`.
            this.update(cx, |store, cx| {
                if let Some(native) = connection
                    .clone()
                    .downcast::<claude_native::ClaudeNativeConnection>()
                {
                    let desired = store
                        .session(meta.id)
                        .and_then(|s| s.read(cx).desired_model.clone());
                    native.set_desired_model(&acp_session_id, desired);
                    let effort = store
                        .session(meta.id)
                        .and_then(|s| s.read(cx).desired_effort.clone());
                    native.set_desired_effort(&acp_session_id, effort);
                }
            })?;

            let mut last_err: Option<anyhow::Error> = None;
            let mut attached: Option<(Entity<acp_thread::AcpThread>, PathBuf)> = None;
            // `true` only while EVERY cwd candidate so far has failed
            // with `Resource not found`. A single non-RNF error
            // (transport, auth, allow-list, …) flips this to `false`
            // and disables the new-session fallback below — the
            // failure isn't a "claude-acp forgot the session" case
            // and re-creating wouldn't help.
            let mut all_resource_gone = true;
            for attempt_cwd in attempts {
                let work_dirs = util::path_list::PathList::new(&[attempt_cwd
                    .to_string_lossy()
                    .into_owned()]);
                let acp_thread_task: Task<Result<Entity<acp_thread::AcpThread>>> = cx
                    .update(|cx| {
                        if connection.supports_load_session() {
                            Ok(connection.clone().load_session(
                                acp_session_id.clone(),
                                project.clone(),
                                work_dirs.clone(),
                                title_for_load.clone(),
                                cx,
                            ))
                        } else if connection.supports_resume_session() {
                            Ok(connection.clone().resume_session(
                                acp_session_id.clone(),
                                project.clone(),
                                work_dirs.clone(),
                                title_for_load.clone(),
                                cx,
                            ))
                        } else {
                            Err(anyhow!(
                                "agent {:?} does not support loading or resuming sessions",
                                meta.agent_id,
                            ))
                        }
                    })?;
                match acp_thread_task.await {
                    Ok(thread) => {
                        attached = Some((thread, attempt_cwd));
                        break;
                    }
                    Err(err) => {
                        let err_str = format!("{err:#}");
                        let resource_gone = is_session_gone_error(&err_str);
                        if !resource_gone {
                            // Non-recoverable (auth, transport, …). Fall
                            // through with this error — fallback would
                            // just hit the same wall.
                            all_resource_gone = false;
                            last_err = Some(err);
                            break;
                        }
                        log::warn!(
                            target: "solution_agent::resume",
                            "session={} cwd={} returned session-gone error ({}); will try next candidate",
                            meta.id,
                            attempt_cwd.to_string_lossy(),
                            err_str,
                        );
                        last_err = Some(err);
                    }
                }
            }
            // If every cwd candidate returned "Resource not found" the
            // ACP session is genuinely gone (claude-acp lost its jsonl,
            // was restarted, or the agent rotated state under us) and
            // no further resume attempt against the SAME acp_session_id
            // can recover. Mint a fresh ACP session on the same
            // connection so the caller's pending prompt still lands —
            // the alternative is bouncing the user's message with an
            // unactionable "Resource not found" snackbar.
            //
            // The new ACP session has NO conversation history from
            // claude-acp's perspective. We log the transition loudly so
            // the user-visible side ("agent forgot the previous turns,
            // but my message went through") is at least traceable. The
            // SolutionSession entity below picks up the new session id
            // via `acp_thread.read(cx).session_id()`, so persistence and
            // the navigator stay aligned with claude-acp on the next
            // round-trip.
            if attached.is_none() && all_resource_gone {
                let acp_meta = this.update(cx, |store, cx| {
                    store.build_session_meta(&pair.1, &solution, Some(meta.id), None, cx)
                })?;
                let fallback_cwd = if primary_cwd != solution.root {
                    primary_cwd.clone()
                } else {
                    solution.root.clone()
                };
                let work_dirs = util::path_list::PathList::new(&[fallback_cwd
                    .to_string_lossy()
                    .into_owned()]);
                log::warn!(
                    target: "solution_agent::resume",
                    "session={} every cwd candidate returned Resource not found — \
                     claude-acp lost session {}; minting a NEW ACP session on the \
                     same connection (conversation history will appear empty to the \
                     agent on the next turn)",
                    meta.id,
                    acp_session_id.0,
                );
                let new_session_task: Task<Result<Entity<acp_thread::AcpThread>>> =
                    cx.update(|cx| {
                        connection.clone().new_session_with_meta(
                            project.clone(),
                            work_dirs,
                            acp_meta,
                            cx,
                        )
                    });
                match new_session_task.await {
                    Ok(thread) => {
                        attached = Some((thread, fallback_cwd));
                    }
                    Err(err) => {
                        log::error!(
                            target: "solution_agent::resume",
                            "session={} new_session fallback failed after exhausting \
                             resume candidates: {err:#}",
                            meta.id,
                        );
                        last_err = Some(err);
                    }
                }
            }

            let (acp_thread, applied_cwd) = match attached {
                Some(pair) => pair,
                None => {
                    this.update(cx, |store, cx| {
                        store.pool_release_session(pair.clone(), cx);
                    })
                    .ok();
                    return Err(last_err.unwrap_or_else(|| {
                        anyhow!("resume_session: no cwd candidates produced a thread")
                    }));
                }
            };
            // Reflect the cwd the agent actually accepted in the rest
            // of the resume — store update + persist below — so a
            // future resume hits this cwd first instead of replaying
            // the same primary→fallback search.
            let resume_cwd = applied_cwd;

            // Best-effort preload of the persisted transcript blob. Used
            // by the fresh-entity branch below to seed `cold_entries`
            // when the user closed the session within the current
            // editor lifetime and is now reopening it from History.
            // The hot-path (existing in-memory session) keeps its
            // already-populated `cold_entries` untouched, so a blob
            // load here is wasted work — but resume_session is a rare,
            // user-triggered action and a single sqlite read is
            // negligible compared to the agent subprocess spawn we
            // already paid for above. Errors are logged and treated as
            // "no blob": worst case the user sees an empty conversation,
            // which is exactly what was happening BEFORE this fix.
            // Phase 4: prefer per-entry rows. Load rows + epoch off the
            // foreground thread; only load+deserialize the legacy transcript
            // blob when there are no rows yet (the fresh-entity branch below
            // then lazily migrates the blob to rows).
            let (preloaded_rows, preloaded_epoch) = {
                let tasks = this.update(cx, |store, _| {
                    store
                        .persistence()
                        .map(|db| (db.load_entries(meta.id), db.load_epoch(meta.id)))
                })?;
                match tasks {
                    Some((rows_task, epoch_task)) => {
                        let rows = rows_task.await.unwrap_or_else(|err| {
                            log::warn!(
                                target: "solution_agent::resume",
                                "session={} entry-row load failed on reopen: {err}",
                                meta.id
                            );
                            Vec::new()
                        });
                        let epoch = epoch_task.await.ok().flatten().unwrap_or(0);
                        (rows, epoch)
                    }
                    None => (Vec::new(), 0),
                }
            };
            let preloaded_persisted: Option<PersistedSession> = if !preloaded_rows.is_empty() {
                None
            } else {
                let load_task = this.update(cx, |store, _| {
                    store.persistence().map(|db| db.load_blob(meta.id))
                })?;
                match load_task {
                    Some(task) => match task.await {
                        Ok(Some(bytes)) => {
                            match serde_json::from_slice::<PersistedSession>(&bytes) {
                                Ok(p) => Some(p),
                                Err(err) => {
                                    log::warn!(
                                        target: "solution_agent::resume",
                                        "session={} blob decode failed on reopen: {err}",
                                        meta.id
                                    );
                                    None
                                }
                            }
                        }
                        Ok(None) => None,
                        Err(err) => {
                            log::warn!(
                                target: "solution_agent::resume",
                                "session={} blob load failed on reopen: {err}",
                                meta.id
                            );
                            None
                        }
                    },
                    None => None,
                }
            };

            let session_id = this.update(cx, |store, cx| {
                // Reuse the metadata's existing internal id — minting a fresh
                // SolutionSessionId on every resume duplicated the row in the
                // History popover (each restart added another "Session
                // <new-uuid>" pointing at the same `acp_session_id`).
                let session_id = meta.id;
                let new_thread_session_id = acp_thread.read(cx).session_id().clone();
                if let Some(existing) = store.sessions.get(&session_id).cloned() {
                    // Cold-session path: this id was hydrated by
                    // `restore_open_tabs` with `acp_thread: None` and
                    // populated `cold_entries`. Update the existing
                    // `Entity` in place instead of replacing it — the
                    // navigator's `SolutionSessionView` already holds
                    // this handle, so a swap would leave the UI bound
                    // to a stale entity. The `cx.notify()` is what
                    // wakes the view's `cx.observe(&session)` callback
                    // — without it, `sync_thread_subscription` never
                    // attaches to the new `AcpThread` (view sees no
                    // streaming) and `flush_pending_send_if_ready`
                    // never dispatches the message the user typed
                    // while the tab was cold (Send button gets stuck
                    // because `resuming` stays `true`).
                    let had_pending = existing.update(cx, |session, cx| {
                        let had_pending = !session.pending_messages.is_empty();
                        if had_pending {
                            // Cold→live transition with queued messages
                            // shouldn't normally happen (cold sessions
                            // can't queue), but log if it ever does so
                            // we don't lose them silently.
                            let previews: Vec<String> = session
                                .pending_messages
                                .iter()
                                .map(|b| queue::summarize_blocks_for_log(&b.blocks))
                                .collect();
                            log::warn!(
                                target: "solution_agent::queue",
                                "session={session_id} dropped {} queued bundle(s) on resume_session cold→live promotion — content: [{}]",
                                session.pending_messages.len(),
                                previews.join(" | "),
                            );
                        }
                        session.acp_session_id = new_thread_session_id;
                        session.last_activity_at = Utc::now();
                        session.state = SessionState::Idle;
                        session.context_count = meta.context_count;
                        session.project = Some(project.clone());
                        session.pending_messages.clear();
                        session.flush_after_cancel = false;
                        session.cwd = resume_cwd.clone();
                        // KEEP `cold_entries`: claude --resume does NOT re-emit
                        // the transcript through stream-json, so clearing them
                        // wipes the chat history from the UI — old code assumed
                        // a replay that the native backend doesn't get. The
                        // build-entries path now concatenates cold + live.
                        // `set_acp_thread` emits ThreadReplaced + notify;
                        // it must be the last mutation so SessionView
                        // observers see a fully-populated session when
                        // they wake up to re-attach.
                        session.set_acp_thread(Some(acp_thread.clone()), cx);
                        had_pending
                    });
                    if had_pending {
                        store.mark_queue_changed(session_id, cx);
                    }
                } else {
                    // Hydrate cold prefix BEFORE attaching the live thread.
                    // claude --resume does NOT re-emit the transcript through
                    // stream-json, and `build_entries` concatenates cold + live:
                    // skipping this seeds an empty conversation visually even
                    // though the agent subprocess will happily continue from
                    // where it left off (the close→reopen empty-history bug).
                    //
                    // Phase 4: prefer the per-entry rows (no epoch bump — read
                    // the persisted generation). Fall back to the legacy blob
                    // only when there are no rows, then lazily migrate it.
                    let migrating = preloaded_rows.is_empty();
                    let entries = if !preloaded_rows.is_empty() {
                        entries_from_rows(preloaded_rows)
                    } else {
                        let (cold_entries, restored_created_ms) =
                            cold_entries_from_persisted(preloaded_persisted, cx);
                        crate::session_entry::rebuild_entries(
                            &cold_entries,
                            &[],
                            &restored_created_ms,
                            0,
                            cx,
                        )
                    };
                    let entity = cx.new(|cx| {
                        let mut s = SolutionSession::new_idle(
                            session_id,
                            meta.solution_id.clone(),
                            meta.agent_id.clone(),
                            new_thread_session_id,
                        );
                        s.title = meta.title.clone();
                        s.created_at = meta.created_at;
                        s.context_count = meta.context_count;
                        s.project = Some(project.clone());
                        // Persist the same cwd we resumed against so the
                        // next restart finds the row aligned with the
                        // agent state.
                        s.cwd = resume_cwd.clone();
                        s.cached_total_tokens = meta.total_tokens;
                        s.parent_session_id = meta.parent_session_id;
                        s.desired_model = meta.desired_model.clone();
                        s.desired_effort = meta.desired_effort.clone();
                        s.cached_models = meta.cached_models.clone();
                        s.entries = entries;
                        s.init_change_seq_from_entries();
                        if migrating {
                            s.bump_epoch();
                        } else {
                            s.epoch = preloaded_epoch as u64;
                        }
                        s.set_acp_thread(Some(acp_thread.clone()), cx);
                        s
                    });
                    store.sessions.insert(session_id, entity);
                    // Legacy → rows lazy migration (idempotent; guarded by
                    // rows-empty). Blob kept until Task 5 removes it.
                    if migrating {
                        store.persist_all_rows(session_id, cx);
                    }
                }
                let by_sol = store
                    .by_solution
                    .entry(meta.solution_id.clone())
                    .or_default();
                if !by_sol.contains(&session_id) {
                    by_sol.push(session_id);
                }
                // Re-seed token usage from the persisted metadata so the
                // status-row meter doesn't claim "0 tokens" for a long
                // resumed conversation. We only have a coarse aggregate
                // (`total_tokens`); the model will fill in the
                // input/output split + max_tokens on the next turn via
                // session_update events.
                if let Some(total) = meta.total_tokens {
                    acp_thread.update(cx, |thread, cx| {
                        thread.update_token_usage(
                            Some(acp_thread::TokenUsage {
                                used_tokens: total,
                                ..Default::default()
                            }),
                            cx,
                        );
                    });
                }
                let sub = store.subscribe_to_session(session_id, acp_thread, cx);
                store
                    .sessions
                    .get(&session_id)
                    .ok_or_else(|| anyhow!("session vanished after insert"))?
                    .update(cx, |s, _| s._acp_subscription = Some(sub));
                store.persist_session_row(session_id, cx);
                // Resume re-livens a previously soft-closed row. Clear
                // the marker so MCP `read_session_history` (and any
                // future "Archived sessions" UI) reports it as live
                // again until the user closes the tab next time.
                if let Some(db) = &store.persistence {
                    db.mark_closed(session_id, None).detach_and_log_err(cx);
                }
                cx.emit(SolutionAgentStoreEvent::SessionCreated {
                    id: session_id,
                    parent_session_id: meta.parent_session_id,
                });
                cx.notify();
                anyhow::Ok(session_id)
            })??;

            Ok(session_id)
        })
    }

    pub fn sessions_for(&self, solution_id: &SolutionId) -> Vec<Entity<SolutionSession>> {
        self.by_solution
            .get(solution_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.sessions.get(id).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn session(&self, id: SolutionSessionId) -> Option<Entity<SolutionSession>> {
        self.sessions.get(&id).cloned()
    }

    /// Models to offer for `session_id`: the cached list (live-captured or
    /// persisted). Empty → the status row shows a read-only label.
    pub fn session_models(
        &self,
        session_id: SolutionSessionId,
        cx: &App,
    ) -> Vec<claude_native::ModelInfo> {
        let Some(session) = self.session(session_id) else {
            return Vec::new();
        };
        let session = session.read(cx);
        if !session.cached_models.is_empty() {
            return session.cached_models.clone();
        }
        self.agent_models
            .get(&session.agent_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Currently-selected model `value`: explicit `desired_model`, else the
    /// live `active_model` mapped to a known entry, else None.
    pub fn selected_model(&self, session_id: SolutionSessionId, cx: &App) -> Option<String> {
        let session = self.session(session_id)?;
        let session = session.read(cx);
        if session.desired_model.is_some() {
            return session.desired_model.clone();
        }
        let active = session.acp_thread().and_then(|t| {
            let t = t.read(cx);
            t.connection().active_model(t.session_id())
        })?;
        let active_str = active.as_ref();
        session
            .cached_models
            .iter()
            .find(|m| active_str.contains(m.value.as_str()) || m.value.contains(active_str))
            .map(|m| m.value.clone())
            .or_else(|| Some(active.to_string()))
    }

    /// Models to offer (and the default to pre-select) when creating a NEW
    /// session for `(solution_id, agent_id)`, derived from that pair's most-
    /// recently-active session (its captured/persisted list + chosen model).
    /// Empty list + None when the pair has no sessions yet.
    pub fn new_chat_model_options(
        &self,
        solution_id: &SolutionId,
        agent_id: &AgentServerId,
        cx: &App,
    ) -> (Vec<claude_native::ModelInfo>, Option<String>) {
        let latest = self
            .sessions
            .values()
            .filter(|s| {
                let s = s.read(cx);
                &s.solution_id == solution_id && &s.agent_id == agent_id
            })
            .max_by_key(|s| s.read(cx).last_activity_at)
            .map(|s| s.read(cx).id);
        let (mut models, default) = match latest {
            Some(id) => (self.session_models(id, cx), self.selected_model(id, cx)),
            None => (Vec::new(), None),
        };
        if models.is_empty() {
            models = self.agent_models.get(agent_id).cloned().unwrap_or_default();
        }
        (models, default)
    }

    /// Probe `claude` for its current model list for `(solution_id, agent_id)`
    /// without a session — used by the new-chat "Refresh models". Resolves the
    /// registered server + the solution root as work_dir. Empty on any failure.
    pub fn probe_models_for_agent(
        &self,
        solution_id: &SolutionId,
        agent_id: &AgentServerId,
        cx: &mut App,
    ) -> Task<Result<Vec<claude_native::ModelInfo>>> {
        let Some(server) = self.server_registry.get(agent_id).cloned() else {
            return Task::ready(Ok(Vec::new()));
        };
        let Some(native) = server
            .into_any()
            .downcast::<claude_native::ClaudeNativeAgentServer>()
            .ok()
        else {
            return Task::ready(Ok(Vec::new()));
        };
        let work_dir = SolutionStore::try_global(cx).and_then(|st| {
            st.read(cx)
                .solutions()
                .iter()
                .find(|s| &s.id == solution_id)
                .map(|s| s.root.clone())
        });
        let Some(work_dir) = work_dir else {
            return Task::ready(Ok(Vec::new()));
        };
        native.probe_models(work_dir, &cx.to_async())
    }

    /// If we have no model list for `agent_id` yet, fire a one-shot probe to fill
    /// the global cache (so fresh sessions show a picker before their first turn).
    /// Deduped per agent. No-op if a list is already known or a probe is running.
    pub fn ensure_agent_models(
        &mut self,
        solution_id: SolutionId,
        agent_id: AgentServerId,
        cx: &mut Context<Self>,
    ) {
        if self
            .agent_models
            .get(&agent_id)
            .map_or(false, |m| !m.is_empty())
            || self.agent_models_probing.contains(&agent_id)
        {
            return;
        }
        self.agent_models_probing.insert(agent_id.clone());
        let task = self.probe_models_for_agent(&solution_id, &agent_id, cx);
        cx.spawn(async move |this, cx| {
            let models = task.await.log_err().unwrap_or_default();
            this.update(cx, |this, cx| {
                this.agent_models_probing.remove(&agent_id);
                if !models.is_empty() {
                    this.agent_models.insert(agent_id.clone(), models);
                    // Re-render every session of this agent so its status-row
                    // dropdown appears now that a list exists.
                    let ids: Vec<_> = this
                        .sessions
                        .values()
                        .filter(|s| s.read(cx).agent_id == agent_id)
                        .map(|s| s.read(cx).id)
                        .collect();
                    for id in ids {
                        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
                    }
                    cx.notify();
                }
            })
            .log_err();
        })
        .detach();
    }

    /// Record + apply a model choice. Persists `desired_model`; if the session
    /// is live, also pushes a `set_model` control request.
    pub fn select_model(
        &mut self,
        session_id: SolutionSessionId,
        value: String,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let live = session.read(cx).acp_thread().and_then(|t| {
            let t = t.read(cx);
            let acp_sid = t.session_id().clone();
            t.connection()
                .clone()
                .downcast::<claude_native::ClaudeNativeConnection>()
                .map(|c| (c, acp_sid))
        });
        session.update(cx, |s, _| s.desired_model = Some(value.clone()));
        if let Some((conn, acp_sid)) = live {
            conn.select_model(&acp_sid, value.clone());
            conn.set_desired_model(&acp_sid, Some(value));
        }
        self.persist_session_row(session_id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(session_id));
    }

    /// The session's chosen effort (`desired_effort`), if any.
    pub fn selected_effort(&self, session_id: SolutionSessionId, cx: &App) -> Option<String> {
        self.session(session_id)?.read(cx).desired_effort.clone()
    }

    /// Record + apply an effort choice. Persists `desired_effort`; seeds the
    /// native respawn map; if the session is live, sends `apply_flag_settings`
    /// so it takes effect on the next turn. Mirrors `select_model`.
    pub fn select_effort(
        &mut self,
        session_id: SolutionSessionId,
        value: String,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let live = session.read(cx).acp_thread().and_then(|t| {
            let t = t.read(cx);
            let acp_sid = t.session_id().clone();
            t.connection()
                .clone()
                .downcast::<claude_native::ClaudeNativeConnection>()
                .map(|c| (c, acp_sid))
        });
        session.update(cx, |s, _| s.desired_effort = Some(value.clone()));
        if let Some((conn, acp_sid)) = live {
            conn.set_desired_effort(&acp_sid, Some(value.clone()));
            conn.select_effort(&acp_sid, value);
        }
        self.persist_session_row(session_id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(session_id));
    }

    /// Re-query the model list. Live → re-read the connection's captured list.
    /// Cold → probe (wired in a later task). Updates `cached_models` + persists.
    pub fn refresh_models(&mut self, session_id: SolutionSessionId, cx: &mut Context<Self>) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let live = session.read(cx).acp_thread().and_then(|t| {
            let t = t.read(cx);
            let acp_sid = t.session_id().clone();
            t.connection()
                .clone()
                .downcast::<claude_native::ClaudeNativeConnection>()
                .map(|c| c.available_models(&acp_sid))
        });
        // Live-non-empty → update the per-session + global cache and persist.
        // Otherwise (the session is live but its list is still empty — a fresh
        // session that hasn't taken its first turn yet — OR the session is
        // cold) fall through to the probe path, which fills the caches by
        // spawning a throwaway `claude` keyed by server + cwd.
        if let Some(models) = live {
            if !models.is_empty() {
                let agent_id = session.read(cx).agent_id.clone();
                self.agent_models.insert(agent_id, models.clone());
                session.update(cx, |s, _| s.cached_models = models);
                self.persist_session_row(session_id, cx);
                cx.emit(SolutionAgentStoreEvent::SessionStateChanged(session_id));
                return;
            }
        }
        self.refresh_models_cold(session_id, cx);
    }

    /// Cold-session model refresh: a COLD session has no live process AND
    /// `project: None`, so the normal connection path is unavailable. Instead
    /// ask the registered server to spawn a throwaway `claude`, read its
    /// advertised model list, and kill it — without waking the real session.
    fn refresh_models_cold(&mut self, session_id: SolutionSessionId, cx: &mut Context<Self>) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let agent_id = session.read(cx).agent_id.clone();
        let work_dir = session.read(cx).cwd.clone();
        let Some(server) = self.server_registry.get(&agent_id).cloned() else {
            return;
        };
        let Some(native) = server
            .into_any()
            .downcast::<claude_native::ClaudeNativeAgentServer>()
            .ok()
        else {
            return;
        };
        let task = native.probe_models(work_dir, &cx.to_async());
        cx.spawn(async move |this, cx| {
            let models = task.await.log_err().unwrap_or_default();
            if models.is_empty() {
                return;
            }
            this.update(cx, |this, cx| {
                this.agent_models.insert(agent_id, models.clone());
                if let Some(session) = this.session(session_id) {
                    session.update(cx, |s, _| s.cached_models = models);
                    this.persist_session_row(session_id, cx);
                    cx.emit(SolutionAgentStoreEvent::SessionStateChanged(session_id));
                }
            })
            .log_err();
        })
        .detach();
    }

    /// Reverse-lookup the `SolutionSessionId` owning a given ACP session id.
    /// The native hook pull-closure knows only the `acp::SessionId`, but the
    /// queue is keyed by `SolutionSessionId`; there are only a handful of live
    /// sessions per solution so a linear scan is fine.
    pub fn session_id_for_acp(
        &self,
        acp_session_id: &acp::SessionId,
        cx: &App,
    ) -> Option<SolutionSessionId> {
        self.sessions
            .iter()
            .find(|(_, session)| session.read(cx).acp_session_id == *acp_session_id)
            .map(|(id, _)| *id)
    }

    pub fn all_sessions(&self) -> impl Iterator<Item = Entity<SolutionSession>> + '_ {
        self.sessions.values().cloned()
    }

    /// Per-session inbox dir for image attachments delivered mid-turn:
    /// `<solution_root>/.agents/<sid>/inbox/` when the owning solution
    /// resolves (co-located with the session's compact handoff dumps, sits at
    /// the solution root — outside the member git repos — and is reaped with
    /// the session). Falls back to the OS temp dir when there is no
    /// `SolutionStore` or the solution isn't registered (headless / test).
    fn session_inbox_dir(&self, session_id: SolutionSessionId, cx: &App) -> std::path::PathBuf {
        let solution_root = self.sessions.get(&session_id).and_then(|s| {
            let solution_id = s.read(cx).solution_id.clone();
            SolutionStore::try_global(cx)?
                .read(cx)
                .solutions()
                .iter()
                .find(|sol| sol.id == solution_id)
                .map(|sol| sol.root.clone())
        });
        match solution_root {
            Some(root) => root
                .join(".agents")
                .join(session_id.to_string())
                .join("inbox"),
            None => std::env::temp_dir()
                .join("sawe-inbox")
                .join(session_id.to_string()),
        }
    }

    /// Drain the session's queued follow-ups, push them into the live thread as
    /// one user entry, and return the agent-facing text (per-message timestamps
    /// already baked in; a leading hint line when `is_end_of_turn`). Returns
    /// `None` when the queue is empty or the session/thread is gone. Invoked by
    /// the native hook pull-closure (see `subscribe_to_session`).
    pub fn take_pending_for_delivery(
        &mut self,
        session_id: SolutionSessionId,
        agent_id: Option<&str>,
        is_end_of_turn: bool,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        // Per-tab routing: drain only the bundles addressed to the firing
        // hook. The main agent's hook carries no `agent_id` and drains
        // `QueueTarget::Main` bundles; an Agent Teams teammate's hook carries
        // its `agent_id` and drains only its own `QueueTarget::Subagent(id)`
        // bundles. Differently-targeted bundles stay queued for their own
        // addressee's hook (or get dropped at turn end if that addressee is
        // already gone — see the `Stopped` idle-flush).
        let session = self.session(session_id)?;
        let combined: Vec<acp::ContentBlock> = session.update(cx, |s, _| {
            let mut taken: Vec<acp::ContentBlock> = Vec::new();
            let mut kept: std::collections::VecDeque<crate::model::PendingBundle> =
                std::collections::VecDeque::with_capacity(s.pending_messages.len());
            for bundle in s.pending_messages.drain(..) {
                // `additionalContext` is TEXT-ONLY, so an image block's bytes
                // can't ride it. MID-TURN we still drain image bundles and hand
                // the agent a saved-file path (it `Read`s the pixels — see
                // `save_inbox_image` below); that's the whole point of this
                // path. AT END-OF-TURN we keep deferring them so the `Stopped`
                // idle-flush re-sends the full multimodal blocks as a fresh
                // turn (richer, and the turn is ending anyway).
                let has_image = bundle
                    .blocks
                    .iter()
                    .any(|b| matches!(b, acp::ContentBlock::Image(_)));
                let defer_image = has_image && is_end_of_turn;
                if bundle.target.matches_hook(agent_id) && !defer_image {
                    taken.extend(bundle.blocks);
                } else {
                    kept.push_back(bundle);
                }
            }
            s.pending_messages = kept;
            taken
        });
        if combined.is_empty() {
            return None;
        }

        // Save any image attachments to inbox files and reference them by path
        // in the agent-facing text (the agent opens them with `Read`). The
        // timeline push below keeps the ORIGINAL image blocks, so the user's
        // conversation bubble still shows the picture. At end-of-turn there are
        // no images in `combined` (they were deferred above), so this collapses
        // to plain text.
        let mut image_paths: Vec<Option<std::path::PathBuf>> = Vec::new();
        if combined
            .iter()
            .any(|b| matches!(b, acp::ContentBlock::Image(_)))
        {
            // Resolve the inbox dir only when a bundle actually carries an
            // image (the common text-only path pays nothing).
            let dir = self.session_inbox_dir(session_id, cx);
            for block in &combined {
                if let acp::ContentBlock::Image(img) = block {
                    image_paths.push(queue::save_inbox_image(&dir, image_paths.len(), img));
                }
            }
        }
        // Computed before the timeline push so `combined` can be moved into it
        // (no clone) below. Prepend the hint only at end-of-turn.
        let body = queue::inject_text_from_blocks_with_image_paths(&combined, &image_paths);
        let text = if is_end_of_turn {
            format!("{}\n\n{}", queue::QUEUE_HINT_LINE, body)
        } else {
            body
        };

        // Timeline entry = the raw (timestamp-baked) blocks; the baked stamp is
        // stripped at render time, so the bubble shows clean user text. Only
        // do this for a MAIN delivery (`agent_id` is None): the parent
        // `AcpThread` IS the Main-view timeline, so a subagent-targeted
        // message pushed here would wrongly surface in the Main conversation
        // rather than the teammate's tab (which is sourced from the teammate's
        // own on-disk JSONL, not the parent thread). The message is still
        // delivered to the teammate via `additionalContext`; only the
        // optimistic bubble is skipped for the subagent case.
        if agent_id.is_none()
            && let Some(thread) = session.read(cx).acp_thread().cloned()
        {
            thread.update(cx, |thread, cx| {
                thread.push_user_message_entry(None, combined, cx);
            });
        }
        self.mark_queue_changed(session_id, cx);
        cx.notify();

        Some(text)
    }

    /// Test-only helper: register a session whose `acp_thread` was constructed
    /// elsewhere (or left `None`). Real `create_session` (Task 3.3) replaces
    /// this for production use.
    #[cfg(any(feature = "test-support", test))]
    pub fn register_prebuilt_session(
        &mut self,
        session: SolutionSession,
        cx: &mut Context<Self>,
    ) -> SolutionSessionId {
        let id = session.id;
        let solution_id = session.solution_id.clone();
        let parent_session_id = session.parent_session_id;
        let entity = cx.new(|_| session);
        self.sessions.insert(id, entity);
        self.by_solution.entry(solution_id).or_default().push(id);
        cx.emit(SolutionAgentStoreEvent::SessionCreated {
            id,
            parent_session_id,
        });
        cx.notify();
        id
    }

    /// Test-only helper: insert a minimal `SolutionSession` (idle, no acp
    /// thread) into the store for the given solution. Returns the new session
    /// id. Used by integration tests that need a session without going through
    /// the full `create_session` flow.
    #[cfg(any(test, feature = "test-support"))]
    pub fn create_for_test_minimal(
        &mut self,
        solution_id: &SolutionId,
        title: &str,
        cx: &mut Context<Self>,
    ) -> SolutionSessionId {
        let id = SolutionSessionId::new();
        let mut session = SolutionSession::new_idle(
            id,
            solution_id.clone(),
            SharedString::from("mock-agent"),
            acp::SessionId::new(format!("acp-{}", id.as_str())),
        );
        session.title = SharedString::from(title);
        self.register_prebuilt_session(session, cx)
    }

    /// Restore tabs the user had open the last time they closed this
    /// Solution, **without spawning the agent subprocess**. For each
    /// session id where `tab_order IS NOT NULL`, hydrate a
    /// `SolutionSession` with `acp_thread: None` and `cold_entries`
    /// populated from the persisted JSON blob. The session view will
    /// render those entries as a read-only conversation; the live
    /// `AcpThread` is only attached if/when the user submits a new
    /// message via `resume_session`.
    ///
    /// Sessions that already exist in `self.sessions` (created earlier
    /// in this process — e.g. via MCP from another window) are left
    /// untouched: they keep their live `acp_thread` and the navigator
    /// will pick them up via the normal reconcile path.
    ///
    /// Returns the ordered ids matching `tab_order ASC`. Caller (the
    /// navigator) uses that order directly to populate the strip,
    /// instead of relying on `created_at` sort.
    pub fn restore_open_tabs(
        &self,
        solution_id: SolutionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<SolutionSessionId>>> {
        let Some(db) = self.persistence.clone() else {
            return Task::ready(Ok(Vec::new()));
        };
        let already_open: std::collections::HashSet<SolutionSessionId> =
            self.sessions.keys().copied().collect();
        cx.spawn(async move |this, cx| {
            let ordered_ids = db.list_open_tabs(solution_id.clone()).await?;
            if ordered_ids.is_empty() {
                return Ok(Vec::new());
            }
            // Pull metadata for the whole solution once (single query) and
            // index by id. Cheaper than N round-trips when the user had
            // five-plus tabs open.
            let metas = db.list_for_solution(solution_id.clone()).await?;
            let by_id: std::collections::HashMap<SolutionSessionId, SolutionSessionMetadata> =
                metas.into_iter().map(|m| (m.id, m)).collect();
            // Phase 4: prefer per-entry rows. Load rows + epoch for every id
            // we'll hydrate; only fall back to (and deserialize) the legacy
            // transcript blob when a session has no rows yet — that blob path
            // also triggers a lazy row migration in the foreground block below.
            let mut rows_per_session: std::collections::HashMap<
                SolutionSessionId,
                Vec<crate::db::EntryRow>,
            > = std::collections::HashMap::new();
            let mut epoch_per_session: std::collections::HashMap<SolutionSessionId, i64> =
                std::collections::HashMap::new();
            let mut blobs: std::collections::HashMap<SolutionSessionId, Vec<u8>> =
                std::collections::HashMap::new();
            for id in &ordered_ids {
                if already_open.contains(id) {
                    continue;
                }
                let rows = db.load_entries(*id).await?;
                let epoch = db.load_epoch(*id).await?.unwrap_or(0);
                epoch_per_session.insert(*id, epoch);
                if rows.is_empty() {
                    if let Some(bytes) = db.load_blob(*id).await? {
                        blobs.insert(*id, bytes);
                    }
                } else {
                    rows_per_session.insert(*id, rows);
                }
            }
            // Apply on the foreground thread so the cx.new + emit
            // observe-callbacks all happen in the GPUI scheduler.
            // Collect the ids that survive into a result vec — orphans
            // (tab_order pointing at deleted metadata) and
            // hydration failures must NOT appear in the navigator's
            // restored strip, so the returned Vec only contains ids
            // that are now backed by a live `Entity<SolutionSession>`.
            let result_ids: Vec<SolutionSessionId> = this.update(cx, |this, cx| {
                let mut hydrated: Vec<SolutionSessionId> = Vec::with_capacity(ordered_ids.len());
                for (tab_idx, id) in ordered_ids.iter().enumerate() {
                    let tab_order = Some(tab_idx as i64);
                    if let Some(entity) = this.sessions.get(id) {
                        // Session already live — just stamp the tab_order so the
                        // in-memory view stays consistent with the DB column.
                        entity.update(cx, |s, _| s.tab_order = tab_order);
                        hydrated.push(*id);
                        continue;
                    }
                    let Some(meta) = by_id.get(id) else {
                        // tab_order pointed at a session whose metadata
                        // was deleted out from under it. Skip — the
                        // navigator never sees this id in the
                        // returned slice.
                        log::warn!("restore_open_tabs: orphaned tab_order for {id}");
                        continue;
                    };
                    // Phase 4: row-native sessions load their transcript from
                    // the per-entry rows and READ the persisted epoch (no bump —
                    // a restart loading the same transcript must not look like a
                    // new generation to the mobile delta client). Legacy sessions
                    // (no rows) keep the blob path verbatim and lazily migrate to
                    // rows afterwards.
                    let epoch = epoch_per_session.get(id).copied().unwrap_or(0);
                    let rows = rows_per_session.remove(id);
                    // Only deserialize the blob in the legacy (no-rows) branch.
                    let persisted = if rows.is_some() {
                        None
                    } else {
                        blobs.remove(id).and_then(|bytes| {
                            serde_json::from_slice::<PersistedSession>(&bytes).ok()
                        })
                    };
                    let migrating = rows.is_none();
                    // Read model/effort/cached_models from metadata columns first
                    // (Task 3a); fall back to the blob for legacy rows written
                    // before these columns existed (NULL = not yet migrated). In
                    // the rows branch `persisted` is None so the fallback degrades
                    // to column-only. For the migrate branch, persist_session_row
                    // below flushes the recovered model/effort to columns so the
                    // next cold-restore (rows branch) retains them.
                    let restored_available_models = if !meta.cached_models.is_empty() {
                        meta.cached_models.clone()
                    } else {
                        persisted
                            .as_ref()
                            .map(|p| p.available_models.clone())
                            .unwrap_or_default()
                    };
                    let restored_desired_model = meta.desired_model.clone().or_else(|| {
                        persisted.as_ref().and_then(|p| p.desired_model.clone())
                    });
                    let restored_desired_effort = meta.desired_effort.clone().or_else(|| {
                        persisted.as_ref().and_then(|p| p.desired_effort.clone())
                    });
                    let entries = if let Some(rows) = rows {
                        entries_from_rows(rows)
                    } else {
                        // Reconstruct the persisted dialog as live-shape
                        // `AgentThreadEntry`s. Prefer the structured v2 payload;
                        // legacy v1 / pre-v1 blobs degrade to a single
                        // Assistant-shaped entry per flat markdown summary.
                        let (cold_entries, restored_created_ms) =
                            cold_entries_from_persisted(persisted, cx);
                        crate::session_entry::rebuild_entries(
                            &cold_entries,
                            &[],
                            &restored_created_ms,
                            0,
                            cx,
                        )
                    };
                    let entity = cx.new(|_| {
                        let mut s = SolutionSession::new_idle(
                            meta.id,
                            meta.solution_id.clone(),
                            meta.agent_id.clone(),
                            meta.acp_session_id.clone(),
                        );
                        s.title = meta.title.clone();
                        s.created_at = meta.created_at;
                        s.last_activity_at = meta.last_activity_at;
                        s.context_count = meta.context_count;
                        s.cwd = meta.cwd.clone();
                        s.entries = entries;
                        s.init_change_seq_from_entries();
                        if migrating {
                            s.bump_epoch();
                        } else {
                            s.epoch = epoch as u64;
                        }
                        // Seed from the persisted metadata so the
                        // status-row meter shows the last-known total
                        // for cold tabs (no live thread → no
                        // `TokenUsage`). The live path refreshes this
                        // on every `TokenUsageUpdated` event.
                        s.cached_total_tokens = meta.total_tokens;
                        s.parent_session_id = meta.parent_session_id;
                        s.tab_order = tab_order;
                        s.cached_models = restored_available_models;
                        s.desired_model = restored_desired_model;
                        s.desired_effort = restored_desired_effort;
                        s
                    });
                    this.sessions.insert(meta.id, entity);
                    // Legacy → rows lazy migration: write the freshly-built
                    // transcript out as rows so the next restore takes the rows
                    // branch. Blob is kept until Task 5 removes it; model/effort
                    // flushed to columns during migration so the next cold-restore
                    // (rows branch, no blob read) retains them. Idempotent:
                    // guarded by the rows-empty check above.
                    if migrating {
                        this.persist_all_rows(meta.id, cx);
                        this.persist_session_row(meta.id, cx);
                    }
                    this.by_solution
                        .entry(solution_id.clone())
                        .or_default()
                        .push(meta.id);
                    cx.emit(SolutionAgentStoreEvent::SessionCreated {
                        id: meta.id,
                        parent_session_id: meta.parent_session_id,
                    });
                    hydrated.push(meta.id);
                }
                cx.notify();
                hydrated
            })?;
            Ok(result_ids)
        })
    }

    /// Like [`restore_open_tabs`], but loads **every** session row for the
    /// solution — including ones with `tab_order IS NULL` (closed tabs).
    /// Sessions already in `self.sessions` are skipped. Each freshly-
    /// hydrated session gets a `cold_entries` reconstruction from its
    /// persisted blob, so subsequent `get_session` / `list_sessions`
    /// calls see the full conversation history without needing the
    /// subprocess respawned.
    ///
    /// Driven by `solution_agent.list_sessions` so an MCP-only consumer
    /// (the phone) can see closed-tab sessions — the desktop's tab strip
    /// path was the only thing populating the in-memory store before,
    /// which left closed sessions invisible to MCP regardless of how
    /// much data was on disk.
    /// Best-effort GC of on-disk per-session archive dirs
    /// (`<solution_root>/.agents/<sid>/` — compact handoff dumps + the
    /// mid-turn image inbox). Only kicks in once a solution has accumulated
    /// more than [`ARCHIVE_REAP_MIN_SESSIONS`] sessions (counting closed ones),
    /// and only removes those whose last activity was over
    /// [`ARCHIVE_REAP_MAX_AGE_DAYS`] days ago — small or active workspaces keep
    /// everything. Runs off the foreground thread; failures are logged, not
    /// surfaced.
    fn reap_stale_session_archives(&self, solution_id: SolutionId, cx: &mut Context<Self>) {
        let Some(db) = self.persistence.clone() else {
            return;
        };
        let Some(root) = SolutionStore::try_global(cx).and_then(|store| {
            store
                .read(cx)
                .solutions()
                .iter()
                .find(|sol| sol.id == solution_id)
                .map(|sol| sol.root.clone())
        }) else {
            return;
        };
        cx.background_spawn(async move {
            let metas = match db.list_for_solution(solution_id).await {
                Ok(metas) => metas,
                Err(_) => return,
            };
            for dir in stale_archive_dirs(&root, &metas, Utc::now()) {
                if dir.exists() {
                    std::fs::remove_dir_all(&dir).log_err();
                }
            }
        })
        .detach();
    }

    pub fn hydrate_all_for_solution(
        &self,
        solution_id: SolutionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<SolutionSessionId>>> {
        // Opening a solution is a natural, infrequent point to garbage-collect
        // stale on-disk session archives under `.agents/`.
        self.reap_stale_session_archives(solution_id.clone(), cx);
        let Some(db) = self.persistence.clone() else {
            return Task::ready(Ok(Vec::new()));
        };
        let already_open: std::collections::HashSet<SolutionSessionId> =
            self.sessions.keys().copied().collect();
        cx.spawn(async move |this, cx| {
            // `list_open_session_ids` filters out rows whose `closed_at`
            // is set — sessions the user explicitly closed via the
            // desktop's close-tab affordance. Without this, every
            // refresh after a close would re-hydrate the closed
            // session back into self.sessions, undoing the close from
            // the phone's perspective on the very next list_sessions.
            let open_ids: std::collections::HashSet<SolutionSessionId> = db
                .list_open_session_ids(solution_id.clone())
                .await?
                .into_iter()
                .collect();
            // Fetch the ordered tab-strip list so we can stamp
            // `tab_order` on freshly-hydrated sessions. Sessions not
            // in this list get `tab_order = None` (closed/hidden tab).
            let tabbed_ids: Vec<SolutionSessionId> =
                db.list_open_tabs(solution_id.clone()).await.unwrap_or_default();
            let tab_order_map: std::collections::HashMap<SolutionSessionId, i64> = tabbed_ids
                .iter()
                .enumerate()
                .map(|(i, id)| (*id, i as i64))
                .collect();
            if open_ids.is_empty() {
                return Ok(Vec::new());
            }
            let metas = db.list_for_solution(solution_id.clone()).await?;
            if metas.is_empty() {
                return Ok(Vec::new());
            }
            let to_hydrate: Vec<&SolutionSessionMetadata> = metas
                .iter()
                .filter(|m| open_ids.contains(&m.id) && !already_open.contains(&m.id))
                .collect();
            if to_hydrate.is_empty() {
                return Ok(Vec::new());
            }
            // Phase 4: prefer per-entry rows. Load rows + epoch for every
            // session; only load+deserialize the legacy transcript blob when a
            // session has no rows yet (the foreground block then lazily migrates
            // that blob to rows). Missing rows AND blob just mean the session
            // never had conversation content — hydrates with empty entries.
            let mut rows_per_session: std::collections::HashMap<
                SolutionSessionId,
                Vec<crate::db::EntryRow>,
            > = std::collections::HashMap::new();
            let mut epoch_per_session: std::collections::HashMap<SolutionSessionId, i64> =
                std::collections::HashMap::new();
            let mut blobs: std::collections::HashMap<SolutionSessionId, Vec<u8>> =
                std::collections::HashMap::new();
            for meta in &to_hydrate {
                let rows = db.load_entries(meta.id).await?;
                let epoch = db.load_epoch(meta.id).await?.unwrap_or(0);
                epoch_per_session.insert(meta.id, epoch);
                if rows.is_empty() {
                    if let Some(bytes) = db.load_blob(meta.id).await? {
                        blobs.insert(meta.id, bytes);
                    }
                } else {
                    rows_per_session.insert(meta.id, rows);
                }
            }
            // Pre-load background_agent rows for every session about to
            // hydrate. Mirrors the blob pre-load above — keeps the
            // foreground update block free of awaits. `unwrap_or_default`
            // so one bad row doesn't abort all hydration.
            let mut bg_rows_per_session: std::collections::HashMap<
                SolutionSessionId,
                Vec<crate::db::BackgroundAgentRow>,
            > = std::collections::HashMap::new();
            for meta in &to_hydrate {
                let rows = db
                    .load_background_agents(meta.id.to_string())
                    .await
                    .unwrap_or_default();
                bg_rows_per_session.insert(meta.id, rows);
            }
            let result_ids: Vec<SolutionSessionId> = this.update(cx, |this, cx| {
                let mut hydrated: Vec<SolutionSessionId> = Vec::with_capacity(to_hydrate.len());
                for meta in &to_hydrate {
                    if this.sessions.contains_key(&meta.id) {
                        continue;
                    }
                    // Phase 4: row-native sessions load from rows + read the
                    // persisted epoch (no bump). Legacy sessions (no rows) keep
                    // the blob path verbatim, then lazily migrate to rows.
                    let epoch = epoch_per_session.get(&meta.id).copied().unwrap_or(0);
                    let rows = rows_per_session.remove(&meta.id);
                    let migrating = rows.is_none();
                    let session_tab_order = tab_order_map.get(&meta.id).copied();
                    let entries = if let Some(rows) = rows {
                        entries_from_rows(rows)
                    } else {
                        let persisted = blobs.remove(&meta.id).and_then(|bytes| {
                            serde_json::from_slice::<PersistedSession>(&bytes).ok()
                        });
                        let restored_created_ms = persisted
                            .as_ref()
                            .map(|p| p.entry_created_ms.clone())
                            .unwrap_or_default();
                        let (cold_entries, _) = cold_entries_from_persisted(persisted, cx);
                        crate::session_entry::rebuild_entries(
                            &cold_entries,
                            &[],
                            &restored_created_ms,
                            0,
                            cx,
                        )
                    };
                    let entity = cx.new(|_| {
                        let mut s = SolutionSession::new_idle(
                            meta.id,
                            meta.solution_id.clone(),
                            meta.agent_id.clone(),
                            meta.acp_session_id.clone(),
                        );
                        s.title = meta.title.clone();
                        s.created_at = meta.created_at;
                        s.last_activity_at = meta.last_activity_at;
                        s.context_count = meta.context_count;
                        s.cwd = meta.cwd.clone();
                        s.entries = entries;
                        s.init_change_seq_from_entries();
                        if migrating {
                            s.bump_epoch();
                        } else {
                            s.epoch = epoch as u64;
                        }
                        s.cached_total_tokens = meta.total_tokens;
                        s.parent_session_id = meta.parent_session_id;
                        s.tab_order = session_tab_order;
                        s
                    });
                    // Insert into `self.sessions` so the phone's
                    // list_sessions (via all_sessions()) and get_session
                    // (via self.sessions.get()) can find it. INTENTIONALLY
                    // skip `by_solution` and the SessionCreated event —
                    // those are the desktop navigator's input. The
                    // navigator's reconcile_open_sessions_with_store
                    // reads sessions_for() (= by_solution lookup), so
                    // leaving by_solution alone keeps the navigator
                    // ignorant of cold-hydrated sessions, which is what
                    // we want: hydration is read-only metadata exposure
                    // for the phone, not a 'reopen all closed tabs'
                    // command. If/when the user genuinely reopens one
                    // of these via the tab strip, restore_open_tabs's
                    // contains_key check will skip the re-insert but
                    // the navigator's own open_session path will add
                    // it to by_solution at that point.
                    this.sessions.insert(meta.id, entity);
                    // Legacy → rows lazy migration (idempotent; guarded by
                    // rows-empty). Blob kept (model/effort fallback; Task 5).
                    if migrating {
                        this.persist_all_rows(meta.id, cx);
                    }
                    hydrated.push(meta.id);
                }
                // Task 13: restore persisted background_agents per session.
                // Done after the session entities exist so
                // `reconcile_background_agents_for` can look them up via
                // `self.session(...)`. Iterates `hydrated` rather than
                // `to_hydrate` so we never touch a session that the
                // `contains_key` guard above skipped.
                for sid in &hydrated {
                    let rows = bg_rows_per_session.remove(sid).unwrap_or_default();
                    if !rows.is_empty() {
                        this.reconcile_background_agents_for(*sid, rows, cx);
                    }
                }
                // Background shell rows are ephemeral: the subprocess and
                // its /tmp output file are both gone after a restart. Drop
                // the stale rows so they don't accumulate across restarts.
                // We never restore them into `background_shells` — a fresh
                // shell must be launched by the user after resume.
                if let Some(db) = this.persistence.clone() {
                    for sid in &hydrated {
                        let session_id = sid.to_string();
                        cx.background_spawn({
                            let db = db.clone();
                            async move {
                                db.delete_background_shells_for_session(session_id)
                                    .await
                                    .log_err();
                            }
                        })
                        .detach();
                    }
                }
                // Fan out `workspace.session_opened` for every freshly-hydrated
                // session that ended up tab-pinned. The store path that drives
                // the sequenced delta (`persist_tab_order`) is NOT invoked
                // here because the tab_order was set directly on the in-memory
                // entity above; without this manual emit a mobile client
                // that's already connected to the desktop process would never
                // hear about the just-hydrated sessions (their `tab_order` is
                // populated but no notification ever fired). The mobile-side
                // mirror would only learn via the next `workspace.snapshot`
                // round-trip — which doesn't happen until the user toggles
                // reconnect or backgrounds and resumes the app. Symptom:
                // opening a previously-closed solution from the picker
                // showed the row with zero consoles even though the desktop
                // had restored them. The emit shape is identical to
                // `persist_tab_order`'s; the mobile applier is idempotent
                // on duplicate session_opened with the same id.
                if let Some(coord) =
                    editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx)
                {
                    for id in &hydrated {
                        let Some(entity) = this.sessions.get(id) else {
                            continue;
                        };
                        let (is_tabbed, summary) = entity.read_with(cx, |s, cx| {
                            (s.tab_order.is_some(), crate::mcp::session_summary(s, cx))
                        });
                        if !is_tabbed {
                            continue;
                        }
                        coord.emit_sequenced(
                            cx,
                            "workspace.session_opened",
                            serde_json::json!({
                                "solution_id": solution_id.as_str(),
                                "session": summary,
                            }),
                        );
                    }
                }
                if !hydrated.is_empty() {
                    cx.notify();
                }
                hydrated
            })?;
            Ok(result_ids)
        })
    }

    /// Lazy sibling of [`hydrate_all_for_solution`] used by the console
    /// panel's tab restore. Instead of loading every open session's
    /// `acp_thread_blob` before any tab can paint, this materialises
    /// *placeholder* session entities (metadata only, empty `cold_entries`,
    /// `hydrating = true`) for all open chat tabs in one fast foreground
    /// pass and resolves the returned task as soon as the `priority`
    /// session's blob has loaded. Every other session's transcript loads on
    /// detached background tasks and lands on its entity afterwards (the
    /// session view shows a spinner until then). The net effect: opening a
    /// solution with many heavy chat tabs paints the strip + the active
    /// tab's content immediately rather than blocking on a serial blob load.
    ///
    /// Registration mirrors `hydrate_all_for_solution` exactly — sessions
    /// are inserted into `self.sessions` only (NOT `by_solution`) and a
    /// `workspace.session_opened` is emitted for tab-pinned rows — so the
    /// mobile `list_sessions` / navigator stay consistent regardless of
    /// which restore path ran. Idempotent against `already_open`.
    ///
    /// `priority` is the session id of the tab that will be active when the
    /// panel finishes restoring; pass `None` to load every blob detached
    /// (the task then resolves right after the placeholders are created).
    pub fn hydrate_open_tabs_lazy(
        &self,
        solution_id: SolutionId,
        priority: Option<SolutionSessionId>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<SolutionSessionId>>> {
        self.reap_stale_session_archives(solution_id.clone(), cx);
        let Some(db) = self.persistence.clone() else {
            return Task::ready(Ok(Vec::new()));
        };
        let already_open: std::collections::HashSet<SolutionSessionId> =
            self.sessions.keys().copied().collect();
        cx.spawn(async move |this, cx| {
            // Metadata-only queries — deliberately NO blob loads here so the
            // placeholder pass below can return fast.
            let open_ids: std::collections::HashSet<SolutionSessionId> = db
                .list_open_session_ids(solution_id.clone())
                .await?
                .into_iter()
                .collect();
            if open_ids.is_empty() {
                return Ok(Vec::new());
            }
            let tabbed_ids: Vec<SolutionSessionId> =
                db.list_open_tabs(solution_id.clone()).await.unwrap_or_default();
            let tab_order_map: std::collections::HashMap<SolutionSessionId, i64> = tabbed_ids
                .iter()
                .enumerate()
                .map(|(i, id)| (*id, i as i64))
                .collect();
            let metas = db.list_for_solution(solution_id.clone()).await?;
            if metas.is_empty() {
                return Ok(Vec::new());
            }

            // Foreground pass 1: create empty placeholders for every open,
            // not-yet-loaded session and emit the same tab-pinned
            // `session_opened` deltas `hydrate_all_for_solution` would. No
            // blob touched, so this returns near-instantly.
            let hydrated: Vec<SolutionSessionId> = this.update(cx, |this, cx| {
                let mut hydrated: Vec<SolutionSessionId> = Vec::new();
                for meta in &metas {
                    if !open_ids.contains(&meta.id) || already_open.contains(&meta.id) {
                        continue;
                    }
                    if this.sessions.contains_key(&meta.id) {
                        continue;
                    }
                    let session_tab_order = tab_order_map.get(&meta.id).copied();
                    let entity = cx.new(|_| {
                        let mut s = SolutionSession::new_idle(
                            meta.id,
                            meta.solution_id.clone(),
                            meta.agent_id.clone(),
                            meta.acp_session_id.clone(),
                        );
                        s.title = meta.title.clone();
                        s.created_at = meta.created_at;
                        s.last_activity_at = meta.last_activity_at;
                        s.context_count = meta.context_count;
                        s.cwd = meta.cwd.clone();
                        // Blob not loaded yet — left empty, filled by the
                        // background pass below. `hydrating` flips the
                        // session view's empty state to a spinner.
                        s.hydrating = true;
                        s.cached_total_tokens = meta.total_tokens;
                        s.parent_session_id = meta.parent_session_id;
                        s.tab_order = session_tab_order;
                        s
                    });
                    // Same intentional partial registration as
                    // `hydrate_all_for_solution`: `self.sessions` only, skip
                    // `by_solution` + `SessionCreated` (see that method's
                    // comment for why).
                    this.sessions.insert(meta.id, entity);
                    hydrated.push(meta.id);
                }
                if let Some(coord) =
                    editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx)
                {
                    for id in &hydrated {
                        let Some(entity) = this.sessions.get(id) else {
                            continue;
                        };
                        let (is_tabbed, summary) = entity.read_with(cx, |s, cx| {
                            (s.tab_order.is_some(), crate::mcp::session_summary(s, cx))
                        });
                        if !is_tabbed {
                            continue;
                        }
                        coord.emit_sequenced(
                            cx,
                            "workspace.session_opened",
                            serde_json::json!({
                                "solution_id": solution_id.as_str(),
                                "session": summary,
                            }),
                        );
                    }
                }
                if !hydrated.is_empty() {
                    cx.notify();
                }
                hydrated
            })?;

            if hydrated.is_empty() {
                return Ok(Vec::new());
            }

            // Load the priority (soon-to-be-active) tab's blob inline so the
            // panel paints its content immediately instead of a spinner; the
            // returned task only resolves once this lands.
            let priority = priority.filter(|p| hydrated.contains(p));
            if let Some(priority_id) = priority {
                Self::load_cold_blob_into_session(db.clone(), this.clone(), cx, priority_id).await;
            }

            // Every other restored tab hydrates on its own detached task so a
            // big backlog can't block the foreground; each lands on its entity
            // and clears its spinner independently.
            for sid in hydrated
                .iter()
                .copied()
                .filter(|id| Some(*id) != priority)
            {
                let db = db.clone();
                let this = this.clone();
                cx.spawn(async move |cx| {
                    Self::load_cold_blob_into_session(db, this, cx, sid).await;
                })
                .detach();
            }

            Ok(hydrated)
        })
    }

    /// Background helper for [`hydrate_open_tabs_lazy`]: load one session's
    /// transcript blob + background-agent rows off-thread and apply them to
    /// the already-materialised placeholder entity, clearing `hydrating`. A
    /// missing entity (session closed before the blob landed) or a failed
    /// load is logged and dropped — the placeholder simply stays empty.
    async fn load_cold_blob_into_session(
        db: Arc<crate::db::SolutionAgentDb>,
        this: WeakEntity<Self>,
        cx: &mut AsyncApp,
        session_id: SolutionSessionId,
    ) {
        // Phase 4: prefer per-entry rows. Load rows + epoch; only load+
        // deserialize the legacy blob when there are no rows (then lazily
        // migrate it below).
        let rows = db.load_entries(session_id).await.unwrap_or_default();
        let epoch = db.load_epoch(session_id).await.ok().flatten().unwrap_or(0);
        let blob = if rows.is_empty() {
            db.load_blob(session_id).await.unwrap_or(None)
        } else {
            None
        };
        let bg_rows = db
            .load_background_agents(session_id.to_string())
            .await
            .unwrap_or_default();
        this.update(cx, |this, cx| {
            let migrating = rows.is_empty();
            let persisted = if migrating {
                blob.and_then(|bytes| serde_json::from_slice::<PersistedSession>(&bytes).ok())
            } else {
                None
            };
            let mut rows = Some(rows);
            if let Some(entity) = this.sessions.get(&session_id).cloned() {
                entity.update(cx, |session, cx| {
                    let entries = if let Some(rows) =
                        rows.take().filter(|r| !r.is_empty())
                    {
                        entries_from_rows(rows)
                    } else {
                        let (cold_entries, created_ms) =
                            cold_entries_from_persisted(persisted, cx);
                        crate::session_entry::rebuild_entries(
                            &cold_entries,
                            &[],
                            &created_ms,
                            0,
                            cx,
                        )
                    };
                    session.entries = entries;
                    session.init_change_seq_from_entries();
                    if migrating {
                        session.bump_epoch();
                    } else {
                        session.epoch = epoch as u64;
                    }
                    session.hydrating = false;
                    // Drives the session view's `cx.observe(&session)` →
                    // re-render → cold-list resize catch-up so the freshly
                    // loaded transcript paints.
                    cx.notify();
                });
                // Legacy → rows lazy migration (idempotent; guarded by
                // rows-empty). Blob kept until Task 5 removes it.
                if migrating {
                    this.persist_all_rows(session_id, cx);
                }
            }
            if !bg_rows.is_empty() {
                this.reconcile_background_agents_for(session_id, bg_rows, cx);
            }
            // Background shells are ephemeral across restarts — drop the stale
            // rows just like `hydrate_all_for_solution` does.
            if let Some(db) = this.persistence.clone() {
                let session_id = session_id.to_string();
                cx.background_spawn(async move {
                    db.delete_background_shells_for_session(session_id)
                        .await
                        .log_err();
                })
                .detach();
            }
        })
        .log_err();
    }

    pub fn close_session(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) -> Result<()> {
        // Flush the latest transcript and stop any in-flight turn while the
        // session is still live in `self.sessions`. The flush guarantees a
        // later "Reopen Closed Chat" restores the full conversation; the
        // cancel keeps the pooled subprocess from churning on a session the
        // user just dismissed. Both must run before the `remove` below.
        self.persist_all_rows(id, cx);
        if let Some(entity) = self.sessions.get(&id)
            && matches!(entity.read(cx).state, SessionState::Running { .. })
        {
            self.cancel_turn(id, cx).log_err();
        }
        let removed = self
            .sessions
            .remove(&id)
            .ok_or_else(|| anyhow!("unknown session {id}"))?;
        // If the session is being torn down with queued messages still
        // unflushed, surface them in the log — closing a tab silently
        // drops everything in `pending_messages` (no Stopped event ever
        // fires for the torn-down thread).
        let session_read = removed.read(cx);
        if !session_read.pending_messages.is_empty() {
            let previews: Vec<String> = session_read
                .pending_messages
                .iter()
                .map(|b| queue::summarize_blocks_for_log(&b.blocks))
                .collect();
            log::warn!(
                target: "solution_agent::queue",
                "session={id} dropped {} queued bundle(s) on close_session — content: [{}]",
                session_read.pending_messages.len(),
                previews.join(" | "),
            );
        }
        let solution_id = session_read.solution_id.clone();
        if let Some(list) = self.by_solution.get_mut(&solution_id) {
            list.retain(|sid| *sid != id);
        }
        // Drop any per-entry update throttles for the closed session;
        // each holds a live debounce `Task`, so leaving them would leak
        // for the process lifetime (the throttle is only otherwise
        // removed when its own timer fires against a still-open session).
        self.entry_update_throttles.retain(|(sid, _), _| *sid != id);
        // Soft-close: keep the persisted blob so downstream tooling
        // (MCP read_session_history, future "View archived sessions"
        // UI, etc.) can still read the transcript. Hard-delete only
        // happens when the whole solution is removed via
        // `delete_for_solution`.
        if let Some(db) = &self.persistence {
            db.mark_closed(id, Some(Utc::now())).detach_and_log_err(cx);
        }
        cx.emit(SolutionAgentStoreEvent::SessionClosed(id));
        // Emit sequenced workspace notification so remote clients can
        // drop the session from their in-memory maps immediately.
        // `solution_id` was captured above while `session_read` was live
        // (before the entity was removed from `self.sessions`).
        // Guard with `try_global` so test contexts that don't install the
        // MCP layer don't panic.
        if let Some(coord) = editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx) {
            coord.emit_sequenced(
                cx,
                "workspace.session_deleted",
                serde_json::json!({
                    "solution_id": solution_id.as_str(),
                    "session_id": id.to_string(),
                }),
            );
        }
        cx.notify();
        Ok(())
    }

    /// Update the user-visible title of a session and persist the change
    /// (best-effort). Emits `SessionTitleChanged` so the navigator
    /// re-renders the row immediately.
    pub fn rename_session(
        &mut self,
        session_id: SolutionSessionId,
        title: SharedString,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let session = self
            .sessions
            .get(&session_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown session {session_id}"))?;
        session.update(cx, |s, _| s.title = title.clone());
        // Reuse `persist_session_row` so preview + token columns get
        // populated from the live thread instead of being NULL'd by this
        // title-only write path.
        self.persist_session_row(session_id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionTitleChanged(session_id));
        cx.notify();
        Ok(())
    }

    /// Restart the agent backing `session_id`: drop the pool entry so the
    /// next `create_session` call forces a fresh subprocess spawn, close
    /// the existing session, and open a new one against the cached project.
    /// v1 does not replay history — the new session starts empty (deferred
    /// per Phase-5 spec "Open implementation questions" item 5).
    ///
    /// Returns the freshly minted `SolutionSessionId` so callers can
    /// reattach navigator focus to it.
    pub fn restart_agent(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<SolutionSessionId>> {
        let Some(session) = self.sessions.get(&session_id).cloned() else {
            return Task::ready(Err(anyhow!("unknown session {session_id}")));
        };
        let (solution_id, agent_id, project, previous_cwd, previous_model, previous_effort) = {
            let s = session.read(cx);
            let project = match s.project.clone() {
                Some(project) => project,
                None => {
                    return Task::ready(Err(anyhow!(
                        "session {session_id} has no cached project — was it created via \
                         register_prebuilt_session?"
                    )));
                }
            };
            // Preserve the session's working directory across restart. Without
            // this the fresh session falls back to `solution.root` (the
            // `create_session` default), silently relocating a member-project
            // session — for the user that looks like "claude lost the project
            // root after I clicked Restart". Empty cwd is the legacy-row
            // marker meaning "fall back to solution.root"; pass `None` in
            // that case so `create_session_with_cwd` takes its own default.
            let cwd_override = if s.cwd.as_os_str().is_empty() {
                None
            } else {
                Some(s.cwd.clone())
            };
            (
                s.solution_id.clone(),
                s.agent_id.clone(),
                project,
                cwd_override,
                s.desired_model.clone(),
                s.desired_effort.clone(),
            )
        };
        let pair = (solution_id.clone(), agent_id.clone());
        {
            let mut pool = self.pool.lock();
            pool.remove(&pair);
        }
        // Mark the old session as restarting so the UI can show feedback
        // before the new session is registered.
        session.update(cx, |s, _| {
            s.state = SessionState::Errored(SharedString::from("restarting…"));
        });
        self.mark_state_changed(session_id, cx);
        // Best-effort close of the old session; we still spawn the new
        // one even if removal fails so the user isn't stranded.
        if let Err(err) = self.close_session(session_id, cx) {
            log::warn!("restart_agent: close_session({session_id}) failed: {err:?}");
        }
        let create_task = self.create_session_with_cwd(
            solution_id,
            agent_id,
            project,
            previous_cwd,
            previous_model,
            previous_effort,
            cx,
        );
        cx.spawn(async move |_this, _cx: &mut AsyncApp| create_task.await)
    }

    /// In-place context rotation: drop the current AcpThread, spawn a
    /// fresh ACP-level session against the SAME pooled connection, and
    /// graft it onto the existing `SolutionSession`. The user-facing
    /// `SolutionSessionId` and tab identity stay stable so dump
    /// directories from successive compacts cluster under one
    /// `<root>/.agents/<sid>/` tree, distinguishable only by the
    /// `context_count` (= which rotation).
    ///
    /// Different from `restart_agent` in two ways:
    ///   1. Keeps `SolutionSessionId` (restart_agent mints a fresh
    ///      one because its goal is "this session is broken — please
    ///      give me a clean slate" while rotate's goal is "same
    ///      conversation, just freed up the context window").
    ///   2. Reuses the same pooled subprocess (restart_agent drops
    ///      the pool entry to force a subprocess respawn).
    pub fn rotate_context(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<u32>> {
        let Some(session_entity) = self.sessions.get(&session_id).cloned() else {
            return Task::ready(Err(anyhow!("unknown session {session_id}")));
        };
        let (solution_id, agent_id, project, current_count, session_cwd) = {
            let s = session_entity.read(cx);
            let project = match s.project.clone() {
                Some(project) => project,
                None => {
                    return Task::ready(Err(anyhow!(
                        "session {session_id} has no cached project — rotate_context not supported \
                         for prebuilt test sessions"
                    )));
                }
            };
            (
                s.solution_id.clone(),
                s.agent_id.clone(),
                project,
                s.context_count,
                s.cwd.clone(),
            )
        };
        let pair = (solution_id.clone(), agent_id);

        cx.spawn(async move |this, cx: &mut AsyncApp| {
            // Resolve the live Solution so `connection.new_session`
            // gets a real cwd.
            let solution = cx.update(|cx| {
                SolutionStore::try_global(cx)
                    .ok_or_else(|| anyhow!("SolutionStore global is not initialised"))
                    .and_then(|store| {
                        store
                            .read(cx)
                            .solutions()
                            .iter()
                            .find(|s| s.id == solution_id)
                            .cloned()
                            .ok_or_else(|| anyhow!("solution {:?} not found", solution_id))
                    })
            })?;
            let (connection_task, acp_meta) = this.update(cx, |store, cx| {
                let task =
                    store.get_or_spawn_connection(pair.clone(), &solution, project.clone(), cx);
                let meta = store.build_session_meta(&pair.1, &solution, Some(session_id), None, cx);
                (task, meta)
            })?;
            let connection = connection_task.await?;
            // Preserve the session's per-tab working directory across
            // /compact. Without this the rotated thread would be created
            // with cwd=solution.root, so the agent's bash tool — which
            // inherits NewSessionRequest.cwd as its "Primary working
            // directory" — would silently switch from the member subdir
            // (e.g. `voxelcraft`) to the solution root after compaction
            // and then fail commands that depend on `Cargo.toml` /
            // `.git` being present.
            let work_dir = if session_cwd.as_os_str().is_empty() {
                solution.root.clone()
            } else {
                session_cwd.clone()
            };
            let work_dirs =
                util::path_list::PathList::new(&[work_dir.to_string_lossy().into_owned()]);
            let new_thread_task = cx.update(|cx| {
                connection
                    .clone()
                    .new_session_with_meta(project.clone(), work_dirs, acp_meta, cx)
            });
            let new_thread = new_thread_task.await?;

            let new_count = this.update(cx, |store, cx| {
                let new_acp_session_id = new_thread.read(cx).session_id().clone();
                let new_count = current_count.saturating_add(1);
                session_entity.update(cx, |s, cx| {
                    s.acp_session_id = new_acp_session_id;
                    s.context_count = new_count;
                    s.state = SessionState::Idle;
                    s.last_activity_at = Utc::now();
                    // Status-row meter falls back to `cached_total_tokens`
                    // when the live thread has no `token_usage` yet (the
                    // freshly-spawned thread does not). Without a reset,
                    // the meter would keep reading the pre-rotation count
                    // until the agent emits its first `TokenUsageUpdated`
                    // — confusing right after a context rotation. Same
                    // story for `last_turn_duration` (the "Done in Xs"
                    // hint should not survive past the rotation).
                    s.cached_total_tokens = None;
                    s.last_turn_duration = None;
                    s.entries.clear();
                    s.bump_epoch();
                    // `set_acp_thread` emits ThreadReplaced + notify;
                    // last so SessionView re-attaches against a fully
                    // updated session struct.
                    s.set_acp_thread(Some(new_thread.clone()), cx);
                });
                // Re-subscribe to the new AcpThread's event stream.
                // Dropping the old subscription unhooks us from the
                // dead thread automatically.
                let new_sub = store.subscribe_to_session(session_id, new_thread, cx);
                session_entity.update(cx, |s, _| s._acp_subscription = Some(new_sub));
                store.persist_session_row(session_id, cx);
                // /compact cleared+rebuilt `entries` and bumped the epoch above.
                // Rewrite the rows wholesale (deleting the now-stale pre-rotation
                // idx>0 rows) so the next cold load doesn't see new idx 0 + stale
                // idx 1..N. Targeted upserts alone would leak the old rows.
                store.persist_all_rows(session_id, cx);
                store.mark_state_changed(session_id, cx);
                cx.emit(SolutionAgentStoreEvent::SessionContextReset {
                    id: session_id,
                    context_count: new_count,
                });
                cx.notify();
                new_count
            })?;

            Ok(new_count)
        })
    }

    /// Reset the session's conversation context: drop the current
    /// `AcpThread` and spawn a fresh ACP-level session under the same
    /// `SolutionSessionId` and pooled subprocess.
    ///
    /// Different from [`rotate_context`](Self::rotate_context) in that
    /// `context_count` is left untouched (no `c<N>` directory bump) —
    /// this is the path wired to the user-facing `/clear` slash command,
    /// where the intent is "wipe this conversation, keep the tab"
    /// rather than "archive a long-running conversation as a numbered
    /// rotation". Agent-agnostic: nothing is forwarded to the agent
    /// subprocess; the new ACP session has zero history by construction.
    ///
    /// Returns the same `SolutionSessionId` for caller convenience (so
    /// the call site can chain "reset then dispatch follow-up" without
    /// re-plumbing the id).
    pub fn reset_context(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<SolutionSessionId>> {
        let Some(session_entity) = self.sessions.get(&session_id).cloned() else {
            return Task::ready(Err(anyhow!("unknown session {session_id}")));
        };
        // `project` is None for a COLD session (loaded from the DB, never
        // promoted to live this run) — the common case for `/clear` on a
        // session whose conversation was generated in a previous editor
        // run. Rather than bail, we resolve a headless project from the
        // solution below (same fallback the cold→live auto-wake path uses
        // in `queue::send_message_blocks_with_wake`), so reset works on
        // cold sessions too.
        let (solution_id, agent_id, cached_project, session_cwd) = {
            let s = session_entity.read(cx);
            (
                s.solution_id.clone(),
                s.agent_id.clone(),
                s.project.clone(),
                s.cwd.clone(),
            )
        };
        let pair = (solution_id.clone(), agent_id);

        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let solution = cx.update(|cx| {
                SolutionStore::try_global(cx)
                    .ok_or_else(|| anyhow!("SolutionStore global is not initialised"))
                    .and_then(|store| {
                        store
                            .read(cx)
                            .solutions()
                            .iter()
                            .find(|s| s.id == solution_id)
                            .cloned()
                            .ok_or_else(|| anyhow!("solution {:?} not found", solution_id))
                    })
            })?;
            let project = match cached_project {
                Some(project) => project,
                None => {
                    let solution = solution.clone();
                    cx.update(move |cx| {
                        SolutionAgentStore::make_headless_project_for_solution(&solution, cx)
                    })?
                }
            };
            let (connection_task, acp_meta) = this.update(cx, |store, cx| {
                let task =
                    store.get_or_spawn_connection(pair.clone(), &solution, project.clone(), cx);
                let meta = store.build_session_meta(&pair.1, &solution, Some(session_id), None, cx);
                (task, meta)
            })?;
            let connection = connection_task.await?;
            // Preserve the session's per-tab working directory across
            // /clear. Same reason as `rotate_context` above: the rotated
            // thread otherwise inherits cwd=solution.root and the agent's
            // bash tool silently switches away from the member subdir
            // the tab was bound to.
            let work_dir = if session_cwd.as_os_str().is_empty() {
                solution.root.clone()
            } else {
                session_cwd.clone()
            };
            let work_dirs =
                util::path_list::PathList::new(&[work_dir.to_string_lossy().into_owned()]);
            let new_thread_task = cx.update(|cx| {
                connection
                    .clone()
                    .new_session_with_meta(project.clone(), work_dirs, acp_meta, cx)
            });
            let new_thread = new_thread_task.await?;

            this.update(cx, |store, cx| {
                let new_acp_session_id = new_thread.read(cx).session_id().clone();
                let had_pending = session_entity.update(cx, |s, cx| {
                    let had_pending = !s.pending_messages.is_empty();
                    if had_pending {
                        // `/clear` wipes the session's conversation —
                        // queued follow-ups are tied to the OLD context
                        // and don't apply to a freshly-empty thread, so
                        // discard. WARN log so post-mortem of "I typed
                        // a follow-up then hit /clear and lost it" is
                        // recoverable from the log.
                        let previews: Vec<String> = s
                            .pending_messages
                            .iter()
                            .map(|b| queue::summarize_blocks_for_log(&b.blocks))
                            .collect();
                        log::warn!(
                            target: "solution_agent::queue",
                            "session={session_id} dropped {} queued bundle(s) on /clear (reset_context) — content: [{}]",
                            s.pending_messages.len(),
                            previews.join(" | "),
                        );
                    }
                    s.acp_session_id = new_acp_session_id;
                    s.state = SessionState::Idle;
                    s.last_activity_at = Utc::now();
                    s.pending_messages.clear();
                    s.flush_after_cancel = false;
                    // Status-row meter falls back to `cached_total_tokens`
                    // when the live thread has no `token_usage` yet — the
                    // freshly-spawned thread does not. Without a reset
                    // here the meter would keep reading the pre-`/clear`
                    // count (the bug this whole change exists to fix).
                    // `last_turn_duration` is cleared for the same reason
                    // — "Done in Xs" must not survive a context wipe.
                    s.cached_total_tokens = None;
                    s.last_turn_duration = None;
                    s.entries.clear();
                    s.bump_epoch();
                    // Cache the (possibly freshly-built headless) project so
                    // a subsequent reset/restart on this now-live session
                    // doesn't have to rebuild it.
                    s.project = Some(project.clone());
                    // `set_acp_thread` emits ThreadReplaced + notify;
                    // last so SessionView re-attaches against a fully
                    // wiped session struct.
                    s.set_acp_thread(Some(new_thread.clone()), cx);
                    had_pending
                });
                let new_sub = store.subscribe_to_session(session_id, new_thread, cx);
                session_entity.update(cx, |s, _| s._acp_subscription = Some(new_sub));
                store.persist_session_row(session_id, cx);
                // /clear cleared `entries` and bumped the epoch above. Rewrite
                // the rows wholesale (here that deletes ALL rows + saves the
                // bumped epoch) so the next cold load doesn't replay the stale
                // pre-clear transcript. Targeted upserts can't delete; this must
                // run on the empty-entries clear path.
                store.persist_all_rows(session_id, cx);
                // `reset_context` does not bump `context_count` (only
                // `rotate_context` does), so read the current value to
                // forward as-is on the wire.
                let context_count = session_entity.read(cx).context_count;
                store.mark_state_changed(session_id, cx);
                cx.emit(SolutionAgentStoreEvent::SessionContextReset {
                    id: session_id,
                    context_count,
                });
                if had_pending {
                    store.mark_queue_changed(session_id, cx);
                }
                cx.notify();
            })?;

            Ok(session_id)
        })
    }

    /// Returns a clone of the persistence handle if one was configured
    /// (i.e. the editor is running with a real on-disk DB, not the test
    /// in-memory mode). Used by MCP tools that need to read archived
    /// session blobs without re-hydrating the full session.
    pub fn persistence(&self) -> Option<Arc<crate::db::SolutionAgentDb>> {
        self.persistence.clone()
    }

    /// Persists the tab strip's open-session order for `solution_id`.
    /// Sessions in `ordered_ids` get `tab_order = 0..N`; everything else
    /// for the solution is set to `tab_order = NULL`. Called from the
    /// navigator on reorder, open, and close so the strip survives an
    /// editor restart.
    pub fn persist_tab_order(
        &self,
        solution_id: SolutionId,
        ordered_ids: Vec<SolutionSessionId>,
        cx: &mut Context<Self>,
    ) {
        // Capture the OLD set of in-strip session ids (tab_order.is_some())
        // BEFORE the apply mutates in-memory state.
        let old_set: std::collections::HashSet<SolutionSessionId> = self
            .sessions
            .values()
            .filter_map(|entity| {
                let s = entity.read(cx);
                if s.solution_id == solution_id && s.tab_order.is_some() {
                    Some(s.id)
                } else {
                    None
                }
            })
            .collect();

        // Update the in-memory field first (synchronous, on the foreground
        // thread) so that `workspace.snapshot` sees the new strip state
        // immediately — before the async DB write completes.
        self.apply_tab_order_to_memory(&solution_id, &ordered_ids, cx);

        // Compute NEW set from the ordered_ids that were just applied.
        let new_set: std::collections::HashSet<SolutionSessionId> =
            ordered_ids.iter().cloned().collect();

        // Diff and emit one workspace.session_opened / workspace.session_closed
        // per actual transition so downstream clients stay in sync without a
        // full snapshot refresh. Guard with `try_global` so test contexts that
        // don't install the MCP layer don't panic.
        let opened_ids: Vec<SolutionSessionId> = new_set.difference(&old_set).copied().collect();
        let closed_ids: Vec<SolutionSessionId> = old_set.difference(&new_set).copied().collect();
        if let Some(coord) = editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx) {
            for opened_id in &opened_ids {
                if let Some(entity) = self.sessions.get(opened_id) {
                    let summary = entity.read_with(cx, |s, cx| crate::mcp::session_summary(s, cx));
                    coord.emit_sequenced(
                        cx,
                        "workspace.session_opened",
                        serde_json::json!({
                            "solution_id": solution_id.as_str(),
                            "session": summary,
                        }),
                    );
                }
            }
            for closed_id in &closed_ids {
                coord.emit_sequenced(
                    cx,
                    "workspace.session_closed",
                    serde_json::json!({
                        "solution_id": solution_id.as_str(),
                        "session_id": closed_id.to_string(),
                    }),
                );
            }
        }

        // Local fan-out: the ConsolePanel observes this to add / remove the
        // actual tab on the desktop strip in response to mutations driven
        // from outside the panel (notably the wire-side
        // `workspace.{open,close}_session` RPCs from mobile clients).
        // Always emit, even when both lists are empty (a pure reorder) —
        // future consumers may want to react to that too; current
        // `ConsolePanel` subscriber filters out the empty case.
        cx.emit(SolutionAgentStoreEvent::TabsChanged {
            solution_id: solution_id.clone(),
            opened: opened_ids,
            closed: closed_ids,
        });

        let Some(db) = self.persistence.clone() else {
            return;
        };
        cx.background_spawn(async move {
            db.update_tab_orders(solution_id, ordered_ids)
                .await
                .log_err();
        })
        .detach();
    }

    /// Pin `session_id` into its solution's tab strip (the open-set) if it
    /// is not already there, appending it at the end of the current order.
    /// Routes through [`persist_tab_order`], so it emits the
    /// `workspace.session_opened` wire delta + the local `TabsChanged`
    /// fan-out and persists the new order — exactly what makes the session
    /// appear on the desktop ConsolePanel strip and the mobile workspace
    /// mirror. Idempotent: a no-op when the session is unknown or already
    /// pinned.
    ///
    /// This is the one definition of "open a session". Both the
    /// create-implies-open path ([`create_session_with_parent`]) and the
    /// wire `workspace.open_session` RPC call it, so "create" and "open"
    /// can no longer diverge into doing different things.
    pub fn open_session_in_strip(&mut self, session_id: SolutionSessionId, cx: &mut Context<Self>) {
        let Some(entity) = self.sessions.get(&session_id) else {
            return;
        };
        let (solution_id, already_pinned) = {
            let s = entity.read(cx);
            (s.solution_id.clone(), s.tab_order.is_some())
        };
        if already_pinned {
            return;
        }
        let mut pinned: Vec<(SolutionSessionId, i64)> = self
            .sessions
            .values()
            .filter_map(|entity| {
                let s = entity.read(cx);
                match s.tab_order {
                    Some(order) if s.solution_id == solution_id => Some((s.id, order)),
                    _ => None,
                }
            })
            .collect();
        pinned.sort_by_key(|(_, order)| *order);
        let mut ordered: Vec<SolutionSessionId> = pinned.into_iter().map(|(id, _)| id).collect();
        ordered.push(session_id);
        self.persist_tab_order(solution_id, ordered, cx);
    }

    /// Metadata for the solution's explicitly-closed sessions (`closed_at`
    /// set), most-recently-active first, top-level only (subagent rows
    /// excluded). Backs the "Reopen Closed Chat" picker — each row carries
    /// title / token total / last activity so the user can tell heavy and
    /// recent sessions apart. Reads straight from the DB because closed
    /// sessions are not held in memory (`close_session` evicts them).
    pub fn list_closed_sessions(
        &self,
        solution_id: SolutionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<SolutionSessionMetadata>>> {
        let Some(db) = self.persistence.clone() else {
            return Task::ready(Ok(Vec::new()));
        };
        cx.background_spawn(async move {
            let closed: HashSet<SolutionSessionId> = db
                .list_closed_session_ids(solution_id.clone())
                .await?
                .into_iter()
                .collect();
            if closed.is_empty() {
                return Ok(Vec::new());
            }
            // `list_for_solution` is already ordered by `last_activity_at`
            // DESC, so the filtered result keeps that ordering.
            let metas = db.list_for_solution(solution_id).await?;
            Ok(metas
                .into_iter()
                .filter(|m| closed.contains(&m.id) && m.parent_session_id.is_none())
                .collect())
        })
    }

    /// Bring a previously-closed session back into the strip. Clears the
    /// `closed_at` marker so `hydrate_all_for_solution` stops skipping it,
    /// AND clears the stale `tab_order` (see [`SolutionAgentDb::reopen_session`])
    /// so the freshly-hydrated session is not mistaken for an already-pinned
    /// tab — without that, `open_session_in_strip` early-returns on its
    /// `already_pinned` guard and the tab never reappears. Hydrates it into
    /// memory as a cold tab, then pins it. Reuses the existing restore + pin
    /// machinery rather than reconstructing the session inline.
    pub fn reopen_closed_session(
        &mut self,
        id: SolutionSessionId,
        solution_id: SolutionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(db) = self.persistence.clone() else {
            return Task::ready(Err(anyhow!("no persistence backend")));
        };
        cx.spawn(async move |this, cx| {
            db.reopen_session(id).await?;
            let hydrate = this.update(cx, |this, cx| {
                this.hydrate_all_for_solution(solution_id.clone(), cx)
            })?;
            hydrate.await?;
            this.update(cx, |this, cx| this.open_session_in_strip(id, cx))?;
            Ok(())
        })
    }

    /// Update the in-memory `tab_order` field on every session that belongs to
    /// `solution_id`. Sessions whose id appears in `ordered_ids` receive their
    /// 0-based index; all others are cleared to `None` (tab closed / hidden).
    ///
    /// Must be called from the foreground thread (takes `cx` for entity access).
    fn apply_tab_order_to_memory(
        &self,
        solution_id: &SolutionId,
        ordered_ids: &[SolutionSessionId],
        cx: &mut Context<Self>,
    ) {
        for entity in self.sessions.values() {
            let entity = entity.clone();
            let belongs = entity.read(cx).solution_id == *solution_id;
            if !belongs {
                continue;
            }
            let id = entity.read(cx).id;
            let new_order = ordered_ids
                .iter()
                .position(|oid| *oid == id)
                .map(|i| i as i64);
            entity.update(cx, |s, _| s.tab_order = new_order);
        }
    }

    /// Phase 4 row tuple: `(idx, mod_seq, created_ms, subagent_id, payload)`
    /// in the casts `upsert_entry` expects. An empty `payload` signals a serde
    /// failure in `to_payload()` — callers MUST skip persisting it.
    fn entry_row_tuple(
        global_idx: usize,
        entry: &crate::session_entry::SessionEntry,
    ) -> (i64, i64, i64, Option<String>, Vec<u8>) {
        (
            global_idx as i64,
            entry.mod_seq as i64,
            entry.created_ms,
            entry.subagent_id.as_ref().map(|s| s.to_string()),
            entry.to_payload(),
        )
    }

    /// Flush the WHOLE transcript as rows: upsert every current entry, delete
    /// any stale trailing rows beyond `entries.len()`, and save the epoch.
    /// This is the path that handles clears/compactions and close — targeted
    /// upserts alone would leave orphaned idx>len rows that corrupt the next
    /// cold load. On empty `entries` it degrades to "delete all rows + save
    /// epoch".
    pub fn persist_all_rows(&self, session_id: SolutionSessionId, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        let Some(db) = self.persistence.clone() else {
            return;
        };
        cx.spawn(async move |_this, cx: &mut AsyncApp| {
            let (rows, len, epoch) = cx.update(|cx| {
                let s = session.read(cx);
                let rows: Vec<_> = s
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(idx, entry)| Self::entry_row_tuple(idx, entry))
                    .collect();
                (rows, s.entries.len() as i64, s.epoch as i64)
            });
            for (idx, mod_seq, created_ms, subagent_id, payload) in rows {
                if payload.is_empty() {
                    log::warn!(
                        target: "solution_agent::store",
                        "skipping empty-payload upsert for session={session_id} idx={idx}",
                    );
                    continue;
                }
                db.upsert_entry(session_id, idx, mod_seq, created_ms, subagent_id, payload)
                    .await
                    .log_err();
            }
            db.delete_entries_from(session_id, len).await.log_err();
            db.save_epoch(session_id, epoch).await.log_err();
        })
        .detach();
    }

    /// Upsert `entries[start_idx..]` (used by `NewEntry`, which can append more
    /// than one entry via gap-fill) + save the epoch.
    pub fn persist_upsert_range(
        &self,
        session_id: SolutionSessionId,
        start_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        let Some(db) = self.persistence.clone() else {
            return;
        };
        cx.spawn(async move |_this, cx: &mut AsyncApp| {
            let (rows, epoch) = cx.update(|cx| {
                let s = session.read(cx);
                let rows: Vec<_> = s
                    .entries
                    .iter()
                    .enumerate()
                    .skip(start_idx)
                    .map(|(idx, entry)| Self::entry_row_tuple(idx, entry))
                    .collect();
                (rows, s.epoch as i64)
            });
            for (idx, mod_seq, created_ms, subagent_id, payload) in rows {
                if payload.is_empty() {
                    log::warn!(
                        target: "solution_agent::store",
                        "skipping empty-payload upsert for session={session_id} idx={idx}",
                    );
                    continue;
                }
                db.upsert_entry(session_id, idx, mod_seq, created_ms, subagent_id, payload)
                    .await
                    .log_err();
            }
            db.save_epoch(session_id, epoch).await.log_err();
        })
        .detach();
    }

    /// Upsert exactly `entries[global_idx]` (used by `EntryUpdated`) + save the
    /// epoch.
    pub fn persist_upsert_entry(
        &self,
        session_id: SolutionSessionId,
        global_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        let Some(db) = self.persistence.clone() else {
            return;
        };
        cx.spawn(async move |_this, cx: &mut AsyncApp| {
            let row = cx.update(|cx| {
                let s = session.read(cx);
                let epoch = s.epoch as i64;
                let row = s
                    .entries
                    .get(global_idx)
                    .map(|entry| Self::entry_row_tuple(global_idx, entry));
                (row, epoch)
            });
            let (row, epoch) = row;
            if let Some((idx, mod_seq, created_ms, subagent_id, payload)) = row {
                if payload.is_empty() {
                    log::warn!(
                        target: "solution_agent::store",
                        "skipping empty-payload upsert for session={session_id} idx={idx}",
                    );
                } else {
                    db.upsert_entry(session_id, idx, mod_seq, created_ms, subagent_id, payload)
                        .await
                        .log_err();
                }
            }
            db.save_epoch(session_id, epoch).await.log_err();
        })
        .detach();
    }

    /// Delete rows `idx >= from_idx` (used by `EntriesRemoved`) + save the
    /// epoch.
    pub fn persist_delete_from(
        &self,
        session_id: SolutionSessionId,
        from_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        let Some(db) = self.persistence.clone() else {
            return;
        };
        cx.spawn(async move |_this, cx: &mut AsyncApp| {
            let epoch = cx.update(|cx| session.read(cx).epoch as i64);
            db.delete_entries_from(session_id, from_idx as i64)
                .await
                .log_err();
            db.save_epoch(session_id, epoch).await.log_err();
        })
        .detach();
    }

    /// Subscribe to a session's `AcpThread` event stream so that ACP-level
    /// state changes (turn completion, tool authorization, errors, etc.)
    /// translate into `SessionState` transitions on `SolutionSession`.
    /// Returns the `Subscription` — caller must store it on the session
    /// (in `_acp_subscription`) or it will drop and unsubscribe immediately.
    fn subscribe_to_session(
        &mut self,
        session_id: SolutionSessionId,
        acp_thread: Entity<acp_thread::AcpThread>,
        cx: &mut Context<Self>,
    ) -> Subscription {
        // Wire the native follow-up pull: at each hook the connection asks the
        // store for this session's queued follow-ups. The closure captures a
        // weak store handle so `claude_native` stays free of any
        // `solution_agent` dependency. First-write-wins inside `set_store_pull`,
        // so re-attaches (resume / new thread) are harmless no-ops.
        if let Some(connection) = acp_thread
            .read(cx)
            .connection()
            .clone()
            .downcast::<claude_native::ClaudeNativeConnection>()
        {
            let weak = cx.weak_entity();
            connection.set_store_pull(std::rc::Rc::new(
                move |acp_sid: &acp::SessionId,
                      agent_id: Option<&str>,
                      is_end_of_turn: bool,
                      cx: &mut AsyncApp| {
                    weak.update(cx, |store, cx| {
                        let session_id = store.session_id_for_acp(acp_sid, cx)?;
                        store.take_pending_for_delivery(session_id, agent_id, is_end_of_turn, cx)
                    })
                    .ok()
                    .flatten()
                },
            ));
        }

        cx.subscribe(&acp_thread, move |store, _thread, event, cx| {
            store.handle_acp_event(session_id, event, cx);
        })
    }

    /// Subagent-tab lifecycle hook. Inspects the entry at `entry_index` in
    /// the session's live `AcpThread` and:
    ///   * if it's a brand-new `Task`/`Agent` ToolCall in `InProgress` and
    ///     not already tracked → registers it on
    ///     `SolutionSession::active_subagents` (+ insertion-order vec) and
    ///     emits [`SolutionAgentStoreEvent::SessionSubagentsChanged`];
    ///   * if it's a tracked id whose status just flipped to a terminal
    ///     state (`Completed`/`Failed`/`Rejected`/`Canceled`) → removes it
    ///     and emits the same event.
    ///
    /// Any other shape (non-tool entry, non-Task tool, status still
    /// `InProgress`/`Pending` on an already-tracked id, terminal status on
    /// an unknown id) is a no-op and emits nothing. Map mutations are gated
    /// behind a structural check to keep `SessionSubagentsChanged` from
    /// firing on every chunk of a streaming Task subagent's body.
    ///
    /// The cold-thread branch is excluded: an entry only exists in a live
    /// `AcpThread`, so when the session is cold (`acp_thread()` is `None`)
    /// there is nothing to track yet. The next live attach will replay the
    /// in-flight tool calls through `NewEntry`, which re-enters this hook.
    fn apply_subagent_lifecycle(
        &mut self,
        session_id: SolutionSessionId,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(session_entity) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        // Capture the relevant ToolCall fields in a small read scope so we
        // can mutate the session entity right after without overlapping
        // borrows.
        struct Snapshot {
            id: SharedString,
            is_task_like: bool,
            is_in_progress: bool,
            is_terminal: bool,
            label_from_raw_input: Option<SharedString>,
            subagent_type: Option<String>,
            /// The tool's programmatic name (e.g. `"Task"`, `"Agent"`)
            /// captured so the post-lifecycle branch can dispatch on
            /// `eq_ignore_ascii_case("agent")` without re-borrowing the
            /// entry from the thread.
            tool_name: Option<String>,
            /// JSON-encoded `raw_output` payload (only meaningful for the
            /// terminal `Agent` branch — claude's managed-agent dispatcher
            /// stashes `agentId` + `output_file` here when the tool call
            /// completes). Empty for in-progress / non-Agent calls.
            raw_output_text: Option<String>,
            /// Raw tool-call input JSON, captured so the background-shell
            /// branch can read `run_in_background` + `command` without
            /// re-borrowing the entry. `None` when the tool call has no
            /// `raw_input`.
            raw_input: Option<serde_json::Value>,
        }
        let snapshot = {
            let session = session_entity.read(cx);
            let Some(thread) = session.acp_thread() else {
                return;
            };
            let thread_ref = thread.read(cx);
            let Some(entry) = thread_ref.entries().get(entry_index) else {
                return;
            };
            let acp_thread::AgentThreadEntry::ToolCall(call) = entry else {
                return;
            };
            let tool_name = call
                .tool_name
                .as_ref()
                .map(|s| s.as_ref())
                .unwrap_or_default();
            let is_task_like = matches!(tool_name, "Task" | "Agent");
            let is_in_progress = matches!(call.status, acp_thread::ToolCallStatus::InProgress);
            let is_terminal = matches!(
                call.status,
                acp_thread::ToolCallStatus::Completed
                    | acp_thread::ToolCallStatus::Failed
                    | acp_thread::ToolCallStatus::Rejected
                    | acp_thread::ToolCallStatus::Canceled
            );
            let (label_from_raw_input, subagent_type) = match call.raw_input.as_ref() {
                Some(raw) => {
                    let desc = raw
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(|s| SharedString::from(s.to_owned()));
                    let stype = raw
                        .get("subagent_type")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_owned());
                    (desc, stype)
                }
                None => (None, None),
            };
            let tool_name_owned = if tool_name.is_empty() {
                None
            } else {
                Some(tool_name.to_string())
            };
            let raw_output_text = call
                .raw_output
                .as_ref()
                .and_then(|v| serde_json::to_string(v).ok());
            Snapshot {
                id: SharedString::from(call.id.0.to_string()),
                is_task_like,
                is_in_progress,
                is_terminal,
                label_from_raw_input,
                subagent_type,
                tool_name: tool_name_owned,
                raw_output_text,
                raw_input: call.raw_input.clone(),
            }
        };

        // Background-shell registration (Tasks 7 + 9 of the Background
        // Shells Strip plan). claude's `Bash(run_in_background=true)` launches
        // a detached process that writes its combined stdout/stderr to an
        // on-disk `tasks/<id>.output` file; the path + short id are surfaced
        // in the launch announcement carried in the tool call's `raw_output`.
        //
        // This MUST run before the `is_task_like` early-return below: `Bash`
        // is not in the `Task | Agent` task-like set, so the gate would
        // otherwise skip it. We register the shell, persist it, arm the
        // per-session `tasks/` watcher, and do one inline tail to close the
        // launch→first-write race — then fall through to the early-return
        // (which fires for `Bash`, leaving the subagent-pill logic untouched).
        if snapshot.is_terminal
            && snapshot.tool_name.as_deref() == Some("Bash")
            && snapshot
                .raw_input
                .as_ref()
                .and_then(|v| v.get("run_in_background"))
                .and_then(|v| v.as_bool())
                == Some(true)
        {
            let raw_output_text = snapshot.raw_output_text.clone().unwrap_or_default();
            if let Some((shell_id, output_path)) =
                crate::background_shell::parse_bash_bg_launch(&raw_output_text)
            {
                let already = session_entity
                    .read(cx)
                    .background_shells
                    .contains_key(&shell_id);
                if !already {
                    // Command label: prefer `raw_input.command`, fall back to
                    // `raw_input.description`; truncate to 120 chars so a long
                    // pipeline doesn't blow out the strip.
                    let command_label: SharedString = snapshot
                        .raw_input
                        .as_ref()
                        .and_then(|v| {
                            v.get("command")
                                .or_else(|| v.get("description"))
                                .and_then(|c| c.as_str())
                        })
                        .map(|s| s.chars().take(120).collect::<String>())
                        .unwrap_or_default()
                        .into();
                    let registered_at = chrono::Utc::now();
                    let id_for_insert = shell_id.clone();
                    let path_for_insert = output_path.clone();
                    let command_for_insert = command_label.clone();
                    session_entity.update(cx, |s, _| {
                        s.background_shells.insert(
                            id_for_insert.clone(),
                            crate::background_shell::BackgroundShell {
                                id: id_for_insert.clone(),
                                command: command_for_insert,
                                output_path: path_for_insert,
                                registered_at,
                                latest: None,
                                last_offset: 0,
                                state: crate::background_shell::ShellRuntimeState::Running,
                            },
                        );
                        s.background_shell_order.push(id_for_insert);
                    });
                    cx.emit(SolutionAgentStoreEvent::SessionBackgroundShellsChanged(
                        session_id,
                    ));

                    // Persist to SQLite if the store has a backing DB. The
                    // in-memory test stores leave `persistence` as `None`.
                    if let Some(db) = self.persistence.clone() {
                        let row = crate::db::BackgroundShellRow {
                            solution_session_id: session_id.to_string(),
                            shell_id: shell_id.as_str().to_string(),
                            command: command_label.to_string(),
                            output_path: output_path.to_string_lossy().into_owned(),
                            registered_at_ms: registered_at.timestamp_millis(),
                            last_tail: None,
                            last_mtime_ms: None,
                            state_text: "running".to_string(),
                        };
                        cx.background_spawn(async move {
                            db.save_background_shell(row).await.log_err();
                        })
                        .detach();
                    }

                    // Arm the per-session watcher on the `tasks/` directory
                    // (the announcement path's parent). A session without a
                    // project skips the watcher — the row is still registered
                    // and the inline refresh below seeds the first snapshot.
                    if let (Some(fs), Some(tasks_dir)) = (
                        session_entity
                            .read(cx)
                            .project
                            .as_ref()
                            .map(|p| p.read(cx).fs().clone()),
                        output_path.parent().map(|p| p.to_path_buf()),
                    ) {
                        self.ensure_background_shell_watcher(session_id, fs, tasks_dir, cx);
                    }

                    // Close the launch→watcher-subscribe race: claude often
                    // has already written the first bytes by the time `Bash`
                    // returns, but `fs.watch` resolves on a background task.
                    self.refresh_background_shell_snapshot(session_id, shell_id, cx);
                }
            }
        }

        // `KillShell` terminal tool_call → mark the targeted background shell
        // `Killed`. claude emits a `KillShell` ToolCall (Execute kind) whose
        // `raw_input` carries the `shell_id`/`bash_id` of the shell to stop;
        // when it completes, the shell is dead. Like the `Bash(bg)` branch
        // above, this runs BEFORE the `is_task_like` early-return because
        // `KillShell` is not in the `Task | Agent` set.
        if snapshot.is_terminal && snapshot.tool_name.as_deref() == Some("KillShell") {
            if let Some(shell_id) = snapshot
                .raw_input
                .as_ref()
                .and_then(crate::background_shell::parse_kill_shell_input)
            {
                if session_entity
                    .read(cx)
                    .background_shells
                    .contains_key(&shell_id)
                {
                    self.mark_background_shell_state(
                        session_id,
                        shell_id,
                        crate::background_shell::ShellRuntimeState::Killed,
                        cx,
                    );
                }
            }
        }

        if !snapshot.is_task_like {
            return;
        }
        let id = snapshot.id;

        let changed = if snapshot.is_in_progress {
            // Defensive: a duplicate NewEntry for the same id (or an
            // InProgress→InProgress EntryUpdated as raw_input streams in) must
            // not re-insert or re-emit. Only the first observation registers
            // the tab.
            let already_tracked = session_entity.read(cx).active_subagents.contains_key(&id);
            if already_tracked {
                // Label is intentionally locked at first observation. Later
                // EntryUpdated events that finally fill in raw_input.description
                // are discarded here on purpose — otherwise a streamed tool_use
                // input would relabel the tab mid-flight and flicker the strip.
                false
            } else {
                let label = snapshot
                    .label_from_raw_input
                    .unwrap_or_else(|| label_fallback(&id, snapshot.subagent_type.as_deref()));
                let id_for_closure = id.clone();
                session_entity.update(cx, |s, _| {
                    s.active_subagents.insert(
                        id_for_closure.clone(),
                        SubagentTab {
                            label,
                            started_at: chrono::Utc::now(),
                        },
                    );
                    s.active_subagent_order.push(id_for_closure);
                });
                true
            }
        } else if snapshot.is_terminal {
            // Symmetric defensive guard: a terminal-status EntryUpdated on an
            // id we never registered (e.g. the InProgress event arrived after
            // a status flip on a cold→live transition) is a no-op.
            let tracked = session_entity.read(cx).active_subagents.contains_key(&id);
            if tracked {
                session_entity.update(cx, |s, _| {
                    s.active_subagents.remove(&id);
                    s.active_subagent_order
                        .retain(|tracked_id| tracked_id != &id);
                });
                true
            } else {
                false
            }
        } else {
            // Pending / WaitingForConfirmation transitions on a Task/Agent
            // tool call are not lifecycle signals — claude almost never goes
            // through these for subagents (they spawn directly into
            // InProgress), but be defensive in case future SDK shapes do.
            false
        };

        if changed {
            self.mark_subagents_changed(session_id, cx);
        }

        // Managed-agent registration (Task 8 of the Background Agents Strip
        // plan). claude_code's `Agent` tool is its async sub-agent dispatch;
        // when the call completes its `raw_output` carries `agentId: <hex>`
        // + `output_file: <path>.output` so we can tail the JSONL transcript
        // the worker is appending to. We register a `BackgroundAgent` for
        // every fresh announcement and spawn the per-session directory
        // watcher (idempotent — `ensure_background_agent_watcher` no-ops on
        // a duplicate call). The Task branch above already removed the
        // subagent pill, so the Agent dispatch briefly shows as an active
        // subagent and then transitions to a background-agent strip entry —
        // matches the pre-feature behaviour for `Task` and adds the strip
        // on top.
        if snapshot.is_terminal && tool_name_is_agent(snapshot.tool_name.as_deref()) {
            let raw_output_text = snapshot.raw_output_text.unwrap_or_default();
            if let Some((agent_id_str, output_file)) =
                crate::background_agent::parse_managed_agent_announcement(&raw_output_text)
            {
                let canonical =
                    std::fs::read_link(&output_file).unwrap_or_else(|_| output_file.clone());
                let id = crate::background_agent::BackgroundAgentId::new(agent_id_str);
                let already = session_entity.read(cx).background_agents.contains_key(&id);
                if !already {
                    let id_for_insert = id.clone();
                    let path_for_insert = canonical.clone();
                    session_entity.update(cx, |s, _| {
                        s.background_agents.insert(
                            id_for_insert.clone(),
                            crate::background_agent::BackgroundAgent {
                                id: id_for_insert.clone(),
                                jsonl_path: path_for_insert,
                                registered_at: chrono::Utc::now(),
                                latest: None,
                                last_offset: 0,
                            },
                        );
                        s.background_agent_order.push(id_for_insert);
                    });
                    cx.emit(SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(
                        session_id,
                    ));

                    // Persist to SQLite if the store has a backing DB.
                    // In-memory test stores leave `persistence` as `None`
                    // and rely on the in-RAM map only.
                    if let Some(db) = self.persistence.clone() {
                        let row = crate::db::BackgroundAgentRow {
                            solution_session_id: session_id.to_string(),
                            agent_id: id.as_str().to_string(),
                            jsonl_path: canonical.to_string_lossy().into_owned(),
                            registered_at_ms: chrono::Utc::now().timestamp_millis(),
                            last_seen_label: None,
                            last_mtime_ms: None,
                            stop_reason: None,
                        };
                        cx.background_spawn(async move {
                            db.save_background_agent(row).await.log_err();
                        })
                        .detach();
                    }

                    // The watcher needs a `fs::Fs` handle. `SolutionAgentStore`
                    // has no `fs` field; source it from the session's project
                    // (most live sessions have one). A session without a
                    // project just skips the watcher — the row is still
                    // registered and the UI can render the pill, but live
                    // tailing waits for a project attach.
                    if let Some(fs) = session_entity
                        .read(cx)
                        .project
                        .as_ref()
                        .map(|p| p.read(cx).fs().clone())
                    {
                        self.ensure_background_agent_watcher(session_id, fs, cx);
                    }

                    // Close the registration→watcher-subscribe race window:
                    // claude writes the first JSONL line nearly instantly
                    // after `Agent` returns, but `fs.watch` resolves on a
                    // background task — so without an inline refresh the
                    // first snapshot can be missed entirely and the pill
                    // would sit at the default `Generating…` until the
                    // sub-agent's next write.
                    self.refresh_background_agent_snapshot(session_id, id, cx);
                }
            }
        }
    }

    /// Spawn (idempotently) a per-session watcher on the
    /// `~/.claude/projects/<encoded-cwd>/<session-id>/subagents/`
    /// directory. Each `PathEvent` on an `agent-<id>.jsonl` filename
    /// triggers a `refresh_background_agent_snapshot` for the matching
    /// tracked `BackgroundAgent`. The watcher task lives in
    /// `background_agent_watchers` keyed by `session_id` — drop the
    /// entry (or drop the store) to cancel.
    ///
    /// Called from the tool-call handler (Task 8) when claude announces
    /// a managed agent. Safe to call repeatedly: a second call for the
    /// same session is a no-op.
    pub(crate) fn ensure_background_agent_watcher(
        &mut self,
        session_id: SolutionSessionId,
        fs: Arc<dyn fs::Fs>,
        cx: &mut Context<Self>,
    ) {
        if self.background_agent_watchers.contains_key(&session_id) {
            return;
        }
        let Some(session) = self.session(session_id) else {
            return;
        };
        let acp_session_id = session.read(cx).acp_session_id.clone();
        let cwd = session.read(cx).cwd.clone();
        let subagents_dir = match background_agent_dir_for(&cwd, acp_session_id.0.as_ref()) {
            Some(p) => p,
            None => {
                log::warn!(
                    "background_agents: cannot resolve subagents dir for session {}",
                    session_id
                );
                return;
            }
        };
        let task = cx.spawn(async move |this, cx| {
            let (mut stream, _watcher) = fs
                .watch(&subagents_dir, std::time::Duration::from_millis(200))
                .await;
            use futures::StreamExt;
            while let Some(events) = stream.next().await {
                for event in events {
                    let Some(name) = event.path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if !name.starts_with("agent-") || !name.ends_with(".jsonl") {
                        continue;
                    }
                    let agent_id_str = name
                        .trim_start_matches("agent-")
                        .trim_end_matches(".jsonl")
                        .to_string();
                    // Dropping the Result is the established cancellation
                    // signal: if the store entity is gone, the watcher
                    // task is about to be dropped anyway.
                    let _ = this.update(cx, |this, cx| {
                        this.refresh_background_agent_snapshot(
                            session_id,
                            crate::background_agent::BackgroundAgentId::new(agent_id_str),
                            cx,
                        );
                    });
                }
            }
        });
        self.background_agent_watchers.insert(session_id, task);
    }

    /// Tail the JSONL file for `agent_id` on `session_id`, parse the
    /// last line into a [`BackgroundAgentSnapshot`], write it to
    /// `BackgroundAgent::latest`, and emit
    /// [`SolutionAgentStoreEvent::SessionBackgroundAgentsChanged`] iff
    /// the snapshot was actually stored. No-op when the session has
    /// gone away, the agent isn't tracked anymore, the file can't be
    /// read, or it has no usable last line.
    pub(crate) fn refresh_background_agent_snapshot(
        &mut self,
        session_id: SolutionSessionId,
        agent_id: crate::background_agent::BackgroundAgentId,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let Some((jsonl_path, since_offset)) = session
            .read(cx)
            .background_agents
            .get(&agent_id)
            .map(|ba| (ba.jsonl_path.clone(), ba.last_offset))
        else {
            return;
        };
        let tail = match crate::background_agent::tail_jsonl(&jsonl_path, since_offset) {
            Ok(t) => t,
            Err(_) => return,
        };
        let new_offset = tail.new_offset;
        let snapshot = tail.last_line.as_ref().map(|line| {
            let mut snap = crate::background_agent::parse_jsonl_snapshot(line);
            snap.mtime = tail.mtime;
            snap
        });
        let mut changed = false;
        session.update(cx, |s, _| {
            if let Some(ba) = s.background_agents.get_mut(&agent_id) {
                // Always advance the offset (or rewind on truncation —
                // `tail_jsonl` already handled the reset). Only update
                // `latest` when this tail actually yielded a new line;
                // otherwise the previously-known snapshot remains the
                // user-visible state.
                ba.last_offset = new_offset;
                if let Some(snap) = snapshot {
                    ba.latest = Some(snap);
                    changed = true;
                }
            }
        });
        if changed {
            cx.emit(SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(
                session_id,
            ));
        }
    }

    /// Spawn (idempotently) a per-session watcher on the `tasks/`
    /// directory that hosts the background-shell `.output` files (passed
    /// in as `tasks_dir` — it's the parent of the announcement path; we do
    /// NOT re-derive it from cwd the way the managed-agent watcher does,
    /// since the layout is `/tmp/claude-<uid>/<encoded-cwd>/<ses>/tasks/`
    /// rather than `~/.claude/...`). Each `PathEvent` on a `<id>.output`
    /// filename triggers a `refresh_background_shell_snapshot` for the
    /// matching tracked `BackgroundShell`. The watcher task lives in
    /// `background_shell_watchers` keyed by `session_id` — drop the entry
    /// (or drop the store) to cancel. Safe to call repeatedly: a second
    /// call for the same session is a no-op.
    pub(crate) fn ensure_background_shell_watcher(
        &mut self,
        session_id: SolutionSessionId,
        fs: Arc<dyn fs::Fs>,
        tasks_dir: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.background_shell_watchers.contains_key(&session_id) {
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            let (mut stream, _watcher) = fs
                .watch(&tasks_dir, std::time::Duration::from_millis(200))
                .await;
            use futures::StreamExt;
            while let Some(events) = stream.next().await {
                for event in events {
                    let Some(name) = event.path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if !name.ends_with(".output") {
                        continue;
                    }
                    let shell_id_str = name.trim_end_matches(".output").to_string();
                    // Dropping the Result is the established cancellation
                    // signal: if the store entity is gone, the watcher
                    // task is about to be dropped anyway.
                    let _ = this.update(cx, |this, cx| {
                        this.refresh_background_shell_snapshot(
                            session_id,
                            crate::background_shell::BackgroundShellId::new(shell_id_str),
                            cx,
                        );
                    });
                }
            }
        });
        self.background_shell_watchers.insert(session_id, task);
    }

    /// Live-tail the `.output` file for `shell_id` on `session_id`, write
    /// the trailing window into `BackgroundShell::latest`, and emit
    /// [`SolutionAgentStoreEvent::SessionBackgroundShellsChanged`] iff the
    /// file actually advanced. Unlike the managed-agent snapshot (last
    /// JSONL line only), this reads the full trailing window for display —
    /// so we pass `0` to `tail_output` and let its 64 KiB cap bound the
    /// read. No-op when the session is gone, the shell isn't tracked, or
    /// the file can't be read yet (missing file → "no snapshot yet", not a
    /// failure). Does NOT touch `state`: registration sets `Running` and
    /// the terminal-state transition (Task 8) owns the rest.
    pub(crate) fn refresh_background_shell_snapshot(
        &mut self,
        session_id: SolutionSessionId,
        shell_id: crate::background_shell::BackgroundShellId,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let Some((output_path, stored_last_offset)) = session
            .read(cx)
            .background_shells
            .get(&shell_id)
            .map(|sh| (sh.output_path.clone(), sh.last_offset))
        else {
            return;
        };
        // Always read the full trailing window (offset 0) for display; the
        // `changed` decision below uses the file length, not this read start.
        let tail = match crate::background_shell::tail_output(&output_path, 0) {
            Ok(t) => t,
            Err(_) => return,
        };
        let new_offset = tail.new_offset;
        // The file advanced iff its end moved past what we last recorded.
        // A first non-empty read (stored offset 0, file non-empty) also
        // counts as changed.
        let changed = new_offset != stored_last_offset;
        let tail_text = tail.text;
        let tail_mtime = tail.mtime;
        session.update(cx, |s, _| {
            if let Some(sh) = s.background_shells.get_mut(&shell_id) {
                sh.last_offset = new_offset;
                if !tail_text.is_empty() {
                    sh.latest = Some(crate::background_shell::BackgroundShellSnapshot {
                        mtime: tail_mtime,
                        output_tail: tail_text.into(),
                    });
                }
            }
        });
        if changed {
            cx.emit(SolutionAgentStoreEvent::SessionBackgroundShellsChanged(
                session_id,
            ));
        }
    }

    /// Flip a tracked background shell's [`ShellRuntimeState`] (terminal
    /// signal handler). Mutates the in-memory map entry, emits
    /// [`SolutionAgentStoreEvent::SessionBackgroundShellsChanged`], and
    /// fire-and-forget upserts the row's `state_text` to SQLite (rebuilt
    /// from the in-memory shell). No-op when the session or the shell id is
    /// no longer tracked. Used by both terminal signals: the `KillShell`
    /// tool_call (→ `Killed`) and the `<task-notification>` user message
    /// (→ `Exited(code)`).
    fn mark_background_shell_state(
        &mut self,
        session_id: SolutionSessionId,
        shell_id: crate::background_shell::BackgroundShellId,
        new_state: crate::background_shell::ShellRuntimeState,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        // Capture the row fields under a short read scope, mutating the state
        // in the same `update` so the persisted row matches the in-memory one.
        let row = session.update(cx, |s, _| {
            let shell = s.background_shells.get_mut(&shell_id)?;
            if shell.state == new_state {
                // Idempotent: a duplicate terminal signal (e.g. a re-observed
                // KillShell on a cold→live replay) must not re-emit.
                return None;
            }
            shell.state = new_state.clone();
            Some(crate::db::BackgroundShellRow {
                solution_session_id: session_id.to_string(),
                shell_id: shell.id.as_str().to_string(),
                command: shell.command.to_string(),
                output_path: shell.output_path.to_string_lossy().into_owned(),
                registered_at_ms: shell.registered_at.timestamp_millis(),
                last_tail: shell
                    .latest
                    .as_ref()
                    .map(|snap| snap.output_tail.to_string()),
                last_mtime_ms: shell.latest.as_ref().and_then(|snap| {
                    snap.mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_millis() as i64)
                }),
                state_text: new_state.to_state_text(),
            })
        });
        let Some(row) = row else {
            return;
        };
        cx.emit(SolutionAgentStoreEvent::SessionBackgroundShellsChanged(
            session_id,
        ));
        if let Some(db) = self.persistence.clone() {
            cx.background_spawn(async move {
                db.save_background_shell(row).await.log_err();
            })
            .detach();
        }
    }

    /// Scan a freshly-observed thread entry for a `<task-notification>`
    /// completion block and, when it targets a tracked background shell,
    /// flip that shell to its terminal [`ShellRuntimeState`] via
    /// [`Self::mark_background_shell_state`].
    ///
    /// claude's harness injects a `<task-notification>` **user-role message**
    /// into the thread when a `Bash(run_in_background=true)` command finishes.
    /// That arrives as an [`acp_thread::AgentThreadEntry::UserMessage`], NOT a
    /// `ToolCall`, so `apply_subagent_lifecycle` (which early-returns on
    /// non-ToolCall entries) never sees it — hence this separate scan, called
    /// from the `NewEntry` / `EntryUpdated` arms.
    ///
    /// No-op for any other entry shape, an unparseable / non-notification
    /// user message, or a notification whose `<task-id>` isn't a shell we
    /// track. The text is read from the user message's `ContentBlock` via
    /// `to_markdown`, which returns the raw markdown source (the unescaped
    /// `<task-notification>` block) for a `Markdown` block.
    fn observe_task_notification(
        &mut self,
        session_id: SolutionSessionId,
        local_entry_index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(session_entity) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        let notification = {
            let session = session_entity.read(cx);
            let Some(thread) = session.acp_thread() else {
                return;
            };
            let thread_ref = thread.read(cx);
            let Some(entry) = thread_ref.entries().get(local_entry_index) else {
                return;
            };
            let acp_thread::AgentThreadEntry::UserMessage(message) = entry else {
                return;
            };
            let text = message.content.to_markdown(cx);
            crate::background_shell::parse_task_notification(text)
        };
        let Some(notification) = notification else {
            return;
        };
        if session_entity
            .read(cx)
            .background_shells
            .contains_key(&notification.id)
        {
            self.mark_background_shell_state(session_id, notification.id, notification.status, cx);
        }
    }

    /// One pass over every session's background agents. Removes agents
    /// whose latest snapshot carries a `stop_reason` (terminal done),
    /// plus agents that have been silently dead beyond
    /// `agent.managed_agent_stale_timeout_secs +
    /// agent.managed_agent_dead_linger_secs`. Dead detection itself
    /// (orange pill) is rendering-side using the same stale timeout —
    /// the tick just drops the entries that have fully expired.
    pub fn tick_background_agents(&mut self, cx: &mut Context<Self>) {
        use ::agent_settings::AgentSettings;
        let (stale_secs, linger_secs) = (
            MANAGED_AGENT_STALE_TIMEOUT_SECS,
            MANAGED_AGENT_DEAD_LINGER_SECS,
        );
        let expiry = std::time::Duration::from_secs(stale_secs + linger_secs);
        let now = std::time::SystemTime::now();
        let session_ids: Vec<SolutionSessionId> =
            self.all_sessions().map(|e| e.read(cx).id).collect();
        for session_id in session_ids {
            let Some(session) = self.session(session_id) else {
                continue;
            };
            // Skip sessions with no registered agents — the vast majority of
            // sessions never spawn a managed agent, and `update` is not free.
            if session.read(cx).background_agents.is_empty() {
                continue;
            }
            let to_remove: Vec<crate::background_agent::BackgroundAgentId> =
                session.update(cx, |s, _| {
                    let candidates: Vec<crate::background_agent::BackgroundAgentId> = s
                        .background_agent_order
                        .iter()
                        .filter(|id| {
                            let Some(ba) = s.background_agents.get(id) else {
                                return false;
                            };
                            let Some(snap) = ba.latest.as_ref() else {
                                return false;
                            };
                            if snap.stop_reason.is_some() {
                                return true;
                            }
                            let elapsed = now.duration_since(snap.mtime).unwrap_or_default();
                            elapsed > expiry
                        })
                        .cloned()
                        .collect();
                    for id in &candidates {
                        s.background_agents.remove(id);
                        s.background_agent_order.retain(|x| x != id);
                    }
                    candidates
                });
            if !to_remove.is_empty() {
                cx.emit(SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(
                    session_id,
                ));
                if let Some(db) = self.persistence.clone() {
                    let session_id_string = session_id.to_string();
                    for agent_id in to_remove {
                        let db = db.clone();
                        let session_id_string = session_id_string.clone();
                        let agent_id_string = agent_id.as_str().to_string();
                        cx.background_spawn(async move {
                            db.delete_background_agent(session_id_string, agent_id_string)
                                .await
                                .log_err();
                        })
                        .detach();
                    }
                }
            }
        }
    }

    /// One pass over every session: incrementally tail the PARENT session
    /// JSONL for `<task-notification>` lines and flip matching tracked shells
    /// to their terminal [`ShellRuntimeState`]. Runs on the same 1 Hz tick as
    /// the reap pass, BEFORE it, so a freshly-Exited shell is flipped this
    /// tick (and the reap can later drop it once stale).
    pub fn scan_parent_jsonls_for_completions(&mut self, cx: &mut Context<Self>) {
        let session_ids: Vec<SolutionSessionId> =
            self.all_sessions().map(|e| e.read(cx).id).collect();
        for session_id in session_ids {
            self.scan_parent_jsonl_for_completions(session_id, cx);
        }
    }

    /// Scan a single session's parent JSONL transcript for newly-appended
    /// `<task-notification>` completion lines and flip the matching tracked
    /// shells via [`Self::mark_background_shell_state`].
    ///
    /// Forward-only: the per-session offset is lazily initialised to the
    /// file's CURRENT length on first sight (so historical notifications are
    /// never re-applied) and only advanced past the last COMPLETE newline, so
    /// a half-written trailing line is re-read next tick. No-op when the
    /// session tracks no shells, when none of them are still `Running`, or
    /// when the parent JSONL can't be resolved / doesn't exist.
    fn scan_parent_jsonl_for_completions(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            self.parent_jsonl_scan_offsets.remove(&session_id);
            return;
        };
        let (has_shells, any_running, cwd, acp_session_id) = {
            let s = session.read(cx);
            let any_running = s.background_shells.values().any(|sh| {
                matches!(
                    sh.state,
                    crate::background_shell::ShellRuntimeState::Running
                )
            });
            (
                !s.background_shells.is_empty(),
                any_running,
                s.cwd.clone(),
                s.acp_session_id.0.to_string(),
            )
        };
        if !has_shells {
            // Re-arm from the then-current EOF the next time a shell registers.
            self.parent_jsonl_scan_offsets.remove(&session_id);
            return;
        }
        if !any_running {
            // Everything already terminal — nothing left to flip.
            return;
        }
        let Some(path) = parent_session_jsonl_for(&cwd, &acp_session_id) else {
            return;
        };
        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => return,
        };
        let len = metadata.len();
        // Lazy-init: first sight pins the cursor at the current EOF so we only
        // observe completions forward from now.
        let offset = match self.parent_jsonl_scan_offsets.get(&session_id) {
            Some(off) => {
                // Truncation / rotation: cursor past EOF → re-read from start.
                if *off > len { 0 } else { *off }
            }
            None => {
                self.parent_jsonl_scan_offsets.insert(session_id, len);
                return;
            }
        };
        if len <= offset {
            return;
        }
        let (lines, consumed) = match read_complete_lines_from(&path, offset, len) {
            Some(read) => read,
            None => return,
        };
        // Advance the cursor past the bytes we've fully consumed (the last
        // complete newline), leaving any trailing partial line for next tick.
        self.parent_jsonl_scan_offsets
            .insert(session_id, offset + consumed);
        if lines.is_empty() {
            return;
        }
        let completions = {
            let s = session.read(cx);
            scan_lines_for_completions(&lines, &s.background_shells)
        };
        for (shell_id, state) in completions {
            self.mark_background_shell_state(session_id, shell_id, state, cx);
        }
    }

    /// 1 Hz healthcheck for background shells, the analog of
    /// [`tick_background_agents`]. Reaps a shell when it is in a terminal
    /// state (`Exited`/`Killed`) OR when it has gone stale beyond
    /// `managed_agent_stale_timeout_secs + managed_agent_dead_linger_secs`.
    ///
    /// The staleness check is load-bearing, not redundant: even though
    /// `scan_parent_jsonls_for_completions` now flips most finished shells to
    /// `Exited` live (via the parent-JSONL `<task-notification>` scan), a shell
    /// whose subprocess dies without emitting a notification (crash, restart,
    /// killed harness) would otherwise leak as a "Running" pill forever. Age is
    /// measured from `latest.mtime` (the output file's last-observed write — it
    /// stops advancing once the command finishes) when a snapshot exists, else
    /// from `registered_at` (a shell that produced zero output and finished must
    /// still age out).
    pub fn tick_background_shells(&mut self, cx: &mut Context<Self>) {
        use ::agent_settings::AgentSettings;
        let (stale_secs, linger_secs) = (
            MANAGED_AGENT_STALE_TIMEOUT_SECS,
            MANAGED_AGENT_DEAD_LINGER_SECS,
        );
        let expiry = std::time::Duration::from_secs(stale_secs + linger_secs);
        let now = std::time::SystemTime::now();
        let session_ids: Vec<SolutionSessionId> =
            self.all_sessions().map(|e| e.read(cx).id).collect();
        for session_id in session_ids {
            let Some(session) = self.session(session_id) else {
                continue;
            };
            if session.read(cx).background_shells.is_empty() {
                continue;
            }
            let to_remove: Vec<crate::background_shell::BackgroundShellId> =
                session.update(cx, |s, _| {
                    let candidates: Vec<crate::background_shell::BackgroundShellId> = s
                        .background_shell_order
                        .iter()
                        .filter(|id| {
                            let Some(shell) = s.background_shells.get(id) else {
                                return false;
                            };
                            if matches!(
                                shell.state,
                                crate::background_shell::ShellRuntimeState::Exited(_)
                                    | crate::background_shell::ShellRuntimeState::Killed
                            ) {
                                return true;
                            }
                            // Age from the output file's last-observed mtime when a
                            // snapshot exists, else from registration time.
                            let age = match shell.latest.as_ref() {
                                Some(snap) => now.duration_since(snap.mtime).unwrap_or_default(),
                                None => {
                                    let registered: std::time::SystemTime =
                                        shell.registered_at.into();
                                    now.duration_since(registered).unwrap_or_default()
                                }
                            };
                            age > expiry
                        })
                        .cloned()
                        .collect();
                    for id in &candidates {
                        s.background_shells.remove(id);
                        s.background_shell_order.retain(|x| x != id);
                    }
                    candidates
                });
            if !to_remove.is_empty() {
                cx.emit(SolutionAgentStoreEvent::SessionBackgroundShellsChanged(
                    session_id,
                ));
                if let Some(db) = self.persistence.clone() {
                    let session_id_string = session_id.to_string();
                    for shell_id in to_remove {
                        let db = db.clone();
                        let session_id_string = session_id_string.clone();
                        let shell_id_string = shell_id.to_string();
                        cx.background_spawn(async move {
                            db.delete_background_shell(session_id_string, shell_id_string)
                                .await
                                .log_err();
                        })
                        .detach();
                    }
                }
            }
        }
    }

    /// Restore persisted background_agents for a freshly-hydrated session.
    /// Per row, stats the JSONL file:
    ///   * file missing → drop the SQLite row (best-effort).
    ///   * file present, latest line carries `stop_reason` → drop the row
    ///     (the agent finished while we were closed).
    ///   * else → register with the live snapshot. The render-side
    ///     classifier decides Dead vs Running based on mtime and
    ///     `agent.managed_agent_stale_timeout_secs`, so we don't need to flag dead
    ///     here.
    /// Always called inside the foreground hydrate path with the DB rows
    /// already loaded (caller pre-fetches off the foreground thread).
    pub(crate) fn reconcile_background_agents_for(
        &mut self,
        session_id: SolutionSessionId,
        rows: Vec<crate::db::BackgroundAgentRow>,
        cx: &mut Context<Self>,
    ) {
        if rows.is_empty() {
            return;
        }
        let Some(session) = self.session(session_id) else {
            return;
        };

        let mut to_drop_from_db: Vec<(String, String)> = Vec::new();
        let mut to_register: Vec<(
            crate::background_agent::BackgroundAgentId,
            std::path::PathBuf,
            Option<crate::background_agent::BackgroundAgentSnapshot>,
            u64,
        )> = Vec::new();

        for row in rows {
            let path = std::path::PathBuf::from(&row.jsonl_path);
            let agent_id = crate::background_agent::BackgroundAgentId::new(row.agent_id.clone());
            if !path.exists() {
                to_drop_from_db.push((row.solution_session_id.clone(), row.agent_id));
                continue;
            }
            let (snap, last_offset) = match crate::background_agent::tail_jsonl(&path, 0) {
                Ok(t) => {
                    let mtime = t.mtime;
                    let s = t.last_line.map(|line| {
                        let mut s = crate::background_agent::parse_jsonl_snapshot(&line);
                        s.mtime = mtime;
                        s
                    });
                    (s, t.new_offset)
                }
                Err(_) => (None, 0),
            };
            if let Some(ref s) = snap
                && s.stop_reason.is_some()
            {
                to_drop_from_db.push((row.solution_session_id.clone(), row.agent_id));
                continue;
            }
            to_register.push((agent_id, path, snap, last_offset));
        }

        if !to_register.is_empty() {
            session.update(cx, |s, _| {
                for (agent_id, path, snap, last_offset) in &to_register {
                    s.background_agents.insert(
                        agent_id.clone(),
                        crate::background_agent::BackgroundAgent {
                            id: agent_id.clone(),
                            jsonl_path: path.clone(),
                            registered_at: chrono::Utc::now(),
                            latest: snap.clone(),
                            last_offset: *last_offset,
                        },
                    );
                    s.background_agent_order.push(agent_id.clone());
                }
            });
            cx.emit(SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(
                session_id,
            ));
        }

        if !to_drop_from_db.is_empty()
            && let Some(db) = self.persistence.clone()
        {
            cx.background_spawn(async move {
                for (sid, aid) in to_drop_from_db {
                    db.delete_background_agent(sid, aid).await.log_err();
                }
            })
            .detach();
        }

        if !session.read(cx).background_agents.is_empty()
            && let Some(fs) = session
                .read(cx)
                .project
                .as_ref()
                .map(|p| p.read(cx).fs().clone())
        {
            self.ensure_background_agent_watcher(session_id, fs, cx);
        }
    }

    /// User-initiated removal of a background-agent pill from the strip
    /// (the × close affordance, only shown in the Dead state). Drops the
    /// agent from the session's in-memory tracking, deletes the persisted
    /// row, emits the change event so the UI re-renders, and — if this
    /// was the session's last tracked agent — cancels the per-session
    /// JSONL watcher task. No-op when the session or agent is already
    /// gone (defensive against races with `tick_background_agents`).
    pub fn remove_background_agent(
        &mut self,
        session_id: SolutionSessionId,
        id: crate::background_agent::BackgroundAgentId,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let mut removed = false;
        session.update(cx, |s, _| {
            if s.background_agents.remove(&id).is_some() {
                s.background_agent_order.retain(|x| x != &id);
                removed = true;
            }
        });
        if !removed {
            return;
        }
        if let Some(db) = self.persistence.clone() {
            let sid = session_id.to_string();
            let aid = id.as_str().to_string();
            cx.background_spawn(async move {
                db.delete_background_agent(sid, aid).await.log_err();
            })
            .detach();
        }
        cx.emit(SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(
            session_id,
        ));
        // Drop the watcher when the session no longer tracks any agents
        // — keeping a dangling notify-loop alive after the last pill is
        // gone is wasted work and would also delay re-arming on a future
        // re-registration (the watcher is `idempotent only when absent`).
        if session.read(cx).background_agents.is_empty() {
            self.background_agent_watchers.remove(&session_id);
        }
    }

    /// Manually drop a tracked background shell — the × affordance on a
    /// terminal/stale shell pill. Symmetric to [`Self::remove_background_agent`]:
    /// removes from the `background_shells` map + `background_shell_order` vec,
    /// fire-and-forgets the SQLite delete, emits
    /// [`SolutionAgentStoreEvent::SessionBackgroundShellsChanged`], and drops the
    /// fs-watch task once no shells remain. No-op when the session is gone or the
    /// id isn't tracked.
    pub fn remove_background_shell(
        &mut self,
        session_id: SolutionSessionId,
        id: crate::background_shell::BackgroundShellId,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let mut removed = false;
        session.update(cx, |s, _| {
            if s.background_shells.remove(&id).is_some() {
                s.background_shell_order.retain(|x| x != &id);
                removed = true;
            }
        });
        if !removed {
            return;
        }
        if let Some(db) = self.persistence.clone() {
            let sid = session_id.to_string();
            let shell_id = id.to_string();
            cx.background_spawn(async move {
                db.delete_background_shell(sid, shell_id).await.log_err();
            })
            .detach();
        }
        cx.emit(SolutionAgentStoreEvent::SessionBackgroundShellsChanged(
            session_id,
        ));
        if session.read(cx).background_shells.is_empty() {
            self.background_shell_watchers.remove(&session_id);
        }
    }

    fn handle_acp_event(
        &mut self,
        session_id: SolutionSessionId,
        event: &acp_thread::AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(session_entity) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        match event {
            acp_thread::AcpThreadEvent::NewEntry => {
                self.mutate_state(
                    session_id,
                    |state| {
                        if matches!(state, SessionState::Idle | SessionState::AwaitingInput) {
                            *state = SessionState::Running {
                                started_at: std::time::Instant::now(),
                                notified: false,
                            };
                        }
                    },
                    cx,
                );
                if let Some(s) = self.sessions.get(&session_id).cloned() {
                    s.update(cx, |s, _| s.last_activity_at = Utc::now());
                }
                // First user message appends a NewEntry — refresh DB so the
                // History popover preview stops being NULL.
                self.persist_session_row(session_id, cx);
                // `entry_index` on AcpThreadEvent is LOCAL to the live thread's
                // entries vector. The global index counts the cold prefix first.
                // Without the offset, the first live entry after a cold→live
                // transition would overwrite a cold entry in `session.entries`.
                let (cold_count, live_last_local) = {
                    let session = session_entity.read(cx);
                    let cold = session.live_base;
                    let live_last = session
                        .acp_thread()
                        .map(|thread| thread.read(cx).entries().len().saturating_sub(1))
                        .unwrap_or(0);
                    (cold, live_last)
                };
                let global_entry_index = cold_count + live_last_local;
                // Incremental NewEntry: convert just the new live entry and push
                // it onto `session.entries`, stamping `created_ms = now_ms`.
                // Fill any gap below the new global index with sentinel entries
                // (a resumed pre-feature session whose cold timestamps were never
                // captured). The genuinely-new entry at `global_entry_index`
                // gets `now_ms`; entries already present are left untouched.
                let now_ms = Utc::now().timestamp_millis();
                let new_entries = {
                    let s = session_entity.read(cx);
                    let live = s.acp_thread().map(|t| t.read(cx).entries()).unwrap_or(&[]);
                    // Gap entries: existing cold entries beyond what `entries` already
                    // holds (pre-feature sessions restored without timestamps).
                    let current_len = s.entries.len();
                    let mut additions: Vec<crate::session_entry::SessionEntry> = Vec::new();
                    // Fill any gap between the current entries length and the new index.
                    // After unification (Phase 2), cold restore guarantees entries.len() ==
                    // live_base, so gap indices are always in live space — the cold branch
                    // below is unreachable and has been removed to prevent accidental
                    // duplication of cold entries.
                    let live_base = s.live_base;
                    for gap_idx in current_len..global_entry_index {
                        // These are pre-existing entries whose creation time was never
                        // captured; convert from live and stamp with the sentinel.
                        let local = gap_idx - live_base;
                        let entry = {
                            let Some(e) = live.get(local) else {
                                log::warn!(
                                    "solution_agent NewEntry gap-fill: live entry at local idx {} missing (live.len={})",
                                    local,
                                    live.len(),
                                );
                                continue;
                            };
                            crate::session_entry::to_session_entry(e, cx)
                        };
                        let mut gap_entry = entry;
                        gap_entry.created_ms = crate::model::NO_TIMESTAMP_MS;
                        additions.push(gap_entry);
                    }
                    // The new entry at global_entry_index, stamped with now_ms.
                    if current_len + additions.len() == global_entry_index {
                        let local = global_entry_index - s.live_base;
                        if let Some(live_entry) = live.get(local) {
                            let mut new_entry =
                                crate::session_entry::to_session_entry(live_entry, cx);
                            new_entry.created_ms = now_ms;
                            additions.push(new_entry);
                        }
                    }
                    additions
                };
                // Pre-extend length is the first index the upsert range must
                // cover; captured before the closure so it survives for the
                // post-update `persist_upsert_range` call.
                let first_new = session_entity.read(cx).entries.len();
                session_entity.update(cx, |s, cx| {
                    s.entries.extend(new_entries);
                    let new_count = s.entries.len() - first_new;
                    let seqs: Vec<u64> = (0..new_count).map(|_| s.bump_change_seq()).collect();
                    for (entry, seq) in s.entries[first_new..].iter_mut().zip(seqs) {
                        entry.mod_seq = seq;
                    }
                    cx.notify();
                });
                // Persist the newly-appended entries (+ any gap-fill) as rows.
                self.persist_upsert_range(session_id, first_new, cx);
                cx.emit(SolutionAgentStoreEvent::SessionMessageAppended(
                    session_id,
                    global_entry_index,
                ));
                // Subagent-tab lifecycle: a brand-new Task/Agent ToolCall in
                // InProgress is a spawn signal. The `local_entry_index` here is
                // the live thread's local index (entries.len() - 1), which is
                // what `apply_subagent_lifecycle` needs to look up the entry.
                let local_entry_index = session_entity
                    .read(cx)
                    .acp_thread()
                    .map(|thread| thread.read(cx).entries().len().saturating_sub(1));
                if let Some(idx) = local_entry_index {
                    self.apply_subagent_lifecycle(session_id, idx, cx);
                    // A `<task-notification>` completion block arrives as a
                    // user-role message, which `apply_subagent_lifecycle`
                    // ignores (non-ToolCall). Scan the same entry separately so
                    // a finished `Bash(bg)` shell flips to `Exited(code)`.
                    self.observe_task_notification(session_id, idx, cx);
                }
            }
            acp_thread::AcpThreadEvent::Stopped(_) => {
                // Snapshot the Running turn's elapsed time BEFORE the
                // state flip — `mutate_state` overwrites `started_at`
                // with `SessionState::Idle` so we can't recover it
                // after. Stamped onto the session for the status row's
                // "Done in Xs" indicator (cleared on the next Running).
                let elapsed = self.sessions.get(&session_id).and_then(|entity| {
                    if let SessionState::Running { started_at, .. } = &entity.read(cx).state {
                        Some(started_at.elapsed())
                    } else {
                        None
                    }
                });
                self.mutate_state(session_id, |state| *state = SessionState::Idle, cx);
                if let Some(s) = self.sessions.get(&session_id).cloned() {
                    s.update(cx, |s, _| {
                        s.last_activity_at = Utc::now();
                        if let Some(d) = elapsed {
                            s.last_turn_duration = Some(d);
                        }
                    });
                    // Emit a metrics notification on turn completion so the
                    // mobile client sees an updated last_activity_at without
                    // waiting for the next TokenUsageUpdated. Throttled
                    // (2 s window) and non-sequenced per spec.
                    let (last_activity_at, total_tokens, max_tokens) = {
                        let r = s.read(cx);
                        (
                            r.last_activity_at,
                            r.cached_total_tokens,
                            r.cached_max_tokens,
                        )
                    };
                    self.metrics_emitter.emit_if_ready(
                        cx,
                        &session_id,
                        serde_json::json!({
                            "session_id": session_id.to_string(),
                            "last_activity_at": last_activity_at,
                            "total_tokens": total_tokens,
                            "max_tokens": max_tokens,
                        }),
                    );
                }
                // Token usage is finalised on turn completion — refresh DB
                // so the History popover token column reflects the latest.
                self.persist_session_row(session_id, cx);
                // Flush queued follow-ups (if any). All pending entries
                // are drained and concatenated into ONE send — the user
                // typed them as a fast-fire stream while the agent was
                // working, so it's their joint intent for the next turn
                // rather than N independent prompts. A Cancelled stop
                // (user pressed Stop) is treated as "abandon what I
                // queued too": the queue is cleared without sending.
                if let acp_thread::AcpThreadEvent::Stopped(reason) = event {
                    // `flush_after_cancel` (set by `interrupt_and_flush_pending`)
                    // flips Cancelled's default semantics from "abandon the
                    // queue too" to "cancel the current turn but immediately
                    // start the next one with the queued follow-ups". One-
                    // shot — clear the flag whether or not the queue had
                    // anything left to send.
                    let flush_after_cancel = self
                        .sessions
                        .get(&session_id)
                        .map(|s| {
                            s.update(cx, |s, _| {
                                let was = s.flush_after_cancel;
                                s.flush_after_cancel = false;
                                was
                            })
                        })
                        .unwrap_or(false);
                    let cancelled =
                        matches!(reason, agent_client_protocol::schema::StopReason::Cancelled);
                    if cancelled && !flush_after_cancel {
                        // Silent-drop path: user pressed Stop, queue
                        // gets discarded without surfacing what was in
                        // it. Log the dropped bundles BEFORE the clear
                        // so post-mortem of "where did my queued
                        // message go?" can reconstruct it from the
                        // log line. WARN level (not INFO) — this is
                        // user-typed content vanishing without a
                        // trace, which is exactly the failure mode we
                        // want to be able to grep for.
                        let had_pending = if let Some(s) = self.sessions.get(&session_id).cloned() {
                            s.update(cx, |s, _| {
                                let dropped = s.pending_messages.len();
                                if dropped > 0 {
                                    let previews: Vec<String> = s
                                        .pending_messages
                                        .iter()
                                        .map(|bundle| {
                                            queue::summarize_blocks_for_log(&bundle.blocks)
                                        })
                                        .collect();
                                    log::warn!(
                                        target: "solution_agent::queue",
                                        "session={session_id} dropped {dropped} queued bundle(s) on Cancelled stop \
                                         (no flush_after_cancel) — content: [{}]",
                                        previews.join(" | "),
                                    );
                                }
                                s.pending_messages.clear();
                                dropped > 0
                            })
                        } else {
                            false
                        };
                        if had_pending {
                            self.mark_queue_changed(session_id, cx);
                        }
                    } else {
                        // Idle / flush-after-cancel. Deliver the MAIN-targeted
                        // bundles as a new turn. Any Subagent-targeted leftover
                        // belongs to a teammate that the now-ending parent turn
                        // has finished — per design it is LOST (a follow-up for
                        // teammate X is meaningless to the parent), so drop it
                        // with a WARN rather than mis-route it to the main
                        // thread. Partition the queue in one update.
                        let (main_blocks, dropped_subagent) = self
                            .sessions
                            .get(&session_id)
                            .cloned()
                            .map(|s| {
                                s.update(cx, |s, _| {
                                    let mut main: Vec<acp::ContentBlock> = Vec::new();
                                    let mut dropped: Vec<crate::model::PendingBundle> = Vec::new();
                                    for bundle in s.pending_messages.drain(..) {
                                        match bundle.target {
                                            crate::model::QueueTarget::Main => {
                                                main.extend(bundle.blocks)
                                            }
                                            crate::model::QueueTarget::Subagent(_) => {
                                                dropped.push(bundle)
                                            }
                                        }
                                    }
                                    (main, dropped)
                                })
                            })
                            .unwrap_or_default();
                        if !dropped_subagent.is_empty() {
                            let previews: Vec<String> = dropped_subagent
                                .iter()
                                .map(|b| {
                                    let to = match &b.target {
                                        crate::model::QueueTarget::Subagent(id) => id.as_ref(),
                                        crate::model::QueueTarget::Main => "main",
                                    };
                                    format!("→{to}: {}", queue::summarize_blocks_for_log(&b.blocks))
                                })
                                .collect();
                            log::warn!(
                                target: "solution_agent::queue",
                                "session={session_id} dropped {} subagent-targeted bundle(s) on turn end \
                                 (addressee teammate finished without draining; no fallback to main) — content: [{}]",
                                dropped_subagent.len(),
                                previews.join(" | "),
                            );
                        }
                        let had_pending = !main_blocks.is_empty() || !dropped_subagent.is_empty();
                        if had_pending {
                            self.mark_queue_changed(session_id, cx);
                        }
                        if !main_blocks.is_empty() {
                            log::info!(
                                target: "solution_agent::queue",
                                "session={session_id} flushing {} Main block(s) \
                                 (flush_after_cancel={flush_after_cancel}) preview={}",
                                main_blocks.len(),
                                queue::summarize_blocks_for_log(&main_blocks),
                            );
                            // Idle-flush is always end-of-turn: the agent
                            // already produced a complete message, so prepend
                            // the "not a reply" hint (stripped on render, like
                            // the per-message timestamps already in the blocks).
                            let mut with_hint = Vec::with_capacity(main_blocks.len() + 1);
                            with_hint.push(acp::ContentBlock::Text(acp::TextContent::new(
                                format!("{}\n\n", queue::QUEUE_HINT_LINE),
                            )));
                            with_hint.extend(main_blocks);
                            self.send_message_blocks(session_id, with_hint, cx).detach();
                        }
                    }
                }
            }
            acp_thread::AcpThreadEvent::TokenUsageUpdated => {
                // claude-acp ships incremental usage during a turn, not
                // just at the end. Persist on every update so a session
                // closed mid-turn (or right before `Stopped` fires)
                // resumes with the correct meter — without this the DB
                // value lags behind the live meter and a resume drops
                // back to whatever the previous Stopped wrote.
                // Also mirror the new total onto `cached_total_tokens`
                // so the next cold-restore (or any read of the session
                // entity bypassing the live thread) sees the latest
                // figure without the meter regressing to zero.
                if let Some(s) = self.sessions.get(&session_id).cloned() {
                    let usage = s
                        .read(cx)
                        .acp_thread()
                        .and_then(|t| t.read(cx).token_usage().cloned());
                    let total = usage.as_ref().map(|u| u.used_tokens);
                    // `max_tokens == 0` is the "agent didn't fill it in"
                    // sentinel claude-acp ships under some beta paths.
                    // Treat that as None so MCP consumers can fall back
                    // to `DEFAULT_CONTEXT_WINDOW` instead of rendering
                    // "X / 0" on the meter.
                    let max = usage.as_ref().map(|u| u.max_tokens).filter(|m| *m > 0);
                    s.update(cx, |s, _| {
                        s.cached_total_tokens = total;
                        s.cached_max_tokens = max;
                    });
                    // The initialize response (carrying `models`) only lands after the
                    // first turn, so the first TokenUsageUpdated is the earliest capture.
                    let live_models = s.read(cx).acp_thread().and_then(|t| {
                        let t = t.read(cx);
                        t.connection()
                            .clone()
                            .downcast::<claude_native::ClaudeNativeConnection>()
                            .map(|c| c.available_models(t.session_id()))
                    });
                    if let Some(models) = live_models {
                        if !models.is_empty() {
                            let agent_id = s.read(cx).agent_id.clone();
                            self.agent_models.insert(agent_id, models.clone());
                            if s.read(cx).cached_models != models {
                                s.update(cx, |s, _| s.cached_models = models);
                                self.persist_session_row(session_id, cx);
                            }
                        }
                    }
                    // Throttled non-sequenced notification — at most one
                    // emit per 2 s per session. The client treats a
                    // missed metric notify as "check on next snapshot
                    // resync"; no gap-detection or seq field needed.
                    let (last_activity_at, total_tokens, max_tokens) = {
                        let r = s.read(cx);
                        (
                            r.last_activity_at,
                            r.cached_total_tokens,
                            r.cached_max_tokens,
                        )
                    };
                    self.metrics_emitter.emit_if_ready(
                        cx,
                        &session_id,
                        serde_json::json!({
                            "session_id": session_id.to_string(),
                            "last_activity_at": last_activity_at,
                            "total_tokens": total_tokens,
                            "max_tokens": max_tokens,
                        }),
                    );
                }
                self.persist_session_row(session_id, cx);
            }
            acp_thread::AcpThreadEvent::Error | acp_thread::AcpThreadEvent::LoadError(_) => {
                self.mutate_state(
                    session_id,
                    |state| *state = SessionState::Errored(SharedString::from("agent error")),
                    cx,
                );
            }
            acp_thread::AcpThreadEvent::ToolAuthorizationRequested(_) => {
                self.mutate_state(session_id, |state| *state = SessionState::AwaitingInput, cx);
            }
            acp_thread::AcpThreadEvent::ToolAuthorizationReceived(_) => {
                self.mutate_state(
                    session_id,
                    |state| {
                        if matches!(state, SessionState::AwaitingInput) {
                            *state = SessionState::Running {
                                started_at: std::time::Instant::now(),
                                notified: false,
                            };
                        }
                    },
                    cx,
                );
            }
            acp_thread::AcpThreadEvent::TitleUpdated => {
                let new_title = session_entity
                    .read(cx)
                    .acp_thread()
                    .and_then(|t| t.read(cx).title())
                    .unwrap_or_default();
                session_entity.update(cx, |s, _| s.title = new_title);
                cx.emit(SolutionAgentStoreEvent::SessionTitleChanged(session_id));
            }
            acp_thread::AcpThreadEvent::EntriesRemoved(range) => {
                // Truncate `session.entries` to match: a rewind removes all
                // entries from `range.start` onward (in global index space).
                // Cold entries are never removed by a live-thread rewind, so
                // the truncation point is `live_base + range.start`.
                let cold_count = session_entity.read(cx).live_base;
                let global_truncate = cold_count + range.start;
                session_entity.update(cx, |s, cx| {
                    s.entries.truncate(global_truncate);
                    s.bump_change_seq();
                    cx.notify();
                });
                // A rewind drops the removed rows: targeted delete keeps the
                // persisted transcript in lockstep so a stale idx>=truncate row
                // can't corrupt the next cold load.
                self.persist_delete_from(session_id, global_truncate, cx);
                // The user-facing `/clear` does NOT reach this branch:
                // it's intercepted client-side and routed through
                // `reset_context` (which spawns a brand-new `AcpThread`
                // and never emits `EntriesRemoved`); the corresponding
                // token-meter reset lives at the swap site in
                // `reset_context` / `rotate_context`.
                //
                // What this branch covers is a thread-local truncation
                // that happens to remove every entry — today the only
                // in-tree producer is `acp_thread::rewind` /
                // refusal-truncate (`acp_thread.rs:2369`, `:2491`)
                // when rewinding to before the very first user message.
                // The post-event `entries().is_empty()` check
                // discriminates this "rewind to zero" case from a
                // partial rewind: the latter leaves a surviving
                // prefix whose token usage is still meaningful, and
                // the agent will emit a fresh `TokenUsageUpdated`
                // against that prefix on the next turn — so we MUST
                // NOT preemptively wipe state in the partial case.
                let thread = session_entity.read(cx).acp_thread().cloned();
                let cleared = thread
                    .as_ref()
                    .map(|t| t.read(cx).entries().is_empty())
                    .unwrap_or(false);
                if cleared {
                    if let Some(t) = thread {
                        t.update(cx, |t, cx| t.update_token_usage(None, cx));
                    }
                    session_entity.update(cx, |s, _| {
                        s.cached_total_tokens = None;
                        s.last_turn_duration = None;
                    });
                    self.persist_session_row(session_id, cx);
                }
            }
            acp_thread::AcpThreadEvent::EntryUpdated(idx) => {
                // Subagent-tab lifecycle: a tracked Task/Agent ToolCall that
                // just flipped to a terminal status is a finish signal. We
                // run this BEFORE the EntryUpdated throttle plumbing so the
                // `SessionSubagentsChanged` emit happens on the same tick
                // the parent thread's `EntryUpdated` is observed, without
                // waiting for the 500 ms debounce that gates
                // `SessionMessageAppended`.
                self.apply_subagent_lifecycle(session_id, *idx, cx);
                // A `<task-notification>` can also surface via an in-place
                // EntryUpdated (a user message whose text streams in); scan it
                // here too. `observe_task_notification` is idempotent — the
                // `mark_background_shell_state` no-op guard rejects a re-observed
                // terminal state, so a NewEntry + EntryUpdated pair on the same
                // notification flips the shell exactly once.
                self.observe_task_notification(session_id, *idx, cx);
                // Tool-call arg deltas, assistant-text chunks, and tool-
                // status transitions on an existing entry all surface
                // here. The pre-fix behaviour fell through to the
                // `_ => {}` catch-all, so external MCP consumers (the
                // Android client) never learned the entry changed and
                // displayed only the initial empty `args_preview = "{}"`
                // for a tool call or the first preview snapshot of a
                // streaming assistant reply.
                //
                // Coalesced via a trailing-edge debounce: a 500 ms quiet
                // window collapses a token-by-token streaming burst
                // into roughly 2 emits/sec, and a 2 s max-stale guard
                // forces an emit when an entry is continuously dirty so
                // the consumer doesn't starve. Replacing an entry in
                // `entry_update_throttles` drops the previous `Task`,
                // which cancels its inflight timer → only the latest
                // debounce window's task survives to fire.
                let key = (session_id, *idx);
                let now = std::time::Instant::now();
                let existing_first_dirty_at = self
                    .entry_update_throttles
                    .get(&key)
                    .map(|t| t.first_dirty_at);
                let max_stale_breached = existing_first_dirty_at
                    .map(|t| {
                        now.saturating_duration_since(t) >= std::time::Duration::from_millis(2000)
                    })
                    .unwrap_or(false);
                if max_stale_breached {
                    self.entry_update_throttles.remove(&key);
                    cx.emit(SolutionAgentStoreEvent::SessionMessageAppended(
                        session_id, *idx,
                    ));
                } else {
                    let first_dirty_at = existing_first_dirty_at.unwrap_or(now);
                    let entry_index = *idx;
                    let task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(500))
                            .await;
                        this.update(cx, |this, cx| {
                            if this.entry_update_throttles.remove(&key).is_some() {
                                cx.emit(SolutionAgentStoreEvent::SessionMessageAppended(
                                    session_id,
                                    entry_index,
                                ));
                            }
                        })
                        .ok();
                    });
                    self.entry_update_throttles.insert(
                        key,
                        EntryUpdateThrottle {
                            first_dirty_at,
                            _task: task,
                        },
                    );
                }
                // Incremental EntryUpdated: reconvert only the changed entry and
                // replace it in `session.entries`, preserving its `created_ms`
                // (no restamp — the creation time is fixed at first append).
                let cold_count = session_entity.read(cx).live_base;
                let global_idx = cold_count + *idx;
                let updated_entry = {
                    let s = session_entity.read(cx);
                    let live = s.acp_thread().map(|t| t.read(cx).entries()).unwrap_or(&[]);
                    live.get(*idx).map(|live_entry| {
                        let mut entry =
                            crate::session_entry::to_session_entry(live_entry, cx);
                        // Preserve the creation time stamped at first append.
                        entry.created_ms = s
                            .entries
                            .get(global_idx)
                            .map(|e| e.created_ms)
                            .unwrap_or(crate::model::NO_TIMESTAMP_MS);
                        entry
                    })
                };
                if let Some(entry) = updated_entry {
                    session_entity.update(cx, |s, cx| {
                        let seq = s.bump_change_seq();
                        if let Some(slot) = s.entries.get_mut(global_idx) {
                            *slot = entry;
                            slot.mod_seq = seq;
                        }
                        cx.notify();
                    });
                    // Row upsert happens unconditionally on the in-memory update
                    // (one cheap row); the 500ms/2s throttle above governs only
                    // the MCP `SessionMessageAppended` emit, NOT this persist.
                    self.persist_upsert_entry(session_id, global_idx, cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Move the `state_seq` section watermark to a fresh `change_seq` and emit
    /// the `SessionStateChanged` signal (both the internal store event and the
    /// sequenced workspace notification). Centralizes the watermark+emit pair so
    /// the mobile delta's "state section changed iff `state_seq > since_seq`"
    /// invariant can never drift from an emit (decision 2).
    pub(crate) fn mark_state_changed(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.sessions.get(&session_id).cloned() {
            session.update(cx, |s, _| s.state_seq = s.bump_change_seq());
        }
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(session_id));
        self.emit_session_state_changed_workspace(&session_id, cx);
    }

    /// Move the `queue_seq` section watermark to a fresh `change_seq` and emit
    /// `SessionQueueChanged`. See [`Self::mark_state_changed`] for the invariant.
    pub(crate) fn mark_queue_changed(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.sessions.get(&session_id).cloned() {
            session.update(cx, |s, _| s.queue_seq = s.bump_change_seq());
        }
        cx.emit(SolutionAgentStoreEvent::SessionQueueChanged(session_id));
    }

    /// Move the `subagents_seq` section watermark to a fresh `change_seq` and
    /// emit `SessionSubagentsChanged`. See [`Self::mark_state_changed`].
    pub(crate) fn mark_subagents_changed(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.sessions.get(&session_id).cloned() {
            session.update(cx, |s, _| s.subagents_seq = s.bump_change_seq());
        }
        cx.emit(SolutionAgentStoreEvent::SessionSubagentsChanged(session_id));
    }

    /// Emit a sequenced `workspace.session_state_changed` notification for
    /// `session_id`. Reads the current session state from `self.sessions`
    /// and builds the wire payload using the same `session_summary` helper
    /// that the MCP `list_sessions` / `get_session` tools use, so remote
    /// clients receive a fully consistent state object.
    ///
    /// No-ops gracefully when the session is not found (already removed) or
    /// when `WorkspaceEventCoordinator` is not installed (test contexts that
    /// don't initialise the MCP layer).
    fn emit_session_state_changed_workspace(&self, session_id: &SolutionSessionId, cx: &App) {
        let Some(coord) = editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx)
        else {
            return;
        };
        let Some(entity) = self.sessions.get(session_id) else {
            return;
        };
        let summary = entity.read_with(cx, |s, cx| crate::mcp::session_summary(s, cx));
        coord.emit_sequenced(
            cx,
            "workspace.session_state_changed",
            serde_json::json!({
                "solution_id": summary.solution_id,
                "session_id": summary.id,
                "state": summary.state,
            }),
        );
    }

    /// Wraps a `SessionState` mutation so notifier hooks fire uniformly:
    ///   1. Snapshot previous state.
    ///   2. Apply `f` to mutate state.
    ///   3. Emit `SessionStateChanged` only when the discriminant changed.
    ///   4. Ask the notifier whether the transition warrants a desktop
    ///      notification, dispatch it, emit `SessionNotified`, and mark
    ///      the session's `Running { notified: true }` to suppress dupes.
    ///
    /// Side-channel updates (e.g. `last_activity_at`) stay outside `f` so
    /// they don't accidentally affect the notification decision.
    pub(crate) fn mutate_state<F: FnOnce(&mut SessionState)>(
        &mut self,
        session_id: SolutionSessionId,
        f: F,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        let previous = session.read(cx).state.clone();
        session.update(cx, |s, _| f(&mut s.state));
        let next = session.read(cx).state.clone();
        if std::mem::discriminant(&previous) != std::mem::discriminant(&next) {
            self.mark_state_changed(session_id, cx);
        }
        // Drop the Stopping safety-net task whenever the session leaves
        // Stopping by any path (Stopped event handler, Error handler,
        // force_idle, restart_agent's restarting flip, …). Leaving a
        // stale task armed would let it fire 40s later onto a now-Idle
        // session — a harmless no-op but a noisy warn-log we'd then
        // have to explain.
        if matches!(previous, SessionState::Stopping { .. })
            && !matches!(next, SessionState::Stopping { .. })
        {
            session.update(cx, |s, _| s.stopping_safety_net = None);
        }
        // Garbage-collect inline Task subagents on any transition INTO Idle.
        // An inline subagent (keyed by its Task tool-call id) lives strictly
        // inside the parent turn — once the turn ends the parent is Idle and no
        // subagent can still be running. Normally `apply_subagent_lifecycle`
        // removes each one as its tool call goes terminal, but a turn that ends
        // without terminalising the Task tool call (observed: an inner tool
        // Cancelled, the outer Task tool-call left non-terminal / orphaned)
        // would otherwise strand the pill forever — a ~14h-stuck "Run the §G …"
        // tab was seen live via the MCP socket. Stranded pills also keep the
        // subagent strip non-empty, which keeps the `__main__` per-tab filter
        // engaged and hides most of the conversation. Clearing here is the
        // catch-all the per-tool-call path misses.
        if !matches!(previous, SessionState::Idle) && matches!(next, SessionState::Idle) {
            session.update(cx, |s, _| {
                if !s.active_subagents.is_empty() || !s.active_subagent_order.is_empty() {
                    s.active_subagents.clear();
                    s.active_subagent_order.clear();
                }
            });
        }
        let now = std::time::Instant::now();
        let is_focused = self
            .focus_resolver
            .as_ref()
            .map(|f| f(session_id, cx))
            .unwrap_or(false);
        let has_pending_messages = !session.read(cx).pending_messages.is_empty();
        if let Some(decision) = notifier::decide_notification(
            session_id,
            &previous,
            &next,
            now,
            is_focused,
            has_pending_messages,
        ) {
            let (title, body) = {
                let s = session.read(cx);
                let title = format!("Sawe — {} ({})", s.agent_id, s.title);
                let body = match decision.kind {
                    notifier::NotifyKind::Completed => {
                        format!("Done after {} min", decision.elapsed.as_secs() / 60)
                    }
                    notifier::NotifyKind::AwaitingInput => format!(
                        "Awaiting your input after {} min",
                        decision.elapsed.as_secs() / 60
                    ),
                    notifier::NotifyKind::Errored => match &next {
                        SessionState::Errored(msg) => format!("Failed: {msg}"),
                        _ => "Failed".to_string(),
                    },
                };
                (title, body)
            };
            notifier::dispatch(&decision, &title, &body, cx);
            cx.emit(SolutionAgentStoreEvent::SessionNotified(
                session_id,
                decision.kind,
            ));
            session.update(cx, |s, _| {
                if let SessionState::Running { notified, .. } = &mut s.state {
                    *notified = true;
                }
            });
        }
    }

    fn on_solution_event(
        &mut self,
        _: Entity<SolutionStore>,
        event: &SolutionStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            SolutionStoreEvent::Changed => self.gc_orphan_solutions(cx),
            SolutionStoreEvent::Closed { id } => self.cold_close_solution(id, cx),
            _ => {}
        }
    }

    /// Solution-window close: stop the solution's pooled subprocess(es) and
    /// evict its sessions from memory, WITHOUT marking them `closed_at`. The
    /// transcript + `tab_order` stay in the DB, so reopening the solution
    /// restores every tab via `restore_open_tabs`. Distinct from
    /// [`close_session`](Self::close_session) (a permanent per-tab close that
    /// sets `closed_at`) and from [`gc_orphan_solutions`](Self::gc_orphan_solutions)
    /// (which fires only when a solution is *deleted* from the store).
    pub fn cold_close_solution(&mut self, solution_id: &SolutionId, cx: &mut Context<Self>) {
        let session_ids = self
            .by_solution
            .get(solution_id)
            .cloned()
            .unwrap_or_default();
        // Flush each transcript before dropping the live thread. Incremental
        // saves usually have the latest state already; this captures any
        // un-debounced tail so a reopen restores the full conversation.
        for id in &session_ids {
            self.persist_all_rows(*id, cx);
        }
        self.by_solution.remove(solution_id);
        for id in &session_ids {
            self.sessions.remove(id);
            self.entry_update_throttles.retain(|(sid, _), _| sid != id);
        }
        // Drop the pool's connection handle(s) for this solution. Together
        // with the session eviction above (whose entities release their own
        // connection refs once the closing window's views tear down) this
        // releases the last Rc, so the subprocess exits now instead of
        // lingering for the 60s idle debounce.
        let mut pool = self.pool.lock();
        let keys: Vec<(SolutionId, AgentServerId)> =
            pool.keys_for_solution(solution_id).collect();
        for key in &keys {
            pool.remove(key);
        }
    }

    /// Construct a headless `project::Project` bound to nothing in
    /// particular — no worktree, no env, no window/workspace. Used by
    /// the MCP-driven auto-wake path (`queue::send_message_blocks_with_wake`):
    /// when a client (the mobile app) sends to a Cold session and the
    /// desktop has no window open for the solution, we still need a
    /// project handle to feed into `resume_session`.
    ///
    /// The `_solution` arg is taken for symmetry with the call site
    /// (and to make the intent obvious at call sites) but isn't used —
    /// `resume_session` keys claude-acp's jsonl lookup off the
    /// metadata's `cwd`, not the project's worktree. Empty worktree is
    /// fine.
    ///
    /// Pulls dependencies from `workspace::AppState::global` — the
    /// editor's `main.rs` sets this before any MCP server can hit us,
    /// so absence is a programmer error in init order (returns Err so
    /// the caller surfaces it instead of panicking).
    pub(crate) fn make_headless_project_for_solution(
        _solution: &solutions::Solution,
        cx: &mut App,
    ) -> Result<Entity<project::Project>> {
        let app_state = workspace::AppState::try_global(cx)
            .ok_or_else(|| anyhow!("workspace::AppState global is not initialised"))?;
        Ok(project::Project::local(
            app_state.client.clone(),
            app_state.node_runtime.clone(),
            app_state.user_store.clone(),
            app_state.languages.clone(),
            app_state.fs.clone(),
            None,
            project::LocalProjectFlags {
                init_worktree_trust: false,
                ..Default::default()
            },
            cx,
        ))
    }

    fn gc_orphan_solutions(&mut self, cx: &mut Context<Self>) {
        let Some(store) = SolutionStore::try_global(cx) else {
            return;
        };
        let alive: std::collections::HashSet<SolutionId> = store
            .read(cx)
            .solutions()
            .iter()
            .map(|s| s.id.clone())
            .collect();
        let orphan_ids: Vec<SolutionId> = self
            .by_solution
            .keys()
            .filter(|sid| !alive.contains(*sid))
            .cloned()
            .collect();
        for sid in orphan_ids {
            if let Some(session_ids) = self.by_solution.remove(&sid) {
                for session_id in session_ids {
                    self.sessions.remove(&session_id);
                    if let Some(db) = &self.persistence {
                        db.delete(session_id).detach_and_log_err(cx);
                    }
                    cx.emit(SolutionAgentStoreEvent::SessionClosed(session_id));
                }
            }
            if let Some(db) = &self.persistence {
                db.delete_for_solution(sid).detach_and_log_err(cx);
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod label_unit_tests {
    use super::{label_fallback, short_id_suffix};
    use gpui::SharedString;

    #[test]
    fn short_id_suffix_truncates_long_ids() {
        assert_eq!(short_id_suffix("toolu_01abcdef"), "cdef");
    }

    #[test]
    fn short_id_suffix_returns_full_id_when_short() {
        assert_eq!(short_id_suffix("abc"), "abc");
        assert_eq!(short_id_suffix(""), "");
    }

    #[test]
    fn label_fallback_uses_subagent_type_when_present() {
        let id = SharedString::from("toolu_xyzwabcd");
        assert_eq!(
            label_fallback(&id, Some("general-purpose")).as_ref(),
            "general-purpose#abcd"
        );
    }

    #[test]
    fn label_fallback_falls_back_to_agent_short_when_subagent_type_missing() {
        let id = SharedString::from("toolu_xyzwabcd");
        assert_eq!(label_fallback(&id, None).as_ref(), "Agent abcd");
    }

    #[test]
    fn label_fallback_treats_empty_subagent_type_as_missing() {
        let id = SharedString::from("toolu_xyzwabcd");
        assert_eq!(label_fallback(&id, Some("")).as_ref(), "Agent abcd");
    }
}

#[cfg(test)]
mod background_agent_dir_tests {
    #[test]
    fn background_agent_dir_for_encodes_cwd() {
        let dir = super::background_agent_dir_for(
            std::path::Path::new("/home/spk/projects/foo.bar"),
            "ses-xyz",
        );
        let dir = dir.expect("home_dir must resolve in test env");
        assert!(
            dir.to_string_lossy().contains("-home-spk-projects-foo-bar"),
            "expected encoded cwd in path, got {:?}",
            dir
        );
        assert!(dir.ends_with("subagents"));
    }

    #[test]
    fn background_agent_dir_for_empty_cwd_returns_none() {
        assert!(super::background_agent_dir_for(std::path::Path::new(""), "ses-x").is_none());
    }

    #[test]
    fn parent_session_jsonl_for_encodes_cwd_and_appends_jsonl() {
        let path = super::parent_session_jsonl_for(
            std::path::Path::new("/home/spk/projects/foo.bar"),
            "ses-xyz",
        );
        let path = path.expect("home_dir must resolve in test env");
        let s = path.to_string_lossy();
        assert!(
            s.contains("-home-spk-projects-foo-bar"),
            "expected encoded cwd in path, got {:?}",
            path
        );
        assert!(s.ends_with("ses-xyz.jsonl"), "got {:?}", path);
    }

    #[test]
    fn parent_session_jsonl_for_empty_cwd_returns_none() {
        assert!(super::parent_session_jsonl_for(std::path::Path::new(""), "ses-x").is_none());
    }
}

#[cfg(test)]
mod parent_jsonl_scan_tests {
    use super::*;
    use crate::background_shell::{BackgroundShell, BackgroundShellId, ShellRuntimeState};
    use std::collections::HashMap;

    fn running_shell(id: &str) -> (BackgroundShellId, BackgroundShell) {
        let bid = BackgroundShellId::new(id);
        (
            bid.clone(),
            BackgroundShell {
                id: bid,
                command: "sleep 60".into(),
                output_path: std::path::PathBuf::from(format!("/tmp/{id}.output")),
                registered_at: chrono::Utc::now(),
                latest: None,
                last_offset: 0,
                state: ShellRuntimeState::Running,
            },
        )
    }

    const REAL_JSONL_LINE: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<task-notification>\n<task-id>bvb4ful1z</task-id>\n<tool-use-id>toolu_01AqJufkNFAd7Aef3ojZ8d5J</tool-use-id>\n<status>completed</status>\n<summary>Background command \"Sleep for 60 seconds in background\" completed (exit code 0)</summary>\n</task-notification>"}]},"uuid":"abc-123"}"#;

    #[test]
    fn scan_lines_flips_known_shell_to_exited_with_code() {
        let mut shells = HashMap::new();
        let (id, shell) = running_shell("bvb4ful1z");
        shells.insert(id.clone(), shell);
        let lines = vec![REAL_JSONL_LINE.to_string()];
        let out = scan_lines_for_completions(&lines, &shells);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, id);
        assert_eq!(out[0].1, ShellRuntimeState::Exited(Some(0)));
    }

    #[test]
    fn scan_lines_ignores_unknown_id() {
        let mut shells = HashMap::new();
        let (id, shell) = running_shell("someotherid");
        shells.insert(id, shell);
        let lines = vec![REAL_JSONL_LINE.to_string()];
        assert!(scan_lines_for_completions(&lines, &shells).is_empty());
    }

    #[test]
    fn scan_lines_ignores_non_notification_lines() {
        let mut shells = HashMap::new();
        let (id, shell) = running_shell("bvb4ful1z");
        shells.insert(id, shell);
        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":"hi"}}"#.to_string(),
        ];
        assert!(scan_lines_for_completions(&lines, &shells).is_empty());
    }

    #[test]
    fn read_complete_lines_leaves_trailing_partial_for_next_tick() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("ses.jsonl");
        // Two complete lines + a trailing partial (no newline).
        let content = "line one\nline two\npartial-no-newline";
        std::fs::write(&path, content).expect("write");
        let end = content.len() as u64;
        let (lines, consumed) = read_complete_lines_from(&path, 0, end).expect("read ok");
        assert_eq!(lines, vec!["line one".to_string(), "line two".to_string()]);
        // Consumed only through the second newline; the partial is left.
        assert_eq!(consumed, "line one\nline two\n".len() as u64);
        // A re-read from the advanced offset with no new bytes yields nothing.
        let (lines2, consumed2) = read_complete_lines_from(&path, consumed, end).expect("read ok");
        assert!(lines2.is_empty());
        assert_eq!(consumed2, 0);
    }

    #[test]
    fn read_complete_lines_skips_oversized_line_instead_of_wedging() {
        // A single line longer than the read cap (e.g. a large inline `Read`
        // result in the transcript) has no newline in the first cap window.
        // The scan must SKIP it (advance by the cap) rather than pin the
        // offset at 0 forever — otherwise live completion detection wedges.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("ses.jsonl");
        let huge = "x".repeat((PARENT_JSONL_READ_CAP + 4096) as usize);
        let content = format!("{huge}\nshort tail line\n");
        std::fs::write(&path, &content).expect("write");
        let end = content.len() as u64;
        // First read: full cap window, no newline → skip by exactly the cap.
        let (lines, consumed) = read_complete_lines_from(&path, 0, end).expect("read ok");
        assert!(lines.is_empty());
        assert_eq!(
            consumed, PARENT_JSONL_READ_CAP,
            "must advance past oversized line"
        );
        // Subsequent reads eventually resync at a newline and surface the tail
        // line (offset advances every call, so the scan can never wedge).
        let mut offset = consumed;
        let mut saw_tail = false;
        for _ in 0..4 {
            let (lines, consumed) = read_complete_lines_from(&path, offset, end).expect("read ok");
            if lines.iter().any(|l| l == "short tail line") {
                saw_tail = true;
                break;
            }
            offset += consumed;
            if consumed == 0 {
                break;
            }
        }
        assert!(
            saw_tail,
            "tail line must resurface after skipping the oversized line"
        );
    }
}

#[cfg(test)]
mod subagent_view_tests {
    use super::*;

    #[test]
    fn subagent_view_main_matches_only_parentless_entries() {
        let v = SubagentView::Main;
        assert!(v.matches_parent_entry(None));
        assert!(!v.matches_parent_entry(Some(&"toolu_xyz".into())));
    }

    #[test]
    fn subagent_view_task_matches_exact_id() {
        let v = SubagentView::Task("toolu_a".into());
        assert!(v.matches_parent_entry(Some(&"toolu_a".into())));
        assert!(!v.matches_parent_entry(Some(&"toolu_b".into())));
        assert!(!v.matches_parent_entry(None));
    }

    #[test]
    fn subagent_view_background_matches_no_parent_entry() {
        let v = SubagentView::Background(crate::background_agent::BackgroundAgentId::new("a30f"));
        assert!(!v.matches_parent_entry(None));
        assert!(!v.matches_parent_entry(Some(&"toolu_x".into())));
    }

    #[test]
    fn subagent_view_is_parent_thread_view() {
        assert!(SubagentView::Main.is_parent_thread_view());
        assert!(SubagentView::Task("x".into()).is_parent_thread_view());
        assert!(
            !SubagentView::Background(crate::background_agent::BackgroundAgentId::new("a30f"))
                .is_parent_thread_view()
        );
        assert!(
            !SubagentView::Shell(crate::background_shell::BackgroundShellId::new("bvb4ful1z"))
                .is_parent_thread_view()
        );
    }
}
