//! AtomicGitOp wrappers for the resolver's destructive actions
//! (Continue / Abort / Skip) and a shared `run_git_void` helper used by
//! the resolver, sidebar, binary view, and MCP tools.
//!
//! Continue + Skip funnel through `OpRunner` so a successful merge/rebase
//! commit ends up in the undo registry; Abort is destructive and likewise
//! wraps OpRunner.

use anyhow::{Context as _, Result, anyhow};
use git::operations::{AtomicGitOp, OpRunner};
use gpui::{AppContext as _, Context, Task};
use std::path::{Path, PathBuf};
use util::ResultExt as _;
use util::command::{Stdio, new_command};

use crate::conflict_parser::{InProgressOp, detect_in_progress_op};
use crate::resolver_view::ConflictResolverView;

pub(crate) async fn run_git_void(work_dir: &Path, args: &[&str]) -> Result<()> {
    run_git(work_dir, args).await.map(|_| ())
}

pub(crate) async fn run_git(work_dir: &Path, args: &[&str]) -> Result<String> {
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

/// `git <op> --continue`. Op detection by `.git/<op>_HEAD` happens in
/// `detect_in_progress_op` — caller passes that subcommand in.
pub struct ContinueMergeOp {
    pub op: InProgressOp,
}

impl AtomicGitOp for ContinueMergeOp {
    type Output = ();

    fn op_name(&self) -> &'static str {
        match self.op {
            InProgressOp::Merge => "merge_continue",
            InProgressOp::Rebase => "rebase_continue",
            InProgressOp::CherryPick => "cherry_pick_continue",
            InProgressOp::Revert => "revert_continue",
        }
    }

    fn affected_branches(&self, _repo_path: &Path) -> Vec<String> {
        Vec::new()
    }

    fn run(&mut self, repo_path: &Path) -> Result<()> {
        run_git_blocking(repo_path, &[self.op.cli_subcommand(), "--continue"])
    }
}

/// `git <op> --abort`. Always destructive — drops in-progress state.
pub struct AbortMergeOp {
    pub op: InProgressOp,
}

impl AtomicGitOp for AbortMergeOp {
    type Output = ();

    fn op_name(&self) -> &'static str {
        match self.op {
            InProgressOp::Merge => "merge_abort",
            InProgressOp::Rebase => "rebase_abort",
            InProgressOp::CherryPick => "cherry_pick_abort",
            InProgressOp::Revert => "revert_abort",
        }
    }

    fn is_destructive(&self) -> bool {
        true
    }

    fn affected_branches(&self, _repo_path: &Path) -> Vec<String> {
        Vec::new()
    }

    fn run(&mut self, repo_path: &Path) -> Result<()> {
        run_git_blocking(repo_path, &[self.op.cli_subcommand(), "--abort"])
    }
}

/// `git <op> --skip`. Only valid for cherry-pick / rebase / revert.
pub struct SkipRebaseOp {
    pub op: InProgressOp,
}

impl AtomicGitOp for SkipRebaseOp {
    type Output = ();

    fn op_name(&self) -> &'static str {
        match self.op {
            InProgressOp::Rebase => "rebase_skip",
            InProgressOp::CherryPick => "cherry_pick_skip",
            InProgressOp::Revert => "revert_skip",
            InProgressOp::Merge => "merge_skip",
        }
    }

    fn is_destructive(&self) -> bool {
        true
    }

    fn affected_branches(&self, _repo_path: &Path) -> Vec<String> {
        Vec::new()
    }

    fn run(&mut self, repo_path: &Path) -> Result<()> {
        if !self.op.supports_skip() {
            return Err(anyhow!(
                "git {} does not support --skip",
                self.op.cli_subcommand()
            ));
        }
        run_git_blocking(repo_path, &[self.op.cli_subcommand(), "--skip"])
    }
}

#[allow(clippy::disallowed_methods)]
fn run_git_blocking(repo_path: &Path, args: &[&str]) -> Result<()> {
    use std::process::Command;
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo_path).args(args);
    let output = cmd.output().map_err(|err| anyhow!("spawn git: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Returns `true` if `git status --porcelain` reports any path that is
/// not in the resolver's known conflict set. Used as the guard for the
/// Continue button per the spec ("Working tree has unrelated changes").
#[allow(clippy::disallowed_methods)]
pub fn has_unrelated_changes(
    work_dir: &Path,
    known_conflict_paths: &[String],
) -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(work_dir)
        .args(["status", "--porcelain=1", "-z"])
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut unrelated = Vec::new();
    for record in stdout.split('\0') {
        if record.len() < 4 {
            continue;
        }
        let xy = &record[..2];
        let path = &record[3..];
        if xy == "UU" || xy == "AA" || xy == "DD" || xy.contains('U') {
            // a conflict — already in known set
            continue;
        }
        if known_conflict_paths.iter().any(|p| p == path) {
            continue;
        }
        unrelated.push(path.to_string());
    }
    Ok(unrelated)
}

pub(crate) fn continue_op(this: &mut ConflictResolverView, cx: &mut Context<ConflictResolverView>) {
    let Some(op) = this.op() else {
        return;
    };
    let work_dir = this.work_dir().to_path_buf();
    let known: Vec<String> = this
        .conflicts()
        .iter()
        .map(|f| f.path.as_std_path().to_string_lossy().into_owned())
        .collect();
    cx.spawn(async move |this, cx| {
        let unrelated = cx
            .background_spawn({
                let work_dir = work_dir.clone();
                async move { has_unrelated_changes(&work_dir, &known) }
            })
            .await
            .log_err()
            .unwrap_or_default();
        if !unrelated.is_empty() {
            log::warn!(
                "conflict resolver: {} unrelated change(s) in working tree, blocking continue: {:?}",
                unrelated.len(),
                unrelated
            );
            this.update(cx, |_, cx| cx.notify()).ok();
            return;
        }
        cx.background_spawn(async move {
            OpRunner::run(ContinueMergeOp { op }, &work_dir)
        })
        .await
        .log_err();
        this.update(cx, |this, cx| {
            this.refresh_conflict_list(cx);
        })
        .ok();
    })
    .detach();
}

pub(crate) fn abort_op(this: &mut ConflictResolverView, cx: &mut Context<ConflictResolverView>) {
    let Some(op) = this.op() else {
        return;
    };
    let work_dir = this.work_dir().to_path_buf();
    cx.spawn(async move |this, cx| {
        cx.background_spawn(async move { OpRunner::run(AbortMergeOp { op }, &work_dir) })
            .await
            .log_err();
        this.update(cx, |this, cx| {
            this.refresh_conflict_list(cx);
        })
        .ok();
    })
    .detach();
}

pub(crate) fn skip_op(this: &mut ConflictResolverView, cx: &mut Context<ConflictResolverView>) {
    let Some(op) = this.op() else {
        return;
    };
    if !op.supports_skip() {
        return;
    }
    let work_dir = this.work_dir().to_path_buf();
    cx.spawn(async move |this, cx| {
        cx.background_spawn(async move { OpRunner::run(SkipRebaseOp { op }, &work_dir) })
            .await
            .log_err();
        this.update(cx, |this, cx| {
            this.refresh_conflict_list(cx);
        })
        .ok();
    })
    .detach();
}

/// Helper: re-detect the in-progress op for `repo_path`. Used by MCP
/// tools that operate at the work-dir level without holding a resolver
/// view.
pub fn op_for_dir(repo_path: &Path) -> Option<InProgressOp> {
    let dot_git = repo_path.join(".git");
    let git_dir = if dot_git.is_file() {
        std::fs::read_to_string(&dot_git)
            .ok()
            .and_then(|s| {
                s.lines().find_map(|line| {
                    line.strip_prefix("gitdir:").map(|p| {
                        let p = p.trim();
                        let path = Path::new(p);
                        if path.is_absolute() {
                            path.to_path_buf()
                        } else {
                            repo_path.join(path)
                        }
                    })
                })
            })
            .unwrap_or(dot_git)
    } else {
        dot_git
    };
    detect_in_progress_op(&git_dir)
}

/// Best-effort no-op `Task` builder used in early returns; keeps callers
/// short.
#[allow(dead_code)]
pub(crate) fn ready_ok<T: 'static + Send>(value: T) -> Task<Result<T>> {
    Task::ready(Ok(value))
}
