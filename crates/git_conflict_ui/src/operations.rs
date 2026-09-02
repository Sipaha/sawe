//! AtomicGitOp wrappers for the resolver's destructive actions
//! (Continue / Abort / Skip) and a shared `run_git_void` helper used by
//! the resolver, sidebar, binary view, and MCP tools.
//!
//! Continue + Skip funnel through `OpRunner` so a successful merge/rebase
//! commit ends up in the undo registry; Abort is destructive and likewise
//! wraps OpRunner.

use anyhow::{Context as _, Result, anyhow};
use git::operations::{AtomicGitOp, OpRunner};
use gpui::{App, AppContext as _, Context, Task, WeakEntity};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use util::command::{Stdio, new_command};
use workspace::Workspace;
use workspace::notifications::NotificationId;
use workspace::notifications::simple_message_notification::MessageNotification;

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

fn run_git_blocking(repo_path: &Path, args: &[&str]) -> Result<()> {
    run_git_blocking_output(repo_path, args).map(|_| ())
}

fn run_git_blocking_output(repo_path: &Path, args: &[&str]) -> Result<String> {
    let output = git_command(repo_path, args)
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[allow(clippy::disallowed_methods)]
fn git_command(repo_path: &Path, args: &[&str]) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(repo_path).args(args);
    // `<op> --continue` opens $EDITOR for the commit message and there is no
    // terminal on the other end of our pipes, so git aborts with "Standard
    // input is not a terminal" and no commit is made. `--no-edit` is not an
    // option: `git merge --continue --no-edit` is rejected with
    // "fatal: --continue expects no arguments". GIT_SEQUENCE_EDITOR covers the
    // todo-list editor that `rebase` can open along the same paths.
    //
    // Consequence: the merge message cannot be edited from this button. It is
    // already correct in MERGE_MSG, and the git panel's commit box is where a
    // message gets edited.
    cmd.env("GIT_EDITOR", "true");
    cmd.env("GIT_SEQUENCE_EDITOR", "true");
    cmd
}

/// One `git status --porcelain=1 -z` record: the index column, the
/// worktree column, and the path they describe.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StatusRecord<'a> {
    pub index: char,
    pub worktree: char,
    pub path: &'a str,
}

impl StatusRecord<'_> {
    /// Unmerged (conflicted) index entries: `DD`, `AU`, `UD`, `UA`, `DU`,
    /// `AA`, `UU`.
    fn is_conflict(&self) -> bool {
        self.index == 'U'
            || self.worktree == 'U'
            || (self.index == 'A' && self.worktree == 'A')
            || (self.index == 'D' && self.worktree == 'D')
    }

    fn is_untracked_or_ignored(&self) -> bool {
        (self.index == '?' && self.worktree == '?') || (self.index == '!' && self.worktree == '!')
    }

    /// Whether the path has content in the index that differs from HEAD.
    /// This is the only kind of change `git <op> --continue` can sweep into
    /// the commit it creates.
    fn is_staged(&self) -> bool {
        !self.is_conflict() && !self.is_untracked_or_ignored() && self.index != ' '
    }
}

/// Parse `git status --porcelain=1 -z` output.
///
/// Records are `XY SP <path> NUL`. Rename/copy records (`R`/`C` in the index
/// column) are followed by a bare `<origPath> NUL` field with no `XY` prefix,
/// which has to be consumed rather than parsed as a record of its own.
pub(crate) fn parse_porcelain_z(stdout: &str) -> Vec<StatusRecord<'_>> {
    let mut records = Vec::new();
    let mut fields = stdout.split('\0');
    while let Some(field) = fields.next() {
        let mut chars = field.chars();
        let (Some(index), Some(worktree), Some(' ')) = (chars.next(), chars.next(), chars.next())
        else {
            continue;
        };
        let path = &field[field
            .char_indices()
            .nth(3)
            .map(|(offset, _)| offset)
            .unwrap_or(field.len())..];
        if path.is_empty() {
            continue;
        }
        if index == 'R' || index == 'C' {
            fields.next();
        }
        records.push(StatusRecord {
            index,
            worktree,
            path,
        });
    }
    records
}

/// Paths whose *staged* content is the user's own work rather than the
/// in-progress operation's.
///
/// The guard exists so `git <op> --continue` does not sweep unrelated work
/// into the commit it creates. `--continue` commits the **index**, so only
/// staged paths can be swept: unstaged edits and untracked files are
/// untouchable by it and must never block.
///
/// Staged-but-related is the majority case and it is why a plain
/// "any staged path" test does not work: a merge auto-stages every path the
/// incoming side changed cleanly, so `M  <path>` is the normal state of a
/// half-resolved merge. `incoming_paths` is that set, and it is subtracted.
pub(crate) fn classify_unrelated_staged(
    porcelain_z: &str,
    known_conflict_paths: &[String],
    incoming_paths: &HashSet<String>,
) -> Vec<String> {
    parse_porcelain_z(porcelain_z)
        .into_iter()
        .filter(|record| record.is_staged())
        .filter(|record| !incoming_paths.contains(record.path))
        .filter(|record| !known_conflict_paths.iter().any(|p| p == record.path))
        .map(|record| record.path.to_string())
        .collect()
}

/// Paths touched by the change the in-progress operation is applying.
///
/// For a merge that is `merge-base(HEAD, MERGE_HEAD)..MERGE_HEAD`; for the
/// commit-replaying operations it is the single commit being replayed. Both
/// are the set git itself auto-staged, including paths both sides edited
/// that merged cleanly — comparing the index against the incoming head
/// instead would misclassify exactly those as the user's work.
fn incoming_change_paths(work_dir: &Path, op: InProgressOp) -> Result<HashSet<String>> {
    let range: &[&str] = match op {
        InProgressOp::Merge => &["HEAD...MERGE_HEAD"],
        InProgressOp::Rebase => &["REBASE_HEAD^", "REBASE_HEAD"],
        InProgressOp::CherryPick => &["CHERRY_PICK_HEAD^", "CHERRY_PICK_HEAD"],
        InProgressOp::Revert => &["REVERT_HEAD^", "REVERT_HEAD"],
    };
    let mut args = vec!["diff", "--name-only", "-z"];
    args.extend_from_slice(range);
    let stdout = run_git_blocking_output(work_dir, &args)?;
    Ok(stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect())
}

/// Staged paths that `git <op> --continue` would sweep into its commit but
/// that belong to neither the conflict set nor the incoming change.
pub fn unrelated_staged_changes(
    work_dir: &Path,
    op: InProgressOp,
    known_conflict_paths: &[String],
) -> Result<Vec<String>> {
    let status = run_git_blocking_output(work_dir, &["status", "--porcelain=1", "-z"])?;
    let incoming = incoming_change_paths(work_dir, op)?;
    Ok(classify_unrelated_staged(
        &status,
        known_conflict_paths,
        &incoming,
    ))
}

struct ConflictOpNotification;

/// Surface `message` to the user as a workspace notification. Every failure
/// path in this module goes through here: an op that only logs is
/// indistinguishable from a button that does nothing.
fn notify_user(workspace: &WeakEntity<Workspace>, message: String, cx: &mut App) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        workspace.show_notification(NotificationId::unique::<ConflictOpNotification>(), cx, |cx| {
            cx.new(|cx| MessageNotification::new(message, cx))
        });
    });
}

pub(crate) fn continue_op(this: &mut ConflictResolverView, cx: &mut Context<ConflictResolverView>) {
    let Some(op) = this.op() else {
        return;
    };
    let work_dir = this.work_dir().to_path_buf();
    let workspace = this.workspace();
    let known: Vec<String> = this
        .conflicts()
        .iter()
        .map(|f| f.path.as_std_path().to_string_lossy().into_owned())
        .collect();
    cx.spawn(async move |this, cx| {
        let guard = cx
            .background_spawn({
                let work_dir = work_dir.clone();
                async move { unrelated_staged_changes(&work_dir, op, &known) }
            })
            .await;
        // Fail open. The guard is a courtesy; a repository state it cannot
        // read must not leave Continue as the button that never works.
        let unrelated = match guard {
            Ok(paths) => paths,
            Err(err) => {
                log::warn!(
                    "conflict resolver: unrelated-change guard failed, allowing continue: {err:#}"
                );
                Vec::new()
            }
        };
        if !unrelated.is_empty() {
            let message = format!(
                "Can't continue the {}: {} staged change(s) unrelated to it would be committed too — unstage them first ({}).",
                op.cli_subcommand(),
                unrelated.len(),
                unrelated.join(", ")
            );
            cx.update(|cx| notify_user(&workspace, message, cx));
            this.update(cx, |_, cx| cx.notify()).ok();
            return;
        }
        let outcome = cx
            .background_spawn({
                let work_dir = work_dir.clone();
                async move { OpRunner::run(ContinueMergeOp { op }, &work_dir) }
            })
            .await;
        report_failure(&workspace, op, "--continue", outcome, cx);
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
    let workspace = this.workspace();
    cx.spawn(async move |this, cx| {
        let outcome = cx
            .background_spawn(async move { OpRunner::run(AbortMergeOp { op }, &work_dir) })
            .await;
        report_failure(&workspace, op, "--abort", outcome, cx);
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
    let workspace = this.workspace();
    cx.spawn(async move |this, cx| {
        let outcome = cx
            .background_spawn(async move { OpRunner::run(SkipRebaseOp { op }, &work_dir) })
            .await;
        report_failure(&workspace, op, "--skip", outcome, cx);
        this.update(cx, |this, cx| {
            this.refresh_conflict_list(cx);
        })
        .ok();
    })
    .detach();
}

/// Toast the error from a `git <op> --continue|--abort|--skip` that failed.
/// Without this the button silently does nothing on failure, which is how
/// `--continue` looked when it was aborting on a missing terminal.
fn report_failure(
    workspace: &WeakEntity<Workspace>,
    op: InProgressOp,
    flag: &str,
    outcome: Result<()>,
    cx: &mut gpui::AsyncApp,
) {
    let Err(err) = outcome else {
        return;
    };
    log::warn!("conflict resolver: git {} {flag} failed: {err:#}", op.cli_subcommand());
    let message = format!("`git {} {flag}` failed: {err}", op.cli_subcommand());
    cx.update(|cx| notify_user(workspace, message, cx));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `-z` porcelain payload from `XY <path>` strings.
    fn porcelain(records: &[&str]) -> String {
        records
            .iter()
            .map(|r| format!("{r}\0"))
            .collect::<String>()
    }

    fn incoming(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn blocking_git_runs_with_the_message_editor_suppressed() {
        // Without these, `git <op> --continue` aborts on "Standard input is
        // not a terminal" and no commit is made.
        let command = git_command(Path::new("/tmp"), &["merge", "--continue"]);
        let envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(envs.contains(&("GIT_EDITOR".to_string(), Some("true".to_string()))));
        assert!(envs.contains(&("GIT_SEQUENCE_EDITOR".to_string(), Some("true".to_string()))));
    }

    #[test]
    fn conflicts_are_not_unrelated() {
        let status = porcelain(&["UU a.txt", "AA b.txt", "DD c.txt", "AU d.txt", "UD e.txt"]);
        assert_eq!(
            classify_unrelated_staged(&status, &[], &HashSet::new()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn unstaged_modification_is_not_unrelated() {
        // ` M` — worktree-only. `--continue` commits the index, so this
        // cannot be swept in and must never block.
        let status = porcelain(&["UU a.txt", " M mine.rs"]);
        assert_eq!(
            classify_unrelated_staged(&status, &[], &HashSet::new()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn untracked_file_is_not_unrelated() {
        let status = porcelain(&["UU a.txt", "?? notes.md"]);
        assert_eq!(
            classify_unrelated_staged(&status, &[], &HashSet::new()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn staged_modification_outside_the_incoming_change_is_unrelated() {
        let status = porcelain(&["UU a.txt", "M  mine.rs"]);
        assert_eq!(
            classify_unrelated_staged(&status, &[], &HashSet::new()),
            vec!["mine.rs".to_string()]
        );
    }

    #[test]
    fn staged_by_the_merge_itself_is_not_unrelated() {
        // A merge auto-stages every path the incoming side changed cleanly;
        // treating those as the user's work blocks every real merge.
        let status = porcelain(&["UU a.txt", "M  auto_merged.rs"]);
        assert_eq!(
            classify_unrelated_staged(&status, &[], &incoming(&["a.txt", "auto_merged.rs"])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn known_conflict_path_is_not_unrelated_once_marked_resolved() {
        // `Mark as Resolved` runs `git add`, flipping `UU` to `M `.
        let status = porcelain(&["M  a.txt"]);
        assert_eq!(
            classify_unrelated_staged(&status, &["a.txt".to_string()], &HashSet::new()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn mixed_working_tree_reports_only_the_staged_stranger() {
        let status = porcelain(&[
            "UU a.txt",
            "M  auto_merged.rs",
            " M unstaged.rs",
            "?? notes.md",
            "M  staged_stranger.rs",
            "A  staged_addition.rs",
            "D  staged_deletion.rs",
        ]);
        assert_eq!(
            classify_unrelated_staged(&status, &[], &incoming(&["a.txt", "auto_merged.rs"])),
            vec![
                "staged_stranger.rs".to_string(),
                "staged_addition.rs".to_string(),
                "staged_deletion.rs".to_string(),
            ]
        );
    }

    #[test]
    fn staged_rename_consumes_its_origin_field() {
        // `R  new NUL old NUL` — the origin field carries no XY prefix. Its
        // third character being a space is what makes it look like a record,
        // so the origin path here deliberately has one.
        let status = "R  new.rs\0my old.rs\0M  after.rs\0";
        let records = parse_porcelain_z(&status);
        assert_eq!(
            records
                .iter()
                .map(|r| r.path)
                .collect::<Vec<_>>(),
            vec!["new.rs", "after.rs"]
        );
        assert_eq!(
            classify_unrelated_staged(&status, &[], &HashSet::new()),
            vec!["new.rs".to_string(), "after.rs".to_string()]
        );
    }

    #[test]
    fn paths_with_spaces_survive_parsing() {
        let status = porcelain(&["M  dir with spaces/file name.rs"]);
        assert_eq!(
            classify_unrelated_staged(&status, &[], &HashSet::new()),
            vec!["dir with spaces/file name.rs".to_string()]
        );
    }
}
