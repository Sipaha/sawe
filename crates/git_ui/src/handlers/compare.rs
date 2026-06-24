//! S-CTM Compare submenu handlers — open `ProjectDiff` (branch-mode) for
//! various reference flavors.
//!
//! The existing infrastructure (`branch_diff::DiffBase::Merge { base_ref }`)
//! diffs the working tree against a single base ref via `git --merge-base`.
//! That cleanly maps to **Compare with Local Working Tree** (base_ref =
//! commit_sha). The other compare flavors (with HEAD / with Branch / with
//! Commit, all of which need a true commit-vs-commit diff) require a more
//! general `commit_vs_commit` infrastructure that doesn't exist yet — they
//! land in a follow-up.

use gpui::{Context, SharedString, Window};
use workspace::Workspace;

use crate::project_diff::ProjectDiff;

/// Compare a commit against the current working tree.
///
/// Implementation: opens `ProjectDiff` in branch-mode with `base_ref` set
/// to the commit SHA. The resulting tab shows "Changes since <sha>" — i.e.
/// every file modified between `<sha>` and the current working tree.
pub fn compare_with_local_working_tree(
    workspace: &mut Workspace,
    sha: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    ProjectDiff::deploy_at_revision(workspace, SharedString::from(sha.to_string()), window, cx);
}
