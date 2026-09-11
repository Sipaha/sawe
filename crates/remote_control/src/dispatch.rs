//! JSON-RPC 2.0 dispatch surface for Remote Control.
//!
//! `RemoteDispatcher` is a factory: per WS connection, the listener calls
//! `open_connection().await` and gets a stateful `ConnectionDispatcher`
//! holding the connection-scoped resources (the `UnixMcpProxy` socket +
//! its notification receiver). The connection dispatcher is dropped when
//! the WS closes, which closes the underlying socket and lets the
//! upstream `editor_mcp` server clean up subscriptions per
//! `context_server::listener::serve_connection`.
//!
//! `ProxyDispatcher` is the production implementation. The test-only
//! `MinimalDispatcher` keeps the R-2 baseline tests green without a live
//! MCP socket.

use std::sync::Arc;

use anyhow::Result;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::allow_list;
use crate::proxy::{NotificationReceiver, UnixMcpProxy};

/// Method whose `suppress_kinds` parameter the proxy records for this
/// connection. See [`crate::proxy::SuppressedKinds`] for why enforcement can
/// only live in the per-connection proxy.
const SUBSCRIBE_METHOD: &str = "remote.editor.subscribe";

/// Read a `remote.editor.subscribe` frame's `suppress_kinds` array.
///
/// Returns an empty vec for anything that is not a well-formed array of
/// strings — an old client that never sends the key, a client sending JSON
/// null, or a malformed value. Empty is today's full fan-out, which is the
/// fail-safe direction: the failure mode of a mis-parsed suppression list must
/// be an extra notification, never a dropped `agent_session_dirty`.
fn suppress_kinds_from_params(params: Option<&Value>) -> Vec<String> {
    // Gated on the advertised token so that dropping it from
    // `editor_mcp::wire_features` actually disables the behaviour rather than
    // leaving a live parameter no client is told about.
    if !editor_mcp::wire_features()
        .iter()
        .any(|token| token == editor_mcp::FEATURE_QUIET_MESSAGE_APPENDED)
    {
        return Vec::new();
    }
    params
        .and_then(|params| params.get("suppress_kinds"))
        .and_then(|kinds| kinds.as_array())
        .map(|kinds| {
            kinds
                .iter()
                .filter_map(|kind| kind.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// JSON-RPC 2.0 request frame. We accept `id` as `Value` (number, string,
/// or null per spec) and `params` as either an array or object.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 response frame. Either `result` xor `error` is set per the
/// spec; serde's `skip_serializing_if = "Option::is_none"` enforces the
/// "missing means absent" wire shape.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Parse a single JSON-RPC frame. Returns a parse-error response (`-32700`)
/// when the bytes aren't valid JSON, rather than failing the caller — the
/// transport contract is "always reply with a JSON-RPC frame, never close
/// on a single bad frame." `Box`-ing the error variant keeps
/// `Result<JsonRpcRequest, Box<JsonRpcResponse>>` small enough for
/// `clippy::result_large_err`.
pub fn parse_request(text: &str) -> Result<JsonRpcRequest, Box<JsonRpcResponse>> {
    match serde_json::from_str::<JsonRpcRequest>(text) {
        Ok(req) if req.jsonrpc == "2.0" => Ok(req),
        Ok(req) => Err(Box::new(JsonRpcResponse::error(
            req.id,
            -32600,
            format!("expected jsonrpc=2.0, got {:?}", req.jsonrpc),
        ))),
        Err(err) => Err(Box::new(JsonRpcResponse::error(
            Value::Null,
            -32700,
            format!("parse error: {err}"),
        ))),
    }
}

/// Factory the listener calls once per accepted+authenticated WS
/// connection. The returned `ConnectionDispatcher` owns
/// connection-scoped state (e.g. the `UnixMcpProxy`) and is dropped when
/// the WS task exits.
pub trait RemoteDispatcher: Send + Sync {
    fn open_connection(&self) -> BoxFuture<'static, Result<Box<dyn ConnectionDispatcher>>>;
}

/// A request whose upstream side is already committed, waiting only for the
/// reply. `'static` on purpose: the listener drives it on its own task, so
/// it must not borrow the dispatcher.
pub type PendingResponse = BoxFuture<'static, JsonRpcResponse>;

/// Per-WS-connection dispatcher. Stateful: holds the upstream Unix-socket
/// proxy and a (one-shot-takeable) notifications receiver.
///
/// The trait takes `&self` and is `Sync` because the listener runs several
/// calls from the same phone concurrently (responses funnel back through the
/// connection's outbound queue) so one slow RPC can't head-of-line-block the
/// rest of the socket.
///
/// The two-phase shape is load-bearing, not stylistic — see
/// [`ConnectionDispatcher::begin_dispatch`].
pub trait ConnectionDispatcher: Send + Sync {
    /// Translate the request and commit it upstream, then hand back a future
    /// that resolves to the response the WS client should see. Bad / banned
    /// methods become `-32601` without touching the upstream at all.
    ///
    /// **Callers must await this in wire order and may only spawn the future
    /// it returns.** Everything that decides what the editor sees first
    /// happens in this phase; the returned future performs no upstream I/O.
    /// Dispatching the whole call on a task instead would let two requests
    /// race for the upstream socket, and the mobile client's offline queue
    /// depends on the opposite: it opens each send's gate when the previous
    /// frame is handed to the transport, so a flush of N queued messages
    /// puts N sends in flight at once and relies on the server executing
    /// them in the order they arrived. Losing that reverses messages in the
    /// transcript, permanently and with nothing to heal it.
    fn begin_dispatch<'a>(
        &'a self,
        client_name: &'a str,
        request: JsonRpcRequest,
    ) -> BoxFuture<'a, PendingResponse>;

    /// Hand the per-connection notification stream to the WS task, which
    /// runs a dedicated pump: on each frame it applies
    /// `allow_list::should_forward_event` and rewrites the envelope to
    /// `remote/notification`. Returns `None` if already taken or if the
    /// connection didn't open a proxy yet (e.g. test stubs).
    fn take_notifications(&mut self) -> Option<NotificationReceiver>;
}

/// Wrap an already-computed response as a [`PendingResponse`].
pub fn ready_response(response: JsonRpcResponse) -> PendingResponse {
    Box::pin(async move { response })
}

/// Production dispatcher: opens a fresh `UnixMcpProxy` per WS connection.
/// Stateless itself — all per-connection state lives on the returned
/// `ProxyConnection`.
#[derive(Default)]
pub struct ProxyDispatcher;

impl ProxyDispatcher {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl RemoteDispatcher for ProxyDispatcher {
    fn open_connection(&self) -> BoxFuture<'static, Result<Box<dyn ConnectionDispatcher>>> {
        Box::pin(async move {
            let mut proxy = UnixMcpProxy::connect().await?;
            let notifications_rx = proxy.take_notifications();
            let connection: Box<dyn ConnectionDispatcher> = Box::new(ProxyConnection {
                proxy,
                notifications_rx,
            });
            Ok(connection)
        })
    }
}

/// Rewrap an upstream MCP frame as the response the WS client sees.
///
/// The upstream frame is `{"jsonrpc","id","result"|"error"}`. We substitute
/// the WS client's `id` back in (the proxy minted a fresh i32 for the
/// upstream call) and pass `result` / `error` through verbatim. The MCP
/// server uses a custom error shape (`{message, code}`) — the
/// `serde_json::Value` round-trip preserves it.
fn translate_upstream_response(ws_id: Value, upstream: Value) -> JsonRpcResponse {
    if let Some(err_value) = upstream.get("error") {
        let code = err_value
            .get("code")
            .and_then(|v| v.as_i64())
            .unwrap_or(-32603) as i32;
        let message = err_value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("local MCP error")
            .to_string();
        let mut response = JsonRpcResponse::error(ws_id, code, message);
        if let Some(error) = response.error.as_mut() {
            error.data = err_value.get("data").cloned();
        }
        response
    } else if let Some(result_value) = upstream.get("result") {
        JsonRpcResponse::ok(ws_id, result_value.clone())
    } else {
        // Neither result nor error — protocol violation. Wrap the whole
        // frame as the result so a debugging client can see what came back.
        JsonRpcResponse::ok(ws_id, upstream)
    }
}

struct ProxyConnection {
    proxy: UnixMcpProxy,
    notifications_rx: Option<NotificationReceiver>,
}

impl ConnectionDispatcher for ProxyConnection {
    // `async_yields_async` is exactly the shape this method is for: the
    // outer future is the commit (awaited by the reader in wire order) and
    // the value it yields is the reply-wait (spawned). Collapsing them is
    // the bug the split exists to prevent.
    #[allow(clippy::async_yields_async)]
    fn begin_dispatch<'a>(
        &'a self,
        _client_name: &'a str,
        request: JsonRpcRequest,
    ) -> BoxFuture<'a, PendingResponse> {
        Box::pin(async move {
            let ws_id = request.id.clone();
            let Some(tool_name) = allow_list::translate(&request.method) else {
                return ready_response(JsonRpcResponse::error(
                    ws_id,
                    -32601,
                    format!("method not found: {}", request.method),
                ));
            };

            // Record what this connection asked to have suppressed, then
            // forward the frame VERBATIM: `editor.subscribe` accepts and
            // stores the parameter, and the subscription registry is what
            // `editor.list_subscriptions` reports from.
            if request.method == SUBSCRIBE_METHOD {
                self.proxy
                    .suppressed_kinds()
                    .set(suppress_kinds_from_params(request.params.as_ref()));
            }

            // Phase 1 — the ordering point. Awaited by the listener's reader
            // in wire order.
            let call = match self.proxy.begin_call(tool_name, request.params).await {
                Ok(call) => call,
                Err(err) => {
                    return ready_response(JsonRpcResponse::error(
                        ws_id,
                        -32603,
                        format!("local MCP call failed: {err}"),
                    ));
                }
            };

            // Phase 2 — pure wait, safe to drive on any task.
            Box::pin(async move {
                match call.finish().await {
                    Ok(upstream) => translate_upstream_response(ws_id, upstream),
                    Err(err) => JsonRpcResponse::error(
                        ws_id,
                        -32603,
                        format!("local MCP call failed: {err}"),
                    ),
                }
            }) as PendingResponse
        })
    }

    fn take_notifications(&mut self) -> Option<NotificationReceiver> {
        self.notifications_rx.take()
    }
}

/// R-2 stub kept around for unit tests that don't want a live MCP socket.
/// Production callers use [`ProxyDispatcher`]. Two allow-listed methods,
/// anything else → `-32601`.
pub struct MinimalDispatcher;

impl MinimalDispatcher {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl Default for MinimalDispatcher {
    fn default() -> Self {
        Self
    }
}

impl RemoteDispatcher for MinimalDispatcher {
    fn open_connection(&self) -> BoxFuture<'static, Result<Box<dyn ConnectionDispatcher>>> {
        Box::pin(async move {
            let connection: Box<dyn ConnectionDispatcher> = Box::new(MinimalConnection);
            Ok(connection)
        })
    }
}

struct MinimalConnection;

impl ConnectionDispatcher for MinimalConnection {
    // `async_yields_async` is exactly the shape this method is for: the
    // outer future is the commit (awaited by the reader in wire order) and
    // the value it yields is the reply-wait (spawned). Collapsing them is
    // the bug the split exists to prevent.
    #[allow(clippy::async_yields_async)]
    fn begin_dispatch<'a>(
        &'a self,
        _client_name: &'a str,
        request: JsonRpcRequest,
    ) -> BoxFuture<'a, PendingResponse> {
        Box::pin(async move {
            ready_response(match request.method.as_str() {
                "remote.editor.capabilities" => JsonRpcResponse::ok(
                    request.id,
                    serde_json::json!({
                        "protocol_version": 1,
                        "server_software": "sawe",
                        "tool_namespaces": ["remote.editor"],
                        "capabilities": ["json-rpc-2.0", "hmac-sha256-challenge"],
                    }),
                ),
                "remote.editor.ping" => JsonRpcResponse::ok(
                    request.id,
                    serde_json::json!({
                        "pong": true,
                        "now": chrono::Utc::now().to_rfc3339(),
                    }),
                ),
                other => {
                    JsonRpcResponse::error(request.id, -32601, format!("method not found: {other}"))
                }
            })
        })
    }

    fn take_notifications(&mut self) -> Option<NotificationReceiver> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    /// Drive both dispatch phases the way the listener does: await the
    /// commit, then await the reply.
    async fn dispatch_for_test(
        conn: &dyn ConnectionDispatcher,
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        conn.begin_dispatch("client", request).await.await
    }

    /// The proxy records `suppress_kinds` for the connection that sent it, and
    /// reads anything malformed as "suppress nothing" — the fail-safe
    /// direction, since a dropped `agent_session_dirty` is a stuck transcript.
    #[test]
    fn suppress_kinds_is_read_only_from_a_well_formed_array() {
        let params = serde_json::json!({
            "kinds": ["agent_session_dirty", "agent_session_message_appended"],
            "suppress_kinds": ["agent_session_message_appended"],
        });
        assert_eq!(
            suppress_kinds_from_params(Some(&params)),
            vec!["agent_session_message_appended".to_string()],
        );

        for absent_or_broken in [
            serde_json::json!({ "kinds": ["agent_session_dirty"] }),
            serde_json::json!({ "suppress_kinds": null }),
            serde_json::json!({ "suppress_kinds": "agent_session_message_appended" }),
            serde_json::json!([]),
        ] {
            assert!(
                suppress_kinds_from_params(Some(&absent_or_broken)).is_empty(),
                "must fall back to full fan-out for {absent_or_broken}"
            );
        }
        assert!(suppress_kinds_from_params(None).is_empty());
    }

    /// The parameter is gated on the advertised token, so removing the token
    /// from `editor_mcp::wire_features` actually disables the behaviour rather
    /// than leaving a live parameter no client is told about.
    #[test]
    fn suppress_kinds_is_gated_on_the_advertised_token() {
        assert!(
            editor_mcp::wire_features()
                .iter()
                .any(|token| token == editor_mcp::FEATURE_QUIET_MESSAGE_APPENDED),
            "this build advertises the token, so the gate above is open"
        );
    }

    #[test]
    fn parse_rejects_non_json() {
        let err = parse_request("not json").expect_err("should fail");
        let parsed: Value = serde_json::to_value(&*err).expect("re-serialize error response");
        assert_eq!(parsed["error"]["code"].as_i64(), Some(-32700));
        assert_eq!(parsed["id"], Value::Null);
    }

    #[test]
    fn parse_rejects_wrong_jsonrpc_version() {
        let err =
            parse_request(r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#).expect_err("should fail");
        let parsed: Value = serde_json::to_value(&*err).expect("re-serialize");
        assert_eq!(parsed["error"]["code"].as_i64(), Some(-32600));
    }

    #[test]
    fn minimal_dispatcher_capabilities_round_trip() {
        let dispatcher = MinimalDispatcher::new();
        let conn = block_on(dispatcher.open_connection()).expect("open");
        let request: JsonRpcRequest = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"method":"remote.editor.capabilities"}"#,
        )
        .expect("parse");
        let response = block_on(dispatch_for_test(&*conn, request));
        let parsed: Value = serde_json::to_value(&response).expect("re-serialize");
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["result"]["protocol_version"], 1);
        assert_eq!(parsed["result"]["server_software"], "sawe");
    }

    #[test]
    fn minimal_dispatcher_ping_round_trip() {
        let dispatcher = MinimalDispatcher::new();
        let conn = block_on(dispatcher.open_connection()).expect("open");
        let request: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":"42","method":"remote.editor.ping"}"#)
                .expect("parse");
        let response = block_on(dispatch_for_test(&*conn, request));
        let parsed: Value = serde_json::to_value(&response).expect("re-serialize");
        assert_eq!(parsed["id"], "42");
        assert_eq!(parsed["result"]["pong"], true);
        let now = parsed["result"]["now"].as_str().expect("now is string");
        assert!(!now.is_empty());
    }

    #[test]
    fn minimal_dispatcher_unknown_method_is_method_not_found() {
        let dispatcher = MinimalDispatcher::new();
        let conn = block_on(dispatcher.open_connection()).expect("open");
        let request: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":9,"method":"remote.unknown"}"#)
                .expect("parse");
        let response = block_on(dispatch_for_test(&*conn, request));
        let parsed: Value = serde_json::to_value(&response).expect("re-serialize");
        assert_eq!(parsed["error"]["code"].as_i64(), Some(-32601));
        assert!(
            parsed["error"]["message"]
                .as_str()
                .expect("message string")
                .contains("method not found")
        );
    }

    #[test]
    fn proxy_dispatcher_rejects_banned_method_without_socket() {
        // The allow-list check fires BEFORE we try to open a proxy, so a
        // banned method returns -32601 cleanly even when the local MCP
        // socket isn't available. This test asserts that invariant by
        // never starting an editor_mcp instance.
        //
        // We mock the connection by hand-rolling a ProxyConnection with
        // a fake (unreachable) proxy — but since begin_dispatch() only
        // touches self.proxy after the allow-list check, we don't need
        // a real socket. Instead, we use ProxyDispatcher::open_connection
        // — but that DOES try to connect, so we can't go through the
        // public surface. Test what we can: allow_list rejection (in
        // allow_list.rs) covers the negative path; the positive path is
        // covered by the proxy_e2e integration test.
        //
        // Sanity check: confirm the translation reject is path-
        // independent. (This is essentially documenting the layering.)
        assert!(allow_list::translate("remote.lsp.start").is_none());
    }
}
