//! Attachment-upload `solution_agent` MCP tools. Relocated verbatim from the
//! former monolithic `mcp.rs`.
use anyhow::{Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::model::SolutionSessionId;
use crate::store::SolutionAgentStore;

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

pub(crate) fn register_uploads(cx: &mut App) {
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
}
