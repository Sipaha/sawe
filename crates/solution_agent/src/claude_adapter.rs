use gpui::SharedString;
use solutions::Solution;
use ui::IconName;

use crate::adapter::SolutionAgentAdapter;
use crate::model::AgentServerId;

pub const CLAUDE_ACP_AGENT_ID: &str = "claude-acp";

pub struct ClaudeAcpAdapter;

impl SolutionAgentAdapter for ClaudeAcpAdapter {
    fn agent_id(&self) -> AgentServerId {
        SharedString::from(CLAUDE_ACP_AGENT_ID)
    }

    fn display_name(&self) -> SharedString {
        SharedString::from("Claude")
    }

    fn icon(&self) -> IconName {
        IconName::AiClaude
    }

    fn supports_resume(&self) -> bool {
        true
    }

    fn build_initial_system_prompt(&self, solution: &Solution) -> String {
        let mut buf = String::new();
        buf.push_str("You are working inside a Solution — a multi-project workspace.\n\n");
        buf.push_str(&format!("Solution root: {}\n", solution.root.display()));
        buf.push_str("Member projects (subdirectories you can navigate freely):\n");
        if solution.members.is_empty() {
            buf.push_str("  (none yet — solution is empty)\n");
        } else {
            for member in &solution.members {
                let label = member
                    .local_path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| member.local_path.display().to_string());
                buf.push_str(&format!("  - {label}\n"));
            }
        }
        buf.push_str(
            "\nEach member may contain its own CLAUDE.md with project-specific guidance — \
             read them on demand when working in that subdirectory.\n\n",
        );
        buf.push_str(
            "Build / test / git commands must be run from within a member subdirectory \
             (the solution root has no .git, no Cargo.toml, etc.).\n",
        );
        buf.push_str(&format!(
            "\nThis solution's id is `{id}`. You can manage its member projects with \
             these MCP tools (provided by the `sawe` server):\n\
             - `catalog.list` — list every available registry (catalog) project.\n\
             - `solutions.add_member {{\"solution_id\": {id}, \"catalog_id\": \
             <id from catalog.list>}}` — clone an existing registry project into \
             THIS solution (runs asynchronously).\n\
             - `solutions.add_empty_member {{\"solution_id\": {id}, \"name\": \
             \"<project name>\"}}` — create a new empty, git-initialised (no remote) \
             project in THIS solution.\n\
             Use these whenever the user asks to add an existing project or create a \
             new one in this solution.\n",
            id = solution.id.0,
        ));
        buf.push_str(
            "Stay inside the solution. All file edits, git operations, and shell \
             commands that mutate source code must be confined to the solution \
             root and its member subdirectories. Paths outside it — including \
             other clones of the same repository on disk (~/IdeaProjects, \
             ~/projects, etc.), system directories, and unrelated home folders \
             — are read-only by default; read them when context demands. \
             Editing, committing, or deleting anything out there is allowed \
             only after you name the exact path and the exact change and the \
             user gives an explicit per-action go-ahead. A generic \"do \
             whatever you need\" or blanket up-front permission does not \
             count — confirm each out-of-scope action.\n",
        );
        buf.push_str(
            "\nHow to work (standing rules for this session):\n\
             - Quality over speed. Do the task correctly and robustly, not just \
             fast enough to pass; settle for a lesser solution only after you have \
             genuinely exhausted the viable approaches.\n\
             - Finish the whole task. Partial completion is not done — if any part \
             of the goal remains, keep going instead of stopping at a fraction. But \
             don't gold-plate: do what was asked well, don't invent extra scope.\n\
             - Prefer sub-agents. When a piece of work could be done by sub-agents \
             OR inline in this session, default to dispatching sub-agents — they \
             parallelise independent work, isolate failures, and keep this session's \
             context clean. Keep inline only what is trivial or inseparable from the \
             main thread.\n\
             - Verify before you claim done. Run the real checks — tests (show the \
             output), a clean build, and for any user-visible UI an actual \
             screenshot — and watch for regressions in adjacent behaviour. \"It \
             should work\" is not done.\n\
             - Test your own work. If you lack a tool to verify something, BUILD that \
             tooling yourself (within this solution) and test with it — don't ask the \
             human to test manually until you have exhausted self-verification.\n\
             - Keep docs current. When you finish, update the project's docs to match \
             reality: record new decisions, findings, and changed behaviour, capture \
             decisions the user made during the task, and DELETE stale or wrong info \
             rather than leaving it (skip if the project has no docs).\n\
             - Don't idle on a blocker. Before asking the human a question, check \
             whether the project docs already answer it. If you are genuinely blocked \
             on something only the human can resolve, first record the blocker durably \
             in the project docs, then switch to other independent work that doesn't \
             need the human — don't sit waiting.\n",
        );
        buf
    }
}

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
        // Catalog / project-management awareness: the agent must know the
        // solution id and the tools to list + add projects to it.
        assert!(prompt.contains("solution's id is `14`"));
        assert!(prompt.contains("catalog.list"));
        assert!(prompt.contains("solutions.add_member"));
        assert!(prompt.contains("solutions.add_empty_member"));
        assert!(prompt.contains("\"solution_id\": 14"));
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
