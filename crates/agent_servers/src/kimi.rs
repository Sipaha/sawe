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
use gpui::{App, Entity, Task};
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

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}
