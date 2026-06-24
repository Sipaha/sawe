use std::sync::Arc;

use collections::HashMap;
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, Task, WeakEntity,
};
use project::{Project, Worktree, WorktreeId};
use settings::watch_config_file;
use util::ResultExt as _;

use crate::file_format;
use crate::model::{ConfigScope, Executor, RunConfigId, RunConfiguration};
use crate::provider::{ArcProvider, RunConfigProvider};

/// A request from an MCP tool (which lives in `run_config`, can't depend on
/// `run_config_ui`) for "the active workspace's `RunController`" to act.
#[derive(Clone, Debug)]
pub enum RunCommand {
    Run { id: RunConfigId, executor: Executor },
    Stop { id: RunConfigId },
    Select { id: RunConfigId },
}

type CommandSink = Arc<dyn Fn(RunCommand, &mut App) + Send + Sync>;

pub struct RunConfigStore {
    providers: HashMap<&'static str, ArcProvider>,
    /// Persisted configs keyed by id. Project + Global scope.
    persisted: HashMap<RunConfigId, RunConfiguration>,
    /// Ephemeral discovered configs keyed by id. Rebuilt by `refresh_discovered`.
    ephemeral: HashMap<RunConfigId, RunConfiguration>,
    /// Insertion order for stable dropdown listing.
    order: Vec<RunConfigId>,
    /// Project handle captured by `watch_project`, for `save_to_disk` later & re-discovery.
    project: Option<WeakEntity<Project>>,
    fs: Option<Arc<dyn fs::Fs>>,
    /// Configs loaded from the global `run-configurations.json`.
    global_configs: Vec<RunConfiguration>,
    /// Configs loaded from each worktree's `.sawe/run-configurations.json`.
    worktree_configs: HashMap<WorktreeId, Vec<RunConfiguration>>,
    /// Running config ids keyed by the source controller's entity id (as u64).
    /// Each `RunController` owns its own entry; `is_running` unions across all sources.
    running_by_source: collections::HashMap<u64, collections::HashSet<RunConfigId>>,
    /// Sink for `RunCommand`s, registered by `run_config_ui` (routes to the
    /// active workspace's `RunController`). `None` until the UI installs it.
    command_sink: Option<CommandSink>,
    /// Live FS watcher tasks (dropped → watchers stop).
    _watchers: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Debug)]
pub enum RunConfigStoreEvent {
    ConfigsChanged,
}

impl EventEmitter<RunConfigStoreEvent> for RunConfigStore {}

struct GlobalRunConfigStore(Entity<RunConfigStore>);
impl Global for GlobalRunConfigStore {}

impl RunConfigStore {
    fn empty() -> Self {
        RunConfigStore {
            providers: HashMap::default(),
            persisted: HashMap::default(),
            ephemeral: HashMap::default(),
            order: Vec::new(),
            project: None,
            fs: None,
            global_configs: Vec::new(),
            worktree_configs: HashMap::default(),
            running_by_source: collections::HashMap::default(),
            command_sink: None,
            _watchers: Vec::new(),
            _subscriptions: Vec::new(),
        }
    }

    pub fn init_global(cx: &mut App) {
        let store = cx.new(|_| RunConfigStore::empty());
        cx.set_global(GlobalRunConfigStore(store));
    }

    pub fn global(cx: &App) -> Entity<RunConfigStore> {
        cx.global::<GlobalRunConfigStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<RunConfigStore>> {
        cx.try_global::<GlobalRunConfigStore>().map(|g| g.0.clone())
    }

    // --- provider registry ---
    pub fn register_provider(&mut self, provider: impl RunConfigProvider) {
        let provider: ArcProvider = Arc::new(provider);
        self.providers.insert(provider.type_id(), provider);
    }

    pub fn provider(&self, type_id: &str) -> Option<ArcProvider> {
        self.providers.get(type_id).cloned()
    }

    pub fn providers(&self) -> impl Iterator<Item = &ArcProvider> + '_ {
        self.providers.values()
    }

    /// The project handle captured by `watch_project`, if still alive.
    pub fn project(&self) -> Option<Entity<Project>> {
        self.project.as_ref().and_then(|project| project.upgrade())
    }

    // --- config set ---
    /// All configs (persisted in insertion order, then ephemeral sorted by name).
    pub fn configs(&self) -> Vec<RunConfiguration> {
        let mut out: Vec<RunConfiguration> = self
            .order
            .iter()
            .filter_map(|id| self.persisted.get(id).cloned())
            .collect();
        let mut ephemeral: Vec<_> = self.ephemeral.values().cloned().collect();
        ephemeral.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(ephemeral);
        out
    }

    pub fn config(&self, id: &RunConfigId) -> Option<RunConfiguration> {
        self.persisted
            .get(id)
            .or_else(|| self.ephemeral.get(id))
            .cloned()
    }

    // --- running set ---

    /// Replace the set of currently-running config ids for the given source
    /// controller (identified by its entity id). `RunController`s push their
    /// own slice; `is_running` unions across all sources so multiple workspace
    /// windows don't overwrite each other.
    pub fn set_running(
        &mut self,
        source: u64,
        ids: collections::HashSet<RunConfigId>,
        cx: &mut Context<Self>,
    ) {
        let changed = self
            .running_by_source
            .get(&source)
            .map(|existing| existing != &ids)
            .unwrap_or(true);
        if changed {
            self.running_by_source.insert(source, ids);
            cx.notify();
            cx.emit(RunConfigStoreEvent::ConfigsChanged);
        }
    }

    /// Drop the running set for a source controller (called from the
    /// `RunController`'s release handler when its workspace window closes).
    /// Without this, configs that were running when a window closed mid-run
    /// would keep showing as running forever.
    pub fn clear_running_source(&mut self, source: u64, cx: &mut Context<Self>) {
        if self.running_by_source.remove(&source).is_some() {
            cx.notify();
            cx.emit(RunConfigStoreEvent::ConfigsChanged);
        }
    }

    pub fn is_running(&self, id: &RunConfigId) -> bool {
        self.running_by_source.values().any(|set| set.contains(id))
    }

    pub fn running_ids(&self) -> impl Iterator<Item = &RunConfigId> + '_ {
        self.running_by_source.values().flatten()
    }

    // --- command sink ---

    /// Register the sink that routes `RunCommand`s to the active workspace's
    /// `RunController`. Called once from `run_config_ui::init`.
    pub fn set_command_sink(&mut self, sink: CommandSink) {
        self.command_sink = Some(sink);
    }

    /// Dispatch a run command via the registered sink. Returns `false` (no-op)
    /// if no sink has been registered (e.g. headless).
    pub fn dispatch_command(cx: &mut App, command: RunCommand) -> bool {
        let Some(store) = Self::try_global(cx) else {
            return false;
        };
        let sink = store.read(cx).command_sink.clone();
        match sink {
            Some(sink) => {
                sink(command, cx);
                true
            }
            None => false,
        }
    }

    /// Replace the full set of persisted configs (called by the Edit modal's
    /// `apply` with its full list of non-ephemeral drafts). Rebuilds the
    /// per-source buckets from `configs` so that an interleaved watcher-fired
    /// `rebuild_persisted` is a no-op rather than reverting this edit.
    ///
    /// Worktrees that previously had a bucket but now have zero configs keep an
    /// empty `Vec` entry so `save_to_disk` knows to rewrite their file empty.
    pub fn set_persisted(&mut self, configs: Vec<RunConfiguration>, cx: &mut Context<Self>) {
        let mut global_configs: Vec<RunConfiguration> = Vec::new();
        let mut worktree_configs: HashMap<WorktreeId, Vec<RunConfiguration>> = HashMap::default();
        // Preserve existing worktree keys (possibly emptied) so files that lost
        // all their configs still get rewritten with an empty document.
        for worktree_id in self.worktree_configs.keys() {
            worktree_configs.entry(*worktree_id).or_default();
        }
        for config in configs {
            match &config.scope {
                ConfigScope::Global => global_configs.push(config),
                ConfigScope::Project { worktree } => {
                    worktree_configs.entry(*worktree).or_default().push(config);
                }
                ConfigScope::Ephemeral => {}
            }
        }
        self.global_configs = global_configs;
        self.worktree_configs = worktree_configs;
        self.rebuild_persisted(cx);
    }

    /// Insert/update one persisted config (used by the `run_config.create` MCP
    /// tool). Patches the matching per-source bucket then rebuilds `persisted`.
    pub fn upsert(&mut self, config: RunConfiguration, cx: &mut Context<Self>) {
        debug_assert!(config.scope.is_persisted());
        let target_worktree = match &config.scope {
            ConfigScope::Global => None,
            ConfigScope::Project { worktree } => Some(*worktree),
            ConfigScope::Ephemeral => {
                debug_assert!(false, "upsert called with an ephemeral config");
                return;
            }
        };
        // Remove any existing config with this id from every bucket (its scope
        // may have changed), then push it into the target bucket.
        self.global_configs
            .retain(|existing| existing.id != config.id);
        for bucket in self.worktree_configs.values_mut() {
            bucket.retain(|existing| existing.id != config.id);
        }
        match target_worktree {
            None => self.global_configs.push(config),
            Some(worktree) => self
                .worktree_configs
                .entry(worktree)
                .or_default()
                .push(config),
        }
        self.rebuild_persisted(cx);
    }

    pub fn remove(&mut self, id: &RunConfigId, cx: &mut Context<Self>) -> Option<RunConfiguration> {
        let mut removed: Option<RunConfiguration> = None;
        if let Some(position) = self
            .global_configs
            .iter()
            .position(|config| &config.id == id)
        {
            removed = Some(self.global_configs.remove(position));
        }
        for bucket in self.worktree_configs.values_mut() {
            if let Some(position) = bucket.iter().position(|config| &config.id == id) {
                removed = Some(bucket.remove(position));
            }
        }
        if removed.is_some() {
            self.rebuild_persisted(cx);
        }
        removed
    }

    /// Re-run every provider's `discover` and replace the ephemeral set.
    pub fn refresh_discovered(&mut self, project: &Entity<Project>, cx: &mut Context<Self>) {
        let providers: Vec<ArcProvider> = self.providers.values().cloned().collect();
        let mut next = HashMap::default();
        for provider in providers {
            for config in provider.discover(project, cx) {
                debug_assert!(matches!(config.scope, ConfigScope::Ephemeral));
                next.insert(config.id.clone(), config);
            }
        }
        if next != self.ephemeral {
            self.ephemeral = next;
            cx.emit(RunConfigStoreEvent::ConfigsChanged);
            cx.notify();
        }
    }

    // --- file watching ---

    /// Rebuild `persisted` + `order` from the per-source buckets, then notify.
    fn rebuild_persisted(&mut self, cx: &mut Context<Self>) {
        self.persisted.clear();
        self.order.clear();
        let mut insert = |config: &RunConfiguration| {
            if !self.persisted.contains_key(&config.id) {
                self.order.push(config.id.clone());
            }
            self.persisted.insert(config.id.clone(), config.clone());
        };
        for config in &self.global_configs {
            insert(config);
        }
        // Iterate worktree buckets in a stable order (by id) for deterministic listing.
        let mut worktree_ids: Vec<WorktreeId> = self.worktree_configs.keys().copied().collect();
        worktree_ids.sort_by_key(|id| id.to_usize());
        for worktree_id in worktree_ids {
            if let Some(configs) = self.worktree_configs.get(&worktree_id) {
                for config in configs {
                    insert(config);
                }
            }
        }
        cx.emit(RunConfigStoreEvent::ConfigsChanged);
        cx.notify();
    }

    fn spawn_global_watch(&mut self, cx: &mut Context<Self>) {
        let Some(fs) = self.fs.clone() else {
            return;
        };
        let path = paths::run_configurations_file().clone();
        let task = cx.spawn(async move |this, cx| {
            let (mut contents_rx, _watcher) = watch_config_file(cx.background_executor(), fs, path);
            while let Some(text) = contents_rx.next().await {
                let parsed = parse_text(&text, ConfigScope::Global);
                if this
                    .update(cx, |this, cx| {
                        this.global_configs = parsed;
                        this.rebuild_persisted(cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        self._watchers.push(task);
    }

    fn spawn_worktree_watch(&mut self, worktree: Entity<Worktree>, cx: &mut Context<Self>) {
        let Some(fs) = self.fs.clone() else {
            return;
        };
        let worktree = worktree.read(cx);
        let worktree_id = worktree.id();
        let path = worktree
            .abs_path()
            .join(paths::local_run_configurations_file_relative_path().as_std_path());
        let task = cx.spawn(async move |this, cx| {
            let (mut contents_rx, _watcher) = watch_config_file(cx.background_executor(), fs, path);
            while let Some(text) = contents_rx.next().await {
                let parsed = parse_text(
                    &text,
                    ConfigScope::Project {
                        worktree: worktree_id,
                    },
                );
                if this
                    .update(cx, |this, cx| {
                        this.worktree_configs.insert(worktree_id, parsed);
                        this.rebuild_persisted(cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        self._watchers.push(task);
    }

    fn drop_worktree_configs(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        if self.worktree_configs.remove(&worktree_id).is_some() {
            self.rebuild_persisted(cx);
        }
    }

    /// Write the current persisted configs back to disk: global-scoped ones to
    /// the global `run-configurations.json`, project-scoped ones to each
    /// worktree's `.sawe/run-configurations.json`. Best-effort: with no `fs` /
    /// no project handle only what can be written is written; failures are
    /// logged, not propagated.
    ///
    /// The global file is always written (even if empty) so the global config
    /// dir is seeded on first launch. For worktree files, an emptied bucket
    /// causes the file to be **deleted** rather than rewritten as an empty
    /// document — a missing file is treated as empty by the watcher, so the
    /// deletion is effectively persisted without leaving a stub.
    pub fn save_to_disk(&self, cx: &App) -> Task<()> {
        let Some(fs) = self.fs.clone() else {
            log::warn!("run_config: cannot save run configurations — no fs");
            return Task::ready(());
        };

        let mut global: Vec<RunConfiguration> = Vec::new();
        let mut per_worktree: HashMap<WorktreeId, Vec<RunConfiguration>> = HashMap::default();
        // Seed with every known worktree bucket so emptied ones are handled below.
        for worktree_id in self.worktree_configs.keys() {
            per_worktree.entry(*worktree_id).or_default();
        }
        for config in self.configs() {
            match &config.scope {
                ConfigScope::Global => global.push(config),
                ConfigScope::Project { worktree } => {
                    per_worktree.entry(*worktree).or_default().push(config);
                }
                ConfigScope::Ephemeral => {}
            }
        }

        let mut writes: Vec<(std::path::PathBuf, String)> = Vec::new();
        let mut deletes: Vec<std::path::PathBuf> = Vec::new();
        writes.push((
            paths::run_configurations_file().clone(),
            document_text(&global),
        ));
        if let Some(project) = self.project.as_ref().and_then(|project| project.upgrade()) {
            let relative_path = paths::local_run_configurations_file_relative_path().as_std_path();
            for (worktree_id, configs) in per_worktree {
                if let Some(worktree) = project.read(cx).worktree_for_id(worktree_id, cx) {
                    let path = worktree.read(cx).abs_path().join(relative_path);
                    if configs.is_empty() {
                        deletes.push(path);
                    } else {
                        writes.push((path, document_text(&configs)));
                    }
                }
            }
        }

        cx.background_spawn(async move {
            for (path, text) in writes {
                if let Some(parent) = path.parent() {
                    if let Err(err) = fs.create_dir(parent).await {
                        log::warn!("run_config: creating {parent:?}: {err:#}");
                        continue;
                    }
                }
                if let Err(err) = fs.atomic_write(path.clone(), text).await {
                    log::warn!("run_config: writing {path:?}: {err:#}");
                }
            }
            for path in deletes {
                if let Err(err) = fs
                    .remove_file(
                        &path,
                        fs::RemoveOptions {
                            recursive: false,
                            ignore_if_not_exists: true,
                        },
                    )
                    .await
                {
                    log::warn!("run_config: deleting {path:?}: {err:#}");
                }
            }
        })
    }

    /// Load and live-watch the global + per-worktree `run-configurations.json`
    /// files for `project`, and keep the ephemeral (discovered) set in sync with
    /// the project. Idempotent: a second call for any project is a no-op.
    pub fn watch_project(
        &mut self,
        project: Entity<Project>,
        fs: Arc<dyn fs::Fs>,
        cx: &mut Context<Self>,
    ) {
        if self.project.is_some() {
            return;
        }
        self.project = Some(project.downgrade());
        self.fs = Some(fs);

        self.spawn_global_watch(cx);
        for worktree in project.read(cx).worktrees(cx).collect::<Vec<_>>() {
            self.spawn_worktree_watch(worktree, cx);
        }

        let task_store = project.read(cx).task_store().clone();
        let task_store_subscription = cx.subscribe(&task_store, |this, _task_store, _event, cx| {
            if let Some(project) = this.project.clone().and_then(|p| p.upgrade()) {
                this.refresh_discovered(&project, cx);
            }
        });
        let project_subscription = cx.subscribe(&project, |this, project, event, cx| match event {
            project::Event::WorktreeAdded(worktree_id) => {
                if let Some(worktree) = project.read(cx).worktree_for_id(*worktree_id, cx) {
                    this.spawn_worktree_watch(worktree, cx);
                }
            }
            project::Event::WorktreeRemoved(worktree_id) => {
                this.drop_worktree_configs(*worktree_id, cx);
            }
            _ => {}
        });
        self._subscriptions.push(project_subscription);
        self._subscriptions.push(task_store_subscription);

        self.refresh_discovered(&project, cx);
    }
}

fn parse_text(text: &str, scope: ConfigScope) -> Vec<RunConfiguration> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    file_format::parse_document(text, scope)
        .log_err()
        .unwrap_or_default()
}

fn document_text(configs: &[RunConfiguration]) -> String {
    serde_json::to_string_pretty(&file_format::build_document(configs))
        .map(|mut text| {
            text.push('\n');
            text
        })
        .unwrap_or_else(|_| "{\n  \"configurations\": []\n}\n".to_string())
}

/// Register a provider on the global store. Call from `init` / extension setup.
pub fn register_provider(cx: &mut App, provider: impl RunConfigProvider) {
    if let Some(store) = RunConfigStore::try_global(cx) {
        store.update(cx, |store, _| store.register_provider(provider));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Executor, RunConfigId};
    use fs::Fs as _;
    use gpui::TestAppContext;
    use std::path::Path;

    fn cfg(name: &str) -> RunConfiguration {
        RunConfiguration {
            id: RunConfigId::from_raw(format!("shell:{name}")),
            name: name.into(),
            provider_type: "shell".into(),
            settings: serde_json::json!({}),
            executors: vec![Executor::Run],
            before_launch: vec![],
            folder: None,
            scope: ConfigScope::Global,
        }
    }

    #[gpui::test]
    fn upsert_remove_order(cx: &mut TestAppContext) {
        let store = cx.new(|_| RunConfigStore::empty());
        store.update(cx, |s, cx| {
            s.upsert(cfg("a"), cx);
            s.upsert(cfg("b"), cx);
        });
        store.read_with(cx, |s, _| {
            let names: Vec<_> = s.configs().iter().map(|c| c.name.to_string()).collect();
            assert_eq!(names, vec!["a", "b"]);
        });
        store.update(cx, |s, cx| {
            s.remove(&RunConfigId::from_raw("shell:a"), cx);
        });
        store.read_with(cx, |s, _| {
            assert_eq!(s.configs().len(), 1);
            assert_eq!(s.configs()[0].name.as_ref(), "b");
        });
    }

    #[gpui::test]
    fn running_set_and_command_sink(cx: &mut TestAppContext) {
        use std::sync::{Arc, Mutex};

        let store = cx.new(|_| RunConfigStore::empty());
        let id = RunConfigId::from_raw("shell:a");
        store.read_with(cx, |s, _| assert!(!s.is_running(&id)));
        store.update(cx, |s, cx| {
            s.set_running(1u64, collections::HashSet::from_iter([id.clone()]), cx)
        });
        store.read_with(cx, |s, _| {
            assert!(s.is_running(&id));
            assert_eq!(s.running_ids().count(), 1);
        });

        // No sink installed yet → dispatch_command is a no-op returning false.
        cx.update(|cx| {
            cx.set_global(GlobalRunConfigStore(store.clone()));
            assert!(!RunConfigStore::dispatch_command(
                cx,
                RunCommand::Stop { id: id.clone() }
            ));
        });

        let seen: Arc<Mutex<Vec<RunCommand>>> = Arc::new(Mutex::new(Vec::new()));
        store.update(cx, |s, _| {
            let seen = seen.clone();
            s.set_command_sink(Arc::new(move |command, _cx| {
                seen.lock().expect("lock").push(command);
            }));
        });
        cx.update(|cx| {
            assert!(RunConfigStore::dispatch_command(
                cx,
                RunCommand::Select { id: id.clone() }
            ));
        });
        assert_eq!(seen.lock().expect("lock").len(), 1);
    }

    #[gpui::test]
    fn running_set_unions_across_sources(cx: &mut TestAppContext) {
        let store = cx.new(|_| RunConfigStore::empty());
        let id_a = RunConfigId::from_raw("shell:a");
        let id_b = RunConfigId::from_raw("shell:b");

        store.update(cx, |s, cx| {
            s.set_running(1u64, collections::HashSet::from_iter([id_a.clone()]), cx);
        });
        store.update(cx, |s, cx| {
            s.set_running(2u64, collections::HashSet::from_iter([id_b.clone()]), cx);
        });
        store.read_with(cx, |s, _| {
            assert!(s.is_running(&id_a), "source 1's config should be running");
            assert!(s.is_running(&id_b), "source 2's config should be running");
            assert_eq!(s.running_ids().count(), 2);
        });

        // Clearing source 1 does not affect source 2.
        store.update(cx, |s, cx| {
            s.set_running(1u64, collections::HashSet::default(), cx);
        });
        store.read_with(cx, |s, _| {
            assert!(
                !s.is_running(&id_a),
                "source 1's config should no longer be running"
            );
            assert!(
                s.is_running(&id_b),
                "source 2's config should still be running"
            );
        });
    }

    #[gpui::test]
    fn clear_running_source_removes_entry(cx: &mut TestAppContext) {
        let store = cx.new(|_| RunConfigStore::empty());
        let id = RunConfigId::from_raw("shell:a");
        store.update(cx, |s, cx| {
            s.set_running(1u64, collections::HashSet::from_iter([id.clone()]), cx);
        });
        store.read_with(cx, |s, _| assert!(s.is_running(&id)));
        store.update(cx, |s, cx| s.clear_running_source(1u64, cx));
        store.read_with(cx, |s, _| {
            assert!(
                !s.is_running(&id),
                "clearing the source removes its running set"
            );
            assert_eq!(s.running_ids().count(), 0);
        });
        // Clearing an unknown source is a harmless no-op.
        store.update(cx, |s, cx| s.clear_running_source(99u64, cx));
    }

    #[gpui::test]
    async fn save_to_disk_writes_project_and_global_files(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/proj", serde_json::json!({})).await;
        let project = project::Project::test(fs.clone(), [Path::new("/proj")], cx).await;
        cx.update(|cx| RunConfigStore::init_global(cx));
        let store = cx.update(|cx| RunConfigStore::global(cx));
        store.update(cx, |s, cx| s.watch_project(project.clone(), fs.clone(), cx));
        cx.run_until_parked();

        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("project has a worktree")
                .read(cx)
                .id()
        });

        store.update(cx, |s, cx| {
            s.upsert(
                RunConfiguration {
                    id: RunConfigId::from_raw("shell:echo"),
                    name: "Echo".into(),
                    provider_type: "shell".into(),
                    settings: serde_json::json!({ "command": "echo", "args": ["hi"] }),
                    executors: vec![Executor::Run],
                    before_launch: vec![],
                    folder: None,
                    scope: ConfigScope::Project {
                        worktree: worktree_id,
                    },
                },
                cx,
            );
            s.upsert(
                RunConfiguration {
                    id: RunConfigId::from_raw("shell:global-one"),
                    name: "GlobalOne".into(),
                    provider_type: "shell".into(),
                    settings: serde_json::json!({ "command": "true" }),
                    executors: vec![Executor::Run],
                    before_launch: vec![],
                    folder: None,
                    scope: ConfigScope::Global,
                },
                cx,
            );
            s.save_to_disk(cx).detach();
        });
        cx.run_until_parked();

        let project_text = fs
            .load(Path::new("/proj/.sawe/run-configurations.json"))
            .await
            .expect("project run-configurations.json was written");
        assert!(
            project_text.contains("\"Echo\""),
            "project file: {project_text}"
        );
        assert!(
            project_text.contains("\"echo\""),
            "project file: {project_text}"
        );
        assert!(
            !project_text.contains("\"GlobalOne\""),
            "global config leaked into project file: {project_text}"
        );

        let global_text = fs
            .load(paths::run_configurations_file().as_path())
            .await
            .expect("global run-configurations.json was written");
        assert!(
            global_text.contains("\"GlobalOne\""),
            "global file: {global_text}"
        );
        assert!(
            !global_text.contains("\"Echo\""),
            "project config leaked into global file: {global_text}"
        );

        store.read_with(cx, |s, _| {
            assert!(s.configs().iter().any(|c| c.name.as_ref() == "Echo"));
            assert!(s.configs().iter().any(|c| c.name.as_ref() == "GlobalOne"));
        });
    }

    #[gpui::test]
    async fn deleting_project_config_persists_and_does_not_reappear(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/proj", serde_json::json!({})).await;
        let project = project::Project::test(fs.clone(), [Path::new("/proj")], cx).await;
        cx.update(|cx| RunConfigStore::init_global(cx));
        let store = cx.update(|cx| RunConfigStore::global(cx));
        store.update(cx, |s, cx| s.watch_project(project.clone(), fs.clone(), cx));
        cx.run_until_parked();

        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("project has a worktree")
                .read(cx)
                .id()
        });

        // Upsert a single project-scoped config and persist it.
        store.update(cx, |s, cx| {
            s.upsert(
                RunConfiguration {
                    id: RunConfigId::from_raw("shell:only"),
                    name: "Only".into(),
                    provider_type: "shell".into(),
                    settings: serde_json::json!({ "command": "true" }),
                    executors: vec![Executor::Run],
                    before_launch: vec![],
                    folder: None,
                    scope: ConfigScope::Project {
                        worktree: worktree_id,
                    },
                },
                cx,
            );
            s.save_to_disk(cx).detach();
        });
        cx.run_until_parked();

        let project_path = Path::new("/proj/.sawe/run-configurations.json");
        let text = fs.load(project_path).await.expect("project file written");
        assert!(text.contains("\"Only\""), "project file: {text}");
        store.read_with(cx, |s, _| {
            assert!(s.configs().iter().any(|c| c.name.as_ref() == "Only"));
        });

        // Now remove it (the only project-scoped config) and persist again.
        store.update(cx, |s, cx| {
            let removed = s.remove(&RunConfigId::from_raw("shell:only"), cx);
            assert!(removed.is_some(), "config should have been removed");
            s.save_to_disk(cx).detach();
        });
        // Let the FS watcher fire on the rewritten (now-empty) file.
        cx.run_until_parked();

        store.read_with(cx, |s, _| {
            assert!(
                s.configs().is_empty(),
                "config reappeared after deletion: {:?}",
                s.configs()
            );
        });
        assert!(
            !fs.is_file(project_path).await,
            "emptied worktree config file should have been deleted"
        );
    }

    #[gpui::test]
    async fn loads_and_reloads_project_file(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/proj",
            serde_json::json!({
                ".sawe": {
                    "run-configurations.json": r#"{ "configurations": [ { "name": "X", "type": "shell", "command": "echo" } ] }"#
                }
            }),
        )
        .await;
        let project = project::Project::test(fs.clone(), [Path::new("/proj")], cx).await;
        cx.update(|cx| RunConfigStore::init_global(cx));
        let store = cx.update(|cx| RunConfigStore::global(cx));
        store.update(cx, |s, cx| s.watch_project(project.clone(), fs.clone(), cx));
        cx.run_until_parked();
        store.read_with(cx, |s, _| {
            assert!(
                s.configs().iter().any(|c| c.name.as_ref() == "X"),
                "configs: {:?}",
                s.configs()
            );
        });

        fs.write(
            Path::new("/proj/.sawe/run-configurations.json"),
            br#"{ "configurations": [ { "name": "Y", "type": "shell", "command": "echo" } ] }"#,
        )
        .await
        .unwrap();
        cx.run_until_parked();
        store.read_with(cx, |s, _| {
            let names: Vec<_> = s.configs().iter().map(|c| c.name.to_string()).collect();
            assert!(names.contains(&"Y".to_string()), "got {names:?}");
            assert!(
                !names.contains(&"X".to_string()),
                "X should be gone, got {names:?}"
            );
        });
    }
}
