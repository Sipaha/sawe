use anyhow::{Context as _, Result};
use collections::HashMap;
use gpui::App;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ui::IconName;

use crate::model::{Executor, RunConfiguration, RunRequest};
use crate::provider::{RunConfigProvider, RunResolveContext};

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ShellSettings {
    /// The program to run.
    pub command: String,
    /// Arguments passed to the program.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory. Supports `$ZED_WORKTREE_ROOT` etc. Defaults to the worktree root.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

pub struct ShellProvider;

impl RunConfigProvider for ShellProvider {
    fn type_id(&self) -> &'static str {
        "shell"
    }

    fn display_name(&self) -> &'static str {
        "Shell Command"
    }

    fn icon(&self) -> IconName {
        IconName::Terminal
    }

    fn supported_executors(&self) -> &'static [Executor] {
        &[Executor::Run]
    }

    fn settings_schema(&self) -> schemars::Schema {
        schemars::schema_for!(ShellSettings)
    }

    fn new_template(&self, _cx: &App) -> serde_json::Value {
        serde_json::json!({ "command": "", "args": [] })
    }

    fn resolve(
        &self,
        config: &RunConfiguration,
        _executor: Executor,
        cx: &mut RunResolveContext,
        _app: &App,
    ) -> Result<RunRequest> {
        let settings: ShellSettings =
            serde_json::from_value(config.settings.clone()).context("invalid shell settings")?;
        let template = task::TaskTemplate {
            label: config.name.to_string(),
            command: settings.command,
            args: settings.args,
            env: settings.env,
            cwd: settings.cwd.or_else(|| {
                cx.worktree_root
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
            }),
            ..Default::default()
        };
        let resolved = template
            .resolve_task(config.id.as_str(), &cx.task_context)
            .context("failed to resolve shell task")?;
        Ok(RunRequest::Terminal(resolved.resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConfigScope, RunConfigId};
    use std::path::Path;

    #[gpui::test]
    async fn resolves_to_spawn_in_terminal(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/proj", serde_json::json!({})).await;
        let project = project::Project::test(fs, [Path::new("/proj")], cx).await;

        let config = RunConfiguration {
            id: RunConfigId::from_raw("shell:echo-hi"),
            name: "Echo hi".into(),
            provider_type: "shell".into(),
            settings: serde_json::json!({ "command": "echo", "args": ["hi"] }),
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
                ShellProvider.resolve(&config, Executor::Run, &mut resolve_context, cx)
            })
            .unwrap();

        match result {
            RunRequest::Terminal(spawn) => {
                assert_eq!(spawn.command.as_deref().unwrap_or_default(), "echo");
                assert_eq!(spawn.args, vec!["hi".to_string()]);
            }
            RunRequest::Debug(_) => panic!("expected Terminal"),
        }
    }
}
