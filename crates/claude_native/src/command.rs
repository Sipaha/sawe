//! Pure builder for the `claude` subprocess invocation: argv + env +
//! `--mcp-config` JSON. No spawning here — it produces a
//! [`std::process::Command`] the process layer runs.
//!
//! The argv reproduces what the `claude-agent-acp` node wrapper's SDK passes to
//! the binary (captured from a live `ps`): static stream-json flags +
//! `--mcp-config <json>` + the session arg. The system prompt is passed via
//! `--append-system-prompt` (the binary supports it directly — `claude --help`),
//! so no `initialize` control handshake is needed.

use std::path::PathBuf;
use std::process::Command;

use agent_client_protocol::schema as acp;

/// How the spawned `claude` process binds to a session.
pub enum SessionArg {
    /// Start a fresh session with this id (`--session-id`).
    New(String),
    /// Resume an existing session, replaying its transcript (`--resume`).
    Resume(String),
}

impl SessionArg {
    /// The session id this invocation binds to. `claude` honors the
    /// `--session-id`/`--resume` value we pass and echoes it back in its `init`
    /// message, so we can adopt it up front without waiting for `init`.
    pub fn session_id(&self) -> &str {
        match self {
            SessionArg::New(id) | SessionArg::Resume(id) => id,
        }
    }
}

/// Everything needed to build one `claude` invocation.
pub struct ClaudeCommandSpec {
    pub binary: PathBuf,
    pub work_dir: PathBuf,
    pub session: SessionArg,
    /// The value for `--mcp-config` (see [`mcp_config_json`]).
    pub mcp_servers_json: String,
    /// Appended to the default system prompt via `--append-system-prompt`.
    pub append_system_prompt: Option<String>,
    pub extra_env: Vec<(String, String)>,
    /// Model alias or full id passed as `--model`. `None` → claude uses
    /// its default. Used both for the initial spawn and every respawn so
    /// a chosen model survives kill+resume.
    pub model: Option<String>,
    /// The editor-owned settings layer (`--settings <file>`): the
    /// `WorktreeCreate`/`WorktreeRemove` hooks + `autoMemoryDirectory`. See
    /// [`crate::claude_settings`]. `None` when the project isn't under an open
    /// Solution (standalone window, tests) or when the layer is disabled via
    /// `SAWE_CLAUDE_SETTINGS_DISABLED`.
    pub settings_path: Option<PathBuf>,
}

impl ClaudeCommandSpec {
    pub fn to_std_command(&self) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&self.work_dir);
        cmd.envs(self.extra_env.iter().cloned());

        // Enable Claude Code's "Agent Teams" feature. Without this gate the
        // agent can spawn subagents (the `Agent`/`Task` tools) but has no way
        // to message a still-running one — the `SendMessage` tool (plus
        // `TeamCreate`/`TaskCreate`) is registered only when this flag is set.
        // It's an experimental feature keyed off an env var rather than a CLI
        // option (claude 2.1.x). See anthropics/claude-code issues #42737 /
        // #35240. Set after `extra_env` so it always wins.
        cmd.env("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS", "1");

        cmd.args([
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--permission-prompt-tool",
            "stdio",
            "--disallowedTools",
            "AskUserQuestion",
            "--tools",
            "default",
        ]);
        cmd.args(["--mcp-config", &self.mcp_servers_json]);
        cmd.args(["--setting-sources", "user,project,local"]);
        // `--settings` sits at the command-line precedence tier: it overrides the
        // *same keys* in user/project/local settings but does not stop those
        // sources from loading. `claude_settings` therefore re-emits the user's
        // own `hooks` alongside ours, since `hooks` is one such key.
        if let Some(settings) = &self.settings_path {
            cmd.arg("--settings").arg(settings);
        }
        cmd.args(["--permission-mode", "bypassPermissions"]);
        cmd.args([
            "--allow-dangerously-skip-permissions",
            "--include-partial-messages",
            "--replay-user-messages",
        ]);
        match &self.session {
            SessionArg::New(id) => cmd.args(["--session-id", id]),
            SessionArg::Resume(id) => cmd.args(["--resume", id]),
        };
        if let Some(prompt) = &self.append_system_prompt {
            cmd.args(["--append-system-prompt", prompt]);
        }
        if let Some(model) = &self.model {
            cmd.args(["--model", model]);
        }
        cmd
    }
}

/// Build the `--mcp-config` JSON from the ACP `mcp_servers` the editor would
/// have sent in the ACP `session/new` request. Only the stdio transport (the
/// `sawe` bridge) is mapped — the fork uses no http/sse MCP servers; any
/// such (or future `#[non_exhaustive]`) variants are skipped.
pub fn mcp_config_json(servers: &[acp::McpServer]) -> String {
    let mut map = serde_json::Map::new();
    for server in servers {
        if let acp::McpServer::Stdio(stdio) = server {
            let env: serde_json::Map<String, serde_json::Value> = stdio
                .env
                .iter()
                .map(|e| (e.name.clone(), serde_json::Value::String(e.value.clone())))
                .collect();
            map.insert(
                stdio.name.clone(),
                serde_json::json!({
                    "type": "stdio",
                    "command": stdio.command.to_string_lossy(),
                    "args": stdio.args,
                    "env": env,
                }),
            );
        }
    }
    serde_json::json!({ "mcpServers": serde_json::Value::Object(map) }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_resume_argv_with_flags() {
        let spec = ClaudeCommandSpec {
            binary: "claude".into(),
            work_dir: "/w".into(),
            session: SessionArg::Resume("sid".into()),
            mcp_servers_json: r#"{"mcpServers":{}}"#.into(),
            append_system_prompt: Some("SYS".into()),
            extra_env: vec![("K".into(), "V".into())],
            model: None,
            settings_path: None,
        };
        let cmd = spec.to_std_command();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--input-format" && w[1] == "stream-json")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--output-format" && w[1] == "stream-json")
        );
        assert!(args.contains(&"--include-partial-messages".to_string()));
        assert!(args.windows(2).any(|w| w[0] == "--resume" && w[1] == "sid"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--mcp-config" && w[1] == r#"{"mcpServers":{}}"#)
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--permission-mode" && w[1] == "bypassPermissions")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--append-system-prompt" && w[1] == "SYS")
        );
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("/w")));
        // Agent Teams must be enabled so the agent gets the SendMessage tool.
        let teams = cmd.get_envs().any(|(k, v)| {
            k == std::ffi::OsStr::new("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS")
                && v == Some(std::ffi::OsStr::new("1"))
        });
        assert!(teams, "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 must be set");
    }

    #[test]
    fn new_session_uses_session_id_not_resume() {
        let spec = ClaudeCommandSpec {
            binary: "claude".into(),
            work_dir: "/w".into(),
            session: SessionArg::New("uuid".into()),
            mcp_servers_json: "{}".into(),
            append_system_prompt: None,
            extra_env: vec![],
            model: None,
            settings_path: None,
        };
        let args: Vec<String> = spec
            .to_std_command()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--session-id" && w[1] == "uuid")
        );
        assert!(!args.iter().any(|a| a == "--resume"));
        assert!(!args.iter().any(|a| a == "--append-system-prompt"));
    }

    #[test]
    fn passes_model_flag_when_set() {
        let spec = ClaudeCommandSpec {
            binary: "claude".into(),
            work_dir: "/w".into(),
            session: SessionArg::New("uuid".into()),
            mcp_servers_json: "{}".into(),
            append_system_prompt: None,
            extra_env: vec![],
            model: Some("opus".into()),
            settings_path: None,
        };
        let args: Vec<String> = spec.to_std_command().get_args()
            .map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.windows(2).any(|w| w[0] == "--model" && w[1] == "opus"));
    }

    #[test]
    fn omits_model_flag_when_none() {
        let spec = ClaudeCommandSpec {
            binary: "claude".into(),
            work_dir: "/w".into(),
            session: SessionArg::New("uuid".into()),
            mcp_servers_json: "{}".into(),
            append_system_prompt: None,
            extra_env: vec![],
            model: None,
            settings_path: None,
        };
        let args: Vec<String> = spec.to_std_command().get_args()
            .map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(!args.iter().any(|a| a == "--model"));
    }

    #[test]
    fn passes_the_editor_settings_file_without_dropping_the_user_sources() {
        let spec = ClaudeCommandSpec {
            binary: "claude".into(),
            work_dir: "/w".into(),
            session: SessionArg::New("uuid".into()),
            mcp_servers_json: "{}".into(),
            append_system_prompt: None,
            extra_env: vec![],
            model: None,
            settings_path: Some("/state/solutions/7/claude-settings.json".into()),
        };
        let args: Vec<String> = spec
            .to_std_command()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|w| w[0] == "--settings"
            && w[1] == "/state/solutions/7/claude-settings.json"));
        // `--settings` is a command-line-tier OVERRIDE, not a replacement for the
        // file sources: the user's own settings must keep loading.
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--setting-sources" && w[1] == "user,project,local")
        );
    }

    #[test]
    fn omits_the_settings_flag_when_there_is_no_solution() {
        let spec = ClaudeCommandSpec {
            binary: "claude".into(),
            work_dir: "/w".into(),
            session: SessionArg::New("uuid".into()),
            mcp_servers_json: "{}".into(),
            append_system_prompt: None,
            extra_env: vec![],
            model: None,
            settings_path: None,
        };
        let args: Vec<String> = spec
            .to_std_command()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|a| a == "--settings"));
    }

    #[test]
    fn maps_stdio_bridge_server() {
        let server = acp::McpServer::Stdio(
            acp::McpServerStdio::new("sawe", "/path/exe")
                .args(vec!["--nc".to_string(), "/tmp/mcp.sock".to_string()])
                .env(vec![acp::EnvVariable::new(
                    "SAWE_MCP_BRIDGE_CAPS",
                    "write",
                )]),
        );
        let json: serde_json::Value = serde_json::from_str(&mcp_config_json(&[server])).unwrap();
        let s = &json["mcpServers"]["sawe"];
        assert_eq!(s["type"], "stdio");
        assert_eq!(s["command"], "/path/exe");
        assert_eq!(s["args"][0], "--nc");
        assert_eq!(s["args"][1], "/tmp/mcp.sock");
        assert_eq!(s["env"]["SAWE_MCP_BRIDGE_CAPS"], "write");
    }
}
