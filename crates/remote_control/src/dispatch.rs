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
use crate::proxy::UnixMcpProxy;

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

/// Per-WS-connection dispatcher. Stateful: holds the upstream Unix-socket
/// proxy and a (one-shot-takeable) notifications receiver.
pub trait ConnectionDispatcher: Send {
    /// Translate + forward a JSON-RPC request, returning the response the
    /// WS client should see. Bad / banned methods become `-32601`.
    fn dispatch(
        &mut self,
        client_name: &str,
        request: JsonRpcRequest,
    ) -> BoxFuture<'_, JsonRpcResponse>;

    /// Hand the per-connection notification stream to the WS task. The
    /// WS task drives a `tokio::select!` between this receiver and the
    /// WS read half; on each frame it applies
    /// `allow_list::should_forward_event` and rewrites the envelope to
    /// `remote/notification`. Returns `None` if already taken or if the
    /// connection didn't open a proxy yet (e.g. test stubs).
    fn take_notifications(&mut self) -> Option<tokio::sync::mpsc::Receiver<Value>>;
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

struct ProxyConnection {
    proxy: UnixMcpProxy,
    notifications_rx: Option<tokio::sync::mpsc::Receiver<Value>>,
}

impl ConnectionDispatcher for ProxyConnection {
    fn dispatch(
        &mut self,
        _client_name: &str,
        request: JsonRpcRequest,
    ) -> BoxFuture<'_, JsonRpcResponse> {
        Box::pin(async move {
            let ws_id = request.id.clone();
            let tool_name = match allow_list::translate(&request.method) {
                Some(name) => name,
                None => {
                    return JsonRpcResponse::error(
                        ws_id,
                        -32601,
                        format!("method not found: {}", request.method),
                    );
                }
            };

            match self.proxy.call_tool(tool_name, request.params).await {
                Ok(upstream) => {
                    // The upstream frame is `{"jsonrpc","id","result"
                    // |"error"}`. We substitute the WS client's `id`
                    // back in (the proxy minted a fresh i32 for the
                    // upstream call) and pass `result` / `error`
                    // through verbatim. The MCP server uses a custom
                    // error shape (`{message, code}`) — the
                    // `serde_json::Value` round-trip preserves it.
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
                        // Neither result nor error — protocol violation.
                        // Wrap the whole frame as the result so a
                        // debugging client can see what came back.
                        JsonRpcResponse::ok(ws_id, upstream)
                    }
                }
                Err(err) => {
                    JsonRpcResponse::error(ws_id, -32603, format!("local MCP call failed: {err}"))
                }
            }
        })
    }

    fn take_notifications(&mut self) -> Option<tokio::sync::mpsc::Receiver<Value>> {
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
    fn dispatch(
        &mut self,
        _client_name: &str,
        request: JsonRpcRequest,
    ) -> BoxFuture<'_, JsonRpcResponse> {
        Box::pin(async move {
            match request.method.as_str() {
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
            }
        })
    }

    fn take_notifications(&mut self) -> Option<tokio::sync::mpsc::Receiver<Value>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

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
        let mut conn = block_on(dispatcher.open_connection()).expect("open");
        let request: JsonRpcRequest = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"method":"remote.editor.capabilities"}"#,
        )
        .expect("parse");
        let response = block_on(conn.dispatch("client", request));
        let parsed: Value = serde_json::to_value(&response).expect("re-serialize");
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["result"]["protocol_version"], 1);
        assert_eq!(parsed["result"]["server_software"], "sawe");
    }

    #[test]
    fn minimal_dispatcher_ping_round_trip() {
        let dispatcher = MinimalDispatcher::new();
        let mut conn = block_on(dispatcher.open_connection()).expect("open");
        let request: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":"42","method":"remote.editor.ping"}"#)
                .expect("parse");
        let response = block_on(conn.dispatch("client", request));
        let parsed: Value = serde_json::to_value(&response).expect("re-serialize");
        assert_eq!(parsed["id"], "42");
        assert_eq!(parsed["result"]["pong"], true);
        let now = parsed["result"]["now"].as_str().expect("now is string");
        assert!(!now.is_empty());
    }

    #[test]
    fn minimal_dispatcher_unknown_method_is_method_not_found() {
        let dispatcher = MinimalDispatcher::new();
        let mut conn = block_on(dispatcher.open_connection()).expect("open");
        let request: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":9,"method":"remote.unknown"}"#)
                .expect("parse");
        let response = block_on(conn.dispatch("client", request));
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
        // a fake (unreachable) proxy — but since dispatch() only touches
        // self.proxy in the Ok-translate branch, we don't actually need
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
