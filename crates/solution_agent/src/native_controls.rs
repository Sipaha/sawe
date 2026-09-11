//! Native-runtime controls shared by session creation, restoration and the UI.
//!
//! Keep backend dispatch here so a new runtime cannot silently miss one of
//! the live/cold model and effort paths.
use std::{path::PathBuf, rc::Rc};

use acp_thread::{AgentConnection, NativeAgentModelInfo};
use agent_client_protocol::schema as acp;
use agent_servers::AgentServer;
use anyhow::Result;
use gpui::{AsyncApp, Task};
use util::ResultExt as _;

fn model_info(model: codex_native::CodexModelInfo) -> NativeAgentModelInfo {
    NativeAgentModelInfo {
        value: model.value,
        display_name: model.display_name,
        description: model.description,
    }
}

pub(crate) fn available_models(
    connection: Rc<dyn AgentConnection>,
    session: &acp::SessionId,
) -> Vec<NativeAgentModelInfo> {
    if let Some(native) = connection
        .clone()
        .downcast::<claude_native::ClaudeNativeConnection>()
    {
        native.available_models(session)
    } else if let Some(native) = connection.downcast::<codex_native::CodexConnection>() {
        native
            .available_models(session)
            .into_iter()
            .map(model_info)
            .collect()
    } else {
        Vec::new()
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
        Task::ready(Ok(Vec::new()))
    }
}

pub(crate) fn set_model(
    connection: Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    value: Option<String>,
    apply: bool,
) {
    if let Some(native) = connection
        .clone()
        .downcast::<claude_native::ClaudeNativeConnection>()
    {
        native.set_desired_model(session, value.clone());
        if apply && let Some(value) = value {
            native.select_model(session, value);
        }
    } else if let Some(native) = connection.downcast::<codex_native::CodexConnection>() {
        native.set_desired_model(session, value.clone());
        if apply && let Some(value) = value {
            native.select_model(session, value).log_err();
        }
    }
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
