//! Handoff: connect to an existing instance's MCP socket and forward CLI args.
use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::json;
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

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
                        "id": 1,
                        "method": "tools/call",
                        "params": {
                            "name": "editor.handle_cli_args",
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

                    // Read until newline.
                    let mut buffer = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        match stream.read(&mut byte).await {
                            Ok(0) => break,
                            Ok(_) => {
                                if byte[0] == b'\n' {
                                    break;
                                }
                                buffer.push(byte[0]);
                            }
                            Err(err) => return Err(err.into()),
                        }
                    }
                    let response: serde_json::Value =
                        serde_json::from_slice(&buffer).context("parse handoff response")?;
                    let structured = response
                        .get("result")
                        .and_then(|r| r.get("structuredContent"))
                        .cloned()
                        .ok_or_else(|| anyhow!("missing result.structuredContent"))?;
                    let outcome: HandleCliArgsResult = serde_json::from_value(structured)?;
                    if !outcome.handled {
                        let detail = outcome.error.as_deref().unwrap_or("(no detail)");
                        return Err(anyhow!("existing instance refused handoff: {detail}"));
                    }
                    return Ok(HandoffOutcome::HandedOff {
                        focused_window_id: outcome.focused_window_id,
                    });
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
