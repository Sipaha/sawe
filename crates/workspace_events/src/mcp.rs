use anyhow::Result;
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};

use crate::dto::{SnapshotParams, WorkspaceSnapshot};
use crate::snapshot::build_snapshot;

#[derive(Clone)]
pub struct SnapshotTool;

impl McpServerTool for SnapshotTool {
    type Input = SnapshotParams;
    type Output = WorkspaceSnapshot;
    const NAME: &'static str = "workspace.snapshot";

    async fn run(
        &self,
        _input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let snap = cx.update(|cx| build_snapshot(cx));
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "snapshot seq={} solutions={}",
                    snap.seq,
                    snap.solutions.len()
                ),
            }],
            structured_content: snap,
        })
    }
}

pub fn register(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SnapshotTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(crate::list::ListSolutionsTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(crate::lifecycle::OpenSolutionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(crate::lifecycle::CloseSolutionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(crate::lifecycle::OpenSessionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(crate::lifecycle::CloseSessionTool);
    });
}
