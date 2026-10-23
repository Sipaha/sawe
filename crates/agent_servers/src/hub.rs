//! SPK AI Hub client. The editor launches only the transparent ACP bridge;
//! provider binaries, machine protocols and process ownership live in the hub.
use std::{any::Any, path::PathBuf, rc::Rc};

use acp_thread::AgentConnection;
use anyhow::Result;
use collections::HashMap;
use gpui::{App, AppContext as _, Entity, Task};
use project::{AgentId, Project, agent_server_store::AgentServerCommand};
use ui::IconName;

use crate::{AgentServer, AgentServerDelegate};

pub struct HubAgentServer {
    id: AgentId,
    provider: String,
    key_file: PathBuf,
}

impl HubAgentServer {
    pub fn new(id: AgentId, provider: String, key_file: PathBuf) -> Self {
        Self {
            id,
            provider,
            key_file,
        }
    }

    pub fn probe_models(
        &self,
        cwd: PathBuf,
        cx: &gpui::AsyncApp,
    ) -> Task<Result<Vec<acp_thread::NativeAgentModelInfo>>> {
        let provider = self.provider.clone();
        let key_file = self.key_file.clone();
        cx.background_spawn(async move {
            use futures::AsyncReadExt as _;
            use std::process::Stdio;
            let binary =
                std::env::var_os("SPK_AI_HUB_BINARY").unwrap_or_else(|| "spk-ai-hub".into());
            let endpoint =
                std::env::var("SPK_AI_HUB_URL").unwrap_or_else(|_| "http://127.0.0.1:5195".into());
            let mut command = std::process::Command::new(binary);
            command
                .args([
                    "models",
                    "--provider",
                    &provider,
                    "--hub-url",
                    &endpoint,
                    "--client-key-file",
                ])
                .arg(key_file)
                .arg("--cwd")
                .arg(&cwd)
                .current_dir(cwd);
            let mut child = util::process::Child::spawn_tracked(
                command,
                Stdio::null(),
                Stdio::piped(),
                Stdio::piped(),
                "SPK AI Hub model discovery",
            )?;
            async fn read_bounded(
                reader: impl futures::AsyncRead + Unpin,
                limit: u64,
            ) -> Result<Vec<u8>> {
                let mut data = Vec::new();
                reader.take(limit + 1).read_to_end(&mut data).await?;
                anyhow::ensure!(
                    data.len() as u64 <= limit,
                    "Hub model discovery exceeded its response limit"
                );
                Ok(data)
            }
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("Hub model discovery has no stdout"))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| anyhow::anyhow!("Hub model discovery has no stderr"))?;
            let (data, diagnostic) = match futures::try_join!(
                read_bounded(stdout, 1 << 20),
                read_bounded(stderr, 16 << 10)
            ) {
                Ok(value) => value,
                Err(error) => {
                    let _ = child.kill();
                    return Err(error);
                }
            };
            let status = child.status().await?;
            anyhow::ensure!(
                status.success(),
                "SPK AI Hub model discovery failed ({status}): {}",
                if diagnostic.is_empty() {
                    "no diagnostic was returned".into()
                } else {
                    String::from_utf8_lossy(&diagnostic).trim().to_owned()
                }
            );
            Ok(serde_json::from_slice(&data)?)
        })
    }
}

impl AgentServer for HubAgentServer {
    fn agent_id(&self) -> AgentId {
        self.id.clone()
    }
    fn logo(&self) -> IconName {
        IconName::Sparkle
    }
    fn connect(
        &self,
        delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let id = self.id.clone();
        let provider = self.provider.clone();
        let key_file = self.key_file.clone();
        let store = delegate.store.downgrade();
        let environment = project.read(cx).environment().downgrade();
        cx.spawn(async move |cx| {
            let env = environment
                .update(cx, |environment, cx| environment.default_environment(cx))?
                .await
                .unwrap_or_default();
            let binary = env
                .get("SPK_AI_HUB_BINARY")
                .cloned()
                .or_else(|| std::env::var("SPK_AI_HUB_BINARY").ok())
                .unwrap_or_else(|| "spk-ai-hub".into());
            let endpoint = env
                .get("SPK_AI_HUB_URL")
                .cloned()
                .or_else(|| std::env::var("SPK_AI_HUB_URL").ok())
                .unwrap_or_else(|| "http://127.0.0.1:5195".into());
            let command = AgentServerCommand {
                path: PathBuf::from(binary),
                args: vec![
                    "agent".into(),
                    "--hub-url".into(),
                    endpoint,
                    "--provider".into(),
                    provider.into(),
                    "--client-key-file".into(),
                    key_file.to_string_lossy().into_owned(),
                ],
                env: Some(env),
            };
            crate::acp::connect(id, project, command, store, None, HashMap::default(), cx).await
        })
    }
    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}
