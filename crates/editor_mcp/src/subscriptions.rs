//! In-memory registry of `editor.subscribe` requests.
//!
//! Real-time server-push notifications are not yet wired (requires an
//! upstream API in `context_server::McpServer` to send unsolicited
//! JSON-RPC notifications to connected clients). For now the registry
//! tracks subscriptions so:
//!  - `editor.list_subscriptions` returns the list,
//!  - `editor.unsubscribe` removes them,
//!  - tools can check whether interested clients exist before doing
//!    expensive notification-formatting work,
//!  - clients can poll `editor.get_operation` for op-progress updates.

use chrono::{DateTime, Utc};
use collections::HashMap;
use gpui::{App, Global};
use std::cell::RefCell;

#[derive(Debug, Clone)]
pub struct Subscription {
    pub id: String,
    pub kinds: Vec<String>,
    pub solution_id: Option<String>,
    pub filter: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Default)]
pub struct SubscriptionRegistry {
    subscriptions: RefCell<HashMap<String, Subscription>>,
    next_id: RefCell<u64>,
}

impl Global for SubscriptionRegistry {}

pub fn init(cx: &mut App) {
    if cx.try_global::<SubscriptionRegistry>().is_none() {
        cx.set_global(SubscriptionRegistry::default());
    }
}

pub fn create(
    kinds: Vec<String>,
    solution_id: Option<String>,
    filter: Option<serde_json::Value>,
    cx: &mut App,
) -> String {
    init(cx);
    let registry = cx.global::<SubscriptionRegistry>();
    let mut next = registry.next_id.borrow_mut();
    let id = format!("sub-{}", *next);
    *next += 1;
    drop(next);

    registry.subscriptions.borrow_mut().insert(
        id.clone(),
        Subscription {
            id: id.clone(),
            kinds,
            solution_id,
            filter,
            created_at: Utc::now(),
        },
    );
    id
}

pub fn delete(id: &str, cx: &App) -> bool {
    let Some(registry) = cx.try_global::<SubscriptionRegistry>() else {
        return false;
    };
    registry.subscriptions.borrow_mut().remove(id).is_some()
}

pub fn list(cx: &App) -> Vec<Subscription> {
    let Some(registry) = cx.try_global::<SubscriptionRegistry>() else {
        return Vec::new();
    };
    let mut out: Vec<Subscription> = registry.subscriptions.borrow().values().cloned().collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn create_list_delete(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let id1 = create(vec!["op_progress".into()], None, None, cx);
            let id2 = create(vec!["buffer_saved".into()], Some("sol-1".into()), None, cx);
            let listed = list(cx);
            assert_eq!(listed.len(), 2);
            assert!(delete(&id1, cx));
            let listed_after = list(cx);
            assert_eq!(listed_after.len(), 1);
            assert_eq!(listed_after[0].id, id2);
        });
    }
}
