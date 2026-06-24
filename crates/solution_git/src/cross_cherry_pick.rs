//! S-SOL-CHP — Cross-member cherry-pick.
//!
//! Take a commit from one Solution member and apply it as a new commit
//! in another via `git format-patch -1 <sha> --stdout` →
//! optional path-mapping transform → `git apply --3way`. After clean
//! apply (or after the user resolves any conflicts via the
//! [`git_conflict_ui::ConflictResolverView`] and clicks Continue), commit
//! with `git interpret-trailers` injecting an
//! `X-Spke-Cherry-Picked-From: <source-member>:<sha>` trailer. The trailer
//! prefix matches the convention used by S-SOL-CMT (`X-Spke-Solution`).
//!
//! Path mapping is line-based: it rewrites only the unified-diff header
//! lines emitted by `git format-patch` —
//! `diff --git a/<old> b/<old>`, `--- a/<old>`, `+++ b/<old>`,
//! and the `rename from` / `rename to` lines. It assumes valid
//! `git format-patch` output; arbitrary text patches are out of scope.
//!
//! The flow is gated by a [`branch_protection::check`] call before any
//! git mutation runs. Today the stub always returns `Allowed`; once
//! S-SOL-PRT lands the same call site will start refusing protected
//! targets without a code change here.

use anyhow::{Context as _, Result, anyhow};
use git::repository::RepoPath;
use gpui::{
    AnyElement, App, AppContext as _, AsyncApp, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, IntoElement, Render, SharedString, WeakEntity, Window,
};
use menu::{Cancel, Confirm};
use solutions::{Solution, SolutionStore};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use ui::prelude::*;
use ui::{Checkbox, Icon, IconName, IconSize, Label, LabelCommon, LabelSize, ToggleState, Tooltip};
use util::ResultExt as _;
use util::command::new_command;
use workspace::{ModalView, Workspace};

use crate::branch_protection;

const OP_NAME: &str = "solution_cherry_pick_to_member";
const TRAILER_PREFIX: &str = "X-Spke-Cherry-Picked-From";

/// Status of [`cross_cherry_pick`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Patch applied cleanly (and committed, unless `no_commit`).
    Completed,
    /// `git apply --3way` left unmerged paths in the target. Caller is
    /// expected to surface the conflict resolver and finish via the
    /// existing "mark resolved → continue" flow (which commits the
    /// fixed working tree).
    PausedForConflict,
    /// Branch protection refused the op, or apply failed before any
    /// conflict arose (e.g. patch syntax error).
    Failed,
}

#[derive(Debug, Clone)]
pub struct CrossCherryPickRequest {
    pub source_member: SharedString,
    pub source_sha: String,
    pub target_member: SharedString,
    /// `(old_path, new_path)` pairs. `old_path` is the path as it appears
    /// in the source patch (relative, no `a/` / `b/` prefix); `new_path`
    /// is what it should become in the target. Pairs whose old==new are
    /// no-ops and may be omitted.
    pub path_mapping: Vec<(String, String)>,
    /// Skip the final `git commit` after a clean apply. The caller can
    /// then inspect / amend before committing.
    pub no_commit: bool,
}

#[derive(Debug, Clone)]
pub struct CrossCherryPickOutcome {
    pub status: Status,
    /// Populated when `status == PausedForConflict`. Repository-relative
    /// paths reported by `git status --porcelain` as unmerged in the
    /// target.
    pub conflicted_files: Option<Vec<RepoPath>>,
    /// New commit SHA in the target. `None` when `no_commit`, when the
    /// flow paused for conflict, or on failure.
    pub commit_sha: Option<String>,
    pub error: Option<String>,
}

impl CrossCherryPickOutcome {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            status: Status::Failed,
            conflicted_files: None,
            commit_sha: None,
            error: Some(message.into()),
        }
    }
}

/// Resolve a member's working directory against `solution`. Returns an
/// error if the member id isn't a Solution member, or if its path isn't
/// a git repo (the cherry-pick would fail with "fatal: not a git
/// repository" downstream — better to surface the reason up front).
fn member_work_dir(solution: &Solution, member_id: &str) -> Result<PathBuf> {
    let member = solution
        .members
        .iter()
        .find(|m| m.catalog_id.0 == member_id)
        .ok_or_else(|| {
            anyhow!(
                "`{member_id}` is not a member of solution `{}`",
                solution.name
            )
        })?;
    if !member.local_path.join(".git").exists() {
        return Err(anyhow!(
            "`{member_id}` is not a git repository (path: {})",
            member.local_path.display()
        ));
    }
    Ok(member.local_path.clone())
}

/// Run `cross_cherry_pick` end-to-end. The `&mut AsyncApp` parameter is
/// kept in the signature for future GPUI-context-aware variants (e.g.
/// dispatching the conflict resolver inline). Today the body is pure
/// data + subprocess work; tests use [`cross_cherry_pick_blocking`] to
/// avoid the GPUI context plumbing.
pub async fn cross_cherry_pick(
    solution: &Solution,
    request: CrossCherryPickRequest,
    _cx: &mut AsyncApp,
) -> Result<CrossCherryPickOutcome> {
    cross_cherry_pick_inner(solution, request).await
}

/// Internal entry point — pure data, no GPUI context. Reused by tests
/// and by [`cross_cherry_pick`].
async fn cross_cherry_pick_inner(
    solution: &Solution,
    request: CrossCherryPickRequest,
) -> Result<CrossCherryPickOutcome> {
    if request.source_member == request.target_member {
        return Ok(CrossCherryPickOutcome::failed(
            "source and target members must differ — use the in-repo cherry-pick instead",
        ));
    }
    if request.source_sha.trim().is_empty() {
        return Ok(CrossCherryPickOutcome::failed("source_sha is empty"));
    }
    let source_path = member_work_dir(solution, &request.source_member)?;
    let target_path = member_work_dir(solution, &request.target_member)?;

    // Branch-protection check on the target. We treat both Forbidden
    // and unconfirmed RequiresConfirmation as a refusal here — the
    // cross-member orchestrator is invoked from a UI gesture that
    // already presented its own confirmation step (the modal), so the
    // background-task layer is conservatively fail-closed. Surface
    // sites that want a confirm-then-retry flow check the decision
    // ahead of calling this entry point.
    let target_branch = current_branch(&target_path)
        .await
        .with_context(|| format!("resolving HEAD in {}", target_path.display()))?;
    match branch_protection::check(&target_path, &target_branch, OP_NAME) {
        branch_protection::Decision::Allowed => {}
        branch_protection::Decision::Forbidden { reason } => {
            return Ok(CrossCherryPickOutcome::failed(format!(
                "branch protection forbids cherry-pick into `{target_branch}`: {reason}"
            )));
        }
        branch_protection::Decision::RequiresConfirmation { reason } => {
            return Ok(CrossCherryPickOutcome::failed(format!(
                "branch protection requires confirmation for cherry-pick into `{target_branch}`: {reason}"
            )));
        }
    }

    // 1. format-patch in the source.
    let patch = format_patch_one(&source_path, &request.source_sha)
        .await
        .with_context(|| {
            format!(
                "running `git format-patch -1 {} --stdout` in {}",
                request.source_sha,
                source_path.display(),
            )
        })?;

    // 2. apply path-mapping transform.
    let transformed = transform_patch_paths(&patch, &request.path_mapping);

    // 3. Persist the patch under target's `.git/spke-patches/` so `git
    // apply` has a real path. Mirrors `git::operations::patch::create_patch`
    // when out_dir is None.
    let patch_path = write_patch_file(&target_path, &request.source_sha, &transformed)?;

    // 4. apply --3way in the target.
    let apply_outcome = run_git_apply_3way(&target_path, &patch_path).await?;

    match apply_outcome {
        ApplyResult::Clean => {
            if request.no_commit {
                return Ok(CrossCherryPickOutcome {
                    status: Status::Completed,
                    conflicted_files: None,
                    commit_sha: None,
                    error: None,
                });
            }
            // 5. commit with X-Spke-Cherry-Picked-From trailer.
            // `git apply --3way` updates the index (so the change is
            // staged) only when it had to do a 3-way merge. For a
            // straight-line apply nothing is staged — the working tree
            // is dirty but the index matches HEAD. Stage everything the
            // patch touched before committing.
            stage_all(&target_path).await?;
            let original_message = read_commit_message(&source_path, &request.source_sha)
                .await
                .with_context(|| {
                    format!(
                        "reading commit message for {} in {}",
                        request.source_sha,
                        source_path.display()
                    )
                })?;
            let trailer_value = format!(
                "{}:{}",
                request.source_member.as_ref(),
                request.source_sha.trim()
            );
            let final_message = match interpret_trailers(&original_message, &trailer_value).await {
                Ok(s) => s,
                Err(err) => {
                    log::warn!(
                        "cross_cherry_pick: interpret-trailers failed ({err}); falling back to raw append"
                    );
                    let mut s = original_message.clone();
                    if !s.ends_with('\n') {
                        s.push('\n');
                    }
                    s.push_str(&format!("\n{TRAILER_PREFIX}: {trailer_value}\n"));
                    s
                }
            };
            let new_sha = git_commit_capture_sha(&target_path, &final_message).await?;
            Ok(CrossCherryPickOutcome {
                status: Status::Completed,
                conflicted_files: None,
                commit_sha: Some(new_sha),
                error: None,
            })
        }
        ApplyResult::Conflict { conflicted_files } => {
            let repo_paths = conflicted_files
                .into_iter()
                .filter_map(|p| {
                    let s = p.to_string_lossy().to_string();
                    RepoPath::new(&s).log_err()
                })
                .collect();
            Ok(CrossCherryPickOutcome {
                status: Status::PausedForConflict,
                conflicted_files: Some(repo_paths),
                commit_sha: None,
                error: None,
            })
        }
        ApplyResult::Failed { stderr } => Ok(CrossCherryPickOutcome::failed(format!(
            "`git apply --3way` failed: {stderr}"
        ))),
    }
}

// ---------------------------------------------------------------------
// Path mapping transformer
// ---------------------------------------------------------------------

/// Rewrite header lines in `patch_bytes` according to `mapping`. Mapping
/// pairs whose old==new are skipped. Non-header lines are emitted
/// verbatim. Assumes the input is valid `git format-patch` output.
///
/// Header lines handled:
///
/// - `diff --git a/<old> b/<old>` (also handles `a/<old> b/<old>` where
///   the two sides differ — i.e. an in-repo rename — by independently
///   matching the `a/` and `b/` halves).
/// - `--- a/<old>`
/// - `+++ b/<old>`
/// - `rename from <old>` / `rename to <old>` (if git emitted them).
/// - `copy from <old>` / `copy to <old>`.
///
/// Non-text bytes are preserved as-is by treating the input as bytes,
/// splitting on `\n`, applying replacements to each line as UTF-8 (only
/// the header forms are ever transformed; the line is emitted verbatim
/// when it isn't valid UTF-8).
pub fn transform_patch_paths(patch_bytes: &[u8], mapping: &[(String, String)]) -> Vec<u8> {
    let active: BTreeMap<&str, &str> = mapping
        .iter()
        .filter(|(old, new)| old != new)
        .map(|(old, new)| (old.as_str(), new.as_str()))
        .collect();
    if active.is_empty() {
        return patch_bytes.to_vec();
    }

    let mut out = Vec::with_capacity(patch_bytes.len());
    for (idx, line) in patch_bytes.split(|b| *b == b'\n').enumerate() {
        if idx > 0 {
            out.push(b'\n');
        }
        let Ok(text) = std::str::from_utf8(line) else {
            out.extend_from_slice(line);
            continue;
        };
        match transform_header_line(text, &active) {
            Some(rewritten) => out.extend_from_slice(rewritten.as_bytes()),
            None => out.extend_from_slice(line),
        }
    }
    out
}

fn transform_header_line(line: &str, mapping: &BTreeMap<&str, &str>) -> Option<String> {
    let trimmed = line.trim_end_matches('\r');
    let cr_suffix = if trimmed.len() != line.len() {
        "\r"
    } else {
        ""
    };

    if let Some(rest) = trimmed.strip_prefix("diff --git ") {
        let rewritten = rewrite_diff_git_paths(rest, mapping)?;
        return Some(format!("diff --git {rewritten}{cr_suffix}"));
    }
    if let Some(rest) = trimmed.strip_prefix("--- a/") {
        let new = lookup(rest, mapping)?;
        return Some(format!("--- a/{new}{cr_suffix}"));
    }
    if let Some(rest) = trimmed.strip_prefix("+++ b/") {
        let new = lookup(rest, mapping)?;
        return Some(format!("+++ b/{new}{cr_suffix}"));
    }
    for prefix in ["rename from ", "rename to ", "copy from ", "copy to "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let new = lookup(rest, mapping)?;
            return Some(format!("{prefix}{new}{cr_suffix}"));
        }
    }
    None
}

fn lookup(path: &str, mapping: &BTreeMap<&str, &str>) -> Option<String> {
    mapping.get(path).map(|s| (*s).to_string())
}

/// Rewrite the `a/<old> b/<old>` half of a `diff --git` line.
///
/// Format is `a/<path-a> b/<path-b>` where `<path-a>` and `<path-b>` may
/// contain spaces if quoted (we do not handle the quoted form — git
/// only quotes when paths contain shell-meta chars; in practice rare for
/// solution-internal paths, and the rewrite is preserved as-is).
fn rewrite_diff_git_paths(rest: &str, mapping: &BTreeMap<&str, &str>) -> Option<String> {
    // Find the unique split point between `a/<path>` and ` b/<path>` —
    // because paths may contain spaces, we need to find ` b/` such that
    // the substring before it is exactly `a/<path>`. We scan from the
    // right because `a/...` may itself contain ` b/` (rare but possible).
    let mut split: Option<usize> = None;
    let bytes = rest.as_bytes();
    let needle = b" b/";
    let mut i = bytes.len();
    while i >= needle.len() {
        let start = i - needle.len();
        if &bytes[start..i] == needle && rest.get(..start).is_some_and(|s| s.starts_with("a/")) {
            split = Some(start);
            break;
        }
        i -= 1;
    }
    let split = split?;
    let path_a = rest.get(2..split)?;
    let path_b = rest.get(split + 3..)?;
    let new_a = mapping.get(path_a).copied().unwrap_or(path_a);
    let new_b = mapping.get(path_b).copied().unwrap_or(path_b);
    if new_a == path_a && new_b == path_b {
        return None;
    }
    Some(format!("a/{new_a} b/{new_b}"))
}

/// Parse the post-image paths that `transform_patch_paths` would emit /
/// pass through unchanged. Surfaces them in the modal preview pane and
/// in the auto-suggest table.
pub fn parse_source_paths(patch_bytes: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Ok(text) = std::str::from_utf8(patch_bytes) else {
        return out;
    };
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(idx) = rest.rfind(" b/") {
                let post = &rest[idx + 3..];
                if !out.contains(&post.to_string()) {
                    out.push(post.to_string());
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------
// Subprocess helpers
// ---------------------------------------------------------------------

#[derive(Debug)]
enum ApplyResult {
    Clean,
    Conflict { conflicted_files: Vec<PathBuf> },
    Failed { stderr: String },
}

async fn current_branch(work_dir: &std::path::Path) -> Result<String> {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["rev-parse", "--abbrev-ref", "HEAD"]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await?;
    if !output.status.success() {
        return Err(anyhow!(
            "`git rev-parse --abbrev-ref HEAD` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn format_patch_one(work_dir: &std::path::Path, sha: &str) -> Result<Vec<u8>> {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["format-patch", "-1", sha, "--stdout"]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await?;
    if !output.status.success() {
        return Err(anyhow!(
            "format-patch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn write_patch_file(target: &std::path::Path, sha: &str, bytes: &[u8]) -> Result<PathBuf> {
    let scratch = target.join(".git").join("spke-patches");
    std::fs::create_dir_all(&scratch)
        .map_err(|err| anyhow!("creating {}: {err}", scratch.display()))?;
    let short: String = sha.chars().take(12).collect();
    let path = scratch.join(format!("cross-{short}.patch"));
    std::fs::write(&path, bytes).map_err(|err| anyhow!("writing {}: {err}", path.display()))?;
    Ok(path)
}

async fn run_git_apply_3way(
    work_dir: &std::path::Path,
    patch_path: &std::path::Path,
) -> Result<ApplyResult> {
    let path_arg = patch_path.to_string_lossy().to_string();
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["apply", "--3way", &path_arg]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // `git apply --3way` exits non-zero on conflict but still leaves
    // unmerged stages in the index. Probe `git status --porcelain` for
    // unmerged paths regardless of exit code — it's the authoritative
    // signal.
    let conflicts = list_conflicted_paths(work_dir).await.unwrap_or_default();
    if !conflicts.is_empty() {
        return Ok(ApplyResult::Conflict {
            conflicted_files: conflicts,
        });
    }
    if output.status.success() {
        Ok(ApplyResult::Clean)
    } else {
        Ok(ApplyResult::Failed {
            stderr: stderr.trim().to_string(),
        })
    }
}

async fn list_conflicted_paths(work_dir: &std::path::Path) -> Result<Vec<PathBuf>> {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["status", "--porcelain"]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await?;
    if !output.status.success() {
        return Err(anyhow!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let body = String::from_utf8_lossy(&output.stdout);
    let mut paths = Vec::new();
    for line in body.lines() {
        if line.len() < 4 {
            continue;
        }
        let xy = &line[..2];
        let bytes = xy.as_bytes();
        let conflict = matches!(xy, "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU")
            || bytes[0] == b'U'
            || bytes[1] == b'U';
        if conflict {
            paths.push(PathBuf::from(line[3..].trim()));
        }
    }
    Ok(paths)
}

async fn read_commit_message(work_dir: &std::path::Path, sha: &str) -> Result<String> {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["log", "-1", "--format=%B", sha]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await?;
    if !output.status.success() {
        return Err(anyhow!(
            "git log failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

async fn interpret_trailers(message: &str, trailer_value: &str) -> Result<String> {
    use smol::io::AsyncWriteExt as _;
    let trailer_arg = format!("{TRAILER_PREFIX}: {trailer_value}");
    let mut command = new_command("git");
    command.args(["interpret-trailers", "--trailer", &trailer_arg]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(message.as_bytes()).await?;
        if !message.ends_with('\n') {
            stdin.write_all(b"\n").await?;
        }
        drop(stdin);
    }
    let output = child.output().await?;
    if !output.status.success() {
        return Err(anyhow!(
            "interpret-trailers failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn stage_all(work_dir: &std::path::Path) -> Result<()> {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["add", "-A"]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await?;
    if !output.status.success() {
        return Err(anyhow!(
            "git add -A failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

async fn git_commit_capture_sha(work_dir: &std::path::Path, message: &str) -> Result<String> {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["commit", "--cleanup=verbatim", "-m", message]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await?;
    if !output.status.success() {
        return Err(anyhow!(
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut rev = new_command("git");
    rev.current_dir(work_dir);
    rev.args(["rev-parse", "HEAD"]);
    rev.stdout(Stdio::piped());
    rev.stderr(Stdio::piped());
    let rev_out = rev.output().await?;
    if !rev_out.status.success() {
        return Err(anyhow!(
            "rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&rev_out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&rev_out.stdout).trim().to_string())
}

// ---------------------------------------------------------------------
// Action wiring + workspace registration
// ---------------------------------------------------------------------

/// Open the cross-member cherry-pick modal pre-filled with `source_member`
/// + `source_sha`. Dispatched from `git_ui::commit_context_menu` via a
/// dynamically-built action so `git_ui` doesn't need a build-time dep on
/// `solution_git`.
#[derive(Clone, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = solution_git)]
pub struct CrossCherryPick {
    pub source_member: String,
    pub source_sha: String,
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, action: &CrossCherryPick, window, cx| {
        open_modal_for(
            workspace,
            action.source_member.clone(),
            action.source_sha.clone(),
            window,
            cx,
        );
    });
}

fn open_modal_for(
    workspace: &mut Workspace,
    source_member: String,
    source_sha: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(store) = SolutionStore::try_global(cx) else {
        log::warn!("solution_git::cross_cherry_pick: SolutionStore global missing");
        return;
    };
    let solution = {
        let store_ref = store.read(cx);
        let mut best: Option<&Solution> = None;
        for sol in store_ref.solutions() {
            best = Some(match best {
                None => sol,
                Some(prev) => match (prev.last_opened_at, sol.last_opened_at) {
                    (Some(a), Some(b)) if b > a => sol,
                    (None, Some(_)) => sol,
                    _ => prev,
                },
            });
        }
        best.cloned()
    };
    let Some(solution) = solution else {
        log::warn!("solution_git::cross_cherry_pick: no active solution");
        return;
    };
    if solution.members.len() < 2 {
        log::info!(
            "solution_git::cross_cherry_pick: solution `{}` has < 2 members; skipping",
            solution.name
        );
        return;
    }
    let workspace_handle = workspace.weak_handle();
    workspace.toggle_modal(window, cx, |window, cx| {
        CrossCherryPickModal::new(
            workspace_handle,
            solution,
            source_member.into(),
            source_sha,
            window,
            cx,
        )
    });
}

// ---------------------------------------------------------------------
// Modal UI
// ---------------------------------------------------------------------

/// Modal driving [`cross_cherry_pick`]. Shows source / target / path
/// mapping table / `--no-commit` toggle, and dispatches the cherry-pick
/// on Confirm.
pub struct CrossCherryPickModal {
    workspace: WeakEntity<Workspace>,
    solution: Solution,
    source_member: SharedString,
    source_sha: String,
    /// Other Solution members eligible as targets, in solution-order.
    target_options: Vec<SharedString>,
    target_member: SharedString,
    /// `(old_path, new_path)` rows. `new_path` is editable; `old_path` is
    /// fixed (it's what the source patch contains).
    path_rows: Vec<PathRow>,
    /// Set after the source patch text loads. Drives the preview pane and
    /// auto-populates the path-mapping rows.
    source_patch: Option<Vec<u8>>,
    no_commit: bool,
    in_flight: bool,
    error: Option<SharedString>,
    focus_handle: FocusHandle,
}

#[derive(Debug, Clone)]
struct PathRow {
    old: SharedString,
    new: SharedString,
    /// Whether the `new` value differs from `old` (informational; drives
    /// the row's visual cue).
    changed: bool,
}

impl CrossCherryPickModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        solution: Solution,
        source_member: SharedString,
        source_sha: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let target_options: Vec<SharedString> = solution
            .members
            .iter()
            .map(|m| SharedString::from(m.catalog_id.0.clone()))
            .filter(|id| id != &source_member)
            .collect();
        let target_member = target_options.first().cloned().unwrap_or_default();
        let focus_handle = cx.focus_handle();

        let mut this = Self {
            workspace,
            solution,
            source_member,
            source_sha,
            target_options,
            target_member,
            path_rows: Vec::new(),
            source_patch: None,
            no_commit: false,
            in_flight: false,
            error: None,
            focus_handle,
        };
        this.load_patch_preview(cx);
        this
    }

    fn load_patch_preview(&mut self, cx: &mut Context<Self>) {
        let Some(member) = self
            .solution
            .members
            .iter()
            .find(|m| m.catalog_id.0 == self.source_member.as_ref())
        else {
            return;
        };
        let work_dir = member.local_path.clone();
        let sha = self.source_sha.clone();
        let task = cx.background_spawn(async move { format_patch_one(&work_dir, &sha).await });
        cx.spawn(async move |this, cx| {
            let bytes = task.await;
            this.update(cx, |this, cx| {
                match bytes {
                    Ok(b) => {
                        let paths = parse_source_paths(&b);
                        this.path_rows = paths
                            .into_iter()
                            .map(|p| PathRow {
                                old: SharedString::from(p.clone()),
                                new: SharedString::from(p),
                                changed: false,
                            })
                            .collect();
                        this.source_patch = Some(b);
                    }
                    Err(err) => {
                        this.error = Some(SharedString::from(format!(
                            "Failed to load source patch: {err}"
                        )));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn cycle_target(&mut self, cx: &mut Context<Self>) {
        if self.target_options.is_empty() {
            return;
        }
        let idx = self
            .target_options
            .iter()
            .position(|t| t == &self.target_member)
            .unwrap_or(0);
        let next = (idx + 1) % self.target_options.len();
        self.target_member = self.target_options[next].clone();
        cx.notify();
    }

    fn toggle_no_commit(&mut self, cx: &mut Context<Self>) {
        self.no_commit = !self.no_commit;
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        if self.in_flight {
            return;
        }
        self.in_flight = true;
        self.error = None;
        cx.notify();

        let request = CrossCherryPickRequest {
            source_member: self.source_member.clone(),
            source_sha: self.source_sha.clone(),
            target_member: self.target_member.clone(),
            path_mapping: self
                .path_rows
                .iter()
                .filter(|row| row.old != row.new)
                .map(|row| (row.old.to_string(), row.new.to_string()))
                .collect(),
            no_commit: self.no_commit,
        };
        let solution = self.solution.clone();
        let workspace = self.workspace.clone();
        let task =
            cx.background_spawn(async move { cross_cherry_pick_inner(&solution, request).await });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| {
                this.in_flight = false;
                match outcome {
                    Ok(o) => match o.status {
                        Status::Completed => {
                            if let Some(workspace) = workspace.upgrade() {
                                let msg = match &o.commit_sha {
                                    Some(sha) => format!(
                                        "Cherry-pick succeeded; new commit {}",
                                        &sha.chars().take(7).collect::<String>()
                                    ),
                                    None => "Cherry-pick applied (--no-commit)".to_string(),
                                };
                                show_status_toast(&workspace, msg, false, cx);
                            }
                            cx.emit(DismissEvent);
                        }
                        Status::PausedForConflict => {
                            let count = o.conflicted_files.as_ref().map(|v| v.len()).unwrap_or(0);
                            if let Some(workspace) = workspace.upgrade() {
                                show_status_toast(
                                    &workspace,
                                    format!("Cherry-pick paused: {count} file(s) need resolution"),
                                    true,
                                    cx,
                                );
                            }
                            cx.emit(DismissEvent);
                        }
                        Status::Failed => {
                            this.error = Some(
                                o.error
                                    .map(SharedString::from)
                                    .unwrap_or_else(|| "Cherry-pick failed".into()),
                            );
                            cx.notify();
                        }
                    },
                    Err(err) => {
                        this.error = Some(SharedString::from(format!("{err}")));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }
}

fn show_status_toast(
    workspace: &Entity<Workspace>,
    message: impl Into<SharedString>,
    is_warning: bool,
    cx: &mut App,
) {
    use notifications::status_toast::StatusToast;
    let message = message.into();
    workspace.update(cx, |workspace, cx| {
        let toast = StatusToast::new(message, cx, move |this, _cx| {
            let icon = if is_warning {
                IconName::Warning
            } else {
                IconName::Check
            };
            let color = if is_warning {
                ui::Color::Warning
            } else {
                ui::Color::Success
            };
            this.icon(Icon::new(icon).size(IconSize::Small).color(color))
        });
        workspace.toggle_status_toast(toast, cx);
    });
}

impl EventEmitter<DismissEvent> for CrossCherryPickModal {}
impl ModalView for CrossCherryPickModal {}
impl Focusable for CrossCherryPickModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CrossCherryPickModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let short: String = self.source_sha.chars().take(7).collect();
        let theme_colors = cx.theme().colors();
        let target_label: SharedString = format!("Target: {}", self.target_member).into();
        let cycle_tooltip = if self.target_options.len() > 1 {
            "Click to switch target member"
        } else {
            "Only one other member available"
        };
        let no_commit_state = if self.no_commit {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };
        let confirm_disabled =
            self.in_flight || self.target_options.is_empty() || self.target_member.is_empty();

        let path_rows: Vec<AnyElement> = self
            .path_rows
            .iter()
            .map(|row| {
                let prefix = if row.changed { "→ " } else { "  " };
                ui::h_flex()
                    .gap_2()
                    .child(
                        Label::new(row.old.clone())
                            .size(LabelSize::Small)
                            .color(ui::Color::Muted),
                    )
                    .child(
                        Label::new(SharedString::from(format!("{prefix}{}", row.new)))
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            })
            .collect();

        let preview_summary: SharedString = match &self.source_patch {
            Some(_) => format!("{} file(s) in patch", self.path_rows.len()).into(),
            None => "Loading patch…".into(),
        };

        ui::v_flex()
            .key_context("CrossCherryPickModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(40.0))
            .child(
                ui::h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .child(
                        ui::Headline::new(format!("Cherry-pick to Other Member ({short})"))
                            .size(ui::HeadlineSize::XSmall),
                    ),
            )
            .child(
                ui::v_flex()
                    .px_3()
                    .pb_2()
                    .gap_2()
                    .child(
                        Label::new(SharedString::from(format!(
                            "Source: {} @ {short}",
                            self.source_member
                        )))
                        .size(LabelSize::Small)
                        .color(ui::Color::Muted),
                    )
                    .child(
                        ui::h_flex()
                            .id("ccp-target-row")
                            .gap_2()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.cycle_target(cx)))
                            .tooltip(Tooltip::text(cycle_tooltip))
                            .child(Label::new(target_label).size(LabelSize::Small)),
                    )
                    .child(
                        ui::v_flex()
                            .gap_1()
                            .child(
                                Label::new("Path mapping")
                                    .size(LabelSize::XSmall)
                                    .color(ui::Color::Muted),
                            )
                            .child(
                                ui::v_flex()
                                    .border_1()
                                    .rounded_sm()
                                    .border_color(theme_colors.border)
                                    .p_2()
                                    .gap_1()
                                    .children(path_rows),
                            ),
                    )
                    .child(
                        ui::v_flex()
                            .gap_1()
                            .child(
                                Label::new("Preview")
                                    .size(LabelSize::XSmall)
                                    .color(ui::Color::Muted),
                            )
                            .child(Label::new(preview_summary).size(LabelSize::Small)),
                    )
                    .child(
                        ui::h_flex()
                            .id("ccp-no-commit-row")
                            .gap_2()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_no_commit(cx)))
                            .child(Checkbox::new("ccp-no-commit", no_commit_state).on_click(
                                cx.listener(|this, state: &ToggleState, _, cx| {
                                    this.no_commit = matches!(state, ToggleState::Selected);
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Label::new("--no-commit (apply only, don't commit)")
                                    .size(LabelSize::Small),
                            ),
                    ),
            )
            .when_some(self.error.clone(), |this, err| {
                this.child(
                    ui::div().px_3().pb_2().child(
                        Label::new(err)
                            .size(LabelSize::Small)
                            .color(ui::Color::Error),
                    ),
                )
            })
            .child(
                ui::h_flex()
                    .px_3()
                    .pb_3()
                    .gap_2()
                    .justify_end()
                    .child(ui::Button::new("ccp-cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&Cancel, window, cx)),
                    ))
                    .child({
                        let label = if self.in_flight {
                            "Cherry-picking…"
                        } else {
                            "Cherry-pick"
                        };
                        let mut button = ui::Button::new("ccp-confirm", label).on_click(
                            cx.listener(|this, _, window, cx| this.confirm(&Confirm, window, cx)),
                        );
                        if confirm_disabled {
                            button = button.disabled(true);
                        }
                        button
                    }),
            )
    }
}

// ---------------------------------------------------------------------
// MCP tool — solution.git.cherry_pick_to_member (Write tier)
// ---------------------------------------------------------------------

pub mod mcp {
    use super::*;
    use anyhow::Result;
    use context_server::listener::{McpServerTool, ToolResponse};
    use context_server::types::ToolResponseContent;
    use editor_mcp::{ToolTier, register_typed_tool_with_tier};
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
    #[serde(default, deny_unknown_fields)]
    pub struct PathMappingPair {
        pub old: String,
        pub new: String,
    }

    /// Input parameters for the cherry pick to member tool.
    #[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
    #[serde(default, deny_unknown_fields)]
    pub struct CherryPickToMemberInput {
        pub source_member: String,
        pub source_sha: String,
        pub target_member: String,
        pub path_mapping: Option<Vec<PathMappingPair>>,
        pub no_commit: Option<bool>,
        pub solution_id: Option<String>,
    }

    /// Output of the cherry pick to member tool.
    #[derive(Debug, Clone, Serialize, JsonSchema)]
    pub struct CherryPickToMemberOutput {
        pub status: String,
        pub conflicted_files: Option<Vec<String>>,
        pub commit_sha: Option<String>,
        pub error: Option<String>,
    }

    fn status_label(status: &Status) -> &'static str {
        match status {
            Status::Completed => "completed",
            Status::PausedForConflict => "paused_for_conflict",
            Status::Failed => "failed",
        }
    }

    fn resolve_solution(solution_id: Option<&str>, cx: &App) -> Result<Solution> {
        let store = SolutionStore::try_global(cx)
            .ok_or_else(|| anyhow!("no SolutionStore global — `solution_git::init` must run"))?;
        let store_ref = store.read(cx);
        if let Some(id) = solution_id {
            return store_ref
                .solutions()
                .iter()
                .find(|s| s.id.0 == id)
                .cloned()
                .ok_or_else(|| anyhow!("no solution with id `{id}`"));
        }
        let mut best: Option<&Solution> = None;
        for sol in store_ref.solutions() {
            best = Some(match best {
                None => sol,
                Some(prev) => match (prev.last_opened_at, sol.last_opened_at) {
                    (Some(a), Some(b)) if b > a => sol,
                    (None, Some(_)) => sol,
                    _ => prev,
                },
            });
        }
        best.cloned().ok_or_else(|| anyhow!("no active solution"))
    }

    #[derive(Clone)]
    pub struct CherryPickToMemberTool;

    impl McpServerTool for CherryPickToMemberTool {
        type Input = CherryPickToMemberInput;
        type Output = CherryPickToMemberOutput;
        const NAME: &'static str = "solution.git.cherry_pick_to_member";

        async fn run(
            &self,
            input: Self::Input,
            cx: &mut AsyncApp,
        ) -> Result<ToolResponse<Self::Output>> {
            if input.source_member.trim().is_empty() {
                return Err(anyhow!("`source_member` is required"));
            }
            if input.target_member.trim().is_empty() {
                return Err(anyhow!("`target_member` is required"));
            }
            if input.source_sha.trim().is_empty() {
                return Err(anyhow!("`source_sha` is required"));
            }
            let solution_id = input.solution_id.clone();
            let solution = cx.update(|cx| resolve_solution(solution_id.as_deref(), cx))?;
            let request = CrossCherryPickRequest {
                source_member: SharedString::from(input.source_member),
                source_sha: input.source_sha,
                target_member: SharedString::from(input.target_member),
                path_mapping: input
                    .path_mapping
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| (p.old, p.new))
                    .collect(),
                no_commit: input.no_commit.unwrap_or(false),
            };
            let outcome = cross_cherry_pick(&solution, request, cx).await?;
            let summary = match (&outcome.status, &outcome.commit_sha) {
                (Status::Completed, Some(sha)) => format!("completed; new commit {sha}"),
                (Status::Completed, None) => "completed (no-commit)".to_string(),
                (Status::PausedForConflict, _) => format!(
                    "paused for conflict ({} file(s))",
                    outcome
                        .conflicted_files
                        .as_ref()
                        .map(|v| v.len())
                        .unwrap_or(0)
                ),
                (Status::Failed, _) => outcome
                    .error
                    .clone()
                    .unwrap_or_else(|| "failed".to_string()),
            };
            let wire = CherryPickToMemberOutput {
                status: status_label(&outcome.status).to_string(),
                conflicted_files: outcome.conflicted_files.as_ref().map(|v| {
                    v.iter()
                        .map(|p| p.as_std_path().to_string_lossy().to_string())
                        .collect()
                }),
                commit_sha: outcome.commit_sha,
                error: outcome.error,
            };
            Ok(ToolResponse {
                content: vec![ToolResponseContent::Text { text: summary }],
                structured_content: wire,
            })
        }
    }

    pub(crate) fn register(cx: &mut App) {
        register_typed_tool_with_tier(cx, ToolTier::Write, CherryPickToMemberTool);
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use solutions::{CatalogId, SolutionId, SolutionMember};
    use std::path::Path;
    use std::process::Command;

    // -----------------------------------------------------------------
    // Path mapping transformer tests
    // -----------------------------------------------------------------

    #[test]
    fn path_mapping_transformer() {
        let patch = b"From abc123 Mon Sep 17 00:00:00 2001\n\
                      Subject: change\n\
                      ---\n\
                      diff --git a/old/foo.rs b/old/foo.rs\n\
                      index 1234..5678 100644\n\
                      --- a/old/foo.rs\n\
                      +++ b/old/foo.rs\n\
                      @@ -1,1 +1,1 @@\n\
                      -before\n\
                      +after\n";
        let mapping = vec![("old/foo.rs".to_string(), "new/foo.rs".to_string())];
        let out = transform_patch_paths(patch, &mapping);
        let text = std::str::from_utf8(&out).expect("utf-8");
        assert!(text.contains("diff --git a/new/foo.rs b/new/foo.rs"));
        assert!(text.contains("--- a/new/foo.rs"));
        assert!(text.contains("+++ b/new/foo.rs"));
        // Body lines untouched.
        assert!(text.contains("-before"));
        assert!(text.contains("+after"));
        // `index` line untouched.
        assert!(text.contains("index 1234..5678 100644"));
    }

    #[test]
    fn path_mapping_handles_diff_git_lines() {
        let patch = b"diff --git a/old b/old\n\
                      --- a/old\n\
                      +++ b/old\n";
        let mapping = vec![("old".to_string(), "new".to_string())];
        let out = transform_patch_paths(patch, &mapping);
        let text = std::str::from_utf8(&out).expect("utf-8");
        assert_eq!(
            text,
            "diff --git a/new b/new\n\
             --- a/new\n\
             +++ b/new\n"
        );
    }

    #[test]
    fn path_mapping_idempotent_when_empty() {
        let patch = b"diff --git a/foo b/foo\n--- a/foo\n+++ b/foo\n";
        let out = transform_patch_paths(patch, &[]);
        assert_eq!(out, patch);

        // A no-op mapping (old == new) is also a no-op.
        let mapping = vec![("foo".into(), "foo".into())];
        let out2 = transform_patch_paths(patch, &mapping);
        assert_eq!(out2, patch);
    }

    #[test]
    fn path_mapping_preserves_crlf() {
        let patch = b"diff --git a/old b/old\r\n--- a/old\r\n+++ b/old\r\n";
        let mapping = vec![("old".into(), "new".into())];
        let out = transform_patch_paths(patch, &mapping);
        assert_eq!(
            out,
            b"diff --git a/new b/new\r\n--- a/new\r\n+++ b/new\r\n".to_vec()
        );
    }

    #[test]
    fn path_mapping_handles_renames() {
        let patch = b"diff --git a/old b/new\n\
                      similarity index 95%\n\
                      rename from old\n\
                      rename to new\n";
        let mapping = vec![
            ("old".to_string(), "renamed_old".to_string()),
            ("new".to_string(), "renamed_new".to_string()),
        ];
        let out = transform_patch_paths(patch, &mapping);
        let text = std::str::from_utf8(&out).expect("utf-8");
        assert!(text.contains("diff --git a/renamed_old b/renamed_new"));
        assert!(text.contains("rename from renamed_old"));
        assert!(text.contains("rename to renamed_new"));
    }

    #[test]
    fn parse_source_paths_extracts_b_side() {
        let patch = b"diff --git a/foo b/foo\n\
                      --- a/foo\n\
                      +++ b/foo\n\
                      diff --git a/bar.rs b/bar.rs\n\
                      --- a/bar.rs\n\
                      +++ b/bar.rs\n";
        let paths = parse_source_paths(patch);
        assert_eq!(paths, vec!["foo".to_string(), "bar.rs".to_string()]);
    }

    // -----------------------------------------------------------------
    // End-to-end tests against real git repos
    // -----------------------------------------------------------------

    #[allow(clippy::disallowed_methods)]
    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .expect("spawn git");
        assert!(status.success(), "`git {}` failed", args.join(" "));
    }

    #[allow(clippy::disallowed_methods)]
    fn capture(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        run(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("README"), "init\n").expect("write");
        run(dir.path(), &["add", "README"]);
        run(
            dir.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@x",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        dir
    }

    /// Build a Solution with two members rooted at the given temp dirs.
    fn solution(a: &Path, b: &Path) -> Solution {
        Solution {
            id: SolutionId("test".into()),
            name: "Test".into(),
            root: PathBuf::from("/tmp/cross-cp"),
            members: vec![
                SolutionMember {
                    catalog_id: CatalogId("a".into()),
                    local_path: a.to_path_buf(),
                },
                SolutionMember {
                    catalog_id: CatalogId("b".into()),
                    local_path: b.to_path_buf(),
                },
            ],
            last_opened_at: None,
        }
    }

    #[test]
    fn cross_cherry_pick_clean_apply() {
        let a = init_repo();
        let b = init_repo();
        // Source: add `feature.txt`.
        std::fs::write(a.path().join("feature.txt"), "hello\n").expect("write");
        run(a.path(), &["add", "feature.txt"]);
        run(
            a.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@x",
                "commit",
                "-q",
                "-m",
                "Add feature\n\nDetails about the feature.",
            ],
        );
        let sha = capture(a.path(), &["rev-parse", "HEAD"]).trim().to_string();
        let sol = solution(a.path(), b.path());
        let request = CrossCherryPickRequest {
            source_member: "a".into(),
            source_sha: sha,
            target_member: "b".into(),
            path_mapping: Vec::new(),
            no_commit: false,
        };
        let outcome = smol::block_on(cross_cherry_pick_inner(&sol, request)).expect("run");
        assert_eq!(outcome.status, Status::Completed);
        assert!(outcome.commit_sha.is_some(), "must report new commit");
        // Target now has feature.txt and a commit referencing the source.
        assert!(b.path().join("feature.txt").exists());
        let log = capture(b.path(), &["log", "--format=%B", "-1"]);
        assert!(log.contains("Add feature"));
        assert!(
            log.contains("X-Spke-Cherry-Picked-From: a:"),
            "trailer not found in:\n{log}"
        );
    }

    #[test]
    fn cross_cherry_pick_with_no_commit() {
        let a = init_repo();
        let b = init_repo();
        std::fs::write(a.path().join("doc.md"), "doc\n").expect("write");
        run(a.path(), &["add", "doc.md"]);
        run(
            a.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@x",
                "commit",
                "-q",
                "-m",
                "Doc",
            ],
        );
        let sha = capture(a.path(), &["rev-parse", "HEAD"]).trim().to_string();
        let sol = solution(a.path(), b.path());
        let request = CrossCherryPickRequest {
            source_member: "a".into(),
            source_sha: sha,
            target_member: "b".into(),
            path_mapping: Vec::new(),
            no_commit: true,
        };
        let outcome = smol::block_on(cross_cherry_pick_inner(&sol, request)).expect("run");
        assert_eq!(outcome.status, Status::Completed);
        assert!(
            outcome.commit_sha.is_none(),
            "no_commit must skip commit, got {:?}",
            outcome.commit_sha
        );
        // Target shows `doc.md` as an unstaged change (worktree dirty).
        let status = capture(b.path(), &["status", "--porcelain"]);
        assert!(
            status.lines().any(|l| l.contains("doc.md")),
            "expected doc.md in status:\n{status}"
        );
        // Target HEAD did NOT advance (still at init).
        let log_count = capture(b.path(), &["rev-list", "--count", "HEAD"])
            .trim()
            .to_string();
        assert_eq!(log_count, "1");
    }

    #[test]
    fn cross_cherry_pick_conflict_path() {
        let a = init_repo();
        let b = init_repo();
        // Source: change README's contents.
        std::fs::write(a.path().join("README"), "alpha\nbeta\ngamma\n").expect("write");
        run(a.path(), &["add", "README"]);
        run(
            a.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@x",
                "commit",
                "-q",
                "-m",
                "Source change",
            ],
        );
        let sha = capture(a.path(), &["rev-parse", "HEAD"]).trim().to_string();
        // Target: divergent change to README so the 3-way apply
        // produces a conflict.
        std::fs::write(b.path().join("README"), "DIVERGENT\n").expect("write");
        run(b.path(), &["add", "README"]);
        run(
            b.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@x",
                "commit",
                "-q",
                "-m",
                "Target divergent",
            ],
        );
        let sol = solution(a.path(), b.path());
        let request = CrossCherryPickRequest {
            source_member: "a".into(),
            source_sha: sha,
            target_member: "b".into(),
            path_mapping: Vec::new(),
            no_commit: false,
        };
        let outcome = smol::block_on(cross_cherry_pick_inner(&sol, request)).expect("run");
        assert_eq!(outcome.status, Status::PausedForConflict);
        let conflicted = outcome.conflicted_files.as_ref().expect("conflicted_files");
        assert!(
            conflicted
                .iter()
                .any(|p| p.as_std_path().to_string_lossy().contains("README")),
            "expected README in conflict list, got {conflicted:?}"
        );
    }

    #[test]
    fn cross_cherry_pick_rejects_same_source_target() {
        let a = init_repo();
        let b = init_repo();
        let sol = solution(a.path(), b.path());
        let request = CrossCherryPickRequest {
            source_member: "a".into(),
            source_sha: "deadbeef".into(),
            target_member: "a".into(),
            path_mapping: Vec::new(),
            no_commit: false,
        };
        let outcome = smol::block_on(cross_cherry_pick_inner(&sol, request)).expect("run");
        assert_eq!(outcome.status, Status::Failed);
        assert!(outcome.error.is_some());
    }
}
