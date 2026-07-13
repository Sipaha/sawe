//! Build a `WorkspaceSnapshot` for the wire `workspace.snapshot` tool.

use crate::coordinator::WorkspaceEventCoordinator;
use crate::dto::{WorkspaceSnapshot, WorkspaceSolution};
use gpui::App;

pub(crate) fn build_snapshot(cx: &App) -> WorkspaceSnapshot {
    let coord = WorkspaceEventCoordinator::global(cx);
    // Hold the read guard for the entire snapshot body so that no
    // `emit_sequenced` write (seq increment + notification) can interleave
    // between our `current_seq` read and our state read. Multiple concurrent
    // snapshots don't block each other; only writes block reads.
    let _read = coord.snapshot_lock();
    let seq = coord.current_seq();

    // If either store is uninitialised (very early during boot or in tests
    // that didn't wire it), return an empty snapshot — never panic.
    let solution_store = match solutions::SolutionStore::try_global(cx) {
        Some(s) => s,
        None => {
            return WorkspaceSnapshot {
                seq,
                solutions: Vec::new(),
            };
        }
    };

    let agent_store = solution_agent::store::SolutionAgentStore::try_global(cx);

    let solutions: Vec<WorkspaceSolution> = solution_store.read_with(cx, |store, cx| {
        let mut result = Vec::new();
        for sol in store.solutions() {
            // Filter 1: only include solutions that currently have an open
            // desktop window (`SolutionSummary.open == true`). Build the
            // summary first so we can reuse it in the push below without
            // a second call.
            let summary = solutions::mcp::build_summary(sol, cx);
            if !summary.open {
                continue;
            }
            let sol_id = sol.id;
            let sessions = if let Some(agent_store_ref) = agent_store.as_ref() {
                agent_store_ref.read_with(cx, |agent, cx| {
                    agent
                        .all_sessions()
                        .filter_map(|entity| {
                            let session = entity.read(cx);
                            // Filter 2: only include sessions that are
                            // currently visible in the desktop session-tab
                            // strip (`tab_order IS NOT NULL`). The
                            // `!is_supervisor_ephemeral` check is defense-in-depth:
                            // hidden judge/auditor sessions never get a tab_order,
                            // but assert it explicitly so a future tab_order leak
                            // can't surface a judge in the mobile snapshot.
                            if session.solution_id == sol_id
                                && session.tab_order.is_some()
                                && !session.is_supervisor_ephemeral
                            {
                                Some(solution_agent::mcp::session_summary(session, cx))
                            } else {
                                None
                            }
                        })
                        .collect()
                })
            } else {
                Vec::new()
            };
            result.push(WorkspaceSolution {
                solution: summary,
                sessions,
            });
        }
        result
    });

    WorkspaceSnapshot { seq, solutions }
}
