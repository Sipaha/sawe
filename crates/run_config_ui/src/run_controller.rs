use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::anyhow;
use collections::{HashMap, HashSet};
use console_panel::ConsolePanel;
use dap::client::SessionId;
use gpui::{Action as _, Context, Entity, EventEmitter, Subscription, Task, WeakEntity, Window};
use project::Project;
use project::debugger::dap_store::{DapStore, DapStoreEvent};
use run_config::{
    BeforeLaunchStep, Executor, RunConfigId, RunConfigStore, RunConfigStoreEvent, RunRequest,
    RunResolveContext,
};
use terminal::Terminal;
use workspace::Workspace;

/// How long a debug run may sit in `active` with no started session before we
/// assume the adapter died during launch and clear it. Debug adapters can be
/// slow to come up (downloading, building), so this is a generous upper bound,
/// not a tight deadline.
const DEBUG_LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Per-`Workspace` coordinator for run configurations: tracks the selected
/// config (what the toolbar dropdown shows), runs / stops configs, and records
/// which configs are currently running.
pub struct RunController {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    /// The config the toolbar dropdown currently shows. In-memory only for v1.
    // TODO: persist to WorkspaceDb so the selection survives a restart.
    selected: Option<RunConfigId>,
    active: HashMap<RunConfigId, ActiveRun>,
    /// Debug runs whose `DapStore` session id we haven't matched yet, oldest
    /// first. `start_debug_session` doesn't hand back a `SessionId`, so we
    /// can't learn the id directly. Instead, right before each launch we
    /// snapshot the set of session ids that already existed; the session our
    /// launch creates is then the one that's *not* in that snapshot. On each
    /// `DebugClientStarted(id)` we hand `id` to the first pending entry whose
    /// snapshot doesn't contain it (so it's "new" relative to that launch) and
    /// drop that entry. This is robust to two configs sharing a display name:
    /// we never match by label. Residual ambiguity is only the rare race where
    /// the user starts an unrelated debug session in the same tick a config's
    /// debug launch is in flight (same race the old label-matching code had).
    pending_debug_launches: VecDeque<(RunConfigId, HashSet<SessionId>)>,
    /// Monotonic per-`run()` token. Each terminal launch captures one so a
    /// stale poller (from a launch that's since been stopped + re-run) can tell
    /// it's no longer the current launch for its config.
    launch_counter: u64,
    /// Launch tokens of terminal runs that were `stop()`-ed before their
    /// terminal handle resolved. The poller for such a launch, once the handle
    /// arrives, kills the terminal and exits instead of tracking it. Keyed by
    /// token (not config id) so a Stop of a *newer* launch can't make an older
    /// launch's poller kill the newer launch's terminal. Each poller drains its
    /// own token, so this never grows unbounded.
    terminal_launches_pending_kill: HashSet<u64>,
    /// Fire-and-forget tasks that must outlive the `ActiveRun` they relate to:
    /// debug-launch-timeout timers (see `DEBUG_LAUNCH_TIMEOUT`) and terminal
    /// completion pollers that were orphaned by `stop()` (so they can still
    /// kill the terminal once its handle resolves). Each self-completes within
    /// a bounded time; `Task` has no "finished?" probe, so we accept that this
    /// grows by one per run for the life of the workspace window (negligible).
    _detached_tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

pub struct ActiveRun {
    pub config_id: RunConfigId,
    pub executor: Executor,
    pub kind: ActiveRunKind,
}

pub enum ActiveRunKind {
    /// A terminal task. The poller task removes the `ActiveRun` once the
    /// spawned process reports completion. `terminal` is filled in once the
    /// terminal panel has created the task terminal, so Stop can kill it; if
    /// the workspace has no terminal panel wired up (headless test harness) it
    /// stays `None` and Stop only drops the tracking entry.
    Terminal {
        terminal: Option<WeakEntity<Terminal>>,
        /// The `run()` launch token for this run; see `launch_counter`.
        launch_token: u64,
        /// Keeps the completion poller alive while the run is tracked.
        _poller: Option<Task<()>>,
    },
    /// A debug session. `session_id` is filled in from the next
    /// `DapStoreEvent::DebugClientStarted` after launch; Stop shuts down that
    /// specific session, and the entry clears when that session reports it has
    /// shut down.
    Debug { session_id: Option<SessionId> },
}

#[derive(Clone, Debug)]
pub enum RunControllerEvent {
    SelectedChanged,
    ActiveRunsChanged,
}

impl EventEmitter<RunControllerEvent> for RunController {}

impl RunController {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let mut subscriptions = Vec::new();

        let mut selected = None;
        if let Some(store) = RunConfigStore::try_global(cx) {
            selected = store
                .read(cx)
                .configs()
                .first()
                .map(|config| config.id.clone());
            subscriptions.push(cx.subscribe(&store, Self::on_store_event));
        }

        let dap_store = project.read(cx).dap_store();
        subscriptions.push(cx.subscribe(&dap_store, Self::on_dap_store_event));

        // When this controller's workspace window closes, drop our running-set
        // entry from the global store; otherwise configs that were running at
        // close time keep showing as running. `entity_id` values can be reused
        // after release, so this also closes the (tiny) collision window.
        let source = cx.entity_id().as_u64();
        cx.on_release(move |_this, app| {
            if let Some(store) = RunConfigStore::try_global(app) {
                store.update(app, |store, cx| store.clear_running_source(source, cx));
            }
        })
        .detach();

        Self {
            workspace: workspace.weak_handle(),
            project,
            selected,
            active: HashMap::default(),
            pending_debug_launches: VecDeque::new(),
            launch_counter: 0,
            terminal_launches_pending_kill: HashSet::default(),
            _detached_tasks: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    fn on_store_event(
        &mut self,
        store: Entity<RunConfigStore>,
        event: &RunConfigStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            RunConfigStoreEvent::ConfigsChanged => {
                let selected_still_exists = self
                    .selected
                    .as_ref()
                    .map(|id| store.read(cx).config(id).is_some())
                    .unwrap_or(false);
                if self.selected.is_some() && !selected_still_exists {
                    self.selected = None;
                    cx.emit(RunControllerEvent::SelectedChanged);
                }
                if self.selected.is_none() {
                    let first = store
                        .read(cx)
                        .configs()
                        .first()
                        .map(|config| config.id.clone());
                    if let Some(first) = first {
                        self.selected = Some(first);
                        cx.emit(RunControllerEvent::SelectedChanged);
                    }
                }
                cx.notify();
            }
        }
    }

    fn on_dap_store_event(
        &mut self,
        _dap_store: Entity<DapStore>,
        event: &DapStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            DapStoreEvent::DebugClientStarted(session_id) => {
                // Hand the started session to whichever of our pending launches
                // didn't already see it (so it must be the one that launch
                // created). A session we didn't launch (user-initiated
                // debug-panel run) is "new" relative to every pending launch,
                // so it could be mis-claimed if a launch is in flight at the
                // same time — but that's a far narrower race than two configs
                // sharing a name (the bug this replaced), and unavoidable
                // without an id handed back from `start_debug_session`.
                let Some(config_id) =
                    claim_started_session(&mut self.pending_debug_launches, *session_id)
                else {
                    return;
                };
                if let Some(ActiveRun {
                    kind: ActiveRunKind::Debug { session_id: slot },
                    ..
                }) = self.active.get_mut(&config_id)
                {
                    *slot = Some(*session_id);
                }
            }
            DapStoreEvent::DebugClientShutdown(session_id) => {
                let finished: Option<RunConfigId> = self
                    .active
                    .iter()
                    .find(|(_, run)| {
                        matches!(
                            run.kind,
                            ActiveRunKind::Debug { session_id: Some(id) } if id == *session_id
                        )
                    })
                    .map(|(id, _)| id.clone());
                if let Some(config_id) = finished {
                    self.active.remove(&config_id);
                    cx.emit(RunControllerEvent::ActiveRunsChanged);
                    self.publish_running(cx);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    /// The project this controller's workspace is bound to. Used by the
    /// run-config strip to resolve the solution-wide active member's worktree.
    pub fn project(&self) -> &Entity<Project> {
        &self.project
    }

    /// Re-validate the current selection against the set of ids visible for the
    /// solution-wide active member. If the selected config is no longer in
    /// `allowed_ids` (e.g. the active project changed and the selection belonged
    /// to a different project), reselect the first allowed id, or clear the
    /// selection if `allowed_ids` is empty. Mirrors the reselect path in
    /// `on_store_event` (ConfigsChanged).
    pub fn revalidate_selection_against(
        &mut self,
        allowed_ids: &[RunConfigId],
        cx: &mut Context<Self>,
    ) {
        let selected_allowed = self
            .selected
            .as_ref()
            .map(|id| allowed_ids.contains(id))
            .unwrap_or(false);
        if selected_allowed {
            return;
        }
        let new_selection = allowed_ids.first().cloned();
        if new_selection != self.selected {
            self.selected = new_selection;
            cx.emit(RunControllerEvent::SelectedChanged);
            cx.notify();
        }
    }

    // --- selection ---

    pub fn selected_id(&self) -> Option<&RunConfigId> {
        self.selected.as_ref()
    }

    pub fn select(&mut self, id: RunConfigId, cx: &mut Context<Self>) {
        if self.selected.as_ref() != Some(&id) {
            self.selected = Some(id);
            cx.emit(RunControllerEvent::SelectedChanged);
            cx.notify();
        }
    }

    pub fn select_next(&mut self, cx: &mut Context<Self>) {
        let Some(store) = RunConfigStore::try_global(cx) else {
            return;
        };
        let configs = store.read(cx).configs();
        if configs.is_empty() {
            return;
        }
        let next_index = self
            .selected
            .as_ref()
            .and_then(|selected| configs.iter().position(|config| &config.id == selected))
            .map(|index| (index + 1) % configs.len())
            .unwrap_or(0);
        self.select(configs[next_index].id.clone(), cx);
    }

    // --- active runs ---

    /// Push the current set of running config ids into the global store so that
    /// non-UI consumers (the toolbar strip, MCP `run_config.list`) see them.
    /// Keyed by this controller's entity id so multiple workspace windows don't
    /// overwrite each other's running state.
    fn publish_running(&self, cx: &mut Context<Self>) {
        let source = cx.entity_id().as_u64();
        let ids: collections::HashSet<RunConfigId> = self.active.keys().cloned().collect();
        if let Some(store) = RunConfigStore::try_global(cx) {
            store.update(cx, |store, cx| store.set_running(source, ids, cx));
        }
    }

    pub fn is_running(&self, id: &RunConfigId) -> bool {
        self.active.contains_key(id)
    }

    pub fn active_runs(&self) -> impl Iterator<Item = &ActiveRun> + '_ {
        self.active.values()
    }

    // --- run / stop ---

    pub fn run(
        &mut self,
        config_id: RunConfigId,
        executor: Executor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_running(&config_id) {
            self.stop(&config_id, cx);
        }

        let Some(store) = RunConfigStore::try_global(cx) else {
            self.notify_error("Run configurations are not available".into(), cx);
            return;
        };
        let Some(config) = store.read(cx).config(&config_id) else {
            self.notify_error("That run configuration no longer exists".into(), cx);
            return;
        };
        let Some(provider) = store.read(cx).provider(&config.provider_type) else {
            self.notify_error(
                format!("No provider for type `{}`", config.provider_type),
                cx,
            );
            return;
        };
        if !config.executors.contains(&executor) {
            self.notify_error(
                format!("`{}` does not support {executor:?}", config.name),
                cx,
            );
            return;
        }
        let config_name = config.name.clone();

        for step in &config.before_launch {
            match step {
                BeforeLaunchStep::SaveAllFiles => {
                    // `Workspace::save_all` is a private action handler; dispatch
                    // the action so the workspace runs its own save-all logic.
                    // Fire-and-forget: we don't block the run on the save.
                    window.dispatch_action(
                        workspace::SaveAll {
                            save_intent: Some(workspace::SaveIntent::SaveAll),
                        }
                        .boxed_clone(),
                        cx,
                    );
                }
            }
        }

        let worktree = self.project.read(cx).worktrees(cx).next();
        let worktree_root: Option<PathBuf> = worktree
            .as_ref()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf());
        let worktree_id = worktree.as_ref().map(|worktree| worktree.read(cx).id());

        let mut task_variables = task::TaskVariables::default();
        if let Some(root) = worktree_root.as_ref() {
            task_variables.insert(
                task::VariableName::WorktreeRoot,
                root.to_string_lossy().into_owned(),
            );
            if let Some(name) = root.file_name().and_then(|name| name.to_str()) {
                task_variables.insert(task::VariableName::Dirname, name.to_string());
            }
        }
        let task_context = task::TaskContext {
            cwd: worktree_root.clone(),
            task_variables,
            project_env: HashMap::default(),
        };

        let mut resolve_context = RunResolveContext {
            project: self.project.clone(),
            worktree_id,
            worktree_root,
            task_context: task_context.clone(),
        };

        let request = match provider.resolve(&config, executor, &mut resolve_context, cx) {
            Ok(request) => request,
            Err(err) => {
                self.notify_error(format!("{err:#}"), cx);
                return;
            }
        };

        self.launch_counter += 1;
        let launch_token = self.launch_counter;

        match request {
            RunRequest::Terminal(spawn) => {
                let Some(workspace) = self.workspace.upgrade() else {
                    return;
                };

                let poller =
                    if let Some(terminal_panel) = workspace.read(cx).panel::<ConsolePanel>(cx) {
                        // Real path: the terminal panel hands back the task
                        // terminal so Stop can kill it.
                        let spawn_task = terminal_panel.update(cx, |terminal_panel, cx| {
                            terminal_panel.spawn_task(&spawn, window, cx)
                        });
                        let poller_config_id = config_id.clone();
                        cx.spawn(async move |this, cx| {
                            match spawn_task.await {
                                Ok(terminal) => {
                                    // If Stop was pressed while the handle was in
                                    // flight, kill the terminal now and don't track
                                    // it (the `ActiveRun` was already removed by
                                    // `stop()`).
                                    let killed = this
                                        .update(cx, |this, cx| {
                                            if !this
                                                .terminal_launches_pending_kill
                                                .remove(&launch_token)
                                            {
                                                return false;
                                            }
                                            if let Some(terminal) = terminal.upgrade() {
                                                terminal.update(cx, |terminal, _| {
                                                    terminal.kill_active_task()
                                                });
                                            }
                                            true
                                        })
                                        .unwrap_or(false);
                                    if killed {
                                        return;
                                    }
                                    this.update(cx, |this, _| {
                                        if let Some(ActiveRun {
                                            kind: ActiveRunKind::Terminal { terminal: slot, .. },
                                            ..
                                        }) = this.active.get_mut(&poller_config_id)
                                        {
                                            *slot = Some(terminal.clone());
                                        }
                                    })
                                    .ok();
                                    let completion = terminal
                                        .read_with(cx, |terminal, cx| {
                                            terminal.wait_for_completed_task(cx)
                                        })
                                        .ok();
                                    if let Some(completion) = completion {
                                        completion.await;
                                    }
                                }
                                Err(err) => {
                                    this.update(cx, |this, _| {
                                        this.terminal_launches_pending_kill.remove(&launch_token);
                                    })
                                    .ok();
                                    log::warn!(
                                        "run_config: terminal task `{}` failed to launch: {err:#}",
                                        poller_config_id.as_str()
                                    );
                                    this.update(cx, |this, cx| {
                                        this.notify_error(
                                            format!("Failed to launch run configuration: {err:#}"),
                                            cx,
                                        );
                                    })
                                    .ok();
                                }
                            }
                            this.update(cx, |this, cx| {
                                if this.active.remove(&poller_config_id).is_some() {
                                    cx.emit(RunControllerEvent::ActiveRunsChanged);
                                    this.publish_running(cx);
                                    cx.notify();
                                }
                            })
                            .ok();
                        })
                    } else {
                        // Fallback (no terminal panel, e.g. headless tests): we get
                        // only an exit-status future, no killable handle.
                        let spawn_task = workspace.update(cx, |workspace, cx| {
                            workspace.spawn_in_terminal(spawn, window, cx)
                        });
                        let poller_config_id = config_id.clone();
                        cx.spawn(async move |this, cx| {
                            // `Some(_)` => the process actually exited or failed to
                            // launch; `None` => the spawn was cancelled / no
                            // terminal provider — leave the run tracked so the user
                            // can Stop it explicitly.
                            let result = spawn_task.await;
                            // Drain any pending-kill token recorded by `stop()`;
                            // there's no killable handle on this path, so this just
                            // keeps the set from growing.
                            this.update(cx, |this, _| {
                                this.terminal_launches_pending_kill.remove(&launch_token);
                            })
                            .ok();
                            let Some(result) = result else {
                                return;
                            };
                            if let Err(err) = &result {
                                log::warn!(
                                    "run_config: terminal task `{}` failed to launch: {err:#}",
                                    poller_config_id.as_str()
                                );
                                this.update(cx, |this, cx| {
                                    this.notify_error(
                                        format!("Failed to launch run configuration: {err:#}"),
                                        cx,
                                    );
                                })
                                .ok();
                            }
                            this.update(cx, |this, cx| {
                                if this.active.remove(&poller_config_id).is_some() {
                                    cx.emit(RunControllerEvent::ActiveRunsChanged);
                                    this.publish_running(cx);
                                    cx.notify();
                                }
                            })
                            .ok();
                        })
                    };

                self.active.insert(
                    config_id.clone(),
                    ActiveRun {
                        config_id,
                        executor,
                        kind: ActiveRunKind::Terminal {
                            terminal: None,
                            launch_token,
                            _poller: Some(poller),
                        },
                    },
                );
                cx.emit(RunControllerEvent::ActiveRunsChanged);
                self.publish_running(cx);
                cx.notify();
            }
            RunRequest::Debug(scenario) => {
                let Some(workspace) = self.workspace.upgrade() else {
                    return;
                };
                // Snapshot the sessions that already exist so we can later
                // recognise the one this launch creates as "the new one".
                let prior_sessions: HashSet<SessionId> = self
                    .project
                    .read(cx)
                    .dap_store()
                    .read(cx)
                    .sessions()
                    .map(|session| session.read(cx).session_id())
                    .collect();
                workspace.update(cx, |workspace, cx| {
                    workspace.start_debug_session(
                        scenario,
                        task_context.into(),
                        None,
                        worktree_id,
                        window,
                        cx,
                    );
                });
                self.pending_debug_launches
                    .push_back((config_id.clone(), prior_sessions));
                self.active.insert(
                    config_id.clone(),
                    ActiveRun {
                        config_id: config_id.clone(),
                        executor,
                        kind: ActiveRunKind::Debug { session_id: None },
                    },
                );

                // If the adapter dies before `DapStoreEvent::DebugClientStarted`
                // ever fires, no `DebugClientShutdown` will follow either, so
                // the entry would be stuck "running" forever. Clear it after a
                // grace period if it never got a session id (i.e. is still in
                // `pending_debug_launches`). A run that *did* start a session,
                // or was already stopped, makes this a no-op.
                let timeout_config_id = config_id.clone();
                let timer = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(DEBUG_LAUNCH_TIMEOUT).await;
                    this.update(cx, |this, cx| {
                        if !debug_launch_timed_out(
                            &this.active,
                            &this.pending_debug_launches,
                            &timeout_config_id,
                        ) {
                            return;
                        }
                        this.active.remove(&timeout_config_id);
                        this.pending_debug_launches
                            .retain(|(pending, _)| pending != &timeout_config_id);
                        log::warn!(
                            "run_config: debug config `{config_name}` did not start a session \
                             within {}s; clearing",
                            DEBUG_LAUNCH_TIMEOUT.as_secs()
                        );
                        cx.emit(RunControllerEvent::ActiveRunsChanged);
                        this.publish_running(cx);
                        cx.notify();
                    })
                    .ok();
                });
                self._detached_tasks.push(timer);

                cx.emit(RunControllerEvent::ActiveRunsChanged);
                self.publish_running(cx);
                cx.notify();
            }
        }
    }

    pub fn rerun(
        &mut self,
        config_id: RunConfigId,
        executor: Executor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop(&config_id, cx);
        self.run(config_id, executor, window, cx);
    }

    pub fn stop(&mut self, id: &RunConfigId, cx: &mut Context<Self>) {
        let Some(run) = self.active.remove(id) else {
            return;
        };
        match run.kind {
            ActiveRunKind::Terminal {
                terminal,
                launch_token,
                _poller,
            } => {
                if let Some(terminal) = terminal.and_then(|terminal| terminal.upgrade()) {
                    // We already have the task terminal: kill it now; dropping
                    // `_poller` then cancels the (no-longer-needed) completion
                    // watcher.
                    terminal.update(cx, |terminal, _| terminal.kill_active_task());
                } else {
                    // The terminal handle hasn't resolved yet (mid-launch). The
                    // poller is the only thing that will ever get the handle, so
                    // it must outlive this `ActiveRun` — move it to
                    // `_detached_tasks` instead of dropping it — and flag its
                    // launch token so it kills the terminal (rather than
                    // tracking it) once the handle arrives. On the no-terminal-
                    // panel fallback path the poller has nothing to kill, but
                    // keeping it alive is harmless and it still drains its token.
                    self.terminal_launches_pending_kill.insert(launch_token);
                    if let Some(poller) = _poller {
                        self._detached_tasks.push(poller);
                    }
                }
            }
            ActiveRunKind::Debug { session_id } => {
                self.pending_debug_launches
                    .retain(|(pending, _)| pending != id);
                if let Some(session_id) = session_id {
                    let dap_store = self.project.read(cx).dap_store();
                    dap_store
                        .update(cx, |dap_store, cx| {
                            dap_store.shutdown_session(session_id, cx)
                        })
                        .detach_and_log_err(cx);
                }
            }
        }
        cx.emit(RunControllerEvent::ActiveRunsChanged);
        self.publish_running(cx);
        cx.notify();
    }

    pub fn stop_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<RunConfigId> = self.active.keys().cloned().collect();
        for id in ids {
            self.stop(&id, cx);
        }
    }

    fn notify_error(&self, message: String, cx: &mut Context<Self>) {
        log::error!("run configuration error: {message}");
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.show_error(&anyhow!(message), cx);
            });
        }
    }
}

/// Match a just-started debug session against the oldest pending launch that
/// hadn't already seen it (its snapshot doesn't contain `started`), removing
/// that entry and returning its config id. Returns `None` if every pending
/// launch already knew about `started` (so it isn't ours) or the queue is
/// empty. Pulled out as a free function so the matching logic can be unit
/// tested without a live DAP adapter.
fn claim_started_session(
    pending: &mut VecDeque<(RunConfigId, HashSet<SessionId>)>,
    started: SessionId,
) -> Option<RunConfigId> {
    let position = pending
        .iter()
        .position(|(_, prior_sessions)| !prior_sessions.contains(&started))?;
    pending.remove(position).map(|(config_id, _)| config_id)
}

/// Whether the debug run for `config_id` should be treated as a failed launch:
/// it's still tracked, still has no session id, and is still pending (so its
/// session never got claimed by `DebugClientStarted`). Pulled out as a free
/// function so the decision can be unit tested without a live DAP adapter.
fn debug_launch_timed_out(
    active: &HashMap<RunConfigId, ActiveRun>,
    pending: &VecDeque<(RunConfigId, HashSet<SessionId>)>,
    config_id: &RunConfigId,
) -> bool {
    let still_unstarted = matches!(
        active.get(config_id).map(|run| &run.kind),
        Some(ActiveRunKind::Debug { session_id: None })
    );
    let still_pending = pending.iter().any(|(pending, _)| pending == config_id);
    still_unstarted && still_pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use gpui::{App, AppContext as _, TestAppContext};
    use project::Project;
    use run_config::{ConfigScope, RunConfigProvider, RunConfiguration};
    use std::future;
    use std::process::ExitStatus;
    use ui::IconName;
    use workspace::{AppState, Workspace};

    struct MockProvider;

    impl RunConfigProvider for MockProvider {
        fn type_id(&self) -> &'static str {
            "mock"
        }
        fn display_name(&self) -> &'static str {
            "Mock"
        }
        fn icon(&self) -> IconName {
            IconName::Terminal
        }
        fn supported_executors(&self) -> &'static [Executor] {
            &[Executor::Run]
        }
        fn settings_schema(&self) -> schemars::Schema {
            schemars::json_schema!({ "type": "object" })
        }
        fn new_template(&self, _cx: &App) -> serde_json::Value {
            serde_json::json!({})
        }
        fn resolve(
            &self,
            _config: &RunConfiguration,
            _executor: Executor,
            _cx: &mut RunResolveContext,
            _app: &App,
        ) -> Result<RunRequest> {
            Ok(RunRequest::Terminal(task::SpawnInTerminal {
                command: Some("true".into()),
                ..Default::default()
            }))
        }
    }

    /// Like `MockProvider` but its `resolve` returns a `RunRequest::Debug`, so
    /// `run` goes through the debug arm. With no `debugger_provider` wired up in
    /// the test `Workspace`, `start_debug_session` is a no-op and no
    /// `DebugClientStarted` ever fires — exactly the "adapter never started"
    /// case the launch timeout exists for.
    struct MockDebugProvider;

    impl RunConfigProvider for MockDebugProvider {
        fn type_id(&self) -> &'static str {
            "mock_debug"
        }
        fn display_name(&self) -> &'static str {
            "Mock Debug"
        }
        fn icon(&self) -> IconName {
            IconName::Debug
        }
        fn supported_executors(&self) -> &'static [Executor] {
            &[Executor::Debug]
        }
        fn settings_schema(&self) -> schemars::Schema {
            schemars::json_schema!({ "type": "object" })
        }
        fn new_template(&self, _cx: &App) -> serde_json::Value {
            serde_json::json!({})
        }
        fn resolve(
            &self,
            _config: &RunConfiguration,
            _executor: Executor,
            _cx: &mut RunResolveContext,
            _app: &App,
        ) -> Result<RunRequest> {
            Ok(RunRequest::Debug(task::DebugScenario {
                adapter: "mock-adapter".into(),
                label: "mock debug".into(),
                build: None,
                config: serde_json::json!({}),
                tcp_connection: None,
            }))
        }
    }

    /// A `TerminalProvider` whose spawned task never completes, so a `run`
    /// stays in the `active` set until the controller is told to `stop`.
    struct PendingTerminalProvider;

    impl workspace::TerminalProvider for PendingTerminalProvider {
        fn spawn(
            &self,
            _task: task::SpawnInTerminal,
            _window: &mut Window,
            cx: &mut App,
        ) -> Task<Option<Result<ExitStatus>>> {
            cx.background_executor()
                .spawn(async { future::pending::<Option<Result<ExitStatus>>>().await })
        }
    }

    fn mock_config(name: &str) -> RunConfiguration {
        RunConfiguration {
            id: RunConfigId::from_raw(format!("mock:{name}")),
            name: name.into(),
            provider_type: "mock".into(),
            settings: serde_json::json!({}),
            executors: vec![Executor::Run],
            before_launch: vec![],
            folder: None,
            scope: ConfigScope::Global,
        }
    }

    fn mock_debug_config(name: &str) -> RunConfiguration {
        RunConfiguration {
            id: RunConfigId::from_raw(format!("mock_debug:{name}")),
            name: name.into(),
            provider_type: "mock_debug".into(),
            settings: serde_json::json!({}),
            executors: vec![Executor::Debug],
            before_launch: vec![],
            folder: None,
            scope: ConfigScope::Global,
        }
    }

    async fn setup(cx: &mut TestAppContext, configs: &[&str]) -> Entity<Workspace> {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            cx.set_global(db::AppDatabase::test_new());
            editor::init(cx);
            RunConfigStore::init_global(cx);
            run_config::register_provider(cx, MockProvider);
            run_config::register_provider(cx, MockDebugProvider);
            app_state
        });
        let store = cx.update(|cx| RunConfigStore::global(cx));
        for name in configs {
            store.update(cx, |store, cx| store.upsert(mock_config(name), cx));
        }
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, _cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        workspace
    }

    #[gpui::test]
    async fn select_next_cycles(cx: &mut TestAppContext) {
        let workspace = setup(cx, &["a", "b"]).await;
        let controller = workspace.update(cx, |workspace, cx| {
            cx.new(|cx| RunController::new(workspace, cx))
        });
        cx.run_until_parked();

        controller.read_with(cx, |controller, _| {
            assert_eq!(
                controller.selected_id().map(RunConfigId::as_str),
                Some("mock:a"),
                "first config is auto-selected"
            );
        });

        controller.update(cx, |controller, cx| controller.select_next(cx));
        controller.read_with(cx, |controller, _| {
            assert_eq!(
                controller.selected_id().map(RunConfigId::as_str),
                Some("mock:b")
            );
        });

        controller.update(cx, |controller, cx| controller.select_next(cx));
        controller.read_with(cx, |controller, _| {
            assert_eq!(
                controller.selected_id().map(RunConfigId::as_str),
                Some("mock:a")
            );
        });
    }

    #[gpui::test]
    async fn run_then_stop_tracks_state(cx: &mut TestAppContext) {
        let workspace = setup(cx, &["a"]).await;
        workspace.update(cx, |workspace, _| {
            workspace.set_terminal_provider(PendingTerminalProvider)
        });
        let controller = workspace.update(cx, |workspace, cx| {
            cx.new(|cx| RunController::new(workspace, cx))
        });
        cx.run_until_parked();

        let id = RunConfigId::from_raw("mock:a");
        let window = cx
            .update(|cx| cx.windows().first().copied())
            .expect("a window exists");

        window
            .update(cx, |_, window, cx| {
                controller.update(cx, |controller, cx| {
                    controller.run(id.clone(), Executor::Run, window, cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        controller.read_with(cx, |controller, _| {
            assert!(
                controller.is_running(&id),
                "run should be tracked as active"
            );
        });

        controller.update(cx, |controller, cx| controller.stop(&id, cx));
        controller.read_with(cx, |controller, _| {
            assert!(
                !controller.is_running(&id),
                "stop should clear the active run"
            );
        });
    }

    #[gpui::test]
    async fn stop_during_terminal_launch_window_records_pending_kill(cx: &mut TestAppContext) {
        // On the no-terminal-panel fallback path the `ActiveRun` always has
        // `terminal: None`, so `stop()` exercises the "handle hasn't resolved
        // yet" branch: it must flag the launch token for kill-on-arrival and
        // keep the poller alive (move it to `_detached_tasks`) rather than
        // dropping it.
        let workspace = setup(cx, &["a"]).await;
        workspace.update(cx, |workspace, _| {
            workspace.set_terminal_provider(PendingTerminalProvider)
        });
        let controller = workspace.update(cx, |workspace, cx| {
            cx.new(|cx| RunController::new(workspace, cx))
        });
        cx.run_until_parked();

        let id = RunConfigId::from_raw("mock:a");
        let window = cx
            .update(|cx| cx.windows().first().copied())
            .expect("a window exists");
        window
            .update(cx, |_, window, cx| {
                controller.update(cx, |controller, cx| {
                    controller.run(id.clone(), Executor::Run, window, cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        let detached_before =
            controller.read_with(cx, |controller, _| controller._detached_tasks.len());

        controller.update(cx, |controller, cx| controller.stop(&id, cx));
        controller.read_with(cx, |controller, _| {
            assert!(!controller.is_running(&id), "stop clears the active run");
            assert_eq!(
                controller.terminal_launches_pending_kill.len(),
                1,
                "stop during the launch window records the launch token for kill-on-arrival"
            );
            assert_eq!(
                controller._detached_tasks.len(),
                detached_before + 1,
                "the completion poller is kept alive (not dropped) so it can still kill the \
                 terminal once the handle resolves"
            );
        });
    }

    #[gpui::test]
    async fn debug_launch_clears_after_timeout(cx: &mut TestAppContext) {
        // No `debugger_provider` is wired up, so `start_debug_session` is a
        // no-op and `DebugClientStarted` never fires. The run should clear
        // itself once `DEBUG_LAUNCH_TIMEOUT` elapses.
        let workspace = setup(cx, &[]).await;
        let store = cx.update(|cx| RunConfigStore::global(cx));
        store.update(cx, |store, cx| store.upsert(mock_debug_config("d"), cx));
        let controller = workspace.update(cx, |workspace, cx| {
            cx.new(|cx| RunController::new(workspace, cx))
        });
        cx.run_until_parked();

        let id = RunConfigId::from_raw("mock_debug:d");
        let window = cx
            .update(|cx| cx.windows().first().copied())
            .expect("a window exists");
        window
            .update(cx, |_, window, cx| {
                controller.update(cx, |controller, cx| {
                    controller.run(id.clone(), Executor::Debug, window, cx)
                })
            })
            .unwrap();
        cx.run_until_parked();
        controller.read_with(cx, |controller, _| {
            assert!(
                controller.is_running(&id),
                "debug run is tracked while it launches"
            );
        });

        cx.executor()
            .advance_clock(DEBUG_LAUNCH_TIMEOUT + Duration::from_secs(1));
        cx.run_until_parked();
        controller.read_with(cx, |controller, _| {
            assert!(
                !controller.is_running(&id),
                "a debug run whose adapter never started a session is cleared after the timeout"
            );
            assert!(
                controller.pending_debug_launches.is_empty(),
                "the pending-launch bookkeeping is dropped too"
            );
        });
    }

    #[test]
    fn debug_launch_timed_out_predicate() {
        let debug_id = RunConfigId::from_raw("mock_debug:d");
        let terminal_id = RunConfigId::from_raw("mock:t");

        // Still unstarted (no session id) and still pending => timed out.
        let mut active: HashMap<RunConfigId, ActiveRun> = HashMap::default();
        active.insert(
            debug_id.clone(),
            ActiveRun {
                config_id: debug_id.clone(),
                executor: Executor::Debug,
                kind: ActiveRunKind::Debug { session_id: None },
            },
        );
        let pending: VecDeque<(RunConfigId, HashSet<SessionId>)> =
            VecDeque::from(vec![(debug_id.clone(), HashSet::default())]);
        assert!(debug_launch_timed_out(&active, &pending, &debug_id));

        // Got a session id in the meantime => not timed out (even if somehow
        // still in `pending`).
        active.insert(
            debug_id.clone(),
            ActiveRun {
                config_id: debug_id.clone(),
                executor: Executor::Debug,
                kind: ActiveRunKind::Debug {
                    session_id: Some(SessionId(1)),
                },
            },
        );
        assert!(!debug_launch_timed_out(&active, &pending, &debug_id));

        // No pending entry (claimed already / stopped) => not timed out.
        active.insert(
            debug_id.clone(),
            ActiveRun {
                config_id: debug_id.clone(),
                executor: Executor::Debug,
                kind: ActiveRunKind::Debug { session_id: None },
            },
        );
        assert!(!debug_launch_timed_out(
            &active,
            &VecDeque::new(),
            &debug_id
        ));

        // Not even tracked any more => not timed out.
        assert!(!debug_launch_timed_out(
            &HashMap::default(),
            &pending,
            &debug_id
        ));

        // A terminal run with the same id shape is never "timed out" by this.
        let mut terminal_active: HashMap<RunConfigId, ActiveRun> = HashMap::default();
        terminal_active.insert(
            terminal_id.clone(),
            ActiveRun {
                config_id: terminal_id.clone(),
                executor: Executor::Run,
                kind: ActiveRunKind::Terminal {
                    terminal: None,
                    launch_token: 1,
                    _poller: None,
                },
            },
        );
        let terminal_pending: VecDeque<(RunConfigId, HashSet<SessionId>)> =
            VecDeque::from(vec![(terminal_id.clone(), HashSet::default())]);
        assert!(!debug_launch_timed_out(
            &terminal_active,
            &terminal_pending,
            &terminal_id
        ));
    }

    #[gpui::test]
    async fn dropping_controller_clears_running_source(cx: &mut TestAppContext) {
        let workspace = setup(cx, &["a"]).await;
        workspace.update(cx, |workspace, _| {
            workspace.set_terminal_provider(PendingTerminalProvider)
        });
        let controller = workspace.update(cx, |workspace, cx| {
            cx.new(|cx| RunController::new(workspace, cx))
        });
        cx.run_until_parked();

        let id = RunConfigId::from_raw("mock:a");
        let window = cx
            .update(|cx| cx.windows().first().copied())
            .expect("a window exists");
        window
            .update(cx, |_, window, cx| {
                controller.update(cx, |controller, cx| {
                    controller.run(id.clone(), Executor::Run, window, cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        let store = cx.update(|cx| RunConfigStore::global(cx));
        store.read_with(cx, |store, _| {
            assert!(
                store.is_running(&id),
                "run is published to the global store"
            );
        });

        drop(controller);
        // Entity release (and thus the `on_release` handler) runs during the
        // next effect flush, not from the `Rc` drop itself.
        cx.update(|_| {});
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert!(
                !store.is_running(&id),
                "dropping the controller clears its running set"
            );
        });
    }

    #[test]
    fn claim_started_session_matches_by_novelty_not_label() {
        let alpha = RunConfigId::from_raw("debug:same-name");
        let beta = RunConfigId::from_raw("debug:same-name-2");

        // Two of our debug launches are in flight. They have *identical*
        // scenario labels, so label matching would be ambiguous — but their
        // session-id snapshots differ: `alpha` launched first (snapshot empty),
        // then `beta` launched after `alpha`'s session (id 1) already existed.
        let mut pending: VecDeque<(RunConfigId, HashSet<SessionId>)> = VecDeque::from(vec![
            (alpha.clone(), HashSet::default()),
            (beta.clone(), HashSet::from_iter([SessionId(1)])),
        ]);

        // Session 1 started: it's new only relative to `alpha`'s snapshot, so
        // `alpha` claims it (even though `beta` is also pending, and even
        // though both share a label).
        assert_eq!(
            claim_started_session(&mut pending, SessionId(1)),
            Some(alpha)
        );
        // Session 2 started: now `beta` is the only pending launch, and 2 is
        // new relative to its snapshot, so `beta` claims it.
        assert_eq!(
            claim_started_session(&mut pending, SessionId(2)),
            Some(beta)
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn claim_started_session_ignores_already_known_session() {
        let config = RunConfigId::from_raw("debug:run");
        // The only pending launch already had session 7 in its snapshot, so a
        // `DebugClientStarted(7)` can't be ours — leave the queue untouched.
        let mut pending: VecDeque<(RunConfigId, HashSet<SessionId>)> =
            VecDeque::from(vec![(config, HashSet::from_iter([SessionId(7)]))]);
        assert_eq!(claim_started_session(&mut pending, SessionId(7)), None);
        assert_eq!(pending.len(), 1);
    }
}
