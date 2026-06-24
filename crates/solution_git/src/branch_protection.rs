//! Re-export shim for [`solutions::branch_protection`].
//!
//! The real policy lives in the `solutions` crate so non-git call
//! sites (UI handlers in `git_ui`, the MCP registry hook in
//! `editor_mcp`) can reach it without a downward `solution_git` dep —
//! `solution_git` itself is just one of several callers.
//!
//! This module is preserved as a re-export so the original S-DST
//! callers (`solution_git::cross_cherry_pick`) keep working without
//! touching their imports.

pub use solutions::branch_protection::{
    ActiveSnapshot, Decision, check, check_with_snapshot, make_snapshot,
};
