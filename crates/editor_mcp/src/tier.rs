//! Capability tiers for MCP tools (P-4 in `docs/superpowers/plans/git-panel-plan.md`).
//!
//! Three-level hierarchy:
//! - [`ToolTier::ReadOnly`]: pure reads (status, log, diff, blame). Always safe.
//! - [`ToolTier::Write`]: index/working-tree mutations that don't lose history
//!   (commit, stage, fetch, pull, push without force).
//! - [`ToolTier::Destructive`]: ops that can lose work (force push, reset --hard,
//!   branch -D, history rewrite).
//!
//! Each registered tool declares its tier (see [`crate::registry::register_tool_with_tier`]).
//! Each caller (subagent over `--nc` bridge, in-process Solution Agent) presents a
//! [`CallerCapabilities`] on handshake declaring its `allowed_tier`. The registry
//! refuses dispatch if `tool.tier > caller.allowed_tier`.
//!
//! **Default for unmigrated tools = `Destructive`** — fail-safe so a missed
//! migration shows up as `TierForbidden` for subagents rather than a silent
//! destructive call slipping through.

use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolTier {
    ReadOnly,
    Write,
    Destructive,
}

impl ToolTier {
    fn rank(self) -> u8 {
        match self {
            ToolTier::ReadOnly => 0,
            ToolTier::Write => 1,
            ToolTier::Destructive => 2,
        }
    }

    /// True if a caller with `caller_tier` is permitted to invoke a tool with
    /// `self`. The check is hierarchical: `Destructive` capability allows
    /// `Write` and `ReadOnly` tools as well, etc.
    pub fn permits(self, caller_tier: ToolTier) -> bool {
        caller_tier.rank() >= self.rank()
    }
}

impl PartialOrd for ToolTier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ToolTier {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank().cmp(&other.rank())
    }
}

/// Capabilities a connecting MCP caller declares on handshake.
///
/// `--nc` bridge subagents default to [`ToolTier::Write`]; Solution Agent
/// long-running sessions read this from per-Solution setting
/// `solution_agent.allow_destructive_git`. See P-4.
#[derive(Debug, Clone, Copy)]
pub struct CallerCapabilities {
    pub allowed_tier: ToolTier,
}

impl CallerCapabilities {
    pub const SUBAGENT_DEFAULT: Self = Self {
        allowed_tier: ToolTier::Write,
    };

    pub const SUBAGENT_DESTRUCTIVE: Self = Self {
        allowed_tier: ToolTier::Destructive,
    };

    /// Parse the value of [`BRIDGE_CAPS_ENV_VAR`]. Unknown values fall back to
    /// [`Self::SUBAGENT_DEFAULT`] (fail-closed against typos that would
    /// otherwise grant `Destructive`).
    pub fn from_bridge_env_value(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "destructive" => Self::SUBAGENT_DESTRUCTIVE,
            "write" | "" => Self::SUBAGENT_DEFAULT,
            "read_only" | "readonly" => Self {
                allowed_tier: ToolTier::ReadOnly,
            },
            other => {
                log::warn!(
                    "editor_mcp: unrecognized {BRIDGE_CAPS_ENV_VAR} value {other:?}, defaulting to Write"
                );
                Self::SUBAGENT_DEFAULT
            }
        }
    }
}

/// Env-var name used by the `--nc` bridge to declare caller capabilities to
/// the editor-side MCP server. Set on subprocess spawn in
/// `agent_servers::acp::spk_editor_mcp_bridge_server`; read by the `nc` mode
/// when it accepts a connection.
///
/// Values: `"read_only"`, `"write"` (default), `"destructive"`. Anything else
/// is treated as `"write"` per [`CallerCapabilities::from_bridge_env_value`].
pub const BRIDGE_CAPS_ENV_VAR: &str = "SPK_EDITOR_MCP_BRIDGE_CAPS";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_is_hierarchical() {
        assert!(ToolTier::ReadOnly.permits(ToolTier::Write));
        assert!(ToolTier::Write.permits(ToolTier::Destructive));
        assert!(ToolTier::Destructive.permits(ToolTier::Destructive));
        assert!(!ToolTier::Destructive.permits(ToolTier::Write));
        assert!(!ToolTier::Write.permits(ToolTier::ReadOnly));
    }

    #[test]
    fn ordering_matches_rank() {
        assert!(ToolTier::ReadOnly < ToolTier::Write);
        assert!(ToolTier::Write < ToolTier::Destructive);
    }
}
