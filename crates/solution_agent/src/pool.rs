use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use futures::future::Shared;
use gpui::Task;

use acp_thread::AgentConnection;
use solutions::SolutionId;

use crate::model::AgentServerId;

/// Production-side synthetic-Project construction stub. Task 3.3 leaves
/// production wiring as an explicit error: `create_session` accepts an
/// `Entity<Project>` from the caller (typically the active workspace), and
/// the synthetic-project path is reserved for a follow-up that decides
/// whether sessions get a dedicated worktree or share the workspace project.
#[cfg(not(any(feature = "test-support", test)))]
#[allow(dead_code)]
pub fn make_production_project_for_solution(
    _solution_root: &std::path::Path,
    _cx: &mut gpui::App,
) -> anyhow::Result<gpui::Entity<project::Project>> {
    anyhow::bail!(
        "solution_agent: synthetic Project construction is not yet wired in production; \
         pass an existing Entity<Project> to create_session instead"
    )
}

pub(crate) const SHUTDOWN_DEBOUNCE: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub(crate) enum SpawnState {
    Pending(Shared<Task<Result<Rc<dyn AgentConnection>, std::sync::Arc<anyhow::Error>>>>),
    Ready(Rc<dyn AgentConnection>),
    // The wrapped error is captured but never inspected today — kept for
    // future use (e.g. surfacing to the UI). Discriminant-only matching is
    // intentional, hence the allow.
    #[allow(dead_code)]
    Failed(std::sync::Arc<anyhow::Error>),
}

pub(crate) struct PooledConnection {
    pub(crate) state: SpawnState,
    pub(crate) live_session_count: usize,
    pub(crate) shutdown_task: Option<Task<()>>,
}

pub(crate) struct SubprocessPool {
    entries: HashMap<(SolutionId, AgentServerId), PooledConnection>,
}

impl SubprocessPool {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub fn entry_mut(
        &mut self,
        key: &(SolutionId, AgentServerId),
    ) -> Option<&mut PooledConnection> {
        self.entries.get_mut(key)
    }

    pub fn insert(&mut self, key: (SolutionId, AgentServerId), entry: PooledConnection) {
        self.entries.insert(key, entry);
    }

    pub fn remove(&mut self, key: &(SolutionId, AgentServerId)) {
        self.entries.remove(key);
    }

    #[allow(dead_code)]
    pub fn keys_for_solution<'a>(
        &'a self,
        solution_id: &'a SolutionId,
    ) -> impl Iterator<Item = (SolutionId, AgentServerId)> + 'a {
        self.entries
            .keys()
            .filter(move |(s, _)| s == solution_id)
            .cloned()
    }

    #[cfg(any(feature = "test-support", test))]
    pub fn pair_count(&self) -> usize {
        self.entries.len()
    }
}
