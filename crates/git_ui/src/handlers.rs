//! S-CTM operation handlers — non-destructive commit operations dispatched
//! from the Git Graph commit context menu. Each submodule owns one
//! action family: clipboard / branch / tag / checkout / compare. All
//! handlers return `Task<Result<()>>` so the caller can attach
//! `.detach_and_prompt_err(...)` for UI-side error reporting.
//!
//! Destructive operations (cherry-pick / revert / reset / drop / squash /
//! merge / rebase) are out of scope here — they land via the S-DST work
//! along with backup-ref creation and the `OpRunner` framework.

pub mod branch;
pub mod checkout;
pub mod cherry_pick;
pub mod compare;
pub mod copy;
pub mod drop;
pub mod edit_message;
pub mod fixup;
pub mod merge;
pub mod move_commit;
pub mod patch;
pub mod protection;
pub mod rebase;
pub mod reset;
pub mod revert;
pub mod show_at_revision;
pub mod squash;
pub mod tag;
