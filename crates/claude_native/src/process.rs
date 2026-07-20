//! Subprocess lifecycle for the `claude` binary: spawn + the reader/writer/
//! stderr async tasks over its stdio. This is the only module that touches the
//! OS; everything above it works against the [`InputMessage`]/[`OutputMessage`]
//! channels this type exposes.

use std::collections::HashMap;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use futures::future::Shared;
use futures::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{App, Task};
use util::ResultExt as _;
use util::process::Child;

use crate::command::ClaudeCommandSpec;
use crate::protocol::{ControlRequestOut, InputMessage, OutputMessage};

/// Pending `send_control` calls awaiting their matching `control_response`,
/// keyed by the `request_id` we allocated. The reader fulfils and removes the
/// entry when the response arrives; a dropped sender (process gone) resolves
/// the awaiting receiver with `Cancelled`.
type PendingControls = Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>;

/// One running `claude` process. Holds the child plus the three stdio tasks;
/// dropping it cancels the tasks AND process-group-kills the subprocess (see
/// the `Drop` impl below). Messages flow over the `outgoing` sender (stdin)
/// and `incoming` receiver (stdout).
pub struct ClaudeProcess {
    child: Child,
    pub outgoing: UnboundedSender<InputMessage>,
    pub incoming: UnboundedReceiver<OutputMessage>,
    /// Stream of critical stderr lines that downstream layers MUST react to —
    /// currently just the "Error in hook callback" pattern, which is the
    /// observed leading edge of the "Thinking forever after background-task
    /// container restart" hang (claude's own `Result` never lands so the
    /// pump's prompt oneshot would otherwise wait forever). Routine stderr
    /// stays in the log; only escalations come through here. Take with
    /// `take_critical_stderr` and select! it alongside `incoming` in the
    /// update pump.
    critical_stderr: UnboundedReceiver<CriticalStderr>,
    pending_controls: PendingControls,
    next_request_id: AtomicU64,
    exited: Shared<Task<Option<ExitStatus>>>,
    _reader: Task<()>,
    _writer: Task<()>,
    _stderr: Task<()>,
}

/// Categorised stderr line worth waking the update pump for.
#[derive(Clone, Debug)]
pub enum CriticalStderr {
    /// `Error in hook callback <id>: …` — claude is reporting a failure
    /// in its own hook handling. Empirically (seen in production on
    /// 2026-05-25 with `stop_inj`) the agent then never emits a
    /// terminating `Result` for the in-flight turn, so the prompt
    /// oneshot needs to be force-resolved by the pump.
    HookCallbackError {
        callback_id: String,
        /// Trimmed first line of the error so the user-facing message
        /// has something concrete to show (e.g.
        /// `"The container was restarted. …"`).
        first_line: String,
    },
}

impl ClaudeProcess {
    pub fn spawn(spec: ClaudeCommandSpec, cx: &App) -> Result<Self> {
        let mut child = Child::spawn(
            spec.to_std_command(),
            Stdio::piped(),
            Stdio::piped(),
            Stdio::piped(),
        )?;

        let stdout = child.stdout.take().context("claude stdout missing")?;
        let stdin = child.stdin.take().context("claude stdin missing")?;
        let stderr = child.stderr.take().context("claude stderr missing")?;

        let (incoming_sender, incoming) = mpsc::unbounded::<OutputMessage>();
        let (outgoing, outgoing_receiver) = mpsc::unbounded::<InputMessage>();
        let (critical_stderr_tx, critical_stderr) = mpsc::unbounded::<CriticalStderr>();
        let pending_controls: PendingControls = Arc::new(Mutex::new(HashMap::new()));

        let executor = cx.background_executor();
        let reader = executor.spawn(read_stdout(
            stdout,
            incoming_sender,
            pending_controls.clone(),
        ));
        let writer = executor.spawn(write_stdin(stdin, outgoing_receiver));
        let stderr_task = executor.spawn(drain_stderr(stderr, critical_stderr_tx));

        // `Child::status` clones an internal handle, so the resulting future is
        // independent of the `Child` we keep around (for `kill`). Driving it in
        // a shared task lets `wait_status()` hand out cheap clones.
        let status_future = child.status();
        let exited = executor
            .spawn(async move { status_future.await.ok() })
            .shared();

        Ok(Self {
            child,
            outgoing,
            incoming,
            critical_stderr,
            pending_controls,
            next_request_id: AtomicU64::new(0),
            exited,
            _reader: reader,
            _writer: writer,
            _stderr: stderr_task,
        })
    }

    /// Take the per-process critical-stderr stream (one-shot — subsequent
    /// calls return a closed stream). The connection's update pump owns it
    /// after spawn so it can `select!` it alongside `incoming` and force-
    /// resolve in-flight prompts on a hook-callback error.
    pub fn take_critical_stderr(&mut self) -> UnboundedReceiver<CriticalStderr> {
        let (sender, closed) = mpsc::unbounded::<CriticalStderr>();
        drop(sender);
        std::mem::replace(&mut self.critical_stderr, closed)
    }

    /// Take ownership of the `incoming` output stream, leaving a closed stream
    /// in its place. The connection's per-session update-pump owns the receiver
    /// (it drains it to translate output into thread updates), while the rest of
    /// `ClaudeProcess` stays in `SessionState` for stdin writes / control / kill.
    pub fn take_incoming(&mut self) -> UnboundedReceiver<OutputMessage> {
        let (sender, closed) = mpsc::unbounded::<OutputMessage>();
        drop(sender);
        std::mem::replace(&mut self.incoming, closed)
    }

    /// Resolves with the child's exit status once it terminates (or `None` if
    /// the status could not be collected). Cheap to call repeatedly — each call
    /// returns a clone of the same shared future.
    pub fn wait_status(&self) -> impl std::future::Future<Output = Option<ExitStatus>> + 'static {
        self.exited.clone()
    }

    /// Send a control request to `claude`, returning a receiver that resolves
    /// with the matching `control_response` payload. The request_id is
    /// allocated and registered here so the reader can route the response back.
    pub fn send_control(
        &self,
        request: ControlRequestOut,
    ) -> Result<oneshot::Receiver<serde_json::Value>> {
        let request_id = format!(
            "claude-native-{}",
            self.next_request_id.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = oneshot::channel();
        self.pending_controls
            .lock()
            .unwrap_or_else(|guard| guard.into_inner())
            .insert(request_id.clone(), sender);
        let message = InputMessage::ControlRequest {
            request_id,
            request,
        };
        self.outgoing
            .unbounded_send(message)
            .context("claude process stdin closed")?;
        Ok(receiver)
    }

    /// Reply to a `can_use_tool` control request from `claude` with an
    /// allow/deny decision. `request_id` is the id `claude` sent.
    pub fn send_control_response(&self, request_id: &str, allow: bool) -> Result<()> {
        self.outgoing
            .unbounded_send(InputMessage::permission_response(request_id, allow))
            .context("claude process stdin closed")
    }

    /// The OS process id of the running child. Distinguishes one spawn from a
    /// later respawn (the Stop-escalation / watchdog recovery replaces the
    /// process, so a changed pid is how callers detect a kill+resume happened).
    pub fn process_id(&self) -> u32 {
        self.child.id()
    }

    /// SIGKILL the process group. Used by Stop escalation / close_session in
    /// later phases; kept here next to spawn so the OS surface stays in one
    /// place.
    pub fn kill(&mut self) -> Result<()> {
        self.child.kill()
    }
}

/// Reap the subprocess when the owning `ClaudeProcess` goes away.
///
/// Neither `smol::process::Child` nor `util::process::Child` kills on drop
/// (the latter's process-group machinery only covers the *editor* exiting), so
/// every drop path that did not remember an explicit `kill()` orphaned a live
/// `claude`. That was not a benign leak: an orphan spawned with
/// `--resume <session id>` keeps executing the interrupted turn, and the
/// recovery that dropped it immediately spawns another process on the SAME
/// session id — so a stuck-turn reconnect loop accumulates concurrent writers
/// on one worktree and one transcript (observed live: three processes resuming
/// a single session, committing over each other).
///
/// Killing here makes "drop == reaped" true for every path at once, which is
/// what the reconnect / pool-eviction call sites already assumed. Explicit
/// `kill()` callers are unaffected — `killpg` on an already-dead group is a
/// no-op `ESRCH`.
impl Drop for ClaudeProcess {
    fn drop(&mut self) {
        self.child.kill().log_err();
    }
}

async fn read_stdout(
    stdout: impl futures::AsyncRead + Unpin,
    incoming_sender: UnboundedSender<OutputMessage>,
    pending_controls: PendingControls,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next().await {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                log::warn!("claude stdout read error: {error}");
                break;
            }
        };
        if line.is_empty() {
            continue;
        }
        match OutputMessage::parse(&line) {
            // Control responses fulfil a pending `send_control`; route them to
            // the matching oneshot rather than the general output stream.
            Ok(OutputMessage::ControlResponse(envelope)) => {
                let request_id = envelope.request_id().to_string();
                let payload = envelope.into_response();
                let sender = pending_controls
                    .lock()
                    .unwrap_or_else(|guard| guard.into_inner())
                    .remove(&request_id);
                match sender {
                    Some(sender) => {
                        if sender.send(payload).is_err() {
                            log::debug!("control response for {request_id} dropped: caller gone");
                        }
                    }
                    // Expected for fire-and-forget control_requests (we send
                    // `initialize` via the outgoing channel directly, not via
                    // `send_control`, so its response has no pending sender)
                    // and for any unsolicited acks claude may emit. Logged at
                    // debug so sawe.log isn't flooded — set RUST_LOG to
                    // include it if you're debugging the control plane.
                    None => log::debug!("control response for unknown request_id {request_id}"),
                }
            }
            Ok(message) => {
                // Diagnostic: a successful parse that lands in `Unknown`
                // means claude emitted a top-level message type our
                // protocol enum doesn't cover (`#[serde(other)]` catch-all).
                // Without surfacing the raw payload here, a new SDK
                // message kind (ping, rate_limit_event, future additions)
                // disappears silently — grepping for this target tells us
                // what we're missing. Truncated to 2 KB to keep the log
                // bounded; the message type is at the head so it survives.
                if matches!(message, OutputMessage::Unknown) {
                    let preview: String = line.chars().take(2048).collect();
                    log::debug!(
                        target: "claude_native::unknown",
                        "OutputMessage::Unknown — raw line: {preview}"
                    );
                }
                if incoming_sender.unbounded_send(message).is_err() {
                    // Receiver dropped — nobody is listening anymore.
                    break;
                }
            }
            Err(error) => {
                log::warn!("claude stdout parse error: {error}; line: {line}");
            }
        }
    }
    // Reader ended (EOF or read error): dropping `incoming_sender` here closes
    // the `incoming` stream so awaiters observe the end-of-output.
    drop(incoming_sender);
}

async fn write_stdin(
    mut stdin: impl futures::AsyncWrite + Unpin,
    mut outgoing_receiver: UnboundedReceiver<InputMessage>,
) {
    while let Some(message) = outgoing_receiver.next().await {
        let mut line = match serde_json::to_string(&message) {
            Ok(line) => line,
            Err(error) => {
                log::error!("failed to serialize claude input message: {error}");
                continue;
            }
        };
        // Diagnostic mirror of `claude_native::stream`/`turn_end`: log what
        // we're writing to claude's stdin so a "we sent user message X but
        // claude emitted Result(end_turn, text='')" report can be checked
        // wire-vs-model without instrumenting per call site. Long inline
        // base64 image payloads make the log line useless, so cap at
        // ~2 KB; the rest is irrelevant to "did the bytes go out".
        log::debug!(
            target: "claude_native::stdin",
            "→ {preview}",
            preview = stdin_preview(&line)
        );
        line.push('\n');
        if let Err(error) = stdin.write_all(line.as_bytes()).await {
            log::warn!("claude stdin write error: {error}");
            break;
        }
        if let Err(error) = stdin.flush().await {
            log::warn!("claude stdin flush error: {error}");
            break;
        }
    }
}

/// Trim a serialised stdin message for the diagnostic log: cap at ~2 KB and
/// note the original length so a truncated entry is obviously truncated
/// (vs. mysteriously short). 2 KB is enough to see the JSON skeleton plus a
/// preview of any user text/prompt content; long base64 blobs (images,
/// uploads) get cut here and don't drown the log file.
fn stdin_preview(line: &str) -> String {
    const CAP: usize = 2048;
    if line.len() <= CAP {
        line.to_string()
    } else {
        let head: String = line.chars().take(CAP).collect();
        format!("{head}…[truncated, full_len={}]", line.len())
    }
}

async fn drain_stderr(
    stderr: impl futures::AsyncRead + Unpin,
    critical: UnboundedSender<CriticalStderr>,
) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    // Multi-line errors (claude dumps a JS source fragment after the header)
    // belong to the most recently seen `Error in hook callback <id>:` —
    // hold on to the id+first-content-line so the critical signal carries
    // a useful preview instead of just the bare header.
    let mut pending_hook_error: Option<(String, Option<String>)> = None;
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\n', '\r']);
                if let Some((callback_id, inline_preview)) = parse_hook_callback_error(trimmed) {
                    log::warn!("claude stderr: {trimmed}");
                    // claude usually prints the human-readable cause on the same
                    // line as the header (after a `<lineno> |` marker), then
                    // dumps minified source on the following lines. When the
                    // header carries that message, emit it immediately — it's a
                    // far better preview than the `${…}` source line that the
                    // subsequent-line scan would otherwise capture.
                    if let Some(preview) = inline_preview {
                        let _ = critical.unbounded_send(CriticalStderr::HookCallbackError {
                            callback_id: callback_id.clone(),
                            first_line: preview.clone(),
                        });
                        pending_hook_error = Some((callback_id, Some(preview)));
                    } else {
                        pending_hook_error = Some((callback_id, None));
                    }
                    continue;
                }
                if let Some((cb_id, first_line_slot)) = pending_hook_error.as_mut() {
                    // Capture the first non-empty, non-line-number content
                    // line as the human preview. claude's dump intersperses
                    // `<lineno> | <text>` markers; we strip the leader so
                    // the user-facing preview reads cleanly.
                    if first_line_slot.is_none() {
                        let content = strip_line_number_prefix(trimmed);
                        if !content.is_empty() {
                            *first_line_slot = Some(content.to_string());
                            let _ = critical.unbounded_send(CriticalStderr::HookCallbackError {
                                callback_id: cb_id.clone(),
                                first_line: content.to_string(),
                            });
                        }
                    }
                }
                if is_benign_agent_stderr(trimmed) {
                    log::debug!("claude stderr: {trimmed}");
                } else {
                    log::warn!("claude stderr: {trimmed}");
                }
            }
        }
    }
}

/// Match the `Error in hook callback <id>:` header claude writes to stderr
/// when a hook callback fails inside its runtime (the leading edge of the
/// container-restart hang). Returns the callback id plus an optional inline
/// preview: the text after the colon, with claude's `<lineno> |` source-frame
/// marker stripped. That tail is frequently the actual human-readable cause
/// (e.g. `… stop_inj: 2283 | The container was restarted.`), so capturing it
/// here avoids falling through to the minified source line that follows.
fn parse_hook_callback_error(line: &str) -> Option<(String, Option<String>)> {
    let stripped = line.strip_prefix("Error in hook callback ")?;
    // `<id>: …` or `<id>` alone.
    let id_end = stripped.find(':').unwrap_or(stripped.len());
    let id = stripped[..id_end].trim();
    if id.is_empty() {
        return None;
    }
    let inline_preview = if id_end < stripped.len() {
        let tail = strip_line_number_prefix(&stripped[id_end + 1..]).trim();
        (!tail.is_empty()).then(|| tail.to_string())
    } else {
        None
    };
    Some((id.to_string(), inline_preview))
}

/// Drop a leading `<digits> | ` marker claude inserts on every line of its
/// dumped source fragment (e.g. `12368 | The container was restarted.`).
/// Returns the input unchanged when the marker isn't present, so this is
/// safe to call on any line.
fn strip_line_number_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();
    let mut chars = trimmed.char_indices();
    let mut last_digit_end = 0;
    while let Some((i, c)) = chars.next() {
        if c.is_ascii_digit() {
            last_digit_end = i + 1;
        } else {
            break;
        }
    }
    if last_digit_end == 0 {
        return trimmed;
    }
    let rest = &trimmed[last_digit_end..];
    if let Some(rest) = rest.strip_prefix(" | ") {
        rest
    } else {
        trimmed
    }
}

/// Mirrors `agent_servers::acp::is_benign_agent_stderr`: lines that fire on
/// routine internals (and the `{"type":"ping"}` keepalive the SDK sometimes
/// writes to stderr) are downgraded to debug so they don't look like errors.
fn is_benign_agent_stderr(line: &str) -> bool {
    line.contains("No onPostToolUseHook found for tool use ID") || line.contains(r#""type":"ping""#)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load-bearing claim behind the reconnect fix: dropping a
    /// `ClaudeProcess` REAPS the subprocess.
    ///
    /// It did not, before the `Drop` impl — neither `smol::process::Child` nor
    /// `util::process::Child` kills on drop — and the stuck-turn reconnect path
    /// dropped the wedged connection expecting exactly that. Each spurious
    /// reconnect therefore left a live `claude --resume <session id>` still
    /// executing the interrupted turn while a fresh process resumed the SAME
    /// id, so N recoveries produced N concurrent writers on one worktree and
    /// one transcript (observed live: three).
    ///
    /// Uses a stub binary that ignores the claude CLI args and sleeps, so the
    /// test exercises the real spawn → drop → reap path without needing claude.
    #[gpui::test]
    async fn dropping_the_process_kills_the_child(cx: &mut gpui::TestAppContext) {
        use std::io::Write as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("fake-claude");
        {
            let mut f = std::fs::File::create(&stub).expect("create stub");
            // `exec` so the sleeping process keeps THIS pid — the one we assert on.
            writeln!(f, "#!/bin/sh\nexec sleep 300").expect("write stub");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }

        let spec = crate::command::ClaudeCommandSpec {
            binary: stub,
            work_dir: dir.path().to_path_buf(),
            session: crate::command::SessionArg::New("drop-reap-test".to_string()),
            mcp_servers_json: "{}".to_string(),
            append_system_prompt: None,
            extra_env: Vec::new(),
            model: None,
            settings_path: None,
        };

        let pid = cx.update(|cx| {
            let process = ClaudeProcess::spawn(spec, cx).expect("spawn stub");
            let pid = process.process_id();
            assert!(process_alive(pid), "stub must be running before the drop");
            drop(process);
            pid
        });

        // SIGKILL delivery + reap is asynchronous; poll briefly rather than
        // assuming the process is gone the instant `drop` returns.
        for _ in 0..100 {
            if !process_alive(pid) {
                return;
            }
            cx.background_executor
                .timer(std::time::Duration::from_millis(20))
                .await;
        }
        panic!("subprocess {pid} survived the drop — the Drop impl is not reaping");
    }

    #[cfg(unix)]
    fn process_alive(pid: u32) -> bool {
        // Signal 0 probes for existence without delivering anything. A zombie
        // still answers, so the stub `exec`s (no intermediate shell to reap).
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[test]
    fn parses_hook_callback_error_header() {
        // Plain header with a textual tail after the colon: id + inline preview.
        assert_eq!(
            parse_hook_callback_error("Error in hook callback stop_inj: boom"),
            Some(("stop_inj".to_string(), Some("boom".to_string())))
        );
        // Header carrying claude's `<lineno> | <message>` source-frame marker
        // inline: the marker is stripped and the message is the preview.
        assert_eq!(
            parse_hook_callback_error(
                "Error in hook callback stop_inj: 2283 | The container was restarted."
            ),
            Some((
                "stop_inj".to_string(),
                Some("The container was restarted.".to_string())
            ))
        );
        // No colon → id only, no inline preview (fall back to subsequent lines).
        assert_eq!(
            parse_hook_callback_error("Error in hook callback pti"),
            Some(("pti".to_string(), None))
        );
        assert!(parse_hook_callback_error("not a hook error").is_none());
        assert!(parse_hook_callback_error("Error in hook callback : empty").is_none());
    }

    #[test]
    fn drain_stderr_prefers_inline_header_message_over_source_dump() {
        // Reproduces the production dump observed 2026-06-01: claude prints the
        // human-readable cause on the SAME line as the header (after a
        // `<lineno> |` marker), then dumps minified template source on the
        // following lines. The preview must be the readable message, not the
        // `${H.map(...)` gibberish that follows.
        let dump = concat!(
            "Error in hook callback stop_inj: 2283 | The container was restarted. ",
            "The following background tasks were running and are now stopped:\n",
            "2284 | ${H.map((q)=>`- ${q.description||\"(no description)\"} (task ${q.task_id})`).join(`\n",
            "2285 | `)}\n",
            "2286 | Re-create them if still needed.\n"
        );
        let (tx, mut rx) = mpsc::unbounded::<CriticalStderr>();
        futures::executor::block_on(drain_stderr(
            futures::io::Cursor::new(dump.as_bytes().to_vec()),
            tx,
        ));
        let first = rx.try_recv().ok().expect("expected a HookCallbackError");
        let CriticalStderr::HookCallbackError {
            callback_id,
            first_line,
        } = first;
        assert_eq!(callback_id, "stop_inj");
        assert_eq!(
            first_line,
            "The container was restarted. The following background tasks were running and are now stopped:"
        );
    }

    #[test]
    fn strips_line_number_prefix_from_dumped_source() {
        assert_eq!(
            strip_line_number_prefix("12368 | The container was restarted."),
            "The container was restarted."
        );
        // No marker → returned unchanged (trimmed).
        assert_eq!(strip_line_number_prefix("plain line"), "plain line");
        // Leading whitespace stripped.
        assert_eq!(strip_line_number_prefix("   foo"), "foo");
    }
}
