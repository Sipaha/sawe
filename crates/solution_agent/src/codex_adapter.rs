use gpui::SharedString;
use solutions::Solution;
use ui::IconName;

use crate::{
    adapter::{AgentBrand, SolutionAgentAdapter},
    model::AgentServerId,
};

pub const CODEX_AGENT_ID: &str = "codex-native";

/// Codex's chrome in the "new session" picker, on its session tabs and in
/// the status row. See [`AgentBrand`] for why the model list carries no
/// version numbers.
pub const BRAND: AgentBrand = AgentBrand {
    name: "Codex",
    vendor: "OpenAI",
    logo: IconName::AiOpenAi,
    models: "GPT-5 Codex · GPT-5 · GPT-5 Codex Mini",
};

pub struct CodexAdapter;

impl SolutionAgentAdapter for CodexAdapter {
    fn agent_id(&self) -> AgentServerId {
        CODEX_AGENT_ID.into()
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
        crate::claude_adapter::solution_system_prompt(solution, "AGENTS.md")
    }
}
