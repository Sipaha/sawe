use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use acp_thread::AcpThread;
use agent_client_protocol::schema as acp;
use chrono::{DateTime, Utc};
use gpui::{Context, Entity, EventEmitter, SharedString, Subscription, Task};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use solutions::SolutionId;

use crate::background_agent;
use crate::background_shell;
use crate::session_entry::SessionEntry;

/// Length of a `SolutionSessionId` in ASCII characters. 8 chars over a
/// 36-char alphabet ≈ 36⁸ ≈ 2.8 × 10¹² combinations — comfortably
/// collision-free for the realistic upper bound of a few thousand
/// sessions per user, while staying short enough to read at a glance
/// and to use as a filesystem path component (`<root>/.agents/<id>/`).
const SHORT_ID_LEN: usize = 8;

/// Alphabet for fresh session ids: lowercase ASCII + digits. Avoids
/// shell-special chars, mixed case, and `_-` so the id is safe in
/// filesystem paths, JSON-RPC frames, and shell commands without
/// quoting.
const SHORT_ID_ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// SPK-Editor-internal session id. Distinct from `acp::SessionId`,
/// which is the per-subprocess ACP-level identifier.
///
/// Stored as a fixed-size ASCII array so the type stays `Copy` (lots
/// of HashMap-key usage), but rendered as a string everywhere it
/// crosses the I/O boundary (DB rows, MCP JSON, file paths). Old
/// 36-char UUID strings persisted by earlier builds are still
/// parseable: see `parse` for the legacy migration.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SolutionSessionId([u8; SHORT_ID_LEN]);

impl SolutionSessionId {
    pub fn new() -> Self {
        // Rejection-sample so the alphabet stays uniform: `% 36` on a
        // single u8 would over-represent the first 4 letters (256 mod
        // 36 = 4). Sampling 1 byte at a time and dropping anything ≥
        // 252 (= 7 × 36) gives a flat distribution.
        let mut bytes = [0u8; SHORT_ID_LEN];
        let mut rng = rand::rng();
        let mut buf = [0u8; 1];
        for slot in &mut bytes {
            loop {
                rng.fill_bytes(&mut buf);
                let x = buf[0];
                if (x as usize) < 252 {
                    *slot = SHORT_ID_ALPHABET[(x as usize) % 36];
                    break;
                }
            }
        }
        Self(bytes)
    }

    /// Accepts:
    ///   1. The current `[a-z0-9]{8}` form (fresh ids written by `new`).
    ///   2. A legacy hyphenated UUID (any version) persisted by older
    ///      builds — collapsed to the first 8 hex chars of its u128
    ///      representation. Lossy in theory (8 hex chars = 32 bits)
    ///      but every UUID prefix is uniquely identifying inside one
    ///      user's DB, and we'd rather migrate in-place than wipe the
    ///      session history.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        if s.len() == SHORT_ID_LEN && s.bytes().all(|b| SHORT_ID_ALPHABET.contains(&b)) {
            let mut bytes = [0u8; SHORT_ID_LEN];
            bytes.copy_from_slice(s.as_bytes());
            return Ok(Self(bytes));
        }
        if let Ok(uuid) = uuid::Uuid::parse_str(s) {
            // `:032x` pads to 32 hex chars regardless of leading-zero
            // UUIDs so the `take` below always finds 8 characters.
            let hex = format!("{:032x}", uuid.as_u128());
            let mut bytes = [0u8; SHORT_ID_LEN];
            for (i, slot) in bytes.iter_mut().enumerate() {
                *slot = hex.as_bytes()[i];
            }
            return Ok(Self(bytes));
        }
        anyhow::bail!("invalid session id: {s:?}")
    }

    pub fn as_str(&self) -> &str {
        // Bytes are constructed only from ASCII alphabet entries (in
        // `new`) or copied from an already-validated string (in
        // `parse`), so the array is always valid UTF-8.
        std::str::from_utf8(&self.0).expect("invariant: short id is ASCII")
    }
}

impl std::fmt::Display for SolutionSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Debug for SolutionSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SolutionSessionId({})", self.as_str())
    }
}

impl Serialize for SolutionSessionId {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SolutionSessionId {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Identifier of a registered `AgentServer` (e.g. `claude-acp`, `codex`).
/// Mirrors `acp_thread::AgentId` / `agent_servers` naming for transparent passing.
pub type AgentServerId = SharedString;

#[derive(Clone, Debug)]
pub enum SessionState {
    Idle,
    Running {
        started_at: Instant,
        notified: bool,
    },
    /// A cancel was requested; the turn has not yet ended. Bounded by the
    /// backend's 30s interrupt→kill escalation AND by the queue-level
    /// safety net (~40s wall-clock) that force-flips Stopping→Idle if no
    /// natural `Stopped` event arrives — covers the
    /// `claude_native::connection::cancel` no-op race where `prompt_tx`
    /// was already consumed at cancel time and the AcpThread chain never
    /// emits `Stopped`. `started_at` is the monotonic anchor the safety
    /// net uses to compute "Stopping… N seconds" for diagnostics.
    Stopping {
        started_at: Instant,
    },
    AwaitingInput,
    Errored(SharedString),
}

impl SessionState {
    pub fn is_terminal_for_notification(&self) -> bool {
        matches!(self, Self::Idle | Self::AwaitingInput | Self::Errored(_))
    }

    /// Flip an inactive/stale state to `Running` because genuine (non-system)
    /// agent activity just arrived — a new entry or a streaming update. Returns
    /// `true` when it transitioned. Crucially this clears a latched
    /// `Errored`: an `AcpThreadEvent::Error` can fire on a transient/recoverable
    /// condition while the SAME subprocess keeps streaming (claude_native keeps
    /// its pump alive), and the error paths deliberately never emit `Stopped`,
    /// so without this the status row stays red "Error: …" forever while text
    /// streams (bug #5). A genuinely terminal error still surfaces: no further
    /// entries arrive, so this is never called, and the eventual `Stopped`
    /// settles the session to `Idle`. No-op (returns `false`) when already
    /// `Running`/`Stopping`, so an in-flight turn's `notified` flag and a
    /// pending cancel are never disturbed.
    pub fn resume_on_activity(&mut self) -> bool {
        if matches!(self, Self::Idle | Self::AwaitingInput | Self::Errored(_)) {
            *self = Self::Running {
                started_at: Instant::now(),
                notified: false,
            };
            true
        } else {
            false
        }
    }

    /// Narrower sibling of [`resume_on_activity`](Self::resume_on_activity) for
    /// an in-place streaming update (`EntryUpdated`): clear a latched `Errored`
    /// (bug #5) but leave `Idle`/`AwaitingInput` ALONE. A finished turn can
    /// still receive late streaming-reveal `EntryUpdated`s after its `Stopped`
    /// flushed the buffer; resurrecting it to `Running` would wrongly show
    /// "Thinking…" on a turn that already ended. Only a genuinely new entry
    /// (`NewEntry`, via `resume_on_activity`) starts a fresh turn.
    pub fn clear_error_on_activity(&mut self) -> bool {
        if matches!(self, Self::Errored(_)) {
            *self = Self::Running {
                started_at: Instant::now(),
                notified: false,
            };
            true
        } else {
            false
        }
    }

    /// Short, user-facing status label. Use this in the UI instead of
    /// `format!("{:?}", state)` — Debug renders the `Running` variant as
    /// `Running { started_at: Instant { tv_sec: 148873, tv_nsec: ... } }`,
    /// which is what we previously leaked into the session-view header.
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Running { .. } => "Running",
            Self::Stopping { .. } => "Stopping",
            Self::AwaitingInput => "Awaiting input",
            Self::Errored(_) => "Error",
        }
    }
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.short_label())
    }
}

/// 1-based counter of how many context-windows this session has
/// chewed through. Starts at 1 and increments each time the user
/// compacts: the dump dir for the *current* context is named after
/// this number (`.agents/<sid>/c01/`, `c02/`, …) so all rotations of
/// one logical conversation share a single `<sid>` directory.
pub type SessionContextCount = u32;

/// One conversation entry as persisted in the `solution_sessions.acp_thread_blob`
/// payload. Used by the navigator's cold-tab renderer to display the
/// dialog before the agent subprocess is spawned (and by MCP tools that
/// archive transcripts). `markdown` is what `acp_thread::AgentThreadEntry::to_markdown`
/// produced at save time; `role` is the only extra info the cold
/// renderer needs to apply user/assistant/tool styling.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PersistedEntry {
    pub role: PersistedRole,
    pub markdown: String,
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PersistedRole {
    User,
    Assistant,
    /// Tool call + result. Rendered as a collapsed block in cold mode.
    Tool,
    /// Agent-emitted plan (`AgentThreadEntry::CompletedPlan`). Rendered
    /// like an assistant message but tagged so future styling tweaks
    /// can distinguish.
    Plan,
    /// Legacy blob entry where the per-entry role wasn't recorded —
    /// `entry_summaries` is the only data we have, and an idx-based
    /// User/Assistant guess mis-roles every conversation that includes
    /// tool calls. The cold renderer paints these as a neutral block
    /// so a misleading role label doesn't smear across the dialog.
    Archived,
}

/// Sentinel stored in `SolutionSession::entry_created_ms` (and the persisted
/// mirror) for an entry whose creation time was never captured — e.g. a
/// message that predates the timestamp feature, surfaced through a resumed
/// pre-feature session. Real unix-millis timestamps are always positive, so a
/// negative marker is unambiguous. The wire layer maps this to "no time"
/// rather than fabricating one.
pub(crate) const NO_TIMESTAMP_MS: i64 = -1;

/// Which agent a queued follow-up is addressed to. Stamped at enqueue from
/// the tab the user typed it on (`session_view::selected_stream`). The
/// firing hook's `agent_id` selects which bundles drain: the MAIN agent's
/// hook (`agent_id == None`) drains [`QueueTarget::Main`] bundles; an Agent
/// Teams teammate's hook (`agent_id == Some(x)`) drains only
/// [`QueueTarget::Subagent`]`(x)` bundles.
///
/// A `Subagent` bundle whose addressee finishes without draining it is
/// DROPPED at turn end — never re-routed to the main agent (a follow-up
/// written for teammate X is meaningless to the parent). Lives only in
/// memory alongside [`SolutionSession::pending_messages`], so no
/// serialization compatibility is required.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueTarget {
    Main,
    /// `claude` `agent_id` of the addressee teammate — equal to its
    /// [`background_agent::BackgroundAgentId`] (same managed-agent id
    /// namespace), so the hook's `agent_id` matches it byte-for-byte.
    Subagent(SharedString),
}

impl QueueTarget {
    /// True when a hook firing with the given `agent_id` should drain a
    /// bundle addressed to this target. `Main` bundles drain on the main
    /// agent's hook (which carries no `agent_id`); `Subagent(x)` bundles
    /// drain only on the matching teammate's hook.
    pub fn matches_hook(&self, agent_id: Option<&str>) -> bool {
        match (self, agent_id) {
            (QueueTarget::Main, None) => true,
            (QueueTarget::Subagent(id), Some(hook_id)) => id.as_ref() == hook_id,
            _ => false,
        }
    }
}

/// One queued follow-up bundle: the (timestamp-baked) content blocks plus
/// the agent they're addressed to. Consecutive same-target follow-ups merge
/// into one bundle's `blocks` (so the agent gets a single prompt); a
/// differently-targeted follow-up starts a new bundle, letting the queue
/// hold e.g. one `Main` bundle and one `Subagent` bundle simultaneously.
#[derive(Clone, Debug)]
pub struct PendingBundle {
    pub target: QueueTarget,
    pub blocks: Vec<acp::ContentBlock>,
}

/// Live, in-memory representation of one Solution-scoped AI session.
///
/// `acp_thread` is `Option` because a `SolutionSession` may exist briefly
/// without a constructed `AcpThread` (e.g. during test scaffolding before
/// Task 3.3 wires the real subprocess pool, or in the moment between
/// session row creation and ACP `new_session` resolving). Production callers
/// added in Task 3.3 will populate it before exposing the session to the UI.
pub struct SolutionSession {
    pub id: SolutionSessionId,
    pub solution_id: SolutionId,
    pub agent_id: AgentServerId,
    pub acp_session_id: acp::SessionId,
    /// Live thread, when one is attached. **Private**: any write must
    /// go through [`SolutionSession::set_acp_thread`] so subscribers
    /// (notably `SolutionSessionView::_thread_subscription`) re-attach.
    /// Read with [`SolutionSession::acp_thread`].
    acp_thread: Option<Entity<AcpThread>>,
    pub title: SharedString,
    pub created_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    pub state: SessionState,
    /// Working directory the session was originally created against.
    /// claude-acp buckets sessions by encoded cwd under
    /// `~/.claude/projects/<encoded-cwd>/<acp-session-id>.jsonl`, so
    /// resume must replay the SAME cwd or the agent returns
    /// `Resource not found`. Empty `PathBuf` means "fall back to
    /// `solution.root`" — used for legacy DB rows that pre-date the
    /// column.
    pub cwd: PathBuf,
    /// 1-based count of how many context-windows this session has
    /// burned through. `1` for a fresh session, `2` after one
    /// compact, etc. The compact dump dir for the *current* context
    /// is named `c<context_count>` so a single `.agents/<sid>/`
    /// directory groups every rotation of one logical conversation.
    pub context_count: SessionContextCount,
    /// Project the session was created against. Cached so `restart_agent`
    /// can re-issue `create_session` without the caller having to reach
    /// back into a workspace window. `None` for prebuilt-session test
    /// scaffolding that never went through `create_session`.
    pub project: Option<Entity<project::Project>>,
    /// Subscription to the `AcpThread`'s `AcpThreadEvent` stream. Held so
    /// the callback registered by `SolutionAgentStore::subscribe_to_session`
    /// stays alive for the lifetime of the session. Underscore-prefixed
    /// because nothing reads it back; dropping it implicitly unsubscribes.
    pub _acp_subscription: Option<Subscription>,
    /// FIFO queue of user messages submitted while the session was
    /// already running. The store flushes the queue on `Stopped` —
    /// matches the Claude Code CLI experience where you can keep
    /// typing follow-ups while the agent is still working.
    pub pending_messages: VecDeque<PendingBundle>,
    /// One-shot signal set by `interrupt_and_flush_pending`: tells the
    /// next `Stopped(Cancelled)` handler to FLUSH `pending_messages`
    /// instead of clearing them. Without it, `Cancelled` (the user
    /// pressed Stop) drops the queue — which is the right default,
    /// but the "Send now" button needs the inverse behaviour.
    pub flush_after_cancel: bool,
    /// Length of the cold-restored prefix in `entries` at the moment the live `AcpThread` was
    /// attached. Used by the live-event handlers (`NewEntry`/`EntryUpdated`/`EntriesRemoved`)
    /// to map a live thread's local entry index to a global index in `entries`. Set by
    /// `set_acp_thread` to `entries.len()` when a thread is attached, and reset to `0` when
    /// detached. For sessions that have never been restored from cold (fresh sessions or sessions
    /// after `reset_context`/`rotate_context`), this is `0`.
    pub live_base: usize,
    /// Internal 1:1 mirror of the live `AcpThread.entries` — the mod_seq-carrying
    /// ingest buffer and the INPUT to the per-source demux (`rebuild_streams`).
    /// As of phase 6b this is NO LONGER the persisted authority (`streams[Main]`
    /// is — see `persist_main_stream`) and, after the phase-6b revert of quick-fix
    /// #3, MAY be torn / interleaved in memory (concurrent teammate chunks split a
    /// parent bubble). That is harmless: render, wire, and persist all read the
    /// coalesced `streams`; only a few dev/notification readers still touch flat
    /// `entries` (see brief §D) and they stay 1:1-aligned with `AcpThread`.
    /// Mutated only through `set_entries` setter or the store's ingest arms.
    pub entries: Vec<SessionEntry>,
    /// Per-source stream mirror of `entries`, maintained by `rebuild_streams`
    /// after every `entries` mutation. Source of truth for per-stream rendering,
    /// the wire (phases 2c-5), AND persistence (`streams[Main]`, phase 6b).
    /// `StreamId::Main` is always present.
    pub streams: indexmap::IndexMap<crate::stream::StreamId, crate::stream::Stream>,
    /// Secondary streams auto-closed on a genuine `Done` signal. Currently fed
    /// only by an inline `Task` subagent's tool-call reaching a terminal status
    /// (its tool-call stays InProgress for the whole run, so terminal == done).
    /// An async `Agent` teammate is deliberately NOT closed here — its spawn
    /// tool-call goes terminal at spawn-ack while the teammate streams on, so its
    /// demux stream must stay live (its real done-signal drives its close in a
    /// later phase). Ephemeral, NOT persisted: the tagged entries stay in
    /// `entries` (persistence/flat wire untouched until later phases), so
    /// `rebuild_streams` consults this overlay and removes the closed ids —
    /// otherwise a finished teammate's stream would reappear from its
    /// still-present tagged entries. Value = the `Done` reason (surfaced as
    /// phase-4's `stream_removed` reason). `StreamId::Main` is never inserted.
    pub closed_streams: HashMap<crate::stream::StreamId, SharedString>,
    /// Cold-load hydration orphans: finished teammates re-demuxed from restored
    /// tagged rows. Suppressed from the mirror while the session is cold, but —
    /// unlike a permanent `closed_streams` Done-close — REOPENED if a live resume
    /// streams new activity (an entry at index >= `hydration_watermark`) for that
    /// toolu. Async `Agent` subagents do not survive an editor restart, so their
    /// persisted `background_agents` rows are dropped on cold-load and never
    /// re-registered (see `SolutionAgentStore::reconcile_background_agents_for`);
    /// with no watcher re-armed, no new tagged rows arrive, so this overlay keeps
    /// each such finished teammate collapsed permanently. Ephemeral, not persisted.
    pub hydration_orphan_streams: std::collections::HashSet<crate::stream::StreamId>,
    /// `entries.len()` captured when `hydrate_streams_main_only` ran — the boundary
    /// between the restored cold prefix and any post-restart (resume-streamed)
    /// entries. An orphan with an entry at index >= this boundary is being streamed
    /// anew and is reopened; a purely-cold orphan (all entries below it) stays
    /// suppressed. 0 when no hydration happened (the orphan set is then empty and
    /// the check is skipped).
    pub hydration_watermark: usize,
    /// Watermark for the incremental persist of the Main stream (phase 6b): the
    /// `streams[StreamId::Main].seq` (= max `mod_seq` over Main's entries) as of
    /// the last `persist_main_stream` flush. `persist_main_stream` only upserts
    /// Main-local rows whose `mod_seq` exceeds this, then advances it — so a
    /// steady-state ingest re-writes only the touched boundary entry, not the
    /// whole transcript. Ephemeral, NOT persisted to the DB: reset to 0 on
    /// `clear_closed_streams` (context reset / clear) and seeded to
    /// `streams[Main].seq` at the end of `hydrate_streams_main_only` so freshly
    /// loaded cold rows are not needlessly re-upserted on the first post-load
    /// persist.
    pub persisted_main_seq: u64,
    /// `true` while a restored tab's `acp_thread_blob` is still being
    /// deserialised on a background task. Set by the lazy-hydration path
    /// ([`SolutionAgentStore::restore_open_tabs_lazy`]) when a placeholder
    /// session entity is materialised with empty `entries`; cleared
    /// once the blob lands and `entries` is populated. The session
    /// view renders a loading spinner (instead of "no messages yet") while
    /// this is `true` and there is no live thread, so a not-yet-hydrated
    /// background tab reads as "loading", not "empty". Always `false` for
    /// fresh/live sessions and for tabs hydrated synchronously.
    pub hydrating: bool,
    /// Wall-clock duration of the most recently completed turn (set on
    /// `Running → Idle`). The status row reads this to render
    /// "Done in 2m15s" instead of a bare "Idle" so the user has an
    /// explicit signal that the agent finished — the desktop-notification
    /// path only fires when the panel is unfocused, leaving an
    /// in-foreground user with only "Thinking…" disappearing as the cue.
    /// Cleared the moment a new turn starts (state flips back to Running).
    /// Not persisted across restarts — purely a foreground-UX hint.
    pub last_turn_duration: Option<std::time::Duration>,
    /// Last-known total token count for the conversation, used by the
    /// status-row meter to keep showing "X / Y · Z%" on a cold tab
    /// (no live `AcpThread` → no `TokenUsage` to read). Populated on
    /// `restore_open_tabs` from the persisted metadata, refreshed
    /// whenever the live thread emits a `TokenUsageUpdated` event so
    /// the cached value stays in sync until the next cold restore.
    /// `None` for fresh sessions whose first turn hasn't shipped yet.
    pub cached_total_tokens: Option<u64>,
    /// Last-known max context window for the conversation. Sibling to
    /// `cached_total_tokens` — populated on every live `TokenUsageUpdated`
    /// event so MCP consumers (the phone client's context-fill meter)
    /// can read a model-specific limit even when the live `AcpThread`
    /// hasn't shipped a token usage yet. NOT persisted to disk: the
    /// agent re-emits `TokenUsageUpdated` (with `max_tokens`) on the
    /// first turn of any cold-resumed session, so the cache rebuilds
    /// itself naturally. `None` when no live event has been observed
    /// since the session entity was hydrated.
    pub cached_max_tokens: Option<u64>,
    /// Models advertised by claude for this session, cached so the status-row
    /// dropdown works on a cold tab (no live process to ask). Captured from
    /// the live `initialize` response and persisted; reloaded on cold restore.
    pub cached_models: Vec<claude_native::ModelInfo>,
    /// The user's chosen model (SDK `value`). Persisted. Applied via `--model`
    /// at the next spawn; for a live session also pushed via `set_model`.
    /// `None` → claude's default.
    pub desired_model: Option<String>,
    /// The user's chosen effort level (Claude Code's effort flag value, e.g.
    /// `"high"`). Persisted. Seeded into the native respawn map so a cold
    /// session wakes onto it; for a live session also pushed via
    /// `apply_flag_settings`. `None` → claude's default.
    pub desired_effort: Option<String>,
    /// F: parent session reference for sub-agent indication. `None` for
    /// top-level sessions. Set at creation time via
    /// `solution_agent.create_session({parent_session_id})` and
    /// persisted in `solution_sessions.parent_session_id` so the
    /// parent / child link survives editor restarts. The session view's
    /// sub-agents strip uses this to render a child under its parent
    /// (and to navigate up from a child to its parent).
    pub parent_session_id: Option<SolutionSessionId>,
    /// Safety-net timer armed by `cancel_turn` whenever the session
    /// flips to `Stopping`. If the natural `AcpThreadEvent::Stopped`
    /// (or `Error`) chain fails to fire within
    /// [`crate::store::queue::STOPPING_SAFETY_NET`], this task force-
    /// transitions the session back to `Idle` and logs a warning.
    /// Covers the `claude_native::connection::cancel` race where the
    /// pump consumed `prompt_tx` between the queue's authoritative
    /// flip-to-Stopping and the cancel forward, leaving nothing
    /// downstream to ever emit `Stopped`.
    ///
    /// Dropped (and therefore cancelled) by every code path that
    /// transitions the session out of Stopping naturally
    /// (`Stopped`/`Error`/`close_session`/`restart_agent`/
    /// `reset_context`) — leaving a stale task would let a delayed
    /// safety-net fire onto a now-Idle session and trigger a
    /// no-op (harmless) but spammy warn-log.
    pub stopping_safety_net: Option<Task<()>>,
    /// Durable friendly label for each teammate stream, keyed by the parent
    /// tool call's `acp::ToolCallId` (`toolu_xxx`, cast to `SharedString`).
    /// Captured once at registration for EVERY teammate — both an inline
    /// `Task` and an async `Agent` — and removed when that teammate's demux
    /// stream is closed (`close_stream`), NOT at the async spawn-ack (so a
    /// still-streaming async teammate keeps its label). `rebuild_streams`
    /// reads this to enrich each `StreamId::Teammate` stream's `Stream.label`,
    /// which is then the single source of truth for the desktop strip AND the
    /// mobile wire (`StreamDto.label`). Ephemeral — not persisted; a resumed
    /// session re-captures labels from the replayed tool-call stream.
    pub teammate_labels: HashMap<SharedString, SharedString>,
    /// Managed Agents (Claude Code's built-in async `Agent` tool dispatch)
    /// the parent has launched in this session. This map tracks Anthropic's
    /// standalone background processes whose lifecycle is bound to a separate
    /// JSONL file on disk.
    /// Persisted in `solution_session_background_agent`.
    pub background_agents:
        HashMap<background_agent::BackgroundAgentId, background_agent::BackgroundAgent>,
    /// Insertion order of `background_agents`. Used to render pills in
    /// spawn order (HashMap iteration is hash-seeded and unstable).
    pub background_agent_order: Vec<background_agent::BackgroundAgentId>,
    /// Background shells (`Bash(run_in_background=true)`) launched from
    /// this session. Keyed by [`background_shell::BackgroundShellId`].
    /// Output lives in an on-disk `.output` file tracked per-shell.
    /// Not cleared on context reset — shells outlive the conversation
    /// window and are reaped by a later task.
    pub background_shells:
        HashMap<background_shell::BackgroundShellId, background_shell::BackgroundShell>,
    /// Insertion order of `background_shells`. Used to render pills in
    /// spawn order (HashMap iteration is hash-seeded and unstable).
    pub background_shell_order: Vec<background_shell::BackgroundShellId>,
    /// Position in the desktop session-tab strip. `None` means the session is
    /// not currently in the strip (either never opened, or its tab was closed
    /// via `persist_tab_order(.., None)`). Populated on `restore_open_tabs`
    /// from `solution_sessions.tab_order` and maintained by every code path
    /// that mutates that DB column.
    ///
    /// The mobile `workspace.snapshot` filter uses `tab_order.is_some()` to
    /// decide whether a session is visible to the unified open-workspace screen.
    pub tab_order: Option<i64>,
    /// Monotonic change sequence for this session. Starts at 0 and incremented
    /// via `bump_change_seq()` (pre-increment: the first call returns 1). Used
    /// by mobile delta-sync to stamp each entry so `mod_seq == 0` stays the
    /// "unstamped" sentinel. Private write via helpers; pub read.
    pub change_seq: u64,
    /// Transcript generation counter. Incremented via `bump_epoch()` on
    /// wholesale replacements (`/clear`, compact, rehydrate). The mobile delta
    /// uses this to force a full reload when the transcript history changes
    /// structurally.
    pub epoch: u64,
    /// `change_seq` at this section's last change; ephemeral (rebuilt on restart
    /// per `init_change_seq_from_entries`). The mobile delta omits the section
    /// when `watermark <= since_seq`. Tracks `pending_messages` (the queue).
    pub queue_seq: u64,
    /// `change_seq` at this section's last change; ephemeral (rebuilt on restart
    /// per `init_change_seq_from_entries`). The mobile delta omits the section
    /// when `watermark <= since_seq`. Tracks the teammate stream set.
    pub subagents_seq: u64,
    /// `change_seq` at this section's last change; ephemeral (rebuilt on restart
    /// per `init_change_seq_from_entries`). The mobile delta omits the section
    /// when `watermark <= since_seq`. Tracks `state`.
    pub state_seq: u64,
    /// Set when the supervisor escalates a question to the human. Rendered as
    /// a 🛡 banner above the compose row; cleared when the user next sends a
    /// message. NOT part of the agent conversation history.
    pub supervisor_question: Option<SharedString>,
    /// True for the supervisor's hidden judge/auditor sessions; excluded from
    /// all user-visible session enumerations and from wire create/close
    /// notifications. The flag lives on the session entity (not a side map)
    /// so it remains readable at close time, when the in-flight judge/auditor
    /// maps have already dropped their handles.
    pub is_supervisor_ephemeral: bool,
    /// True for internal one-shot AI-helper sessions (commit-message
    /// generation, AI conflict/cherry-pick/rebase/explain — see
    /// `message_generator::run_ephemeral_task`). Unlike
    /// `is_supervisor_ephemeral` these are genuinely top-level, but they are
    /// equally invisible to the wire: create/close/state-changed notifications
    /// are suppressed so a connected mobile client never sees the create+close
    /// churn of a brief helper it was never told about. The flag lives on the
    /// entity (not a side map) so it stays readable at close time, mirroring
    /// `is_supervisor_ephemeral`.
    pub is_ephemeral: bool,
}

impl SolutionSession {
    /// Fresh, idle session with no live `AcpThread` attached. All
    /// optional state (`title`, `cwd`, `entries`, …) defaults to
    /// "empty"; callers should poke the relevant `pub` fields after
    /// construction. Use [`set_acp_thread`](Self::set_acp_thread) to
    /// attach the live thread once available.
    ///
    /// This is the only legal way to materialise a `SolutionSession`
    /// outside `model` — direct struct-literal construction is blocked
    /// by the private `acp_thread` field, which is exactly the point:
    /// every entry-point goes through a constructor where the thread
    /// starts unattached and reaches the entity only via `set_acp_thread`.
    pub fn new_idle(
        id: SolutionSessionId,
        solution_id: SolutionId,
        agent_id: AgentServerId,
        acp_session_id: acp::SessionId,
    ) -> Self {
        Self {
            id,
            solution_id,
            agent_id,
            acp_session_id,
            acp_thread: None,
            title: SharedString::default(),
            created_at: Utc::now(),
            last_activity_at: Utc::now(),
            state: SessionState::Idle,
            cwd: PathBuf::new(),
            context_count: 1,
            project: None,
            _acp_subscription: None,
            pending_messages: VecDeque::new(),
            flush_after_cancel: false,
            live_base: 0,
            entries: Vec::new(),
            streams: {
                let mut streams = indexmap::IndexMap::new();
                streams.insert(crate::stream::StreamId::Main, crate::stream::Stream::main());
                streams
            },
            closed_streams: HashMap::new(),
            hydration_orphan_streams: std::collections::HashSet::new(),
            hydration_watermark: 0,
            persisted_main_seq: 0,
            hydrating: false,
            last_turn_duration: None,
            cached_total_tokens: None,
            cached_max_tokens: None,
            cached_models: Vec::new(),
            desired_model: None,
            desired_effort: None,
            parent_session_id: None,
            stopping_safety_net: None,
            teammate_labels: HashMap::new(),
            background_agents: HashMap::new(),
            background_agent_order: Vec::new(),
            background_shells: HashMap::new(),
            background_shell_order: Vec::new(),
            tab_order: None,
            change_seq: 0,
            epoch: 0,
            queue_seq: 0,
            subagents_seq: 0,
            state_seq: 0,
            supervisor_question: None,
            is_supervisor_ephemeral: false,
            is_ephemeral: false,
        }
    }

    /// `true` when this session was restored from the DB but the agent
    /// subprocess hasn't been spawned yet — so rendering must come
    /// from `entries` (the cold-restored prefix) rather than `acp_thread.entries()`.
    pub fn is_cold(&self) -> bool {
        self.acp_thread.is_none()
    }

    /// Live thread reference. `None` for cold tabs.
    pub fn acp_thread(&self) -> Option<&Entity<AcpThread>> {
        self.acp_thread.as_ref()
    }

    /// Replace the live `AcpThread` on this session. Atomically emits
    /// `SolutionSessionEvent::ThreadReplaced` and `cx.notify()` so
    /// `SolutionSessionView` can re-attach its per-thread subscription
    /// (`_thread_subscription`) to the new thread.
    ///
    /// All callers MUST go through this method instead of poking
    /// `acp_thread` directly. Direct assignment inside a nested
    /// `session_entity.update(cx, |s, _| ...)` does not reliably
    /// trigger `cx.observe(&session)` callbacks (auto-notify can be
    /// dropped by the outer flush's deduplication), which strands
    /// `SessionView::_thread_subscription` on the dead thread and
    /// silently halts conversation-list rendering for that session.
    pub fn set_acp_thread(&mut self, thread: Option<Entity<AcpThread>>, cx: &mut Context<Self>) {
        self.live_base = if thread.is_some() {
            self.entries.len()
        } else {
            0
        };
        self.acp_thread = thread;
        cx.emit(SolutionSessionEvent::ThreadReplaced);
        cx.notify();
    }

    /// Store the given session entries and notify observers. Used by
    /// the store to maintain the mobile delta-sync payload (Phase 2+).
    pub fn set_entries(&mut self, entries: Vec<SessionEntry>, cx: &mut Context<Self>) {
        self.entries = entries;
        self.rebuild_streams();
        cx.notify();
    }

    /// Rebuild the `streams` mirror from the current `entries`. MUST be called
    /// after every mutation of `entries` (see `set_entries` + the store's
    /// clear/extend/truncate/slot-write sites) or the mirror drifts.
    pub fn rebuild_streams(&mut self) {
        let mut streams = crate::stream::demux(&self.entries);
        for id in self.closed_streams.keys() {
            streams.shift_remove(id); // Main is never inserted into closed_streams
        }
        // Hydration orphans: suppressed unless a live resume is streaming fresh
        // activity for them. An orphan whose entries all sit below the hydration
        // watermark finished before the restart → stays collapsed; one with an
        // entry at/after the watermark is being streamed anew → reopen it (it then
        // gets a live bg-agent registration whose real stop_reason can close it via
        // the permanent path, or it stays open if genuinely live). Keyed on a pure
        // index watermark, not the thread handle, so cold == "no entries past the
        // boundary yet" and the check is unit-testable.
        if !self.hydration_orphan_streams.is_empty() {
            let mut streamed_anew: std::collections::HashSet<crate::stream::StreamId> =
                std::collections::HashSet::new();
            for entry in self.entries.iter().skip(self.hydration_watermark) {
                if let Some(toolu) = &entry.subagent_id {
                    streamed_anew.insert(crate::stream::StreamId::Teammate(toolu.clone()));
                }
            }
            for id in &self.hydration_orphan_streams {
                if !streamed_anew.contains(id) {
                    streams.shift_remove(id);
                }
            }
        }
        // Phase 6d-A: fold each RUNNING background shell into the stream mirror
        // as a `StreamId::Shell` tab, inserted AFTER Main + teammates (IndexMap
        // insertion order) so shell pills sort last. Terminal (`Exited`/`Killed`)
        // shells are SKIPPED — a shell stream exists only while `Running`, which
        // is the product's auto-close (there is no dismissible terminal pill).
        // Derived HERE (not injected anywhere else) so the next entries-driven
        // rebuild can't wipe it; `background_shells` stays the source of truth.
        // The label + body come from the `cx`-free `BackgroundShell` helpers, so
        // this stays inside the `&mut self`/no-`cx` rebuild.
        let shell_now = chrono::Utc::now();
        for id in &self.background_shell_order {
            let Some(shell) = self.background_shells.get(id) else {
                continue;
            };
            if !matches!(
                shell.state,
                crate::background_shell::ShellRuntimeState::Running
            ) {
                continue;
            }
            let stream = crate::stream::Stream {
                id: crate::stream::StreamId::Shell(id.clone()),
                kind: crate::stream::StreamKind::Shell,
                label: shell.stream_label(),
                entries: vec![shell.stream_entry(shell_now)],
                seq: 0,
                state: crate::stream::StreamState::Live,
                source: crate::stream::StreamSource::FileTail(shell.output_path.clone()),
            };
            streams.insert(crate::stream::StreamId::Shell(id.clone()), stream);
        }
        // Fold each LIVE background AGENT into the mirror as its
        // `StreamId::Teammate(parent_tool_use_id)` tab. A fully detached async
        // `Agent` (its own `output_file`, no parent-thread interleaving)
        // produces NO `subagent_id`-tagged entries, so the `demux` above never
        // builds its stream — without this fold it has a `background_agents`
        // entry + a `latest` snapshot but no stream and therefore no pill
        // (phase 6d-B removed the dedicated Background pill on the assumption
        // that every async agent rides a demux teammate stream, which detached
        // ones do not). Mirrors the shell fold above: only agents still running
        // (`is_messageable`) are folded — a terminal one is dropped by
        // `tick_background_agents` — and an existing demux stream (the agent DID
        // interleave) is never clobbered.
        let agent_now = chrono::Utc::now();
        for id in &self.background_agent_order {
            let Some(agent) = self.background_agents.get(id) else {
                continue;
            };
            if !agent.is_messageable() {
                continue;
            }
            let Some(parent_toolu) = agent.parent_tool_use_id.clone() else {
                continue;
            };
            let key = crate::stream::StreamId::Teammate(parent_toolu.clone());
            // Respect an existing demux stream (don't clobber real entries) AND
            // the age-out: `reconcile_finished_teammate_streams` closes a stale /
            // done agent's stream via `closed_streams` (removed at the top of
            // this fn), and the agent lingers in `background_agents` for its
            // dead-linger window — without this guard the fold would re-add the
            // just-closed pill every rebuild and the age-out could never win.
            if streams.contains_key(&key) || self.closed_streams.contains_key(&key) {
                continue;
            }
            let stream = crate::stream::Stream {
                id: key.clone(),
                kind: crate::stream::StreamKind::Teammate,
                // Placeholder — the teammate-label enrichment pass below
                // overwrites this with `teammate_labels[toolu]` (or the raw
                // toolu when none was captured), the single label source.
                label: parent_toolu,
                entries: vec![agent.stream_entry(agent_now)],
                seq: 0,
                state: crate::stream::StreamState::Live,
                source: crate::stream::StreamSource::FileTail(agent.jsonl_path.clone()),
            };
            streams.insert(key, stream);
        }
        // Enrich each teammate stream's `label` from `teammate_labels` (the
        // durable friendly label captured at registration). After this,
        // `Stream.label` is the single source of truth for a teammate's display
        // label on BOTH the desktop strip and the mobile wire (`StreamDto.label`).
        // Fallback: keep the raw `toolu` when no label was captured. Main/Shell
        // labels are set at their own construction and are untouched here.
        for (id, stream) in streams.iter_mut() {
            if let crate::stream::StreamId::Teammate(toolu) = id {
                stream.label = self
                    .teammate_labels
                    .get(toolu)
                    .cloned()
                    .unwrap_or_else(|| toolu.clone());
            }
        }
        // Per-stream `seq` = the stream's high-water mark on the `change_seq`
        // axis (max `mod_seq` of its entries). `push_coalesced` keeps this
        // coalesce-aware, so it advances on append AND on a coalesce-merge the
        // frozen first-fragment mod_seq would otherwise hide (decision #5). This
        // single axis is BOTH the descriptor watermark and the per-entry delta
        // cursor for the phase-4b wire. Monotonic per stream except across a
        // rewind/truncate, where the wire's `total_count` shrink drives the
        // client's tail-truncate instead. The shell streams inserted above pick
        // up their `seq` from the derived entry's mtime-based `mod_seq` here too.
        for stream in streams.values_mut() {
            stream.seq = stream.entries.iter().map(|e| e.mod_seq).max().unwrap_or(0);
        }
        self.streams = streams;
    }

    /// Auto-close a secondary stream on a genuine `Done` signal: record the id
    /// in the `closed_streams` overlay and rebuild so it drops out of the mirror
    /// even though its tagged `entries` are still present. `StreamId::Main` is
    /// never closable (a no-op) — the parent stream always stays live.
    pub fn close_stream(&mut self, id: crate::stream::StreamId, reason: SharedString) {
        if matches!(id, crate::stream::StreamId::Main) {
            return;
        }
        // A permanent Done-close outranks the reopenable hydration-orphan overlay:
        // drop any orphan record for this id so post-watermark activity can't
        // resurrect a stream the completion path (Task terminal / async-Agent
        // stop_reason) has authoritatively finished.
        self.hydration_orphan_streams.remove(&id);
        // Label cleanup is tied to stream close: a teammate's durable label
        // lives only as long as its stream. Both the inline-Task terminal path
        // and the async-Agent real-stop_reason path funnel through here, so this
        // is the single point that reclaims a teammate label. Shell/Main ids have
        // no `teammate_labels` entry → no-op.
        if let crate::stream::StreamId::Teammate(toolu) = &id {
            self.teammate_labels.remove(toolu);
        }
        self.closed_streams.insert(id, reason);
        self.rebuild_streams();
    }

    /// Drop the whole close overlay so previously-closed streams reappear on the
    /// next rebuild. Used by the context reset/clear paths after `entries.clear()`.
    /// Also drops the hydration-orphan overlay: a context reset wipes both
    /// suppression categories.
    pub fn clear_closed_streams(&mut self) {
        self.closed_streams.clear();
        self.hydration_orphan_streams.clear();
        self.hydration_watermark = 0;
        // A context reset / clear wipes the transcript, so the next persist must
        // treat the (now-empty or fresh) Main stream as un-persisted and re-flush
        // from seq 0 rather than skipping rows below a stale watermark.
        self.persisted_main_seq = 0;
    }

    /// Cold-load hydration: a restored session's tagged rows belong to teammates
    /// that finished before the restart, so collapse to a Main-only view by
    /// recording every non-Main stream as a hydration orphan. Unlike a permanent
    /// `close_stream` Done-close, an orphan REOPENS if a live resume streams new
    /// activity for it (an entry at index >= the recorded watermark) — see
    /// `rebuild_streams`. Call AFTER assigning `entries` on a cold-restore path.
    pub fn hydrate_streams_main_only(&mut self) {
        // Derive orphans from a direct demux of the freshly-assigned `entries`,
        // NOT from `self.streams`: all 4 cold-load sites assign `entries` and
        // call this with no `rebuild_streams()` between, so `self.streams` is
        // still the stale pre-load (Main-only) mirror here — reading its keys
        // would record zero orphans and let the trailing `rebuild_streams()`
        // resurrect finished teammates as zombie tabs (decision #16).
        let orphans: Vec<crate::stream::StreamId> = crate::stream::demux(&self.entries)
            .keys()
            .filter(|id| !matches!(id, crate::stream::StreamId::Main))
            .cloned()
            .collect();
        for id in orphans {
            self.hydration_orphan_streams.insert(id);
        }
        self.hydration_watermark = self.entries.len();
        self.rebuild_streams();
        // Seed the persist watermark. All 4 cold-load sites call this, so it is
        // the single init point for `persisted_main_seq` on a restored session.
        //
        // Decide whether the on-disk rows already match the Main-LOCAL layout
        // `persist_main_stream` writes. When the flat mirror is LONGER than the
        // Main stream, the persisted rows are the LEGACY pre-6b layout: extra
        // teammate-tagged rows (and/or un-coalesced fragments) sit at global flat
        // indices that do NOT line up with Main-local indices. A naive skip (seed
        // = Main.seq) would then let the first incremental persist overwrite a
        // Main row's slot with a different entry and strand the stale row forever
        // (phase-6b keystone bug). Seed the watermark to 0 in that case so the
        // first persist re-writes the ENTIRE Main stream at Main-local indices and
        // `delete_entries_from(Main.len)` trims the leftovers — a one-time realign
        // that self-heals the legacy layout. When the counts match, the rows are
        // already Main-local (a clean 6b session, whose persisted rows ARE the
        // coalesced Main stream), so seed to Main.seq and skip the redundant
        // re-upsert. Keyed on the count (not `hydration_orphan_streams`, which is
        // populated from the pre-rebuild `streams` snapshot and so may be empty on
        // a direct-`entries`-assign cold-load) so it is independent of that path.
        let main_len = self
            .streams
            .get(&crate::stream::StreamId::Main)
            .map(|s| s.entries.len())
            .unwrap_or(0);
        self.persisted_main_seq = if self.entries.len() == main_len {
            self.streams
                .get(&crate::stream::StreamId::Main)
                .map(|s| s.seq)
                .unwrap_or(0)
        } else {
            0
        };
    }

    /// Allocate the next monotonic change sequence for this session. Pre-increment:
    /// the first call returns 1, so `mod_seq == 0` stays the "unstamped" sentinel.
    pub fn bump_change_seq(&mut self) -> u64 {
        self.change_seq += 1;
        self.change_seq
    }

    /// Seat `change_seq` at `anchor`, then seed the three section watermarks
    /// STRICTLY ABOVE it (each a fresh `bump_change_seq()`).
    ///
    /// The queue and subagents sections are not persisted, so after a desktop
    /// restart they are empty while a paired mobile client may still hold a stale
    /// non-empty cache pinned at `since_seq = anchor`. Bumping all three
    /// watermarks above the anchor forces the next delta to re-send exactly
    /// the three ephemeral sections (now correct/empty) while entries
    /// (`mod_seq <= anchor`) are NOT re-sent — the sections self-heal without a
    /// full transcript reload. The caller picks the anchor: the persisted
    /// `change_seq` (restart-monotonic cursor) when available, else
    /// `max(mod_seq)` for legacy rows (see [`Self::restore_change_seq`]).
    ///
    /// The bump count (3, one per section watermark) is part of the
    /// restart-determinism contract: a cold restore re-derives `change_seq =
    /// anchor + 3` purely from the persisted anchor, so a cursor issued from a
    /// prior boot's seed is always reproduced. Changing the count would make a
    /// restart derive a different `change_seq` that could fall below an
    /// already-issued cursor — keep it equal to the number of section watermarks.
    pub fn seed_change_seq(&mut self, anchor: u64) {
        self.change_seq = anchor;
        self.queue_seq = self.bump_change_seq();
        self.subagents_seq = self.bump_change_seq();
        self.state_seq = self.bump_change_seq();
    }

    /// Re-seat `change_seq` above the highest stamped entry after a cold restore,
    /// then seed the section watermarks above it. Legacy fallback for rows whose
    /// `change_seq` column predates Task 5.1b — see [`Self::seed_change_seq`].
    pub fn init_change_seq_from_entries(&mut self) {
        let max_mod_seq = self.entries.iter().map(|e| e.mod_seq).max().unwrap_or(0);
        self.seed_change_seq(max_mod_seq);
    }

    /// Cold-load anchor for `change_seq`: when the session row carried a persisted
    /// `change_seq` (Task 5.1b), restore from it so the cursor stays monotonic
    /// across restarts (otherwise new entries that fall below an already-issued
    /// client cursor silently drop out of the mobile delta). The persisted value
    /// is always >= `max(mod_seq)` (it was bumped to produce those mod_seqs), so
    /// restored entries never collide. A NULL column means a legacy row with no
    /// pre-restart delta client — fall back to `max(mod_seq)`.
    pub fn restore_change_seq(&mut self, persisted: Option<u64>) {
        match persisted {
            Some(anchor) => self.seed_change_seq(anchor),
            None => self.init_change_seq_from_entries(),
        }
    }

    /// Bump the transcript generation (cleared/replaced wholesale: /clear, compact,
    /// rehydrate). The mobile delta uses this to force a full reload.
    pub fn bump_epoch(&mut self) {
        self.epoch += 1;
    }
}

/// Events emitted by a `SolutionSession` entity. Currently only the
/// thread-swap signal — extend as new push channels become necessary.
#[derive(Debug, Clone, Copy)]
pub enum SolutionSessionEvent {
    /// The session's `acp_thread` was just replaced (compact, `/clear`,
    /// cold→live, restart_agent reuse path). Subscribers must drop any
    /// per-thread state and re-attach to the new `AcpThread`.
    ThreadReplaced,
}

impl EventEmitter<SolutionSessionEvent> for SolutionSession {}

/// Lightweight metadata row used for navigator listing without hydrating
/// the full conversation blob.
#[derive(Clone, Debug)]
pub struct SolutionSessionMetadata {
    pub id: SolutionSessionId,
    pub solution_id: SolutionId,
    pub agent_id: AgentServerId,
    pub acp_session_id: acp::SessionId,
    pub title: SharedString,
    pub created_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    /// First user prompt (truncated) so the History popover can disambiguate
    /// otherwise-identical "Session <uuid>" rows. `None` for sessions that
    /// haven't received a user message yet.
    pub preview: Option<SharedString>,
    /// Cumulative tokens (input + output) reported by the agent in
    /// session_update events. Surfaces in the History popover so the user
    /// can pick a heavy/light session.
    pub total_tokens: Option<u64>,
    /// Persisted copy of [`SolutionSession::context_count`] so a session
    /// re-hydrated from the DB on editor restart resumes its compact
    /// numbering instead of resetting to 1.
    pub context_count: SessionContextCount,
    /// Persisted copy of [`SolutionSession::cwd`]. Empty for rows
    /// written before the column existed; the resume path treats
    /// empty as "use `solution.root`".
    pub cwd: PathBuf,
    /// F: persisted copy of [`SolutionSession::parent_session_id`].
    /// `None` for top-level sessions and for legacy rows written
    /// before the column existed.
    pub parent_session_id: Option<SolutionSessionId>,
    /// Persisted copy of [`SolutionSession::desired_model`]. `None` for
    /// sessions where the user hasn't made a model selection yet.
    pub desired_model: Option<String>,
    /// Persisted copy of [`SolutionSession::desired_effort`]. `None` for
    /// sessions where the user hasn't made an effort selection yet.
    pub desired_effort: Option<String>,
    /// Persisted copy of [`SolutionSession::cached_models`]. Empty for
    /// sessions that haven't yet fetched the model list from the agent.
    pub cached_models: Vec<claude_native::ModelInfo>,
    /// Persisted copy of [`SolutionSession::tab_order`]. Carried through so the
    /// metadata INSERT can COALESCE it against any value a concurrent
    /// `update_tab_orders` already wrote, instead of clobbering it to NULL. A
    /// fresh create passes `None` (the strip position is written separately by
    /// `persist_tab_order` -> `update_tab_orders`); COALESCE(NULL, existing)
    /// preserves that write even when the INSERT lands AFTER the UPDATE (the
    /// lost-update race at create time that left idle never-touched sessions
    /// with `tab_order = NULL`, so `restore_open_tabs` never re-hydrated them).
    pub tab_order: Option<i64>,
    /// The member this session belongs to. `None` = the solution root (the
    /// "ROOT" label). Source of truth for the project label and console-tab
    /// scoping — replaces the old cwd-equality inference.
    pub member_id: Option<solutions::MemberId>,
}

#[cfg(test)]
mod tests;
