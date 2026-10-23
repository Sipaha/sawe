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
pub static BRAND: std::sync::LazyLock<AgentBrand> = std::sync::LazyLock::new(|| AgentBrand {
    name: "Codex".into(),
    vendor: "OpenAI".into(),
    icon_path: None,
    permissions:true,
    context_usage_current: true,
    effort_options:Vec::new(),
    logo: IconName::AiOpenAi,
    // OpenAI's green. The current OpenAI mark is monochrome; this is the
    // colour its products have been recognised by.
    color: 0x10A37F,
    models: "GPT-5 Codex · GPT-5 · GPT-5 Codex Mini".into(),
});

pub struct CodexAdapter;

impl SolutionAgentAdapter for CodexAdapter {
    fn agent_id(&self) -> AgentServerId {
        CODEX_AGENT_ID.into()
    }
    fn display_name(&self) -> SharedString {
        BRAND.name.clone().into()
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
