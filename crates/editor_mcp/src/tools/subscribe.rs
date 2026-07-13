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
}

impl<'de> Deserialize<'de> for SubscribeParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: Option<i64>,
            kinds: Vec<String>,
            filter: Option<serde_json::Value>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            kinds: inner.kinds,
            filter: inner.filter,
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
                input.solution_id.clone(),
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
