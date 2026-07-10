//! Tool-call authorization `solution_agent` MCP tool. Relocated verbatim from
//! the former monolithic `mcp.rs`.
use agent_client_protocol::schema as acp;
use anyhow::{Context as _, Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::model::SolutionSessionId;
use crate::store::SolutionAgentStore;

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

pub(crate) fn register_authorization(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(AuthorizeToolCallTool);
    });
}
