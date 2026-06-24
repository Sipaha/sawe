//! MCP server lifecycle: lock acquisition, server bind, graceful shutdown.
use anyhow::{Context as _, Result};
use context_server::listener::McpServer;
use fs2::FileExt;
use gpui::{App, AppContext as _, Entity, Global, TaskExt as _};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use util::ResultExt as _;

/// Overrides the directory containing `mcp.lock` and `mcp.sock`. Set by
/// integration tests to isolate the well-known socket from any live
/// `sawe` instance running on the same machine. Without this,
/// concurrent e2e tests would race the live editor's lock and clobber
/// the user's `~/.config/sawe/mcp.{lock,sock}` files.
///
/// For end-to-end probe instances (running a real editor in parallel with the
/// user's), use `script/run-mcp --runtime-dir DIR` instead — that overrides
/// `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_CACHE_HOME` so the editor's
/// entire state (settings, db, mcp socket, …) lands in DIR.
static RUNTIME_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Pin the lock + socket paths to a test-owned directory. Must be called
/// before [`start_server`]. Idempotent only with the same value — a second
/// call with a different path is silently ignored (`OnceLock`).
pub fn set_runtime_dir_for_test(dir: PathBuf) {
    let _ = RUNTIME_DIR_OVERRIDE.set(dir);
}

pub fn runtime_dir() -> PathBuf {
    RUNTIME_DIR_OVERRIDE
        .get()
        .cloned()
        .unwrap_or_else(|| paths::config_dir().clone())
}

#[derive(Debug)]
pub struct SingleInstanceLock {
    file: File,
}

#[derive(Debug)]
pub enum LockError {
    Busy { holder_pid: Option<u32> },
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Busy {
                holder_pid: Some(pid),
            } => {
                write!(f, "another sawe instance holds the lock (PID {pid})")
            }
            LockError::Busy { holder_pid: None } => {
                write!(f, "another sawe instance holds the lock")
            }
            LockError::Io(err) => write!(f, "io error: {err}"),
        }
    }
}

impl std::error::Error for LockError {}

impl SingleInstanceLock {
    pub fn acquire(path: &Path) -> std::result::Result<Self, LockError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(LockError::Io)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(LockError::Io)?;

        if FileExt::try_lock_exclusive(&file).is_err() {
            let mut body = String::new();
            file.read_to_string(&mut body).ok();
            let holder_pid = body.trim().parse::<u32>().ok();
            return Err(LockError::Busy { holder_pid });
        }
        file.set_len(0).map_err(LockError::Io)?;
        let pid = std::process::id();
        writeln!(file, "{pid}").map_err(LockError::Io)?;
        file.sync_all().map_err(LockError::Io)?;
        Ok(SingleInstanceLock { file })
    }
}

impl Drop for SingleInstanceLock {
    fn drop(&mut self) {
        FileExt::unlock(&self.file).ok();
    }
}

pub fn lock_path() -> PathBuf {
    runtime_dir().join("mcp.lock")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("mcp.sock")
}

struct ActiveServer {
    _lock: SingleInstanceLock,
    server: Entity<McpServer>,
}

impl Global for ActiveServer {}

pub fn start_server(cx: &mut App) -> Result<()> {
    // S-BAK: derive process-global caller capabilities from the
    // `SAWE_MCP_BRIDGE_CAPS` env var on first server start. The bridge
    // (`agent_servers::acp::sawe_mcp_bridge_server`) stamps this on
    // each subagent's `--nc` subprocess; the editor itself usually inherits
    // an unset value, in which case we default to Write (subagent-safe).
    // See `tier_guard.rs` for the trade-off (process-global, not per-conn).
    let caps_value = std::env::var(crate::tier::BRIDGE_CAPS_ENV_VAR).unwrap_or_default();
    let caps = crate::tier::CallerCapabilities::from_bridge_env_value(&caps_value);
    crate::tier_guard::set_process_caps(caps);

    let lock = match SingleInstanceLock::acquire(&lock_path()) {
        Ok(lock) => lock,
        Err(err) => {
            log::warn!("editor_mcp: not starting server — {err}");
            return Ok(());
        }
    };

    let sock = socket_path();
    if sock.exists() {
        std::fs::remove_file(&sock).log_err();
    }

    crate::registry::mark_started(cx);
    let drained = crate::registry::drain(cx);

    let async_cx = cx.to_async();
    let server_task = McpServer::new(&async_cx);

    cx.spawn(async move |cx| {
        let mut server = server_task.await.context("creating MCP server")?;

        // Apply tool registrations BEFORE publishing the well-known socket
        // symlink. Otherwise a client connecting between symlink creation and
        // registration application would see an empty `tools/list`.
        for registration in drained {
            registration(&mut server);
        }

        // McpServer binds its own socket inside a tempdir. Symlink the
        // well-known path to it so clients can find us deterministically.
        let actual_socket = server.socket_path().to_path_buf();
        if actual_socket != sock {
            std::fs::remove_file(&sock).log_err();
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&actual_socket, &sock).with_context(|| {
                    format!("linking {} to {}", actual_socket.display(), sock.display())
                })?;
            }
        }

        cx.update(|cx| {
            let server_entity = cx.new(|_| server);
            cx.set_global(ActiveServer {
                _lock: lock,
                server: server_entity,
            });
        });
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);

    Ok(())
}

pub fn server(cx: &App) -> Option<Entity<McpServer>> {
    cx.try_global::<ActiveServer>().map(|a| a.server.clone())
}

#[cfg(test)]
pub fn start_server_for_test(_cx: &mut App) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn acquire_lock_writes_pid() {
        let dir = tempdir().expect("tempdir");
        let lock_path = dir.path().join("mcp.lock");
        let lock = SingleInstanceLock::acquire(&lock_path).expect("acquire");
        let body = std::fs::read_to_string(&lock_path).expect("read");
        let pid: u32 = body.trim().parse().expect("pid is u32");
        assert_eq!(pid, std::process::id());
        drop(lock);
    }

    #[test]
    fn second_acquire_fails_while_held() {
        let dir = tempdir().expect("tempdir");
        let lock_path = dir.path().join("mcp.lock");
        let lock = SingleInstanceLock::acquire(&lock_path).expect("first");
        match SingleInstanceLock::acquire(&lock_path) {
            Err(LockError::Busy { holder_pid }) => {
                assert_eq!(holder_pid, Some(std::process::id()));
            }
            other => panic!("expected Busy, got {other:?}"),
        }
        drop(lock);
    }

    #[test]
    fn release_then_reacquire_works() {
        let dir = tempdir().expect("tempdir");
        let lock_path = dir.path().join("mcp.lock");
        {
            let _lock = SingleInstanceLock::acquire(&lock_path).expect("first");
        }
        let _lock = SingleInstanceLock::acquire(&lock_path).expect("second");
    }
}
