//! Per-chat "supervisor": types, on-disk verdict log, and pure predicates.
//! GPUI-free so it unit-tests in isolation. Orchestration lives in `store.rs`
//! (`tick_supervisor`) and `mcp.rs` (the verdict tools).
//!
//! Split into four cohesive submodules:
//! - [`state`] — the state machine: status/verdict types, guard predicates,
//!   and usage-limit classification/parsing.
//! - [`gist`] — condensing operator-facing prose down to the one-or-two-
//!   sentence lede the toast + question banner show.
//! - [`persistence`] — the diary / verdict-log / intent / session-log disk I/O.
//! - [`briefing`] — judge/auditor briefing construction and verdict nonces.
//!
//! The submodules are re-exported flat so every existing `crate::supervisor::*`
//! path keeps resolving.

mod briefing;
mod gist;
mod persistence;
mod state;

#[cfg(test)]
mod tests;

pub use briefing::*;
pub use gist::*;
pub use persistence::*;
pub use state::*;
