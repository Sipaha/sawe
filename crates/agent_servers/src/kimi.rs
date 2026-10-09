//! Kimi Code as a Solution-chat runtime.
//!
//! Unlike Claude and Codex, whose machine protocols are private and therefore
//! needed their own `*_native` crates to translate, Kimi Code speaks ACP
//! natively (`kimi acp`). This server is consequently just a launcher: it
//! resolves the installed CLI, and hands the process to [`crate::acp::connect`]
//! so the session runs on the same connection, thread and event machinery as
//! any other ACP agent.
//!
//! Auth follows the same contract as decision 8 — the CLI's own. `kimi`
//! reuses the user's existing login state (see `kimi login`); the editor
//! never sees a token and injects none. When no usable login exists,
//! `session/new` fails with an auth error and the connection surfaces the
//! CLI's own `terminal` auth method.
//!
//! Two capabilities make the fork's existing provider chrome work unmodified:
//! `session/new` returns `modes` (plan / default / auto / yolo), which the
//! pinned ACP schema parses into `AcpConnection::session_modes`, and
//! `configOptions`, which carries the model list as a `select` under the id
//! `model` — the schema dropped the per-session `models` field, so models
//! arrive through config options instead. Kimi also advertises both
//! `session/load` and `session/resume`; resume is preferred, per the
//! "Resume attaches without replaying server history" contract (FORK.md
//! #161). See `solution_agent::kimi_adapter` for how modes and config
//! options are mapped onto the fork's permission-mode and model-picker
//! surfaces.

use std::{any::Any, path::PathBuf, rc::Rc};

use acp_thread::AgentConnection;
use anyhow::Result;
use collections::HashMap;
use gpui::{App, AppContext as _, Entity, Task};
use project::{Project, agent_server_store::AgentId, agent_server_store::AgentServerCommand};
use ui::IconName;

use crate::{AgentServer, AgentServerDelegate, load_proxy_env};

/// The agent id Kimi sessions are keyed by. Matches the ACP registry's id so a
/// `kimi-code` entry in `agent_servers` settings resolves to the same agent.
pub const KIMI_CODE_ID: &str = "kimi-code";

/// Resolved through `$PATH` captured from the project's shell environment, the
/// same way `codex_native` resolves `codex`.
pub const KIMI_BINARY: &str = "kimi";

/// The subcommand that puts the CLI in ACP mode instead of its interactive
/// TUI. Note: a positional subcommand, not a `--flag`.
pub const KIMI_ACP_SUBCOMMAND: &str = "acp";

/// The session config option Kimi publishes its model list under, read by the
/// generic ACP connection's `active_model` and by
/// `solution_agent::native_controls`.
pub const MODEL_CONFIG_OPTION_ID: &str = "model";

pub struct KimiAgentServer {
    agent_id: AgentId,
}

impl KimiAgentServer {
    pub fn new(agent_id: AgentId) -> Self {
        Self { agent_id }
    }
}

impl AgentServer for KimiAgentServer {
    fn agent_id(&self) -> AgentId {
        self.agent_id.clone()
    }

    fn logo(&self) -> IconName {
        IconName::AiKimi
    }

    fn connect(
        &self,
        delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let agent_id = self.agent_id();
        // Layered exactly like `LocalCustomAgent::get_command`: the shell
        // environment is the base, and the editor's own overrides win on top.
        let extra_env = load_proxy_env(cx);
        let agent_server_store = delegate.store.downgrade();
        let environment = project.read(cx).environment().downgrade();

        cx.spawn(async move |cx| {
            let mut env = environment
                .update(cx, |environment, cx| environment.default_environment(cx))?
                .await
                .unwrap_or_default();
            env.extend(extra_env);

            let command = AgentServerCommand {
                path: PathBuf::from(KIMI_BINARY),
                args: vec![KIMI_ACP_SUBCOMMAND.to_owned()],
                env: Some(env),
            };

            let connection = crate::acp::connect(
                agent_id,
                project,
                command,
                agent_server_store,
                None,
                HashMap::default(),
                cx,
            )
            .await?;
            Ok(connection)
        })
    }

    fn connect_generation_only(
        &self,
        delegate: AgentServerDelegate,
        project: Entity<Project>,
        root: PathBuf,
        system_prompt: String,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let agent_id = self.agent_id();
        let extra_env = load_proxy_env(cx);
        let environment = project.read(cx).environment().downgrade();
        let store = delegate.store.downgrade();
        cx.spawn(async move |cx| {
            let mut env: HashMap<String, String> = std::env::vars_os()
                .map(|(key, value)| (key.to_string_lossy().into_owned(), value.to_string_lossy().into_owned()))
                .collect();
            env.extend(environment.update(cx, |environment, cx| environment.default_environment(cx))?.await.unwrap_or_default());
            env.extend(extra_env);
            if env.get("KIMI_MODEL_NAME").is_some_and(|model| !model.is_empty()) {
                anyhow::bail!("Kimi text generation requires the CLI subscription login; KIMI_MODEL_NAME overrides are not used.");
            }
            if env.get("KIMI_CODE_CUSTOM_HEADERS").is_some_and(|headers| headers.lines().any(|line| line.split(':').next().is_some_and(|key| key.trim().eq_ignore_ascii_case("authorization")))) {
                anyhow::bail!("Kimi text generation does not use authentication header overrides.");
            }
            let profile = cx.background_spawn(async move {
                crate::verify_generation_cli_version(KIMI_BINARY, "2.1.1", &root, Some(&env)).await?;
                generation_profile(&root, &system_prompt, &env).map(|profile| (profile, env))
            }).await?;
            let (profile, mut env) = profile;
            env.insert("KIMI_CODE_HOME".into(), profile.path().join("home").to_string_lossy().into_owned());
            env.insert("KIMI_CODE_NO_AUTO_UPDATE".into(), "1".into());
            env.insert("KIMI_DISABLE_CRON".into(), "1".into());
            env.insert("KIMI_CODE_WATCH".into(), "0".into());
            env.insert("KIMI_CODE_INFINITE_RETRY".into(), "0".into());
            env.insert("KIMI_LOOP_MAX_ATTEMPTS_PER_STEP".into(), "1".into());
            env.insert("KIMI_CODE_BUILTIN_PRODUCT_SKILLS".into(), "0".into());
            let command = AgentServerCommand {
                path: KIMI_BINARY.into(),
                args: vec![KIMI_ACP_SUBCOMMAND.into()],
                env: Some(env),
            };
            Ok(Rc::new(crate::acp::AcpConnection::stdio_generation(agent_id, project, command, store, profile, cx).await?) as Rc<dyn AgentConnection>)
        })
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

fn generation_profile(
    root: &std::path::Path,
    system_prompt: &str,
    env: &HashMap<String, String>,
) -> Result<tempfile::TempDir> {
    use anyhow::Context as _;
    let scratch = root.join(".tmp").join("ai-generation");
    std::fs::create_dir_all(&scratch)?;
    let profile = tempfile::Builder::new()
        .prefix("kimi-")
        .tempdir_in(scratch)?;
    let home = profile.path().join("home");
    std::fs::create_dir(&home)?;
    std::fs::create_dir(profile.path().join("skills"))?;
    let source = env
        .get("KIMI_CODE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            env.get("HOME")
                .map(|home| PathBuf::from(home).join(".kimi-code"))
        })
        .context("Cannot locate Kimi CLI login")?;
    let config_path = source.join("config.toml");
    let original = match std::fs::read_to_string(&config_path) {
        Ok(text) => text
            .parse::<toml::Table>()
            .context("Invalid Kimi CLI configuration")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(error) => return Err(error.into()),
    };
    let model = original.get("default_model").and_then(toml::Value::as_str)
        .context("Kimi CLI has no configured subscription model. Run `kimi login` in a terminal.")?;
    let provider_id = original.get("models").and_then(|models| models.get(model)).and_then(|model| model.get("provider")).and_then(toml::Value::as_str);
    anyhow::ensure!(provider_id == Some("managed:kimi-code"), "Kimi text generation requires a configured CLI subscription model. No fallback was attempted.");
    let provider = original.get("providers").and_then(|providers| providers.get("managed:kimi-code"))
        .context("Kimi text generation requires the CLI subscription login. Run `kimi login` in a terminal.")?;
    anyhow::ensure!(provider.get("oauth").is_some_and(toml::Value::is_table), "Kimi text generation requires the CLI subscription login; API-key accounts are not used.");
    anyhow::ensure!(provider.get("type").and_then(toml::Value::as_str).is_none_or(|kind| kind == "kimi"), "Kimi text generation requires the CLI subscription provider.");
    for key in ["api_key", "api_key_env"] {
        anyhow::ensure!(!provider.get(key).and_then(toml::Value::as_str).is_some_and(|value| !value.is_empty()), "Kimi text generation does not use {key} credentials.");
    }
    if let Some(environment) = provider.get("env").and_then(toml::Value::as_table) {
        anyhow::ensure!(!environment.iter().any(|(key, value)| key.ends_with("_API_KEY") && value.as_str().is_some_and(|value| !value.is_empty())), "Kimi text generation does not use provider API-key environment overrides.");
    }
    if let Some(headers) = provider.get("custom_headers").and_then(toml::Value::as_table) {
        anyhow::ensure!(!headers.keys().any(|key| key.eq_ignore_ascii_case("authorization")), "Kimi text generation does not use authentication header overrides.");
    }
    let mut config = toml::Table::new();
    // Keep account/model routing, while excluding executable customizations.
    for key in ["providers", "models", "default_model", "thinking"] {
        if let Some(value) = original.get(key) {
            config.insert(key.into(), value.clone());
        }
    }
    std::fs::write(home.join("config.toml"), toml::to_string(&config)?)?;
    let credentials = source.join("credentials");
    if credentials.exists() {
        let destination = home.join("credentials");
        std::fs::create_dir(&destination)?;
        for entry in std::fs::read_dir(credentials)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
            {
                std::fs::copy(entry.path(), destination.join(entry.file_name()))?;
            }
        }
    }
    let agents = profile.path().join(".kimi-code").join("agents");
    std::fs::create_dir_all(&agents)?;
    // Kimi ACP ignores the print/TUI --agent-file flag; override its default
    // profile in this isolated project instead.
    std::fs::write(
        agents.join("agent.md"),
        format!(
            "---\nname: agent\noverride: true\ndescription: Generate text from supplied evidence\ntools: []\nsubagents: []\n---\n\n{system_prompt}\n"
        ),
    )?;
    Ok(profile)
}

#[cfg(test)]
mod generation_tests {
    use super::*;

    #[test]
    fn generation_keeps_account_routing_and_excludes_customizations() {
        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("original");
        std::fs::create_dir(&source).expect("source");
        std::fs::write(source.join("config.toml"), "default_model = 'chosen'\nextra_skill_dirs = ['/unsafe']\n[[hooks]]\nevent = 'UserPromptSubmit'\ncommand = 'touch sentinel'\n[models.chosen]\nprovider = 'managed:kimi-code'\nmodel = 'selected-model'\n[providers.\"managed:kimi-code\".oauth]\nstorage = 'file'\nkey = 'oauth/fixture'\n").expect("config");
        let env = HashMap::from_iter([(
            "KIMI_CODE_HOME".into(),
            source.to_string_lossy().into_owned(),
        )]);
        let profile = generation_profile(root.path(), "Fixed generation instructions", &env)
            .expect("profile");
        let config =
            std::fs::read_to_string(profile.path().join("home/config.toml")).expect("config");
        let config = config.parse::<toml::Table>().expect("parse");
        assert_eq!(config["default_model"].as_str(), Some("chosen"));
        assert_eq!(
            config["models"]["chosen"]["model"].as_str(),
            Some("selected-model")
        );
        assert!(!config.contains_key("hooks"));
        assert!(!config.contains_key("extra_skill_dirs"));
        let agent = std::fs::read_to_string(profile.path().join(".kimi-code/agents/agent.md"))
            .expect("agent");
        assert!(agent.contains("tools: []"));
        assert!(agent.contains("subagents: []"));
        assert!(!agent.contains("${"));
        assert!(!profile.path().join("home/mcp.json").exists());
    }
    #[test]
    fn generation_rejects_api_key_routes_without_subscription_fallback() {
        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("original");
        std::fs::create_dir(&source).expect("source");
        let env = HashMap::from_iter([("KIMI_CODE_HOME".into(), source.to_string_lossy().into_owned())]);
        for credentials in ["api_key = 'dummy'", "api_key_env = 'FIXTURE_KEY'", "[providers.\"managed:kimi-code\".env]\nKIMI_API_KEY = 'dummy'"] {
            let config = format!("default_model = 'chosen'\n[models.chosen]\nprovider = 'managed:kimi-code'\nmodel = 'selected-model'\n[providers.\"managed:kimi-code\"]\n{credentials}\n");
            std::fs::write(source.join("config.toml"), config).expect("config");
            assert!(generation_profile(root.path(), "Fixed instructions", &env).is_err());
        }
    }

}
