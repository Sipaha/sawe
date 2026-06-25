//! Pure translation of parsed `claude` stream-json messages into the
//! `acp::SessionUpdate` values `AcpThread::handle_session_update` already
//! consumes, plus turn-end classification. No GPUI / I/O.

use std::cell::Cell;

use agent_client_protocol::schema as acp;

use crate::protocol::{ConversationMessage, OutputMessage, ResultMessage, StreamEvent, Usage};

/// How a turn ended, derived from the `result` message.
#[derive(Debug, PartialEq)]
pub enum TurnEnd {
    Stop(acp::StopReason),
    Error(String),
}

/// Top-level `_meta` key for Claude Code-specific extensions on ACP updates.
/// Mirrors the `claudeCode.*` namespace claude itself stamps on its own
/// stream-json output; downstream consumers read this object to recover info
/// the ACP schema doesn't model (today: the parent tool_use id of a subagent
/// emission).
pub const CLAUDE_CODE_META_KEY: &str = "claudeCode";
/// Nested key under [`CLAUDE_CODE_META_KEY`] that carries the parent
/// `tool_use_id` for any SessionUpdate produced from a subagent
/// (`OutputMessage::parent_tool_use_id.is_some()`) message. Etap 2 reads this
/// to attach a `subagent_id` to AssistantMessage / ToolCall entries.
pub const PARENT_TOOL_USE_ID_META_KEY: &str = "parentToolUseId";

/// Stamp `_meta.claudeCode.parentToolUseId = <id>` onto the SessionUpdate
/// variants the subagent emit path actually produces (chunks / tool call /
/// tool-call update / plan). Other variants are left untouched: a subagent
/// can't legitimately emit an `AvailableCommandsUpdate`,
/// `CurrentModeUpdate`, `ConfigOptionUpdate`, `SessionInfoUpdate`, or
/// `UsageUpdate` (`apply_stream_usage` short-circuits when
/// `parent_tool_use_id.is_some()`).
///
/// Merges into any existing meta the variant might already carry rather than
/// replacing — the helper is composable so a future stamp on the same update
/// won't silently overwrite ours.
pub fn stamp_subagent_meta(update: &mut acp::SessionUpdate, parent_tool_use_id: &str) {
    let meta = match update {
        acp::SessionUpdate::UserMessageChunk(c)
        | acp::SessionUpdate::AgentMessageChunk(c)
        | acp::SessionUpdate::AgentThoughtChunk(c) => &mut c.meta,
        acp::SessionUpdate::ToolCall(t) => &mut t.meta,
        acp::SessionUpdate::ToolCallUpdate(t) => &mut t.meta,
        acp::SessionUpdate::Plan(p) => &mut p.meta,
        _ => return,
    };
    let outer = meta.get_or_insert_with(serde_json::Map::new);
    let nested = outer
        .entry(CLAUDE_CODE_META_KEY.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let serde_json::Value::Object(map) = nested {
        map.insert(
            PARENT_TOOL_USE_ID_META_KEY.to_string(),
            serde_json::Value::String(parent_tool_use_id.to_string()),
        );
    }
}

/// Translate one output message into zero or more `SessionUpdate`s.
///
/// Subagent output (`parent_tool_use_id.is_some()`) is passed through: tool_use
/// blocks surface as `ToolCall` entries so the user can see the subagent's work;
/// usage gating still keeps subagent token counts out of the parent meter.
pub fn translate(msg: &OutputMessage) -> Vec<acp::SessionUpdate> {
    match msg {
        OutputMessage::StreamEvent(ev) => translate_stream_event(ev),
        // Subagent messages (`parent_tool_use_id.is_some()`) are kept: their
        // text streams via stream_event deltas at the top level already, and
        // their tool_use / tool_result blocks need to be visible too so the
        // user can see the work claude's Task subagent did, not just the
        // final summary that comes back in the parent Task tool_result. The
        // token-meter side of the pipeline still gates on
        // `parent_tool_use_id.is_none()` (in `apply_stream_usage` and the
        // `assistant_usage_update` call site) so subagent usage doesn't
        // pollute the parent session's context-window readout.
        OutputMessage::Assistant(m) => translate_assistant(m),
        OutputMessage::User(m) => translate_user(m),
        _ => Vec::new(),
    }
}

fn translate_stream_event(ev: &StreamEvent) -> Vec<acp::SessionUpdate> {
    if ev.event.get("type").and_then(|t| t.as_str()) != Some("content_block_delta") {
        return Vec::new();
    }
    let delta = match ev.event.get("delta") {
        Some(d) => d,
        None => return Vec::new(),
    };
    match delta.get("type").and_then(|t| t.as_str()) {
        Some("text_delta") => delta
            .get("text")
            .and_then(|t| t.as_str())
            .map(|text| {
                vec![acp::SessionUpdate::AgentMessageChunk(
                    acp::ContentChunk::new(text_block(text)),
                )]
            })
            .unwrap_or_default(),
        Some("thinking_delta") => delta
            .get("thinking")
            .and_then(|t| t.as_str())
            .map(|text| {
                vec![acp::SessionUpdate::AgentThoughtChunk(
                    acp::ContentChunk::new(text_block(text)),
                )]
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn translate_assistant(m: &ConversationMessage) -> Vec<acp::SessionUpdate> {
    content_blocks(&m.message)
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                // Text arrives via stream_event deltas; skip assistant text here.
                return None;
            }
            let id = block.get("id").and_then(|v| v.as_str())?;
            let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
            // Map Claude Code's tool name to the ACP ToolKind so the desktop
            // UI picks the right widget (Edit shows diff, Read shows file
            // preview, Bash shows terminal output, etc.). Default Other
            // would render a bare "Tool: Name (done)" header with no body.
            let kind = tool_kind_for(name);
            // Stamp the programmatic tool name into `_meta.tool_name` so
            // downstream consumers (solution_agent's subagent-tabs lifecycle,
            // session_view rendering) can discriminate by name without parsing
            // the user-facing title. The ACP schema has no dedicated `name`
            // field on `ToolCall`; the convention lives in `acp_thread::TOOL_NAME_META_KEY`.
            let mut call = acp::ToolCall::new(acp::ToolCallId::new(id), name.to_string())
                .kind(kind)
                .status(acp::ToolCallStatus::InProgress)
                .meta(Some(acp_thread::meta_with_tool_name(name)));
            if let Some(input) = block.get("input") {
                // For file-based tools, also surface the path as a
                // ToolCallLocation so the UI can render a clickable file
                // link header.
                let locs = tool_locations_from_input(name, input);
                if !locs.is_empty() {
                    call = call.locations(locs);
                }
                call = call.raw_input(input.clone());
            }
            Some(acp::SessionUpdate::ToolCall(call))
        })
        .collect()
}

/// Map a Claude Code tool name to the ACP `ToolKind` so the desktop UI
/// picks the right rendering widget. Names not recognised fall through
/// to `Other` (the default), which is the safe rendering. Mirrors the
/// JS wrapper's `toolInfoFromToolUse` (`tools.js:19–276`) so plan-mode
/// agent tools (`Task`, `Agent`, `TaskCreate`/`Update`/`List`/`Get`) and
/// the `ExitPlanMode` switch don't fall back to a generic "Tool: X"
/// header.
fn tool_kind_for(name: &str) -> acp::ToolKind {
    match name {
        "Read" | "NotebookRead" => acp::ToolKind::Read,
        "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => acp::ToolKind::Edit,
        "Bash" | "BashOutput" | "KillShell" => acp::ToolKind::Execute,
        "Glob" | "Grep" => acp::ToolKind::Search,
        "WebFetch" | "WebSearch" => acp::ToolKind::Fetch,
        "TodoWrite" | "Think" | "Task" | "Agent" | "TaskCreate" | "TaskUpdate" | "TaskList"
        | "TaskGet" => acp::ToolKind::Think,
        "ExitPlanMode" => acp::ToolKind::SwitchMode,
        _ => acp::ToolKind::Other,
    }
}

/// Extract file paths from a Claude Code tool's `input` JSON as ACP
/// `ToolCallLocation`s so the desktop UI can render clickable file
/// links above the tool body. Empty vec when no path field is found
/// or the tool is non-file (Bash, WebFetch, etc.).
fn tool_locations_from_input(name: &str, input: &serde_json::Value) -> Vec<acp::ToolCallLocation> {
    let path_field = match name {
        "Read" | "Edit" | "Write" | "NotebookRead" | "NotebookEdit" | "MultiEdit" => "file_path",
        "Glob" => "path",
        _ => return Vec::new(),
    };
    input
        .get(path_field)
        .and_then(|v| v.as_str())
        .map(|p| vec![acp::ToolCallLocation::new(std::path::PathBuf::from(p))])
        .unwrap_or_default()
}

fn translate_user(m: &ConversationMessage) -> Vec<acp::SessionUpdate> {
    content_blocks(&m.message)
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                return None;
            }
            let id = block.get("tool_use_id").and_then(|v| v.as_str())?;
            let is_error = block
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let content = tool_result_content(block.get("content"), is_error);
            let status = if is_error {
                acp::ToolCallStatus::Failed
            } else {
                acp::ToolCallStatus::Completed
            };
            let fields = acp::ToolCallUpdateFields::new()
                .status(status)
                .content(content);
            Some(acp::SessionUpdate::ToolCallUpdate(
                acp::ToolCallUpdate::new(acp::ToolCallId::new(id), fields),
            ))
        })
        .collect()
}

/// Classify a `result` message as a turn-end stop reason or an error.
pub fn classify_result(r: &ResultMessage) -> TurnEnd {
    if r.is_error {
        // Prefer the structured `errors` array (when present and non-empty),
        // fall back to `result.result` (claude often puts a human-readable
        // explanation here for `error_during_execution` and similar cases
        // where `errors` is empty), and only finally fall back to the
        // synthetic "agent error (subtype)" string. Without the
        // `result.result` fallback the user sees a generic red banner with
        // no detail even when claude described exactly what went wrong.
        let msg = if !r.errors.is_empty() {
            r.errors.join("; ")
        } else if let Some(detail) = r.result.as_deref().filter(|s| !s.trim().is_empty()) {
            detail.trim().to_string()
        } else {
            format!("agent error ({})", r.subtype)
        };
        // Dump the full result so we can post-mortem mystery error_during_
        // execution / "agent error (success)" cases — without this the
        // error string alone has zero diagnostic value (the user just sees
        // a red banner with no clue what claude tried to say). Logged at
        // warn level so it shows up in sawe.log next to the wire spam.
        log::warn!(
            target: "claude_native::result",
            "is_error result classified to TurnEnd::Error: subtype={subtype:?} stop_reason={stop_reason:?} errors={errors:?} usage={usage:?} result={result:?}",
            subtype = r.subtype,
            stop_reason = r.stop_reason,
            errors = r.errors,
            usage = r.usage,
            result = r.result,
        );
        return TurnEnd::Error(msg);
    }
    let stop = match r.stop_reason.as_deref() {
        Some("max_tokens") => acp::StopReason::MaxTokens,
        Some("refusal") => acp::StopReason::Refusal,
        Some("cancelled") | Some("canceled") => acp::StopReason::Cancelled,
        // "end_turn", "tool_use", null, or anything else → a normal end.
        _ => acp::StopReason::EndTurn,
    };
    TurnEnd::Stop(stop)
}

/// Tokens occupying the model's context window on the most recent API call
/// that produced this `Usage` block. Mirrors Claude Code's own formula
/// (`cli.js` `wS1`): `input + cache_creation + cache_read`, deliberately
/// without `output_tokens`. The output of the current call only becomes
/// context on the NEXT call; counting it here doublecounts on the very
/// next message and inflates the meter past 100% on max-output turns.
fn context_tokens(u: &Usage) -> u64 {
    u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens
}

/// Build a `UsageUpdate` from a `result`. The window is the model's advertised
/// `contextWindow`, falling back to the last-known `sticky_window` so the meter
/// limit never regresses (the 200k/1M flicker fix). Returns `None` when neither
/// a window nor a used-token count is available. When the result carries a
/// `total_cost_usd`, attach it as a cumulative `Cost` so the UI can show
/// running spend (JS `acp-agent.js:631-634`).
///
/// `latest_used` is the most recent per-message context-token reading the
/// caller observed during this turn (from `apply_stream_usage` /
/// `assistant_usage_update`). When present, that value is preferred over
/// `result.usage` for the meter — `result.usage` is the SDK's SUM across
/// all sub-calls in a multi-step turn, so a turn with three sub-calls each
/// cache-reading ~900 k of context aggregates to ~2.7 M and pushes the
/// meter above the actual context window (the observed 1.8 M / 1.0 M bug).
/// The per-message cumulative is bounded by the window and reflects real
/// post-turn occupancy; the cumulative `result.usage` does not. Falls back
/// to `result.usage` only when no stream/message usage was seen this turn
/// (e.g. a degenerate turn that emits only a terminal `result`).
pub fn usage_update(
    r: &ResultMessage,
    sticky_window: Option<u64>,
    latest_used: Option<u64>,
) -> Option<acp::SessionUpdate> {
    let window = real_window(r).or(sticky_window);
    // Only consider `result.usage` as a measurement if it carries a non-zero
    // context-token total. Local slash commands (`/context`, `/cost`,
    // `/agents` …) bypass the model entirely — they emit a terminal
    // `result` with `usage: {0,0,0,0}` and no stream events, so both the
    // per-message and per-result paths report zero. Emitting
    // `UsageUpdate(used=0, window=W)` then overwrites the meter to 0 / 1M
    // (and the desktop's `smooth_used_tokens` ratchet-down rule —
    // `raw_used <= peak/10` — collapses the cached peak too, since `0`
    // trivially satisfies the threshold). Returning `None` here leaves the
    // last real measurement in place; a context reset (`/clear` /
    // `/compact`) goes through `reset_context` / `rotate_context` which
    // mints a fresh `AcpThread`, so the zero-after-reset case is handled
    // structurally, not through this path.
    let used = latest_used
        .or_else(|| r.usage.as_ref().map(context_tokens))
        .filter(|&u| u > 0);
    let cost = r
        .total_cost_usd
        .filter(|amount| amount.is_finite() && *amount > 0.0)
        .map(|amount| acp::Cost::new(amount, "USD"));
    // A turn that produced no usable used-tokens reading means we have
    // nothing new to say about context occupancy. Returning `None` keeps
    // the AcpThread's prior `token_usage` (and the status row's cached
    // peak) untouched. Emitting `UsageUpdate(0, …)` would clobber both
    // ("0 / 1.0M · 0.0%" after a local command). Window-tracking happens
    // separately via `apply_usage`'s `sticky_window.set` side effect, so
    // the meter limit still advances when a fresh result advertises one.
    used?;
    let update = acp::UsageUpdate::new(used.unwrap_or(0), window.unwrap_or(0)).cost(cost);
    Some(acp::SessionUpdate::UsageUpdate(update))
}

/// Hardcoded fallback Claude window — matches `DEFAULT_CONTEXT_WINDOW` in
/// `solution_agent::status_row` and the JS wrapper's literal 200 000 in
/// `acp-agent.js:29`. Used as the meter limit when neither the result's
/// `modelUsage.contextWindow` nor the model-name heuristic gives us a value.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// Infer a context-window size from a Claude model id. JS only bumps to 1 M
/// when the id contains `1m` (case-insensitive, word-bounded): the public
/// `claude-sonnet-4-5-1m` / `claude-opus-4-8-1m` variants opt-in to the
/// 1 M-token window; everything else uses the 200 k default. Returns `None`
/// when the heuristic doesn't fire so callers can fall back to the global
/// default. Mirrors `acp-agent.js::inferContextWindowFromModel:2547`.
pub fn infer_context_window_from_model(model: &str) -> Option<u64> {
    let bytes = model.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        let c1 = bytes[i];
        let c2 = bytes[i + 1];
        let is_one = c1 == b'1';
        let is_m = c2 == b'm' || c2 == b'M';
        if !(is_one && is_m) {
            continue;
        }
        let left_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let right_ok = i + 2 >= bytes.len() || !bytes[i + 2].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return Some(1_000_000);
        }
    }
    None
}

/// Merge an Anthropic-API `usage` JSON value emitted by a `message_delta`
/// stream event into a running cumulative snapshot. `message_delta.usage`
/// fields are *cumulative replacements*, not additive (per Anthropic API
/// docs and confirmed by JS comment `acp-agent.js:727–736`): when a field
/// is present it supersedes the prior snapshot; when omitted (`null`) the
/// prior value carries over. Only `output_tokens` is guaranteed non-null
/// per JS, but defensively all four are nullable here. Returns the new
/// snapshot or `None` when both the prior snapshot and the delta are
/// empty (no usage seen yet).
pub fn merge_stream_usage(
    prev: Option<&Usage>,
    delta: Option<&serde_json::Value>,
) -> Option<Usage> {
    let delta = delta?;
    let pick = |field: &str, fallback: u64| -> u64 {
        delta
            .get(field)
            .and_then(|v| v.as_u64())
            .unwrap_or(fallback)
    };
    let base = prev.cloned().unwrap_or_default();
    Some(Usage {
        input_tokens: pick("input_tokens", base.input_tokens),
        output_tokens: pick("output_tokens", base.output_tokens),
        cache_read_input_tokens: pick("cache_read_input_tokens", base.cache_read_input_tokens),
        cache_creation_input_tokens: pick(
            "cache_creation_input_tokens",
            base.cache_creation_input_tokens,
        ),
    })
}

/// Snapshot a `message_start` stream event's `event.message.usage` block
/// into our `Usage` type. Returns `None` when the event is shaped
/// unexpectedly (no `message.usage`); callers then leave the prior
/// snapshot in place. Mirrors JS `snapshotFromUsage` semantics — missing
/// fields default to 0 because `message_start` is the *initial* snapshot
/// (no prior to fall back to).
pub fn snapshot_message_start_usage(event: &serde_json::Value) -> Option<Usage> {
    let usage = event.get("message").and_then(|m| m.get("usage"))?;
    serde_json::from_value(usage.clone()).ok()
}

/// Extract `message.model` from a `message_start` stream event. JS uses
/// this to drive the per-session model-name latch (`acp-agent.js:708-722`),
/// which then feeds `infer_context_window_from_model` as a window
/// fallback. Skips the synthetic `<synthetic>` model (placeholder claude
/// uses on locally-handled slash commands — never a real budget).
pub fn extract_message_start_model(event: &serde_json::Value) -> Option<String> {
    let model = event
        .get("message")
        .and_then(|m| m.get("model"))
        .and_then(|v| v.as_str())?;
    if model == "<synthetic>" {
        None
    } else {
        Some(model.to_string())
    }
}

/// Output of [`apply_stream_usage`]: the new cumulative usage snapshot (to
/// store back in `SessionShared::stream_usage`) and an optional ACP update
/// to forward to the thread. The update is `None` when the cumulative
/// total didn't change — JS suppresses the redundant emission too
/// (`acp-agent.js:739-749`) to avoid spamming the meter on every delta.
pub struct StreamUsageOutcome {
    pub new_usage: Option<Usage>,
    pub update: Option<acp::SessionUpdate>,
    /// Latched model id, if a `message_start` carried one. Caller stores
    /// this in `SessionShared::active_model`.
    pub new_model: Option<String>,
}

/// Process a `stream_event` for usage / model tracking. Mirrors the JS
/// block at `acp-agent.js:703-749`:
///
/// - `message_start` → snapshot `event.message.usage` as the running
///   cumulative; latch `event.message.model` for window inference.
/// - `message_delta` → merge `event.usage` into the running cumulative
///   (cumulative replacement semantics — see [`merge_stream_usage`]).
///
/// Returns the new snapshot, the model latch (if any), and an
/// [`acp::SessionUpdate`] only when the cumulative total changed since
/// the last emission. Other stream event types yield an all-`None`
/// outcome.
///
/// `prev_usage` is the prior cumulative snapshot for this turn (cleared
/// on `result` by the caller). `prev_emitted_total` is the last
/// `used_tokens` we sent to the meter — used to dedupe redundant
/// updates. `sticky_window` is the current per-session window limit.
pub fn apply_stream_usage(
    ev: &StreamEvent,
    prev_usage: Option<&Usage>,
    prev_emitted_total: Option<u64>,
    sticky_window: Option<u64>,
) -> StreamUsageOutcome {
    if ev.parent_tool_use_id.is_some() {
        return StreamUsageOutcome {
            new_usage: None,
            update: None,
            new_model: None,
        };
    }
    let event_type = ev.event.get("type").and_then(|t| t.as_str());
    let (new_usage, new_model) = match event_type {
        Some("message_start") => (
            snapshot_message_start_usage(&ev.event),
            extract_message_start_model(&ev.event),
        ),
        Some("message_delta") => (merge_stream_usage(prev_usage, ev.event.get("usage")), None),
        _ => {
            return StreamUsageOutcome {
                new_usage: None,
                update: None,
                new_model: None,
            };
        }
    };
    let update = new_usage.as_ref().and_then(|usage| {
        let total = context_tokens(usage);
        if Some(total) == prev_emitted_total {
            return None;
        }
        Some(acp::SessionUpdate::UsageUpdate(acp::UsageUpdate::new(
            total,
            sticky_window.unwrap_or(0),
        )))
    });
    StreamUsageOutcome {
        new_usage,
        update,
        new_model,
    }
}

/// Build a `UsageUpdate` from an assistant message's inline `usage` block —
/// the per-API-call counter that drives Claude Code's own meter. Within a
/// multi-step turn (assistant → tool_use → tool_result → assistant) there
/// are multiple assistant messages, each with its own `usage`; the one with
/// the largest `cache_read_input_tokens` reflects everything the model
/// actually saw, while the SDK's terminal `result` may report only the last
/// sub-call's small numbers. Returns `None` when the message carries no
/// usage block, or when the block has no input-side tokens at all (a
/// streaming pre-final fragment whose usage has not been settled yet).
///
/// `sticky_window` is the per-session context-window cell maintained by the
/// connection; we don't mutate it from here because an assistant message
/// doesn't advertise a window — only `result` does. The cell carries the
/// most recently seen `contextWindow` so the meter limit stays put.
pub fn assistant_usage_update(
    m: &ConversationMessage,
    sticky_window: Option<u64>,
) -> Option<acp::SessionUpdate> {
    let usage = m.usage()?;
    let used = context_tokens(&usage);
    if used == 0 {
        return None;
    }
    Some(acp::SessionUpdate::UsageUpdate(acp::UsageUpdate::new(
        used,
        sticky_window.unwrap_or(0),
    )))
}

/// Build the `UsageUpdate` for a `result` against a session's sticky-window
/// cell, then advance the cell: a freshly-observed real (non-zero) window
/// becomes the new sticky baseline, while a missing or zero window leaves the
/// prior baseline untouched. This is the stateful counterpart to
/// [`usage_update`] — the update-pump calls it once per `result` so a later
/// result that omits (or zeroes) `contextWindow` never downgrades the meter
/// limit (the 200k/1M flicker fix).
pub fn apply_usage(
    r: &ResultMessage,
    sticky_window: &Cell<Option<u64>>,
    latest_used: Option<u64>,
) -> Option<acp::SessionUpdate> {
    let update = usage_update(r, sticky_window.get(), latest_used);
    if let Some(window) = real_window(r) {
        sticky_window.set(Some(window));
    }
    update
}

/// The model's advertised context window, but only when it is a real (non-zero)
/// value. `claude` occasionally reports `contextWindow: 0` (or omits it); a 0 is
/// not a meaningful limit, so it is treated identically to "missing" — the
/// sticky fallback applies instead of overwriting a known window with 0.
fn real_window(r: &ResultMessage) -> Option<u64> {
    r.context_window_for_active_model()
        .filter(|window| *window > 0)
}

fn text_block(text: &str) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(text.to_string()))
}

fn content_blocks(message: &serde_json::Value) -> Vec<serde_json::Value> {
    message
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Translate a `tool_result` block's `content` (string | array | object) into
/// ACP `ToolCallContent`s, preserving structured payloads the JS wrapper
/// also handled (`tools.js:308–508`):
///
/// - `text` blocks → `ContentBlock::Text` (wrapped in a fenced block if the
///   tool reported an error, mirroring JS's error formatting).
/// - `image` blocks with a `base64` source → `ContentBlock::Image` so the UI
///   can render screenshots from Read / WebFetch results instead of dumping
///   raw JSON text.
/// - `bash_code_execution_result` (Anthropic's structured Bash output) →
///   formatted as a `\`\`\`console …` block with `stdout`, `stderr`, and the
///   `return_code` surfaced as a final `exit: N` line. The JS wrapper routes
///   this via `_meta.terminal_exit` for clients that support terminals; we
///   take the JS fallback path until terminals are wired through to ACP.
/// - Anthropic helper blocks (`web_search_result`, `web_fetch_result`,
///   `code_execution_result`, etc.) → compact one-line text summaries so
///   they don't appear as opaque JSON.
/// - Anything unrecognised → JSON-stringified text (the JS default).
fn tool_result_content(
    content: Option<&serde_json::Value>,
    is_error: bool,
) -> Vec<acp::ToolCallContent> {
    let blocks = match content {
        Some(serde_json::Value::String(s)) if !s.is_empty() => {
            vec![text_block(&wrap_error_text(s, is_error))]
        }
        Some(serde_json::Value::String(_)) | None => Vec::new(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| anthropic_block_to_acp(item, is_error))
            .collect(),
        Some(other @ serde_json::Value::Object(_)) => {
            vec![anthropic_block_to_acp(other, is_error)]
        }
        Some(other) => vec![text_block(&other.to_string())],
    };
    blocks.into_iter().map(acp::ToolCallContent::from).collect()
}

/// Translate one Anthropic content-block JSON value into an ACP
/// `ContentBlock`. Mirrors JS `toAcpContentBlock` (`tools.js:453–507`); see
/// [`tool_result_content`] for the per-`type` rationale.
fn anthropic_block_to_acp(block: &serde_json::Value, is_error: bool) -> acp::ContentBlock {
    let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "text" => {
            let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
            text_block(&wrap_error_text(text, is_error))
        }
        "image" => image_block_from_anthropic(block).unwrap_or_else(|| text_block("[image]")),
        "bash_code_execution_result" => text_block(&format_bash_result(block)),
        "code_execution_result" => {
            let stdout = block.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
            let stderr = block.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
            let output = if !stdout.is_empty() { stdout } else { stderr };
            text_block(&format!("Output: {output}"))
        }
        "web_fetch_result" => {
            let url = block.get("url").and_then(|v| v.as_str()).unwrap_or("");
            text_block(&format!("Fetched: {url}"))
        }
        "web_search_result" => {
            let title = block.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = block.get("url").and_then(|v| v.as_str()).unwrap_or("");
            text_block(&format!("{title} ({url})"))
        }
        // Error variants — the SDK reports them as their own block types
        // alongside (or instead of) the success result. Surface the error
        // code so the UI can show "Error: rate_limited" instead of raw JSON.
        "bash_code_execution_tool_result_error"
        | "code_execution_tool_result_error"
        | "web_fetch_tool_result_error"
        | "web_search_tool_result_error" => {
            let code = block
                .get("error_code")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            text_block(&format!("Error: {code}"))
        }
        _ => text_block(&block.to_string()),
    }
}

/// Build an ACP `ImageContent` from an Anthropic `image` block. Only
/// `source.type == "base64"` carries inline bytes ACP can render; URL or
/// file-reference sources have no data field so we fall back to a textual
/// placeholder (mirrors JS `tools.js:464–475`). Exposed `pub(crate)` so the
/// pump's assistant-message handler can reuse the same shape when claude
/// emits an image as a top-level assistant content block (rare but real).
pub(crate) fn image_block_from_anthropic(block: &serde_json::Value) -> Option<acp::ContentBlock> {
    let source = block.get("source")?;
    let source_type = source.get("type").and_then(|v| v.as_str());
    match source_type {
        Some("base64") => {
            let data = source.get("data").and_then(|v| v.as_str()).unwrap_or("");
            let mime = source
                .get("media_type")
                .and_then(|v| v.as_str())
                .unwrap_or("image/png");
            Some(acp::ContentBlock::Image(acp::ImageContent::new(
                data.to_string(),
                mime.to_string(),
            )))
        }
        Some("url") => {
            let url = source.get("url").and_then(|v| v.as_str()).unwrap_or("");
            Some(text_block(&format!("[image: {url}]")))
        }
        _ => Some(text_block("[image: file reference]")),
    }
}

/// Format an Anthropic `bash_code_execution_result` block as a fenced
/// console code block followed by the exit code. JS's preferred path uses
/// `_meta.terminal_exit` so the client can render a real terminal pane; we
/// don't have a terminal capability wired through ACP yet, so we fall back
/// to JS's text path (`tools.js:391–404`) plus an explicit `exit: N` line
/// so the user can see non-zero codes that would otherwise be invisible.
fn format_bash_result(block: &serde_json::Value) -> String {
    let stdout = block.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
    let stderr = block.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
    let return_code = block
        .get("return_code")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let mut output = String::new();
    if !stdout.is_empty() {
        output.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(stderr);
    }
    let trimmed = output.trim_end();
    if trimmed.is_empty() {
        format!("exit: {return_code}")
    } else {
        format!("```console\n{trimmed}\n```\nexit: {return_code}")
    }
}

/// Wrap raw text in a fenced code block when the originating tool result
/// is_error=true. JS does this to make untrusted command output (which the
/// model might confuse with prose) visually distinct in the chat (`tools.js`
/// `wrapText`, lines 454–457).
fn wrap_error_text(text: &str, is_error: bool) -> String {
    let stripped = strip_local_command_metadata(text);
    if is_error {
        format!("```\n{stripped}\n```")
    } else {
        stripped
    }
}

/// Strip Claude Code's local-slash-command marker tags
/// (`<command-name>`, `<command-message>`, `<command-args>`,
/// `<local-command-stdout>`, `<local-command-stderr>`) from a text
/// payload. The SDK persists local command invocations (e.g. `/model`,
/// `/help`) and their output as user messages wrapped in these markers
/// for the CLI's own display. The live prompt loop drops them, but on
/// session replay (`--resume`) they arrive through the normal stream
/// and would leak into the chat if surfaced verbatim. Mirrors JS
/// `stripLocalCommandMetadata` (`acp-agent.js:95–135`).
///
/// Trims trailing whitespace introduced by the removal but preserves
/// real prose mixed in alongside the markers (e.g.
/// `<command-name>/model</command-name>switch to opus` becomes
/// `switch to opus`).
pub fn strip_local_command_metadata(text: &str) -> String {
    const TAGS: [&str; 5] = [
        "command-name",
        "command-message",
        "command-args",
        "local-command-stdout",
        "local-command-stderr",
    ];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'outer: while !rest.is_empty() {
        // Look for the next `<tag>` opener that matches one of our markers.
        if let Some(lt) = rest.find('<') {
            // Copy anything before the candidate tag.
            out.push_str(&rest[..lt]);
            let tail = &rest[lt + 1..];
            for tag in TAGS {
                if tail.len() > tag.len()
                    && tail.starts_with(tag)
                    && matches!(tail.as_bytes().get(tag.len()), Some(b'>') | Some(b' '))
                {
                    // Find matching `</tag>` closer.
                    let close = format!("</{tag}>");
                    if let Some(end) = tail.find(&close) {
                        rest = &tail[end + close.len()..];
                        continue 'outer;
                    }
                }
            }
            // Not a marker tag — keep the literal `<` and advance one char.
            out.push('<');
            rest = tail;
        } else {
            out.push_str(rest);
            break;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::OutputMessage;

    #[test]
    fn text_delta_becomes_agent_message_chunk() {
        let msg = OutputMessage::parse(r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}},"uuid":"u","session_id":"s"}"#).unwrap();
        let ups = translate(&msg);
        assert_eq!(ups.len(), 1);
        match &ups[0] {
            acp::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                acp::ContentBlock::Text(t) => assert_eq!(t.text, "Hi"),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn thinking_delta_becomes_thought_chunk() {
        let msg = OutputMessage::parse(r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}},"uuid":"u","session_id":"s"}"#).unwrap();
        let ups = translate(&msg);
        assert!(matches!(
            ups.as_slice(),
            [acp::SessionUpdate::AgentThoughtChunk(_)]
        ));
    }

    #[test]
    fn message_start_yields_nothing() {
        let msg = OutputMessage::parse(r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"m","usage":{}}},"uuid":"u","session_id":"s"}"#).unwrap();
        assert!(translate(&msg).is_empty());
    }

    #[test]
    fn assistant_tool_use_becomes_tool_call() {
        let msg = OutputMessage::parse(r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"text","text":"x"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]},"uuid":"u","session_id":"s"}"#).unwrap();
        let ups = translate(&msg);
        assert_eq!(ups.len(), 1, "text block skipped, one tool_use mapped");
        match &ups[0] {
            acp::SessionUpdate::ToolCall(call) => {
                assert_eq!(call.tool_call_id.0.as_ref(), "toolu_1");
                assert_eq!(call.title, "Bash");
                assert_eq!(call.status, acp::ToolCallStatus::InProgress);
                // tool_name meta is the only place the programmatic tool name
                // survives — the title goes through Markdown rendering and is
                // user-facing, so consumers must filter by meta. Pin this here
                // so future refactors of translate_assistant can't silently
                // strip the meta and break subagent-tab discrimination
                // (Task/Agent ↔ everything else).
                assert_eq!(
                    acp_thread::tool_name_from_meta(&call.meta).as_deref(),
                    Some("Bash"),
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn user_tool_result_becomes_tool_call_update() {
        let msg = OutputMessage::parse(r#"{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"file.txt"}]},"uuid":"u","session_id":"s"}"#).unwrap();
        let ups = translate(&msg);
        match ups.as_slice() {
            [acp::SessionUpdate::ToolCallUpdate(update)] => {
                assert_eq!(update.tool_call_id.0.as_ref(), "toolu_1");
                assert_eq!(update.fields.status, Some(acp::ToolCallStatus::Completed));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn subagent_assistant_surfaces_tool_use() {
        // Previously: subagent output (`parent_tool_use_id.is_some()`) was
        // collapsed at translate time so users could never see what the Task
        // subagent did, only its final result that came back through the
        // parent's tool_result. We now surface subagent tool_use blocks as
        // top-level ToolCalls so the work is visible in the timeline; the
        // usage meter still ignores subagent sub-calls so the parent
        // session's token reading stays correct.
        let msg = OutputMessage::parse(r#"{"type":"assistant","parent_tool_use_id":"toolu_parent","message":{"role":"assistant","content":[{"type":"tool_use","id":"x","name":"Read","input":{}}]},"uuid":"u","session_id":"s"}"#).unwrap();
        let updates = translate(&msg);
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            acp::SessionUpdate::ToolCall(call) => {
                assert_eq!(call.tool_call_id.0.as_ref(), "x");
                assert_eq!(call.title, "Read");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn classifies_success_end_turn() {
        let r = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","is_error":false,"stop_reason":"end_turn"}"#,
        )
        .unwrap();
        assert_eq!(classify_result(&r), TurnEnd::Stop(acp::StopReason::EndTurn));
    }

    #[test]
    fn classifies_max_tokens_and_cancelled() {
        let mt = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","stop_reason":"max_tokens"}"#,
        )
        .unwrap();
        assert_eq!(
            classify_result(&mt),
            TurnEnd::Stop(acp::StopReason::MaxTokens)
        );
        let c = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","stop_reason":"cancelled"}"#,
        )
        .unwrap();
        assert_eq!(
            classify_result(&c),
            TurnEnd::Stop(acp::StopReason::Cancelled)
        );
    }

    #[test]
    fn classifies_error() {
        let r = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"error_during_execution","is_error":true,"errors":["boom"]}"#,
        )
        .unwrap();
        match classify_result(&r) {
            TurnEnd::Error(e) => assert!(e.contains("boom")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn apply_usage_makes_window_sticky_across_results() {
        use std::cell::Cell;

        let sticky: Cell<Option<u64>> = Cell::new(None);

        // First result advertises a real window; the emitted update carries it
        // and the cell retains it.
        let first = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","modelUsage":{"m":{"contextWindow":1000000}},"usage":{"input_tokens":10,"output_tokens":5}}"#,
        )
        .unwrap();
        match apply_usage(&first, &sticky, None) {
            Some(acp::SessionUpdate::UsageUpdate(u)) => assert_eq!(u.size, 1_000_000),
            other => panic!("{other:?}"),
        }
        assert_eq!(sticky.get(), Some(1_000_000));

        // A later result with NO window must NOT downgrade the emitted size to 0
        // — the cell keeps the prior real window sticky.
        let second = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","usage":{"input_tokens":20,"output_tokens":7}}"#,
        )
        .unwrap();
        match apply_usage(&second, &sticky, None) {
            Some(acp::SessionUpdate::UsageUpdate(u)) => {
                assert_eq!(u.size, 1_000_000, "window stays sticky, never 0");
                // output_tokens (7) is excluded — only input-side counts toward
                // current context occupancy.
                assert_eq!(u.used, 20);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(sticky.get(), Some(1_000_000));

        // A still-later result with a zero window is treated as "no window" and
        // must also stay sticky rather than overwrite the cell with 0.
        let zero = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","modelUsage":{"m":{"contextWindow":0}},"usage":{"input_tokens":1,"output_tokens":1}}"#,
        )
        .unwrap();
        match apply_usage(&zero, &sticky, None) {
            Some(acp::SessionUpdate::UsageUpdate(u)) => assert_eq!(u.size, 1_000_000),
            other => panic!("{other:?}"),
        }
        assert_eq!(sticky.get(), Some(1_000_000));
    }

    #[test]
    fn usage_prefers_real_window_then_sticky() {
        let with_window = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","modelUsage":{"m":{"contextWindow":1000000}},"usage":{"input_tokens":10,"output_tokens":5}}"#,
        )
        .unwrap();
        match usage_update(&with_window, None, None) {
            Some(acp::SessionUpdate::UsageUpdate(u)) => {
                assert_eq!(u.size, 1_000_000);
                // output_tokens (5) is excluded — only input-side counts.
                assert_eq!(u.used, 10);
            }
            other => panic!("{other:?}"),
        }
        let no_window = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","usage":{"input_tokens":10,"output_tokens":5}}"#,
        )
        .unwrap();
        match usage_update(&no_window, Some(1_000_000), None) {
            Some(acp::SessionUpdate::UsageUpdate(u)) => assert_eq!(u.size, 1_000_000),
            other => panic!("{other:?}"),
        }
        let nothing = serde_json::from_str::<ResultMessage>(r#"{"subtype":"success"}"#).unwrap();
        assert!(usage_update(&nothing, None, None).is_none());
    }

    #[test]
    fn usage_counts_cache_tokens_without_output() {
        // The cached-prefix case: a deep session where the prompt cache
        // pre-loads ~200k tokens. cache_read makes up the bulk of context
        // occupancy; output_tokens (newly generated this turn) is NOT
        // context yet and must be excluded — including it inflates the
        // meter past 100 % on max-output turns.
        let r = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","modelUsage":{"m":{"contextWindow":1000000}},"usage":{"input_tokens":1500,"output_tokens":8000,"cache_read_input_tokens":210000,"cache_creation_input_tokens":3000}}"#,
        )
        .unwrap();
        match usage_update(&r, None, None) {
            Some(acp::SessionUpdate::UsageUpdate(u)) => {
                // 1500 + 210000 + 3000 = 214500 (output_tokens excluded)
                assert_eq!(u.used, 214_500);
                assert_eq!(u.size, 1_000_000);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn usage_update_skips_emit_when_no_real_measurement() {
        // Local slash commands (`/context`, `/cost`, `/agents` …) handle
        // their answer entirely in the SDK without calling the model, so
        // the terminal `result` carries `usage: {0,0,0,0}` and no stream
        // events fired during the turn. Emitting `UsageUpdate(used=0, …)`
        // there overwrites the meter to "0 / 1.0M · 0.0%" — observed bug
        // after invoking `/context`. The fix returns None when no real
        // used-tokens reading is available; the AcpThread's prior
        // token_usage is left in place.
        let r = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","modelUsage":{"m":{"contextWindow":1000000}},"usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#,
        )
        .unwrap();
        // No per-message reading either (no stream events on local
        // commands), so `latest_used = None`.
        assert!(usage_update(&r, None, None).is_none());
        // Same `result.usage` with a real per-message reading → emit (the
        // per-message path provides the actual occupancy snapshot).
        match usage_update(&r, None, Some(800_000)) {
            Some(acp::SessionUpdate::UsageUpdate(u)) => {
                assert_eq!(u.used, 800_000);
                assert_eq!(u.size, 1_000_000);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn usage_update_prefers_per_message_latest_over_aggregated_result() {
        // Regression for the 1.8M / 1.0M meter overflow. A multi-step turn
        // (three sub-calls each cache-reading ~900 k) makes `result.usage`
        // aggregate to ~2.7 M — well over the 1 M context window. The
        // per-message cumulative tracked during the turn (920 k) reflects
        // real occupancy. `usage_update` must prefer the latter so the
        // meter never reads above 100 % on multi-step turns.
        let r = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","modelUsage":{"m":{"contextWindow":1000000}},"usage":{"input_tokens":3000,"output_tokens":40000,"cache_read_input_tokens":2700000,"cache_creation_input_tokens":15000}}"#,
        )
        .unwrap();
        match usage_update(&r, None, Some(920_000)) {
            Some(acp::SessionUpdate::UsageUpdate(u)) => {
                assert_eq!(
                    u.used, 920_000,
                    "must use latest_used, not the inflated result.usage"
                );
                assert_eq!(u.size, 1_000_000);
            }
            other => panic!("{other:?}"),
        }
        // Fallback path: no per-message reading seen → fall back to
        // result.usage so degenerate turns (terminal result only, no
        // stream/message events) still report something.
        match usage_update(&r, None, None) {
            Some(acp::SessionUpdate::UsageUpdate(u)) => {
                // 3000 + 2700000 + 15000 = 2718000
                assert_eq!(u.used, 2_718_000);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn assistant_usage_update_extracts_inline_usage() {
        // Anthropic API attaches `usage` to the assistant message itself —
        // this is the per-API-call counter Claude Code's own meter drives
        // off (see cli.js `zD1`). On a cache-warm follow-up turn this is
        // ~all the context; the terminal `result` event may report just
        // the last sub-call's tiny numbers, so we MUST read from here too.
        let msg = OutputMessage::parse(
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[],"usage":{"input_tokens":12,"output_tokens":4,"cache_read_input_tokens":205000,"cache_creation_input_tokens":1500}},"uuid":"u","session_id":"s"}"#,
        )
        .unwrap();
        let conv = match msg {
            OutputMessage::Assistant(c) => c,
            other => panic!("{other:?}"),
        };
        match assistant_usage_update(&conv, Some(1_000_000)) {
            Some(acp::SessionUpdate::UsageUpdate(u)) => {
                // 12 + 205000 + 1500 = 206512 (output excluded)
                assert_eq!(u.used, 206_512);
                assert_eq!(u.size, 1_000_000);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn assistant_usage_update_skips_when_no_usage() {
        let msg = OutputMessage::parse(
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[]},"uuid":"u","session_id":"s"}"#,
        )
        .unwrap();
        let conv = match msg {
            OutputMessage::Assistant(c) => c,
            other => panic!("{other:?}"),
        };
        assert!(assistant_usage_update(&conv, Some(1_000_000)).is_none());
    }

    #[test]
    fn plan_mode_tools_get_think_or_switch_mode_kinds() {
        let cases = [
            ("Task", acp::ToolKind::Think),
            ("Agent", acp::ToolKind::Think),
            ("TaskCreate", acp::ToolKind::Think),
            ("TaskUpdate", acp::ToolKind::Think),
            ("TaskList", acp::ToolKind::Think),
            ("TaskGet", acp::ToolKind::Think),
            ("ExitPlanMode", acp::ToolKind::SwitchMode),
        ];
        for (name, expected) in cases {
            let json = format!(
                r#"{{"type":"assistant","parent_tool_use_id":null,"message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_x","name":"{name}","input":{{}}}}]}},"uuid":"u","session_id":"s"}}"#
            );
            let msg = OutputMessage::parse(&json).unwrap();
            match translate(&msg).as_slice() {
                [acp::SessionUpdate::ToolCall(call)] => {
                    assert_eq!(call.kind, expected, "tool {name}");
                }
                other => panic!("{name}: {other:?}"),
            }
        }
    }

    #[test]
    fn tool_result_image_becomes_image_content_block() {
        let msg = OutputMessage::parse(
            r#"{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0K"}}]}]},"uuid":"u","session_id":"s"}"#,
        )
        .unwrap();
        match translate(&msg).as_slice() {
            [acp::SessionUpdate::ToolCallUpdate(update)] => {
                let content = update.fields.content.as_ref().expect("content set");
                assert_eq!(content.len(), 1);
                match &content[0] {
                    acp::ToolCallContent::Content(c) => match &c.content {
                        acp::ContentBlock::Image(img) => {
                            assert_eq!(img.data, "iVBORw0K");
                            assert_eq!(img.mime_type, "image/png");
                        }
                        other => panic!("expected Image, got {other:?}"),
                    },
                    other => panic!("expected Content, got {other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_result_bash_code_execution_surfaces_exit_code() {
        let msg = OutputMessage::parse(
            r#"{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_b","content":[{"type":"bash_code_execution_result","stdout":"hello\n","stderr":"","return_code":2}]}]},"uuid":"u","session_id":"s"}"#,
        )
        .unwrap();
        match translate(&msg).as_slice() {
            [acp::SessionUpdate::ToolCallUpdate(update)] => {
                let content = update.fields.content.as_ref().expect("content set");
                match &content[0] {
                    acp::ToolCallContent::Content(c) => match &c.content {
                        acp::ContentBlock::Text(t) => {
                            assert!(t.text.contains("hello"), "stdout preserved: {}", t.text);
                            assert!(t.text.contains("exit: 2"), "exit code rendered: {}", t.text);
                            assert!(
                                t.text.contains("```console"),
                                "fenced as console block: {}",
                                t.text
                            );
                        }
                        other => panic!("expected Text, got {other:?}"),
                    },
                    other => panic!("expected Content, got {other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_result_is_error_wraps_text_and_marks_failed() {
        let msg = OutputMessage::parse(
            r#"{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_e","is_error":true,"content":"boom"}]},"uuid":"u","session_id":"s"}"#,
        )
        .unwrap();
        match translate(&msg).as_slice() {
            [acp::SessionUpdate::ToolCallUpdate(update)] => {
                assert_eq!(update.fields.status, Some(acp::ToolCallStatus::Failed));
                let content = update.fields.content.as_ref().expect("content set");
                match &content[0] {
                    acp::ToolCallContent::Content(c) => match &c.content {
                        acp::ContentBlock::Text(t) => {
                            assert!(t.text.starts_with("```"), "error wrapped: {}", t.text);
                            assert!(t.text.contains("boom"), "text preserved: {}", t.text);
                        }
                        other => panic!("expected Text, got {other:?}"),
                    },
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn infer_context_window_recognises_1m_variants_only() {
        assert_eq!(
            infer_context_window_from_model("claude-sonnet-4-5-1m"),
            Some(1_000_000)
        );
        assert_eq!(
            infer_context_window_from_model("claude-opus-4-8-1M"),
            Some(1_000_000)
        );
        // No "1m" token → falls through to default.
        assert_eq!(infer_context_window_from_model("claude-sonnet-4-5"), None);
        // "1m" embedded inside another token is NOT a window opt-in (e.g.
        // some hypothetical "1mfoo" suffix); JS uses a word-boundary
        // regex, we mirror with surrounding-non-alphanumeric.
        assert_eq!(
            infer_context_window_from_model("claude-1monkey-5"),
            None,
            "1m must be word-bounded"
        );
        assert_eq!(infer_context_window_from_model(""), None);
    }

    #[test]
    fn merge_stream_usage_treats_delta_as_cumulative_replacement() {
        let prev = Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_input_tokens: 5000,
            cache_creation_input_tokens: 1000,
        };
        // Delta carries new output_tokens (always present per JS) plus a
        // bumped cache_read; input_tokens & cache_creation are omitted, so
        // they carry over from prev.
        let delta = serde_json::json!({
            "output_tokens": 120,
            "cache_read_input_tokens": 6500
        });
        let next = merge_stream_usage(Some(&prev), Some(&delta)).expect("merge yields snapshot");
        assert_eq!(next.input_tokens, 100, "missing field falls back");
        assert_eq!(next.output_tokens, 120, "present field replaces");
        assert_eq!(next.cache_read_input_tokens, 6500, "present field replaces");
        assert_eq!(
            next.cache_creation_input_tokens, 1000,
            "missing field falls back"
        );
    }

    #[test]
    fn apply_stream_usage_emits_only_on_total_change() {
        let event = serde_json::json!({
            "type": "message_start",
            "message": {
                "model": "claude-sonnet-4-5",
                "usage": {
                    "input_tokens": 1000,
                    "output_tokens": 0,
                    "cache_read_input_tokens": 50_000,
                    "cache_creation_input_tokens": 0
                }
            }
        });
        let ev = StreamEvent {
            event,
            parent_tool_use_id: None,
        };
        let first = apply_stream_usage(&ev, None, None, Some(200_000));
        let usage = first.new_usage.as_ref().expect("snapshot taken");
        assert_eq!(context_tokens(usage), 51_000);
        match first.update {
            Some(acp::SessionUpdate::UsageUpdate(ref u)) => {
                assert_eq!(u.used, 51_000);
                assert_eq!(u.size, 200_000);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(first.new_model.as_deref(), Some("claude-sonnet-4-5"));

        // Second event reports the same cumulative total → no update.
        let same_delta = StreamEvent {
            event: serde_json::json!({
                "type": "message_delta",
                "usage": {
                    "output_tokens": 0,
                    "cache_read_input_tokens": 50_000
                }
            }),
            parent_tool_use_id: None,
        };
        let dedup = apply_stream_usage(
            &same_delta,
            first.new_usage.as_ref(),
            Some(51_000),
            Some(200_000),
        );
        assert!(
            dedup.update.is_none(),
            "no UsageUpdate when total unchanged"
        );
    }

    #[test]
    fn apply_stream_usage_skips_subagent_events() {
        let ev = StreamEvent {
            event: serde_json::json!({"type":"message_start","message":{"model":"x","usage":{"input_tokens":1}}}),
            parent_tool_use_id: Some("toolu_parent".into()),
        };
        let out = apply_stream_usage(&ev, None, None, Some(200_000));
        assert!(out.update.is_none());
        assert!(out.new_usage.is_none());
        assert!(out.new_model.is_none());
    }

    #[test]
    fn usage_update_attaches_cost_when_present() {
        let r = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","modelUsage":{"m":{"contextWindow":1000000}},"usage":{"input_tokens":100,"output_tokens":50},"total_cost_usd":0.0123}"#,
        )
        .unwrap();
        match usage_update(&r, None, None) {
            Some(acp::SessionUpdate::UsageUpdate(u)) => {
                assert_eq!(u.used, 100);
                let cost = u.cost.expect("cost attached");
                assert!((cost.amount - 0.0123).abs() < 1e-9);
                assert_eq!(cost.currency, "USD");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn usage_update_skips_zero_or_missing_cost() {
        let no_cost = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","usage":{"input_tokens":1,"output_tokens":1}}"#,
        )
        .unwrap();
        match usage_update(&no_cost, Some(200_000), None) {
            Some(acp::SessionUpdate::UsageUpdate(u)) => assert!(u.cost.is_none()),
            other => panic!("{other:?}"),
        }
        // total_cost_usd=0 must NOT produce a cost block (avoids a "$0.00"
        // banner that flickers on for local-only command turns).
        let zero_cost = serde_json::from_str::<ResultMessage>(
            r#"{"subtype":"success","usage":{"input_tokens":1,"output_tokens":1},"total_cost_usd":0.0}"#,
        )
        .unwrap();
        match usage_update(&zero_cost, Some(200_000), None) {
            Some(acp::SessionUpdate::UsageUpdate(u)) => assert!(u.cost.is_none()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn strip_local_command_metadata_removes_marker_tags() {
        // Pure marker block → empty after trim.
        assert_eq!(
            strip_local_command_metadata("<command-name>/model</command-name>"),
            ""
        );
        // Marker followed by real prose → prose preserved.
        assert_eq!(
            strip_local_command_metadata(
                "<command-name>/model</command-name>switch to opus please"
            ),
            "switch to opus please"
        );
        // Mixed prose around markers.
        assert_eq!(
            strip_local_command_metadata(
                "before <command-args>foo</command-args> middle <local-command-stdout>hi</local-command-stdout> after"
            ),
            "before  middle  after"
        );
        // Non-marker XML-ish tag is preserved.
        assert_eq!(
            strip_local_command_metadata("<note>keep</note>"),
            "<note>keep</note>"
        );
        // Multiline content inside the marker is consumed.
        assert_eq!(
            strip_local_command_metadata(
                "head\n<local-command-stdout>\nline1\nline2\n</local-command-stdout>\ntail"
            ),
            "head\n\ntail"
        );
    }

    #[test]
    fn stamp_subagent_meta_writes_nested_claude_code_key() {
        let mut update = acp::SessionUpdate::ToolCall(acp::ToolCall::new(
            acp::ToolCallId::new("toolu_child"),
            "Read",
        ));
        stamp_subagent_meta(&mut update, "toolu_parent");
        let meta = match &update {
            acp::SessionUpdate::ToolCall(t) => t.meta.as_ref().expect("meta set"),
            other => panic!("{other:?}"),
        };
        let nested = meta
            .get(CLAUDE_CODE_META_KEY)
            .and_then(|v| v.as_object())
            .expect("claudeCode object");
        assert_eq!(
            nested
                .get(PARENT_TOOL_USE_ID_META_KEY)
                .and_then(|v| v.as_str()),
            Some("toolu_parent"),
        );
    }

    #[test]
    fn stamp_subagent_meta_preserves_existing_keys() {
        // Sanity check the merge semantics — a future stamp on the same update
        // must not nuke unrelated keys that another helper wrote first.
        let mut existing = serde_json::Map::new();
        existing.insert("otherKey".into(), serde_json::json!(42));
        let mut update = acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(text_block("hi")).meta(Some(existing)),
        );
        stamp_subagent_meta(&mut update, "toolu_parent");
        let meta = match &update {
            acp::SessionUpdate::AgentMessageChunk(c) => c.meta.as_ref().expect("meta set"),
            other => panic!("{other:?}"),
        };
        assert_eq!(meta.get("otherKey"), Some(&serde_json::json!(42)));
        let nested = meta
            .get(CLAUDE_CODE_META_KEY)
            .and_then(|v| v.as_object())
            .expect("claudeCode object");
        assert_eq!(
            nested
                .get(PARENT_TOOL_USE_ID_META_KEY)
                .and_then(|v| v.as_str()),
            Some("toolu_parent"),
        );
    }

    #[test]
    fn stamp_subagent_meta_ignores_non_subagent_variants() {
        let mut update = acp::SessionUpdate::UsageUpdate(acp::UsageUpdate::new(10, 100));
        stamp_subagent_meta(&mut update, "toolu_parent");
        // UsageUpdate has no `meta` field today, and the subagent path never
        // emits one — verify we silently no-op rather than panicking.
        match update {
            acp::SessionUpdate::UsageUpdate(u) => {
                assert_eq!(u.used, 10);
                assert_eq!(u.size, 100);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn assistant_usage_update_skips_zero_usage() {
        // Streaming pre-final fragments can include an empty/zeroed usage
        // block; emitting a UsageUpdate with used=0 would yank the meter
        // back to 0 on every chunk.
        let msg = OutputMessage::parse(
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[],"usage":{"input_tokens":0,"output_tokens":0}},"uuid":"u","session_id":"s"}"#,
        )
        .unwrap();
        let conv = match msg {
            OutputMessage::Assistant(c) => c,
            other => panic!("{other:?}"),
        };
        assert!(assistant_usage_update(&conv, Some(1_000_000)).is_none());
    }
}
