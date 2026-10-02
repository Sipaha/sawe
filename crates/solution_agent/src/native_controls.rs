//! Native-runtime controls shared by session creation, restoration and the UI.
//!
//! Keep backend dispatch here so a new runtime cannot silently miss one of
//! the live/cold model and effort paths.
use std::{path::PathBuf, rc::Rc};

use acp_thread::{AgentConnection, NativeAgentModelInfo};
use agent_client_protocol::schema::v1 as acp;
use agent_servers::AgentServer;
use anyhow::{Result, anyhow};
use gpui::{App, AsyncApp, Task, TaskExt as _};
use util::ResultExt as _;

use crate::{kimi_adapter, model::SessionPermissionMode};

fn model_info(model: codex_native::CodexModelInfo) -> NativeAgentModelInfo {
    NativeAgentModelInfo {
        value: model.value,
        display_name: model.display_name,
        description: model.description,
    }
}

/// The models an agent publishes as a `select` session config option.
///
/// This is the ACP-native path — Kimi Code has no `*_native` runtime, it
/// speaks ACP directly. The pinned ACP schema dropped `session/new`'s
/// `models` field, but the same list also arrives as a config option, which
/// the schema does parse. Deliberately not gated on an agent id: any ACP
/// agent that publishes a `model` select gets a populated picker, and the two
/// native runtimes are downcast ahead of this and never reach it.
fn config_option_models(
    connection: &Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    cx: &App,
) -> Vec<NativeAgentModelInfo> {
    let Some(config) = connection.session_config_options(session, cx) else {
        return Vec::new();
    };
    let Some(select) = config
        .config_options()
        .into_iter()
        .find(|option| option.id.0.as_ref() == kimi_adapter::MODEL_CONFIG_OPTION_ID)
        .and_then(|option| match option.kind {
            acp::SessionConfigKind::Select(select) => Some(select),
            // `#[non_exhaustive]`: a config kind this build cannot name is not
            // a model list, and guessing at one would put junk in the picker.
            _ => None,
        })
    else {
        return Vec::new();
    };
    select_options(&select.options)
        .into_iter()
        .map(|option| NativeAgentModelInfo {
            value: option.value.0.to_string(),
            display_name: option.name,
            description: option.description.unwrap_or_default(),
        })
        .collect()
}

fn select_options(
    options: &acp::SessionConfigSelectOptions,
) -> Vec<acp::SessionConfigSelectOption> {
    match options {
        acp::SessionConfigSelectOptions::Ungrouped(options) => options.clone(),
        acp::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.clone())
            .collect(),
        // `#[non_exhaustive]`.
        _ => Vec::new(),
    }
}

pub(crate) fn available_models(
    connection: Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    cx: &App,
) -> Vec<NativeAgentModelInfo> {
    if let Some(native) = connection
        .clone()
        .downcast::<claude_native::ClaudeNativeConnection>()
    {
        native.available_models(session)
    } else if let Some(native) = connection
        .clone()
        .downcast::<codex_native::CodexConnection>()
    {
        native
            .available_models(session)
            .into_iter()
            .map(model_info)
            .collect()
    } else {
        config_option_models(&connection, session, cx)
    }
}

pub(crate) fn probe_models(
    server: Rc<dyn AgentServer>,
    cwd: PathBuf,
    cx: &AsyncApp,
) -> Task<Result<Vec<NativeAgentModelInfo>>> {
    if let Some(native) = server
        .clone()
        .downcast::<claude_native::ClaudeNativeAgentServer>()
    {
        native.probe_models(cwd, cx)
    } else if let Some(native) = server.downcast::<codex_native::CodexAgentServer>() {
        let task = native.probe_models(cwd, cx);
        cx.spawn(async move |_| Ok(task.await?.into_iter().map(model_info).collect()))
    } else {
        // No throwaway-probe for an ACP-native agent: its model list is a
        // per-session config option, so there is nothing to read without
        // creating a session. The list is captured when the session goes live
        // and persisted on the session row, which covers the picker.
        Task::ready(Ok(Vec::new()))
    }
}

pub(crate) fn set_model(
    connection: Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    value: Option<String>,
    apply: bool,
    cx: &mut App,
) {
    if let Some(native) = connection
        .clone()
        .downcast::<claude_native::ClaudeNativeConnection>()
    {
        native.set_desired_model(session, value.clone());
        if apply && let Some(value) = value {
            native.select_model(session, value);
        }
    } else if let Some(native) = connection
        .clone()
        .downcast::<codex_native::CodexConnection>()
    {
        native.set_desired_model(session, value.clone());
        if apply && let Some(value) = value {
            native.select_model(session, value).log_err();
        }
    } else if apply && let Some(value) = value {
        // An ACP agent keeps no editor-side "desired" state to seed — the
        // model exists only as a session config option — so a non-applying
        // call has nothing to do here. The store persists the choice itself
        // and re-applies it once the session is live.
        set_config_option(
            &connection,
            session,
            kimi_adapter::MODEL_CONFIG_OPTION_ID,
            value,
            cx,
        );
    }
}

fn set_config_option(
    connection: &Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    config_id: &str,
    value: String,
    cx: &mut App,
) {
    let Some(config) = connection.session_config_options(session, cx) else {
        log::warn!(
            "session {} publishes no config options; cannot set `{config_id}` to `{value}`",
            session.0
        );
        return;
    };
    config
        .set_config_option(
            acp::SessionConfigId::new(config_id),
            acp::SessionConfigOptionValue::value_id(acp::SessionConfigValueId::new(value)),
            cx,
        )
        .detach_and_log_err(cx);
}

pub(crate) fn set_effort(
    connection: Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    value: Option<String>,
    apply: bool,
) {
    if let Some(native) = connection
        .clone()
        .downcast::<claude_native::ClaudeNativeConnection>()
    {
        native.set_desired_effort(session, value.clone());
        if apply && let Some(value) = value {
            native.select_effort(session, value);
        }
    } else if let Some(native) = connection.downcast::<codex_native::CodexConnection>() {
        native.set_desired_effort(session, value.clone());
        if apply && let Some(value) = value {
            native.select_effort(session, value).log_err();
        }
    }
}

pub(crate) fn codex_efforts(
    connection: Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    model: Option<&str>,
) -> Vec<String> {
    let Some(native) = connection.downcast::<codex_native::CodexConnection>() else {
        return Vec::new();
    };
    let models = native.available_models(session);
    let active = native.active_model(session);
    let selected = model.or_else(|| active.as_deref());
    models
        .iter()
        .find(|m| Some(m.value.as_str()) == selected)
        .map(|m| m.supported_efforts.clone())
        .unwrap_or_default()
}

/// Apply session-owned controls before exposing an ACP session to prompts.
/// A rejected mode/model fails the attach, rather than leaving the UI and the
/// running agent on different settings. Native runtimes use spawn metadata.
pub(crate) fn apply_acp_controls(
    connection: Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    mode: SessionPermissionMode,
    model: Option<String>,
    cx: &mut App,
) -> Task<Result<()>> {
    if !uses_acp_permission_modes(&connection) {
        return Task::ready(Ok(()));
    }
    let Some(modes) = connection.session_modes(session, cx) else {
        return Task::ready(Err(anyhow!(
            "The agent publishes no session permission modes"
        )));
    };
    let mode_id = kimi_adapter::mode_for(mode);
    if !modes.all_modes().iter().any(|m| m.id == mode_id) {
        return Task::ready(Err(anyhow!(
            "The agent does not support permission mode {mode_id:?}"
        )));
    }
    let config = connection.session_config_options(session, cx);
    if model.is_some() && config.is_none() {
        return Task::ready(Err(anyhow!("The agent publishes no model configuration")));
    }
    let mode_task = modes.set_mode(mode_id, cx);
    cx.spawn(async move |cx| {
        mode_task.await?;
        if let (Some(model), Some(config)) = (model, config) {
            cx.update(|cx| {
                config.set_config_option(
                    acp::SessionConfigId::new(kimi_adapter::MODEL_CONFIG_OPTION_ID),
                    acp::SessionConfigOptionValue::value_id(acp::SessionConfigValueId::new(model)),
                    cx,
                )
            })
            .await?;
        }
        Ok(())
    })
}

/// Whether `connection` enforces the fork's permission modes through ACP
/// session modes rather than through the `sawePermissionMode` session meta.
pub(crate) fn uses_acp_permission_modes(connection: &Rc<dyn AgentConnection>) -> bool {
    uses_acp_permission_modes_for_agent(connection.agent_id().0.as_ref())
}

/// [`uses_acp_permission_modes`] for a call site that has only the agent id —
/// a resume that has not attached a thread yet, or a `_meta` payload being
/// built for one.
pub(crate) fn uses_acp_permission_modes_for_agent(agent_id: &str) -> bool {
    agent_id == kimi_adapter::KIMI_AGENT_ID
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::{AgentSessionConfigOptions, AgentSessionModes};
    use std::cell::RefCell;

    struct Controls {
        mode_reply: async_channel::Receiver<bool>,
        model_reply: async_channel::Receiver<bool>,
        calls: RefCell<Vec<String>>,
    }

    impl AgentSessionModes for Controls {
        fn current_mode(&self) -> acp::SessionModeId {
            acp::SessionModeId::new("default")
        }
        fn all_modes(&self) -> Vec<acp::SessionMode> {
            vec![
                acp::SessionMode::new("auto", "Auto"),
                acp::SessionMode::new("plan", "Plan"),
            ]
        }
        fn set_mode(&self, mode: acp::SessionModeId, cx: &mut App) -> Task<Result<()>> {
            self.calls.borrow_mut().push(format!("mode:{}", mode.0));
            let reply = self.mode_reply.clone();
            cx.spawn(async move |_| {
                anyhow::ensure!(reply.recv().await?, "mode refused");
                Ok(())
            })
        }
    }
    impl AgentSessionConfigOptions for Controls {
        fn config_options(&self) -> Vec<acp::SessionConfigOption> {
            vec![]
        }
        fn set_config_option(
            &self,
            _: acp::SessionConfigId,
            _: acp::SessionConfigOptionValue,
            cx: &mut App,
        ) -> Task<Result<Vec<acp::SessionConfigOption>>> {
            self.calls.borrow_mut().push("model".into());
            let reply = self.model_reply.clone();
            cx.spawn(async move |_| {
                anyhow::ensure!(reply.recv().await?, "model refused");
                Ok(vec![])
            })
        }
    }
    struct Connection(Rc<Controls>);
    impl AgentConnection for Connection {
        fn agent_id(&self) -> project::AgentId {
            project::AgentId::new(kimi_adapter::KIMI_AGENT_ID)
        }
        fn telemetry_id(&self) -> gpui::SharedString {
            "test".into()
        }
        fn new_session(
            self: Rc<Self>,
            _: gpui::Entity<project::Project>,
            _: util::path_list::PathList,
            _: &mut App,
        ) -> Task<Result<gpui::Entity<acp_thread::AcpThread>>> {
            unimplemented!()
        }
        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }
        fn authenticate(&self, _: acp::AuthMethodId, _: &mut App) -> Task<Result<()>> {
            unimplemented!()
        }
        fn prompt(&self, _: acp::PromptRequest, _: &mut App) -> Task<Result<acp::PromptResponse>> {
            unimplemented!()
        }
        fn cancel(&self, _: &acp::SessionId, _: &mut App) {
            unimplemented!()
        }
        fn session_modes(&self, _: &acp::SessionId, _: &App) -> Option<Rc<dyn AgentSessionModes>> {
            Some(self.0.clone())
        }
        fn session_config_options(
            &self,
            _: &acp::SessionId,
            _: &App,
        ) -> Option<Rc<dyn AgentSessionConfigOptions>> {
            Some(self.0.clone())
        }
        fn into_any(self: Rc<Self>) -> Rc<dyn std::any::Any> {
            self
        }
    }

    fn fixture() -> (
        Rc<Controls>,
        async_channel::Sender<bool>,
        async_channel::Sender<bool>,
    ) {
        let (mode_tx, mode_reply) = async_channel::bounded(1);
        let (model_tx, model_reply) = async_channel::bounded(1);
        (
            Rc::new(Controls {
                mode_reply,
                model_reply,
                calls: RefCell::new(vec![]),
            }),
            mode_tx,
            model_tx,
        )
    }

    #[gpui::test]
    async fn attach_waits_for_permission_then_model_acknowledgement(cx: &mut gpui::TestAppContext) {
        let (controls, mode_tx, model_tx) = fixture();
        let (done_tx, done_rx) = async_channel::bounded(1);
        let apply = cx.update(|cx| {
            apply_acp_controls(
                Rc::new(Connection(controls.clone())),
                &acp::SessionId::new("session"),
                SessionPermissionMode::FullAccess,
                Some("chosen".into()),
                cx,
            )
        });
        cx.spawn(async move |_| {
            done_tx.send(apply.await).await.unwrap();
        })
        .detach();
        cx.executor().run_until_parked();
        assert_eq!(&*controls.calls.borrow(), &["mode:auto"]);
        assert!(done_rx.try_recv().is_err());
        mode_tx.send(true).await.unwrap();
        cx.executor().run_until_parked();
        assert_eq!(&*controls.calls.borrow(), &["mode:auto", "model"]);
        assert!(done_rx.try_recv().is_err());
        model_tx.send(true).await.unwrap();
        done_rx.recv().await.unwrap().unwrap();
    }

    #[gpui::test]
    async fn rejected_permission_blocks_model_application(cx: &mut gpui::TestAppContext) {
        let (controls, mode_tx, _model_tx) = fixture();
        let apply = cx.update(|cx| {
            apply_acp_controls(
                Rc::new(Connection(controls.clone())),
                &acp::SessionId::new("session"),
                SessionPermissionMode::ReadOnly,
                Some("chosen".into()),
                cx,
            )
        });
        mode_tx.send(false).await.unwrap();
        assert!(
            apply
                .await
                .unwrap_err()
                .to_string()
                .contains("mode refused")
        );
        assert_eq!(&*controls.calls.borrow(), &["mode:plan"]);
    }

    #[gpui::test]
    async fn rejected_model_fails_the_attach(cx: &mut gpui::TestAppContext) {
        let (controls, mode_tx, model_tx) = fixture();
        let apply = cx.update(|cx| {
            apply_acp_controls(
                Rc::new(Connection(controls)),
                &acp::SessionId::new("session"),
                SessionPermissionMode::FullAccess,
                Some("chosen".into()),
                cx,
            )
        });
        mode_tx.send(true).await.unwrap();
        model_tx.send(false).await.unwrap();
        assert!(
            apply
                .await
                .unwrap_err()
                .to_string()
                .contains("model refused")
        );
    }
}
