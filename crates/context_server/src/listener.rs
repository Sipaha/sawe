use ::serde::{Deserialize, Serialize};
use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::AsyncReadExt;
use futures::stream::StreamExt;
use futures::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, FutureExt,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
    io::BufReader,
    select_biased,
};
use gpui::{App, AppContext, AsyncApp, Task};
use net::async_net::{UnixListener, UnixStream};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{json, value::RawValue};
use std::{
    any::TypeId,
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};
use util::ResultExt;

use crate::{
    client::{CspResult, RequestId, Response},
    types::{
        CallToolParams, CallToolResponse, Implementation, InitializeResponse,
        LATEST_PROTOCOL_VERSION, ListToolsResponse, ProtocolVersion, Request, ServerCapabilities,
        Tool, ToolAnnotations, ToolResponseContent, ToolsCapabilities,
        requests::{CallTool, Initialize, ListTools, Ping},
    },
};

pub struct McpServer {
    socket_path: PathBuf,
    tools: Rc<RefCell<HashMap<&'static str, RegisteredTool>>>,
    handlers: Rc<RefCell<HashMap<&'static str, RequestHandler>>>,
    connections: Rc<RefCell<HashMap<u64, UnboundedSender<String>>>>,
    #[allow(dead_code)]
    next_connection_id: Rc<RefCell<u64>>,
    _server_task: Task<()>,
}

struct RegisteredTool {
    tool: Tool,
    handler: ToolHandler,
}

type ToolHandler = Box<
    dyn Fn(
        Option<serde_json::Value>,
        &mut AsyncApp,
    ) -> Task<Result<ToolResponse<serde_json::Value>>>,
>;
type RequestHandler = Box<dyn Fn(RequestId, Option<Box<RawValue>>, &App) -> Task<String>>;

impl McpServer {
    pub fn new(cx: &AsyncApp) -> Task<Result<Self>> {
        let task = cx.background_spawn(async move {
            let temp_dir = tempfile::Builder::new().prefix("zed-mcp").tempdir()?;
            let socket_path = temp_dir.path().join("mcp.sock");
            let listener = UnixListener::bind(&socket_path).context("creating mcp socket")?;

            anyhow::Ok((temp_dir, socket_path, listener))
        });

        cx.spawn(async move |cx| {
            let (temp_dir, socket_path, listener) = task.await?;
            let tools = Rc::new(RefCell::new(HashMap::default()));
            let handlers = Rc::new(RefCell::new(HashMap::default()));
            let connections = Rc::new(RefCell::new(HashMap::default()));
            let next_connection_id = Rc::new(RefCell::new(0u64));
            let server_task = cx.spawn({
                let tools = tools.clone();
                let handlers = handlers.clone();
                let connections = connections.clone();
                let next_connection_id = next_connection_id.clone();
                async move |cx| {
                    while let Ok((stream, _)) = listener.accept().await {
                        Self::serve_connection(
                            stream,
                            tools.clone(),
                            handlers.clone(),
                            connections.clone(),
                            next_connection_id.clone(),
                            cx,
                        );
                    }
                    drop(temp_dir)
                }
            });
            Ok(Self {
                socket_path,
                _server_task: server_task,
                tools,
                handlers,
                connections,
                next_connection_id,
            })
        })
    }

    pub fn add_tool<T: McpServerTool + Clone + 'static>(&mut self, tool: T) {
        let mut settings = schemars::generate::SchemaSettings::draft07();
        settings.inline_subschemas = true;
        let mut generator = settings.into_generator();

        let input_schema = generator.root_schema_for::<T::Input>();

        let description = input_schema
            .get("description")
            .and_then(|desc| desc.as_str())
            .map(|desc| desc.to_string());
        debug_assert!(
            description.is_some(),
            "Input schema struct must include a doc comment for the tool description"
        );

        let registered_tool = RegisteredTool {
            tool: Tool {
                name: T::NAME.into(),
                title: None,
                description,
                input_schema: input_schema.into(),
                output_schema: if TypeId::of::<T::Output>() == TypeId::of::<()>() {
                    None
                } else {
                    Some(generator.root_schema_for::<T::Output>().into())
                },
                annotations: Some(tool.annotations()),
            },
            handler: Box::new({
                move |input_value, cx| {
                    let input = match input_value {
                        Some(input) => serde_json::from_value(input),
                        None => serde_json::from_value(serde_json::Value::Null),
                    };

                    let tool = tool.clone();
                    match input {
                        Ok(input) => cx.spawn(async move |cx| {
                            let output = tool.run(input, cx).await?;

                            Ok(ToolResponse {
                                content: output.content,
                                structured_content: serde_json::to_value(output.structured_content)
                                    .unwrap_or_default(),
                            })
                        }),
                        Err(err) => Task::ready(Err(err.into())),
                    }
                }
            }),
        };

        self.tools.borrow_mut().insert(T::NAME, registered_tool);
    }

    pub fn handle_request<R: Request>(
        &mut self,
        f: impl Fn(R::Params, &App) -> Task<Result<R::Response>> + 'static,
    ) {
        let f = Box::new(f);
        self.handlers.borrow_mut().insert(
            R::METHOD,
            Box::new(move |req_id, opt_params, cx| {
                let result = match opt_params {
                    Some(params) => serde_json::from_str(params.get()),
                    None => serde_json::from_value(serde_json::Value::Null),
                };

                let params: R::Params = match result {
                    Ok(params) => params,
                    Err(e) => {
                        return Task::ready(
                            serde_json::to_string(&Response::<R::Response> {
                                jsonrpc: "2.0",
                                id: req_id,
                                value: CspResult::Error(Some(crate::client::Error {
                                    message: format!("{e}"),
                                    code: -32700,
                                })),
                            })
                            .unwrap(),
                        );
                    }
                };
                let task = f(params, cx);
                cx.background_spawn(async move {
                    match task.await {
                        Ok(result) => serde_json::to_string(&Response {
                            jsonrpc: "2.0",
                            id: req_id,
                            value: CspResult::Ok(Some(result)),
                        })
                        .unwrap(),
                        Err(e) => serde_json::to_string(&Response {
                            jsonrpc: "2.0",
                            id: req_id,
                            value: CspResult::Error::<R::Response>(Some(crate::client::Error {
                                message: format!("{e}"),
                                code: -32603,
                            })),
                        })
                        .unwrap(),
                    }
                })
            }),
        );
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn serve_connection(
        stream: UnixStream,
        tools: Rc<RefCell<HashMap<&'static str, RegisteredTool>>>,
        handlers: Rc<RefCell<HashMap<&'static str, RequestHandler>>>,
        connections: Rc<RefCell<HashMap<u64, UnboundedSender<String>>>>,
        next_connection_id: Rc<RefCell<u64>>,
        cx: &mut AsyncApp,
    ) {
        let (read, write) = stream.split();
        let (incoming_tx, mut incoming_rx) = unbounded();
        let (outgoing_tx, outgoing_rx) = unbounded();

        let connection_id = {
            let mut next = next_connection_id.borrow_mut();
            let id = *next;
            *next += 1;
            connections.borrow_mut().insert(id, outgoing_tx.clone());
            id
        };

        cx.background_spawn(Self::handle_io(outgoing_rx, incoming_tx, write, read))
            .detach();

        cx.spawn(async move |cx| {
            while let Some(request) = incoming_rx.next().await {
                let Some(request_id) = request.id.clone() else {
                    continue;
                };

                if request.method == CallTool::METHOD {
                    Self::handle_call_tool(request_id, request.params, &tools, &outgoing_tx, cx)
                        .await;
                } else if request.method == ListTools::METHOD {
                    Self::handle_list_tools(request.id.unwrap(), &tools, &outgoing_tx);
                } else if request.method == Initialize::METHOD {
                    Self::handle_initialize(request_id, &outgoing_tx);
                } else if request.method == Ping::METHOD {
                    Self::handle_ping(request_id, &outgoing_tx);
                } else if let Some(handler) = handlers.borrow().get(&request.method.as_ref()) {
                    let outgoing_tx = outgoing_tx.clone();

                    let task = cx.update(|cx| handler(request_id, request.params, cx));
                    cx.spawn(async move |_| {
                        let response = task.await;
                        outgoing_tx.unbounded_send(response).ok();
                    })
                    .detach();
                } else {
                    Self::send_err(
                        request_id,
                        format!("unhandled method {}", request.method),
                        &outgoing_tx,
                    );
                }
            }
            connections.borrow_mut().remove(&connection_id);
        })
        .detach();
    }

    /// Broadcast a JSON-RPC notification (no `id` field) to all connected
    /// clients. Notification shape:
    /// `{"jsonrpc": "2.0", "method": method, "params": params}`.
    /// Failed sends (closed connections) are silently dropped — broadcast is
    /// best-effort and the connection's read side will deregister it shortly.
    pub fn broadcast_notification(&self, method: &str, params: serde_json::Value) {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let serialized = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(err) => {
                log::error!("context_server: failed to serialize notification: {err}");
                return;
            }
        };
        let connections = self.connections.borrow();
        for sender in connections.values() {
            sender.unbounded_send(serialized.clone()).ok();
        }
    }

    /// Respond to the MCP `initialize` handshake. Required by the spec
    /// before any other request — clients (Claude SDK, codex, gemini)
    /// will refuse to use the server otherwise. We advertise only the
    /// `tools` capability since this server doesn't ship prompts /
    /// resources / completions / sampling. `notifications/initialized`
    /// (the post-handshake follow-up) has no `id` and is silently
    /// dropped by the request loop's no-id early-continue.
    fn handle_initialize(request_id: RequestId, outgoing_tx: &UnboundedSender<String>) {
        let response = InitializeResponse {
            protocol_version: ProtocolVersion(LATEST_PROTOCOL_VERSION.to_string()),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapabilities {
                    list_changed: Some(false),
                }),
                ..Default::default()
            },
            server_info: Implementation {
                name: "spk-editor".into(),
                title: None,
                version: env!("CARGO_PKG_VERSION").into(),
                description: None,
            },
            meta: None,
        };
        outgoing_tx
            .unbounded_send(
                serde_json::to_string(&Response {
                    jsonrpc: "2.0",
                    id: request_id,
                    value: CspResult::Ok(Some(response)),
                })
                .unwrap_or_default(),
            )
            .ok();
    }

    /// Respond to MCP `ping` — used by some clients to keep the
    /// connection alive. Empty result, no params.
    fn handle_ping(request_id: RequestId, outgoing_tx: &UnboundedSender<String>) {
        outgoing_tx
            .unbounded_send(
                serde_json::to_string(&Response {
                    jsonrpc: "2.0",
                    id: request_id,
                    value: CspResult::Ok(Some(())),
                })
                .unwrap_or_default(),
            )
            .ok();
    }

    fn handle_list_tools(
        request_id: RequestId,
        tools: &Rc<RefCell<HashMap<&'static str, RegisteredTool>>>,
        outgoing_tx: &UnboundedSender<String>,
    ) {
        let response = ListToolsResponse {
            tools: tools.borrow().values().map(|t| t.tool.clone()).collect(),
            next_cursor: None,
            meta: None,
        };

        outgoing_tx
            .unbounded_send(
                serde_json::to_string(&Response {
                    jsonrpc: "2.0",
                    id: request_id,
                    value: CspResult::Ok(Some(response)),
                })
                .unwrap_or_default(),
            )
            .ok();
    }

    async fn handle_call_tool(
        request_id: RequestId,
        params: Option<Box<RawValue>>,
        tools: &Rc<RefCell<HashMap<&'static str, RegisteredTool>>>,
        outgoing_tx: &UnboundedSender<String>,
        cx: &mut AsyncApp,
    ) {
        let result: Result<CallToolParams, serde_json::Error> = match params.as_ref() {
            Some(params) => serde_json::from_str(params.get()),
            None => serde_json::from_value(serde_json::Value::Null),
        };

        match result {
            Ok(params) => {
                if let Some(tool) = tools.borrow().get(&params.name.as_ref()) {
                    let outgoing_tx = outgoing_tx.clone();

                    let task = (tool.handler)(params.arguments, cx);
                    cx.spawn(async move |_| {
                        let response = match task.await {
                            Ok(result) => CallToolResponse {
                                content: result.content,
                                is_error: Some(false),
                                meta: None,
                                structured_content: if result.structured_content.is_null() {
                                    None
                                } else {
                                    Some(result.structured_content)
                                },
                            },
                            Err(err) => CallToolResponse {
                                content: vec![ToolResponseContent::Text {
                                    text: err.to_string(),
                                }],
                                is_error: Some(true),
                                meta: None,
                                structured_content: None,
                            },
                        };

                        outgoing_tx
                            .unbounded_send(
                                serde_json::to_string(&Response {
                                    jsonrpc: "2.0",
                                    id: request_id,
                                    value: CspResult::Ok(Some(response)),
                                })
                                .unwrap_or_default(),
                            )
                            .ok();
                    })
                    .detach();
                } else {
                    Self::send_err(
                        request_id,
                        format!("Tool not found: {}", params.name),
                        outgoing_tx,
                    );
                }
            }
            Err(err) => {
                Self::send_err(request_id, err.to_string(), outgoing_tx);
            }
        }
    }

    fn send_err(
        request_id: RequestId,
        message: impl Into<String>,
        outgoing_tx: &UnboundedSender<String>,
    ) {
        outgoing_tx
            .unbounded_send(
                serde_json::to_string(&Response::<()> {
                    jsonrpc: "2.0",
                    id: request_id,
                    value: CspResult::Error(Some(crate::client::Error {
                        message: message.into(),
                        code: -32601,
                    })),
                })
                .unwrap(),
            )
            .ok();
    }

    async fn handle_io(
        mut outgoing_rx: UnboundedReceiver<String>,
        incoming_tx: UnboundedSender<RawRequest>,
        mut outgoing_bytes: impl Unpin + AsyncWrite,
        incoming_bytes: impl Unpin + AsyncRead,
    ) -> Result<()> {
        let mut output_reader = BufReader::new(incoming_bytes);
        let mut incoming_line = String::new();
        loop {
            select_biased! {
                message = outgoing_rx.next().fuse() => {
                    if let Some(message) = message {
                        log::trace!("send: {}", &message);
                        outgoing_bytes.write_all(message.as_bytes()).await?;
                        outgoing_bytes.write_all(&[b'\n']).await?;
                    } else {
                        break;
                    }
                }
                bytes_read = output_reader.read_line(&mut incoming_line).fuse() => {
                    if bytes_read? == 0 {
                        break
                    }
                    log::trace!("recv: {}", &incoming_line);
                    match serde_json::from_str(&incoming_line) {
                        Ok(message) => {
                            incoming_tx.unbounded_send(message).log_err();
                        }
                        Err(error) => {
                            outgoing_bytes.write_all(serde_json::to_string(&json!({
                                "jsonrpc": "2.0",
                                "error": json!({
                                    "code": -32603,
                                    "message": format!("Failed to parse: {error}"),
                                }),
                            }))?.as_bytes()).await?;
                            outgoing_bytes.write_all(&[b'\n']).await?;
                            log::error!("failed to parse incoming message: {error}. Raw: {incoming_line}");
                        }
                    }
                    incoming_line.clear();
                }
            }
        }
        Ok(())
    }
}

pub trait McpServerTool {
    type Input: DeserializeOwned + JsonSchema;
    type Output: Serialize + JsonSchema;

    const NAME: &'static str;

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            title: None,
            read_only_hint: None,
            destructive_hint: None,
            idempotent_hint: None,
            open_world_hint: None,
        }
    }

    fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> impl Future<Output = Result<ToolResponse<Self::Output>>>;
}

#[derive(Debug)]
pub struct ToolResponse<T> {
    pub content: Vec<ToolResponseContent>,
    pub structured_content: T,
}

#[derive(Debug, Serialize, Deserialize)]
struct RawRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<RequestId>,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Box<serde_json::value::RawValue>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{AsyncBufReadExt, AsyncWriteExt, io::BufReader};
    use gpui::TestAppContext;
    use net::async_net::UnixStream;

    /// Round-trip the MCP `initialize` handshake against a live server
    /// over a Unix socket. Regression for the bug that left the SDK
    /// unable to register the spk-editor MCP server because the server
    /// answered `-32601 unhandled method initialize`.
    #[gpui::test]
    async fn initialize_handshake_succeeds(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let server = cx
            .update(|cx| McpServer::new(&cx.to_async()))
            .await
            .expect("server start");
        let socket_path = cx.update(|_| server.socket_path().to_path_buf());

        let stream = UnixStream::connect(&socket_path).await.expect("connect");
        let (read, mut write) = stream.split();
        let mut reader = BufReader::new(read);

        let init = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": Initialize::METHOD,
            "params": {
                "protocolVersion": LATEST_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "0.0.0"},
            },
        }))
        .unwrap();
        write.write_all(init.as_bytes()).await.unwrap();
        write.write_all(b"\n").await.unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();

        assert!(
            parsed.get("error").is_none(),
            "unexpected error in initialize response: {parsed}"
        );
        let result = parsed.get("result").expect("result missing");
        assert_eq!(
            result.get("protocolVersion").and_then(|v| v.as_str()),
            Some(LATEST_PROTOCOL_VERSION),
        );
        assert!(
            result
                .get("capabilities")
                .and_then(|c| c.get("tools"))
                .is_some(),
            "tools capability missing: {result}"
        );
        assert!(result.get("serverInfo").is_some(), "serverInfo missing");

        // ping after initialize to confirm the second built-in is wired
        let ping = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": Ping::METHOD,
        }))
        .unwrap();
        write.write_all(ping.as_bytes()).await.unwrap();
        write.write_all(b"\n").await.unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(parsed.get("error").is_none(), "ping error: {parsed}");

        drop(server);
    }
}
