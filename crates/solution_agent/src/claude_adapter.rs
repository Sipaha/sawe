use gpui::SharedString;
use solutions::Solution;
use ui::IconName;

use crate::adapter::{AgentBrand, SolutionAgentAdapter};
use crate::model::AgentServerId;

pub const CLAUDE_ACP_AGENT_ID: &str = "claude-acp";

/// Claude's chrome in the "new session" picker, on its session tabs and in
/// the status row. See [`AgentBrand`] for why the model list carries no
/// version numbers.
pub static BRAND: std::sync::LazyLock<AgentBrand> = std::sync::LazyLock::new(|| AgentBrand {
    name: "Claude".into(),
    vendor: "Anthropic".into(),
    icon_path: None,
    permissions:true,
    context_usage_current: false,
    effort_options:["low","medium","high","xhigh","max","ultracode"].into_iter().map(str::to_owned).collect(),
    logo: IconName::AiClaude,
    // Claude's orange.
    color: 0xD97757,
    models: "Sonnet · Opus · Haiku".into(),
});

pub struct ClaudeAcpAdapter;

impl SolutionAgentAdapter for ClaudeAcpAdapter {
    fn agent_id(&self) -> AgentServerId {
        SharedString::from(CLAUDE_ACP_AGENT_ID)
    }

    fn display_name(&self) -> SharedString {
        SharedString::from(BRAND.name.clone())
    }

    fn icon(&self) -> IconName {
        BRAND.logo
    }

    fn supports_resume(&self) -> bool {
        true
    }

    fn build_initial_system_prompt(&self, solution: &Solution) -> String {
        solution_system_prompt(solution, "CLAUDE.md")
    }
}

pub(crate) use crate::client_instructions::solution_system_prompt;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use solutions::{MemberId, Solution, SolutionId, SolutionMember};
    use std::path::PathBuf;

    fn solution(members: Vec<&str>) -> Solution {
        Solution {
            id: SolutionId(14),
            name: "test".into(),
            root: PathBuf::from("/tmp/sol-x"),
            members: members
                .into_iter()
                .enumerate()
                .map(|(i, m)| SolutionMember {
                    id: MemberId(i as i64 + 1),
                    name: m.to_string(),
                    local_path: PathBuf::from(format!("/tmp/sol-x/{m}")),
                    origin_catalog_id: None,
                })
                .collect(),
            last_opened_at: Some(Utc::now().timestamp_millis()),
        }
    }

    #[test]
    fn prompt_lists_members_and_includes_root_path() {
        let sol = solution(vec!["ecos-records", "ecos-app"]);
        let prompt = ClaudeAcpAdapter.build_initial_system_prompt(&sol);
        assert!(prompt.contains("Solution root: /tmp/sol-x"));
        assert!(prompt.contains("- ecos-records"));
        assert!(prompt.contains("- ecos-app"));
        assert!(prompt.contains("CLAUDE.md"));
        assert!(prompt.contains("Stay inside the solution"));
        assert!(prompt.contains("per-action go-ahead"));
        assert!(prompt.contains("under /tmp/sol-x/.tmp/"));
        assert!(prompt.contains("mktemp -d -p /tmp/sol-x/.tmp"));
        // Catalog / project-management awareness: the agent must know the
        // solution id and the tools to list + add projects to it.
        assert!(prompt.contains("solution's id is `14`"));
        assert!(prompt.contains("catalog.list"));
        assert!(prompt.contains("solutions.add_member"));
        assert!(prompt.contains("solutions.add_empty_member"));
        assert!(prompt.contains("\"solution_id\": 14"));
    }

    #[test]
    fn codex_prompt_uses_agents_instructions_and_preserves_solution_scope() {
        let sol = solution(vec!["app"]);
        let prompt = crate::codex_adapter::CodexAdapter.build_initial_system_prompt(&sol);
        assert!(prompt.contains("AGENTS.md"));
        assert!(!prompt.contains("CLAUDE.md"));
        assert!(prompt.contains("Solution root: /tmp/sol-x"));
        assert!(prompt.contains("solutions.add_member"));
        assert!(prompt.contains("Stay inside the solution"));
        assert!(prompt.contains("under /tmp/sol-x/.tmp/"));
    }

    /// Every backticked `namespace.tool` the prompt names, in the order it
    /// names them. Derived from the prompt text rather than hard-coded, so a
    /// tool added to the prompt later is checked without anyone remembering to
    /// list it here too.
    fn tools_named_in(prompt: &str) -> Vec<&str> {
        prompt
            .split('`')
            .skip(1)
            .step_by(2)
            .filter_map(|span| span.split_whitespace().next())
            .filter(|token| {
                let mut parts = token.split('.');
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(namespace), Some(tool), None) => {
                        !namespace.is_empty()
                            && !tool.is_empty()
                            && token
                                .chars()
                                .all(|c| c.is_ascii_lowercase() || c == '_' || c == '.')
                    }
                    _ => false,
                }
            })
            .collect()
    }

    // A Solution Agent talks to its *per-solution* MCP socket
    // (`agent_servers::acp::mcp_servers_for_project` points the `--nc` bridge
    // at it), which serves only the solution-scoped slice of the catalog. A
    // tool this prompt promises but the split keeps global-only is an
    // instruction the agent physically cannot follow: that is what happened to
    // `catalog.list`, leaving the agent with `solutions.add_member` and no way
    // to learn a `catalog_id` for it.
    #[test]
    fn every_tool_the_prompt_promises_reaches_a_scoped_socket() {
        let prompt = ClaudeAcpAdapter.build_initial_system_prompt(&solution(vec!["m"]));
        let named = tools_named_in(&prompt);
        for expected in [
            "catalog.list",
            "solutions.add_member",
            "solutions.add_empty_member",
        ] {
            assert!(
                named.contains(&expected),
                "the extractor stopped seeing the prompt's tool names: {named:?}"
            );
        }
        for name in named {
            assert!(
                editor_mcp::is_solution_scoped_tool(name),
                "the prompt tells the agent to call `{name}`, but it is not served                  from a per-solution socket — add it to editor_mcp's SHARED_TOOLS                  or stop promising it"
            );
        }
    }

    #[test]
    fn prompt_includes_working_principles() {
        let prompt = ClaudeAcpAdapter.build_initial_system_prompt(&solution(vec!["m"]));
        assert!(prompt.contains("Quality over speed"));
        assert!(prompt.contains("Partial completion is not done"));
        assert!(prompt.contains("Prefer sub-agents"));
        assert!(prompt.contains("Verify before you claim done"));
        assert!(prompt.contains("Test your own work"));
        assert!(prompt.contains("Keep docs current"));
        assert!(prompt.contains("Don't idle on a blocker"));
    }

    #[test]
    fn prompt_handles_empty_solution() {
        let sol = solution(vec![]);
        let prompt = ClaudeAcpAdapter.build_initial_system_prompt(&sol);
        assert!(prompt.contains("(none yet — solution is empty)"));
    }
}
