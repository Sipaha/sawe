//! MCP tools exposed by the `solution_agent` crate. Tools register with the
//! central `editor_mcp` registry from `solution_agent::init` so that
//! `start_server` (called later from `crates/zed/src/main.rs`) sees them
//! when binding the socket.
//!
//! The tool implementations live in per-namespace submodules; this root only
//! declares them, re-exports their public surface, and fans out registration.
use gpui::App;

mod authorization;
mod context;
mod dto;
mod lifecycle;
mod messaging;
mod read;
mod supervisor;
mod uploads;

#[cfg(debug_assertions)]
mod debug;

#[cfg(test)]
mod tests;

// Re-export the submodule surface at `crate::mcp::*`. These globs look unused to
// a lib-only build but are required by the in-crate test module, `store::tests`
// (`crate::mcp::GetSessionChangesTool`), and `event_sources` — do not drop them.
#[allow(unused_imports)]
pub(crate) use {
    authorization::*, context::*, dto::*, lifecycle::*, messaging::*, read::*, supervisor::*,
    uploads::*,
};

#[cfg(debug_assertions)]
#[allow(unused_imports)]
pub(crate) use debug::*;

// Cross-crate consumers (`workspace_events`) reference these directly, so keep
// them re-exported at `solution_agent::mcp::*` with their original visibility.
pub use dto::{SessionSummary, session_summary};

pub fn register(cx: &mut App) {
    read::register_read(cx);
    lifecycle::register_lifecycle(cx);
    messaging::register_messaging(cx);
    authorization::register_authorization(cx);
    context::register_context(cx);
    uploads::register_uploads(cx);
    supervisor::register_supervisor(cx);
    #[cfg(debug_assertions)]
    debug::register_debug(cx);
}
