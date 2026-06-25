//! MCP server lifecycle: lock acquisition, server bind, graceful shutdown.
use anyhow::{Context as _, Result};
use context_server::listener::{ExportedTool, McpServer};
use fs2::FileExt;
use gpui::{App, AppContext as _, Entity, Global, TaskExt as _};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
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
    /// Solution-scoped tools split off the global catalog at startup. Cloned
    /// into each per-solution server created by [`open_solution_socket`].
    scoped_template: Vec<ExportedTool>,
    /// Live per-solution sockets, keyed by `SolutionId` string.
    solution_sockets: Rc<RefCell<HashMap<String, SolutionSocket>>>,
}

impl Global for ActiveServer {}

/// One open Solution's MCP socket. The record is reserved synchronously on
/// [`open_solution_socket`] (so the deterministic path is discoverable
/// immediately); `server` is filled once the listener has bound and the
/// scoped tools are installed.
struct SolutionSocket {
    socket: PathBuf,
    root: PathBuf,
    server: Option<Entity<McpServer>>,
}

/// Tools that remain on the editor-global socket. Everything else is
/// solution-scoped: split off the global catalog and served only from
/// per-solution sockets (with `solution_id` force-injected). A brand-new
/// tool defaults to solution-scoped — fail-safe, since the worst case is it
/// being absent from the global socket (a visible functional gap) rather
/// than leaking unscoped to a per-solution subagent.
const GLOBAL_TOOLS: &[&str] = &[
    // Probe / control plane.
    "editor.capabilities",
    "editor.get_operation",
    "editor.cancel_operation",
    "editor.subscribe",
    "editor.unsubscribe",
    "editor.list_subscriptions",
    // Solution lifecycle + discovery (a scoped subagent never
    // creates/opens/closes/deletes/reorders Solutions).
    "solutions.list",
    "solutions.create",
    "solutions.delete",
    "solutions.open",
    "solutions.close",
    "solutions.rename",
    "solutions.reorder_members",
    "solutions.find_for_path",
    // Member management + inspection: shared (see SHARED_TOOLS) — kept on the
    // global socket (the operator addresses any Solution by id, and a member
    // must be addable before the Solution can open) and also cloned into each
    // per-solution socket (a scoped subagent manages its own membership).
    "solutions.get",
    "solutions.add_member",
    "solutions.add_empty_member",
    "solutions.remove_member",
    // Catalog (registry of cloneable projects) is global state.
    "catalog.list",
    "catalog.add_project",
    "catalog.remove_project",
    "catalog.edit_project",
    "catalog.refresh_cache",
    "catalog.clear_cache",
    // Windows are orthogonal to Solutions — one window hosts many Solutions,
    // so a window has no single owner and window ops can't be solution-scoped.
    // The whole `windows.*` surface is cross-solution (operator-level), kept
    // off per-solution sockets entirely.
    "windows.list",
    "windows.focus",
    "windows.close",
    "windows.dispatch_action",
    "windows.send_keystroke",
    "windows.send_text",
    "windows.click_at",
    "windows.click_id",
    "windows.hover_at",
    "windows.hover_id",
    "windows.dump_visual_structure",
    // Remote Control surface. Every tool the Android client can reach through
    // `remote_control::allow_list` MUST live on the global socket: the mobile
    // proxy (`remote_control::proxy::connect`) dials `editor_mcp::socket_path()`
    // — the GLOBAL socket — not a per-solution one. These were previously
    // solution-scoped, so the split moved them off the global socket and every
    // `remote.workspace.*` / `remote.solution_agent.*` call came back as
    // "Tool not found". They are ALSO in SHARED_TOOLS, so per-solution sockets
    // keep them for scoped subagents (with `solution_id` injected where the
    // tool declares it; cross-solution tools like `workspace.snapshot` carry no
    // `solution_id` and are left untouched by the bound-solution injection).
    "workspace.snapshot",
    "workspace.list_solutions",
    "workspace.open_solution",
    "workspace.close_solution",
    "workspace.open_session",
    "workspace.close_session",
    "solution_agent.list_agents",
    "solution_agent.list_sessions",
    "solution_agent.get_session",
    "solution_agent.get_session_entry",
    "solution_agent.create_session",
    "solution_agent.delete_session",
    "solution_agent.send_message",
    "solution_agent.send_message_blocks",
    "solution_agent.cancel_turn",
    "solution_agent.authorize_tool_call",
    "solution_agent.get_session_children",
    "solution_agent.get_session_background_shells",
    "solution_agent.get_session_background_agents",
    "solution_agent.rename_session",
    "solution_agent.restart_agent",
    "solution_agent.reset_context",
    "solution_agent.start_compact",
    "solution_agent.upload_init",
    "solution_agent.upload_status",
    "solution_agent.upload_finish",
    "solution_agent.upload_abort",
];

fn is_global_tool(name: &str) -> bool {
    GLOBAL_TOOLS.contains(&name)
}

/// Tools that stay on the global socket (so they are in [`GLOBAL_TOOLS`]) but
/// are ALSO served from every per-solution socket. On a per-solution socket
/// their `solution_id` is force-injected to the bound Solution.
const SHARED_TOOLS: &[&str] = &[
    "solutions.get",
    "solutions.add_member",
    "solutions.add_empty_member",
    "solutions.remove_member",
    // Remote Control tools (see GLOBAL_TOOLS): kept on the global socket for
    // the mobile proxy AND cloned into each per-solution socket so scoped
    // subagents retain them. `solution_id` is force-injected only into the
    // tools whose schema declares it (per-solution sockets); the global socket
    // never injects, so the mobile passes ids explicitly.
    "workspace.snapshot",
    "workspace.list_solutions",
    "workspace.open_solution",
    "workspace.close_solution",
    "workspace.open_session",
    "workspace.close_session",
    "solution_agent.list_agents",
    "solution_agent.list_sessions",
    "solution_agent.get_session",
    "solution_agent.get_session_entry",
    "solution_agent.create_session",
    "solution_agent.delete_session",
    "solution_agent.send_message",
    "solution_agent.send_message_blocks",
    "solution_agent.cancel_turn",
    "solution_agent.authorize_tool_call",
    "solution_agent.get_session_children",
    "solution_agent.get_session_background_shells",
    "solution_agent.get_session_background_agents",
    "solution_agent.rename_session",
    "solution_agent.restart_agent",
    "solution_agent.reset_context",
    "solution_agent.start_compact",
    "solution_agent.upload_init",
    "solution_agent.upload_status",
    "solution_agent.upload_finish",
    "solution_agent.upload_abort",
];

fn is_shared_tool(name: &str) -> bool {
    SHARED_TOOLS.contains(&name)
}

/// Deterministic path of a Solution's per-solution MCP socket. Pure — the
/// socket only exists between [`open_solution_socket`] and
/// [`close_solution_socket`].
pub fn solution_socket_path(solution_id: &str) -> PathBuf {
    runtime_dir().join("solutions").join(solution_id).join("mcp.sock")
}

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

        // Split the solution-scoped tools off the global catalog: the global
        // socket keeps only `GLOBAL_TOOLS`; the rest become a template cloned
        // into each per-solution socket. SHARED_TOOLS stay on the global socket
        // too but are additionally cloned into the template.
        let mut scoped_template = server.split_off_tools(is_global_tool);
        scoped_template.extend(server.export_tools(is_shared_tool));

        cx.update(|cx| {
            let server_entity = cx.new(|_| server);
            cx.set_global(ActiveServer {
                _lock: lock,
                server: server_entity,
                scoped_template,
                solution_sockets: Rc::new(RefCell::new(HashMap::new())),
            });
        });
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);

    Ok(())
}

/// Open a per-solution MCP socket bound to `solution_id`, serving the
/// solution-scoped tool subset with `solution_id` force-injected. Idempotent:
/// a repeat call for an already-open Solution is a no-op. No-op when the
/// global server never started (tests / failed bind).
pub fn open_solution_socket(cx: &mut App, solution_id: &str, root: PathBuf) {
    let Some(active) = cx.try_global::<ActiveServer>() else {
        return;
    };
    if active.solution_sockets.borrow().contains_key(solution_id) {
        return;
    }
    let template = active.scoped_template.clone();
    let sockets = active.solution_sockets.clone();
    let socket = solution_socket_path(solution_id);
    let solution_id: Arc<str> = Arc::from(solution_id);

    // Reserve the record synchronously so the deterministic path is
    // discoverable immediately; `server` is filled once the listener binds.
    sockets.borrow_mut().insert(
        solution_id.to_string(),
        SolutionSocket {
            socket: socket.clone(),
            root,
            server: None,
        },
    );

    let async_cx = cx.to_async();
    let server_task = McpServer::new(&async_cx);
    cx.spawn(async move |cx| {
        let server = server_task.await.context("creating per-solution MCP server")?;
        server.install_tools(template);
        server.set_bound_solution(solution_id.clone());

        let actual_socket = server.socket_path().to_path_buf();
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        if actual_socket != socket {
            std::fs::remove_file(&socket).log_err();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&actual_socket, &socket).with_context(|| {
                format!("linking {} to {}", actual_socket.display(), socket.display())
            })?;
        }

        cx.update(|cx| {
            let entity = cx.new(|_| server);
            if let Some(active) = cx.try_global::<ActiveServer>() {
                if let Some(record) =
                    active.solution_sockets.borrow_mut().get_mut(solution_id.as_ref())
                {
                    record.server = Some(entity);
                } // else: closed before it finished binding — let the entity drop.
            }
        });
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Tear down a Solution's per-solution socket. Dropping the stored server
/// entity closes its listener; the symlink and the `solutions/<id>` dir are
/// removed.
pub fn close_solution_socket(cx: &mut App, solution_id: &str) {
    let Some(active) = cx.try_global::<ActiveServer>() else {
        return;
    };
    let removed = active.solution_sockets.borrow_mut().remove(solution_id);
    if let Some(record) = removed {
        std::fs::remove_file(&record.socket).log_err();
        if let Some(parent) = record.socket.parent() {
            std::fs::remove_dir_all(parent).log_err();
        }
    }
}

/// Socket of the open Solution whose `root` is an ancestor of (or equal to)
/// `path`, if its listener is bound. Used by the `--nc` bridge to point a
/// subagent at its own Solution's socket. The most specific (longest) root
/// wins when Solutions nest.
pub fn solution_socket_for_path(cx: &App, path: &Path) -> Option<PathBuf> {
    let active = cx.try_global::<ActiveServer>()?;
    active
        .solution_sockets
        .borrow()
        .values()
        .filter(|record| record.server.is_some() && path.starts_with(&record.root))
        .max_by_key(|record| record.root.as_os_str().len())
        .map(|record| record.socket.clone())
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
