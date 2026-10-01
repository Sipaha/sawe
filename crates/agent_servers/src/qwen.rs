//! Qwen Code as a Solution-chat runtime.
//!
//! Unlike Claude and Codex, whose machine protocols are private and therefore
//! needed their own `*_native` crates to translate, Qwen Code speaks ACP
//! natively (`qwen --acp`). This server is consequently just a launcher: it
//! resolves the installed CLI, and hands the process to [`crate::acp::connect`]
//! so the session runs on the same connection, thread and event machinery as
//! any other ACP agent.
//!
//! Auth follows FORK.md decision 8 — the CLI's own. `qwen` reads
//! `~/.qwen/settings.json` and its OAuth credentials itself; the editor never
//! sees a token and injects none.
//!
//! Two capabilities make the fork's existing provider chrome work unmodified:
//! `session/new` returns `modes` (plan / default / auto-edit / auto / yolo),
//! which the pinned ACP schema parses into `AcpConnection::session_modes`, and
//! `configOptions`, which carries the model list as a `select` — the schema
//! dropped the per-session `models` field, so models arrive through config
//! options instead. See `solution_agent::qwen_adapter` for how both are mapped
//! onto the fork's permission-mode and model-picker surfaces.

use std::{any::Any, path::PathBuf, rc::Rc};

use acp_thread::AgentConnection;
use anyhow::Result;
use collections::HashMap;
use gpui::{App, Entity, Task};
use project::{Project, agent_server_store::AgentId, agent_server_store::AgentServerCommand};
use ui::IconName;

use crate::{AgentServer, AgentServerDelegate, load_proxy_env};

/// The agent id Qwen sessions are keyed by. Matches the ACP registry's id so a
/// `qwen-code` entry in `agent_servers` settings resolves to the same agent.
pub const QWEN_CODE_ID: &str = "qwen-code";

/// Resolved through `$PATH` captured from the project's shell environment, the
/// same way `codex_native` resolves `codex`.
pub const QWEN_BINARY: &str = "qwen";

/// The flag that puts the CLI in ACP mode instead of its interactive TUI.
pub const QWEN_ACP_FLAG: &str = "--acp";

pub struct QwenAgentServer {
    agent_id: AgentId,
}

impl QwenAgentServer {
    pub fn new(agent_id: AgentId) -> Self {
        Self { agent_id }
    }
}

impl AgentServer for QwenAgentServer {
    fn agent_id(&self) -> AgentId {
        self.agent_id.clone()
    }

    fn logo(&self) -> IconName {
        IconName::AiQwen
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
        let mut extra_env = load_proxy_env(cx);
        if delegate.store.read(cx).no_browser() {
            extra_env.insert("NO_BROWSER".to_owned(), "1".to_owned());
        }
        let agent_server_store = delegate.store.downgrade();
        let environment = project.read(cx).environment().downgrade();

        cx.spawn(async move |cx| {
            let mut env = environment
                .update(cx, |environment, cx| environment.default_environment(cx))?
                .await
                .unwrap_or_default();
            env.extend(extra_env);

            let command = AgentServerCommand {
                path: PathBuf::from(QWEN_BINARY),
                args: vec![QWEN_ACP_FLAG.to_owned()],
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
