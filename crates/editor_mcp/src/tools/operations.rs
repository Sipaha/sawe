//! `editor.get_operation` and `editor.cancel_operation` MCP tools — backed
//! by the global OperationTracker (`crate::operations`).
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::AsyncApp;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

/// Get the current state of an operation by id. Returns the latest known
/// progress and (if completed) the result or error.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetOperationParams {
    pub operation_id: String,
}

impl<'de> Deserialize<'de> for GetOperationParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            operation_id: String,
        }
        Ok(Self {
            operation_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .operation_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetOperationResult {
    pub operation_id: String,
    pub kind: String,
    pub status: String,
    pub progress: ProgressInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub cancellation_requested: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProgressInfo {
    pub stage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
}

#[derive(Clone)]
pub struct GetOperationTool;

impl McpServerTool for GetOperationTool {
    type Input = GetOperationParams;
    type Output = GetOperationResult;
    const NAME: &'static str = "editor.get_operation";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.operation_id.is_empty(),
            "invalid_params: operation_id is required"
        );
        let state = cx
            .update(|cx| crate::op_get(&input.operation_id, cx))
            .ok_or_else(|| anyhow::anyhow!("operation_not_found: {}", input.operation_id))?;

        let status_str = match state.status {
            crate::OperationStatus::Pending => "pending",
            crate::OperationStatus::Completed => "completed",
            crate::OperationStatus::Failed => "failed",
            crate::OperationStatus::Cancelled => "cancelled",
        };

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{}: {}", state.id, status_str),
            }],
            structured_content: GetOperationResult {
                operation_id: state.id.clone(),
                kind: state.kind.clone(),
                status: status_str.to_string(),
                progress: ProgressInfo {
                    stage: state.progress.stage.clone(),
                    percent: state.progress.percent,
                },
                result: state.result.clone(),
                error: state.error.clone(),
                started_at: state.started_at.to_rfc3339(),
                completed_at: state.completed_at.map(|t| t.to_rfc3339()),
                cancellation_requested: state.cancellation_requested,
            },
        })
    }
}

/// Request cancellation of a pending operation. Best-effort: the tool
/// running the operation must check `op_is_cancelled` periodically and
/// abort. Returns whether the request was accepted.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CancelOperationParams {
    pub operation_id: String,
}

impl<'de> Deserialize<'de> for CancelOperationParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            operation_id: String,
        }
        Ok(Self {
            operation_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .operation_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CancelOperationResult {
    pub cancellation_requested: bool,
}

#[derive(Clone)]
pub struct CancelOperationTool;

impl McpServerTool for CancelOperationTool {
    type Input = CancelOperationParams;
    type Output = CancelOperationResult;
    const NAME: &'static str = "editor.cancel_operation";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.operation_id.is_empty(),
            "invalid_params: operation_id is required"
        );
        let requested = cx.update(|cx| crate::op_request_cancellation(&input.operation_id, cx));
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("cancellation_requested: {requested}"),
            }],
            structured_content: CancelOperationResult {
                cancellation_requested: requested,
            },
        })
    }
}
