//! S-CTM Commit row context menu — moved to
//! [`git_ui::commit_context_menu`] in S-ANN so the blame gutter can
//! reuse the same menu without `git_ui` having to depend on
//! `git_graph` (which would invert the existing dependency direction).
//!
//! This shim preserves the existing call sites (`context_menu::CommitContext`,
//! `context_menu::build_commit_context_menu`).

pub use git_ui::commit_context_menu::{CommitContext, build_commit_context_menu};
