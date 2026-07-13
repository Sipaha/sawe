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

/// The socket, its lock file, the per-solution socket dirs and the upload
/// spool are *runtime state*, not configuration — they must not live in
/// `config/` next to `settings.json` (a `rm -r ~/.spk/sawe/config` to reset
/// settings would otherwise take the lock with it, and conversely a config
/// sync would ship a dead socket).
fn default_runtime_dir() -> PathBuf {
    paths::state_dir().clone()
}

pub fn runtime_dir() -> PathBuf {
    RUNTIME_DIR_OVERRIDE
        .get()
        .cloned()
        .unwrap_or_else(default_runtime_dir)
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
    /// Live per-solution sockets, keyed by the Solution's numeric id.
    solution_sockets: Rc<RefCell<HashMap<i64, SolutionSocket>>>,
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
    "solutions.rename_member",
    "solutions.reorder_members",
    "solutions.set_active_member",
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
    "catalog.merge_project",
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
    "windows.scroll_at",
    "windows.click_id",
    "windows.hover_at",
    "windows.hover_id",
    "windows.dump_visual_structure",
    "windows.screenshot",
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
    "solution_agent.get_session_changes",
    "solution_agent.get_session_entry",
    "solution_agent.create_session",
    "solution_agent.delete_session",
    "solution_agent.send_message",
    "solution_agent.send_message_blocks",
    "solution_agent.cancel_turn",
    "solution_agent.authorize_tool_call",
    "solution_agent.get_session_children",
    "solution_agent.rename_session",
    "solution_agent.restart_agent",
    "solution_agent.reconnect_agent",
    "solution_agent.reset_context",
    "solution_agent.start_compact",
    "solution_agent.set_supervisor_enabled",
    "solution_agent.set_supervisor_prompt",
    "solution_agent.get_supervisor_state",
    // Debugging primitive: inject an in-conversation SystemNote breadcrumb
    // into a session addressed by explicit id. Global (operator/agent reaches
    // any session by id), not solution-scoped, so it stays out of SHARED_TOOLS.
    "solution_agent.push_system_note",
    "solution_agent.upload_init",
    "solution_agent.upload_status",
    "solution_agent.upload_finish",
    "solution_agent.upload_abort",
];

/// Whether `name` lives on the global MCP socket. The mobile proxy
/// (`remote_control`) dials the global socket, so EVERY remote-allow-listed
/// upstream tool must be global — `remote_control`'s allow-list test asserts
/// exactly this against `is_global_tool` to stop a forwarded-but-not-global
/// tool ("Tool not found" on the phone) from shipping again.
pub fn is_global_tool(name: &str) -> bool {
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
    "solution_agent.get_session_changes",
    "solution_agent.get_session_entry",
    "solution_agent.create_session",
    "solution_agent.delete_session",
    "solution_agent.send_message",
    "solution_agent.send_message_blocks",
    "solution_agent.cancel_turn",
    "solution_agent.authorize_tool_call",
    "solution_agent.get_session_children",
    "solution_agent.rename_session",
    "solution_agent.restart_agent",
    "solution_agent.reconnect_agent",
    "solution_agent.reset_context",
    "solution_agent.start_compact",
    "solution_agent.set_supervisor_enabled",
    "solution_agent.set_supervisor_prompt",
    "solution_agent.get_supervisor_state",
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
///
/// The directory component is the Solution's numeric id: an id is stable across
/// a rename, which is the whole point of the identity model.
pub fn solution_socket_path(solution_id: i64) -> PathBuf {
    runtime_dir()
        .join("solutions")
        .join(solution_id.to_string())
        .join("mcp.sock")
}

/// Delete `<runtime>/solutions/<name>` entries whose name is not a numeric
/// solution id. Those are leftovers from the pre-identity build, where the
/// directory was the solution's slug; nothing will ever bind them again and a
/// stale `mcp.sock` in one is an attractive nuisance for an agent that reads the
/// directory listing instead of `solutions.get`. Returns the number removed.
///
/// Deliberately narrow: it only ever touches direct children of
/// `<runtime>/solutions`, and it never recurses through a symlink — a symlinked
/// child is unlinked, its target left alone.
pub(crate) fn remove_stale_solution_socket_dirs(runtime: &Path) -> usize {
    let solutions_dir = runtime.join("solutions");
    let Ok(entries) = std::fs::read_dir(&solutions_dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().parse::<i64>().is_ok() {
            continue;
        }
        let path = entry.path();
        // `file_type` here comes from the directory entry / `symlink_metadata`,
        // so a symlink reports as a symlink and is unlinked rather than walked.
        let outcome = match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => std::fs::remove_dir_all(&path),
            Ok(_) => std::fs::remove_file(&path),
            Err(err) => Err(err),
        };
        match outcome {
            Ok(()) => {
                log::info!(
                    "editor_mcp: removed stale slug-named socket dir {}",
                    path.display()
                );
                removed += 1;
            }
            Err(err) => log::warn!(
                "editor_mcp: could not remove stale socket dir {}: {err}",
                path.display()
            ),
        }
    }
    removed
}

/// One-time migration: before this build the socket, its lock, the
/// per-solution socket dirs and the upload spool lived under `config/`.
/// A stale `config/mcp.lock` left behind by an old build would make a new
/// build's `SingleInstanceLock` believe another instance is running, so we
/// sweep the old location at startup. Best-effort and idempotent; skipped
/// under a test override (a test's runtime dir was never the config dir).
pub fn cleanup_legacy_runtime_dir() {
    if RUNTIME_DIR_OVERRIDE.get().is_some() {
        return;
    }
    cleanup_legacy_runtime_dir_in(paths::config_dir());
}

/// Only the runtime artefacts named here are ever removed, and only as direct
/// children of `legacy` — real configuration (`settings.json`, `themes/`, the
/// remote-control keys) sits in the same directory and must survive.
fn cleanup_legacy_runtime_dir_in(legacy: &Path) {
    // `mcp.sock` is a *symlink* to a short `/tmp/zed-mcp*/mcp.sock` (the
    // 108-byte `sun_path` limit), and a dangling symlink reports
    // `exists() == false` — so probe with `symlink_metadata`.
    for name in ["mcp.sock", "mcp.lock"] {
        let path = legacy.join(name);
        if std::fs::symlink_metadata(&path).is_ok() {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing legacy {}", path.display()))
                .log_err();
        }
    }

    let solutions = legacy.join("solutions");
    if let Ok(entries) = std::fs::read_dir(&solutions) {
        for entry in entries.flatten() {
            let socket = entry.path().join("mcp.sock");
            if std::fs::symlink_metadata(&socket).is_ok() {
                std::fs::remove_file(&socket)
                    .with_context(|| format!("removing legacy {}", socket.display()))
                    .log_err();
            }
            // Only ever remove the dir we just emptied — never recurse, so a
            // future non-socket file under `config/solutions/` survives and
            // shows up in the log instead of being deleted.
            if is_empty_dir(&entry.path()) {
                std::fs::remove_dir(entry.path())
                    .with_context(|| format!("removing legacy {}", entry.path().display()))
                    .log_err();
            }
        }
        if is_empty_dir(&solutions) {
            std::fs::remove_dir(&solutions)
                .with_context(|| format!("removing legacy {}", solutions.display()))
                .log_err();
        }
    }

    // `symlink_metadata`, not `is_dir()`: if `uploads` were ever a symlink we
    // unlink the link and leave whatever it points at alone.
    let uploads = legacy.join("uploads");
    if let Ok(metadata) = std::fs::symlink_metadata(&uploads) {
        let outcome = if metadata.is_dir() {
            std::fs::remove_dir_all(&uploads)
        } else {
            std::fs::remove_file(&uploads)
        };
        outcome
            .with_context(|| format!("removing legacy {}", uploads.display()))
            .log_err();
    }
}

fn is_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
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

    cleanup_legacy_runtime_dir();

    let lock = match SingleInstanceLock::acquire(&lock_path()) {
        Ok(lock) => lock,
        Err(err) => {
            log::warn!("editor_mcp: not starting server — {err}");
            return Ok(());
        }
    };

    // We hold the single-instance lock, so no other editor process owns the
    // per-solution sockets: any slug-named dir under `solutions/` is a leftover
    // from the pre-identity build and safe to remove.
    remove_stale_solution_socket_dirs(&runtime_dir());

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
pub fn open_solution_socket(cx: &mut App, solution_id: i64, root: PathBuf) {
    let Some(active) = cx.try_global::<ActiveServer>() else {
        return;
    };
    if active.solution_sockets.borrow().contains_key(&solution_id) {
        return;
    }
    let template = active.scoped_template.clone();
    let sockets = active.solution_sockets.clone();
    let socket = solution_socket_path(solution_id);

    // Reserve the record synchronously so the deterministic path is
    // discoverable immediately; `server` is filled once the listener binds.
    sockets.borrow_mut().insert(
        solution_id,
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
        server.set_bound_solution(solution_id);

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
                if let Some(record) = active.solution_sockets.borrow_mut().get_mut(&solution_id) {
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
pub fn close_solution_socket(cx: &mut App, solution_id: i64) {
    let Some(active) = cx.try_global::<ActiveServer>() else {
        return;
    };
    let removed = active.solution_sockets.borrow_mut().remove(&solution_id);
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
    fn solution_management_tools_are_global() {
        // These live on the editor-global socket (the operator addresses any
        // Solution by id). A brand-new tool defaults to solution-scoped, so a
        // solution-management tool must be added to GLOBAL_TOOLS explicitly or
        // it silently vanishes from the global socket — guard the ones that must
        // be reachable there.
        for name in [
            "solutions.set_active_member",
            "solutions.reorder_members",
            "solutions.open",
        ] {
            assert!(is_global_tool(name), "{name} must be a global-socket tool");
        }
    }

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

    #[test]
    fn stale_slug_socket_dirs_are_removed_numeric_ones_kept() {
        let dir = tempdir().expect("tempdir");
        let solutions = dir.path().join("solutions");
        for name in ["spk-solutions", "ecos-platform", "12", "7"] {
            std::fs::create_dir_all(solutions.join(name)).expect("mkdir");
            std::fs::write(solutions.join(name).join("mcp.sock"), b"").expect("touch sock");
        }

        let removed = remove_stale_solution_socket_dirs(dir.path());

        assert_eq!(removed, 2, "both slug-named dirs must go");
        assert!(!solutions.join("spk-solutions").exists());
        assert!(!solutions.join("ecos-platform").exists());
        assert!(
            solutions.join("12").exists() && solutions.join("7").exists(),
            "numeric dirs are live socket homes and must be left alone"
        );
    }

    #[test]
    fn sweep_is_a_no_op_when_the_solutions_dir_is_absent() {
        let dir = tempdir().expect("tempdir");
        assert_eq!(remove_stale_solution_socket_dirs(dir.path()), 0);
    }

    // The sweep deletes directories. Everything it can reach must live under
    // `<runtime>/solutions/`: never a sibling of it, never a file, and never a
    // symlink's target (removing the link itself is enough).
    #[test]
    fn sweep_touches_nothing_outside_the_solutions_dir() {
        let dir = tempdir().expect("tempdir");
        let sibling = dir.path().join("sibling-dir");
        std::fs::create_dir_all(&sibling).expect("mkdir sibling");
        std::fs::write(sibling.join("precious"), b"keep me").expect("write precious");
        std::fs::write(dir.path().join("mcp.lock"), b"1").expect("write lock");

        let solutions = dir.path().join("solutions");
        std::fs::create_dir_all(solutions.join("slug")).expect("mkdir slug");
        // A stray *file* (not a dir) with a non-numeric name.
        std::fs::write(solutions.join("stray-file"), b"").expect("write stray");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&sibling, solutions.join("link-out")).expect("symlink");

        let removed = remove_stale_solution_socket_dirs(dir.path());

        assert!(removed >= 1, "the slug dir must go");
        assert!(!solutions.join("slug").exists());
        assert!(!solutions.join("stray-file").exists());
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(solutions.join("link-out")).is_err(),
            "the dangling-out symlink itself must be unlinked"
        );
        assert!(
            sibling.join("precious").exists(),
            "a symlinked-to directory outside solutions/ must survive"
        );
        assert!(dir.path().join("mcp.lock").exists());
        assert!(solutions.exists(), "the solutions dir itself must survive");
    }

    #[test]
    fn cleanup_legacy_removes_socket_lock_and_solution_dirs() {
        let legacy = tempdir().expect("tempdir");
        let root = legacy.path();
        std::fs::write(root.join("mcp.lock"), b"1234").expect("lock");
        std::fs::write(root.join("mcp.sock"), b"").expect("sock");
        std::fs::write(root.join("settings.json"), b"{}").expect("settings");
        std::fs::create_dir_all(root.join("themes")).expect("themes");
        std::fs::create_dir_all(root.join("solutions/7")).expect("sol dir");
        std::fs::write(root.join("solutions/7/mcp.sock"), b"").expect("sol sock");
        std::fs::create_dir_all(root.join("uploads")).expect("uploads");
        std::fs::write(root.join("uploads/1.bin"), b"x").expect("upload");

        cleanup_legacy_runtime_dir_in(root);

        assert!(!root.join("mcp.lock").exists());
        assert!(!root.join("mcp.sock").exists());
        assert!(!root.join("solutions").exists());
        assert!(!root.join("uploads").exists());
        assert!(
            root.join("settings.json").exists() && root.join("themes").is_dir(),
            "cleanup must never touch real configuration"
        );

        // Idempotent: a second pass on an already-clean dir is a no-op.
        cleanup_legacy_runtime_dir_in(root);
        assert!(root.join("settings.json").exists());
    }

    // The live socket is a symlink into /tmp, so it is usually *dangling* by
    // the time we sweep — `exists()` reports false on it and would leave it
    // behind.
    #[cfg(unix)]
    #[test]
    fn cleanup_legacy_removes_a_dangling_socket_symlink() {
        let legacy = tempdir().expect("tempdir");
        let root = legacy.path();
        std::os::unix::fs::symlink("/tmp/zed-mcp-gone/mcp.sock", root.join("mcp.sock"))
            .expect("symlink");
        assert!(!root.join("mcp.sock").exists(), "precondition: dangling");

        cleanup_legacy_runtime_dir_in(root);

        assert!(std::fs::symlink_metadata(root.join("mcp.sock")).is_err());
    }

    // A non-socket file under `config/solutions/<id>/` means someone put
    // something there we don't understand: keep the dir rather than recurse.
    #[test]
    fn cleanup_legacy_keeps_a_solution_dir_holding_an_unknown_file() {
        let legacy = tempdir().expect("tempdir");
        let root = legacy.path();
        std::fs::create_dir_all(root.join("solutions/7")).expect("sol dir");
        std::fs::write(root.join("solutions/7/mystery.json"), b"{}").expect("mystery");

        cleanup_legacy_runtime_dir_in(root);

        assert!(root.join("solutions/7/mystery.json").exists());
    }

    #[test]
    fn default_runtime_dir_is_state_not_config() {
        let dir = default_runtime_dir();
        assert!(
            dir.ends_with("state"),
            "the mcp socket + lock are runtime state, not configuration: {}",
            dir.display()
        );
        assert_ne!(
            dir,
            *paths::config_dir(),
            "runtime_dir must no longer alias config_dir"
        );
    }

    #[test]
    fn solution_socket_path_is_keyed_on_the_numeric_id() {
        let path = solution_socket_path(42);
        assert_eq!(
            path.strip_prefix(runtime_dir()).expect("under runtime dir"),
            Path::new("solutions").join("42").join("mcp.sock")
        );
    }
}
