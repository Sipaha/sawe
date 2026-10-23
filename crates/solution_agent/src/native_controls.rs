//! Harness-neutral controls published by SPK AI Hub through ACP capabilities.
use crate::model::SessionPermissionMode;
use acp_thread::{AgentConnection, NativeAgentModelInfo};
use agent_client_protocol::schema::v1 as acp;
use agent_servers::AgentServer;
use anyhow::{Result, anyhow};
use gpui::{App, AsyncApp, Task, TaskExt as _};
use std::{path::PathBuf, rc::Rc};

#[cfg(any(test, feature = "test-support"))]
#[path = "test_support/native_controls_legacy.rs"]
mod legacy;

pub(crate) fn runtime_turn_active(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    cx: &App,
) -> Option<bool> {
    let t = thread.read(cx);
    t.connection().runtime_active(t.session_id())
}
fn select_options(
    options: &acp::SessionConfigSelectOptions,
) -> Vec<acp::SessionConfigSelectOption> {
    match options {
        acp::SessionConfigSelectOptions::Ungrouped(o) => o.clone(),
        acp::SessionConfigSelectOptions::Grouped(g) => {
            g.iter().flat_map(|g| g.options.clone()).collect()
        }
        _ => Vec::new(),
    }
}
pub(crate) fn available_models(
    connection: Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    cx: &App,
) -> Vec<NativeAgentModelInfo> {
    #[cfg(any(test, feature = "test-support"))]
    if !connection.is_hub() {
        return legacy::available_models(connection, session, cx);
    }
    connection
        .session_config_options(session, cx)
        .and_then(|c| {
            c.config_options()
                .into_iter()
                .find(|o| o.id.0.as_ref() == "model")
        })
        .and_then(|o| match o.kind {
            acp::SessionConfigKind::Select(s) => Some(
                select_options(&s.options)
                    .into_iter()
                    .map(|o| NativeAgentModelInfo {
                        value: o.value.0.to_string(),
                        display_name: o.name,
                        description: o.description.unwrap_or_default(),
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}
pub(crate) fn probe_models(
    server: Rc<dyn AgentServer>,
    cwd: PathBuf,
    cx: &AsyncApp,
) -> Task<Result<Vec<NativeAgentModelInfo>>> {
    if let Some(hub) = server.clone().downcast::<agent_servers::HubAgentServer>() {
        return hub.probe_models(cwd, cx);
    }
    #[cfg(any(test, feature = "test-support"))]
    {
        return legacy::probe_models(server, cwd, cx);
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        Task::ready(Ok(Vec::new()))
    }
}
fn set_config(
    connection: &Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    id: &str,
    value: String,
    cx: &mut App,
) {
    if let Some(config) = connection.session_config_options(session, cx) {
        config
            .set_config_option(
                acp::SessionConfigId::new(id.to_owned()),
                acp::SessionConfigOptionValue::value_id(acp::SessionConfigValueId::new(value)),
                cx,
            )
            .detach_and_log_err(cx)
    }
}
pub(crate) fn set_model(
    connection: Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    value: Option<String>,
    apply: bool,
    cx: &mut App,
) {
    #[cfg(any(test, feature = "test-support"))]
    if !connection.is_hub() {
        return legacy::set_model(connection, session, value, apply, cx);
    }
    if apply && let Some(value) = value {
        set_config(&connection, session, "model", value, cx)
    }
}
pub(crate) fn set_effort(
    connection: Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    value: Option<String>,
    apply: bool,
    cx: &mut App,
) {
    #[cfg(any(test, feature = "test-support"))]
    if !connection.is_hub() {
        return legacy::set_effort(connection, session, value, apply, cx);
    }
    if apply && let Some(value) = value {
        set_config(&connection, session, "reasoning_effort", value, cx)
    }
}
pub(crate) fn effort_options(
    connection: Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    model: Option<&str>,
    cx: &App,
) -> Vec<String> {
    #[cfg(any(test, feature = "test-support"))]
    if !connection.is_hub() {
        return legacy::codex_efforts(connection, session, model, cx);
    }
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = model;
    connection
        .session_config_options(session, cx)
        .and_then(|c| {
            c.config_options()
                .into_iter()
                .find(|o| o.id.0.as_ref() == "reasoning_effort")
        })
        .and_then(|o| match o.kind {
            acp::SessionConfigKind::Select(s) => Some(
                select_options(&s.options)
                    .into_iter()
                    .map(|o| o.value.0.to_string())
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}
pub(crate) fn uses_acp_permission_modes_for_agent(_id: &str) -> bool {
    #[cfg(any(test, feature = "test-support"))]
    {
        return legacy::uses_acp_permission_modes_for_agent(_id);
    }
    #[cfg(not(any(test, feature = "test-support")))]
    false
}
pub(crate) fn apply_acp_controls(
    connection: Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    mode: SessionPermissionMode,
    model: Option<String>,
    cx: &mut App,
) -> Task<Result<()>> {
    #[cfg(any(test, feature = "test-support"))]
    if !connection.is_hub() {
        return legacy::apply_acp_controls(connection, session, mode, model, cx);
    }
    if connection.is_hub() {
        return Task::ready(Ok(()));
    }
    let Some(modes) = connection.session_modes(session, cx) else {
        return Task::ready(Err(anyhow!("The agent publishes no permission controls")));
    };
    let mode = acp::SessionModeId::new(mode.as_str().to_owned());
    let config = connection.session_config_options(session, cx);
    let task = modes.set_mode(mode, cx);
    cx.spawn(async move |cx| {
        task.await?;
        if let (Some(model), Some(config)) = (model, config) {
            cx.update(|cx| {
                config.set_config_option(
                    acp::SessionConfigId::new("model"),
                    acp::SessionConfigOptionValue::value_id(acp::SessionConfigValueId::new(model)),
                    cx,
                )
            })
            .await?;
        }
        Ok(())
    })
}
