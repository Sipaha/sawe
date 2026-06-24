use std::collections::HashSet;

use anyhow::{Context as _, Result};
use gpui::{App, Entity};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ui::IconName;

use crate::model::{ConfigScope, Executor, RunConfigId, RunConfiguration, RunRequest};
use crate::provider::{RunConfigProvider, RunResolveContext};

/// Folder label shown for configs synthesised from existing project tasks.
const DETECTED_TASKS_FOLDER: &str = "Detected tasks";

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct TaskRefSettings {
    /// Label of a task defined in `.sawe/tasks.json` (or a language runnable).
    pub task_label: String,
}

pub struct TaskRefProvider;

impl RunConfigProvider for TaskRefProvider {
    fn type_id(&self) -> &'static str {
        "task-ref"
    }

    fn display_name(&self) -> &'static str {
        "Project Task"
    }

    fn icon(&self) -> IconName {
        IconName::PlayFilled
    }

    fn supported_executors(&self) -> &'static [Executor] {
        &[Executor::Run]
    }

    fn settings_schema(&self) -> schemars::Schema {
        schemars::schema_for!(TaskRefSettings)
    }

    fn new_template(&self, _cx: &App) -> serde_json::Value {
        serde_json::json!({ "task_label": "" })
    }

    fn discover(&self, project: &Entity<Project>, cx: &mut App) -> Vec<RunConfiguration> {
        let mut seen_labels = HashSet::new();
        let mut configs = Vec::new();
        for (_, template) in collect_task_templates(project, cx) {
            if !seen_labels.insert(template.label.clone()) {
                continue;
            }
            configs.push(RunConfiguration {
                id: RunConfigId::discovered("task-ref", &template.label),
                name: template.label.clone().into(),
                provider_type: "task-ref".into(),
                settings: serde_json::json!({ "task_label": template.label }),
                executors: vec![Executor::Run],
                before_launch: vec![],
                folder: Some(DETECTED_TASKS_FOLDER.into()),
                scope: ConfigScope::Ephemeral,
            });
        }
        configs
    }

    fn resolve(
        &self,
        config: &RunConfiguration,
        _executor: Executor,
        cx: &mut RunResolveContext,
        app: &App,
    ) -> Result<RunRequest> {
        let settings: TaskRefSettings =
            serde_json::from_value(config.settings.clone()).context("invalid task-ref settings")?;
        anyhow::ensure!(
            !settings.task_label.is_empty(),
            "task-ref config has no task_label"
        );
        let template = find_task_template(&cx.project, &settings.task_label, app)
            .with_context(|| format!("task `{}` not found", settings.task_label))?;
        let resolved = template
            .resolve_task(config.id.as_str(), &cx.task_context)
            .context("failed to resolve task")?;
        Ok(RunRequest::Terminal(resolved.resolved))
    }
}

/// Lists the settings-derived task templates visible to the project. Uses the
/// project's first visible worktree for worktree-scoped tasks plus the global
/// task settings. Language runnables are intentionally excluded — they require
/// an async, buffer-aware listing that a synchronous `discover`/`resolve` can't
/// perform here.
fn collect_task_templates(
    project: &Entity<Project>,
    cx: &App,
) -> Vec<(project::TaskSourceKind, task::TaskTemplate)> {
    let project = project.read(cx);
    let Some(inventory) = project.task_store().read(cx).task_inventory() else {
        return Vec::new();
    };
    let worktree_id = project
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).id());
    inventory.read(cx).task_templates_from_settings(worktree_id)
}

fn find_task_template(
    project: &Entity<Project>,
    label: &str,
    cx: &App,
) -> Option<task::TaskTemplate> {
    collect_task_templates(project, cx)
        .into_iter()
        .find(|(_, template)| template.label == label)
        .map(|(_, template)| template)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RunConfigId;
    use std::path::Path;

    async fn test_project(cx: &mut gpui::TestAppContext) -> Entity<Project> {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/proj",
            serde_json::json!({
                ".sawe": {
                    "tasks.json": r#"[
                        { "label": "build", "command": "cargo", "args": ["build"] }
                    ]"#
                }
            }),
        )
        .await;
        let project = project::Project::test(fs, [Path::new("/proj")], cx).await;
        // Let the worktree scan + settings observer load `.sawe/tasks.json` into
        // the task inventory before we read it back.
        cx.run_until_parked();
        project
    }

    #[gpui::test]
    async fn discover_lists_project_tasks(cx: &mut gpui::TestAppContext) {
        let project = test_project(cx).await;
        let configs = cx.update(|cx| TaskRefProvider.discover(&project, cx));
        let build = configs
            .iter()
            .find(|config| config.name.as_ref() == "build")
            .expect("expected a discovered config named `build`");
        assert_eq!(build.scope, ConfigScope::Ephemeral);
        assert_eq!(
            build.folder.as_ref().map(|folder| folder.as_ref()),
            Some(DETECTED_TASKS_FOLDER)
        );
        assert_eq!(build.provider_type.as_ref(), "task-ref");
        assert_eq!(build.settings, serde_json::json!({ "task_label": "build" }));
    }

    #[gpui::test]
    async fn resolves_named_task(cx: &mut gpui::TestAppContext) {
        let project = test_project(cx).await;
        let config = RunConfiguration {
            id: RunConfigId::from_raw("task-ref:build"),
            name: "build".into(),
            provider_type: "task-ref".into(),
            settings: serde_json::json!({ "task_label": "build" }),
            executors: vec![Executor::Run],
            before_launch: vec![],
            folder: None,
            scope: ConfigScope::Global,
        };

        let result = cx
            .update(|cx| {
                let mut resolve_context = RunResolveContext {
                    project: project.clone(),
                    worktree_id: None,
                    worktree_root: Some("/proj".into()),
                    task_context: task::TaskContext::default(),
                };
                TaskRefProvider.resolve(&config, Executor::Run, &mut resolve_context, cx)
            })
            .expect("resolve should succeed");

        match result {
            RunRequest::Terminal(spawn) => {
                assert_eq!(spawn.command.as_deref(), Some("cargo"));
                assert_eq!(spawn.args, vec!["build".to_string()]);
            }
            RunRequest::Debug(_) => panic!("expected Terminal"),
        }
    }
}
