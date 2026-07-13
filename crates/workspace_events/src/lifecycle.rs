//! Lifecycle MCP tools for the workspace.* namespace:
//! `open_solution`, `close_solution`, `open_session`, `close_session`.
//! Each is idempotent at the store level — if the requested state
//! already holds, the tool returns the current seq with no emit.

use anyhow::{Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use solutions::{SolutionId, SolutionStore};

use crate::coordinator::WorkspaceEventCoordinator;
use crate::dto::{SeqAck, SessionIdParam, SolutionIdParam};

pub(crate) fn open_solution_impl(cx: &mut App, id: SolutionId) -> Result<u64> {
    let store =
        SolutionStore::try_global(cx).ok_or_else(|| anyhow!("SolutionStore not initialised"))?;
    let coord = WorkspaceEventCoordinator::global(cx);

    let was_open = store.read(cx).is_open(id);
    if was_open {
        return Ok(coord.current_seq());
    }

    // Dispatch the real desktop window-open. Without this the mobile picker's
    // "Open" tap only flipped `open_solutions: HashSet`, leaving the desktop
    // window closed — the user saw nothing happen on the desktop and a blank
    // strip on the mobile (no consoles materialised because no ConsolePanel
    // had hydrated yet to flip `tab_order`). Mirrors `solutions.open`'s
    // open_paths call. Detached: the mobile RPC returns the reserved seq
    // immediately; the window comes up async. The window-lifecycle hook in
    // event_sources.rs will fire mark_open again when the workspace is bound;
    // it's idempotent on the HashSet so the duplicate is a harmless no-op.
    let paths = store.read_with(cx, |s, _| s.paths_for_open(id))?;
    if !paths.is_empty() {
        let app_state = workspace::AppState::global(cx);
        let mut options = workspace::OpenOptions::default();
        options.focus = Some(true);
        options.open_mode = workspace::OpenMode::Activate;
        let task = workspace::open_paths(&paths, app_state, options, cx);
        cx.spawn(async move |_| {
            if let Err(err) = task.await {
                log::warn!("workspace.open_solution: open_paths failed: {err:#}");
            }
        })
        .detach();
    }

    // Hydrate restored sessions for this solution (idempotent if already hydrated).
    // Done before mark_open so any sessions in memory are captured by the snapshot.
    if let Some(agent) = solution_agent::store::SolutionAgentStore::try_global(cx) {
        let _ = agent.update(cx, |a, cx| a.hydrate_all_for_solution(id, cx));
        // The hydration is a Task<_> — we don't await here; the notification
        // reflects whatever state is in memory. The mobile client re-syncs on
        // reconnect via workspace.snapshot anyway.
    }

    // `mark_open` emits the sequenced `workspace.solution_opened` notification
    // (with `sessions: []` due to the cross-crate cycle constraint — solutions
    // cannot see solution_agent) AND the local `SolutionStoreEvent::Opened`
    // event. The per-session-deltas fan-out is driven from that event by the
    // `install_solution_open_observer` subscriber in this crate, which sees
    // both stores. That keeps the walk consistent across every `mark_open`
    // call site (wire RPC here, desktop UI button, event-source bootstrap
    // observers) without each one re-implementing the same walk.
    store.update(cx, |s, cx| s.mark_open(id, cx));

    // Return the seq just reserved by mark_open's emit_sequenced call.
    Ok(WorkspaceEventCoordinator::global(cx).current_seq())
}

pub(crate) fn close_solution_impl(cx: &mut App, id: SolutionId) -> Result<u64> {
    let store =
        SolutionStore::try_global(cx).ok_or_else(|| anyhow!("SolutionStore not initialised"))?;
    let coord = WorkspaceEventCoordinator::global(cx);

    let was_open = store.read(cx).is_open(id);
    if !was_open {
        return Ok(coord.current_seq());
    }

    // Terminate the solution's agent threads + terminals first. Sessions stay
    // on disk (tab_order, transcripts preserved). Only running runtime state
    // is killed.
    crate::shutdown::shutdown_solution_runtime(id, cx);

    // mark_closed itself emits the sequenced workspace.solution_closed event.
    store.update(cx, |s, cx| s.mark_closed(id, cx));

    Ok(WorkspaceEventCoordinator::global(cx).current_seq())
}

#[derive(Clone)]
pub struct OpenSolutionTool;

impl McpServerTool for OpenSolutionTool {
    type Input = SolutionIdParam;
    type Output = SeqAck;
    const NAME: &'static str = "workspace.open_solution";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let id = SolutionId(input.solution_id);
        let seq = cx.update(|cx| open_solution_impl(cx, id))?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("seq={seq}"),
            }],
            structured_content: SeqAck { seq },
        })
    }
}

#[derive(Clone)]
pub struct CloseSolutionTool;

impl McpServerTool for CloseSolutionTool {
    type Input = SolutionIdParam;
    type Output = SeqAck;
    const NAME: &'static str = "workspace.close_solution";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let id = SolutionId(input.solution_id);
        let seq = cx.update(|cx| close_solution_impl(cx, id))?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("seq={seq}"),
            }],
            structured_content: SeqAck { seq },
        })
    }
}

// ── Session lifecycle ─────────────────────────────────────────────────────────

pub(crate) fn open_session_impl(cx: &mut App, session_id_str: &str) -> Result<u64> {
    let agent = solution_agent::store::SolutionAgentStore::try_global(cx)
        .ok_or_else(|| anyhow!("SolutionAgentStore not initialised"))?;
    let session_id = solution_agent::SolutionSessionId::parse(session_id_str)
        .map_err(|e| anyhow!("bad session_id: {e}"))?;

    // Validate the session exists before delegating: `open_session_in_strip`
    // is a silent no-op on a missing id, but this RPC's contract wants an
    // explicit "session not found" error for the caller.
    agent.read_with(cx, |a, _| {
        a.session(session_id)
            .map(|_| ())
            .ok_or_else(|| anyhow!("session not found"))
    })?;

    // Delegate to the shared "open a session" definition on the store.
    // It appends to tab_order + emits workspace.session_opened (via
    // persist_tab_order) and is idempotent if already pinned — so the
    // wire `open_session` and the create-implies-open path stay identical.
    agent.update(cx, |a, cx| a.open_session_in_strip(session_id, cx));

    // Return the seq that persist_tab_order just reserved.
    Ok(WorkspaceEventCoordinator::global(cx).current_seq())
}

pub(crate) fn close_session_impl(cx: &mut App, session_id_str: &str) -> Result<u64> {
    let agent = solution_agent::store::SolutionAgentStore::try_global(cx)
        .ok_or_else(|| anyhow!("SolutionAgentStore not initialised"))?;
    let session_id = solution_agent::SolutionSessionId::parse(session_id_str)
        .map_err(|e| anyhow!("bad session_id: {e}"))?;

    let (solution_id, was_in_strip) = agent.read_with(cx, |a, cx| {
        let entity = a
            .session(session_id)
            .ok_or_else(|| anyhow!("session not found"))?;
        let s = entity.read(cx);
        Ok::<_, anyhow::Error>((s.solution_id, s.tab_order.is_some()))
    })?;
    if !was_in_strip {
        return Ok(WorkspaceEventCoordinator::global(cx).current_seq());
    }

    // Build new ordered list = current minus this session.
    let new_order: Vec<solution_agent::SolutionSessionId> = agent.read_with(cx, |a, cx| {
        let mut current: Vec<_> = a
            .all_sessions()
            .filter_map(|entity| {
                let s = entity.read(cx);
                match s.tab_order {
                    Some(order) if s.solution_id == solution_id && s.id != session_id => {
                        Some((s.id, order))
                    }
                    _ => None,
                }
            })
            .collect();
        current.sort_by_key(|(_, ord)| *ord);
        current.into_iter().map(|(id, _)| id).collect()
    });

    // persist_tab_order now emits workspace.session_closed internally
    // via WorkspaceEventCoordinator::emit_sequenced (F5). No manual
    // emit here — doing so would double-fire the notification.
    agent.update(cx, |a, cx| {
        a.persist_tab_order(solution_id, new_order, cx)
    });

    // Return the seq that persist_tab_order just reserved.
    Ok(WorkspaceEventCoordinator::global(cx).current_seq())
}

#[derive(Clone)]
pub struct OpenSessionTool;

impl McpServerTool for OpenSessionTool {
    type Input = SessionIdParam;
    type Output = SeqAck;
    const NAME: &'static str = "workspace.open_session";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let seq = cx.update(|cx| open_session_impl(cx, &input.session_id))?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("seq={seq}"),
            }],
            structured_content: SeqAck { seq },
        })
    }
}

#[derive(Clone)]
pub struct CloseSessionTool;

impl McpServerTool for CloseSessionTool {
    type Input = SessionIdParam;
    type Output = SeqAck;
    const NAME: &'static str = "workspace.close_session";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let seq = cx.update(|cx| close_session_impl(cx, &input.session_id))?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("seq={seq}"),
            }],
            structured_content: SeqAck { seq },
        })
    }
}
