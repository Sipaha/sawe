//! Kimi Code as a Solution-chat provider: brand chrome plus the mapping from
//! Kimi's ACP session surface onto the fork's two provider-agnostic controls.
//!
//! The launcher lives in `agent_servers::kimi` — Kimi speaks ACP natively, so
//! there is no protocol to translate and no `*_native` crate. What is left here
//! is the two places where Kimi's own vocabulary is wider than the fork's:
//!
//! - **Permissions.** Kimi publishes four ACP session modes; the fork's UI
//!   offers two ([`crate::model::SessionPermissionMode`]). The ends of the
//!   range are the honest mapping — see [`mode_for`].
//! - **Models.** The pinned ACP schema no longer parses `session/new`'s
//!   `models` field, but Kimi also publishes the same list as a `select`
//!   config option, which the schema does parse. [`MODEL_CONFIG_OPTION_ID`]
//!   names it for `native_controls`.

use agent_client_protocol::schema::v1 as acp;
use gpui::SharedString;
use solutions::Solution;
use ui::IconName;

use crate::{
    adapter::{AgentBrand, SolutionAgentAdapter},
    model::{AgentServerId, SessionPermissionMode},
};

/// The agent id Kimi sessions are keyed by. Taken from the crate that owns the
/// launcher so the two cannot drift apart.
pub const KIMI_AGENT_ID: &str = agent_servers::KIMI_CODE_ID;

/// The config option Kimi publishes its model list under. Re-exported from
/// the launcher crate so the adapter and the generic ACP connection's
/// `active_model` cannot drift apart.
pub const MODEL_CONFIG_OPTION_ID: &str = agent_servers::MODEL_CONFIG_OPTION_ID;

/// Kimi's "analyze only" mode: it cannot modify files or execute commands.
const MODE_READ_ONLY: &str = "plan";

/// The installed CLI's engine calls `auto` "Never Ask". Its ACP descriptions
/// are stale: `yolo` still requests approval for dangerous commands and
/// sensitive files. Neither mode is a filesystem sandbox; the Solution
/// boundary remains part of the injected instructions.
const MODE_FULL_ACCESS: &str = "auto";

/// Kimi's chrome in the "new session" picker, on its session tabs and in the
/// status row. See [`AgentBrand`] for why the model list carries no version
/// numbers.
pub const BRAND: AgentBrand = AgentBrand {
    name: "Kimi",
    vendor: "Moonshot AI",
    logo: IconName::AiKimi,
    // Kimi's brand blue.
    color: 0x1783FF,
    models: "K2 · K3",
};

/// The Kimi ACP session mode that enforces `permission_mode`.
pub fn mode_for(permission_mode: SessionPermissionMode) -> acp::SessionModeId {
    acp::SessionModeId::new(match permission_mode {
        SessionPermissionMode::FullAccess => MODE_FULL_ACCESS,
        SessionPermissionMode::ReadOnly => MODE_READ_ONLY,
    })
}

/// The fork permission mode a Kimi session mode corresponds to, or `None` for
/// the two modes in between that the fork's binary control does not express.
/// Used when adopting a mode the CLI reported on its own — a session restored
/// from `session/new`, or one the user changed inside the CLI.
pub fn permission_mode_for(mode_id: &acp::SessionModeId) -> Option<SessionPermissionMode> {
    match mode_id.0.as_ref() {
        MODE_FULL_ACCESS => Some(SessionPermissionMode::FullAccess),
        MODE_READ_ONLY => Some(SessionPermissionMode::ReadOnly),
        _ => None,
    }
}

pub struct KimiAdapter;

impl SolutionAgentAdapter for KimiAdapter {
    fn agent_id(&self) -> AgentServerId {
        KIMI_AGENT_ID.into()
    }

    fn display_name(&self) -> SharedString {
        BRAND.name.into()
    }

    fn icon(&self) -> IconName {
        BRAND.logo
    }

    fn supports_resume(&self) -> bool {
        true
    }

    fn build_initial_system_prompt(&self, solution: &Solution) -> String {
        crate::claude_adapter::solution_system_prompt(solution, "KIMI.md")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solution() -> Solution {
        Solution {
            id: solutions::SolutionId(7),
            name: "test".into(),
            root: std::path::PathBuf::from("/tmp/sol-kimi"),
            members: Vec::new(),
            last_opened_at: None,
        }
    }

    #[test]
    fn the_two_permission_modes_map_onto_the_ends_of_kimis_range() {
        assert_eq!(
            mode_for(SessionPermissionMode::ReadOnly).0.as_ref(),
            "plan",
            "read-only must map to the mode that cannot touch the filesystem"
        );
        assert_eq!(
            mode_for(SessionPermissionMode::FullAccess).0.as_ref(),
            "auto",
            "full access must map to the mode that approves every tool"
        );
    }

    #[test]
    fn the_mode_mapping_round_trips() {
        for permission_mode in [
            SessionPermissionMode::ReadOnly,
            SessionPermissionMode::FullAccess,
        ] {
            assert_eq!(
                permission_mode_for(&mode_for(permission_mode)),
                Some(permission_mode)
            );
        }
    }

    /// `default` and `yolo` are real Kimi modes that the fork's binary control
    /// cannot express. Reporting one of them as "full access" would tell the
    /// user their chat can edit files when it may still stop to ask, so the
    /// adoption path has to see `None` and leave the stored mode alone.
    #[test]
    fn modes_the_fork_cannot_express_are_not_guessed_at() {
        for mode in ["default", "yolo", "", "YOLO", "Plan"] {
            assert_eq!(
                permission_mode_for(&acp::SessionModeId::new(mode)),
                None,
                "`{mode}` must not be read as a fork permission mode"
            );
        }
    }

    #[test]
    fn the_agent_id_matches_the_launcher() {
        assert_eq!(KIMI_AGENT_ID, "kimi-code");
        assert_eq!(KIMI_AGENT_ID, agent_servers::KIMI_CODE_ID);
    }

    #[test]
    fn the_adapter_reports_its_own_brand() {
        assert_eq!(KimiAdapter.agent_id().as_ref(), KIMI_AGENT_ID);
        assert_eq!(KimiAdapter.display_name().as_ref(), BRAND.name);
        assert_eq!(KimiAdapter.icon(), BRAND.logo);
        assert!(KimiAdapter.supports_resume());
    }

    #[test]
    fn the_prompt_points_at_kimis_own_instruction_file() {
        let prompt = KimiAdapter.build_initial_system_prompt(&solution());
        assert!(prompt.contains("KIMI.md"));
        assert!(!prompt.contains("CLAUDE.md"));
        assert!(!prompt.contains("AGENTS.md"));
        assert!(prompt.contains("Solution root: /tmp/sol-kimi"));
        assert!(prompt.contains("Stay inside the solution"));
    }
}
