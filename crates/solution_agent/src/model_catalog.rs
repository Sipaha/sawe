//! Per-agent model/effort catalog extracted out of `SolutionAgentStore`.
//!
//! `ModelCatalog` owns the *global* model state that is shared across all of an
//! agent's sessions — the last-known model list per agent and the in-flight
//! probe-dedup set — plus the fixed effort options. The store's orchestration
//! methods (which also touch sessions, persist rows, and emit store events)
//! stay on `SolutionAgentStore` but route their catalog-state access through
//! here, so the probe-dedup invariant and the map ownership live in one place.

use std::collections::{HashMap, HashSet};

use claude_native::ModelInfo;

use crate::model::AgentServerId;

/// The fixed effort options offered in the UI (no per-agent list — these are
/// Claude Code's effort levels; `ultracode` = "xhigh + workflows").
pub const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultracode"];

/// Global per-agent model catalog. Owned by `SolutionAgentStore`.
#[derive(Default)]
pub struct ModelCatalog {
    /// Last-known model list per agent, shared across that agent's sessions so
    /// a fresh session (no turn yet → empty per-session list) still offers a
    /// model picker. Filled on the first live capture and by a probe at create.
    agent_models: HashMap<AgentServerId, Vec<ModelInfo>>,
    /// Agents with an in-flight `ensure_agent_models` probe (dedupe).
    agent_models_probing: HashSet<AgentServerId>,
}

impl ModelCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached model list for `agent_id`, or an empty list when none is
    /// known yet.
    pub fn models_for(&self, agent_id: &AgentServerId) -> Vec<ModelInfo> {
        self.agent_models.get(agent_id).cloned().unwrap_or_default()
    }

    /// Record (or replace) the model list for `agent_id`.
    pub fn set_models(&mut self, agent_id: AgentServerId, models: Vec<ModelInfo>) {
        self.agent_models.insert(agent_id, models);
    }

    /// Decide whether a fresh probe should run for `agent_id`, atomically
    /// claiming the probe slot when it should. Returns `false` (no probe) when a
    /// non-empty list is already cached or a probe is already in flight;
    /// otherwise marks a probe as running and returns `true`.
    pub fn begin_probe_if_needed(&mut self, agent_id: &AgentServerId) -> bool {
        let already_known = self
            .agent_models
            .get(agent_id)
            .map_or(false, |m| !m.is_empty());
        if already_known || self.agent_models_probing.contains(agent_id) {
            return false;
        }
        self.agent_models_probing.insert(agent_id.clone());
        true
    }

    /// Mark the in-flight probe for `agent_id` as finished.
    pub fn end_probe(&mut self, agent_id: &AgentServerId) {
        self.agent_models_probing.remove(agent_id);
    }
}
