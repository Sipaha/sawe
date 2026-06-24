//! MCP tools exposed by the `run_config` crate (`run_config.*` namespace).
//! Tools register with the central `editor_mcp` registry from `run_config::init`
//! so that `start_server` sees them when binding the socket.
//!
//! `list` / `create` / `delete` work headlessly (they only touch the global
//! `RunConfigStore`). `select` / `run` / `stop` route a `RunCommand` through
//! the store's command sink, which is installed by `run_config_ui::init` and
//! targets a window's `RunController` — so they are no-ops (returning
//! `{ "ok": false }`) until a workspace window with a run controller exists.

use anyhow::{Context as _, Result};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::model::{ConfigScope, Executor, RunConfigId, RunConfiguration};
use crate::store::{RunCommand, RunConfigStore};

pub fn register(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ListRunConfigsTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(CreateRunConfigTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(DeleteRunConfigTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SelectRunConfigTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RunRunConfigTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(StopRunConfigTool);
    });
}

// --- shared helpers ---

fn executor_str(executor: Executor) -> &'static str {
    match executor {
        Executor::Run => "run",
        Executor::Debug => "debug",
    }
}

fn parse_executor(name: &str) -> Result<Executor> {
    match name {
        "run" => Ok(Executor::Run),
        "debug" => Ok(Executor::Debug),
        other => {
            anyhow::bail!("invalid_params: unknown executor `{other}` (expected `run` or `debug`)")
        }
    }
}

fn scope_str(scope: &ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Project { .. } => "project",
        ConfigScope::Global => "global",
        ConfigScope::Ephemeral => "ephemeral",
    }
}

/// Find a persisted/ephemeral config by its string id (`"<type>:<slug>"`).
fn find_config(cx: &App, id_str: &str) -> Option<RunConfiguration> {
    let store = RunConfigStore::try_global(cx)?;
    store
        .read(cx)
        .configs()
        .into_iter()
        .find(|config| config.id.as_str() == id_str)
}

// =====================================================================
// run_config.list
// =====================================================================

/// List every known run configuration (persisted + discovered) with summary
/// metadata and whether it is currently running.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ListRunConfigsParams {}

impl<'de> Deserialize<'de> for ListRunConfigsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let _ = serde::de::IgnoredAny::deserialize(de)?;
        Ok(ListRunConfigsParams {})
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RunConfigSummary {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub executors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub scope: String,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListRunConfigsResult {
    pub configurations: Vec<RunConfigSummary>,
}

#[derive(Clone)]
pub struct ListRunConfigsTool;

impl McpServerTool for ListRunConfigsTool {
    type Input = ListRunConfigsParams;
    type Output = ListRunConfigsResult;
    const NAME: &'static str = "run_config.list";

    async fn run(
        &self,
        _input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let configurations = cx.update(|cx| {
            let Some(store) = RunConfigStore::try_global(cx) else {
                return Vec::new();
            };
            let store = store.read(cx);
            store
                .configs()
                .iter()
                .map(|config| RunConfigSummary {
                    id: config.id.as_str().to_string(),
                    name: config.name.to_string(),
                    provider_type: config.provider_type.to_string(),
                    executors: config
                        .executors
                        .iter()
                        .map(|executor| executor_str(*executor).to_string())
                        .collect(),
                    folder: config.folder.as_ref().map(|folder| folder.to_string()),
                    scope: scope_str(&config.scope).to_string(),
                    running: store.is_running(&config.id),
                })
                .collect::<Vec<_>>()
        });
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} run configuration(s)", configurations.len()),
            }],
            structured_content: ListRunConfigsResult { configurations },
        })
    }
}

// =====================================================================
// run_config.create
// =====================================================================

/// Create a new persisted run configuration and write it to disk. The `type`
/// must match a registered provider. The id is `"<type>:<slug of name>"`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CreateRunConfigParams {
    #[serde(rename = "type")]
    pub provider_type: String,
    pub name: String,
    #[serde(default)]
    pub settings: serde_json::Value,
    /// `"global"` (default) or `"project"`.
    #[serde(default)]
    pub scope: Option<String>,
    /// Executors this config supports. Defaults to the provider's full set.
    #[serde(default)]
    pub executors: Option<Vec<String>>,
    #[serde(default)]
    pub folder: Option<String>,
}

impl<'de> Deserialize<'de> for CreateRunConfigParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            #[serde(rename = "type")]
            provider_type: String,
            name: String,
            settings: serde_json::Value,
            scope: Option<String>,
            executors: Option<Vec<String>>,
            folder: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            provider_type: inner.provider_type,
            name: inner.name,
            settings: inner.settings,
            scope: inner.scope,
            executors: inner.executors,
            folder: inner.folder,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CreateRunConfigResult {
    pub id: String,
}

#[derive(Clone)]
pub struct CreateRunConfigTool;

impl McpServerTool for CreateRunConfigTool {
    type Input = CreateRunConfigParams;
    type Output = CreateRunConfigResult;
    const NAME: &'static str = "run_config.create";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.provider_type.trim().is_empty(),
            "invalid_params: type is required"
        );
        anyhow::ensure!(
            !input.name.trim().is_empty(),
            "invalid_params: name is required"
        );

        let id = cx.update(|cx| -> Result<String> {
            let store =
                RunConfigStore::try_global(cx).context("run configurations are not available")?;

            let provider = store
                .read(cx)
                .provider(&input.provider_type)
                .with_context(|| format!("unknown_provider_type: {}", input.provider_type))?;

            let executors: Vec<Executor> = match &input.executors {
                Some(names) => names
                    .iter()
                    .map(|name| parse_executor(name))
                    .collect::<Result<_>>()?,
                None => provider.supported_executors().to_vec(),
            };
            anyhow::ensure!(
                !executors.is_empty(),
                "invalid_params: executors must not be empty"
            );

            let scope = match input.scope.as_deref().unwrap_or("global") {
                "global" => ConfigScope::Global,
                "project" => store
                    .read(cx)
                    .project()
                    .and_then(|project| {
                        project
                            .read(cx)
                            .worktrees(cx)
                            .next()
                            .map(|worktree| worktree.read(cx).id())
                    })
                    .map(|worktree| ConfigScope::Project { worktree })
                    .unwrap_or(ConfigScope::Global),
                other => anyhow::bail!(
                    "invalid_params: unknown scope `{other}` (expected `global` or `project`)"
                ),
            };

            let settings = if input.settings.is_null() {
                serde_json::json!({})
            } else {
                input.settings.clone()
            };

            let id = RunConfigId::new_random();
            let config = RunConfiguration {
                id: id.clone(),
                name: input.name.clone().into(),
                provider_type: input.provider_type.clone().into(),
                settings,
                executors,
                before_launch: vec![],
                folder: input.folder.clone().map(Into::into),
                scope,
            };
            store.update(cx, |store, cx| {
                store.upsert(config, cx);
                store.save_to_disk(cx).detach();
            });
            Ok(id.as_str().to_string())
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("created: {id}"),
            }],
            structured_content: CreateRunConfigResult { id },
        })
    }
}

// =====================================================================
// run_config.delete
// =====================================================================

/// Delete a persisted run configuration and rewrite the affected file(s).
/// Discovered (ephemeral) configs can't be deleted; `deleted` is `false` for
/// them and for unknown ids.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct DeleteRunConfigParams {
    pub id: String,
}

impl<'de> Deserialize<'de> for DeleteRunConfigParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            id: String,
        }
        Ok(Self {
            id: Option::<Inner>::deserialize(de)?.unwrap_or_default().id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeleteRunConfigResult {
    pub deleted: bool,
}

#[derive(Clone)]
pub struct DeleteRunConfigTool;

impl McpServerTool for DeleteRunConfigTool {
    type Input = DeleteRunConfigParams;
    type Output = DeleteRunConfigResult;
    const NAME: &'static str = "run_config.delete";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(!input.id.is_empty(), "invalid_params: id is required");
        let deleted = cx.update(|cx| {
            let Some(store) = RunConfigStore::try_global(cx) else {
                return false;
            };
            let Some(config) = find_config(cx, &input.id) else {
                return false;
            };
            if !config.scope.is_persisted() {
                return false;
            }
            store.update(cx, |store, cx| {
                let removed = store.remove(&config.id, cx).is_some();
                if removed {
                    store.save_to_disk(cx).detach();
                }
                removed
            })
        });
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: if deleted { "deleted" } else { "not_found" }.to_string(),
            }],
            structured_content: DeleteRunConfigResult { deleted },
        })
    }
}

// =====================================================================
// run_config.select
// =====================================================================

/// Select a run configuration in the active window's run-config dropdown.
/// `ok` is `false` if no window with a run controller is open.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SelectRunConfigParams {
    pub id: String,
}

impl<'de> Deserialize<'de> for SelectRunConfigParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            id: String,
        }
        Ok(Self {
            id: Option::<Inner>::deserialize(de)?.unwrap_or_default().id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OkResult {
    pub ok: bool,
}

#[derive(Clone)]
pub struct SelectRunConfigTool;

impl McpServerTool for SelectRunConfigTool {
    type Input = SelectRunConfigParams;
    type Output = OkResult;
    const NAME: &'static str = "run_config.select";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(!input.id.is_empty(), "invalid_params: id is required");
        let ok = cx.update(|cx| -> Result<bool> {
            let config = find_config(cx, &input.id)
                .with_context(|| format!("run_config_not_found: {}", input.id))?;
            Ok(RunConfigStore::dispatch_command(
                cx,
                RunCommand::Select { id: config.id },
            ))
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: if ok { "selected" } else { "no_run_controller" }.to_string(),
            }],
            structured_content: OkResult { ok },
        })
    }
}

// =====================================================================
// run_config.run
// =====================================================================

/// Run (or debug) a run configuration in the active window. `ok` is `false`
/// if no window with a run controller is open.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RunRunConfigParams {
    pub id: String,
    /// `"run"` (default) or `"debug"`.
    #[serde(default)]
    pub executor: Option<String>,
}

impl<'de> Deserialize<'de> for RunRunConfigParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            id: String,
            executor: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            id: inner.id,
            executor: inner.executor,
        })
    }
}

#[derive(Clone)]
pub struct RunRunConfigTool;

impl McpServerTool for RunRunConfigTool {
    type Input = RunRunConfigParams;
    type Output = OkResult;
    const NAME: &'static str = "run_config.run";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(!input.id.is_empty(), "invalid_params: id is required");
        let executor = parse_executor(input.executor.as_deref().unwrap_or("run"))?;
        let ok = cx.update(|cx| -> Result<bool> {
            let config = find_config(cx, &input.id)
                .with_context(|| format!("run_config_not_found: {}", input.id))?;
            anyhow::ensure!(
                config.executors.contains(&executor),
                "unsupported_executor: `{}` does not support {}",
                config.name,
                executor_str(executor)
            );
            Ok(RunConfigStore::dispatch_command(
                cx,
                RunCommand::Run {
                    id: config.id,
                    executor,
                },
            ))
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: if ok { "running" } else { "no_run_controller" }.to_string(),
            }],
            structured_content: OkResult { ok },
        })
    }
}

// =====================================================================
// run_config.stop
// =====================================================================

/// Stop the active run of a run configuration. `ok` is `false` if no window
/// with a run controller is open.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct StopRunConfigParams {
    pub id: String,
}

impl<'de> Deserialize<'de> for StopRunConfigParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            id: String,
        }
        Ok(Self {
            id: Option::<Inner>::deserialize(de)?.unwrap_or_default().id,
        })
    }
}

#[derive(Clone)]
pub struct StopRunConfigTool;

impl McpServerTool for StopRunConfigTool {
    type Input = StopRunConfigParams;
    type Output = OkResult;
    const NAME: &'static str = "run_config.stop";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(!input.id.is_empty(), "invalid_params: id is required");
        let ok = cx.update(|cx| -> Result<bool> {
            let config = find_config(cx, &input.id)
                .with_context(|| format!("run_config_not_found: {}", input.id))?;
            Ok(RunConfigStore::dispatch_command(
                cx,
                RunCommand::Stop { id: config.id },
            ))
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: if ok { "stopped" } else { "no_run_controller" }.to_string(),
            }],
            structured_content: OkResult { ok },
        })
    }
}
