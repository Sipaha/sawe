//! Session-lifecycle `solution_agent` MCP tools. Relocated verbatim from the
//! former monolithic `mcp.rs`.
use anyhow::{Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp, Entity};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::model::SolutionSessionId;
use crate::store::SolutionAgentStore;
use gpui::SharedString;
use solutions::{SolutionId, SolutionStore};

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
    pub solution_id: i64,
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
            solution_id: i64,
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
            !input.agent_id.is_empty(),
            "invalid_params: agent_id is required"
        );
        let solution_id = SolutionId(input.solution_id);
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
                            .map(|entity| entity.read(cx).solution_id)
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
            .update(|cx| project_for_solution(solution_id, cx))
            .ok_or_else(|| {
                anyhow!(
                    "no_active_workspace_for_solution: open Solution {} via solutions.open before \
                     creating a session",
                    input.solution_id
                )
            })?;

        let cwd: Option<std::path::PathBuf> = input.cwd.as_ref().map(std::path::PathBuf::from);

        // The wire tool takes a path, not a member id, so bind the session to
        // the member that owns that path (longest match); with no cwd the
        // session lands wherever `create_session` would put it — the solution's
        // active member.
        let member_id = cx.update(|cx| {
            let store = solutions::SolutionStore::try_global(cx)?;
            let store = store.read(cx);
            match cwd.as_deref() {
                Some(cwd) => store
                    .find_solution(solution_id)
                    .ok()?
                    .member_for_path(cwd)
                    .map(|member| member.id),
                None => store.active_member(solution_id),
            }
        });

        let create_task = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session_with_parent(
                    solution_id,
                    agent_id,
                    project,
                    cwd,
                    member_id,
                    parent_session_id,
                    None,
                    None,
                    false,
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
fn project_for_solution(solution_id: SolutionId, cx: &mut App) -> Option<Entity<project::Project>> {
    let store = SolutionStore::try_global(cx)?;
    let root = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .find(|sol| sol.id == solution_id)
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
// solution_agent.push_system_note
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
// solution_agent.reconnect_agent
// =====================================================================

/// Non-destructively recover a wedged session: respawn its subprocess and
/// replay the SAME `acp_session_id` from the transcript, keeping the
/// conversation (entries + claude context). Unlike `restart_agent` this does
/// not wipe history and keeps the session id/title. Returns the session id.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ReconnectAgentParams {
    pub session_id: String,
}

impl<'de> Deserialize<'de> for ReconnectAgentParams {
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
pub struct ReconnectAgentResult {
    pub session_id: String,
}

#[derive(Clone)]
pub struct ReconnectAgentTool;

impl McpServerTool for ReconnectAgentTool {
    type Input = ReconnectAgentParams;
    type Output = ReconnectAgentResult;
    const NAME: &'static str = "solution_agent.reconnect_agent";

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

        let task = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| store.reconnect_agent(session_id, cx))
        });
        let resumed = task.await?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: resumed.to_string(),
            }],
            structured_content: ReconnectAgentResult {
                session_id: resumed.to_string(),
            },
        })
    }
}

// =====================================================================
// solution_agent.reset_context
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

pub(crate) fn register_lifecycle(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(CreateSessionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(DeleteSessionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RenameSessionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RestartAgentTool);
        server.add_tool(ReconnectAgentTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ForceIdleTool);
    });
}
