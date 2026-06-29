//! Per-session throttler for `workspace.session_metrics_changed`.
//!
//! These metrics (last_activity_at, total_tokens, max_tokens) change
//! frequently — every assistant message, every token-usage update. The
//! mobile workspace screen wants them live for the active screen but
//! cannot afford an emit per change.
//!
//! Contract:
//! - Server emits at most one notification per session per ~2 seconds.
//! - Skipped emits are NOT made up later — the next refetch (snapshot
//!   resync) provides ground truth.
//! - The notification carries NO `seq` field and does NOT participate
//!   in the workspace.* gap-detection protocol on the client.

use crate::model::SolutionSessionId;
use gpui::App;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::Instant;

const THROTTLE_MS: u64 = 2000;

#[derive(Default)]
pub struct MetricsEmitter {
    pub(crate) last_emit: Mutex<HashMap<SolutionSessionId, Instant>>,
}

impl MetricsEmitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Emit a `workspace.session_metrics_changed` notification if the
    /// per-session throttle window has elapsed since the last emit.
    /// Caller supplies the JSON payload (without `seq`).
    pub fn emit_if_ready(
        &self,
        cx: &App,
        session_id: &SolutionSessionId,
        payload: serde_json::Value,
    ) {
        let now = Instant::now();
        let mut last = self.last_emit.lock();
        if let Some(t) = last.get(session_id) {
            if now.duration_since(*t).as_millis() < THROTTLE_MS as u128 {
                return;
            }
        }
        last.insert(*session_id, now);
        editor_mcp::emit_notification(cx, "workspace.session_metrics_changed", payload);
    }

    /// Drop the throttle bookkeeping for a closed session. Without this the
    /// `last_emit` map keeps one `(SolutionSessionId, Instant)` entry per
    /// session FOREVER — a slow but unbounded leak over the editor's lifetime
    /// as sessions open and close across a multi-day run. Called from
    /// `SolutionAgentStore::evict_session_runtime_maps`.
    pub fn clear_session(&self, session_id: &SolutionSessionId) {
        self.last_emit.lock().remove(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn emit_if_ready_throttles_repeat_calls_for_same_session(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let emitter = MetricsEmitter::new();
            let sid = SolutionSessionId::new();
            // First emit goes through (no panic, no return-early). We can't
            // easily observe the actual notification without a real MCP
            // server, but we CAN observe the `last_emit` map state.
            emitter.emit_if_ready(
                cx,
                &sid,
                serde_json::json!({ "session_id": sid.to_string() }),
            );
            assert!(emitter.last_emit.lock().contains_key(&sid));
            let first_time = *emitter.last_emit.lock().get(&sid).unwrap();
            // Immediate second emit should be throttled — last_emit unchanged.
            emitter.emit_if_ready(
                cx,
                &sid,
                serde_json::json!({ "session_id": sid.to_string() }),
            );
            let second_time = *emitter.last_emit.lock().get(&sid).unwrap();
            assert_eq!(first_time, second_time, "throttle within window");
        });
    }

    #[gpui::test]
    async fn emit_if_ready_allows_different_sessions(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let emitter = MetricsEmitter::new();
            let sid_a = SolutionSessionId::new();
            let sid_b = SolutionSessionId::new();
            emitter.emit_if_ready(
                cx,
                &sid_a,
                serde_json::json!({ "session_id": sid_a.to_string() }),
            );
            emitter.emit_if_ready(
                cx,
                &sid_b,
                serde_json::json!({ "session_id": sid_b.to_string() }),
            );
            assert!(emitter.last_emit.lock().contains_key(&sid_a));
            assert!(emitter.last_emit.lock().contains_key(&sid_b));
        });
    }
}
