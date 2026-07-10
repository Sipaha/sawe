//! Teammate-watcher state extracted out of `SolutionAgentStore` (survey cluster
//! C10 — the background-agent / background-shell watching subsystem).
//!
//! `TeammateWatchers` owns the three C10-local maps: the per-session managed-
//! agent JSONL watcher tasks, the per-session background-shell `.output` watcher
//! tasks, and the forward-only parent-JSONL scan cursors. Dropping a watcher
//! `Task` cancels it; the scan offsets are lazily initialised to a file's
//! current EOF so completions are only ever observed FORWARD from editor launch.
//!
//! The orchestration methods that arm these watchers, tail the files, mutate
//! session entities, and emit `SolutionAgentStoreEvent`s stay on
//! `SolutionAgentStore` — they are intrinsically `&mut Store`-coupled (they read
//! `sessions`, spawn on `Context<Store>`, and emit store events). Only the map
//! STATE and its access invariants (the arm-once guards, the forward-only cursor
//! lifecycle) live here, so the ownership of that state is in one place and the
//! coordinator no longer names the raw fields. The `#7`/`#9` watchdog hardening
//! semantics are entirely in the Store-side methods and are unaffected by this
//! state relocation.

use std::collections::HashMap;

use gpui::Task;

use crate::model::SolutionSessionId;

/// Per-session watcher tasks + parent-JSONL scan cursors. Owned by
/// `SolutionAgentStore` (constructed in `new_in_app`).
#[derive(Default)]
pub(crate) struct TeammateWatchers {
    /// One per-session background-agent watcher task — alive as long as the
    /// session has >=1 registered `background_agents`. Stored as `Task<()>` so
    /// dropping kills the watcher cleanly. Armed by
    /// `SolutionAgentStore::ensure_background_agent_watcher`.
    background_agent_watchers: HashMap<SolutionSessionId, Task<()>>,
    /// One per-session background-shell watcher task — alive as long as the
    /// session has >=1 registered `background_shells`. Stored as `Task<()>` so
    /// dropping kills the watcher cleanly. Armed by
    /// `SolutionAgentStore::ensure_background_shell_watcher`. Structurally
    /// identical to `background_agent_watchers` (separate map so the two
    /// pipelines arm / cancel independently).
    background_shell_watchers: HashMap<SolutionSessionId, Task<()>>,
    /// Forward-only scan cursor into each session's PARENT session JSONL
    /// transcript, used by `scan_parent_jsonl_for_completions` to detect
    /// `<task-notification>` completion lines on the 1 Hz tick. Lazily
    /// initialised to the file's CURRENT length the first time a session is
    /// scanned — so we only observe completions FORWARD from editor launch and
    /// never re-flip shells off historical notifications. Cleared for a session
    /// once it has no `background_shells`, so a future shell re-arms from the
    /// then-current EOF.
    parent_jsonl_scan_offsets: HashMap<SolutionSessionId, u64>,
}

impl TeammateWatchers {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// True when a background-agent watcher is already armed for `session_id`
    /// (the arm-once guard — a second `ensure_background_agent_watcher` is a
    /// no-op).
    pub(crate) fn has_agent_watcher(&self, session_id: SolutionSessionId) -> bool {
        self.background_agent_watchers.contains_key(&session_id)
    }

    /// Store the background-agent watcher task for `session_id`.
    pub(crate) fn arm_agent_watcher(&mut self, session_id: SolutionSessionId, task: Task<()>) {
        self.background_agent_watchers.insert(session_id, task);
    }

    /// True when a background-shell watcher is already armed for `session_id`.
    pub(crate) fn has_shell_watcher(&self, session_id: SolutionSessionId) -> bool {
        self.background_shell_watchers.contains_key(&session_id)
    }

    /// Store the background-shell watcher task for `session_id`.
    pub(crate) fn arm_shell_watcher(&mut self, session_id: SolutionSessionId, task: Task<()>) {
        self.background_shell_watchers.insert(session_id, task);
    }

    /// The current parent-JSONL scan cursor for `session_id`, or `None` when the
    /// session hasn't been scanned yet (first-sight lazy-init is the caller's
    /// responsibility via [`Self::set_scan_offset`]).
    pub(crate) fn scan_offset(&self, session_id: SolutionSessionId) -> Option<u64> {
        self.parent_jsonl_scan_offsets.get(&session_id).copied()
    }

    /// Set / advance the parent-JSONL scan cursor for `session_id`.
    pub(crate) fn set_scan_offset(&mut self, session_id: SolutionSessionId, offset: u64) {
        self.parent_jsonl_scan_offsets.insert(session_id, offset);
    }

    /// Drop the parent-JSONL scan cursor for `session_id` so a future shell
    /// re-arms it from the then-current EOF.
    pub(crate) fn clear_scan_offset(&mut self, session_id: SolutionSessionId) {
        self.parent_jsonl_scan_offsets.remove(&session_id);
    }

    /// True when a parent-JSONL scan cursor is recorded for `session_id`.
    /// (Test-only observability of the forward-only cursor lifecycle.)
    #[cfg(test)]
    pub(crate) fn has_scan_offset(&self, session_id: SolutionSessionId) -> bool {
        self.parent_jsonl_scan_offsets.contains_key(&session_id)
    }

    /// Forget every watcher + cursor for `session_id` (hard session teardown).
    /// Dropping the two `Task`s cancels the watchers.
    pub(crate) fn forget_session(&mut self, session_id: SolutionSessionId) {
        self.background_agent_watchers.remove(&session_id);
        self.background_shell_watchers.remove(&session_id);
        self.parent_jsonl_scan_offsets.remove(&session_id);
    }
}
