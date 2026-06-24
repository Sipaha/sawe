//! In-process tracker for long-running operations exposed via MCP.
//!
//! Tools that take seconds-to-minutes (e.g. `solutions.add_member` cloning
//! a large repo) return an `operation_id` immediately and continue work
//! in `cx.spawn`. Clients poll via `editor.get_operation(id)`.
//!
//! State changes (progress, completion) are also broadcast via
//! `editor/notification` MCP notifications so clients can react in real time
//! without polling.

use chrono::{DateTime, Utc};
use collections::HashMap;
use gpui::{App, Global};
use std::cell::RefCell;
use std::time::Duration;

const OPERATION_RETENTION: Duration = Duration::from_secs(300); // 5 minutes after completion

#[derive(Debug, Clone, Default)]
pub struct OperationProgress {
    pub stage: String,
    pub percent: Option<u8>,
}

#[derive(Debug, Clone)]
pub enum OperationStatus {
    Pending,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct OperationState {
    pub id: String,
    pub kind: String,
    pub status: OperationStatus,
    pub progress: OperationProgress,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub cancellation_requested: bool,
}

#[derive(Default)]
pub struct OperationTracker {
    operations: RefCell<HashMap<String, OperationState>>,
    next_id: RefCell<u64>,
}

impl Global for OperationTracker {}

pub fn init(cx: &mut App) {
    if cx.try_global::<OperationTracker>().is_none() {
        cx.set_global(OperationTracker::default());
    }
}

/// Allocate a fresh `operation_id` and record the operation as pending.
pub fn start(kind: &str, cx: &mut App) -> String {
    init(cx);
    let tracker = cx.global::<OperationTracker>();
    let mut next = tracker.next_id.borrow_mut();
    let id = format!("op-{}", *next);
    *next += 1;
    drop(next);

    tracker.operations.borrow_mut().insert(
        id.clone(),
        OperationState {
            id: id.clone(),
            kind: kind.to_string(),
            status: OperationStatus::Pending,
            progress: OperationProgress::default(),
            result: None,
            error: None,
            started_at: Utc::now(),
            completed_at: None,
            cancellation_requested: false,
        },
    );
    id
}

pub fn record_progress(id: &str, stage: String, percent: Option<u8>, cx: &App) {
    let Some(tracker) = cx.try_global::<OperationTracker>() else {
        return;
    };
    let updated = if let Some(op) = tracker.operations.borrow_mut().get_mut(id) {
        op.progress = OperationProgress {
            stage: stage.clone(),
            percent,
        };
        true
    } else {
        false
    };
    if updated {
        crate::notifications::emit(
            cx,
            "operation_progress",
            serde_json::json!({
                "operation_id": id,
                "stage": stage,
                "percent": percent,
            }),
        );
    }
}

pub fn complete_ok(id: &str, result: serde_json::Value, cx: &App) {
    let Some(tracker) = cx.try_global::<OperationTracker>() else {
        return;
    };
    let updated = if let Some(op) = tracker.operations.borrow_mut().get_mut(id) {
        op.status = OperationStatus::Completed;
        op.result = Some(result.clone());
        op.completed_at = Some(Utc::now());
        true
    } else {
        false
    };
    if updated {
        crate::notifications::emit(
            cx,
            "operation_completed",
            serde_json::json!({
                "operation_id": id,
                "result": { "ok": true, "data": result },
            }),
        );
    }
    gc(cx);
}

pub fn complete_err(id: &str, error: String, cx: &App) {
    let Some(tracker) = cx.try_global::<OperationTracker>() else {
        return;
    };
    let updated = if let Some(op) = tracker.operations.borrow_mut().get_mut(id) {
        op.status = OperationStatus::Failed;
        op.error = Some(error.clone());
        op.completed_at = Some(Utc::now());
        true
    } else {
        false
    };
    if updated {
        crate::notifications::emit(
            cx,
            "operation_completed",
            serde_json::json!({
                "operation_id": id,
                "result": { "ok": false, "error": error },
            }),
        );
    }
    gc(cx);
}

pub fn complete_cancelled(id: &str, cx: &App) {
    let Some(tracker) = cx.try_global::<OperationTracker>() else {
        return;
    };
    let updated = if let Some(op) = tracker.operations.borrow_mut().get_mut(id) {
        op.status = OperationStatus::Cancelled;
        op.completed_at = Some(Utc::now());
        true
    } else {
        false
    };
    if updated {
        crate::notifications::emit(
            cx,
            "operation_completed",
            serde_json::json!({
                "operation_id": id,
                "result": { "ok": false, "error": "cancelled" },
            }),
        );
    }
    gc(cx);
}

pub fn request_cancellation(id: &str, cx: &App) -> bool {
    let Some(tracker) = cx.try_global::<OperationTracker>() else {
        return false;
    };
    let mut ops = tracker.operations.borrow_mut();
    let Some(op) = ops.get_mut(id) else {
        return false;
    };
    if matches!(op.status, OperationStatus::Pending) {
        op.cancellation_requested = true;
        true
    } else {
        false
    }
}

pub fn is_cancelled(id: &str, cx: &App) -> bool {
    let Some(tracker) = cx.try_global::<OperationTracker>() else {
        return false;
    };
    tracker
        .operations
        .borrow()
        .get(id)
        .is_some_and(|op| op.cancellation_requested)
}

pub fn get(id: &str, cx: &App) -> Option<OperationState> {
    let tracker = cx.try_global::<OperationTracker>()?;
    tracker.operations.borrow().get(id).cloned()
}

fn gc(cx: &App) {
    let Some(tracker) = cx.try_global::<OperationTracker>() else {
        return;
    };
    let now = Utc::now();
    let cutoff = chrono::Duration::from_std(OPERATION_RETENTION)
        .unwrap_or_else(|_| chrono::Duration::seconds(300));
    tracker
        .operations
        .borrow_mut()
        .retain(|_, op| match op.completed_at {
            Some(completed) => now - completed < cutoff,
            None => true,
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn start_and_complete_ok(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let id = start("test", cx);
            assert!(id.starts_with("op-"));
            complete_ok(&id, serde_json::json!({"value": 42}), cx);
            let state = get(&id, cx).expect("present");
            assert!(matches!(state.status, OperationStatus::Completed));
            assert_eq!(state.result, Some(serde_json::json!({"value": 42})));
        });
    }

    #[gpui::test]
    async fn cancellation_flow(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let id = start("test", cx);
            assert!(request_cancellation(&id, cx));
            assert!(is_cancelled(&id, cx));
            complete_cancelled(&id, cx);
            let state = get(&id, cx).expect("present");
            assert!(matches!(state.status, OperationStatus::Cancelled));
        });
    }

    #[gpui::test]
    async fn progress_tracking(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let id = start("test", cx);
            record_progress(&id, "stage 1".to_string(), Some(50), cx);
            let state = get(&id, cx).expect("present");
            assert_eq!(state.progress.stage, "stage 1");
            assert_eq!(state.progress.percent, Some(50));
        });
    }
}
