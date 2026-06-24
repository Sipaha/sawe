//! `editor.git.*` MCP tool registrations owned by `git_graph`.
//!
//! Tools register from [`crate::init`] so the central `editor_mcp` registry
//! sees them before `start_server` binds the socket. Per FORK.md decision #1
//! tools live in their owning crate (here: `git_graph` owns the
//! [`crate::filters::LogFilters`] type and the graph data path).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use editor_mcp::{ToolTier, register_typed_tool_with_tier};
use futures::AsyncBufReadExt as _;
use gpui::{App, AsyncApp};
use project::git_store::RepositoryId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use util::command::new_command;

use crate::filters::LogFilters;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 5000;

pub fn register(cx: &mut App) {
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, LogTool);
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, FileHistoryTool);
}

/// Input for `editor.git.log`. The `repo_id` selects which open repository
/// to query; obtain it from prior calls (e.g. from another tool that surfaces
/// repo IDs). When `repo_id` is `None`, the active repository of the focused
/// `MultiWorkspace` is used.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LogToolInput {
    /// Filter set produced by the chip-toolbar (chip-Branch / chip-User /
    /// chip-Date / chip-Path / chip-Query / chip-AllRefs / chip-Sha). An empty
    /// filter set matches the pre-S-FLT default — `--all` traversal of the
    /// reachable history.
    pub filters: LogFilters,
    /// Maximum number of commits to return. Defaults to 200; capped at 5000
    /// to avoid pathological asks. The MCP tool cannot stream — it returns
    /// a single bounded slice from the head of the log.
    pub limit: Option<usize>,
    /// Repository to query. Omit to use the focused window's active repo.
    /// `RepositoryId` is the wire-friendly `u64` pulled from `Repository::id`.
    pub repo_id: Option<u64>,
}

/// Output of the log tool tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LogToolOutput {
    pub commits: Vec<LogCommit>,
    /// `true` if the `git log` produced more rows than `limit`; the tool
    /// truncated the result.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LogCommit {
    pub sha: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub committer_date_unix: i64,
    pub subject: String,
    /// Decorations from `git log --decorate=full`: branch refs, tag refs,
    /// HEAD pointer. Empty when the commit has no decorations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ref_names: Vec<String>,
}

#[derive(Clone)]
pub struct LogTool;

impl McpServerTool for LogTool {
    type Input = LogToolInput;
    type Output = LogToolOutput;
    const NAME: &'static str = "editor.git.log";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;

        let mut args: Vec<String> = vec![
            "log".to_string(),
            // `%H\x00%P\x00%ct\x00%an\x00%ae\x00%D\x00%s` — committer-date in
            // unix seconds (%ct) so the consumer doesn't have to reparse.
            // %D is the decoration list (refs/heads/..., tag: ..., HEAD ->).
            "--format=%H%x00%P%x00%ct%x00%an%x00%ae%x00%D%x00%s".to_string(),
            "--decorate=full".to_string(),
            // Cap server-side at limit+1 so we can flag truncation.
            format!("--max-count={}", limit.saturating_add(1)),
        ];
        args.extend(input.filters.to_git_args());
        let extra_paths = input.filters.paths_args();
        if !extra_paths.is_empty() {
            args.push("--".to_string());
            args.extend(extra_paths);
        }

        let commits = run_git_log(&work_dir, &args).await?;
        let truncated = commits.len() > limit;
        let mut commits = commits;
        commits.truncate(limit);

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "{} commit(s){}",
                    commits.len(),
                    if truncated { " (truncated)" } else { "" }
                ),
            }],
            structured_content: LogToolOutput { commits, truncated },
        })
    }
}

/// Find the working directory for the requested `RepositoryId`. With `None`,
/// uses the focused `MultiWorkspace`'s active repository.
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

    // Default: focused window's active repo.
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

/// Spawn a `git log` subprocess in `work_dir`, parse the rich format
/// described in [`LogTool::run`], and collect into [`LogCommit`]s. Output
/// is captured in full because the MCP tool returns a single bounded slice.
async fn run_git_log(work_dir: &Path, args: &[String]) -> Result<Vec<LogCommit>> {
    let work_dir_buf: PathBuf = work_dir.to_path_buf();
    let mut command = new_command("git");
    command.current_dir(&work_dir_buf);
    command.args(args);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn().context("spawning `git log`")?;
    let stdout = child
        .stdout
        .take()
        .context("`git log` stdout pipe unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("`git log` stderr pipe unavailable")?;

    let mut reader = futures::io::BufReader::new(stdout);
    let mut commits = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("reading git log")?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches('\n');
        if trimmed.is_empty() {
            continue;
        }
        if let Some(commit) = parse_log_line(trimmed) {
            commits.push(commit);
        }
    }

    let status = child.status().await.context("waiting for `git log`")?;
    if !status.success() {
        let mut err_out = String::new();
        futures::io::AsyncReadExt::read_to_string(
            &mut futures::io::BufReader::new(stderr),
            &mut err_out,
        )
        .await
        .ok();
        if err_out.is_empty() {
            return Err(anyhow!("`git log` failed with status {status}"));
        }
        return Err(anyhow!("`git log` failed with status {status}: {err_out}"));
    }
    Ok(commits)
}

fn parse_log_line(line: &str) -> Option<LogCommit> {
    // Format: SHA\x00PARENTS\x00CT\x00AUTHOR_NAME\x00AUTHOR_EMAIL\x00REFS\x00SUBJECT
    let mut parts = line.splitn(7, '\x00');
    let sha = parts.next()?.to_string();
    let parents = parts
        .next()?
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    let committer_date_unix = parts.next()?.parse::<i64>().ok().unwrap_or(0);
    let author_name = parts.next()?.to_string();
    let author_email = parts.next()?.to_string();
    let refs_raw = parts.next()?;
    let ref_names = if refs_raw.is_empty() {
        Vec::new()
    } else {
        refs_raw.split(", ").map(|s| s.to_string()).collect()
    };
    let subject = parts.next().unwrap_or("").to_string();
    Some(LogCommit {
        sha,
        parents,
        author_name,
        author_email,
        committer_date_unix,
        subject,
        ref_names,
    })
}

/// Input for `editor.git.file_history`. The `path` is repository-relative
/// (the same shape `editor.git.log` accepts in `filters.paths`). When
/// `follow_renames` is `true` (default), `git log --follow` is used so the
/// history walks across rename commits.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FileHistoryToolInput {
    /// Repository-relative path of the file to inspect.
    pub path: String,
    /// Walk across rename commits via `git log --follow`. Defaults to
    /// `true` to mirror the in-app file-history view's default toggle
    /// state.
    pub follow_renames: Option<bool>,
    /// Maximum number of commits to return. Defaults to 200; capped at
    /// 5000.
    pub limit: Option<usize>,
    /// Repository to query. Omit to use the focused window's active repo.
    pub repo_id: Option<u64>,
}

#[derive(Clone)]
pub struct FileHistoryTool;

impl McpServerTool for FileHistoryTool {
    type Input = FileHistoryToolInput;
    type Output = LogToolOutput;
    const NAME: &'static str = "editor.git.file_history";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        if input.path.trim().is_empty() {
            return Err(anyhow!("`path` must be non-empty"));
        }

        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let follow_renames = input.follow_renames.unwrap_or(true);

        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;

        let mut args: Vec<String> = vec![
            "log".to_string(),
            "--format=%H%x00%P%x00%ct%x00%an%x00%ae%x00%D%x00%s".to_string(),
            "--decorate=full".to_string(),
            format!("--max-count={}", limit.saturating_add(1)),
        ];
        if follow_renames {
            args.push("--follow".to_string());
        }
        args.push("--".to_string());
        args.push(input.path.clone());

        let commits = run_git_log(&work_dir, &args).await?;
        let truncated = commits.len() > limit;
        let mut commits = commits;
        commits.truncate(limit);

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "{} commit(s){} for {}",
                    commits.len(),
                    if truncated { " (truncated)" } else { "" },
                    input.path,
                ),
            }],
            structured_content: LogToolOutput { commits, truncated },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_log_line() {
        let raw = concat!(
            "0123456789abcdef0123456789abcdef01234567",
            "\x00",
            "abcd1234abcd1234abcd1234abcd1234abcd1234 ef00ef00ef00ef00ef00ef00ef00ef00ef00ef00",
            "\x00",
            "1700000000",
            "\x00",
            "Alice",
            "\x00",
            "alice@example.com",
            "\x00",
            "HEAD -> refs/heads/main, refs/remotes/origin/main",
            "\x00",
            "Fix a thing"
        );
        let parsed = parse_log_line(raw).expect("parsed");
        assert_eq!(parsed.sha, "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(parsed.parents.len(), 2);
        assert_eq!(parsed.committer_date_unix, 1_700_000_000);
        assert_eq!(parsed.author_name, "Alice");
        assert_eq!(parsed.author_email, "alice@example.com");
        assert_eq!(
            parsed.ref_names,
            vec![
                "HEAD -> refs/heads/main".to_string(),
                "refs/remotes/origin/main".to_string(),
            ]
        );
        assert_eq!(parsed.subject, "Fix a thing");
    }

    #[test]
    fn parses_root_commit_no_parents_no_refs() {
        let raw = "deadbeef\x00\x00100\x00Bob\x00bob@example.com\x00\x00Initial commit";
        let parsed = parse_log_line(raw).expect("parsed");
        assert!(parsed.parents.is_empty());
        assert!(parsed.ref_names.is_empty());
        assert_eq!(parsed.subject, "Initial commit");
    }

    #[test]
    fn parses_subject_with_null_byte_resilient() {
        // splitn(7) means subject keeps any trailing \x00 bytes verbatim,
        // which is what we want — git formatters won't emit \x00 in subject
        // output by themselves but the parser should not panic if they appear.
        let raw = "abc\x00\x00200\x00\x00\x00\x00body\x00with-null";
        let parsed = parse_log_line(raw).expect("parsed");
        assert_eq!(parsed.subject, "body\x00with-null");
    }
}
