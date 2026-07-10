use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
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
use crate::model_catalog::ModelCatalog;
use crate::model::{
    AgentServerId, SessionContextCount, SessionState, SolutionSession, SolutionSessionId,
    SolutionSessionMetadata,
};
use crate::notifier;
use crate::pool::SubprocessPool;
use crate::teammate_watchers::TeammateWatchers;

mod connection_pool;
mod queue;
mod supervisor_engine;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
pub(crate) mod tests;

pub(crate) use queue::{QUEUE_HINT_LINE, TS_PREFIX_CLOSE, TS_PREFIX_OPEN};
pub(crate) use supervisor_engine::{JudgeHandle, VerdictAuth};

// Fork-local managed-agent lifecycle tunables. Upstream v1.7.2's resolved
// `AgentSettings` dropped these fields (they live only in `settings_content`
// as `Option<u64>`); since this crate may not edit those crates, we pin the
// historical defaults here. Stale = a session with no recent activity is a
// candidate for tear-down; dead-linger = grace period before reaping.
const MANAGED_AGENT_STALE_TIMEOUT_SECS: u64 = 120;
const MANAGED_AGENT_DEAD_LINGER_SECS: u64 = 300;
/// Hard cap on how long a still-`Running` background shell whose PARENT agent
/// subprocess is still alive may go silent before it's reaped anyway. While the
/// parent is alive, a completing shell is flipped to `Exited` by the
/// parent-JSONL `<task-notification>` scan, so a mere output-silence is NOT
/// death (a long silent build / `sleep` / quiet `curl`) — the shell is kept,
/// which preserves the `has_live_background_work` supervisor-suppression instead
/// of reaping it at the ~7min `STALE + DEAD_LINGER` mark (hardening #9). This cap
/// still ages out a runaway that never completes and never prints. A shell whose
/// parent subprocess is GONE (reconnect / crash / close → no `acp_thread`, so no
/// completion notification can ever arrive) is reaped on the ordinary
/// `STALE + DEAD_LINGER` timeout instead — that orphan is the real leak case.
const BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS: u64 = 60 * 60;
/// How long a session may sit in `Running` with zero streaming activity AND no
/// in-progress tool call before the stuck-session watchdog
/// ([`SolutionAgentStore::tick_stuck_sessions`]) treats its subprocess as
/// wedged and non-destructively reconnects it. A healthy turn streams well
/// within this; a turn legitimately blocked on a foreground command is held off
/// by the in-progress-tool check (see [`TOOL_STUCK_SECS`]).
const STUCK_TURN_SECS: u64 = 5 * 60;
/// Backstop for the watchdog when a tool call is in-progress (claude is blocked
/// on a foreground command): only after the SAME tool has been running this
/// long do we treat it as truly stuck and reconnect. Generous so real
/// builds/tests aren't killed; covers a command that hangs forever or claude
/// wedging mid-tool.
const TOOL_STUCK_SECS: u64 = 20 * 60;
/// Output-silence window for an in-progress foreground tool past
/// [`TOOL_STUCK_SECS`]. A display-only terminal's output (claude-acp's path)
/// each rides a `ToolCallUpdate` that bumps `last_activity_at`, so for that path
/// the session silence clock (`silent_secs`) already tracks how long ago the
/// command last printed. Only when a tool that has already run past
/// `TOOL_STUCK_SECS` has ALSO been silent this long (and has no running OS
/// process — [`acp_thread::Terminal::is_process_running`], the signal for the
/// real-PTY path whose output does NOT bump the clock) do we treat it as truly
/// hung and reconnect. Must be well above [`STUCK_TURN_SECS`] or it would never
/// spare a tool the candidate gate hasn't already cleared: a legitimately long
/// command that printed within this window is left alone (hardening #7); a truly
/// wedged one prints nothing and crosses it.
const TOOL_OUTPUT_SILENCE_SECS: u64 = 15 * 60;

/// Decide whether a silent-for-[`STUCK_TURN_SECS`] `Running` turn is wedged,
/// given its newest in-progress tool call as `(tool_secs, shows_liveness)` —
/// how long that tool has been running and whether it's still making progress.
/// `None` means no tool is executing (claude hung between steps → wedged). A
/// running tool is wedged only once it has exceeded [`TOOL_STUCK_SECS`] AND is
/// no longer showing liveness (hardening #7 — a live long build isn't killed).
/// Pure so the decision is unit-testable without a live thread.
fn turn_is_wedged(active_tool: Option<(i64, bool)>) -> bool {
    match active_tool {
        Some((tool_secs, shows_liveness)) => {
            tool_secs >= TOOL_STUCK_SECS as i64 && !shows_liveness
        }
        None => true,
    }
}

/// Per-attempt timeout for a reconnect's `resume_session` (subprocess respawn +
/// ACP handshake). A dead subprocess can start but never complete the handshake,
/// hanging the resume forever and stranding the session at `Errored("reconnecting…")`.
/// Generous enough for a slow cold start; a timeout is retried once, then surfaced.
const RECONNECT_RESUME_TIMEOUT_SECS: u64 = 60;

/// Retry delay when a judge/auditor spawn is skipped because the per-solution
/// MCP socket (which serves the scoped verdict tools) isn't resolvable yet.
/// Gates the next fire so a sustained socket outage doesn't re-fire→re-skip on
/// every 1 Hz tick (flooding the diary / DB). Short enough that a brief startup
/// race barely delays the first real judge.
const JUDGE_SPAWN_RETRY_MS: i64 = 15_000;

/// Continuation prompt sent to the agent after [`SolutionAgentStore::reconnect_agent`]
/// brings a wedged (mid-turn) session back, so it resumes instead of parking at
/// Idle. Deliberately a fresh "carry on" instruction, NOT a replay of the
/// interrupted turn (replaying could re-run tool calls whose side effects already
/// landed).
const RECONNECT_CONTINUATION_PROMPT: &str = "Твой процесс завис, поэтому редактор перезапустил его. \
     История и контекст сохранены — продолжай работу с того места, на котором остановился.";

/// Continuation sent after [`SolutionAgentStore::reconnect_agent`] when the
/// wedge happened on an UNANSWERED user message — the transcript tail is a human
/// message with no assistant reply after it (the agent hung *before* it started
/// answering). The generic "carry on where you left off" prompt is actively
/// WRONG here: there was no work in progress to resume, and telling a fresh
/// subprocess to "continue" makes it treat the replayed user message as
/// already-handled history and skip it — the reported "my message never reached
/// you" bug. So point it explicitly at the user's message instead.
const RECONNECT_UNANSWERED_USER_PROMPT: &str = "Твой процесс завис, не успев ответить на \
     ПОСЛЕДНЕЕ сообщение пользователя (оно выше в истории). История и контекст сохранены. \
     Перечитай это сообщение и выполни его сейчас — НЕ считай его уже обработанным.";

/// Classify a `done` verdict's `reasoning`. `done` has two modes (see
/// `supervisor_judge_instructions.md`): a genuine completion, or a PARK awaiting
/// the operator. The judge prefixes a park's reasoning with the `PARK:` token, so
/// this returns `(is_park, body)` where `body` is the reasoning with that
/// internal token stripped — used both to label the session log / notification
/// honestly (a stall must not read as a finished task) AND to keep the raw
/// `PARK:` marker out of user-visible text. Robust leading-token check, NOT a
/// fuzzy parse: a missing token degrades to a completion (a real completion is
/// never mislabeled a park, only the reverse).
fn classify_done_reasoning(reasoning: &str) -> (bool, &str) {
    match reasoning.trim_start().strip_prefix("PARK:") {
        Some(rest) => (true, rest.trim_start()),
        None => (false, reasoning),
    }
}

/// True when the transcript tail is an UNANSWERED human message: scanning from
/// the end past editor-injected `System` notes, the first real entry is a
/// `UserMessage` that is NOT a supervisor observer-nudge. This is the "agent
/// hung before it answered the user" shape — distinct from a mid-work wedge
/// (tail is an assistant/tool entry), which the generic continuation handles.
/// An observer nudge tail is excluded: it's the supervisor's own voice, not the
/// human's, and the generic "carry on" is right for it.
fn tail_is_unanswered_user_message(entries: &[crate::session_entry::SessionEntry]) -> bool {
    use crate::session_entry::SessionEntryKind;
    entries
        .iter()
        .rev()
        .find(|e| !matches!(e.kind, SessionEntryKind::System { .. }))
        .is_some_and(|e| {
            matches!(
                &e.kind,
                SessionEntryKind::UserMessage { chunks, .. }
                    // Exclude editor-injected non-human messages: an observer nudge
                    // AND a prior reconnect-recovery prompt (else a SECOND
                    // consecutive hang points the recovery at the editor's own
                    // "your process hung" message).
                    if !acp_thread::is_observer_nudge_blocks(chunks)
                        && !acp_thread::is_editor_recovery_blocks(chunks)
            )
        })
}

/// Human-readable reason a reconnect resume attempt did not succeed, folding the
/// two failure shapes `with_timeout` produces — `Err(timeout)` and `Ok(Err(resume
/// error))` — into one string for the log.
fn reconnect_attempt_error(outcome: Result<Result<SolutionSessionId>>) -> String {
    match outcome {
        Ok(Ok(_)) => "ok".to_string(),
        Ok(Err(err)) => err.to_string(),
        Err(timeout) => timeout.to_string(),
    }
}

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
    /// Global per-agent model/effort catalog: last-known model list per agent
    /// (shared across that agent's sessions so a fresh session with no turn yet
    /// still offers a model picker) plus the in-flight probe-dedup set. The
    /// orchestration methods below route their catalog-state access through it.
    model_catalog: ModelCatalog,
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
    /// One per-session chain that SERIALIZES the entry-row persist writes
    /// (`persist_main_stream` / `persist_all_rows`). Each helper captures its
    /// plan synchronously (in event order) then chains its detached DB work
    /// behind the previous chain link (`prev.await` before touching the DB), so
    /// the upsert + `delete_entries_from(main_len)` pairs apply in issue order.
    /// Without this, GPUI's detached tasks have NO FIFO guarantee (a later
    /// append's upsert can land before an earlier link's stale
    /// `delete_entries_from` runs, silently deleting the just-written row —
    /// phase-6b keystone bug). Stored as `Task<()>` so it stays alive across
    /// links; removed on `purge_session_hard`.
    entries_persist_chain: HashMap<SolutionSessionId, Task<()>>,
    /// Per-session teammate-watching state (survey cluster C10): the managed-
    /// agent + background-shell JSONL/`.output` watcher tasks and the
    /// forward-only parent-JSONL scan cursors. The arming / tailing methods
    /// below stay on `Store` (they read `sessions`, spawn on `Context<Store>`,
    /// and emit store events) but route their map-state access through this.
    teammate_watchers: TeammateWatchers,
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
    /// Per-session supervisor control state (enabled flag, counters, status).
    /// Loaded from `supervisor_state` at init; mutated by the toggle UI, the
    /// watchdog (`tick_supervisor`), and the verdict tools. Narrative (diary,
    /// verdict log) lives on disk, not here.
    supervisor_states: HashMap<SolutionSessionId, crate::supervisor::SupervisorState>,
    /// In-flight ephemeral judge sessions, keyed by the SUPERVISED
    /// session id. Holds the judge's own session id (for cleanup) and the
    /// driving task. Dropping the task cancels the spawn.
    judge_sessions: HashMap<SolutionSessionId, JudgeHandle>,
    /// In-flight ephemeral meta-auditor sessions, keyed by the SUPERVISED
    /// session id. Structurally identical to `judge_sessions` (same
    /// `JudgeHandle`) but kept in a separate map so a live auditor and a live
    /// judge for the same supervised session don't clobber each other. Both
    /// maps feed `live_supervisor_session_ids` for UI hiding.
    auditor_sessions: HashMap<SolutionSessionId, JudgeHandle>,
    /// Live transient-failure backoff timers, keyed by the SUPERVISED session id.
    /// Held only so the timer task isn't dropped (cancelled) immediately. The
    /// timer's wake-up is a no-op — the watchdog re-fire gate is enforced by
    /// `SupervisorState::next_eligible_ms`, not by the timer itself — but a live
    /// timer keeps the 1 Hz tick loop honest about when the session becomes
    /// eligible again. Kept SEPARATE from `judge_sessions` so a stale backoff
    /// handle never blocks the next `spawn_judge`.
    backoff_timers: HashMap<SolutionSessionId, Task<()>>,
    /// Per-session ordered history of abandoned ACP session ids (one is orphaned
    /// on each `/compact` rotation and `/clear`). claude writes a
    /// `~/.claude/projects/<cwd>/<acp_session_id>.jsonl` transcript (+ a
    /// `<acp_session_id>/` subagents dir) per ACP session and NEVER deletes
    /// them — GBs accrue over a multi-day session. We keep only the most recent
    /// `KEEP_RAW_TRANSCRIPTS` (the live one + the last `KEEP_RAW_TRANSCRIPTS-1`
    /// abandoned) and delete older ones. Keyed by our stable
    /// `SolutionSessionId` so we only ever delete THIS session's transcripts,
    /// never another session that happens to share the same project cwd.
    /// In-memory + best-effort: an editor restart forgets the history (the
    /// transcripts from before the restart are then left in place), but new
    /// growth stays bounded for the whole life of a running session.
    raw_transcript_history: HashMap<SolutionSessionId, VecDeque<String>>,
}

/// Metadata captured by [`SolutionAgentStore::teardown_session_runtime`] while
/// the live session entity is still reachable, handed back so the caller can
/// finish the DB/disk/pool side after the in-memory state is gone.
struct SessionTeardown {
    solution_id: SolutionId,
    agent_id: AgentServerId,
    /// `(live connection, ACP session id)` for a session that was spawned on the
    /// pool — used to close the ACP session + release the pool refcount. `None`
    /// for a cold/restored session that never held a subprocess.
    pool_teardown: Option<(Rc<dyn acp_thread::AgentConnection>, acp::SessionId)>,
    /// Hidden supervisor judge/auditor session — suppress all close
    /// notifications (mirrors the create-side suppression).
    was_ephemeral: bool,
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
    /// The session's teammate set changed: a `Task` / `Agent` subagent was
    /// either spawned (parent ToolCall flipped to `InProgress`) or finished
    /// (parent ToolCall flipped to a terminal status). Emitted only when the
    /// set *actually* changed — a duplicate spawn event for a known id, or a
    /// terminal status on an unknown id, is silently ignored to keep the event
    /// stream debounce-friendly. Since wire v5 this drives a lean dirty-poke:
    /// consumers re-poll `streams` rather than receive a subagent list.
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
/// A session the user SOFT-closed (tab close) is fully hard-purged (row +
/// `.agents/<sid>/` tree) once its `closed_at` is older than this — the
/// "lifetime after which a closed chat is finally cleaned." Reopen clears
/// `closed_at`, so a restored session restarts the clock from its next close.
const CLOSED_SESSION_REAP_DAYS: i64 = 30;

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

/// How many of a session's raw claude JSONL transcripts to keep on disk: the
/// live one plus the last `KEEP_RAW_TRANSCRIPTS - 1` abandoned rotations.
const KEEP_RAW_TRANSCRIPTS: usize = 3;

/// Push `abandoned` onto a session's transcript ring and return the ids that
/// now fall outside the keep-window (oldest first). Pure (no IO) so the
/// retention math is unit-tested directly. With `keep = 3` the live transcript
/// is kept implicitly and the last 2 abandoned ones are retained.
fn push_and_evict_transcripts(
    history: &mut VecDeque<String>,
    abandoned: String,
    keep: usize,
) -> Vec<String> {
    history.push_back(abandoned);
    let mut evicted = Vec::new();
    while history.len() > keep.saturating_sub(1) {
        match history.pop_front() {
            Some(old) => evicted.push(old),
            None => break,
        }
    }
    evicted
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
    // Prefer the catalog display name; fall back to the member's slug
    // (catalog id) for empty members that have no catalog entry, matching
    // how the project tab strip labels the same member. The member matched
    // a real project folder, so it must get a project name — never fall
    // through to the solution name here.
    let catalog_name = store.read_with(cx, |s, _| {
        s.catalog()
            .iter()
            .find(|c| c.id == member.catalog_id)
            .map(|c| c.name.clone())
    });
    Some(SharedString::from(
        catalog_name.unwrap_or_else(|| member.catalog_id.0.clone()),
    ))
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

/// Re-export so the historical `crate::store::EFFORT_LEVELS` path (used by
/// `status_row` and the crate-root re-export) keeps resolving after the const
/// moved into `model_catalog`.
pub use crate::model_catalog::EFFORT_LEVELS;

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
                        // Mid-session teammate-pill reconcile: closes finished
                        // subagent streams the moment completion is provable, so a
                        // long busy (never-Idle) session doesn't strand pills until
                        // the →Idle GC (which only fires on the !Idle→Idle edge).
                        // Runs AFTER the reap so a terminal async agent that the
                        // reap already closed isn't re-examined.
                        this.reconcile_all_finished_teammate_streams(cx);
                        this.scan_parent_jsonls_for_completions(cx);
                        this.tick_background_shells(cx);
                        this.tick_supervisor(cx);
                        this.tick_stuck_sessions(cx);
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
            model_catalog: ModelCatalog::new(),
            focus_resolver: None,
            entry_update_throttles: HashMap::new(),
            entries_persist_chain: HashMap::new(),
            teammate_watchers: TeammateWatchers::new(),
            metrics_emitter: MetricsEmitter::new(),
            _solution_subscription: solution_subscription,
            _bg_agents_tick: Some(bg_agents_tick),
            supervisor_states: HashMap::new(),
            judge_sessions: HashMap::new(),
            auditor_sessions: HashMap::new(),
            backoff_timers: HashMap::new(),
            raw_transcript_history: HashMap::new(),
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

    pub fn set_persistence(&mut self, db: Arc<SolutionAgentDb>, cx: &mut Context<Self>) {
        self.persistence = Some(db.clone());
        // One-time load: merge persisted supervisor states into the in-memory
        // map. Uses `or_insert` semantics so any states already toggled before
        // the DB was ready are not clobbered.
        cx.spawn(async move |this, cx| {
            let states = db
                .load_supervisor_states()
                .await
                .log_err()
                .unwrap_or_default();
            this.update(cx, |this, _| {
                let now_ms = chrono::Utc::now().timestamp_millis();
                for mut st in states {
                    // Restart path: anchor the idle clock to now so a session
                    // that was idle before this process started (its persisted
                    // `last_activity_at` is stale) waits a full idle window
                    // before the first judge, rather than firing one the instant
                    // the editor reopens. Fresh in-session enables never take
                    // this path, so their immediate-idle semantics are unchanged.
                    st.watch_started_ms = Some(now_ms);
                    this.supervisor_states.entry(st.session_id).or_insert(st);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Reload session `id`'s persisted supervisor row into the in-memory map when
    /// the session is reopened IN-PROCESS (a soft/cold close evicted the runtime
    /// state via `evict_session_runtime_maps`, but `load_supervisor_states` only
    /// runs once at startup). Without this a reopened session shows the observer
    /// OFF even though its persisted row says `enabled` — silent unsupervision —
    /// and the stale `enabled=true` row then RESURRECTS supervision on the NEXT
    /// editor restart, on a session the user believed unsupervised (finding #5).
    /// No-op if the state is already live (never evicted / re-enabled since the
    /// close) so an in-session toggle isn't clobbered. Anchors `watch_started_ms
    /// = now` like the startup load so a pre-close idle session isn't judged the
    /// instant it reopens.
    fn reload_supervisor_state_for(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) {
        if self.supervisor_states.contains_key(&id) {
            return;
        }
        let Some(db) = self.persistence.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let states = db
                .load_supervisor_states()
                .await
                .log_err()
                .unwrap_or_default();
            this.update(cx, |this, _| {
                if this.supervisor_states.contains_key(&id) {
                    return;
                }
                if let Some(mut st) = states.into_iter().find(|s| s.session_id == id) {
                    st.watch_started_ms = Some(chrono::Utc::now().timestamp_millis());
                    this.supervisor_states.entry(id).or_insert(st);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Returns the database handle if set. Used by the navigator to list
    /// historic sessions (those persisted across editor restarts) for the
    /// "Resume" / "Continue last session" affordances.
    pub fn db(&self) -> Option<Arc<SolutionAgentDb>> {
        self.persistence.clone()
    }

    pub fn supervisor_state(
        &self,
        id: SolutionSessionId,
    ) -> Option<crate::supervisor::SupervisorState> {
        self.supervisor_states.get(&id).cloned()
    }

    pub fn set_supervision_enabled(
        &mut self,
        id: SolutionSessionId,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let state = self
            .supervisor_states
            .entry(id)
            .or_insert_with(|| crate::supervisor::SupervisorState::new(id));
        if state.enabled == enabled {
            return;
        }
        state.enabled = enabled;
        // Every enable/disable toggle clears the activity counters (fire count +
        // consecutive-continue cap) — a fresh on/off starts the tally over.
        state.trigger_count = 0;
        state.consecutive_continues = 0;
        if enabled {
            state.status = crate::supervisor::SupervisorStatus::Watching;
            state.consecutive_continues = 0;
            state.backoff_attempt = 0;
            // A fresh enable must never inherit a stale backoff gate from a
            // previous transient-failure run, or the watchdog would refuse to
            // fire until the old delay elapsed.
            state.next_eligible_ms = None;
            // Nor inherit transient markers from a previous run: a leftover
            // supersede flag, a nudge held for a since-departed draft, or a
            // parked wait would otherwise leak into the first fresh cycle.
            state.judge_superseded = false;
            state.pending_nudge = None;
            state.wait_until_ms = None;
            self.backoff_timers.remove(&id);
        } else {
            state.status = crate::supervisor::SupervisorStatus::Disabled;
            // Turning supervision OFF must take effect immediately, including on
            // work already in flight: discard any nudge held for the user to
            // stop typing, and mark so a verdict already racing out of a judge
            // we're about to tear down is dropped by `apply_verdict`'s send-time
            // gate (belt-and-suspenders with the `!enabled` check there).
            state.pending_nudge = None;
            state.wait_until_ms = None;
            state.judge_superseded = true;
            // `state` borrow ends here — the `self.*` teardown calls below need
            // `&mut self`.
            self.backoff_timers.remove(&id);
            self.clear_supervisor_question(id, cx);
            // Interrupt an in-flight observer: a judge/auditor mid-run would
            // otherwise keep running and deliver a nudge after the user already
            // switched supervision off. `hold_supervisor` tears the judge down
            // for the manual-Stop path; disable must do the same (plus the
            // auditor). Without this, disabling did NOT stop a running observer.
            self.finish_judge(id, cx);
            self.finish_auditor(id, cx);
        }
        self.persist_supervisor_state(id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    pub fn set_supervisor_prompt(
        &mut self,
        id: SolutionSessionId,
        prompt: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let prompt = prompt.filter(|p| !p.trim().is_empty());
        let changed = {
            let state = self
                .supervisor_states
                .entry(id)
                .or_insert_with(|| crate::supervisor::SupervisorState::new(id));
            let changed = state.custom_prompt != prompt;
            state.custom_prompt = prompt;
            changed
        };
        // Changing the supervisor's instruction makes an IN-FLIGHT judge's
        // verdict useless — it reviewed the conversation under the OLD
        // instruction. Rather than let it run to completion and drop the stale
        // verdict at send time, interrupt it now and return to `Watching` so the
        // next tick re-fires a fresh judge under the new instruction. `superseded`
        // covers a verdict already racing out of the torn-down judge.
        if changed && self.judge_sessions.contains_key(&id) {
            self.finish_judge(id, cx);
            if let Some(state) = self.supervisor_states.get_mut(&id) {
                state.judge_superseded = true;
                if matches!(state.status, crate::supervisor::SupervisorStatus::Judging) {
                    state.status = crate::supervisor::SupervisorStatus::Watching;
                }
            }
        }
        self.persist_supervisor_state(id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    /// Wipe the observer's durable memory (diary, verdicts, user-intent) AND
    /// reset its in-memory reasoning cursor for `id`, on a HUMAN-initiated
    /// `/clear` or `/compact`. Gives the supervisor a clean slate so it doesn't
    /// re-read stale notes or re-litigate settled directives after the user
    /// reset the thread. NOT invoked on an observer-issued `compact` verdict
    /// (that path keeps `user_intent.md`). See
    /// [`crate::supervisor::wipe_supervisor_memory`].
    pub(crate) fn wipe_supervisor_memory(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(root) = self.solution_root_for(id, cx) {
            let dir = crate::supervisor::supervisor_dir(&root, id);
            crate::supervisor::wipe_supervisor_memory(&dir);
        }
        // Reset the in-memory reasoning cursor too: the continue-loop counter
        // and any parked/one-shot verdict state, so a stale nudge or wait can't
        // fire against the freshly-cleared thread and the continue cadence
        // restarts from zero. Leaves identity/config (enabled, status,
        // custom_prompt, trigger_count) intact.
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            state.consecutive_continues = 0;
            state.pending_nudge = None;
            state.judge_superseded = false;
            state.wait_until_ms = None;
        }
        self.persist_supervisor_state(id, cx);
    }

    /// Drop all per-session in-memory runtime maps for `id`: supervisor control
    /// state, the background-agent / background-shell watcher tasks, the
    /// transient-failure backoff timer, the parent-jsonl scan cursor, and the
    /// per-entry update throttles. Shared by every session-teardown path
    /// (`close_session`, `cold_close_solution`, `gc_orphan_solutions`) so none of
    /// these maps accumulates stale entries over a long-lived editor process —
    /// each was previously only pruned on its own narrow path (or, for
    /// `supervisor_states`, never), leaking one entry per closed session.
    /// Does NOT touch the DB, emit events, release the pool, or reap an in-flight
    /// judge/auditor — callers handle those (`finish_judge`/`finish_auditor` must
    /// run separately while the supervised session is still reachable).
    fn evict_session_runtime_maps(&mut self, id: SolutionSessionId) {
        self.supervisor_states.remove(&id);
        self.teammate_watchers.forget_session(id);
        self.backoff_timers.remove(&id);
        self.entry_update_throttles.retain(|(sid, _), _| *sid != id);
        // Drop the persist-serialization chain: a hard teardown abandons any
        // in-flight entry-row write (the session's rows are being purged anyway).
        self.entries_persist_chain.remove(&id);
        // The metrics throttle map is keyed by session id and is otherwise
        // never pruned — one entry would leak per closed session for the
        // editor's whole lifetime.
        self.metrics_emitter.clear_session(&id);
        self.raw_transcript_history.remove(&id);
    }

    /// Record `abandoned_acp_id` (the ACP session orphaned by a `/compact`
    /// rotation or `/clear`) for session `id` and delete claude's on-disk JSONL
    /// transcripts that fall outside the keep-window — the live transcript plus
    /// the last `KEEP_RAW_TRANSCRIPTS - 1` abandoned ones. Each ACP session
    /// leaves a `~/.claude/projects/<cwd>/<acp_session_id>.jsonl` (+ a
    /// `<acp_session_id>/` subagents dir) that claude never cleans up, so a
    /// multi-day session would otherwise accrue gigabytes of dead transcripts.
    /// Best-effort: any IO error is ignored. Keyed by our `SolutionSessionId`,
    /// so only THIS session's transcripts are ever deleted even when several
    /// sessions share the same project cwd.
    fn prune_raw_transcripts(
        &mut self,
        id: SolutionSessionId,
        abandoned_acp_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(cwd) = self.sessions.get(&id).map(|s| s.read(cx).cwd.clone()) else {
            return;
        };
        if cwd.as_os_str().is_empty() {
            return;
        }
        let history = self.raw_transcript_history.entry(id).or_default();
        let evicted = push_and_evict_transcripts(history, abandoned_acp_id, KEEP_RAW_TRANSCRIPTS);
        for old in evicted {
            if let Some(jsonl) = parent_session_jsonl_for(&cwd, &old) {
                let _ = std::fs::remove_file(jsonl);
            }
            if let Some(proj) = claude_project_dir_for(&cwd) {
                let _ = std::fs::remove_dir_all(proj.join(&old));
            }
        }
    }

    /// Called when the HUMAN sends a message into a supervised session. Three
    /// effects, all keyed off the supervisor's current status:
    ///
    /// * resets the consecutive-continue counter (cap / audit cadence restart);
    /// * resumes a `WaitingUser` pause (the human answered the `ask`) → `Watching`;
    /// * **re-arms a `Stopped(Done)` session**: the supervisor had declared the
    ///   task complete and auto-disabled itself, but a new user message means the
    ///   work continues, so supervision re-enables → `Watching`. Re-arm is scoped
    ///   to `Done` only — a user-driven toggle-off (`Disabled`) stays off, and a
    ///   `Quota` / `ProviderError` stop is an infra wall we don't auto-retry here.
    pub(crate) fn reset_supervisor_continue_counter(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        use crate::supervisor::{StoppedReason, SupervisorStatus};
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            let was_done = matches!(state.status, SupervisorStatus::Stopped(StoppedReason::Done));
            let waiting = matches!(state.status, SupervisorStatus::WaitingUser);
            // A `Held` session (the user manually stopped the agent) re-arms on
            // the next human message — the user has decided to continue.
            let held = matches!(state.status, SupervisorStatus::Held);
            if state.consecutive_continues == 0 && !waiting && !was_done && !held {
                return;
            }
            state.consecutive_continues = 0;
            if held {
                // Leaving Held: clear any stale backoff so the watchdog can fire
                // on the next idle once the agent finishes the new turn.
                state.next_eligible_ms = None;
            }
            if was_done {
                // Re-arm: Done auto-disabled supervision; the user is continuing
                // the work, so restore their original enabled intent. Clear any
                // stale backoff so the watchdog can fire on the next idle.
                state.enabled = true;
                state.backoff_attempt = 0;
                state.next_eligible_ms = None;
            }
            if state.enabled {
                state.status = SupervisorStatus::Watching;
            }
            if was_done || held {
                self.backoff_timers.remove(&id);
            }
            self.persist_supervisor_state(id, cx);
            self.clear_supervisor_question(id, cx);
            cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
        }
    }

    /// Re-arm supervision when a PARKED session resumes **on its own** — the
    /// agent produced genuinely-new activity (a self-scheduled monitor /
    /// `ScheduleWakeup` fired and continued the work, or a background task the
    /// editor doesn't track came back) while the supervisor was parked. Two park
    /// states rest on the premise "nothing moves until the human acts" and are
    /// falsified by a self-resume, so they return to `Watching`:
    /// * `WaitingUser` — an `ask` escalation ("Waiting for you").
    /// * `Held` **when `held_by_done`** — a `done` verdict parked it ("On hold").
    ///
    /// A `Held` set by a MANUAL user Stop (`held_by_done == false`) is
    /// deliberately EXCLUDED: only a human message may resume that (the "don't
    /// drag it back before I decide" rule). This is the whole reason `held_by_done`
    /// exists — `done` and manual-stop share the `Held` status but must not share
    /// self-resume behaviour. No-op in any other state (already `Watching`,
    /// `Disabled`, `Quota`/`ProviderError`), so it's safe to call on every agent
    /// entry — the first self-resume entry re-arms and the rest early-return.
    ///
    /// Distinct from [`reset_supervisor_continue_counter`] (the HUMAN-message
    /// re-arm, which resumes ALL of `Held`/`WaitingUser`/`Done`): a self-resume is
    /// narrower — it must not resume a manual stop.
    pub(crate) fn rearm_supervisor_on_self_activity(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        use crate::supervisor::SupervisorStatus;
        {
            let Some(state) = self.supervisor_states.get_mut(&id) else {
                return;
            };
            let waiting = matches!(state.status, SupervisorStatus::WaitingUser);
            let done_hold =
                matches!(state.status, SupervisorStatus::Held) && state.held_by_done;
            if !waiting && !done_hold {
                return;
            }
            state.consecutive_continues = 0;
            state.next_eligible_ms = None;
            state.held_by_done = false;
            // Both park states keep `enabled == true`; if the user disabled
            // supervision the status would be `Disabled`, filtered out above. So
            // the session is always eligible to return to active watching.
            if state.enabled {
                state.status = SupervisorStatus::Watching;
            }
        }
        self.backoff_timers.remove(&id);
        self.persist_supervisor_state(id, cx);
        self.clear_supervisor_question(id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    /// Park the supervisor in `Held` because the HUMAN manually stopped the
    /// agent (Stop button / `cancel_turn`). Supervision stays enabled but must
    /// not re-engage on the current dialog state — no judge, no nudge — until the
    /// next human message re-arms it (`reset_supervisor_continue_counter`). This
    /// is the fix for "I stopped the agent myself; don't let the observer drag it
    /// back to work before I decide to continue." No-op unless supervision is
    /// enabled and currently `Watching`/`Judging`. Any in-flight judge is torn
    /// down so a verdict already in flight can't nudge the agent after the stop.
    pub(crate) fn hold_supervisor(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) {
        use crate::supervisor::SupervisorStatus;
        let should_hold = self.supervisor_states.get(&id).is_some_and(|s| {
            s.enabled
                && matches!(
                    s.status,
                    SupervisorStatus::Watching | SupervisorStatus::Judging
                )
        });
        if !should_hold {
            return;
        }
        // Tear down an in-flight judge AND a racing meta-auditor: at user-stop
        // time the session is usually Running (status Watching, no judge), but if
        // a judge/auditor had just spawned it would otherwise still deliver a
        // verdict and nudge/escalate the agent after the user stopped it. (The
        // auditor spawns while `Watching`, so `finish_judge` alone misses it — a
        // late audit `escalate` would force `WaitingUser`, which self-resumes,
        // dragging back the very session the user manually stopped.)
        self.finish_judge(id, cx);
        self.finish_auditor(id, cx);
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            state.status = SupervisorStatus::Held;
            // A MANUAL stop — NOT done-sourced. Only a human message may resume
            // it; a self-resume must NOT re-arm it (the "don't drag it back"
            // rule). Set explicitly so a stale `held_by_done` can't leak in.
            state.held_by_done = false;
            state.next_eligible_ms = None;
        }
        self.backoff_timers.remove(&id);
        self.persist_supervisor_state(id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    /// A HUMAN reply into a supervised session that is mid-`Judging` supersedes
    /// the in-flight judge: the user has taken over direction, so the judge's
    /// pending verdict is stale and must not nudge the agent afterwards. Tear
    /// the judge down (its verdict, if it still races in via the MCP tool, is
    /// then dropped by [`apply_verdict`]'s staleness guard) and return
    /// supervision to `Watching`. No-op unless a judge is actually in flight.
    ///
    /// Distinct from [`hold_supervisor`] (manual STOP → `Held`, supervision
    /// stands by): a reply means "keep working on what I just said", so normal
    /// watching resumes rather than parking. Called from the single user-send
    /// funnel ([`send_message_blocks_targeted`] with `from_user`), alongside
    /// `reset_supervisor_continue_counter` — separate because that reset
    /// early-returns when `consecutive_continues == 0`, which would skip the
    /// FIRST judge of a session.
    pub(crate) fn supersede_judge_on_user_reply(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        use crate::supervisor::SupervisorStatus;
        if !self.judge_sessions.contains_key(&id) {
            return;
        }
        self.finish_judge(id, cx);
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            // Mark so a verdict that already left the judge (racing the
            // teardown) is dropped by `apply_verdict`'s guard.
            state.judge_superseded = true;
            // The user answered for themselves, so any Observer nudge parked for
            // the "user stopped typing" flush is now stale — forget it, don't
            // deliver it after the user's own message.
            state.pending_nudge = None;
            if matches!(state.status, SupervisorStatus::Judging) {
                state.status = SupervisorStatus::Watching;
            }
        }
        self.persist_supervisor_state(id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    /// Note that the HUMAN is typing into `id`'s compose box. Pushes the
    /// supervisor's idle clock forward (transient `last_user_input_ms`), so the
    /// watchdog treats the session as active for another `IDLE_THRESHOLD_SECS`
    /// and never fires a nudge while the user is mid-message. Cheap + frequent
    /// (one per keystroke burst): in-memory only, no persist, no event.
    pub(crate) fn note_user_input(&mut self, id: SolutionSessionId) {
        if let Some(state) = self.supervisor_states.get_mut(&id)
            && state.enabled
        {
            state.last_user_input_ms = Some(chrono::Utc::now().timestamp_millis());
        }
    }

    /// Clear the pending supervisor question banner for session `id`. Emits
    /// `SessionStateChanged` only when the field was actually set (avoids a
    /// spurious notify when it was already `None`).
    pub(crate) fn clear_supervisor_question(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.session(id) {
            let was_set = session.read(cx).supervisor_question.is_some();
            if was_set {
                session.update(cx, |s, _| s.supervisor_question = None);
                cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
            }
        }
    }

    /// Escalate a supervisor question to the user: set `WaitingUser`, store
    /// the question on the session for the banner, surface it in-chat as an
    /// agent-invisible Observer bubble, fire a high-priority desktop
    /// notification, and emit `SessionStateChanged`.
    pub(crate) fn escalate_to_user(
        &mut self,
        id: SolutionSessionId,
        question: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            state.status = crate::supervisor::SupervisorStatus::WaitingUser;
        }
        self.persist_supervisor_state(id, cx);
        if let Some(session) = self.session(id) {
            session.update(cx, |s, _| {
                s.supervisor_question = Some(question.clone().into())
            });
        }
        // Surface the observer's question in the transcript as an Observer
        // bubble (FORK.md #29 render — eye badge, Accent). It is a `SystemNote`,
        // so it is DISPLAY-ONLY: the user reads it in-chat but the working agent
        // never sees it (not sent to the agent / not in its transcript). This
        // complements the persistent `supervisor_question` banner + the desktop
        // toast below, so the ask isn't lost when the toast is dismissed.
        self.push_system_note(
            id,
            acp_thread::SystemNoteLevel::Observer,
            question.clone(),
            cx,
        );
        let title = "Sawe — Supervisor".to_string();
        let body = format!("🛡 {question}");
        crate::notifier::dispatch_raw(
            id,
            crate::notifier::NotifyKind::AwaitingInput,
            &title,
            &body,
            cx,
        );
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
    }

    /// Notify the user that the supervisor concluded the turn — a genuine
    /// completion (`is_park == false`) or a PARK awaiting the operator
    /// (`is_park == true`). `reason` is the human-facing body with the internal
    /// `PARK:` marker already stripped (see `classify_done_reasoning`), so a park
    /// is never announced as "Work complete" and the raw token never leaks.
    pub(crate) fn notify_supervisor_done(
        &mut self,
        id: SolutionSessionId,
        is_park: bool,
        reason: &str,
        cx: &mut Context<Self>,
    ) {
        let label = if is_park {
            "⏸ Parked — awaiting you"
        } else {
            "✓ Work complete"
        };
        // In-chat Observer bubble (display-only, agent-invisible — see
        // `escalate_to_user`) so the verdict is visible in the transcript, not
        // just as a transient desktop toast.
        self.push_system_note(
            id,
            acp_thread::SystemNoteLevel::Observer,
            format!("{label}: {reason}"),
            cx,
        );
        let title = "Sawe — Supervisor".to_string();
        let body = format!("{label}: {reason}");
        // A park is "the agent is blocked on YOU" — the same attention class as
        // `AwaitingInput` (→ high-priority toast), not a genuine completion.
        let kind = if is_park {
            crate::notifier::NotifyKind::AwaitingInput
        } else {
            crate::notifier::NotifyKind::Completed
        };
        crate::notifier::dispatch_raw(id, kind, &title, &body, cx);
    }

    /// Send a supervisor-generated nudge message. Unlike the public
    /// [`send_message`](crate::store::queue) entry point, this does NOT reset
    /// the consecutive-continue counter — supervisor nudges must never clear
    /// the guard that was incremented just before the nudge was issued.
    fn send_supervisor_nudge(
        &mut self,
        id: SolutionSessionId,
        content: String,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        // "Hold on typing": if the human is composing a message RIGHT NOW (a
        // keystroke within `IDLE_THRESHOLD_SECS`), do not drop the nudge into
        // the conversation mid-sentence. The verdict has already been accepted
        // by `apply_verdict` (the continue-counter bumped); park its nudge in
        // `pending_nudge` and let `tick_supervisor` deliver it once the user has
        // gone quiet for the standard idle window — or a genuine user SEND
        // discards it (`supersede_judge_on_user_reply`). The start-time guard
        // (`should_fire`) only blocks a NEW judge from firing while the user
        // types; it cannot cover a judge that fired while the user was idle and
        // finished after the user began typing — this is that missing seam.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let composing = self
            .supervisor_states
            .get(&id)
            .and_then(|s| s.last_user_input_ms)
            .is_some_and(|t| {
                now_ms.saturating_sub(t) < (crate::supervisor::IDLE_THRESHOLD_SECS as i64) * 1000
            });
        if composing {
            if let Some(state) = self.supervisor_states.get_mut(&id) {
                state.pending_nudge = Some(content);
            }
            cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
            return gpui::Task::ready(Ok(()));
        }
        self.deliver_nudge_now(id, content, cx)
    }

    /// Deliver an Observer nudge into the supervised session's conversation
    /// unconditionally (the hold-on-typing decision lives in the callers:
    /// [`send_supervisor_nudge`] parks it when the user is mid-message,
    /// `tick_supervisor` flushes a parked nudge once the user is quiet).
    fn deliver_nudge_now(
        &mut self,
        id: SolutionSessionId,
        content: String,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        // The nudge is the SINGLE visible element: stamp it with the
        // `spk_observer_nudge` `_meta` marker so `conversation_render` shows it
        // as an OBSERVER comment (eye plaque) instead of a plain user bubble. We
        // no longer emit a separate "Наблюдатель направил агента: …" breadcrumb
        // note — the marked message itself carries the full instruction and the
        // observer attribution, so the old two-element layout (gist note + plain
        // bubble) is gone. The marker rides on `_meta`, invisible to the agent's
        // text.
        let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
            agent_client_protocol::schema::TextContent::new(content)
                .meta(Some(acp_thread::meta_with_observer_nudge())),
        )];
        // `from_user: false` — a supervisor nudge must NOT reset the
        // continue-cap counter (apply_verdict just incremented it) nor be
        // mistaken for a human reply that resumes a `WaitingUser` pause.
        self.send_message_blocks_targeted(id, blocks, crate::model::QueueTarget::Main, false, cx)
    }

    /// Recovery sweep for wedged sessions. A session stuck in `Running` with
    /// no streaming / tool activity for [`STUCK_TURN_SECS`] has a hung or dead
    /// claude subprocess: a healthy turn streams thinking / text / tool calls
    /// well within that window, each of which bumps `last_activity_at` (so the
    /// silence clock self-resets on any progress). A cleanly *exited*
    /// subprocess is already recovered by the connection's EOF path (it fails
    /// the pending prompt → `Errored`); this catches the harder hung-but-alive
    /// case the EOF path can't see.
    ///
    /// A genuinely-busy turn IS distinguishable from a hang: when claude blocks
    /// on a slow FOREGROUND command it leaves that tool call in
    /// [`ToolCallStatus::InProgress`] for the command's whole duration. So we
    /// only treat a silent turn as wedged when there is NO in-progress tool call.
    /// When there IS one we leave it alone until that single tool has both
    /// exceeded an unreasonable [`TOOL_STUCK_SECS`] AND stopped showing liveness
    /// — no output for [`TOOL_OUTPUT_SILENCE_SECS`] and no running OS process. A
    /// display-only command's output each bumps `last_activity_at` (it rides a
    /// `ToolCallUpdate`), so `silent_secs` already measures how long ago it last
    /// printed; a real client-side PTY's output does not, so that path is covered
    /// by [`acp_thread::Terminal::is_process_running`]. A build/deploy that keeps
    /// printing (or whose process is alive) is therefore never reconnected out
    /// from under itself (hardening #7); only one that truly hangs (silent, no
    /// live process) is. Background (`run_in_background`) commands don't block
    /// claude, so they keep streaming and never get here.
    ///
    /// Recovery is [`reconnect_agent`] — non-destructive: it respawns the
    /// subprocess and replays the same transcript, keeping the conversation.
    /// `reconnect_agent` synchronously flips the session out of `Running` (to
    /// `Errored("reconnecting…")`), so the next tick won't re-fire for it.
    pub(crate) fn tick_stuck_sessions(&mut self, cx: &mut Context<Self>) {
        use crate::model::SessionState;
        use acp_thread::{AgentThreadEntry, AssistantMessageChunk, ToolCallStatus};
        let now = Utc::now();
        // Each wedged session is tagged with `Some(limit_message)` when its
        // stall is actually a usage/session/weekly-limit wall rather than a hung
        // subprocess — those are recovered differently (see the loop below).
        let stuck: Vec<(SolutionSessionId, Option<String>)> = self
            .sessions
            .iter()
            .filter_map(|(id, session)| {
                let s = session.read(cx);
                // Live, project-backed, non-ephemeral sessions mid-turn only.
                // (Ephemeral judge/auditor sessions are short-lived and cold /
                // prebuilt sessions have nothing to reconnect.)
                if s.is_supervisor_ephemeral
                    || s.project.is_none()
                    || !matches!(s.state, SessionState::Running { .. })
                {
                    return None;
                }
                let thread = s.acp_thread()?;
                let silent_secs = now.signed_duration_since(s.last_activity_at).num_seconds();
                // Not silent long enough yet → claude is clearly alive.
                if silent_secs < STUCK_TURN_SECS as i64 {
                    return None;
                }
                let thread_ref = thread.read(cx);
                // The most-recent in-progress tool call as `(secs_running,
                // shows_liveness)`. `None` = no tool is executing right now. A
                // foreground build/deploy is "alive" — and so must NOT be
                // reconnected out from under itself (hardening #7) — when its OS
                // process is still running (real client-side PTY) OR it printed
                // within `TOOL_OUTPUT_SILENCE_SECS`. For the display-only path
                // (claude-acp) each output chunk bumps `last_activity_at`, so
                // `silent_secs` is exactly the time since the command last
                // printed; the real-PTY path's output does not, so it's covered
                // by the direct process check.
                let active_tool = thread_ref.entries().iter().rev().find_map(|e| match e {
                    AgentThreadEntry::ToolCall(tc)
                        if matches!(tc.status, ToolCallStatus::InProgress) =>
                    {
                        let since = tc.status_started_at.unwrap_or(s.last_activity_at);
                        let tool_secs = now.signed_duration_since(since).num_seconds();
                        let pty_running =
                            tc.terminals().any(|term| term.read(cx).is_process_running(cx));
                        let shows_liveness = pty_running
                            || silent_secs < TOOL_OUTPUT_SILENCE_SECS as i64;
                        Some((tool_secs, shows_liveness))
                    }
                    _ => None,
                });
                if !turn_is_wedged(active_tool) {
                    return None;
                }
                // Distinguish a usage/session-limit WALL from a genuine hang: a
                // turn that hit the limit prints the wall as its last assistant
                // message and then stalls (nothing ends the turn), so it looks
                // silent-and-wedged. Reconnecting + "carry on" there just re-hits
                // the wall and burns more quota (observed loop: repeated "You've
                // hit your session limit" + a spurious "your process hung,
                // continue" nudge). Detect it by scanning the latest assistant
                // message so the loop can route it to quota recovery instead.
                let limit_message = thread_ref
                    .entries()
                    .iter()
                    .rev()
                    .find_map(|e| match e {
                        AgentThreadEntry::AssistantMessage(m) => Some(
                            m.chunks
                                .iter()
                                .map(|chunk| match chunk {
                                    AssistantMessageChunk::Message { block }
                                    | AssistantMessageChunk::Thought { block } => {
                                        block.to_markdown(cx)
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(" "),
                        ),
                        _ => None,
                    })
                    .filter(|text| crate::supervisor::is_usage_limit_error(text));
                Some((*id, limit_message))
            })
            .collect();
        for (id, limit_message) in stuck {
            match limit_message {
                // Usage/session-limit wall, NOT a hang: stop the runaway turn and
                // hand off to the shared quota handler (auto-resume at the reset
                // time if supervised, else `Stopped(Quota)`) instead of
                // reconnecting + nudging "continue" (which re-hits the wall).
                Some(message) => {
                    log::warn!(
                        target: "solution_agent::store",
                        "session={id} stalled on a usage/session-limit wall (not a hang) — \
                         stopping turn + scheduling quota recovery, no reconnect"
                    );
                    self.append_supervisor_diary_note(
                        id,
                        "turn hit a usage/session limit while Running; stopped (no reconnect), \
                         quota recovery scheduled",
                        cx,
                    );
                    self.mutate_state(
                        id,
                        |st| *st = SessionState::Errored(SharedString::from(message.clone())),
                        cx,
                    );
                    self.push_system_note(
                        id,
                        acp_thread::SystemNoteLevel::Error,
                        "Достигнут лимит claude — текущий ход остановлен (без переподключения).",
                        cx,
                    );
                    self.apply_usage_limit_stop(id, &message, cx);
                }
                // Genuine hang: reconnect (respawn subprocess + replay transcript).
                None => {
                    log::warn!(
                        target: "solution_agent::store",
                        "session={id} wedged in Running (no progress {STUCK_TURN_SECS}s, no live \
                         tool — or a tool in-progress >{TOOL_STUCK_SECS}s with no output for \
                         >{TOOL_OUTPUT_SILENCE_SECS}s) — auto-reconnecting (respawn subprocess + \
                         replay transcript)"
                    );
                    self.append_supervisor_diary_note(
                        id,
                        "session wedged while Running (hung subprocess); auto-reconnect",
                        cx,
                    );
                    self.reconnect_agent(id, cx).detach_and_log_err(cx);
                }
            }
        }
    }

    pub(crate) fn tick_supervisor(&mut self, cx: &mut Context<Self>) {
        use crate::model::SessionState;
        let now_ms = chrono::Utc::now().timestamp_millis();

        // Auditor-stuck sweep: a meta-auditor spawns while the supervised
        // session is `Watching` (not `Judging`), so the judge-stuck timeout in
        // the per-session loop below never catches it. An auditor that
        // errors/ends WITHOUT calling `supervisor_audit_verdict` would leave its
        // `auditor_sessions` handle live forever and permanently disable
        // meta-audit for that session. Clean up any auditor older than the
        // timeout so the next audit cycle can spawn fresh. No supervision
        // backoff is applied — the auditor failing is not the judge failing.
        let stale_auditors: Vec<SolutionSessionId> = self
            .auditor_sessions
            .iter()
            .filter(|(_, handle)| {
                now_ms.saturating_sub(handle.started_ms)
                    >= (crate::supervisor::AUDITOR_TIMEOUT_SECS as i64) * 1000
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale_auditors {
            self.finish_auditor(id, cx);
            self.append_supervisor_diary_note(
                id,
                "meta-auditor timed out / ended without verdict; handle cleaned up",
                cx,
            );
        }

        let session_ids: Vec<SolutionSessionId> = self
            .supervisor_states
            .iter()
            .filter(|(_, st)| st.enabled)
            .map(|(id, _)| *id)
            .collect();
        for id in session_ids {
            let Some(state) = self.supervisor_states.get(&id) else {
                continue;
            };
            let enabled = state.enabled;
            let status = state.status.clone();
            let next_eligible_ms = state.next_eligible_ms;
            let last_fired_at = state.last_fired_at;
            let last_user_input_ms = state.last_user_input_ms;
            // Anchors the idle clock to "since this process started watching",
            // set only on the restart/load path (`set_persistence`). A session
            // whose supervision was enabled fresh THIS process leaves it `None`
            // (no baseline → normal immediate idle semantics).
            let watch_started_ms = state.watch_started_ms;

            // Judge-stuck watchdog: a judge that errored / ended WITHOUT calling
            // its verdict tool leaves the session pinned in `Judging` forever
            // (finish_judge never ran), so the watchdog would never re-fire.
            // Treat an over-long `Judging` window as a transient failure. This
            // single timeout uniformly catches crash, error, AND silent-end
            // without per-judge session-state subscriptions.
            if matches!(status, crate::supervisor::SupervisorStatus::Judging) {
                // A `Judging` status with no `last_fired_at` is a corrupt/phantom
                // wedge (currently unreachable — every fire sets it and the DB
                // load coerces `Judging`+`None` → `Watching`). Treat `None` as
                // "already stuck" (`0`, i.e. infinitely old) so it un-wedges
                // immediately rather than being pinned forever by `now_ms`.
                let fired_at = last_fired_at.unwrap_or(0);
                let stuck_ms = now_ms.saturating_sub(fired_at);
                if stuck_ms >= (crate::supervisor::JUDGE_TIMEOUT_SECS as i64) * 1000 {
                    // `Some(judge_id)` = a real judge handle is registered (the
                    // inner `judge_id` may still be `None` if its session hasn't
                    // been created yet); `None` = phantom (spawn early-returned).
                    if let Some(judge_id) = self.judge_sessions.get(&id).map(|h| h.judge_id) {
                        // LIVENESS (finding #5): the wall-clock timeout is crossed,
                        // but don't kill a judge that is still demonstrably working.
                        // Check the judge SESSION's own activity clock — a streaming
                        // judge bumps it on every thinking/text/tool event — and
                        // extend while it progresses, up to a hard cap that still
                        // catches a runaway (infinite-thinking) judge.
                        let judge_silent_ms = judge_id
                            .and_then(|jid| self.session(jid))
                            .map(|js| {
                                now_ms.saturating_sub(
                                    js.read(cx).last_activity_at.timestamp_millis(),
                                )
                            });
                        let judge_alive = judge_silent_ms.is_some_and(|ms| {
                            ms < (crate::supervisor::JUDGE_LIVENESS_SILENCE_SECS as i64) * 1000
                        });
                        let under_hard_cap =
                            stuck_ms < (crate::supervisor::JUDGE_HARD_TIMEOUT_SECS as i64) * 1000;
                        if judge_alive && under_hard_cap {
                            // Still streaming — leave it be; re-check next tick.
                            continue;
                        }
                        // A real judge that timed out / ended / went silent without
                        // a verdict. If it stalled on the usage wall (its own
                        // transcript shows it — the judge hits the same account wall
                        // as the worker), route to QUOTA recovery (schedule the
                        // reset-time resume) instead of a transient failure that
                        // spirals to a false `Stopped(ProviderError)` (finding #3).
                        // Otherwise it's a genuine timeout.
                        let message = self
                            .judge_wall_message(id, cx)
                            .unwrap_or_else(|| "judge timed out / ended without verdict".to_string());
                        self.on_judge_failed(id, message, cx);
                    } else {
                        // PHANTOM `Judging` (finding #2): the fire set `Judging` +
                        // `last_fired_at`, but the judge SPAWN early-returned (a
                        // cold session with no project / no live thread), so no
                        // judge handle was ever registered. This is NOT a timed-out
                        // judge — charging it as a transient failure would, over
                        // repeated phantoms, spiral to a FALSE
                        // `Stopped(ProviderError)` that silently kills supervision
                        // (and breaks the "продолжит автоматически" quota promise on
                        // a cold-restored tab). Un-wedge to `Watching` with NO
                        // penalty; the fire re-engages once the session warms up.
                        if let Some(st) = self.supervisor_states.get_mut(&id) {
                            st.status = crate::supervisor::SupervisorStatus::Watching;
                            st.last_fired_at = None;
                        }
                        self.persist_supervisor_state(id, cx);
                        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
                    }
                }
                continue;
            }

            let Some(session) = self.session(id) else {
                continue;
            };
            let (idle_or_errored, last_activity_ms, has_live_background_work) = {
                let s = session.read(cx);
                let idle_or_errored =
                    matches!(s.state, SessionState::Idle | SessionState::Errored(_));
                // A session sitting idle OVER a background command/agent it
                // launched is legitimately idle — the agent is waiting on that
                // work, so there is nothing for the supervisor to judge. Live =
                // any background shell still `Running`, or any managed agent that
                // has not hit a terminal stop.
                let has_live_background_work =
                    s.background_shells.values().any(|sh| {
                        matches!(
                            sh.state,
                            crate::background_shell::ShellRuntimeState::Running
                        )
                    }) || s.background_agents.values().any(|a| a.is_messageable());
                (
                    idle_or_errored,
                    s.last_activity_at.timestamp_millis(),
                    has_live_background_work,
                )
            };

            // Don't fire the supervisor while a background command/agent is
            // running: the agent's idleness is expected (it's waiting on that
            // work), and hung background work is already watched elsewhere (the
            // background-shell watcher + the `Running`-stuck watchdog). Stay
            // quiet like we do while the user types; the supervisor re-engages
            // once the work finishes and the agent goes genuinely idle. This is
            // what keeps the judge from firing a stream of `wait` verdicts over
            // a session parked on a long build/test.
            if has_live_background_work {
                continue;
            }
            // Treat live human typing as activity: the supervisor's idle clock
            // counts silence from the LATER of the session's last activity and
            // the user's last keystroke, so a nudge never fires while the user
            // is mid-message (note_user_input bumps `last_user_input_ms`).
            let quiet_since_ms = last_activity_ms.max(last_user_input_ms.unwrap_or(0));

            // Inherited idle after a restart is left ALONE until a manual kick.
            // `watch_started_ms` is stamped only on the restart/load path; a
            // session whose last activity predates it was already parked when the
            // editor closed, so the supervisor must NOT auto-resume it on reopen
            // — the operator resumes each session by hand, and only once it
            // produces genuinely-new activity THIS process (a manual kick starts
            // a turn, which bumps `last_activity_at` past the baseline) does the
            // normal idle-nudge cycle re-engage. `None` (a fresh in-session
            // enable) is always eligible — its idle arose under our watch.
            let eligible_for_watch = watch_started_ms
                .is_none_or(|baseline| last_activity_ms > baseline)
                // A session with a SCHEDULED fire (`next_eligible_ms` — a
                // usage-limit auto-resume or a transient-failure backoff) is not
                // plain inherited idle: the schedule is explicit intent. Honor
                // it across a restart, or `watch_started_ms` would gate it out
                // forever and silently break the "observer will auto-continue at
                // HH:MM" promise (the plain-idle "manual kick" rule still applies
                // to sessions with no schedule).
                || next_eligible_ms.is_some();

            // Flush a nudge that was held because the user was typing when the
            // judge finished (`send_supervisor_nudge`'s hold-on-typing). Deliver
            // it once the user has been quiet for the standard idle window and
            // the session is idle — the "user changed their mind and stopped
            // writing" case. A genuine user SEND would already have discarded it
            // via the `from_user` funnel. Never fire a FRESH judge while a nudge
            // is still parked (that would double up), so `continue` regardless.
            let has_pending = self
                .supervisor_states
                .get(&id)
                .is_some_and(|s| s.pending_nudge.is_some());
            if has_pending {
                // The held nudge only applies while we're still actively
                // `Watching`. If the session moved to a paused state since the
                // nudge was parked (user hit Stop → `Held`, supervisor
                // escalated → `WaitingUser`, quota → `Stopped`, disabled), it is
                // stale — DROP it rather than dragging the agent back to work
                // after the user paused it. (Mirrors the wait-wake `Watching`
                // gate below; the pause paths — `hold_supervisor` etc. — don't
                // all clear `pending_nudge` themselves.)
                if !matches!(status, crate::supervisor::SupervisorStatus::Watching) {
                    if let Some(st) = self.supervisor_states.get_mut(&id) {
                        st.pending_nudge = None;
                    }
                    continue;
                }
                let quiet_enough = now_ms.saturating_sub(quiet_since_ms)
                    >= (crate::supervisor::IDLE_THRESHOLD_SECS as i64) * 1000;
                if idle_or_errored && quiet_enough {
                    let pending = self
                        .supervisor_states
                        .get_mut(&id)
                        .and_then(|s| s.pending_nudge.take());
                    if let Some(content) = pending {
                        self.deliver_nudge_now(id, content, cx).detach();
                        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
                    }
                }
                continue;
            }

            // One-shot `wait`: a judge that decided "the agent is waiting on X,
            // park until here" committed a single timeout the mechanism honors
            // in full — no re-judging in between (re-deciding an unchanged wait
            // is the poll we're eliminating). While the deadline is in the
            // future, stay quiet. When it elapses, the mechanism itself wakes
            // the agent (a deterministic "check the result" nudge, only if it's
            // idle — if it already resumed we just drop the wait and let the
            // normal cycle judge the new state). Gated on `Watching` so a stale
            // deadline on a Held/WaitingUser session can't act.
            if matches!(status, crate::supervisor::SupervisorStatus::Watching)
                && let Some(wake_at) = self
                    .supervisor_states
                    .get(&id)
                    .and_then(|s| s.wait_until_ms)
            {
                if now_ms < wake_at {
                    continue;
                }
                if let Some(st) = self.supervisor_states.get_mut(&id) {
                    st.wait_until_ms = None;
                }
                if idle_or_errored {
                    self.deliver_nudge_now(
                        id,
                        "The task you were waiting on should be done by now — \
                         check the result and continue."
                            .to_string(),
                        cx,
                    )
                    .detach();
                }
                self.persist_supervisor_state(id, cx);
                cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
                continue;
            }

            if now_ms >= next_eligible_ms.unwrap_or(0)
                && eligible_for_watch
                && crate::supervisor::should_fire(
                    enabled,
                    &status,
                    idle_or_errored,
                    quiet_since_ms,
                    now_ms,
                    crate::supervisor::IDLE_THRESHOLD_SECS,
                )
            {
                if let Some(st) = self.supervisor_states.get_mut(&id) {
                    st.status = crate::supervisor::SupervisorStatus::Judging;
                    // Fresh judge cycle: clear any stale supersede marker from a
                    // prior reply whose judge never emitted, so this verdict
                    // isn't pre-suppressed (bug #1).
                    st.judge_superseded = false;
                    st.last_fired_at = Some(now_ms);
                    // One more supervisor firing — surfaced next to the status
                    // icon. Reset on enable/disable toggle.
                    st.trigger_count = st.trigger_count.saturating_add(1);
                    // We've consumed the backoff window; clear the gate so a stale
                    // value can't block a later eligible fire.
                    st.next_eligible_ms = None;
                }
                self.backoff_timers.remove(&id);
                self.persist_supervisor_state(id, cx);
                cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
                self.spawn_judge(id, cx);
            }
        }
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

    /// Create a hidden one-shot session for an internal AI helper (commit-message
    /// generation, AI conflict-resolve, etc.). Unlike `create_session` it is NOT
    /// pinned into the tab strip and emits no `SessionCreated`, so its brief
    /// lifetime never surfaces a (possibly orphaned) console tab.
    pub fn create_ephemeral_session(
        &mut self,
        solution_id: SolutionId,
        agent_id: AgentServerId,
        project: Entity<project::Project>,
        cx: &mut Context<Self>,
    ) -> Task<Result<SolutionSessionId>> {
        self.create_session_with_parent(
            solution_id,
            agent_id,
            project,
            None,  // cwd
            None,  // parent_session_id
            None,  // model
            None,  // effort
            false, // ephemeral_supervisor
            true,  // ephemeral
            cx,
        )
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
            false,
            false,
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
        // True only for the supervisor's hidden judge/auditor sessions. The
        // flag must be stamped on the session entity BEFORE the synchronous
        // `SessionCreated` emit below so every enumeration / wire-notification
        // surface can filter on it from the first observation onward — the
        // caller can't set it after this task resolves because the emit has
        // already fired by then.
        ephemeral_supervisor: bool,
        // True for internal one-shot AI helpers (commit-message generation,
        // AI conflict/cherry-pick/rebase/explain — see
        // `message_generator::run_ephemeral_task`). Unlike a supervisor
        // ephemeral these are genuinely top-level (`parent_session_id = None`),
        // so without this flag they would be pinned into the tab strip and
        // emit `SessionCreated`. Their lifetime is so brief that the async
        // console-panel tab-ADD can lose the race to the synchronous tab-REMOVE
        // on close, orphaning a ghost tab. Setting this suppresses BOTH the
        // strip pin and the `SessionCreated` emit so no tab is ever surfaced.
        ephemeral: bool,
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
                    s.is_supervisor_ephemeral = ephemeral_supervisor;
                    s.is_ephemeral = ephemeral;
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
                // Hidden supervisor judge/auditor sessions are excluded from
                // the wire `agent_session_created` churn (a connected mobile
                // client would otherwise see a create+close pair every idle
                // wake-up). They're never user-visible, so suppressing the
                // store event at the source is the single cleanest chokepoint.
                if !ephemeral_supervisor && !ephemeral {
                    cx.emit(SolutionAgentStoreEvent::SessionCreated {
                        id: session_id,
                        parent_session_id,
                    });
                }
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
            if parent_session_id.is_none() && !ephemeral {
                this.update(cx, |store, cx| {
                    store.open_session_in_strip(session_id, cx);
                    // Re-persist the metadata row now that `open_session_in_strip`
                    // has stamped the in-memory `tab_order`. The earlier
                    // `persist_session_row` (before the strip pin) wrote the row
                    // with `tab_order = None`, and `persist_tab_order` only issues
                    // a bare UPDATE — which no-ops if it loses the race to the
                    // metadata INSERT (the row doesn't exist yet). This second
                    // write carries the real `tab_order`, and the INSERT's
                    // `ON CONFLICT … tab_order = COALESCE(excluded, existing)`
                    // makes the outcome order-independent: whichever of the two
                    // DB writes lands last, the row ends with the concrete
                    // tab_order rather than NULL. Without this an idle,
                    // never-touched new chat could persist with `tab_order = NULL`
                    // and vanish from `restore_open_tabs` on restart ("unknown
                    // session" on the next send).
                    store.persist_session_row(session_id, cx);
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
        // Ephemeral supervisor judge/auditor sessions get a Supervisor system
        // prompt instead of the solution's worker framing, so they judge from
        // the outside rather than drifting into doing the task.
        let is_supervisor = session_id
            .and_then(|id| self.sessions.get(&id))
            .map(|s| s.read(cx).is_supervisor_ephemeral)
            .unwrap_or(false);
        if is_supervisor {
            meta.insert(
                "systemPrompt".to_string(),
                serde_json::json!({ "append": crate::supervisor::SUPERVISOR_SYSTEM_PROMPT }),
            );
        } else if let Some(adapter) = self.adapters.get(agent_id) {
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
    /// Ephemeral supervisor judge/auditor sessions must leave NO durable trace
    /// in the DB. `is_supervisor_ephemeral` is an in-memory-only flag (there is
    /// no persisted column for it), so a judge row written to `solution_sessions`
    /// reloads after a restart as an ordinary child session — leaking the
    /// judge's private supervisor reasoning as a visible session chip on the
    /// desktop session list and the paired mobile (the live create/close/state
    /// emits are already suppressed; persistence was the unguarded path). Every
    /// persist-to-DB helper guards on this, so ephemeral sessions are never
    /// written — which also stops the DB from accreting one judge transcript per
    /// supervisor wake-up.
    fn is_ephemeral_session(&self, session_id: SolutionSessionId, cx: &App) -> bool {
        self.sessions
            .get(&session_id)
            .is_some_and(|s| s.read(cx).is_supervisor_ephemeral)
    }

    fn persist_session_row(&self, session_id: SolutionSessionId, cx: &mut Context<Self>) {
        if self.is_ephemeral_session(session_id, cx) {
            return;
        }
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
            // Carried through so the INSERT's ON CONFLICT path COALESCEs it
            // against any value a concurrent `persist_tab_order` already wrote;
            // it never CLEARS an existing tab_order (None -> COALESCE keeps the
            // stored value). The authoritative strip-position write stays
            // `update_tab_orders`; this is the live in-memory value (usually
            // None at create time, before the strip pin lands).
            tab_order: s.tab_order,
        };
        db.save_metadata(meta).detach_and_log_err(cx);
    }

    /// Flush just the session's current `change_seq` to its persisted column.
    /// Used by the section `mark_*` helpers, which advance `change_seq` (via a
    /// watermark bump) WITHOUT touching any entry row, so the entry-persist
    /// helpers don't cover them. INVARIANT (Task 5.1b): the durable `change_seq`
    /// should track every value handed to a delta client as `current_seq`, so
    /// each watermark advance is flushed here before the matching section event
    /// is emitted — i.e. before any in-process read could expose it. The write
    /// itself is detached, so a hard crash in the gap between issuing a cursor
    /// and the flush landing can still leave durable briefly behind; the
    /// `max()`-guarded UPDATE plus the deterministic restore seed absorb the
    /// common cases (write reordering and no-activity restarts).
    fn persist_change_seq(&self, session_id: SolutionSessionId, cx: &mut Context<Self>) {
        if self.is_ephemeral_session(session_id, cx) {
            return;
        }
        let Some(db) = self.persistence.clone() else {
            return;
        };
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
        let change_seq = session.read(cx).change_seq as i64;
        db.save_change_seq(session_id, change_seq)
            .detach_and_log_err(cx);
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
            let (preloaded_rows, preloaded_epoch, preloaded_change_seq) = {
                let tasks = this.update(cx, |store, _| {
                    store.persistence().map(|db| {
                        (
                            db.load_entries(meta.id),
                            db.load_epoch(meta.id),
                            db.load_change_seq(meta.id),
                        )
                    })
                })?;
                match tasks {
                    Some((rows_task, epoch_task, change_seq_task)) => {
                        let rows = rows_task.await.unwrap_or_else(|err| {
                            log::warn!(
                                target: "solution_agent::resume",
                                "session={} entry-row load failed on reopen: {err}",
                                meta.id
                            );
                            Vec::new()
                        });
                        let epoch = epoch_task.await.ok().flatten().unwrap_or(0);
                        let change_seq =
                            change_seq_task.await.ok().flatten().map(|v| v as u64);
                        (rows, epoch, change_seq)
                    }
                    None => (Vec::new(), 0, None),
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
                        // Rebuild the per-source `streams` mirror the desktop
                        // render reads from (phase 2c). Cold-load/hydration
                        // assigns `entries` directly, so without this the mirror
                        // stays Main-only-empty and a restored session paints
                        // blank. Collapse restored tagged rows to a Main-only
                        // view (an O(N) demux at load time); the live thread
                        // attached below reopens any still-live teammate.
                        s.hydrate_streams_main_only();
                        // Legacy/migrating rows have no persisted change_seq and no
                        // pre-restart delta client → fall back to max(mod_seq).
                        s.restore_change_seq(if migrating {
                            None
                        } else {
                            preloaded_change_seq
                        });
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

    /// Count of `solution_id`'s live sessions that are user-visible — i.e.
    /// [`sessions_for`](Self::sessions_for) minus the ephemeral supervisor
    /// judge sessions. The judge is spawned on every idle wake-up, runs for
    /// seconds, then is torn down by `finish_judge`; it must not tick the
    /// Sparkle AI-session badge up by 1 each time. User-created child sessions
    /// (`parent_session_id` set via the wire `create_session` tool) are NOT
    /// excluded — only live judges, identified authoritatively from the
    /// `judge_sessions` handle map.
    pub fn visible_session_count(&self, solution_id: &SolutionId) -> usize {
        let hidden_ids = self.live_supervisor_session_ids();
        self.by_solution
            .get(solution_id)
            .map(|ids| ids.iter().filter(|id| !hidden_ids.contains(id)).count())
            .unwrap_or(0)
    }

    /// Set of session ids that are currently live ephemeral SUPERVISOR sessions
    /// — the UNION of in-flight judges (`judge_sessions`) and meta-auditors
    /// (`auditor_sessions`). Both kinds are spawned by the supervisor, run for a
    /// few seconds, then torn down via `finish_judge` / `finish_auditor`. They
    /// must be excluded from user-visible surfaces (AI-session badge, subagent
    /// strip) so neither flickers as a child bubble while it is alive. This is
    /// the single source of truth for that hiding — adding a new ephemeral
    /// supervisor map means unioning it in here.
    pub(crate) fn live_supervisor_session_ids(&self) -> HashSet<SolutionSessionId> {
        self.judge_sessions
            .values()
            .chain(self.auditor_sessions.values())
            .filter_map(|handle| handle.judge_id)
            .collect()
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
        self.model_catalog.models_for(&session.agent_id)
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
            models = self.model_catalog.models_for(agent_id);
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
        if !self.model_catalog.begin_probe_if_needed(&agent_id) {
            return;
        }
        let task = self.probe_models_for_agent(&solution_id, &agent_id, cx);
        cx.spawn(async move |this, cx| {
            let models = task.await.log_err().unwrap_or_default();
            this.update(cx, |this, cx| {
                this.model_catalog.end_probe(&agent_id);
                if !models.is_empty() {
                    this.model_catalog.set_models(agent_id.clone(), models);
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
                self.model_catalog.set_models(agent_id, models.clone());
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
                this.model_catalog.set_models(agent_id, models.clone());
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

    /// Delete the session's inbox attachment files (the whole `inbox/` dir) and
    /// their DB rows. Safe because the image pixels also live as base64 in the
    /// persisted transcript entries (so the UI / reopen are unaffected) — the
    /// on-disk inbox file is only needed transiently for the agent's `Read`
    /// during the turn that delivered it. Called at context compaction / clear /
    /// session close, the points where any agent reference to the path becomes
    /// unreachable, so a long-running session stops accumulating attachments.
    /// Must run while the session is still in `self.sessions` (the inbox dir is
    /// resolved from the session's solution root).
    fn purge_session_attachments(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) {
        let dir = self.session_inbox_dir(id, cx);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).log_err();
        }
        if let Some(db) = &self.persistence {
            db.delete_attachments_for_session(id.to_string())
                .detach_and_log_err(cx);
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
            // Bind each saved attachment to the session in the DB so it can be
            // cascade-cleaned (purge_session_attachments / delete_for_solution)
            // rather than lingering on disk.
            if let Some(db) = &self.persistence {
                let solution_id = self
                    .sessions
                    .get(&session_id)
                    .map(|s| s.read(cx).solution_id.0.to_string());
                if let Some(solution_id) = solution_id {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    for path in image_paths.iter().flatten() {
                        db.record_attachment(
                            session_id.to_string(),
                            solution_id.clone(),
                            path.to_string_lossy().into_owned(),
                            now_ms,
                        )
                        .detach_and_log_err(cx);
                    }
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
            let mut change_seq_per_session: std::collections::HashMap<
                SolutionSessionId,
                Option<u64>,
            > = std::collections::HashMap::new();
            let mut blobs: std::collections::HashMap<SolutionSessionId, Vec<u8>> =
                std::collections::HashMap::new();
            for id in &ordered_ids {
                if already_open.contains(id) {
                    continue;
                }
                let rows = db.load_entries(*id).await?;
                let epoch = db.load_epoch(*id).await?.unwrap_or(0);
                epoch_per_session.insert(*id, epoch);
                change_seq_per_session
                    .insert(*id, db.load_change_seq(*id).await?.map(|v| v as u64));
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
                    let restored_change_seq = change_seq_per_session.get(id).copied().flatten();
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
                    let restored_desired_model = meta
                        .desired_model
                        .clone()
                        .or_else(|| persisted.as_ref().and_then(|p| p.desired_model.clone()));
                    let restored_desired_effort = meta
                        .desired_effort
                        .clone()
                        .or_else(|| persisted.as_ref().and_then(|p| p.desired_effort.clone()));
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
                        // Rebuild the per-source `streams` mirror (phase 2c) —
                        // the desktop render reads it, and this cold-load path
                        // assigns `entries` directly. Without it a restored
                        // session renders blank. Collapse tagged rows to a
                        // Main-only view (no live thread here → teammates that
                        // finished before the restart stay closed).
                        s.hydrate_streams_main_only();
                        s.restore_change_seq(if migrating { None } else { restored_change_seq });
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

    /// TTL reaper: hard-purge sessions the user soft-closed (tab close) more
    /// than [`CLOSED_SESSION_REAP_DAYS`] ago. A soft close intentionally keeps
    /// the row + `.agents/<sid>/` tree for "Reopen Closed Chat"; this reclaims
    /// that disk/DB once the chat has been closed long enough. `reopen_session`
    /// clears `closed_at`, so restoring a chat restarts the clock from its next
    /// close. Routes through [`purge_session_hard`](Self::purge_session_hard) —
    /// the single canonical per-session hard primitive — so a reaped session is
    /// cleaned exactly like a member/solution delete. Runs at the same
    /// infrequent seam as [`reap_stale_session_archives`](Self::reap_stale_session_archives)
    /// (solution open). `&self`: the mutation happens inside the spawned
    /// `this.update`, so this only schedules.
    fn reap_stale_closed_sessions(&self, solution_id: SolutionId, cx: &mut Context<Self>) {
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
        let cutoff_ms =
            (Utc::now() - chrono::Duration::days(CLOSED_SESSION_REAP_DAYS)).timestamp_millis();
        cx.spawn(async move |this, cx| {
            let ids = match db.list_sessions_closed_before(solution_id, cutoff_ms).await {
                Ok(ids) => ids,
                Err(_) => return,
            };
            if ids.is_empty() {
                return;
            }
            this.update(cx, |this, cx| {
                for id in ids {
                    this.purge_session_hard(id, Some(root.clone()), cx);
                }
            })
            .log_err();
        })
        .detach();
    }

    pub fn hydrate_all_for_solution(
        &self,
        solution_id: SolutionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<SolutionSessionId>>> {
        // Opening a solution is a natural, infrequent point to garbage-collect
        // stale on-disk session archives under `.agents/`, and to hard-purge
        // sessions that have sat soft-closed past their TTL.
        self.reap_stale_session_archives(solution_id.clone(), cx);
        self.reap_stale_closed_sessions(solution_id.clone(), cx);
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
            let tabbed_ids: Vec<SolutionSessionId> = db
                .list_open_tabs(solution_id.clone())
                .await
                .unwrap_or_default();
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
            let mut change_seq_per_session: std::collections::HashMap<
                SolutionSessionId,
                Option<u64>,
            > = std::collections::HashMap::new();
            let mut blobs: std::collections::HashMap<SolutionSessionId, Vec<u8>> =
                std::collections::HashMap::new();
            for meta in &to_hydrate {
                let rows = db.load_entries(meta.id).await?;
                let epoch = db.load_epoch(meta.id).await?.unwrap_or(0);
                epoch_per_session.insert(meta.id, epoch);
                change_seq_per_session.insert(
                    meta.id,
                    db.load_change_seq(meta.id).await?.map(|v| v as u64),
                );
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
                    let restored_change_seq =
                        change_seq_per_session.get(&meta.id).copied().flatten();
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
                        // Rebuild the per-source `streams` mirror (phase 2c) —
                        // the desktop render reads it, and this cold-load path
                        // assigns `entries` directly. Without it a restored
                        // session renders blank. Collapse tagged rows to a
                        // Main-only view (no live thread here → teammates that
                        // finished before the restart stay closed).
                        s.hydrate_streams_main_only();
                        s.restore_change_seq(if migrating { None } else { restored_change_seq });
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
                    // Reload the supervisor row a soft/cold close evicted, so a
                    // reopened session resumes supervision (and doesn't surprise-
                    // resurrect it on the next restart) — finding #5.
                    this.reload_supervisor_state_for(*sid, cx);
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
            let tabbed_ids: Vec<SolutionSessionId> = db
                .list_open_tabs(solution_id.clone())
                .await
                .unwrap_or_default();
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
                    // Reload the supervisor row a soft/cold close evicted so a
                    // reopened session resumes supervision. This lazy console-panel
                    // hydration path usually WINS the reopen race against
                    // `hydrate_all_for_solution`, so the reload must live here too
                    // or finding #5 reproduces on a normal window reopen. Idempotent
                    // (its own `contains_key` guard) if both paths run.
                    this.reload_supervisor_state_for(meta.id, cx);
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
            for sid in hydrated.iter().copied().filter(|id| Some(*id) != priority) {
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
        let restored_change_seq = db
            .load_change_seq(session_id)
            .await
            .ok()
            .flatten()
            .map(|v| v as u64);
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
                    let entries = if let Some(rows) = rows.take().filter(|r| !r.is_empty()) {
                        entries_from_rows(rows)
                    } else {
                        let (cold_entries, created_ms) = cold_entries_from_persisted(persisted, cx);
                        crate::session_entry::rebuild_entries(
                            &cold_entries,
                            &[],
                            &created_ms,
                            0,
                            cx,
                        )
                    };
                    session.entries = entries;
                    // Rebuild the per-source `streams` mirror (phase 2c) — the
                    // desktop render reads it; this cold-blob load assigns
                    // `entries` directly, so without it the restored session
                    // paints blank. Collapse tagged rows to a Main-only view
                    // (no live thread here → finished teammates stay closed).
                    session.hydrate_streams_main_only();
                    session.restore_change_seq(if migrating { None } else { restored_change_seq });
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

    /// Tear down the IN-MEMORY runtime state shared by every per-session
    /// teardown path ([`close_session`](Self::close_session) and
    /// [`purge_session_hard`](Self::purge_session_hard)): reap any in-flight
    /// judge/auditor, cancel an in-flight turn, drop the live entity (releasing
    /// its `Project`/worktree fd), remove the id from `by_solution` (dropping the
    /// solution key when it empties), and evict every per-session runtime map.
    /// Returns the metadata the callers need to finish the DB/disk/pool side
    /// (captured BEFORE the entity dropped), or `None` when `id` wasn't
    /// hydrated. This is the single canonical in-memory teardown primitive — no
    /// call site re-implements finish_judge/cancel/evict inline.
    fn teardown_session_runtime(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) -> Option<SessionTeardown> {
        // Reap any in-flight ephemeral judge/auditor FIRST, while the supervised
        // session is still reachable. Each closes its own hidden child session
        // (releasing that child's pooled `claude` subprocess + refcount);
        // skipping this strands the judge/auditor open forever — its pool
        // refcount never releases, so its subprocess never hits the idle
        // shutdown debounce and lingers for the editor's whole lifetime (the
        // dozens-of-orphaned-`claude`-processes leak on a long supervised run).
        // No-ops when `id` has no live judge/auditor (incl. when `id` is itself
        // an ephemeral child — those are never keys in these maps).
        self.finish_judge(id, cx);
        self.finish_auditor(id, cx);
        if let Some(entity) = self.sessions.get(&id)
            && matches!(entity.read(cx).state, SessionState::Running { .. })
        {
            self.cancel_turn(id, cx).log_err();
        }
        let removed = self.sessions.remove(&id)?;
        let session_read = removed.read(cx);
        // If the session is being torn down with queued messages still
        // unflushed, surface them in the log — teardown silently drops
        // everything in `pending_messages` (no Stopped event ever fires for the
        // torn-down thread).
        if !session_read.pending_messages.is_empty() {
            let previews: Vec<String> = session_read
                .pending_messages
                .iter()
                .map(|b| queue::summarize_blocks_for_log(&b.blocks))
                .collect();
            log::warn!(
                target: "solution_agent::queue",
                "session={id} dropped {} queued bundle(s) on teardown — content: [{}]",
                session_read.pending_messages.len(),
                previews.join(" | "),
            );
        }
        let solution_id = session_read.solution_id.clone();
        // Captured while the entity is still live (the flag is dropped with the
        // entity below). Hidden supervisor judge/auditor sessions suppress all
        // close notifications, mirroring the create-side suppression so a
        // connected mobile client never sees their per-wake-up churn.
        let was_ephemeral = session_read.is_supervisor_ephemeral || session_read.is_ephemeral;
        let agent_id = session_read.agent_id.clone();
        // Capture the live connection + ACP session id BEFORE the entity drops,
        // so callers can tear down THIS session's `claude` subprocess and
        // release the pool refcount. `None` for a cold/restored session that was
        // never spawned on the pool — those neither hold a subprocess nor a
        // refcount to release.
        let pool_teardown = session_read.acp_thread().map(|thread| {
            let thread = thread.read(cx);
            (thread.connection().clone(), thread.session_id().clone())
        });
        drop(session_read);
        if let Some(list) = self.by_solution.get_mut(&solution_id) {
            list.retain(|sid| *sid != id);
            if list.is_empty() {
                self.by_solution.remove(&solution_id);
            }
        }
        // Drop ALL per-session runtime maps for the torn-down session (entry
        // throttles, supervisor state, background watchers, backoff timer,
        // parent-jsonl cursor) — each holds a live `Task` and/or grows one entry
        // per closed session, so leaving them leaks for the process lifetime.
        self.evict_session_runtime_maps(id);
        Some(SessionTeardown {
            solution_id,
            agent_id,
            pool_teardown,
            was_ephemeral,
        })
    }

    /// Emit the per-session close notifications (`SessionClosed` +
    /// `workspace.session_deleted`) and tear down the pool side of the session.
    /// Shared close-out tail of [`close_session`](Self::close_session) and
    /// [`purge_session_hard`](Self::purge_session_hard). The pooled
    /// `ClaudeNativeConnection` is shared across the `(solution, agent)` pair and
    /// OUTLIVES the session, so dropping the `SolutionSession` + its `AcpThread`
    /// does NOT remove the session from the connection's `sessions` map — this
    /// session's `claude` subprocess would leak. Explicitly close the ACP session
    /// (claude_native removes the `SessionState` and kills its process) and
    /// release the pool refcount so the connection itself shuts down once its
    /// last session closes.
    fn finalize_session_teardown(
        &mut self,
        id: SolutionSessionId,
        teardown: SessionTeardown,
        cx: &mut Context<Self>,
    ) {
        if !teardown.was_ephemeral {
            cx.emit(SolutionAgentStoreEvent::SessionClosed(id));
            // Guard with `try_global` so test contexts that don't install the
            // MCP layer don't panic.
            if let Some(coord) =
                editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx)
            {
                coord.emit_sequenced(
                    cx,
                    "workspace.session_deleted",
                    serde_json::json!({
                        "solution_id": teardown.solution_id.as_str(),
                        "session_id": id.to_string(),
                    }),
                );
            }
        }
        if let Some((connection, acp_session_id)) = teardown.pool_teardown {
            if connection.supports_close_session() {
                connection.close_session(&acp_session_id, cx).detach();
            }
            self.pool_release_session((teardown.solution_id, teardown.agent_id), cx);
        }
    }

    pub fn close_session(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) -> Result<()> {
        // Delete the session's inbox attachments (files + DB rows) while the
        // session is still in `self.sessions` (the inbox dir resolves from its
        // solution root). The pixels survive as base64 in the persisted entries,
        // so reopen is unaffected. Must run BEFORE teardown (it needs the entity).
        self.purge_session_attachments(id, cx);
        // Flush the latest transcript while the session is still live, so a later
        // "Reopen Closed Chat" restores the full conversation. The in-flight-turn
        // cancel + entity drop happen inside `teardown_session_runtime`.
        self.persist_all_rows(id, cx);
        let teardown = self
            .teardown_session_runtime(id, cx)
            .ok_or_else(|| anyhow!("unknown session {id}"))?;
        // Soft-close: keep the persisted blob so downstream tooling
        // (MCP read_session_history, future "View archived sessions"
        // UI, etc.) can still read the transcript. The supervisor_state row is
        // also kept — `load_supervisor_states` restores it on reopen. Hard-delete
        // only happens via `purge_session_hard` / `purge_solution_fully`.
        if let Some(db) = &self.persistence {
            db.mark_closed(id, Some(Utc::now())).detach_and_log_err(cx);
        }
        self.finalize_session_teardown(id, teardown, cx);
        cx.notify();
        Ok(())
    }

    /// Delete an `.agents/<sid>/` archive tree off the foreground thread.
    /// NotFound is fine (a cold/never-archived session has no dir); any other IO
    /// error is surfaced rather than silently dropped. Shared by the hard-purge
    /// paths.
    fn spawn_remove_archive_dir(&self, archive: PathBuf, cx: &mut Context<Self>) {
        cx.background_spawn(async move {
            if let Err(err) = std::fs::remove_dir_all(&archive) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("remove_dir_all {archive:?}: {err}");
                }
            }
        })
        .detach();
    }

    /// HARD teardown of a single session whose backing directory has been
    /// removed (its member was dropped from the solution, or its whole solution
    /// was deleted). Unlike [`close_session`](Self::close_session) (soft /
    /// reopenable: keeps the row, purges only the inbox), this deletes
    /// EVERYTHING — the in-memory entity (releasing its `Project`/worktree fd),
    /// every per-session runtime map, the whole `<solution_root>/.agents/<sid>/`
    /// on-disk tree (observer files, compacts, session-log, inbox), all six DB
    /// tables, and the pool refcount. There is nothing to reopen, so no
    /// `closed_at` soft-close and no tab_order is kept.
    ///
    /// `root_override` supplies the solution root explicitly for callers that
    /// already removed the solution from the store (e.g. the `Deleted` event /
    /// [`purge_solution_fully`](Self::purge_solution_fully)), where
    /// `solution_root_for` would no longer resolve. `None` falls back to the
    /// store lookup, which is what the member-removal GC path uses.
    pub fn purge_session_hard(
        &mut self,
        id: SolutionSessionId,
        root_override: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        // Capture the on-disk archive dir (`.agents/<sid>/`) BEFORE removing the
        // entity — its path resolves from the session's solution root, which is
        // only reachable via `solution_root_for` while the session is still in
        // `self.sessions` (hence the `root_override` escape hatch).
        let archive = root_override
            .or_else(|| self.solution_root_for(id, cx))
            .map(|root| root.join(".agents").join(id.to_string()));
        let Some(teardown) = self.teardown_session_runtime(id, cx) else {
            // Nothing hydrated for this id — purge the persisted rows + disk
            // tree anyway so a never-loaded orphan is still cleaned up.
            if let Some(db) = &self.persistence {
                db.purge_session(id).detach_and_log_err(cx);
            }
            if let Some(archive) = archive {
                self.spawn_remove_archive_dir(archive, cx);
            }
            return;
        };
        // Delete the on-disk `.agents/<sid>/` tree off the foreground thread.
        if let Some(archive) = archive {
            self.spawn_remove_archive_dir(archive, cx);
        }
        // HARD-delete the persisted rows across all six tables.
        if let Some(db) = &self.persistence {
            db.purge_session(id).detach_and_log_err(cx);
        }
        self.finalize_session_teardown(id, teardown, cx);
        cx.notify();
    }

    /// THE single solution-level hard purge. Funneled into by the `Deleted`
    /// store event (with the captured `root`) and by
    /// [`gc_orphan_solutions`](Self::gc_orphan_solutions) (with `root: None`
    /// when a solution vanished from a `Changed` signal, where no root is
    /// available). Purges every hydrated session via
    /// [`purge_session_hard`](Self::purge_session_hard), sweeps any non-hydrated
    /// persisted rows via `delete_for_solution` (all six tables), nukes the
    /// whole `<root>/.agents` tree when a root is known, and releases the
    /// solution's pool connection(s). Idempotent: re-running on an
    /// already-purged solution is a sequence of no-ops (the `by_solution` entry
    /// is gone, `purge_session`/`delete_for_solution` on missing rows do
    /// nothing, and a missing `.agents` dir is ignored).
    pub fn purge_solution_fully(
        &mut self,
        solution_id: SolutionId,
        root: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        // Snapshot the hydrated ids first — `purge_session_hard` mutates
        // `by_solution`, so we must not iterate it while purging.
        let session_ids = self
            .by_solution
            .get(&solution_id)
            .cloned()
            .unwrap_or_default();
        for id in session_ids {
            self.purge_session_hard(id, root.clone(), cx);
        }
        // Sweep any non-hydrated rows (sessions persisted but never loaded this
        // process) across all six tables. The attachment files are deleted first
        // while their paths are still queryable.
        if let Some(db) = &self.persistence {
            let db = db.clone();
            let solution_id = solution_id.clone();
            cx.background_spawn(async move {
                if let Ok(paths) = db
                    .attachment_paths_for_solution(solution_id.0.to_string())
                    .await
                {
                    for path in paths {
                        std::fs::remove_file(path).log_err();
                    }
                }
                db.delete_for_solution(solution_id).await.log_err();
            })
            .detach();
        }
        // Nuke any remaining `<root>/.agents` archive dirs wholesale (the
        // per-session purges already removed each hydrated `.agents/<sid>`, but
        // a never-hydrated session's dir would otherwise linger). Only possible
        // when the root is known — a `Changed`-detected vanish carries none.
        if let Some(root) = root {
            self.spawn_remove_archive_dir(root.join(".agents"), cx);
        }
        // Release the pool connection(s) for the solution so its `claude`
        // subprocess(es) exit now, mirroring `cold_close_solution`.
        let keys: Vec<(SolutionId, AgentServerId)> = {
            let pool = self.pool.lock();
            pool.keys_for_solution(&solution_id).collect()
        };
        if !keys.is_empty() {
            let mut pool = self.pool.lock();
            for key in &keys {
                pool.remove(key);
            }
        }
        cx.notify();
    }

    /// Purge every hydrated, non-ephemeral session whose `cwd` no longer falls
    /// under any alive member's `local_path` (nor the solution root) — i.e. the
    /// member directory the session was scoped to has been removed from the
    /// Solution. Ephemeral supervisor children are skipped (their parent's purge
    /// reaps them via `finish_judge`/`finish_auditor`). Driven from
    /// `on_solution_event` on a `Changed` (member add/remove) signal.
    fn gc_orphan_members(&mut self, cx: &mut Context<Self>) {
        let Some(store) = SolutionStore::try_global(cx) else {
            return;
        };
        // (solution root, member paths) per alive solution, keyed by id.
        let roots: HashMap<SolutionId, (PathBuf, Vec<PathBuf>)> = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .map(|sol| {
                    let members = sol.members.iter().map(|m| m.local_path.clone()).collect();
                    (sol.id.clone(), (sol.root.clone(), members))
                })
                .collect()
        });
        // Collect orphan ids first; purging mutates `by_solution`, so we must not
        // iterate it while purging.
        let mut orphans: Vec<SolutionSessionId> = Vec::new();
        for (solution_id, session_ids) in &self.by_solution {
            let Some((root, members)) = roots.get(solution_id) else {
                // Whole solution vanished — handled by gc_orphan_solutions.
                continue;
            };
            for id in session_ids {
                let Some(session) = self.sessions.get(id) else {
                    continue;
                };
                let session = session.read(cx);
                if session.is_supervisor_ephemeral {
                    continue;
                }
                let cwd = &session.cwd;
                if cwd.as_os_str().is_empty() {
                    continue;
                }
                // A session is in-scope iff its cwd is the solution root itself
                // (a root-scoped / supervisor-style session) OR sits under a
                // still-present member directory. A removed member's directory
                // physically remains under `root`, so we must match `root`
                // EXACTLY here — a `strip_prefix(root)` test would wrongly keep
                // every removed-member session (they all live at `root/<member>`).
                let at_root = cwd == root;
                let under_member = members
                    .iter()
                    .any(|m| cwd == m || cwd.strip_prefix(m).is_ok());
                if !at_root && !under_member {
                    orphans.push(*id);
                }
            }
        }
        for id in orphans {
            // The member dir is gone but the solution (and its root) is still in
            // the store, so `purge_session_hard` resolves the archive path via
            // `solution_root_for` — no `root_override` needed.
            self.purge_session_hard(id, None, cx);
        }
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

    /// Non-destructive recovery for a wedged session (claude subprocess hung
    /// or dead): force a fresh subprocess and REPLAY the SAME `acp_session_id`
    /// from its on-disk transcript, keeping the conversation. Unlike
    /// [`restart_agent`] (mints a fresh, empty session — "v1 does not replay
    /// history") this preserves both the displayed entries and claude's own
    /// context (it `--resume`s the jsonl). Unlike [`rotate_context`] (same
    /// pooled subprocess, fresh empty ACP session) it drops the pool entry so
    /// the next connection spawns a clean subprocess.
    ///
    /// Implementation: transition the live session into exactly the shape of a
    /// cold-restored tab (live `AcpThread` dropped, `SolutionSessionId` +
    /// persisted `entries` kept) and hand off to the proven [`resume_session`]
    /// cold path, which grafts the replayed thread onto the existing entity
    /// in place (no duplication, no loss). Returns the (unchanged) session id.
    ///
    /// If the session was actively Running when reconnected, a continuation
    /// prompt ([`RECONNECT_CONTINUATION_PROMPT`]) is sent once it's back so the
    /// agent resumes instead of parking at Idle.
    pub fn reconnect_agent(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<SolutionSessionId>> {
        let Some(session) = self.sessions.get(&session_id).cloned() else {
            return Task::ready(Err(anyhow!("unknown session {session_id}")));
        };
        // The stuck-session watchdog only reconnects sessions wedged mid-turn
        // (Running), so capture whether real work was in flight BEFORE we flip
        // the state to `Errored("reconnecting…")` below. If so, once the
        // session is back we re-engage the agent with a continuation prompt
        // (see the spawn) instead of leaving it parked at Idle. A manual
        // reconnect of an already-idle session was_running == false → no
        // spurious nudge.
        let was_running = matches!(session.read(cx).state, SessionState::Running { .. });
        // Also capture — BEFORE cold-ize drops the live thread — whether the
        // wedge happened on an unanswered human message (transcript tail is a
        // non-nudge `UserMessage`). If so the continuation must point the fresh
        // subprocess AT that message rather than tell it to "carry on", or the
        // message is silently dropped (see `RECONNECT_UNANSWERED_USER_PROMPT`).
        let tail_unanswered_user = tail_is_unanswered_user_message(&session.read(cx).entries);
        let project = match session.read(cx).project.clone() {
            Some(project) => project,
            None => {
                return Task::ready(Err(anyhow!(
                    "session {session_id} has no cached project — reconnect_agent not supported \
                     for prebuilt/cold sessions (nothing to reconnect)"
                )));
            }
        };
        // Flush any un-debounced tail so the cold entry set + transcript replay
        // are complete before we drop the live thread.
        self.persist_all_rows(session_id, cx);
        let meta = {
            let s = session.read(cx);
            let (preview, total_tokens) = s
                .acp_thread()
                .map(|thread| {
                    let thread = thread.read(cx);
                    (
                        extract_preview(thread.entries()),
                        thread.token_usage().map(|u| u.used_tokens),
                    )
                })
                .unwrap_or((None, None));
            SolutionSessionMetadata {
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
                tab_order: s.tab_order,
            }
        };
        let pair = (meta.solution_id.clone(), meta.agent_id.clone());
        // Drop the pooled connection so `resume_session`'s
        // `get_or_spawn_connection` forces a fresh subprocess — the current one
        // is wedged. Live co-tenant sessions of the same (solution, agent) keep
        // their own connection Rc, so this only affects the next spawn (mirrors
        // `restart_agent`).
        {
            let mut pool = self.pool.lock();
            pool.remove(&pair);
        }
        // Cold-ize: drop the wedged thread (releasing its connection Rc so the
        // hung subprocess can be reaped) and surface a transient "reconnecting…"
        // status. `set_acp_thread(None)` keeps `entries`, so `resume_session`
        // sees the cold-session shape and grafts in place.
        session.update(cx, |s, cx| {
            s.state = SessionState::Errored(SharedString::from("reconnecting…"));
            s.set_acp_thread(None, cx);
            // Bump the activity clock so the SUPERVISOR tick doesn't treat the
            // mid-reconnect `Errored` session as idle-eligible and fire a judge
            // INTO a transient state — wasting a judge turn and risking a
            // verdict that lands during the reconnect (which would kick a second
            // concurrent `resume_session` — finding #7). The reconnect's own
            // continuation prompt bumps it again on completion, so a normal
            // (sub-`IDLE_THRESHOLD`) reconnect is fully covered.
            s.last_activity_at = chrono::Utc::now();
        });
        self.mark_state_changed(session_id, cx);
        // Retry material for a second attempt (see the spawn): `meta`/`project`
        // are moved into the first `resume_session`, so clone them first.
        let meta_retry = meta.clone();
        let project_retry = project.clone();
        let resume = self.resume_session(meta, project, cx);
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let timeout = std::time::Duration::from_secs(RECONNECT_RESUME_TIMEOUT_SECS);
            // Attempt 1, bounded: a resume that hangs on a dead-subprocess
            // handshake must not strand the session at `Errored("reconnecting…")`
            // forever. `with_timeout` drops (cancels) the resume task on timeout.
            let resumed = match crate::message_generator::with_timeout(resume, timeout, cx).await {
                Ok(Ok(resumed)) => resumed,
                first => {
                    log::warn!(
                        "session={session_id} reconnect resume attempt 1 failed ({}); retrying once",
                        reconnect_attempt_error(first)
                    );
                    this.update(cx, |store, cx| {
                        // Force a fresh subprocess on the retry: attempt 1 may
                        // have pooled a half-dead connection.
                        store.pool.lock().remove(&pair);
                        // Re-bump the activity clock so the supervisor tick doesn't
                        // fire a judge INTO the still-transient reconnecting state
                        // during attempt 2 (finding #7): attempt 1 timing out at
                        // RECONNECT_RESUME_TIMEOUT_SECS would otherwise cross
                        // IDLE_THRESHOLD_SECS exactly as attempt 2 begins. No await
                        // between the timeout and this bump, so no tick interleaves.
                        if let Some(s) = store.sessions.get(&session_id).cloned() {
                            s.update(cx, |s, _| s.last_activity_at = chrono::Utc::now());
                        }
                    })
                    .ok();
                    let retry = this
                        .update(cx, |store, cx| {
                            store.resume_session(meta_retry, project_retry, cx)
                        })?;
                    match crate::message_generator::with_timeout(retry, timeout, cx).await {
                        Ok(Ok(resumed)) => resumed,
                        second => {
                            let detail = reconnect_attempt_error(second);
                            log::error!(
                                "session={session_id} reconnect resume failed after retry ({detail})"
                            );
                            // Leave a CLEAR, actionable terminal error (not the
                            // transient "reconnecting…" that never resolves). The
                            // guidance rides in the state string itself, NOT a
                            // system note: the session is cold here (its thread was
                            // dropped at reconnect start and never re-grafted), and
                            // `push_system_note` no-ops without a live thread — the
                            // Errored state is the only surface the user still sees.
                            this.update(cx, |store, cx| {
                                store.mutate_state(
                                    session_id,
                                    |st| {
                                        *st = SessionState::Errored(SharedString::from(
                                            "переподключение не удалось — перезапустите агента (Restart)",
                                        ))
                                    },
                                    cx,
                                );
                            })
                            .ok();
                            return Err(anyhow!(
                                "reconnect of session {session_id} failed after retry: {detail}"
                            ));
                        }
                    }
                }
            };
            // Leave a visible breadcrumb in the conversation so the user knows
            // the editor recovered the session (vs it silently coming back).
            this.update(cx, |store, cx| {
                store.push_system_note(
                    resumed,
                    acp_thread::SystemNoteLevel::Info,
                    "Агент не отвечал — переподключил сессию (история и контекст сохранены).",
                    cx,
                );
                store.maybe_send_reconnect_continuation(
                    resumed,
                    was_running,
                    tail_unanswered_user,
                    cx,
                );
            })
            .ok();
            Ok(resumed)
        })
    }

    /// After [`reconnect_agent`](Self::reconnect_agent) brings a session back,
    /// re-engage the agent with a fresh continuation prompt so it resumes
    /// instead of parking at Idle — but ONLY when the session was actually
    /// Running (mid-turn) at reconnect time (`was_running`). A reconnect of an
    /// already-idle session (e.g. a manual MCP reconnect) gets no spurious
    /// nudge. The prompt is normally a "carry on" instruction, deliberately NOT
    /// a replay of the interrupted turn (replaying could re-run tool calls whose
    /// side effects already landed) — EXCEPT when `tail_unanswered_user` says the
    /// wedge happened on an unanswered human message, where it instead points the
    /// agent AT that message (`RECONNECT_UNANSWERED_USER_PROMPT`) so it isn't
    /// dropped as already-handled history. `from_user: false`: editor-originated,
    /// so it must not reset the supervisor's continue counter / resume a
    /// `WaitingUser` hold.
    pub(crate) fn maybe_send_reconnect_continuation(
        &mut self,
        session_id: SolutionSessionId,
        was_running: bool,
        tail_unanswered_user: bool,
        cx: &mut Context<Self>,
    ) {
        if !was_running {
            return;
        }
        // When the wedge happened on an unanswered human message, drive the
        // fresh subprocess AT that message — a generic "carry on" would make it
        // treat the replayed message as already-handled and drop it.
        let prompt = if tail_unanswered_user {
            RECONNECT_UNANSWERED_USER_PROMPT
        } else {
            RECONNECT_CONTINUATION_PROMPT
        };
        self.send_message_blocks_targeted(
            session_id,
            vec![agent_client_protocol::schema::ContentBlock::Text(
                // Stamp the editor-recovery `_meta` marker (invisible to the
                // agent's text) so consumers that reason about "the user's goal"
                // exclude it: the supervisor must not distill "your process hung"
                // into `user_intent.md`, and `tail_is_unanswered_user_message`
                // must not mistake THIS prompt for an unanswered human message on
                // a second consecutive hang.
                agent_client_protocol::schema::TextContent::new(prompt.to_string())
                    .meta(Some(acp_thread::meta_with_editor_recovery())),
            )],
            crate::model::QueueTarget::Main,
            false,
            cx,
        )
        .detach_and_log_err(cx);
    }

    /// A successful worker turn (`Stopped`) proves the agent is responding, so a
    /// pending usage-limit / transient-backoff resume gate (`next_eligible_ms`
    /// plus its `backoff_timers` wake task) is stale and must be cleared (#7).
    /// Otherwise the worker stays gated until the now-irrelevant reset time AND
    /// the wake timer later fires a redundant judge. A genuine re-hit of the
    /// usage wall surfaces as `AcpThreadEvent::Error` (not `Stopped`) — see the
    /// claude_native orphan-result handling — so the gate correctly SURVIVES
    /// that case and we keep waiting. Idempotent; no-op when no gate is set.
    /// Mirrors the success-clear in [`apply_verdict`](Self::apply_verdict).
    pub(crate) fn clear_resume_gate_on_agent_response(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        let gated = self
            .supervisor_states
            .get(&id)
            .is_some_and(|s| s.next_eligible_ms.is_some());
        if !gated {
            return;
        }
        if let Some(state) = self.supervisor_states.get_mut(&id) {
            state.next_eligible_ms = None;
            state.backoff_attempt = 0;
        }
        self.backoff_timers.remove(&id);
        self.persist_supervisor_state(id, cx);
    }

    /// Interleave an editor-originated [`acp_thread::SystemNote`] into the
    /// session's conversation (watchdog / usage-limit / supervisor breadcrumbs).
    /// Pushes onto the live `AcpThread` so it flows through the normal
    /// NewEntry → persist → mobile-delta pipeline. No-op for a cold session
    /// (no live thread) — callers inject right after a resume, when one exists.
    /// Synchronously flush every pending `EntryUpdated` append throttle for
    /// `session_id`, emitting each entry's `SessionMessageAppended` now and
    /// dropping its debounce slot (so its timer can't double-fire). Called on
    /// terminal turn events (`Stopped`/`Error`): the last assistant text of a
    /// turn arrives via a debounced `EntryUpdated`, and at turn end that pending
    /// timer is the only signal carrying the final tail — flushing it here
    /// guarantees the final entry's append + `agent_session_dirty` ride out
    /// immediately instead of racing the turn-completion teardown.
    pub(crate) fn flush_pending_entry_appends(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        let pending_throttled: Vec<usize> = self
            .entry_update_throttles
            .keys()
            .filter(|(sid, _)| *sid == session_id)
            .map(|(_, idx)| *idx)
            .collect();
        for entry_index in pending_throttled {
            self.entry_update_throttles
                .remove(&(session_id, entry_index));
            cx.emit(SolutionAgentStoreEvent::SessionMessageAppended(
                session_id,
                entry_index,
            ));
        }
    }

    pub(crate) fn push_system_note(
        &mut self,
        session_id: SolutionSessionId,
        level: acp_thread::SystemNoteLevel,
        text: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.sessions.get(&session_id).cloned()
            && let Some(thread) = session.read(cx).acp_thread().cloned()
        {
            thread.update(cx, |t, cx| t.push_system_note(level, text, cx));
        }
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
                // The PRE-rotation ACP session id, captured before the graft
                // overwrites it — needed to tear down its now-orphaned subprocess.
                // Only meaningful if the session was actually live (a cold session
                // never spawned an old child and never held a pool refcount slot).
                let old_acp_session_id = session_entity.read(cx).acp_session_id.clone();
                let old_thread_was_live = session_entity.read(cx).acp_thread().is_some();
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
                    s.clear_closed_streams();
                    s.rebuild_streams();
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
                // Compact reused the pooled connection for a FRESH ACP session
                // but left the PRE-rotation session live in the connection's
                // `sessions` map — its `claude` subprocess would leak one process
                // (plus its MCP children) per /compact. And `get_or_spawn_connection`
                // above incremented `live_session_count` again WITHOUT releasing
                // the slot the old ACP session held, so the pair's refcount would
                // climb by one each rotation and never reach zero on close. Close
                // the old ACP session (kills its subprocess) and release the extra
                // refcount so the count still reflects exactly one live session.
                if old_thread_was_live {
                    if connection.supports_close_session() {
                        connection
                            .clone()
                            .close_session(&old_acp_session_id, cx)
                            .detach();
                    }
                    store.pool_release_session(pair.clone(), cx);
                }
                // Bound disk: the pre-rotation transcript is now orphaned —
                // keep only the most recent few of this session's transcripts.
                store.prune_raw_transcripts(session_id, old_acp_session_id.0.to_string(), cx);
                // The pre-rotation context (with any attached-image path
                // references) is wiped, so the inbox files can never be `Read`
                // again — purge them. Pixels live on as base64 in entries.
                store.purge_session_attachments(session_id, cx);
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
        // Human-initiated `/clear`: give the observer a clean slate (wipe its
        // diary/verdicts/user-intent + reset its reasoning cursor) so it doesn't
        // carry stale reasoning across the reset.
        self.wipe_supervisor_memory(session_id, cx);
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
                // Capture the PRE-clear ACP session id + liveness before the graft
                // overwrites them, so we can reap its orphaned subprocess + release
                // the pool slot it held (skipped for a cold session — it never
                // spawned an old child nor took a refcount slot).
                let old_acp_session_id = session_entity.read(cx).acp_session_id.clone();
                let old_thread_was_live = session_entity.read(cx).acp_thread().is_some();
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
                    s.clear_closed_streams();
                    s.rebuild_streams();
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
                // Reap the pre-clear ACP session's subprocess + balance the pool
                // refcount that `get_or_spawn_connection` re-incremented above
                // (same leak/double-count as `rotate_context`).
                if old_thread_was_live {
                    if connection.supports_close_session() {
                        connection
                            .clone()
                            .close_session(&old_acp_session_id, cx)
                            .detach();
                    }
                    store.pool_release_session(pair.clone(), cx);
                }
                // Bound disk: the pre-clear transcript is now orphaned — keep
                // only the most recent few of this session's transcripts.
                store.prune_raw_transcripts(session_id, old_acp_session_id.0.to_string(), cx);
                // `/clear` wipes the context, so the inbox attachment paths are
                // unreachable — purge the files + rows. Pixels survive as base64.
                store.purge_session_attachments(session_id, cx);
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

    /// Debug/verification only: register a COLD session (no live `AcpThread`)
    /// pre-populated with `entries`, made UI-visible via `open_session_in_strip`,
    /// then return its id. Exists so an agent driving the editor over MCP can
    /// screenshot arbitrary multi-stream render states (Main + a Task teammate,
    /// background shells, …) WITHOUT a live claude subprocess — the render path
    /// reads `session.streams` (rebuilt by `set_entries`), so a seeded cold
    /// session paints exactly like a hydrated one. Not compiled into release
    /// builds (`debug_assertions`), so it never reaches a user binary.
    #[cfg(debug_assertions)]
    pub(crate) fn seed_cold_session(
        &mut self,
        solution_id: SolutionId,
        title: SharedString,
        entries: Vec<crate::session_entry::SessionEntry>,
        live_teammates: bool,
        live_shell: Option<String>,
        cx: &mut Context<Self>,
    ) -> SolutionSessionId {
        let session_id = SolutionSessionId::new();
        // Use the first member's path as cwd (falling back to the solution
        // root) so the ConsolePanel's active-member tab filter
        // (`tab_cwd_in_scope`: cwd must `starts_with` the active member) shows
        // the seeded session instead of scoping it out.
        let root = SolutionStore::try_global(cx)
            .and_then(|store| {
                store.read_with(cx, |s, _| {
                    s.solutions().iter().find(|sol| sol.id == solution_id).map(
                        |sol| {
                            sol.members
                                .first()
                                .map(|m| m.local_path.clone())
                                .unwrap_or_else(|| sol.root.clone())
                        },
                    )
                })
            })
            .unwrap_or_default();
        let entity = cx.new(|cx| {
            let mut s = SolutionSession::new_idle(
                session_id,
                solution_id.clone(),
                SharedString::from("claude-acp"),
                acp::SessionId::new("seed-cold"),
            );
            s.title = title;
            s.cwd = root;
            s.set_entries(entries, cx);
            if live_teammates {
                // Capture a friendly label for each distinct teammate id (from the
                // just-demux'd streams) so `rebuild_streams` enriches its
                // `Stream.label` and the desktop strip paints a labelled pill. The
                // `task-` prefix makes the label DISTINCT from the raw toolu so the
                // screenshot gate visibly proves `teammate_labels`→`Stream.label`→
                // pill (not the demux default). Debug/screenshot-only path.
                let teammate_ids: Vec<SharedString> = s
                    .streams
                    .keys()
                    .filter_map(|id| match id {
                        crate::stream::StreamId::Teammate(toolu) => Some(toolu.clone()),
                        _ => None,
                    })
                    .collect();
                for toolu in teammate_ids {
                    s.teammate_labels
                        .insert(toolu.clone(), SharedString::from(format!("task-{}", toolu)));
                }
                // Re-enrich the already-demux'd teammate streams' labels now that
                // `teammate_labels` is populated (the streams were built by
                // `set_entries` above when the map was still empty).
                s.rebuild_streams();
            }
            if let Some(command) = live_shell {
                // Phase 6d-A screenshot gate: register ONE `Running` shell with a
                // synthetic snapshot so `rebuild_streams` folds it into
                // `session.streams` as a `StreamId::Shell` tab. Debug-only path.
                let shell_id = crate::background_shell::BackgroundShellId::new("seedshell");
                s.background_shells.insert(
                    shell_id.clone(),
                    crate::background_shell::BackgroundShell {
                        id: shell_id.clone(),
                        command: SharedString::from(command),
                        output_path: std::path::PathBuf::from("/tmp/sawe-seed-shell.output"),
                        registered_at: chrono::Utc::now(),
                        latest: Some(crate::background_shell::BackgroundShellSnapshot {
                            mtime: std::time::SystemTime::now(),
                            output_tail: SharedString::from(
                                "seed shell output line 1\nseed shell output line 2\n",
                            ),
                        }),
                        last_offset: 0,
                        state: crate::background_shell::ShellRuntimeState::Running,
                    },
                );
                s.background_shell_order.push(shell_id);
                s.rebuild_streams();
            }
            s
        });
        self.sessions.insert(session_id, entity);
        self.by_solution
            .entry(solution_id)
            .or_default()
            .push(session_id);
        self.open_session_in_strip(session_id, cx);
        // Persist the metadata row + entry rows (with their `subagent_id` tags)
        // so a restart reloads this session through the real cold-load /
        // hydration path — which must `rebuild_streams()` after assigning
        // `entries`, else a restored session renders blank. Lets the seed
        // double as a hydration-fix check.
        self.persist_session_row(session_id, cx);
        self.persist_all_rows(session_id, cx);
        cx.emit(SolutionAgentStoreEvent::SessionCreated {
            id: session_id,
            parent_session_id: None,
        });
        cx.notify();
        session_id
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

    /// Persist row tuple: `(idx, mod_seq, created_ms, subagent_id, payload)` in
    /// the casts `upsert_entry` expects. An empty `payload` signals a serde
    /// failure in `to_payload()` — callers MUST skip persisting it. As of phase
    /// 6b the authoritative rows are the Main stream's entries, so `idx` is the
    /// entry's Main-LOCAL index and the persisted `subagent_id` is always `None`
    /// (Main entries carry no tag). The `subagent_id` column survives as
    /// vestigial for any legacy tagged rows still on disk.
    fn entry_row_tuple(
        idx: usize,
        entry: &crate::session_entry::SessionEntry,
    ) -> (i64, i64, i64, Option<String>, Vec<u8>) {
        (
            idx as i64,
            entry.mod_seq as i64,
            entry.created_ms,
            entry.subagent_id.as_ref().map(|s| s.to_string()),
            entry.to_payload(),
        )
    }

    /// Flush the WHOLE Main stream as rows: upsert every current
    /// `streams[StreamId::Main]` entry (keyed by Main-LOCAL index, subagent_id
    /// always `None`), delete any stale trailing rows beyond the Main length, and
    /// save the epoch. This is the path that handles clears/compactions and
    /// close — targeted upserts alone would leave orphaned idx>len rows that
    /// corrupt the next cold load. On an empty Main stream it degrades to
    /// "delete all rows + save epoch". Since it re-writes the entire Main stream
    /// it also resets `persisted_main_seq` to the Main stream's current `seq`
    /// (the incremental `persist_main_stream` then skips these rows next time).
    pub fn persist_all_rows(&mut self, session_id: SolutionSessionId, cx: &mut Context<Self>) {
        if self.is_ephemeral_session(session_id, cx) {
            return;
        }
        let Some(session) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        let Some(db) = self.persistence.clone() else {
            return;
        };
        // Capture the full-flush plan + advance the watermark SYNCHRONOUSLY (in
        // event order), so a concurrent `persist_main_stream` doesn't re-upsert
        // the rows this flush covers, and so the plan can't drift before the
        // chained DB task runs.
        let (rows, len, epoch, change_seq) = session.update(cx, |s, _| {
            let main = s.streams.get(&crate::stream::StreamId::Main);
            let main_entries = main.map(|stream| stream.entries.as_slice()).unwrap_or(&[]);
            let rows: Vec<_> = main_entries
                .iter()
                .enumerate()
                .map(|(idx, entry)| Self::entry_row_tuple(idx, entry))
                .collect();
            let len = main_entries.len() as i64;
            s.persisted_main_seq = main.map(|stream| stream.seq).unwrap_or(0);
            (rows, len, s.epoch as i64, s.change_seq as i64)
        });
        // Serialize behind this session's prior persist link (see
        // `persist_main_stream`) — a full flush's `delete_entries_from(len)` must
        // not race a concurrent incremental append's upsert.
        let prev = self.entries_persist_chain.remove(&session_id);
        let task = cx.spawn(async move |_this, _cx: &mut AsyncApp| {
            if let Some(prev) = prev {
                prev.await;
            }
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
            db.save_change_seq(session_id, change_seq).await.log_err();
        });
        self.entries_persist_chain.insert(session_id, task);
    }

    /// Incremental persist of the Main stream (phase 6b's persist authority).
    /// Reads `streams[StreamId::Main].entries` and upserts only the rows whose
    /// `mod_seq` exceeds `persisted_main_seq` (keyed by Main-LOCAL index,
    /// subagent_id always `None`), then always `delete_entries_from(main_len)` to
    /// trim any torn/teammate leftover rows past the Main tail. The Main length
    /// and the new watermark (`streams[Main].seq`) are captured — and
    /// `persisted_main_seq` advanced — SYNCHRONOUSLY before spawning the detached
    /// DB task, so a burst of ingest events each persist only their own delta and
    /// never double-write a row. Replaces the old flat-index
    /// `persist_upsert_range`/`persist_upsert_entry`/`persist_delete_from` calls
    /// in the ingest arms.
    pub fn persist_main_stream(&mut self, session_id: SolutionSessionId, cx: &mut Context<Self>) {
        if self.is_ephemeral_session(session_id, cx) {
            return;
        }
        let Some(session) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        let Some(db) = self.persistence.clone() else {
            return;
        };
        // Capture the persist plan + advance the watermark synchronously, so
        // concurrent detached tasks can't each re-read the pre-advance value and
        // redundantly upsert the same rows.
        let (rows, main_len, epoch, change_seq) = session.update(cx, |s, _| {
            let old_watermark = s.persisted_main_seq;
            let main = s.streams.get(&crate::stream::StreamId::Main);
            let main_entries = main.map(|stream| stream.entries.as_slice()).unwrap_or(&[]);
            let watermark = main.map(|stream| stream.seq).unwrap_or(0);
            let rows: Vec<_> = main_entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.mod_seq > old_watermark)
                .map(|(idx, entry)| Self::entry_row_tuple(idx, entry))
                .collect();
            let main_len = main_entries.len() as i64;
            s.persisted_main_seq = watermark;
            (rows, main_len, s.epoch as i64, s.change_seq as i64)
        });
        // SERIALIZE behind this session's prior persist link: the plan above is
        // captured in event order, but `delete_entries_from(main_len)` carries a
        // point-in-time length — if a later append's upsert lands before an
        // earlier link's stale delete runs (detached tasks are NOT FIFO), the
        // just-appended row is deleted. Chaining `prev.await` first makes the
        // upsert+delete pairs apply in issue order.
        let prev = self.entries_persist_chain.remove(&session_id);
        let task = cx.spawn(async move |_this, _cx: &mut AsyncApp| {
            if let Some(prev) = prev {
                prev.await;
            }
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
            db.delete_entries_from(session_id, main_len).await.log_err();
            db.save_epoch(session_id, epoch).await.log_err();
            db.save_change_seq(session_id, change_seq).await.log_err();
        });
        self.entries_persist_chain.insert(session_id, task);
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
    ///     not already tracked → captures its friendly label in
    ///     `SolutionSession::teammate_labels` and emits
    ///     [`SolutionAgentStoreEvent::SessionSubagentsChanged`];
    ///   * if it's a tracked id whose status just flipped to a terminal
    ///     state (`Completed`/`Failed`/`Rejected`/`Canceled`) → closes the
    ///     inline Task's stream (reclaiming its label) and emits the same event.
    ///
    /// Any other shape (non-tool entry, non-Task tool, status still
    /// `InProgress`/`Pending` on an already-tracked id, terminal status on
    /// an unknown id) is a no-op and emits nothing. Mutations are gated
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
            /// The tool call's rendered content text (the `tool_result` body).
            /// For an async `Agent` launch claude puts the "Async agent
            /// launched successfully… agentId: … output_file: …" announcement
            /// HERE (the tool_result content), NOT in `raw_output` — so the
            /// managed-agent registration parses this as a fallback. Populated
            /// only for terminal task-like calls; `None` otherwise.
            content_text: Option<String>,
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
            // Only the terminal task-like branch reads the content (async-agent
            // announcement fallback), so skip the string-building otherwise.
            let content_text = if is_task_like && is_terminal {
                let mut text = String::new();
                for content in &call.content {
                    if let acp_thread::ToolCallContent::ContentBlock(block) = content {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(block.to_markdown(cx));
                    }
                }
                (!text.is_empty()).then_some(text)
            } else {
                None
            };
            Snapshot {
                id: SharedString::from(call.id.0.to_string()),
                is_task_like,
                is_in_progress,
                is_terminal,
                label_from_raw_input,
                subagent_type,
                tool_name: tool_name_owned,
                raw_output_text,
                content_text,
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
                        // Phase 6d-A: the shell's derived `StreamId::Shell` tab
                        // is produced by `rebuild_streams` from `background_shells`,
                        // so every shell mutation must rebuild or the mirror drifts.
                        s.rebuild_streams();
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
            let already_tracked = session_entity.read(cx).teammate_labels.contains_key(&id);
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
                // Capture the durable friendly label for BOTH inline `Task` and
                // async `Agent` teammates (both are `is_task_like`). It rides
                // `Stream.label` via `rebuild_streams` and is reclaimed when the
                // teammate's stream closes (`close_stream`).
                session_entity.update(cx, |s, _| {
                    s.teammate_labels.insert(id.clone(), label);
                });
                true
            }
        } else if snapshot.is_terminal {
            // Symmetric defensive guard: a terminal-status EntryUpdated on an
            // id we never registered (e.g. the InProgress event arrived after
            // a status flip on a cold→live transition) is a no-op.
            let tracked = session_entity.read(cx).teammate_labels.contains_key(&id);
            if tracked {
                // Terminal status is a GENUINE "teammate done" signal ONLY for an
                // inline `Task` (its tool-call stays InProgress for the whole run
                // and completes only when the Task finishes). An async `Agent`
                // teammate's spawn tool-call flips to Completed IMMEDIATELY at
                // spawn-ack while the teammate keeps streaming `subagent_id`-tagged
                // entries into the parent thread for minutes — so closing its
                // demux `Teammate` stream here would suppress the still-live
                // teammate (decision #5: the parent-thread demux IS its source of
                // truth). So auto-close the stream for `Task` only; the async
                // `Agent`'s real done-signal (stop_reason / completion) drives its
                // close in a later phase. `close_stream` reclaims the inline Task's
                // `teammate_labels` entry; the async `Agent` keeps its label (its
                // stream stays open past spawn-ack) and reclaims it on its own close.
                let is_async_agent = tool_name_is_agent(snapshot.tool_name.as_deref());
                session_entity.update(cx, |s, _| {
                    if !is_async_agent {
                        s.close_stream(
                            crate::stream::StreamId::Teammate(id.clone()),
                            gpui::SharedString::new_static("done"),
                        );
                    }
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
            // The announcement (`agentId:` + `output_file:`) lives in the
            // tool_result body, which the native adapter surfaces as the tool
            // call's CONTENT, not `raw_output` (that stays null for an async
            // `Agent` launch). Parse `raw_output` first for forward-compat with
            // any dispatcher that stashes it there, then fall back to content —
            // the current claude path. Without the content fallback the
            // background-agent pill never registers, so an actively-streaming
            // teammate shows no strip tab and its output (tagged in the parent
            // thread) has nowhere to go but the Main tab.
            let announcement = crate::background_agent::managed_agent_announcement(
                snapshot.raw_output_text.as_deref(),
                snapshot.content_text.as_deref(),
            );
            if let Some((agent_id_str, output_file)) = announcement {
                let canonical =
                    std::fs::read_link(&output_file).unwrap_or_else(|_| output_file.clone());
                // Capture the parent `Agent` spawn tool-call's tool_use id
                // BEFORE the `BackgroundAgentId::new` binding below shadows the
                // outer `id` (= `snapshot.id`). This is the key of the teammate's
                // demux `Teammate` stream, needed to auto-close it on the agent's
                // real terminal `stop_reason`.
                let parent_toolu = id.clone();
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
                                parent_tool_use_id: Some(parent_toolu),
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

        // Self-heal immediately when a Task terminalises: the mid-session
        // reconcile is cheap for one session and catches the same terminal-but-
        // missed cases the event-driven close above can drop (e.g. a Task whose
        // tool-call flipped terminal on an EntryUpdated we didn't route through
        // the `is_terminal` branch). It is SELECTIVE, so a still-live teammate
        // in this session is untouched.
        self.reconcile_finished_teammate_streams(session_id, cx);
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
        if self.teammate_watchers.has_agent_watcher(session_id) {
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
        self.teammate_watchers.arm_agent_watcher(session_id, task);
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
            let mut close_teammate: Option<(SharedString, SharedString)> = None; // (parent toolu, reason)
            if let Some(ba) = s.background_agents.get_mut(&agent_id) {
                // Always advance the offset (or rewind on truncation —
                // `tail_jsonl` already handled the reset). Only update
                // `latest` when this tail actually yielded a new line;
                // otherwise the previously-known snapshot remains the
                // user-visible state.
                ba.last_offset = new_offset;
                if let Some(snap) = snapshot {
                    // A managed background agent reaching a terminal stop is
                    // fresh session activity — reset the silence clock so the
                    // supervisor gives the parent a full idle window to resume
                    // on its own before judging, exactly like a background shell
                    // completing (`mark_background_shell_state`). Only on the
                    // transition into terminal (a done agent's JSONL stops
                    // growing, so this fires once).
                    let was_terminal = ba
                        .latest
                        .as_ref()
                        .is_some_and(|s| s.stop_reason.is_some());
                    let now_terminal = snap.stop_reason.is_some();
                    let parent = ba.parent_tool_use_id.clone();
                    let reason = snap.stop_reason.clone();
                    ba.latest = Some(snap);
                    changed = true;
                    if now_terminal && !was_terminal {
                        s.last_activity_at = Utc::now();
                        if let Some(parent_toolu) = parent {
                            close_teammate = Some((
                                parent_toolu,
                                reason.unwrap_or_else(|| gpui::SharedString::new_static("done")),
                            ));
                        }
                    }
                }
            }
            // Auto-close the async `Agent` teammate's demux stream on its REAL
            // terminal signal (deferred from phase 3, where the spawn tool-call
            // terminal is only spawn-ack). Done after the `ba` borrow ends so
            // `close_stream` can take `&mut s`.
            if let Some((parent_toolu, reason)) = close_teammate {
                s.close_stream(crate::stream::StreamId::Teammate(parent_toolu), reason);
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
        if self.teammate_watchers.has_shell_watcher(session_id) {
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
        self.teammate_watchers.arm_shell_watcher(session_id, task);
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
            // Phase 6d-A: refresh the derived Shell stream so its fenced body +
            // mtime-based `seq` track the new tail.
            s.rebuild_streams();
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
            // A background command COMPLETING is fresh session activity: reset
            // the silence clock. While it ran, `has_live_background_work` kept
            // the supervisor quiet, but `last_activity_at` stayed frozen at
            // launch — so the moment it finishes the accrued silence is already
            // past `IDLE_THRESHOLD_SECS` and the judge would fire INSTANTLY,
            // racing (and usually losing to) the agent resuming ON ITS OWN to
            // read the result (a `Bash(run_in_background)` orphan continuation /
            // `<task-notification>`). Bumping the clock here gives the agent a
            // full fresh idle window to self-resume before the supervisor
            // judges; if it genuinely doesn't, the judge fires after the window
            // as intended. (The send-time session-idle re-check in
            // `apply_verdict` is the backstop for the residual race.)
            if matches!(
                new_state,
                crate::background_shell::ShellRuntimeState::Exited(_)
                    | crate::background_shell::ShellRuntimeState::Killed
            ) {
                s.last_activity_at = Utc::now();
            }
            let row = crate::db::BackgroundShellRow {
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
            };
            // Phase 6d-A: a terminal flip (`Exited`/`Killed`) drops this shell
            // from the derived stream mirror (only `Running` shells are folded
            // in) — that IS the auto-close. Rebuild here now the `shell` borrow
            // above has been released into the owned `row`.
            s.rebuild_streams();
            Some(row)
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
        let expiry = std::time::Duration::from_secs(
            MANAGED_AGENT_STALE_TIMEOUT_SECS + MANAGED_AGENT_DEAD_LINGER_SECS,
        );
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
                            // Age from the snapshot's mtime when one exists, else
                            // from `registered_at` — mirroring the shell reaper.
                            // A snapshot-less async agent (JSONL never parsed)
                            // must still age out or its map entry (and stream)
                            // would leak forever.
                            let age = match ba.latest.as_ref() {
                                Some(snap) => {
                                    if snap.stop_reason.is_some() {
                                        return true;
                                    }
                                    now.duration_since(snap.mtime).unwrap_or_default()
                                }
                                None => {
                                    let registered: std::time::SystemTime =
                                        ba.registered_at.into();
                                    now.duration_since(registered).unwrap_or_default()
                                }
                            };
                            age > expiry
                        })
                        .cloned()
                        .collect();
                    // Safety-net close of each reaped teammate's demux stream:
                    // covers a missed terminal-transition edge in
                    // `refresh_background_agent_snapshot` (e.g. an agent that
                    // registered already-terminal, or one reaped as stale-dead).
                    // Collect before removal so `close_stream` (which takes
                    // `&mut s`) runs after the borrow ends; it is idempotent.
                    let close_teammates: Vec<(SharedString, SharedString)> = candidates
                        .iter()
                        .filter_map(|id| {
                            let ba = s.background_agents.get(id)?;
                            let parent = ba.parent_tool_use_id.clone()?;
                            let reason = ba
                                .latest
                                .as_ref()
                                .and_then(|snap| snap.stop_reason.clone())
                                .unwrap_or_else(|| gpui::SharedString::new_static("done"));
                            Some((parent, reason))
                        })
                        .collect();
                    for id in &candidates {
                        s.background_agents.remove(id);
                        s.background_agent_order.retain(|x| x != id);
                    }
                    for (parent_toolu, reason) in close_teammates {
                        s.close_stream(crate::stream::StreamId::Teammate(parent_toolu), reason);
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

    /// One pass over every session's teammate pills, closing each stream whose
    /// completion is provable. Runs on the 1 Hz tick so it fires mid-session,
    /// unlike the →Idle GC. See
    /// [`Self::reconcile_finished_teammate_streams`] for the per-session rules.
    pub fn reconcile_all_finished_teammate_streams(&mut self, cx: &mut Context<Self>) {
        let session_ids: Vec<SolutionSessionId> =
            self.all_sessions().map(|e| e.read(cx).id).collect();
        for session_id in session_ids {
            self.reconcile_finished_teammate_streams(session_id, cx);
        }
    }

    /// Mid-session SELECTIVE reconcile of a session's teammate (subagent) pills.
    ///
    /// Desktop strip pills mirror `session.streams`; a teammate pill vanishes
    /// only when its `Teammate(toolu)` stream closes. The event-driven close
    /// paths ([`Self::apply_subagent_lifecycle`] for inline `Task`s,
    /// [`Self::refresh_background_agent_snapshot`] for async `Agent`s) can miss
    /// their signal (a dropped `EntryUpdated`, a missed JSONL watcher write, a
    /// last line that isn't terminal). The ONLY catch-all today is the →Idle
    /// strip GC in [`Self::mutate_state`], which is gated on the `!Idle → Idle`
    /// transition — so a session that stays busy for a long time NEVER runs it
    /// and finished-teammate pills linger until the session finally goes Idle
    /// (observed: ~1 hour). This reconcile closes such a stream the moment its
    /// completion is provable, WITHOUT waiting for →Idle.
    ///
    /// Unlike the →Idle GC (which blanket-closes every non-async teammate
    /// because Idle proves nothing is running), this must be SELECTIVE — some
    /// teammates are genuinely still running mid-session. A `Teammate(toolu)`
    /// stream is closed only when (any):
    ///   1. it is NOT a registered async `Agent` and has no matching tool-call
    ///      entry left in the thread (rewound/removed → orphaned);
    ///   2. it is an inline `Task` (its spawn tool-call's `tool_name` is NOT
    ///      agent) whose tool-call entry is TERMINAL; or
    ///   3. it is an async `Agent` (some `background_agent` has
    ///      `parent_tool_use_id == toolu`) whose latest snapshot either carries
    ///      a terminal `stop_reason` OR is stale beyond
    ///      [`MANAGED_AGENT_STALE_TIMEOUT_SECS`].
    ///
    /// It is NEVER closed for a live inline `Task` (tool-call present +
    /// non-terminal), a fresh async `Agent` (snapshot recent, no stop_reason),
    /// or — critically — an async `Agent` merely because its spawn tool-call is
    /// terminal: that is spawn-ack, the teammate streams for minutes after, and
    /// closing there is the pre-6c premature-close regression. Async
    /// classification is by the `background_agents` map (NOT the tool-call
    /// tool_name), so a spawn-ack terminal `Agent` whose registration is still
    /// pending is kept via the tool_name guard below.
    pub(crate) fn reconcile_finished_teammate_streams(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        // Cheap read-side guard: the vast majority of sessions have no teammate
        // pills, and `update` is not free.
        let has_teammate = session.read(cx).streams.keys().any(|id| {
            matches!(id, crate::stream::StreamId::Teammate(_))
        });
        if !has_teammate {
            return;
        }
        let now = std::time::SystemTime::now();
        let stale = std::time::Duration::from_secs(MANAGED_AGENT_STALE_TIMEOUT_SECS);
        let closed_any = session.update(cx, |s, _| {
            let teammate_ids: Vec<SharedString> = s
                .streams
                .keys()
                .filter_map(|id| match id {
                    crate::stream::StreamId::Teammate(toolu) => Some(toolu.clone()),
                    _ => None,
                })
                .collect();
            // (parent toolu, close reason)
            let mut to_close: Vec<(SharedString, SharedString)> = Vec::new();
            for toolu in teammate_ids {
                // Async classification FIRST and by the `background_agents` map,
                // not the tool-call tool_name: an async teammate's stream is kept
                // alive by its registration + tagged entries, so it must never
                // fall through to the tool-call rules below (rule 1 would close a
                // live async whose spawn tool-call was rewound).
                let async_agent = s
                    .background_agents
                    .values()
                    .find(|ba| ba.parent_tool_use_id.as_ref() == Some(&toolu));
                if let Some(ba) = async_agent {
                    let terminal_stop = ba
                        .latest
                        .as_ref()
                        .is_some_and(|snap| snap.stop_reason.is_some());
                    let stale_mtime = ba.latest.as_ref().is_some_and(|snap| {
                        now.duration_since(snap.mtime).unwrap_or_default() > stale
                    });
                    // An async agent whose JSONL never produced a parseable
                    // snapshot (`latest == None`) has no mtime to age from, so
                    // neither branch above ever fires and its `Teammate` pill
                    // would linger forever (the →Idle GC excludes async parents).
                    // Mirror the shell reaper's fallback: age from
                    // `registered_at` and close once older than `stale`.
                    let stale_no_snapshot = ba.latest.is_none() && {
                        let registered: std::time::SystemTime = ba.registered_at.into();
                        now.duration_since(registered).unwrap_or_default() > stale
                    };
                    if terminal_stop || stale_mtime || stale_no_snapshot {
                        let reason = ba
                            .latest
                            .as_ref()
                            .and_then(|snap| snap.stop_reason.clone())
                            .unwrap_or_else(|| gpui::SharedString::new_static("done"));
                        to_close.push((toolu, reason));
                    }
                    // else: fresh async teammate still streaming → keep.
                    continue;
                }
                // Not a registered async agent → an inline `Task` or an orphan.
                let toolcall = s.entries.iter().find_map(|e| match &e.kind {
                    crate::session_entry::SessionEntryKind::ToolCall {
                        id,
                        status,
                        tool_name,
                        ..
                    } if id.as_str() == toolu.as_ref() => {
                        Some((status.clone(), tool_name.clone()))
                    }
                    _ => None,
                });
                match toolcall {
                    None => {
                        // Rule 1: the spawn tool-call entry is gone (rewound /
                        // removed) and this is not a live async agent → orphaned.
                        to_close.push((toolu, gpui::SharedString::new_static("orphaned")));
                    }
                    Some((status, tool_name)) => {
                        if tool_name_is_agent(tool_name.as_deref()) {
                            // Spawn-ack terminal on an async `Agent` whose
                            // background_agent registration hasn't landed yet:
                            // the teammate keeps streaming. Keep it — the async
                            // branch (once registered) or the background-agent GC
                            // owns its real close. Closing here is the pre-6c
                            // premature-close bug.
                        } else if status.is_terminal() {
                            // Rule 2: inline `Task` tool-call terminal → done.
                            to_close.push((toolu, gpui::SharedString::new_static("done")));
                        }
                        // else: live inline `Task` (non-terminal) → keep.
                    }
                }
            }
            if to_close.is_empty() {
                return false;
            }
            for (toolu, reason) in to_close {
                s.close_stream(crate::stream::StreamId::Teammate(toolu), reason);
            }
            true
        });
        if closed_any {
            self.mark_subagents_changed(session_id, cx);
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
            self.teammate_watchers.clear_scan_offset(session_id);
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
            self.teammate_watchers.clear_scan_offset(session_id);
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
        let offset = match self.teammate_watchers.scan_offset(session_id) {
            Some(off) => {
                // Truncation / rotation: cursor past EOF → re-read from start.
                if off > len { 0 } else { off }
            }
            None => {
                self.teammate_watchers.set_scan_offset(session_id, len);
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
        self.teammate_watchers
            .set_scan_offset(session_id, offset + consumed);
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
    ///
    /// The staleness threshold for a still-`Running` shell depends on whether its
    /// PARENT agent subprocess is still alive (hardening #9). Output-silence is
    /// NOT death: a long silent build/`sleep` produces no output but is running.
    /// While the parent is alive its completion WILL be marked `Exited` by the
    /// parent-JSONL scan when it finishes, so a silent-Running shell is kept up to
    /// the generous [`BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS`] cap — preserving the
    /// `has_live_background_work` supervisor-suppression instead of dropping it at
    /// ~7min and letting the supervisor act while background work is still live.
    /// Only when the parent subprocess is GONE (no `acp_thread` → no completion
    /// can ever arrive, the documented orphan-leak case) does the ordinary
    /// `STALE + DEAD_LINGER` timeout apply. Terminal (`Exited`/`Killed`) shells
    /// are always reaped immediately, regardless of parent state.
    pub fn tick_background_shells(&mut self, cx: &mut Context<Self>) {
        let expiry = std::time::Duration::from_secs(
            MANAGED_AGENT_STALE_TIMEOUT_SECS + MANAGED_AGENT_DEAD_LINGER_SECS,
        );
        let live_parent_cap =
            std::time::Duration::from_secs(BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS);
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
                    // A live `acp_thread` means the owning agent subprocess is
                    // still up, so a completing shell's `<task-notification>` will
                    // still reach the parent-JSONL scan; a silent-Running shell is
                    // presumed alive and only aged out at the generous cap. No
                    // thread (reconnect / crash / close) → the shell is orphaned
                    // and can never be flipped `Exited`, so the ordinary staleness
                    // timeout applies.
                    let running_stale_threshold = if s.acp_thread().is_some() {
                        live_parent_cap
                    } else {
                        expiry
                    };
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
                            age > running_stale_threshold
                        })
                        .cloned()
                        .collect();
                    for id in &candidates {
                        s.background_shells.remove(id);
                        s.background_shell_order.retain(|x| x != id);
                    }
                    // Phase 6d-A: reaping a still-`Running`-but-stale shell drops
                    // its derived stream; rebuild so the mirror matches the map.
                    if !candidates.is_empty() {
                        s.rebuild_streams();
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

    /// Purge stale persisted `background_agents` rows on cold-load — this is
    /// NOT a restore pass.
    ///
    /// Async `Agent` subagents do not survive an editor restart: the `claude`
    /// session restarts and the subagents are gone (they stop writing their
    /// JSONL). So every persisted `BackgroundAgentRow` is stale by the time we
    /// cold-hydrate. Re-registering them (the old behavior) is exactly what made
    /// finished/dead teammate pills reappear in the console after a restart, and
    /// they were never reaped. We therefore register NONE and drop ALL rows.
    ///
    /// Their teammate streams stay collapsed: the cold-load path
    /// (`hydrate_streams_main_only`) already folds every tagged teammate stream
    /// into `hydration_orphan_streams` (rendered Main-only, no pill). With
    /// nothing re-registered here, there is no JSONL watcher, so no new tagged
    /// entries ever arrive and the orphan is never reopened — it stays collapsed.
    ///
    /// Always called inside the foreground hydrate path with the DB rows already
    /// loaded (caller pre-fetches off the foreground thread).
    pub(crate) fn reconcile_background_agents_for(
        &mut self,
        _session_id: SolutionSessionId,
        rows: Vec<crate::db::BackgroundAgentRow>,
        cx: &mut Context<Self>,
    ) {
        if rows.is_empty() {
            return;
        }
        // Treat every persisted row as dead (see doc comment): the subagents did
        // not survive the restart, so we drop all rows and register none.
        let to_drop_from_db: Vec<(String, String)> = rows
            .into_iter()
            .map(|row| (row.solution_session_id, row.agent_id))
            .collect();

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
                // An editor-injected `SystemNote` is not agent activity — it
                // must NOT flip an Idle session to Running (that would make the
                // stuck-session watchdog and the status row think a turn is in
                // flight) nor reset the silence clock. Still convert + persist +
                // delta-sync it below so it shows in the conversation.
                let is_system_note = session_entity
                    .read(cx)
                    .acp_thread()
                    .map(|t| {
                        matches!(
                            t.read(cx).entries().last(),
                            Some(acp_thread::AgentThreadEntry::SystemNote(_))
                        )
                    })
                    .unwrap_or(false);
                if !is_system_note {
                    self.mutate_state(
                        session_id,
                        |state| {
                            // Also clears a latched `Errored` — see
                            // `SessionState::resume_on_activity` (bug #5).
                            state.resume_on_activity();
                        },
                        cx,
                    );
                    if let Some(s) = self.sessions.get(&session_id).cloned() {
                        s.update(cx, |s, _| s.last_activity_at = Utc::now());
                    }
                    // Genuinely-new agent activity (NOT a system note) on a
                    // session parked in `WaitingUser`/`Stopped(Done)` means it
                    // resumed on its own — re-arm supervision so the status stops
                    // hanging at "waiting for user" while the agent works again.
                    self.rearm_supervisor_on_self_activity(session_id, cx);
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
                // Pre-extend length is the first index the newly-stamped entries
                // begin at; captured before the closure so we can stamp exactly
                // the appended entries' `mod_seq`.
                let first_new = session_entity.read(cx).entries.len();
                session_entity.update(cx, |s, cx| {
                    s.entries.extend(new_entries);
                    let new_count = s.entries.len() - first_new;
                    let seqs: Vec<u64> = (0..new_count).map(|_| s.bump_change_seq()).collect();
                    for (entry, seq) in s.entries[first_new..].iter_mut().zip(seqs) {
                        entry.mod_seq = seq;
                    }
                    s.rebuild_streams();
                    cx.notify();
                });
                // Persist authority is `streams[Main]` (phase 6b): flush the Main
                // stream incrementally after the rebuild so the coalesced,
                // Main-local rows land — NOT the (possibly torn) flat entries.
                self.persist_main_stream(session_id, cx);
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
                // A turn that runs to `Stopped` is proof the agent responded —
                // cancel any pending usage-limit / backoff resume gate so the
                // session isn't kept waiting (and a stale wake timer doesn't
                // fire a redundant judge) after the wall has cleared (#7). A
                // re-hit of the wall arrives as `Error`, not `Stopped`, so the
                // gate survives that case.
                self.clear_resume_gate_on_agent_response(session_id, cx);
                // A completed turn is genuinely-new state: cancel any parked
                // one-shot `wait`. Otherwise, if the agent self-resumed and
                // FINISHED before the wait deadline, the mechanism would still
                // wake it at the deadline ("the task you were waiting on should be
                // done — check it") minutes after it already did exactly that
                // (finding #8). A user message already clears this in the send
                // funnel; an agent-side completion must too.
                if self
                    .supervisor_states
                    .get(&session_id)
                    .is_some_and(|s| s.wait_until_ms.is_some())
                {
                    if let Some(state) = self.supervisor_states.get_mut(&session_id) {
                        state.wait_until_ms = None;
                    }
                    self.persist_supervisor_state(session_id, cx);
                }
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
                // Flush any pending end-of-turn entry-update debounce SYNCHRONOUSLY.
                //
                // The last assistant text of a turn arrives via `EntryUpdated`,
                // whose `SessionMessageAppended` emit (and thus its
                // `agent_session_dirty` re-poll signal) is debounced 500 ms / 2 s
                // to coalesce a streaming burst. At turn end that pending debounce
                // task is the ONLY append signal carrying the final flushed tail,
                // and it is fragile: if `Stopped` does not change the state
                // discriminant (e.g. the session was already Idle / Stopping, so
                // `mark_state_changed` below emits no dirty), or the debounce task
                // is dropped before it fires, the final entry's append notification
                // never reaches the client. The mobile then keeps showing the
                // turn WITHOUT its last message until the next client→server
                // interaction re-polls — the bug this flush fixes. Emitting the
                // queued append here (and clearing the slot so it can't double-fire
                // when its timer elapses) guarantees the final entry's
                // `SessionMessageAppended` + `agent_session_dirty` ride out
                // immediately on the turn-completion tick.
                self.flush_pending_entry_appends(session_id, cx);
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
                            self.model_catalog.set_models(agent_id, models.clone());
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
            acp_thread::AcpThreadEvent::Error => {
                // Symmetric with the `Stopped` arm: flush any pending end-of-turn
                // entry-append throttle synchronously so the final entry's
                // `SessionMessageAppended` (+ `agent_session_dirty`) rides out on
                // the turn-error tick rather than depending on the 500 ms timer —
                // which, if `Running→Errored` doesn't change the state
                // discriminant (already Errored), would be the only remaining
                // dirty signal.
                self.flush_pending_entry_appends(session_id, cx);
                // A provider usage/session-limit wall can arrive as a fast `Error`
                // (not only as the silent stall the stuck-turn watchdog catches):
                // the worker's fast-error path lands here before the watchdog's
                // silence window elapses. The generic "agent error" string would
                // then bury the reset time. Classify the wall from the session's
                // own last assistant message and, for a SUPERVISED session, hand
                // off to quota recovery so the observer schedules an auto-resume at
                // the reset (mirroring the stuck-watchdog wall branch). For an
                // unsupervised session, at least surface the wall text so the user
                // sees when it resets. `apply_usage_limit_stop` is intentionally
                // NOT called in the unsupervised case: it would leave a diary
                // breadcrumb for a session that has no observer.
                match self.session_wall_message(session_id, cx) {
                    Some(wall) => {
                        let supervised = self
                            .supervisor_states
                            .get(&session_id)
                            .is_some_and(|s| s.enabled);
                        self.mutate_state(
                            session_id,
                            |state| {
                                *state = SessionState::Errored(SharedString::from(wall.clone()))
                            },
                            cx,
                        );
                        if supervised {
                            self.push_system_note(
                                session_id,
                                acp_thread::SystemNoteLevel::Error,
                                "Достигнут лимит claude — текущий ход остановлен.",
                                cx,
                            );
                            self.apply_usage_limit_stop(session_id, &wall, cx);
                        }
                    }
                    None => {
                        self.mutate_state(
                            session_id,
                            |state| {
                                *state = SessionState::Errored(SharedString::from("agent error"))
                            },
                            cx,
                        );
                    }
                }
            }
            acp_thread::AcpThreadEvent::LoadError(_) => {
                // A thread load/reconnect failure — distinct from a turn wall, so
                // no wall-classification here (a stale prior wall in the transcript
                // must not schedule a spurious resume). Flush + generic error,
                // same as before.
                self.flush_pending_entry_appends(session_id, cx);
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
                    // Rebuild the streams FIRST so `streams[Main]` reflects the
                    // truncated, re-coalesced transcript, THEN re-stamp on Main.
                    s.rebuild_streams();
                    // Decision #11 re-homed onto the Main stream (phase 6b): a
                    // truncate that splits a coalesced same-source assistant group
                    // leaves the Main-stream survivor's content changed (a fragment
                    // removed) but its first-fragment mod_seq unchanged — possibly
                    // BELOW a delta client's cursor or `persisted_main_seq` — while
                    // the stream's `total_count` is unchanged (the removed fragment
                    // was coalesced INTO the survivor, not a separate stream entry).
                    // Since the per-stream wire delta AND `persist_main_stream` now
                    // both key on the Main stream (`entry.mod_seq > watermark`), the
                    // re-stamp must land on the Main stream's boundary entry, not
                    // the flat one. Bump the surviving Main entry's mod_seq to a
                    // fresh change_seq and lift `streams[Main].seq` to it so the
                    // next delta re-delivers the now-shorter entry and
                    // `persist_main_stream` re-upserts its row.
                    let seq = s.bump_change_seq();
                    if let Some(main) = s.streams.get_mut(&crate::stream::StreamId::Main)
                        && let Some(last) = main.entries.last_mut()
                    {
                        last.mod_seq = seq;
                        main.seq = seq;
                    }
                    cx.notify();
                });
                // Persist authority is `streams[Main]` (phase 6b): a rewind drops
                // the removed rows and shrinks the coalesce survivor.
                // `persist_main_stream` trims via `delete_entries_from(main_len)`
                // AND re-upserts the re-stamped survivor (its mod_seq now exceeds
                // `persisted_main_seq`), keeping the persisted transcript in lockstep
                // so a stale idx>=len row can't corrupt the next cold load.
                self.persist_main_stream(session_id, cx);
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
                // A streaming update on a non-system entry (assistant-text
                // chunk, tool-status transition) is proof the agent is live, so
                // clear a latched `Errored` — the visible "Error while
                // streaming" symptom is EntryUpdated-driven (bug #5). Mirror the
                // NewEntry arm's SystemNote guard so an injected note can't flip
                // state.
                let updated_is_system_note = self
                    .sessions
                    .get(&session_id)
                    .and_then(|s| s.read(cx).acp_thread().cloned())
                    .map(|t| {
                        matches!(
                            t.read(cx).entries().get(*idx),
                            Some(acp_thread::AgentThreadEntry::SystemNote(_))
                        )
                    })
                    .unwrap_or(false);
                if !updated_is_system_note {
                    self.mutate_state(
                        session_id,
                        |state| {
                            state.clear_error_on_activity();
                        },
                        cx,
                    );
                    // A streaming chunk or a tool-status transition is agent
                    // activity too, so it must reset the silence clock the
                    // stuck-session watchdog reads — exactly like `NewEntry`
                    // does above. Without this, a long silent FOREGROUND command
                    // (one `NewEntry` at tool start, then minutes blocked while
                    // streaming nothing, then a terminal-status `EntryUpdated`
                    // when it finishes) leaves `last_activity_at` frozen at
                    // tool-start: the instant the tool leaves `InProgress` the
                    // watchdog's `TOOL_STUCK_SECS` shield drops while `silent_secs`
                    // is already >= `STUCK_TURN_SECS`, so a perfectly-alive agent
                    // is falsely declared wedged and reconnected the moment its
                    // command completes.
                    if let Some(s) = self.sessions.get(&session_id).cloned() {
                        s.update(cx, |s, _| s.last_activity_at = Utc::now());
                    }
                }
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
                        let mut entry = crate::session_entry::to_session_entry(live_entry, cx);
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
                        s.rebuild_streams();
                        cx.notify();
                    });
                    // Row upsert happens unconditionally on the in-memory update;
                    // the 500ms/2s throttle above governs only the MCP
                    // `SessionMessageAppended` emit, NOT this persist. Persist
                    // authority is `streams[Main]` (phase 6b): flush the Main
                    // stream incrementally after the rebuild so the coalesced
                    // Main-local row lands (the edited flat entry may map to a
                    // coalesced Main entry at a different index).
                    self.persist_main_stream(session_id, cx);
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
        self.persist_change_seq(session_id, cx);
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
        self.persist_change_seq(session_id, cx);
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
        self.persist_change_seq(session_id, cx);
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
        // Ephemeral supervisor judge/auditor sessions are invisible: their own
        // Idle->Running->Idle churn must not reach mobile. `remote_control`'s
        // allow-list forwards every `workspace.*` event to the phone, so an
        // unfiltered emit here leaks the hidden judge on every supervisor
        // wake-up. Mirrors the create/close-side suppression. The supervised
        // session's own state changes still emit (only the EPHEMERAL session's
        // do not), and the internal `SessionStateChanged` event is untouched —
        // `message_generator` relies on it to detect the judge going Idle.
        if entity.read(cx).is_supervisor_ephemeral || entity.read(cx).is_ephemeral {
            return;
        }
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
            let cleared = session.update(cx, |s, _| {
                // Close each stranded inline-Task teammate stream on →Idle. The
                // desktop snap-back (`next_selection_after_change`) recovers a
                // viewer pinned to a vanished tab by watching `streams`, so sourcing
                // the ids from `s.streams` teammate keys keeps the pill and the
                // stream in lockstep — `close_stream` also reclaims each one's
                // `teammate_labels` entry.
                //
                // Async `Agent` teammates OUTLIVE the parent turn (their stream
                // closes on the real stop_reason via the background-agent GC), so
                // they must be EXCLUDED here — identified by having a registered
                // `background_agent` whose `parent_tool_use_id` is the stream id.
                // Closing them here would suppress a still-live async teammate
                // (decision #5) and drop its label.
                let async_parents: std::collections::HashSet<SharedString> = s
                    .background_agents
                    .values()
                    .filter_map(|ba| ba.parent_tool_use_id.clone())
                    .collect();
                let stranded: Vec<SharedString> = s
                    .streams
                    .keys()
                    .filter_map(|id| match id {
                        crate::stream::StreamId::Teammate(toolu)
                            if !async_parents.contains(toolu) =>
                        {
                            Some(toolu.clone())
                        }
                        _ => None,
                    })
                    .collect();
                if stranded.is_empty() {
                    return false;
                }
                for id in stranded {
                    s.close_stream(
                        crate::stream::StreamId::Teammate(id),
                        gpui::SharedString::new_static("orphaned"),
                    );
                }
                true
            });
            // The clear is a strip mutation: route it through the watermark+emit
            // helper so `SessionSubagentsChanged` fires (desktop strip re-render)
            // and `subagents_seq` advances. The mobile delta now always sends
            // the strip, but the bump keeps the watermark honest and the emit
            // drives the dirty poke — never mutate a section silently (the same
            // invariant `mark_state_changed` documents).
            if cleared {
                self.mark_subagents_changed(session_id, cx);
            }
        }
        let now = std::time::Instant::now();
        let is_focused = self
            .focus_resolver
            .as_ref()
            .map(|f| f(session_id, cx))
            .unwrap_or(false);
        // "Agent finished" must only fire when the session is genuinely
        // quiescent. Beyond a pending queue, two more things mean "more work
        // is coming without the user": (a) the agent is idle OVER a live
        // background command/agent it launched (it resumes on its own when
        // that finishes — the same `has_live_background_work` the supervisor
        // watchdog uses), and (b) the Observer is enabled and auto-driving
        // (`Watching`/`Judging`) — it will nudge onward and fires its own
        // done/ask notification when the work actually concludes.
        let (has_pending_messages, has_live_background_work, is_supervisor_ephemeral) = {
            let s = session.read(cx);
            let has_live_background_work = s.background_shells.values().any(|sh| {
                matches!(sh.state, crate::background_shell::ShellRuntimeState::Running)
            }) || s.background_agents.values().any(|a| a.is_messageable());
            (
                !s.pending_messages.is_empty(),
                has_live_background_work,
                s.is_supervisor_ephemeral,
            )
        };
        let supervisor_will_continue = self.supervisor_states.get(&session_id).is_some_and(|st| {
            st.enabled
                && matches!(
                    st.status,
                    crate::supervisor::SupervisorStatus::Watching
                        | crate::supervisor::SupervisorStatus::Judging
                )
        });
        // Ephemeral supervisor judge/auditor sessions are invisible — no tray
        // toast for an Errored judge or one whose turn crosses JUDGE_TIMEOUT_SECS.
        // Same suppression intent as the wire emit above; internal bookkeeping
        // (state transitions, subagent GC, Stopping safety-net) already ran.
        if !is_supervisor_ephemeral
            && let Some(decision) = notifier::decide_notification(
                session_id,
                &previous,
                &next,
                now,
                is_focused,
                has_pending_messages,
                has_live_background_work,
                supervisor_will_continue,
            )
        {
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
            SolutionStoreEvent::Changed => {
                // A member add/remove (among other store mutations) lands here.
                // First reap whole vanished solutions, then individual sessions
                // whose member directory was just removed.
                self.gc_orphan_solutions(cx);
                self.gc_orphan_members(cx);
            }
            SolutionStoreEvent::Deleted { id, root } => {
                // Authoritative solution-delete cleanup: `delete_solution` emits
                // `Changed` (which may already have purged the hydrated sessions
                // via `gc_orphan_solutions`) and THEN `Deleted` carrying the root
                // captured before removal. We funnel into the consolidated
                // solution-level hard purge with that root so the wholesale
                // `<root>/.agents` removal + all-six-table DB sweep run even for
                // never-hydrated sessions. Idempotent against the earlier
                // `Changed`-driven purge (by_solution is already empty → just the
                // DB sweep + `.agents` removal).
                self.purge_solution_fully(id.clone(), Some(root.clone()), cx);
            }
            SolutionStoreEvent::Closed { id } => self.cold_close_solution(id, cx),
            SolutionStoreEvent::Opened { id } => {
                // On window open, force-load this solution's persisted sessions
                // (existing orphans are `closed_at IS NULL` but un-hydrated), then
                // GC any whose member dir no longer exists. Without the hydrate,
                // an orphan already in the DB before this feature would never be
                // loaded — and so never purged — until it was touched some other
                // way.
                let id = id.clone();
                cx.spawn(async move |this, cx| {
                    let hydrate = this
                        .update(cx, |this, cx| this.hydrate_all_for_solution(id.clone(), cx))
                        .log_err();
                    if let Some(task) = hydrate {
                        task.await.log_err();
                    }
                    this.update(cx, |this, cx| this.gc_orphan_members(cx))
                        .log_err();
                })
                .detach();
            }
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
        // Reap each session's in-flight judge/auditor (closes their hidden child
        // sessions) and drop ALL per-session runtime maps — this path bypasses
        // `close_session`, so without it the supervisor state / watcher tasks /
        // judge handles for every session in a closed-window solution leak.
        for id in &session_ids {
            self.finish_judge(*id, cx);
            self.finish_auditor(*id, cx);
        }
        self.by_solution.remove(solution_id);
        for id in &session_ids {
            self.sessions.remove(id);
            self.evict_session_runtime_maps(*id);
        }
        // Drop the pool's connection handle(s) for this solution. Together
        // with the session eviction above (whose entities release their own
        // connection refs once the closing window's views tear down) this
        // releases the last Rc, so the subprocess exits now instead of
        // lingering for the 60s idle debounce.
        let mut pool = self.pool.lock();
        let keys: Vec<(SolutionId, AgentServerId)> = pool.keys_for_solution(solution_id).collect();
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
        // Funnel every vanished solution through the single solution-level hard
        // primitive. A `Changed`-detected vanish carries no root (the store
        // mapping is already gone), so `.agents` wholesale removal is skipped —
        // the per-session purges still drop each hydrated `.agents/<sid>`, and
        // the authoritative `Deleted` event (with the captured root) handles the
        // wholesale `.agents` sweep when a real delete is the cause.
        for sid in orphan_ids {
            self.purge_solution_fully(sid, None, cx);
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
    fn stream_auto_close_on_terminal_excludes_async_agent() {
        // Phase 3 gate: a teammate stream auto-closes on the tool-call's
        // TERMINAL status only for an inline `Task` (whose tool-call stays
        // InProgress for the whole run → terminal == genuinely done). An async
        // `Agent`'s spawn tool-call goes terminal at spawn-ack while the teammate
        // streams on, so it must NOT auto-close here. The gate is
        // `!tool_name_is_agent(tool_name)`.
        assert!(!tool_name_is_agent(Some("Task")), "Task → auto-close");
        assert!(tool_name_is_agent(Some("Agent")), "Agent → do NOT auto-close");
        assert!(
            tool_name_is_agent(Some("agent")),
            "case-insensitive: agent → do NOT auto-close"
        );
        assert!(!tool_name_is_agent(None), "unknown tool → auto-close path");
    }

}
