//! `editor.subscribe`, `editor.unsubscribe`, and `editor.list_subscriptions`
//! MCP tools — backed by the global SubscriptionRegistry.
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::AsyncApp;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

/// Subscribe to event kinds. Notifications are not yet pushed in real time
/// (clients should poll `editor.get_operation` for op-progress); this tool
/// records the subscription server-side so the API is stable for clients.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SubscribeParams {
    /// Optional Solution scope. Omit for global subscription.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solution_id: Option<i64>,
    /// List of event kinds to subscribe to (e.g., `operation_progress`).
    pub kinds: Vec<String>,
    /// Optional filter object (kind-specific).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<serde_json::Value>,
    /// Kinds the caller wants delivery suppressed for on THIS connection,
    /// even though they stay in `kinds`. Gated on the
    /// `quiet_message_appended` feature token.
    ///
    /// Accepted and echoed here, but deliberately NOT consulted by
    /// `editor_mcp::emit_notification`: the registry is process-global and
    /// never pruned on disconnect, so honouring a suppression list at the
    /// emit layer would apply one client's preferences to every other client.
    /// Enforcement lives in `remote_control`'s per-connection proxy, which is
    /// the only layer that knows which socket asked. The field exists on this
    /// tool solely so the parameter survives `deny_unknown_fields` when the
    /// proxy forwards the frame verbatim — without it a client that asked for
    /// suppression would get `-32602` and a failed subscribe.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suppress_kinds: Vec<String>,
}

impl<'de> Deserialize<'de> for SubscribeParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: Option<i64>,
            kinds: Vec<String>,
            filter: Option<serde_json::Value>,
            suppress_kinds: Vec<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            kinds: inner.kinds,
            filter: inner.filter,
            suppress_kinds: inner.suppress_kinds,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SubscribeResult {
    pub subscription_id: String,
}

#[derive(Clone)]
pub struct SubscribeTool;

impl McpServerTool for SubscribeTool {
    type Input = SubscribeParams;
    type Output = SubscribeResult;
    const NAME: &'static str = "editor.subscribe";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.kinds.is_empty(),
            "invalid_params: at least one kind is required"
        );
        let id = cx.update(|cx| {
            crate::sub_create(
                input.kinds.clone(),
                input.solution_id,
                input.filter.clone(),
                cx,
            )
        });
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("subscription: {id}"),
            }],
            structured_content: SubscribeResult {
                subscription_id: id,
            },
        })
    }
}

/// Remove a subscription by id.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct UnsubscribeParams {
    pub subscription_id: String,
}

impl<'de> Deserialize<'de> for UnsubscribeParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            subscription_id: String,
        }
        Ok(Self {
            subscription_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .subscription_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UnsubscribeResult {
    pub unsubscribed: bool,
}

#[derive(Clone)]
pub struct UnsubscribeTool;

impl McpServerTool for UnsubscribeTool {
    type Input = UnsubscribeParams;
    type Output = UnsubscribeResult;
    const NAME: &'static str = "editor.unsubscribe";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.subscription_id.is_empty(),
            "invalid_params: subscription_id is required"
        );
        let removed = cx.update(|cx| crate::sub_delete(&input.subscription_id, cx));
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("unsubscribed: {removed}"),
            }],
            structured_content: UnsubscribeResult {
                unsubscribed: removed,
            },
        })
    }
}

/// List all active subscriptions.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ListSubscriptionsParams {}

impl<'de> Deserialize<'de> for ListSubscriptionsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let _ = serde::de::IgnoredAny::deserialize(de)?;
        Ok(ListSubscriptionsParams {})
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SubscriptionInfo {
    pub id: String,
    pub kinds: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solution_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<serde_json::Value>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListSubscriptionsResult {
    pub subscriptions: Vec<SubscriptionInfo>,
}

#[derive(Clone)]
pub struct ListSubscriptionsTool;

impl McpServerTool for ListSubscriptionsTool {
    type Input = ListSubscriptionsParams;
    type Output = ListSubscriptionsResult;
    const NAME: &'static str = "editor.list_subscriptions";

    async fn run(
        &self,
        _input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let subs = cx.update(|cx| crate::sub_list(cx));
        let infos: Vec<SubscriptionInfo> = subs
            .into_iter()
            .map(|s| SubscriptionInfo {
                id: s.id,
                kinds: s.kinds,
                solution_id: s.solution_id,
                filter: s.filter,
                created_at: s.created_at.to_rfc3339(),
            })
            .collect();
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} subscription(s)", infos.len()),
            }],
            structured_content: ListSubscriptionsResult {
                subscriptions: infos,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SubscribeParams` is `deny_unknown_fields`, so the parameter has to be
    /// declared here even though only `remote_control`'s per-connection proxy
    /// may act on it — otherwise the proxy's verbatim forward of a
    /// `remote.editor.subscribe` carrying `suppress_kinds` would come back
    /// `-32602` and the client would see a failed subscribe.
    #[test]
    fn subscribe_accepts_suppress_kinds() {
        let params: SubscribeParams = serde_json::from_value(serde_json::json!({
            "kinds": ["agent_session_dirty", "agent_session_message_appended"],
            "suppress_kinds": ["agent_session_message_appended"],
        }))
        .expect("suppress_kinds must not be an unknown field");
        assert_eq!(params.kinds.len(), 2);
        assert_eq!(
            params.suppress_kinds,
            vec!["agent_session_message_appended"]
        );

        let old_client: SubscribeParams = serde_json::from_value(serde_json::json!({
            "kinds": ["agent_session_dirty"],
        }))
        .expect("old clients omit the key");
        assert!(old_client.suppress_kinds.is_empty());

        let unknown: Result<SubscribeParams, _> = serde_json::from_value(serde_json::json!({
            "kinds": ["agent_session_dirty"],
            "no_such_param": true,
        }));
        assert!(
            unknown.is_err(),
            "deny_unknown_fields must survive the addition"
        );
    }
}
