//! `solution.git.*` MCP tool registrations owned by `solution_git`.
//!
//! Mirrors the pattern in `git_graph::mcp` — each tool is declared as a
//! typed [`McpServerTool`], registered through
//! [`editor_mcp::register_typed_tool_with_tier`] from
//! [`crate::register_mcp_tools`] which the crate's `init` calls before
//! `editor_mcp::start_server` binds the socket.
//!
//! Tier guard: `solution.git.aggregated_log` is `ReadOnly` — no writes,
//! no destructive ops. Subagent capabilities at `read_only` are enough.

use anyhow::{Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use editor_mcp::{ToolTier, register_typed_tool_with_tier};
use git_ui::providers::{AggregatedCommit, LogQuery};
use gpui::{App, AsyncApp, SharedString};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 5_000;

pub(crate) fn register(cx: &mut App) {
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, AggregatedLogTool);
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, BranchProtectionCheckTool);
}

/// Filter shape mirroring `git_graph::filters::LogFilters` — copied here
/// rather than imported because `solution_git` depends *downward* on
/// `git_ui` only (P-9). The fields are passed straight through to
/// `git log` as CLI args by [`AggregatedLogToolInput::into_query`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AggregatedLogFilters {
    pub branches: Vec<String>,
    pub authors: Vec<String>,
    /// `--since=@<unix>`. Optional.
    pub since_unix: Option<i64>,
    /// `--until=@<unix>`. Optional.
    pub until_unix: Option<i64>,
    /// Repository-relative paths. Each path is interpreted relative to
    /// each member's root; members where the path doesn't exist in HEAD
    /// are skipped (per S-SOL-LOG semantics).
    pub paths: Vec<String>,
    /// Maps to `--grep=<text>`. Mutually exclusive with `search_in_diffs`.
    pub grep: Option<String>,
    /// Maps to `-G<text>` (search added/removed lines).
    pub search_in_diffs: Option<String>,
    /// Treat `grep` / `search_in_diffs` as extended regex.
    pub regex: bool,
    /// Case-insensitive grep.
    pub ignore_case: bool,
    /// `--all` toggle. Ignored when `branches` is non-empty (matches the
    /// precedence rule in `LogFilters::to_git_args`).
    pub all_refs: bool,
    /// Optional SHA pin — passed as a positional `git log` argument.
    pub sha: Option<String>,
}

/// Input parameters for the aggregated log tool tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AggregatedLogToolInput {
    pub filters: AggregatedLogFilters,
    /// Optional members chip filter — list of `SolutionMember::name`
    /// strings. Empty / omitted ⇒ all members of the active Solution.
    pub members: Option<Vec<String>>,
    /// Maximum commits to return. Defaults to 200; capped at 5000.
    pub limit: Option<usize>,
    /// Optional Solution selector. When omitted the aggregator picks the
    /// most-recently-opened Solution (same heuristic the title bar uses).
    /// **Currently unused** — the aggregator always queries the active
    /// Solution. Reserved for the multi-window use case.
    pub solution_id: Option<i64>,
}

impl AggregatedLogToolInput {
    fn into_query(self) -> (LogQuery, Option<Vec<SharedString>>, usize) {
        let mut git_args: Vec<String> = Vec::new();

        if self.filters.all_refs && self.filters.branches.is_empty() {
            git_args.push("--all".to_string());
        }
        if !self.filters.authors.is_empty() {
            let pattern = self.filters.authors.join("|");
            git_args.push(format!("--author={pattern}"));
        }
        if let Some(since) = self.filters.since_unix {
            git_args.push(format!("--since=@{since}"));
        }
        if let Some(until) = self.filters.until_unix {
            git_args.push(format!("--until=@{until}"));
        }
        if let Some(grep) = &self.filters.grep {
            git_args.push(format!("--grep={grep}"));
        }
        if let Some(g) = &self.filters.search_in_diffs {
            git_args.push(format!("-G{g}"));
        }
        if self.filters.regex {
            git_args.push("--extended-regexp".to_string());
        }
        if self.filters.ignore_case {
            git_args.push("--regexp-ignore-case".to_string());
        }
        for branch in &self.filters.branches {
            git_args.push(branch.clone());
        }
        if let Some(sha) = &self.filters.sha {
            git_args.push(sha.clone());
        }

        let paths: Vec<String> = self.filters.paths.clone();
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let members = self
            .members
            .map(|m| m.into_iter().map(SharedString::from).collect());
        (LogQuery { git_args, paths }, members, limit)
    }
}

/// Output of the aggregated log tool tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AggregatedLogToolOutput {
    pub commits: Vec<AggregatedLogCommit>,
    /// `true` if the aggregator returned exactly `limit` commits — caller
    /// should request a wider range to keep paging.
    pub truncated: bool,
}

/// Wire-format mirror of `AggregatedCommit` — `Hsla` is rendered to a
/// CSS-style `hsla(h, s%, l%, a)` string so the output stays JSON-clean.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AggregatedLogCommit {
    pub member_id: String,
    pub member_color: String,
    pub sha: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub committer_date_unix: i64,
    pub subject: String,
    pub ref_names: Vec<String>,
}

impl From<AggregatedCommit> for AggregatedLogCommit {
    fn from(c: AggregatedCommit) -> Self {
        Self {
            member_id: c.member_id.to_string(),
            member_color: format!(
                "hsla({:.0}, {:.0}%, {:.0}%, {:.2})",
                c.member_color.h * 360.0,
                c.member_color.s * 100.0,
                c.member_color.l * 100.0,
                c.member_color.a,
            ),
            sha: c.sha,
            parents: c.parents,
            author_name: c.author_name,
            author_email: c.author_email,
            committer_date_unix: c.committer_date_unix,
            subject: c.subject,
            ref_names: c.ref_names,
        }
    }
}

#[derive(Clone)]
pub struct AggregatedLogTool;

impl McpServerTool for AggregatedLogTool {
    type Input = AggregatedLogToolInput;
    type Output = AggregatedLogToolOutput;
    const NAME: &'static str = "solution.git.aggregated_log";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let solution_id = input.solution_id;
        let (query, members, limit) = input.into_query();
        // `AsyncApp::update` returns the closure's return value directly.
        // The closure produces `Result<Task<Result<…>>, anyhow::Error>` so a
        // single `?` here unwraps the missing-provider error and exposes
        // the `Task`; the second `?` (after `.await`) unwraps the
        // aggregator's per-fetch error.
        let task = cx.update(|cx| {
            let source = git_ui::providers::log_data_source().ok_or_else(|| {
                anyhow!(
                    "no LogDataSource registered — \
                     `solution_git::init` must run before this tool is invoked"
                )
            })?;
            Ok::<_, anyhow::Error>(source.fetch_log(query, solution_id, members, 0..limit, cx))
        })?;
        let commits: Vec<AggregatedCommit> = task.await?;
        let truncated = commits.len() >= limit;
        let wire: Vec<AggregatedLogCommit> = commits.into_iter().map(Into::into).collect();
        let count = wire.len();
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "{} commit(s){}",
                    count,
                    if truncated { " (truncated)" } else { "" }
                ),
            }],
            structured_content: AggregatedLogToolOutput {
                commits: wire,
                truncated,
            },
        })
    }
}

// =====================================================================
// S-SOL-PRT — `solution.git.branch_protection_check` ReadOnly tool.
// Lets a subagent (or any client) query the protection decision for a
// given (repo, branch, op) without performing the op. Useful for the
// agent's pre-flight reasoning ("is this op going to need a confirm?")
// and for the future Settings UI's policy preview.
// =====================================================================

/// Input parameters for the branch protection check tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BranchProtectionCheckInput {
    /// Either an absolute `repo_path` or a `member_id` from the active
    /// Solution. Mutually exclusive — supply exactly one.
    pub repo_path: Option<String>,
    /// `SolutionMember::name` from the active (or specified) Solution.
    pub member_id: Option<String>,
    /// Branch the op targets.
    pub branch: String,
    /// Op name — see `solutions::branch_protection::check` for the
    /// recognised set (`force_push`, `reset`, `delete_branch`, …).
    pub op: String,
    /// Optional Solution selector. When omitted, the active Solution
    /// is used (most-recently-opened heuristic).
    pub solution_id: Option<i64>,
}

/// Output of the branch protection check tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BranchProtectionCheckOutput {
    /// One of `"allowed" | "requires_confirmation" | "forbidden"`.
    pub decision: &'static str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone)]
pub struct BranchProtectionCheckTool;

impl McpServerTool for BranchProtectionCheckTool {
    type Input = BranchProtectionCheckInput;
    type Output = BranchProtectionCheckOutput;
    const NAME: &'static str = "solution.git.branch_protection_check";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        if input.branch.trim().is_empty() {
            return Err(anyhow!("branch must be non-empty"));
        }
        if input.op.trim().is_empty() {
            return Err(anyhow!("op must be non-empty"));
        }
        let repo_path = if let Some(p) = input.repo_path.clone() {
            std::path::PathBuf::from(p)
        } else if let Some(member_id) = input.member_id.clone() {
            let solution_id = input.solution_id;
            cx.update(|cx| resolve_member_path(cx, solution_id, &member_id))
                .ok_or_else(|| anyhow!("member '{member_id}' not found in the active Solution"))?
        } else {
            return Err(anyhow!("either repo_path or member_id is required"));
        };

        let decision = solutions::branch_protection::check(&repo_path, &input.branch, &input.op);
        let (label, reason) = match decision {
            solutions::branch_protection::Decision::Allowed => ("allowed", None),
            solutions::branch_protection::Decision::RequiresConfirmation { reason } => {
                ("requires_confirmation", Some(reason))
            }
            solutions::branch_protection::Decision::Forbidden { reason } => {
                ("forbidden", Some(reason))
            }
        };
        let summary = match &reason {
            Some(r) => format!("{label}: {r}"),
            None => label.to_string(),
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: BranchProtectionCheckOutput {
                decision: label,
                reason,
            },
        })
    }
}

fn resolve_member_path(
    cx: &gpui::App,
    solution_id: Option<i64>,
    member_id: &str,
) -> Option<std::path::PathBuf> {
    let store = solutions::SolutionStore::try_global(cx)?;
    let store = store.read(cx);
    let solution = match solution_id {
        Some(id) => store.find_solution(solutions::SolutionId(id)).ok()?,
        None => store
            .solutions()
            .iter()
            .filter(|s| s.last_opened_at.is_some())
            .max_by_key(|s| s.last_opened_at)
            .or_else(|| store.solutions().first())?,
    };
    solution
        .members
        .iter()
        .find(|m| m.name == member_id)
        .map(|m| m.local_path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn into_query_emits_all_refs_when_no_branches() {
        let input = AggregatedLogToolInput {
            filters: AggregatedLogFilters {
                all_refs: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let (q, _, _) = input.into_query();
        assert!(q.git_args.iter().any(|a| a == "--all"));
    }

    #[test]
    fn into_query_drops_all_refs_when_branches_present() {
        let input = AggregatedLogToolInput {
            filters: AggregatedLogFilters {
                all_refs: true,
                branches: vec!["main".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let (q, _, _) = input.into_query();
        assert!(!q.git_args.iter().any(|a| a == "--all"));
        assert!(q.git_args.iter().any(|a| a == "main"));
    }

    #[test]
    fn into_query_renders_authors_and_dates() {
        let input = AggregatedLogToolInput {
            filters: AggregatedLogFilters {
                authors: vec!["alice".into(), "bob".into()],
                since_unix: Some(100),
                until_unix: Some(200),
                ..Default::default()
            },
            ..Default::default()
        };
        let (q, _, _) = input.into_query();
        assert!(q.git_args.contains(&"--author=alice|bob".to_string()));
        assert!(q.git_args.contains(&"--since=@100".to_string()));
        assert!(q.git_args.contains(&"--until=@200".to_string()));
    }

    #[test]
    fn into_query_clamps_limit() {
        let input = AggregatedLogToolInput {
            limit: Some(10_000_000),
            ..Default::default()
        };
        let (_, _, limit) = input.into_query();
        assert_eq!(limit, MAX_LIMIT);

        let input = AggregatedLogToolInput {
            limit: None,
            ..Default::default()
        };
        let (_, _, limit) = input.into_query();
        assert_eq!(limit, DEFAULT_LIMIT);
    }
}
