use editor_mcp::tools_for_test::{
    CancelOperationParams, GetOperationParams, ListSubscriptionsParams, SubscribeParams,
    UnsubscribeParams,
};

#[test]
fn get_operation_params_round_trip() {
    let p: GetOperationParams = serde_json::from_value(serde_json::json!({
        "operation_id": "op-1"
    }))
    .expect("parse");
    assert_eq!(p.operation_id, "op-1");
}

#[test]
fn get_operation_params_accepts_null() {
    let p: GetOperationParams = serde_json::from_value(serde_json::Value::Null).expect("null");
    assert!(p.operation_id.is_empty());
}

#[test]
fn cancel_operation_params_round_trip() {
    let p: CancelOperationParams = serde_json::from_value(serde_json::json!({
        "operation_id": "op-2"
    }))
    .expect("parse");
    assert_eq!(p.operation_id, "op-2");
}

#[test]
fn subscribe_params_round_trip() {
    let p: SubscribeParams = serde_json::from_value(serde_json::json!({
        "kinds": ["operation_progress", "buffer_saved"],
        "solution_id": "sol-1"
    }))
    .expect("parse");
    assert_eq!(p.kinds, vec!["operation_progress", "buffer_saved"]);
    assert_eq!(p.solution_id.as_deref(), Some("sol-1"));
}

#[test]
fn subscribe_params_accepts_null() {
    let p: SubscribeParams = serde_json::from_value(serde_json::Value::Null).expect("null");
    assert!(p.kinds.is_empty());
    assert!(p.solution_id.is_none());
}

#[test]
fn unsubscribe_params_round_trip() {
    let p: UnsubscribeParams = serde_json::from_value(serde_json::json!({
        "subscription_id": "sub-1"
    }))
    .expect("parse");
    assert_eq!(p.subscription_id, "sub-1");
}

#[test]
fn list_subscriptions_params_accepts_null() {
    let _: ListSubscriptionsParams = serde_json::from_value(serde_json::Value::Null).expect("null");
}
