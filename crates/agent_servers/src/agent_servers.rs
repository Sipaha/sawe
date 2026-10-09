mod acp;
mod custom;
mod kimi;

#[cfg(any(test, feature = "test-support"))]
pub mod e2e_tests;

use client::ProxySettings;
use collections::{HashMap, HashSet};
pub use custom::*;
pub use kimi::*;
use fs::Fs;
use http_client::read_no_proxy_from_env;
use project::{AgentId, Project, agent_server_store::AgentServerStore};

use acp_thread::{AgentConnection, AgentModelId};
use agent_client_protocol::schema::v1 as acp_schema;
use anyhow::Result;
use gpui::{App, AppContext, Entity, Task};
use settings::{AgentConfigOptionValue, SettingsStore};
use std::{any::Any, rc::Rc, sync::Arc};

#[cfg(any(test, feature = "test-support"))]
pub use acp::test_support::{
    FakeAcpAgentServer, FakeAcpConnectionHarness, connect_fake_acp_connection,
};
pub use acp::{
    AcpConnection, AcpDebugMessage, AcpDebugMessageContent, AcpDebugMessageDirection,
    GEMINI_TERMINAL_AUTH_METHOD_ID, mcp_servers_for_project, solution_scope_for_project,
};

pub struct AgentServerDelegate {
    store: Entity<AgentServerStore>,
    new_version_available: Option<watch::Sender<Option<String>>>,
    loading_status: Option<watch::Sender<Option<String>>>,
}

impl AgentServerDelegate {
    pub fn new(
        store: Entity<AgentServerStore>,
        new_version_tx: Option<watch::Sender<Option<String>>>,
        loading_status_tx: Option<watch::Sender<Option<String>>>,
    ) -> Self {
        Self {
            store,
            new_version_available: new_version_tx,
            loading_status: loading_status_tx,
        }
    }
}

pub trait AgentServer: Send {
    fn logo(&self) -> ui::IconName;
    fn agent_id(&self) -> AgentId;
    fn connect(
        &self,
        delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>>;

    fn connect_generation_only(
        &self,
        delegate: AgentServerDelegate,
        project: Entity<Project>,
        _root: std::path::PathBuf,
        _system_prompt: String,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        self.connect(delegate, project, cx)
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any>;

    fn default_mode(&self, _cx: &App) -> Option<acp_schema::SessionModeId> {
        None
    }

    fn set_default_mode(
        &self,
        _mode_id: Option<acp_schema::SessionModeId>,
        _fs: Arc<dyn Fs>,
        _cx: &mut App,
    ) {
    }

    fn default_model(&self, _cx: &App) -> Option<AgentModelId> {
        None
    }

    fn set_default_model(&self, _model_id: Option<AgentModelId>, _fs: Arc<dyn Fs>, _cx: &mut App) {}

    fn favorite_model_ids(&self, _cx: &mut App) -> HashSet<AgentModelId> {
        HashSet::default()
    }

    fn default_config_option(&self, _config_id: &str, _cx: &App) -> Option<AgentConfigOptionValue> {
        None
    }

    fn set_default_config_option(
        &self,
        _config_id: &str,
        _value: Option<AgentConfigOptionValue>,
        _fs: Arc<dyn Fs>,
        _cx: &mut App,
    ) {
    }

    fn favorite_config_option_value_ids(
        &self,
        _config_id: &acp_schema::SessionConfigId,
        _cx: &mut App,
    ) -> HashSet<acp_schema::SessionConfigValueId> {
        HashSet::default()
    }

    fn toggle_favorite_config_option_value(
        &self,
        _config_id: acp_schema::SessionConfigId,
        _value_id: acp_schema::SessionConfigValueId,
        _should_be_favorite: bool,
        _fs: Arc<dyn Fs>,
        _cx: &App,
    ) {
    }

    fn toggle_favorite_model(
        &self,
        _model_id: AgentModelId,
        _should_be_favorite: bool,
        _fs: Arc<dyn Fs>,
        _cx: &App,
    ) {
    }
}

impl dyn AgentServer {
    pub fn downcast<T: 'static + AgentServer + Sized>(self: Rc<Self>) -> Option<Rc<T>> {
        self.into_any().downcast().ok()
    }
}

/// Load the default proxy environment variables to pass through to the agent
pub fn load_proxy_env(cx: &mut App) -> HashMap<String, String> {
    let proxy_url = cx
        .read_global(|settings: &SettingsStore, _| settings.get::<ProxySettings>(None).proxy_url());
    let mut env = HashMap::default();

    if let Some(proxy_url) = &proxy_url {
        let env_var = if proxy_url.scheme() == "https" {
            "HTTPS_PROXY"
        } else {
            "HTTP_PROXY"
        };
        env.insert(env_var.to_owned(), proxy_url.to_string());
    }

    if let Some(no_proxy) = read_no_proxy_from_env() {
        env.insert("NO_PROXY".to_owned(), no_proxy);
    } else if proxy_url.is_some() {
        // We sometimes need local MCP servers that we don't want to proxy
        env.insert("NO_PROXY".to_owned(), "localhost,127.0.0.1".to_owned());
    }

    env
}


pub async fn verify_generation_cli_version(
    binary: &str,
    expected: &str,
    directory: &std::path::Path,
    environment: Option<&HashMap<String, String>>,
) -> Result<()> {
    use anyhow::Context as _;
    use std::process::{Command, Stdio};
    let mut command = Command::new(binary);
    command.arg("--version").current_dir(directory);
    if let Some(environment) = environment { command.envs(environment); }
    let child = util::process::Child::spawn_tracked(command, Stdio::null(), Stdio::piped(), Stdio::piped(), binary)
        .with_context(|| format!("Cannot check {binary} CLI version"))?;
    let output = child.output().await?;
    if !output.status.success() {
        anyhow::bail!("Cannot check {binary} CLI version: {}", String::from_utf8_lossy(&output.stderr));
    }
    validate_generation_cli_version(binary, expected, &String::from_utf8(output.stdout)?)
}

fn validate_generation_cli_version(binary: &str, expected: &str, output: &str) -> Result<()> {
    let output = output.trim();
    if output != expected && output != format!("codex-cli {expected}") {
        anyhow::bail!("{binary} CLI {output} does not have verified text-only generation support in this build. Supported version: {expected}.");
    }
    Ok(())
}

#[cfg(test)]
mod generation_version_tests {
    use super::*;

    #[test]
    fn unknown_cli_versions_fail_closed() {
        assert!(validate_generation_cli_version("codex", "0.160.1", "codex-cli 0.160.1\n").is_ok());
        assert!(validate_generation_cli_version("kimi", "2.1.1", "2.1.1\n").is_ok());
        for version in ["", "0.159.0", "2.1.0", "2.2.0", "unrecognized CLI"] {
            assert!(validate_generation_cli_version("kimi", "2.1.1", version).is_err());
        }
    }
}
