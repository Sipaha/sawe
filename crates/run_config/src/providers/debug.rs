use anyhow::{Context as _, Result};
use gpui::App;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ui::IconName;

use crate::model::{Executor, RunConfiguration, RunRequest};
use crate::provider::{RunConfigProvider, RunResolveContext};

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct DebugSettings {
    /// The debug adapter id (e.g. `"CodeLLDB"`, `"Debugpy"`, `"JavaScript"`).
    pub adapter: String,
    /// Adapter-specific launch/attach config passed through verbatim (e.g. `request`, `program`).
    #[serde(default)]
    pub config: serde_json::Value,
    /// Optional label of a `.sawe/tasks.json` task to run as a build step before launching.
    #[serde(default)]
    pub build: Option<String>,
}

pub struct DebugProvider;

impl RunConfigProvider for DebugProvider {
    fn type_id(&self) -> &'static str {
        "debug"
    }

    fn display_name(&self) -> &'static str {
        "Debug Session"
    }

    fn icon(&self) -> IconName {
        IconName::Debug
    }

    fn supported_executors(&self) -> &'static [Executor] {
        &[Executor::Debug]
    }

    fn settings_schema(&self) -> schemars::Schema {
        schemars::schema_for!(DebugSettings)
    }

    fn new_template(&self, _cx: &App) -> serde_json::Value {
        serde_json::json!({ "adapter": "", "config": {} })
    }

    fn resolve(
        &self,
        config: &RunConfiguration,
        _executor: Executor,
        _cx: &mut RunResolveContext,
        _app: &App,
    ) -> Result<RunRequest> {
        let settings: DebugSettings =
            serde_json::from_value(config.settings.clone()).context("invalid debug settings")?;
        anyhow::ensure!(!settings.adapter.is_empty(), "debug config has no adapter");
        let scenario = task::DebugScenario {
            adapter: settings.adapter.into(),
            label: config.name.to_string().into(),
            config: settings.config,
            build: settings
                .build
                .map(|label| task::BuildTaskDefinition::ByName(label.into())),
            tcp_connection: None,
        };
        Ok(RunRequest::Debug(scenario))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConfigScope, RunConfigId};
    use std::path::Path;

    #[gpui::test]
    async fn resolves_to_debug_scenario(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/proj", serde_json::json!({})).await;
        let project = project::Project::test(fs, [Path::new("/proj")], cx).await;

        let config = RunConfiguration {
            id: RunConfigId::from_raw("debug:lldb-main"),
            name: "Debug main".into(),
            provider_type: "debug".into(),
            settings: serde_json::json!({
                "adapter": "CodeLLDB",
                "config": { "request": "launch", "program": "./a.out" }
            }),
            executors: vec![Executor::Debug],
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
                DebugProvider.resolve(&config, Executor::Debug, &mut resolve_context, cx)
            })
            .unwrap();

        match result {
            RunRequest::Debug(scenario) => {
                assert_eq!(scenario.adapter.as_ref(), "CodeLLDB");
                assert_eq!(scenario.label.as_ref(), "Debug main");
                assert!(scenario.build.is_none());
                assert!(scenario.tcp_connection.is_none());
            }
            RunRequest::Terminal(_) => panic!("expected Debug"),
        }
    }

    #[gpui::test]
    async fn resolves_build_step(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/proj", serde_json::json!({})).await;
        let project = project::Project::test(fs, [Path::new("/proj")], cx).await;

        let config = RunConfiguration {
            id: RunConfigId::from_raw("debug:with-build"),
            name: "Debug with build".into(),
            provider_type: "debug".into(),
            settings: serde_json::json!({
                "adapter": "CodeLLDB",
                "build": "cargo build",
                "config": { "request": "launch", "program": "./target/debug/myapp" }
            }),
            executors: vec![Executor::Debug],
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
                DebugProvider.resolve(&config, Executor::Debug, &mut resolve_context, cx)
            })
            .unwrap();

        match result {
            RunRequest::Debug(scenario) => {
                assert_eq!(scenario.adapter.as_ref(), "CodeLLDB");
                match scenario.build.as_ref().unwrap() {
                    task::BuildTaskDefinition::ByName(name) => {
                        assert_eq!(name.as_ref(), "cargo build")
                    }
                    _ => panic!("expected ByName build definition"),
                }
            }
            RunRequest::Terminal(_) => panic!("expected Debug"),
        }
    }

    #[gpui::test]
    async fn empty_adapter_is_error(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/proj", serde_json::json!({})).await;
        let project = project::Project::test(fs, [Path::new("/proj")], cx).await;

        let config = RunConfiguration {
            id: RunConfigId::from_raw("debug:bad"),
            name: "Bad".into(),
            provider_type: "debug".into(),
            settings: serde_json::json!({ "adapter": "" }),
            executors: vec![Executor::Debug],
            before_launch: vec![],
            folder: None,
            scope: ConfigScope::Global,
        };

        let err = cx.update(|cx| {
            let mut resolve_context = RunResolveContext {
                project: project.clone(),
                worktree_id: None,
                worktree_root: None,
                task_context: task::TaskContext::default(),
            };
            DebugProvider.resolve(&config, Executor::Debug, &mut resolve_context, cx)
        });
        assert!(err.is_err());
    }
}
