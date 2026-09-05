//! Handoff: connect to an existing instance's MCP socket and forward CLI args.
use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use smol::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Wire name of the tool the handoff calls on the existing instance. The
/// handoff dials the GLOBAL socket, so this name must be in
/// [`crate::lifecycle::GLOBAL_TOOLS`] — `lifecycle`'s
/// `handoff_tool_is_global` test asserts exactly that against this constant,
/// so a rename or an accidental drop from the allow-list fails a test instead
/// of silently breaking `sawe <path>` on a second launch.
pub const HANDOFF_TOOL_NAME: &str = "editor.handle_cli_args";

/// The two tools that carry `--solution` into the running instance.
///
/// `editor.handle_cli_args` cannot do it: it lives in `crates/workspace`,
/// which sits *below* `crates/solutions` in the crate graph, so the tool that
/// opens paths cannot reach a `SolutionStore`. Rather than push the gap onto
/// the operator, the hand-off makes the two calls the operator would have
/// made — resolve the name against the running instance's own list, then ask
/// it to open the id. Both dial the same global socket as the hand-off, so
/// both must be in `GLOBAL_TOOLS`; `every_tool_the_handoff_calls_is_global`
/// below pins that (FORK.md #112).
const SOLUTIONS_LIST_TOOL_NAME: &str = "solutions.list";
const SOLUTIONS_OPEN_TOOL_NAME: &str = "solutions.open";

/// JSON-RPC ids this handoff sends. One per call rather than one reused id:
/// `read_reply_frame` matches a frame by id, so a late reply to an earlier
/// call must not be mistaken for the answer to the current one.
const HANDOFF_REQUEST_ID: i64 = 1;
const SOLUTIONS_LIST_REQUEST_ID: i64 = 2;
const SOLUTIONS_OPEN_REQUEST_ID: i64 = 3;

/// How long to wait for the reply frame once the request is on the wire.
///
/// This bounds the *whole* read phase rather than each frame. A per-frame
/// timer would be the wrong shape: the server broadcasts `editor/notification`
/// frames to every connected client (this one never subscribes and still
/// receives them), so a busy editor can refresh a per-frame timer
/// indefinitely while our reply never arrives.
///
/// 30s is chosen against an asymmetric cost. Too short is the worse failure:
/// `handle_cli_args` awaits `open_paths`, so the reply is gated on a real
/// workspace open that can take seconds on a large cold project, and a
/// premature timeout puts us straight back into the silent "continuing as
/// canonical" path that drops the user's file. Too long merely makes a wedged
/// editor take 30s to report itself. So: comfortably above any plausible
/// open, still a bounded wait instead of hanging the user's terminal forever.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

const RETRY_COUNT: u32 = 5;
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// How long to pause after connect attempt `attempt` failed, or `None` when
/// there is nothing left to wait for.
///
/// The last attempt has no successor, so pausing after it burned one of the
/// user's seconds on a decision that was already made: the loop's next act is
/// to return `LockBusyButUnreachable`.
fn retry_delay(attempt: u32) -> Option<Duration> {
    (attempt < RETRY_COUNT).then_some(RETRY_INTERVAL)
}

/// The wait a user still faces once the first connect attempt has failed —
/// what the message printed at that point promises them.
fn remaining_retry_wait() -> Duration {
    RETRY_INTERVAL * (RETRY_COUNT - 1)
}

#[derive(Debug)]
pub enum HandoffOutcome {
    /// We acquired the lock — we are the canonical instance.
    BecameCanonical,
    /// Existing instance accepted the handoff. The caller should exit(0).
    HandedOff {
        focused_window_id: Option<String>,
        /// A ready-to-print stderr line when `--solution` was asked for and the
        /// running instance could not honour it; `None` when it was not asked
        /// for, or it opened. Rendered here rather than in `main.rs` because
        /// the reason is the running instance's own words and only this module
        /// ever sees them.
        solution_note: Option<String>,
    },
    /// Lock held but socket unreachable after retries.
    LockBusyButUnreachable { lockholder_pid: Option<u32> },
    /// The request was written to the running instance's socket and no usable
    /// reply came back — the read timed out, the peer closed, or the frame did
    /// not parse.
    ///
    /// Deliberately not an `Err`. "We never delivered the request" and "we
    /// delivered it but did not see the reply" are different facts and only
    /// the first is a loss: `READ_TIMEOUT` is 30s and an ordinary cold-project
    /// open can exceed it, so reporting this as dropped work sent users off to
    /// run the command again and collect a second window.
    DeliveredButUnconfirmed { reason: String },
}

#[derive(Serialize)]
struct HandleCliArgsArgs {
    paths: Vec<String>,
    cwd: Option<String>,
    new_window: Option<bool>,
    focus: Option<bool>,
}

#[derive(Deserialize)]
struct HandleCliArgsResult {
    handled: bool,
    #[serde(default)]
    #[allow(dead_code)]
    opened_paths: Vec<String>,
    #[serde(default)]
    focused_window_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Probe the lock file. Returns:
/// - `Ok(None)` if the lock is free or the file does not exist (we can take it).
/// - `Ok(Some(holder_pid))` if locked, with the recorded PID if available.
/// - `Err(_)` on unexpected I/O failure.
fn probe_lock() -> Result<Option<Option<u32>>> {
    use std::fs::File;
    use std::io::Read;
    let path = crate::lifecycle::lock_path();
    if !path.exists() {
        return Ok(None);
    }
    let mut file = File::open(&path)?;
    if fs2::FileExt::try_lock_exclusive(&file).is_ok() {
        // Grabbed it — release immediately so a real acquire can take it.
        fs2::FileExt::unlock(&file).ok();
        return Ok(None);
    }
    let mut body = String::new();
    file.read_to_string(&mut body).ok();
    let pid = body.trim().parse::<u32>().ok();
    Ok(Some(pid))
}

/// What the hand-off managed to do about `--solution`.
enum SolutionHandoff {
    /// `solutions.open` opened it; the window it landed in, when the running
    /// instance told us.
    Opened { window_id: Option<String> },
    /// The running instance's own `solutions.list` has no entry with that name
    /// or id. The canonical path reaches the same conclusion and falls through
    /// to the welcome screen; here there is no welcome screen to fall through
    /// to, so the caller says it out loud instead.
    Missing,
    /// The instance answered and declined — an empty Solution (`solutions.open`
    /// requires members, where the canonical path shows an `EmptySolutionPage`
    /// instead), a store error, or the tool missing from its catalog.
    Refused(String),
    /// The call went out and no usable reply came back. Same distinction as
    /// [`HandoffOutcome::DeliveredButUnconfirmed`].
    Undelivered(String),
}

/// Hand this process's command line to the instance holding the MCP lock.
///
/// `solution` is `--solution <name-or-id>`, and it is carried rather than
/// dropped: `editor.handle_cli_args` cannot open a Solution (see
/// [`SOLUTIONS_LIST_TOOL_NAME`]), so the hand-off makes the two calls that
/// can. The caller is responsible for only passing it when the canonical path
/// would have honoured it — `main.rs::split_handoff_args` owns that rule.
pub fn try_handoff_to_existing_instance(
    paths: Vec<PathBuf>,
    solution: Option<String>,
) -> Result<HandoffOutcome> {
    let lock_status = probe_lock()?;
    let holder_pid = match lock_status {
        None => return Ok(HandoffOutcome::BecameCanonical),
        Some(pid) => pid,
    };

    let socket_path = crate::lifecycle::socket_path();
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let path_strings: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    smol::block_on(async move {
        let Some(mut stream) = connect_with_retries(&socket_path).await else {
            return Ok(HandoffOutcome::LockBusyButUnreachable {
                lockholder_pid: holder_pid,
            });
        };
        hand_off_on(&mut stream, path_strings, cwd, solution).await
    })
}

/// Connect to the running instance's socket, retrying while it may still be
/// starting up. `None` once every attempt has failed.
async fn connect_with_retries(socket_path: &Path) -> Option<UnixStream> {
    for attempt in 1..=RETRY_COUNT {
        match UnixStream::connect(socket_path).await {
            Ok(stream) => return Some(stream),
            Err(err) => {
                log::debug!("editor_mcp: handoff attempt {attempt}/{RETRY_COUNT} failed: {err}");
                if attempt == 1 {
                    // Said on the terminal rather than only in the log,
                    // because this is a command the user typed and the
                    // alternative is several seconds of total silence before
                    // either an opened file or an error. The healthy hand-off
                    // never reaches this arm — it connects on the first
                    // attempt and returns above — so an ordinary
                    // `sawe <path>` stays silent.
                    eprintln!(
                        "sawe: the running instance is not answering yet; retrying for up to {}s…",
                        remaining_retry_wait().as_secs()
                    );
                }
                if let Some(delay) = retry_delay(attempt) {
                    // Main-thread retry loop, not a test.
                    #[allow(clippy::disallowed_methods)]
                    smol::Timer::after(delay).await;
                }
            }
        }
    }
    None
}

/// Drive the whole hand-off over one already-connected socket.
async fn hand_off_on<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    paths: Vec<String>,
    cwd: Option<String>,
    solution: Option<String>,
) -> Result<HandoffOutcome> {
    let mut solution_note = None;
    if let Some(name_or_id) = solution.as_deref() {
        match open_solution(stream, name_or_id).await? {
            SolutionHandoff::Opened { window_id } => {
                // The Solution's window is up and was asked for focus. A
                // trailing `editor.handle_cli_args` would carry an empty path
                // list — which that tool answers by activating whichever
                // window it finds first — and would undo exactly the focus we
                // just requested.
                return Ok(HandoffOutcome::HandedOff {
                    focused_window_id: window_id,
                    solution_note: None,
                });
            }
            SolutionHandoff::Missing => {
                solution_note = Some(format!(
                    "sawe: --solution {name_or_id}: the running instance has no Solution with \
                     that name or id — nothing was opened for it."
                ));
            }
            SolutionHandoff::Refused(reason) => {
                solution_note = Some(format!(
                    "sawe: --solution {name_or_id} could not be opened in the running instance: \
                     {reason}"
                ));
            }
            SolutionHandoff::Undelivered(reason) => {
                return Ok(HandoffOutcome::DeliveredButUnconfirmed { reason });
            }
        }
    }

    let arguments = serde_json::to_value(HandleCliArgsArgs {
        paths,
        cwd,
        new_window: None,
        focus: Some(true),
    })?;
    match call_tool(stream, HANDOFF_REQUEST_ID, HANDOFF_TOOL_NAME, arguments).await? {
        CallOutcome::Undelivered(reason) => Ok(HandoffOutcome::DeliveredButUnconfirmed { reason }),
        CallOutcome::Failed(reason) => Err(anyhow!("{reason}")),
        CallOutcome::Ok(structured) => {
            let outcome: HandleCliArgsResult = serde_json::from_value(structured)?;
            if !outcome.handled {
                let detail = outcome.error.as_deref().unwrap_or("(no detail)");
                return Err(anyhow!("existing instance refused handoff: {detail}"));
            }
            Ok(HandoffOutcome::HandedOff {
                focused_window_id: outcome.focused_window_id,
                solution_note,
            })
        }
    }
}

/// Resolve `--solution <name-or-id>` against the running instance's own list
/// and ask it to open the match.
async fn open_solution<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    name_or_id: &str,
) -> Result<SolutionHandoff> {
    let listed = match call_tool(
        stream,
        SOLUTIONS_LIST_REQUEST_ID,
        SOLUTIONS_LIST_TOOL_NAME,
        json!({}),
    )
    .await?
    {
        CallOutcome::Ok(value) => value,
        CallOutcome::Failed(reason) => {
            return Ok(SolutionHandoff::Refused(format!(
                "{SOLUTIONS_LIST_TOOL_NAME}: {reason}"
            )));
        }
        CallOutcome::Undelivered(reason) => return Ok(SolutionHandoff::Undelivered(reason)),
    };
    let Some(solution_id) = resolve_solution_id(&listed, name_or_id) else {
        return Ok(SolutionHandoff::Missing);
    };
    match call_tool(
        stream,
        SOLUTIONS_OPEN_REQUEST_ID,
        SOLUTIONS_OPEN_TOOL_NAME,
        json!({ "solution_id": solution_id, "focus": true }),
    )
    .await?
    {
        CallOutcome::Ok(value) => Ok(SolutionHandoff::Opened {
            window_id: value
                .get("window_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        CallOutcome::Failed(reason) => Ok(SolutionHandoff::Refused(format!(
            "{SOLUTIONS_OPEN_TOOL_NAME}: {reason}"
        ))),
        CallOutcome::Undelivered(reason) => Ok(SolutionHandoff::Undelivered(reason)),
    }
}

/// Pick out of a `solutions.list` reply the Solution that `--solution
/// <name-or-id>` names.
///
/// Deliberately the same rule `main.rs::open_solution_by_name_or_id` applies
/// on the canonical side — a numeric argument may match `id`, and any argument
/// may match `name` — because the same argv must not select a different
/// Solution depending on whether another editor happened to be running
/// (FORK.md #114's parity rule). The numeric match is guarded on the argument
/// actually parsing as a number, or a non-numeric `--solution` would match the
/// first entry whose `id` is missing from the reply.
fn resolve_solution_id(listed: &Value, name_or_id: &str) -> Option<i64> {
    let by_id = name_or_id.parse::<i64>().ok();
    listed
        .get("solutions")?
        .as_array()?
        .iter()
        .find(|solution| {
            let id = solution.get("id").and_then(Value::as_i64);
            (by_id.is_some() && id == by_id)
                || solution.get("name").and_then(Value::as_str) == Some(name_or_id)
        })?
        .get("id")?
        .as_i64()
}

/// What one `tools/call` round trip came back as.
enum CallOutcome {
    /// The call succeeded; the payload is `result.structuredContent`.
    Ok(Value),
    /// The reply arrived and says the call did not happen — a JSON-RPC `error`
    /// member, a `result.isError: true`, or a result with no
    /// `structuredContent`. The message is already a complete sentence.
    Failed(String),
    /// The request went out and no usable reply came back.
    Undelivered(String),
}

/// Send one `tools/call` and classify what comes back.
///
/// An `Err` from here means the request itself could not be put on the wire,
/// which is the only shape of failure that is unambiguously a loss.
async fn call_tool<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    id: i64,
    name: &str,
    arguments: Value,
) -> Result<CallOutcome> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await.context("send handoff")?;

    match read_reply_frame(stream, id, READ_TIMEOUT).await {
        Ok(frame) => Ok(match structured_content(frame) {
            Ok(value) => CallOutcome::Ok(value),
            Err(err) => CallOutcome::Failed(err.to_string()),
        }),
        Err(err) => Ok(CallOutcome::Undelivered(err.to_string())),
    }
}

/// One newline-delimited unit read off the socket.
enum Frame {
    Line(Vec<u8>),
    /// Peer closed with nothing buffered.
    Eof,
}

async fn read_line<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Frame> {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte).await {
            // A final frame without a trailing newline is still a frame.
            Ok(0) => {
                return Ok(if buffer.is_empty() {
                    Frame::Eof
                } else {
                    Frame::Line(buffer)
                });
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    return Ok(Frame::Line(buffer));
                }
                buffer.push(byte[0]);
            }
            Err(err) => return Err(err.into()),
        }
    }
}

/// Read newline-delimited frames until one carries `request_id`.
///
/// `editor/notification` frames interleave with responses on this socket and
/// carry no `id` at all — and opening a path makes the server emit
/// `buffer_opened` *before* the reply, so the first frame back is routinely a
/// notification. Taking frame 1 as the reply (what this used to do) therefore
/// failed with "missing result.structuredContent" exactly when the handoff had
/// in fact worked, and only for paths not already open, which is why it read
/// as intermittent.
///
/// A frame that does not parse is a hard error rather than something to skip:
/// every frame on this socket is `serde_json` output from our own server, so
/// garbage means the protocol is broken and failing loudly beats waiting out
/// the timeout.
async fn read_reply_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    request_id: i64,
    timeout: Duration,
) -> Result<serde_json::Value> {
    let read = async {
        loop {
            match read_line(reader).await? {
                Frame::Eof => {
                    return Err(anyhow!(
                        "existing instance closed the connection before replying"
                    ));
                }
                Frame::Line(buffer) => {
                    // Blank keepalive lines are not frames.
                    if buffer.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    let frame: serde_json::Value =
                        serde_json::from_slice(&buffer).context("parse handoff response")?;
                    // A notification has no `id`; a foreign or null `id` is
                    // someone else's reply. Neither is ours.
                    if frame.get("id").and_then(serde_json::Value::as_i64) == Some(request_id) {
                        return Ok(frame);
                    }
                }
            }
        }
    };
    let expire = async {
        #[allow(clippy::disallowed_methods)]
        smol::Timer::after(timeout).await;
        Err(anyhow!(
            "existing instance did not reply within {timeout:?} (it may be wedged)"
        ))
    };
    smol::future::or(read, expire).await
}

/// Pull `result.structuredContent` out of the matched reply frame, or say
/// which of the server's three failure shapes came back instead.
///
/// The `Err` message is a complete sentence on purpose: it is what the caller
/// prints, and every branch here exists because its shape was once flattened
/// into the generic "missing result.structuredContent" below.
fn structured_content(response: serde_json::Value) -> Result<Value> {
    // Surface a JSON-RPC error verbatim. "Tool not found" here means
    // `HANDOFF_TOOL_NAME` fell off the global socket's catalog (see
    // `lifecycle::GLOBAL_TOOLS`), which the generic message below used to hide.
    if let Some(err) = response.get("error") {
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("(no message)");
        return Err(anyhow!("existing instance returned an error: {message}"));
    }
    // A *tool-level* failure is not a JSON-RPC error: `context_server`'s
    // dispatcher (`listener.rs`, the `Err(err)` arm of `handle_call_tool`)
    // answers with a successful response whose `result.isError` is `true`,
    // the real message in `result.content[..].text`, and
    // `structuredContent` omitted. Without this branch that shape fell
    // through to the generic "missing result.structuredContent" below and
    // the actual reason was thrown away. `isError` is also serialized as
    // `false` on success, so this must test for `true`, not for presence.
    let result = response.get("result");
    if result
        .and_then(|r| r.get("isError"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        let message = result
            .and_then(|r| r.get("content"))
            .and_then(Value::as_array)
            .map(|chunks| {
                // Mirrors `CallToolResponse::text_contents`: only `text`
                // chunks carry a message, and the first chunk is not
                // guaranteed to be one (a tool may answer with an image or
                // a resource link), so collect every text chunk rather than
                // indexing content[0].
                chunks
                    .iter()
                    .filter_map(|chunk| chunk.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| "(no message)".to_string());
        return Err(anyhow!(
            "existing instance reported a tool failure: {message}"
        ));
    }
    result
        .and_then(|r| r.get("structuredContent"))
        .cloned()
        .ok_or_else(|| anyhow!("missing result.structuredContent"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol::io::Cursor;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    const TEST_TIMEOUT: Duration = Duration::from_millis(250);

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        smol::block_on(future)
    }

    /// A socket whose replies are scripted ahead of time: writes are recorded
    /// so a test can assert which tools were called, reads come out of the
    /// script in order.
    ///
    /// The hand-off is strictly request-then-response, so pre-scripting the
    /// replies is faithful: nothing it reads depends on what a real server
    /// would have made of what it wrote. Running out of script is EOF, which
    /// is how the "no reply came back" cases are provoked without waiting out
    /// `READ_TIMEOUT`.
    struct ScriptedPeer {
        replies: Cursor<Vec<u8>>,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl ScriptedPeer {
        fn new(replies: &[&str]) -> Self {
            let mut bytes = Vec::new();
            for reply in replies {
                bytes.extend_from_slice(reply.as_bytes());
                bytes.push(b'\n');
            }
            Self {
                replies: Cursor::new(bytes),
                written: Arc::default(),
            }
        }

        fn requested_tools(&self) -> Vec<String> {
            let written = self.written.lock().expect("written");
            String::from_utf8_lossy(&written)
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter_map(|frame| Some(frame.get("params")?.get("name")?.as_str()?.to_string()))
                .collect()
        }
    }

    impl AsyncRead for ScriptedPeer {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.replies).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for ScriptedPeer {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.written.lock().expect("written").extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn read_frames(script: &str) -> Result<serde_json::Value> {
        let mut reader = Cursor::new(script.as_bytes().to_vec());
        block_on(read_reply_frame(
            &mut reader,
            HANDOFF_REQUEST_ID,
            TEST_TIMEOUT,
        ))
    }

    /// The regression this whole change exists for: the reply is NOT the first
    /// frame. A leading `buffer_opened` notification (no `id`) and a foreign
    /// reply (`id: 99`) must both be skipped.
    #[test]
    fn reply_is_found_behind_a_notification_and_a_foreign_id() {
        let script = concat!(
            r#"{"jsonrpc":"2.0","method":"editor/notification","params":{"kind":"buffer_opened"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":99,"result":{"structuredContent":{"handled":false}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"structuredContent":{"handled":true,"opened_paths":["/tmp/a"],"focused_window_id":"window:7"}}}"#,
            "\n",
        );
        let frame = read_frames(script).expect("reply found");
        assert_eq!(frame["id"].as_i64(), Some(1));
        let structured = structured_content(frame).expect("structured content");
        assert_eq!(structured["focused_window_id"], "window:7");
    }

    #[test]
    fn frame_stream_edge_cases() {
        // (name, script, expected substring of the error)
        let cases: &[(&str, &str, &str)] = &[
            (
                "eof before any frame",
                "",
                "closed the connection before replying",
            ),
            (
                "eof after only a notification",
                "{\"jsonrpc\":\"2.0\",\"method\":\"editor/notification\"}\n",
                "closed the connection before replying",
            ),
            (
                "malformed frame is a hard error",
                "not json at all\n",
                "parse handoff response",
            ),
            (
                "null id is not our id, then eof",
                "{\"jsonrpc\":\"2.0\",\"id\":null,\"result\":{}}\n",
                "closed the connection before replying",
            ),
        ];
        for (name, script, expected) in cases {
            let err = read_frames(script)
                .expect_err(&format!("{name}: expected an error"))
                .to_string();
            assert!(
                err.contains(expected),
                "{name}: expected error containing {expected:?}, got {err:?}"
            );
        }
    }

    /// Bare newlines are keepalives, not frames, and must not be mistaken for
    /// EOF. The reply still arrives after them.
    #[test]
    fn blank_lines_are_skipped_not_treated_as_eof() {
        let script = concat!(
            "\n",
            "  \n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"structuredContent":{"handled":true}}}"#,
            "\n",
        );
        let frame = read_frames(script).expect("reply found past blank lines");
        assert_eq!(frame["id"].as_i64(), Some(1));
    }

    /// A final frame with no trailing newline is still a frame.
    #[test]
    fn reply_without_trailing_newline_is_read() {
        let script = r#"{"jsonrpc":"2.0","id":1,"result":{"structuredContent":{"handled":true}}}"#;
        let frame = read_frames(script).expect("unterminated reply read");
        assert_eq!(frame["id"].as_i64(), Some(1));
    }

    /// An error response carrying OUR id is returned by the reader (it is our
    /// reply) and turned into a legible error by `structured_content`. This is the
    /// shape that "Tool not found" arrives in when the tool falls off
    /// `GLOBAL_TOOLS` — the defect this module's constant is pinned against.
    #[test]
    fn jsonrpc_error_with_our_id_is_reported_verbatim() {
        let script = concat!(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Tool not found: editor.handle_cli_args"}}"#,
            "\n",
        );
        let frame = read_frames(script).expect("error frame is still our reply");
        let err = structured_content(frame)
            .expect_err("error response")
            .to_string();
        assert!(
            err.contains("Tool not found: editor.handle_cli_args"),
            "got {err:?}"
        );
    }

    /// A tool-level failure is a *successful* JSON-RPC response carrying
    /// `result.isError: true` — not a JSON-RPC `error` member — so it needs
    /// its own branch. Before that branch existed every one of these shapes
    /// came back as "missing result.structuredContent" and the reason the
    /// hand-off failed was discarded.
    #[test]
    fn tool_level_error_is_reported_verbatim() {
        // (name, `result` object, expected substring of the error)
        let cases: &[(&str, serde_json::Value, &str)] = &[
            (
                "text message",
                serde_json::json!({
                    "isError": true,
                    "content": [{"type": "text", "text": "open_paths failed: no such file"}]
                }),
                "open_paths failed: no such file",
            ),
            (
                "content absent",
                serde_json::json!({"isError": true}),
                "(no message)",
            ),
            (
                "content empty",
                serde_json::json!({"isError": true, "content": []}),
                "(no message)",
            ),
            (
                "first chunk is not text",
                serde_json::json!({
                    "isError": true,
                    "content": [
                        {"type": "image", "data": "AAAA", "mimeType": "image/png"},
                        {"type": "text", "text": "the real reason"}
                    ]
                }),
                "the real reason",
            ),
            (
                "no text chunk at all",
                serde_json::json!({
                    "isError": true,
                    "content": [{"type": "image", "data": "AAAA", "mimeType": "image/png"}]
                }),
                "(no message)",
            ),
        ];
        for (name, result, expected) in cases {
            let frame = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": result});
            let err = structured_content(frame)
                .expect_err(&format!("{name}: expected an error"))
                .to_string();
            assert!(
                err.contains(expected),
                "{name}: expected error containing {expected:?}, got {err:?}"
            );
            assert!(
                !err.contains("missing result.structuredContent"),
                "{name}: tool failure fell through to the generic message: {err:?}"
            );
        }
    }

    /// The success response also carries `isError`, as `false` — so the
    /// branch above must test for `true` rather than for the key's presence,
    /// or every successful hand-off becomes a reported failure.
    #[test]
    fn is_error_false_is_not_a_failure() {
        let frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "isError": false,
                "content": [{"type": "text", "text": "opened 1 path(s)"}],
                "structuredContent": {"handled": true, "focused_window_id": "window:3"}
            }
        });
        let structured = structured_content(frame).expect("handed off");
        assert_eq!(structured["focused_window_id"], "window:3");
    }

    #[test]
    fn reply_missing_structured_content_is_reported() {
        let frame = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}});
        let err = structured_content(frame)
            .expect_err("no structuredContent")
            .to_string();
        assert!(
            err.contains("missing result.structuredContent"),
            "got {err:?}"
        );
    }

    /// A refusal is a well-formed reply carrying `handled: false`, so it only
    /// becomes an error once the hand-off has deserialized it — which is the
    /// full round trip, not `structured_content` alone.
    #[test]
    fn refusal_is_reported_with_its_detail() {
        let mut peer = ScriptedPeer::new(&[
            r#"{"jsonrpc":"2.0","id":1,"result":{"structuredContent":{"handled":false,"error":"path resolution failed"}}}"#,
        ]);
        let err = block_on(hand_off_on(&mut peer, vec!["/tmp/a".into()], None, None))
            .expect_err("refused")
            .to_string();
        assert!(err.contains("path resolution failed"), "got {err:?}");
    }

    /// A reader that never yields and never closes — the wedged-handler case.
    /// Before the bound was added this hung the user's terminal forever.
    struct NeverReady;

    impl AsyncRead for NeverReady {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Pending
        }
    }

    #[test]
    fn a_wedged_peer_times_out_instead_of_hanging() {
        let mut reader = NeverReady;
        let err = block_on(read_reply_frame(
            &mut reader,
            HANDOFF_REQUEST_ID,
            TEST_TIMEOUT,
        ))
        .expect_err("must not hang");
        assert!(
            err.to_string().contains("did not reply within"),
            "got {err:?}"
        );
    }

    /// A notification arriving before a wedged handler must not refresh the
    /// bound: the deadline covers the whole read phase, not each frame.
    #[test]
    fn notification_then_silence_still_times_out() {
        let notification = b"{\"jsonrpc\":\"2.0\",\"method\":\"editor/notification\"}\n";
        let mut reader = Cursor::new(notification.to_vec()).chain(NeverReady);
        let err = block_on(read_reply_frame(
            &mut reader,
            HANDOFF_REQUEST_ID,
            TEST_TIMEOUT,
        ))
        .expect_err("must not hang");
        assert!(
            err.to_string().contains("did not reply within"),
            "got {err:?}"
        );
    }

    /// The retry loop must not pause after its final attempt: that pause
    /// expires with the decision already made, so it is pure added latency on
    /// a command the user typed. Guarding it turns the worst case from
    /// `RETRY_COUNT` intervals into `RETRY_COUNT - 1`, which is also what the
    /// message printed after the first failure promises.
    #[test]
    fn the_last_attempt_does_not_pause_before_giving_up() {
        assert_eq!(
            retry_delay(RETRY_COUNT),
            None,
            "the final attempt has nothing to wait for"
        );
        let total: Duration = (1..=RETRY_COUNT).filter_map(retry_delay).sum();
        assert_eq!(
            total,
            RETRY_INTERVAL * (RETRY_COUNT - 1),
            "the loop must spend one interval less than it has attempts"
        );
        assert_eq!(
            total,
            remaining_retry_wait(),
            "the wait promised after the first failure must be the wait actually served"
        );
    }

    /// Defect 1: `--solution` used to be dropped in silence — the hand-off
    /// carried paths only, so `sawe --solution probe-test` against a running
    /// instance activated a window, opened nothing and exited 0. It is now
    /// carried: resolve the name against the instance's own `solutions.list`,
    /// then `solutions.open` the id it found.
    #[test]
    fn a_solution_is_resolved_and_opened_in_the_running_instance() {
        let mut peer = ScriptedPeer::new(&[
            r#"{"jsonrpc":"2.0","id":2,"result":{"structuredContent":{"solutions":[{"id":4,"name":"other"},{"id":7,"name":"probe-test"}]}}}"#,
            r#"{"jsonrpc":"2.0","id":3,"result":{"structuredContent":{"window_id":"window:9","focused":true,"opened_paths":["/ss/probe-test/a"]}}}"#,
        ]);
        let outcome = block_on(hand_off_on(
            &mut peer,
            Vec::new(),
            None,
            Some("probe-test".to_string()),
        ))
        .expect("the solution hand-off must succeed");
        match outcome {
            HandoffOutcome::HandedOff {
                focused_window_id,
                solution_note,
            } => {
                assert_eq!(focused_window_id.as_deref(), Some("window:9"));
                assert_eq!(solution_note, None, "nothing was lost, so nothing is said");
            }
            other => panic!("expected HandedOff, got {other:?}"),
        }
        assert_eq!(
            peer.requested_tools(),
            vec![SOLUTIONS_LIST_TOOL_NAME, SOLUTIONS_OPEN_TOOL_NAME],
            "the trailing empty-path `editor.handle_cli_args` would activate \
             whichever window it found first and undo the focus we just asked for"
        );
    }

    /// The two lookup rules `main.rs::open_solution_by_name_or_id` applies on
    /// the canonical side, which this must match or the same argv selects a
    /// different Solution depending on whether an editor was running.
    #[test]
    fn a_solution_is_resolved_by_id_or_by_name() {
        let listed = serde_json::json!({
            "solutions": [
                {"id": 4, "name": "alpha"},
                {"id": 7, "name": "probe-test"},
                {"name": "no id at all"},
            ]
        });
        assert_eq!(resolve_solution_id(&listed, "probe-test"), Some(7));
        assert_eq!(resolve_solution_id(&listed, "7"), Some(7));
        assert_eq!(resolve_solution_id(&listed, "alpha"), Some(4));
        assert_eq!(resolve_solution_id(&listed, "nope"), None);
        assert_eq!(
            resolve_solution_id(&listed, "no id at all"),
            None,
            "an entry the reply gave no id for cannot be opened"
        );
        assert_eq!(
            resolve_solution_id(&serde_json::json!({}), "probe-test"),
            None
        );
    }

    /// A `--solution` the running instance does not have is the one case the
    /// canonical path answers with a welcome screen. There is no welcome
    /// screen at the far end of a hand-off, so it has to be said out loud —
    /// and the paths on the same command line still go, which is why the
    /// `editor.handle_cli_args` call still happens.
    #[test]
    fn a_solution_the_instance_does_not_have_is_named_not_dropped() {
        let mut peer = ScriptedPeer::new(&[
            r#"{"jsonrpc":"2.0","id":2,"result":{"structuredContent":{"solutions":[]}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"structuredContent":{"handled":true}}}"#,
        ]);
        let outcome = block_on(hand_off_on(
            &mut peer,
            Vec::new(),
            None,
            Some("probe-test".to_string()),
        ))
        .expect("hand-off still succeeds");
        match outcome {
            HandoffOutcome::HandedOff { solution_note, .. } => {
                let note = solution_note.expect("the loss must be reported");
                assert!(note.contains("probe-test"), "got {note:?}");
                assert!(note.contains("no Solution"), "got {note:?}");
            }
            other => panic!("expected HandedOff, got {other:?}"),
        }
        assert_eq!(
            peer.requested_tools(),
            vec![SOLUTIONS_LIST_TOOL_NAME, HANDOFF_TOOL_NAME]
        );
    }

    /// `solutions.open` refuses an empty Solution (the canonical path shows an
    /// `EmptySolutionPage` instead), and the instance's own words are what the
    /// user needs to see.
    #[test]
    fn a_solution_the_instance_refuses_carries_its_reason() {
        let mut peer = ScriptedPeer::new(&[
            r#"{"jsonrpc":"2.0","id":2,"result":{"structuredContent":{"solutions":[{"id":7,"name":"probe-test"}]}}}"#,
            r#"{"jsonrpc":"2.0","id":3,"result":{"isError":true,"content":[{"type":"text","text":"solution 7 has no members"}]}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"structuredContent":{"handled":true}}}"#,
        ]);
        let outcome = block_on(hand_off_on(
            &mut peer,
            Vec::new(),
            None,
            Some("probe-test".to_string()),
        ))
        .expect("hand-off still succeeds");
        match outcome {
            HandoffOutcome::HandedOff { solution_note, .. } => {
                let note = solution_note.expect("the refusal must be reported");
                assert!(note.contains("solution 7 has no members"), "got {note:?}");
                assert!(note.contains(SOLUTIONS_OPEN_TOOL_NAME), "got {note:?}");
            }
            other => panic!("expected HandedOff, got {other:?}"),
        }
    }

    /// Defect 3b: a request that went out and whose reply never came back is
    /// NOT a dropped command line. `READ_TIMEOUT` is 30s and an ordinary cold
    /// project open can exceed it, so calling this a failure sent users off to
    /// run the command again and collect a second window.
    #[test]
    fn a_reply_that_never_arrives_is_distinguished_from_a_request_never_sent() {
        let mut peer = ScriptedPeer::new(&[]);
        let outcome = block_on(hand_off_on(&mut peer, vec!["/tmp/a".into()], None, None))
            .expect("an unconfirmed delivery is an outcome, not an error");
        match outcome {
            HandoffOutcome::DeliveredButUnconfirmed { reason } => {
                assert!(reason.contains("closed the connection"), "got {reason:?}");
            }
            other => panic!("expected DeliveredButUnconfirmed, got {other:?}"),
        }
        assert_eq!(
            peer.requested_tools(),
            vec![HANDOFF_TOOL_NAME],
            "the request really did go out — that is the whole point"
        );
    }

    /// FORK.md #112: every tool a non-agent client calls has to be on the
    /// GLOBAL socket, or it comes back `-32601 Tool not found`. The hand-off
    /// dials `socket_path()`, so all three of its callees are pinned here
    /// against the allow-list the server actually splits on.
    #[test]
    fn every_tool_the_handoff_calls_is_global() {
        for name in [
            HANDOFF_TOOL_NAME,
            SOLUTIONS_LIST_TOOL_NAME,
            SOLUTIONS_OPEN_TOOL_NAME,
        ] {
            assert!(
                crate::lifecycle::is_global_tool(name),
                "{name} is solution-scoped, so the CLI hand-off would get 'Tool not found'"
            );
        }
    }

    /// Finding 3: the handoff's constant must equal the name the tool actually
    /// registers under. `lifecycle`'s `handoff_tool_is_global` only pins
    /// constant <-> allow-list; without this, renaming `HandleCliArgsTool::NAME`
    /// in `crates/workspace/src/mcp/handle_cli_args.rs` leaves every test green
    /// and silently re-breaks `sawe <path>` on a second launch.
    #[test]
    fn constant_matches_the_registered_tool_name() {
        use context_server::listener::McpServerTool as _;
        assert_eq!(
            workspace::mcp::handle_cli_args::HandleCliArgsTool::NAME,
            HANDOFF_TOOL_NAME,
            "the handoff calls HANDOFF_TOOL_NAME but the tool registers under a \
             different wire name — `sawe <path>` would get 'Tool not found'"
        );
    }
}
