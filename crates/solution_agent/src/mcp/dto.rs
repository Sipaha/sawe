//! Shared read-only DTO types + conversion helpers for the `solution_agent`
//! MCP tools. Relocated verbatim from the former monolithic `mcp.rs`.
use agent_client_protocol::schema as acp;
use gpui::App;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::model::SolutionSession;
use gpui::SharedString;

/// Structured, serde-tagged wire representation of `SessionState`. Replaces the
/// former `format!("{:?}", state)` string — `Debug` output is not a stable
/// protocol. `Running` carries the wall-clock start anchor (the monotonic
/// `Instant` isn't serialisable); `Errored` carries the message.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionStateDto {
    Idle,
    Running {
        started_at_ms: i64,
    },
    /// Carries the wall-clock instant the user-facing Stopping state
    /// started so diagnostics tools (and a stuck-session triage script)
    /// can see "Stopping since N seconds ago" without guessing.
    /// `started_at_ms` is the same anchor scheme as `Running`: monotonic
    /// `Instant` rebased onto unix-millis via the current wall clock at
    /// serialization time.
    Stopping {
        started_at_ms: i64,
    },
    AwaitingInput,
    Errored {
        message: String,
    },
}

impl SessionStateDto {
    pub(crate) fn from_state(
        state: &crate::model::SessionState,
        running_started_at_ms: i64,
        stopping_started_at_ms: i64,
    ) -> Self {
        use crate::model::SessionState;
        match state {
            SessionState::Idle => SessionStateDto::Idle,
            SessionState::Running { .. } => SessionStateDto::Running {
                started_at_ms: running_started_at_ms,
            },
            SessionState::Stopping { .. } => SessionStateDto::Stopping {
                started_at_ms: stopping_started_at_ms,
            },
            SessionState::AwaitingInput => SessionStateDto::AwaitingInput,
            SessionState::Errored(msg) => SessionStateDto::Errored {
                message: msg.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionSummary {
    pub id: String,
    pub solution_id: i64,
    /// Sessions are solution-scoped since 2026-08-26 (FORK.md decision #89)
    /// and this is always `None`; `SessionSummary` is `Serialize`-only, and
    /// `skip_serializing_if` means the field is never actually written to
    /// the wire — the JSON is byte-identical to what deleting it would
    /// produce. It is kept purely because it derives `JsonSchema`, which
    /// still advertises the property: a client codegen'd from the published
    /// schema keeps its (always-absent) field without regenerating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_id: Option<i64>,
    pub agent_id: String,
    pub title: String,
    pub state: SessionStateDto,
    pub created_at: i64,
    pub last_activity_at: i64,
    /// F: cumulative tokens reported by the agent for this session.
    /// Sourced from the live `AcpThread::token_usage().used_tokens` when
    /// a thread is attached, falling back to
    /// `SolutionSession::cached_total_tokens` (populated from persistent
    /// metadata) for cold tabs. `None` when neither source has a value
    /// yet — e.g. a fresh session whose first turn hasn't shipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Model context window in tokens (`used_tokens / max_tokens` is the
    /// percentage the desktop's status-row meter shows). Live thread's
    /// `TokenUsage::max_tokens` when hot, falling back to the in-memory
    /// `SolutionSession::cached_max_tokens` mirrored from the last
    /// observed live event. `None` when no live event has arrived yet —
    /// clients should choose their own default rather than assume a
    /// specific window size (the desktop picks `DEFAULT_CONTEXT_WINDOW`,
    /// but a phone client might prefer a smaller assumption for an
    /// unknown model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// F: parent session reference for sub-agent indication. `None` for
    /// top-level sessions. Set at creation time via
    /// `solution_agent.create_session({parent_session_id})`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Underlying ACP session id — for the `claude-acp` agent this is
    /// the same UUID claude prints to `~/.claude/projects/<cwd>/<uuid>.jsonl`,
    /// which is what `claude --resume <uuid>` takes. Exposed for
    /// diagnostics: lets a triage script correlate a hung
    /// `SolutionSession` with its concrete subprocess (`pgrep -af
    /// 'claude .* --resume <uuid>'`) without having to guess from
    /// process start times.
    pub acp_session_id: String,
    /// Working directory the agent subprocess was launched with — either
    /// `solution.root` (default) or a member project's `local_path` when
    /// the chat was opened via the "+" popover's "New AI Chat" submenu.
    /// Drives `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl` bucketing
    /// so the field is the only authoritative way to locate the on-disk
    /// transcript without poking at the DB. `None` for legacy DB rows
    /// that predate the `session_cwd` column (empty `PathBuf` in
    /// `SolutionSession::cwd`); those sessions implicitly run at
    /// `solution.root`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Wire identity of a stream. Tagged object (locked encoding — identical in the
/// mobile client). All three variants ride the wire: `Main`, `Teammate` (inline
/// Task AND async `Agent` — the latter folded onto its demux stream in 6d-B),
/// and `Shell` (background shells, folded onto `streams` as `kind: shell` in
/// 6d-A/6d-B — no separate bg tools remain).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamIdDto {
    Main,
    Teammate { toolu: String },
    Shell { id: String },
}

impl StreamIdDto {
    pub fn from_model(id: &crate::stream::StreamId) -> Self {
        match id {
            crate::stream::StreamId::Main => StreamIdDto::Main,
            crate::stream::StreamId::Teammate(toolu) => StreamIdDto::Teammate {
                toolu: toolu.to_string(),
            },
            crate::stream::StreamId::Shell(bsid) => StreamIdDto::Shell {
                id: bsid.as_str().to_string(),
            },
        }
    }

    pub fn to_model(&self) -> crate::stream::StreamId {
        match self {
            StreamIdDto::Main => crate::stream::StreamId::Main,
            StreamIdDto::Teammate { toolu } => {
                crate::stream::StreamId::Teammate(SharedString::from(toolu.clone()))
            }
            StreamIdDto::Shell { id } => crate::stream::StreamId::Shell(
                crate::background_shell::BackgroundShellId::new(id.clone()),
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StreamKindDto {
    Main,
    Teammate,
    Shell,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamStateDto {
    Live,
    Done { reason: String },
}

/// Descriptor of one live stream — id/kind/label/state/seq/total_count for ALL
/// streams. Entries are NOT here; they ride the top-level `entries` /
/// `changed_entries` for the client-SELECTED stream only (decision #7: bounds the
/// payload, reuses the proven pagination; the descriptor list drives the tab strip).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StreamDto {
    pub id: StreamIdDto,
    pub kind: StreamKindDto,
    pub label: String,
    pub state: StreamStateDto,
    /// Per-stream delta watermark (max entry mod_seq). The client compares this to
    /// its per-stream cursor to know a stream advanced; passes it back as
    /// `since_seq` when it selects the stream.
    pub seq: u64,
    /// This stream's entry count (stream-local) — the client paginates the stream,
    /// and tail-truncates its per-stream list to this on a rewind.
    pub total_count: usize,
}

impl StreamDto {
    pub fn from_stream(stream: &crate::stream::Stream) -> StreamDto {
        let kind = match stream.kind {
            crate::stream::StreamKind::Main => StreamKindDto::Main,
            crate::stream::StreamKind::Teammate => StreamKindDto::Teammate,
            crate::stream::StreamKind::Shell => StreamKindDto::Shell,
        };
        let state = match &stream.state {
            crate::stream::StreamState::Live => StreamStateDto::Live,
            crate::stream::StreamState::Done { reason } => StreamStateDto::Done {
                reason: reason.to_string(),
            },
        };
        StreamDto {
            id: StreamIdDto::from_model(&stream.id),
            kind,
            label: stream.label.to_string(),
            state,
            seq: stream.seq,
            total_count: stream.entries.len(),
        }
    }
}

/// The descriptor list served by BOTH `get_session` and `get_session_changes`
/// (decision #7): one [`StreamDto`] per live stream in `session.streams`
/// (IndexMap insertion order = Main first, teammates in first-seen order). The
/// client diffs this against its held set to derive stream add/remove and drives
/// its tab strip from it.
pub(crate) fn build_streams_vec(session: &SolutionSession) -> Vec<StreamDto> {
    // 6d-B: shells now ride the wire (v4) as `kind: shell` streams alongside
    // Main + teammates — no filter.
    session
        .streams
        .values()
        .map(StreamDto::from_stream)
        .collect()
}

pub fn session_summary(session: &SolutionSession, cx: &App) -> SessionSummary {
    // Prefer the live thread's `TokenUsage.used_tokens` so an active
    // session reports the current count (R-5/R-6 token tracking already
    // writes through to `cached_total_tokens` on every
    // `TokenUsageUpdated` event, but live > cached when the two
    // disagree). Cold tabs fall back to the persisted cache.
    let live_usage = session
        .acp_thread()
        .and_then(|thread| thread.read(cx).token_usage().cloned());
    let total_tokens = live_usage
        .as_ref()
        .map(|usage| usage.used_tokens)
        .or(session.cached_total_tokens);
    // `max_tokens == 0` is the "agent didn't fill it in yet" sentinel
    // claude-acp ships under beta-gated paths. Treat that as None so
    // clients can apply their own default instead of dividing by zero.
    let max_tokens = live_usage
        .as_ref()
        .map(|usage| usage.max_tokens)
        .filter(|m| *m > 0)
        .or(session.cached_max_tokens);
    // Wall-clock anchors for Running / Stopping live counters (monotonic
    // Instant → serialisable ms). Each rebases `Instant` onto unix-millis
    // via the current wall clock at serialization time; only the variant
    // matching the current state ends up in the DTO.
    let instant_to_ms = |started_at: std::time::Instant| -> i64 {
        let wall = chrono::Utc::now()
            - chrono::Duration::from_std(started_at.elapsed()).unwrap_or_default();
        wall.timestamp_millis()
    };
    let running_started_at_ms = match &session.state {
        crate::model::SessionState::Running { started_at, .. } => instant_to_ms(*started_at),
        _ => 0,
    };
    let stopping_started_at_ms = match &session.state {
        crate::model::SessionState::Stopping { started_at } => instant_to_ms(*started_at),
        _ => 0,
    };
    SessionSummary {
        id: session.id.to_string(),
        solution_id: session.solution_id.0,
        // Always None (never emitted — see the doc comment on the field
        // above): sessions are solution-scoped since 2026-08-26 (FORK.md #89).
        member_id: None,
        agent_id: session.agent_id.to_string(),
        title: session.title.to_string(),
        state: SessionStateDto::from_state(
            &session.state,
            running_started_at_ms,
            stopping_started_at_ms,
        ),
        created_at: session.created_at.timestamp_millis(),
        last_activity_at: session.last_activity_at.timestamp_millis(),
        total_tokens,
        max_tokens,
        parent_session_id: session.parent_session_id.map(|id| id.to_string()),
        acp_session_id: session.acp_session_id.0.to_string(),
        cwd: (!session.cwd.as_os_str().is_empty())
            .then(|| session.cwd.to_string_lossy().into_owned()),
    }
}

// =====================================================================
// solution_agent.get_session_children
// =====================================================================

/// Structured wire role for an `EntrySummary`. Replaces the former
/// free-form `"user"|"assistant"|"tool_call"|"plan"` string — the
/// client matched on those exact strings, so a typed enum makes the
/// contract explicit and unbreakable.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EntryRoleDto {
    User,
    Assistant,
    ToolCall,
    Plan,
    ContextCompaction,
    /// Editor-originated annotation (watchdog / usage-limit / supervisor). The
    /// severity is carried in `EntrySummary::system_level`; the text is in
    /// `preview` / `markdown` like any other entry.
    System,
}

/// Severity of a `role == "system"` entry, so the client can render it
/// distinctly (info vs error vs observer/supervisor).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SystemLevelDto {
    Info,
    Error,
    Observer,
}

impl From<crate::session_entry::SystemEntryLevel> for SystemLevelDto {
    fn from(level: crate::session_entry::SystemEntryLevel) -> Self {
        use crate::session_entry::SystemEntryLevel;
        match level {
            SystemEntryLevel::Info => Self::Info,
            SystemEntryLevel::Error => Self::Error,
            SystemEntryLevel::Observer => Self::Observer,
        }
    }
}

/// Structured wire status for a tool call. Mirrors the
/// `conversation_render::tool_call_status_text` mapping (kept for the
/// desktop UI), but as a typed enum so the client need not string-match.
/// Note `WaitingForConfirmation` serializes to `"waiting_for_confirmation"`
/// (snake_case), not the desktop UI's `"waiting for confirmation"` label.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatusDto {
    Pending,
    WaitingForConfirmation,
    Running,
    Done,
    Failed,
    Rejected,
    Canceled,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EntrySummary {
    /// Role of this entry: user / assistant / tool_call / plan.
    pub role: EntryRoleDto,
    /// R-6e: absolute 0-based index in the session, stable across
    /// paginated calls. Always populated regardless of whether the
    /// caller requested a slice — lets the client reassemble a sparse
    /// map from multiple paginated responses.
    pub index: usize,
    /// Markdown rendering of the entry, truncated to roughly 200 scalar
    /// values.
    ///
    /// `skip_serializing_if` on EMPTINESS rather than an `Option` so the
    /// field's Rust type, its `JsonSchema` and every internal consumer stay
    /// unchanged.
    ///
    /// THAT IS ONLY SAFE BECAUSE A RENDERING CAN NEVER BE EMPTY. A shipped
    /// client declares `preview` non-null with no default, so an omitted key
    /// does not render as empty — it throws, and since the entry is a member
    /// of `changed_entries` it fails the decode of the WHOLE response, which
    /// freezes the transcript until the user force-quits. Every arm of
    /// `session_entry_to_markdown` prefixes a literal heading or appends a
    /// literal `\n\n`, so the truncation below is non-empty even for an entry
    /// with no content of its own. `every_entry_kind_renders_a_non_empty_preview`
    /// pins that for every variant and fails to COMPILE when a new one is
    /// added. Do not weaken either half without making this field an `Option`
    /// first.
    ///
    /// So the key is omitted in exactly one situation: a caller that asked for
    /// `omit_preview_when_markdown` on an entry this response also carries a
    /// body for. Never omitted for `role == "user"` entries —
    /// the mobile client keys csid-less optimistic bubbles off the user
    /// preview, and reconstructing that key from `markdown` would force it to
    /// re-implement `truncate_preview`'s Unicode-scalar truncation against
    /// UTF-16 string lengths.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preview: String,
    /// Full untruncated markdown rendering. Populated only when the
    /// caller passes `include_full_content: true`, or when the entry
    /// came back via `solution_agent.get_session_entry` (which always
    /// includes the full markdown for the single-entry case).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    /// Inline images present in this entry, raw base64 (no `data:` URI
    /// prefix). Populated only when the caller opts in via
    /// `include_images: true`. `None` means "the caller did not ask";
    /// an empty `Vec` means "the caller asked but the entry has no
    /// image content blocks".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<EntryImage>>,
    /// How many inlineable images this entry holds — i.e. the length
    /// `images` would have had if the caller had asked for it.
    ///
    /// Deliberately NOT `skip_serializing_if`: `0` has to go on the wire.
    /// The client treats an absent field as "this server is too old to
    /// know" and falls back to probing the entry with a
    /// `get_session_entry` round trip, so omitting zeros would send it
    /// probing exactly the text-only entries this field exists to let it
    /// skip — the optimisation would look wired up and do nothing. A
    /// 200-message session with two photos pays ~198 useless round trips
    /// per open without it.
    ///
    /// Also independent of the request's `include_images`: the mobile
    /// client polls with `include_images: false`, which is precisely when
    /// it needs the count.
    ///
    /// Counts only EXTRACTABLE images, matching `count_images_in_entry` —
    /// so it is 0 for assistant / tool-call / plan entries, whose image
    /// blocks are flattened to `spk-image://N` links with the raw blocks
    /// discarded. That matches the client, whose lazy backfill only
    /// targets user entries.
    pub image_count: usize,
    /// Present only for `role == "tool_call"` entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<ToolCallSummary>,
    /// Present only for `role == "plan"` entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanSummary>,
    /// Present only for `role == "system"` entries — the annotation severity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_level: Option<SystemLevelDto>,
    /// Originating client's locally-generated send id, plumbed verbatim
    /// from the user message's content-block `_meta.spk_client_send_id`
    /// (see `acp_thread::SPK_CLIENT_SEND_ID_META_KEY`). Present only for
    /// `role == "user"` entries that came from a client that stamped one
    /// (the mobile client today; desktop-originated sends leave it
    /// `None`). Lets the originating client dedupe its in-flight
    /// optimistic bubble against the server-echoed entry by an exact
    /// id-match instead of fragile content-equality on truncated previews.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_send_id: Option<i64>,
    /// Every distinct `spk_client_send_id` carried by this user
    /// entry, in source order. The single-id `client_send_id` field
    /// above is kept for back-compat (old mobile builds only look
    /// there); modern clients should prefer this list, since the
    /// server-side queue-merge path (`store::queue::send_message_blocks`'s
    /// `pending_messages` flush) rolls N originating bundles into one
    /// ACP message with N distinct stamps. Empty for non-user entries
    /// or for user entries from clients that don't stamp ids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_send_ids: Vec<i64>,
    /// Unix-millis creation time of this entry, captured server-side at first
    /// append. Absent (`None`, omitted from JSON) for entries that predate the
    /// feature — clients show no time rather than a fabricated one. Only
    /// positive values are real; the server maps the internal absent-sentinel
    /// to `None` here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_ms: Option<i64>,
    /// Parent `Task` / `Agent` tool_use id (`toolu_xxx`) when this entry was
    /// produced inside a claude subagent context, `None` for parent-level /
    /// user / plan entries. Sourced from `AgentThreadEntry::subagent_id()`
    /// which itself reads `_meta.claudeCode.parentToolUseId` off the
    /// underlying `acp::Meta`. Lets the client filter the conversation view
    /// by teammate tab — match against a `StreamDto`'s `teammate` id in `streams`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_id: Option<String>,
    /// True for a `role == "user"` entry that is actually a SUPERVISOR
    /// (observer) nudge, not a message the human typed. A nudge is delivered
    /// into the thread AS a user message (so the agent acts on it) but carries
    /// the `spk_observer_nudge` `_meta` marker (see
    /// `acp_thread::is_observer_nudge_blocks`). Two consumers rely on this:
    /// (1) the mobile / desktop clients render it as an Observer plaque instead
    /// of a plain user bubble; (2) the Supervisor judge must NOT anchor on its
    /// own past nudges as if they were fresh user goals (see
    /// `apply_user_anchored_filter`). Always `false` for non-user entries.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub observer_nudge: bool,
    /// True for a `role == "user"` entry that is actually an EDITOR
    /// reconnect-recovery prompt ("your process hung, the editor restarted it,
    /// continue"), injected by the stuck-session watchdog and carrying the
    /// `spk_editor_recovery` `_meta` marker (see
    /// `acp_thread::is_editor_recovery_blocks`). Kept SEPARATE from
    /// `observer_nudge` so clients don't mislabel an editor watchdog message as
    /// the AI supervisor's voice. The Supervisor judge's user-anchored filter
    /// excludes it too (it must not distill "your process hung" into
    /// `user_intent.md`). Always `false` for non-user entries.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub editor_recovery: bool,
    /// Total UTF-8 BYTE length of this entry's full body — the length of
    /// `markdown`, or of the body the client will hold once it has appended
    /// `markdown_tail`. Emitted whenever the server built a body at all, in
    /// BOTH the whole and the delta forms. Absent means "old server, or no
    /// body was built for this entry" — never "length zero".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown_len: Option<u64>,
    /// Present exactly when the server verified the caller's `known_entries`
    /// digest for this index and is therefore sending only the tail. Its value
    /// is the caller's own `markdown_len`, echoed back.
    ///
    /// MUST NOT be emitted when the request carried no `known_entries`: an old
    /// client that received it would decode `markdown = null` and render an
    /// empty bubble.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown_prefix_len: Option<u64>,
    /// The bytes to append after the caller's first `markdown_prefix_len`
    /// bytes of held body. Present iff `markdown_prefix_len` is present.
    ///
    /// `""` is legal, and it is neither "empty body" nor "absent": it means
    /// the prefix the caller offered already reached the end of the body this
    /// server holds.
    ///
    /// EMPTINESS IS NOT THE "UNCHANGED" SIGNAL — never branch on it. A client
    /// offers the digest over its held body MINUS the trailing whitespace run
    /// (the rendering wraps every body in a role heading and a trailing
    /// `\n\n`, so that run moves as the body grows), which means an unchanged
    /// entry comes back with that whitespace as its tail, not with `""`. A
    /// caller that offered the exact current body sees `""` instead. Both are
    /// ordinary tails and both splice identically. The honest test for "did
    /// this entry actually change" is `markdown_len` against the held length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown_tail: Option<String>,
}

/// One entry's body digest as the caller already holds it, so the server can
/// answer with a tail instead of the whole body (audit N-29).
///
/// The digest agreement with the mobile client is byte-for-byte load-bearing:
/// a divergence splices a tail onto a body the client does not actually hold,
/// which corrupts the transcript SILENTLY rather than failing to decode. See
/// [`sha256_16_hex`].
///
/// `deny_unknown_fields` makes this struct UNEXTENDABLE without a new feature
/// token, exactly like `GetSessionChangesParams` itself: a newer client that
/// adds a field here does not get it politely ignored by an older desktop —
/// serde rejects the element, which fails the whole `get_session_changes`
/// call, i.e. a failed poll rather than a degraded one. A future field must
/// therefore be gated on a NEW `wire_features` token that the client checks
/// before sending, never added on the assumption that old servers will skip
/// it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KnownEntryDto {
    /// STREAM-LOCAL index, the same space as `EntrySummary.index`.
    pub index: usize,
    /// UTF-8 BYTE length of the body the client holds for `index`. Never a
    /// UTF-16 unit count and never a `char` count.
    pub markdown_len: u64,
    /// Lowercase hex of the first 16 bytes of SHA-256 over those bytes.
    pub markdown_hash: String,
}

/// Hard cap on how many `known_entries` digests one poll may carry. The
/// response page itself is capped at `CHANGED_ENTRIES_PAGE`, so a caller
/// offering more digests than that is either confused or hostile; 32 leaves
/// generous headroom above the client's own cap of 8.
pub(crate) const KNOWN_ENTRIES_MAX: usize = 32;

/// Validate a caller's `known_entries` and index it by stream-local index.
///
/// Every failure is an `invalid_params:` tool error, never a panic — a
/// malformed digest is a client bug, and answering it with a whole body would
/// hide that bug rather than surface it.
pub(crate) fn index_known_entries(
    known: &[KnownEntryDto],
) -> anyhow::Result<HashMap<usize, &KnownEntryDto>> {
    anyhow::ensure!(
        known.len() <= KNOWN_ENTRIES_MAX,
        "invalid_params: known_entries capped at {KNOWN_ENTRIES_MAX}"
    );
    let mut by_index: HashMap<usize, &KnownEntryDto> = HashMap::with_capacity(known.len());
    for entry in known {
        anyhow::ensure!(
            entry.markdown_hash.len() == 32
                && entry
                    .markdown_hash
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "invalid_params: markdown_hash must be 32 lowercase hex chars"
        );
        anyhow::ensure!(
            by_index.insert(entry.index, entry).is_none(),
            "invalid_params: duplicate index in known_entries"
        );
    }
    Ok(by_index)
}

/// Lowercase hex of the FIRST 16 BYTES of SHA-256(`bytes`) — exactly 32
/// characters, `[0-9a-f]`, no prefix and no separators.
///
/// Byte-for-byte identical to the mobile client's `Digests.bodyDigest`; the
/// shared reference vectors in `mcp::tests` are what prove it. Truncation is
/// fixed at 16 bytes, not "half the digest".
pub(crate) fn sha256_16_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let full = Sha256::digest(bytes);
    let mut out = String::with_capacity(32);
    for byte in &full[..16] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EntryImage {
    /// 0-based stable index of the image within the session, in the
    /// order images appear when walking entries oldest-first. Lets
    /// renderers cross-reference the `spk-image://N` URL scheme used by
    /// the desktop side (see `conversation_render.rs`'s
    /// `clean_user_message_text`).
    pub index: usize,
    /// e.g. "image/png", "image/jpeg".
    pub mime_type: String,
    /// Raw base64, no `data:` URI prefix.
    pub data_base64: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ToolCallSummary {
    /// Opaque tool-call id, matching the id `authorize_tool_call` resolves
    /// against. Always populated. The client echoes this verbatim when
    /// answering an authorization prompt.
    pub tool_call_id: String,
    /// Human-readable tool name (e.g. "Read", "Edit", "Bash"). Derived
    /// from `tool_name` when set, falling back to the markdown source of
    /// the call's label entity.
    pub name: String,
    /// Tool-call status as a structured enum. Mirrors the desktop UI's
    /// `conversation_render::tool_call_status_text` mapping, but typed.
    pub status: ToolCallStatusDto,
    /// JSON-serialised `raw_input`, truncated to ~500 chars. Empty
    /// string when the agent didn't supply structured args.
    pub args_preview: String,
    /// JSON-serialised `raw_output`, truncated to ~500 chars. Empty
    /// string until the call completes (or fails) with structured
    /// output.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub result_preview: String,
    /// Unix epoch in milliseconds captured the first time this tool
    /// call's status transitioned into `InProgress`. Preserved across
    /// the transition to terminal statuses so clients can render
    /// "ran for Xs" on a completed call too. `None` for tool calls
    /// that have never entered `InProgress` (e.g. cold-rehydrated
    /// entries restored straight into a terminal status, or pending
    /// calls that haven't started yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_status_started_at_ms: Option<i64>,
    /// Authorization options when this tool call is awaiting confirmation
    /// (status == "waiting for confirmation"). Empty otherwise. The client
    /// renders one button per option and answers via
    /// `solution_agent.authorize_tool_call`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ToolCallAuthOption>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ToolCallAuthOption {
    /// Opaque option id — pass back verbatim to authorize_tool_call.
    pub option_id: String,
    /// Display label for the button.
    pub label: String,
    /// One of: "allow_once" | "allow_always" | "reject_once" | "reject_always".
    pub kind: String,
    /// True for allow-style options (render as primary), false for reject-style.
    pub is_allow: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlanSummary {
    /// One markdown source per plan item, in order. Tool-call plans
    /// surface as completed checklists in the desktop UI; rendering
    /// them remotely typically means a `- [x]` bullet list.
    pub items: Vec<String>,
}

/// One descriptor from `SolutionSession::pending_messages` exposed to
/// MCP consumers. Mirrors the wire shape that
/// `event_sources::build_queue_changed_payload` emits on the
/// `agent_session_queue_changed` notification.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct QueuedBundleSummary {
    /// Every distinct `spk_client_send_id` carried by the bundle's
    /// content blocks, in source order. Empty for desktop-typed
    /// bundles (no csid stamp) — clients should still render them as
    /// Queued bubbles, they just can't dedupe against local
    /// optimistic state in that case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub csids: Vec<i64>,
    /// Markdown preview of the bundle, queue-marker stripped, image
    /// placeholders rendered inline as `[image #N]`.
    pub preview: String,
    /// Number of image blocks in the bundle. Lets the client surface
    /// an `[image #N]` affordance without holding the image bytes.
    pub image_count: u32,
}

/// Reduce a transcript window to the entries a supervisor judge actually
/// needs: every `User` entry, the `lead` entries immediately before each
/// one, and the final entry (where the agent came to rest). Preserves order
/// and the absolute `EntrySummary.index` of every surviving entry. A no-op
/// when there are no user entries (nothing to anchor on → keep the window
/// as-is, so the judge still sees *something*).
/// Trail cap: how many of the agent's ASSISTANT text turns to keep after each
/// user anchor. This is the agent's ANSWER to that message — the thing the
/// judge must see to know a directive was delivered. Tool calls between them
/// are skipped (noise), so this counts text turns, not raw entries.
pub(crate) const USER_ANCHORED_TRAIL_ASSISTANT: usize = 5;

pub(crate) fn apply_user_anchored_filter(
    kept: &mut Vec<EntrySummary>,
    lead: usize,
    since_ms: Option<i64>,
) {
    if kept.is_empty() {
        return;
    }
    // A user entry anchors the slice only when it's the HUMAN's own message —
    // NEVER a supervisor nudge (those are user-role but carry the observer
    // marker; anchoring on them makes the judge re-read its own past nudges as
    // fresh user goals and loop) — AND newer than `since_ms` (the judge's
    // previous-wake cutoff). An entry with no timestamp is kept (can't prove
    // it's old). With no `since_ms` every human user entry anchors.
    let anchors = |e: &EntrySummary| {
        e.role == EntryRoleDto::User
            && !e.observer_nudge
            && !e.editor_recovery
            && since_ms.is_none_or(|s| e.created_ms.is_none_or(|c| c > s))
    };
    let has_user = kept.iter().any(anchors);
    if !has_user {
        // Incremental mode with nothing new since the cutoff: keep ONLY the
        // resting turn, so the judge sees where the agent stopped but not the
        // old user messages (already distilled into `user_intent.md`). In
        // non-incremental mode (`since_ms == None`) with no user entries at all,
        // preserve the original no-op (keep the window so the judge sees
        // something).
        if since_ms.is_some()
            && let Some(last) = kept.pop()
        {
            kept.clear();
            kept.push(last);
        }
        return;
    }
    let mut keep = vec![false; kept.len()];
    for pos in 0..kept.len() {
        if !anchors(&kept[pos]) {
            continue;
        }
        // Lead: the anchor plus the `lead` entries before it (the context that
        // prompted the user's message).
        let start = pos.saturating_sub(lead);
        for flag in keep.iter_mut().take(pos + 1).skip(start) {
            *flag = true;
        }
        // Trail: the agent's answer — up to `USER_ANCHORED_TRAIL_ASSISTANT`
        // assistant text turns after the anchor, skipping tool calls. Stop at
        // the next user-role entry (the next anchor OR a supervisor nudge) so
        // adjacent user messages never overlap and a nudge's own follow-up work
        // isn't attributed to this message.
        let mut assistant_kept = 0;
        for j in (pos + 1)..kept.len() {
            if kept[j].role == EntryRoleDto::User {
                break;
            }
            if kept[j].role == EntryRoleDto::Assistant {
                keep[j] = true;
                assistant_kept += 1;
                if assistant_kept >= USER_ANCHORED_TRAIL_ASSISTANT {
                    break;
                }
            }
        }
    }
    // Always retain the resting turn — the judge needs to see where the
    // agent stopped, which is rarely a user entry.
    if let Some(last) = keep.last_mut() {
        *last = true;
    }
    let mut iter = keep.into_iter();
    kept.retain(|_| iter.next().unwrap_or(false));
}

/// Default for `GetSessionChangesParams::include_images`. The delta is the
/// live render source (unlike `get_session`, which defaults `include_images`
/// false for cheap listing), so image payloads are inlined by default.
pub(crate) fn default_true() -> bool {
    true
}

/// Build the [QueuedBundleSummary] list off a session's
/// `pending_messages` queue. Pulled out so `get_session` and any
/// future cold-load surface can reuse the same shape that
/// `event_sources::build_queue_changed_payload` emits on the live
/// notification path. Empty queue → empty Vec.
pub(crate) fn build_pending_bundle_summaries(
    session: &crate::model::SolutionSession,
    _cx: &App,
) -> Vec<QueuedBundleSummary> {
    session
        .pending_messages
        .iter()
        .map(|bundle| {
            let csids = acp_thread::csids_from_blocks(&bundle.blocks);
            let preview = crate::conversation_render::pending_blocks_preview(&bundle.blocks, _cx);
            let image_count: u32 = bundle
                .blocks
                .iter()
                .filter(|b| matches!(b, acp::ContentBlock::Image(_)))
                .count() as u32;
            QueuedBundleSummary {
                csids,
                preview,
                image_count,
            }
        })
        .collect()
}

pub(crate) fn entry_role(kind: &crate::session_entry::SessionEntryKind) -> EntryRoleDto {
    use crate::session_entry::SessionEntryKind;
    match kind {
        SessionEntryKind::UserMessage { .. } => EntryRoleDto::User,
        SessionEntryKind::AssistantMessage { .. } => EntryRoleDto::Assistant,
        SessionEntryKind::ToolCall { .. } => EntryRoleDto::ToolCall,
        SessionEntryKind::Plan(_) => EntryRoleDto::Plan,
        SessionEntryKind::ContextCompaction { .. } => EntryRoleDto::ContextCompaction,
        SessionEntryKind::System { .. } => EntryRoleDto::System,
    }
}

/// Maps the unified `session_entry::ToolStatus` to the structured wire
/// enum. Parallels `conversation_render::tool_call_status_text` (which
/// stays for the desktop UI's human labels) but emits the typed wire
/// variant. Note this consumes `session_entry::ToolStatus`, not
/// `acp_thread::ToolCallStatus` — the variants line up one-to-one.
pub(crate) fn tool_status_dto(status: &crate::session_entry::ToolStatus) -> ToolCallStatusDto {
    use crate::session_entry::ToolStatus;
    match status {
        ToolStatus::Pending => ToolCallStatusDto::Pending,
        ToolStatus::WaitingForConfirmation => ToolCallStatusDto::WaitingForConfirmation,
        ToolStatus::InProgress => ToolCallStatusDto::Running,
        ToolStatus::Completed => ToolCallStatusDto::Done,
        ToolStatus::Failed => ToolCallStatusDto::Failed,
        ToolStatus::Rejected => ToolCallStatusDto::Rejected,
        ToolStatus::Canceled => ToolCallStatusDto::Canceled,
    }
}

/// Human label for a `session_entry::ToolStatus`, byte-identical to the
/// `acp_thread::ToolCallStatus` `Display` impl that the old
/// `ToolCall::to_markdown` printed after `Status: `. Kept in lock-step so
/// the reconstructed tool-call markdown matches what the live thread used
/// to emit on the wire.
pub(crate) fn tool_status_label(status: &crate::session_entry::ToolStatus) -> &'static str {
    use crate::session_entry::ToolStatus;
    match status {
        ToolStatus::Pending => "Pending",
        ToolStatus::WaitingForConfirmation => "Waiting for confirmation",
        ToolStatus::InProgress => "In Progress",
        ToolStatus::Completed => "Completed",
        ToolStatus::Failed => "Failed",
        ToolStatus::Rejected => "Rejected",
        ToolStatus::Canceled => "Canceled",
    }
}

/// Reconstruct the wire markdown for a `SessionEntry`, byte-for-byte
/// matching what the old `acp_thread::AgentThreadEntry::to_markdown`
/// produced for the same conversation content. The unified entry model
/// holds markdown source strings rather than live `Markdown` entities, so
/// each variant is recomposed from those sources using the exact same
/// templates the live path used (see `acp_thread.rs`). The one
/// unavoidable loss is the user "(checkpoint)" header suffix: SessionEntry
/// does not retain the checkpoint flag, so a checkpointed user message now
/// renders as a plain `## User`. That suffix never affected the structured
/// wire fields (role/preview/tool_call/images), only the cosmetic header.
pub(crate) fn session_entry_to_markdown(kind: &crate::session_entry::SessionEntryKind) -> String {
    use crate::session_entry::{AssistantChunk, SessionEntryKind};
    match kind {
        SessionEntryKind::UserMessage { content_md, .. } => {
            format!("## User\n\n{content_md}\n\n")
        }
        SessionEntryKind::AssistantMessage { chunks } => {
            let body = chunks
                .iter()
                .map(|chunk| match chunk {
                    AssistantChunk::Message(md) => md.clone(),
                    AssistantChunk::Thought(md) => format!("<thinking>\n{md}\n</thinking>"),
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            format!("## Assistant\n\n{body}\n\n")
        }
        SessionEntryKind::ToolCall {
            label_md,
            status,
            content_md,
            ..
        } => {
            let mut markdown = format!(
                "**Tool Call: {}**\nStatus: {}\n\n",
                label_md,
                tool_status_label(status)
            );
            for content in content_md {
                markdown.push_str(content);
                markdown.push_str("\n\n");
            }
            markdown
        }
        SessionEntryKind::Plan(items) => {
            let mut md = String::from("## Plan\n\n");
            for item in items {
                md.push_str(&format!("- [x] {}\n", item.content_md));
            }
            md
        }
        SessionEntryKind::ContextCompaction { .. } => "--- Context Compacted ---\n\n".to_string(),
        SessionEntryKind::System { text_md, .. } => format!("{text_md}\n\n"),
    }
}

pub(crate) fn truncate_preview(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, ch) in s.chars().enumerate() {
        if count >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

/// Hard cap on per-field previews other than the 200-char top-level
/// `preview`. Picked at ~500 chars to fit a chat-bubble truncation
/// without ballooning the wire payload for tool calls that dump huge
/// JSON args / results.
const FIELD_PREVIEW_MAX_CHARS: usize = 500;

/// Builds the per-entry `EntrySummary` for the MCP wire shape.
///
/// `index` is the entry's absolute position in the session — set on the
/// returned `EntrySummary` so the client can reassemble paginated
/// responses into a sparse map without computing offsets itself.
///
/// `include_full_content` controls whether the untruncated markdown is
/// populated on every variant. `include_images` controls whether image
/// content blocks get inlined as base64 (caller pays the wire cost).
///
/// `image_cursor` is the session-scoped 0-based image counter; the
/// caller threads it through `summarize_entry` calls in oldest-first
/// order so each `EntryImage.index` is stable across the session even
/// when an entry holds multiple images.
///
/// `body_delta` is the digest the caller claims to already hold for this
/// entry (audit N-29). `Some` ONLY on the `get_session_changes` path and
/// ONLY when that request carried a matching `known_entries` element —
/// every other caller passes `None`, which is what keeps
/// `markdown_prefix_len` / `markdown_tail` off the wire for a client that
/// would decode their presence as `markdown = null` and render an empty
/// bubble.
///
/// `omit_preview_when_markdown` drops `preview` for entries this response
/// also carries a body for (audit N-37). Honoured only for `role != "user"`
/// entries; see the field's doc on [`EntrySummary::preview`].
pub(crate) fn summarize_entry(
    entry: &crate::session_entry::SessionEntry,
    index: usize,
    include_full_content: bool,
    include_images: bool,
    image_cursor: &mut usize,
    live_auth_options: &HashMap<String, Vec<ToolCallAuthOption>>,
    body_delta: Option<&KnownEntryDto>,
    omit_preview_when_markdown: bool,
) -> EntrySummary {
    use crate::session_entry::SessionEntryKind;
    let kind = &entry.kind;
    let role = entry_role(kind);
    // `created_ms` is read directly off the unified entry (the store
    // stamps it on append); the absent-sentinel / 0 maps to `None`.
    let created_ms = (entry.created_ms > 0).then_some(entry.created_ms);
    let raw_markdown = session_entry_to_markdown(kind);
    // Snapshot the image cursor BEFORE the entry's images are extracted /
    // counted so we have a stable base for rewriting `` `Image` ``
    // placeholders in assistant markdown into `spk-image://N` links. The
    // global cursor advances by `count_images_in_entry` after this call
    // (either via `extract_images_for_entry` or the count_only branch
    // below) so the next entry's base lines up correctly.
    let image_index_base = *image_cursor;
    let markdown_source = if matches!(role, EntryRoleDto::Assistant) {
        // Rewrite agent-emitted image chunks into clickable `spk-image://N`
        // links so mobile (and any other ACP client) renders them through
        // the same path it already uses for user-attached images. The
        // base index is the cursor at this entry's start — see
        // `clean_assistant_message_text` in conversation_render. The
        // assistant markdown already carries the `` `Image` `` literal
        // (from `block.to_markdown` at conversion time) so the rewrite
        // still fires even though the raw image bytes were flattened away.
        crate::conversation_render::clean_assistant_message_text(&raw_markdown, image_index_base)
    } else {
        raw_markdown
    };
    let preview = truncate_preview(&markdown_source, 200);
    let markdown_len = markdown_source.len() as u64;
    // Exactly one row of the §2.4 invariant table per entry: whole body,
    // delta body, or no body. `is_char_boundary` is not optional — without it
    // `&bytes[..len]` could hash a partial code point and the tail slice
    // would panic. A non-boundary length is simply a mismatch, so the caller
    // gets the whole body.
    let (markdown, markdown_prefix_len, markdown_tail, markdown_len) = if !include_full_content {
        (None, None, None, None)
    } else if let Some(known) = body_delta {
        let held = known.markdown_len as usize;
        let matches = held <= markdown_source.len()
            && markdown_source.is_char_boundary(held)
            && sha256_16_hex(&markdown_source.as_bytes()[..held]) == known.markdown_hash;
        if matches {
            let tail = markdown_source[held..].to_string();
            (
                None,
                Some(known.markdown_len),
                Some(tail),
                Some(markdown_len),
            )
        } else {
            (Some(markdown_source), None, None, Some(markdown_len))
        }
    } else {
        (Some(markdown_source), None, None, Some(markdown_len))
    };
    let has_body = markdown.is_some() || markdown_tail.is_some();
    let preview = if omit_preview_when_markdown && has_body && !matches!(role, EntryRoleDto::User) {
        String::new()
    } else {
        preview
    };
    // Computed unconditionally — see the field's doc on `EntrySummary`.
    // It is the client's only way to tell a photo-bearing user entry from
    // a text-only one without asking, and it polls with
    // `include_images: false`.
    let image_count = count_images_in_entry(kind);
    let images = if include_images {
        Some(extract_images_for_entry(kind, image_cursor))
    } else {
        // Advance the cursor even when the caller didn't opt in, so
        // toggling `include_images` between calls preserves the same
        // stable indices.
        *image_cursor += image_count;
        None
    };
    let tool_call = if let SessionEntryKind::ToolCall { .. } = kind {
        Some(tool_call_summary(kind, live_auth_options))
    } else {
        None
    };
    let plan = if let SessionEntryKind::Plan(items) = kind {
        Some(PlanSummary {
            items: items.iter().map(|item| item.content_md.clone()).collect(),
        })
    } else {
        None
    };
    let client_send_ids: Vec<i64> = if let SessionEntryKind::UserMessage { chunks, .. } = kind {
        acp_thread::csids_from_blocks(chunks)
    } else {
        Vec::new()
    };
    let client_send_id = client_send_ids.first().copied();
    let subagent_id = entry.subagent_id.as_ref().map(|s| s.to_string());
    let system_level = if let SessionEntryKind::System { level, .. } = kind {
        Some((*level).into())
    } else {
        None
    };
    // A supervisor nudge is a user-role entry stamped with the
    // `spk_observer_nudge` `_meta` marker on its chunks. Surface it so clients
    // render the Observer plaque and the judge doesn't re-anchor on its own
    // past nudges.
    let observer_nudge = matches!(kind, SessionEntryKind::UserMessage { chunks, .. }
        if acp_thread::is_observer_nudge_blocks(chunks));
    // An editor reconnect-recovery prompt ("your process hung, continue") is a
    // SEPARATE non-human user-role entry — kept distinct from `observer_nudge` so
    // clients don't mislabel an editor watchdog message as the AI supervisor's
    // voice. The judge's user-anchored filter excludes BOTH (neither is a user
    // goal — it must not distill "your process hung" into `user_intent.md`).
    let editor_recovery = matches!(kind, SessionEntryKind::UserMessage { chunks, .. }
        if acp_thread::is_editor_recovery_blocks(chunks));

    EntrySummary {
        role,
        index,
        preview,
        markdown,
        images,
        image_count,
        tool_call,
        plan,
        system_level,
        client_send_id,
        client_send_ids,
        created_ms,
        subagent_id,
        observer_nudge,
        editor_recovery,
        markdown_len,
        markdown_prefix_len,
        markdown_tail,
    }
}

/// Counts image content blocks in an entry without allocating image
/// payloads. Used to keep `image_cursor` stable when the caller
/// opted out of `include_images`.
pub(crate) fn count_images_in_entry(kind: &crate::session_entry::SessionEntryKind) -> usize {
    use crate::session_entry::SessionEntryKind;
    match kind {
        // Only user-message images are countable: `UserMessage.chunks`
        // keeps the raw `acp::ContentBlock::Image` blocks. Assistant and
        // tool-call content are flattened to markdown in the unified
        // entry model (the raw Image blocks are NOT retained), so they
        // contribute zero EXTRACTABLE images. We deliberately count only
        // what we can extract so `image_cursor` / `EntryImage.index` stay
        // in lock-step with the inlined payloads — assistant images
        // survive purely as `spk-image://N` markdown links (see the
        // module-level note on `extract_images_for_entry`).
        SessionEntryKind::UserMessage { chunks, .. } => chunks
            .iter()
            .filter(|chunk| matches!(chunk, acp::ContentBlock::Image(_)))
            .count(),
        SessionEntryKind::AssistantMessage { .. }
        | SessionEntryKind::ToolCall { .. }
        | SessionEntryKind::Plan(_)
        | SessionEntryKind::ContextCompaction { .. }
        | SessionEntryKind::System { .. } => 0,
    }
}

/// Pulls inline image payloads out of an entry as wire-ready
/// `EntryImage` records.
///
/// IMAGE FIDELITY (graceful degradation, Phase 4 Task 5a): only
/// USER-message images are recoverable. `UserMessage.chunks` retains the
/// raw `acp::ContentBlock::Image` blocks, so the original base64 payload
/// round-trips byte-for-byte — identical to the pre-repoint user path.
///
/// ASSISTANT and TOOL-CALL images cannot be inlined: the unified
/// `SessionEntry` model flattens assistant chunks and tool content to
/// markdown strings and does NOT retain the raw `gpui::Image` bytes, so
/// there is nothing to base64-encode here. Those images survive only as
/// `spk-image://N` links in the markdown (assistant via
/// `clean_assistant_message_text`); the inline base64 payload is
/// unavailable. This is acceptable in practice — claude does not emit
/// image content, and tool-image content blocks are rare. To restore full
/// fidelity a future task would have to enrich
/// `SessionEntryKind::{AssistantMessage, ToolCall}` to keep the raw image
/// blocks (do NOT attempt to recover them from the model here — they are
/// genuinely gone). `image_cursor` only advances for extractable (user)
/// images so the `spk-image://N` indices stay stable across calls.
pub(crate) fn extract_images_for_entry(
    kind: &crate::session_entry::SessionEntryKind,
    image_cursor: &mut usize,
) -> Vec<EntryImage> {
    use crate::session_entry::SessionEntryKind;
    let mut out = Vec::new();

    if let SessionEntryKind::UserMessage { chunks, .. } = kind {
        for chunk in chunks {
            if let acp::ContentBlock::Image(image_content) = chunk {
                out.push(EntryImage {
                    index: *image_cursor,
                    mime_type: image_content.mime_type.clone(),
                    data_base64: image_content.data.clone(),
                });
                *image_cursor += 1;
            }
        }
    }
    out
}

/// Builds a `ToolCallSummary` for the wire from a unified
/// `SessionEntryKind::ToolCall`. Caller must pass the `ToolCall` variant;
/// other variants yield a default-ish summary (the callers above gate on
/// the variant first).
///
/// Authorization `options` are NOT stored on `SessionEntry` — a
/// `WaitingForConfirmation` tool call's permission choices live on the
/// LIVE `acp_thread` only (it's an in-flight, side-channel concern; a
/// cold/resumed session has no pending authorizations). So the caller
/// passes `live_auth_options`, a map of `tool_call_id ->
/// Vec<ToolCallAuthOption>` harvested from the live thread (empty for cold
/// sessions). This keeps the live auth-prompt wire shape intact while the
/// transcript itself is served from the unified entry model.
pub(crate) fn tool_call_summary(
    kind: &crate::session_entry::SessionEntryKind,
    live_auth_options: &HashMap<String, Vec<ToolCallAuthOption>>,
) -> ToolCallSummary {
    use crate::session_entry::SessionEntryKind;
    let SessionEntryKind::ToolCall {
        id,
        label_md,
        status,
        raw_input,
        raw_output,
        tool_name,
        status_started_at,
        ..
    } = kind
    else {
        // Unreachable in practice (callers gate on the ToolCall variant),
        // but produce a benign empty summary rather than panicking.
        return ToolCallSummary {
            tool_call_id: String::new(),
            name: String::new(),
            status: ToolCallStatusDto::Pending,
            args_preview: String::new(),
            result_preview: String::new(),
            tool_status_started_at_ms: None,
            options: Vec::new(),
        };
    };
    let name = tool_name
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| label_md.clone());
    let status_dto = tool_status_dto(status);
    let args_preview = raw_input
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .map(|s| truncate_preview(&s, FIELD_PREVIEW_MAX_CHARS))
        .unwrap_or_default();
    let result_preview = raw_output
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .map(|s| truncate_preview(&s, FIELD_PREVIEW_MAX_CHARS))
        .unwrap_or_default();
    let tool_status_started_at_ms = *status_started_at;
    // Surface authorization choices only while the call is blocked on the
    // user, sourced from the live thread (cold sessions have none).
    let options = live_auth_options.get(id).cloned().unwrap_or_default();
    ToolCallSummary {
        tool_call_id: id.clone(),
        name,
        status: status_dto,
        args_preview,
        result_preview,
        tool_status_started_at_ms,
        options,
    }
}

/// Harvest the live `WaitingForConfirmation` authorization options off a
/// session's live thread, keyed by tool-call id. Empty for cold sessions
/// (no live thread) or when no tool call is awaiting confirmation. Used to
/// re-attach the live auth-prompt options onto the SessionEntry-served
/// `ToolCallSummary` (the options are not stored on `SessionEntry`).
pub(crate) fn live_auth_options_for_session(
    session: &crate::model::SolutionSession,
    cx: &App,
) -> HashMap<String, Vec<ToolCallAuthOption>> {
    let mut map = HashMap::new();
    let Some(thread) = session.acp_thread() else {
        return map;
    };
    for entry in thread.read(cx).entries() {
        if let acp_thread::AgentThreadEntry::ToolCall(call) = entry {
            if let acp_thread::ToolCallStatus::WaitingForConfirmation { options, .. } = &call.status
            {
                let buttons = crate::conversation_render::permission_buttons(options)
                    .into_iter()
                    .map(|button| ToolCallAuthOption {
                        option_id: button.option_id.0.to_string(),
                        label: button.label.to_string(),
                        kind: permission_kind_str(button.kind).to_string(),
                        is_allow: button.is_allow(),
                    })
                    .collect();
                map.insert(call.id.0.to_string(), buttons);
            }
        }
    }
    map
}

/// Snake-case wire string for a `PermissionOptionKind`, matching the
/// kinds documented on `ToolCallAuthOption.kind`. The ACP enum already
/// serializes to exactly these strings (`#[serde(rename_all =
/// "snake_case")]`), but spelling them out here keeps the wire contract
/// stable even if the upstream serde representation ever drifts, and
/// avoids a serde round-trip per option.
pub(crate) fn permission_kind_str(kind: acp::PermissionOptionKind) -> &'static str {
    match kind {
        acp::PermissionOptionKind::AllowOnce => "allow_once",
        acp::PermissionOptionKind::AllowAlways => "allow_always",
        acp::PermissionOptionKind::RejectOnce => "reject_once",
        acp::PermissionOptionKind::RejectAlways => "reject_always",
        other => {
            log::warn!(
                "unknown PermissionOptionKind {other:?}; presenting as reject_once on the wire"
            );
            "reject_once"
        }
    }
}

// =====================================================================
// solution_agent.get_session_entry
// =====================================================================
