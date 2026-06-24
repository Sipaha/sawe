//! Fork-local git actions.
//!
//! These actions belong to the Sawe fork's git feature set (S-STH
//! stashes pane, S-SHL shelf pane, patch apply, S-BAK undo registry, S-SAR
//! show-at-revision, interactive rebase). Upstream Zed v1.7.2 does not define
//! them. They keep the `git` keymap namespace so existing keybindings
//! (`git::ShowAtRevision`, …) continue to resolve, but the Rust types live in
//! `git_ui` because the re-fork's `git` crate is kept close to upstream.
//!
//! NOTE: the donor fork defined these in `crates/git/src/git.rs`; they were
//! relocated here to keep the re-fork's edits scoped to `git_ui`.

use gpui::Action;
use schemars::JsonSchema;
use serde::Deserialize;

/// Opens the Stashes pane item (S-STH).
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = git)]
#[serde(deny_unknown_fields)]
pub struct Stashes;

/// Opens the Shelf pane item (S-SHL).
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = git)]
#[serde(deny_unknown_fields)]
pub struct Shelf;

/// Applies a patch / mbox / diff file via a file picker.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = git)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchFromFile;

/// Applies a patch / mbox / diff stored in the system clipboard.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = git)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchFromClipboard;

/// Opens the undo registry as a modal — restore a branch to before a recent
/// destructive op (cherry-pick, drop, squash, rebase, …) recorded by the
/// S-BAK auto-backup framework.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = git)]
#[serde(deny_unknown_fields)]
pub struct UndoLast;

/// Deletes sawe backup-refs older than `older_than_days` from the
/// active repository. Default 30 days.
#[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = git)]
#[serde(deny_unknown_fields)]
pub struct CleanupBackups {
    #[serde(default = "default_cleanup_days")]
    pub older_than_days: u32,
}

impl Default for CleanupBackups {
    fn default() -> Self {
        Self {
            older_than_days: default_cleanup_days(),
        }
    }
}

fn default_cleanup_days() -> u32 {
    30
}

/// Open the Interactive Rebase view starting at `sha`. Requires `sha` to
/// be reachable from HEAD on the current branch.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = git)]
#[serde(deny_unknown_fields)]
pub struct InteractiveRebaseFromHere {
    pub sha: String,
}

/// S-SAR — open a read-only snapshot of the current repository at `sha`
/// in a brand-new top-level workspace window.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = git)]
#[serde(deny_unknown_fields)]
pub struct ShowAtRevision {
    pub sha: String,
}
