//! WorkspaceEventCoordinator has been relocated to `editor_mcp::workspace_seq`
//! so that lower-level crates (solutions, solution_agent) can call into it
//! without depending on this crate. This module re-exports the API for
//! existing callers within workspace_events.
pub use editor_mcp::workspace_seq::{WorkspaceEventCoordinator, install};
