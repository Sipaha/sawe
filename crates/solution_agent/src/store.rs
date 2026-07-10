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
mod hydration;
mod teardown;
mod supervisor_engine;
mod teammate_reconciler;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
pub(crate) mod tests;

pub(crate) use queue::{QUEUE_HINT_LINE, TS_PREFIX_CLOSE, TS_PREFIX_OPEN};
pub(crate) use supervisor_engine::{JudgeHandle, VerdictAuth};
// Free helpers + companion consts relocated into `teammate_reconciler`; re-exported
// so `crate::store::X` stays reachable for the sibling `supervisor_engine` module
// (`use super::*`), the inline `#[cfg(test)]` modules below, and the
// `store/tests/{teammate_reconciler,teardown}.rs` buckets that call them.
pub(crate) use teammate_reconciler::{
    KEEP_RAW_TRANSCRIPTS, claude_project_dir_for, parent_session_jsonl_for,
    push_and_evict_transcripts,
};
// Only reached from the inline `#[cfg(test)]` modules below and the `store/tests/*`
// buckets; their non-test callers live inside `teammate_reconciler` itself, so gate
// the re-export to avoid an unused-import warning in release builds.
#[cfg(test)]
pub(crate) use teammate_reconciler::{
    PARENT_JSONL_READ_CAP, background_agent_dir_for, read_complete_lines_from,
    scan_lines_for_completions,
};

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

pub use crate::model::{PersistedEntry, PersistedRole};
pub(crate) use queue::summarize_blocks_for_log;
// Session-hydration / cold→live-resume cluster relocated into `hydration`;
// re-exported so `crate::store::X` keeps resolving for the staying store.rs
// methods that still call these helpers (`persist_session_row`,
// `create_session_with_parent`, `reconnect_agent`), the `mcp/read.rs`
// consumers of `PersistedSession`/`entries_from_rows`, and the
// `store/tests/{hydration,model_catalog}.rs` buckets.
pub use hydration::PersistedSession;
pub(crate) use hydration::entries_from_rows;
use hydration::{extract_preview, unique_session_title};
// Every store.rs caller of `cold_entries_from_persisted` moved into `hydration`;
// only the `store/tests/hydration.rs` bucket still reaches it via
// `crate::store::cold_entries_from_persisted`, so gate the re-export to avoid a
// release unused-import warning.
#[cfg(test)]
pub(crate) use hydration::cold_entries_from_persisted;
// Archive-GC/purge + session-teardown cluster relocated into `teardown`. The
// free `stale_archive_dirs` helper is re-exported so `crate::store::X` keeps
// resolving for the staying `hydration.rs` reap path (`use super::*`) and the
// `store/tests/teardown.rs` bucket; the relocated methods are instance methods
// on `SolutionAgentStore`, so they need no re-export.
pub(crate) use teardown::stale_archive_dirs;

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
