mod approval;
mod process;
mod steering;
mod translate;
pub use steering::SteerOutcome;

use acp_thread::{AcpThread, AgentConnection, AuthorizationKind, PermissionOptions};
use action_log::ActionLog;
use agent_client_protocol::schema::v1 as acp;
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
    _generation_profile: Option<GenerationCatalog>,
    process: Rc<Process>,
    state: Rc<RefCell<TurnState>>,
    models: Vec<CodexModelInfo>,
    thread: gpui::WeakEntity<AcpThread>,
    _pump: Task<()>,
    _capacity_retry: Rc<RefCell<Option<Task<()>>>>,
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
    active_effort: Option<String>,
    retry_model: Option<String>,
    capacity_retry_count: usize,
    capacity_retry_waiting: bool,
    capacity_retry_starting: bool,
    capacity_retry_started: bool,
}
impl TurnState {
    fn has_active_turn(&self) -> bool {
        !self.disconnected && (self.sender.is_some() || self.turn_id.is_some()
            || self.capacity_retry_waiting || self.capacity_retry_starting)
    }

    fn started(&mut self, id: String) -> bool {
        let autonomous = self.sender.is_none() && self.turn_id.as_ref() != Some(&id)
            && !self.capacity_retry_starting;
        if autonomous {
            self.generation += 1;
            self.cancel_requested = false;
            self.capacity_retry_count = 0;
        }
        self.capacity_retry_waiting = false;
        self.capacity_retry_starting = false;
        self.capacity_retry_started = self.capacity_retry_count > 0;
        self.turn_id = Some(id);
        autonomous
    }

    /// Client-owned completion resolves prompt(); autonomous completion needs
    /// an explicit thread event. A late completion cannot finish a newer turn.
    fn completed(&mut self, id: &str, result: Result<acp::PromptResponse>) -> Option<Result<acp::PromptResponse>> {
        if self.turn_id.as_deref() != Some(id) {
            return None;
        }
        if self.sender.is_some() {
            self.finish(result);
            None
        } else {
            self.turn_id = None;
            self.cancel_requested = false;
            self.capacity_retry_count = 0;
            self.capacity_retry_waiting = false;
            self.capacity_retry_starting = false;
            Some(result)
        }
    }

    fn capacity_retry_delay(&mut self, turn: &Value) -> Option<Duration> {
        let Some(id) = turn["id"].as_str() else { return None; };
        if self.turn_id.as_deref() != Some(id)
            || self.cancel_requested || !translate::is_capacity_failure(turn) {
            return None;
        }
        let seconds = [10, 20, 40].get(self.capacity_retry_count).copied()?;
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        self.turn_id.hash(&mut hash);
        self.capacity_retry_count.hash(&mut hash);
        let jitter = Duration::from_millis(hash.finish() % 1000);
        self.capacity_retry_count += 1;
        self.capacity_retry_started = false;
        self.turn_id = None;
        self.capacity_retry_waiting = true;
        self.capacity_retry_starting = false;
        Some(Duration::from_secs(seconds) + jitter)
    }

    fn finish(&mut self, result: Result<acp::PromptResponse>) {
        self.capacity_retry_waiting = false;
        self.capacity_retry_starting = false;
        self.capacity_retry_count = 0;
        self.capacity_retry_started = false;
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
    /// Includes provider-owned turns, which have no client prompt sender.
    pub fn has_active_turn(&self, id: &acp::SessionId) -> bool {
        self.sessions.borrow().get(id).is_some_and(|session| session.state.borrow().has_active_turn())
    }
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
                        || !state.has_active_turn()
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
        let generation_only = meta
            .as_ref()
            .and_then(|m| m.get("generationOnly"))
            .and_then(Value::as_bool)
            == Some(true);
        let read_only = generation_only
            || meta
                .as_ref()
                .and_then(|m| m.get("sawePermissionMode"))
                .and_then(Value::as_str)
                == Some("read_only");
        let mut config = session_config(&if generation_only {
            vec![]
        } else {
            mcp_servers_for_project(&project, cx)
        });
        if generation_only {
            if let Some(config) = config.as_object_mut() {
                config.extend(generation_config());
            }
        }
        cx.spawn(async move |cx| {
            let mut process = cx.update(|cx| {
                if generation_only {
                    Process::spawn_with_config(&directory, config.as_object().context("Codex configuration is not an object")?, cx)
                } else {
                    Process::spawn(&directory, cx)
                }
            })?;
            process.initialize().await?;
            let effective = if read_only {
                Some(process.request("config/read", json!({"includeLayers":false,"cwd":directory})).await?)
            } else { None };

            let account = process.request("account/read", json!({"refreshToken":false})).await?;
            if account["requiresOpenaiAuth"].as_bool() == Some(true) && account["account"].is_null() { bail!("Codex is not signed in. Run `codex login` in a terminal, complete sign-in, then reopen this chat."); }
            if generation_only && (account["account"]["type"].as_str() != Some("chatgpt") || account["requiresOpenaiAuth"].as_bool() != Some(true)) {
                bail!("Codex text generation requires the CLI subscription login. Run `codex login` in a terminal; API-key accounts are not used.");
            }
            let available_models = models(&process).await?;
            let generation_profile = if generation_only {
                let resolved = effective.as_ref().map(|response| response["config"].clone()).context("Generation requires resolved Codex configuration")?;
                disable_mcp_servers(&mut config, &resolved);
                let root = directory.clone();
                let profile = cx.background_spawn(async move { generation_catalog(&root, &resolved) }).await?;
                config["model_catalog_json"] = json!(profile.directory.path().join("models.json"));
                process.kill();
                process = cx.update(|cx| Process::spawn_with_config(&directory, config.as_object().context("Codex configuration is not an object")?, cx))?;
                process.initialize().await?;
                Some(profile)
            } else { None };
            let mut params = session_open_params(&directory, config, read_only, effective.as_ref().map(|response| &response["config"]))?;
            if generation_only {
                params["ephemeral"] = json!(true);
                params["baseInstructions"] = json!(meta.as_ref().and_then(|m| m.get("systemPrompt")).and_then(|prompt| prompt.as_str().or_else(|| prompt.get("append").and_then(Value::as_str))).context("Generation requires fixed system instructions")?);
                params["developerInstructions"] = json!("");
            }
            if let Some(meta) = &meta {
                if let Some(prompt) = meta.get("systemPrompt").filter(|_| !generation_only).and_then(|prompt| prompt.as_str().or_else(|| prompt.get("append").and_then(Value::as_str))) { params["developerInstructions"] = json!(prompt); }
                if let Some(model) = meta.get("modelId").and_then(Value::as_str) { params["model"] = json!(model); }
            }
            let method = if let Some(id) = &resume {
                params["threadId"] = json!(id.0); params["excludeTurns"] = json!(true);
                if let Some(model) = self.desired_models.borrow().get(id) { params["model"] = json!(model); }
                "thread/resume"
            } else { "thread/start" };
            let response = process.request(method, params).await?;
            if let Some(profile) = &generation_profile {
                let model = response["model"].as_str().context("Codex did not identify its generation model")?;
                if !profile.models.iter().any(|verified| verified == model) {
                    bail!("Codex generation model {model} has no verified tool-free metadata. No fallback was attempted.");
                }
            }
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
            let capacity_retry = Rc::new(RefCell::new(None));
            let pump_capacity_retry = capacity_retry.clone();
            let pump = cx.spawn(async move |cx| {
                let mut translator = translate::Translator::default();
                while let Some(message) = incoming.next().await {
                    if generation_only && (message.get("id").is_some() || is_generation_tool_event(&message)) {
                        pump_state.borrow_mut().finish(Err(anyhow!("Codex requested a tool or approval during text generation")));
                        if let Some(process) = weak_process.upgrade() { process.kill(); }
                        break;
                    }
                    if message.get("id").is_some() {
                        if let Some(process) = weak_process.upgrade() { handle_approval(message, &pump_id, weak_thread.clone(), process, read_only, cx); }
                        continue;
                    }
                    let method = message["method"].as_str().unwrap_or("");
                    if method == "sawe/disconnected" { break; }
                    let params = &message["params"];
                    if !belongs_to_thread(params, &pump_id) {continue;}
                    if method == "turn/completed" && pump_state.borrow().turn_id.as_deref() != params["turn"]["id"].as_str() {
                        continue;
                    }
                    if method == "turn/started" {
                        if let Some(id) = params["turn"]["id"].as_str() {
                            let recovering = {
                                let state = pump_state.borrow();
                                state.capacity_retry_count > 0 && !state.capacity_retry_started
                            };
                            let autonomous = pump_state.borrow_mut().started(id.to_owned());
                            if autonomous || recovering {
                                weak_thread.update(cx, |thread, cx| {
                                    thread.flush_end_of_turn_tail(cx);
                                    thread.push_system_note(acp_thread::SystemNoteLevel::Info,
                                        if recovering { "Codex resumed after temporary overload." }
                                        else { "Codex started a turn on its own (for example a goal continuation)." }, cx);
                                    cx.emit(acp_thread::AcpThreadEvent::ExternalTurnStarted);
                                }).log_err();
                            }
                        }
                    }
                    for update in translator.translate(method, params) { weak_thread.update(cx, |thread, cx| thread.handle_session_update(update, cx).log_err()).log_err(); }
                    if method == "turn/completed" {
                        let delay = pump_state.borrow_mut().capacity_retry_delay(&params["turn"]);
                        if let Some(delay) = delay {
                            let (attempt, generation) = {
                                let state = pump_state.borrow();
                                (state.capacity_retry_count, state.generation)
                            };
                            weak_thread.update(cx, |thread, cx| {
                                thread.flush_end_of_turn_tail(cx);
                                thread.push_system_note(acp_thread::SystemNoteLevel::Info,
                                    format!("Codex is temporarily overloaded. Retrying in {}s ({attempt}/3).", delay.as_secs()), cx);
                            }).log_err();
                            let task_state = pump_state.clone();
                            let task_process = weak_process.clone();
                            let task_thread = weak_thread.clone();
                            let task_id = pump_id.clone();
                            *pump_capacity_retry.borrow_mut() = Some(cx.spawn(async move |cx| {
                                retry_after_capacity(task_state, task_process, task_thread,
                                    task_id, generation, delay, cx).await;
                            }));
                            translator = translate::Translator::default();
                            continue;
                        }
                        if let Some(id) = params["turn"]["id"].as_str() {
                            let orphan = pump_state.borrow_mut().completed(id, translate::turn_result(&params["turn"]));
                            if let Some(result) = orphan {
                                weak_thread.update(cx, |thread, cx| finish_external_turn(thread, result, cx)).log_err();
                            }
                        }
                        translator = translate::Translator::default();
                    }
                }
                if let Some(process) = weak_process.upgrade() { process.kill(); }
            let orphaned = pump_state.borrow().sender.is_none() && pump_state.borrow().turn_id.is_some();
            pump_state.borrow_mut().disconnected = true;
            pump_state.borrow_mut().finish(Err(anyhow!("Codex process disconnected. Reopen this chat to reconnect.")));
            if orphaned {
                weak_thread.update(cx, |thread, cx| finish_external_turn(thread,
                    Err(anyhow!("Codex process disconnected. Reopen this chat to reconnect.")), cx)).log_err();
            }
            });
            self.sessions.borrow_mut().insert(id, Session {_generation_profile: generation_profile, process, state, models: available_models, thread: thread.downgrade(), _pump: pump, _capacity_retry: capacity_retry});
            Ok(thread)
        })
    }
}
impl AgentConnection for CodexConnection {
    fn supports_generation_only(&self) -> bool {
        true
    }
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
    fn resume_session_with_meta(
        self: Rc<Self>,
        id: acp::SessionId,
        project: Entity<Project>,
        paths: PathList,
        title: Option<SharedString>,
        meta: Option<acp::Meta>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        self.open(Some(id), project, paths, title, meta, cx)
    }
    fn supports_close_session(&self) -> bool {
        true
    }
    fn close_session(self: Rc<Self>, id: &acp::SessionId, _: &mut App) -> Task<Result<()>> {
        self.sessions.borrow_mut().remove(id);
        Task::ready(Ok(()))
    }

    /// Dropping the `Session` kills its app-server process (`Drop for Session`),
    /// which is why this clears the map rather than killing by hand.
    fn kill_all_sessions(&self) {
        let sessions = std::mem::take(&mut *self.sessions.borrow_mut());
        if !sessions.is_empty() {
            log::info!(
                "reaping {} live codex app-server process(es) before the editor exits",
                sessions.len()
            );
        }
        drop(sessions);
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
        if session.state.borrow().has_active_turn() {
            return Task::ready(Err(anyhow!("Codex is already responding in this chat")));
        }
        let (sender, receiver) = oneshot::channel();
        session.state.borrow_mut().sender = Some(sender);
        session.state.borrow_mut().generation += 1;
        let generation = session.state.borrow().generation;
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
        state.borrow_mut().active_effort = effort.clone();
        state.borrow_mut().retry_model = model.clone();
        cx.spawn(async move |_| {
            let response = process.request("turn/start",json!({"threadId":params.session_id.0,"input":input,"model":model,"effort":effort})).await;
            match response {
                Ok(response) => {
                    let mut state = state.borrow_mut();
                    if state.generation == generation && state.sender.is_some() {
                        if state.turn_id.is_none() && !state.capacity_retry_waiting && !state.capacity_retry_starting {
                            state.turn_id = response["turn"]["id"].as_str().map(str::to_owned);
                        }
                        state.active_model = model.or(state.active_model.take());
                    }
                }
                Err(error) => {
                    if state.borrow().generation == generation && state.borrow().sender.is_some() {
                        process.kill(); state.borrow_mut().disconnected = true; state.borrow_mut().finish(Err(error));
                    }
                }
            }
            receiver.await.context("Codex session closed during response")?
        })
    }
    fn cancel(&self, id: &acp::SessionId, cx: &mut App) {
        let sessions = self.sessions.borrow();
        let Some(session) = sessions.get(id) else {
            return;
        };
        if !session.state.borrow().has_active_turn() || session.state.borrow().cancel_requested {
            return;
        }
        if session.state.borrow().capacity_retry_waiting {
            let orphaned = session.state.borrow().sender.is_none();
            session.state.borrow_mut().finish(Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)));
            session._capacity_retry.borrow_mut().take();
            if orphaned {
                session.thread.update(cx, |thread, cx| finish_external_turn(thread,
                    Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)), cx)).log_err();
            }
            return;
        }
        session.state.borrow_mut().cancel_requested = true;
        let generation = session.state.borrow().generation;
        let state = session.state.clone();
        let process = session.process.clone();
        let weak_thread = session.thread.clone();
        let id = id.clone();
        cx.spawn(async move |cx| {
            // A stop can arrive before turn/start responds; wait briefly for its id.
            for _ in 0..100 {
                if state.borrow().generation != generation {
                    return;
                }
                if state.borrow().turn_id.is_some() || !state.borrow().has_active_turn() {
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
                let orphaned = state.borrow().sender.is_none();
                state.borrow_mut().disconnected = true;
                state
                    .borrow_mut()
                    .finish(Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)));
                if orphaned {
                    weak_thread.update(cx, |thread, cx| finish_external_turn(thread,
                        Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)), cx)).log_err();
                }
            }
        })
        .detach();
    }
    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}
const CAPACITY_CONTINUATION: &str = "Sawe runtime recovery notice, not a human request or new authorization. The previous turn stopped because the selected model was temporarily overloaded. Continue the same unfinished authorized work in this conversation. First reconcile completed steps and tool results; do not repeat actions that already succeeded. If the work is complete or needs human input, report that instead of starting new work.";

async fn retry_after_capacity(
    state: Rc<RefCell<TurnState>>, process: std::rc::Weak<Process>,
    thread: gpui::WeakEntity<AcpThread>, id: acp::SessionId,
    generation: u64, delay: Duration, cx: &mut AsyncApp,
) {
    cx.background_executor().timer(delay).await;
    let params = {
        let mut state = state.borrow_mut();
        if state.generation != generation || state.disconnected || state.cancel_requested
            || !state.capacity_retry_waiting { return; }
        state.capacity_retry_waiting = false;
        state.capacity_retry_starting = true;
        json!({"threadId":id.0,"model":state.retry_model.as_ref().or(state.active_model.as_ref()),"effort":state.active_effort,
            "input":[{"type":"text","text":CAPACITY_CONTINUATION,"text_elements":[]}]})
    };
    let Some(process) = process.upgrade() else { return; };
    let result = process.request("turn/start", params).await.and_then(|response| {
        response["turn"]["id"].as_str().map(str::to_owned)
            .context("Codex retry did not return a turn id")
    });
    let mut state = state.borrow_mut();
    if state.generation != generation || !state.capacity_retry_starting { return; }
    match result {
        Ok(turn_id) => {
            state.turn_id = Some(turn_id);
            state.capacity_retry_starting = false;
        }
        Err(error) => {
            let orphaned = state.sender.is_none();
            let message = error.to_string();
            state.finish(Err(error));
            drop(state);
            if orphaned {
                thread.update(cx, |thread, cx| finish_external_turn(thread,
                    Err(anyhow!(message)), cx)).log_err();
            }
        }
    }
}

fn finish_external_turn(thread: &mut AcpThread, result: Result<acp::PromptResponse>, cx: &mut gpui::Context<AcpThread>) {
    thread.flush_end_of_turn_tail(cx);
    match result {
        Ok(response) => cx.emit(acp_thread::AcpThreadEvent::Stopped(response.stop_reason)),
        Err(error) => {
            thread.push_system_note(acp_thread::SystemNoteLevel::Error, error.to_string(), cx);
            cx.emit(acp_thread::AcpThreadEvent::Error);
        }
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
// Shared by start and resume. Read-only construction requires the resolved
// configuration so inherited MCP cannot accidentally survive the launch path.
fn generation_config() -> serde_json::Map<String, Value> {
    let mut config = serde_json::Map::new();
    for feature in [
        "shell_tool",
        "unified_exec",
        "view_image",
        "goals",
        "hooks",
        "apps",
        "plugins",
        "multi_agent",
        "multi_agent_v2",
        "image_generation",
        "code_mode",
        "code_mode_only",
        "code_mode_host",
        "browser_use",
        "computer_use",
        "deferred_executor",
        "request_permissions_tool",
        "current_time_reminder",
        "sleep_tool",
        "token_budget",
        "send_message_to_user_async",
        "tool_suggest",
        "memories",
        "daemon_auto_start",
    ] {
        config.insert(format!("features.{feature}"), json!(false));
    }
    for key in [
        "tools.experimental_request_user_input.enabled",
        "tools.update_plan.enabled",
        "skills.include_instructions",
        "cloud.skills.enabled",
        "include_apps_instructions",
        "include_collaboration_mode_instructions",
        "include_environment_context",
        "include_permissions_instructions",
        "agents.enabled",
    ] {
        config.insert(key.into(), json!(false));
    }
    config.insert("features.skip_host_skill_discovery".into(), json!(true));
    config.insert("project_doc_max_bytes".into(), json!(0));
    config.insert("web_search".into(), json!("disabled"));
    config.insert("notify".into(), json!([]));
    config.insert("instructions".into(), json!(""));
    config.insert("developer_instructions".into(), json!(""));
    config
}

struct GenerationCatalog {
    directory: tempfile::TempDir,
    models: Vec<String>,
}

fn generation_catalog(root: &std::path::Path, resolved: &Value) -> Result<GenerationCatalog> {
    let source = resolved["model_catalog_json"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
                .map(|home| home.join("models_cache.json"))
        })
        .context("Cannot locate Codex model metadata")?;
    let mut catalog: Value =
        serde_json::from_slice(&std::fs::read(&source).with_context(|| {
            format!("Cannot read Codex model metadata at {}", source.display())
        })?)?;
    let models = catalog["models"]
        .as_array_mut()
        .context("Codex model metadata has no model list")?;
    if models.is_empty() {
        bail!("Codex model metadata is empty; run the installed Codex CLI to refresh it");
    }
    // Tools can also come from model metadata and travel in additional_tools,
    // even with all CLI tool features disabled. Retain model identity/capacity.
    let mut verified_models = Vec::new();
    for model in models {
        verified_models.push(
            model["slug"]
                .as_str()
                .context("Codex metadata is missing a model id")?
                .to_owned(),
        );
        let model = model
            .as_object_mut()
            .context("Invalid Codex model metadata")?;
        model.insert("tool_mode".into(), json!("direct"));
        model.insert("shell_type".into(), json!("disabled"));
        model.insert("apply_patch_tool_type".into(), Value::Null);
        model.insert("experimental_supported_tools".into(), json!([]));
        model.insert("supports_search_tool".into(), json!(false));
        model.insert("multi_agent_version".into(), Value::Null);
    }
    let scratch = root.join(".tmp").join("ai-generation");
    std::fs::create_dir_all(&scratch)?;
    let profile = tempfile::Builder::new()
        .prefix("codex-")
        .tempdir_in(scratch)?;
    std::fs::write(
        profile.path().join("models.json"),
        serde_json::to_vec(&catalog)?,
    )?;
    Ok(GenerationCatalog {
        directory: profile,
        models: verified_models,
    })
}

fn is_generation_tool_event(message: &Value) -> bool {
    let method = message["method"].as_str().unwrap_or_default();
    method.starts_with("item/")
        && matches!(
            message["params"]["item"]["type"].as_str(),
            Some(
                "commandExecution"
                    | "fileChange"
                    | "mcpToolCall"
                    | "webSearch"
                    | "imageGeneration"
                    | "collabAgentToolCall"
                    | "dynamicToolCall"
            )
        )
}

fn session_open_params(
    directory: &std::path::Path,
    mut config: Value,
    read_only: bool,
    effective: Option<&Value>,
) -> Result<Value> {
    if read_only {
        let effective = effective
            .filter(|value| value.is_object())
            .context("Codex did not return the resolved configuration for read-only mode")?;
        disable_mcp_servers(&mut config, effective);
    }
    Ok(
        json!({"cwd":directory,"config":config,"approvalPolicy":"never","approvalsReviewer":"user","sandbox":if read_only {"read-only"} else {"danger-full-access"}}),
    )
}

/// Spell one name as a segment of a dotted configuration-override path. A name
/// that is a valid bare key is emitted as-is; anything else — a dot, a space,
/// `@`, a quote — MUST be quoted, or the override lands on a different path
/// (`mcp_servers.my.server.enabled` disables a phantom `server` under `my`) and
/// the server it was meant to disable stays ENABLED in a read-only session.
/// The inherited names come from the user's own Codex configuration, so they
/// are arbitrary; only the editor-injected ones are known-safe.
fn override_key_segment(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        name.to_owned()
    } else {
        serde_json::to_string(name).expect("string serialization")
    }
}

// Empty tables merge with inherited configuration. Explicitly disable both
// inherited servers and editor-injected servers so read-only cannot call MCP.
fn disable_mcp_servers(config: &mut Value, effective: &Value) {
    let inherited = effective["mcp_servers"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.keys())
        .cloned();
    let injected: Vec<String> = config
        .as_object()
        .into_iter()
        .flat_map(|m| m.keys())
        .filter_map(|key| key.strip_prefix("mcp_servers.").map(str::to_owned))
        .collect();
    for name in inherited.chain(injected) {
        let segment = override_key_segment(&name);
        config[format!("mcp_servers.{segment}.enabled")] = json!(false);
    }
    // Plugins may contribute additional MCP servers not in mcp_servers.
    for name in effective["plugins"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.keys())
    {
        let segment = override_key_segment(name);
        config[format!("plugins.{segment}.enabled")] = json!(false);
    }
    config["features.apps"] = json!(false);
}
fn session_config(servers: &[acp::McpServer]) -> Value {
    let mut config = serde_json::Map::new();
    // Request the large window for editor-owned threads. Codex clamps this
    // to the selected model's maximum, including after a model switch, and
    // reports its effective usable capacity through tokenUsage updates.
    // This override applies to thread/start and thread/resume only; it does
    // not modify the user's global Codex configuration.
    config.insert("model_context_window".into(), json!(872_000));
    for server in servers {
        match server {
            acp::McpServer::Stdio(server) => {
                let env: serde_json::Map<String, Value> = server
                    .env
                    .iter()
                    .map(|entry| (entry.name.clone(), json!(entry.value)))
                    .collect();
                let mut entry = json!({"command":server.command,"args":server.args,"env":env,"default_tools_approval_mode":"approve"});
                // Full-access sessions approve editor MCP tools. Explicit peer
                // entries preserve that policy for collaboration; host checks
                // still enforce Solution scope and human-input boundaries.
                // Read-only sessions disable MCP separately.
                if server.name == "sawe"
                    && server.args.first().is_some_and(|arg| arg == "--nc")
                    && server.args.len() == 2
                {
                    entry["tools"] = json!({
                        "solution_agent.list_sessions": {"approval_mode":"approve"},
                        "solution_agent.get_session": {"approval_mode":"approve"},
                        "solution_agent.get_session_entry": {"approval_mode":"approve"},
                        "solution_agent.get_session_changes": {"approval_mode":"approve"},
                        "solution_agent.get_session_children": {"approval_mode":"approve"},
                        "solution_agent.read_session_history": {"approval_mode":"approve"},
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
                    json!({"url":server.url,"http_headers":headers,"default_tools_approval_mode":"approve"}),
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
    read_only: bool,
    cx: &mut AsyncApp,
) {
    let permitted_thread = belongs_to_thread(&message["params"], session_id);
    let outgoing = process.outgoing.clone();
    drop(process);
    cx.spawn(async move |cx| {
        let method = message["method"].as_str().unwrap_or_default(); let params = &message["params"];
        let result = if matches!(method,"item/commandExecution/requestApproval"|"item/fileChange/requestApproval") {
            if !permitted_thread || read_only {
                outgoing.unbounded_send(json!({"id":message["id"],"result":{"decision":"decline"}})).log_err();
                return;
            }
            let title = params["command"].as_str().or(params["reason"].as_str()).unwrap_or("Codex requests permission");
            let call = acp::ToolCallUpdate::new(acp::ToolCallId::new(params["itemId"].as_str().unwrap_or("approval").to_owned()), acp::ToolCallUpdateFields::new().title(title.to_owned()).raw_input(params.clone()));
            let choices = approval::ApprovalChoices::from_params(params);
            let task = thread.update(cx, |thread,cx| thread.request_tool_call_authorization(call, PermissionOptions::Flat(choices.options()), AuthorizationKind::PermissionGrant,cx));
            let decision = match task {
                Ok(Ok(task)) => {
                    let outcome: acp::RequestPermissionOutcome = task.await.into();
                    match outcome {
                        acp::RequestPermissionOutcome::Selected(selected) => choices.decision(Some(selected.option_id.0.as_ref())),
                        _ => choices.decision(None),
                    }
                },
                _ => choices.decision(None),
            };
            json!({"decision":decision})
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
    fn overloaded_turn(id: &str) -> Value {
        json!({"id":id,"status":"failed","error":{"message":"Selected model is at capacity.","codexErrorInfo":"serverOverloaded"}})
    }

    #[test]
    fn capacity_recovery_keeps_one_client_completion_and_is_bounded() {
        let (sender, mut receiver) = oneshot::channel();
        let mut state = TurnState { sender: Some(sender), generation: 7, ..Default::default() };
        for (attempt, seconds) in [10, 20, 40].into_iter().enumerate() {
            let id = format!("retry-{attempt}");
            state.capacity_retry_starting = attempt > 0;
            assert!(!state.started(id.clone()));
            assert_eq!(state.generation, 7);
            let delay = state.capacity_retry_delay(&overloaded_turn(&id)).expect("retry");
            assert!(delay >= Duration::from_secs(seconds) && delay < Duration::from_secs(seconds + 1));
            assert!(state.has_active_turn());
            assert!(state.turn_id.is_none());
            assert!(receiver.try_recv().unwrap().is_none(), "no early failure delivered");
        }
        state.capacity_retry_starting = true;
        state.started("exhausted".into());
        assert!(state.capacity_retry_delay(&overloaded_turn("exhausted")).is_none());
        assert!(state.completed("exhausted", Err(anyhow!("capacity exhausted"))).is_none());
        assert!(!state.has_active_turn());
        assert!(receiver.try_recv().unwrap().unwrap().is_err());
    }

    #[test]
    fn capacity_wait_can_be_cancelled_without_disconnect_or_replay() {
        let (sender, mut receiver) = oneshot::channel();
        let mut state = TurnState { sender: Some(sender), ..Default::default() };
        state.started("client".into());
        assert!(state.capacity_retry_delay(&overloaded_turn("client")).is_some());
        state.finish(Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)));
        assert_eq!(receiver.try_recv().unwrap().unwrap().unwrap().stop_reason, acp::StopReason::Cancelled);
        assert!(!state.has_active_turn());
        assert!(!state.disconnected);
        assert!(!state.capacity_retry_waiting);
    }

    #[test]
    fn autonomous_capacity_retry_preserves_generation_and_final_event() {
        let mut state = TurnState::default();
        assert!(state.started("goal".into()));
        let generation = state.generation;
        assert!(state.capacity_retry_delay(&overloaded_turn("goal")).is_some());
        state.capacity_retry_waiting = false;
        state.capacity_retry_starting = true;
        assert!(!state.started("goal-retry".into()));
        assert_eq!(state.generation, generation);
        assert!(state.completed("goal-retry", Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))).unwrap().is_ok());
        assert!(!state.has_active_turn());
    }

    #[test]
    fn only_terminal_overload_is_retried() {
        assert!(translate::is_capacity_failure(&overloaded_turn("t")));
        for code in ["usageLimitExceeded", "unauthorized", "contextWindowExceeded", "other"] {
            assert!(!translate::is_capacity_failure(&json!({"status":"failed","error":{"codexErrorInfo":code}})));
        }
        assert!(!translate::is_capacity_failure(&json!({"status":"inProgress","error":{"codexErrorInfo":"serverOverloaded"},"willRetry":true})));
        let mut state = TurnState::default();
        state.started("current".into());
        assert!(state.capacity_retry_delay(&overloaded_turn("old")).is_none());
        state.cancel_requested = true;
        assert!(state.capacity_retry_delay(&overloaded_turn("current")).is_none());
    }

    #[test]
    fn client_completion_then_autonomous_goal_has_independent_lifecycle() {
        smol::block_on(async {
            let (sender, receiver) = oneshot::channel();
            let mut state = TurnState { sender: Some(sender), generation: 1, ..Default::default() };
            assert!(!state.started("client".into()));
            assert!(state.completed("client", Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))).is_none());
            assert_eq!(receiver.await.unwrap().unwrap().stop_reason, acp::StopReason::EndTurn);
            assert!(!state.has_active_turn());
            assert!(state.started("goal".into()));
            assert!(state.has_active_turn(), "a goal is active without a client sender");
            assert_eq!(state.generation, 2);
            assert!(!state.started("goal".into()), "duplicate start is not a new goal");
            assert_eq!(state.generation, 2);
            assert!(state.completed("client", Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))).is_none());
            assert!(state.has_active_turn(), "late client completion cannot finish the goal");
            let response = state.completed("goal", Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))).unwrap().unwrap();
            assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
            assert!(!state.has_active_turn());
            assert!(state.completed("goal", Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))).is_none(), "duplicate completion emits no extra stop");
        });
    }

    #[test]
    fn autonomous_failure_and_interruption_are_terminal_not_silent() {
        let mut state = TurnState::default();
        state.started("failure".into());
        let error = state.completed("failure", Err(anyhow!("provider unavailable"))).unwrap().unwrap_err();
        assert!(error.to_string().contains("provider unavailable"));
        assert!(!state.has_active_turn());
        state.started("cancel".into());
        state.cancel_requested = true;
        let result = state.completed("cancel", Ok(acp::PromptResponse::new(acp::StopReason::Cancelled))).unwrap().unwrap();
        assert_eq!(result.stop_reason, acp::StopReason::Cancelled);
        assert!(!state.cancel_requested);
        assert!(!state.has_active_turn());
    }
    #[test]
    fn forwards_stdio_and_http_mcp_configuration() {
        let config = session_config(&[
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
        assert_eq!(config["model_context_window"], 872_000);
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
            7
        );
        assert!(config["mcp_servers.remote"].get("tools").is_none());
        let external = session_config(&[acp::McpServer::Stdio(acp::McpServerStdio::new(
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
    fn session_launch_requires_resolved_config_and_disables_mcp_in_read_only() {
        let directory = std::path::Path::new("/tmp/project");
        assert!(session_open_params(directory, session_config(&[]), true, None).is_err());
        assert!(
            session_open_params(directory, session_config(&[]), true, Some(&Value::Null)).is_err()
        );
        let params = session_open_params(
            directory,
            session_config(&[]),
            true,
            Some(&json!({"mcp_servers":{"global":{"command":"unsafe"}}})),
        )
        .unwrap();
        assert_eq!(params["sandbox"], "read-only");
        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["config"]["mcp_servers.global.enabled"], false);
        let full = session_open_params(directory, session_config(&[]), false, None).unwrap();
        assert_eq!(full["sandbox"], "danger-full-access");
        assert_eq!(full["approvalPolicy"], "never");
        assert!(full["config"].get("features.apps").is_none());
    }

    #[test]
    fn read_only_disables_inherited_and_editor_mcp_without_changing_context_window() {
        let mut config = session_config(&[acp::McpServer::Stdio(acp::McpServerStdio::new(
            "sawe", "/sawe",
        ))]);
        disable_mcp_servers(
            &mut config,
            &json!({"mcp_servers":{"global":{"url":"https://example.com"},"sawe":{"command":"old"}}}),
        );
        assert_eq!(config["mcp_servers.global.enabled"], false);
        assert_eq!(config["mcp_servers.sawe.enabled"], false);
        assert_eq!(config["model_context_window"], 872_000);
        disable_mcp_servers(
            &mut config,
            &json!({"plugins":{"sample@test":{"enabled":true}}}),
        );
        assert_eq!(config["plugins.\"sample@test\".enabled"], false);
        assert_eq!(config["features.apps"], false);
        let mut empty = session_config(&[]);
        disable_mcp_servers(&mut empty, &json!({}));
        assert_eq!(empty["features.apps"], false);
        // A name the user is free to choose but that is NOT a bare key: an
        // unquoted `mcp_servers.my.server.enabled` would disable a phantom
        // `server` table and leave the real one callable from a read-only
        // session.
        let mut exotic = session_config(&[]);
        disable_mcp_servers(
            &mut exotic,
            &json!({"mcp_servers":{"my.server":{"command":"x"},"team mcp":{"command":"y"},"ok-1_A":{"command":"z"}}}),
        );
        assert_eq!(exotic["mcp_servers.\"my.server\".enabled"], false);
        assert_eq!(exotic["mcp_servers.\"team mcp\".enabled"], false);
        assert_eq!(exotic["mcp_servers.ok-1_A.enabled"], false);
        assert!(exotic.get("mcp_servers.my.server.enabled").is_none());
    }
    #[test]
    fn child_and_unscoped_events_cannot_complete_parent_turn() {
        let id = acp::SessionId::new("parent");
        assert!(belongs_to_thread(&json!({"threadId":"parent"}), &id));
        assert!(!belongs_to_thread(&json!({"threadId":"child"}), &id));
        assert!(!belongs_to_thread(&json!({}), &id));
    }
}
