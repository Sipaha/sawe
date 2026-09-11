use anyhow::{Context as _, Result, anyhow, bail};
use futures::{
    FutureExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
};
use gpui::{App, AppContext as _, Task};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use util::{ResultExt as _, process::Child};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;
#[derive(Debug)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
}
impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex: {}", self.message)
    }
}
impl std::error::Error for RpcError {}

pub struct Process {
    child: Mutex<Child>,
    executor: gpui::BackgroundExecutor,
    pub outgoing: mpsc::UnboundedSender<Value>,
    pub incoming: mpsc::UnboundedReceiver<Value>,
    pending: Pending,
    next_id: AtomicU64,
    _tasks: Vec<Task<()>>,
}
impl Process {
    pub fn spawn(directory: &Path, cx: &App) -> Result<Self> {
        let mut command = Command::new("codex");
        command.args(["app-server"]).current_dir(directory);
        let mut child = Child::spawn(command, Stdio::piped(), Stdio::piped(), Stdio::piped())
            .context("Could not start Codex. Install the Codex CLI and ensure `codex` is on PATH, then run `codex login` in a terminal.")?;
        let stdout = child.stdout.take().context("Codex stdout missing")?;
        let mut stdin = child.stdin.take().context("Codex stdin missing")?;
        let stderr = child.stderr.take().context("Codex stderr missing")?;
        let (outgoing, mut outgoing_rx) = mpsc::unbounded::<Value>();
        let (incoming_tx, incoming) = mpsc::unbounded();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let exit_sender = incoming_tx.clone();
        let status = child.status();
        let exit_pending = pending.clone();
        let exited = cx.background_spawn(async move {
            if let Err(error) = status.await {
                log::debug!("Codex exit: {error}");
            }
            exit_pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear();
            if exit_sender
                .unbounded_send(json!({"method":"sawe/disconnected"}))
                .is_err()
            {
                log::debug!("Codex event receiver closed");
            }
        });
        let reader = cx.background_spawn(read_messages(stdout, pending.clone(), incoming_tx));
        let writer_pending = pending.clone();
        let writer = cx.background_spawn(async move {
            while let Some(value) = outgoing_rx.next().await {
                let mut bytes = value.to_string().into_bytes();
                bytes.push(b'\n');
                if let Err(error) = stdin.write_all(&bytes).await {
                    log::error!("Codex stdin: {error}");
                    break;
                }
                if let Err(error) = stdin.flush().await {
                    log::error!("Codex flush: {error}");
                    break;
                }
            }
            writer_pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear();
        });
        let stderr = cx.background_spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Some(line) = lines.next().await {
                match line {
                    Ok(line) => log::debug!("Codex stderr: {line}"),
                    Err(error) => {
                        log::debug!("Codex stderr closed: {error}");
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child: Mutex::new(child),
            executor: cx.background_executor().clone(),
            outgoing,
            incoming,
            pending,
            next_id: AtomicU64::new(1),
            _tasks: vec![reader, writer, stderr, exited],
        })
    }
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        request_on_wire(
            &self.outgoing,
            &self.pending,
            self.next_id.fetch_add(1, Ordering::Relaxed),
            method,
            params,
            self.executor.timer(Duration::from_secs(45)).map(|_| ()),
        )
        .await
    }

    pub async fn initialize(&self) -> Result<()> {
        self.request("initialize", json!({"clientInfo":{"name":"sawe","title":"Sawe","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":false}})).await?;
        self.outgoing
            .unbounded_send(json!({"method":"initialized"}))?;
        Ok(())
    }
    pub fn kill(&self) {
        self.child
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .kill()
            .log_err();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn request_on_wire(
    outgoing: &mpsc::UnboundedSender<Value>,
    pending: &Pending,
    id: u64,
    method: &str,
    params: Value,
    timeout: impl std::future::Future<Output = ()>,
) -> Result<Value> {
    let (sender, receiver) = oneshot::channel();
    pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id, sender);
    if outgoing
        .unbounded_send(json!({"id":id,"method":method,"params":params}))
        .is_err()
    {
        pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        bail!("Codex input closed. Reopen this chat to reconnect.");
    }
    let timeout = timeout.fuse();
    futures::pin_mut!(timeout);
    let result = futures::select_biased! {
        response = receiver.fuse() => response.context("Codex process disconnected. Reopen this chat to reconnect.")?,
        _ = timeout => Err(anyhow!("Codex {method} timed out. Reopen this chat to reconnect.")),
    };
    pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&id);
    result
}

#[cfg(test)]
pub(crate) async fn mock_steer_exchange(
    params: Value,
    mut response: Value,
    before_response: Vec<Value>,
) -> Result<Value> {
    let pending: Pending = Default::default();
    let (outgoing, mut requests) = mpsc::unbounded();
    let expected = params.clone();
    let request = request_on_wire(
        &outgoing,
        &pending,
        41,
        "turn/steer",
        params,
        futures::future::pending(),
    );
    let server = async {
        let message = requests.next().await.unwrap();
        assert_eq!(
            message,
            json!({"id":41,"method":"turn/steer","params":expected})
        );
        response["id"] = json!(41);
        let bytes = before_response
            .iter()
            .chain(std::iter::once(&response))
            .map(|v| format!("{v}\n"))
            .collect::<String>();
        let (events, mut incoming) = mpsc::unbounded();
        read_messages(futures::io::Cursor::new(bytes), pending.clone(), events).await;
        for event in before_response {
            assert_eq!(incoming.next().await.unwrap(), event);
        }
    };
    let (result, _) = futures::join!(request, server);
    result
}

async fn read_messages(
    stdout: impl futures::io::AsyncRead + Unpin,
    pending: Pending,
    incoming_tx: mpsc::UnboundedSender<Value>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next().await {
        let value = match line
            .map_err(anyhow::Error::from)
            .and_then(|line| serde_json::from_str::<Value>(&line).map_err(Into::into))
        {
            Ok(value) => value,
            Err(error) => {
                log::error!("Codex protocol read failed: {error}");
                break;
            }
        };
        if value.get("method").is_none() {
            if let Some(id) = value["id"].as_u64()
                && let Some(sender) = pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id)
            {
                let result = if let Some(error) = value.get("error") {
                    Err(RpcError {
                        code: error["code"].as_i64().unwrap_or(0),
                        message: error["message"]
                            .as_str()
                            .unwrap_or("request failed")
                            .to_owned(),
                    }
                    .into())
                } else {
                    Ok(value["result"].clone())
                };
                if sender.send(result).is_err() {
                    log::debug!("Codex response receiver dropped");
                }
            }
        } else if incoming_tx.unbounded_send(value).is_err() {
            break;
        }
    }
    if incoming_tx
        .unbounded_send(json!({"method":"sawe/disconnected"}))
        .is_err()
    {
        log::debug!("Codex event receiver closed");
    }
    pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routes_interleaved_replies_and_server_requests() {
        smol::block_on(async {
            let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
            let (first, first_rx) = oneshot::channel();
            let (second, second_rx) = oneshot::channel();
            pending.lock().unwrap().insert(1, first);
            pending.lock().unwrap().insert(2, second);
            let (events, mut receiver) = mpsc::unbounded();
            let bytes = concat!(
                "{\"id\":2,\"result\":{\"ok\":true}}\n",
                "{\"id\":1,\"method\":\"item/commandExecution/requestApproval\",\"params\":{}}\n",
                "{\"id\":1,\"error\":{\"message\":\"denied\"}}\n"
            );
            read_messages(futures::io::Cursor::new(bytes), pending.clone(), events).await;
            assert_eq!(second_rx.await.unwrap().unwrap()["ok"], true);
            assert!(
                first_rx
                    .await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("denied")
            );
            assert_eq!(
                receiver.next().await.unwrap()["method"],
                "item/commandExecution/requestApproval"
            );
            assert_eq!(
                receiver.next().await.unwrap()["method"],
                "sawe/disconnected"
            );
            assert!(pending.lock().unwrap().is_empty());
        });
    }
    #[test]
    fn malformed_output_resolves_pending_requests_as_disconnected() {
        smol::block_on(async {
            let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
            let (sender, receiver) = oneshot::channel();
            pending.lock().unwrap().insert(1, sender);
            let (events, mut incoming) = mpsc::unbounded();
            read_messages(futures::io::Cursor::new("not JSON\n"), pending, events).await;
            assert!(receiver.await.is_err());
            assert_eq!(
                incoming.next().await.unwrap()["method"],
                "sawe/disconnected"
            );
        });
    }
}
