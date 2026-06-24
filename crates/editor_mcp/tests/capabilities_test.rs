use context_server::listener::McpServerTool;
use editor_mcp::tools_for_test::{CapabilitiesParams, CapabilitiesTool};
use gpui::TestAppContext;

#[gpui::test]
async fn capabilities_returns_protocol_version(cx: &mut TestAppContext) {
    let response = cx
        .update(|cx| {
            let tool = CapabilitiesTool;
            cx.spawn(async move |cx| tool.run(CapabilitiesParams {}, cx).await)
        })
        .await
        .expect("run task");

    let caps = response.structured_content;
    assert_eq!(caps.protocol_version, "2024-11-05");
    assert!(!caps.editor_mcp_version.is_empty());
    assert!(!caps.supported_event_kinds.is_empty());
    assert!(
        caps.supported_event_kinds
            .iter()
            .any(|k| k == "operation_progress")
    );
    assert!(
        caps.supported_event_kinds
            .iter()
            .any(|k| k == "buffer_saved")
    );
    assert!(
        caps.supported_event_kinds
            .iter()
            .any(|k| k == "cli_args_received")
    );
}

#[test]
fn capabilities_params_deserializes_from_null() {
    let _: editor_mcp::tools_for_test::CapabilitiesParams =
        serde_json::from_value(serde_json::Value::Null).expect("null accepted");
}

#[test]
fn capabilities_params_deserializes_from_empty_object() {
    let _: editor_mcp::tools_for_test::CapabilitiesParams =
        serde_json::from_value(serde_json::json!({})).expect("empty object accepted");
}
