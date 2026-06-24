//! Workspace-events crate.
//!
//! Owns the sequenced event protocol that backs the mobile `workspace.*` MCP
//! surface. Hosts:
//!   - `WorkspaceEventCoordinator` — an atomic `seq` counter + the sequenced
//!     emit helper used by mutation paths in `solutions` and `solution_agent`.
//!   - The `workspace.*` MCP tools: `snapshot`, `list_solutions`, `open_solution`,
//!     `close_solution`, `open_session`, `close_session`.
//!
//! Subsequent tasks fill these out. For now we expose `init` so `crates/zed`
//! can wire us up without further plumbing later.

use gpui::{App, AppContext, Context, Entity, Global, Subscription};
use solutions::{SolutionStore, SolutionStoreEvent};

mod coordinator;
mod dto;
pub(crate) mod lifecycle;
mod list;
mod mcp;
pub(crate) mod shutdown;
mod snapshot;

pub use coordinator::WorkspaceEventCoordinator;
pub use dto::*;
pub use list::ListSolutionsTool;

/// Install the coordinator + register MCP tools. Idempotent.
pub fn init(cx: &mut App) {
    coordinator::install(cx);
    mcp::register(cx);
    install_solution_open_observer(cx);
}

/// Tiny holder entity that owns a `SolutionStore` subscription for the
/// lifetime of the process. The subscriber listens for
/// [`SolutionStoreEvent::Opened`] and fans out a
/// `workspace.session_opened` notification per session whose
/// `tab_order IS NOT NULL` for the just-opened solution.
///
/// This lives here (not in `solutions`) because building the session
/// list requires reading `SolutionAgentStore`, and the `solutions`
/// crate cannot depend on `solution_agent` (cycle). `workspace_events`
/// already sees both stores.
///
/// Idempotent: a second `install_solution_open_observer` call replaces
/// the global, dropping the previous subscription.
struct SolutionOpenObserver {
    _subscription: Subscription,
}

struct GlobalSolutionOpenObserver(#[allow(dead_code)] Entity<SolutionOpenObserver>);
impl Global for GlobalSolutionOpenObserver {}

fn install_solution_open_observer(cx: &mut App) {
    let Some(store) = SolutionStore::try_global(cx) else {
        return;
    };
    let observer = cx.new(|cx: &mut Context<SolutionOpenObserver>| {
        let subscription = cx.subscribe(&store, |_this, _store, event, cx| {
            let SolutionStoreEvent::Opened { id } = event else {
                return;
            };
            let id = id.clone();
            let Some(agent) = solution_agent::store::SolutionAgentStore::try_global(cx) else {
                return;
            };
            let summaries = agent.read_with(cx, |a, cx| {
                a.all_sessions()
                    .filter_map(|entity| {
                        let s = entity.read(cx);
                        if s.solution_id == id && s.tab_order.is_some() {
                            Some(solution_agent::mcp::session_summary(s, cx))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            });
            if summaries.is_empty() {
                return;
            }
            let coord = WorkspaceEventCoordinator::global(cx);
            for summary in summaries {
                coord.emit_sequenced(
                    cx,
                    "workspace.session_opened",
                    serde_json::json!({
                        "solution_id": id.as_str(),
                        "session": summary,
                    }),
                );
            }
        });
        SolutionOpenObserver {
            _subscription: subscription,
        }
    });
    cx.set_global(GlobalSolutionOpenObserver(observer));
}

/// Expose `build_snapshot` for integration tests that need to check the
/// snapshot filter logic without going through a live MCP socket.
///
/// This re-exports the internal `snapshot::build_snapshot` — it has no
/// side-effects and is safe to call in any context where the
/// `WorkspaceEventCoordinator` and `SolutionStore` globals are installed.
pub fn build_snapshot_for_test(cx: &App) -> WorkspaceSnapshot {
    snapshot::build_snapshot(cx)
}

/// Test-only direct invocation of the workspace.list_solutions logic,
/// bypassing the MCP socket. Used by integration tests.
pub fn list_solutions_for_test(cx: &App, open: Option<bool>) -> dto::ListSolutionsResult {
    list::build_list(cx, open)
}

/// Test-only direct invocation of `workspace.open_solution`.
/// Call from tests as: `cx.update(|cx| workspace_events::open_solution_for_test(cx, &id))`.
pub fn open_solution_for_test(cx: &mut App, id: &solutions::SolutionId) -> dto::SeqAck {
    let seq = lifecycle::open_solution_impl(cx, id).expect("open_solution");
    dto::SeqAck { seq }
}

/// Test-only direct invocation of `workspace.close_solution`.
/// Call from tests as: `cx.update(|cx| workspace_events::close_solution_for_test(cx, &id))`.
pub fn close_solution_for_test(cx: &mut App, id: &solutions::SolutionId) -> dto::SeqAck {
    let seq = lifecycle::close_solution_impl(cx, id).expect("close_solution");
    dto::SeqAck { seq }
}

/// Test-only accessor for the current event sequence number.
pub fn current_seq_for_test(cx: &App) -> u64 {
    coordinator::WorkspaceEventCoordinator::global(cx).current_seq()
}

/// Test-only direct invocation of `workspace.open_session`.
pub fn open_session_for_test(cx: &mut App, id: &solution_agent::SolutionSessionId) -> dto::SeqAck {
    let seq = lifecycle::open_session_impl(cx, id.as_str()).expect("open_session");
    dto::SeqAck { seq }
}

/// Test-only direct invocation of `workspace.close_session`.
pub fn close_session_for_test(cx: &mut App, id: &solution_agent::SolutionSessionId) -> dto::SeqAck {
    let seq = lifecycle::close_session_impl(cx, id.as_str()).expect("close_session");
    dto::SeqAck { seq }
}
