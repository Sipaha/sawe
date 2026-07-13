//! MCP tools exposed by the `solutions` crate. Tools register with the
//! central `editor_mcp` registry from `solutions::init` so that
//! `start_server` (called later from `crates/zed/src/main.rs`) sees them
//! when binding the socket.
use gpui::App;

mod catalog;
mod diagnostics;
mod member_mgmt;
mod project_files;
mod solutions_lifecycle;
mod visual_structure;
mod workspace_state;

#[cfg(test)]
mod tests;

pub use catalog::*;
pub use diagnostics::*;
pub use member_mgmt::*;
pub use project_files::*;
pub use solutions_lifecycle::*;
pub use visual_structure::*;
pub use workspace_state::*;

/// Resolve the `solution_id` of a solution-scoped MCP tool call.
///
/// On a per-solution socket the listener force-injects the bound id into the
/// params before the handler runs (`context_server::listener::handle_call_tool`
/// — it keys the injection off the `solution_id` *property* existing in the
/// tool's input schema, which an `Option<i64>` still emits, and it overwrites
/// whatever the caller sent). So `None` here can only mean "called on the
/// editor-global socket without an id" — never "the caller is scoped and just
/// omitted it".
pub(crate) fn resolve_solution_id(raw: Option<i64>) -> anyhow::Result<crate::SolutionId> {
    let id = raw.ok_or_else(|| {
        anyhow::anyhow!(
            "invalid_params: solution_id is required on the editor-global socket — \
             connect to the per-solution socket (`solutions.get` → `mcp_socket`) \
             and it is injected for you"
        )
    })?;
    anyhow::ensure!(
        id > 0,
        "invalid_params: solution_id must be a positive numeric id, got {id}"
    );
    Ok(crate::SolutionId(id))
}

pub fn register(cx: &mut App) {
    solutions_lifecycle::register_solutions_lifecycle(cx);
    catalog::register_catalog(cx);
    member_mgmt::register_member_mgmt(cx);
    workspace_state::register_workspace_state(cx);
    visual_structure::register_visual_structure(cx);
    diagnostics::register_diagnostics(cx);
    project_files::register_project_files(cx);
}
