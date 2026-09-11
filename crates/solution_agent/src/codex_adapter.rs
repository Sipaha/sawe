use gpui::SharedString;
use solutions::Solution;
use ui::IconName;

use crate::{adapter::SolutionAgentAdapter, model::AgentServerId};

pub const CODEX_AGENT_ID: &str = "codex-native";

pub struct CodexAdapter;

impl SolutionAgentAdapter for CodexAdapter {
    fn agent_id(&self) -> AgentServerId {
        CODEX_AGENT_ID.into()
    }
    fn display_name(&self) -> SharedString {
        "Codex".into()
    }
    fn icon(&self) -> IconName {
        IconName::AiOpenAi
    }
    fn supports_resume(&self) -> bool {
        true
    }
    fn build_initial_system_prompt(&self, solution: &Solution) -> String {
        crate::claude_adapter::solution_system_prompt(solution, "AGENTS.md")
    }
}
