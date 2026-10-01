//! Native-runtime controls shared by session creation, restoration and the UI.
//!
//! Keep backend dispatch here so a new runtime cannot silently miss one of
//! the live/cold model and effort paths.
use std::{path::PathBuf, rc::Rc};

use acp_thread::{AgentConnection, NativeAgentModelInfo};
use agent_client_protocol::schema::v1 as acp;
use agent_servers::AgentServer;
use anyhow::Result;
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
    } else if let Some(native) = connection.clone().downcast::<codex_native::CodexConnection>() {
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
    } else if let Some(native) = connection.clone().downcast::<codex_native::CodexConnection>() {
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

/// Push the fork's binary permission mode onto an agent that enforces it
/// through ACP session modes, i.e. Kimi. Returns `false` when this agent has
/// no ACP modes to set, so the caller knows the mode was not applied and can
/// leave its own persisted value alone rather than claiming a change that did
/// not reach the CLI.
///
/// The two native runtimes enforce permissions through the `sawePermissionMode`
/// session meta instead and are rejected by the id check below — they do not
/// implement `session_modes`, so the check is belt and braces, but the
/// dispatch belongs here where every other runtime branch lives.
pub(crate) fn set_permission_mode(
    connection: Rc<dyn AgentConnection>,
    session: &acp::SessionId,
    mode: SessionPermissionMode,
    cx: &mut App,
) -> bool {
    if !uses_acp_permission_modes(&connection) {
        return false;
    }
    let Some(modes) = connection.session_modes(session, cx) else {
        log::warn!(
            "kimi session {} advertises no ACP modes; permission mode not applied",
            session.0
        );
        return false;
    };
    let mode_id = kimi_adapter::mode_for(mode);
    // Check before setting: a mode the CLI never advertised would be rejected
    // anyway, and failing here keeps the session on the mode it actually runs
    // in instead of letting the UI read "full access" over a chat that is
    // still stopping to ask.
    if !modes.all_modes().iter().any(|m| m.id == mode_id) {
        log::warn!(
            "kimi does not offer session mode `{mode_id:?}`; permission mode not applied"
        );
        return false;
    }
    modes.set_mode(mode_id, cx).detach_and_log_err(cx);
    true
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
