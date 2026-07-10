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

pub fn register(cx: &mut App) {
    solutions_lifecycle::register_solutions_lifecycle(cx);
    catalog::register_catalog(cx);
    member_mgmt::register_member_mgmt(cx);
    workspace_state::register_workspace_state(cx);
    visual_structure::register_visual_structure(cx);
    diagnostics::register_diagnostics(cx);
    project_files::register_project_files(cx);
}
