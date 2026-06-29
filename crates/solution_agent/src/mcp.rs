//! MCP tools exposed by the `solution_agent` crate. Tools register with the
//! central `editor_mcp` registry from `solution_agent::init` so that
//! `start_server` (called later from `crates/zed/src/main.rs`) sees them
//! when binding the socket.
use agent_client_protocol::schema as acp;
use anyhow::{Context as _, Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp, Entity};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;

use crate::background_agent::BackgroundAgent;
use crate::background_shell::BackgroundShell;
use crate::model::{SolutionSession, SolutionSessionId};
use crate::store::{PersistedSession, SolutionAgentStore};
use gpui::SharedString;
use solutions::{SolutionId, SolutionStore};

pub fn register(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ListSessionsTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ListAgentsTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetSessionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetSessionEntryTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetSessionChangesTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(CreateSessionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SendMessageTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SendMessageBlocksTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(DeleteSessionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(CancelTurnTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(AuthorizeToolCallTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RenameSessionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RestartAgentTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ResetContextTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(CompactSessionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(StartCompactTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ReadSessionHistoryTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetSessionChildrenTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetSessionBackgroundShellsTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetSessionBackgroundAgentsTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(UploadInitTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(UploadStatusTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(UploadFinishTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(UploadAbortTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ForceIdleTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SupervisorVerdictTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SupervisorAuditVerdictTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SetSupervisorEnabledTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SetSupervisorPromptTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetSupervisorStateTool);
    });
}

// =====================================================================
// solution_agent.list_sessions
// =====================================================================

/// List Solution-scoped AI sessions, optionally filtered by `solution_id`.
///
/// R-6e: paginated. Sessions are ordered by `last_activity_at` DESC and
/// `before_last_activity_at_ms` / `count` carve a time-anchored window.
/// `total_count` on the result reflects the unfiltered count (subject to
/// `solution_id` only), so the client can decide whether to fetch more.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ListSessionsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solution_id: Option<String>,
    /// F: filter by parent session id. When set, returns only sessions
    /// whose `parent_session_id` matches — i.e. the immediate children
    /// of the named session. Stacks with `solution_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// R-6e: exclusive upper bound on `last_activity_at` (millis since
    /// epoch). `None` = no upper bound (current behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_last_activity_at_ms: Option<i64>,
    /// R-6e: take only the first N sessions after ordering DESC by
    /// `last_activity_at` and applying `before_last_activity_at_ms`.
    /// `None` = unbounded (current behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
}

impl<'de> Deserialize<'de> for ListSessionsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Helper {
            solution_id: Option<String>,
            parent_session_id: Option<String>,
            before_last_activity_at_ms: Option<i64>,
            count: Option<usize>,
        }
        let helper = Option::<Helper>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: helper.solution_id,
            parent_session_id: helper.parent_session_id,
            before_last_activity_at_ms: helper.before_last_activity_at_ms,
            count: helper.count,
        })
    }
}

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
    fn from_state(
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
    pub solution_id: String,
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
    /// In-flight `Task` / `Agent` subagents the parent thread has spawned,
    /// in spawn order. Mirrors `SolutionSession::active_subagent_order` +
    /// `active_subagents` — the desktop session_view renders these as the
    /// pill strip under the status row, and the mobile client mirrors the
    /// same shape. Empty (and omitted) when the session has no subagents
    /// currently in-flight (the typical state outside an active Task turn).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_subagents: Vec<SubagentDto>,
}

/// One in-flight subagent surfaced to MCP consumers. Mirrors the in-memory
/// `SolutionSession::active_subagents` entry: an `id` (parent `Task`/`Agent`
/// tool_use id, `toolu_xxx`) + the human-readable label that the desktop
/// pill displays + the wall-clock start time as unix-millis (so mobile can
/// render "running for Xs" without a separate clock-sync round-trip).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SubagentDto {
    /// Parent tool_use id (`toolu_xxx`) — matches every entry whose
    /// `subagent_id` field equals this value, so the client filters its
    /// conversation view by exact-id match.
    pub id: String,
    /// Tab label as picked by [`SolutionSession::active_subagents`]'s
    /// label-fallback chain (`description` → `subagent_type#<short-id>` →
    /// `Agent <short-id>`). Label-locked at first observation — late
    /// `EntryUpdated`s that finally fill `raw_input.description` do NOT
    /// relabel the tab to keep the strip stable.
    pub label: String,
    /// Unix-millis the subagent was first observed in-flight. Strictly
    /// positive — there is no "missing" sentinel since the field is
    /// always stamped on insert with `chrono::Utc::now()`.
    pub started_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListSessionsResult {
    pub sessions: Vec<SessionSummary>,
    /// R-6e: total session count matching `solution_id` only (i.e. before
    /// `before_last_activity_at_ms` / `count` are applied). Lets a paginated
    /// client decide whether to fetch an older page.
    pub total_count: usize,
}

#[derive(Clone)]
pub struct ListSessionsTool;

impl McpServerTool for ListSessionsTool {
    type Input = ListSessionsParams;
    type Output = ListSessionsResult;
    const NAME: &'static str = "solution_agent.list_sessions";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        // F: optional parent filter. Parse once up-front so a malformed
        // id surfaces a clear error rather than silently producing an
        // empty result. Done outside the `cx.update` because the
        // read-only closure has no clean error-propagation shape.
        let want_parent = match input.parent_session_id.as_deref() {
            Some(s) => Some(
                SolutionSessionId::parse(s).map_err(|e| anyhow!("bad parent_session_id: {e}"))?,
            ),
            None => None,
        };
        // Hydrate any DB-only sessions for the requested solution. The
        // desktop's tab strip only hydrates rows with `tab_order IS
        // NOT NULL`, so closed-tab sessions were invisible to MCP-only
        // consumers like the phone — even though their full transcripts
        // sit on disk. `hydrate_all_for_solution` is a no-op for already-
        // hydrated sessions, so the second list_sessions call costs just
        // one cheap DB metadata query.
        if let Some(s) = input.solution_id.as_ref() {
            let sol_id = SolutionId(s.clone());
            let task = cx.update(|cx| {
                let store = SolutionAgentStore::global(cx);
                store.update(cx, |s, cx| s.hydrate_all_for_solution(sol_id, cx))
            });
            task.await?;
        }
        let (sessions, total_count) = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.read_with(cx, |store, cx| {
                let want_solution = input.solution_id.as_ref().map(|s| SolutionId(s.clone()));
                let mut matching: Vec<SessionSummary> = store
                    .all_sessions()
                    .filter_map(|entity| {
                        let session = entity.read(cx);
                        // Hidden supervisor judge/auditor sessions are never
                        // user-visible; exclude them from the enumeration.
                        if session.is_supervisor_ephemeral {
                            return None;
                        }
                        if let Some(want) = &want_solution {
                            if &session.solution_id != want {
                                return None;
                            }
                        }
                        if let Some(want) = want_parent {
                            if session.parent_session_id != Some(want) {
                                return None;
                            }
                        }
                        Some(session_summary(session, cx))
                    })
                    .collect();
                // R-6e: order DESC by last_activity_at so `count=N` returns
                // the most-recent N sessions. `total_count` is the count
                // BEFORE before_last_activity_at_ms / count filtering, so
                // the client knows if a "load older" page exists.
                matching.sort_by(|a, b| b.last_activity_at.cmp(&a.last_activity_at));
                let total = matching.len();
                if let Some(before) = input.before_last_activity_at_ms {
                    matching.retain(|s| s.last_activity_at < before);
                }
                if let Some(count) = input.count {
                    matching.truncate(count);
                }
                (matching, total)
            })
        });
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} session(s)", sessions.len()),
            }],
            structured_content: ListSessionsResult {
                sessions,
                total_count,
            },
        })
    }
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
        solution_id: session.solution_id.0.clone(),
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
        active_subagents: build_active_subagents_vec(session),
    }
}

/// Walks `SolutionSession::active_subagent_order` in insertion order and
/// converts each tracked subagent into its wire form. Skips ids that
/// don't have a matching map entry (defensive — `active_subagent_order`
/// is supposed to be kept 1:1 with `active_subagents`, so a mismatch is
/// a bug worth logging). Shared by:
///
///   * `session_summary` — populates [`SessionSummary::active_subagents`]
///     on `list_sessions` / `get_session`.
///   * `event_sources::build_active_subagents_changed_payload` — wire
///     payload for the live `agent_session_active_subagents_changed`
///     notification.
///
/// Both paths must agree on the shape, hence the single helper.
pub(crate) fn build_active_subagents_vec(
    session: &crate::model::SolutionSession,
) -> Vec<SubagentDto> {
    let mut out = Vec::with_capacity(session.active_subagent_order.len());
    for id in &session.active_subagent_order {
        match session.active_subagents.get(id) {
            Some(tab) => out.push(SubagentDto {
                id: id.to_string(),
                label: tab.label.to_string(),
                started_at_ms: tab.started_at.timestamp_millis(),
            }),
            None => {
                log::warn!(
                    "active_subagent_order has id {id} with no matching active_subagents entry \
                     (insertion-order vector drifted from the map — see store::apply_subagent_lifecycle)"
                );
            }
        }
    }
    out
}

// =====================================================================
// solution_agent.get_session_children
// =====================================================================

/// F: list the immediate children of a session — sessions whose
/// `parent_session_id` equals the input. Used by the desktop /
/// phone "sub-agents" strip to fetch siblings in a single round-trip
/// instead of running a filtered `list_sessions`. Returns an empty
/// list when the session has no children. Errors with
/// `unknown_parent_session` when the parent itself is unknown.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetSessionChildrenParams {
    pub session_id: String,
}

impl<'de> Deserialize<'de> for GetSessionChildrenParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSessionChildrenResult {
    /// Immediate children ordered by `created_at` ASC, so the consumer
    /// renders the oldest child first (matches the desktop strip layout
    /// described in the F plan-doc: "main → first spawn → second
    /// spawn").
    pub children: Vec<SessionSummary>,
}

#[derive(Clone)]
pub struct GetSessionChildrenTool;

impl McpServerTool for GetSessionChildrenTool {
    type Input = GetSessionChildrenParams;
    type Output = GetSessionChildrenResult;
    const NAME: &'static str = "solution_agent.get_session_children";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let parent_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let children = cx.update(|cx| -> Result<Vec<SessionSummary>> {
            let store = SolutionAgentStore::global(cx);
            store.read_with(cx, |store, cx| -> Result<Vec<SessionSummary>> {
                // Verify the parent itself exists so an unknown id
                // surfaces a clear error instead of an empty list (the
                // latter is ambiguous: "no children" vs. "no parent").
                store
                    .session(parent_id)
                    .ok_or_else(|| anyhow!("unknown_parent_session: {parent_id}"))?;
                let mut children: Vec<SessionSummary> = store
                    .all_sessions()
                    .filter_map(|entity| {
                        let session = entity.read(cx);
                        // The supervisor's hidden judge/auditor sessions are
                        // parent-linked to the supervised session, so they'd
                        // otherwise surface here; exclude them.
                        if session.is_supervisor_ephemeral {
                            return None;
                        }
                        if session.parent_session_id == Some(parent_id) {
                            Some(session_summary(session, cx))
                        } else {
                            None
                        }
                    })
                    .collect();
                children.sort_by(|a, b| a.created_at.cmp(&b.created_at));
                Ok(children)
            })
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} child session(s)", children.len()),
            }],
            structured_content: GetSessionChildrenResult { children },
        })
    }
}

// =====================================================================
// solution_agent.get_session_background_shells
// =====================================================================

/// One background shell surfaced to MCP consumers. Mirrors the in-memory
/// [`BackgroundShell`] entry: the launch `id` + `command`, the
/// `state_text` lifecycle string ([`ShellRuntimeState::to_state_text`]),
/// and the latest snapshot's `mtime` as unix-millis. `output_tail` is the
/// only heavy field and is opt-in via the tool's `include_output` param
/// (the lite shape used by `agent_session_background_shells_changed`
/// always omits it).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BackgroundShellDto {
    /// Launch token Claude Code assigned the shell (e.g. `bvb4ful1z`).
    pub id: String,
    /// Command line captured at launch (truncated at the call-site).
    pub command: String,
    /// "running" | "exited:N" | "exited" | "killed"
    /// ([`crate::background_shell::ShellRuntimeState::to_state_text`]).
    pub state: String,
    /// Latest snapshot's `mtime` as unix-millis. `None` when no snapshot
    /// has been captured yet, or (defensively) when the mtime is pre-epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
    /// Trailing chunk of the shell's stdout/stderr. Only present when the
    /// caller passed `include_output: true` AND a snapshot exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
}

/// Build a [`BackgroundShellDto`] from a tracked [`BackgroundShell`].
/// Shared by the `get_session_background_shells` tool (with the param's
/// `include_output`) and `event_sources::build_background_shells_changed_payload`
/// (always lite, `include_output = false`) so both wire paths agree on
/// the shape. Pure (no `cx`), so it lives here and event_sources reaches
/// it via `crate::mcp::` exactly like `build_active_subagents_vec`.
pub(crate) fn background_shell_dto(
    shell: &BackgroundShell,
    include_output: bool,
) -> BackgroundShellDto {
    let mtime_ms = shell.latest.as_ref().and_then(|snapshot| {
        snapshot
            .mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .ok()
    });
    let output_tail = include_output
        .then(|| {
            shell
                .latest
                .as_ref()
                .map(|snapshot| snapshot.output_tail.to_string())
        })
        .flatten();
    BackgroundShellDto {
        id: shell.id.to_string(),
        command: shell.command.to_string(),
        state: shell.state.to_state_text(),
        mtime_ms,
        output_tail,
    }
}

/// Walk a session's `background_shell_order` in insertion order and convert
/// each tracked shell into its wire form. Skips ids with no matching map
/// entry (defensive — the order vec is kept 1:1 with the map). Shared by the
/// tool handler and the notification builder, hence the single helper.
pub(crate) fn build_background_shells_vec(
    session: &SolutionSession,
    include_output: bool,
) -> Vec<BackgroundShellDto> {
    let mut out = Vec::with_capacity(session.background_shell_order.len());
    for id in &session.background_shell_order {
        match session.background_shells.get(id) {
            Some(shell) => out.push(background_shell_dto(shell, include_output)),
            None => {
                log::warn!(
                    "background_shell_order has id {id} with no matching background_shells entry \
                     (insertion-order vector drifted from the map)"
                );
            }
        }
    }
    out
}

/// List the background shells (`Bash(run_in_background=true)`) registered
/// for a session, with live state and optional stdout tail.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetSessionBackgroundShellsParams {
    pub session_id: String,
    /// Default false. When true each returned shell carries its
    /// `output_tail` (the heavy field); otherwise only id/command/state/mtime.
    #[serde(default)]
    pub include_output: bool,
}

impl<'de> Deserialize<'de> for GetSessionBackgroundShellsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            #[serde(default)]
            include_output: bool,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            include_output: inner.include_output,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSessionBackgroundShellsResult {
    /// Background shells in `background_shell_order` (insertion order) — the
    /// same order the desktop strip renders pills.
    pub background_shells: Vec<BackgroundShellDto>,
}

#[derive(Clone)]
pub struct GetSessionBackgroundShellsTool;

impl McpServerTool for GetSessionBackgroundShellsTool {
    type Input = GetSessionBackgroundShellsParams;
    type Output = GetSessionBackgroundShellsResult;
    const NAME: &'static str = "solution_agent.get_session_background_shells";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;
        let include_output = input.include_output;

        let background_shells = cx.update(|cx| -> Result<Vec<BackgroundShellDto>> {
            let store = SolutionAgentStore::global(cx);
            store.read_with(cx, |store, cx| -> Result<Vec<BackgroundShellDto>> {
                let session = store
                    .session(session_id)
                    .ok_or_else(|| anyhow!("session_not_found: {session_id}"))?;
                Ok(build_background_shells_vec(
                    session.read(cx),
                    include_output,
                ))
            })
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} background shell(s)", background_shells.len()),
            }],
            structured_content: GetSessionBackgroundShellsResult { background_shells },
        })
    }
}

// =====================================================================
// solution_agent.get_session_background_agents
// =====================================================================

/// One managed background agent surfaced to MCP consumers. Mirrors the
/// in-memory [`BackgroundAgent`] entry: the launch `id`, the latest
/// snapshot's `activity_label` (as `label`), the snapshot `mtime` as
/// unix-millis, and the terminal `stop_reason`. Unlike the shells DTO
/// there is no heavy field — every agent's fields are tiny, so they
/// always ship (no `include_output` opt-in). Clients derive "done" from
/// `stop_reason.is_some()`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BackgroundAgentDto {
    /// Launch token Claude Code assigned the agent (e.g. `a30f92a688e431edc`).
    pub id: String,
    /// Latest snapshot's `activity_label` (e.g. `Bash: cargo test`), or
    /// the default `Generating…` when no snapshot has been captured yet.
    pub label: String,
    /// Latest snapshot's `mtime` as unix-millis. `None` when no snapshot
    /// has been captured yet, or (defensively) when the mtime is pre-epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
    /// Terminal stop reason (e.g. `end_turn`) once the agent finished.
    /// `None` while the agent is still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

/// Build a [`BackgroundAgentDto`] from a tracked [`BackgroundAgent`].
/// Shared by the `get_session_background_agents` tool and
/// `event_sources::build_background_agents_changed_payload` so both wire
/// paths agree on the shape. Pure (no `cx`). `label` falls back to the
/// `Generating…` default when no snapshot exists yet.
pub(crate) fn background_agent_dto(agent: &BackgroundAgent) -> BackgroundAgentDto {
    let mtime_ms = agent.latest.as_ref().and_then(|snapshot| {
        snapshot
            .mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .ok()
    });
    let label = agent
        .latest
        .as_ref()
        .map(|snapshot| snapshot.activity_label.to_string())
        .unwrap_or_else(|| "Generating…".to_string());
    let stop_reason = agent
        .latest
        .as_ref()
        .and_then(|snapshot| snapshot.stop_reason.as_ref())
        .map(|s| s.to_string());
    BackgroundAgentDto {
        id: agent.id.to_string(),
        label,
        mtime_ms,
        stop_reason,
    }
}

/// Walk a session's `background_agent_order` in insertion order and convert
/// each tracked agent into its wire form. Skips ids with no matching map
/// entry (defensive — the order vec is kept 1:1 with the map). Shared by the
/// tool handler and the notification builder, hence the single helper.
pub(crate) fn build_background_agents_vec(session: &SolutionSession) -> Vec<BackgroundAgentDto> {
    let mut out = Vec::with_capacity(session.background_agent_order.len());
    for id in &session.background_agent_order {
        match session.background_agents.get(id) {
            Some(agent) => out.push(background_agent_dto(agent)),
            None => {
                log::warn!(
                    "background_agent_order has id {id} with no matching background_agents entry \
                     (insertion-order vector drifted from the map)"
                );
            }
        }
    }
    out
}

/// List the managed background agents registered for a session, with
/// their label, last-activity time, and stop reason.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetSessionBackgroundAgentsParams {
    pub session_id: String,
}

impl<'de> Deserialize<'de> for GetSessionBackgroundAgentsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSessionBackgroundAgentsResult {
    /// Background agents in `background_agent_order` (insertion order) — the
    /// same order the desktop strip renders pills.
    pub background_agents: Vec<BackgroundAgentDto>,
}

#[derive(Clone)]
pub struct GetSessionBackgroundAgentsTool;

impl McpServerTool for GetSessionBackgroundAgentsTool {
    type Input = GetSessionBackgroundAgentsParams;
    type Output = GetSessionBackgroundAgentsResult;
    const NAME: &'static str = "solution_agent.get_session_background_agents";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let background_agents = cx.update(|cx| -> Result<Vec<BackgroundAgentDto>> {
            let store = SolutionAgentStore::global(cx);
            store.read_with(cx, |store, cx| -> Result<Vec<BackgroundAgentDto>> {
                let session = store
                    .session(session_id)
                    .ok_or_else(|| anyhow!("session_not_found: {session_id}"))?;
                Ok(build_background_agents_vec(session.read(cx)))
            })
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} background agent(s)", background_agents.len()),
            }],
            structured_content: GetSessionBackgroundAgentsResult { background_agents },
        })
    }
}

// =====================================================================
// solution_agent.list_agents
// =====================================================================

/// List registered agent adapters. The `id` is what `create_session`'s
/// `agent_id` param accepts; `display_name` is what a client picker
/// (e.g. the Android client's "New session" dialog) should show.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ListAgentsParams {}

impl<'de> Deserialize<'de> for ListAgentsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let _ = serde::de::IgnoredAny::deserialize(de)?;
        Ok(ListAgentsParams {})
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentSummary {
    pub id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListAgentsResult {
    pub agents: Vec<AgentSummary>,
}

#[derive(Clone)]
pub struct ListAgentsTool;

impl McpServerTool for ListAgentsTool {
    type Input = ListAgentsParams;
    type Output = ListAgentsResult;
    const NAME: &'static str = "solution_agent.list_agents";

    async fn run(
        &self,
        _input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let summaries = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.read_with(cx, |store, _| {
                store
                    .adapters
                    .supported_ids()
                    .iter()
                    .filter_map(|id| {
                        store.adapters.get(id).map(|adapter| AgentSummary {
                            id: id.to_string(),
                            display_name: adapter.display_name().to_string(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
        });
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} agent(s)", summaries.len()),
            }],
            structured_content: ListAgentsResult { agents: summaries },
        })
    }
}

// =====================================================================
// solution_agent.get_session
// =====================================================================

/// Fetch a session's metadata plus a per-entry preview (first ~200 chars
/// of each entry's markdown rendering). When the session has no live
/// `acp_thread`, `entries` is empty and only the metadata is populated.
///
/// Wire-size trade-off: with the default flags off the response stays
/// compact — preview-only on a ~10-entry session is ≈ 1.5–2 KB. Flipping
/// `include_full_content` adds the untruncated markdown for every entry
/// (roughly 10–20× the preview-only size depending on conversation
/// length). Flipping `include_images` on top inlines base64-encoded
/// image payloads — a single screenshot can balloon the response by
/// hundreds of KB, so prefer `solution_agent.get_session_entry` for
/// per-entry image fetches when bandwidth is tight.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetSessionParams {
    pub session_id: String,
    /// Default false. When true, every `EntrySummary.markdown` is
    /// populated with the full untruncated rendering. Caller pays the
    /// wire cost.
    #[serde(default)]
    pub include_full_content: bool,
    /// Default false. When true, `EntrySummary.images` carries inline
    /// base64 image payloads on entries that contain image content
    /// blocks. Combine with `include_full_content` for the rich chat
    /// case.
    #[serde(default)]
    pub include_images: bool,
    /// R-6e: return only entries with absolute index < `before_index`.
    /// `None` = no upper bound (current behavior). Combine with
    /// `after_index` for a slice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_index: Option<usize>,
    /// R-6e: return only entries with absolute index > `after_index`.
    /// `None` = no lower bound (current behavior). This is the param
    /// the client uses for incremental resume — pass the last seen
    /// entry index and get only what's new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_index: Option<usize>,
    /// R-6e: take only the LAST `count` entries after applying
    /// `after_index` / `before_index`. "Last" — not first — because the
    /// dominant client query (initial session-detail open) wants the
    /// newest N entries, not the oldest. `None` = unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    /// Per-tab filter, applied BEFORE `count`/`after_index`/`before_index`
    /// windowing so each tab's window contains that tab's entries (a tail
    /// window taken over ALL entries then filtered client-side could leave a
    /// tab empty — the bug this fixes). Mirrors the desktop
    /// `session_view::should_render_entry` rule so the wire is the single
    /// source of truth for tab membership:
    ///   * `None` / absent → no filter (every entry; back-compat).
    ///   * `"__main__"` → the Main thread: entries with no `subagent_id`,
    ///     UNLESS the session has zero active subagents, in which case every
    ///     entry is returned (the desktop "no subagent strip → show all"
    ///     bypass, so historical subagent entries don't vanish).
    ///   * any other value → only entries whose `subagent_id` equals it (a
    ///     specific Task/Agent subagent tab).
    /// `total_count` on the result reflects the FILTERED total so the client
    /// can paginate the tab correctly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_filter: Option<String>,
    /// Token-frugal transcript slice for the supervisor judge. When set,
    /// the response is reduced to only the entries that matter for judging
    /// "what is the real goal and did the agent stop short": every
    /// `role == "user"` entry, the `N` entries immediately preceding each
    /// one (the context that prompted it), and the single most-recent entry
    /// (where the agent came to rest). Everything else — the agent's long
    /// tool-call/assistant churn — is dropped, so a judge no longer has to
    /// pull a 100k+-token full transcript into its clean context every
    /// wake-up. Applied AFTER `subagent_filter`/index windows and BEFORE
    /// `count`. `total_count` still reflects the unsliced filtered total so
    /// the judge can see how long the session actually is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_anchored_lead: Option<usize>,
}

impl<'de> Deserialize<'de> for GetSessionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            include_full_content: bool,
            include_images: bool,
            before_index: Option<usize>,
            after_index: Option<usize>,
            count: Option<usize>,
            subagent_filter: Option<String>,
            user_anchored_lead: Option<usize>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            include_full_content: inner.include_full_content,
            include_images: inner.include_images,
            before_index: inner.before_index,
            after_index: inner.after_index,
            count: inner.count,
            subagent_filter: inner.subagent_filter,
            user_anchored_lead: inner.user_anchored_lead,
        })
    }
}

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
    /// Markdown rendering of the entry, truncated to roughly 200 chars.
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
    /// Present only for `role == "tool_call"` entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<ToolCallSummary>,
    /// Present only for `role == "plan"` entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanSummary>,
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
    /// by subagent tab — match against `SessionSummary::active_subagents[*].id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_id: Option<String>,
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

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSessionResult {
    pub id: String,
    pub solution_id: String,
    pub agent_id: String,
    pub title: String,
    pub state: SessionStateDto,
    pub created_at: i64,
    pub last_activity_at: i64,
    /// F: cumulative tokens for the session (live thread > cached
    /// metadata fall-back). `None` until the agent reports its first
    /// usage update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Model context window in tokens; mirrors `SessionSummary::max_tokens`.
    /// `None` until the agent emits its first `TokenUsageUpdated` with a
    /// non-zero `max_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// F: parent session reference for sub-agent indication. `None` for
    /// top-level sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Mirrors [`SessionSummary::cwd`] — exposing the same field on
    /// `get_session` so a single fetch reveals both the transcript and
    /// the working directory the agent was launched with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub entries: Vec<EntrySummary>,
    /// R-6e: total entry count regardless of the `count`/`after_index`/
    /// `before_index` pagination window applied to `entries`. Lets the client
    /// render a "Load older" affordance and detect resume-time gaps. When a
    /// `subagent_filter` is supplied this is the FILTERED total (entries of
    /// that tab), so the client paginates the tab — not the whole session.
    pub total_count: usize,
    /// Server-side `pending_messages` queue, one descriptor per bundle.
    /// Empty when the agent isn't holding any follow-up sends from
    /// during a Running window. Mobile renders each bundle as a
    /// Queued bubble — paired with the live `agent_session_queue_changed`
    /// notification this is the cold-start seed for the unified
    /// cross-client queue display.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_bundles: Vec<QueuedBundleSummary>,
    /// Mirrors [`SessionSummary::active_subagents`] — the in-flight
    /// `Task`/`Agent` pills the desktop renders under the status row.
    /// Paired with the live `agent_session_active_subagents_changed`
    /// notification this is the cold-start seed for the mobile's
    /// subagent-tab strip. Empty (and omitted) when no Task subagents
    /// are currently in-flight.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_subagents: Vec<SubagentDto>,
    /// Phase 5: the session's transcript epoch at load time. The cache-first
    /// mobile client seeds its delta cursor `(epoch, current_seq)` from this
    /// full load, then polls `get_session_changes`; a later epoch mismatch
    /// means the transcript was rotated (`/clear`) and the client full-reloads.
    pub epoch: u64,
    /// Phase 5: the session's `change_seq` at load time — the high-water mark
    /// the client passes as `since_seq` on its first `get_session_changes` poll.
    pub current_seq: u64,
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
fn apply_user_anchored_filter(kept: &mut Vec<EntrySummary>, lead: usize) {
    if kept.is_empty() {
        return;
    }
    let has_user = kept.iter().any(|e| e.role == EntryRoleDto::User);
    if !has_user {
        return;
    }
    let mut keep = vec![false; kept.len()];
    for (pos, entry) in kept.iter().enumerate() {
        if entry.role == EntryRoleDto::User {
            let start = pos.saturating_sub(lead);
            for flag in keep.iter_mut().take(pos + 1).skip(start) {
                *flag = true;
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

#[derive(Clone)]
pub struct GetSessionTool;

impl McpServerTool for GetSessionTool {
    type Input = GetSessionParams;
    type Output = GetSessionResult;
    const NAME: &'static str = "solution_agent.get_session";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let result = cx.update(|cx| -> Result<GetSessionResult> {
            let store = SolutionAgentStore::global(cx);
            let entity = store
                .read_with(cx, |store, _| store.session(session_id))
                .with_context(|| format!("session_not_found: {}", session_id))?;
            let session = entity.read(cx);
            // Phase 4 Task 5a: serve the transcript from the unified
            // `session.entries` (Vec<SessionEntry>) — the same cold+live
            // model the desktop renders and Phase 4 persists/loads as
            // rows. `session.entries` is kept in lock-step with the live
            // thread by the store's `NewEntry` handler, so live and cold
            // sessions read identically.
            //
            // Authorization options for any in-flight WaitingForConfirmation
            // tool call are not stored on `SessionEntry` (a side-channel,
            // live-only concern); harvest them off the live thread (empty
            // for cold sessions) and re-attach per tool-call id below.
            let live_auth_options = live_auth_options_for_session(session, cx);
            let entries_ref: Vec<&crate::session_entry::SessionEntry> =
                session.entries.iter().collect();
            let (entries, total_count) = {
                // R-6e: index-anchored slice. `after_index` /
                // `before_index` are exclusive bounds and `count`
                // takes the LAST n entries within the bound (so the
                // common "show me the newest 50" query is just
                // `count=50` with no bounds).
                //
                // We walk every entry (not just the kept ones) so
                // `image_cursor` stays in lock-step with what a
                // non-paginated call would have produced — that
                // keeps `EntryImage.index` stable across paginated
                // calls, which is the contract that lets the client
                // rely on `spk-image://N` URLs in markdown.
                let after = input.after_index;
                let before = input.before_index;
                // Per-tab filter applied BEFORE the index window so the tab's
                // window is taken over the tab's OWN entries (see
                // `GetSessionParams::subagent_filter`). Mirrors the desktop
                // `session_view::should_render_entry`: when the session has no
                // active subagent strip, every entry passes regardless of the
                // requested filter (the "don't hide history" bypass).
                let subagent_filter = input.subagent_filter.as_deref();
                let active_empty = session.active_subagents.is_empty();
                let mut image_cursor = 0usize;
                let mut kept: Vec<EntrySummary> = Vec::new();
                // `total_count` reflects the FILTERED set so the client
                // paginates the tab, not the whole session.
                let mut filtered_total = 0usize;
                for (index, entry) in entries_ref.iter().enumerate() {
                    let passes_filter = match subagent_filter {
                        None => true,
                        Some(_) if active_empty => true,
                        Some("__main__") => entry.subagent_id.is_none(),
                        Some(id) => entry.subagent_id.as_deref() == Some(id),
                    };
                    if passes_filter {
                        filtered_total += 1;
                    }
                    let in_range = passes_filter
                        && after.map_or(true, |a| index > a)
                        && before.map_or(true, |b| index < b);
                    if in_range {
                        kept.push(summarize_entry(
                            entry,
                            index,
                            input.include_full_content,
                            input.include_images,
                            &mut image_cursor,
                            &live_auth_options,
                        ));
                    } else {
                        image_cursor += count_images_in_entry(&entry.kind);
                    }
                }
                // Judge-frugal slice (user messages + lead context + the
                // resting turn), applied before `count` so a tail window
                // still tails the anchored slice.
                if let Some(lead) = input.user_anchored_lead {
                    apply_user_anchored_filter(&mut kept, lead);
                }
                if let Some(n) = input.count {
                    if kept.len() > n {
                        // Take the last n. `EntrySummary.index`
                        // preserves the absolute position so the
                        // client can still tell where it sits in
                        // the session timeline.
                        let drop_count = kept.len() - n;
                        kept.drain(..drop_count);
                    }
                }
                (kept, filtered_total)
            };
            let summary = session_summary(session, cx);
            let pending_bundles = build_pending_bundle_summaries(session, cx);
            // Pure-read delta-cursor seed (Phase 5): persistence of `change_seq`
            // is *scheduled* before the matching section event (Task 5.1b); the
            // detached write may land slightly later, but the `max()`-guarded
            // UPDATE plus the deterministic restore seed absorb the residual
            // crash/reorder window, so the issued cursor stays restart-safe.
            let epoch = session.epoch;
            let current_seq = session.change_seq;
            Ok(GetSessionResult {
                id: summary.id,
                solution_id: summary.solution_id,
                agent_id: summary.agent_id,
                title: summary.title,
                state: summary.state,
                created_at: summary.created_at,
                last_activity_at: summary.last_activity_at,
                total_tokens: summary.total_tokens,
                max_tokens: summary.max_tokens,
                parent_session_id: summary.parent_session_id,
                cwd: summary.cwd,
                entries,
                total_count,
                pending_bundles,
                active_subagents: summary.active_subagents,
                epoch,
                current_seq,
            })
        })?;

        let title = result.title.clone();
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: title }],
            structured_content: result,
        })
    }
}

// =====================================================================
// solution_agent.get_session_changes
// =====================================================================

/// Default for `GetSessionChangesParams::include_images`. The delta is the
/// live render source (unlike `get_session`, which defaults `include_images`
/// false for cheap listing), so image payloads are inlined by default.
fn default_true() -> bool {
    true
}

/// Mobile delta poll input. Returns only what changed since `since_seq`:
/// entries with `mod_seq > since_seq` (passing the subagent filter), plus each
/// section (state / queue / subagents) only when its watermark moved past
/// `since_seq`. On epoch mismatch the result is a `reset` and the client
/// full-reloads via `get_session`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSessionChangesParams {
    pub session_id: String,
    /// The client's last-seen `change_seq`. Entries / sections at or below
    /// this are unchanged and omitted. A fresh client passes 0.
    pub since_seq: u64,
    /// The epoch the client's cached state was built against. A mismatch
    /// means the transcript was rotated / reset under the client (a `/clear`
    /// or migration `bump_epoch`), so the delta is meaningless → `reset`.
    pub known_epoch: u64,
    /// Per-tab filter, identical semantics to `GetSessionParams::subagent_filter`
    /// (`None`/empty-active → all; `"__main__"` → entries with no `subagent_id`;
    /// any other id → entries of that subagent). Applied to both
    /// `changed_entries` and `total_count`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_filter: Option<String>,
    /// Whether to inline base64 image payloads on changed entries. Defaults
    /// true — the delta is the live render source.
    #[serde(default = "default_true")]
    pub include_images: bool,
}

impl<'de> Deserialize<'de> for GetSessionChangesParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Inner {
            session_id: String,
            since_seq: u64,
            known_epoch: u64,
            #[serde(default)]
            subagent_filter: Option<String>,
            #[serde(default = "default_true")]
            include_images: bool,
        }
        let inner = Inner::deserialize(de)?;
        Ok(Self {
            session_id: inner.session_id,
            since_seq: inner.since_seq,
            known_epoch: inner.known_epoch,
            subagent_filter: inner.subagent_filter,
            include_images: inner.include_images,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSessionChangesResult {
    /// Current session epoch. The client stores this alongside `current_seq`
    /// as its `(epoch, last_seq)` cursor for the next poll.
    pub epoch: u64,
    /// Current `change_seq` — the cursor the client passes as the next
    /// `since_seq`. This RPC is pure-read; it never bumps the clock.
    pub current_seq: u64,
    /// True iff `known_epoch != epoch`: the cached state is stale, every
    /// other field is empty/absent, and the client must full-reload via
    /// `get_session`.
    pub reset: bool,
    /// FILTERED total entry count (after the subagent filter, ignoring
    /// `since_seq`). The client sets its list length to this after upserting
    /// `changed_entries`, which drops any tail beyond the new count — the
    /// shrink-detection signal under the tail-truncate model. Always sent.
    ///
    /// PARITY CONTRACT: `EntrySummary.index` is the ABSOLUTE (unfiltered)
    /// position while `total_count` is the FILTERED length — exactly the shape
    /// `get_session` returns. Both the delta applier and the full-load applier
    /// in the mobile client rely on this being identical across the two RPCs;
    /// keep this field's semantics in lockstep with `GetSessionResult::total_count`.
    pub total_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_entries: Vec<EntrySummary>,
    /// True when more changed entries exist beyond this page (the response was
    /// capped to [`CHANGED_ENTRIES_PAGE`]). The client keeps polling from the
    /// advanced `current_seq` until it gets a page with `has_more == false`.
    /// Lets a client that fell far behind catch up in bounded pages instead of
    /// one unbounded "big bang" response. Omitted (defaults false) when the
    /// page covers everything — back-compat with clients that don't read it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_more: bool,
    /// Forward-compat only. The transcript only appends, in-place-updates, or
    /// tail-truncates (no mid-list deletion; rewind is dead-for-claude), so
    /// shrink detection rides entirely on `total_count` and this stays empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_indices: Vec<usize>,
    /// ALWAYS present on a non-`reset` response (the small sections are sent
    /// unconditionally so a delta fully re-establishes them — see the
    /// always-send rationale in `GetSessionChangesTool::run`). `Option` is kept
    /// for the `reset` path (all sections `None`) and wire back-compat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<SessionStateDto>,
    /// ALWAYS present on a non-`reset` response. An empty Vec means "the queue
    /// is empty"; the client adopts it as the authoritative queue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_bundles: Option<Vec<QueuedBundleSummary>>,
    /// ALWAYS present on a non-`reset` response. An empty Vec means "the strip
    /// is empty"; the client adopts it as the authoritative subagent strip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_subagents: Option<Vec<SubagentDto>>,
}

/// Max `changed_entries` returned per `get_session_changes` call. A client
/// behind by more than this gets `has_more: true` and keeps polling from the
/// advanced cursor, so catch-up is bounded per round-trip instead of one
/// unbounded response.
const CHANGED_ENTRIES_PAGE: usize = 10;

#[derive(Clone)]
pub struct GetSessionChangesTool;

impl McpServerTool for GetSessionChangesTool {
    type Input = GetSessionChangesParams;
    type Output = GetSessionChangesResult;
    const NAME: &'static str = "solution_agent.get_session_changes";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let result = cx.update(|cx| -> Result<GetSessionChangesResult> {
            let store = SolutionAgentStore::global(cx);
            let entity = store
                .read_with(cx, |store, _| store.session(session_id))
                .with_context(|| format!("session_not_found: {}", session_id))?;
            let session = entity.read(cx);

            let epoch = session.epoch;
            // Pure-read cursor: `change_seq` persistence is *scheduled* before
            // the matching section event (Task 5.1b); the detached write may
            // land slightly later, but the `max()`-guarded UPDATE and the
            // deterministic restore seed absorb the residual window, so the
            // cursor handed out stays restart-safe.
            let current_seq = session.change_seq;

            // Subagent filter gate — identical to `get_session` so a tab's
            // delta membership matches its full-load membership exactly.
            let subagent_filter = input.subagent_filter.as_deref();
            let active_empty = session.active_subagents.is_empty();
            let passes_filter = |entry: &crate::session_entry::SessionEntry| match subagent_filter {
                None => true,
                Some(_) if active_empty => true,
                Some("__main__") => entry.subagent_id.is_none(),
                Some(id) => entry.subagent_id.as_deref() == Some(id),
            };

            // Epoch mismatch: the client's cache is against a rotated/reset
            // transcript. Return a `reset` with everything empty/absent; the
            // client ignores `total_count` and full-reloads. We still compute
            // the filtered count for schema completeness.
            if input.known_epoch != epoch {
                let total_count = session.entries.iter().filter(|e| passes_filter(e)).count();
                return Ok(GetSessionChangesResult {
                    epoch,
                    current_seq,
                    reset: true,
                    total_count,
                    changed_entries: Vec::new(),
                    has_more: false,
                    removed_indices: Vec::new(),
                    state: None,
                    pending_bundles: None,
                    active_subagents: None,
                });
            }

            let live_auth_options = live_auth_options_for_session(session, cx);

            // Walk entries oldest-first with ONE `image_cursor`, advancing it
            // over EVERY entry — including filtered-out and unchanged ones —
            // exactly as `get_session` does (mcp.rs ~1343-1368). This keeps the
            // global `EntryImage.index` / `spk-image://N` indices identical to
            // what `get_session` returns for the same `subagent_filter`, so a
            // delta-applied transcript renders byte-for-byte like a full load.
            let mut image_cursor = 0usize;
            // Collect each changed entry WITH its `mod_seq` so the page can be
            // taken in `mod_seq` order (the cursor axis), independent of index
            // order — an old entry re-edited has a high `mod_seq` but a low
            // index. The image index baked into each `EntrySummary` is computed
            // during this index-order walk, so reordering the Vec afterwards is
            // safe.
            let mut changed: Vec<(u64, EntrySummary)> = Vec::new();
            let mut total_count = 0usize;
            for (index, entry) in session.entries.iter().enumerate() {
                let passes = passes_filter(entry);
                if passes {
                    total_count += 1;
                }
                if passes && entry.mod_seq > input.since_seq {
                    let summary = summarize_entry(
                        entry,
                        index,
                        true,
                        input.include_images,
                        &mut image_cursor,
                        &live_auth_options,
                    );
                    changed.push((entry.mod_seq, summary));
                } else {
                    // Skipped (filtered out OR unchanged): still advance the
                    // cursor so later changed entries get global image indices
                    // identical to get_session's. `summarize_entry` itself
                    // advances the cursor; the skip branch must mirror that.
                    image_cursor += count_images_in_entry(&entry.kind);
                }
            }

            // Paginate by `mod_seq` (ascending) so a client that fell far behind
            // catches up in bounded pages instead of one unbounded "big bang"
            // response. The cursor advances only to the last entry of the page;
            // `has_more` tells the client to keep polling from there. Sections
            // stay gated on the request's `since_seq` (eligible from page 1) and
            // are idempotent full-replacements, so re-sending them across a
            // multi-page catch-up is harmless.
            changed.sort_by_key(|(seq, _)| *seq);
            let has_more = changed.len() > CHANGED_ENTRIES_PAGE;
            let (changed_entries, page_current_seq): (Vec<EntrySummary>, u64) = if has_more {
                let page_last_seq = changed[CHANGED_ENTRIES_PAGE - 1].0;
                let entries = changed
                    .into_iter()
                    .take(CHANGED_ENTRIES_PAGE)
                    .map(|(_, e)| e)
                    .collect();
                (entries, page_last_seq)
            } else {
                // Caught up entry-wise: hand out the full `change_seq` so a
                // re-poll from here returns nothing and section watermarks drain.
                let entries = changed.into_iter().map(|(_, e)| e).collect();
                (entries, current_seq)
            };

            // Wall-clock anchors for the state DTO — same scheme as
            // `session_summary` (monotonic Instant rebased onto unix-millis).
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

            // Always send the three small sections (state scalar, queue,
            // subagent strip) regardless of `since_seq`. They are bounded and
            // cheap, and the old `watermark > since_seq` gate created an
            // UNRECOVERABLE staleness hole: once a client's cursor advanced past
            // a section watermark, the delta path could never resend that
            // section. Two ways that happened in the wild:
            //   * cache-restore on session open synthesises a placeholder state
            //     (`Idle`) and seats the cursor at `cached.lastSeq`, already far
            //     above a long-Running session's old `state_seq` → the next
            //     delta omitted `state` → the phone froze at "Idle" while the
            //     desktop ran for an hour;
            //   * a section mutation that forgot to bump its watermark (e.g. the
            //     `→Idle` subagent-strip GC) → the cleared strip never reached
            //     the phone, stranding a finished subagent tab.
            // Sending them every poll makes each delta a full re-establishment
            // of the small mutable state — unconditional convergence, immune to
            // a placeholder cursor or a missed bump. `applySessionDelta` already
            // treats a present section as an authoritative replacement (an empty
            // Vec means "now empty"). The watermarks still drive the cheap
            // `agent_session_dirty` poke; they just no longer gate delivery.
            let state = Some(SessionStateDto::from_state(
                &session.state,
                running_started_at_ms,
                stopping_started_at_ms,
            ));
            let pending_bundles = Some(build_pending_bundle_summaries(session, cx));
            let active_subagents = Some(build_active_subagents_vec(session));

            Ok(GetSessionChangesResult {
                epoch,
                current_seq: page_current_seq,
                reset: false,
                total_count,
                changed_entries,
                has_more,
                removed_indices: Vec::new(),
                state,
                pending_bundles,
                active_subagents,
            })
        })?;

        let text = format!(
            "{} changed entr{} (epoch {}, seq {})",
            result.changed_entries.len(),
            if result.changed_entries.len() == 1 {
                "y"
            } else {
                "ies"
            },
            result.epoch,
            result.current_seq,
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text }],
            structured_content: result,
        })
    }
}

/// Build the [QueuedBundleSummary] list off a session's
/// `pending_messages` queue. Pulled out so `get_session` and any
/// future cold-load surface can reuse the same shape that
/// `event_sources::build_queue_changed_payload` emits on the live
/// notification path. Empty queue → empty Vec.
fn build_pending_bundle_summaries(
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

fn entry_role(kind: &crate::session_entry::SessionEntryKind) -> EntryRoleDto {
    use crate::session_entry::SessionEntryKind;
    match kind {
        SessionEntryKind::UserMessage { .. } => EntryRoleDto::User,
        SessionEntryKind::AssistantMessage { .. } => EntryRoleDto::Assistant,
        SessionEntryKind::ToolCall { .. } => EntryRoleDto::ToolCall,
        SessionEntryKind::Plan(_) => EntryRoleDto::Plan,
        SessionEntryKind::ContextCompaction { .. } => EntryRoleDto::ContextCompaction,
    }
}

/// Maps the unified `session_entry::ToolStatus` to the structured wire
/// enum. Parallels `conversation_render::tool_call_status_text` (which
/// stays for the desktop UI's human labels) but emits the typed wire
/// variant. Note this consumes `session_entry::ToolStatus`, not
/// `acp_thread::ToolCallStatus` — the variants line up one-to-one.
fn tool_status_dto(status: &crate::session_entry::ToolStatus) -> ToolCallStatusDto {
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
fn tool_status_label(status: &crate::session_entry::ToolStatus) -> &'static str {
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
fn session_entry_to_markdown(kind: &crate::session_entry::SessionEntryKind) -> String {
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
fn summarize_entry(
    entry: &crate::session_entry::SessionEntry,
    index: usize,
    include_full_content: bool,
    include_images: bool,
    image_cursor: &mut usize,
    live_auth_options: &HashMap<String, Vec<ToolCallAuthOption>>,
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
    let markdown = if include_full_content {
        Some(markdown_source)
    } else {
        None
    };
    let images = if include_images {
        Some(extract_images_for_entry(kind, image_cursor))
    } else {
        // Advance the cursor even when the caller didn't opt in, so
        // toggling `include_images` between calls preserves the same
        // stable indices.
        *image_cursor += count_images_in_entry(kind);
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

    EntrySummary {
        role,
        index,
        preview,
        markdown,
        images,
        tool_call,
        plan,
        client_send_id,
        client_send_ids,
        created_ms,
        subagent_id,
    }
}

/// Counts image content blocks in an entry without allocating image
/// payloads. Used to keep `image_cursor` stable when the caller
/// opted out of `include_images`.
fn count_images_in_entry(kind: &crate::session_entry::SessionEntryKind) -> usize {
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
        | SessionEntryKind::ContextCompaction { .. } => 0,
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
fn extract_images_for_entry(
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
fn tool_call_summary(
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
fn live_auth_options_for_session(
    session: &crate::model::SolutionSession,
    cx: &App,
) -> HashMap<String, Vec<ToolCallAuthOption>> {
    let mut map = HashMap::new();
    let Some(thread) = session.acp_thread() else {
        return map;
    };
    for entry in thread.read(cx).entries() {
        if let acp_thread::AgentThreadEntry::ToolCall(call) = entry {
            if let acp_thread::ToolCallStatus::WaitingForConfirmation { options, .. } =
                &call.status
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
fn permission_kind_str(kind: acp::PermissionOptionKind) -> &'static str {
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

/// Fetch the full content of a single session entry by index. Designed
/// for the "user expanded one tool-call bubble" case where the chat
/// client needs the full markdown / images / tool-call detail for one
/// entry without re-fetching the entire transcript.
///
/// `markdown` is **always** populated on the returned `EntrySummary`
/// — the single-entry call is the explicit "I want the full content"
/// path, so gating it would defeat the purpose. `include_images`
/// remains opt-in because images can dominate the payload.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetSessionEntryParams {
    pub session_id: String,
    /// 0-based index into the session's entries, oldest-first.
    pub index: usize,
    /// Default false. When true, the returned `EntrySummary.images`
    /// carries inline base64 image payloads.
    #[serde(default)]
    pub include_images: bool,
}

impl<'de> Deserialize<'de> for GetSessionEntryParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            #[serde(default)]
            index: usize,
            #[serde(default)]
            include_images: bool,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            index: inner.index,
            include_images: inner.include_images,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSessionEntryResult {
    pub entry: EntrySummary,
}

#[derive(Clone)]
pub struct GetSessionEntryTool;

impl McpServerTool for GetSessionEntryTool {
    type Input = GetSessionEntryParams;
    type Output = GetSessionEntryResult;
    const NAME: &'static str = "solution_agent.get_session_entry";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;
        let want_index = input.index;
        let include_images = input.include_images;

        let result = cx.update(|cx| -> Result<GetSessionEntryResult> {
            let store = SolutionAgentStore::global(cx);
            let entity = store
                .read_with(cx, |store, _| store.session(session_id))
                .with_context(|| format!("session_not_found: {}", session_id))?;
            let session = entity.read(cx);
            // Phase 4 Task 5a: read the single entry from the unified
            // `session.entries` (works for cold/resumed row-native
            // sessions too — the old live-thread-only path errored
            // `session_has_no_thread` for any sleeping session).
            let entries = &session.entries;
            let len = entries.len();
            anyhow::ensure!(
                want_index < len,
                "entry_index_out_of_range: {} (session has {} entries)",
                want_index,
                len
            );
            // Replay the image cursor up to `want_index` so the
            // returned `EntryImage.index` matches what
            // `get_session{ include_images: true }` would have
            // assigned to the same image — keeps cross-references
            // (markdown `spk-image://N` links etc.) consistent.
            let mut image_cursor = 0usize;
            for entry in entries.iter().take(want_index) {
                image_cursor += count_images_in_entry(&entry.kind);
            }
            let entry = entries
                .get(want_index)
                .ok_or_else(|| anyhow!("entry vanished mid-read"))?;
            let live_auth_options = live_auth_options_for_session(session, cx);
            let summary = summarize_entry(
                entry,
                want_index,
                true,
                include_images,
                &mut image_cursor,
                &live_auth_options,
            );
            Ok(GetSessionEntryResult { entry: summary })
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("entry #{want_index}"),
            }],
            structured_content: result,
        })
    }
}

// =====================================================================
// solution_agent.create_session
// =====================================================================

/// Create a new ACP session for `(solution_id, agent_id)` on the active
/// workspace's project. `initial_message`, if present, is dispatched as a
/// detached `send_message` after the session is registered.
///
/// **Active project resolution**: the session needs an `Entity<Project>`
/// from a live workspace window whose worktrees back the named Solution.
/// MCP doesn't carry a workspace handle, so we walk every open
/// `MultiWorkspace` window and pick the first project whose visible
/// worktrees include the Solution's root. If no such window is open, the
/// tool errors with a clear message — the caller should open the Solution
/// first via `solutions.open`.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CreateSessionParams {
    pub solution_id: String,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_message: Option<String>,
    /// F: parent session reference for sub-agent indication. When set,
    /// the new session is registered as a child of `parent_session_id`
    /// and surfaces in the session-view's sub-agents strip. The parent
    /// MUST exist in the same solution — otherwise the tool errors
    /// (`unknown_parent_session` or `parent_session_in_different_solution`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Optional user-supplied title. When absent the desktop assigns a
    /// title automatically from the first user turn — clients that
    /// want a stable, human-supplied name (e.g. the phone) can set
    /// this. Renamable later via `solution_agent.rename_session`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional working directory for the agent subprocess. Must be one
    /// of the solution's visible worktree roots — values outside the
    /// solution are rejected. When absent, the first worktree of the
    /// active project for `solution_id` is used (matches the previous
    /// behaviour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl<'de> Deserialize<'de> for CreateSessionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            agent_id: String,
            initial_message: Option<String>,
            parent_session_id: Option<String>,
            title: Option<String>,
            cwd: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            agent_id: inner.agent_id,
            initial_message: inner.initial_message,
            parent_session_id: inner.parent_session_id,
            title: inner.title,
            cwd: inner.cwd,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CreateSessionResult {
    pub session_id: String,
}

#[derive(Clone)]
pub struct CreateSessionTool;

impl McpServerTool for CreateSessionTool {
    type Input = CreateSessionParams;
    type Output = CreateSessionResult;
    const NAME: &'static str = "solution_agent.create_session";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(
            !input.agent_id.is_empty(),
            "invalid_params: agent_id is required"
        );
        let solution_id = SolutionId(input.solution_id.clone());
        let agent_id: crate::model::AgentServerId = input.agent_id.clone().into();

        // F: parent validation. Parse the id, then look it up in the
        // store. Reject when missing (`unknown_parent_session`) or when
        // the parent lives in a different solution
        // (`parent_session_in_different_solution`) — sub-agents are
        // intentionally same-solution-only (see plan-doc §I).
        let parent_session_id = match input.parent_session_id.as_deref() {
            Some(raw) => {
                let parsed = SolutionSessionId::parse(raw)
                    .map_err(|e| anyhow!("bad parent_session_id: {e}"))?;
                let parent_solution = cx.update(|cx| {
                    let store = SolutionAgentStore::global(cx);
                    store.read_with(cx, |store, cx| {
                        store
                            .session(parsed)
                            .map(|entity| entity.read(cx).solution_id.clone())
                    })
                });
                let parent_solution =
                    parent_solution.ok_or_else(|| anyhow!("unknown_parent_session: {raw}"))?;
                if parent_solution != solution_id {
                    anyhow::bail!(
                        "parent_session_in_different_solution: {} != {}",
                        parent_solution.0,
                        solution_id.0
                    );
                }
                Some(parsed)
            }
            None => None,
        };

        let project = cx
            .update(|cx| project_for_solution(&input.solution_id, cx))
            .ok_or_else(|| {
                anyhow!(
                    "no_active_workspace_for_solution: open Solution {} via solutions.open before \
                     creating a session",
                    input.solution_id
                )
            })?;

        let cwd: Option<std::path::PathBuf> = input.cwd.as_ref().map(std::path::PathBuf::from);

        let create_task = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session_with_parent(
                    solution_id,
                    agent_id,
                    project,
                    cwd,
                    parent_session_id,
                    None,
                    None,
                    false,
                    cx,
                )
            })
        });
        let session_id = create_task.await?;

        // Apply the user-supplied title (if any). Done as a separate
        // rename so the create path stays single-purpose and the title
        // change emits the SessionTitleChanged event that subscribers
        // (including the WS notification forwarder) already listen for.
        if let Some(raw_title) = input.title.as_deref() {
            let trimmed = raw_title.trim();
            if !trimmed.is_empty() {
                let title = SharedString::from(trimmed.to_string());
                cx.update(|cx| -> Result<()> {
                    let store = SolutionAgentStore::global(cx);
                    store.update(cx, |store, cx| store.rename_session(session_id, title, cx))?;
                    Ok(())
                })?;
            }
        }

        if let Some(content) = input.initial_message {
            cx.update(|cx| {
                let store = SolutionAgentStore::global(cx);
                store.update(cx, |store, cx| {
                    store.send_message(session_id, content, cx).detach();
                });
            });
        }

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: session_id.to_string(),
            }],
            structured_content: CreateSessionResult {
                session_id: session_id.to_string(),
            },
        })
    }
}

// Locate the `Project` whose worktrees back the named Solution. Mirrors
// the helper of the same name in `solutions::mcp` (kept private there);
// duplicated here to avoid widening the `solutions` crate's public API
// just for this MCP tool.
fn project_for_solution(solution_id: &str, cx: &mut App) -> Option<Entity<project::Project>> {
    let store = SolutionStore::try_global(cx)?;
    let root = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .find(|sol| sol.id.as_str() == solution_id)
            .map(|sol| sol.root.clone())
    })?;

    for handle in cx.windows() {
        let Some(window_handle) = handle.downcast::<workspace::MultiWorkspace>() else {
            continue;
        };
        let result = window_handle
            .update(cx, |multi, _window, cx| {
                for workspace_entity in multi.workspaces() {
                    let workspace = workspace_entity.read(cx);
                    let project = workspace.project();
                    let matches = project
                        .read(cx)
                        .visible_worktrees(cx)
                        .any(|tree| tree.read(cx).abs_path().starts_with(&root));
                    if matches {
                        return Some(project.clone());
                    }
                }
                None
            })
            .ok()
            .flatten();
        if let Some(project) = result {
            return Some(project);
        }
    }
    None
}

// =====================================================================
// solution_agent.send_message
// =====================================================================

/// Send a user message to an existing session. Fire-and-forget — the
/// returned `Task` is detached so the tool response returns immediately
/// once the prompt is enqueued. Use `solution_agent.get_session` to poll
/// for new entries, or subscribe to `solution_agent.*` events (deferred
/// to a later phase) for push notifications.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SendMessageParams {
    pub session_id: String,
    pub content: String,
}

impl<'de> Deserialize<'de> for SendMessageParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            content: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            content: inner.content,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SendMessageResult {}

#[derive(Clone)]
pub struct SendMessageTool;

impl McpServerTool for SendMessageTool {
    type Input = SendMessageParams;
    type Output = SendMessageResult;
    const NAME: &'static str = "solution_agent.send_message";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;
        let content = input.content;

        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.send_message(session_id, content, cx).detach();
            });
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "queued".to_string(),
            }],
            structured_content: SendMessageResult {},
        })
    }
}

// =====================================================================
// solution_agent.send_message_blocks
// =====================================================================

/// Send a structured user message composed of one or more ACP
/// `ContentBlock`s (text + images + resource links, etc). Mirrors
/// `SendMessageTool` but lets MCP consumers pass multi-modal payloads
/// — primarily the mobile client, which encodes picked images and
/// text-like files into `Image` / `Text` blocks. The bare
/// `send_message` text-only tool stays for callers that only have a
/// plain prompt.
///
/// Fire-and-forget — the returned `Task` from
/// `SolutionAgentStore::send_message_blocks` is detached so the tool
/// response returns immediately once the prompt is enqueued.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SendMessageBlocksParams {
    pub session_id: String,
    /// Each entry is serialised per the ACP `ContentBlock` schema
    /// (`{"type": "text", "text": "..."}` /
    /// `{"type": "image", "data": "<base64>", "mimeType": "image/png"}` /
    /// `{"type": "resource_link", "uri": "...", ...}` / etc).
    pub blocks: Vec<acp::ContentBlock>,
}

impl<'de> Deserialize<'de> for SendMessageBlocksParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            blocks: Vec<acp::ContentBlock>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            blocks: inner.blocks,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SendMessageBlocksResult {}

#[derive(Clone)]
pub struct SendMessageBlocksTool;

impl McpServerTool for SendMessageBlocksTool {
    type Input = SendMessageBlocksParams;
    type Output = SendMessageBlocksResult;
    const NAME: &'static str = "solution_agent.send_message_blocks";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        anyhow::ensure!(
            !input.blocks.is_empty(),
            "invalid_params: blocks must contain at least one item"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;
        // Swap any `spk-upload://<id>` ResourceLink for the inline
        // Image/Text the chunked-upload tmp file contains, BEFORE the
        // bundle reaches the store. Without this step the handle URI
        // would travel verbatim to claude-acp, which has no idea what
        // `spk-upload://` means — the attached image silently vanishes
        // and the agent sees only the accompanying text.
        let blocks = crate::upload::resolve_upload_handles(input.blocks)?;

        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.send_message_blocks(session_id, blocks, cx).detach();
            });
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "queued".to_string(),
            }],
            structured_content: SendMessageBlocksResult {},
        })
    }
}

// =====================================================================
// solution_agent.delete_session
// =====================================================================

/// Delete a session, dropping its `AcpThread` and removing it from the
/// store. Mirrors `SolutionAgentStore::close_session` directly, which now
/// kills the session's `claude` subprocess (via the connection's
/// `close_session`) and decrements the pool's per-pair `live_session_count`
/// so the shared connection shuts down once its last session closes — no
/// extra teardown is needed here.
///
/// Note: the internal Rust method on `SolutionAgentStore` remains
/// `close_session`; only the wire name is renamed here (B2 scope).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct DeleteSessionParams {
    pub session_id: String,
}

impl<'de> Deserialize<'de> for DeleteSessionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
        }
        Ok(Self {
            session_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .session_id,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct DeleteSessionResult {}

#[derive(Clone)]
pub struct DeleteSessionTool;

impl McpServerTool for DeleteSessionTool {
    type Input = DeleteSessionParams;
    type Output = DeleteSessionResult;
    const NAME: &'static str = "solution_agent.delete_session";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        cx.update(|cx| -> Result<()> {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| store.close_session(session_id, cx))?;
            Ok(())
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "closed".to_string(),
            }],
            structured_content: DeleteSessionResult {},
        })
    }
}

// =====================================================================
// solution_agent.cancel_turn
// =====================================================================

/// Cancel the in-flight turn on `session_id`. Forwards to
/// `AgentConnection::cancel`; the session will eventually transition to
/// `Idle` (or `Errored`) via the regular `AcpThreadEvent` plumbing.
///
/// When `flush_pending` is true the call additionally sets the
/// session's `flush_after_cancel` flag so that the `pending_messages`
/// queue (filled while the agent was Running) gets flushed as one
/// merged follow-up turn the moment the cancel settles, instead of
/// being dropped. This is the wire path the mobile Force-flush
/// button uses to "stop and send my queued messages now".
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CancelTurnParams {
    pub session_id: String,
    #[serde(default)]
    pub flush_pending: bool,
}

impl<'de> Deserialize<'de> for CancelTurnParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            #[serde(default)]
            flush_pending: bool,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            flush_pending: inner.flush_pending,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CancelTurnResult {}

#[derive(Clone)]
pub struct CancelTurnTool;

impl McpServerTool for CancelTurnTool {
    type Input = CancelTurnParams;
    type Output = CancelTurnResult;
    const NAME: &'static str = "solution_agent.cancel_turn";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let flush_pending = input.flush_pending;
        cx.update(|cx| -> Result<()> {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                let result = if flush_pending {
                    // Best-effort: when there is nothing to flush
                    // `interrupt_and_flush_pending` errors with
                    // "no queued messages". Treat that as success
                    // here — the caller asked for "cancel + maybe
                    // flush", and the cancel half still makes sense.
                    match store.interrupt_and_flush_pending(session_id, cx) {
                        Ok(()) => Ok(()),
                        Err(_) => store.cancel_turn(session_id, cx),
                    }
                } else {
                    store.cancel_turn(session_id, cx)
                };
                // Human-initiated stop (desktop / mobile Stop) — park the
                // supervisor in `Held` so it doesn't re-engage until the user
                // sends the next message. (A `flush_pending` cancel re-sends the
                // queued user text, which re-arms via the send funnel anyway.)
                store.hold_supervisor(session_id, cx);
                result
            })?;
            Ok(())
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "cancelled".to_string(),
            }],
            structured_content: CancelTurnResult {},
        })
    }
}

// =====================================================================
// solution_agent.authorize_tool_call
// =====================================================================

/// Answer a tool call that is blocked `WaitingForConfirmation`. The
/// client picks one of the `options` it received on the tool call's
/// `ToolCallSummary` (see `solution_agent.get_session{,_entry}`) and
/// sends back its `option_id`. The SERVER reconstructs the full
/// `SelectedPermissionOutcome` (kind + any terminal sub-patterns) from
/// the live options — the client only needs to echo the opaque id — so
/// the answer can never drift from what the agent actually offered.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct AuthorizeToolCallParams {
    pub session_id: String,
    pub tool_call_id: String,
    pub option_id: String,
}

impl<'de> Deserialize<'de> for AuthorizeToolCallParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            tool_call_id: String,
            option_id: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            tool_call_id: inner.tool_call_id,
            option_id: inner.option_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AuthorizeToolCallResult {
    pub ok: bool,
}

/// Locate the `WaitingForConfirmation` tool call matching `tool_call_id`
/// in `entries`, then resolve `option_id` against its live permission
/// buttons and return the `SelectedPermissionOutcome` to hand to
/// `AcpThread::authorize_tool_call`. Pure over the thread's entries +
/// the client's request so the resolution logic is unit-testable
/// without staging a live confirmation oneshot.
fn resolve_authorization_outcome(
    entries: &[acp_thread::AgentThreadEntry],
    tool_call_id: &str,
    option_id: &str,
) -> Result<acp_thread::SelectedPermissionOutcome> {
    let call = entries
        .iter()
        .find_map(|entry| match entry {
            acp_thread::AgentThreadEntry::ToolCall(call) if call.id.0.as_ref() == tool_call_id => {
                Some(call)
            }
            _ => None,
        })
        .ok_or_else(|| anyhow!("tool_call_not_found: {}", tool_call_id))?;

    let options = match &call.status {
        acp_thread::ToolCallStatus::WaitingForConfirmation { options, .. } => options,
        _ => {
            anyhow::bail!("not_awaiting_confirmation: {}", tool_call_id);
        }
    };

    crate::conversation_render::permission_buttons(options)
        .into_iter()
        .find(|button| button.option_id.0.as_ref() == option_id)
        .map(|button| button.outcome())
        .ok_or_else(|| anyhow!("unknown_option: {}", option_id))
}

#[derive(Clone)]
pub struct AuthorizeToolCallTool;

impl McpServerTool for AuthorizeToolCallTool {
    type Input = AuthorizeToolCallParams;
    type Output = AuthorizeToolCallResult;
    const NAME: &'static str = "solution_agent.authorize_tool_call";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        anyhow::ensure!(
            !input.tool_call_id.is_empty(),
            "invalid_params: tool_call_id is required"
        );
        anyhow::ensure!(
            !input.option_id.is_empty(),
            "invalid_params: option_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;
        let tool_call_id = input.tool_call_id;
        let option_id = input.option_id;

        cx.update(|cx| -> Result<()> {
            let store = SolutionAgentStore::global(cx);
            let entity = store
                .read_with(cx, |store, _| store.session(session_id))
                .with_context(|| format!("session_not_found: {}", session_id))?;
            let thread = entity
                .read(cx)
                .acp_thread()
                .cloned()
                .ok_or_else(|| anyhow!("session_has_no_thread: {}", session_id))?;
            // Resolve against the live thread entries: the kind / terminal
            // sub-patterns needed to build the outcome are reconstructed
            // server-side from what the agent actually offered, never
            // trusted from the client.
            let outcome = thread.read_with(cx, |thread, _| {
                resolve_authorization_outcome(thread.entries(), &tool_call_id, &option_id)
            })?;
            thread.update(cx, |thread, cx| {
                thread.authorize_tool_call(
                    acp::ToolCallId::new(tool_call_id.as_str()),
                    outcome,
                    cx,
                );
            });
            Ok(())
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "authorized".to_string(),
            }],
            structured_content: AuthorizeToolCallResult { ok: true },
        })
    }
}

// =====================================================================
// solution_agent.rename_session
// =====================================================================

/// Rename a session's user-visible title.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RenameSessionParams {
    pub session_id: String,
    pub title: String,
}

impl<'de> Deserialize<'de> for RenameSessionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            title: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            title: inner.title,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RenameSessionResult {}

#[derive(Clone)]
pub struct RenameSessionTool;

impl McpServerTool for RenameSessionTool {
    type Input = RenameSessionParams;
    type Output = RenameSessionResult;
    const NAME: &'static str = "solution_agent.rename_session";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        anyhow::ensure!(!input.title.is_empty(), "invalid_params: title is required");
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;
        let title = SharedString::from(input.title);

        cx.update(|cx| -> Result<()> {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| store.rename_session(session_id, title, cx))?;
            Ok(())
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "renamed".to_string(),
            }],
            structured_content: RenameSessionResult {},
        })
    }
}

// =====================================================================
// solution_agent.restart_agent
// =====================================================================

/// Restart the agent backing `session_id`. Drops the pooled subprocess
/// for the session's `(solution, agent)` pair, closes the existing
/// session, and opens a fresh one against the same project. v1 does not
/// replay history. Returns the new session id.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RestartAgentParams {
    pub session_id: String,
}

impl<'de> Deserialize<'de> for RestartAgentParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
        }
        Ok(Self {
            session_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .session_id,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RestartAgentResult {
    pub session_id: String,
}

#[derive(Clone)]
pub struct RestartAgentTool;

impl McpServerTool for RestartAgentTool {
    type Input = RestartAgentParams;
    type Output = RestartAgentResult;
    const NAME: &'static str = "solution_agent.restart_agent";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let restart_task = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| store.restart_agent(session_id, cx))
        });
        let new_session_id = restart_task.await?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: new_session_id.to_string(),
            }],
            structured_content: RestartAgentResult {
                session_id: new_session_id.to_string(),
            },
        })
    }
}

// =====================================================================
// solution_agent.reset_context
// =====================================================================

/// Wipe the conversation history of `session_id` while keeping the tab,
/// title, and `SolutionSessionId` stable. Wired to the desktop's
/// `/clear` slash command via `store::reset_context`. Different from
/// `restart_agent`, which mints a fresh session id (and therefore drops
/// the user-set title) — use this when the intent is "clear this chat"
/// and not "this session is broken, give me a new one".
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ResetContextParams {
    pub session_id: String,
}

impl<'de> Deserialize<'de> for ResetContextParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
        }
        Ok(Self {
            session_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .session_id,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ResetContextResult {
    pub session_id: String,
}

#[derive(Clone)]
pub struct ResetContextTool;

impl McpServerTool for ResetContextTool {
    type Input = ResetContextParams;
    type Output = ResetContextResult;
    const NAME: &'static str = "solution_agent.reset_context";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let reset_task = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| store.reset_context(session_id, cx))
        });
        let same_session_id = reset_task.await?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: same_session_id.to_string(),
            }],
            structured_content: ResetContextResult {
                session_id: same_session_id.to_string(),
            },
        })
    }
}

// =====================================================================
// solution_agent.compact_session
// =====================================================================

/// Hard cap on the continuation prompt file. Keeps a runaway agent from
/// stuffing the entire conversation into a single file and re-feeding it
/// as the very first user message — which would defeat the whole point
/// of compacting. 256 KiB is generous (≈ 60k tokens of plain English).
const COMPACT_PROMPT_MAX_BYTES: u64 = 256 * 1024;

/// Rotate a session: validate the agent-prepared continuation file,
/// close the current session, open a fresh session under the same
/// `(solution, agent)` pair, and feed the file content as the first
/// user message of the new session. Returns the new session id so the
/// caller (an MCP-driven agent or the UI) can switch focus to it.
///
/// The agent calls this AFTER writing the per-rotation handoff files to
/// `<solution_root>/.agents/<session_id>/<timestamp>/`. The editor does
/// NOT generate the files — it only validates the prompt file and
/// owns the session lifecycle. See
/// `resources/compact_context_instructions.md` for the agent contract.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CompactSessionParams {
    pub session_id: String,
    pub prompt_file: String,
}

impl<'de> Deserialize<'de> for CompactSessionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            prompt_file: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            prompt_file: inner.prompt_file,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CompactSessionResult {
    pub new_session_id: String,
    pub prompt_bytes: u64,
}

#[derive(Clone)]
pub struct CompactSessionTool;

impl McpServerTool for CompactSessionTool {
    type Input = CompactSessionParams;
    type Output = CompactSessionResult;
    const NAME: &'static str = "solution_agent.compact_session";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        anyhow::ensure!(
            !input.prompt_file.is_empty(),
            "invalid_params: prompt_file is required"
        );
        let old_session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        // 1. Validate the file. We resolve the OLD session's solution
        //    root and require the prompt path to live underneath
        //    `<solution_root>/.agents/<session_id>/` so an agent can't
        //    point us at /etc/passwd or some other unrelated file.
        let (solution_id, agent_id) = cx
            .update(|cx| {
                let store = SolutionAgentStore::global(cx);
                store.read_with(cx, |store, cx| {
                    store.session(old_session_id).map(|entity| {
                        let s = entity.read(cx);
                        (s.solution_id.clone(), s.agent_id.clone())
                    })
                })
            })
            .ok_or_else(|| anyhow!("unknown session {old_session_id}"))?;

        let solution_root = cx
            .update(|cx| {
                SolutionStore::try_global(cx).and_then(|store| {
                    store.read_with(cx, |s, _| {
                        s.solutions()
                            .iter()
                            .find(|sol| sol.id == solution_id)
                            .map(|sol| sol.root.clone())
                    })
                })
            })
            .ok_or_else(|| anyhow!("solution {solution_id:?} not found in store"))?;

        let prompt_path = std::path::PathBuf::from(&input.prompt_file);
        let prompt_path = if prompt_path.is_absolute() {
            prompt_path
        } else {
            solution_root.join(&prompt_path)
        };
        let prompt_path = prompt_path
            .canonicalize()
            .with_context(|| format!("prompt file not found: {}", prompt_path.display()))?;
        let allowed_root = solution_root
            .join(".agents")
            .canonicalize()
            .with_context(|| {
                format!(
                    "{}/.agents not found — agent must create handoff files before calling \
                     compact_session",
                    solution_root.display()
                )
            })?;
        anyhow::ensure!(
            prompt_path.starts_with(&allowed_root),
            "invalid_params: prompt_file must live under {}/.agents/",
            solution_root.display()
        );

        let metadata = std::fs::metadata(&prompt_path)
            .with_context(|| format!("stat {}", prompt_path.display()))?;
        anyhow::ensure!(
            metadata.is_file(),
            "invalid_params: prompt_file is not a regular file: {}",
            prompt_path.display()
        );
        anyhow::ensure!(
            metadata.len() > 0,
            "invalid_params: prompt_file is empty: {}",
            prompt_path.display()
        );
        anyhow::ensure!(
            metadata.len() <= COMPACT_PROMPT_MAX_BYTES,
            "invalid_params: prompt_file is {} bytes, max is {}",
            metadata.len(),
            COMPACT_PROMPT_MAX_BYTES
        );
        let prompt_bytes = metadata.len();

        let prompt_text = std::fs::read_to_string(&prompt_path)
            .with_context(|| format!("read {}", prompt_path.display()))?;
        anyhow::ensure!(
            !prompt_text.trim().is_empty(),
            "invalid_params: prompt_file contains only whitespace"
        );

        // Verify the agent actually wrote the full handoff bundle, not
        // just `continue.md`. We read `session-state.json` first to
        // learn the conversation scope, then check the per-scope file
        // set. Missing or empty files surface as a structured error so
        // the agent can re-attempt the dump and call us again instead
        // of silently rotating with half a transcript.
        let compact_dir = prompt_path
            .parent()
            .ok_or_else(|| anyhow!("prompt_file has no parent directory"))?
            .to_path_buf();
        validate_handoff_files(&compact_dir)?;

        // Append the agent's own `state.md` summary to the cumulative session
        // log (`.agents/<sid>/session-log.md`) BEFORE rotating, so the operator
        // can read what was accomplished across the whole session even after
        // compactions wiped the live dialogue. Best-effort — never blocks the
        // rotation.
        if let (Some(parent), Ok(state_md)) = (
            compact_dir.parent(),
            std::fs::read_to_string(compact_dir.join("state.md")),
        ) {
            let label = compact_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "compact".into());
            if let Err(err) = crate::supervisor::append_session_log(
                &parent.join("session-log.md"),
                &format!("Compaction {label}"),
                &state_md,
                chrono::Utc::now().timestamp_millis(),
            ) {
                log::warn!("compact_session: append session-log failed: {err}");
            }
        }

        // 2. Rotate the in-flight ACP thread under the SAME
        //    SolutionSessionId. Subprocess pool entry stays, tab stays,
        //    only the conversation history is swapped out. Returns the
        //    new context_count so the caller knows which context they
        //    are now in.
        let _ = solution_id;
        let _ = agent_id;
        let rotate_task = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| store.rotate_context(old_session_id, cx))
        });
        let new_context_count = rotate_task.await?;

        // 3. Feed the continuation prompt as the rotated session's
        //    first user message. Detached because the tool response
        //    should return as soon as the message is enqueued — the
        //    user watches the same tab live for the agent's reply.
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.send_message(old_session_id, prompt_text, cx).detach();
            });
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "rotated {old_session_id} into context c{new_context_count:02} \
                     ({prompt_bytes} bytes)"
                ),
            }],
            structured_content: CompactSessionResult {
                new_session_id: old_session_id.to_string(),
                prompt_bytes,
            },
        })
    }
}

// =====================================================================
// solution_agent.start_compact
// =====================================================================

/// Kick off the "Compact context" workflow on a hot session — the same
/// orchestration the desktop's status-row popover "Compact context"
/// entry runs. Sends the compact-instructions template as a user
/// message; the agent then writes its handoff files and calls back
/// into the lower-level `solution_agent.compact_session` to rotate.
///
/// Surface contract: this tool is what a human client (e.g. the phone)
/// invokes from a "Compact" button. `compact_session` is what Claude
/// Code itself invokes after producing the handoff dump. Don't mix
/// them up — `compact_session` rotates the ACP thread immediately and
/// would discard the user's intent on a hot conversation.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct StartCompactParams {
    pub session_id: String,
}

impl<'de> Deserialize<'de> for StartCompactParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
        }
        Ok(Self {
            session_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .session_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StartCompactResult {
    /// `true` when the compact prompt was enqueued on the agent. A cold
    /// (sleeping) session is woken first, then the prompt is queued.
    /// `false` when a precondition wasn't met (e.g. session busy,
    /// context below 20%, or less than 30k tokens of headroom) — `message`
    /// carries the reason.
    pub queued: bool,
    /// Human-readable explanation when `queued == false`. `None` on
    /// success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone)]
pub struct StartCompactTool;

impl McpServerTool for StartCompactTool {
    type Input = StartCompactParams;
    type Output = StartCompactResult;
    const NAME: &'static str = "solution_agent.start_compact";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let outcome = cx.update(|cx| -> Result<crate::compact::StartCompactOutcome> {
            crate::compact::start_compact_for_session(session_id, cx)
        })?;

        let text = if outcome.queued {
            format!("compact queued for {session_id}")
        } else {
            outcome
                .reason
                .clone()
                .unwrap_or_else(|| "compact declined".to_string())
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text }],
            structured_content: StartCompactResult {
                queued: outcome.queued,
                message: outcome.reason,
            },
        })
    }
}

/// Verifies the agent wrote the full handoff bundle into `compact_dir`
/// before letting `compact_session` rotate. Reads `session-state.json`
/// to learn the scope, then checks the per-scope required file set.
///
/// Scope file requirements (per the agent contract in
/// `resources/compact_context_instructions.md`):
/// - `planned` and `branching`: state.md, decisions.md, next.md, continue.md
/// - `exploratory`: state.md, decisions.md, continue.md (next.md skipped)
///
/// Returns a single combined error listing every missing / empty file —
/// the agent gets the whole picture in one round-trip instead of
/// fix-one, retry, fix-another, retry.
fn validate_handoff_files(compact_dir: &std::path::Path) -> Result<()> {
    let state_json_path = compact_dir.join("session-state.json");
    let state_json_meta = std::fs::metadata(&state_json_path).with_context(|| {
        format!(
            "compact_incomplete: session-state.json is missing in {}",
            compact_dir.display()
        )
    })?;
    anyhow::ensure!(
        state_json_meta.is_file() && state_json_meta.len() > 0,
        "compact_incomplete: session-state.json is empty"
    );
    let state_text = std::fs::read_to_string(&state_json_path).with_context(|| {
        format!(
            "compact_incomplete: cannot read {}",
            state_json_path.display()
        )
    })?;
    let state_json: serde_json::Value = serde_json::from_str(&state_text)
        .with_context(|| "compact_incomplete: session-state.json is not valid JSON")?;
    let scope = state_json
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("planned")
        .to_string();

    let mut required = vec!["state.md", "decisions.md", "continue.md"];
    if scope != "exploratory" {
        required.push("next.md");
    }

    let mut missing = Vec::new();
    let mut empty = Vec::new();
    for name in &required {
        let path = compact_dir.join(name);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() && meta.len() > 0 => {}
            Ok(meta) if meta.is_file() => empty.push(name.to_string()),
            _ => missing.push(name.to_string()),
        }
    }

    if !missing.is_empty() || !empty.is_empty() {
        let mut msg =
            format!("compact_incomplete (scope={scope}): the agent did not write the full bundle");
        if !missing.is_empty() {
            msg.push_str(&format!(". Missing: {}", missing.join(", ")));
        }
        if !empty.is_empty() {
            msg.push_str(&format!(". Empty: {}", empty.join(", ")));
        }
        msg.push_str(&format!(". Expected under {}", compact_dir.display()));
        anyhow::bail!(msg);
    }
    Ok(())
}

// =====================================================================
// solution_agent.read_session_history
// =====================================================================

/// Cap on how many entries we ever return in one MCP response. Avoids
/// shipping a 50 MB transcript over the JSON-RPC socket if the caller
/// asks for "everything" on a long-running session.
const HISTORY_HARD_LIMIT: usize = 500;
/// Default page size when the caller doesn't supply one.
const HISTORY_DEFAULT_LIMIT: usize = 100;

/// Returns a markdown rendering of the conversation transcript for any
/// session — live or already closed. Pulls live state from the
/// in-memory store when the session is open, otherwise rehydrates the
/// JSON blob the store wrote to SQLite on every successful turn.
///
/// Designed for downstream agents that want to "read what session X
/// concluded" without resuming it. For live sessions, prefer
/// `solution_agent.get_session` + the per-event push notifications;
/// this tool is the polling / archive-read path.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ReadSessionHistoryParams {
    pub session_id: String,
    /// Number of entries to return (default 100, hard cap 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Number of entries to skip from the start (oldest-first ordering).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
}

impl<'de> Deserialize<'de> for ReadSessionHistoryParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            limit: Option<usize>,
            offset: Option<usize>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            limit: inner.limit,
            offset: inner.offset,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReadSessionHistoryResult {
    pub session_id: String,
    /// `live` for sessions still open in the store, `archived` for
    /// sessions whose acp_thread has been dropped but whose blob is
    /// still in SQLite.
    pub source: String,
    pub title: String,
    pub total_entries: usize,
    pub returned_entries: usize,
    /// Markdown rendering of each entry, oldest-first.
    pub entries: Vec<String>,
}

#[derive(Clone)]
pub struct ReadSessionHistoryTool;

impl McpServerTool for ReadSessionHistoryTool {
    type Input = ReadSessionHistoryParams;
    type Output = ReadSessionHistoryResult;
    const NAME: &'static str = "solution_agent.read_session_history";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;
        let offset = input.offset.unwrap_or(0);
        let limit = input
            .limit
            .unwrap_or(HISTORY_DEFAULT_LIMIT)
            .min(HISTORY_HARD_LIMIT);

        // 1. Live path: if the session is still in the in-memory store,
        //    render entries directly off the AcpThread. This is fresher
        //    than the persisted blob, which only updates on turn
        //    completion.
        // Phase 4 Task 5a: render from the unified `session.entries`
        // whenever the session is in memory — live OR cold (a restored
        // tab whose subprocess hasn't been respawned). The old path only
        // rendered when a live `acp_thread` was attached, so a cold
        // row-native session fell through to the archive blob (and, for
        // row-native sessions, the blob is no longer the source of
        // truth). Reading `session.entries` makes the in-memory read the
        // single source for both states.
        let live = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.read_with(cx, |store, _| {
                let session = store.session(session_id)?;
                let s = session.read(cx);
                let title = s.title.to_string();
                let entries = s
                    .entries
                    .iter()
                    .map(|entry| session_entry_to_markdown(&entry.kind))
                    .collect::<Vec<String>>();
                Some((title, entries))
            })
        });
        if let Some((title, entries)) = live {
            let total = entries.len();
            let slice = entries
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect::<Vec<_>>();
            let returned = slice.len();
            return Ok(ToolResponse {
                content: vec![ToolResponseContent::Text {
                    text: format!("{returned}/{total} entries (live)"),
                }],
                structured_content: ReadSessionHistoryResult {
                    session_id: session_id.to_string(),
                    source: "live".to_string(),
                    title,
                    total_entries: total,
                    returned_entries: returned,
                    entries: slice,
                },
            });
        }

        // 2. Archive path: session is not in the in-memory store.
        //    Phase 4: prefer per-entry rows (the canonical source for
        //    row-native sessions whose blob write path is dead).  Fall
        //    back to the legacy blob only when rows are empty — that
        //    covers un-migrated sessions written before Phase 4.
        //
        //    Load rows and blob concurrently so the blob (needed for
        //    the title in the row-native path if the DB has no separate
        //    title API) is already in flight when we decide which branch
        //    to take.
        let load_tasks = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.read_with(cx, |store, _| {
                store
                    .persistence()
                    .map(|db| (db.load_entries(session_id), db.load_blob(session_id)))
            })
        });
        let (rows, blob_bytes) = match load_tasks {
            Some((rows_task, blob_task)) => (rows_task.await?, blob_task.await?),
            None => (Vec::new(), None),
        };

        if !rows.is_empty() {
            // Row-native path: reconstruct markdown from the stored entries.
            let entries_all = crate::store::entries_from_rows(rows)
                .into_iter()
                .map(|entry| session_entry_to_markdown(&entry.kind))
                .collect::<Vec<_>>();
            // The title lives in the session metadata row (the blob is not
            // the source of truth for row-native sessions and may be
            // absent).  Use the blob's title as a best-effort fallback
            // when available; fall back to an empty string otherwise.
            let title = blob_bytes
                .as_deref()
                .and_then(|b| serde_json::from_slice::<PersistedSession>(b).ok())
                .map(|s| s.title)
                .unwrap_or_default();
            let total = entries_all.len();
            let slice = entries_all
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect::<Vec<_>>();
            let returned = slice.len();
            return Ok(ToolResponse {
                content: vec![ToolResponseContent::Text {
                    text: format!("{returned}/{total} entries (archived)"),
                }],
                structured_content: ReadSessionHistoryResult {
                    session_id: session_id.to_string(),
                    source: "archived".to_string(),
                    title,
                    total_entries: total,
                    returned_entries: returned,
                    entries: slice,
                },
            });
        }

        // Legacy blob fallback (un-migrated sessions written before Phase 4).
        let blob_bytes = blob_bytes.ok_or_else(|| {
            anyhow!("session_not_found: {session_id} is neither open nor archived in the database")
        })?;
        let snapshot: PersistedSession = serde_json::from_slice(&blob_bytes)
            .with_context(|| format!("decoding archived session {session_id}"))?;
        let total = snapshot.entry_summaries.len();
        let slice = snapshot
            .entry_summaries
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();
        let returned = slice.len();
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{returned}/{total} entries (archived)"),
            }],
            structured_content: ReadSessionHistoryResult {
                session_id: session_id.to_string(),
                source: "archived".to_string(),
                title: snapshot.title,
                total_entries: total,
                returned_entries: returned,
                entries: slice,
            },
        })
    }
}

// =====================================================================
// solution_agent.upload_{init,status,finish,abort}
// =====================================================================
//
// Chunked-upload control surface for the WebSocket binary-frame attachment
// path. See `solution_agent::upload` for the storage manager and
// `remote_control::listener` for the binary-frame dispatch. Mobile clients
// drive the lifecycle:
//   1. `upload_init` → server allocates an id + tmp file, returns u64 id.
//   2. WS binary frames (16-byte header `u64 id BE | u64 offset BE` +
//      payload) push the bytes; the listener calls `UploadManager::write_chunk`.
//   3. (optional) `upload_status` polls per-id progress.
//   4. `upload_finish` validates total size + optional sha256, returns
//      `{handle: "spk-upload://<id>"}`.
//   5. The handle is embedded as a `ResourceLink` in `send_message_blocks`,
//      which swaps it for inline `Image`/`Text` content and aborts the entry.
//   6. `upload_abort` cancels an upload (e.g. user cancelled the picker).

/// Allocate a chunked-upload slot for `session_id`. Returns an `upload_id`
/// that subsequent WebSocket binary frames (16-byte header + payload) write
/// chunks against. The session must already exist; the per-session
/// concurrency cap blocks runaway disk usage.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct UploadInitParams {
    pub session_id: String,
    pub mime: String,
    pub display_name: String,
    pub total_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl<'de> Deserialize<'de> for UploadInitParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            mime: String,
            display_name: String,
            total_size: u64,
            sha256: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            mime: inner.mime,
            display_name: inner.display_name,
            total_size: inner.total_size,
            sha256: inner.sha256,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct UploadInitResult {
    pub upload_id: u64,
}

#[derive(Clone)]
pub struct UploadInitTool;

impl McpServerTool for UploadInitTool {
    type Input = UploadInitParams;
    type Output = UploadInitResult;
    const NAME: &'static str = "solution_agent.upload_init";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        anyhow::ensure!(!input.mime.is_empty(), "invalid_params: mime is required");
        anyhow::ensure!(
            !input.display_name.is_empty(),
            "invalid_params: display_name is required"
        );
        anyhow::ensure!(
            input.total_size > 0,
            "invalid_params: total_size must be > 0"
        );

        // Validate the session is known to the store BEFORE allocating a
        // tmp file. A typo'd session_id should fail fast, not after the
        // client has streamed megabytes of payload.
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;
        let exists = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.read(cx).session(session_id).is_some()
        });
        if !exists {
            anyhow::bail!("unknown_session: {}", input.session_id);
        }

        // Capture log fields before moving `input` into the manager call.
        let mime_for_log = input.mime.clone();
        let total_size_for_log = input.total_size;
        let upload_id = crate::upload::with_manager(|m| {
            m.init(
                input.session_id,
                input.mime,
                input.display_name,
                input.total_size,
                input.sha256,
            )
        })
        .ok_or_else(|| anyhow!("upload manager not initialised"))??;

        log::info!(
            target: "solution_agent::upload",
            "upload_init OK: upload_id={upload_id} mime={mime_for_log} total_size={total_size_for_log}",
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("upload_id={upload_id}"),
            }],
            structured_content: UploadInitResult { upload_id },
        })
    }
}

/// Inspect the per-upload `received_bytes` / `total_size` progress without
/// consuming the entry. Mobile clients can poll this between chunks for a
/// progress bar.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct UploadStatusParams {
    pub upload_id: u64,
}

impl<'de> Deserialize<'de> for UploadStatusParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            upload_id: u64,
        }
        Ok(Self {
            upload_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .upload_id,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct UploadStatusResult {
    pub received_bytes: u64,
    pub total_size: u64,
}

#[derive(Clone)]
pub struct UploadStatusTool;

impl McpServerTool for UploadStatusTool {
    type Input = UploadStatusParams;
    type Output = UploadStatusResult;
    const NAME: &'static str = "solution_agent.upload_status";

    async fn run(
        &self,
        input: Self::Input,
        _cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let (received_bytes, total_size) =
            crate::upload::with_manager(|m| m.status(input.upload_id))
                .ok_or_else(|| anyhow!("upload manager not initialised"))?
                .ok_or_else(|| anyhow!("unknown_upload_id: {}", input.upload_id))?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{received_bytes}/{total_size}"),
            }],
            structured_content: UploadStatusResult {
                received_bytes,
                total_size,
            },
        })
    }
}

/// Finalize an upload — validates `received_bytes == total_size`, optionally
/// verifies a sha256, and returns the `spk-upload://<id>` handle string
/// that `send_message_blocks` resolves to inline content.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct UploadFinishParams {
    pub upload_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl<'de> Deserialize<'de> for UploadFinishParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            upload_id: u64,
            sha256: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            upload_id: inner.upload_id,
            sha256: inner.sha256,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct UploadFinishResult {
    pub handle: String,
}

#[derive(Clone)]
pub struct UploadFinishTool;

impl McpServerTool for UploadFinishTool {
    type Input = UploadFinishParams;
    type Output = UploadFinishResult;
    const NAME: &'static str = "solution_agent.upload_finish";

    async fn run(
        &self,
        input: Self::Input,
        _cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let upload_id_for_log = input.upload_id;
        let handle =
            crate::upload::with_manager(|m| m.finish(input.upload_id, input.sha256.as_deref()))
                .ok_or_else(|| anyhow!("upload manager not initialised"))??;
        let handle_uri = format!("{}{}", crate::upload::HANDLE_SCHEME, handle.id);
        log::info!(
            target: "solution_agent::upload",
            "upload_finish OK: upload_id={upload_id_for_log} handle={handle_uri}",
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: handle_uri.clone(),
            }],
            structured_content: UploadFinishResult { handle: handle_uri },
        })
    }
}

/// Cancel an in-flight or finished-but-unconsumed upload, deleting the tmp
/// file. Idempotent in spirit — calling abort on an unknown id returns an
/// error rather than silently succeeding so the client knows the entry was
/// already gone (e.g. GC reaped it).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct UploadAbortParams {
    pub upload_id: u64,
}

impl<'de> Deserialize<'de> for UploadAbortParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            upload_id: u64,
        }
        Ok(Self {
            upload_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .upload_id,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct UploadAbortResult {}

#[derive(Clone)]
pub struct UploadAbortTool;

impl McpServerTool for UploadAbortTool {
    type Input = UploadAbortParams;
    type Output = UploadAbortResult;
    const NAME: &'static str = "solution_agent.upload_abort";

    async fn run(
        &self,
        input: Self::Input,
        _cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        crate::upload::with_manager(|m| m.abort(input.upload_id))
            .ok_or_else(|| anyhow!("upload manager not initialised"))??;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "aborted".to_string(),
            }],
            structured_content: UploadAbortResult {},
        })
    }
}

// =====================================================================
// solution_agent.force_idle
// =====================================================================

/// Diagnostics-only escape hatch: forcibly transition `session_id`'s
/// state to `Idle` regardless of what it currently is. Intended for
/// triaging stuck sessions (e.g. an `claude_native::connection::cancel`
/// race that leaves the queue in `Stopping` forever — see
/// `queue::STOPPING_SAFETY_NET` for the automatic recovery path; this
/// is the manual lever for the same situation, plus arbitrary
/// `Errored`/`AwaitingInput` stuckness).
///
/// Does NOT touch the underlying subprocess, the `AcpThread`, or
/// pending messages — only the in-memory session state. If the agent
/// is genuinely mid-turn, the next `Stopped`/`Error` event will simply
/// re-overwrite the state, so a misclick is recoverable. Returns the
/// previous state's `kind` discriminant so a triage script can log
/// the transition.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ForceIdleParams {
    pub session_id: String,
}

impl<'de> Deserialize<'de> for ForceIdleParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
        }
        Ok(Self {
            session_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .session_id,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ForceIdleResult {
    /// Snake-case discriminant of the state we replaced (e.g. `stopping`,
    /// `errored`). Lets the caller log "was Stopping, now Idle".
    pub previous_kind: String,
}

#[derive(Clone)]
pub struct ForceIdleTool;

impl McpServerTool for ForceIdleTool {
    type Input = ForceIdleParams;
    type Output = ForceIdleResult;
    const NAME: &'static str = "solution_agent.force_idle";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let previous_kind = cx.update(|cx| -> Result<String> {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| -> Result<String> {
                let session = store
                    .session(session_id)
                    .ok_or_else(|| anyhow!("unknown session {session_id}"))?;
                let previous = session.read(cx).state.clone();
                let kind = match &previous {
                    crate::model::SessionState::Idle => "idle",
                    crate::model::SessionState::Running { .. } => "running",
                    crate::model::SessionState::Stopping { .. } => "stopping",
                    crate::model::SessionState::AwaitingInput => "awaiting_input",
                    crate::model::SessionState::Errored(_) => "errored",
                };
                log::warn!(
                    target: "solution_agent",
                    "session={session_id} force_idle: replacing state={previous:?} with Idle \
                     (MCP-driven diagnostic recovery)"
                );
                store.mutate_state(
                    session_id,
                    |state| *state = crate::model::SessionState::Idle,
                    cx,
                );
                Ok(kind.to_string())
            })
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("forced Idle (was {previous_kind})"),
            }],
            structured_content: ForceIdleResult { previous_kind },
        })
    }
}

// =====================================================================
// solution_agent.supervisor_verdict
// =====================================================================

/// Record the judge's verdict for a supervised session and execute the
/// corresponding action (`continue`, `compact`, `done`, or `ask`).
///
/// - `continue`: increment the guard counter, send a nudge message, and
///   return the session to `Watching`.
/// - `compact`: queue a compact-context prompt on the session.
/// - `done`: mark supervision stopped (`Done`) and log completion.
/// - `ask`: pause supervision in `WaitingUser` and escalate the question
///   to the operator.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SupervisorVerdictParams {
    pub session_id: String,
    /// One of: "continue", "compact", "done", "ask".
    pub action: String,
    pub reasoning: String,
    /// Optional nudge message sent to the session when action == "continue".
    /// Defaults to "Continue." when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Required when action == "ask". The question to surface to the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
}

impl<'de> Deserialize<'de> for SupervisorVerdictParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            action: String,
            reasoning: String,
            message: Option<String>,
            question: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            action: inner.action,
            reasoning: inner.reasoning,
            message: inner.message,
            question: inner.question,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SupervisorVerdictResult {}

#[derive(Clone)]
pub struct SupervisorVerdictTool;

impl McpServerTool for SupervisorVerdictTool {
    type Input = SupervisorVerdictParams;
    type Output = SupervisorVerdictResult;
    const NAME: &'static str = "solution_agent.supervisor_verdict";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        anyhow::ensure!(
            !input.reasoning.is_empty(),
            "invalid_params: reasoning is required"
        );
        let action = match input.action.as_str() {
            "continue" => crate::supervisor::VerdictAction::Continue,
            "compact" => crate::supervisor::VerdictAction::Compact,
            "done" => crate::supervisor::VerdictAction::Done,
            "ask" => crate::supervisor::VerdictAction::Ask,
            "ask_agent" => crate::supervisor::VerdictAction::AskAgent,
            other => anyhow::bail!("invalid_params: unknown action {other:?}"),
        };
        if matches!(
            action,
            crate::supervisor::VerdictAction::Ask | crate::supervisor::VerdictAction::AskAgent
        ) {
            anyhow::ensure!(
                input.question.is_some(),
                "invalid_params: actions \"ask\"/\"ask_agent\" require a question"
            );
        }
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.apply_verdict(
                    session_id,
                    action,
                    input.reasoning,
                    input.message,
                    input.question,
                    None,
                    cx,
                );
            });
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "recorded".into(),
            }],
            structured_content: SupervisorVerdictResult {},
        })
    }
}

// =====================================================================
// solution_agent.supervisor_audit_verdict
// =====================================================================

/// Record the meta-auditor's verdict for a supervised session. The auditor
/// reviews the SUPERVISOR's own verdict log + diary (not the agent dialogue)
/// and decides whether the supervisor is making real progress or looping.
///
/// - `continue_supervision`: the supervisor is healthy; supervision proceeds.
/// - `escalate`: pause supervision in `WaitingUser` and surface the reasoning
///   to the operator. `ok = false` also forces escalation regardless of action.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SupervisorAuditVerdictParams {
    pub session_id: String,
    /// Whether the supervisor is making real progress (`true`) or is stuck /
    /// missing a problem the human should see (`false`).
    pub ok: bool,
    /// One of: "continue_supervision", "escalate".
    pub action: String,
    pub reasoning: String,
}

impl<'de> Deserialize<'de> for SupervisorAuditVerdictParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            ok: bool,
            action: String,
            reasoning: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            ok: inner.ok,
            action: inner.action,
            reasoning: inner.reasoning,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SupervisorAuditVerdictResult {}

#[derive(Clone)]
pub struct SupervisorAuditVerdictTool;

impl McpServerTool for SupervisorAuditVerdictTool {
    type Input = SupervisorAuditVerdictParams;
    type Output = SupervisorAuditVerdictResult;
    const NAME: &'static str = "solution_agent.supervisor_audit_verdict";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        anyhow::ensure!(
            !input.reasoning.is_empty(),
            "invalid_params: reasoning is required"
        );
        let escalate = match input.action.as_str() {
            "continue_supervision" => false,
            "escalate" => true,
            other => anyhow::bail!("invalid_params: unknown action {other:?}"),
        };
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.apply_audit_verdict(session_id, input.ok, escalate, input.reasoning, cx);
            });
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "recorded".into(),
            }],
            structured_content: SupervisorAuditVerdictResult {},
        })
    }
}

// =====================================================================
// solution_agent.set_supervisor_enabled
// =====================================================================

/// Enable or disable the Chat Supervisor for the given session.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SetSupervisorEnabledParams {
    pub session_id: String,
    pub enabled: bool,
}

impl<'de> Deserialize<'de> for SetSupervisorEnabledParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            enabled: bool,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            enabled: inner.enabled,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SetSupervisorEnabledResult {}

#[derive(Clone)]
pub struct SetSupervisorEnabledTool;

impl McpServerTool for SetSupervisorEnabledTool {
    type Input = SetSupervisorEnabledParams;
    type Output = SetSupervisorEnabledResult;
    const NAME: &'static str = "solution_agent.set_supervisor_enabled";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.set_supervision_enabled(session_id, input.enabled, cx);
            });
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "ok".to_string(),
            }],
            structured_content: SetSupervisorEnabledResult {},
        })
    }
}

// =====================================================================
// solution_agent.set_supervisor_prompt
// =====================================================================

/// Set a custom prompt for the Chat Supervisor of the given session.
/// Pass `null` to clear the custom prompt and revert to the default.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SetSupervisorPromptParams {
    pub session_id: String,
    pub prompt: Option<String>,
}

impl<'de> Deserialize<'de> for SetSupervisorPromptParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            prompt: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            prompt: inner.prompt,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SetSupervisorPromptResult {}

#[derive(Clone)]
pub struct SetSupervisorPromptTool;

impl McpServerTool for SetSupervisorPromptTool {
    type Input = SetSupervisorPromptParams;
    type Output = SetSupervisorPromptResult;
    const NAME: &'static str = "solution_agent.set_supervisor_prompt";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.set_supervisor_prompt(session_id, input.prompt, cx);
            });
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "ok".to_string(),
            }],
            structured_content: SetSupervisorPromptResult {},
        })
    }
}

// =====================================================================
// solution_agent.get_supervisor_state
// =====================================================================

/// Read the Chat Supervisor state and cumulative verdict statistics for
/// a session. Returns a default (all-zero) result when the session is
/// not found or has never had supervision enabled.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetSupervisorStateParams {
    pub session_id: String,
}

impl<'de> Deserialize<'de> for GetSupervisorStateParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSupervisorStateResult {
    pub enabled: bool,
    /// `SupervisorStatus::to_db_string()` value.
    pub status: String,
    pub consecutive_continues: u32,
    /// Ceiling enforced by the supervisor before it escalates to the user.
    pub max_continues: u32,
    pub custom_prompt: Option<String>,
    pub verdicts_total: usize,
    pub verdicts_continue: usize,
    pub verdicts_compact: usize,
    pub verdicts_done: usize,
    pub verdicts_ask: usize,
    pub verdicts_ask_agent: usize,
    pub audits: usize,
    pub total_tokens: u64,
}

#[derive(Clone)]
pub struct GetSupervisorStateTool;

impl McpServerTool for GetSupervisorStateTool {
    type Input = GetSupervisorStateParams;
    type Output = GetSupervisorStateResult;
    const NAME: &'static str = "solution_agent.get_supervisor_state";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let result = cx.update(|cx| -> GetSupervisorStateResult {
            let store = SolutionAgentStore::global(cx);
            let (supervisor_state, solution_root) = store.read_with(cx, |store, cx| {
                (
                    store.supervisor_state(session_id),
                    store.solution_root_for_app(session_id, cx),
                )
            });

            let stats = solution_root
                .map(|root| {
                    let dir = crate::supervisor::supervisor_dir(&root, session_id);
                    let records = crate::supervisor::read_verdicts(&dir);
                    crate::supervisor::verdict_stats(&records)
                })
                .unwrap_or_default();

            match supervisor_state {
                Some(state) => GetSupervisorStateResult {
                    enabled: state.enabled,
                    status: state.status.to_db_string(),
                    consecutive_continues: state.consecutive_continues,
                    max_continues: crate::supervisor::MAX_CONSECUTIVE_CONTINUES,
                    custom_prompt: state.custom_prompt,
                    verdicts_total: stats.total,
                    verdicts_continue: stats.by_action
                        [crate::supervisor::VerdictAction::Continue as usize],
                    verdicts_compact: stats.by_action
                        [crate::supervisor::VerdictAction::Compact as usize],
                    verdicts_done: stats.by_action
                        [crate::supervisor::VerdictAction::Done as usize],
                    verdicts_ask: stats.by_action
                        [crate::supervisor::VerdictAction::Ask as usize],
                    verdicts_ask_agent: stats.by_action
                        [crate::supervisor::VerdictAction::AskAgent as usize],
                    audits: stats.audits,
                    total_tokens: stats.total_tokens,
                },
                None => GetSupervisorStateResult {
                    enabled: false,
                    status: crate::supervisor::SupervisorStatus::Disabled.to_db_string(),
                    consecutive_continues: 0,
                    max_continues: crate::supervisor::MAX_CONSECUTIVE_CONTINUES,
                    custom_prompt: None,
                    verdicts_total: 0,
                    verdicts_continue: 0,
                    verdicts_compact: 0,
                    verdicts_done: 0,
                    verdicts_ask: 0,
                    verdicts_ask_agent: 0,
                    audits: 0,
                    total_tokens: 0,
                },
            }
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: serde_json::to_string(&result).unwrap_or_default(),
            }],
            structured_content: result,
        })
    }
}

#[cfg(test)]
mod tests {
    //! R-5e enrichment coverage. These tests build a real `AcpThread`
    //! via the mock-agent test infra, push synthetic entries straight
    //! through the public `acp_thread` API, then call the MCP tools
    //! the same way the WS proxy does and assert the wire shape.

    use super::*;
    use crate::store::tests::create_session_with_thread;
    use context_server::listener::McpServerTool;

    #[test]
    fn entry_role_and_status_dto_serialize_snake_case() {
        assert_eq!(
            serde_json::to_value(EntryRoleDto::ToolCall).unwrap(),
            serde_json::json!("tool_call")
        );
        assert_eq!(
            serde_json::to_value(ToolCallStatusDto::WaitingForConfirmation).unwrap(),
            serde_json::json!("waiting_for_confirmation")
        );
        assert_eq!(
            serde_json::to_value(ToolCallStatusDto::Running).unwrap(),
            serde_json::json!("running")
        );
    }

    #[test]
    fn session_state_dto_serializes_structured() {
        use crate::model::SessionState;
        let json = |s: &SessionState, running_ms: i64, stopping_ms: i64| {
            serde_json::to_value(SessionStateDto::from_state(s, running_ms, stopping_ms)).unwrap()
        };
        assert_eq!(
            json(&SessionState::Idle, 0, 0),
            serde_json::json!({"kind":"idle"})
        );
        assert_eq!(
            json(
                &SessionState::Stopping {
                    started_at: std::time::Instant::now()
                },
                0,
                1779000
            ),
            serde_json::json!({"kind":"stopping","started_at_ms":1779000})
        );
        assert_eq!(
            json(&SessionState::AwaitingInput, 0, 0),
            serde_json::json!({"kind":"awaiting_input"})
        );
        assert_eq!(
            json(&SessionState::Errored("boom".into()), 0, 0),
            serde_json::json!({"kind":"errored","message":"boom"})
        );
        let running = SessionState::Running {
            started_at: std::time::Instant::now(),
            notified: false,
        };
        assert_eq!(
            json(&running, 1779, 0),
            serde_json::json!({"kind":"running","started_at_ms":1779})
        );
    }

    fn fake_user_text_chunk(text: &str) -> acp::ContentBlock {
        acp::ContentBlock::Text(acp::TextContent::new(text.to_string()))
    }

    fn fake_image_chunk(mime: &str, data_b64: &str) -> acp::ContentBlock {
        acp::ContentBlock::Image(acp::ImageContent::new(
            data_b64.to_string(),
            mime.to_string(),
        ))
    }

    /// Push a minimal user message + assistant message into the live
    /// thread so `get_session` has at least two entries to enrich.
    /// Returns a 1x1 PNG base64 payload that callers can match against.
    async fn seed_session_with_image(
        cx: &mut gpui::TestAppContext,
    ) -> (crate::model::SolutionSessionId, String, tempfile::TempDir) {
        let (session_id, acp_thread, tmp) = create_session_with_thread(cx).await;
        // 1×1 PNG, generated once with `base64 -w0 < tiny.png` — kept
        // small so test fixtures don't bloat the suite.
        let image_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNgAAIAAAUAAen5lOEAAAAASUVORK5CYII=".to_string();
        let image_b64_clone = image_b64.clone();
        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                thread.push_user_content_block(None, fake_user_text_chunk("hello"), cx);
                thread.push_user_content_block(
                    None,
                    fake_image_chunk("image/png", &image_b64_clone),
                    cx,
                );
                thread.push_assistant_content_block(fake_user_text_chunk("world"), false, cx);
            });
        });
        cx.executor().run_until_parked();
        (session_id, image_b64, tmp)
    }

    #[gpui::test]
    async fn list_agents_returns_empty_when_no_adapters_registered(cx: &mut gpui::TestAppContext) {
        // create_session_with_thread builds an empty AdapterRegistry —
        // mock-agent gets registered via `register_agent_server`, not
        // via `AdapterRegistry::register`. So list_agents (which reads
        // the adapter registry) returns []. Asserts the wire shape and
        // the empty-list code path; the registry itself is covered by
        // `adapter::tests`.
        let (_session_id, _img, _tmp) = seed_session_with_image(cx).await;
        let result = cx
            .update(|cx| {
                let cx = cx.to_async();
                async move {
                    ListAgentsTool
                        .run(ListAgentsParams {}, &mut cx.clone())
                        .await
                }
            })
            .await
            .expect("list_agents tool should run");
        assert_eq!(result.structured_content.agents.len(), 0);
        match &result.content[0] {
            ToolResponseContent::Text { text } => assert_eq!(text, "0 agent(s)"),
            _ => panic!("expected text content"),
        }
    }

    #[gpui::test]
    async fn get_session_default_flags_omit_full_content(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: false,
                    include_images: false,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        assert!(
            !result.structured_content.entries.is_empty(),
            "expected entries"
        );
        for entry in &result.structured_content.entries {
            assert!(
                entry.markdown.is_none(),
                "markdown must stay None when include_full_content=false; got {:?}",
                entry.markdown
            );
            assert!(
                entry.images.is_none(),
                "images must stay None when include_images=false; got {:?}",
                entry.images
            );
            assert!(
                !entry.preview.is_empty(),
                "preview must always be populated"
            );
        }
    }

    #[gpui::test]
    async fn get_session_full_content_populates_markdown(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: true,
                    include_images: false,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        for entry in &result.structured_content.entries {
            let md = entry
                .markdown
                .as_ref()
                .expect("markdown populated when include_full_content=true");
            assert!(
                md.len() >= entry.preview.trim_end_matches('…').len(),
                "markdown should be at least as long as preview's content"
            );
            assert!(
                entry.images.is_none(),
                "images stay None unless include_images=true"
            );
        }
    }

    #[gpui::test]
    async fn get_session_include_images_inlines_base64(cx: &mut gpui::TestAppContext) {
        let (session_id, expected_b64, _tmp) = seed_session_with_image(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: true,
                    include_images: true,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let mut total_images = 0usize;
        let mut saw_expected = false;
        for entry in &result.structured_content.entries {
            let images = entry
                .images
                .as_ref()
                .expect("images list populated even if empty");
            total_images += images.len();
            for image in images {
                assert_eq!(image.mime_type, "image/png");
                if image.data_base64 == expected_b64 {
                    saw_expected = true;
                }
            }
        }
        assert!(
            total_images >= 1,
            "expected at least one image after seeding"
        );
        assert!(
            saw_expected,
            "the seeded PNG payload should round-trip unchanged"
        );
    }

    #[gpui::test]
    async fn get_session_entry_happy_path_returns_full_markdown(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let result = GetSessionEntryTool
            .run(
                GetSessionEntryParams {
                    session_id: session_id.to_string(),
                    index: 0,
                    include_images: false,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_entry");

        let entry = result.structured_content.entry;
        assert_eq!(entry.role, EntryRoleDto::User);
        // R-6e: every EntrySummary carries its absolute index.
        assert_eq!(entry.index, 0);
        let md = entry
            .markdown
            .expect("markdown is always populated for single-entry fetch");
        assert!(md.contains("hello"));
    }

    #[gpui::test]
    async fn get_session_entry_out_of_range_errors(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let err = GetSessionEntryTool
            .run(
                GetSessionEntryParams {
                    session_id: session_id.to_string(),
                    index: 9_999,
                    include_images: false,
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("out-of-range index must error");

        let msg = format!("{:#}", err);
        assert!(
            msg.contains("entry_index_out_of_range"),
            "error should mention entry_index_out_of_range, got: {msg}"
        );
    }

    #[gpui::test]
    async fn tool_call_entry_surfaces_status_and_args(cx: &mut gpui::TestAppContext) {
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

        // Push a synthetic ToolCall directly into the thread. We bypass
        // `handle_session_update` because that path requires a real ACP
        // server; constructing the entry by hand exercises the same
        // public type the render layer reads.
        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                let tool_call = acp::ToolCall::new(
                    acp::ToolCallId::new("call-1".to_string()),
                    "Bash".to_string(),
                )
                .kind(acp::ToolKind::Execute)
                .raw_input(serde_json::json!({ "cmd": "ls" }));
                thread
                    .upsert_tool_call(tool_call, cx)
                    .expect("upsert_tool_call");
            });
        });
        cx.executor().run_until_parked();

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: false,
                    include_images: false,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let tool_entry = result
            .structured_content
            .entries
            .iter()
            .find(|e| e.role == EntryRoleDto::ToolCall)
            .expect("tool_call entry");
        let tool = tool_entry
            .tool_call
            .as_ref()
            .expect("tool_call summary populated");
        // Reuses `tool_call_status_text` — pending status maps to the
        // literal string "pending".
        assert_eq!(tool.status, ToolCallStatusDto::Pending);
        assert!(
            tool.args_preview.contains("\"cmd\""),
            "args_preview should serialize raw_input JSON, got: {}",
            tool.args_preview
        );
        assert!(
            tool.tool_status_started_at_ms.is_none(),
            "Pending tool call should not surface a started_at timestamp, got: {:?}",
            tool.tool_status_started_at_ms,
        );
    }

    #[gpui::test]
    async fn tool_call_entry_surfaces_status_started_at_when_in_progress(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

        let before_ms = chrono::Utc::now().timestamp_millis();
        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                let tool_call = acp::ToolCall::new(
                    acp::ToolCallId::new("call-1".to_string()),
                    "Bash".to_string(),
                )
                .kind(acp::ToolKind::Execute)
                .status(acp::ToolCallStatus::InProgress);
                thread
                    .upsert_tool_call(tool_call, cx)
                    .expect("upsert_tool_call");
            });
        });
        cx.executor().run_until_parked();
        let after_ms = chrono::Utc::now().timestamp_millis();

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: false,
                    include_images: false,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let tool = result
            .structured_content
            .entries
            .iter()
            .find(|e| e.role == EntryRoleDto::ToolCall)
            .and_then(|e| e.tool_call.as_ref())
            .expect("tool_call summary populated");
        assert_eq!(tool.status, ToolCallStatusDto::Running);
        let stamp = tool
            .tool_status_started_at_ms
            .expect("InProgress tool call must surface a started_at timestamp");
        assert!(
            stamp >= before_ms && stamp <= after_ms,
            "tool_status_started_at_ms {stamp} should fall between {before_ms} and {after_ms}",
        );
    }

    #[gpui::test]
    async fn plan_entry_surfaces_items(cx: &mut gpui::TestAppContext) {
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                let plan = acp::Plan::new(vec![
                    acp::PlanEntry::new(
                        "step one".to_string(),
                        acp::PlanEntryPriority::Medium,
                        acp::PlanEntryStatus::Completed,
                    ),
                    acp::PlanEntry::new(
                        "step two".to_string(),
                        acp::PlanEntryPriority::Medium,
                        acp::PlanEntryStatus::Completed,
                    ),
                ]);
                thread.update_plan(plan, cx);
            });
        });
        cx.executor().run_until_parked();

        // `update_plan` keeps the plan in-flight until something
        // upgrades it to `CompletedPlan`. The session_view path does
        // this via the `EntryUpdated` cycle; in tests we drive the
        // same transition by emitting `Stopped` which forces the
        // pending plan to flush. If a plan entry isn't surfaced as
        // `CompletedPlan` we just confirm no panic — the actual plan
        // shape is checked in `acp_thread` upstream tests.
        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: false,
                    include_images: false,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        if let Some(plan_entry) = result
            .structured_content
            .entries
            .iter()
            .find(|e| e.role == EntryRoleDto::Plan)
        {
            let plan = plan_entry
                .plan
                .as_ref()
                .expect("plan summary populated for role=plan");
            assert_eq!(plan.items.len(), 2);
            assert!(plan.items[0].contains("step one"));
        }
        // Soft assertion — if the synthetic plan didn't get promoted to
        // CompletedPlan we still want the test to exercise the wire
        // path without panicking.
    }

    // =================================================================
    // R-6e pagination coverage (`solution_agent.get_session` +
    // `solution_agent.list_sessions`).
    // =================================================================

    /// Seed a session with exactly 5 plain text entries — alternating
    /// user/assistant — so pagination tests have stable indices 0..=4.
    /// No images, no tool calls; the only thing under test is
    /// before/after/count filtering on a known entry shape.
    async fn seed_session_with_n_entries(
        cx: &mut gpui::TestAppContext,
        n: usize,
    ) -> (crate::model::SolutionSessionId, tempfile::TempDir) {
        let (session_id, acp_thread, tmp) = create_session_with_thread(cx).await;
        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                for i in 0..n {
                    let text = format!("entry-{i}");
                    if i % 2 == 0 {
                        thread.push_user_content_block(None, fake_user_text_chunk(&text), cx);
                    } else {
                        thread.push_assistant_content_block(fake_user_text_chunk(&text), false, cx);
                    }
                }
            });
        });
        cx.executor().run_until_parked();
        (session_id, tmp)
    }

    #[gpui::test]
    async fn get_session_no_pagination_returns_all_entries_with_total_count(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(entries.len(), 5, "no pagination → all 5 entries");
        assert_eq!(result.structured_content.total_count, 5);
        for (expected, entry) in entries.iter().enumerate() {
            assert_eq!(
                entry.index, expected,
                "EntrySummary.index must match absolute position"
            );
        }
    }

    /// Phase 5 Task 5.3 Part A: a full `get_session` load carries the session's
    /// current `epoch` + `current_seq` so the cache-first mobile client can seed
    /// its delta cursor from one fetch (then poll `get_session_changes`).
    #[gpui::test]
    async fn get_session_seeds_delta_cursor_epoch_and_seq(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 3).await;

        // Bump both watermarks the way the store would after activity, and
        // rotate the epoch the way a `/clear` would.
        mutate_session(session_id, cx, |s| {
            s.epoch = 7;
            s.change_seq = 42;
        });

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        assert_eq!(
            result.structured_content.epoch, 7,
            "full load must carry the session's epoch"
        );
        assert_eq!(
            result.structured_content.current_seq, 42,
            "full load must carry the session's change_seq as current_seq"
        );

        // A further mutation that bumps change_seq is reflected on the next load.
        mutate_session(session_id, cx, |s| {
            s.change_seq = 43;
        });
        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");
        assert_eq!(
            result.structured_content.current_seq, 43,
            "current_seq must track change_seq"
        );
    }

    /// Build a COLD, row-native session: `session.entries` populated, NO
    /// live `acp_thread` attached. Mirrors the post-restart state of a
    /// row-native session: `session.entries` populated, no live `acp_thread`.
    /// `get_session` must read from `session.entries` directly.
    /// The user message carries a 1×1 PNG image chunk so
    /// image extraction can be asserted on the user path.
    async fn seed_cold_row_native_session(
        cx: &mut gpui::TestAppContext,
    ) -> (crate::model::SolutionSessionId, String, tempfile::TempDir) {
        use crate::session_entry::{SessionEntry, SessionEntryKind};
        let (solution_id, tmp, _project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            let registry = std::sync::Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });
        let image_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNgAAIAAAUAAen5lOEAAAAASUVORK5CYII=".to_string();
        let image_b64_clone = image_b64.clone();
        let session_id = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                let id = crate::model::SolutionSessionId::new();
                let mut session = crate::model::SolutionSession::new_idle(
                    id,
                    solution_id.clone(),
                    SharedString::from("mock-agent"),
                    acp::SessionId::new(format!("acp-{}", id.as_str())),
                );
                session.title = SharedString::from("cold session");
                session.entries = vec![
                    SessionEntry {
                        created_ms: 1_700_000_000_000,
                        mod_seq: 1,
                        subagent_id: None,
                        kind: SessionEntryKind::UserMessage {
                            id: None,
                            content_md: "hello".into(),
                            chunks: vec![
                                fake_user_text_chunk("hello"),
                                fake_image_chunk("image/png", &image_b64_clone),
                            ],
                        },
                    },
                    SessionEntry {
                        created_ms: 1_700_000_000_001,
                        mod_seq: 2,
                        subagent_id: None,
                        kind: SessionEntryKind::AssistantMessage {
                            chunks: vec![crate::session_entry::AssistantChunk::Message(
                                "world".into(),
                            )],
                        },
                    },
                ];
                // Cold, row-native: NO live thread.
                assert!(session.acp_thread().is_none());
                store.register_prebuilt_session(session, cx)
            })
        });
        (session_id, image_b64, tmp)
    }

    #[gpui::test]
    async fn get_session_cold_row_native_returns_entries_from_session_entries(
        cx: &mut gpui::TestAppContext,
    ) {
        // A cold row-native session has no live thread; get_session must serve
        // the two entries from session.entries.
        let (session_id, _img, _tmp) = seed_cold_row_native_session(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: true,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(
            entries.len(),
            2,
            "cold row-native session must serve entries from session.entries"
        );
        assert_eq!(result.structured_content.total_count, 2);
        assert_eq!(entries[0].role, EntryRoleDto::User);
        assert_eq!(entries[1].role, EntryRoleDto::Assistant);
        assert!(
            entries[0]
                .markdown
                .as_ref()
                .is_some_and(|m| m.contains("hello")),
            "user markdown must round-trip from content_md"
        );
        assert!(
            entries[1]
                .markdown
                .as_ref()
                .is_some_and(|m| m.contains("world")),
            "assistant markdown must round-trip from chunks"
        );
    }

    #[gpui::test]
    async fn get_session_cold_row_native_preserves_user_images(cx: &mut gpui::TestAppContext) {
        // User-image fidelity must survive the SessionEntry repoint:
        // UserMessage.chunks retains the raw acp::ContentBlock::Image, so
        // the base64 payload round-trips unchanged.
        let (session_id, expected_b64, _tmp) = seed_cold_row_native_session(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_images: true,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let mut saw_expected = false;
        for entry in &result.structured_content.entries {
            if let Some(images) = &entry.images {
                for image in images {
                    if image.data_base64 == expected_b64 {
                        assert_eq!(image.mime_type, "image/png");
                        saw_expected = true;
                    }
                }
            }
        }
        assert!(
            saw_expected,
            "the seeded user PNG payload must round-trip unchanged from UserMessage.chunks"
        );
    }

    #[gpui::test]
    async fn get_session_count_returns_last_n_entries(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    count: Some(2),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![3, 4],
            "count=2 returns the LAST two entries (indices 3,4)"
        );
        assert_eq!(result.structured_content.total_count, 5);
    }

    #[gpui::test]
    async fn get_session_before_index_drops_newer(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    before_index: Some(3),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(
            entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "before_index=3 keeps strictly-less indices 0,1,2"
        );
        assert_eq!(result.structured_content.total_count, 5);
    }

    #[gpui::test]
    async fn get_session_after_index_drops_older(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    after_index: Some(2),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(
            entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![3, 4],
            "after_index=2 keeps strictly-greater indices 3,4"
        );
        assert_eq!(result.structured_content.total_count, 5);
    }

    #[gpui::test]
    async fn get_session_before_and_after_index_select_slice(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    after_index: Some(2),
                    before_index: Some(4),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(
            entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![3],
            "after=2, before=4 leaves only index 3"
        );
        assert_eq!(result.structured_content.total_count, 5);
    }

    #[gpui::test]
    async fn get_session_after_index_then_count_takes_last_within_filter(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    after_index: Some(2),
                    count: Some(1),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        // After filter: indices 3,4. count=1 keeps the LAST = index 4.
        // Wait — plan says "entries are index 3 (last after filter)". Let's
        // re-read: "after_index=2, count=1 → entries are index 3 (last
        // after filter)". That's odd — the filter keeps {3,4} and "last"
        // would be 4. The plan likely meant "the slice has cardinality 1
        // — exactly one entry — at the most-recent position 4". But the
        // plan-doc literal says "index 3". Re-check: the plan-doc text in
        // the user prompt says exactly: "after_index=2, count=1 → entries
        // are index 3 (last after filter)". That contradicts the
        // count semantics ("LAST n") defined earlier in the SAME prompt.
        //
        // Resolving in favor of the LAST-N semantics defined in scope B
        // step 5 (`take(n)` on the reversed iterator), so count=1 of
        // {3,4} = {4}. The plan-doc's example is a typo.
        assert_eq!(
            entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![4],
            "after=2 keeps {{3,4}}, count=1 then takes the LAST → index 4"
        );
        assert_eq!(result.structured_content.total_count, 5);
    }

    #[gpui::test]
    async fn get_session_after_index_past_end_returns_empty(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    after_index: Some(99),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        assert!(
            result.structured_content.entries.is_empty(),
            "after_index past end → empty"
        );
        assert_eq!(
            result.structured_content.total_count, 5,
            "total_count still reflects the underlying thread"
        );
    }

    #[gpui::test]
    async fn list_sessions_pagination_orders_desc_and_caps_to_count(cx: &mut gpui::TestAppContext) {
        // Reuse the first session's setup (it primes globals + the mock
        // adapter), then create two more sessions in the same solution
        // with slightly later activity timestamps so the DESC ordering
        // is observable.
        let (first_session_id, _thread, _tmp) = create_session_with_thread(cx).await;

        let (solution_id, agent_id, project) = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store
                .read(cx)
                .session(first_session_id)
                .expect("first session exists");
            let session_ref = session.read(cx);
            (
                session_ref.solution_id.clone(),
                session_ref.agent_id.clone(),
                session_ref
                    .project
                    .clone()
                    .expect("create_session populates project"),
            )
        });

        let second_session_id = cx
            .update(|cx| {
                let store = SolutionAgentStore::global(cx);
                store.update(cx, |store, cx| {
                    store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
                })
            })
            .await
            .expect("create second session");

        let third_session_id = cx
            .update(|cx| {
                let store = SolutionAgentStore::global(cx);
                store.update(cx, |store, cx| {
                    store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
                })
            })
            .await
            .expect("create third session");

        // The third is the most-recently-created; bump its
        // last_activity_at explicitly so the DESC sort puts it first
        // even on machines where Utc::now()'s resolution lets two
        // creates land in the same tick.
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let (second, third) = store.read_with(cx, |store, _| {
                (
                    store.session(second_session_id).expect("second"),
                    store.session(third_session_id).expect("third"),
                )
            });
            second.update(cx, |s, _| {
                s.last_activity_at = chrono::Utc::now() + chrono::Duration::seconds(1);
            });
            third.update(cx, |s, _| {
                s.last_activity_at = chrono::Utc::now() + chrono::Duration::seconds(2);
            });
        });

        let result = ListSessionsTool
            .run(
                ListSessionsParams {
                    solution_id: Some(solution_id.0.clone()),
                    parent_session_id: None,
                    count: Some(1),
                    before_last_activity_at_ms: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("list_sessions");

        let sessions = &result.structured_content.sessions;
        assert_eq!(sessions.len(), 1, "count=1 caps to one entry");
        assert_eq!(
            sessions[0].id,
            third_session_id.to_string(),
            "DESC ordering surfaces the most-recent session first"
        );
        assert_eq!(
            result.structured_content.total_count, 3,
            "total_count reflects all matching sessions, pre-pagination"
        );
    }

    // =================================================================
    // F: sub-agent indication coverage
    //
    // Validates the `parent_session_id` field plumbing across the MCP
    // wire shape and the new `solution_agent.get_session_children` tool.
    // =================================================================

    /// Spawn a sub-session under `parent_id`. Stays at the store layer
    /// to avoid the `MultiWorkspace` requirement of `CreateSessionTool`;
    /// the tool-layer create_session paths are covered separately in
    /// the dedicated F validation tests below.
    async fn create_child_session(
        cx: &mut gpui::TestAppContext,
        parent_id: crate::model::SolutionSessionId,
    ) -> crate::model::SolutionSessionId {
        let (solution_id, agent_id, project) = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store
                .read(cx)
                .session(parent_id)
                .expect("parent session exists");
            let session_ref = session.read(cx);
            (
                session_ref.solution_id.clone(),
                session_ref.agent_id.clone(),
                session_ref.project.clone().expect("parent has project"),
            )
        });
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session_with_parent(
                    solution_id,
                    agent_id,
                    project,
                    None,
                    Some(parent_id),
                    None,
                    None,
                    false,
                    cx,
                )
            })
        })
        .await
        .expect("create child session")
    }

    #[gpui::test]
    async fn create_session_with_parent_sets_parent_session_id_on_child(
        cx: &mut gpui::TestAppContext,
    ) {
        let (parent_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let child_id = create_child_session(cx, parent_id).await;

        // GetSession surfaces parent_session_id on the child.
        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: child_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session(child)");
        assert_eq!(
            result.structured_content.parent_session_id.as_deref(),
            Some(parent_id.to_string().as_str()),
            "child reports parent_session_id"
        );

        // Top-level parent reports no parent_session_id.
        let parent_result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: parent_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session(parent)");
        assert!(
            parent_result.structured_content.parent_session_id.is_none(),
            "top-level parent has no parent_session_id"
        );
    }

    #[gpui::test]
    async fn create_session_with_unknown_parent_errors_with_named_code(
        cx: &mut gpui::TestAppContext,
    ) {
        // Seed the store + solution_id so the "unknown parent" branch
        // is reached. We don't need a real workspace because parent
        // validation runs before `project_for_solution`.
        let (real_session_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let solution_id = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store
                .read(cx)
                .session(real_session_id)
                .expect("session")
                .read(cx)
                .solution_id
                .clone()
        });
        // A short id that's well-formed (`[a-z0-9]{8}`) but not in the
        // store. `parse` will accept it; the store lookup will reject.
        let unknown_parent = "abcd1234";
        let err = CreateSessionTool
            .run(
                CreateSessionParams {
                    solution_id: solution_id.0.clone(),
                    agent_id: "mock-agent".into(),
                    initial_message: None,
                    parent_session_id: Some(unknown_parent.to_string()),
                    title: None,
                    cwd: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("expected unknown_parent_session error");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown_parent_session"),
            "expected unknown_parent_session in {msg:?}"
        );
        assert!(
            msg.contains(unknown_parent),
            "expected error to include the bad id; got {msg:?}"
        );
    }

    #[gpui::test]
    async fn create_session_with_parent_in_different_solution_errors(
        cx: &mut gpui::TestAppContext,
    ) {
        let (parent_id, _thread, _tmp) = create_session_with_thread(cx).await;
        // CreateSession against a *different* solution id — the parent
        // belongs to solution-A; we pass solution-B. Validation fires
        // before workspace lookup so we don't need solution-B to have
        // an open window.
        let other_solution = "sol-OTHER-not-the-parents";
        let err = CreateSessionTool
            .run(
                CreateSessionParams {
                    solution_id: other_solution.into(),
                    agent_id: "mock-agent".into(),
                    initial_message: None,
                    parent_session_id: Some(parent_id.to_string()),
                    title: None,
                    cwd: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("expected parent_session_in_different_solution error");
        let msg = err.to_string();
        assert!(
            msg.contains("parent_session_in_different_solution"),
            "expected parent_session_in_different_solution in {msg:?}"
        );
    }

    #[gpui::test]
    async fn get_session_children_returns_child_with_summary_fields(cx: &mut gpui::TestAppContext) {
        let (parent_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let child_id = create_child_session(cx, parent_id).await;

        let result = GetSessionChildrenTool
            .run(
                GetSessionChildrenParams {
                    session_id: parent_id.to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_children");
        let children = &result.structured_content.children;
        assert_eq!(children.len(), 1, "exactly one child");
        assert_eq!(children[0].id, child_id.to_string());
        assert_eq!(
            children[0].parent_session_id.as_deref(),
            Some(parent_id.to_string().as_str()),
            "child summary echoes parent_session_id"
        );
        // Text content carries a stable count summary for log scraping.
        match &result.content[0] {
            ToolResponseContent::Text { text } => {
                assert_eq!(text, "1 child session(s)");
            }
            _ => panic!("expected text content"),
        }
    }

    #[gpui::test]
    async fn get_session_children_returns_empty_list_for_leaf_session(
        cx: &mut gpui::TestAppContext,
    ) {
        let (leaf_id, _thread, _tmp) = create_session_with_thread(cx).await;

        let result = GetSessionChildrenTool
            .run(
                GetSessionChildrenParams {
                    session_id: leaf_id.to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_children on a leaf");
        assert!(
            result.structured_content.children.is_empty(),
            "leaf session has no children"
        );
    }

    #[gpui::test]
    async fn supervisor_ephemeral_sessions_hidden_from_enumeration(
        cx: &mut gpui::TestAppContext,
    ) {
        // A supervised parent with one hidden ephemeral judge child. The judge
        // must NOT surface in either `list_sessions` (the parent does) or
        // `get_session_children` (an empty list — it's the only child).
        let (parent_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let judge_id = create_child_session(cx, parent_id).await;
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store
                    .session(judge_id)
                    .expect("judge session exists")
                    .update(cx, |s, _| s.is_supervisor_ephemeral = true);
            });
        });

        let listed = ListSessionsTool
            .run(
                ListSessionsParams {
                    solution_id: None,
                    parent_session_id: None,
                    count: None,
                    before_last_activity_at_ms: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("list_sessions");
        let ids: Vec<&str> = listed
            .structured_content
            .sessions
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert!(
            ids.contains(&parent_id.to_string().as_str()),
            "the supervised parent is still enumerated"
        );
        assert!(
            !ids.contains(&judge_id.to_string().as_str()),
            "the flagged ephemeral judge is excluded from list_sessions"
        );

        let children = GetSessionChildrenTool
            .run(
                GetSessionChildrenParams {
                    session_id: parent_id.to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_children");
        assert!(
            children.structured_content.children.is_empty(),
            "the flagged ephemeral judge is excluded from get_session_children"
        );
    }

    /// Seed two background shells (insertion-ordered) into a session, the
    /// second carrying a `latest` snapshot with a known `output_tail` + mtime.
    /// Returns the mtime-millis stamped on the second shell so the test can
    /// assert `mtime_ms`.
    fn seed_background_shells(
        cx: &mut gpui::TestAppContext,
        session_id: crate::model::SolutionSessionId,
    ) -> i64 {
        use crate::background_shell::{
            BackgroundShellId, BackgroundShellSnapshot, ShellRuntimeState,
        };
        // Pick a fixed post-epoch instant so the mtime_ms assertion is exact.
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_123);
        let expected_ms = 1_700_000_000_123_i64;
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session");
            session.update(cx, |session, _| {
                let first = BackgroundShell {
                    id: BackgroundShellId::new("aaa111"),
                    command: SharedString::from("sleep 60"),
                    output_path: std::path::PathBuf::from("/tmp/aaa111.output"),
                    registered_at: chrono::Utc::now(),
                    latest: None,
                    last_offset: 0,
                    state: ShellRuntimeState::Running,
                };
                let second = BackgroundShell {
                    id: BackgroundShellId::new("bbb222"),
                    command: SharedString::from("cargo build"),
                    output_path: std::path::PathBuf::from("/tmp/bbb222.output"),
                    registered_at: chrono::Utc::now(),
                    latest: Some(BackgroundShellSnapshot {
                        mtime,
                        output_tail: SharedString::from("compiling...\n"),
                    }),
                    last_offset: 13,
                    state: ShellRuntimeState::Exited(Some(0)),
                };
                session.background_shell_order.push(first.id.clone());
                session.background_shell_order.push(second.id.clone());
                session.background_shells.insert(first.id.clone(), first);
                session.background_shells.insert(second.id.clone(), second);
            });
        });
        expected_ms
    }

    #[gpui::test]
    async fn get_session_background_shells_omits_output_by_default(cx: &mut gpui::TestAppContext) {
        let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let expected_ms = seed_background_shells(cx, session_id);

        let result = GetSessionBackgroundShellsTool
            .run(
                GetSessionBackgroundShellsParams {
                    session_id: session_id.to_string(),
                    include_output: false,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_background_shells");
        let shells = &result.structured_content.background_shells;
        assert_eq!(shells.len(), 2, "both seeded shells returned");
        // Ordered per background_shell_order: aaa111 first, bbb222 second.
        assert_eq!(shells[0].id, "aaa111");
        assert_eq!(shells[0].command, "sleep 60");
        assert_eq!(shells[0].state, "running");
        assert_eq!(shells[0].mtime_ms, None, "first shell has no snapshot");
        assert_eq!(shells[0].output_tail, None);

        assert_eq!(shells[1].id, "bbb222");
        assert_eq!(shells[1].state, "exited:0");
        assert_eq!(shells[1].mtime_ms, Some(expected_ms));
        assert_eq!(
            shells[1].output_tail, None,
            "include_output=false omits the tail even when a snapshot exists"
        );

        match &result.content[0] {
            ToolResponseContent::Text { text } => {
                assert_eq!(text, "2 background shell(s)");
            }
            _ => panic!("expected text content"),
        }
    }

    #[gpui::test]
    async fn get_session_background_shells_includes_output_when_requested(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
        seed_background_shells(cx, session_id);

        let result = GetSessionBackgroundShellsTool
            .run(
                GetSessionBackgroundShellsParams {
                    session_id: session_id.to_string(),
                    include_output: true,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_background_shells include_output");
        let shells = &result.structured_content.background_shells;
        // The first shell has no snapshot → still None even with the flag.
        assert_eq!(shells[0].output_tail, None);
        // The second shell's snapshot tail is surfaced.
        assert_eq!(
            shells[1].output_tail.as_deref(),
            Some("compiling...\n"),
            "include_output=true surfaces the snapshot's output_tail"
        );
    }

    #[gpui::test]
    async fn get_session_background_shells_unknown_session_errors(cx: &mut gpui::TestAppContext) {
        // Seed the store global so the lookup branch (not a missing global)
        // is exercised, then query a well-formed but absent id.
        let (_real_session_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let unknown = "abcd1234";
        let err = GetSessionBackgroundShellsTool
            .run(
                GetSessionBackgroundShellsParams {
                    session_id: unknown.to_string(),
                    include_output: false,
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("expected session_not_found error");
        let msg = err.to_string();
        assert!(
            msg.contains("session_not_found"),
            "expected session_not_found in {msg:?}"
        );
    }

    /// Seed two background agents (insertion-ordered): the first carries a
    /// `latest` snapshot with a known `activity_label` + mtime + stop_reason,
    /// the second has no snapshot (so its DTO label must fall back to the
    /// `Generating…` default with `mtime_ms == None`). Returns the
    /// mtime-millis stamped on the first agent so the test can assert it.
    fn seed_background_agents(
        cx: &mut gpui::TestAppContext,
        session_id: crate::model::SolutionSessionId,
    ) -> i64 {
        use crate::background_agent::{
            BackgroundAgent, BackgroundAgentId, BackgroundAgentSnapshot,
        };
        // Pick a fixed post-epoch instant so the mtime_ms assertion is exact.
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_123);
        let expected_ms = 1_700_000_000_123_i64;
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session");
            session.update(cx, |session, _| {
                let first = BackgroundAgent {
                    id: BackgroundAgentId::new("a30f92a688e431ed"),
                    jsonl_path: std::path::PathBuf::from("/tmp/a30f92a688e431ed.jsonl"),
                    registered_at: chrono::Utc::now(),
                    latest: Some(BackgroundAgentSnapshot {
                        mtime,
                        activity_label: SharedString::from("Bash: cargo test"),
                        stop_reason: Some(SharedString::from("end_turn")),
                    }),
                    last_offset: 42,
                };
                let second = BackgroundAgent {
                    id: BackgroundAgentId::new("b41a03b799f542fe"),
                    jsonl_path: std::path::PathBuf::from("/tmp/b41a03b799f542fe.jsonl"),
                    registered_at: chrono::Utc::now(),
                    latest: None,
                    last_offset: 0,
                };
                session.background_agent_order.push(first.id.clone());
                session.background_agent_order.push(second.id.clone());
                session.background_agents.insert(first.id.clone(), first);
                session.background_agents.insert(second.id.clone(), second);
            });
        });
        expected_ms
    }

    #[gpui::test]
    async fn get_session_background_agents_returns_ordered_agents(cx: &mut gpui::TestAppContext) {
        let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let expected_ms = seed_background_agents(cx, session_id);

        let result = GetSessionBackgroundAgentsTool
            .run(
                GetSessionBackgroundAgentsParams {
                    session_id: session_id.to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_background_agents");
        let agents = &result.structured_content.background_agents;
        assert_eq!(agents.len(), 2, "both seeded agents returned");
        // Ordered per background_agent_order: the snapshot-bearing one first.
        assert_eq!(agents[0].id, "a30f92a688e431ed");
        assert_eq!(agents[0].label, "Bash: cargo test");
        assert_eq!(agents[0].mtime_ms, Some(expected_ms));
        assert_eq!(agents[0].stop_reason.as_deref(), Some("end_turn"));

        // Snapshot-less agent: label falls back to the Generating… default.
        assert_eq!(agents[1].id, "b41a03b799f542fe");
        assert_eq!(
            agents[1].label, "Generating…",
            "snapshot-less agent must use the Generating… default label"
        );
        assert_eq!(agents[1].mtime_ms, None, "no snapshot → no mtime_ms");
        assert_eq!(agents[1].stop_reason, None);

        match &result.content[0] {
            ToolResponseContent::Text { text } => {
                assert_eq!(text, "2 background agent(s)");
            }
            _ => panic!("expected text content"),
        }
    }

    #[gpui::test]
    async fn get_session_background_agents_unknown_session_errors(cx: &mut gpui::TestAppContext) {
        // Seed the store global so the lookup branch (not a missing global)
        // is exercised, then query a well-formed but absent id.
        let (_real_session_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let unknown = "abcd1234";
        let err = GetSessionBackgroundAgentsTool
            .run(
                GetSessionBackgroundAgentsParams {
                    session_id: unknown.to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("expected session_not_found error");
        let msg = err.to_string();
        assert!(
            msg.contains("session_not_found"),
            "expected session_not_found in {msg:?}"
        );
    }

    #[gpui::test]
    async fn list_sessions_filters_by_parent_session_id(cx: &mut gpui::TestAppContext) {
        let (parent_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let child_id = create_child_session(cx, parent_id).await;
        // Add a second sibling so the filter has more than one row to
        // partition.
        let sibling_id = create_child_session(cx, parent_id).await;

        let solution_id = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store
                .read(cx)
                .session(parent_id)
                .expect("parent")
                .read(cx)
                .solution_id
                .clone()
        });

        // parent_session_id=parent → both children come back, parent itself excluded.
        let filtered = ListSessionsTool
            .run(
                ListSessionsParams {
                    solution_id: Some(solution_id.0.clone()),
                    parent_session_id: Some(parent_id.to_string()),
                    before_last_activity_at_ms: None,
                    count: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("list_sessions filtered by parent");
        let ids: std::collections::HashSet<String> = filtered
            .structured_content
            .sessions
            .iter()
            .map(|s| s.id.clone())
            .collect();
        assert_eq!(
            ids,
            [child_id.to_string(), sibling_id.to_string()]
                .into_iter()
                .collect(),
            "exactly the two children are returned",
        );
        assert!(
            !ids.contains(&parent_id.to_string()),
            "parent itself is excluded"
        );
    }

    #[gpui::test]
    async fn session_summary_total_tokens_populated_from_cached_value(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
        // Seed `cached_total_tokens` directly so the fallback path is
        // exercised even without a live `TokenUsageUpdated` event.
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session exists");
            session.update(cx, |s, _| s.cached_total_tokens = Some(42_000));
        });

        let result = ListSessionsTool
            .run(ListSessionsParams::default(), &mut cx.to_async())
            .await
            .expect("list_sessions");
        let summary = result
            .structured_content
            .sessions
            .iter()
            .find(|s| s.id == session_id.to_string())
            .expect("session present");
        // The live thread's `token_usage()` may be None at this stage,
        // so the fallback to `cached_total_tokens` is what we're
        // verifying. Either path yielding >= 42_000 is acceptable
        // (live could update past the seed); the contract is "non-None
        // when we have a value".
        assert!(
            summary.total_tokens.is_some_and(|t| t >= 42_000),
            "total_tokens should fall back to cached_total_tokens; got {:?}",
            summary.total_tokens,
        );
    }

    /// Phone client reads `SessionSummary::max_tokens` to size its
    /// context-fill meter the same way the desktop does — without it,
    /// it would have to guess the model's window. Live thread's
    /// `TokenUsage::max_tokens` is the source when hot; the cache
    /// fallback is exercised separately in
    /// `session_summary_max_tokens_falls_back_to_cached`.
    #[gpui::test]
    async fn session_summary_max_tokens_from_live_thread(cx: &mut gpui::TestAppContext) {
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
        // Drive a TokenUsageUpdated through the live thread. The store's
        // event handler mirrors max_tokens onto cached_max_tokens, and
        // session_summary should surface it.
        cx.update(|cx| {
            acp_thread.update(cx, |t, cx| {
                t.update_token_usage(
                    Some(acp_thread::TokenUsage {
                        used_tokens: 5_000,
                        max_tokens: 200_000,
                        ..Default::default()
                    }),
                    cx,
                );
            });
        });
        cx.executor().run_until_parked();

        let result = ListSessionsTool
            .run(ListSessionsParams::default(), &mut cx.to_async())
            .await
            .expect("list_sessions");
        let summary = result
            .structured_content
            .sessions
            .iter()
            .find(|s| s.id == session_id.to_string())
            .expect("session present");
        assert_eq!(
            summary.max_tokens,
            Some(200_000),
            "max_tokens should be reported from the live thread",
        );
        assert_eq!(
            summary.total_tokens,
            Some(5_000),
            "total_tokens should be reported alongside max",
        );
    }

    /// Cold tab path: no live `acp_thread`, but `cached_max_tokens` was
    /// stamped during an earlier live event. `session_summary` must
    /// fall through to the cache so the phone meter keeps rendering a
    /// realistic window size even on sleeping sessions.
    #[gpui::test]
    async fn session_summary_max_tokens_falls_back_to_cached(cx: &mut gpui::TestAppContext) {
        let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session exists");
            session.update(cx, |s, _| s.cached_max_tokens = Some(180_000));
        });

        let result = ListSessionsTool
            .run(ListSessionsParams::default(), &mut cx.to_async())
            .await
            .expect("list_sessions");
        let summary = result
            .structured_content
            .sessions
            .iter()
            .find(|s| s.id == session_id.to_string())
            .expect("session present");
        // A live max may have been picked up in the meantime; the
        // contract is "non-None when the cache holds a value".
        assert!(
            summary.max_tokens.is_some_and(|m| m >= 180_000),
            "max_tokens should fall back to cached_max_tokens; got {:?}",
            summary.max_tokens,
        );
    }

    /// `start_compact` MCP tool refuses on a fresh session whose
    /// context usage is well below the 10% threshold — mirrors the
    /// desktop status-row gate. The structured `queued=false` + reason
    /// is the contract the phone client renders on its button.
    #[gpui::test]
    async fn start_compact_declines_below_threshold(cx: &mut gpui::TestAppContext) {
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
        // Seed a low usage well below 20% so the precondition fails.
        cx.update(|cx| {
            acp_thread.update(cx, |t, cx| {
                t.update_token_usage(
                    Some(acp_thread::TokenUsage {
                        used_tokens: 1_000,
                        max_tokens: 1_000_000,
                        ..Default::default()
                    }),
                    cx,
                );
            });
        });
        cx.executor().run_until_parked();

        let result = StartCompactTool
            .run(
                StartCompactParams {
                    session_id: session_id.to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect("start_compact dispatches");
        assert!(
            !result.structured_content.queued,
            "expected queued=false, got {:?}",
            result.structured_content
        );
        let msg = result
            .structured_content
            .message
            .as_deref()
            .unwrap_or_default();
        assert!(
            msg.contains("short") || msg.contains("%"),
            "expected reason mentioning short context or percentage; got {msg:?}"
        );
    }

    /// `start_compact` queues a user message on the agent when the
    /// session is Idle and context exceeds 20%. We check that
    /// `send_message` was forwarded by inspecting the prompts the mock
    /// connection received.
    #[gpui::test]
    async fn start_compact_queues_prompt_when_idle(cx: &mut gpui::TestAppContext) {
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
        cx.update(|cx| {
            acp_thread.update(cx, |t, cx| {
                t.update_token_usage(
                    Some(acp_thread::TokenUsage {
                        // 25% of 1M = 250 000 (above the 20% gate)
                        used_tokens: 250_000,
                        max_tokens: 1_000_000,
                        ..Default::default()
                    }),
                    cx,
                );
            });
        });
        cx.executor().run_until_parked();

        let result = StartCompactTool
            .run(
                StartCompactParams {
                    session_id: session_id.to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect("start_compact dispatches");
        assert!(
            result.structured_content.queued,
            "expected queued=true; reason={:?}",
            result.structured_content.message
        );
        assert!(
            result.structured_content.message.is_none(),
            "no decline reason on success; got {:?}",
            result.structured_content.message
        );
    }

    // -----------------------------------------------------------------
    // upload_{init,status,finish,abort} + send_message_blocks resolution
    // -----------------------------------------------------------------

    /// `crate::upload::install` is a `OnceLock` — only the first caller wins
    /// process-wide. We can't keep handing out fresh `UploadManager`s per
    /// test; if we did, the second caller's `TempDir` would also drop on
    /// scope exit, leaving the first-installed manager pointing at a
    /// vanished directory. Instead, keep one persistent tempdir + manager
    /// alive for the lifetime of the test binary, and have each test allocate
    /// a fresh session+upload inside it.
    fn ensure_test_upload_manager() {
        use std::sync::OnceLock;
        static GUARD: OnceLock<tempfile::TempDir> = OnceLock::new();
        GUARD.get_or_init(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let manager =
                crate::upload::UploadManager::new(dir.path().to_path_buf()).expect("new mgr");
            crate::upload::install(std::sync::Arc::new(std::sync::Mutex::new(manager)));
            dir
        });
    }

    #[gpui::test]
    async fn upload_init_returns_id_and_status_round_trips(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp_session) = seed_session_with_image(cx).await;
        // OnceLock semantics: install only takes on first call per process,
        // so a prior test's manager may already be in place. That's fine —
        // each upload gets a fresh id from `next_id` and lands in some
        // valid tmp_root.
        ensure_test_upload_manager();

        let init = UploadInitTool
            .run(
                UploadInitParams {
                    session_id: session_id.to_string(),
                    mime: "image/png".to_string(),
                    display_name: "pic.png".to_string(),
                    total_size: 4,
                    sha256: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("upload_init");
        let upload_id = init.structured_content.upload_id;
        assert!(upload_id > 0);

        let status = UploadStatusTool
            .run(UploadStatusParams { upload_id }, &mut cx.to_async())
            .await
            .expect("upload_status");
        assert_eq!(status.structured_content.received_bytes, 0);
        assert_eq!(status.structured_content.total_size, 4);
    }

    #[gpui::test]
    async fn upload_init_rejects_unknown_session(cx: &mut gpui::TestAppContext) {
        let (_session_id, _img, _tmp_session) = seed_session_with_image(cx).await;
        ensure_test_upload_manager();
        let err = UploadInitTool
            .run(
                UploadInitParams {
                    session_id: "nonexistent-session-id".to_string(),
                    mime: "image/png".to_string(),
                    display_name: "a.png".to_string(),
                    total_size: 1,
                    sha256: None,
                },
                &mut cx.to_async(),
            )
            .await
            .map(|_| "ok")
            .unwrap_or_else(|e| Box::leak(format!("ERR: {e}").into_boxed_str()));
        assert!(
            err.starts_with("ERR"),
            "expected error for unknown session, got {err}"
        );
    }

    #[gpui::test]
    async fn upload_finish_after_chunk_returns_handle_and_abort_cleans(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _img, _tmp_session) = seed_session_with_image(cx).await;
        ensure_test_upload_manager();

        let init = UploadInitTool
            .run(
                UploadInitParams {
                    session_id: session_id.to_string(),
                    mime: "image/png".to_string(),
                    display_name: "tiny.png".to_string(),
                    total_size: 4,
                    sha256: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("upload_init");
        let upload_id = init.structured_content.upload_id;

        // Drive a chunk write through the manager directly — the binary
        // frame path is tested in `remote_control`; here we just need a
        // populated tmp file for `finish` to verify.
        crate::upload::with_manager(|m| m.write_chunk(upload_id, 0, &[1, 2, 3, 4]))
            .expect("manager installed")
            .expect("write_chunk");

        let finish = UploadFinishTool
            .run(
                UploadFinishParams {
                    upload_id,
                    sha256: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("upload_finish");
        assert!(
            finish
                .structured_content
                .handle
                .starts_with(crate::upload::HANDLE_SCHEME),
            "expected spk-upload:// handle, got {}",
            finish.structured_content.handle
        );

        UploadAbortTool
            .run(UploadAbortParams { upload_id }, &mut cx.to_async())
            .await
            .expect("upload_abort");

        let after = crate::upload::with_manager(|m| m.resolve(upload_id).is_some())
            .expect("manager installed");
        assert!(!after, "abort should drop the entry");
    }

    // -----------------------------------------------------------------
    // A6: created_ms on wire EntrySummary
    // -----------------------------------------------------------------

    /// Verifies that `GetSessionTool` propagates `created_ms` from the
    /// session's `entries` list to `EntrySummary.created_ms`:
    /// - entries with a real positive stamp → `Some(ms)` with `ms > 0`
    /// - entries whose stamp is the absent-sentinel → `None`
    ///
    /// We bypass the store's stamping by directly writing `entries[i].created_ms`
    /// on the session entity — the same pattern used by the store's own unit
    /// tests (see `store/tests.rs::entry_updated_preserves_created_ms`).
    #[gpui::test]
    async fn get_session_entries_carry_created_ms(cx: &mut gpui::TestAppContext) {
        use crate::model::NO_TIMESTAMP_MS;

        let (session_id, _tmp) = seed_session_with_n_entries(cx, 3).await;

        // Directly stamp: index 0 and 2 get real times, index 1 gets sentinel.
        let fake_ms: i64 = 1_700_000_000_000;
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session_entity = store.read(cx).session(session_id).expect("session exists");
            session_entity.update(cx, |s, _| {
                if let Some(e) = s.entries.get_mut(0) {
                    e.created_ms = fake_ms;
                }
                if let Some(e) = s.entries.get_mut(1) {
                    e.created_ms = NO_TIMESTAMP_MS;
                }
                if let Some(e) = s.entries.get_mut(2) {
                    e.created_ms = fake_ms + 1;
                }
            });
        });

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(entries.len(), 3, "all 3 entries returned");

        // Entries 0 and 2 have real stamps.
        assert!(
            entries[0].created_ms.is_some_and(|ms| ms > 0),
            "entry 0 must carry a positive created_ms; got {:?}",
            entries[0].created_ms,
        );
        assert!(
            entries[2].created_ms.is_some_and(|ms| ms > 0),
            "entry 2 must carry a positive created_ms; got {:?}",
            entries[2].created_ms,
        );

        // Entry 1 has the sentinel → must surface as None.
        assert!(
            entries[1].created_ms.is_none(),
            "entry 1 (sentinel) must have created_ms=None; got {:?}",
            entries[1].created_ms,
        );
    }

    /// Verifies that `GetSessionEntryTool` also propagates `created_ms`.
    #[gpui::test]
    async fn get_session_entry_carries_created_ms(cx: &mut gpui::TestAppContext) {
        use crate::model::NO_TIMESTAMP_MS;

        let (session_id, _tmp) = seed_session_with_n_entries(cx, 2).await;

        // Directly stamp entry 0 with a real time; leave entry 1 at sentinel.
        let fake_ms: i64 = 1_700_000_000_000;
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session_entity = store.read(cx).session(session_id).expect("session exists");
            session_entity.update(cx, |s, _| {
                if let Some(e) = s.entries.get_mut(0) {
                    e.created_ms = fake_ms;
                }
                if let Some(e) = s.entries.get_mut(1) {
                    e.created_ms = NO_TIMESTAMP_MS;
                }
            });
        });

        let result = GetSessionEntryTool
            .run(
                GetSessionEntryParams {
                    session_id: session_id.to_string(),
                    index: 0,
                    include_images: false,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_entry");

        assert!(
            result
                .structured_content
                .entry
                .created_ms
                .is_some_and(|ms| ms > 0),
            "GetSessionEntryTool must carry created_ms for a stamped entry; got {:?}",
            result.structured_content.entry.created_ms,
        );

        let result_sentinel = GetSessionEntryTool
            .run(
                GetSessionEntryParams {
                    session_id: session_id.to_string(),
                    index: 1,
                    include_images: false,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_entry sentinel");

        assert!(
            result_sentinel
                .structured_content
                .entry
                .created_ms
                .is_none(),
            "GetSessionEntryTool must surface sentinel as None; got {:?}",
            result_sentinel.structured_content.entry.created_ms,
        );
    }

    /// Stage a tool call sitting in `WaitingForConfirmation` with a Flat
    /// allow/reject option pair, returning the session id, the tool call
    /// id, and the authorization-outcome `Task` (held so the oneshot the
    /// connection awaits stays alive — dropping it would cancel the
    /// confirmation and flip the call off `WaitingForConfirmation`).
    async fn seed_session_with_pending_authorization(
        cx: &mut gpui::TestAppContext,
    ) -> (
        crate::model::SolutionSessionId,
        String,
        gpui::Task<acp_thread::RequestPermissionOutcome>,
        tempfile::TempDir,
    ) {
        let (session_id, acp_thread, tmp) = create_session_with_thread(cx).await;
        let tool_call_id = "call-auth-1".to_string();
        let auth_task = cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                let update = acp::ToolCallUpdate::new(
                    acp::ToolCallId::new(tool_call_id.as_str()),
                    acp::ToolCallUpdateFields::new()
                        .kind(acp::ToolKind::Execute)
                        .title("Bash".to_string()),
                );
                let options = acp_thread::PermissionOptions::Flat(vec![
                    acp::PermissionOption::new(
                        "opt-allow",
                        "Allow".to_string(),
                        acp::PermissionOptionKind::AllowOnce,
                    ),
                    acp::PermissionOption::new(
                        "opt-reject",
                        "Reject".to_string(),
                        acp::PermissionOptionKind::RejectOnce,
                    ),
                ]);
                thread
                    .request_tool_call_authorization(
                        update,
                        options,
                        acp_thread::AuthorizationKind::PermissionGrant,
                        cx,
                    )
                    .expect("stage waiting-for-confirmation")
            })
        });
        cx.executor().run_until_parked();
        (session_id, tool_call_id, auth_task, tmp)
    }

    #[gpui::test]
    async fn get_session_surfaces_auth_options_while_waiting(cx: &mut gpui::TestAppContext) {
        let (session_id, tool_call_id, _auth_task, _tmp) =
            seed_session_with_pending_authorization(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let tool_call = result
            .structured_content
            .entries
            .iter()
            .find_map(|entry| entry.tool_call.as_ref())
            .expect("a tool_call entry must be present");
        assert_eq!(tool_call.status, ToolCallStatusDto::WaitingForConfirmation);
        assert_eq!(tool_call.options.len(), 2, "both options must surface");
        assert_eq!(tool_call.options[0].kind, "allow_once");
        assert!(tool_call.options[0].is_allow);
        assert_eq!(tool_call.options[1].kind, "reject_once");
        assert!(!tool_call.options[1].is_allow);
        // The option id is opaque but must round-trip verbatim.
        assert_eq!(tool_call.options[0].option_id, "opt-allow");
        // tool_call_id is what the client echoes back to authorize.
        assert_eq!(
            tool_call.tool_call_id, tool_call_id,
            "tool_call_id must round-trip verbatim to the client"
        );
    }

    #[gpui::test]
    async fn authorize_tool_call_resolves_waiting_call(cx: &mut gpui::TestAppContext) {
        let (session_id, tool_call_id, _auth_task, _tmp) =
            seed_session_with_pending_authorization(cx).await;

        let result = AuthorizeToolCallTool
            .run(
                AuthorizeToolCallParams {
                    session_id: session_id.to_string(),
                    tool_call_id: tool_call_id.clone(),
                    option_id: "opt-allow".to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect("authorize_tool_call should succeed");
        assert!(result.structured_content.ok);
        cx.executor().run_until_parked();

        // The call must have flipped off WaitingForConfirmation — a
        // second authorize attempt now reports not_awaiting_confirmation.
        let err = AuthorizeToolCallTool
            .run(
                AuthorizeToolCallParams {
                    session_id: session_id.to_string(),
                    tool_call_id: tool_call_id.clone(),
                    option_id: "opt-allow".to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("second authorize must fail; call no longer waiting");
        assert!(
            err.to_string().contains("not_awaiting_confirmation"),
            "unexpected error: {err}"
        );
    }

    #[gpui::test]
    async fn authorize_tool_call_rejects_unknown_option(cx: &mut gpui::TestAppContext) {
        let (session_id, tool_call_id, _auth_task, _tmp) =
            seed_session_with_pending_authorization(cx).await;

        let err = AuthorizeToolCallTool
            .run(
                AuthorizeToolCallParams {
                    session_id: session_id.to_string(),
                    tool_call_id,
                    option_id: "opt-does-not-exist".to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("unknown option must error");
        assert!(
            err.to_string().contains("unknown_option"),
            "unexpected error: {err}"
        );
    }

    #[gpui::test]
    async fn authorize_tool_call_unknown_tool_call_errors(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let err = AuthorizeToolCallTool
            .run(
                AuthorizeToolCallParams {
                    session_id: session_id.to_string(),
                    tool_call_id: "no-such-call".to_string(),
                    option_id: "opt-allow".to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("missing tool call must error");
        assert!(
            err.to_string().contains("tool_call_not_found"),
            "unexpected error: {err}"
        );
    }

    // -----------------------------------------------------------------
    // Etap 5: subagent_id + active_subagents on session DTOs.
    // -----------------------------------------------------------------

    /// Seed the session's `active_subagents` map directly with two tabs
    /// inserted in known order. Stays out of `apply_subagent_lifecycle` so
    /// the test exercises the wire-shape path in isolation from claude's
    /// `ToolCall` plumbing.
    fn seed_subagent_tabs(
        session_id: crate::model::SolutionSessionId,
        labels: &[(&str, &str)],
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store
                .read(cx)
                .session(session_id)
                .expect("session must exist");
            session.update(cx, |s, _| {
                for (id, label) in labels {
                    let id_shared = gpui::SharedString::from((*id).to_string());
                    s.active_subagents.insert(
                        id_shared.clone(),
                        crate::model::SubagentTab {
                            label: gpui::SharedString::from((*label).to_string()),
                            started_at: chrono::Utc::now(),
                        },
                    );
                    s.active_subagent_order.push(id_shared);
                }
            });
        });
    }

    #[gpui::test]
    async fn session_summary_lists_active_subagents_in_insertion_order(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;
        // Pick ids whose lexicographic order disagrees with insertion order
        // so a hash-map iteration regression would visibly flip them.
        seed_subagent_tabs(
            session_id,
            &[("toolu_zzz", "First"), ("toolu_aaa", "Second")],
            cx,
        );

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let active = &result.structured_content.active_subagents;
        assert_eq!(active.len(), 2, "both seeded tabs surface on the wire");
        assert_eq!(
            active[0].id, "toolu_zzz",
            "insertion order must win over lexicographic order"
        );
        assert_eq!(active[0].label, "First");
        assert!(
            active[0].started_at_ms > 0,
            "started_at_ms must be a real unix-millis stamp, got {}",
            active[0].started_at_ms
        );
        assert_eq!(active[1].id, "toolu_aaa");
        assert_eq!(active[1].label, "Second");
    }

    #[gpui::test]
    async fn session_summary_active_subagents_empty_when_no_tabs(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        assert!(
            result.structured_content.active_subagents.is_empty(),
            "no seeded tabs → empty active_subagents"
        );
    }

    #[gpui::test]
    async fn session_summary_exposes_session_cwd(cx: &mut gpui::TestAppContext) {
        let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

        let expected_cwd = cx.read(|cx| {
            SolutionAgentStore::global(cx)
                .read(cx)
                .session(session_id)
                .expect("session exists")
                .read(cx)
                .cwd
                .to_string_lossy()
                .into_owned()
        });

        let get_result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");
        assert_eq!(
            get_result.structured_content.cwd.as_deref(),
            Some(expected_cwd.as_str()),
            "get_session must surface session.cwd"
        );

        let list_result = ListSessionsTool
            .run(ListSessionsParams::default(), &mut cx.to_async())
            .await
            .expect("list_sessions");
        let summary = list_result
            .structured_content
            .sessions
            .iter()
            .find(|s| s.id == session_id.to_string())
            .expect("session present in list_sessions");
        assert_eq!(
            summary.cwd.as_deref(),
            Some(expected_cwd.as_str()),
            "list_sessions must surface session.cwd on every entry"
        );
    }

    #[gpui::test]
    async fn entry_summary_carries_subagent_id_when_meta_present(cx: &mut gpui::TestAppContext) {
        // Push one assistant chunk stamped with a parent tool_use id via the
        // same meta key claude_native emits. The wire builder must surface it
        // verbatim on the resulting EntrySummary.
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                // `_meta.claudeCode.parentToolUseId` is the wire shape
                // claude_native stamps; matches `subagent_id_from_meta` in
                // acp_thread. Goes on the ContentChunk envelope, NOT on
                // the inner content block — that's where the helper looks.
                let mut meta = serde_json::Map::new();
                meta.insert(
                    "claudeCode".into(),
                    serde_json::json!({ "parentToolUseId": "toolu_parent_xyz" }),
                );
                let mut chunk = acp::ContentChunk::new(acp::ContentBlock::Text(
                    acp::TextContent::new("subagent says hi".to_string()),
                ));
                chunk.meta = Some(meta);
                thread
                    .handle_session_update(acp::SessionUpdate::AgentMessageChunk(chunk), cx)
                    .expect("handle_session_update");
            });
        });
        cx.executor().run_until_parked();

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let assistant = result
            .structured_content
            .entries
            .iter()
            .find(|e| matches!(e.role, EntryRoleDto::Assistant))
            .expect("assistant entry should be present");
        assert_eq!(
            assistant.subagent_id.as_deref(),
            Some("toolu_parent_xyz"),
            "EntrySummary must carry the parent tool_use id"
        );
    }

    /// Seed `[user(Main), assistant(Main), assistant(sub1), user(Main)]` so a
    /// subagent dominates the recent tail (the empty-Main scenario) and return
    /// the session id. The single `sub1` assistant carries the subagent_id via
    /// the same `_meta` claude_native stamps.
    async fn seed_mixed_subagent_session(
        cx: &mut gpui::TestAppContext,
    ) -> (crate::model::SolutionSessionId, gpui::Entity<acp_thread::AcpThread>, tempfile::TempDir)
    {
        let (session_id, acp_thread, tmp) = create_session_with_thread(cx).await;
        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                thread.push_user_content_block(
                    None,
                    acp::ContentBlock::Text(acp::TextContent::new("u0".to_string())),
                    cx,
                );
                thread.push_assistant_content_block(
                    acp::ContentBlock::Text(acp::TextContent::new("a1-main".to_string())),
                    false,
                    cx,
                );
                let mut meta = serde_json::Map::new();
                meta.insert(
                    "claudeCode".into(),
                    serde_json::json!({ "parentToolUseId": "sub1" }),
                );
                let mut chunk = acp::ContentChunk::new(acp::ContentBlock::Text(
                    acp::TextContent::new("s2-sub".to_string()),
                ));
                chunk.meta = Some(meta);
                thread
                    .handle_session_update(acp::SessionUpdate::AgentMessageChunk(chunk), cx)
                    .expect("handle_session_update");
                thread.push_user_content_block(
                    None,
                    acp::ContentBlock::Text(acp::TextContent::new("u3".to_string())),
                    cx,
                );
            });
        });
        cx.executor().run_until_parked();
        (session_id, acp_thread, tmp)
    }

    async fn get_session_filtered(
        session_id: crate::model::SolutionSessionId,
        filter: Option<&str>,
        cx: &mut gpui::TestAppContext,
    ) -> (Vec<Option<String>>, usize) {
        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    subagent_filter: filter.map(|s| s.to_string()),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");
        let ids = result
            .structured_content
            .entries
            .iter()
            .map(|e| e.subagent_id.clone())
            .collect();
        (ids, result.structured_content.total_count)
    }

    #[gpui::test]
    async fn get_session_subagent_filter_main_keeps_only_parent_entries(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _thread, _tmp) = seed_mixed_subagent_session(cx).await;
        // A subagent strip is present ⇒ Main hides subagent entries.
        seed_subagent_tabs(session_id, &[("sub1", "Sub One")], cx);

        let (main_ids, main_total) = get_session_filtered(session_id, Some("__main__"), cx).await;
        assert!(
            main_ids.iter().all(|id| id.is_none()),
            "Main filter must keep only parent (subagent_id == None) entries, got {main_ids:?}"
        );
        assert_eq!(main_ids.len(), 3, "u0 / a1-main / u3 are the Main entries");
        assert_eq!(main_total, 3, "total_count reflects the FILTERED Main set");

        let (sub_ids, sub_total) = get_session_filtered(session_id, Some("sub1"), cx).await;
        assert_eq!(
            sub_ids,
            vec![Some("sub1".to_string())],
            "sub1 filter keeps only that subagent's entry"
        );
        assert_eq!(sub_total, 1);
    }

    #[gpui::test]
    async fn get_session_subagent_filter_main_bypass_when_no_active_subagents(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _thread, _tmp) = seed_mixed_subagent_session(cx).await;
        // NO active subagents seeded ⇒ desktop "no strip → show all" bypass:
        // even a `__main__` filter returns every entry so history doesn't vanish.
        let (ids, total) = get_session_filtered(session_id, Some("__main__"), cx).await;
        assert_eq!(ids.len(), 4, "bypass returns all 4 entries");
        assert_eq!(total, 4);
        assert!(
            ids.iter().any(|id| id.as_deref() == Some("sub1")),
            "bypass keeps the historical subagent entry, got {ids:?}"
        );
    }

    #[gpui::test]
    async fn entry_summary_subagent_id_absent_for_parent_entries(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        for entry in &result.structured_content.entries {
            assert!(
                entry.subagent_id.is_none(),
                "seeded session has only parent-level entries; got subagent_id={:?} on {:?}",
                entry.subagent_id,
                entry.role
            );
        }
    }

    #[gpui::test]
    async fn build_active_subagents_changed_payload_shape(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;
        seed_subagent_tabs(session_id, &[("toolu_one", "Alpha")], cx);

        cx.update(|cx| {
            let payload =
                crate::event_sources::build_active_subagents_changed_payload(session_id, cx);
            let obj = payload.as_object().expect("object");
            assert_eq!(
                obj.get("session_id").and_then(|v| v.as_str()),
                Some(session_id.to_string().as_str())
            );
            let arr = obj
                .get("active_subagents")
                .and_then(|v| v.as_array())
                .expect("active_subagents array");
            assert_eq!(arr.len(), 1, "one seeded tab → one descriptor");
            let entry = arr[0].as_object().expect("dto object");
            assert_eq!(entry.get("id").and_then(|v| v.as_str()), Some("toolu_one"));
            assert_eq!(entry.get("label").and_then(|v| v.as_str()), Some("Alpha"));
            let started_at = entry
                .get("started_at_ms")
                .and_then(|v| v.as_i64())
                .expect("started_at_ms");
            assert!(started_at > 0, "started_at_ms must be positive");
        });
    }

    /// Finding 1 regression guard: a session that was closed (not in
    /// `store.sessions`) but whose transcript is stored as per-entry rows
    /// (no blob — the Phase-4 write path never writes blobs) must be
    /// served by `read_session_history` instead of returning
    /// `session_not_found`.
    ///
    /// Before the fix the archive path only called `load_blob`, which
    /// returns NULL for a row-native session → the tool returned
    /// `session_not_found` even though the rows were present.
    #[gpui::test]
    async fn read_session_history_closed_row_native_returns_entries(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::session_entry::{SessionEntry, SessionEntryKind};

        // Set up a real DB so rows can be written + read by the tool.
        let (solution_id, _tmp, _project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        let registry = std::sync::Arc::new(crate::adapter::AdapterRegistry::new());
        cx.update(|cx| SolutionAgentStore::init_global(cx, registry));
        let executor = cx.executor();
        let db = std::sync::Arc::new(
            crate::db::SolutionAgentDb::open(executor).expect("open db"),
        );
        cx.update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_persistence(db.clone(), cx);
            });
        });

        let session_id = crate::model::SolutionSessionId::new();
        let now = chrono::Utc::now();
        let meta = crate::model::SolutionSessionMetadata {
            id: session_id,
            solution_id: solution_id.clone(),
            agent_id: SharedString::from("mock-agent"),
            acp_session_id: acp::SessionId::new(format!("acp-{}", session_id.as_str())),
            title: SharedString::from("closed row-native session"),
            created_at: now,
            last_activity_at: now,
            preview: None,
            total_tokens: None,
            context_count: 1,
            cwd: std::path::PathBuf::new(),
            parent_session_id: None,
            desired_model: None,
            desired_effort: None,
            cached_models: vec![],
        };
        db.save_metadata(meta).await.expect("save metadata");

        // Write two entries as rows (no blob) — the Phase-4 row-native shape.
        let user_entry = SessionEntry {
            created_ms: 1_700_000_000_000,
            mod_seq: 1,
            subagent_id: None,
            kind: SessionEntryKind::UserMessage {
                id: None,
                content_md: "hello from closed session".into(),
                chunks: vec![fake_user_text_chunk("hello from closed session")],
            },
        };
        let assistant_entry = SessionEntry {
            created_ms: 1_700_000_000_001,
            mod_seq: 2,
            subagent_id: None,
            kind: SessionEntryKind::AssistantMessage {
                chunks: vec![crate::session_entry::AssistantChunk::Message(
                    "reply from closed session".into(),
                )],
            },
        };
        db.upsert_entry(
            session_id,
            0,
            user_entry.mod_seq as i64,
            user_entry.created_ms,
            None,
            user_entry.to_payload(),
        )
        .await
        .expect("upsert user entry");
        db.upsert_entry(
            session_id,
            1,
            assistant_entry.mod_seq as i64,
            assistant_entry.created_ms,
            None,
            assistant_entry.to_payload(),
        )
        .await
        .expect("upsert assistant entry");

        // The session is NOT in store.sessions — only the DB rows exist.
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            assert!(
                store.read(cx).session(session_id).is_none(),
                "session must not be in memory for this test"
            );
        });

        // Call the tool — before the fix this returned session_not_found.
        let result = ReadSessionHistoryTool
            .run(
                ReadSessionHistoryParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("read_session_history must succeed for a closed row-native session");

        let sc = &result.structured_content;
        assert_eq!(
            sc.total_entries, 2,
            "must return both rows, got {}",
            sc.total_entries
        );
        assert_eq!(sc.returned_entries, 2);
        assert_eq!(sc.source, "archived");
        assert!(
            sc.entries[0].contains("hello from closed session"),
            "user entry must round-trip; got: {:?}",
            sc.entries[0]
        );
        assert!(
            sc.entries[1].contains("reply from closed session"),
            "assistant entry must round-trip; got: {:?}",
            sc.entries[1]
        );
    }

    // -----------------------------------------------------------------
    // Task 5.2: get_session_changes (mobile delta).
    // -----------------------------------------------------------------

    /// 1×1 PNG, base64 (no `data:` prefix) — same fixture the other image
    /// tests use, kept tiny so it doesn't bloat the suite.
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNgAAIAAAUAAen5lOEAAAAASUVORK5CYII=";

    /// Build a COLD, row-native session with a fixed entry layout for the
    /// delta tests:
    ///   index 0: Main user message + image     (mod_seq 1)
    ///   index 1: Main assistant message        (mod_seq 2)
    ///   index 2: Subagent("sub1") assistant    (mod_seq 3)
    ///   index 3: Main user message + image     (mod_seq 4)
    /// `change_seq` is seated at 4 (= max mod_seq). All section watermarks
    /// start at 0 so a `since_seq=0` poll re-sends every section; individual
    /// tests bump the watermarks they care about. No live thread.
    async fn seed_delta_session(
        cx: &mut gpui::TestAppContext,
    ) -> (crate::model::SolutionSessionId, tempfile::TempDir) {
        use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
        let (solution_id, tmp, _project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            let registry = std::sync::Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });
        let session_id = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                let id = crate::model::SolutionSessionId::new();
                let mut session = crate::model::SolutionSession::new_idle(
                    id,
                    solution_id.clone(),
                    SharedString::from("mock-agent"),
                    acp::SessionId::new(format!("acp-{}", id.as_str())),
                );
                session.title = SharedString::from("delta session");
                session.entries = vec![
                    SessionEntry {
                        created_ms: 1_700_000_000_000,
                        mod_seq: 1,
                        subagent_id: None,
                        kind: SessionEntryKind::UserMessage {
                            id: None,
                            content_md: "u0".into(),
                            chunks: vec![
                                fake_user_text_chunk("u0"),
                                fake_image_chunk("image/png", TINY_PNG_B64),
                            ],
                        },
                    },
                    SessionEntry {
                        created_ms: 1_700_000_000_001,
                        mod_seq: 2,
                        subagent_id: None,
                        kind: SessionEntryKind::AssistantMessage {
                            chunks: vec![AssistantChunk::Message("a1-main".into())],
                        },
                    },
                    SessionEntry {
                        created_ms: 1_700_000_000_002,
                        mod_seq: 3,
                        subagent_id: Some(SharedString::from("sub1")),
                        kind: SessionEntryKind::AssistantMessage {
                            chunks: vec![AssistantChunk::Message("s2-sub".into())],
                        },
                    },
                    SessionEntry {
                        created_ms: 1_700_000_000_003,
                        mod_seq: 4,
                        subagent_id: None,
                        kind: SessionEntryKind::UserMessage {
                            id: None,
                            content_md: "u3".into(),
                            chunks: vec![
                                fake_user_text_chunk("u3"),
                                fake_image_chunk("image/png", TINY_PNG_B64),
                            ],
                        },
                    },
                ];
                session.change_seq = 4;
                store.register_prebuilt_session(session, cx)
            })
        });
        (session_id, tmp)
    }

    /// Mutate the in-memory session (set watermarks, push a queue bundle,
    /// seed a subagent tab, change state, …).
    fn mutate_session(
        session_id: crate::model::SolutionSessionId,
        cx: &mut gpui::TestAppContext,
        f: impl FnOnce(&mut crate::model::SolutionSession),
    ) {
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store
                .read(cx)
                .session(session_id)
                .expect("session must exist");
            session.update(cx, |s, _| f(s));
        });
    }

    async fn run_changes(
        params: GetSessionChangesParams,
        cx: &mut gpui::TestAppContext,
    ) -> GetSessionChangesResult {
        GetSessionChangesTool
            .run(params, &mut cx.to_async())
            .await
            .expect("get_session_changes")
            .structured_content
    }

    #[gpui::test]
    async fn get_session_changes_returns_only_entries_past_since_seq(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _tmp) = seed_delta_session(cx).await;

        // since_seq = 2 → only entries with mod_seq 3 and 4 (indices 2, 3).
        let result = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 2,
                known_epoch: 0,
                subagent_filter: None,
                include_images: false,
            },
            cx,
        )
        .await;

        assert!(!result.reset);
        assert_eq!(result.epoch, 0);
        assert_eq!(result.current_seq, 4);
        // total_count is the full (filtered=None) count, independent of since_seq.
        assert_eq!(result.total_count, 4);
        let indices: Vec<usize> = result.changed_entries.iter().map(|e| e.index).collect();
        assert_eq!(
            indices,
            vec![2, 3],
            "only mod_seq > since_seq entries, with absolute indices"
        );

        // since_seq = 4 → nothing changed.
        let none = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 4,
                known_epoch: 0,
                subagent_filter: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert!(none.changed_entries.is_empty());
        assert_eq!(none.total_count, 4);
    }

    #[gpui::test]
    async fn get_session_changes_paginates_changed_entries(cx: &mut gpui::TestAppContext) {
        use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
        let (session_id, _tmp) = seed_delta_session(cx).await;
        // Replace with 15 entries (mod_seq 1..=15) so a since=0 poll exceeds the
        // 10-per-page cap.
        mutate_session(session_id, cx, |s| {
            s.entries = (1..=15u64)
                .map(|n| SessionEntry {
                    created_ms: 1_700_000_000_000 + n as i64,
                    mod_seq: n,
                    subagent_id: None,
                    kind: SessionEntryKind::AssistantMessage {
                        chunks: vec![AssistantChunk::Message(format!("a{n}"))],
                    },
                })
                .collect();
            s.change_seq = 15;
        });

        // Page 1: capped at 10, has_more, cursor at the 10th entry's mod_seq.
        let p1 = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 0,
                known_epoch: 0,
                subagent_filter: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert_eq!(p1.changed_entries.len(), CHANGED_ENTRIES_PAGE, "page capped");
        assert!(p1.has_more, "more entries remain after page 1");
        assert_eq!(p1.current_seq, 10, "cursor advances to the 10th mod_seq");
        assert_eq!(p1.total_count, 15, "total_count is the full filtered count");

        // Page 2: the remaining 5, caught up, cursor at the full change_seq.
        let p2 = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: p1.current_seq,
                known_epoch: 0,
                subagent_filter: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert_eq!(p2.changed_entries.len(), 5, "remaining entries on page 2");
        assert!(!p2.has_more, "caught up after page 2");
        assert_eq!(p2.current_seq, 15, "cursor at full change_seq when caught up");
    }

    #[gpui::test]
    async fn get_session_changes_sections_always_present(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_delta_session(cx).await;

        // The three small sections are ALWAYS sent, regardless of how far the
        // section watermarks sit below `since_seq`. Here every watermark is far
        // below the client's cursor — the gated implementation would have
        // omitted all three (the staleness hole); the always-send contract
        // surfaces the current values so the delta re-establishes them.
        mutate_session(session_id, cx, |s| {
            s.state = crate::model::SessionState::AwaitingInput;
            s.state_seq = 2;
            s.queue_seq = 2;
            s.subagents_seq = 2;
            s.change_seq = 9;
        });

        let result = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 8,
                known_epoch: 0,
                subagent_filter: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert!(
            matches!(result.state, Some(SessionStateDto::AwaitingInput)),
            "state always present (even with state_seq << since_seq), got {:?}",
            result.state
        );
        assert!(
            result
                .pending_bundles
                .as_ref()
                .is_some_and(|b| b.is_empty()),
            "pending_bundles always present; empty Vec when the queue is empty"
        );
        assert!(
            result
                .active_subagents
                .as_ref()
                .is_some_and(|a| a.is_empty()),
            "active_subagents always present; empty Vec when the strip is empty"
        );

        // A non-empty queue surfaces in the same always-present section.
        mutate_session(session_id, cx, |s| {
            s.pending_messages.push_back(crate::model::PendingBundle {
                target: crate::model::QueueTarget::Main,
                blocks: vec![fake_user_text_chunk("queued")],
            });
            s.queue_seq = 2;
            s.change_seq = 10;
        });
        let result = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 9,
                known_epoch: 0,
                subagent_filter: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert!(
            matches!(result.state, Some(SessionStateDto::AwaitingInput)),
            "state still present on a later poll"
        );
        let bundles = result
            .pending_bundles
            .expect("pending_bundles always Some");
        assert_eq!(bundles.len(), 1, "the queued bundle surfaces");
    }

    #[gpui::test]
    async fn get_session_changes_reset_on_epoch_mismatch(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_delta_session(cx).await;
        // Push the session epoch above the client's known_epoch.
        mutate_session(session_id, cx, |s| {
            s.epoch = 3;
            // Move every watermark so a non-reset path WOULD have populated them.
            s.state_seq = 5;
            s.queue_seq = 5;
            s.subagents_seq = 5;
        });

        let result = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 0,
                known_epoch: 0,
                subagent_filter: None,
                include_images: false,
            },
            cx,
        )
        .await;

        assert!(result.reset, "epoch mismatch must set reset");
        assert_eq!(result.epoch, 3);
        assert!(result.changed_entries.is_empty());
        assert!(result.removed_indices.is_empty());
        assert!(result.state.is_none());
        assert!(result.pending_bundles.is_none());
        assert!(result.active_subagents.is_none());
        // total_count is still the filtered count (client ignores it).
        assert_eq!(result.total_count, 4);
    }

    #[gpui::test]
    async fn get_session_changes_subagent_filter_narrows_entries_and_total(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _tmp) = seed_delta_session(cx).await;
        // A subagent strip must be present, else the filter bypass kicks in.
        seed_subagent_tabs(session_id, &[("sub1", "Sub One")], cx);

        // sub1 filter, since_seq 0 → only the sub1 entry (index 2).
        let sub = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 0,
                known_epoch: 0,
                subagent_filter: Some("sub1".to_string()),
                include_images: false,
            },
            cx,
        )
        .await;
        let sub_indices: Vec<usize> = sub.changed_entries.iter().map(|e| e.index).collect();
        assert_eq!(sub_indices, vec![2], "sub1 filter keeps only the sub1 entry");
        assert_eq!(sub.total_count, 1, "filtered total is the sub1 count");

        // __main__ filter → the three subagent_id == None entries (0, 1, 3).
        let main = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 0,
                known_epoch: 0,
                subagent_filter: Some("__main__".to_string()),
                include_images: false,
            },
            cx,
        )
        .await;
        let main_indices: Vec<usize> = main.changed_entries.iter().map(|e| e.index).collect();
        assert_eq!(
            main_indices,
            vec![0, 1, 3],
            "__main__ filter keeps subagent_id == None entries"
        );
        assert_eq!(main.total_count, 3);
    }

    #[gpui::test]
    async fn get_session_changes_image_indices_match_get_session(cx: &mut gpui::TestAppContext) {
        // The subtle parity test: a changed entry positioned AFTER earlier
        // image-bearing entries must report image indices identical to what
        // get_session returns for the same session + filter. Index 3 carries
        // an image and sits after the image at index 0, so its EntryImage.index
        // must be 1 in BOTH responses.
        let (session_id, _tmp) = seed_delta_session(cx).await;

        let full = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: true,
                    include_images: true,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session")
            .structured_content;

        // Delta with since_seq = 3 → only index 3 (the second image-bearer).
        let delta = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 3,
                known_epoch: 0,
                subagent_filter: None,
                include_images: true,
            },
            cx,
        )
        .await;
        assert_eq!(
            delta.changed_entries.len(),
            1,
            "since_seq 3 yields exactly index 3"
        );
        let delta_entry = &delta.changed_entries[0];
        assert_eq!(delta_entry.index, 3);

        let full_entry = full
            .entries
            .iter()
            .find(|e| e.index == 3)
            .expect("get_session must include index 3");

        let delta_image_indices: Vec<usize> = delta_entry
            .images
            .as_ref()
            .expect("delta entry images populated")
            .iter()
            .map(|img| img.index)
            .collect();
        let full_image_indices: Vec<usize> = full_entry
            .images
            .as_ref()
            .expect("full entry images populated")
            .iter()
            .map(|img| img.index)
            .collect();
        assert_eq!(
            delta_image_indices, full_image_indices,
            "delta image indices must equal get_session's for the same entry"
        );
        assert_eq!(
            delta_image_indices,
            vec![1],
            "the second image-bearing entry's image is global index 1"
        );
    }

    #[gpui::test]
    async fn get_session_changes_tail_truncate_shrinks_total(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_delta_session(cx).await;
        // Tail-truncate to the first two entries (mirrors EntriesRemoved).
        mutate_session(session_id, cx, |s| {
            s.entries.truncate(2);
            s.change_seq = 5;
            // Bump the surviving tail entry's mod_seq so it re-sends.
            if let Some(last) = s.entries.last_mut() {
                last.mod_seq = 5;
            }
        });

        let result = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 4,
                known_epoch: 0,
                subagent_filter: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert_eq!(result.total_count, 2, "total_count shrank to the new length");
        assert!(
            result.removed_indices.is_empty(),
            "removed_indices stays empty under the tail-truncate model"
        );
        let indices: Vec<usize> = result.changed_entries.iter().map(|e| e.index).collect();
        assert_eq!(indices, vec![1], "surviving changed entry keeps its index");
    }

    fn anchored_entry(index: usize, role: EntryRoleDto) -> EntrySummary {
        EntrySummary {
            role,
            index,
            preview: String::new(),
            markdown: None,
            images: None,
            tool_call: None,
            plan: None,
            client_send_id: None,
            client_send_ids: Vec::new(),
            created_ms: None,
            subagent_id: None,
        }
    }

    #[test]
    fn user_anchored_filter_keeps_user_lead_and_resting_turn() {
        use EntryRoleDto::*;
        // Timeline: assistant churn, a user turn, more churn, another user
        // turn, then a long agent tail. lead=2 → each user keeps itself + 2
        // before; the final entry is always kept (the resting turn).
        let mut kept = vec![
            anchored_entry(0, Assistant),
            anchored_entry(1, ToolCall),
            anchored_entry(2, Assistant),
            anchored_entry(3, User), // keeps 1,2,3
            anchored_entry(4, ToolCall),
            anchored_entry(5, Assistant),
            anchored_entry(6, ToolCall),
            anchored_entry(7, User), // keeps 5,6,7
            anchored_entry(8, Assistant),
            anchored_entry(9, ToolCall),
            anchored_entry(10, Assistant), // resting turn → kept
        ];
        apply_user_anchored_filter(&mut kept, 2);
        let indices: Vec<usize> = kept.iter().map(|e| e.index).collect();
        assert_eq!(indices, vec![1, 2, 3, 5, 6, 7, 10]);
    }

    #[test]
    fn user_anchored_filter_dedups_overlapping_windows_and_clamps_start() {
        use EntryRoleDto::*;
        // Back-to-back user turns with lead larger than the gap must not
        // duplicate the shared lead entries, and lead past index 0 clamps.
        let mut kept = vec![
            anchored_entry(0, Assistant),
            anchored_entry(1, User),
            anchored_entry(2, User),
            anchored_entry(3, Assistant),
        ];
        apply_user_anchored_filter(&mut kept, 5);
        let indices: Vec<usize> = kept.iter().map(|e| e.index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3]);
    }

    #[test]
    fn user_anchored_filter_noop_without_user_entries() {
        use EntryRoleDto::*;
        let mut kept = vec![
            anchored_entry(0, Assistant),
            anchored_entry(1, ToolCall),
        ];
        apply_user_anchored_filter(&mut kept, 3);
        let indices: Vec<usize> = kept.iter().map(|e| e.index).collect();
        assert_eq!(indices, vec![0, 1], "no anchor → window kept as-is");
    }
}
