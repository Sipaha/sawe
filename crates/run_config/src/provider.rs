use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use gpui::{App, Entity};
use project::{Project, WorktreeId};
use ui::IconName;

use crate::model::{Executor, RunConfiguration, RunRequest};

/// Context handed to `RunConfigProvider::resolve`. Built by `run_config_ui`
/// (which has access to the active editor / workspace) and passed down so that
/// `run_config` itself stays free of `workspace`/`editor` dependencies.
pub struct RunResolveContext {
    pub project: Entity<Project>,
    /// The worktree the config is scoped to (or the project's first worktree
    /// for global/ephemeral configs).
    pub worktree_id: Option<WorktreeId>,
    /// Absolute path of the worktree root for `worktree_id`, if known.
    pub worktree_root: Option<PathBuf>,
    /// `task::TaskContext` variables (`ZED_FILE`, `ZED_WORKTREE_ROOT`, …) for
    /// the active editor. Providers use this when resolving `TaskTemplate`s.
    pub task_context: task::TaskContext,
}

pub trait RunConfigProvider: Send + Sync + 'static {
    /// Stable identifier persisted as the `"type"` field. Lowercase, no spaces.
    fn type_id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn icon(&self) -> IconName;
    fn supported_executors(&self) -> &'static [Executor];

    /// JSON schema for the provider payload (the `settings` value). Used to
    /// validate files and to auto-generate the editor form.
    fn settings_schema(&self) -> schemars::Schema;

    /// Default payload for "+ New <type>" in the Edit dialog.
    fn new_template(&self, cx: &App) -> serde_json::Value;

    /// Configs auto-discovered for the current project (cargo crates, npm
    /// scripts, existing `.sawe/tasks.json` tasks, …). Default: none.
    /// Returned configs MUST carry `ConfigScope::Ephemeral` and a
    /// `RunConfigId::discovered(self.type_id(), …)` id.
    fn discover(&self, _project: &Entity<Project>, _cx: &mut App) -> Vec<RunConfiguration> {
        Vec::new()
    }

    /// Turn a configuration + context into something runnable.
    fn resolve(
        &self,
        config: &RunConfiguration,
        executor: Executor,
        cx: &mut RunResolveContext,
        app: &App,
    ) -> Result<RunRequest>;
}

pub type ArcProvider = Arc<dyn RunConfigProvider>;
