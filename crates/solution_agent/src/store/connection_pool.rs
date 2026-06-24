//! Pool / lifecycle management for shared `AgentConnection`s.
//!
//! The store collapses N sessions for the same `(SolutionId,
//! AgentServerId)` pair onto a single subprocess via the
//! `SubprocessPool`. This module owns the spawn / await-pending /
//! retry-after-failure path (`get_or_spawn_connection`) plus the
//! reference-counting hooks (`pool_release_session`, `arm_shutdown`)
//! that drop the subprocess after the last session closes.

use std::rc::Rc;

use anyhow::{Result, anyhow};
use futures::FutureExt;
use futures::future::Shared;
use gpui::{AsyncApp, Context, Entity, Task};
use solutions::{Solution, SolutionId};

use super::SolutionAgentStore;
use crate::model::AgentServerId;
use crate::pool::{PooledConnection, SHUTDOWN_DEBOUNCE, SpawnState};

impl SolutionAgentStore {
    /// Pool-aware lookup: returns the existing connection, awaits an
    /// in-flight spawn, drops a previously failed entry and retries, or
    /// kicks off a new spawn. Always increments `live_session_count` so
    /// callers must pair this with `pool_release_session` on session close.
    pub(super) fn get_or_spawn_connection(
        &mut self,
        pair: (SolutionId, AgentServerId),
        _solution: &Solution,
        project: Entity<project::Project>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Rc<dyn acp_thread::AgentConnection>>> {
        // Phase 1: short critical section over the pool. Either we observe
        // an existing entry (Ready / Pending / Failed) or we hold no entry
        // and proceed to spawn.
        {
            let mut pool = self.pool.lock();
            if let Some(entry) = pool.entry_mut(&pair) {
                entry.shutdown_task = None;
                entry.live_session_count += 1;
                match &entry.state {
                    SpawnState::Ready(connection) => {
                        return Task::ready(Ok(connection.clone()));
                    }
                    SpawnState::Pending(shared) => {
                        let shared = shared.clone();
                        return cx
                            .foreground_executor()
                            .spawn(async move { shared.await.map_err(|e| anyhow!("{e}")) });
                    }
                    SpawnState::Failed(_) => {
                        // Drop the failed entry and fall through to a fresh spawn.
                        // We've already bumped live_session_count; reset it so
                        // remove() leaves a clean slate.
                        entry.live_session_count = 0;
                        pool.remove(&pair);
                    }
                }
            }
        }

        // Phase 2: look up the registered AgentServer. If absent, return
        // an error without inserting a Failed pool entry — callers should be
        // able to retry once the server is registered.
        let server = match self.server_registry.get(&pair.1).cloned() {
            Some(server) => server,
            None => {
                return Task::ready(Err(anyhow!(
                    "no AgentServer registered for id {:?}",
                    pair.1
                )));
            }
        };

        // Phase 3: kick off connect() on the foreground executor (AgentServer
        // is `!Send` and so are its returned `Rc<dyn AgentConnection>`s; the
        // pool lives on the foreground thread).
        let pair_for_task = pair.clone();
        // AgentServerDelegate requires an `Entity<AgentServerStore>` — we
        // get one from the project. This is the same coupling documented on
        // `create_session`.
        let agent_server_store = project.read(cx).agent_server_store().clone();
        let delegate = agent_servers::AgentServerDelegate::new(agent_server_store, None, None);
        let project_for_connect = project;
        let server_for_connect = server;

        let task: Shared<
            Task<Result<Rc<dyn acp_thread::AgentConnection>, std::sync::Arc<anyhow::Error>>>,
        > = cx
            .spawn(async move |this, cx: &mut AsyncApp| {
                let connect_task =
                    cx.update(|cx| server_for_connect.connect(delegate, project_for_connect, cx));
                let result_for_pool: Result<
                    Rc<dyn acp_thread::AgentConnection>,
                    std::sync::Arc<anyhow::Error>,
                > = connect_task.await.map_err(std::sync::Arc::new);

                // Promote pool state to Ready/Failed once the spawn resolves.
                let _ = this.update(cx, |store, _| {
                    let mut pool = store.pool.lock();
                    if let Some(entry) = pool.entry_mut(&pair_for_task) {
                        entry.state = match &result_for_pool {
                            Ok(connection) => SpawnState::Ready(connection.clone()),
                            Err(err) => SpawnState::Failed(err.clone()),
                        };
                    }
                });
                result_for_pool
            })
            .shared();

        // Phase 4: insert a Pending entry holding the shared task.
        {
            let mut pool = self.pool.lock();
            pool.insert(
                pair.clone(),
                PooledConnection {
                    state: SpawnState::Pending(task.clone()),
                    live_session_count: 1,
                    shutdown_task: None,
                },
            );
        }

        cx.foreground_executor()
            .spawn(async move { task.await.map_err(|e| anyhow!("{e}")) })
    }

    /// Decrement the pool entry's live-session count. When it reaches
    /// zero, arm a debounced shutdown so a quick "close → reopen" round-
    /// trip doesn't pay the subprocess respawn cost.
    pub fn pool_release_session(
        &mut self,
        key: (SolutionId, AgentServerId),
        cx: &mut Context<Self>,
    ) {
        let needs_arm = {
            let mut pool = self.pool.lock();
            let Some(entry) = pool.entry_mut(&key) else {
                return;
            };
            entry.live_session_count = entry.live_session_count.saturating_sub(1);
            entry.live_session_count == 0
        };
        if needs_arm {
            self.arm_shutdown(key, cx);
        }
    }

    fn arm_shutdown(&mut self, key: (SolutionId, AgentServerId), cx: &mut Context<Self>) {
        let task = cx.spawn({
            let key = key.clone();
            async move |this, cx: &mut AsyncApp| {
                cx.background_executor().timer(SHUTDOWN_DEBOUNCE).await;
                this.update(cx, |this, _cx| {
                    let mut pool = this.pool.lock();
                    if let Some(entry) = pool.entry_mut(&key) {
                        if entry.live_session_count == 0 {
                            pool.remove(&key);
                        }
                    }
                })
                .ok();
            }
        });
        let mut pool = self.pool.lock();
        if let Some(entry) = pool.entry_mut(&key) {
            entry.shutdown_task = Some(task);
        }
    }

    /// Test-only: pretend a session was added against an existing connection.
    #[cfg(any(feature = "test-support", test))]
    pub fn pool_pretend_session_added(
        &mut self,
        key: (SolutionId, AgentServerId),
        connection: std::rc::Rc<dyn acp_thread::AgentConnection>,
    ) {
        let mut pool = self.pool.lock();
        if let Some(entry) = pool.entry_mut(&key) {
            entry.live_session_count += 1;
            entry.shutdown_task = None;
        } else {
            pool.insert(
                key,
                PooledConnection {
                    state: SpawnState::Ready(connection),
                    live_session_count: 1,
                    shutdown_task: None,
                },
            );
        }
    }

    #[cfg(any(feature = "test-support", test))]
    pub fn pool_size(&self) -> usize {
        self.pool.lock().pair_count()
    }
}
