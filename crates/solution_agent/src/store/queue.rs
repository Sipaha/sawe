//! User-message submission and follow-up queueing.
//!
//! `send_message` / `send_message_blocks` are the entry points the UI
//! hits when the user presses Send. Behaviour:
//!
//!   - Idle session → flip to `Running`, route through `AcpThread::send`,
//!     persist on success, transition to `Errored` on failure.
//!   - Already-`Running` session → merge into the queued bundle in
//!     `pending_messages` (one bundle ever; subsequent submissions
//!     append to it). The bundle is flushed when `Stopped` arrives.
//!
//! Each enqueued follow-up gets a compact `[HH:MM:SS] ` timestamp prefix
//! baked onto its leading text (see `queue_timestamp_prefix`) so the agent
//! can tell when each one was sent; the prefix is stripped from every UI
//! render site by `conversation_render::strip_injected_meta`.
//!
//! `cancel_turn` and `interrupt_and_flush_pending` belong here because
//! they're the inverse path — stop the in-flight turn so the queue can
//! drain on the next Stopped event.

use anyhow::{Result, anyhow};
use chrono::Utc;
use gpui::{AsyncApp, Context, Entity, SharedString, Task};

use acp_thread::{AcpThread, AgentThreadEntry, SelectedPermissionOutcome, ToolCallStatus};
use agent_client_protocol::schema as acp;

use super::{SolutionAgentStore, SolutionAgentStoreEvent};
use crate::model::{
    PendingBundle, QueueTarget, SessionState, SolutionSessionId, SolutionSessionMetadata,
};

/// How long `Stopping` may persist before the safety net kicks in and
/// force-flips the session back to `Idle`. Chosen larger than the
/// `claude_native` 30s interrupt→kill escalation
/// (`claude_native::connection::DEFAULT_ESCALATION_TIMEOUT`) so a
/// well-behaved escalation path that *does* eventually emit `Stopped`
/// (via `recover_session`'s force-resolve) wins the race and the
/// safety net never trips on a healthy run. 40s leaves a 10s headroom
/// for cross-process latency without making the user wait noticeably
/// longer than they already are.
pub(crate) const STOPPING_SAFETY_NET: std::time::Duration = std::time::Duration::from_secs(40);

/// Scan a thread's entries for a tool call sitting in
/// `WaitingForConfirmation` and, if found, return its id together with a
/// REJECT-flavoured `SelectedPermissionOutcome` to unblock it.
///
/// WHY this exists: when the agent asks for authorization, the ACP turn
/// BLOCKS on a oneshot inside `request_tool_call_authorization` until the
/// client answers. If the user ignores the allow/reject buttons and just
/// types a new message, the old behaviour silently queued the text into
/// `pending_messages` and the turn stayed blocked forever — the reported
/// "messages pile up and nothing happens" bug. Detecting the pending
/// confirmation here lets the send path resolve it first (see
/// `send_message_blocks`).
///
/// Reject-outcome selection: we reuse `conversation_render::permission_buttons`
/// to flatten the live options into clickable buttons, then pick a
/// non-allow (`!is_allow()`) button, preferring `RejectOnce` over
/// `RejectAlways` (decline just this once, don't poison future prompts
/// with a remembered "always reject"). If — unexpectedly — there is no
/// reject-flavoured button at all (a malformed server response offering
/// only allow options), we return `None` and the caller skips the resolve:
/// a stuck turn is the acceptable failure mode here, silently picking an
/// allow button (which would AUTO-APPROVE the tool call) is NOT.
///
/// NOTE on the "custom / free-text answer" branch: the agreed design also
/// wanted, when a question offers a free-text answer, to submit the user's
/// typed text AS that answer. The current ACP protocol cannot express
/// this — `PermissionOptionKind` is only {AllowOnce, AllowAlways,
/// RejectOnce, RejectAlways} and the only `SelectedPermissionParams`
/// variant is `Terminal { patterns }` (terminal command globs, not
/// arbitrary text). There is no option kind or params variant carrying a
/// free-text answer, so the custom-answer branch is currently
/// unreachable. If a future protocol adds one, build that outcome here
/// instead of the reject outcome and short-circuit the send.
pub(crate) fn pending_authorization_reject(
    thread: &Entity<AcpThread>,
    cx: &Context<SolutionAgentStore>,
) -> Option<(acp::ToolCallId, SelectedPermissionOutcome)> {
    let thread = thread.read(cx);
    for entry in thread.entries() {
        let AgentThreadEntry::ToolCall(call) = entry else {
            continue;
        };
        let ToolCallStatus::WaitingForConfirmation { options, .. } = &call.status else {
            continue;
        };
        if let Some(button) = crate::conversation_render::pick_reject_button(options) {
            return Some((call.id.clone(), button.outcome()));
        }
    }
    None
}

/// Sentinel opening every per-message timestamp prefix the queue bakes into
/// the agent-facing text. Shared with `conversation_render::strip_injected_meta`
/// so the writer and the UI stripper never desync.
pub(crate) const TS_PREFIX_OPEN: &str = "[";
/// Closing run after the `HH:MM:SS` timestamp; the stripper skips past this to
/// reach the user's content.
pub(crate) const TS_PREFIX_CLOSE: &str = "] ";

/// One-line hint prepended (at delivery, not enqueue) when a follow-up is
/// handed to the agent after it has already produced a complete message
/// (Stop-hook in-turn delivery, or idle-flush new turn). Shared with the
/// stripper. The "before your turn ended" clause is what makes "not a reply"
/// honest rather than wishy-washy.
pub(crate) const QUEUE_HINT_LINE: &str =
    "[Queued before your turn ended — not a reply to your last message.]";

/// `[HH:MM:SS] ` local-time prefix baked onto each follow-up at send time so
/// the agent can reason about when each message was sent. Compact by design;
/// stripped from every UI render site by `conversation_render::strip_injected_meta`.
pub(crate) fn queue_timestamp_prefix(at: chrono::DateTime<Utc>) -> String {
    let local = at.with_timezone(&chrono::Local);
    format!("{TS_PREFIX_OPEN}{}{TS_PREFIX_CLOSE}", local.format("%H:%M:%S"))
}

/// Flatten a content-block bundle into a single human-readable string the
/// native backend can hand to the agent as `additionalContext` (text-only).
/// Text blocks are concatenated verbatim. Each image block renders as a
/// pointer to its saved inbox file (so the agent can `Read` the actual pixels
/// mid-turn) — `image_paths` is indexed by image-occurrence order; a missing /
/// `None` entry (no path, or the save failed) falls back to the pixel-losing
/// `[image #N]` placeholder. Passing `&[]` is the placeholder-only behaviour.
/// Other variants are silently dropped — the side channel is text-only.
pub(crate) fn inject_text_from_blocks_with_image_paths(
    blocks: &[acp::ContentBlock],
    image_paths: &[Option<std::path::PathBuf>],
) -> String {
    let mut out = String::new();
    let mut image_idx = 0usize;
    for block in blocks {
        match block {
            acp::ContentBlock::Text(t) => {
                // Skip the join-newline when the prior block already ends in
                // whitespace — keeps the `[HH:MM:SS] ` stamp block on the same
                // line as the user text that follows it (the stamp ends in a
                // space), instead of breaking the timestamp onto its own line.
                if !out.is_empty() && !out.ends_with(char::is_whitespace) {
                    out.push('\n');
                }
                out.push_str(&t.text);
            }
            acp::ContentBlock::Image(_) => {
                if !out.is_empty() && !out.ends_with(char::is_whitespace) {
                    out.push('\n');
                }
                match image_paths.get(image_idx).and_then(|p| p.as_ref()) {
                    Some(path) => out.push_str(&format!(
                        "[The user attached an image, saved to {}. Use the Read tool to view it.]",
                        path.display()
                    )),
                    None => out.push_str(&format!("[image #{}]", image_idx + 1)),
                }
                image_idx += 1;
            }
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Write a queued image attachment to `dir` so a mid-turn follow-up can hand
/// the agent a real path (which it opens with the `Read` tool) instead of a
/// `[image #N]` placeholder that loses the pixels — the `additionalContext`
/// side channel is text-only, so the bytes themselves can't ride along.
/// Returns the absolute path on success; a base64-decode or write failure
/// returns `None` so the caller degrades to the placeholder rather than
/// dropping the bundle. `dir` is the per-session inbox the caller resolved
/// (`<solution_root>/.agents/<sid>/inbox/`, temp-dir fallback).
pub(crate) fn save_inbox_image(
    dir: &std::path::Path,
    index: usize,
    image: &acp::ImageContent,
) -> Option<std::path::PathBuf> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image.data.as_bytes())
        .ok()?;
    let ext = match image.mime_type.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "img",
    };
    std::fs::create_dir_all(dir).ok()?;
    // `index` disambiguates multiple images delivered in the same hook;
    // the timestamp keeps successive deliveries from clobbering each other.
    let stamp = Utc::now().format("%Y%m%d-%H%M%S%3f");
    let path = dir.join(format!("{stamp}-{index}.{ext}"));
    std::fs::write(&path, &bytes).ok()?;
    Some(path)
}

/// Compact one-line summary of a content-block bundle for the audit log
/// — enough to reconstruct what was queued / dropped from log lines
/// alone, without dumping multi-MB image blobs. Text is truncated to
/// `MAX_PREVIEW`; images / resources collapse to a typed marker. Kept
/// in this file (vs `conversation_render`) because the queue codepath
/// is the only consumer.
pub(crate) fn summarize_blocks_for_log(
    blocks: &[agent_client_protocol::schema::ContentBlock],
) -> String {
    use agent_client_protocol::schema as acp;
    const MAX_PREVIEW: usize = 200;
    let mut out = String::new();
    let mut text_total = 0usize;
    let mut images = 0usize;
    let mut other = 0usize;
    for block in blocks {
        match block {
            acp::ContentBlock::Text(t) => {
                let snippet: String = t.text.chars().take(MAX_PREVIEW).collect();
                let truncated = t.text.chars().count() > MAX_PREVIEW;
                if !out.is_empty() {
                    out.push_str(" + ");
                }
                out.push('"');
                // Keep the log a single line: replace newlines with `\n`.
                for ch in snippet.chars() {
                    if ch == '\n' {
                        out.push_str("\\n");
                    } else if ch == '"' {
                        out.push_str("\\\"");
                    } else {
                        out.push(ch);
                    }
                }
                if truncated {
                    out.push('…');
                }
                out.push('"');
                text_total += t.text.chars().count();
            }
            acp::ContentBlock::Image(_) => images += 1,
            _ => other += 1,
        }
    }
    if images > 0 || other > 0 || text_total > MAX_PREVIEW {
        let mut suffix = String::new();
        if images > 0 {
            suffix.push_str(&format!(" +{images}img"));
        }
        if other > 0 {
            suffix.push_str(&format!(" +{other}other"));
        }
        if text_total > MAX_PREVIEW {
            suffix.push_str(&format!(" total_chars={text_total}"));
        }
        out.push_str(&suffix);
    }
    if out.is_empty() {
        out.push_str("(empty)");
    }
    out
}

impl SolutionAgentStore {
    /// Best-effort cancel of an in-flight turn. Forwards to the underlying
    /// `AgentConnection::cancel`. Errors only when the session is unknown
    /// or has no live `AcpThread` yet — once the connection accepts the
    /// cancel request, downstream `AcpThreadEvent::Stopped` (or `Error`)
    /// drives the state transition through `handle_acp_event`.
    pub fn cancel_turn(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let session = self
            .session(session_id)
            .ok_or_else(|| anyhow!("unknown session {session_id}"))?;
        // Idempotent: only an in-flight turn can be stopped. A cancel in
        // Stopping/Idle/Errored is a safe no-op (covers repeated taps and the
        // mobile's deferred resend-on-reconnect).
        let in_flight = matches!(
            session.read(cx).state,
            SessionState::Running { .. } | SessionState::AwaitingInput
        );
        if !in_flight {
            return Ok(());
        }
        let (connection, acp_session_id) = {
            let s = session.read(cx);
            let thread = s
                .acp_thread()
                .ok_or_else(|| anyhow!("session {session_id} has no ACP thread yet"))?;
            (
                thread.read(cx).connection().clone(),
                s.acp_session_id.clone(),
            )
        };
        // Authoritative, backend-agnostic: flip to Stopping (broadcasts
        // SessionStateChanged) BEFORE forwarding. Stopping -> Idle still arrives
        // via the AcpThreadEvent::Stopped handler.
        self.mutate_state(
            session_id,
            |state| {
                *state = SessionState::Stopping {
                    started_at: std::time::Instant::now(),
                }
            },
            cx,
        );
        connection.cancel(&acp_session_id, cx);
        self.arm_stopping_safety_net(session_id, cx);
        Ok(())
    }

    /// Arm the safety-net timer that force-flips `Stopping → Idle` if no
    /// `AcpThreadEvent::Stopped` (or `Error`) arrives within
    /// [`STOPPING_SAFETY_NET`]. Defends against the
    /// `claude_native::connection::cancel` no-op race: when the pump has
    /// already consumed `prompt_tx` at the moment of the cancel forward,
    /// neither `cancel` nor its escalation arms anything that ever fires
    /// `Stopped`, and the queue state is left in `Stopping` forever (the
    /// observed bug: 14h+ stuck `sawe` tab on 2026-05-24).
    ///
    /// Idempotent: if a safety net is already armed for the session, the
    /// existing timer is reused. The task is stored on the session and
    /// auto-cancelled by [`super::SolutionAgentStore::mutate_state`]
    /// when the session leaves `Stopping` naturally.
    fn arm_stopping_safety_net(&mut self, session_id: SolutionSessionId, cx: &mut Context<Self>) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        if session.read(cx).stopping_safety_net.is_some() {
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(STOPPING_SAFETY_NET)
                .await;
            let _ = this.update(cx, |store, cx| {
                let Some(session) = store.session(session_id) else {
                    return;
                };
                let still_stopping = matches!(
                    session.read(cx).state,
                    SessionState::Stopping { .. }
                );
                if !still_stopping {
                    return;
                }
                let elapsed_secs = match &session.read(cx).state {
                    SessionState::Stopping { started_at } => started_at.elapsed().as_secs(),
                    _ => 0,
                };
                log::warn!(
                    target: "solution_agent::queue",
                    "session={session_id} safety-net force-flip Stopping→Idle after {elapsed_secs}s \
                     (no AcpThreadEvent::Stopped from backend) — likely the \
                     claude_native::connection::cancel no-prompt-tx race"
                );
                store.mutate_state(
                    session_id,
                    |state| *state = SessionState::Idle,
                    cx,
                );
            });
        });
        session.update(cx, |s, _| s.stopping_safety_net = Some(task));
    }

    /// Cancel the in-flight turn AND, once the resulting `Stopped(Cancelled)`
    /// arrives, flush `pending_messages` instead of clearing them. Wired to
    /// the "Send now" button in the compose row — the user typed a follow-up
    /// they want the agent to act on RIGHT NOW, not after the current turn
    /// completes.
    ///
    /// Internally just sets a one-shot flag on the session and delegates to
    /// `cancel_turn`. The handler in `handle_acp_event` (Stopped branch)
    /// reads the flag and routes the queue to `send_message_blocks`.
    pub fn interrupt_and_flush_pending(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let session = self
            .session(session_id)
            .ok_or_else(|| anyhow!("unknown session {session_id}"))?;
        if session.read(cx).pending_messages.is_empty() {
            anyhow::bail!("interrupt_and_flush_pending: no queued messages to flush");
        }
        session.update(cx, |s, _| s.flush_after_cancel = true);
        self.cancel_turn(session_id, cx)
    }

    /// Send a plain-text user message. Convenience wrapper around
    /// `send_message_blocks` for the common single-text-block case.
    pub fn send_message(
        &mut self,
        session_id: SolutionSessionId,
        content: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
            agent_client_protocol::schema::TextContent::new(content),
        )];
        self.send_message_blocks(session_id, blocks, cx)
    }

    /// Send a structured user message composed of one or more `ContentBlock`s
    /// (text + images, etc). Flips `SessionState` to `Running` synchronously
    /// (before the returned `Task` is awaited) so the UI shows activity
    /// immediately, then forwards the prompt to the underlying ACP connection.
    /// On success, schedules a persistence write of the session snapshot. On
    /// failure, transitions the session to `Errored`.
    ///
    /// Targets the MAIN agent — the common case (compose row on the parent
    /// tab, MCP sends, idle-flush re-sends, cold-wake). A follow-up typed on
    /// an Agent Teams teammate's tab goes through
    /// [`send_message_blocks_targeted`] instead.
    pub fn send_message_blocks(
        &mut self,
        session_id: SolutionSessionId,
        blocks: Vec<agent_client_protocol::schema::ContentBlock>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.send_message_blocks_targeted(session_id, blocks, QueueTarget::Main, cx)
    }

    /// Like [`send_message_blocks`], but stamps the queued follow-up with an
    /// explicit [`QueueTarget`] derived from the active tab. Only the
    /// already-running enqueue branch consults `target`: it routes the
    /// follow-up to the bundle for that addressee (the main agent, or a
    /// specific teammate), so the teammate's own hook — not the main agent's
    /// — drains it.
    ///
    /// When the session is idle/cold there is no live subagent to receive a
    /// `Subagent`-targeted message, so it is dropped with a warning rather
    /// than mis-delivered to the main thread (a follow-up written for
    /// teammate X is meaningless to the parent — no fallback to Main).
    pub fn send_message_blocks_targeted(
        &mut self,
        session_id: SolutionSessionId,
        blocks: Vec<agent_client_protocol::schema::ContentBlock>,
        target: QueueTarget,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(session_entity) = self.session(session_id) else {
            return Task::ready(Err(anyhow!("unknown session {session_id}")));
        };

        if blocks.is_empty() {
            return Task::ready(Err(anyhow!(
                "send_message_blocks: at least one ContentBlock required"
            )));
        }

        // "Chat About This" path. If the open session has a tool call
        // sitting in `WaitingForConfirmation`, the ACP turn is BLOCKED on a
        // oneshot until someone answers the allow/reject prompt. Typing a
        // new message used to just enqueue into `pending_messages` while the
        // turn stayed blocked forever — the user appeared stuck and their
        // follow-ups piled up invisibly. Instead, resolve the pending
        // authorization with a REJECT outcome FIRST (so the agent stops
        // waiting and the turn can run to `Stopped`), THEN fall through to
        // the normal send below. Because the session is still `Running`, the
        // message lands in `pending_messages` and the existing
        // flush-on-`Stopped` machinery delivers it as the next turn once the
        // (now-unblocked) turn ends — so it is never dropped.
        //
        // (The "submit typed text AS a custom/free-text answer" branch is
        // intentionally absent: the current ACP protocol can't express a
        // free-text permission answer — see `pending_authorization_reject`.)
        if let Some(thread) = session_entity.read(cx).acp_thread().cloned()
            && let Some((tool_call_id, reject_outcome)) = pending_authorization_reject(&thread, cx)
        {
            log::info!(
                target: "solution_agent::queue",
                "session={session_id} send while tool call {tool_call_id} awaiting \
                 authorization — declining (reject) to unblock the turn, then delivering \
                 the user's message as the next turn",
            );
            // Guarantee the just-queued message is delivered even if the
            // agent treats the rejection as a turn *cancel* rather than an
            // EndTurn: the Cancelled-stop handler clears `pending_messages`
            // WITHOUT sending unless `flush_after_cancel` is set. One-shot
            // flag — no-op on EndTurn, consumed/reset by the Stopped handler.
            session_entity.update(cx, |s, _| s.flush_after_cancel = true);
            thread.update(cx, |thread, cx| {
                thread.authorize_tool_call(tool_call_id, reject_outcome, cx);
            });
            // Fall through. The session is still `Running` (the rejected
            // turn hasn't emitted `Stopped` yet), so the block below enqueues
            // this message and the `Stopped` handler flushes it.
        }

        // Already running → merge into `pending_messages`; flushed on `Stopped`.
        // In-turn delivery for the native backend will be restored via a
        // pull-closure in a later task; for now all mid-turn sends queue.
        let already_running = matches!(session_entity.read(cx).state, SessionState::Running { .. });
        if already_running {
            // Audit log: queueing is a frequent source of "where did
            // my message go?" bug reports — having every enqueue +
            // queue size in the log lets us reconstruct what reached
            // pending_messages even when the message later got dropped
            // silently (e.g. by a `/clear` or a Cancelled stop).
            // `target: "solution_agent::queue"` makes these greppable.
            let blocks_text_summary = summarize_blocks_for_log(&blocks);
            let stamp = queue_timestamp_prefix(Utc::now());
            let stamped: Vec<agent_client_protocol::schema::ContentBlock> =
                std::iter::once(agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(stamp),
                ))
                .chain(blocks)
                .collect();
            let merged = session_entity.update(cx, |s, _| {
                // Merge into the trailing bundle only when it's addressed to
                // the SAME target — consecutive same-tab follow-ups coalesce
                // into one prompt, but a differently-targeted follow-up (e.g.
                // a teammate message after a main-agent one) starts its own
                // bundle so each addressee's hook drains only its own.
                let merge = s
                    .pending_messages
                    .back()
                    .is_some_and(|last| last.target == target);
                if merge {
                    let last = s
                        .pending_messages
                        .back_mut()
                        .expect("back() was Some immediately above");
                    last.blocks.push(agent_client_protocol::schema::ContentBlock::Text(
                        agent_client_protocol::schema::TextContent::new("\n\n".to_string()),
                    ));
                    last.blocks.extend(stamped);
                } else {
                    s.pending_messages.push_back(PendingBundle {
                        target: target.clone(),
                        blocks: stamped,
                    });
                }
                s.last_activity_at = Utc::now();
                merge
            });
            let queue_len = session_entity.read(cx).pending_messages.len();
            log::info!(
                target: "solution_agent::queue",
                "session={session_id} enqueued (merged={merged}, queue_len={queue_len}) preview={blocks_text_summary}",
            );
            cx.emit(SolutionAgentStoreEvent::SessionStateChanged(session_id));
            // External MCP consumers (the mobile client) render queued
            // bundles as Queued bubbles in real time off this event —
            // without it a desktop-typed follow-up to a busy session
            // would stay invisible on a paired mobile until the
            // eventual flush.
            cx.emit(SolutionAgentStoreEvent::SessionQueueChanged(session_id));
            cx.notify();
            return Task::ready(Ok(()));
        }

        // Not already running, so this would start a fresh turn on the MAIN
        // thread. A `Subagent`-targeted follow-up has no live teammate to
        // receive it here (teammates exist only inside a running parent turn),
        // and routing it to the parent would be wrong (decision: no fallback
        // to Main). Drop it with a warning rather than mis-deliver. The
        // compose row is gated on teammate liveness, so this is a defensive
        // backstop for a race (the teammate finished between render and send).
        if let QueueTarget::Subagent(agent_id) = &target {
            log::warn!(
                target: "solution_agent::queue",
                "session={session_id} dropping subagent-targeted follow-up for agent_id={agent_id} \
                 — session is not running, no live teammate to receive it; content={}",
                summarize_blocks_for_log(&blocks),
            );
            return Task::ready(Ok(()));
        }

        // Flip state immediately, before the spawn, so callers observing the
        // session right after this call returns see `Running`. Clear the
        // last-turn duration too — the "Done in Xs" indicator from the
        // previous turn is stale the moment a new turn begins.
        session_entity.update(cx, |s, _| {
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            s.last_activity_at = Utc::now();
            s.last_turn_duration = None;
        });
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(session_id));
        cx.notify();

        let Some(acp_thread) = session_entity.read(cx).acp_thread().cloned() else {
            // Cold session — no live ACP thread. Wake the agent
            // synchronously via `resume_session` (mirrors what the
            // desktop's `SolutionSessionView::start_resume` does, minus
            // the Window — MCP-driven sends don't have one). Re-enters
            // `send_message_blocks` once the thread is attached so the
            // normal hot-path code below runs unchanged.
            return self.send_message_blocks_with_wake(session_id, blocks, cx);
        };

        // Route through `AcpThread::send` (not `connection.prompt` directly)
        // so the turn runs inside `run_turn`. That wrapper appends the
        // user message, drives streaming-text flushing, and — crucially —
        // emits `AcpThreadEvent::Stopped` on success / `Error` on failure.
        // Without those events the store-side subscription never sees the
        // turn end, so `SessionState` stays stuck on `Running` after the
        // assistant has already replied.
        let send_task = acp_thread.update(cx, |thread, cx| thread.send(blocks, cx));
        // Stamped at spawn so the post-await branches can detect that the
        // session's underlying ACP thread was rotated out from under us
        // (`reset_context` for `/clear`, `rotate_context` for `/compact`)
        // while this turn was in flight. Without this guard, the old turn's
        // late `Err` would clobber the freshly-reset `Idle` state with
        // `Errored(...)` — a confusing UX where the user just typed
        // `/clear`, sees a blank conversation, and then watches it flip to
        // an error a second later because the previous turn finally
        // resolved.
        let expected_acp_session_id = session_entity.read(cx).acp_session_id.clone();

        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = send_task.await;
            match &result {
                Err(err) => {
                    // run_turn already emitted `AcpThreadEvent::Error`,
                    // which the store subscription translated into
                    // `Errored("agent error")`. Overwrite that with the
                    // specific error string so the user sees the actual
                    // cause instead of a generic placeholder.
                    let err_message = SharedString::from(err.to_string());
                    this.update(cx, |store, cx| {
                        if let Some(s) = store.session(session_id) {
                            let still_same = s.read(cx).acp_session_id == expected_acp_session_id;
                            if !still_same {
                                log::debug!(
                                    "send_message_blocks: dropping late error for {session_id} \
                                     ({err_message:?}); session was rotated/reset mid-flight"
                                );
                                return;
                            }
                            s.update(cx, |s, _| {
                                s.state = SessionState::Errored(err_message.clone());
                            });
                            cx.emit(SolutionAgentStoreEvent::SessionStateChanged(session_id));
                            cx.notify();
                        }
                    })?;
                }
                Ok(_) => {
                    // Stopped event already transitioned state to Idle
                    // via the store subscription; just persist the snapshot.
                    // Skip the persist when the thread was swapped — the
                    // current snapshot reflects the NEW session, not the
                    // turn that just resolved, so writing it under the
                    // success-of-the-old-turn premise would just be a
                    // misleading log entry.
                    this.update(cx, |store, cx| {
                        let Some(s) = store.session(session_id) else {
                            return;
                        };
                        if s.read(cx).acp_session_id != expected_acp_session_id {
                            return;
                        }
                        // Recovery for a LOST `AcpThreadEvent::Stopped`: the turn
                        // task resolved `Ok`, so the turn is definitively over.
                        // Normally the Stopped handler already flipped us to Idle
                        // and drained the queue. If the backend dropped that
                        // event, the session is wedged on `Running` forever — UI
                        // stuck on "Thinking…", queued follow-ups never sent,
                        // inline subagent pills never GC'd. Force the transition
                        // off the *fact* the turn ended (not a timeout, so a
                        // legitimately long turn is never cut short), then flush
                        // the queue exactly as the Stopped(EndTurn) path does.
                        let still_running =
                            matches!(s.read(cx).state, SessionState::Running { .. });
                        if still_running {
                            log::warn!(
                                target: "solution_agent::queue",
                                "session={session_id} turn task resolved Ok but state still \
                                 Running — no AcpThreadEvent::Stopped arrived; force-flipping to \
                                 Idle and flushing queue (lost-Stopped recovery)",
                            );
                            // mutate_state(Idle) also GCs stranded inline subagents.
                            store.mutate_state(session_id, |st| *st = SessionState::Idle, cx);
                            // Drain Main-targeted pending and re-send as the next
                            // turn (mirrors the Stopped EndTurn idle-flush).
                            // Subagent-targeted leftovers are dropped: their
                            // teammate ended with this turn.
                            let main_blocks = store
                                .session(session_id)
                                .map(|s| {
                                    s.update(cx, |s, _| {
                                        let mut main: Vec<acp::ContentBlock> = Vec::new();
                                        for bundle in s.pending_messages.drain(..) {
                                            if let QueueTarget::Main = bundle.target {
                                                main.extend(bundle.blocks);
                                            }
                                        }
                                        main
                                    })
                                })
                                .unwrap_or_default();
                            if !main_blocks.is_empty() {
                                cx.emit(SolutionAgentStoreEvent::SessionQueueChanged(session_id));
                                let mut with_hint =
                                    Vec::with_capacity(main_blocks.len() + 1);
                                with_hint.push(acp::ContentBlock::Text(
                                    acp::TextContent::new(format!("{QUEUE_HINT_LINE}\n\n")),
                                ));
                                with_hint.extend(main_blocks);
                                store
                                    .send_message_blocks(session_id, with_hint, cx)
                                    .detach();
                            }
                        }
                        store.persist_session_blob(session_id, cx);
                    })?;
                }
            }
            result.map(|_| ()).map_err(|err| anyhow!(err))
        })
    }

    /// Wake a cold session (no ACP thread attached) and queue the
    /// supplied blocks for delivery once the wake handshake completes.
    /// Driven by `send_message_blocks` when it discovers an empty
    /// `acp_thread()` — the user (typically the mobile client over
    /// MCP) sent to a sleeping session and the previous behaviour was
    /// to return a hard "session has no ACP thread yet" error.
    ///
    /// Snapshots the session metadata, resolves the owning solution
    /// from `SolutionStore`, builds a headless project (no worktree —
    /// `resume_session` keys claude-acp's jsonl lookup off
    /// `meta.cwd`, not the project's worktree), then awaits
    /// `resume_session` + re-enters `send_message_blocks`. The
    /// second entry sees the now-attached thread and forwards
    /// normally — if the session became hot during the wake (some
    /// other path attached a thread first), that's benign.
    ///
    /// Reuses `session.project` if it's still cached (sessions
    /// created in this process keep the original handle) instead of
    /// constructing a headless one — keeps the existing worktree set
    /// intact for the resume call.
    fn send_message_blocks_with_wake(
        &mut self,
        session_id: SolutionSessionId,
        blocks: Vec<agent_client_protocol::schema::ContentBlock>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(session_entity) = self.session(session_id) else {
            return Task::ready(Err(anyhow!("unknown session {session_id}")));
        };
        let (meta, cached_project) = session_entity.read_with(cx, |s, _| {
            let meta = SolutionSessionMetadata {
                id: s.id,
                solution_id: s.solution_id.clone(),
                agent_id: s.agent_id.clone(),
                acp_session_id: s.acp_session_id.clone(),
                title: s.title.clone(),
                created_at: s.created_at,
                last_activity_at: s.last_activity_at,
                preview: None,
                total_tokens: None,
                context_count: s.context_count,
                cwd: s.cwd.clone(),
                parent_session_id: s.parent_session_id,
                desired_model: s.desired_model.clone(),
                desired_effort: s.desired_effort.clone(),
                cached_models: s.cached_models.clone(),
            };
            (meta, s.project.clone())
        });

        let solution_id = meta.solution_id.clone();
        let solution = solutions::SolutionStore::try_global(cx).and_then(|store| {
            store
                .read(cx)
                .solutions()
                .iter()
                .find(|s| s.id == solution_id)
                .cloned()
        });
        let Some(solution) = solution else {
            return Task::ready(Err(anyhow!(
                "unknown_solution: cannot wake session {session_id} — solution {solution_id:?} \
                 not found in SolutionStore"
            )));
        };

        let project = match cached_project {
            Some(project) => project,
            None => match SolutionAgentStore::make_headless_project_for_solution(&solution, cx) {
                Ok(project) => project,
                Err(err) => {
                    return Task::ready(Err(anyhow!(
                        "wake_for_send: headless project construction failed for {session_id}: \
                         {err:#}"
                    )));
                }
            },
        };

        log::info!(
            target: "solution_agent::queue",
            "session={session_id} cold-send wake: invoking resume_session before forwarding \
             {} block(s)",
            blocks.len()
        );

        let resume_task = self.resume_session(meta, project, cx);
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let _resumed_id = resume_task.await?;
            // The thread is now attached on the same session entity;
            // re-enter the send path so the hot branch fires. If a
            // racing path attached the thread first, this still
            // resolves correctly — `send_message_blocks` always
            // re-reads `acp_thread()` after the cold check.
            let task = this.update(cx, |store, cx| {
                store.send_message_blocks(session_id, blocks, cx)
            })?;
            task.await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn timestamp_prefix_is_compact_local_hms() {
        // 2026-06-03 10:39:12 UTC; formatted in local tz — assert shape, not tz.
        let at = chrono::Utc.with_ymd_and_hms(2026, 6, 3, 10, 39, 12).unwrap();
        let prefix = queue_timestamp_prefix(at);
        assert!(prefix.starts_with('['), "prefix must open with '['");
        assert!(prefix.ends_with("] "), "prefix must end with '] ' separator");
        let inner = prefix
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix("] "))
            .expect("prefix must be wrapped in '[' … '] '");
        assert_eq!(inner.len(), 8, "expected HH:MM:SS, got {inner:?}");
        assert_eq!(inner.as_bytes()[2], b':');
        assert_eq!(inner.as_bytes()[5], b':');
    }

    #[test]
    fn inject_text_keeps_timestamp_on_the_message_line() {
        // The stamp is its own block ending in a space; the agent-facing text
        // must keep it inline with the user content, not break it onto its own
        // line.
        let blocks = vec![
            acp::ContentBlock::Text(acp::TextContent::new("[10:39:12] ".to_string())),
            acp::ContentBlock::Text(acp::TextContent::new("hello".to_string())),
        ];
        assert_eq!(
            inject_text_from_blocks_with_image_paths(&blocks, &[]),
            "[10:39:12] hello"
        );
    }
}
