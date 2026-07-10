//! Supervisor `solution_agent` MCP tools. Relocated verbatim from the former
//! monolithic `mcp.rs`.
use anyhow::{Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::model::SolutionSessionId;
use crate::store::SolutionAgentStore;

/// Record the judge's verdict for a supervised session and execute the
/// corresponding action (`continue`, `wait`, `compact`, `done`, `ask_agent`,
/// or `ask`).
///
/// - `continue`: increment the guard counter, send a nudge message, and
///   return the session to `Watching`.
/// - `compact`: queue a compact-context prompt on the session.
/// - `done`: park supervision in `Held` (the "done" standby — the operator's
///   next message OR the agent's own self-resume re-arms it) and log completion.
/// - `ask`: pause supervision in `WaitingUser` and escalate the question
///   to the operator.
/// - `ask_agent`: send a `question` to the WORKING agent (counts toward the
///   nudge guard, like `continue`).
/// - `wait`: sleep one-shot for `wait_seconds` (a self-clocked async task).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SupervisorVerdictParams {
    pub session_id: String,
    /// The single-use nonce from your briefing (the `{VERDICT_NONCE}` value).
    /// Echo it verbatim — a verdict without the matching nonce is rejected as
    /// unauthorized.
    pub nonce: String,
    /// One of: "continue", "compact", "done", "ask", "ask_agent", "wait".
    pub action: String,
    pub reasoning: String,
    /// Optional nudge message sent to the session when action == "continue".
    /// Defaults to "Continue." when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Required when action == "ask". The question to surface to the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// Sleep duration for action == "wait", in seconds. Clamped to
    /// [10, 1800] (30 min); defaults to 120 when absent. Ignored for other
    /// actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_seconds: Option<u64>,
}

impl<'de> Deserialize<'de> for SupervisorVerdictParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            nonce: String,
            action: String,
            reasoning: String,
            message: Option<String>,
            question: Option<String>,
            wait_seconds: Option<u64>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            nonce: inner.nonce,
            action: inner.action,
            reasoning: inner.reasoning,
            message: inner.message,
            question: inner.question,
            wait_seconds: inner.wait_seconds,
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
            "wait" => crate::supervisor::VerdictAction::Wait,
            other => anyhow::bail!("invalid_params: unknown action {other:?}"),
        };
        if matches!(
            action,
            crate::supervisor::VerdictAction::Ask | crate::supervisor::VerdictAction::AskAgent
        ) {
            anyhow::ensure!(
                input
                    .question
                    .as_deref()
                    .is_some_and(|q| !q.trim().is_empty()),
                "invalid_params: actions \"ask\"/\"ask_agent\" require a non-empty question"
            );
        }
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let outcome = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.apply_verdict_authenticated(
                    session_id,
                    &input.nonce,
                    action,
                    input.reasoning,
                    input.message,
                    input.question,
                    input.wait_seconds,
                    cx,
                )
            })
        });

        let text = match outcome {
            crate::store::VerdictAuth::Applied => "recorded",
            // Idempotent no-op — reported as success so a retrying judge stops.
            crate::store::VerdictAuth::NoInFlight => {
                "no active supervision for this session (already processed or superseded); ignored"
            }
            crate::store::VerdictAuth::Unauthorized => anyhow::bail!(
                "unauthorized: verdict nonce does not match the active judge briefing for this session"
            ),
        };

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: text.into() }],
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
    /// The single-use nonce from your briefing (the `{VERDICT_NONCE}` value).
    /// Echo it verbatim — an audit verdict without the matching nonce is
    /// rejected as unauthorized.
    pub nonce: String,
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
            nonce: String,
            ok: bool,
            action: String,
            reasoning: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            nonce: inner.nonce,
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

        let outcome = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.apply_audit_verdict_authenticated(
                    session_id,
                    &input.nonce,
                    input.ok,
                    escalate,
                    input.reasoning,
                    cx,
                )
            })
        });

        let text = match outcome {
            crate::store::VerdictAuth::Applied => "recorded",
            crate::store::VerdictAuth::NoInFlight => {
                "no active auditor for this session (already processed); ignored"
            }
            crate::store::VerdictAuth::Unauthorized => anyhow::bail!(
                "unauthorized: audit nonce does not match the active auditor briefing for this session"
            ),
        };

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: text.into() }],
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
    /// Times the supervisor has fired since last (re)enabled — the at-a-glance
    /// activity counter shown next to the status icon. Reset on every toggle.
    pub trigger_count: u32,
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
                    trigger_count: state.trigger_count,
                    max_continues: crate::supervisor::MAX_CONSECUTIVE_CONTINUES,
                    custom_prompt: state.custom_prompt,
                    verdicts_total: stats.total,
                    verdicts_continue: stats.by_action
                        [crate::supervisor::VerdictAction::Continue as usize],
                    verdicts_compact: stats.by_action
                        [crate::supervisor::VerdictAction::Compact as usize],
                    verdicts_done: stats.by_action[crate::supervisor::VerdictAction::Done as usize],
                    verdicts_ask: stats.by_action[crate::supervisor::VerdictAction::Ask as usize],
                    verdicts_ask_agent: stats.by_action
                        [crate::supervisor::VerdictAction::AskAgent as usize],
                    audits: stats.audits,
                    total_tokens: stats.total_tokens,
                },
                None => GetSupervisorStateResult {
                    enabled: false,
                    status: crate::supervisor::SupervisorStatus::Disabled.to_db_string(),
                    consecutive_continues: 0,
                    trigger_count: 0,
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

// =====================================================================
// solution_agent.seed_cold_session  (debug builds only)
// =====================================================================

pub(crate) fn register_supervisor(cx: &mut App) {
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
