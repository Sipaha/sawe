//! Connection + `AgentServer` implementations for the native claude
//! stream-json backend.
//!
//! `ClaudeNativeAgentServer` implements `agent_servers::AgentServer`; its
//! `connect` hands back a `ClaudeNativeConnection` (an `acp_thread::Agent
//! Connection`). The connection owns one `claude` subprocess per session and a
//! per-session update-pump task that drains the process's `incoming` stream,
//! translates each message into `acp::SessionUpdate`s the `AcpThread` consumes,
//! and resolves the in-flight prompt's oneshot on the turn-ending `result`
//! message (the deterministic turn-end that fixes the Running-hang).

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use acp_thread::{AcpThread, AcpThreadEvent, AgentConnection, UserMessageId};
use action_log::ActionLog;
use agent_client_protocol::schema as acp;
use agent_servers::{AgentServer, AgentServerDelegate, mcp_servers_for_project};
use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use futures::{FutureExt as _, StreamExt as _, select_biased};
use gpui::{App, AppContext as _, Entity, SharedString, Task, WeakEntity};
use project::{AgentId, Project};
use scheduler::Instant;
use ui::IconName;
use util::ResultExt as _;
use util::path_list::PathList;

use crate::command::{ClaudeCommandSpec, SessionArg, mcp_config_json};
use crate::process::ClaudeProcess;
use crate::protocol::{
    ControlRequestEnvelope, ControlRequestKind, ControlRequestOut, HookConfig, InputMessage,
    ModelInfo, OutputMessage,
};
use crate::translate::{
    DEFAULT_CONTEXT_WINDOW, TurnEnd, apply_stream_usage, apply_usage, assistant_usage_update,
    classify_result, image_block_from_anthropic, infer_context_window_from_model,
    stamp_subagent_meta, translate,
};
use crate::watchdog::{AnalyzerContext, ClaudeAnalyzer, Watchdog};

/// Store-provided pull invoked at each hook to fetch the next queued follow-up
/// for `session_id`. `agent_id` is `Some` when the firing hook belongs to an
/// Agent Teams subagent (its hook input carries `agent_id`/`agent_type`) and
/// `None` for the main agent — the store uses it to route a queued message to
/// the tab it was typed into. `is_end_of_turn` is true for the `Stop` hook
/// (the agent has produced a complete message). Returns the formatted
/// agent-facing text, or `None` when nothing is queued for that addressee.
/// Type-erased so `claude_native` needs no dependency on `solution_agent`.
pub type HookPull = std::rc::Rc<
    dyn Fn(&acp::SessionId, Option<&str>, bool, &mut gpui::AsyncApp) -> Option<String>,
>;

/// Stable id for the `PostToolUse` hook callback registered in `initialize`.
const HOOK_CALLBACK_POST_TOOL_USE: &str = "pti";
/// Stable id for the `Stop` hook callback registered in `initialize`. When a
/// follow-up is pending and `Stop` fires (no tool ran), we respond with
/// `decision: "block"` so the agent keeps generating to address it.
const HOOK_CALLBACK_STOP: &str = "stop_inj";

/// Max times within a single turn we'll nudge the agent after it emitted a
/// tool call as literal `<invoke …>` text (a known opus degradation — it
/// writes the function-calling XML as prose, ends the turn, and nothing
/// runs). Bounded so a model that keeps doing it can't wedge the turn in an
/// infinite Stop→nudge loop; after this many, we let the turn end (Idle) as
/// before.
const MAX_DEGENERATE_NUDGES: u8 = 2;

/// Injected via the Stop hook (with `decision: "block"`) when the agent wrote
/// a tool call as text and tried to stop — so it retries as a real tool call
/// in the same turn instead of halting the whole run on one bad message.
const DEGENERATE_TOOL_CALL_NUDGE: &str = "Your last message wrote a tool call as literal text (an `<invoke name=…>…</invoke>` block) instead of actually invoking the tool, so nothing ran. Re-issue it now as a real tool call.";

/// True when an assistant message body looks like a tool call written as
/// literal text — the `<invoke name=…>…</invoke>` function-calling format the
/// model is supposed to emit as a structured `tool_use` block, not prose.
/// Requires both the opening `invoke name=` and a closing `invoke>` so a
/// passing mention of the word "invoke" doesn't trigger a spurious nudge.
fn looks_like_text_tool_call(text: &str) -> bool {
    text.contains("invoke name=") && text.contains("invoke>")
}

/// Decide what (if any) text to append from a final `Assistant` message's text
/// block, given the text already streamed for that message. The final block is
/// the AUTHORITATIVE complete text; the streamed deltas are an in-progress
/// prefix that can be cut short. Returns the slice of `final_text` to emit as a
/// new `AgentMessageChunk` (appends, so it must never re-emit streamed bytes):
///
/// * not streamed → the whole block (recovery for the non-streaming path);
/// * streamed a proper prefix shorter than the final → the MISSING SUFFIX
///   (completes a truncated/dropped stream — the "reply stays cut off" bug);
/// * streamed the whole thing (or anything that isn't a shorter prefix) →
///   `None` (genuine duplicate, or a mismatch we don't risk double-appending).
fn final_text_block_suffix<'a>(
    streamed: bool,
    streamed_text: &str,
    final_text: &'a str,
) -> Option<&'a str> {
    if !streamed {
        Some(final_text)
    } else if final_text.len() > streamed_text.len() && final_text.starts_with(streamed_text) {
        Some(&final_text[streamed_text.len()..])
    } else {
        None
    }
}

/// Default grace period after a soft `interrupt` before the Stop escalates to
/// a hard kill + `--resume` respawn. Overridable for tests via
/// [`ClaudeNativeConnection::set_escalation_timeout_for_test`].
const DEFAULT_ESCALATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Default quiet period a turn may go without any output before the silence
/// watchdog asks the analyzer whether `claude` is hung. Overridable for tests
/// via [`ClaudeNativeConnection::set_silence_window_for_test`].
const DEFAULT_SILENCE_WINDOW: Duration = Duration::from_secs(15 * 60);

/// `AgentServer` that spawns the `claude` binary directly (no node wrapper).
pub struct ClaudeNativeAgentServer {
    agent_id: AgentId,
    binary: PathBuf,
    extra_env: Vec<(String, String)>,
}

impl ClaudeNativeAgentServer {
    pub fn new(agent_id: AgentId) -> Self {
        Self {
            agent_id,
            binary: PathBuf::from("claude"),
            extra_env: Vec::new(),
        }
    }

    /// Construct a server bound to a specific `claude` binary (an integration
    /// test points this at the mock script) plus extra environment variables.
    pub fn with_binary(
        agent_id: AgentId,
        binary: PathBuf,
        extra_env: Vec<(String, String)>,
    ) -> Self {
        Self {
            agent_id,
            binary,
            extra_env,
        }
    }

    /// Spawn a throwaway `claude`, run the `initialize` handshake to read its
    /// advertised model list, then kill it. Used to refresh the model list for
    /// a COLD session (no live process, no project) WITHOUT waking it. Returns
    /// an empty vec on any failure/timeout.
    pub fn probe_models(
        &self,
        work_dir: PathBuf,
        cx: &gpui::AsyncApp,
    ) -> Task<Result<Vec<ModelInfo>>> {
        let binary = self.binary.clone();
        let extra_env = self.extra_env.clone();
        cx.spawn(async move |cx| {
            let spec = ClaudeCommandSpec {
                binary,
                work_dir,
                session: SessionArg::New(uuid::Uuid::new_v4().to_string()),
                mcp_servers_json: "{\"mcpServers\":{}}".to_string(),
                append_system_prompt: None,
                extra_env,
                model: None,
            };
            let mut process = cx.update(|cx| ClaudeProcess::spawn(spec, cx))?;
            let receiver = process.send_control(ControlRequestOut::Initialize {
                hooks: build_default_hooks(),
            })?;
            // The real `claude` only emits the initialize response after the first
            // stdin message. `/context` is a LOCAL slash command (no model API
            // call), enough to flush the response. PHASE-6 VERIFICATION: if the
            // response never carries `models` after `/context`, change this to a
            // tiny real prompt (e.g. `"hi"`).
            const PROBE_FLUSH_MESSAGE: &str = "/context";
            process
                .outgoing
                .unbounded_send(InputMessage::user_text(PROBE_FLUSH_MESSAGE))?;

            // Race the response against a timeout so a wedged probe can't hang.
            // The receiver is a `oneshot::Receiver` awaited directly on the
            // foreground executor (this `cx.spawn` body already runs there),
            // mirroring `dispatch_initialize`'s `receiver.await`.
            let timeout = cx
                .background_executor()
                .timer(std::time::Duration::from_secs(20));
            let payload = select_biased! {
                res = receiver.fuse() => res.ok(),
                _ = timeout.fuse() => None,
            };
            process.kill().log_err();
            Ok(payload
                .map(|p| parse_available_models(&p))
                .unwrap_or_default())
        })
    }
}

impl AgentServer for ClaudeNativeAgentServer {
    fn logo(&self) -> IconName {
        IconName::AiClaude
    }

    fn agent_id(&self) -> AgentId {
        self.agent_id.clone()
    }

    fn connect(
        &self,
        _delegate: AgentServerDelegate,
        _project: Entity<Project>,
        _cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let connection = Rc::new(ClaudeNativeConnection {
            agent_id: self.agent_id.clone(),
            binary: self.binary.clone(),
            extra_env: self.extra_env.clone(),
            sessions: RefCell::new(HashMap::new()),
            desired_models: RefCell::new(HashMap::new()),
            desired_efforts: RefCell::new(HashMap::new()),
            escalation_timeout: Cell::new(DEFAULT_ESCALATION_TIMEOUT),
            silence_window: Cell::new(DEFAULT_SILENCE_WINDOW),
            self_handle: RefCell::new(std::rc::Weak::new()),
            escalations_armed: Cell::new(0),
            store_pull: std::rc::Rc::new(std::cell::RefCell::new(None)),
        });
        *connection.self_handle.borrow_mut() = Rc::downgrade(&connection);
        Task::ready(Ok(connection as Rc<dyn AgentConnection>))
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

/// State shared between a session's update-pump and its `prompt`/exit handlers.
/// `prompt_tx` carries the in-flight turn's resolver; the pump fulfils it on the
/// turn-ending `result`, the exit-handler fulfils it with an error on process
/// death. `sticky_window` retains the last advertised context window so the
/// token meter never regresses (the 200k/1M flicker fix).
struct SessionShared {
    prompt_tx: RefCell<Option<oneshot::Sender<Result<TurnEnd>>>>,
    sticky_window: Cell<Option<u64>>,
    /// Wall time (executor clock) of the last message the pump pulled off
    /// `incoming`. The silence watchdog reads this to know how long the turn
    /// has been quiet; the pump bumps it on every message (deltas AND control
    /// requests) so any progress resets the silence timer. An `Rc` so the
    /// watchdog timer task shares the very same cell the pump bumps.
    last_output: Rc<Cell<Instant>>,
    /// Set by `cancel` when a soft interrupt is sent. The real `claude` does NOT
    /// emit a clean `result(cancelled)` on interrupt — mid-tool it emits
    /// `result(subtype="error_during_execution", is_error=true)`, which
    /// `classify_result` would otherwise turn into an `Errored` turn. We can't
    /// infer "cancelled" from claude's encoding, so we record that *we* asked:
    /// the pump resolves the next `result` as `Cancelled` when this is set.
    cancel_requested: Cell<bool>,
    /// User message accumulated while a turn is in flight; consumed by the
    /// next `hook_callback` (PostToolUse or Stop) and injected as
    /// `additionalContext`. `Some` while a follow-up is pending; cleared the
    /// moment the next hook fires.
    pending_inject: RefCell<Option<String>>,
    /// Cumulative usage snapshot for the in-flight assistant turn, built up
    /// from `message_start` + `message_delta` stream events. Anthropic
    /// reports `message_delta.usage` *cumulatively* (each delta supersedes
    /// the prior), so the meter has to merge into the running snapshot
    /// rather than sum. Cleared on `result`. Mirrors JS's
    /// `lastAssistantUsage` (`acp-agent.js:427`,`705–741`).
    stream_usage: RefCell<Option<crate::protocol::Usage>>,
    /// Last emitted meter `used_tokens` for the in-flight turn — only emit a
    /// fresh `UsageUpdate` when the new total changes. JS does the same to
    /// avoid spamming the client with repeats (`acp-agent.js:739`).
    stream_used_total: Cell<Option<u64>>,
    /// The most recently observed `message.model` for this session, latched
    /// from `message_start` stream events. Used as a fallback for the
    /// context-window limit when no `result.modelUsage.contextWindow` has
    /// been seen yet (`inferContextWindowFromModel` in JS:`acp-agent.js:716`).
    /// Reset on session restart but persists across turns within a session.
    active_model: RefCell<Option<String>>,
    /// Models advertised by `claude` in the `initialize` control-response
    /// (only arrives after the first turn — see `dispatch_initialize`).
    /// Read by the store to refresh the session's persisted cache.
    available_models: RefCell<Vec<ModelInfo>>,
    /// The session's own id, so the hook arm can pass it to the store pull.
    session_id: acp::SessionId,
    /// Shared cell holding the store's follow-up pull (see `ClaudeNativeConnection::store_pull`).
    /// A clone of the connection's `Rc`, so a `set_store_pull` AFTER this session
    /// was created is still visible here.
    pending_pull: std::rc::Rc<std::cell::RefCell<Option<HookPull>>>,
}

/// Everything needed to respawn a session's `claude` process under the same
/// session id (Stop-escalation kill+resume, and — in Phase 7.2 — the watchdog's
/// `Hung` recovery). Kept separate from the live process so a respawn can build
/// a fresh `ClaudeCommandSpec` without re-deriving it from scratch.
#[derive(Clone)]
struct RespawnBlueprint {
    project: Entity<Project>,
    work_dirs: PathList,
    append_system_prompt: Option<String>,
    model: Option<String>,
}

struct SessionState {
    process: ClaudeProcess,
    thread: WeakEntity<AcpThread>,
    shared: Rc<SessionShared>,
    blueprint: RespawnBlueprint,
    /// The update-pump task. Stored so dropping the session cancels it.
    _update_pump: Task<()>,
    /// The Stop-escalation task armed by `cancel`. Held so a clean
    /// `result(cancelled)` (which resolves the prompt oneshot) can drop it and
    /// thereby cancel the pending kill+resume. `None` when no Stop is in flight.
    escalation: Option<Task<()>>,
    /// The silence watchdog for the in-flight turn. Armed when a prompt starts,
    /// dropped (which cancels its timer) when the prompt resolves. `None` while
    /// the session is idle.
    watchdog: Option<Watchdog>,
}

/// Per-process connection to one or more `claude` subprocesses (one per
/// session). Implements `acp_thread::AgentConnection`.
pub struct ClaudeNativeConnection {
    agent_id: AgentId,
    binary: PathBuf,
    extra_env: Vec<(String, String)>,
    sessions: RefCell<HashMap<acp::SessionId, SessionState>>,
    /// Model a session should (re)spawn on, seeded by the store before the
    /// resume/load wake (which doesn't carry session meta). Consulted by
    /// `open_session` when `extra_meta` has no `modelId`.
    desired_models: RefCell<HashMap<acp::SessionId, String>>,
    /// Effort level a session should (re)spawn on, seeded by the store before
    /// the wake; applied via `apply_flag_settings` right after spawn. Mirrors
    /// `desired_models`.
    desired_efforts: RefCell<HashMap<acp::SessionId, String>>,
    /// Grace period between a soft `interrupt` and the hard kill+resume
    /// escalation. A `Cell` so tests can shrink it to milliseconds.
    escalation_timeout: Cell<Duration>,
    /// Quiet period the silence watchdog waits before analyzing a turn. A `Cell`
    /// so a test can shrink it to milliseconds.
    silence_window: Cell<Duration>,
    /// A handle back to the `Rc` that owns this connection, set once right after
    /// construction. `cancel` (a `&self` method) needs an owned `Rc<Self>` to
    /// arm the escalation task that may outlive the call; upgrading this weak
    /// handle yields it without changing the trait signature.
    self_handle: RefCell<std::rc::Weak<ClaudeNativeConnection>>,
    /// Test-only tally of how many Stop-escalations `cancel` has armed. The
    /// idempotency guard means a burst of repeated cancels for one in-flight
    /// turn arms exactly one — observable without racing the respawn.
    escalations_armed: Cell<usize>,
    /// Store-provided follow-up pull, registered once by the store
    /// (`subscribe_to_session`). Shared by `Rc` into every `SessionShared` so a
    /// late registration reaches already-created sessions' pumps.
    store_pull: std::rc::Rc<std::cell::RefCell<Option<HookPull>>>,
}

/// Hook map registered in the `initialize` control_request. `PostToolUse`
/// gives us a callback at every safe tool boundary (between `tool_result` and
/// the next assistant block); `Stop` gives us a callback at end-of-turn so a
/// pending follow-up still lands even if no tool fires before the agent tries
/// to stop.
fn build_default_hooks() -> std::collections::BTreeMap<String, Vec<HookConfig>> {
    let mut hooks = std::collections::BTreeMap::new();
    hooks.insert(
        "PostToolUse".to_string(),
        vec![HookConfig {
            matcher: None,
            hook_callback_ids: vec![HOOK_CALLBACK_POST_TOOL_USE.to_string()],
            timeout: 30_000,
        }],
    );
    hooks.insert(
        "Stop".to_string(),
        vec![HookConfig {
            matcher: None,
            hook_callback_ids: vec![HOOK_CALLBACK_STOP.to_string()],
            timeout: 30_000,
        }],
    );
    hooks
}

/// Format a pending follow-up so the agent can tell apart "the user said this
/// at the start of the turn" from "the user added this mid-turn at HH:MM:SS".
fn format_inject_message(message: &str) -> String {
    let now = chrono::Local::now().format("%H:%M:%S");
    format!("[The user added a new message mid-turn at {now}]:\n<<<\n{message}\n>>>")
}

/// Build the `response` value passed to `InputMessage::ControlResponse{response: …}`
/// for an inbound `hook_callback`. When `pending` is `Some`, the agent receives
/// the formatted user message as `additionalContext` (and, for `Stop`, a
/// `decision: "block"` + `reason` so it keeps generating to address it).
/// `request_id` is duplicated inside the response payload because some Claude
/// builds key off the inner id; keeping both consistent is harmless when they
/// don't.
fn build_hook_response(
    request_id: &str,
    callback_id: &str,
    pending: Option<String>,
) -> serde_json::Value {
    let Some(message) = pending else {
        return serde_json::json!({
            "subtype": "success",
            "request_id": request_id,
            "response": {},
        });
    };

    let formatted = format_inject_message(&message);
    let is_stop = callback_id == HOOK_CALLBACK_STOP;
    let event_name = if is_stop { "Stop" } else { "PostToolUse" };

    let mut response = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event_name,
            "additionalContext": formatted,
        },
        "continue": true,
        "suppressOutput": false,
    });
    if is_stop {
        // `decision: "block"` on a Stop hook prevents the agent from ending the
        // turn — it keeps generating to address `reason`.
        if let Some(object) = response.as_object_mut() {
            object.insert("decision".to_string(), serde_json::json!("block"));
            object.insert("reason".to_string(), serde_json::json!(formatted));
        }
    }

    serde_json::json!({
        "subtype": "success",
        "request_id": request_id,
        "response": response,
    })
}

/// Slash commands the upstream `@agentclientprotocol/claude-agent-acp` shim
/// strips from the `available_commands_update` it forwards to the client —
/// they are CLI-local concerns (auth, keybindings help, etc.) that don't make
/// sense over ACP. Mirror that filter so the UI matches the JS-shim path.
const UNSUPPORTED_SLASH_COMMANDS: &[&str] = &[
    "clear",
    "cost",
    "keybindings-help",
    "login",
    "logout",
    "output-style:new",
    "release-notes",
    "todos",
];

/// Map the `commands` array of an `initialize` control_response payload to
/// `acp::AvailableCommand`s. Each element is shaped
/// `{ name: string, description?: string, argumentHint?: string|string[] }`
/// — `argumentHint` becomes an `Unstructured` input hint when present,
/// matching what the upstream `getAvailableSlashCommands` in
/// `acp-agent.js` produces. Unknown / malformed entries are silently
/// skipped (a single bad entry mustn't blank out the whole command list).
fn parse_available_commands(payload: &serde_json::Value) -> Vec<acp::AvailableCommand> {
    let Some(commands) = payload.get("commands").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    commands
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name").and_then(|v| v.as_str())?.to_string();
            if UNSUPPORTED_SLASH_COMMANDS.contains(&name.as_str()) {
                return None;
            }
            let description = entry
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mut command = acp::AvailableCommand::new(name, description);
            if let Some(hint) = entry.get("argumentHint") {
                let hint_text = match hint {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Array(arr) => Some(
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    _ => None,
                };
                if let Some(hint) = hint_text.filter(|s| !s.is_empty()) {
                    command = command.input(acp::AvailableCommandInput::Unstructured(
                        acp::UnstructuredCommandInput::new(hint),
                    ));
                }
            }
            Some(command)
        })
        .collect()
}

/// Map the `models` array of an `initialize` control_response payload to
/// [`ModelInfo`]s. Each element is shaped
/// `{ value: string, displayName?: string, description?: string }`
/// (SDK `SDKControlInitializeResponse.models`). Entries without a `value`
/// are skipped; `displayName` falls back to `value`.
fn parse_available_models(payload: &serde_json::Value) -> Vec<ModelInfo> {
    let Some(models) = payload.get("models").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|entry| {
            let value = entry.get("value").and_then(|v| v.as_str())?.to_string();
            let display_name = entry
                .get("displayName")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&value)
                .to_string();
            let description = entry
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(ModelInfo { value, display_name, description })
        })
        .collect()
}

/// Build the `apply_flag_settings` payload for one of the 6 effort options.
/// `ultracode` is its own boolean flag ("xhigh + workflows"); the other five
/// are plain `effortLevel` values.
fn effort_settings_json(value: &str) -> serde_json::Value {
    if value == "ultracode" {
        serde_json::json!({ "effortLevel": "xhigh", "ultracode": true })
    } else {
        serde_json::json!({ "effortLevel": value, "ultracode": false })
    }
}

/// Send the `initialize` control_request and spawn a detached task that
/// awaits its response, parses out the agent's slash-command list, and
/// pushes it to the `AcpThread` as an `AvailableCommandsUpdate`. Used by
/// both `open_session` (new) and `recover_session` (kill+resume) so a
/// respawned process re-advertises commands too. The real `claude` only
/// emits the response *after* the first user turn, so this MUST stay
/// detached — awaiting it inline would deadlock session creation.
fn dispatch_initialize(
    process: &ClaudeProcess,
    thread: WeakEntity<AcpThread>,
    shared: Rc<SessionShared>,
    cx: &mut gpui::AsyncApp,
) {
    let Ok(receiver) = process.send_control(ControlRequestOut::Initialize {
        hooks: build_default_hooks(),
    }) else {
        log::warn!(
            target: "claude_native::initialize",
            "stdin closed before initialize could be sent — agent commands won't be advertised"
        );
        return;
    };
    cx.spawn(async move |cx| {
        let payload = match receiver.await {
            Ok(payload) => payload,
            Err(_) => {
                log::debug!(
                    target: "claude_native::initialize",
                    "initialize response sender dropped (process gone before first turn)"
                );
                return;
            }
        };
        let models = parse_available_models(&payload);
        if !models.is_empty() {
            *shared.available_models.borrow_mut() = models;
        }
        let commands = parse_available_commands(&payload);
        log::debug!(
            target: "claude_native::initialize",
            "initialize response received: {} command(s) after filter",
            commands.len(),
        );
        if commands.is_empty() {
            return;
        }
        thread
            .update(cx, |thread, cx| {
                thread
                    .handle_session_update(
                        acp::SessionUpdate::AvailableCommandsUpdate(
                            acp::AvailableCommandsUpdate::new(commands),
                        ),
                        cx,
                    )
                    .log_err();
            })
            .ok();
    })
    .detach();
}

impl ClaudeNativeConnection {
    /// Register the store's follow-up pull. First registration wins (the store
    /// calls this on every session attach, but the closure is store-global).
    /// Shared by `Rc` with every `SessionShared`, so this is visible to sessions
    /// created before this call too.
    pub fn set_store_pull(&self, pull: HookPull) {
        let mut slot = self.store_pull.borrow_mut();
        if slot.is_none() {
            *slot = Some(pull);
        }
    }

    /// The models `claude` advertised for this session's last `initialize`
    /// response, if any. Empty until the session has run at least one turn.
    pub fn available_models(&self, session_id: &acp::SessionId) -> Vec<ModelInfo> {
        self.sessions
            .borrow()
            .get(session_id)
            .map(|s| s.shared.available_models.borrow().clone())
            .unwrap_or_default()
    }

    /// Switch a live session's model via the SDK `set_model` control request
    /// (applied by claude on the next turn). No-op (error-logged) if the
    /// session is gone or stdin is closed.
    pub fn select_model(&self, session_id: &acp::SessionId, value: String) {
        let sessions = self.sessions.borrow();
        let Some(session) = sessions.get(session_id) else {
            log::warn!(
                "claude_native: select_model for unknown session {}",
                session_id.0
            );
            return;
        };
        match session
            .process
            .send_control(ControlRequestOut::SetModel { model: value })
        {
            Ok(_receiver) => {}
            Err(error) => log::warn!("claude_native: set_model write failed: {error}"),
        }
    }

    /// Seed (or clear with `None`) the model a session should (re)spawn on.
    /// Consulted by `open_session` when the ACP meta has no `modelId` — i.e.
    /// the resume/load wake path, which doesn't thread session meta.
    pub fn set_desired_model(&self, session_id: &acp::SessionId, value: Option<String>) {
        let mut map = self.desired_models.borrow_mut();
        match value {
            Some(v) => {
                map.insert(session_id.clone(), v);
            }
            None => {
                map.remove(session_id);
            }
        }
    }

    /// Seed (or clear) the effort a session should (re)spawn on. Consulted by
    /// `open_session`/`recover_session` to apply it via `apply_flag_settings`
    /// right after spawn.
    pub fn set_desired_effort(&self, session_id: &acp::SessionId, value: Option<String>) {
        let mut map = self.desired_efforts.borrow_mut();
        match value {
            Some(v) => {
                map.insert(session_id.clone(), v);
            }
            None => {
                map.remove(session_id);
            }
        }
    }

    /// Switch a live session's effort via the `apply_flag_settings` control
    /// request (applied on the next turn).
    pub fn select_effort(&self, session_id: &acp::SessionId, value: String) {
        let sessions = self.sessions.borrow();
        let Some(session) = sessions.get(session_id) else {
            log::warn!(
                "claude_native: select_effort for unknown session {}",
                session_id.0
            );
            return;
        };
        let settings = effort_settings_json(&value);
        match session
            .process
            .send_control(ControlRequestOut::ApplyFlagSettings { settings })
        {
            Ok(_receiver) => {}
            Err(error) => log::warn!("claude_native: apply_flag_settings write failed: {error}"),
        }
    }

    /// Buffer a user-typed follow-up to be injected into the running turn at
    /// the next safe boundary (next `PostToolUse` hook firing, or the `Stop`
    /// hook if no tool fires before end-of-turn). Idempotent on repeated calls
    /// — replaces any previously-buffered, not-yet-consumed text. Caller is
    /// responsible for adding the user message to the AcpThread separately;
    /// this is purely the inject side-channel.
    pub fn inject_user_message(&self, session_id: &acp::SessionId, text: String) {
        let sessions = self.sessions.borrow();
        if let Some(session) = sessions.get(session_id) {
            *session.shared.pending_inject.borrow_mut() = Some(text);
        }
    }

    /// Test-only accessor for the per-session `pending_inject` buffer. Returns
    /// `None` for unknown sessions or for a session whose slot is currently
    /// empty (`Some(None)` is collapsed to `None` for ergonomics).
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_slot_for_test(&self, session_id: &acp::SessionId) -> Option<String> {
        self.sessions
            .borrow()
            .get(session_id)
            .and_then(|session| session.shared.pending_inject.borrow().clone())
    }

    /// Test-only: whether a store follow-up pull has been registered via
    /// [`set_store_pull`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn store_pull_registered_for_test(&self) -> bool {
        self.store_pull.borrow().is_some()
    }

    /// Test-only: invoke the registered store pull for `session_id`, if any.
    /// Lets a store test exercise the pull-closure directly without driving the
    /// live pump. Returns `None` when no pull is registered or the pull yields
    /// nothing (empty queue).
    #[cfg(any(test, feature = "test-support"))]
    pub fn invoke_store_pull_for_test(
        &self,
        session_id: &acp::SessionId,
        agent_id: Option<&str>,
        is_end_of_turn: bool,
        cx: &mut gpui::AsyncApp,
    ) -> Option<String> {
        let pull = self.store_pull.borrow().clone();
        pull.and_then(|p| p(session_id, agent_id, is_end_of_turn, cx))
    }

    /// Extract the `--append-system-prompt` text from the ACP `_meta` extension
    /// the fork uses: `{ "systemPrompt": { "append": "<text>" } }`. Absent /
    /// malformed meta yields `None` (no flag added).
    fn append_system_prompt_from_meta(extra_meta: &Option<acp::Meta>) -> Option<String> {
        extra_meta
            .as_ref()?
            .get("systemPrompt")?
            .get("append")?
            .as_str()
            .map(|text| text.to_string())
    }

    /// Extract the desired model from the ACP session meta the store passes.
    /// The store sets `meta["modelId"] = "<value>"` for a session whose user
    /// picked a model while it was cold.
    fn model_from_meta(extra_meta: &Option<acp::Meta>) -> Option<String> {
        extra_meta
            .as_ref()?
            .get("modelId")?
            .as_str()
            .map(|text| text.to_string())
    }

    /// Shrink the Stop-escalation grace period so an integration test can drive
    /// the kill+resume path without waiting the real 30 seconds.
    pub fn set_escalation_timeout_for_test(&self, timeout: Duration) {
        self.escalation_timeout.set(timeout);
    }

    /// Shrink the silence watchdog window so a test drives the analyzer path
    /// without waiting the real 15 minutes.
    pub fn set_silence_window_for_test(&self, window: Duration) {
        self.silence_window.set(window);
    }

    /// How many Stop-escalations `cancel` has armed so far. A repeated cancel
    /// for the same in-flight turn must not increment this (idempotency guard).
    pub fn escalations_armed_for_test(&self) -> usize {
        self.escalations_armed.get()
    }

    /// The OS process id backing a session, or `None` if the session is gone.
    /// A changed value across a cancel proves the process was killed+respawned.
    pub fn session_process_id_for_test(&self, session_id: &acp::SessionId) -> Option<u32> {
        self.sessions
            .borrow()
            .get(session_id)
            .map(|session| session.process.process_id())
    }

    /// Spawn a `claude` subprocess for `session`, await its `init` message to
    /// learn the real session id, build the `AcpThread`, and start the
    /// per-session update-pump. Shared by `new_session`/`resume_session`.
    fn open_session(
        self: Rc<Self>,
        session: SessionArg,
        project: Entity<Project>,
        work_dirs: PathList,
        title: Option<SharedString>,
        extra_meta: Option<acp::Meta>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let Some(work_dir) = work_dirs.ordered_paths().next().cloned() else {
            return Task::ready(Err(anyhow!("Working directory cannot be empty")));
        };
        let mcp_servers = mcp_servers_for_project(&project, cx);
        let append_system_prompt = Self::append_system_prompt_from_meta(&extra_meta);

        // `claude --input-format stream-json` does NOT emit `init` on spawn — it
        // blocks on stdin and only emits `init` (echoing this id) after the first
        // user message. So we adopt the id we pass via `--session-id`/`--resume`
        // up front; waiting for `init` here would deadlock session creation.
        let session_id = acp::SessionId::new(session.session_id().to_string());

        let model = Self::model_from_meta(&extra_meta)
            .or_else(|| self.desired_models.borrow().get(&session_id).cloned());

        let blueprint = RespawnBlueprint {
            project: project.clone(),
            work_dirs: work_dirs.clone(),
            append_system_prompt: append_system_prompt.clone(),
            model: model.clone(),
        };

        let spec = ClaudeCommandSpec {
            binary: self.binary.clone(),
            work_dir,
            session,
            mcp_servers_json: mcp_config_json(&mcp_servers),
            append_system_prompt,
            extra_env: self.extra_env.clone(),
            model,
        };

        let mut process = match ClaudeProcess::spawn(spec, cx) {
            Ok(process) => process,
            Err(error) => return Task::ready(Err(error)),
        };

        cx.spawn(async move |cx| {
            let shared = Rc::new(SessionShared {
                prompt_tx: RefCell::new(None),
                sticky_window: Cell::new(None),
                last_output: Rc::new(Cell::new(cx.background_executor().now())),
                cancel_requested: Cell::new(false),
                pending_inject: RefCell::new(None),
                stream_usage: RefCell::new(None),
                stream_used_total: Cell::new(None),
                active_model: RefCell::new(None),
                available_models: RefCell::new(Vec::new()),
                session_id: session_id.clone(),
                pending_pull: self.store_pull.clone(),
            });

            let thread: Entity<AcpThread> = cx.update(|cx| {
                let action_log = cx.new(|_| ActionLog::new(project.clone()));
                cx.new(|cx| {
                    AcpThread::new(
                        None,
                        title,
                        Some(work_dirs),
                        self.clone(),
                        project,
                        action_log,
                        session_id.clone(),
                        watch::Receiver::constant(acp::PromptCapabilities::new().image(true)),
                        cx,
                    )
                })
            });

            // Register our hook callbacks AND ask claude for its slash-command
            // list. The real `claude` only emits this response after the first
            // user turn (same constraint as `--session-id` adoption above), so
            // `dispatch_initialize` does NOT block session creation — it spawns
            // a detached task that fires `AvailableCommandsUpdate` on the
            // AcpThread whenever the response eventually arrives.
            dispatch_initialize(&process, thread.downgrade(), shared.clone(), cx);

            // A resumed `claude` IGNORES `--model` (it keeps the model recorded
            // in the session transcript), so a model picked while this session
            // was cold/sleeping must be (re)applied via the runtime `set_model`
            // control request right after spawn — same mechanism as effort.
            // The map is only seeded for resume/live sessions (new sessions get
            // their model via `--model`, which a fresh spawn DOES honor).
            if let Some(model) = self.desired_models.borrow().get(&session_id).cloned() {
                process
                    .send_control(ControlRequestOut::SetModel { model })
                    .log_err();
            }
            if let Some(effort) = self.desired_efforts.borrow().get(&session_id).cloned() {
                process
                    .send_control(ControlRequestOut::ApplyFlagSettings {
                        settings: effort_settings_json(&effort),
                    })
                    .log_err();
            }

            let incoming = process.take_incoming();
            let critical_stderr = process.take_critical_stderr();
            let exited = process.wait_status();
            let outgoing = process.outgoing.clone();
            let update_pump = cx.spawn({
                let thread = thread.downgrade();
                let shared = shared.clone();
                async move |cx| {
                    run_update_pump(
                        incoming,
                        critical_stderr,
                        exited,
                        outgoing,
                        thread,
                        shared,
                        cx,
                    )
                    .await;
                }
            });

            self.sessions.borrow_mut().insert(
                session_id,
                SessionState {
                    process,
                    thread: thread.downgrade(),
                    shared,
                    blueprint,
                    _update_pump: update_pump,
                    escalation: None,
                    watchdog: None,
                },
            );

            Ok(thread)
        })
    }

    /// Arm a silence watchdog for the just-started turn on `session_id`. On a
    /// `Hung` verdict it routes through the same `recover_session` recovery as
    /// the Stop-escalation; on `Working`/`Unknown`/analyzer-failure it re-arms.
    /// Stores the watchdog on the session so the prompt's resolution can drop it.
    fn arm_watchdog(self: &Rc<Self>, session_id: &acp::SessionId, cx: &mut App) {
        let mut sessions = self.sessions.borrow_mut();
        let Some(session) = sessions.get_mut(session_id) else {
            return;
        };

        // Reset the silence baseline to NOW. `last_output` is the wall-time
        // of the last message the pump pulled off `incoming`, and it
        // survives across turns (it's stored on `SessionShared`, which
        // outlives a single prompt). If the user idles for longer than
        // `silence_window` between turns, the next `arm_watchdog` would
        // otherwise see `elapsed > silence_window` immediately, skip the
        // sleep, and dispatch the analyzer right away. The analyzer is a
        // separate `claude -p` call — 5-10s of latency — so a `Hung`
        // verdict on a fresh turn that's barely started fires
        // `recover_session` and silently cancels the user's brand-new
        // message ("опять в Done ушел втихую" bug). Stamping `now()` here
        // tells the watchdog "this turn just started; wait the full
        // window before second-guessing it".
        session
            .shared
            .last_output
            .set(cx.background_executor().now());

        let last_output = session.shared.last_output.clone();
        let process_id = session.process.process_id();
        let window = self.silence_window.get();
        let analyzer: Rc<dyn crate::watchdog::Analyzer> =
            Rc::new(ClaudeAnalyzer::new(self.binary.clone()));

        // The watchdog asks for fresh context at fire time, not arm time. The
        // thread's full event history isn't cheaply readable from a plain `Fn`
        // (it needs a `cx` to `read`); the Foundation analyzer prompt works from
        // silence-duration + pid alone. SP2 can enrich `recent_events`.
        let context_provider: Rc<dyn Fn() -> AnalyzerContext> = Rc::new(move || AnalyzerContext {
            silence_duration: window,
            process_id: Some(process_id),
            recent_events: Vec::new(),
            pending_tool_use: None,
        });

        let connection = Rc::downgrade(self);
        let session_id_for_recovery = session_id.clone();
        let recovery: crate::watchdog::RecoveryCallback =
            Rc::new(move |cx: &mut gpui::AsyncApp| {
                if let Some(connection) = connection.upgrade() {
                    connection.recover_session(session_id_for_recovery.clone(), cx);
                }
            });

        let mut async_cx = cx.to_async();
        let watchdog = Watchdog::arm(
            last_output,
            window,
            analyzer,
            context_provider,
            recovery,
            &mut async_cx,
        );
        session.watchdog = Some(watchdog);
    }

    /// Recovery primitive shared by Stop-escalation and (Phase 7.2) the
    /// watchdog's `Hung` verdict: SIGKILL the wedged `claude`, respawn it under
    /// the same session id with `--resume`, rewire a fresh update-pump onto the
    /// *existing* `AcpThread`, and force-resolve the in-flight prompt oneshot
    /// `Ok(TurnEnd::Stop(Cancelled))` so `store.rs`'s Cancelled queue logic runs.
    ///
    /// Spawned (not awaited) by the caller — it must not hold the `sessions`
    /// `RefCell` borrow across its `.await`s, so it re-borrows for the swap.
    fn recover_session(self: Rc<Self>, session_id: acp::SessionId, cx: &mut gpui::AsyncApp) {
        cx.spawn(async move |cx| {
            // Take only what we need out of the borrow, then drop it before any
            // await — `await_init` and `cx.update` below mustn't run while the
            // `sessions` map is borrowed (re-entrancy + borrow-across-await).
            let Some((blueprint, thread, prompt_tx)) = ({
                let mut sessions = self.sessions.borrow_mut();
                sessions.get_mut(&session_id).map(|session| {
                    session.escalation = None;
                    (
                        session.blueprint.clone(),
                        session.thread.clone(),
                        session.shared.prompt_tx.borrow_mut().take(),
                    )
                })
            }) else {
                return;
            };

            // Resolve the wedged prompt first so the UI leaves Running even if
            // the respawn below fails for any reason.
            if let Some(prompt_tx) = prompt_tx {
                log::warn!(
                    target: "claude_native::prompt_tx",
                    "recover_session took prompt_tx and force-resolved it as Cancelled \
                     (escalation path for stuck cancel / hung turn)"
                );
                prompt_tx
                    .send(Ok(TurnEnd::Stop(acp::StopReason::Cancelled)))
                    .ok();
            } else {
                log::debug!(
                    target: "claude_native::prompt_tx",
                    "recover_session found prompt_tx already None (turn ended between escalation \
                     arming and recovery firing)"
                );
            }

            // Kill the old process. Done via a short-lived borrow so the kill
            // call doesn't straddle an await.
            {
                let mut sessions = self.sessions.borrow_mut();
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.process.kill().log_err();
                }
            }

            let Some(work_dir) = blueprint.work_dirs.ordered_paths().next().cloned() else {
                return;
            };
            let spec = cx.update(|cx| ClaudeCommandSpec {
                binary: self.binary.clone(),
                work_dir,
                session: SessionArg::Resume(session_id.0.to_string()),
                mcp_servers_json: mcp_config_json(&mcp_servers_for_project(&blueprint.project, cx)),
                append_system_prompt: blueprint.append_system_prompt.clone(),
                extra_env: self.extra_env.clone(),
                model: blueprint.model.clone(),
            });

            let mut process = match cx.update(|cx| ClaudeProcess::spawn(spec, cx)) {
                Ok(process) => process,
                Err(error) => {
                    log::error!("claude_native: respawn on Stop escalation failed: {error}");
                    return;
                }
            };

            // No `init` wait: the resumed `claude` only emits `init` after its
            // next user turn, and we already know the (unchanged) session id.

            let shared = Rc::new(SessionShared {
                prompt_tx: RefCell::new(None),
                sticky_window: Cell::new(None),
                last_output: Rc::new(Cell::new(cx.background_executor().now())),
                cancel_requested: Cell::new(false),
                pending_inject: RefCell::new(None),
                stream_usage: RefCell::new(None),
                stream_used_total: Cell::new(None),
                active_model: RefCell::new(None),
                available_models: RefCell::new(Vec::new()),
                session_id: session_id.clone(),
                pending_pull: self.store_pull.clone(),
            });

            // Same `initialize` as `open_session`: the resumed process needs
            // its hook callbacks re-registered or live injection would stop
            // working after any escalation/respawn, AND it needs to re-pump
            // the slash-command list to the (preserved) AcpThread. Detached
            // task, just like the initial spawn.
            dispatch_initialize(&process, thread.clone(), shared.clone(), cx);

            // A resumed `claude` ignores `--model`; re-apply the desired model
            // via `set_model` after respawn (see `open_session`).
            if let Some(model) = self.desired_models.borrow().get(&session_id).cloned() {
                process
                    .send_control(ControlRequestOut::SetModel { model })
                    .log_err();
            }
            if let Some(effort) = self.desired_efforts.borrow().get(&session_id).cloned() {
                process
                    .send_control(ControlRequestOut::ApplyFlagSettings {
                        settings: effort_settings_json(&effort),
                    })
                    .log_err();
            }

            let incoming = process.take_incoming();
            let critical_stderr = process.take_critical_stderr();
            let exited = process.wait_status();
            let outgoing = process.outgoing.clone();
            let update_pump = cx.spawn({
                let shared = shared.clone();
                async move |cx| {
                    run_update_pump(
                        incoming,
                        critical_stderr,
                        exited,
                        outgoing,
                        thread,
                        shared,
                        cx,
                    )
                    .await;
                }
            });

            let mut sessions = self.sessions.borrow_mut();
            if let Some(session) = sessions.get_mut(&session_id) {
                session.process = process;
                session.shared = shared;
                session._update_pump = update_pump;
                // The recovered turn is force-resolved Cancelled; drop the
                // watchdog so its timer (which referenced the old `last_output`)
                // stops. A fresh prompt arms a new one.
                session.watchdog = None;
            }
            // If the session vanished while we were respawning (closed), the new
            // `process`/`update_pump` drop here and tear themselves down.
        })
        .detach();
    }
}

/// Per-turn diagnostic counters; see the `turn_stats` doc comment in
/// `run_update_pump`. All counters reset to zero on each terminating
/// `Result`.
#[derive(Default, Debug)]
struct TurnStats {
    /// Total characters of assistant TEXT content surfaced via
    /// `stream_event{content_block_delta, text_delta}` — what the user
    /// sees as the agent's reply.
    text_chars_streamed: usize,
    /// Same but for extended-thinking blocks. A turn with thinking>0 and
    /// text=0 means the agent thought but said nothing.
    thinking_chars_streamed: usize,
    /// Number of `tool_use` blocks the agent emitted (one per tool call).
    tool_calls_emitted: usize,
    /// Number of top-level `OutputMessage::Assistant` messages seen
    /// (subagent ones excluded — they have `parent_tool_use_id != None`).
    assistant_messages_received: usize,
    /// Number of TEXT content blocks inside `OutputMessage::Assistant`
    /// messages that `translate.rs` silently dropped on the assumption
    /// that text always arrives via stream_event deltas. If this is >0
    /// while `text_chars_streamed == 0`, the SDK's claude path
    /// accumulated those text blocks but our native pump loses them —
    /// strong candidate for the "silent end_turn" the user reports
    /// happens only with the native backend.
    assistant_text_blocks_dropped: usize,
}

/// Drain the process's `incoming` stream until EOF or process exit, applying
/// each message to the `AcpThread` and resolving the in-flight prompt oneshot on
/// the turn-ending `result`. On process exit with a prompt still pending, the
/// prompt is resolved with an error so the thread transitions to `Errored`
/// rather than hanging.
async fn run_update_pump(
    mut incoming: futures::channel::mpsc::UnboundedReceiver<OutputMessage>,
    mut critical_stderr: futures::channel::mpsc::UnboundedReceiver<crate::process::CriticalStderr>,
    exited: impl std::future::Future<Output = Option<std::process::ExitStatus>>,
    outgoing: futures::channel::mpsc::UnboundedSender<InputMessage>,
    thread: WeakEntity<AcpThread>,
    shared: Rc<SessionShared>,
    cx: &mut gpui::AsyncApp,
) {
    // A `can_use_tool` authorization can take arbitrarily long (it waits on the
    // user). The await is spawned off the pump so the loop keeps draining
    // `incoming`; the tasks are retained here for the pump's lifetime (= the
    // session's) so they aren't cancelled before the user responds.
    let mut authorization_tasks: Vec<Task<()>> = Vec::new();
    let mut exited = std::pin::pin!(exited.fuse());
    // Per-turn diagnostic accumulator. Reset after each terminating `Result`
    // and logged alongside the turn_end summary so a "no response where I
    // expected one" report can be cross-referenced with what actually
    // streamed during the turn. Catches: silent-end-turn (all counters 0,
    // text_chars=0 in result), thinking-only turns (thinking_chars>0,
    // text_chars=0), translation drops (assistant_text_blocks_dropped>0
    // means claude emitted text via OutputMessage::Assistant that
    // translate.rs skipped — the SDK accumulated those, we currently
    // don't), or "agent finished without saying anything despite tools"
    // (tool_calls_emitted>0, text_chars=0).
    let mut turn_stats = TurnStats::default();
    // Per-assistant-message flags: true once a `stream_event(*_delta)` for
    // text/thinking has arrived for the message currently being built; reset
    // on every `stream_event(message_start)` and on every
    // `OutputMessage::Assistant` (the boundary between sub-calls within a
    // multi-step turn). Used to distinguish "normal turn (text/thinking
    // already streamed → assistant block would double-render)" from "no
    // stream events → assistant block carries the only copy". Without this,
    // local slash commands like `/context` produce a `result.result` with
    // 9k chars of markdown but zero rendered UI bubbles, and extended-
    // thinking-only turns lose their `thinking` blocks the same way.
    let mut text_streamed_for_current_message: bool = false;
    let mut thinking_streamed_for_current_message: bool = false;
    // Accumulates the current assistant message's streamed TEXT (not
    // thinking), cleared on each `message_start`. Read at the Stop hook to
    // self-heal a degraded turn where the model wrote a tool call as text —
    // see `looks_like_text_tool_call` / `MAX_DEGENERATE_NUDGES`. The counter
    // bounds the retries and resets at each turn's `result`.
    let mut current_message_text = String::new();
    let mut degenerate_tool_call_nudges: u8 = 0;
    loop {
        let message = select_biased! {
            message = incoming.next().fuse() => message,
            critical = critical_stderr.next().fuse() => {
                // Critical stderr: claude reported an internal failure that
                // means the in-flight `Result` will never arrive (observed:
                // `Error in hook callback stop_inj` during a background-task
                // container restart leaves the turn hung indefinitely with
                // no terminating Result). Force-resolve the prompt oneshot
                // with an error so the AcpThread transitions to Errored
                // and the user sees "agent hook error: <detail>" instead
                // of an infinite "Thinking…".
                if let Some(crate::process::CriticalStderr::HookCallbackError {
                    callback_id,
                    first_line,
                }) = critical
                {
                    log::warn!(
                        target: "claude_native::stderr_watchdog",
                        "hook callback {callback_id} errored: {first_line} — force-resolving in-flight prompt"
                    );
                    if let Some(sender) = shared.prompt_tx.borrow_mut().take() {
                        sender
                            .send(Err(anyhow!(
                                "agent hook callback {callback_id} failed: {first_line}"
                            )))
                            .ok();
                    }
                }
                // The pump keeps running — claude may recover and emit more
                // messages on the same process. Only an exit / EOF tears it
                // down. Re-loop to await the next message.
                continue;
            }
            status = exited.as_mut() => {
                // Process died. If a turn was in flight, fail it so the thread
                // surfaces an error instead of hanging forever.
                if let Some(sender) = shared.prompt_tx.borrow_mut().take() {
                    let detail = match status {
                        Some(status) => format!("claude exited: {status}"),
                        None => "claude exited".to_string(),
                    };
                    log::warn!(
                        target: "claude_native::prompt_tx",
                        "process exit took prompt_tx and failed it: {detail}"
                    );
                    sender.send(Err(anyhow!(detail))).ok();
                }
                return;
            }
        };

        let Some(message) = message else {
            // stdout EOF. Fail any in-flight turn for the same reason as exit.
            if let Some(sender) = shared.prompt_tx.borrow_mut().take() {
                log::warn!(
                    target: "claude_native::prompt_tx",
                    "stdout EOF took prompt_tx and failed it"
                );
                sender
                    .send(Err(anyhow!("claude output stream closed")))
                    .ok();
            }
            return;
        };

        // Any output (partial delta or control request) is progress — reset the
        // silence watchdog's baseline before dispatching the message.
        shared.last_output.set(cx.background_executor().now());

        if let OutputMessage::Result(result) = &message {
            // Pass the per-turn stream/message-derived `used` so
            // `apply_usage` doesn't overwrite the meter with `result.usage`,
            // which the SDK aggregates across all sub-calls in a multi-step
            // turn (drives the meter past 100 % — observed 1.8M/1.0M).
            let update = apply_usage(
                result,
                &shared.sticky_window,
                shared.stream_used_total.get(),
            );
            if let Some(update) = update {
                thread
                    .update(cx, |thread, cx| {
                        thread.handle_session_update(update, cx).log_err();
                    })
                    .ok();
            }
            // End-of-turn — drop the per-turn cumulative snapshot built up
            // from message_start/message_delta so the next turn starts
            // fresh. The dedup cell is reset too (next turn may legitimately
            // emit the same total as the prior one as its first sample).
            shared.stream_usage.borrow_mut().take();
            shared.stream_used_total.set(None);

            // If we asked claude to stop, treat whatever terminal `result` it
            // sends as a cancellation — claude reports an interrupted turn as an
            // error (`error_during_execution`), not a clean cancel.
            let turn_end = if shared.cancel_requested.take() {
                TurnEnd::Stop(acp::StopReason::Cancelled)
            } else {
                classify_result(result)
            };
            // Diagnostic: log EVERY turn-end so a "no response where I
            // expected one" report can be cross-referenced against what
            // claude actually emitted. `result_text_len` == 0 with
            // `stop_reason == "end_turn"` and no is_error is the
            // smoking-gun signature of "claude chose to say nothing"
            // (vs a real error, a cancel, or a tool-call sequence still
            // in flight). Logged at info so it's grep-able without
            // raising verbosity.
            let result_preview: String = result
                .result
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(120)
                .collect();
            let result_text_chars = result
                .result
                .as_deref()
                .map(|s| s.chars().count())
                .unwrap_or(0);
            log::info!(
                target: "claude_native::turn_end",
                "subtype={subtype:?} stop_reason={stop:?} is_error={is_err} \
                 result_text_chars={result_chars} cancel_requested={cancel} classified={classified:?} \
                 streamed_text_chars={streamed_text} streamed_thinking_chars={streamed_thinking} \
                 tool_calls={tool_calls} assistant_msgs={assistant_msgs} \
                 dropped_assistant_text_blocks={dropped_text} text_preview={preview:?}",
                subtype = result.subtype,
                stop = result.stop_reason,
                is_err = result.is_error,
                result_chars = result_text_chars,
                cancel = matches!(turn_end, TurnEnd::Stop(acp::StopReason::Cancelled)),
                classified = turn_end,
                streamed_text = turn_stats.text_chars_streamed,
                streamed_thinking = turn_stats.thinking_chars_streamed,
                tool_calls = turn_stats.tool_calls_emitted,
                assistant_msgs = turn_stats.assistant_messages_received,
                dropped_text = turn_stats.assistant_text_blocks_dropped,
                preview = result_preview,
            );
            // Explicit silent-end warning so the smoking-gun case stands out
            // from the normal turn_end info stream without having to grep
            // for `text_chars=0`.
            if !result.is_error
                && result_text_chars == 0
                && turn_stats.text_chars_streamed == 0
                && turn_stats.tool_calls_emitted == 0
                && !matches!(turn_end, TurnEnd::Stop(acp::StopReason::Cancelled))
            {
                log::warn!(
                    target: "claude_native::turn_end",
                    "SILENT END: agent produced no text, no tool calls, and the result text is empty \
                     (stop_reason={stop:?}, thinking_chars={think}, dropped_text_blocks={dropped}). \
                     If thinking > 0, the model only reasoned. If dropped > 0, our translate path \
                     silenced something the SDK would have surfaced.",
                    stop = result.stop_reason,
                    think = turn_stats.thinking_chars_streamed,
                    dropped = turn_stats.assistant_text_blocks_dropped,
                );
            }
            turn_stats = TurnStats::default();
            degenerate_tool_call_nudges = 0;
            if let Some(sender) = shared.prompt_tx.borrow_mut().take() {
                log::debug!(
                    target: "claude_native::prompt_tx",
                    "Result handler took prompt_tx and resolved with {turn_end:?}"
                );
                sender.send(Ok(turn_end)).ok();
            } else {
                // Orphan result: claude emitted a terminating `result` with no
                // matching `prompt()` in flight. Observed shape — the agent
                // launched `Bash(run_in_background=true)`, finished the main
                // turn cleanly (prompt_tx taken by the previous result), and
                // minutes later when the background command completed claude
                // resumed on its own, streamed `BashOutput` + a follow-up
                // assistant message, and emitted a SECOND terminating result
                // here. The intermediate assistant deltas already promoted
                // session state Idle→Running via the store's NewEntry
                // handler (store.rs:1951 — assumes a NewEntry implies a
                // turn is in flight). Without a Stopped event the session
                // sticks on Running forever, and a follow-up user message
                // gets routed to hook injection (queue.rs Running branch)
                // that never delivers because claude is actually idle.
                // Synthesize Stopped on the AcpThread so the store flips
                // back to Idle and the next user message starts a fresh
                // prompt().
                // For a clean orphan stop, synthesize `Stopped` so the store
                // flips Running→Idle. For an orphan *error* (e.g. an account
                // usage-limit `result` that lands after `prompt_tx` was already
                // consumed), emit `Error` instead: synthesizing a benign
                // `Stopped(EndTurn)` here would silently swallow the failure and
                // leave the session looking like it ended normally (the observed
                // "froze with no error" symptom). `Error` flips the session to
                // `Errored` so the user — and the supervisor's usage-limit
                // auto-resume — actually see it.
                match &turn_end {
                    TurnEnd::Stop(reason) => {
                        log::warn!(
                            target: "claude_native::prompt_tx",
                            "Result handler found prompt_tx already None — synthesizing Stopped({reason:?}) \
                             on the AcpThread (orphan result, likely background-bash continuation)"
                        );
                        let reason = *reason;
                        thread
                            .update(cx, |thread, cx| {
                                // Orphan-result turns bypass the run-turn
                                // completion path, so the follow-up assistant
                                // message's final buffered tail is never flushed
                                // into `session.entries` — it stays truncated at
                                // the last streamed intermediate step (the
                                // "mobile shows a stale step + Idle" bug). Flush
                                // it here, in the same step, before the terminal
                                // emit, exactly as the mainline completion does.
                                thread.flush_end_of_turn_tail(cx);
                                cx.emit(AcpThreadEvent::Stopped(reason));
                            })
                            .ok();
                    }
                    TurnEnd::Error(detail) => {
                        log::warn!(
                            target: "claude_native::prompt_tx",
                            "Result handler found prompt_tx already None for an ERROR result — surfacing it \
                             on the AcpThread instead of swallowing it (orphan error; detail={detail})"
                        );
                        thread
                            .update(cx, |thread, cx| {
                                // See the orphan-Stop branch: flush the final
                                // tail before the terminal emit so the last
                                // assistant bytes reach `session.entries`.
                                thread.flush_end_of_turn_tail(cx);
                                cx.emit(AcpThreadEvent::Error);
                            })
                            .ok();
                    }
                }
            }
            continue;
        }

        if let OutputMessage::ControlRequest(envelope) = message {
            match &envelope.request {
                ControlRequestKind::HookCallback {
                    callback_id, input, ..
                } => {
                    // Ask the store for the next queued follow-up (when a pull is
                    // registered), falling back to the local `pending_inject`
                    // buffer (kept for tests with no registered pull). Ship it
                    // back as `additionalContext` (or, for Stop, also as `reason`
                    // with `decision: "block"`). No pending → empty success no-op.
                    let is_end_of_turn = callback_id.as_str() == HOOK_CALLBACK_STOP;
                    // Agent Teams: a subagent's hook input carries `agent_id`
                    // (the main agent's does not). Forward it so the store can
                    // route the queued follow-up to the agent the user aimed it
                    // at — without this, a running subagent's hook swallows a
                    // message meant for the main agent.
                    let agent_id = input.get("agent_id").and_then(|v| v.as_str());
                    let pull = shared.pending_pull.borrow().clone();
                    let mut pending = match pull {
                        Some(pull) => pull(&shared.session_id, agent_id, is_end_of_turn, cx),
                        None => shared.pending_inject.borrow_mut().take(),
                    };
                    // Self-heal a degraded turn: the main agent occasionally
                    // emits a tool call as literal `<invoke name=…>` text and
                    // then stops, so nothing runs and the session would sit
                    // Idle on one bad message. When the Stop hook fires with
                    // nothing else to deliver and the just-finished assistant
                    // text looks like a text-form tool call, inject a bounded
                    // one-line nudge (build_hook_response adds decision:block)
                    // so the agent retries as a real tool call in the SAME
                    // turn. Main agent only — `current_message_text` holds the
                    // main stream, not a teammate's (whose hook carries an
                    // `agent_id`), so it must not be matched against a
                    // teammate's Stop.
                    if is_end_of_turn
                        && pending.is_none()
                        && agent_id.is_none()
                        && degenerate_tool_call_nudges < MAX_DEGENERATE_NUDGES
                        && looks_like_text_tool_call(&current_message_text)
                    {
                        degenerate_tool_call_nudges += 1;
                        log::warn!(
                            target: "claude_native",
                            "session={:?} degraded turn: assistant wrote a tool call as text; \
                             nudging retry ({}/{})",
                            shared.session_id,
                            degenerate_tool_call_nudges,
                            MAX_DEGENERATE_NUDGES,
                        );
                        pending = Some(DEGENERATE_TOOL_CALL_NUDGE.to_string());
                    }
                    let response = build_hook_response(&envelope.request_id, callback_id, pending);
                    outgoing
                        .unbounded_send(InputMessage::ControlResponse {
                            request_id: envelope.request_id.clone(),
                            response,
                        })
                        .log_err();
                }
                ControlRequestKind::CanUseTool { .. } => {
                    if let Some(task) =
                        spawn_tool_authorization(envelope, outgoing.clone(), thread.clone(), cx)
                    {
                        authorization_tasks.push(task);
                    }
                }
                ControlRequestKind::Other => {
                    log::debug!(
                        "claude_native: ignoring unknown control_request {}",
                        envelope.request_id
                    );
                }
            }
            continue;
        }

        // Diagnostic: count text/thinking chars per turn from stream
        // deltas. This is what actually gets rendered to the user; the
        // turn_end log line compares it against `result.result` so a
        // mismatch flags translation drops.
        if let OutputMessage::StreamEvent(ev) = &message
            && ev.event.get("type").and_then(|t| t.as_str()) == Some("content_block_delta")
            && let Some(delta) = ev.event.get("delta")
        {
            match delta.get("type").and_then(|t| t.as_str()) {
                Some("text_delta") => {
                    if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                        turn_stats.text_chars_streamed += t.chars().count();
                        text_streamed_for_current_message = true;
                        current_message_text.push_str(t);
                    }
                }
                Some("thinking_delta") => {
                    if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                        turn_stats.thinking_chars_streamed += t.chars().count();
                        thinking_streamed_for_current_message = true;
                    }
                }
                _ => {}
            }
        }
        // `message_start` opens a fresh assistant message — reset the
        // streamed-* flags so we can detect the next message's local-vs-
        // streamed nature independently.
        if let OutputMessage::StreamEvent(ev) = &message
            && ev.event.get("type").and_then(|t| t.as_str()) == Some("message_start")
        {
            text_streamed_for_current_message = false;
            thinking_streamed_for_current_message = false;
            current_message_text.clear();
        }

        // Mid-turn usage: `message_start` snapshots `event.message.usage`
        // and latches the active model; `message_delta` carries cumulative
        // replacement updates. Emitting from each tick keeps the meter
        // moving on long cache-warm turns instead of jumping only at the
        // terminal `result`. See translate::apply_stream_usage.
        if let OutputMessage::StreamEvent(ev) = &message {
            let prev_usage = shared.stream_usage.borrow().clone();
            let outcome = apply_stream_usage(
                ev,
                prev_usage.as_ref(),
                shared.stream_used_total.get(),
                shared.sticky_window.get(),
            );
            if let Some(model) = outcome.new_model {
                let mut active = shared.active_model.borrow_mut();
                let model_changed = active.as_deref() != Some(model.as_str());
                *active = Some(model.clone());
                drop(active);
                // If we haven't seen a real `result.contextWindow` yet,
                // upgrade the meter limit from the 200k default to the
                // model-inferred window so the first delta doesn't render
                // the meter against a wrong limit. Once a real result
                // value arrives `apply_usage` overwrites sticky_window.
                if model_changed
                    && let Some(inferred) = infer_context_window_from_model(&model)
                    && shared
                        .sticky_window
                        .get()
                        .is_none_or(|w| w == 0 || w == DEFAULT_CONTEXT_WINDOW)
                {
                    shared.sticky_window.set(Some(inferred));
                }
            }
            if let Some(usage) = outcome.new_usage {
                *shared.stream_usage.borrow_mut() = Some(usage);
            }
            if let Some(update) = outcome.update {
                if let acp::SessionUpdate::UsageUpdate(ref u) = update {
                    shared.stream_used_total.set(Some(u.used));
                }
                thread
                    .update(cx, |thread, cx| {
                        thread.handle_session_update(update, cx).log_err();
                    })
                    .ok();
            }
        }

        // Per-call usage from assistant messages drives the meter mid-turn.
        // The terminal `result` event can drop to a tiny number once the
        // cache is warm (the SDK only reports the last sub-call's usage),
        // so depending solely on `apply_usage(result)` makes the meter
        // collapse on cached turns. See translate::assistant_usage_update.
        if let OutputMessage::Assistant(m) = &message
            && let Some(update) = assistant_usage_update(m, shared.sticky_window.get())
        {
            match &m.parent_tool_use_id {
                // Top-level assistant usage drives the parent session meter.
                None => {
                    thread
                        .update(cx, |thread, cx| {
                            thread.handle_session_update(update, cx).log_err();
                        })
                        .ok();
                }
                // Feature B: a Task subagent's usage is NOT folded into the
                // parent meter (it's a separate context window), but it is
                // forwarded keyed by the subagent's parent tool-use id so the
                // status row can show the Task's own context when the user
                // drills into its tab. `assistant_usage_update` always yields a
                // `UsageUpdate`; the match guard is belt-and-suspenders.
                Some(parent) => {
                    if let acp::SessionUpdate::UsageUpdate(u) = update {
                        let parent = parent.clone();
                        thread
                            .update(cx, |thread, cx| {
                                thread.update_subagent_token_usage(parent, u.used, u.size, cx);
                            })
                            .ok();
                    }
                }
            }
        }

        // Diagnostic: dump every top-level assistant message's content
        // block summary (types, text length per block, tool-use names),
        // and count any TEXT blocks that translate.rs will silently drop
        // because it assumes text always arrives via stream_event deltas.
        // If `dropped_assistant_text_blocks > 0 && text_chars_streamed == 0`
        // at turn-end, that's the smoking gun: the SDK's claude path
        // surfaces those blocks; our native pump does not.
        if let OutputMessage::Assistant(m) = &message {
            // Subagent (`parent_tool_use_id.is_some()`) goes through the same
            // diagnostic and fallback paths as a top-level assistant message —
            // we now render subagent text / thinking / image / tool_use so
            // the user sees what the Task subagent did. The only place that
            // still gates on `parent_tool_use_id.is_none()` is the
            // assistant-usage meter (above), which must NOT count subagent
            // sub-calls in the parent's context window.
            if m.parent_tool_use_id.is_none() {
                turn_stats.assistant_messages_received += 1;
            }
            let parent_id = m.parent_tool_use_id.clone();
            let blocks = m
                .message
                .get("content")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            // Local-command path + extended-thinking + agent-emitted images +
            // redacted reasoning: claude can ship these block types in an
            // `Assistant` message WITHOUT corresponding stream-event deltas.
            // Surface each as the right ACP update so nothing's invisible.
            // Skipped when the corresponding stream-delta arrived for this
            // message — would otherwise double-render. Tracks fallbacks per
            // turn so the diagnostic stats reflect real drops.
            let mut text_blocks_recovered: usize = 0;
            let mut thinking_blocks_recovered: usize = 0;
            let mut image_blocks_recovered: usize = 0;
            let mut redacted_blocks_recovered: usize = 0;
            for block in &blocks {
                let kind = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match kind {
                    "text" => {
                        // The final `Assistant` message carries the AUTHORITATIVE
                        // complete text. Three cases:
                        //  - nothing streamed → emit the whole block (recovery for
                        //    the local-command / non-streaming path).
                        //  - streaming was PARTIAL (the streamed deltas are a
                        //    proper prefix of the final, shorter than it — a
                        //    truncated/dropped stream) → emit only the MISSING
                        //    SUFFIX so the message is completed, not left cut off
                        //    (the "interrupted reply stays 'Тел'" bug).
                        //  - streaming already delivered the whole text → emit
                        //    nothing (a genuine duplicate).
                        // Appends, so we must never re-emit already-streamed bytes.
                        if let Some(text) = block.get("text").and_then(|v| v.as_str())
                            && !text.is_empty()
                        {
                            let suffix = final_text_block_suffix(
                                text_streamed_for_current_message,
                                &current_message_text,
                                text,
                            );
                            if let Some(suffix) = suffix.filter(|s| !s.is_empty()) {
                                let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(
                                    acp::TextContent::new(suffix.to_string()),
                                ));
                                let mut update = acp::SessionUpdate::AgentMessageChunk(chunk);
                                if let Some(parent) = parent_id.as_deref() {
                                    stamp_subagent_meta(&mut update, parent);
                                }
                                thread
                                    .update(cx, |thread, cx| {
                                        thread.handle_session_update(update, cx).log_err();
                                    })
                                    .ok();
                                text_blocks_recovered += 1;
                            }
                        }
                    }
                    "thinking" if !thinking_streamed_for_current_message => {
                        if let Some(text) = block.get("thinking").and_then(|v| v.as_str())
                            && !text.is_empty()
                        {
                            let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(
                                acp::TextContent::new(text.to_string()),
                            ));
                            let mut update = acp::SessionUpdate::AgentThoughtChunk(chunk);
                            if let Some(parent) = parent_id.as_deref() {
                                stamp_subagent_meta(&mut update, parent);
                            }
                            thread
                                .update(cx, |thread, cx| {
                                    thread.handle_session_update(update, cx).log_err();
                                })
                                .ok();
                            thinking_blocks_recovered += 1;
                        }
                    }
                    "image" => {
                        // Agent-emitted image: same wire shape Anthropic uses
                        // for user-attached images
                        // (`source.{type:"base64"|"url", media_type, data|url}`).
                        // Reuse the existing `ContentBlock::Image` render path
                        // the compose row already feeds for human-attached
                        // images so the UI handles either side identically.
                        if let Some(image) = image_block_from_anthropic(block) {
                            let chunk = acp::ContentChunk::new(image);
                            let mut update = acp::SessionUpdate::AgentMessageChunk(chunk);
                            if let Some(parent) = parent_id.as_deref() {
                                stamp_subagent_meta(&mut update, parent);
                            }
                            thread
                                .update(cx, |thread, cx| {
                                    thread.handle_session_update(update, cx).log_err();
                                })
                                .ok();
                            image_blocks_recovered += 1;
                        } else {
                            log::debug!(
                                target: "claude_native::dropped",
                                "assistant image block had no base64/url source — skipped"
                            );
                        }
                    }
                    "redacted_thinking" => {
                        // Anthropic returns reasoning whose content the
                        // safety layer can't expose; just put a placeholder
                        // in the thought stream so the user sees that
                        // SOMETHING was reasoned about, instead of a
                        // mysterious gap in the timeline.
                        let chunk =
                            acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                                "[encrypted reasoning hidden by Anthropic safety layer]"
                                    .to_string(),
                            )));
                        let mut update = acp::SessionUpdate::AgentThoughtChunk(chunk);
                        if let Some(parent) = parent_id.as_deref() {
                            stamp_subagent_meta(&mut update, parent);
                        }
                        thread
                            .update(cx, |thread, cx| {
                                thread.handle_session_update(update, cx).log_err();
                            })
                            .ok();
                        redacted_blocks_recovered += 1;
                    }
                    "server_tool_use" | "web_search_tool_result" => log::debug!(
                        target: "claude_native::dropped",
                        "assistant block {kind} dropped (server-side tool, not surfaced as ACP tool call)"
                    ),
                    _ => {}
                }
            }
            // Reset flags for next message (or sub-call).
            text_streamed_for_current_message = false;
            thinking_streamed_for_current_message = false;
            // Per-message recovery counts feed the assistant_blocks debug
            // line so a "smoking gun" report can tell apart a normal
            // streaming turn from a local-command / image / redacted
            // fallback. Subagent messages don't increment
            // `assistant_messages_received`, so they show as #0 here —
            // that's the on-purpose marker for "this is sub-output".
            log::debug!(
                target: "claude_native::dropped",
                "assistant message #{} recovered: text={text_blocks_recovered} thinking={thinking_blocks_recovered} image={image_blocks_recovered} redacted={redacted_blocks_recovered}",
                turn_stats.assistant_messages_received,
            );
            let summary: Vec<String> = blocks
                .iter()
                .map(|b| {
                    let kind = b.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                    match kind {
                        "text" => {
                            let chars = b
                                .get("text")
                                .and_then(|v| v.as_str())
                                .map(|s| s.chars().count())
                                .unwrap_or(0);
                            // Counter reflects REAL drops only — the fallback
                            // above emits text blocks when no stream-text
                            // arrived for this message. We increment only
                            // when the stream-text path won (block was a
                            // duplicate of already-streamed bytes) — that's
                            // not a "drop" so log it under recovered=0.
                            // When the recovery path emitted the block we
                            // don't count it as dropped; the counter then
                            // matches what the legacy `dropped_assistant_*`
                            // diagnostic was actually trying to surface.
                            if text_blocks_recovered == 0 {
                                turn_stats.assistant_text_blocks_dropped += 1;
                            }
                            format!("text({chars}ch)")
                        }
                        "thinking" => {
                            let chars = b
                                .get("thinking")
                                .and_then(|v| v.as_str())
                                .map(|s| s.chars().count())
                                .unwrap_or(0);
                            format!("thinking({chars}ch)")
                        }
                        "tool_use" => {
                            let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                            turn_stats.tool_calls_emitted += 1;
                            format!("tool_use({name})")
                        }
                        other => other.to_string(),
                    }
                })
                .collect();
            log::debug!(
                target: "claude_native::assistant_blocks",
                "assistant message #{count} blocks=[{summary}]",
                count = turn_stats.assistant_messages_received,
                summary = summary.join(", "),
            );
        }

        let parent_tool_use_id = match &message {
            OutputMessage::Assistant(m) => m.parent_tool_use_id.clone(),
            OutputMessage::User(m) => m.parent_tool_use_id.clone(),
            OutputMessage::StreamEvent(ev) => ev.parent_tool_use_id.clone(),
            _ => None,
        };
        for mut update in translate(&message) {
            if let Some(parent) = parent_tool_use_id.as_deref() {
                stamp_subagent_meta(&mut update, parent);
            }
            thread
                .update(cx, |thread, cx| {
                    thread.handle_session_update(update, cx).log_err();
                })
                .ok();
        }
    }
}

/// Answer a `can_use_tool` control request by AUTO-APPROVING it, without
/// surfacing an Allow/Reject prompt.
///
/// Why auto-approve: the fork spawns the MAIN agent with
/// `--permission-mode bypassPermissions`, so it never sends a `can_use_tool`
/// request — every one that reaches here is from an Agent Teams TEAMMATE.
/// `claude` deliberately does NOT let an auto-spawned sub-agent inherit
/// `bypassPermissions` (an autonomous agent with blanket bypass would be a
/// safety hole), so each teammate tool call would otherwise pop a confirmation
/// the user has to answer — and a busy multi-teammate run drowns in prompts
/// (and the session sits in `AwaitingInput` until each is answered). Since the
/// user has already opted the whole workspace into bypass for the main agent,
/// we extend the same trust to its teammates. The tool call is still visible
/// in the teammate's transcript (`claude` streams the `tool_use` block before
/// this request), so nothing is hidden — only the per-call gate is dropped.
///
/// Returns `None` (no task to retain — the response is sent synchronously
/// here) for every input, including non-`can_use_tool` requests. To restore
/// per-call gating, revert to driving `AcpThread::request_tool_call_authorization`
/// and awaiting the user's outcome.
fn spawn_tool_authorization(
    envelope: ControlRequestEnvelope,
    outgoing: futures::channel::mpsc::UnboundedSender<InputMessage>,
    _thread: WeakEntity<AcpThread>,
    _cx: &mut gpui::AsyncApp,
) -> Option<Task<()>> {
    let ControlRequestKind::CanUseTool {
        tool_name,
        tool_use_id,
        ..
    } = envelope.request
    else {
        return None;
    };
    log::debug!(
        "claude_native: auto-approving teammate tool call {tool_name} (tool_use_id={tool_use_id}, \
         request_id={})",
        envelope.request_id,
    );
    outgoing
        .unbounded_send(InputMessage::permission_response(envelope.request_id, true))
        .log_err();
    None
}

impl AgentConnection for ClaudeNativeConnection {
    fn agent_id(&self) -> AgentId {
        self.agent_id.clone()
    }

    fn telemetry_id(&self) -> SharedString {
        SharedString::new_static("claude-native")
    }

    fn new_session(
        self: Rc<Self>,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        self.new_session_with_meta(project, work_dirs, None, cx)
    }

    fn new_session_with_meta(
        self: Rc<Self>,
        project: Entity<Project>,
        work_dirs: PathList,
        extra_meta: Option<acp::Meta>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let session = SessionArg::New(uuid::Uuid::new_v4().to_string());
        self.open_session(session, project, work_dirs, None, extra_meta, cx)
    }

    fn active_model(&self, session_id: &acp::SessionId) -> Option<SharedString> {
        let sessions = self.sessions.borrow();
        let session = sessions.get(session_id)?;
        session
            .shared
            .active_model
            .borrow()
            .clone()
            .map(SharedString::from)
    }

    fn supports_resume_session(&self) -> bool {
        true
    }

    fn resume_session(
        self: Rc<Self>,
        session_id: acp::SessionId,
        project: Entity<Project>,
        work_dirs: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let session = SessionArg::Resume(session_id.0.to_string());
        self.open_session(session, project, work_dirs, title, None, cx)
    }

    fn supports_close_session(&self) -> bool {
        true
    }

    fn auth_methods(&self) -> &[acp::AuthMethod] {
        &[]
    }

    fn authenticate(&self, _method: acp::AuthMethodId, _cx: &mut App) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn prompt(
        &self,
        _user_message_id: UserMessageId,
        params: acp::PromptRequest,
        cx: &mut App,
    ) -> Task<Result<acp::PromptResponse>> {
        let thread;
        let receiver;
        {
            let sessions = self.sessions.borrow();
            let Some(session) = sessions.get(&params.session_id) else {
                return Task::ready(Err(anyhow!(
                    "no native claude session for {}",
                    params.session_id.0
                )));
            };

            let (sender, prompt_receiver) = oneshot::channel();
            *session.shared.prompt_tx.borrow_mut() = Some(sender);
            log::debug!(
                target: "claude_native::prompt_tx",
                "prompt() armed prompt_tx for session {}", params.session_id.0
            );

            if let Err(error) = session
                .process
                .outgoing
                .unbounded_send(InputMessage::user_blocks(&params.prompt))
            {
                session.shared.prompt_tx.borrow_mut().take();
                return Task::ready(Err(anyhow!("claude stdin closed: {error}")));
            }

            thread = session.thread.clone();
            receiver = prompt_receiver;
        }

        // Arm the silence watchdog for this turn (re-borrow mutably now that the
        // immutable borrow above is dropped). Disarmed below once the prompt
        // resolves, by whichever path resolves it. `arm_watchdog` needs an owned
        // `Rc<Self>` for the recovery callback; the `self_handle` weak yields it.
        let connection = self.self_handle.borrow().clone();
        if let Some(connection) = connection.upgrade() {
            connection.arm_watchdog(&params.session_id, cx);
        }

        let session_id = params.session_id.clone();
        let connection = self.self_handle.borrow().clone();
        cx.spawn(async move |cx| {
            let outcome = match receiver.await {
                Ok(Ok(TurnEnd::Stop(stop_reason))) => Ok(acp::PromptResponse::new(stop_reason)),
                Ok(Ok(TurnEnd::Error(detail))) => {
                    thread
                        .update(cx, |_thread, cx| cx.emit(AcpThreadEvent::Error))
                        .ok();
                    Err(anyhow!(detail))
                }
                Ok(Err(error)) => Err(error),
                // Sender dropped without sending (session torn down): treat as a
                // cancellation rather than a hard error.
                Err(_) => Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)),
            };

            // Turn ended (any path) — drop the watchdog so its silence timer
            // stops until the next prompt re-arms it.
            if let Some(connection) = connection.upgrade()
                && let Some(session) = connection.sessions.borrow_mut().get_mut(&session_id)
            {
                session.watchdog = None;
            }

            outcome
        })
    }

    fn cancel(&self, session_id: &acp::SessionId, cx: &mut App) {
        // Stage 1: a soft `interrupt` control request. A well-behaved `claude`
        // ends the turn with `result(cancelled)`, which the update-pump resolves
        // through the prompt oneshot (the normal path) — no escalation needed.
        {
            let sessions = self.sessions.borrow();
            let Some(session) = sessions.get(session_id) else {
                return;
            };
            // No turn in flight, yet the caller asked us to cancel — the store
            // believes a turn is running while this connection holds no prompt
            // oneshot (the classic desync: a reconnect/respawn replayed the
            // transcript but the store's `Running` survived, or an orphan
            // `result` already consumed `prompt_tx`). Returning silently here
            // was the "no-prompt-tx race": nothing would ever emit `Stopped`,
            // so the store sat in `Stopping` until its 40s safety-net
            // force-flipped it to `Idle` — and because the queue flush is
            // keyed on the `Stopped` event, an `interrupt_and_flush_pending`
            // follow-up never got delivered at the interrupt (it limped in on
            // a later idle-flush instead). Synthesize the terminal event the
            // backend can no longer produce, exactly as the pump's
            // orphan-result path does, so the store settles immediately and
            // `flush_after_cancel` fires.
            if session.shared.prompt_tx.borrow().is_none() {
                let thread = session.thread.clone();
                log::warn!(
                    "claude_native: cancel with no in-flight prompt (store/connection desync) \
                     — synthesizing Stopped(Cancelled) on the AcpThread"
                );
                thread
                    .update(cx, |thread, cx| {
                        thread.flush_end_of_turn_tail(cx);
                        cx.emit(AcpThreadEvent::Stopped(acp::StopReason::Cancelled));
                    })
                    .ok();
                return;
            }
            // Idempotent: a Stop is already in flight for this session — keep the
            // single 30s clock, don't restart it on a repeated cancel.
            if session.escalation.is_some() {
                return;
            }
            // Mark the cancellation so the pump maps claude's interrupt result
            // (an error, not a clean cancel) to `Cancelled` rather than `Errored`.
            session.shared.cancel_requested.set(true);
            // The interrupt's control_response (the returned receiver) is
            // irrelevant to escalation timing — we escalate on the prompt
            // staying pending, not on the ack — so it is dropped here.
            match session.process.send_control(ControlRequestOut::Interrupt) {
                Ok(_receiver) => {}
                Err(error) => log::warn!("claude_native: interrupt write failed: {error}"),
            }
        }

        // Stage 2: arm the escalation. After the grace period, if the prompt
        // oneshot is still pending (claude ignored the interrupt), kill + resume.
        // Capture a *weak* handle so the stored task (owned by the session, owned
        // by this `Rc`) doesn't form a strong cycle that pins the connection.
        let connection = self.self_handle.borrow().clone();
        let timeout = self.escalation_timeout.get();
        let session_id_for_task = session_id.clone();
        let escalation = cx.spawn(async move |cx| {
            cx.background_executor().timer(timeout).await;
            let Some(connection) = connection.upgrade() else {
                return;
            };
            // Two-signal check. A `prompt_tx` that's still `Some` is no
            // longer enough on its own to mean "claude ignored the
            // interrupt": when a well-behaved cancel resolves (pump's
            // Result handler `take`s `prompt_tx` AND `take`s
            // `cancel_requested` → false), the user can immediately
            // arm a fresh `prompt_tx` by sending a new message before
            // this 30s timer expires — that fresh sender belongs to a
            // turn this escalation never targeted, and `recover_session`
            // would force-Cancel a healthy in-flight turn the user just
            // started ("опять в Done ушел втихую" bug). Only escalate
            // when BOTH the prompt is still pending AND
            // `cancel_requested` is still set — i.e. the same cancel
            // we armed this task for is still un-acknowledged by claude.
            let sessions = connection.sessions.borrow();
            let Some(session) = sessions.get(&session_id_for_task) else {
                return;
            };
            let prompt_pending = session.shared.prompt_tx.borrow().is_some();
            let cancel_still_outstanding = session.shared.cancel_requested.get();
            drop(sessions);
            if prompt_pending && cancel_still_outstanding {
                connection.recover_session(session_id_for_task, cx);
            } else if let Some(session) = connection
                .sessions
                .borrow_mut()
                .get_mut(&session_id_for_task)
            {
                // Cancel was acknowledged before our timer fired; clear the
                // armed task slot so the next `cancel()` can re-arm cleanly.
                session.escalation = None;
            }
        });
        if let Some(session) = self.sessions.borrow_mut().get_mut(session_id) {
            session.escalation = Some(escalation);
            self.escalations_armed.set(self.escalations_armed.get() + 1);
        }
    }

    fn close_session(
        self: Rc<Self>,
        session_id: &acp::SessionId,
        _cx: &mut App,
    ) -> Task<Result<()>> {
        if let Some(mut session) = self.sessions.borrow_mut().remove(session_id) {
            session.process.kill().log_err();
        }
        Task::ready(Ok(()))
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_text_block_suffix_cases() {
        // Not streamed → emit the whole final block (recovery path).
        assert_eq!(final_text_block_suffix(false, "", "full reply"), Some("full reply"));
        // Streamed the complete text → duplicate, emit nothing.
        assert_eq!(final_text_block_suffix(true, "full reply", "full reply"), None);
        // Streamed a SHORT prefix (truncated stream) → emit only the suffix.
        assert_eq!(
            final_text_block_suffix(true, "Тел", "Телефон подключён"),
            Some("ефон подключён")
        );
        // Streamed text isn't a prefix of the final (mismatch) → don't
        // double-append; leave it (None).
        assert_eq!(final_text_block_suffix(true, "Tel", "Телефон"), None);
        // Streamed text longer than final (shouldn't happen) → None.
        assert_eq!(final_text_block_suffix(true, "Телефон", "Тел"), None);
    }

    #[test]
    fn build_default_hooks_registers_post_tool_use_and_stop() {
        let hooks = build_default_hooks();
        let post = hooks.get("PostToolUse").expect("PostToolUse registered");
        assert_eq!(post.len(), 1);
        assert_eq!(post[0].hook_callback_ids, vec!["pti".to_string()]);
        let stop = hooks.get("Stop").expect("Stop registered");
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0].hook_callback_ids, vec!["stop_inj".to_string()]);
    }

    #[test]
    fn detects_tool_call_written_as_text() {
        // The exact degraded shape opus emits (leaked function-calling XML).
        let degraded = "card\n<invoke name=\"Bash\">\n<parameter name=\"command\">echo hi</parameter>\n</invoke>";
        assert!(looks_like_text_tool_call(degraded));
        // Closing-only / opening-only or a bare mention must NOT trigger.
        assert!(!looks_like_text_tool_call(
            "I'll invoke the build now and check the output."
        ));
        assert!(!looks_like_text_tool_call(
            "Done — the offscreen smoke test passed with no panics."
        ));
        assert!(!looks_like_text_tool_call("<invoke name=\"Bash\""));
    }

    #[test]
    fn hook_response_empty_when_no_pending_inject() {
        let response = build_hook_response("hk1", "pti", None);
        assert_eq!(response["subtype"], "success");
        assert_eq!(response["request_id"], "hk1");
        assert!(response["response"].as_object().unwrap().is_empty());
    }

    #[test]
    fn hook_response_post_tool_use_carries_additional_context() {
        let response = build_hook_response("hk1", "pti", Some("PURPLE_PINEAPPLE".to_string()));
        assert_eq!(response["subtype"], "success");
        let inner = &response["response"];
        assert_eq!(inner["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        let ctx = inner["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("PURPLE_PINEAPPLE"), "ctx={ctx}");
        assert!(ctx.contains("mid-turn"), "ctx={ctx}");
        assert!(inner.get("decision").is_none());
        assert!(inner.get("reason").is_none());
    }

    #[test]
    fn hook_response_stop_blocks_with_reason() {
        let response = build_hook_response("hk2", "stop_inj", Some("FOLLOWUP".to_string()));
        let inner = &response["response"];
        assert_eq!(inner["hookSpecificOutput"]["hookEventName"], "Stop");
        assert_eq!(inner["decision"], "block");
        let reason = inner["reason"].as_str().unwrap();
        assert!(reason.contains("FOLLOWUP"), "reason={reason}");
    }

    #[test]
    fn hook_response_wraps_pulled_text_as_additional_context() {
        let resp = build_hook_response("req-1", HOOK_CALLBACK_POST_TOOL_USE, Some("PULLED".into()));
        let ctx = resp["response"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("PULLED"));
    }

    #[test]
    fn set_store_pull_registers_and_is_first_write_wins() {
        let connection = ClaudeNativeConnection {
            agent_id: AgentId::new("test"),
            binary: PathBuf::from("claude"),
            extra_env: Vec::new(),
            sessions: RefCell::new(HashMap::new()),
            desired_models: RefCell::new(HashMap::new()),
            desired_efforts: RefCell::new(HashMap::new()),
            escalation_timeout: Cell::new(DEFAULT_ESCALATION_TIMEOUT),
            silence_window: Cell::new(DEFAULT_SILENCE_WINDOW),
            self_handle: RefCell::new(std::rc::Weak::new()),
            escalations_armed: Cell::new(0),
            store_pull: std::rc::Rc::new(std::cell::RefCell::new(None)),
        };
        assert!(!connection.store_pull_registered_for_test());

        // A guard captured by the FIRST closure flips `first_dropped` on drop.
        // If a later `set_store_pull` replaced the first closure, that closure
        // (and its guard) would be dropped — so `first_dropped` staying false
        // proves first-registration-wins.
        struct DropFlag(std::rc::Rc<Cell<bool>>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let first_dropped = std::rc::Rc::new(Cell::new(false));
        let guard = DropFlag(first_dropped.clone());
        connection.set_store_pull(std::rc::Rc::new(move |_id, _agent_id, _eot, _cx| {
            let _ = &guard;
            Some("FIRST".to_string())
        }));
        assert!(connection.store_pull_registered_for_test());

        // Second registration must be a no-op: the first closure stays retained.
        connection.set_store_pull(std::rc::Rc::new(|_id, _agent_id, _eot, _cx| {
            Some("SECOND".to_string())
        }));
        assert!(connection.store_pull_registered_for_test());
        assert!(
            !first_dropped.get(),
            "first registration must survive a second set_store_pull"
        );
    }

    #[test]
    fn parse_available_commands_extracts_name_description_and_filters_unsupported() {
        let payload = serde_json::json!({
            "commands": [
                {"name": "context", "description": "Show context usage"},
                {"name": "compact", "description": "Compact context"},
                {"name": "login"},
                {"name": "cost", "description": "Show cost"},
                {"name": "agents", "description": "List agents", "argumentHint": "[name]"},
                {"name": "skill", "argumentHint": ["one", "two"]},
                {"description": "missing name — must drop"},
                {"name": "todos", "description": "filtered"},
            ],
        });
        let commands = parse_available_commands(&payload);
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["context", "compact", "agents", "skill"]);
        let agents = commands.iter().find(|c| c.name == "agents").unwrap();
        match &agents.input {
            Some(acp::AvailableCommandInput::Unstructured(input)) => {
                assert_eq!(input.hint, "[name]");
            }
            other => panic!("expected unstructured input, got {other:?}"),
        }
        let skill = commands.iter().find(|c| c.name == "skill").unwrap();
        match &skill.input {
            Some(acp::AvailableCommandInput::Unstructured(input)) => {
                assert_eq!(input.hint, "one two");
            }
            other => panic!("expected unstructured input for skill, got {other:?}"),
        }
        // Missing description on `skill` becomes empty — never None.
        assert_eq!(skill.description, "");
    }

    #[test]
    fn parse_available_commands_handles_missing_or_malformed_array() {
        assert!(parse_available_commands(&serde_json::json!({})).is_empty());
        assert!(
            parse_available_commands(&serde_json::json!({ "commands": "not-an-array" })).is_empty()
        );
        // A single non-object entry must not blank out the rest.
        let payload = serde_json::json!({
            "commands": ["garbage", {"name": "context", "description": "ok"}],
        });
        let commands = parse_available_commands(&payload);
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["context"]);
    }

    #[test]
    fn parse_available_models_extracts_value_displayname_description() {
        let payload = serde_json::json!({
            "models": [
                {"value": "opus", "displayName": "Opus 4.8", "description": "Most capable"},
                {"value": "sonnet", "displayName": "Sonnet 4.6", "description": ""},
                {"displayName": "no value — skipped"},
            ]
        });
        let models = parse_available_models(&payload);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].value, "opus");
        assert_eq!(models[0].display_name, "Opus 4.8");
        assert_eq!(models[0].description, "Most capable");
        assert_eq!(models[1].display_name, "Sonnet 4.6");
    }

    #[test]
    fn parse_available_models_handles_missing_array() {
        assert!(parse_available_models(&serde_json::json!({})).is_empty());
        assert!(parse_available_models(&serde_json::json!({"models": 5})).is_empty());
    }
}
