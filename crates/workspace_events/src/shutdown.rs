//! Runtime shutdown for a closing solution: kill agent processes + terminals.
//! Called from `workspace.close_solution` BEFORE `mark_closed` flips the flag.
//!
//! # Terminals (Research task C result)
//! The `terminal` / `terminal_view` crates do NOT maintain a global registry
//! of all running terminals keyed by solution or working directory. Terminals
//! are owned by a `TerminalPanel` which is itself attached to a `Workspace`
//! window entity — there is no "for_solution" API. The `TerminalPanel` does
//! not store the `SolutionId` it belongs to, and there is no global handle
//! we can walk from `App` to find all terminals for a given solution.
//!
//! As a result, terminal shutdown is not implemented here. A `log::warn!`
//! below documents the gap. The more critical agent-thread cancellation
//! is fully implemented.

use gpui::App;
use solutions::SolutionId;

/// Kill all running agent threads and terminals associated with `id`.
///
/// Sessions (tab_order + conversation entries) are preserved on disk.
/// Only in-process runtime state (AcpThread processes, terminal ptys) is
/// terminated.
pub fn shutdown_solution_runtime(id: &SolutionId, cx: &mut App) {
    shutdown_agent_threads(id, cx);
    shutdown_terminals(id, cx);
}

fn shutdown_agent_threads(id: &SolutionId, cx: &mut App) {
    let Some(agent_store) = solution_agent::store::SolutionAgentStore::try_global(cx) else {
        return;
    };

    // Use `sessions_for` (indexed by solution) rather than walking all sessions —
    // avoids a full scan and avoids holding any iterator across the mutable
    // `thread.update` calls below.
    let threads_to_cancel: Vec<gpui::Entity<acp_thread::AcpThread>> =
        agent_store.read_with(cx, |store, cx| {
            store
                .sessions_for(id)
                .into_iter()
                .filter_map(|entity| entity.read(cx).acp_thread().cloned())
                .collect()
        });

    for thread in threads_to_cancel {
        thread.update(cx, |t, cx| {
            // cancel() returns a Task — detach it; we don't wait for the
            // cancellation handshake. The agent process receives a SIGTERM /
            // protocol-level cancel message; the session state transitions to
            // Closed via the existing acp event flow.
            let _task = t.cancel(cx);
        });
    }
}

fn shutdown_terminals(_id: &SolutionId, _cx: &mut App) {
    // Gap: terminals are owned by TerminalPanel which is attached to a
    // Workspace window entity. There is no global terminal registry keyed
    // by SolutionId or working directory that we can reach from App without
    // a window handle. Terminal shutdown must be driven from the window layer
    // (e.g. by the Workspace/Navigator that owns both the solution reference
    // and the TerminalPanel handle).
    //
    // For now we log a warning so the gap is visible in production logs.
    log::warn!(
        "close_solution: terminal shutdown not implemented \
         (no per-solution terminal registry reachable from App context)"
    );
}
