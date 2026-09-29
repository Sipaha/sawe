//! Per-session throttler for `workspace.session_metrics_changed`.
//!
//! These metrics (last_activity_at, total_tokens, max_tokens) change
//! frequently — every assistant message, every token-usage update. The
//! mobile workspace screen wants them live for the active screen but
//! cannot afford an emit per change.
//!
//! Contract:
//! - Server emits at most one notification per session per ~2 seconds.
//! - An emit that falls inside the window is not dropped: the NEWEST payload
//!   is held and sent when the window closes. Dropping it lost the last
//!   value of every burst — a run of file reads followed by a long command
//!   left the phone's meter at the burst's first figure (3% against 16%)
//!   until the next token update, which a long command never produces.
//! - The notification carries NO `seq` field and does NOT participate
//!   in the workspace.* gap-detection protocol on the client.

use crate::model::SolutionSessionId;
use gpui::App;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const THROTTLE: Duration = Duration::from_millis(2000);
const NOTIFICATION: &str = "workspace.session_metrics_changed";

#[derive(Default, Clone)]
pub struct MetricsEmitter {
    pub(crate) inner: Arc<Mutex<EmitterState>>,
}

#[derive(Default)]
pub(crate) struct EmitterState {
    pub(crate) last_emit: HashMap<SolutionSessionId, Instant>,
    /// The newest payload that arrived inside the window, sent when it closes.
    pub(crate) pending: HashMap<SolutionSessionId, serde_json::Value>,
}

impl MetricsEmitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Emit a `workspace.session_metrics_changed` notification now if the
    /// per-session window has elapsed; otherwise hold `payload` (replacing any
    /// older held one) and send it when the window closes.
    pub fn emit_if_ready(
        &self,
        cx: &App,
        session_id: &SolutionSessionId,
        payload: serde_json::Value,
    ) {
        let now = Instant::now();
        let mut state = self.inner.lock();
        if let Some(last) = state.last_emit.get(session_id).copied() {
            let elapsed = now.duration_since(last);
            if elapsed < THROTTLE {
                // One trailing send per window; later payloads only replace
                // what it will carry.
                if state.pending.insert(*session_id, payload).is_none() {
                    self.send_when_window_closes(cx, *session_id, THROTTLE - elapsed);
                }
                return;
            }
        }
        state.last_emit.insert(*session_id, now);
        state.pending.remove(session_id);
        drop(state);
        editor_mcp::emit_notification(cx, NOTIFICATION, payload);
    }

    fn send_when_window_closes(&self, cx: &App, session_id: SolutionSessionId, delay: Duration) {
        let this = self.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(delay).await;
            cx.update(|cx| {
                let payload = {
                    let mut state = this.inner.lock();
                    let payload = state.pending.remove(&session_id);
                    if payload.is_some() {
                        state.last_emit.insert(session_id, Instant::now());
                    }
                    payload
                };
                if let Some(payload) = payload {
                    editor_mcp::emit_notification(cx, NOTIFICATION, payload);
                }
            });
        })
        .detach();
    }

    /// Drop the throttle bookkeeping for a closed session. Without this the
    /// maps keep one entry per session FOREVER — a slow but unbounded leak
    /// over the editor's lifetime as sessions open and close across a
    /// multi-day run. Called from
    /// `SolutionAgentStore::evict_session_runtime_maps`.
    pub fn clear_session(&self, session_id: &SolutionSessionId) {
        let mut state = self.inner.lock();
        state.last_emit.remove(session_id);
        state.pending.remove(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn payload(sid: &SolutionSessionId, total: u64) -> serde_json::Value {
        serde_json::json!({ "session_id": sid.to_string(), "total_tokens": total })
    }

    #[gpui::test]
    async fn a_throttled_emit_is_held_and_sent_when_the_window_closes(cx: &mut TestAppContext) {
        let emitter = MetricsEmitter::new();
        let sid = SolutionSessionId::new();
        cx.update(|cx| {
            emitter.emit_if_ready(cx, &sid, payload(&sid, 1));
            emitter.emit_if_ready(cx, &sid, payload(&sid, 2));
            emitter.emit_if_ready(cx, &sid, payload(&sid, 3));
        });
        assert_eq!(
            emitter.inner.lock().pending.get(&sid),
            Some(&payload(&sid, 3)),
            "inside the window only the newest value is held"
        );
        let first = *emitter.inner.lock().last_emit.get(&sid).unwrap();

        cx.executor().advance_clock(THROTTLE);
        cx.run_until_parked();
        let state = emitter.inner.lock();
        assert!(state.pending.is_empty(), "the held value went out");
        assert!(state.last_emit[&sid] > first, "and opened a new window");
    }

    #[gpui::test]
    async fn emit_if_ready_allows_different_sessions(cx: &mut TestAppContext) {
        let emitter = MetricsEmitter::new();
        let sid_a = SolutionSessionId::new();
        let sid_b = SolutionSessionId::new();
        cx.update(|cx| {
            emitter.emit_if_ready(cx, &sid_a, payload(&sid_a, 1));
            emitter.emit_if_ready(cx, &sid_b, payload(&sid_b, 1));
        });
        let state = emitter.inner.lock();
        assert!(state.last_emit.contains_key(&sid_a));
        assert!(state.last_emit.contains_key(&sid_b));
        assert!(state.pending.is_empty(), "neither was throttled");
    }
}
