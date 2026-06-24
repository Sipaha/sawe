//! Editor MCP — single-instance MCP server embedded in Sawe.
//!
//! Approach C central registry: domain crates register their tools during
//! their own `init()` via [`register_tool`]. After all init is done, the
//! editor calls [`start_server`] which binds the Unix socket and accepts
//! connections.

mod handoff;
mod lifecycle;
mod notifications;
mod operations;
mod registry;
mod subscriptions;
mod tier;
mod tier_guard;
mod tools;
mod window_ids;
pub mod workspace_seq;

pub use handoff::{HandoffOutcome, try_handoff_to_existing_instance};
pub use lifecycle::{
    close_solution_socket, open_solution_socket, runtime_dir, set_runtime_dir_for_test,
    socket_path, solution_socket_for_path, solution_socket_path, start_server,
};
pub use notifications::emit as emit_notification;
pub use registry::{
    init, register_tool, register_tool_with_tier, register_typed_tool_with_protection,
    register_typed_tool_with_tier, tier_for,
};
pub use tier::{BRIDGE_CAPS_ENV_VAR, CallerCapabilities, ToolTier};
pub use tier_guard::{
    BranchProtectionChecker, BranchProtectionDecision, BranchProtectionHint,
    BranchProtectionTarget, RepoPathResolver, TierGuardTool, current_caps,
    set_branch_protection_checker, set_repo_path_resolver,
};
pub use tools::is_confirmed;
pub use window_ids::format as format_window_id;

pub use operations::{
    OperationProgress, OperationState, OperationStatus,
    complete_cancelled as op_complete_cancelled, complete_err as op_complete_err,
    complete_ok as op_complete_ok, get as op_get, is_cancelled as op_is_cancelled,
    record_progress as op_record_progress, request_cancellation as op_request_cancellation,
    start as op_start,
};
pub use subscriptions::{
    Subscription, create as sub_create, delete as sub_delete, list as sub_list,
};

#[cfg(test)]
pub use lifecycle::start_server_for_test;

#[doc(hidden)]
pub mod tools_for_test {
    pub use crate::tools::capabilities::{CapabilitiesParams, CapabilitiesTool};
    pub use crate::tools::operations::{
        CancelOperationParams, CancelOperationTool, GetOperationParams, GetOperationTool,
    };
    pub use crate::tools::subscribe::{
        ListSubscriptionsParams, ListSubscriptionsTool, SubscribeParams, SubscribeTool,
        UnsubscribeParams, UnsubscribeTool,
    };
}
