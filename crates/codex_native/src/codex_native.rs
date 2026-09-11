mod process;
mod steering;
mod translate;
pub use steering::SteerOutcome;

use acp_thread::{AcpThread, AgentConnection, AuthorizationKind, PermissionOptions, UserMessageId};
use action_log::ActionLog;
use agent_client_protocol::schema as acp;
use agent_servers::{AgentServer, AgentServerDelegate, mcp_servers_for_project};
use anyhow::{Context as _, Result, anyhow, bail};
use futures::{StreamExt as _, channel::oneshot};
use gpui::{App, AppContext as _, AsyncApp, Entity, SharedString, Task};
use process::Process;
use project::{AgentId, Project};
use serde_json::{Value, json};
use std::{any::Any, cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, time::Duration};
use ui::IconName;
use util::{ResultExt as _, path_list::PathList};

#[derive(Clone, Debug)]
pub struct CodexModelInfo {
    pub value: String,
    pub display_name: String,
    pub description: String,
    pub supported_efforts: Vec<String>,
    pub default_effort: Option<String>,
}
pub struct CodexAgentServer {
    agent_id: AgentId,
}
impl CodexAgentServer {
    pub fn new(agent_id: AgentId) -> Self {
        Self { agent_id }
    }
    pub fn probe_models(
        &self,
        directory: PathBuf,
        cx: &AsyncApp,
    ) -> Task<Result<Vec<CodexModelInfo>>> {
        cx.spawn(async move |cx| {
            let process = cx.update(|cx| Process::spawn(&directory, cx))?;
            process.initialize().await?;
            models(&process).await
        })
    }
}
impl AgentServer for CodexAgentServer {
    fn logo(&self) -> IconName {
        IconName::AiOpenAi
    }
    fn agent_id(&self) -> AgentId {
        self.agent_id.clone()
    }
    fn connect(
        &self,
        _: AgentServerDelegate,
        _: Entity<Project>,
        _: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        Task::ready(Ok(Rc::new(CodexConnection {
            agent_id: self.agent_id.clone(),
            sessions: RefCell::new(HashMap::new()),
            desired_models: RefCell::new(HashMap::new()),
            desired_efforts: RefCell::new(HashMap::new()),
        })))
    }
    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}
struct Session {
    process: Rc<Process>,
    state: Rc<RefCell<TurnState>>,
    models: Vec<CodexModelInfo>,
    _pump: Task<()>,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.process.kill();
        self.state
            .borrow_mut()
            .finish(Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)));
    }
}
#[derive(Default)]
struct TurnState {
    sender: Option<oneshot::Sender<Result<acp::PromptResponse>>>,
    turn_id: Option<String>,
    cancel_requested: bool,
    disconnected: bool,
    generation: u64,
    active_model: Option<String>,
}
impl TurnState {
    fn finish(&mut self, result: Result<acp::PromptResponse>) {
        self.turn_id = None;
        self.cancel_requested = false;
        if let Some(sender) = self.sender.take()
            && sender.send(result).is_err()
        {
            log::debug!("Codex prompt receiver dropped");
        }
    }
}
pub struct CodexConnection {
    agent_id: AgentId,
    sessions: RefCell<HashMap<acp::SessionId, Session>>,
    desired_models: RefCell<HashMap<acp::SessionId, String>>,
    desired_efforts: RefCell<HashMap<acp::SessionId, String>>,
}
impl CodexConnection {
    /// Add input to the currently active turn. Acceptance is not completion:
    /// the original prompt future remains owned by turn/completed.
    pub fn steer(
        &self,
        id: &acp::SessionId,
        blocks: Vec<acp::ContentBlock>,
        client_message_id: String,
        cx: &mut App,
    ) -> Task<SteerOutcome> {
        let sessions = self.sessions.borrow();
        let Some(session) = sessions.get(id) else {
            return Task::ready(SteerOutcome::Rejected(anyhow!("Codex session is closed")));
        };
        let input = match translate::input(&blocks) {
            Ok(input) => input,
            Err(error) => return Task::ready(SteerOutcome::Rejected(error)),
        };
        let state = session.state.clone();
        let process = session.process.clone();
        let generation = state.borrow().generation;
        let id = id.clone();
        cx.spawn(async move |cx| {
            // A follow-up may precede the turn/start acknowledgement. Wait for
            // this generation's id, never pick up a subsequent turn's id.
            for _ in 0..100 {
                let unavailable = {
                    let state = state.borrow();
                    state.generation != generation
                        || state.sender.is_none()
                        || state.cancel_requested
                        || state.disconnected
                };
                if unavailable {
                    return SteerOutcome::Rejected(anyhow!(
                        "The original Codex turn is no longer active"
                    ));
                }
                let turn_id = state.borrow().turn_id.clone();
                if let Some(turn_id) = turn_id {
                    return steering::request(&id, &turn_id, input, client_message_id, |params| {
                        process.request("turn/steer", params)
                    })
                    .await;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
            SteerOutcome::Rejected(anyhow!("Codex has not acknowledged the active turn yet"))
        })
    }

    pub fn available_models(&self, id: &acp::SessionId) -> Vec<CodexModelInfo> {
        self.sessions
            .borrow()
            .get(id)
            .map(|s| s.models.clone())
            .unwrap_or_default()
    }
    pub fn set_desired_model(&self, id: &acp::SessionId, model: Option<String>) {
        match model {
            Some(model) => {
                self.desired_models.borrow_mut().insert(id.clone(), model);
            }
            None => {
                self.desired_models.borrow_mut().remove(id);
            }
        }
    }
    pub fn select_model(&self, id: &acp::SessionId, model: String) -> Result<()> {
        let models = self.available_models(id);
        let selected = models
            .iter()
            .find(|candidate| candidate.value == model)
            .context("This Codex model is not available")?;
        let effort = self.desired_efforts.borrow().get(id).cloned();
        if effort.is_some_and(|effort| !selected.supported_efforts.contains(&effort)) {
            self.set_desired_effort(id, None);
        }
        self.set_desired_model(id, Some(model));
        Ok(())
    }
    pub fn set_desired_effort(&self, id: &acp::SessionId, effort: Option<String>) {
        match effort {
            Some(effort) => {
                self.desired_efforts.borrow_mut().insert(id.clone(), effort);
            }
            None => {
                self.desired_efforts.borrow_mut().remove(id);
            }
        }
    }
    pub fn select_effort(&self, id: &acp::SessionId, effort: String) -> Result<()> {
        let selected = self.desired_models.borrow().get(id).cloned();
        if !self.available_models(id).iter().any(|model| {
            Some(&model.value) == selected.as_ref() && model.supported_efforts.contains(&effort)
        }) {
            bail!("This reasoning effort is not supported by the selected Codex model");
        }
        self.set_desired_effort(id, Some(effort));
        Ok(())
    }
    fn open(
        self: Rc<Self>,
        resume: Option<acp::SessionId>,
        project: Entity<Project>,
        paths: PathList,
        title: Option<SharedString>,
        meta: Option<acp::Meta>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let Some(directory) = paths.ordered_paths().next().cloned() else {
            return Task::ready(Err(anyhow!("Working directory cannot be empty")));
        };
        let config = mcp_config(&mcp_servers_for_project(&project, cx));
        cx.spawn(async move |cx| {
            let mut process = cx.update(|cx| Process::spawn(&directory, cx))?;
            process.initialize().await?;
            let account = process.request("account/read", json!({"refreshToken":false})).await?;
            if account["requiresOpenaiAuth"].as_bool() == Some(true) && account["account"].is_null() { bail!("Codex is not signed in. Run `codex login` in a terminal, complete sign-in, then reopen this chat."); }
            let available_models = models(&process).await?;
            let mut params = json!({"cwd":directory,"config":config,"approvalPolicy":"on-request","approvalsReviewer":"user","sandbox":"workspace-write"});
            if let Some(meta) = &meta {
                if let Some(prompt) = meta.get("systemPrompt").and_then(|prompt| prompt.as_str().or_else(|| prompt.get("append").and_then(Value::as_str))) { params["developerInstructions"] = json!(prompt); }
                if let Some(model) = meta.get("modelId").and_then(Value::as_str) { params["model"] = json!(model); }
            }
            let method = if let Some(id) = &resume {
                params["threadId"] = json!(id.0); params["excludeTurns"] = json!(true);
                if let Some(model) = self.desired_models.borrow().get(id) { params["model"] = json!(model); }
                "thread/resume"
            } else { "thread/start" };
            let response = process.request(method, params).await?;
            let id = acp::SessionId::new(response["thread"]["id"].as_str().context("Codex did not return a thread id")?.to_owned());
            if let Some(effort) = meta.as_ref().and_then(|m| m.get("reasoningEffort")).and_then(Value::as_str) { self.set_desired_effort(&id, Some(effort.into())); }
            let active_model = response["model"].as_str().map(str::to_owned);
            if let Some(model) = &active_model { self.set_desired_model(&id, Some(model.clone())); }
            let thread = cx.update(|cx| {
                let action_log = cx.new(|_| ActionLog::new(project.clone()));
                cx.new(|cx| AcpThread::new(None, title, Some(paths), self.clone(), project, action_log, id.clone(), watch::Receiver::constant(acp::PromptCapabilities::new().image(true)), cx))
            });
            let (_, closed) = futures::channel::mpsc::unbounded();
            let mut incoming = std::mem::replace(&mut process.incoming, closed);
            let process = Rc::new(process);
            let state = Rc::new(RefCell::new(TurnState {active_model, ..Default::default()}));
            let pump_state = state.clone(); let weak_thread = thread.downgrade(); let weak_process = Rc::downgrade(&process);
            let pump_id = id.clone();
            let pump = cx.spawn(async move |cx| {
                let mut translator = translate::Translator::default();
                while let Some(message) = incoming.next().await {
                    if message.get("id").is_some() {
                        if let Some(process) = weak_process.upgrade() { handle_approval(message, &pump_id, weak_thread.clone(), process, cx); }
                        continue;
                    }
                    let method = message["method"].as_str().unwrap_or("");
                    if method == "sawe/disconnected" { break; }
                    let params = &message["params"];
                    if !belongs_to_thread(params, &pump_id) {continue;}
                    if method == "turn/started" { pump_state.borrow_mut().turn_id = params["turn"]["id"].as_str().map(str::to_owned); }
                    for update in translator.translate(method, params) { weak_thread.update(cx, |thread, cx| thread.handle_session_update(update, cx).log_err()).log_err(); }
                    if method == "turn/completed" { let result = translate::turn_result(&params["turn"]); pump_state.borrow_mut().finish(result); translator = translate::Translator::default(); }
                }
                if let Some(process) = weak_process.upgrade() { process.kill(); }
                pump_state.borrow_mut().disconnected = true;
                pump_state.borrow_mut().finish(Err(anyhow!("Codex process disconnected. Reopen this chat to reconnect.")));
            });
            self.sessions.borrow_mut().insert(id, Session {process, state, models: available_models, _pump: pump});
            Ok(thread)
        })
    }
}
impl AgentConnection for CodexConnection {
    fn agent_id(&self) -> AgentId {
        self.agent_id.clone()
    }
    fn telemetry_id(&self) -> SharedString {
        "codex-native".into()
    }
    fn new_session(
        self: Rc<Self>,
        project: Entity<Project>,
        paths: PathList,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        self.open(None, project, paths, None, None, cx)
    }
    fn new_session_with_meta(
        self: Rc<Self>,
        project: Entity<Project>,
        paths: PathList,
        meta: Option<acp::Meta>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        self.open(None, project, paths, None, meta, cx)
    }
    fn supports_resume_session(&self) -> bool {
        true
    }
    fn resume_session(
        self: Rc<Self>,
        id: acp::SessionId,
        project: Entity<Project>,
        paths: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        self.open(Some(id), project, paths, title, None, cx)
    }
    fn supports_close_session(&self) -> bool {
        true
    }
    fn close_session(self: Rc<Self>, id: &acp::SessionId, _: &mut App) -> Task<Result<()>> {
        self.sessions.borrow_mut().remove(id);
        Task::ready(Ok(()))
    }
    fn auth_methods(&self) -> &[acp::AuthMethod] {
        &[]
    }
    fn authenticate(&self, _: acp::AuthMethodId, _: &mut App) -> Task<Result<()>> {
        Task::ready(Err(anyhow!(
            "Run `codex login` in a terminal, then reopen this chat."
        )))
    }
    fn active_model(&self, id: &acp::SessionId) -> Option<SharedString> {
        self.sessions
            .borrow()
            .get(id)?
            .state
            .borrow()
            .active_model
            .clone()
            .map(Into::into)
    }
    fn prompt(
        &self,
        _: UserMessageId,
        params: acp::PromptRequest,
        cx: &mut App,
    ) -> Task<Result<acp::PromptResponse>> {
        let sessions = self.sessions.borrow();
        let Some(session) = sessions.get(&params.session_id) else {
            return Task::ready(Err(anyhow!("Codex session is closed")));
        };
        if session.state.borrow().disconnected {
            return Task::ready(Err(anyhow!(
                "Codex process disconnected. Reopen this chat to reconnect."
            )));
        }
        let input = match translate::input(&params.prompt) {
            Ok(input) => input,
            Err(error) => return Task::ready(Err(error)),
        };
        if session.state.borrow().sender.is_some() {
            return Task::ready(Err(anyhow!("Codex is already responding in this chat")));
        }
        let (sender, receiver) = oneshot::channel();
        session.state.borrow_mut().sender = Some(sender);
        session.state.borrow_mut().generation += 1;
        let process = session.process.clone();
        let state = session.state.clone();
        let model = self
            .desired_models
            .borrow()
            .get(&params.session_id)
            .cloned();
        let effort = self
            .desired_efforts
            .borrow()
            .get(&params.session_id)
            .cloned()
            .filter(|effort| {
                session.models.iter().any(|candidate| {
                    Some(&candidate.value) == model.as_ref()
                        && candidate.supported_efforts.contains(effort)
                })
            });
        cx.spawn(async move |_| {
            let response = process.request("turn/start",json!({"threadId":params.session_id.0,"input":input,"model":model,"effort":effort})).await;
            match response {
                Ok(response) => {let mut state = state.borrow_mut(); if state.sender.is_some() {state.turn_id = response["turn"]["id"].as_str().map(str::to_owned); state.active_model = model.or(state.active_model.take());}}
                Err(error) => {process.kill(); state.borrow_mut().disconnected = true; state.borrow_mut().finish(Err(error));}
            }
            receiver.await.context("Codex session closed during response")?
        })
    }
    fn cancel(&self, id: &acp::SessionId, cx: &mut App) {
        let sessions = self.sessions.borrow();
        let Some(session) = sessions.get(id) else {
            return;
        };
        if session.state.borrow().sender.is_none() || session.state.borrow().cancel_requested {
            return;
        }
        session.state.borrow_mut().cancel_requested = true;
        let generation = session.state.borrow().generation;
        let state = session.state.clone();
        let process = session.process.clone();
        let id = id.clone();
        cx.spawn(async move |cx| {
            // A stop can arrive before turn/start responds; wait briefly for its id.
            for _ in 0..100 {
                if state.borrow().generation != generation {
                    return;
                }
                if state.borrow().turn_id.is_some() || state.borrow().sender.is_none() {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
            if state.borrow().generation != generation || !state.borrow().cancel_requested {
                return;
            }
            let turn_id = state.borrow().turn_id.clone();
            if let Some(turn_id) = turn_id {
                process
                    .request("turn/interrupt", json!({"threadId":id.0,"turnId":turn_id}))
                    .await
                    .log_err();
            }
            cx.background_executor()
                .timer(Duration::from_secs(10))
                .await;
            if state.borrow().generation == generation && state.borrow().cancel_requested {
                process.kill();
                state
                    .borrow_mut()
                    .finish(Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)));
            }
        })
        .detach();
    }
    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}
async fn models(process: &Process) -> Result<Vec<CodexModelInfo>> {
    let mut cursor = Value::Null;
    let mut models = Vec::new();
    loop {
        let response = process
            .request("model/list", json!({"cursor":cursor,"limit":100}))
            .await?;
        for model in response["data"]
            .as_array()
            .context("Invalid Codex model catalog")?
        {
            if model["hidden"].as_bool() == Some(true) {
                continue;
            }
            models.push(CodexModelInfo {
                value: model["model"].as_str().context("Missing model id")?.into(),
                display_name: model["displayName"].as_str().unwrap_or_default().into(),
                description: model["description"].as_str().unwrap_or_default().into(),
                supported_efforts: model["supportedReasoningEfforts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e["reasoningEffort"].as_str().map(str::to_owned))
                    .collect(),
                default_effort: model["defaultReasoningEffort"].as_str().map(str::to_owned),
            });
        }
        cursor = response["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
    }
    Ok(models)
}
fn mcp_config(servers: &[acp::McpServer]) -> Value {
    let mut config = serde_json::Map::new();
    for server in servers {
        match server {
            acp::McpServer::Stdio(server) => {
                let env: serde_json::Map<String, Value> = server
                    .env
                    .iter()
                    .map(|entry| (entry.name.clone(), json!(entry.value)))
                    .collect();
                let mut entry = json!({"command":server.command,"args":server.args,"env":env});
                // Solution collaboration is an editor capability: peer delivery
                // enforces scope and user-input gates in the host. Approve only
                // discovery and peer send on the built-in bridge, not arbitrary
                // MCP writes or the separate human-input endpoint.
                if server.name == "sawe"
                    && server.args.first().is_some_and(|arg| arg == "--nc")
                    && server.args.len() == 2
                {
                    entry["tools"] = json!({
                        "solution_agent.list_sessions": {"approval_mode":"approve"},
                        "solution_agent.send_agent_message": {"approval_mode":"approve"}
                    });
                }
                config.insert(format!("mcp_servers.{}", server.name), entry);
            }
            acp::McpServer::Http(server) => {
                let headers: serde_json::Map<String, Value> = server
                    .headers
                    .iter()
                    .map(|entry| (entry.name.clone(), json!(entry.value)))
                    .collect();
                config.insert(
                    format!("mcp_servers.{}", server.name),
                    json!({"url":server.url,"http_headers":headers}),
                );
            }
            _ => log::warn!("Codex does not support this MCP transport"),
        }
    }
    Value::Object(config)
}
fn handle_approval(
    message: Value,
    session_id: &acp::SessionId,
    thread: gpui::WeakEntity<AcpThread>,
    process: Rc<Process>,
    cx: &mut AsyncApp,
) {
    let permitted_thread = belongs_to_thread(&message["params"], session_id);
    let outgoing = process.outgoing.clone();
    drop(process);
    cx.spawn(async move |cx| {
        let method = message["method"].as_str().unwrap_or_default(); let params = &message["params"];
        let result = if matches!(method,"item/commandExecution/requestApproval"|"item/fileChange/requestApproval") {
            if !permitted_thread {
                outgoing.unbounded_send(json!({"id":message["id"],"result":{"decision":"decline"}})).log_err();
                return;
            }
            let title = params["command"].as_str().or(params["reason"].as_str()).unwrap_or("Codex requests permission");
            let call = acp::ToolCallUpdate::new(acp::ToolCallId::new(params["itemId"].as_str().unwrap_or("approval").to_owned()), acp::ToolCallUpdateFields::new().title(title.to_owned()).raw_input(params.clone()));
            let options = vec![acp::PermissionOption::new("allow", "Allow once", acp::PermissionOptionKind::AllowOnce),acp::PermissionOption::new("deny", "Deny", acp::PermissionOptionKind::RejectOnce)];
            let task = thread.update(cx, |thread,cx| thread.request_tool_call_authorization(call, PermissionOptions::Flat(options), AuthorizationKind::PermissionGrant,cx));
            let allow = match task {Ok(Ok(task)) => {let outcome: acp::RequestPermissionOutcome = task.await.into(); matches!(outcome, acp::RequestPermissionOutcome::Selected(selected) if selected.option_id.0.as_ref() == "allow")}, _ => false};
            json!({"decision":if allow {"accept"} else {"decline"}})
        } else if method == "item/permissions/requestApproval" {json!({"permissions":{},"scope":"turn"})}
        else if method == "mcpServer/elicitation/request" {json!({"action":"decline","content":null})}
        else if method == "item/tool/requestUserInput" {json!({"answers":{}})}
        else {outgoing.unbounded_send(json!({"id":message["id"],"error":{"code":-32601,"message":"This Codex request is not supported by Sawe"}})).log_err();return;};
        outgoing.unbounded_send(json!({"id":message["id"],"result":result})).log_err();
    }).detach();
}

fn belongs_to_thread(params: &Value, id: &acp::SessionId) -> bool {
    params["threadId"].as_str() == Some(id.0.as_ref())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forwards_stdio_and_http_mcp_configuration() {
        let config = mcp_config(&[
            acp::McpServer::Stdio(
                acp::McpServerStdio::new("sawe", "/bin/sawe")
                    .args(vec!["--nc".into(), "/tmp/solution/mcp.sock".into()])
                    .env(vec![acp::EnvVariable::new("SCOPE", "test")]),
            ),
            acp::McpServer::Http(
                acp::McpServerHttp::new("remote", "https://example.com/mcp")
                    .headers(vec![acp::HttpHeader::new("Authorization", "Bearer test")]),
            ),
        ]);
        assert_eq!(config["mcp_servers.sawe"]["command"], "/bin/sawe");
        assert_eq!(config["mcp_servers.sawe"]["env"]["SCOPE"], "test");
        assert_eq!(
            config["mcp_servers.sawe"]["tools"]["solution_agent.send_agent_message"]["approval_mode"],
            "approve"
        );
        assert_eq!(
            config["mcp_servers.sawe"]["tools"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
        assert!(config["mcp_servers.remote"].get("tools").is_none());
        let external = mcp_config(&[acp::McpServer::Stdio(acp::McpServerStdio::new(
            "sawe",
            "/bin/external",
        ))]);
        assert!(external["mcp_servers.sawe"].get("tools").is_none());
        assert_eq!(
            config["mcp_servers.remote"]["url"],
            "https://example.com/mcp"
        );
        assert_eq!(
            config["mcp_servers.remote"]["http_headers"]["Authorization"],
            "Bearer test"
        );
    }
    #[test]
    fn child_and_unscoped_events_cannot_complete_parent_turn() {
        let id = acp::SessionId::new("parent");
        assert!(belongs_to_thread(&json!({"threadId":"parent"}), &id));
        assert!(!belongs_to_thread(&json!({"threadId":"child"}), &id));
        assert!(!belongs_to_thread(&json!({}), &id));
    }
}
