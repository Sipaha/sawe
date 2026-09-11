//! `UnixMcpProxy`: per-connection client for the embedded `editor_mcp`
//! JSON-RPC server.
//!
//! Each authenticated WebSocket session opens one of these. The reader half
//! of the Unix-socket connection runs as a single background task that:
//!
//! - Demuxes responses (frames with `id`) by looking up the id in a shared
//!   `HashMap<i32, oneshot::Sender>` and firing the oneshot.
//! - Forwards notifications (frames with no `id`) through a bounded,
//!   coalescing queue ([`NotificationSender`] / [`NotificationReceiver`])
//!   that the per-WS notification pump drains.
//!
//! See ADR-0003 ("WebSocket over TLS + HMAC challenge") and the R-4 plan
//! doc for the architectural rationale. The proxy intentionally generates
//! its own monotonic `i32` request ids — the upstream server's
//! `RequestId` enum is `i32 | Str`, while the WS client's id can be any
//! JSON value, so we map the WS id to a fresh local id at request time and
//! substitute the original back into the response before returning.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Mutex, Notify, oneshot};
use tokio::task::JoinHandle;

/// 5 seconds to establish the Unix-socket connection. The local server is
/// in-process, so failure means it's not running — fail fast instead of
/// blocking the per-WS task.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-call timeout. 30 s is the same cap autonomous agents use against
/// the same socket (some tools — e.g. `solutions.add_member` — start an
/// async op and return quickly with `operation_id`, so the synchronous
/// reply is always under a second).
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on the notifications queue. `agent_session_message_appended`
/// streams 10-20 frames/s during an active turn; a momentarily slow WS
/// client shouldn't wedge that source.
///
/// Overflow policy, in order of preference:
///   1. **Coalesce.** `agent_session_dirty` is a content-free "re-poll me"
///      poke carrying the session's CURRENT `change_seq`, so the one with
///      the higher seq supersedes the other for the same session. A push
///      whose `(kind, session_id)` already sits in the queue drops the
///      superseded frame and enqueues the survivor at the BACK, i.e. at its
///      own arrival position — replacing in place would have delivered it
///      ahead of frames that arrived before it, which is a reorder.
///   2. **Drop the OLDEST.** The freshest frame is the one the client needs
///      most (it carries the highest seq / latest state), so an overflow
///      evicts from the front, never the back.
///   3. **Spare `upload_*`.** Chunk acks have no self-healing path — a lost
///      one costs the client a 30 s ack timeout — so eviction picks the
///      oldest NON-upload frame and only falls back to the true front when
///      the whole queue is upload traffic.
const NOTIFICATION_QUEUE_CAPACITY: usize = 256;

/// Notification kind whose frames coalesce by session: the poke with the
/// higher `current_seq` makes the other redundant.
const COALESCING_KIND: &str = "agent_session_dirty";

/// Per-connection set of notification kinds this client asked NOT to receive
/// (`suppress_kinds` on `editor.subscribe`; audit N-34, narrow slice).
///
/// **This is the only layer that may enforce it.** `editor_mcp`'s
/// `SubscriptionRegistry` is process-global and never pruned on disconnect, so
/// consulting a suppression list inside `editor_mcp::emit` would apply one
/// phone's preferences to every other client — including the desktop's own
/// consumers. Here the identity of the asking connection is structural: one
/// `SuppressedKinds` per `UnixMcpProxy`, dropped with the socket, so the
/// suppression reverts on every reconnect until the client asks again.
///
/// Empty by default, which is today's fan-out exactly.
#[derive(Clone, Default)]
pub struct SuppressedKinds(Arc<std::sync::RwLock<Vec<String>>>);

impl SuppressedKinds {
    /// Replace the set. Last `editor.subscribe` on this connection wins: a
    /// re-subscribe that omits `suppress_kinds` restores full fan-out, which
    /// is the fail-safe direction (more traffic, never a dropped poke).
    pub fn set(&self, kinds: Vec<String>) {
        match self.0.write() {
            Ok(mut guard) => *guard = kinds,
            // A poisoned lock means a reader panicked mid-critical-section.
            // The contents are structurally valid (no unwinding happens
            // inside), so recover rather than killing the connection.
            Err(poisoned) => *poisoned.into_inner() = kinds,
        }
    }

    fn contains(&self, kind: &str) -> bool {
        let guard = match self.0.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.iter().any(|suppressed| suppressed == kind)
    }
}

type ResponseMap = Arc<Mutex<HashMap<i32, oneshot::Sender<Value>>>>;

/// Read `params.kind` off an upstream notification frame.
fn notification_kind(frame: &Value) -> &str {
    frame
        .pointer("/params/kind")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
}

/// `params.payload.current_seq` off an `agent_session_dirty` frame, or 0
/// when it is missing (a frame with no seq can never win a coalesce).
fn dirty_seq(frame: &Value) -> u64 {
    frame
        .pointer("/params/payload/current_seq")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

/// The coalescing identity of a frame, or `None` when the frame must be
/// delivered on its own (every kind except [`COALESCING_KIND`]).
fn coalesce_key(frame: &Value) -> Option<(&str, &str)> {
    let kind = notification_kind(frame);
    if kind != COALESCING_KIND {
        return None;
    }
    let session_id = frame
        .pointer("/params/payload/session_id")
        .and_then(|v| v.as_str())?;
    Some((kind, session_id))
}

struct NotificationQueue {
    items: VecDeque<Value>,
    /// Set when the producing reader task exits; makes `recv` return `None`
    /// instead of parking forever.
    producer_gone: bool,
    /// Set when the consuming WS task drops its receiver; makes `send` a
    /// no-op instead of filling a queue nobody drains.
    consumer_gone: bool,
}

struct NotificationShared {
    // `std::sync::Mutex`: every critical section is a few pointer moves, and
    // holding it across an await is impossible by construction (no awaits
    // inside). The async wake-up is carried by `notify` instead.
    queue: std::sync::Mutex<NotificationQueue>,
    notify: Notify,
}

/// Producer half of the per-connection notification queue. Cloneable-free by
/// design: exactly one reader task owns it, and its `Drop` is what tells the
/// consumer the stream ended.
pub struct NotificationSender {
    shared: Arc<NotificationShared>,
}

/// Consumer half. Owned by the per-WS notification pump.
pub struct NotificationReceiver {
    shared: Arc<NotificationShared>,
}

/// Build a per-connection notification channel. `pub` so integration
/// tests can drive a dispatcher stub's notification stream.
pub fn notification_channel() -> (NotificationSender, NotificationReceiver) {
    let shared = Arc::new(NotificationShared {
        queue: std::sync::Mutex::new(NotificationQueue {
            items: VecDeque::new(),
            producer_gone: false,
            consumer_gone: false,
        }),
        notify: Notify::new(),
    });
    (
        NotificationSender {
            shared: shared.clone(),
        },
        NotificationReceiver { shared },
    )
}

impl NotificationSender {
    /// Enqueue one notification frame. Never blocks and never fails: the
    /// overflow policy documented on [`NOTIFICATION_QUEUE_CAPACITY`] decides
    /// what gives way.
    pub fn send(&self, frame: Value) {
        let mut queue = match self.shared.queue.lock() {
            Ok(guard) => guard,
            // A poisoned mutex means a consumer panicked mid-critical-section.
            // The queue contents are still structurally valid (no unwinding
            // happens inside), so recover rather than propagating the panic
            // into the reader task.
            Err(poisoned) => poisoned.into_inner(),
        };
        if queue.consumer_gone {
            return;
        }
        let mut frame = frame;
        if let Some((kind, session_id)) = coalesce_key(&frame) {
            let kind = kind.to_owned();
            let session_id = session_id.to_owned();
            let existing = queue.items.iter().position(|item| {
                coalesce_key(item).is_some_and(|(k, s)| k == kind && s == session_id)
            });
            if let Some(index) = existing {
                // Keep the higher seq rather than trusting arrival order.
                // Emits are single-threaded on GPUI today so the newer frame
                // always carries the higher `current_seq`, but that is a
                // cross-crate coupling this queue shouldn't depend on.
                if let Some(superseded) = queue.items.remove(index) {
                    if dirty_seq(&superseded) > dirty_seq(&frame) {
                        frame = superseded;
                    }
                }
            }
        }
        if queue.items.len() >= NOTIFICATION_QUEUE_CAPACITY {
            let victim = queue
                .items
                .iter()
                .position(|item| !notification_kind(item).starts_with("upload_"))
                .unwrap_or(0);
            let dropped = queue.items.remove(victim);
            log::warn!(
                target: "remote_control",
                "notifications queue full ({NOTIFICATION_QUEUE_CAPACITY}); dropping oldest {:?}",
                dropped.as_ref().map(notification_kind).unwrap_or_default(),
            );
        }
        queue.items.push_back(frame);
        self.shared.notify.notify_one();
    }
}

impl Drop for NotificationSender {
    fn drop(&mut self) {
        if let Ok(mut queue) = self.shared.queue.lock() {
            queue.producer_gone = true;
        }
        self.shared.notify.notify_one();
    }
}

impl NotificationReceiver {
    /// Await the next queued frame. Returns `None` once the queue is drained
    /// AND the producing reader task is gone.
    pub async fn recv(&mut self) -> Option<Value> {
        loop {
            // Arm the waiter BEFORE inspecting the queue: a `send` landing
            // between the check and the await would otherwise notify nobody
            // and park us until the next frame.
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut queue = match self.shared.queue.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if let Some(frame) = queue.items.pop_front() {
                    return Some(frame);
                }
                if queue.producer_gone {
                    return None;
                }
            }
            notified.await;
        }
    }
}

impl Drop for NotificationReceiver {
    fn drop(&mut self) {
        if let Ok(mut queue) = self.shared.queue.lock() {
            queue.consumer_gone = true;
            queue.items.clear();
        }
    }
}

/// Per-WS-connection proxy to the embedded `editor_mcp` Unix socket. Owns
/// the write half (held under `Mutex` to serialise concurrent writers,
/// although the single dispatch loop currently only ever calls one at a
/// time) and a join handle to the background reader.
pub struct UnixMcpProxy {
    write_half: Mutex<OwnedWriteHalf>,
    pending: ResponseMap,
    notifications_rx: Option<NotificationReceiver>,
    next_id: AtomicI32,
    reader_task: Option<JoinHandle<()>>,
    suppressed_kinds: SuppressedKinds,
}

impl UnixMcpProxy {
    /// Resolve `editor_mcp::socket_path()`, connect with a 5 s timeout,
    /// split into read+write halves, and spawn the reader task. Returns
    /// an error if the connect times out or the socket is missing.
    pub async fn connect() -> Result<Self> {
        let socket_path = editor_mcp::socket_path();
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&socket_path))
            .await
            .map_err(|_| {
                anyhow!(
                    "connecting to local MCP socket {} timed out after {}s",
                    socket_path.display(),
                    CONNECT_TIMEOUT.as_secs(),
                )
            })?
            .with_context(|| format!("connecting to {}", socket_path.display()))?;

        let (read_half, write_half) = stream.into_split();
        let pending: ResponseMap = Arc::new(Mutex::new(HashMap::new()));
        let (notifications_tx, notifications_rx) = notification_channel();
        let suppressed_kinds = SuppressedKinds::default();

        let reader_task = tokio::spawn(read_loop(
            read_half,
            pending.clone(),
            notifications_tx,
            suppressed_kinds.clone(),
        ));

        Ok(Self {
            write_half: Mutex::new(write_half),
            pending,
            notifications_rx: Some(notifications_rx),
            next_id: AtomicI32::new(1),
            reader_task: Some(reader_task),
            suppressed_kinds,
        })
    }

    /// Handle onto this connection's `suppress_kinds` set, so the dispatcher
    /// can record what the client asked for when it forwards an
    /// `editor.subscribe`.
    pub fn suppressed_kinds(&self) -> SuppressedKinds {
        self.suppressed_kinds.clone()
    }

    /// Take ownership of the notifications receiver. Returns `None` on a
    /// second call — the per-WS task is the sole reader by contract.
    pub fn take_notifications(&mut self) -> Option<NotificationReceiver> {
        self.notifications_rx.take()
    }

    /// Write one JSON-RPC request upstream and return the handle that will
    /// carry its response. The `method`/`params` are wrapped in the upstream
    /// MCP envelope
    /// (`{"method":"tools/call","params":{"name":method,"arguments":params}}`)
    /// — see `crates/context_server/src/listener.rs::handle_call_tool`. The
    /// caller's WS-side `id` is NOT used on the wire — the proxy mints a
    /// fresh local i32, and the response is rewrapped with the WS id by
    /// `ProxyConnection::begin_dispatch`.
    ///
    /// **The split into `begin_call` + [`PendingCall::finish`] is what keeps
    /// per-connection request ORDER.** The listener reads frames in wire
    /// order and drives dispatch concurrently; if the upstream write also
    /// moved onto those concurrent tasks, whichever task won
    /// `write_half`'s mutex would decide what the editor sees first, and
    /// two messages a user typed in a definite order could land in the
    /// transcript reversed. So the caller awaits `begin_call` inline, in
    /// wire order — it is an id mint, a map insert and a `write_all` on a
    /// local Unix socket, i.e. microseconds, never the 30 s that motivated
    /// splitting reader from writer in the first place — and spawns only
    /// the wait.
    pub async fn begin_call(
        &self,
        tool_name: &str,
        arguments: Option<Value>,
    ) -> Result<PendingCall> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel::<Value>();

        // Insert the oneshot BEFORE writing so an immediate-reply server
        // can't race us — the reader task would otherwise demux to an
        // empty map and drop the response.
        {
            let mut guard = self.pending.lock().await;
            guard.insert(id, response_tx);
        }

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments.unwrap_or(Value::Null),
            },
        });
        let mut serialized = match serde_json::to_vec(&request) {
            Ok(bytes) => bytes,
            Err(err) => {
                self.pending.lock().await.remove(&id);
                return Err(anyhow!("serialising request: {err}"));
            }
        };
        serialized.push(b'\n');

        {
            let mut writer = self.write_half.lock().await;
            if let Err(err) = writer.write_all(&serialized).await {
                self.pending.lock().await.remove(&id);
                return Err(anyhow!("writing request to local socket: {err}"));
            }
            if let Err(err) = writer.flush().await {
                self.pending.lock().await.remove(&id);
                return Err(anyhow!("flushing local socket: {err}"));
            }
        }

        Ok(PendingCall {
            id,
            pending: self.pending.clone(),
            response_rx,
        })
    }
}

/// An upstream request that has already been written, waiting only for its
/// reply. Deliberately `'static` — it borrows nothing from [`UnixMcpProxy`],
/// so the listener can drive it on its own task without pinning the proxy
/// alive past the connection.
pub struct PendingCall {
    id: i32,
    pending: ResponseMap,
    response_rx: oneshot::Receiver<Value>,
}

impl PendingCall {
    /// Await the reply. Performs no upstream I/O, so the order in which
    /// callers await their `PendingCall`s has no bearing on the order the
    /// editor executed the requests. Times out after 30 s.
    pub async fn finish(self) -> Result<Value> {
        let Self {
            id,
            pending,
            response_rx,
        } = self;
        match tokio::time::timeout(CALL_TIMEOUT, response_rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => {
                pending.lock().await.remove(&id);
                Err(anyhow!(
                    "local MCP socket reader closed before response arrived"
                ))
            }
            Err(_) => {
                pending.lock().await.remove(&id);
                Err(anyhow!(
                    "local MCP call timed out after {}s",
                    CALL_TIMEOUT.as_secs()
                ))
            }
        }
    }
}

impl Drop for UnixMcpProxy {
    fn drop(&mut self) {
        // Aborting the reader task is what unblocks any pending oneshot
        // senders (they're dropped → receivers see RecvError → callers
        // see "reader closed before response"). The write half is dropped
        // automatically when the Mutex is — its destructor closes the
        // socket, prompting the embedded server to tear down this
        // connection's subscription state. See
        // `context_server::listener::serve_connection::connections.remove`.
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
    }
}

async fn read_loop(
    read_half: tokio::net::unix::OwnedReadHalf,
    pending: ResponseMap,
    notifications_tx: NotificationSender,
    suppressed_kinds: SuppressedKinds,
) {
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                // EOF — embedded server closed the socket. Clearing
                // `pending` drops every oneshot sender, so any in-flight
                // caller wakes up with a "reader closed" error.
                let mut guard = pending.lock().await;
                guard.clear();
                return;
            }
            Ok(_) => {}
            Err(err) => {
                log::debug!(
                    target: "remote_control",
                    "local MCP read error: {err:#}",
                );
                let mut guard = pending.lock().await;
                guard.clear();
                return;
            }
        }
        let frame: Value = match serde_json::from_str(line.trim_end_matches('\n')) {
            Ok(value) => value,
            Err(err) => {
                log::warn!(
                    target: "remote_control",
                    "local MCP returned non-JSON frame: {err:#} (raw: {line:?})",
                );
                continue;
            }
        };

        let id = frame.get("id").and_then(|v| v.as_i64());
        if let Some(id) = id {
            // Response: route to oneshot. Cast i64 → i32 is safe — we
            // minted the id as i32 ourselves; if it came back as
            // something else it's an alien frame and we just drop it.
            if let Ok(id32) = i32::try_from(id) {
                let removed = {
                    let mut guard = pending.lock().await;
                    guard.remove(&id32)
                };
                if let Some(sender) = removed {
                    let _ = sender.send(frame);
                } else {
                    log::debug!(
                        target: "remote_control",
                        "local MCP response for unknown id {id32}; dropping",
                    );
                }
            } else {
                log::debug!(
                    target: "remote_control",
                    "local MCP response with non-i32 id {id}; dropping",
                );
            }
            continue;
        }

        // Notification (no `id` field). Per-connection `suppress_kinds` is
        // applied HERE — before the queue — so a suppressed kind costs this
        // connection neither a slot nor a coalesce comparison. The set is
        // empty unless THIS client asked for it on THIS socket.
        if suppressed_kinds.contains(notification_kind(&frame)) {
            continue;
        }

        // Enqueue it; the queue itself applies the coalesce / drop-oldest
        // policy documented on `NOTIFICATION_QUEUE_CAPACITY`, and never
        // blocks this reader.
        notifications_tx.send(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::net::UnixListener;

    /// Drive `read_loop` over a socket pair, feed it `frames`, and collect the
    /// notification kinds that survived to the per-connection queue.
    async fn forwarded_kinds(frames: &[Value], suppressed: SuppressedKinds) -> Vec<String> {
        let (client, mut server) = UnixStream::pair().expect("socketpair");
        let (read_half, _write_half) = client.into_split();
        let pending: ResponseMap = Arc::new(Mutex::new(HashMap::new()));
        let (notifications_tx, mut notifications_rx) = notification_channel();
        let reader = tokio::spawn(read_loop(read_half, pending, notifications_tx, suppressed));
        for frame in frames {
            let line = format!("{frame}\n");
            server
                .write_all(line.as_bytes())
                .await
                .expect("write frame");
        }
        server.flush().await.expect("flush");
        // Closing the write half is the EOF that ends `read_loop`, which drops
        // the sender and lets `recv` return `None` after the drain.
        drop(server);
        reader.await.expect("reader task");
        let mut kinds = Vec::new();
        while let Some(frame) = notifications_rx.recv().await {
            kinds.push(notification_kind(&frame).to_string());
        }
        kinds
    }

    fn suppression_frame(kind: &str, session_id: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "editor/notification",
            "params": { "kind": kind, "payload": { "session_id": session_id } },
        })
    }

    /// Suppression is scoped to the connection that asked for it. A second
    /// phone — or the desktop's own consumers — must still receive the kind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suppress_kinds_drops_only_that_kind_on_that_connection() {
        let frames = [
            suppression_frame("agent_session_message_appended", "s-1"),
            suppression_frame("agent_session_dirty", "s-1"),
            suppression_frame("agent_session_state_changed", "s-1"),
        ];

        let quiet = SuppressedKinds::default();
        quiet.set(vec!["agent_session_message_appended".to_string()]);
        assert_eq!(
            forwarded_kinds(&frames, quiet).await,
            vec![
                "agent_session_dirty".to_string(),
                "agent_session_state_changed".to_string(),
            ],
            "only the requested kind is dropped — a lost `dirty` is a stuck transcript"
        );

        let chatty = SuppressedKinds::default();
        assert_eq!(
            forwarded_kinds(&frames, chatty).await,
            vec![
                "agent_session_message_appended".to_string(),
                "agent_session_dirty".to_string(),
                "agent_session_state_changed".to_string(),
            ],
            "a connection that never asked keeps today's fan-out"
        );
    }

    /// Regression guard: the default is full fan-out, and it stays full even
    /// after a re-subscribe that omits `suppress_kinds`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suppress_kinds_absent_forwards_everything() {
        let frames = [
            suppression_frame("agent_session_message_appended", "s-1"),
            suppression_frame("upload_chunk_acked", "s-1"),
        ];
        let suppressed = SuppressedKinds::default();
        suppressed.set(vec!["agent_session_message_appended".to_string()]);
        // Last subscribe wins, and an empty list restores everything.
        suppressed.set(Vec::new());
        assert_eq!(
            forwarded_kinds(&frames, suppressed).await,
            vec![
                "agent_session_message_appended".to_string(),
                "upload_chunk_acked".to_string(),
            ],
        );
    }

    /// Stand up an in-test Unix-socket "server" that echoes a canned
    /// response for any `tools/call` it sees, and interleaves a
    /// notification before the response. Validates that:
    ///
    /// 1. Responses are demuxed to the right `id`.
    /// 2. Notifications interleaved during a call don't get routed to
    ///    a oneshot.
    /// 3. Both are observable after the call returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn demuxes_notification_during_call() {
        let dir = tempdir().expect("tempdir");
        let socket = dir.path().join("test.sock");
        let listener = UnixListener::bind(&socket).expect("bind");

        // Spawn a fake server that, on connect:
        // 1. Reads one request line.
        // 2. Sends one notification frame.
        // 3. Sends a canned response with the SAME id the client used.
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let (read_half, mut write_half) = stream.split();
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("read request");
            let parsed: Value =
                serde_json::from_str(line.trim_end_matches('\n')).expect("parse request");
            let id = parsed["id"].as_i64().expect("request has id");

            let notification = json!({
                "jsonrpc": "2.0",
                "method": "editor/notification",
                "params": { "kind": "agent_session_message_appended", "payload": {} },
            });
            write_half
                .write_all(format!("{}\n", notification).as_bytes())
                .await
                .expect("write notification");

            let response = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "structuredContent": { "echoed_id": id } },
            });
            write_half
                .write_all(format!("{}\n", response).as_bytes())
                .await
                .expect("write response");
            write_half.flush().await.ok();
            // Hold the stream open so the client's reader doesn't see EOF
            // before we've finished asserting on the notification side.
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        // Connect a raw UnixStream and build an UnixMcpProxy around it
        // using a private constructor — but the public `connect()` is
        // hard-wired to `editor_mcp::socket_path()`, so we instead
        // construct an UnixMcpProxy by hand here. Lift the wiring into
        // a `connect_to_path` helper if more tests need it.
        let stream = UnixStream::connect(&socket).await.expect("connect");
        let (read_half, write_half) = stream.into_split();
        let pending: ResponseMap = Arc::new(Mutex::new(HashMap::new()));
        let (notifications_tx, mut notifications_rx) = notification_channel();
        let suppressed_kinds = SuppressedKinds::default();
        let reader_task = tokio::spawn(read_loop(
            read_half,
            pending.clone(),
            notifications_tx,
            suppressed_kinds.clone(),
        ));
        let proxy = UnixMcpProxy {
            write_half: Mutex::new(write_half),
            pending,
            notifications_rx: None,
            next_id: AtomicI32::new(1),
            reader_task: Some(reader_task),
            suppressed_kinds,
        };

        let response = proxy
            .begin_call("editor.capabilities", None)
            .await
            .expect("begin ok")
            .finish()
            .await
            .expect("call ok");
        assert_eq!(response["id"].as_i64(), Some(1));
        assert_eq!(
            response.pointer("/result/structuredContent/echoed_id"),
            Some(&Value::from(1)),
        );

        // Notification should be queued and observable.
        let notification =
            tokio::time::timeout(Duration::from_millis(500), notifications_rx.recv())
                .await
                .expect("notification within 500ms")
                .expect("channel still open");
        assert_eq!(
            notification
                .pointer("/params/kind")
                .and_then(|v| v.as_str()),
            Some("agent_session_message_appended"),
        );

        drop(proxy);
        let _ = server_handle.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn call_times_out_when_server_silent() {
        let dir = tempdir().expect("tempdir");
        let socket = dir.path().join("silent.sock");
        let listener = UnixListener::bind(&socket).expect("bind");

        let server_handle = tokio::spawn(async move {
            let (mut _stream, _) = listener.accept().await.expect("accept");
            // Hold the connection open without ever responding; the
            // client's call() must time out.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let stream = UnixStream::connect(&socket).await.expect("connect");
        let (read_half, write_half) = stream.into_split();
        let pending: ResponseMap = Arc::new(Mutex::new(HashMap::new()));
        let (notifications_tx, _notifications_rx) = notification_channel();
        let suppressed_kinds = SuppressedKinds::default();
        let reader_task = tokio::spawn(read_loop(
            read_half,
            pending.clone(),
            notifications_tx,
            suppressed_kinds.clone(),
        ));

        // Shorten the timeout for this test by patching: we can't change
        // the const, so instead we issue the call against a proxy where
        // we manually replace the timeout. The simplest path is to wrap
        // the call_tool future in a smaller timeout and assert the
        // error variant either way — both "local socket timed out" and
        // "tokio timeout" are acceptable. We bound at 200ms.
        let proxy = UnixMcpProxy {
            write_half: Mutex::new(write_half),
            pending,
            notifications_rx: None,
            next_id: AtomicI32::new(1),
            reader_task: Some(reader_task),
            suppressed_kinds,
        };
        let call = proxy
            .begin_call("editor.capabilities", None)
            .await
            .expect("begin writes even against a silent server");
        let result = tokio::time::timeout(Duration::from_millis(200), call.finish()).await;
        assert!(
            result.is_err(),
            "finish should not return within 200ms when the server is silent"
        );

        drop(proxy);
        let _ = server_handle.await;
    }

    fn notification(kind: &str, payload: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "editor/notification",
            "params": { "kind": kind, "payload": payload },
        })
    }

    fn dirty(session_id: &str, seq: u64) -> Value {
        notification(
            "agent_session_dirty",
            json!({ "session_id": session_id, "current_seq": seq }),
        )
    }

    /// The queue's whole reason to exist: on overflow the OLDEST frame goes,
    /// not the newest. Before the fix the `try_send`-on-a-tokio-mpsc reader
    /// dropped the incoming frame, which is exactly backwards — the newest
    /// `agent_session_dirty` is the one carrying the highest `current_seq`,
    /// and losing it strands the client on a stale convergence target.
    #[tokio::test]
    async fn overflow_drops_the_oldest_frame_not_the_newest() {
        let (tx, mut rx) = notification_channel();
        // Distinct kinds so nothing coalesces; `agent_session_title_changed`
        // is forwarded but carries no coalescing identity.
        for i in 0..NOTIFICATION_QUEUE_CAPACITY {
            tx.send(notification(
                "agent_session_title_changed",
                json!({ "n": i }),
            ));
        }
        tx.send(notification(
            "agent_session_title_changed",
            json!({ "n": "newest" }),
        ));

        let first = rx.recv().await.expect("queue non-empty");
        assert_eq!(
            first.pointer("/params/payload/n"),
            Some(&json!(1)),
            "frame 0 should have been evicted, leaving 1 at the front"
        );
        let mut last = first;
        for _ in 1..NOTIFICATION_QUEUE_CAPACITY {
            last = rx.recv().await.expect("queue non-empty");
        }
        assert_eq!(
            last.pointer("/params/payload/n"),
            Some(&json!("newest")),
            "the newest frame must survive the overflow"
        );
    }

    /// `agent_session_dirty` carries the session's CURRENT `change_seq`, so
    /// the poke with the higher seq supersedes the other for the same
    /// session. The survivor is enqueued at its own arrival position, NOT
    /// in the superseded frame's slot — overwriting in place would deliver
    /// it ahead of frames that genuinely arrived before it.
    #[tokio::test]
    async fn dirty_pokes_coalesce_per_session_without_reordering() {
        let (tx, mut rx) = notification_channel();
        tx.send(dirty("session-a", 1));
        tx.send(dirty("session-b", 10));
        tx.send(dirty("session-a", 2));
        tx.send(dirty("session-a", 3));

        // session-b arrived before the surviving session-a poke, so it must
        // still be delivered first.
        let first = rx.recv().await.expect("b");
        assert_eq!(
            first.pointer("/params/payload/session_id"),
            Some(&json!("session-b"))
        );
        let second = rx.recv().await.expect("a");
        assert_eq!(
            second.pointer("/params/payload/session_id"),
            Some(&json!("session-a"))
        );
        assert_eq!(
            second.pointer("/params/payload/current_seq"),
            Some(&json!(3)),
            "session-a must be represented by its newest seq"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "the two superseded session-a pokes must not be delivered separately"
        );
    }

    /// Coalescing keeps the higher `current_seq`, not simply the last
    /// arrival. Emits are single-threaded on GPUI today so the two coincide,
    /// but this queue must not silently depend on that.
    #[tokio::test]
    async fn coalescing_keeps_the_higher_seq_even_if_it_arrived_first() {
        let (tx, mut rx) = notification_channel();
        tx.send(dirty("session-a", 9));
        tx.send(dirty("session-a", 4));

        let only = rx.recv().await.expect("one frame");
        assert_eq!(only.pointer("/params/payload/current_seq"), Some(&json!(9)));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
        );
    }

    /// A lost `upload_chunk_acked` costs the client a 30 s ack timeout and a
    /// Paused upload — there is no self-healing path — so overflow eviction
    /// steps over upload frames as long as anything else is evictable.
    #[tokio::test]
    async fn overflow_spares_upload_frames() {
        let (tx, mut rx) = notification_channel();
        tx.send(notification(
            "upload_chunk_acked",
            json!({ "upload_id": 7 }),
        ));
        for i in 0..(NOTIFICATION_QUEUE_CAPACITY - 1) {
            tx.send(notification(
                "agent_session_title_changed",
                json!({ "n": i }),
            ));
        }
        tx.send(notification(
            "agent_session_title_changed",
            json!({ "n": "newest" }),
        ));

        let first = rx.recv().await.expect("queue non-empty");
        assert_eq!(
            first.pointer("/params/kind"),
            Some(&json!("upload_chunk_acked")),
            "the upload ack sat at the front but must not be the eviction victim"
        );
    }

    /// `recv` must observe the producer going away, or the WS notification
    /// pump would park forever on a dead proxy.
    #[tokio::test]
    async fn recv_returns_none_once_the_producer_is_gone() {
        let (tx, mut rx) = notification_channel();
        tx.send(notification("agent_session_title_changed", json!({})));
        drop(tx);
        assert!(rx.recv().await.is_some());
        assert!(rx.recv().await.is_none());
    }

    /// H1, at the layer where the ordering actually happens: the upstream
    /// write mutex. `begin_call` is the commit point, and awaiting it in
    /// wire order is what makes the editor see requests in wire order —
    /// even though every reply is then awaited on its own task and the
    /// server answers in reverse.
    ///
    /// Driving the whole `begin_call` + `finish` pair on a task instead
    /// (which is what the listener used to do with `call_tool`) lets the
    /// tasks race for `write_half`, and tokio's LIFO slot makes the newest
    /// one win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn begin_call_commits_in_caller_order_while_replies_are_awaited_concurrently() {
        const CALLS: i64 = 16;
        let dir = tempdir().expect("tempdir");
        let socket = dir.path().join("order.sock");
        let listener = UnixListener::bind(&socket).expect("bind");

        // Fake editor: records the order requests ARRIVE, then answers them
        // all at once in reverse, so reply order can never mask a commit
        // reorder.
        let (arrived_tx, arrived_rx) = oneshot::channel::<Vec<i64>>();
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let (read_half, mut write_half) = stream.split();
            let mut reader = BufReader::new(read_half);
            let mut arrived = Vec::new();
            for _ in 0..CALLS {
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("read request");
                let parsed: Value =
                    serde_json::from_str(line.trim_end_matches('\n')).expect("parse request");
                arrived.push(parsed["id"].as_i64().expect("request has id"));
            }
            let _ = arrived_tx.send(arrived.clone());
            for id in arrived.into_iter().rev() {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "structuredContent": { "echoed_id": id } },
                });
                write_half
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .expect("write response");
            }
            write_half.flush().await.ok();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let stream = UnixStream::connect(&socket).await.expect("connect");
        let (read_half, write_half) = stream.into_split();
        let pending: ResponseMap = Arc::new(Mutex::new(HashMap::new()));
        let (notifications_tx, _notifications_rx) = notification_channel();
        let suppressed_kinds = SuppressedKinds::default();
        let reader_task = tokio::spawn(read_loop(
            read_half,
            pending.clone(),
            notifications_tx,
            suppressed_kinds.clone(),
        ));
        let proxy = UnixMcpProxy {
            write_half: Mutex::new(write_half),
            pending,
            notifications_rx: None,
            next_id: AtomicI32::new(1),
            reader_task: Some(reader_task),
            suppressed_kinds,
        };

        // Exactly the listener's shape: commit inline, spawn only the wait.
        let mut waits = Vec::new();
        for _ in 0..CALLS {
            let call = proxy
                .begin_call("editor.capabilities", None)
                .await
                .expect("begin");
            waits.push(tokio::spawn(async move { call.finish().await }));
        }

        let arrived = tokio::time::timeout(Duration::from_secs(5), arrived_rx)
            .await
            .expect("server saw every request")
            .expect("server reported arrivals");
        assert_eq!(
            arrived,
            (1..=CALLS).collect::<Vec<_>>(),
            "upstream must see requests in the order begin_call was awaited, got {arrived:?}"
        );

        let mut answered = Vec::new();
        for wait in waits {
            let value = wait.await.expect("join").expect("response");
            answered.push(value["id"].as_i64().expect("id"));
        }
        answered.sort_unstable();
        assert_eq!(answered, (1..=CALLS).collect::<Vec<_>>());

        drop(proxy);
        let _ = server_handle.await;
    }
}
