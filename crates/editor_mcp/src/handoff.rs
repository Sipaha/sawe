//! Handoff: connect to an existing instance's MCP socket and forward CLI args.
use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use smol::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Wire name of the tool the handoff calls on the existing instance. The
/// handoff dials the GLOBAL socket, so this name must be in
/// [`crate::lifecycle::GLOBAL_TOOLS`] — `lifecycle`'s
/// `handoff_tool_is_global` test asserts exactly that against this constant,
/// so a rename or an accidental drop from the allow-list fails a test instead
/// of silently breaking `sawe <path>` on a second launch.
pub const HANDOFF_TOOL_NAME: &str = "editor.handle_cli_args";

/// JSON-RPC id the handoff sends, and the id it must match a frame against
/// before treating that frame as the reply.
const HANDOFF_REQUEST_ID: i64 = 1;

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

#[derive(Debug)]
pub enum HandoffOutcome {
    /// We acquired the lock — we are the canonical instance.
    BecameCanonical,
    /// Existing instance accepted the handoff. The caller should exit(0).
    HandedOff { focused_window_id: Option<String> },
    /// Lock held but socket unreachable after retries.
    LockBusyButUnreachable { lockholder_pid: Option<u32> },
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

pub fn try_handoff_to_existing_instance(paths: Vec<PathBuf>) -> Result<HandoffOutcome> {
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
        for attempt in 1..=RETRY_COUNT {
            match UnixStream::connect(&socket_path).await {
                Ok(mut stream) => {
                    let request = json!({
                        "jsonrpc": "2.0",
                        "id": HANDOFF_REQUEST_ID,
                        "method": "tools/call",
                        "params": {
                            "name": HANDOFF_TOOL_NAME,
                            "arguments": HandleCliArgsArgs {
                                paths: path_strings.clone(),
                                cwd: cwd.clone(),
                                new_window: None,
                                focus: Some(true),
                            }
                        }
                    });
                    let mut bytes = serde_json::to_vec(&request)?;
                    bytes.push(b'\n');
                    stream.write_all(&bytes).await.context("send handoff")?;

                    let response =
                        read_reply_frame(&mut stream, HANDOFF_REQUEST_ID, READ_TIMEOUT).await?;
                    return interpret_reply(response);
                }
                Err(err) => {
                    log::debug!(
                        "editor_mcp: handoff attempt {attempt}/{RETRY_COUNT} failed: {err}"
                    );
                    // Main-thread retry loop, not a test.
                    #[allow(clippy::disallowed_methods)]
                    smol::Timer::after(RETRY_INTERVAL).await;
                }
            }
        }
        Ok(HandoffOutcome::LockBusyButUnreachable {
            lockholder_pid: holder_pid,
        })
    })
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

/// Turn the matched reply frame into an outcome.
fn interpret_reply(response: serde_json::Value) -> Result<HandoffOutcome> {
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
    let structured = result
        .and_then(|r| r.get("structuredContent"))
        .cloned()
        .ok_or_else(|| anyhow!("missing result.structuredContent"))?;
    let outcome: HandleCliArgsResult = serde_json::from_value(structured)?;
    if !outcome.handled {
        let detail = outcome.error.as_deref().unwrap_or("(no detail)");
        return Err(anyhow!("existing instance refused handoff: {detail}"));
    }
    Ok(HandoffOutcome::HandedOff {
        focused_window_id: outcome.focused_window_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol::io::Cursor;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    const TEST_TIMEOUT: Duration = Duration::from_millis(250);

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        smol::block_on(future)
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
        match interpret_reply(frame).expect("handed off") {
            HandoffOutcome::HandedOff { focused_window_id } => {
                assert_eq!(focused_window_id.as_deref(), Some("window:7"));
            }
            other => panic!("expected HandedOff, got {other:?}"),
        }
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
    /// reply) and turned into a legible error by `interpret_reply`. This is the
    /// shape that "Tool not found" arrives in when the tool falls off
    /// `GLOBAL_TOOLS` — the defect this module's constant is pinned against.
    #[test]
    fn jsonrpc_error_with_our_id_is_reported_verbatim() {
        let script = concat!(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Tool not found: editor.handle_cli_args"}}"#,
            "\n",
        );
        let frame = read_frames(script).expect("error frame is still our reply");
        let err = interpret_reply(frame)
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
            let err = interpret_reply(frame)
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
        match interpret_reply(frame).expect("handed off") {
            HandoffOutcome::HandedOff { focused_window_id } => {
                assert_eq!(focused_window_id.as_deref(), Some("window:3"));
            }
            other => panic!("expected HandedOff, got {other:?}"),
        }
    }

    #[test]
    fn reply_missing_structured_content_is_reported() {
        let frame = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}});
        let err = interpret_reply(frame)
            .expect_err("no structuredContent")
            .to_string();
        assert!(
            err.contains("missing result.structuredContent"),
            "got {err:?}"
        );
    }

    #[test]
    fn refusal_is_reported_with_its_detail() {
        let frame = serde_json::json!({
            "jsonrpc":"2.0","id":1,
            "result":{"structuredContent":{"handled":false,"error":"path resolution failed"}}
        });
        let err = interpret_reply(frame).expect_err("refused").to_string();
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
