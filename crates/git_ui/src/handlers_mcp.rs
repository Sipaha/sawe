//! MCP tools backing the S-CTM context menu — branch_create_at,
//! tag_create, checkout_revision, compare_revisions. Each tool is tiered
//! per S-BAK so subagents over the `--nc` bridge get the right capability
//! gate (writes need `--write`, reads need only `--read_only`).
//!
//! Source-of-truth: see the parent module `handlers/` for the in-process
//! handlers; the MCP tools wrap raw `git` invocations on the active
//! repository's working directory rather than going through `Repository`,
//! mirroring the existing `editor.git.commit_show` / `editor.git.list_backups`
//! tools (which avoid pulling `git_store::Repository` access into a
//! background task).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use editor_mcp::{
    BranchProtectionHint, ToolTier, register_typed_tool_with_protection,
    register_typed_tool_with_tier,
};
use gpui::{App, AppContext as _, AsyncApp};
use project::git_store::RepositoryId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use util::command::new_command;

pub(crate) fn register(cx: &mut App) {
    register_typed_tool_with_tier(cx, ToolTier::Write, BranchCreateAtTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, TagCreateTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, CheckoutRevisionTool);
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, CompareRevisionsTool);
    // S-BRP — Branches popup MCP surface.
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, ListBranchesTool);
    // S-SOL-PRT — Branch-protection hooks: ops that operate on a
    // payload-supplied branch wire up an extractor; ops that operate on
    // the *current* HEAD branch can't pre-flight via the registry hook
    // (would need synchronous git-CLI access) and rely on the
    // in-process handler check + their existing `confirmed: true`
    // requirement.
    register_typed_tool_with_protection(
        cx,
        ToolTier::Destructive,
        DeleteBranchTool,
        delete_branch_extractor,
    );
    register_typed_tool_with_protection(
        cx,
        ToolTier::Write,
        RenameBranchTool,
        rename_branch_extractor,
    );
    register_typed_tool_with_tier(cx, ToolTier::Write, SetUpstreamTool);
    // Tag / branch lifecycle — round out the surface alongside the context
    // menu / branch picker (list_branches + delete_branch + rename_branch
    // already exist above).
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, ListTagsTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, TagDeleteTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, TagPushTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, TagDeleteRemoteTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, BranchCheckoutTool);
    // S-DST — Destructive commit operations.
    register_typed_tool_with_tier(cx, ToolTier::Write, CherryPickTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, RevertTool);
    register_typed_tool_with_tier(cx, ToolTier::Destructive, ResetTool);
    register_typed_tool_with_tier(cx, ToolTier::Destructive, DropCommitTool);
    register_typed_tool_with_tier(cx, ToolTier::Destructive, SquashRangeTool);
    register_typed_tool_with_tier(cx, ToolTier::Destructive, FixupTool);
    register_typed_tool_with_tier(cx, ToolTier::Destructive, EditCommitMessageTool);
    register_typed_tool_with_tier(cx, ToolTier::Destructive, MoveCommitTool);
    register_typed_tool_with_protection(cx, ToolTier::Write, MergeTool, merge_extractor);
    register_typed_tool_with_protection(cx, ToolTier::Destructive, RebaseTool, rebase_extractor);
    // S-IRB — Interactive rebase + state-machine continuation tools.
    register_typed_tool_with_tier(cx, ToolTier::Destructive, InteractiveRebaseTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, RebaseContinueTool);
    register_typed_tool_with_tier(cx, ToolTier::Destructive, RebaseAbortTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, RebaseSkipTool);
    // S-PCH — Patches: create / apply.
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, CreatePatchTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, ApplyPatchTool);
    // S-SAR — open a snapshot worktree at a specific commit in a new window.
    register_typed_tool_with_tier(cx, ToolTier::Write, ShowAtRevisionTool);
    // S-STH — Stash management surface.
    register_typed_tool_with_tier(cx, ToolTier::Write, StashSaveTool);
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, StashListTool);
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, StashShowTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, StashApplyTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, StashPopTool);
    register_typed_tool_with_tier(cx, ToolTier::Destructive, StashDropTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, StashBranchTool);
    // S-SHL — Shelf MCP surface.
    register_typed_tool_with_tier(cx, ToolTier::Write, ShelfSaveTool);
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, ShelfListTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, ShelfApplyTool);
    register_typed_tool_with_tier(cx, ToolTier::Destructive, ShelfDropTool);
    // S-ANN — read-only blame.
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, BlameTool);
    // S-PCH-HK — run pre-commit checks against the active repository.
    register_typed_tool_with_tier(cx, ToolTier::Write, RunPreCommitChecksTool);
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.branch_create_at`. Creates a branch pointing at
/// `sha` without checking it out by default — flip `checkout` to do both.
pub struct BranchCreateAtInput {
    /// New branch name. Errors if a branch with that name already exists.
    pub name: String,
    /// Commit SHA the new branch should point at.
    pub sha: String,
    /// When `true`, additionally check out the new branch.
    pub checkout: bool,
    /// Repository to operate on. Defaults to the focused window's active
    /// repository.
    pub repo_id: Option<u64>,
}

/// Output of the branch create at tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BranchCreateAtOutput {
    pub branch: String,
    pub sha: String,
    pub checked_out: bool,
}

#[derive(Clone)]
pub struct BranchCreateAtTool;

impl McpServerTool for BranchCreateAtTool {
    type Input = BranchCreateAtInput;
    type Output = BranchCreateAtOutput;
    const NAME: &'static str = "editor.git.branch_create_at";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        if input.name.trim().is_empty() {
            return Err(anyhow!("branch name must be non-empty"));
        }
        // Empty `sha` would otherwise become a stray argument that git
        // rejects with "Failed to resolve '' as a valid ref" — default
        // to `HEAD` so the tool DTRT for "branch from current commit".
        let sha = if input.sha.trim().is_empty() {
            "HEAD"
        } else {
            input.sha.as_str()
        };
        run_git_void(&work_dir, &["branch", &input.name, sha]).await?;
        if input.checkout {
            run_git_void(&work_dir, &["checkout", &input.name]).await?;
        }
        let summary = if input.checkout {
            format!("created and checked out {} at {}", input.name, sha)
        } else {
            format!("created {} at {}", input.name, sha)
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: BranchCreateAtOutput {
                branch: input.name,
                sha: sha.to_string(),
                checked_out: input.checkout,
            },
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.tag_create`. Annotated when `message` is `Some`,
/// lightweight otherwise.
pub struct TagCreateInput {
    pub name: String,
    pub sha: String,
    pub message: Option<String>,
    pub repo_id: Option<u64>,
}

/// Output of the tag create tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagCreateOutput {
    pub tag: String,
    pub sha: String,
    pub annotated: bool,
}

#[derive(Clone)]
pub struct TagCreateTool;

impl McpServerTool for TagCreateTool {
    type Input = TagCreateInput;
    type Output = TagCreateOutput;
    const NAME: &'static str = "editor.git.tag_create";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        if input.name.trim().is_empty() {
            return Err(anyhow!("tag name must be non-empty"));
        }
        let annotated = input.message.is_some();
        // Defaulting `sha` to an empty string makes git see a stray empty
        // argument (`git tag <name> ""`) and abort with "Failed to resolve
        // '' as a valid ref". Treat empty as "tag HEAD".
        let sha = if input.sha.trim().is_empty() {
            "HEAD"
        } else {
            input.sha.as_str()
        };
        if let Some(message) = &input.message {
            run_git_void(&work_dir, &["tag", "-a", "-m", message, &input.name, sha]).await?;
        } else {
            run_git_void(&work_dir, &["tag", &input.name, sha]).await?;
        }
        let summary = format!(
            "created {}{} at {}",
            if annotated { "annotated tag " } else { "tag " },
            input.name,
            sha
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: TagCreateOutput {
                tag: input.name,
                sha: sha.to_string(),
                annotated,
            },
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.checkout_revision`. Accepts a branch name, tag,
/// or commit SHA. The result includes the prior branch name and reports
/// whether HEAD ended up detached (true for raw SHAs, false when the
/// argument resolved to a branch).
pub struct CheckoutRevisionInput {
    pub sha: String,
    pub repo_id: Option<u64>,
    /// When `false` (default), errors if the working tree is dirty. Set
    /// to `true` to invoke `git checkout` regardless — git will refuse
    /// when there are conflicting changes anyway.
    pub force_dirty: bool,
}

/// Output of the checkout revision tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CheckoutRevisionOutput {
    pub sha: String,
    pub detached_head: bool,
    pub prior_branch: Option<String>,
}

#[derive(Clone)]
pub struct CheckoutRevisionTool;

impl McpServerTool for CheckoutRevisionTool {
    type Input = CheckoutRevisionInput;
    type Output = CheckoutRevisionOutput;
    const NAME: &'static str = "editor.git.checkout_revision";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        if !input.force_dirty {
            let status = run_git(&work_dir, &["status", "--porcelain"]).await?;
            if !status.trim().is_empty() {
                return Err(anyhow!(
                    "working tree has uncommitted changes — set force_dirty=true to checkout anyway, or stash first"
                ));
            }
        }
        let prior_branch = run_git(&work_dir, &["symbolic-ref", "--short", "-q", "HEAD"])
            .await
            .ok()
            .and_then(|out| {
                let trimmed = out.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            });
        run_git_void(&work_dir, &["checkout", &input.sha]).await?;
        let detached_head = run_git(&work_dir, &["symbolic-ref", "--short", "-q", "HEAD"])
            .await
            .ok()
            .map(|out| out.trim().is_empty())
            .unwrap_or(true);
        let summary = if detached_head {
            format!("checked out {} (detached HEAD)", input.sha)
        } else {
            format!("checked out {}", input.sha)
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: CheckoutRevisionOutput {
                sha: input.sha,
                detached_head,
                prior_branch,
            },
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.compare_revisions`. Read-only diff between two
/// revisions, optionally restricted to a path subset.
pub struct CompareRevisionsInput {
    pub rev_a: String,
    pub rev_b: String,
    /// Restrict the diff to one or more paths (relative to repo root).
    pub paths: Vec<String>,
    pub repo_id: Option<u64>,
}

/// Output of the compare revisions tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CompareRevisionsOutput {
    pub rev_a: String,
    pub rev_b: String,
    pub files: Vec<DiffFile>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiffFile {
    pub path: String,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    /// Old path when git detected a rename or copy; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename_from: Option<String>,
}

#[derive(Clone)]
pub struct CompareRevisionsTool;

impl McpServerTool for CompareRevisionsTool {
    type Input = CompareRevisionsInput;
    type Output = CompareRevisionsOutput;
    const NAME: &'static str = "editor.git.compare_revisions";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let mut numstat_args: Vec<&str> =
            vec!["diff", "--numstat", "-z", &input.rev_a, &input.rev_b];
        let mut namestatus_args: Vec<&str> =
            vec!["diff", "--name-status", "-z", &input.rev_a, &input.rev_b];
        if !input.paths.is_empty() {
            numstat_args.push("--");
            for p in &input.paths {
                numstat_args.push(p);
            }
            namestatus_args.push("--");
            for p in &input.paths {
                namestatus_args.push(p);
            }
        }
        let stat_out = run_git(&work_dir, &numstat_args).await?;
        let status_out = run_git(&work_dir, &namestatus_args).await?;
        let files = merge_diff(&stat_out, &status_out);
        let summary = format!(
            "diff {}..{}: {} files changed",
            short(&input.rev_a),
            short(&input.rev_b),
            files.len()
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: CompareRevisionsOutput {
                rev_a: input.rev_a,
                rev_b: input.rev_b,
                files,
            },
        })
    }
}

fn short(rev: &str) -> String {
    rev.chars().take(7).collect()
}

fn merge_diff(numstat_z: &str, namestatus_z: &str) -> Vec<DiffFile> {
    let stats = parse_numstat_z(numstat_z);
    let statuses = parse_namestatus_z(namestatus_z);
    stats
        .into_iter()
        .map(|(path, additions, deletions, rename_from)| {
            let status = statuses
                .iter()
                .find(|(p, _, _)| p == &path)
                .map(|(_, status, _)| status.clone())
                .unwrap_or_else(|| "M".to_string());
            DiffFile {
                path,
                status,
                additions,
                deletions,
                rename_from,
            }
        })
        .collect()
}

fn parse_numstat_z(stdout: &str) -> Vec<(String, u32, u32, Option<String>)> {
    let mut out = Vec::new();
    let mut iter = stdout.split('\0').peekable();
    while let Some(record) = iter.next() {
        if record.is_empty() {
            continue;
        }
        let mut tabs = record.splitn(3, '\t');
        let additions: u32 = tabs.next().unwrap_or("0").parse().unwrap_or(0);
        let deletions: u32 = tabs.next().unwrap_or("0").parse().unwrap_or(0);
        let path_part = tabs.next().unwrap_or("");
        if path_part.is_empty() {
            let old = iter.next().unwrap_or("").to_string();
            let new = iter.next().unwrap_or("").to_string();
            out.push((new, additions, deletions, Some(old)));
        } else {
            out.push((path_part.to_string(), additions, deletions, None));
        }
    }
    out
}

fn parse_namestatus_z(stdout: &str) -> Vec<(String, String, Option<String>)> {
    let mut out = Vec::new();
    let mut iter = stdout.split('\0').filter(|s| !s.is_empty());
    while let Some(record) = iter.next() {
        let (status, path_part) = match record.split_once('\t') {
            Some((status, rest)) => (status.to_string(), rest.to_string()),
            None => continue,
        };
        if (status.starts_with('R') || status.starts_with('C')) && path_part.is_empty() {
            let old = iter.next().unwrap_or("").to_string();
            let new = iter.next().unwrap_or("").to_string();
            out.push((new, status, Some(old)));
        } else if status.starts_with('R') || status.starts_with('C') {
            let new = iter.next().unwrap_or("").to_string();
            out.push((new, status, Some(path_part)));
        } else {
            out.push((path_part, status, None));
        }
    }
    out
}

async fn run_git(work_dir: &Path, args: &[&str]) -> Result<String> {
    let work_dir_buf: PathBuf = work_dir.to_path_buf();
    let mut command = new_command("git");
    command.current_dir(&work_dir_buf);
    command.args(args);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await.context("running `git`")?;
    if !output.status.success() {
        return Err(anyhow!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    Ok(String::from_utf8(output.stdout)?)
}

async fn run_git_void(work_dir: &Path, args: &[&str]) -> Result<()> {
    run_git(work_dir, args).await.map(|_| ())
}

/// Synchronous repo-id-to-path resolver, used by the registry-level
/// branch-protection hook. Mirrors [`resolve_work_directory`] but
/// targets a specific id (no "active repo" fallback) and returns
/// `Option<PathBuf>` instead of `Result<Arc<Path>>` because the hook
/// silently skips when the id can't be resolved (the inner tool will
/// surface the error via its own `resolve_work_directory` call).
pub fn resolve_repo_path_by_id(repo_id: u64, cx: &mut App) -> Option<std::path::PathBuf> {
    let want = RepositoryId(repo_id);
    for handle in cx.windows() {
        let Some(multi) = handle.downcast::<workspace::MultiWorkspace>() else {
            continue;
        };
        let found = multi
            .update(cx, |multi, _window, cx| {
                for ws in multi.workspaces() {
                    let project = ws.read(cx).project();
                    let git_store = project.read(cx).git_store().clone();
                    let repo = git_store.read(cx).repositories().get(&want).cloned();
                    if let Some(repo) = repo {
                        return Some(repo.read(cx).work_directory_abs_path.to_path_buf());
                    }
                }
                None
            })
            .ok()
            .flatten();
        if let Some(dir) = found {
            return Some(dir);
        }
    }
    None
}

// ====================================================================
// S-SOL-PRT — `affects_branch` extractors for tools that surface a
// target branch in their typed input. Tools whose target branch is
// "the current HEAD" can't extract anything synchronously (would
// require a git CLI invocation off the dispatcher), so they don't
// register here and rely on the in-process handler check at the UI
// layer plus their `confirmed: true` payload requirement.
// ====================================================================

fn delete_branch_extractor(input: &DeleteBranchInput) -> Option<BranchProtectionHint> {
    if input.name.trim().is_empty() {
        return None;
    }
    Some(BranchProtectionHint {
        repo_path: None,
        repo_id: input.repo_id,
        branch: input.name.clone(),
        op_name: if input.force {
            "delete_branch_force"
        } else {
            "delete_branch"
        },
        confirmed: false,
    })
}

fn rename_branch_extractor(input: &RenameBranchInput) -> Option<BranchProtectionHint> {
    if input.old.trim().is_empty() {
        return None;
    }
    Some(BranchProtectionHint {
        repo_path: None,
        repo_id: input.repo_id,
        branch: input.old.clone(),
        op_name: "rename_branch",
        confirmed: false,
    })
}

fn merge_extractor(input: &MergeInput) -> Option<BranchProtectionHint> {
    if input.target_branch.trim().is_empty() {
        return None;
    }
    Some(BranchProtectionHint {
        repo_path: None,
        repo_id: input.repo_id,
        branch: input.target_branch.clone(),
        op_name: "merge",
        confirmed: false,
    })
}

fn rebase_extractor(input: &RebaseInput) -> Option<BranchProtectionHint> {
    if input.target_branch.trim().is_empty() {
        return None;
    }
    Some(BranchProtectionHint {
        repo_path: None,
        repo_id: input.repo_id,
        branch: input.target_branch.clone(),
        op_name: "rebase",
        confirmed: input.confirmed,
    })
}

fn resolve_work_directory(repo_id: Option<RepositoryId>, cx: &mut App) -> Result<Arc<Path>> {
    let active_window_id = cx.active_window().map(|h| h.window_id());

    if let Some(want) = repo_id {
        for handle in cx.windows() {
            let Some(multi) = handle.downcast::<workspace::MultiWorkspace>() else {
                continue;
            };
            let found = multi
                .update(cx, |multi, _window, cx| {
                    for ws in multi.workspaces() {
                        let project = ws.read(cx).project();
                        let git_store = project.read(cx).git_store().clone();
                        let repo = git_store.read(cx).repositories().get(&want).cloned();
                        if let Some(repo) = repo {
                            return Some(repo.read(cx).work_directory_abs_path.clone());
                        }
                    }
                    None
                })
                .ok()
                .flatten();
            if let Some(dir) = found {
                return Ok(dir);
            }
        }
        return Err(anyhow!("repository_not_found: id={}", want.0));
    }

    for handle in cx.windows() {
        if active_window_id != Some(handle.window_id()) {
            continue;
        }
        let Some(multi) = handle.downcast::<workspace::MultiWorkspace>() else {
            continue;
        };
        let found = multi
            .update(cx, |multi, _window, cx| {
                for ws in multi.workspaces() {
                    let project = ws.read(cx).project();
                    if let Some(repo) = project.read(cx).active_repository(cx) {
                        return Some(repo.read(cx).work_directory_abs_path.clone());
                    }
                }
                None
            })
            .ok()
            .flatten();
        if let Some(dir) = found {
            return Ok(dir);
        }
    }
    Err(anyhow!("no_active_repository"))
}

// =====================================================================
//  S-BRP — Branches popup MCP surface.
//
//  Wraps `git for-each-ref` / `git branch` invocations against the active
//  repository. Lists are read-only; mutations classify as Write or
//  Destructive depending on whether they can lose history.
// =====================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.list_branches`.
pub struct ListBranchesInput {
    /// When `true`, also include remote-tracking branches.
    pub include_remote: bool,
    /// Substring filter applied to the branch name (case-insensitive).
    pub pattern: Option<String>,
    pub repo_id: Option<u64>,
}

/// Output of the list branches tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListBranchesOutput {
    pub branches: Vec<BranchEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BranchEntry {
    pub name: String,
    pub is_remote: bool,
    pub is_head: bool,
    pub upstream: Option<String>,
    pub upstream_track: Option<String>,
    pub subject: Option<String>,
    pub committer_date_relative: Option<String>,
}

#[derive(Clone)]
pub struct ListBranchesTool;

impl McpServerTool for ListBranchesTool {
    type Input = ListBranchesInput;
    type Output = ListBranchesOutput;
    const NAME: &'static str = "editor.git.list_branches";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let mut args: Vec<&str> = vec![
            "for-each-ref",
            "--format=%(HEAD)%09%(refname:short)%09%(refname)%09%(upstream:short)%09%(upstream:track)%09%(committerdate:relative)%09%(contents:subject)",
            "refs/heads",
        ];
        if input.include_remote {
            args.push("refs/remotes");
        }
        let raw = run_git(&work_dir, &args).await?;
        let pattern_lower = input.pattern.as_deref().map(|p| p.to_lowercase());
        let mut branches = Vec::new();
        for line in raw.lines() {
            let cols: Vec<&str> = line.splitn(7, '\t').collect();
            if cols.len() < 3 {
                continue;
            }
            let head_marker = cols[0].trim();
            let short = cols[1].trim().to_string();
            let full_ref = cols[2].trim();
            if let Some(p) = pattern_lower.as_deref() {
                if !short.to_lowercase().contains(p) {
                    continue;
                }
            }
            let upstream = cols
                .get(3)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let upstream_track = cols
                .get(4)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let date = cols
                .get(5)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let subject = cols
                .get(6)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            branches.push(BranchEntry {
                name: short,
                is_remote: full_ref.starts_with("refs/remotes/"),
                is_head: head_marker == "*",
                upstream,
                upstream_track,
                subject,
                committer_date_relative: date,
            });
        }
        let summary = format!("{} branch(es)", branches.len());
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: ListBranchesOutput { branches },
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.delete_branch`.
pub struct DeleteBranchInput {
    pub name: String,
    /// `false` runs `git branch -d` (refuses on unmerged); `true` runs
    /// `git branch -D` (lossy — gated as `Destructive`).
    pub force: bool,
    pub repo_id: Option<u64>,
}

/// Output of the delete branch tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeleteBranchOutput {
    pub name: String,
    pub forced: bool,
}

#[derive(Clone)]
pub struct DeleteBranchTool;

impl McpServerTool for DeleteBranchTool {
    type Input = DeleteBranchInput;
    type Output = DeleteBranchOutput;
    const NAME: &'static str = "editor.git.delete_branch";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let force = input.force;
        let name = input.name.clone();
        cx.background_spawn(async move {
            git::operations::OpRunner::run(
                git::operations::DeleteBranchOp { name, force },
                &work_dir_buf,
            )
        })
        .await?;
        let summary = format!(
            "deleted branch {}{}",
            input.name,
            if input.force { " (force)" } else { "" }
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: DeleteBranchOutput {
                name: input.name,
                forced: input.force,
            },
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.rename_branch`.
pub struct RenameBranchInput {
    pub old: String,
    pub new: String,
    pub repo_id: Option<u64>,
}

/// Output of the rename branch tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RenameBranchOutput {
    pub old: String,
    pub new: String,
}

#[derive(Clone)]
pub struct RenameBranchTool;

impl McpServerTool for RenameBranchTool {
    type Input = RenameBranchInput;
    type Output = RenameBranchOutput;
    const NAME: &'static str = "editor.git.rename_branch";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let old = input.old.clone();
        let new = input.new.clone();
        cx.background_spawn(async move {
            git::operations::OpRunner::run(
                git::operations::RenameBranchOp { old, new },
                &work_dir_buf,
            )
        })
        .await?;
        let summary = format!("renamed branch {} -> {}", input.old, input.new);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: RenameBranchOutput {
                old: input.old,
                new: input.new,
            },
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.set_upstream`.
pub struct SetUpstreamInput {
    pub branch: String,
    /// Remote-tracking ref (e.g. `origin/main`).
    pub upstream_ref: String,
    pub repo_id: Option<u64>,
}

/// Output of the set upstream tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SetUpstreamOutput {
    pub branch: String,
    pub upstream_ref: String,
}

#[derive(Clone)]
pub struct SetUpstreamTool;

impl McpServerTool for SetUpstreamTool {
    type Input = SetUpstreamInput;
    type Output = SetUpstreamOutput;
    const NAME: &'static str = "editor.git.set_upstream";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        run_git_void(
            &work_dir,
            &["branch", "-u", &input.upstream_ref, &input.branch],
        )
        .await?;
        let summary = format!("set upstream of {} to {}", input.branch, input.upstream_ref);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: SetUpstreamOutput {
                branch: input.branch,
                upstream_ref: input.upstream_ref,
            },
        })
    }
}

// =====================================================================
//  Tag / branch lifecycle — round out the `editor.git.*` surface so an
//  agent can do the same things the commit context menu / branch picker
//  expose: list tags, delete / push / delete-remote a tag, delete /
//  rename / checkout a branch. All shell out via `run_git*`, mirroring
//  the rest of this module (no `Repository` entity access — see the
//  module doc comment).
// =====================================================================

/// Input for `editor.git.list_tags`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ListTagsInput {
    /// Case-insensitive substring filter applied to tag names.
    pub pattern: Option<String>,
    /// Repository to query. Defaults to the focused window's active repo.
    pub repo_id: Option<u64>,
}

/// Output of `editor.git.list_tags` — tags newest-first by creator date.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListTagsOutput {
    pub tags: Vec<TagEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagEntry {
    pub name: String,
    pub creator_date_relative: Option<String>,
}

#[derive(Clone)]
pub struct ListTagsTool;

impl McpServerTool for ListTagsTool {
    type Input = ListTagsInput;
    type Output = ListTagsOutput;
    const NAME: &'static str = "editor.git.list_tags";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let raw = run_git(
            &work_dir,
            &[
                "tag",
                "--sort=-creatordate",
                "--format=%(refname:short)%09%(creatordate:relative)",
            ],
        )
        .await?;
        let pattern = input.pattern.as_deref().map(str::to_lowercase);
        let mut tags = Vec::new();
        for line in raw.lines() {
            let mut cols = line.splitn(2, '\t');
            let Some(name) = cols.next().map(str::trim).filter(|s| !s.is_empty()) else {
                continue;
            };
            if let Some(p) = pattern.as_deref()
                && !name.to_lowercase().contains(p)
            {
                continue;
            }
            let date = cols
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            tags.push(TagEntry {
                name: name.to_string(),
                creator_date_relative: date,
            });
        }
        let summary = format!("{} tag(s)", tags.len());
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: ListTagsOutput { tags },
        })
    }
}

/// Input for `editor.git.tag_delete` — `git tag -d <name>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TagDeleteInput {
    /// Tag to delete.
    pub name: String,
    /// Repository to operate on. Defaults to the focused window's active repo.
    pub repo_id: Option<u64>,
}

/// Output of `editor.git.tag_delete`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagDeleteOutput {
    pub tag: String,
}

#[derive(Clone)]
pub struct TagDeleteTool;

impl McpServerTool for TagDeleteTool {
    type Input = TagDeleteInput;
    type Output = TagDeleteOutput;
    const NAME: &'static str = "editor.git.tag_delete";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        if input.name.trim().is_empty() {
            return Err(anyhow!("tag name must be non-empty"));
        }
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        run_git_void(&work_dir, &["tag", "-d", &input.name]).await?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("deleted tag {}", input.name),
            }],
            structured_content: TagDeleteOutput { tag: input.name },
        })
    }
}

/// Input for `editor.git.tag_push` — `git push <remote> <tag>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TagPushInput {
    /// Tag to push.
    pub name: String,
    /// Remote to push to. Defaults to `origin`.
    pub remote: Option<String>,
    /// Repository to operate on. Defaults to the focused window's active repo.
    pub repo_id: Option<u64>,
}

/// Output of `editor.git.tag_push` / `editor.git.tag_delete_remote`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RemoteTagOutput {
    pub tag: String,
    pub remote: String,
}

#[derive(Clone)]
pub struct TagPushTool;

impl McpServerTool for TagPushTool {
    type Input = TagPushInput;
    type Output = RemoteTagOutput;
    const NAME: &'static str = "editor.git.tag_push";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        if input.name.trim().is_empty() {
            return Err(anyhow!("tag name must be non-empty"));
        }
        let remote = input
            .remote
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| "origin".to_string());
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        run_git_void(&work_dir, &["push", &remote, &input.name]).await?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("pushed tag {} to {}", input.name, remote),
            }],
            structured_content: RemoteTagOutput {
                tag: input.name,
                remote,
            },
        })
    }
}

/// Input for `editor.git.tag_delete_remote` — `git push <remote> --delete
/// refs/tags/<name>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TagDeleteRemoteInput {
    /// Tag to delete on the remote.
    pub name: String,
    /// Remote to delete from. Defaults to `origin`.
    pub remote: Option<String>,
    /// Repository to operate on. Defaults to the focused window's active repo.
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct TagDeleteRemoteTool;

impl McpServerTool for TagDeleteRemoteTool {
    type Input = TagDeleteRemoteInput;
    type Output = RemoteTagOutput;
    const NAME: &'static str = "editor.git.tag_delete_remote";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        if input.name.trim().is_empty() {
            return Err(anyhow!("tag name must be non-empty"));
        }
        let remote = input
            .remote
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| "origin".to_string());
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        run_git_void(
            &work_dir,
            &[
                "push",
                &remote,
                "--delete",
                &format!("refs/tags/{}", input.name),
            ],
        )
        .await?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("deleted tag {} on {}", input.name, remote),
            }],
            structured_content: RemoteTagOutput {
                tag: input.name,
                remote,
            },
        })
    }
}

/// Output of `editor.git.branch_checkout`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BranchNameOutput {
    pub branch: String,
}

/// Input for `editor.git.branch_checkout` — switch to an existing branch
/// (`git switch <name>`), or create + switch (`git switch -c <name>`).
/// Unlike `editor.git.checkout_revision` this leaves HEAD attached.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BranchCheckoutInput {
    /// Branch to switch to (or create when `create` is `true`).
    pub name: String,
    /// When `true`, create the branch at the current HEAD first (`-c`).
    pub create: bool,
    /// Repository to operate on. Defaults to the focused window's active repo.
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct BranchCheckoutTool;

impl McpServerTool for BranchCheckoutTool {
    type Input = BranchCheckoutInput;
    type Output = BranchNameOutput;
    const NAME: &'static str = "editor.git.branch_checkout";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        if input.name.trim().is_empty() {
            return Err(anyhow!("branch name must be non-empty"));
        }
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        if input.create {
            run_git_void(&work_dir, &["switch", "-c", &input.name]).await?;
        } else {
            run_git_void(&work_dir, &["switch", &input.name]).await?;
        }
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: if input.create {
                    format!("created and switched to {}", input.name)
                } else {
                    format!("switched to {}", input.name)
                },
            }],
            structured_content: BranchNameOutput { branch: input.name },
        })
    }
}

// =====================================================================
//  S-DST — Destructive commit operations MCP surface.
//
//  Each tool returns the post-op state via a shared `DestructiveOutcome`
//  shape: `outcome: "completed" | "paused_for_conflict"` plus
//  `conflicted_files` when paused. Destructive-tier tools require
//  `confirmed: true` in the input; the registry tier check rejects calls
//  from a `read_only` / `write` subagent before this code runs, but the
//  per-call confirmation is an additional opt-in.
// =====================================================================

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DestructiveOutcome {
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicted_files: Option<Vec<String>>,
}

fn outcome_payload(outcome: git::operations::RunOutcome) -> DestructiveOutcome {
    match outcome {
        git::operations::RunOutcome::Completed => DestructiveOutcome {
            outcome: "completed".into(),
            conflicted_files: None,
        },
        git::operations::RunOutcome::PausedForConflict { conflicted_files } => DestructiveOutcome {
            outcome: "paused_for_conflict".into(),
            conflicted_files: Some(
                conflicted_files
                    .into_iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect(),
            ),
        },
        git::operations::RunOutcome::PausedForExecFailure { command, stderr } => {
            DestructiveOutcome {
                outcome: "paused_for_exec_failure".into(),
                conflicted_files: Some(vec![format!("exec {command} failed: {stderr}")]),
            }
        }
    }
}

fn require_confirmed(input_confirmed: bool, op_label: &str) -> Result<()> {
    if input_confirmed {
        return Ok(());
    }
    Err(anyhow!(
        "{op_label} is destructive and requires confirmed=true in the call payload"
    ))
}

/// Input parameters for the cherry pick tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CherryPickInput {
    pub shas: Vec<String>,
    pub no_commit: bool,
    pub x: bool,
    pub mainline: Option<u32>,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct CherryPickTool;

impl McpServerTool for CherryPickTool {
    type Input = CherryPickInput;
    type Output = DestructiveOutcome;
    const NAME: &'static str = "editor.git.cherry_pick";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let outcome = cx
            .background_spawn(async move {
                git::operations::OpRunner::run(
                    git::operations::cherry_pick::CherryPickOp {
                        shas: input.shas.clone(),
                        no_commit: input.no_commit,
                        mainline: input.mainline,
                        x: input.x,
                    },
                    &work_dir_buf,
                )
            })
            .await?;
        let payload = outcome_payload(outcome);
        let summary = format!(
            "cherry-pick {} commit(s) → {}",
            payload.outcome, payload.outcome
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: payload,
        })
    }
}

/// Input parameters for the revert tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RevertInput {
    pub shas: Vec<String>,
    pub no_commit: bool,
    pub mainline: Option<u32>,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct RevertTool;

impl McpServerTool for RevertTool {
    type Input = RevertInput;
    type Output = DestructiveOutcome;
    const NAME: &'static str = "editor.git.revert";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let outcome = cx
            .background_spawn(async move {
                git::operations::OpRunner::run(
                    git::operations::revert::RevertOp {
                        shas: input.shas.clone(),
                        no_commit: input.no_commit,
                        mainline: input.mainline,
                    },
                    &work_dir_buf,
                )
            })
            .await?;
        let payload = outcome_payload(outcome);
        let summary = format!("revert → {}", payload.outcome);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: payload,
        })
    }
}

/// Input parameters for the reset tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ResetInput {
    pub sha: String,
    /// One of `"soft" | "mixed" | "hard" | "keep"`. Defaults to `"mixed"`.
    pub mode: String,
    pub confirmed: bool,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct ResetTool;

impl McpServerTool for ResetTool {
    type Input = ResetInput;
    type Output = DestructiveOutcome;
    const NAME: &'static str = "editor.git.reset";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        require_confirmed(input.confirmed, "reset")?;
        let mode = parse_reset_mode(&input.mode)?;
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let sha = input.sha.clone();
        let outcome = cx
            .background_spawn(async move {
                git::operations::OpRunner::run(
                    git::operations::reset::ResetOp { sha, mode },
                    &work_dir_buf,
                )
            })
            .await?;
        let payload = outcome_payload(outcome);
        let summary = format!("reset --{} {} → {}", input.mode, input.sha, payload.outcome);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: payload,
        })
    }
}

fn parse_reset_mode(mode: &str) -> Result<git::operations::reset::ResetMode> {
    use git::operations::reset::ResetMode;
    match mode.to_ascii_lowercase().as_str() {
        "soft" => Ok(ResetMode::Soft),
        "mixed" | "" => Ok(ResetMode::Mixed),
        "hard" => Ok(ResetMode::Hard),
        "keep" => Ok(ResetMode::Keep),
        other => Err(anyhow!(
            "unknown reset mode {other:?}; expected soft|mixed|hard|keep"
        )),
    }
}

/// Input parameters for the drop commit tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DropCommitInput {
    pub sha: String,
    pub confirmed: bool,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct DropCommitTool;

impl McpServerTool for DropCommitTool {
    type Input = DropCommitInput;
    type Output = DestructiveOutcome;
    const NAME: &'static str = "editor.git.drop_commit";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        require_confirmed(input.confirmed, "drop_commit")?;
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let sha = input.sha.clone();
        let handle = git::operations::drop_commit::run_drop(
            &work_dir_buf,
            &sha,
            git::operations::rebase::RebaseCallbacks::default(),
        )
        .await?;
        let payload = rebase_outcome_payload(&handle);
        let summary = format!("drop {} → {}", input.sha, payload.outcome);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: payload,
        })
    }
}

fn rebase_outcome_payload(handle: &git::operations::rebase::RebaseHandle) -> DestructiveOutcome {
    use git::operations::rebase::RebaseState;
    match handle.state() {
        RebaseState::Completed => DestructiveOutcome {
            outcome: "completed".into(),
            conflicted_files: None,
        },
        RebaseState::PausedForConflict { conflicted_files } => DestructiveOutcome {
            outcome: "paused_for_conflict".into(),
            conflicted_files: Some(
                conflicted_files
                    .into_iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect(),
            ),
        },
        RebaseState::PausedForEdit { current_sha } => DestructiveOutcome {
            outcome: "paused_for_edit".into(),
            conflicted_files: Some(vec![format!("paused at {current_sha}")]),
        },
        RebaseState::PausedForExecFailure { command, stderr } => DestructiveOutcome {
            outcome: "paused_for_exec_failure".into(),
            conflicted_files: Some(vec![format!("exec {command} failed: {stderr}")]),
        },
        RebaseState::Running => DestructiveOutcome {
            outcome: "running".into(),
            conflicted_files: None,
        },
        RebaseState::Aborted => DestructiveOutcome {
            outcome: "aborted".into(),
            conflicted_files: None,
        },
        RebaseState::Failed(err) => DestructiveOutcome {
            outcome: "failed".into(),
            conflicted_files: Some(vec![err]),
        },
    }
}

/// Input parameters for the squash range tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SquashRangeInput {
    pub shas: Vec<String>,
    pub message: String,
    pub confirmed: bool,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct SquashRangeTool;

impl McpServerTool for SquashRangeTool {
    type Input = SquashRangeInput;
    type Output = DestructiveOutcome;
    const NAME: &'static str = "editor.git.squash_range";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        require_confirmed(input.confirmed, "squash_range")?;
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let handle = git::operations::squash::SquashOp {
            shas: input.shas.clone(),
            final_message: input.message.clone(),
        }
        .run(
            &work_dir_buf,
            git::operations::rebase::RebaseCallbacks::default(),
        )
        .await?;
        let payload = rebase_outcome_payload(&handle);
        let summary = format!("squash {} commits → {}", input.shas.len(), payload.outcome);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: payload,
        })
    }
}

/// Input parameters for the fixup tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FixupInput {
    pub shas: Vec<String>,
    pub confirmed: bool,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct FixupTool;

impl McpServerTool for FixupTool {
    type Input = FixupInput;
    type Output = DestructiveOutcome;
    const NAME: &'static str = "editor.git.fixup";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        require_confirmed(input.confirmed, "fixup")?;
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let handle = git::operations::fixup::FixupOp {
            shas: input.shas.clone(),
        }
        .run(
            &work_dir_buf,
            git::operations::rebase::RebaseCallbacks::default(),
        )
        .await?;
        let payload = rebase_outcome_payload(&handle);
        let summary = format!("fixup {} commits → {}", input.shas.len(), payload.outcome);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: payload,
        })
    }
}

/// Input parameters for the edit commit message tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EditCommitMessageInput {
    pub sha: String,
    pub new_message: String,
    pub confirmed: bool,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct EditCommitMessageTool;

impl McpServerTool for EditCommitMessageTool {
    type Input = EditCommitMessageInput;
    type Output = DestructiveOutcome;
    const NAME: &'static str = "editor.git.edit_commit_message";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        require_confirmed(input.confirmed, "edit_commit_message")?;
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let outcome = git::operations::edit_commit_message::EditMessageOp {
            sha: input.sha.clone(),
            new_message: input.new_message.clone(),
        }
        .run(
            &work_dir_buf,
            git::operations::rebase::RebaseCallbacks::default(),
        )
        .await?;
        let payload = match outcome {
            git::operations::edit_commit_message::EditMessageOutcome::Direct(out) => {
                outcome_payload(out)
            }
            git::operations::edit_commit_message::EditMessageOutcome::ViaRebase(handle) => {
                rebase_outcome_payload(&handle)
            }
        };
        let summary = format!("edit_commit_message {} → {}", input.sha, payload.outcome);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: payload,
        })
    }
}

/// Input parameters for the move commit tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MoveCommitInput {
    pub source_sha: String,
    pub target_sha: String,
    /// `"before"` or `"after"`.
    pub position: String,
    pub confirmed: bool,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct MoveCommitTool;

impl McpServerTool for MoveCommitTool {
    type Input = MoveCommitInput;
    type Output = DestructiveOutcome;
    const NAME: &'static str = "editor.git.move_commit";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        require_confirmed(input.confirmed, "move_commit")?;
        let position = match input.position.to_ascii_lowercase().as_str() {
            "before" => git::operations::move_commit::BeforeOrAfter::Before,
            "after" => git::operations::move_commit::BeforeOrAfter::After,
            other => return Err(anyhow!("unknown position {other:?}; expected before|after")),
        };
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let handle = git::operations::move_commit::MoveCommitOp {
            source_sha: input.source_sha.clone(),
            target_sha: input.target_sha.clone(),
            position,
        }
        .run(
            &work_dir_buf,
            git::operations::rebase::RebaseCallbacks::default(),
        )
        .await?;
        let payload = rebase_outcome_payload(&handle);
        let summary = format!(
            "move {} {} {} → {}",
            input.source_sha, input.position, input.target_sha, payload.outcome
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: payload,
        })
    }
}

/// Input parameters for the merge tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MergeInput {
    pub target_branch: String,
    pub no_ff: bool,
    pub squash: bool,
    pub message: Option<String>,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct MergeTool;

impl McpServerTool for MergeTool {
    type Input = MergeInput;
    type Output = DestructiveOutcome;
    const NAME: &'static str = "editor.git.merge";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let target = input.target_branch.clone();
        let outcome = cx
            .background_spawn(async move {
                git::operations::OpRunner::run(
                    git::operations::merge::MergeOp {
                        target_branch: input.target_branch.clone(),
                        no_ff: input.no_ff,
                        squash: input.squash,
                        message: input.message.clone(),
                    },
                    &work_dir_buf,
                )
            })
            .await?;
        let payload = outcome_payload(outcome);
        let summary = format!("merge {} → {}", target, payload.outcome);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: payload,
        })
    }
}

/// Input parameters for the rebase tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RebaseInput {
    pub target_branch: String,
    pub autostash: bool,
    pub confirmed: bool,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct RebaseTool;

impl McpServerTool for RebaseTool {
    type Input = RebaseInput;
    type Output = DestructiveOutcome;
    const NAME: &'static str = "editor.git.rebase";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        require_confirmed(input.confirmed, "rebase")?;
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let target = input.target_branch.clone();
        let outcome = cx
            .background_spawn(async move {
                git::operations::OpRunner::run(
                    git::operations::linear_rebase::LinearRebaseOp {
                        target_branch: input.target_branch.clone(),
                        autostash: input.autostash,
                    },
                    &work_dir_buf,
                )
            })
            .await?;
        let payload = outcome_payload(outcome);
        let summary = format!("rebase onto {} → {}", target, payload.outcome);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: payload,
        })
    }
}

// =====================================================================
//  S-IRB Interactive rebase MCP surface.
//
//  Delegates to the shared run_rebase engine the UI uses. Shell-command
//  steps in the supplied todo are gated behind the
//  git_panel.interactive_rebase.allow_exec_via_mcp setting (default
//  off): a Destructive subagent cannot ask git to spawn arbitrary shell
//  commands without the user opting in via settings.
//
//  rebase_{continue,abort,skip} wrap the matching git rebase verbs
//  against the active repo. Useful when a rebase was paused via the CLI
//  and the agent should resume it.
// =====================================================================

/// Input parameters for the interactive rebase tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct InteractiveRebaseInput {
    pub base_sha: String,
    pub todo: Vec<RebaseActionInput>,
    pub confirmed: bool,
    pub repo_id: Option<u64>,
}

/// Input parameters for the rebase action tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RebaseActionInput {
    /// One of pick, reword, edit, squash, fixup, drop, exec.
    pub action: String,
    /// Commit SHA. Ignored for the exec action.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha: String,
    /// Replacement message (required for reword, optional for squash).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_message: Option<String>,
    /// Shell command (required for the exec action).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_command: Option<String>,
}

#[derive(Clone)]
pub struct InteractiveRebaseTool;

impl McpServerTool for InteractiveRebaseTool {
    type Input = InteractiveRebaseInput;
    type Output = DestructiveOutcome;
    const NAME: &'static str = "editor.git.interactive_rebase";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        require_confirmed(input.confirmed, "interactive_rebase")?;

        let allow_shell_via_mcp = cx.update(|cx| {
            use settings::Settings as _;
            crate::git_panel_settings::GitPanelSettings::get_global(cx)
                .interactive_rebase
                .allow_exec_via_mcp
        });

        let todo = build_mcp_todo(&input.todo, allow_shell_via_mcp)?;
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let base = input.base_sha.clone();
        let handle = cx
            .background_spawn(async move {
                git::operations::rebase::run_rebase(
                    &work_dir_buf,
                    &base,
                    todo,
                    git::operations::rebase::RebaseCallbacks::default(),
                )
                .await
            })
            .await?;
        let payload = rebase_outcome_payload(&handle);
        let summary = format!(
            "interactive rebase from {} ({} steps) → {}",
            input.base_sha,
            input.todo.len(),
            payload.outcome
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: payload,
        })
    }
}

pub(crate) fn build_mcp_todo(
    actions: &[RebaseActionInput],
    allow_shell_via_mcp: bool,
) -> Result<git::operations::rebase::RebaseTodo> {
    use git::operations::rebase::RebaseTodoBuilder;
    let mut builder = RebaseTodoBuilder::new();
    for action in actions {
        match action.action.to_ascii_lowercase().as_str() {
            "pick" => {
                require_sha(&action.sha, "pick")?;
                builder = builder.pick(action.sha.clone());
            }
            "reword" => {
                require_sha(&action.sha, "reword")?;
                let message = action
                    .new_message
                    .clone()
                    .ok_or_else(|| anyhow!("reword action requires new_message in payload"))?;
                builder = builder.reword(action.sha.clone(), message);
            }
            "edit" => {
                require_sha(&action.sha, "edit")?;
                builder = builder.edit(action.sha.clone());
            }
            "squash" => {
                require_sha(&action.sha, "squash")?;
                builder = builder.squash(action.sha.clone());
            }
            "fixup" => {
                require_sha(&action.sha, "fixup")?;
                builder = builder.fixup(action.sha.clone());
            }
            "drop" => {
                require_sha(&action.sha, "drop")?;
                builder = builder.drop(action.sha.clone());
            }
            "exec" => {
                if !allow_shell_via_mcp {
                    return Err(anyhow!(
                        "exec actions are blocked via MCP; enable git_panel.interactive_rebase.allow_exec_via_mcp to permit them"
                    ));
                }
                let cmd = action
                    .exec_command
                    .clone()
                    .ok_or_else(|| anyhow!("exec action requires exec_command in payload"))?;
                if cmd.trim().is_empty() {
                    return Err(anyhow!("exec_command must be non-empty"));
                }
                builder = builder.exec(cmd);
            }
            other => {
                return Err(anyhow!(
                    "unknown rebase action {other:?}; expected one of pick|reword|edit|squash|fixup|drop|exec"
                ));
            }
        }
    }
    Ok(builder.build())
}

fn require_sha(sha: &str, action: &str) -> Result<()> {
    if sha.trim().is_empty() {
        Err(anyhow!("{action} action requires a non-empty sha"))
    } else {
        Ok(())
    }
}

/// Input parameters for the rebase continuation tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RebaseContinuationInput {
    pub repo_id: Option<u64>,
}

/// Output of the rebase continuation tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RebaseContinuationOutput {
    pub op: &'static str,
    pub completed: bool,
}

#[derive(Clone)]
pub struct RebaseContinueTool;

impl McpServerTool for RebaseContinueTool {
    type Input = RebaseContinuationInput;
    type Output = RebaseContinuationOutput;
    const NAME: &'static str = "editor.git.rebase_continue";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        run_rebase_state_command("continue", input.repo_id, cx).await
    }
}

#[derive(Clone)]
pub struct RebaseAbortTool;

impl McpServerTool for RebaseAbortTool {
    type Input = RebaseContinuationInput;
    type Output = RebaseContinuationOutput;
    const NAME: &'static str = "editor.git.rebase_abort";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        run_rebase_state_command("abort", input.repo_id, cx).await
    }
}

#[derive(Clone)]
pub struct RebaseSkipTool;

impl McpServerTool for RebaseSkipTool {
    type Input = RebaseContinuationInput;
    type Output = RebaseContinuationOutput;
    const NAME: &'static str = "editor.git.rebase_skip";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        run_rebase_state_command("skip", input.repo_id, cx).await
    }
}

async fn run_rebase_state_command(
    op: &'static str,
    repo_id: Option<u64>,
    cx: &mut AsyncApp,
) -> Result<ToolResponse<RebaseContinuationOutput>> {
    let work_dir = cx.update(|cx| resolve_work_directory(repo_id.map(RepositoryId), cx))?;
    let arg = match op {
        "continue" => "--continue",
        "abort" => "--abort",
        "skip" => "--skip",
        other => return Err(anyhow!("unknown rebase op {other:?}")),
    };
    run_git_void(&work_dir, &["rebase", arg]).await?;
    let summary = format!("git rebase {arg} → ok");
    Ok(ToolResponse {
        content: vec![ToolResponseContent::Text { text: summary }],
        structured_content: RebaseContinuationOutput {
            op,
            completed: true,
        },
    })
}

// =====================================================================
//  S-PCH — Patches: create / apply MCP surface.
//
//  `editor.git.create_patch` returns the patch bytes inline so the
//  subagent can decide where to write them; the tool itself doesn't
//  touch the filesystem outside of `git format-patch --stdout`.
//
//  `editor.git.apply_patch` writes the supplied text to a tempfile,
//  detects the format, and shells out to `git am` / `git apply`. The
//  tempfile is deleted after the operation regardless of outcome.
// =====================================================================

/// Input parameters for the create patch tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CreatePatchInput {
    pub sha_from: String,
    /// When `Some`, format-patch the range `sha_from..sha_to`. When `None`,
    /// produces a single-commit patch for `sha_from`.
    pub sha_to: Option<String>,
    pub repo_id: Option<u64>,
}

/// Output of the create patch tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CreatePatchOutput {
    pub patches: Vec<PatchEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PatchEntry {
    pub content: String,
    pub suggested_filename: String,
}

#[derive(Clone)]
pub struct CreatePatchTool;

impl McpServerTool for CreatePatchTool {
    type Input = CreatePatchInput;
    type Output = CreatePatchOutput;
    const NAME: &'static str = "editor.git.create_patch";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let sha_from = input.sha_from.clone();
        let sha_to = input.sha_to.clone();
        let patches = cx
            .background_spawn(async move {
                let temp =
                    tempfile::tempdir().map_err(|err| anyhow!("create_patch tempdir: {err}"))?;
                let paths = git::operations::patch::create_patch(
                    &work_dir_buf,
                    &sha_from,
                    sha_to.as_deref(),
                    Some(temp.path()),
                )?;
                let mut entries = Vec::with_capacity(paths.len());
                for path in &paths {
                    let bytes = std::fs::read(path)
                        .map_err(|err| anyhow!("read patch {}: {err}", path.display()))?;
                    let content = String::from_utf8_lossy(&bytes).to_string();
                    let filename = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "patch".into());
                    entries.push(PatchEntry {
                        content,
                        suggested_filename: filename,
                    });
                }
                Ok::<Vec<PatchEntry>, anyhow::Error>(entries)
            })
            .await?;
        let summary = format!("create_patch → {} file(s)", patches.len());
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: CreatePatchOutput { patches },
        })
    }
}

/// Input parameters for the apply patch tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ApplyPatchInput {
    /// Raw patch / mbox / diff text. The tool detects the format and
    /// picks `git am` vs. `git apply --3way` vs. `git apply` accordingly.
    pub patch_text: String,
    /// When `false`, disable 3-way merge (overrides the format default).
    pub three_way: Option<bool>,
    /// When `true`, pass `--keep-cr` to `git am` (mbox flow).
    pub keep_cr: Option<bool>,
    /// On apply failure, retry with `git apply --reject` (writes `.rej`
    /// files for failed hunks instead of failing).
    pub apply_with_reject: Option<bool>,
    pub repo_id: Option<u64>,
}

/// Output of the apply patch tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApplyPatchOutput {
    /// One of `"clean" | "conflict" | "rejected_hunks"`.
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicted_files: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject_files: Option<Vec<String>>,
    pub format: String,
}

#[derive(Clone)]
pub struct ApplyPatchTool;

impl McpServerTool for ApplyPatchTool {
    type Input = ApplyPatchInput;
    type Output = ApplyPatchOutput;
    const NAME: &'static str = "editor.git.apply_patch";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_buf = PathBuf::from(work_dir.as_ref());
        let bytes = input.patch_text.into_bytes();
        let format = git::operations::patch::detect_patch_format(&bytes)?;
        let format_label = format.label().to_string();
        let three_way_default = matches!(
            format,
            git::operations::patch::PatchFormat::UnifiedWithIndex
                | git::operations::patch::PatchFormat::Mbox
        );
        let keep_cr_default = matches!(format, git::operations::patch::PatchFormat::Mbox);
        let three_way = input.three_way.unwrap_or(three_way_default);
        let keep_cr = input.keep_cr.unwrap_or(keep_cr_default);
        let apply_with_reject = input.apply_with_reject.unwrap_or(false);

        let outcome = cx
            .background_spawn(async move {
                let temp = tempfile::NamedTempFile::with_suffix(".patch")
                    .map_err(|err| anyhow!("apply_patch tempfile: {err}"))?;
                std::fs::write(temp.path(), &bytes)
                    .map_err(|err| anyhow!("write patch tempfile: {err}"))?;
                let outcome = git::operations::patch::apply_patch(
                    &work_dir_buf,
                    temp.path(),
                    git::operations::patch::ApplyOptions {
                        three_way,
                        keep_cr,
                        apply_with_reject,
                    },
                );
                drop(temp);
                outcome
            })
            .await?;
        let payload = match outcome {
            git::operations::patch::ApplyOutcome::Clean => ApplyPatchOutput {
                outcome: "clean".into(),
                conflicted_files: None,
                reject_files: None,
                format: format_label.clone(),
            },
            git::operations::patch::ApplyOutcome::Conflict { conflicted_files } => {
                ApplyPatchOutput {
                    outcome: "conflict".into(),
                    conflicted_files: Some(
                        conflicted_files
                            .into_iter()
                            .map(|p| p.to_string_lossy().to_string())
                            .collect(),
                    ),
                    reject_files: None,
                    format: format_label.clone(),
                }
            }
            git::operations::patch::ApplyOutcome::RejectedHunks { reject_files } => {
                ApplyPatchOutput {
                    outcome: "rejected_hunks".into(),
                    conflicted_files: None,
                    reject_files: Some(
                        reject_files
                            .into_iter()
                            .map(|p| p.to_string_lossy().to_string())
                            .collect(),
                    ),
                    format: format_label.clone(),
                }
            }
        };
        let summary = format!("apply_patch ({format_label}) → {}", payload.outcome);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: payload,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.show_at_revision`. Opens a read-only snapshot
/// worktree of the active (or `repo_id`-selected) repository at `sha`
/// in a brand-new top-level workspace window. The new window does not
/// inherit Solution membership.
pub struct ShowAtRevisionInput {
    /// Commit SHA to snapshot. Resolved by `git worktree add --detach`,
    /// so any rev-parseable expression (full / short SHA, `HEAD~3`,
    /// tag, branch name) is accepted by git itself.
    pub sha: String,
    /// Repository to snapshot. Defaults to the focused window's
    /// active repository.
    pub repo_id: Option<u64>,
}

/// Output of the show at revision tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShowAtRevisionOutput {
    /// `WindowId` of the new top-level workspace window. Use it with
    /// `windows.focus` / `windows.dump_visual_structure` from a
    /// driving harness.
    pub window_id: u64,
    /// Absolute path of the snapshot worktree. The on-close hook
    /// removes it via `git worktree remove --force`; orphan cleanup
    /// at next startup catches leftovers if the editor crashed first.
    pub worktree_path: String,
}

#[derive(Clone)]
pub struct ShowAtRevisionTool;

impl McpServerTool for ShowAtRevisionTool {
    type Input = ShowAtRevisionInput;
    type Output = ShowAtRevisionOutput;
    const NAME: &'static str = "editor.git.show_at_revision";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let want_repo = input.repo_id.map(RepositoryId);
        let sha = input.sha.clone();

        // Find a workspace + repository to snapshot. We can't dispatch
        // through the workspace action directly because we want to
        // hand back the new WindowId, so we replicate the inner shape
        // of `show_at_revision_action` here against the resolved
        // workspace handle.
        let resolved: Result<(
            gpui::WindowHandle<workspace::MultiWorkspace>,
            gpui::Entity<workspace::Workspace>,
            gpui::Entity<project::git_store::Repository>,
        )> = cx.update(|cx| {
            let active_window_id = cx.active_window().map(|h| h.window_id());
            let mut found: Option<(
                gpui::WindowHandle<workspace::MultiWorkspace>,
                gpui::Entity<workspace::Workspace>,
                gpui::Entity<project::git_store::Repository>,
            )> = None;
            for handle in cx.windows() {
                if want_repo.is_none() && active_window_id != Some(handle.window_id()) {
                    continue;
                }
                let Some(multi) = handle.downcast::<workspace::MultiWorkspace>() else {
                    continue;
                };
                let result = multi
                    .update(cx, |multi, _window, cx| {
                        for ws in multi.workspaces() {
                            let project = ws.read(cx).project().clone();
                            let repo = match want_repo {
                                Some(id) => project
                                    .read(cx)
                                    .git_store()
                                    .read(cx)
                                    .repositories()
                                    .get(&id)
                                    .cloned(),
                                None => project.read(cx).active_repository(cx),
                            };
                            if let Some(repo) = repo {
                                return Some((ws.clone(), repo));
                            }
                        }
                        None
                    })
                    .ok()
                    .flatten();
                if let Some((ws, repo)) = result {
                    found = Some((multi, ws, repo));
                    break;
                }
            }
            found.ok_or_else(|| {
                anyhow!(
                    "show_at_revision: no workspace with {} repository",
                    if want_repo.is_some() {
                        "the requested"
                    } else {
                        "an active"
                    }
                )
            })
        });
        let (multi_handle, workspace_entity, repo) = resolved?;

        // Dispatch through a window update so we have both a
        // `&mut Window` and a `Context<Workspace>` for the handler.
        let task = multi_handle.update(cx, |_multi, window, cx| {
            workspace_entity.update(cx, |workspace, cx| {
                crate::handlers::show_at_revision::show_at_revision(
                    workspace, repo, sha, window, cx,
                )
            })
        })?;
        let new_window: gpui::WindowHandle<workspace::MultiWorkspace> = task.await?;

        let path_str: String = cx.update(|cx| {
            let path: Option<std::path::PathBuf> =
                new_window
                    .read(cx)
                    .ok()
                    .and_then(|multi: &workspace::MultiWorkspace| {
                        let ws = multi.workspace().clone();
                        let project = ws.read(cx).project().clone();
                        project
                            .read(cx)
                            .visible_worktrees(cx)
                            .next()
                            .map(|w| w.read(cx).abs_path().to_path_buf())
                    });
            path.map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        });

        let summary = format!(
            "show_at_revision({}) → window {}",
            input.sha,
            new_window.window_id().as_u64()
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: ShowAtRevisionOutput {
                window_id: new_window.window_id().as_u64(),
                worktree_path: path_str,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numstat_with_renames() {
        let raw = "5\t3\tsrc/foo.rs\x002\t1\t\x00src/old.rs\x00src/new.rs\x00";
        let entries = parse_numstat_z(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "src/foo.rs");
        assert_eq!(entries[0].1, 5);
        assert_eq!(entries[0].2, 3);
        assert!(entries[0].3.is_none());
        assert_eq!(entries[1].0, "src/new.rs");
        assert_eq!(entries[1].3.as_deref(), Some("src/old.rs"));
    }

    #[test]
    fn parses_namestatus_with_rename() {
        let raw = "M\tsrc/foo.rs\x00R100\t\x00src/old.rs\x00src/new.rs\x00";
        let entries = parse_namestatus_z(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], ("src/foo.rs".into(), "M".into(), None));
        assert_eq!(
            entries[1],
            (
                "src/new.rs".into(),
                "R100".into(),
                Some("src/old.rs".into())
            )
        );
    }

    #[test]
    fn build_mcp_todo_translates_reword() {
        let actions = vec![
            RebaseActionInput {
                action: "pick".into(),
                sha: "aaaa".into(),
                ..Default::default()
            },
            RebaseActionInput {
                action: "reword".into(),
                sha: "bbbb".into(),
                new_message: Some("rewritten".into()),
                ..Default::default()
            },
        ];
        let todo = build_mcp_todo(&actions, false).expect("build");
        let body = todo.serialize_with_helper("/h --git-message-set");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "pick aaaa");
        assert_eq!(lines[1], "pick bbbb");
        assert!(lines[2].starts_with("exec /h --git-message-set "));
    }

    #[test]
    fn build_mcp_todo_blocks_exec_when_setting_off() {
        let actions = vec![RebaseActionInput {
            action: "exec".into(),
            exec_command: Some("rm -rf /".into()),
            ..Default::default()
        }];
        let result = build_mcp_todo(&actions, false);
        match result {
            Ok(_) => panic!("expected exec to be rejected when setting is off"),
            Err(err) => {
                let msg = format!("{err}");
                assert!(
                    msg.contains("allow_exec_via_mcp"),
                    "expected setting reference in error, got {msg}"
                );
            }
        }
    }

    #[test]
    fn build_mcp_todo_allows_exec_when_setting_on() {
        let actions = vec![RebaseActionInput {
            action: "exec".into(),
            exec_command: Some("make test".into()),
            ..Default::default()
        }];
        let todo = build_mcp_todo(&actions, true).expect("build");
        let body = todo.serialize_with_helper("/x");
        assert_eq!(body, "exec make test\n");
    }

    #[test]
    fn build_mcp_todo_rejects_unknown_action() {
        let actions = vec![RebaseActionInput {
            action: "nope".into(),
            sha: "aaaa".into(),
            ..Default::default()
        }];
        let result = build_mcp_todo(&actions, true);
        match result {
            Ok(_) => panic!("expected unknown action to be rejected"),
            Err(err) => assert!(format!("{err}").contains("unknown rebase action")),
        }
    }
}

// =====================================================================
//  S-STH — Stash management MCP surface.
//
//  All tools shell out via `run_git*` helpers against the resolved working
//  directory rather than going through `project::git_store::Repository`,
//  matching the rest of `handlers_mcp.rs`. The destructive `stash_drop`
//  tool requires `confirmed: true` per `require_confirmed`.
// =====================================================================

/// Input parameters for the stash save tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StashSaveInput {
    /// Optional message for the stash entry. When `None`, git uses the
    /// default `WIP on <branch>` form.
    pub message: Option<String>,
    /// `git stash push --include-untracked`.
    pub include_untracked: bool,
    /// `git stash push --keep-index` (keeps already-staged hunks in the
    /// index after the stash is taken).
    pub keep_index: bool,
    pub repo_id: Option<u64>,
}

/// Output of the stash save tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StashSaveOutput {
    pub created: bool,
    pub message: Option<String>,
    pub include_untracked: bool,
    pub keep_index: bool,
}

#[derive(Clone)]
pub struct StashSaveTool;

impl McpServerTool for StashSaveTool {
    type Input = StashSaveInput;
    type Output = StashSaveOutput;
    const NAME: &'static str = "editor.git.stash_save";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let mut args: Vec<String> = vec!["stash".into(), "push".into()];
        if input.include_untracked {
            args.push("--include-untracked".into());
        }
        if input.keep_index {
            args.push("--keep-index".into());
        }
        if let Some(message) = input.message.as_deref().filter(|m| !m.is_empty()) {
            args.push("-m".into());
            args.push(message.to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let raw = run_git(&work_dir, &arg_refs).await?;
        let created = !raw.contains("No local changes to save");
        let summary = if created {
            "stash saved".to_string()
        } else {
            "no local changes to stash".to_string()
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: StashSaveOutput {
                created,
                message: input.message,
                include_untracked: input.include_untracked,
                keep_index: input.keep_index,
            },
        })
    }
}

/// Input parameters for the stash list tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StashListInput {
    pub repo_id: Option<u64>,
}

/// Output of the stash list tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StashListOutput {
    pub entries: Vec<StashEntryPayload>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StashEntryPayload {
    pub index: usize,
    pub stash_ref: String,
    pub stash_sha: String,
    pub created_at_unix: i64,
    pub message: String,
    pub branch: Option<String>,
}

#[derive(Clone)]
pub struct StashListTool;

impl McpServerTool for StashListTool {
    type Input = StashListInput;
    type Output = StashListOutput;
    const NAME: &'static str = "editor.git.stash_list";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let raw = run_git(
            &work_dir,
            &["stash", "list", "--pretty=format:%gd%x00%H%x00%ct%x00%s"],
        )
        .await?;
        let parsed: git::stash::GitStash = raw.parse().unwrap_or_default();
        let entries: Vec<StashEntryPayload> = parsed
            .entries
            .iter()
            .map(|entry| StashEntryPayload {
                index: entry.index,
                stash_ref: format!("stash@{{{}}}", entry.index),
                stash_sha: entry.oid.to_string(),
                created_at_unix: entry.timestamp,
                message: entry.message.clone(),
                branch: entry.branch.clone(),
            })
            .collect();
        let summary = format!(
            "{} stash entr{}",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" }
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: StashListOutput { entries },
        })
    }
}

/// Input parameters for the stash show tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StashShowInput {
    /// Either a `stash@{N}` ref or a stash sha. Required.
    pub stash_ref: String,
    pub repo_id: Option<u64>,
}

/// Output of the stash show tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StashShowOutput {
    pub stash_ref: String,
    pub patch: String,
}

#[derive(Clone)]
pub struct StashShowTool;

impl McpServerTool for StashShowTool {
    type Input = StashShowInput;
    type Output = StashShowOutput;
    const NAME: &'static str = "editor.git.stash_show";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        require_stash_ref(&input.stash_ref, "stash_show")?;
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let patch = run_git(
            &work_dir,
            &["stash", "show", "-p", "--no-color", &input.stash_ref],
        )
        .await?;
        let summary = format!("stash {} patch ({} bytes)", input.stash_ref, patch.len());
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: StashShowOutput {
                stash_ref: input.stash_ref,
                patch,
            },
        })
    }
}

/// Input parameters for the stash apply tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StashApplyInput {
    pub stash_ref: String,
    pub repo_id: Option<u64>,
}

/// Output of the stash mutation tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StashMutationOutput {
    pub stash_ref: String,
}

#[derive(Clone)]
pub struct StashApplyTool;

impl McpServerTool for StashApplyTool {
    type Input = StashApplyInput;
    type Output = StashMutationOutput;
    const NAME: &'static str = "editor.git.stash_apply";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        require_stash_ref(&input.stash_ref, "stash_apply")?;
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        run_git_void(&work_dir, &["stash", "apply", &input.stash_ref]).await?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("applied {}", input.stash_ref),
            }],
            structured_content: StashMutationOutput {
                stash_ref: input.stash_ref,
            },
        })
    }
}

/// Input parameters for the stash pop tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StashPopInput {
    pub stash_ref: String,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct StashPopTool;

impl McpServerTool for StashPopTool {
    type Input = StashPopInput;
    type Output = StashMutationOutput;
    const NAME: &'static str = "editor.git.stash_pop";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        require_stash_ref(&input.stash_ref, "stash_pop")?;
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        run_git_void(&work_dir, &["stash", "pop", &input.stash_ref]).await?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("popped {}", input.stash_ref),
            }],
            structured_content: StashMutationOutput {
                stash_ref: input.stash_ref,
            },
        })
    }
}

/// Input parameters for the stash drop tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StashDropInput {
    pub stash_ref: String,
    pub confirmed: bool,
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct StashDropTool;

impl McpServerTool for StashDropTool {
    type Input = StashDropInput;
    type Output = StashMutationOutput;
    const NAME: &'static str = "editor.git.stash_drop";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        require_confirmed(input.confirmed, "stash_drop")?;
        require_stash_ref(&input.stash_ref, "stash_drop")?;
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        run_git_void(&work_dir, &["stash", "drop", &input.stash_ref]).await?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("dropped {}", input.stash_ref),
            }],
            structured_content: StashMutationOutput {
                stash_ref: input.stash_ref,
            },
        })
    }
}

/// Input parameters for the stash branch tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StashBranchInput {
    pub name: String,
    pub stash_ref: String,
    pub repo_id: Option<u64>,
}

/// Output of the stash branch tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StashBranchOutput {
    pub name: String,
    pub stash_ref: String,
}

#[derive(Clone)]
pub struct StashBranchTool;

impl McpServerTool for StashBranchTool {
    type Input = StashBranchInput;
    type Output = StashBranchOutput;
    const NAME: &'static str = "editor.git.stash_branch";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        if input.name.trim().is_empty() {
            return Err(anyhow!("stash_branch requires a non-empty name"));
        }
        require_stash_ref(&input.stash_ref, "stash_branch")?;
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        run_git_void(
            &work_dir,
            &["stash", "branch", &input.name, &input.stash_ref],
        )
        .await?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("branched {} from {}", input.name, input.stash_ref),
            }],
            structured_content: StashBranchOutput {
                name: input.name,
                stash_ref: input.stash_ref,
            },
        })
    }
}

fn require_stash_ref(stash_ref: &str, op: &str) -> Result<()> {
    if stash_ref.trim().is_empty() {
        return Err(anyhow!("{op} requires a non-empty stash_ref"));
    }
    Ok(())
}

// ====================================================================
//  S-SHL — Shelf MCP surface.
//
//  Bridges the in-process `git::operations::shelf` API to MCP. The on-disk
//  shelf store is shared across the editor process and any subagent over
//  the `--nc` bridge: subagents read/write through these typed tools, the
//  UI uses the API direct. `shelf_drop` is destructive and gated through
//  `require_confirmed`.
// ====================================================================

/// Input parameters for the shelf save tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ShelfSaveInput {
    pub name: String,
    pub description: Option<String>,
    /// Repo-relative paths. When `None` or empty, the entire working-tree
    /// diff is shelved.
    pub paths: Option<Vec<String>>,
    /// Default `true` — `git stash push` removes shelved changes from the
    /// working tree. Set to `false` to retain them after the entry is
    /// captured (the stash itself still exists).
    pub remove_after: Option<bool>,
    pub repo_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShelfEntryPayload {
    pub name: String,
    pub stash_sha: String,
    pub created_at_unix: i64,
    pub source_branch: Option<String>,
    pub description: Option<String>,
    pub count_added: u32,
    pub count_modified: u32,
    pub count_deleted: u32,
    pub total_lines_added: u32,
    pub total_lines_removed: u32,
    pub top_paths: Vec<String>,
    pub orphaned: bool,
}

impl ShelfEntryPayload {
    fn from_entry(entry: &git::operations::shelf::ShelfEntry, orphaned: bool) -> Self {
        Self {
            name: entry.name.clone(),
            stash_sha: entry.stash_sha.clone(),
            created_at_unix: entry.created_at_unix,
            source_branch: entry.source_branch.clone(),
            description: entry.description.clone(),
            count_added: entry.files_summary.count_added,
            count_modified: entry.files_summary.count_modified,
            count_deleted: entry.files_summary.count_deleted,
            total_lines_added: entry.files_summary.total_lines_added,
            total_lines_removed: entry.files_summary.total_lines_removed,
            top_paths: entry.files_summary.top_paths.clone(),
            orphaned,
        }
    }
}

/// Output of the shelf save tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShelfSaveOutput {
    pub entry: ShelfEntryPayload,
}

#[derive(Clone)]
pub struct ShelfSaveTool;

impl McpServerTool for ShelfSaveTool {
    type Input = ShelfSaveInput;
    type Output = ShelfSaveOutput;
    const NAME: &'static str = "editor.git.shelf_save";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        if input.name.trim().is_empty() {
            return Err(anyhow!("shelf_save requires a non-empty name"));
        }
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let path_bufs: Option<Vec<PathBuf>> = input
            .paths
            .as_ref()
            .filter(|paths| !paths.is_empty())
            .map(|paths| paths.iter().map(PathBuf::from).collect());
        let name = input.name.clone();
        let description = input.description.clone();
        let remove_after = input.remove_after.unwrap_or(true);
        let work_dir_inner = work_dir.clone();
        let entry = cx
            .background_spawn(async move {
                git::operations::shelf::shelve(
                    &work_dir_inner,
                    &name,
                    description,
                    path_bufs,
                    remove_after,
                )
            })
            .await?;
        let payload = ShelfEntryPayload::from_entry(&entry, false);
        let summary = format!("shelved {:?} ({})", entry.name, entry.stash_sha);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: ShelfSaveOutput { entry: payload },
        })
    }
}

/// Input parameters for the shelf list tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ShelfListInput {
    pub repo_id: Option<u64>,
}

/// Output of the shelf list tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShelfListOutput {
    pub entries: Vec<ShelfEntryPayload>,
}

#[derive(Clone)]
pub struct ShelfListTool;

impl McpServerTool for ShelfListTool {
    type Input = ShelfListInput;
    type Output = ShelfListOutput;
    const NAME: &'static str = "editor.git.shelf_list";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let work_dir_inner = work_dir.clone();
        let entries: Vec<ShelfEntryPayload> = cx
            .background_spawn(async move {
                let store = git::operations::shelf::ShelfStore::load(&work_dir_inner)?;
                let orphans = store.lookup_orphaned(&work_dir_inner);
                let payload: Vec<ShelfEntryPayload> = store
                    .entries()
                    .iter()
                    .map(|entry| {
                        let orphaned = orphans.iter().any(|name| name == &entry.name);
                        ShelfEntryPayload::from_entry(entry, orphaned)
                    })
                    .collect();
                anyhow::Ok(payload)
            })
            .await?;
        let summary = format!(
            "{} shelf entr{}",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" }
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: ShelfListOutput { entries },
        })
    }
}

/// Input parameters for the shelf apply tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ShelfApplyInput {
    pub name: String,
    /// When `true`, drop the underlying stash and remove the shelf entry
    /// after a successful apply. Defaults to `false`.
    pub remove_from_shelf: Option<bool>,
    pub repo_id: Option<u64>,
}

/// Output of the shelf apply tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShelfApplyOutput {
    pub name: String,
    pub removed: bool,
}

#[derive(Clone)]
pub struct ShelfApplyTool;

impl McpServerTool for ShelfApplyTool {
    type Input = ShelfApplyInput;
    type Output = ShelfApplyOutput;
    const NAME: &'static str = "editor.git.shelf_apply";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        if input.name.trim().is_empty() {
            return Err(anyhow!("shelf_apply requires a non-empty name"));
        }
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let name = input.name.clone();
        let remove = input.remove_from_shelf.unwrap_or(false);
        let work_dir_inner = work_dir.clone();
        let name_inner = name.clone();
        cx.background_spawn(async move {
            git::operations::shelf::apply(&work_dir_inner, &name_inner, remove)
        })
        .await?;
        let summary = if remove {
            format!("applied {:?} and removed from shelf", name)
        } else {
            format!("applied {:?}", name)
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: ShelfApplyOutput {
                name,
                removed: remove,
            },
        })
    }
}

/// Input parameters for the shelf drop tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ShelfDropInput {
    pub name: String,
    pub confirmed: bool,
    pub repo_id: Option<u64>,
}

/// Output of the shelf drop tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShelfDropOutput {
    pub name: String,
}

#[derive(Clone)]
pub struct ShelfDropTool;

impl McpServerTool for ShelfDropTool {
    type Input = ShelfDropInput;
    type Output = ShelfDropOutput;
    const NAME: &'static str = "editor.git.shelf_drop";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        require_confirmed(input.confirmed, "shelf_drop")?;
        if input.name.trim().is_empty() {
            return Err(anyhow!("shelf_drop requires a non-empty name"));
        }
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let name = input.name.clone();
        let work_dir_inner = work_dir.clone();
        let name_inner = name.clone();
        cx.background_spawn(
            async move { git::operations::shelf::drop(&work_dir_inner, &name_inner) },
        )
        .await?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("dropped shelf entry {:?}", name),
            }],
            structured_content: ShelfDropOutput { name },
        })
    }
}

// ====================================================================
//  S-ANN — `editor.git.blame`
//
//  Read-only blame for a path inside the active repository. Wraps
//  `git blame --line-porcelain` so callers see the same output the
//  editor's gutter would render, plus optional toggles for the IDEA-
//  style `ignore_whitespace` and `follow_renames` flags.
// ====================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.blame`. `path` is interpreted relative to the
/// repository's working directory.
pub struct BlameInput {
    pub path: String,
    pub ignore_whitespace: bool,
    /// When `true`, follow file renames + copy-detection (`-M -C`).
    pub follow_renames: bool,
    pub repo_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct McpBlameEntry {
    pub sha: String,
    pub start_line: u32,
    pub end_line: u32,
    pub author: Option<String>,
    pub author_email: Option<String>,
    pub author_time: Option<i64>,
    pub author_tz: Option<String>,
    pub committer: Option<String>,
    pub committer_email: Option<String>,
    pub committer_time: Option<i64>,
    pub summary: Option<String>,
    pub previous: Option<String>,
    pub filename: Option<String>,
}

/// Output of the blame tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BlameOutput {
    pub entries: Vec<McpBlameEntry>,
}

#[derive(Clone)]
pub struct BlameTool;

impl McpServerTool for BlameTool {
    type Input = BlameInput;
    type Output = BlameOutput;
    const NAME: &'static str = "editor.git.blame";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        if input.path.trim().is_empty() {
            return Err(anyhow!("blame: path is required"));
        }
        let mut args: Vec<&str> = vec!["blame", "--line-porcelain"];
        if input.ignore_whitespace {
            args.push("-w");
        }
        if input.follow_renames {
            args.push("-M");
            args.push("-C");
        }
        args.push("--");
        args.push(&input.path);
        let raw = run_git(&work_dir, &args).await?;
        let entries = parse_line_porcelain(&raw);
        let summary = format!("blame {} returned {} entries", input.path, entries.len());
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: BlameOutput { entries },
        })
    }
}

/// Parse `git blame --line-porcelain` output. Each line block starts
/// with a header `<sha> <orig_line> <final_line> <count?>` followed by
/// metadata lines (`author`, `author-mail`, etc.) and ends with a
/// `\t<source-line>` content line.
///
/// We coalesce consecutive lines pointing at the same commit into a
/// single entry covering the full final-line range.
fn parse_line_porcelain(out: &str) -> Vec<McpBlameEntry> {
    let mut entries: Vec<McpBlameEntry> = Vec::new();
    let mut author_cache: collections::HashMap<String, McpBlameEntry> =
        collections::HashMap::default();
    let mut current: Option<McpBlameEntry> = None;
    let mut current_final_line: Option<u32> = None;

    for line in out.lines() {
        if let Some(entry) = current.as_mut() {
            if let Some(content_after_tab) = line.strip_prefix('\t') {
                let _ = content_after_tab;
                if let Some(final_line) = current_final_line.take() {
                    let mut entry = current.take().expect("current set");
                    let sha = entry.sha.clone();
                    if let Some(seed) = author_cache.get(&sha) {
                        if entry.author.is_none() {
                            entry.author = seed.author.clone();
                        }
                        if entry.author_email.is_none() {
                            entry.author_email = seed.author_email.clone();
                        }
                        if entry.author_time.is_none() {
                            entry.author_time = seed.author_time;
                        }
                        if entry.author_tz.is_none() {
                            entry.author_tz = seed.author_tz.clone();
                        }
                        if entry.committer.is_none() {
                            entry.committer = seed.committer.clone();
                        }
                        if entry.committer_email.is_none() {
                            entry.committer_email = seed.committer_email.clone();
                        }
                        if entry.committer_time.is_none() {
                            entry.committer_time = seed.committer_time;
                        }
                        if entry.summary.is_none() {
                            entry.summary = seed.summary.clone();
                        }
                    } else {
                        author_cache.insert(sha, entry.clone());
                    }
                    entry.start_line = final_line;
                    entry.end_line = final_line;
                    if let Some(prev) = entries.last_mut() {
                        if prev.sha == entry.sha && prev.end_line + 1 == entry.start_line {
                            prev.end_line = entry.end_line;
                            continue;
                        }
                    }
                    entries.push(entry);
                }
                continue;
            }
            if let Some((key, value)) = line.split_once(' ') {
                match key {
                    "author" => entry.author = Some(value.to_string()),
                    "author-mail" => entry.author_email = Some(value.to_string()),
                    "author-time" => entry.author_time = value.parse().ok(),
                    "author-tz" => entry.author_tz = Some(value.to_string()),
                    "committer" => entry.committer = Some(value.to_string()),
                    "committer-mail" => entry.committer_email = Some(value.to_string()),
                    "committer-time" => entry.committer_time = value.parse().ok(),
                    "summary" => entry.summary = Some(value.to_string()),
                    "previous" => entry.previous = Some(value.to_string()),
                    "filename" => entry.filename = Some(value.to_string()),
                    _ => {}
                }
            } else if line == "boundary" {
                // ignored — boundary commits are still real commits we
                // want to surface to the caller.
            }
            continue;
        }
        let mut parts = line.split_whitespace();
        let sha = match parts.next() {
            Some(s) if s.len() == 40 => s.to_string(),
            _ => continue,
        };
        let _orig_line: Option<u32> = parts.next().and_then(|s| s.parse().ok());
        let final_line: Option<u32> = parts.next().and_then(|s| s.parse().ok());
        current = Some(McpBlameEntry {
            sha,
            start_line: final_line.unwrap_or(0),
            end_line: final_line.unwrap_or(0),
            author: None,
            author_email: None,
            author_time: None,
            author_tz: None,
            committer: None,
            committer_email: None,
            committer_time: None,
            summary: None,
            previous: None,
            filename: None,
        });
        current_final_line = final_line;
    }
    entries
}

// =====================================================================
//  S-PCH-HK — Before-commit checks (`editor.git.run_pre_commit_checks`).
//
//  Drives the same `pre_commit::CheckRunner` pipeline the commit panel
//  uses, but exposes a one-shot input so a subagent can ask "would my
//  current staged state pass the configured checks?" without round-
//  tripping through the panel UI. Result mirrors `CheckResult`: passed
//  / failed-with-output / aborted (the latter only on cancellation by
//  the host process and currently unreachable from the MCP path).

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Input for `editor.git.run_pre_commit_checks`. The full set of
/// supported `Check` shapes is encoded as a struct rather than a tagged
/// enum so the MCP schema stays JSON-RPC-friendly.
pub struct RunPreCommitChecksInput {
    /// Sequence of checks to run in order. Empty list returns
    /// immediately with `passed = true`.
    pub checks: Vec<RunPreCommitCheck>,
    /// Optional repo to operate on. Defaults to the focused window's
    /// active repository.
    pub repo_id: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RunPreCommitCheck {
    /// One of `format` / `organize_imports` / `task` / `hook`. Other
    /// values produce a per-check failure outcome rather than a
    /// top-level error.
    pub kind: String,
    /// `tasks.json` `label` to run when `kind = "task"`. Ignored for
    /// other kinds.
    pub task_name: Option<String>,
}

/// Output of the run pre commit checks tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RunPreCommitChecksOutput {
    pub all_passed: bool,
    pub outcomes: Vec<PreCommitOutcomeWire>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PreCommitOutcomeWire {
    pub which: String,
    pub passed: bool,
    pub output: String,
}

#[derive(Clone)]
pub struct RunPreCommitChecksTool;

impl McpServerTool for RunPreCommitChecksTool {
    type Input = RunPreCommitChecksInput;
    type Output = RunPreCommitChecksOutput;
    const NAME: &'static str = "editor.git.run_pre_commit_checks";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;

        let mut outcomes = Vec::with_capacity(input.checks.len());
        let mut all_passed = true;

        for check in &input.checks {
            let outcome = run_one_check(check, &work_dir).await;
            if !outcome.passed {
                all_passed = false;
                outcomes.push(outcome);
                // Mirror panel behavior: stop at the first failure.
                break;
            }
            outcomes.push(outcome);
        }

        let summary = if all_passed {
            format!("ran {} pre-commit check(s); all passed", outcomes.len())
        } else {
            let failed = outcomes
                .last()
                .map(|o| o.which.clone())
                .unwrap_or_else(|| "unknown".to_string());
            format!("pre-commit check failed: {failed}")
        };

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: RunPreCommitChecksOutput {
                all_passed,
                outcomes,
            },
        })
    }
}

async fn run_one_check(check: &RunPreCommitCheck, work_dir: &Path) -> PreCommitOutcomeWire {
    match check.kind.as_str() {
        "hook" => match crate::pre_commit::run_pre_commit_hook(work_dir).await {
            Ok(()) => PreCommitOutcomeWire {
                which: "Run pre-commit hook".to_string(),
                passed: true,
                output: String::new(),
            },
            Err(e) => PreCommitOutcomeWire {
                which: "Run pre-commit hook".to_string(),
                passed: false,
                output: format!("{e:#}"),
            },
        },
        "format" | "organize_imports" => PreCommitOutcomeWire {
            which: match check.kind.as_str() {
                "format" => "Format".to_string(),
                _ => "Organize imports".to_string(),
            },
            passed: false,
            output: format!(
                "{} requires an editor session and is not available via MCP yet",
                check.kind
            ),
        },
        "task" => {
            let label = check
                .task_name
                .clone()
                .unwrap_or_else(|| "<missing>".to_string());
            PreCommitOutcomeWire {
                which: format!("Run task: {label}"),
                passed: false,
                output:
                    "task-based checks require the project task inventory; not yet supported via MCP"
                        .to_string(),
            }
        }
        other => PreCommitOutcomeWire {
            which: format!("<unknown:{other}>"),
            passed: false,
            output: format!("unsupported check kind `{other}`"),
        },
    }
}

#[cfg(test)]
mod blame_tests {
    use super::*;

    #[test]
    fn parses_line_porcelain_simple() {
        let raw = "1234567890123456789012345678901234567890 1 1 1\n\
            author Alice\n\
            author-mail <alice@example.com>\n\
            author-time 1700000000\n\
            author-tz +0100\n\
            committer Alice\n\
            committer-mail <alice@example.com>\n\
            committer-time 1700000000\n\
            committer-tz +0100\n\
            summary Initial commit\n\
            filename foo.txt\n\
            \tHello\n";
        let entries = parse_line_porcelain(raw);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sha, "1234567890123456789012345678901234567890");
        assert_eq!(entries[0].author.as_deref(), Some("Alice"));
        assert_eq!(entries[0].author_time, Some(1700000000));
        assert_eq!(entries[0].summary.as_deref(), Some("Initial commit"));
        assert_eq!(entries[0].filename.as_deref(), Some("foo.txt"));
        assert_eq!(entries[0].start_line, 1);
        assert_eq!(entries[0].end_line, 1);
    }

    #[test]
    fn coalesces_consecutive_lines_from_same_commit() {
        let mut raw = String::new();
        raw.push_str(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 2\n\
            author Bob\n\
            author-mail <bob@example.com>\n\
            author-time 1700000000\n\
            summary Add foo\n\
            filename foo.txt\n\
            \tline 1\n",
        );
        // Second line, same SHA, no header metadata repeated.
        raw.push_str(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 2 2\n\
            filename foo.txt\n\
            \tline 2\n",
        );
        let entries = parse_line_porcelain(&raw);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].start_line, 1);
        assert_eq!(entries[0].end_line, 2);
        assert_eq!(entries[0].author.as_deref(), Some("Bob"));
    }

    #[test]
    fn empty_output_returns_empty_vec() {
        assert!(parse_line_porcelain("").is_empty());
    }
}
