//! `editor.git.commit_show` MCP tool: returns rich commit metadata
//! (parents, ref decorations, branches/tags-containing, per-file numstat)
//! for a given SHA. Owned by `git_ui` because the data surface lives
//! here (see FORK.md decision #1: tool ownership follows the data).

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

pub(crate) fn register(cx: &mut App) {
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, CommitShowTool);
}

/// Input parameters for the commit show tool tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CommitShowToolInput {
    /// Commit SHA (full or short — `git` resolves the rest).
    pub sha: String,
    /// Repository to query. When `None`, uses the active repository of
    /// the focused window.
    pub repo_id: Option<u64>,
}

/// Output of the commit show tool tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommitShowToolOutput {
    pub sha: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub committer_date_unix: i64,
    pub subject: String,
    pub body: String,
    /// Decorations from `git log --decorate=full` for the commit.
    pub ref_names: Vec<String>,
    pub branches_containing: Vec<String>,
    pub tags_containing: Vec<String>,
    pub files: Vec<FileWithNumstat>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FileWithNumstat {
    pub path: String,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    /// Old path when git detected a rename or copy. None otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename_from: Option<String>,
}

#[derive(Clone)]
pub struct CommitShowTool;

impl McpServerTool for CommitShowTool {
    type Input = CommitShowToolInput;
    type Output = CommitShowToolOutput;
    const NAME: &'static str = "editor.git.commit_show";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let work_dir =
            cx.update(|cx| resolve_work_directory(input.repo_id.map(RepositoryId), cx))?;
        let header = run_show_header(&work_dir, &input.sha).await?;
        let files = run_show_numstat(&work_dir, &input.sha).await?;
        let branches = run_contains(&work_dir, &input.sha, "branch")
            .await
            .unwrap_or_default();
        let tags = run_contains(&work_dir, &input.sha, "tag")
            .await
            .unwrap_or_default();
        let summary = format!(
            "{} ({} files, {} branches / {} tags)",
            input.sha,
            files.len(),
            branches.len(),
            tags.len()
        );

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: CommitShowToolOutput {
                sha: header.sha,
                parents: header.parents,
                author_name: header.author_name,
                author_email: header.author_email,
                committer_date_unix: header.committer_date_unix,
                subject: header.subject,
                body: header.body,
                ref_names: header.ref_names,
                branches_containing: branches,
                tags_containing: tags,
                files,
            },
        })
    }
}

struct CommitHeader {
    sha: String,
    parents: Vec<String>,
    author_name: String,
    author_email: String,
    committer_date_unix: i64,
    subject: String,
    body: String,
    ref_names: Vec<String>,
}

async fn run_show_header(work_dir: &Path, sha: &str) -> Result<CommitHeader> {
    // %H<NUL>%P<NUL>%ct<NUL>%an<NUL>%ae<NUL>%D<NUL>%s<NUL>%b
    let format = "--format=%H%x00%P%x00%ct%x00%an%x00%ae%x00%D%x00%s%x00%b";
    let output = run_git(
        work_dir,
        &["show", "--no-patch", "--decorate=full", format, sha],
    )
    .await?;
    parse_show_header(&output).context("parsing `git show --format=` output")
}

fn parse_show_header(stdout: &str) -> Option<CommitHeader> {
    let trimmed = stdout.trim_end_matches('\n');
    let mut parts = trimmed.splitn(8, '\x00');
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
    let body = parts.next().unwrap_or("").to_string();
    Some(CommitHeader {
        sha,
        parents,
        author_name,
        author_email,
        committer_date_unix,
        subject,
        body,
        ref_names,
    })
}

async fn run_show_numstat(work_dir: &Path, sha: &str) -> Result<Vec<FileWithNumstat>> {
    // `--name-status` + `--numstat` in one pass via `--format=`
    // would interleave; run two passes and join by file path.
    let stat_out = run_git(work_dir, &["show", "--format=", "-z", "--numstat", sha]).await?;
    let status_out = run_git(work_dir, &["show", "--format=", "-z", "--name-status", sha]).await?;
    Ok(merge_numstat(&stat_out, &status_out))
}

fn merge_numstat(numstat_z: &str, namestatus_z: &str) -> Vec<FileWithNumstat> {
    let stats = parse_numstat_z(numstat_z);
    let statuses = parse_namestatus_z(namestatus_z);
    let mut out: Vec<FileWithNumstat> = Vec::with_capacity(stats.len().max(statuses.len()));
    for (path, additions, deletions, rename_from) in stats {
        let status = statuses
            .iter()
            .find(|(p, _, _)| p == &path)
            .map(|(_, status, _)| status.clone())
            .unwrap_or_else(|| "M".to_string());
        out.push(FileWithNumstat {
            path,
            status,
            additions,
            deletions,
            rename_from,
        });
    }
    out
}

/// Parse `git show --format= -z --numstat`: per record either
///   `additions\tdeletions\t<path>\0` or
///   `additions\tdeletions\t\0<old>\0<new>\0` (rename/copy; tab-zero is
/// the marker — additions/deletions are `-` for binary files).
fn parse_numstat_z(stdout: &str) -> Vec<(String, u32, u32, Option<String>)> {
    let mut out = Vec::new();
    let mut iter = stdout.split('\0').peekable();
    while let Some(record) = iter.next() {
        if record.is_empty() {
            continue;
        }
        // record looks like "5\t3\tpath" OR "5\t3\t" with the path in the
        // next two NUL fields.
        let mut tabs = record.splitn(3, '\t');
        let additions = tabs.next().unwrap_or("0");
        let deletions = tabs.next().unwrap_or("0");
        let path_part = tabs.next().unwrap_or("");
        let additions: u32 = additions.parse().unwrap_or(0);
        let deletions: u32 = deletions.parse().unwrap_or(0);
        if path_part.is_empty() {
            // Rename: next two NUL fields are <old>, <new>.
            let old = iter.next().unwrap_or("").to_string();
            let new = iter.next().unwrap_or("").to_string();
            out.push((new, additions, deletions, Some(old)));
        } else {
            out.push((path_part.to_string(), additions, deletions, None));
        }
    }
    out
}

/// Parse `git show --format= -z --name-status`. The record layout for `-z`:
/// - normal: `<status>\t<path>\0`
/// - rename/copy: `<status>\t\0<old>\0<new>\0` (the trailing `\t` is part of
///   the same field — git emits `R100\t` then NUL-terminates, then the
///   following two NUL-delimited entries are the old / new paths).
///
/// The simplest robust parse: walk records linearly, peek at the first one
/// to decide how many follow-up records belong to this entry.
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
            // Some git versions emit `R100\told\0new\0` — `path_part` is
            // already `old`, the next NUL field is `new`.
            let new = iter.next().unwrap_or("").to_string();
            out.push((new, status, Some(path_part)));
        } else {
            out.push((path_part, status, None));
        }
    }
    out
}

async fn run_contains(work_dir: &Path, sha: &str, kind: &str) -> Result<Vec<String>> {
    let args: Vec<&str> = match kind {
        "branch" => vec![
            "branch",
            "--list",
            "--contains",
            sha,
            "--format=%(refname:short)",
        ],
        "tag" => vec!["tag", "--contains", sha],
        _ => return Err(anyhow!("unknown contains kind: {kind}")),
    };
    let stdout = run_git(work_dir, &args).await?;
    Ok(stdout
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            let trimmed = trimmed.strip_prefix("* ").unwrap_or(trimmed);
            let trimmed = trimmed.strip_prefix("+ ").unwrap_or(trimmed);
            trimmed.to_string()
        })
        .filter(|s| !s.is_empty())
        .collect())
}

async fn run_git(work_dir: &Path, args: &[&str]) -> Result<String> {
    let work_dir_buf: PathBuf = work_dir.to_path_buf();
    let mut command = new_command("git");
    command.current_dir(&work_dir_buf);
    command.args(args);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn().context("spawning `git`")?;
    let stdout = child
        .stdout
        .take()
        .context("`git` stdout pipe unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("`git` stderr pipe unavailable")?;
    let mut stdout_reader = futures::io::BufReader::new(stdout);
    let mut stdout_buf = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = stdout_reader
            .read_line(&mut line)
            .await
            .context("reading git stdout")?;
        if n == 0 {
            break;
        }
        stdout_buf.push_str(&line);
    }
    let status = child.status().await.context("waiting for git")?;
    if !status.success() {
        let mut err_out = String::new();
        futures::io::AsyncReadExt::read_to_string(
            &mut futures::io::BufReader::new(stderr),
            &mut err_out,
        )
        .await
        .ok();
        return Err(anyhow!(
            "`git {}` failed: {}",
            args.join(" "),
            err_out.trim_end()
        ));
    }
    Ok(stdout_buf)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_show_header_with_subject_and_body() {
        let raw = "abc123\x00parent1 parent2\x001700000000\x00Alice\x00alice@example.com\x00HEAD -> refs/heads/main\x00Subject line\x00Multi\nline body\n";
        let parsed = parse_show_header(raw).expect("parsed");
        assert_eq!(parsed.sha, "abc123");
        assert_eq!(
            parsed.parents,
            vec!["parent1".to_string(), "parent2".to_string()]
        );
        assert_eq!(parsed.committer_date_unix, 1_700_000_000);
        assert_eq!(parsed.author_name, "Alice");
        assert_eq!(parsed.author_email, "alice@example.com");
        assert_eq!(
            parsed.ref_names,
            vec!["HEAD -> refs/heads/main".to_string()]
        );
        assert_eq!(parsed.subject, "Subject line");
        assert!(parsed.body.starts_with("Multi"));
    }

    #[test]
    fn parses_show_header_root_commit() {
        let raw = "deadbeef\x00\x00100\x00Bob\x00bob@example.com\x00\x00Initial\x00";
        let parsed = parse_show_header(raw).expect("parsed");
        assert!(parsed.parents.is_empty());
        assert!(parsed.ref_names.is_empty());
        assert_eq!(parsed.subject, "Initial");
    }

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
        // Two records: a regular Modified entry and a rename. The rename's
        // first record has an empty path (split_once on TAB — git emits
        // `R100<TAB>` then NUL, with the old / new paths as separate
        // NUL-terminated entries).
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
}
