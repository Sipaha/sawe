use std::fmt;
use std::sync::Arc;

use gpui::SharedString;
use project::WorktreeId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Stable, name-independent identifier for a run configuration.
///
/// For persisted configs: a fresh random id assigned when the config is first
/// created, materialized in `run-configurations.json` as the `"id"` key. Legacy
/// entries without an `"id"` key get a deterministic-from-name id on load (see
/// `file_format::legacy_id`) which is then written into the file on the next save.
/// For ephemeral discovered configs: `"<provider_type>:discovered:<provider-supplied key>"`
/// (regenerated each load — never persisted).
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunConfigId(Arc<str>);

impl RunConfigId {
    /// Generate a fresh unique id for a newly-created persisted config.
    pub fn new_random() -> Self {
        Self(uuid::Uuid::new_v4().to_string().into())
    }

    /// Wrap an id string verbatim (parsing files that already carry an `"id"`,
    /// or accepting id strings over the MCP surface).
    pub fn from_raw(s: impl Into<Arc<str>>) -> Self {
        Self(s.into())
    }

    pub fn discovered(provider_type: &str, key: &str) -> Self {
        Self(format!("{provider_type}:discovered:{key}").into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RunConfigId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RunConfigId({:?})", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Executor {
    Run,
    Debug,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BeforeLaunchStep {
    /// Save every dirty buffer before launching. The only v1 step.
    SaveAllFiles,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigScope {
    /// Persisted in `<worktree-root>/.spke/run-configurations.json`.
    Project { worktree: WorktreeId },
    /// Persisted in the global `~/.config/spk-editor/run-configurations.json`.
    Global,
    /// Discovered at load time by a provider; never written to disk.
    Ephemeral,
}

impl ConfigScope {
    pub fn is_persisted(&self) -> bool {
        !matches!(self, ConfigScope::Ephemeral)
    }
}

/// One run configuration. `settings` is the provider-specific payload; the
/// provider owning `provider_type` knows how to interpret it.
#[derive(Clone, Debug, PartialEq)]
pub struct RunConfiguration {
    pub id: RunConfigId,
    pub name: SharedString,
    pub provider_type: SharedString,
    pub settings: serde_json::Value,
    pub executors: Vec<Executor>,
    pub before_launch: Vec<BeforeLaunchStep>,
    pub folder: Option<SharedString>,
    pub scope: ConfigScope,
}

/// What a provider produces from a configuration when the user hits Run/Debug.
pub enum RunRequest {
    Terminal(task::SpawnInTerminal),
    Debug(task::DebugScenario),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_config_id_formats() {
        assert_eq!(
            RunConfigId::from_raw("shell:build-release").as_str(),
            "shell:build-release"
        );
        assert_eq!(
            RunConfigId::discovered("task-ref", "cargo run").as_str(),
            "task-ref:discovered:cargo run"
        );
    }

    #[test]
    fn new_random_ids_are_unique() {
        assert_ne!(RunConfigId::new_random(), RunConfigId::new_random());
    }
}
