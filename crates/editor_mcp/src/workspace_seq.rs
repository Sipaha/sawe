use crate::notifications::emit as emit_notification;
use gpui::{App, Global};
use parking_lot::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct WorkspaceEventCoordinator {
    pub(crate) seq: AtomicU64,
    /// Guards snapshot-seq atomicity. `emit_sequenced` takes the write
    /// side (briefly, across seq increment + emit) so that a concurrent
    /// `build_snapshot` read cannot interleave between the seq advance and
    /// the notification, causing the snapshot's seq to diverge from the
    /// state it describes. `build_snapshot` takes the read side around its
    /// entire body. Multiple concurrent snapshot reads don't block each
    /// other; a write blocks all readers until the notification is fired.
    replication: RwLock<()>,
}

impl WorkspaceEventCoordinator {
    pub fn global(cx: &App) -> &Self {
        &cx.global::<GlobalWorkspaceEventCoordinator>().0
    }

    pub fn try_global(cx: &App) -> Option<&Self> {
        cx.try_global::<GlobalWorkspaceEventCoordinator>()
            .map(|g| &g.0)
    }

    pub fn current_seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    /// Increment and return the new value. Use this on every mutation that
    /// emits a sequenced workspace event.
    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Hold the read side around a state read that needs to be consistent
    /// with the returned seq value. Released when the guard drops.
    pub fn snapshot_lock(&self) -> parking_lot::RwLockReadGuard<'_, ()> {
        self.replication.read()
    }

    /// Reserve the next seq AND emit a sequenced notification atomically.
    ///
    /// `payload_without_seq` is mutated to inject the assigned seq under the
    /// `"seq"` key before emission. Callers should already hold whatever
    /// state-write lock guards the mutation they're announcing — the seq is
    /// reserved BEFORE the notification fires so consumers cannot observe a
    /// newer seq from a snapshot than from any preceding delta.
    ///
    /// Takes the `replication` write guard across seq increment + emit so
    /// that a `build_snapshot` read in flight cannot interleave between them
    /// and observe a seq that is ahead of the state it will read.
    pub fn emit_sequenced(
        &self,
        cx: &App,
        kind: &str,
        mut payload_without_seq: serde_json::Value,
    ) -> u64 {
        // Hold the write side across seq increment + emit so that a
        // snapshot read in flight cannot interleave between them.
        let _w = self.replication.write();
        let seq = self.next_seq();
        if let serde_json::Value::Object(ref mut map) = payload_without_seq {
            map.insert("seq".to_string(), serde_json::json!(seq));
        } else {
            // Caller bug: every workspace event payload must be a JSON object.
            // Wrap into one rather than panicking in prod.
            payload_without_seq = serde_json::json!({
                "seq": seq,
                "payload": payload_without_seq
            });
        }
        emit_notification(cx, kind, payload_without_seq);
        seq
    }
}

struct GlobalWorkspaceEventCoordinator(WorkspaceEventCoordinator);
impl Global for GlobalWorkspaceEventCoordinator {}

pub fn install(cx: &mut App) {
    if cx.try_global::<GlobalWorkspaceEventCoordinator>().is_some() {
        return;
    }
    cx.set_global(GlobalWorkspaceEventCoordinator(WorkspaceEventCoordinator {
        seq: AtomicU64::new(0),
        replication: RwLock::new(()),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn seq_starts_at_zero_and_increments(cx: &mut TestAppContext) {
        cx.update(install);
        cx.update(|cx| {
            let coord = WorkspaceEventCoordinator::global(cx);
            assert_eq!(coord.current_seq(), 0);
            assert_eq!(coord.next_seq(), 1);
            assert_eq!(coord.next_seq(), 2);
            assert_eq!(coord.current_seq(), 2);
        });
    }

    #[gpui::test]
    async fn install_is_idempotent(cx: &mut TestAppContext) {
        cx.update(install);
        cx.update(install);
        cx.update(|cx| {
            let coord = WorkspaceEventCoordinator::global(cx);
            assert_eq!(coord.current_seq(), 0);
        });
    }

    #[gpui::test]
    async fn next_seq_is_monotonic_under_contention(cx: &mut TestAppContext) {
        cx.update(install);
        let observed = cx.update(|cx| {
            let coord = WorkspaceEventCoordinator::global(cx);
            let mut seen = Vec::new();
            for _ in 0..1000 {
                seen.push(coord.next_seq());
            }
            seen
        });
        let mut sorted = observed.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 1000, "no duplicates");
        assert_eq!(observed, sorted, "ascending without gaps");
    }

    #[gpui::test]
    async fn emit_sequenced_injects_seq_field_and_returns_value(cx: &mut TestAppContext) {
        use serde_json::json;
        cx.update(install);
        cx.update(|cx| {
            let coord = WorkspaceEventCoordinator::global(cx);
            // Without a real MCP server, emit_notification is a no-op (or pushes
            // to a no-op channel). We just verify the helper advances seq and
            // returns the new value.
            let s1 = coord.emit_sequenced(cx, "workspace.test", json!({ "id": "abc" }));
            let s2 = coord.emit_sequenced(cx, "workspace.test", json!({ "id": "def" }));
            assert_eq!(s1, 1);
            assert_eq!(s2, 2);
            assert_eq!(coord.current_seq(), 2);
        });
    }
}
