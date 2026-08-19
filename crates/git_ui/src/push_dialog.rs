//! S-PSH push dialog with preview.
//!
//! Modal that surfaces what `git push` is about to send: the local
//! commits ahead of the upstream, a click-through file summary on the
//! right column, force / tags / no-verify toggles, divergence detection,
//! and per-commit context-menu rewrites (squash / reword / drop) that
//! re-use the S-DST AtomicGitOp paths.
//!
//! Each pre-push edit goes through the real S-DST handler — full
//! AtomicGitOp with its own backup-ref + undo entry — so the dialog
//! stays crash-safe and abort-able. The dialog only refreshes its
//! preview after each op completes; nothing is batched.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use git::push_rejection::PushRejection;
use git::repository::{Remote, RemoteCommandOutput};
use gpui::{
    AnyElement, AppContext, ClickEvent, ClipboardItem, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement, ParentElement, Render, SharedString, Styled, Task,
    WeakEntity, Window, div,
};
use menu::Cancel;
use project::git_store::Repository;
use ui::{
    App, Button, ButtonStyle, Checkbox, Clickable, Color, Context, Headline, HeadlineSize, Icon,
    IconName, IconSize, IntoElement, Label, LabelCommon, LabelSize, TintColor, ToggleState,
    Tooltip, h_flex, prelude::*, rems, v_flex,
};
use util::ResultExt as _;
use util::command::new_command;
use workspace::{ModalView, Workspace};

use crate::handlers::askpass::askpass_delegate;
use crate::handlers::branch::split_remote_ref;
use crate::mini_graph::{MiniCommit, MiniGraph};
use crate::remote_output::{RemoteAction, format_output};

/// Force-push posture chosen in the dialog footer.
///
/// The dialog only ever produces `None` or `WithLease`. `Force` (a bare
/// `--force`) is no longer offered and never reaches git as `--force`:
/// every runner in the tree upgrades it to `WithLease`, `solution_git`'s
/// argument builder included. The variant survives purely as a legacy
/// *wire* input — it is how an MCP caller that still spells
/// `force_mode: "force"` can be recognised, so the result text can say
/// the request was upgraded instead of silently reporting something the
/// caller did not ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceMode {
    None,
    WithLease,
    Force,
}

/// Snapshot of preview data used to populate the dialog. Built once on
/// open and after each per-commit edit.
#[derive(Debug, Clone, Default)]
pub struct PushPreview {
    pub branch: String,
    pub remote: String,
    pub remote_branch: String,
    pub ahead: Vec<MiniCommit>,
    pub behind: Vec<MiniCommit>,
    pub will_create_remote_branch: bool,
}

impl PushPreview {
    pub fn divergence(&self) -> bool {
        !self.behind.is_empty()
    }
}

/// Push dialog modal view.
pub struct PushDialog {
    workspace: WeakEntity<Workspace>,
    /// Kept so a completed push can ask the git store to re-scan branches —
    /// `run_push_cli` bypasses `Repository::push`, which is what normally
    /// refreshes the ahead/behind counts. See `Repository::refresh_branches`.
    repository: Entity<Repository>,
    work_dir: PathBuf,
    branch: SharedString,
    remote: SharedString,
    remote_branch_editor: Entity<Editor>,
    preview: PushPreview,
    selected_commit: Option<usize>,
    selected_files: Vec<DiffFileSummary>,
    force_mode: ForceMode,
    push_tags: bool,
    no_verify: bool,
    pull_rebase_first: bool,
    force_locked_reason: Option<SharedString>,
    /// A `RequiresConfirmation` force-push waiting on the user. `Some`
    /// replaces the whole dialog body with the confirmation, so the
    /// toggles and commit rows behind it are unreachable while it is up;
    /// it also keeps a second press from starting a second push.
    force_confirm: Option<ForcePushConfirm>,
    pushing: bool,
    refreshing: bool,
    /// Last failed push or remediation, kept so the dialog can render
    /// git's own words. Before this existed the error was only
    /// `log::warn!`-ed and the button silently flipped back to "Push",
    /// which read as "nothing happened".
    failure: Option<PushFailure>,
    /// Transient success line for a remediation ("Pull succeeded — press
    /// Push to retry"). Cleared as soon as another push starts.
    notice: Option<SharedString>,
    /// Label of the remediation currently running. `Some` disables the
    /// remediation row: two concurrent git commands in one work tree
    /// would fight over `index.lock`.
    remediation: Option<SharedString>,
    focus_handle: FocusHandle,
}

/// A failed push (or a failed remediation), kept verbatim.
#[derive(Debug, Clone)]
struct PushFailure {
    kind: PushRejection,
    /// git's own output plus the `anyhow` context chain, unmodified.
    /// Never paraphrased — the whole point is that the user can read
    /// what git actually said.
    detail: SharedString,
}

impl PushFailure {
    fn from_error(err: &anyhow::Error) -> Self {
        // `{:#}` keeps the whole context chain on one line, so the
        // "running `git push`" / "pull --rebase before push" context that
        // `run_push_cli` attaches survives into the UI.
        let detail = format!("{err:#}");
        Self {
            kind: PushRejection::classify(&detail),
            detail: SharedString::from(detail),
        }
    }

    /// A remediation failure is never a *push* rejection, so it keeps the
    /// verbatim text but never offers another round of pull buttons.
    fn from_remediation_error(operation: &str, message: String) -> Self {
        Self {
            kind: PushRejection::Unknown,
            detail: SharedString::from(format!("{operation} failed: {message}")),
        }
    }
}

#[derive(Debug, Clone)]
struct DiffFileSummary {
    path: String,
    status: String,
    additions: u32,
    deletions: u32,
}

impl EventEmitter<DismissEvent> for PushDialog {}
impl ModalView for PushDialog {}
impl Focusable for PushDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl PushDialog {
    /// Open the dialog for the active repository. Resolves branch /
    /// remote / preview asynchronously; the dialog renders a placeholder
    /// until the first refresh completes.
    pub fn open(
        workspace: &mut Workspace,
        force_preset: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(repo) = workspace.project().read(cx).active_repository(cx) else {
            log::info!("PushDialog: no active repository");
            return;
        };
        let workspace_handle = workspace.weak_handle();
        let work_dir: PathBuf = repo.read(cx).work_directory_abs_path.to_path_buf();
        let branch = repo
            .read(cx)
            .branch
            .as_ref()
            .map(|b| SharedString::from(b.name().to_string()));
        let Some(branch) = branch else {
            log::info!("PushDialog: no current branch");
            return;
        };

        let initial_force = if force_preset {
            ForceMode::WithLease
        } else {
            ForceMode::None
        };

        let protection = check_branch_protection(&work_dir, &branch, "push_force");

        workspace.toggle_modal(window, cx, |window, cx| {
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("remote/branch", window, cx);
                editor
            });
            let mut dialog = PushDialog {
                workspace: workspace_handle,
                repository: repo,
                work_dir,
                branch,
                remote: SharedString::from(""),
                remote_branch_editor: editor,
                preview: PushPreview::default(),
                selected_commit: None,
                selected_files: Vec::new(),
                force_mode: initial_force,
                push_tags: false,
                no_verify: false,
                pull_rebase_first: false,
                force_locked_reason: protection,
                force_confirm: None,
                pushing: false,
                refreshing: false,
                failure: None,
                notice: None,
                remediation: None,
                focus_handle: cx.focus_handle(),
            };
            dialog.refresh_preview(window, cx);
            dialog
        });
    }

    fn refresh_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let work_dir = self.work_dir.clone();
        let branch = self.branch.to_string();
        let remote_override = self.remote_branch_editor.read(cx).text(cx);
        self.refreshing = true;
        cx.spawn_in(window, async move |this, cx| {
            let preview = cx
                .background_spawn({
                    let work_dir = work_dir.clone();
                    let branch = branch.clone();
                    async move { build_preview(&work_dir, &branch, remote_override.as_str()).await }
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.refreshing = false;
                match preview {
                    Ok(preview) => {
                        let editor_text = this.remote_branch_editor.read(cx).text(cx);
                        if editor_text.trim().is_empty() {
                            let initial = preview.remote_branch.clone();
                            this.remote_branch_editor.update(cx, |editor, cx| {
                                editor.set_text(initial, window, cx);
                            });
                        }
                        this.remote = SharedString::from(preview.remote.clone());
                        this.preview = preview;
                        if this.selected_commit.is_none() && !this.preview.ahead.is_empty() {
                            this.set_selected_commit(Some(0), cx);
                        } else if this.preview.ahead.is_empty() {
                            this.selected_commit = None;
                            this.selected_files.clear();
                        } else if let Some(ix) = this.selected_commit
                            && ix >= this.preview.ahead.len()
                        {
                            this.set_selected_commit(Some(this.preview.ahead.len() - 1), cx);
                        }
                    }
                    Err(err) => {
                        log::warn!("PushDialog: preview refresh failed: {err}");
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_selected_commit(&mut self, ix: Option<usize>, cx: &mut Context<Self>) {
        self.selected_commit = ix;
        self.selected_files.clear();
        let Some(ix) = ix else {
            cx.notify();
            return;
        };
        let Some(commit) = self.preview.ahead.get(ix).cloned() else {
            cx.notify();
            return;
        };
        let work_dir = self.work_dir.clone();
        cx.spawn(async move |this, cx| {
            let files = cx
                .background_spawn(async move { commit_file_summary(&work_dir, &commit.sha).await })
                .await
                .log_err()
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                this.selected_files = files;
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        // Escape backs out of the force-push confirmation before it
        // closes the dialog: only the destructive control is ever gated,
        // never the way out of it.
        if self.force_confirm.is_some() {
            self.cancel_force_push_confirm(cx);
            return;
        }
        cx.emit(DismissEvent);
    }

    fn toggle_force_with_lease(&mut self, cx: &mut Context<Self>) {
        if self.force_locked_reason.is_some() {
            return;
        }
        self.force_mode = match self.force_mode {
            ForceMode::WithLease => ForceMode::None,
            _ => ForceMode::WithLease,
        };
        cx.notify();
    }

    fn toggle_tags(&mut self, cx: &mut Context<Self>) {
        self.push_tags = !self.push_tags;
        cx.notify();
    }

    fn toggle_no_verify(&mut self, cx: &mut Context<Self>) {
        self.no_verify = !self.no_verify;
        cx.notify();
    }

    fn toggle_pull_rebase(&mut self, cx: &mut Context<Self>) {
        self.pull_rebase_first = !self.pull_rebase_first;
        cx.notify();
    }

    /// The Push button's press boundary. Everything that has to be
    /// decided *now* rather than at dialog-open time happens here: the
    /// remote / remote-branch text the user may have edited since, and
    /// the S-SOL-PRT branch-protection gate.
    fn confirm_push(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.pushing || self.remediation.is_some() || self.force_confirm.is_some() {
            return;
        }
        self.failure = None;
        self.notice = None;
        let work_dir = self.work_dir.clone();
        let branch = self.branch.to_string();
        let remote = self.remote.to_string();
        let remote_branch = self
            .remote_branch_editor
            .read(cx)
            .text(cx)
            .trim()
            .to_string();
        if remote.is_empty() || remote_branch.is_empty() {
            log::warn!("PushDialog: remote/remote_branch empty, refusing to push");
            return;
        }
        // S-SOL-PRT — consult the policy again here. The dialog's
        // `force_locked_reason` already disabled the toggle for
        // `Forbidden`, but a stale snapshot or a settings change between
        // dialog-open and press could still let the toggle stay on, and
        // `RequiresConfirmation` is only ever asked at this boundary —
        // dialog-open deliberately leaves that tier's toggle enabled.
        if !matches!(self.force_mode, ForceMode::None) {
            let decision = solutions::branch_protection::check(&work_dir, &branch, "force_push");
            match force_push_gate(&decision, self.force_mode, &branch, &remote, &remote_branch) {
                ForcePushGate::Proceed => {}
                ForcePushGate::Locked { reason } => {
                    log::warn!("PushDialog: force-push refused by branch protection: {reason}");
                    self.force_locked_reason = Some(reason);
                    self.force_mode = ForceMode::None;
                    cx.notify();
                    return;
                }
                ForcePushGate::Confirm {
                    title,
                    detail,
                    confirm_label,
                } => {
                    log::info!("PushDialog: force-push requires confirmation ({decision:?})");
                    self.open_force_push_confirm(
                        title,
                        detail,
                        confirm_label,
                        work_dir,
                        branch,
                        remote,
                        remote_branch,
                        cx,
                    );
                    return;
                }
            }
        }
        self.start_push(work_dir, branch, remote, remote_branch, cx);
    }

    /// Runs the push the user already agreed to. Split out of
    /// [`Self::confirm_push`] so the branch-protection confirmation can
    /// re-enter it from the prompt's completion, pushing exactly the
    /// target that was gated rather than re-reading fields that may have
    /// moved.
    fn start_push(
        &mut self,
        work_dir: PathBuf,
        branch: String,
        remote: String,
        remote_branch: String,
        cx: &mut Context<Self>,
    ) {
        let opts = PushInvocation {
            force_mode: self.force_mode,
            tags: self.push_tags,
            no_verify: self.no_verify,
            set_upstream: self.preview.will_create_remote_branch,
            pull_rebase_first: self.pull_rebase_first,
        };
        self.pushing = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let work_dir = work_dir.clone();
                    let branch = branch.clone();
                    let remote = remote.clone();
                    let remote_branch = remote_branch.clone();
                    async move {
                        run_push_cli(&work_dir, &branch, &remote, &remote_branch, &opts).await
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.pushing = false;
                match result {
                    Ok(output) => {
                        let action = RemoteAction::Push(
                            SharedString::from(branch.clone()),
                            Remote {
                                name: SharedString::from(remote.clone()),
                            },
                        );
                        let success = format_output(&action, output);
                        log::info!("PushDialog: push succeeded — {}", success.message);
                        // The push moved `refs/remotes/**` behind the git
                        // store's back (no fs-watcher event covers that), so
                        // without this the branch widget keeps its `↑N` badge
                        // and the graph keeps drawing `origin/…` on the
                        // pre-push commit.
                        let rescan = this
                            .repository
                            .update(cx, |repository, cx| repository.refresh_branches(cx));
                        cx.background_spawn(async move {
                            match rescan.await {
                                Ok(Ok(())) => {}
                                Ok(Err(err)) => {
                                    log::warn!("PushDialog: branch rescan after push failed: {err}")
                                }
                                Err(_) => log::warn!(
                                    "PushDialog: branch rescan after push was dropped by the git store"
                                ),
                            }
                        })
                        .detach();
                        cx.emit(DismissEvent);
                    }
                    Err(err) => {
                        // Keep the dialog open and show git's verbatim
                        // message; a rejected push is the one case where
                        // the user most needs to read what the remote said.
                        // A rejected push does not update `refs/remotes/**`,
                        // so re-deriving the preview here would show the same
                        // stale ahead/behind counts; the refresh happens after
                        // a remediation pull instead.
                        log::warn!("PushDialog: push failed: {err:#}");
                        this.failure = Some(PushFailure::from_error(&err));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Put the S-SOL-PRT confirmation up in place of the dialog body and
    /// start reading what the force-push would destroy. The countdown and
    /// the git query are tasks owned by the confirmation state, so
    /// cancelling it drops both.
    #[allow(clippy::too_many_arguments)]
    fn open_force_push_confirm(
        &mut self,
        title: String,
        detail: String,
        confirm_label: String,
        work_dir: PathBuf,
        branch: String,
        remote: String,
        remote_branch: String,
        cx: &mut Context<Self>,
    ) {
        let load = cx.spawn({
            let work_dir = work_dir.clone();
            let branch = branch.clone();
            let remote = remote.clone();
            let remote_branch = remote_branch.clone();
            async move |this, cx| {
                let overwritten = cx
                    .background_spawn(async move {
                        overwritten_commits(&work_dir, &branch, &remote, &remote_branch).await
                    })
                    .await;
                this.update(cx, |this, cx| this.resolve_overwritten(overwritten, cx))
                    .ok();
            }
        });
        self.force_confirm = Some(ForcePushConfirm {
            title: SharedString::from(title),
            detail: SharedString::from(detail),
            confirm_label: SharedString::from(confirm_label),
            work_dir,
            branch,
            remote,
            remote_branch,
            state: ForcePushConfirmState::Pending,
            _load: load,
        });
        cx.notify();
    }

    /// Turn the overwrite list into the confirmation's final state. This
    /// is where the force push is allowed or refused: an empty list makes
    /// it pointless and an unreadable one makes it blind, and neither is
    /// something a countdown or a warning label should be asked to
    /// protect the user from.
    fn resolve_overwritten(&mut self, overwritten: OverwrittenCommits, cx: &mut Context<Self>) {
        let Some(confirm) = self.force_confirm.as_ref() else {
            return;
        };
        // One press, one read, one answer: a resolution that arrives
        // after the confirmation has already settled is stale by
        // definition, and must not restart a countdown the user is part
        // way through — or, worse, reopen a push that was refused.
        if !matches!(confirm.state, ForcePushConfirmState::Pending) {
            return;
        }
        let state = match overwritten {
            OverwrittenCommits::Unknown { why } => {
                log::warn!(
                    "PushDialog: refusing force-push to {}/{}: {why}",
                    confirm.remote,
                    confirm.remote_branch
                );
                ForcePushConfirmState::Refused(ForcePushRefusal::Undeterminable { why })
            }
            OverwrittenCommits::Known(commits) if commits.is_empty() => {
                log::info!(
                    "PushDialog: refusing force-push to {}/{}: nothing to overwrite",
                    confirm.remote,
                    confirm.remote_branch
                );
                ForcePushConfirmState::Refused(ForcePushRefusal::NothingToOverwrite)
            }
            OverwrittenCommits::Known(commits) => ForcePushConfirmState::Offered {
                commits,
                countdown: FORCE_PUSH_CONFIRM_DELAY_SECS,
                _countdown: Self::spawn_confirm_countdown(cx),
            },
        };
        if let Some(confirm) = self.force_confirm.as_mut() {
            confirm.state = state;
        }
        cx.notify();
    }

    /// Ticks the reflex-click window down once a second. Only ever
    /// started for an offered push, so a refusal never counts down to
    /// something the user cannot have.
    fn spawn_confirm_countdown(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            for _ in 0..FORCE_PUSH_CONFIRM_DELAY_SECS {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let still_counting = this
                    .update(cx, |this, cx| {
                        let Some(ForcePushConfirmState::Offered { countdown, .. }) = this
                            .force_confirm
                            .as_mut()
                            .map(|confirm| &mut confirm.state)
                        else {
                            return false;
                        };
                        *countdown = countdown.saturating_sub(1);
                        // The countdown lives in the retained scene, so
                        // this notify — raised from a task, never from a
                        // draw phase, where it would be discarded — is
                        // the only thing that repaints the label.
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !still_counting {
                    return;
                }
            }
        })
    }

    /// Back out of a pending force-push confirmation. Dropping the state
    /// cancels its countdown and its git query along with it.
    fn cancel_force_push_confirm(&mut self, cx: &mut Context<Self>) {
        if self.force_confirm.take().is_some() {
            log::info!("PushDialog: force-push declined at the branch-protection confirmation");
            cx.notify();
        }
    }

    /// The confirmation's destructive button. Pushes exactly the target
    /// that was gated, and only once the confirmation has armed.
    fn confirm_force_push(&mut self, cx: &mut Context<Self>) {
        if !self
            .force_confirm
            .as_ref()
            .is_some_and(ForcePushConfirm::is_armed)
        {
            return;
        }
        let Some(confirm) = self.force_confirm.take() else {
            return;
        };
        log::info!(
            "PushDialog: force-push confirmed for {}:{} on {}",
            confirm.branch,
            confirm.remote_branch,
            confirm.remote
        );
        self.start_push(
            confirm.work_dir,
            confirm.branch,
            confirm.remote,
            confirm.remote_branch,
            cx,
        );
    }

    /// Remediation for a non-fast-forward rejection: integrate the remote
    /// commits, then let the user press Push again. Goes through
    /// `Repository::pull` (the same typed API the git panel uses) rather
    /// than shelling out, so askpass, the job queue and the git store's
    /// own refresh all behave exactly as they do for a panel pull.
    fn run_pull(&mut self, rebase: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.pushing || self.remediation.is_some() {
            return;
        }
        let remote = self.remote.to_string();
        if remote.is_empty() {
            self.failure = Some(PushFailure::from_remediation_error(
                "pull",
                "no remote is configured for this branch".to_string(),
            ));
            cx.notify();
            return;
        }
        let label: SharedString = if rebase {
            "Pulling (rebase)…".into()
        } else {
            "Pulling (merge)…".into()
        };
        let operation = if rebase {
            "pull --rebase".to_string()
        } else {
            "pull".to_string()
        };
        let askpass = askpass_delegate(
            self.workspace.clone(),
            format!("git {operation} {remote}"),
            window,
            cx,
        );
        // `git pull` needs an explicit refspec when the local branch has no
        // upstream yet; mirrors `GitPanel::pull`.
        let branch_arg = self.repository.read(cx).branch.as_ref().and_then(|branch| {
            branch
                .upstream
                .is_none()
                .then(|| SharedString::from(branch.name().to_string()))
        });
        let pull = self.repository.update(cx, |repository, cx| {
            repository.pull(branch_arg, SharedString::from(remote), rebase, askpass, cx)
        });
        self.remediation = Some(label);
        self.notice = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcome = pull.await;
            this.update(cx, |this, cx| {
                this.remediation = None;
                match outcome {
                    Ok(Ok(output)) => {
                        log::info!(
                            "PushDialog: {operation} succeeded — {}",
                            output.stderr.trim_end()
                        );
                        this.failure = None;
                        this.notice = Some(SharedString::from(format!(
                            "`git {operation}` succeeded. Press Push to retry."
                        )));
                        // The pull moved both HEAD and `refs/remotes/**`, so
                        // the ahead/behind preview must be rebuilt before the
                        // user looks at it again.
                        this.refresh_no_window(cx);
                    }
                    Ok(Err(err)) => {
                        log::warn!("PushDialog: {operation} failed: {err:#}");
                        this.failure = Some(PushFailure::from_remediation_error(
                            &operation,
                            format!("{err:#}"),
                        ));
                    }
                    Err(_) => {
                        log::warn!("PushDialog: {operation} was dropped by the git store");
                        this.failure = Some(PushFailure::from_remediation_error(
                            &operation,
                            "the git store dropped the job before it completed".to_string(),
                        ));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Remediation of last resort: re-run the push with
    /// `--force-with-lease`. Reuses `confirm_push` rather than calling
    /// `run_force_with_lease` directly so the S-SOL-PRT branch-protection
    /// check at the press boundary still applies. Never offers a bare
    /// `--force`; the lease is what makes this recoverable.
    fn run_force_push_with_lease(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pushing || self.remediation.is_some() || self.force_confirm.is_some() {
            return;
        }
        if self.force_locked_reason.is_some() {
            return;
        }
        self.force_mode = ForceMode::WithLease;
        self.confirm_push(window, cx);
    }

    fn copy_failure(&mut self, cx: &mut Context<Self>) {
        let Some(failure) = self.failure.as_ref() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(failure.detail.to_string()));
    }

    fn run_squash_with_previous(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commit) = self.preview.ahead.get(ix).cloned() else {
            return;
        };
        let work_dir = self.work_dir.clone();
        let sha = commit.sha.clone();
        let subject = commit.subject;
        let prev = format!("{sha}^");

        let proceed = self.confirm_remote_reach(&sha, "squash", window, cx);
        cx.spawn(async move |this, cx| {
            if !proceed.await {
                return;
            }
            let task = cx.update(|cx| {
                crate::handlers::squash::run(
                    work_dir,
                    vec![prev, sha],
                    subject,
                    git::operations::rebase::RebaseCallbacks::default(),
                    cx,
                )
            });
            if let Err(err) = task.await {
                log::warn!("PushDialog: squash failed: {err}");
            }
            this.update(cx, |this, cx| {
                this.refresh_no_window(cx);
            })
            .ok();
        })
        .detach();
    }

    fn run_reword(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commit) = self.preview.ahead.get(ix).cloned() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let work_dir = self.work_dir.clone();
        let sha = commit.sha.clone();
        let initial = commit.subject;
        let weak = cx.weak_entity();

        let proceed = self.confirm_remote_reach(&sha, "reword", window, cx);
        cx.spawn_in(window, async move |_, cx| {
            if !proceed.await {
                return;
            }
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        RewordPromptModal::new(weak, work_dir, sha, initial, window, cx)
                    });
                })
                .ok();
        })
        .detach();
    }

    fn run_drop(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commit) = self.preview.ahead.get(ix).cloned() else {
            return;
        };
        let work_dir = self.work_dir.clone();
        let sha = commit.sha;
        let short: String = sha.chars().take(7).collect();
        let proceed_remote = self.confirm_remote_reach(&sha, "drop", window, cx);
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Drop commit {short}?"),
            Some(
                "Rewrites history above this commit. The branch tip is \
                 backed up — use Undo Last Operation to recover.",
            ),
            &["Drop", "Cancel"],
            cx,
        );

        cx.spawn(async move |this, cx| {
            if !proceed_remote.await {
                return;
            }
            if answer.await.ok() != Some(0) {
                return;
            }
            let task = cx.update(|cx| {
                crate::handlers::drop::run(
                    work_dir,
                    sha,
                    git::operations::rebase::RebaseCallbacks::default(),
                    cx,
                )
            });
            if let Err(err) = task.await {
                log::warn!("PushDialog: drop failed: {err}");
            }
            this.update(cx, |this, cx| {
                this.refresh_no_window(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Returns a future that resolves to `true` when the user confirms (or
    /// the commit isn't reachable from any remote ref). Soft-guard for
    /// pre-edit destructive ops on already-pushed commits.
    fn confirm_remote_reach(
        &self,
        sha: &str,
        op_label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        let work_dir = self.work_dir.clone();
        let sha = sha.to_string();
        let op_label = op_label.to_string();
        let window_handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            let reach = cx
                .background_spawn({
                    let work_dir = work_dir.clone();
                    let sha = sha.clone();
                    async move { commit_remote_refs(&work_dir, &sha).await }
                })
                .await
                .log_err()
                .unwrap_or_default();
            if reach.is_empty() {
                return true;
            }
            let summary = reach.join(", ");
            let answer = window_handle
                .update(cx, |_, window, cx| {
                    window.prompt(
                        gpui::PromptLevel::Warning,
                        &format!(
                            "Commit {} exists in {} as well",
                            &sha[..7.min(sha.len())],
                            summary
                        ),
                        Some(&format!(
                            "Rewriting it locally ({op_label}) means a future push to that location will require --force-with-lease. Continue?"
                        )),
                        &["Continue", "Cancel"],
                        cx,
                    )
                })
                .ok();
            match answer {
                Some(a) => a.await.ok() == Some(0),
                None => false,
            }
        })
    }

    /// Refresh without needing a `Window` — used after async S-DST ops
    /// that don't preserve the window across await points.
    fn refresh_no_window(&mut self, cx: &mut Context<Self>) {
        let work_dir = self.work_dir.clone();
        let branch = self.branch.to_string();
        let remote_override = self.remote_branch_editor.read(cx).text(cx);
        self.refreshing = true;
        cx.spawn(async move |this, cx| {
            let preview = cx
                .background_spawn(async move {
                    build_preview(&work_dir, &branch, remote_override.as_str()).await
                })
                .await;
            this.update(cx, |this, cx| {
                this.refreshing = false;
                if let Ok(preview) = preview {
                    this.remote = SharedString::from(preview.remote.clone());
                    this.preview = preview;
                    if this.preview.ahead.is_empty() {
                        this.selected_commit = None;
                        this.selected_files.clear();
                    } else if let Some(ix) = this.selected_commit
                        && ix >= this.preview.ahead.len()
                    {
                        this.set_selected_commit(Some(this.preview.ahead.len() - 1), cx);
                    } else if this.selected_commit.is_none() {
                        this.set_selected_commit(Some(0), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// Bag passed into `run_push_cli` to keep its arg count manageable.
struct PushInvocation {
    force_mode: ForceMode,
    tags: bool,
    no_verify: bool,
    set_upstream: bool,
    pull_rebase_first: bool,
}

impl Render for PushDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The confirmation takes the dialog over rather than layering on
        // top of it: while it is up, none of the toggles or commit rows
        // behind it may be reachable.
        if self.force_confirm.is_some() {
            return self.render_force_push_confirm(cx);
        }
        let header = self.render_header().into_any_element();
        let body = self.render_body(cx).into_any_element();
        let status = self.render_status(cx);
        let footer = self.render_footer(cx).into_any_element();
        v_flex()
            .key_context("PushDialog")
            .on_action(cx.listener(Self::cancel))
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(rems(64.))
            .max_h(rems(40.))
            .p_3()
            .gap_2()
            .child(header)
            .child(body)
            .when_some(status, |this, status| this.child(status))
            .child(footer)
            .into_any_element()
    }
}

impl PushDialog {
    /// The bespoke force-push confirmation: what is about to be run, the
    /// server-side commits it would overwrite, and — only when there are
    /// any — a confirm button that stays dead for
    /// [`FORCE_PUSH_CONFIRM_DELAY_SECS`] so a reflex click aimed at
    /// "Push" cannot land on it. When the push is refused there is no
    /// confirm button at all, dead or otherwise.
    fn render_force_push_confirm(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(confirm) = self.force_confirm.as_ref() else {
            return div().into_any_element();
        };
        let tracking = format!("{}/{}", confirm.remote, confirm.remote_branch);

        let mut body = v_flex().gap_1();
        let mut confirm_button = None;
        match &confirm.state {
            ForcePushConfirmState::Pending => {
                body = body.child(
                    Label::new(format!("Reading what {tracking} holds…"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            ForcePushConfirmState::Refused(refusal) => {
                let (headline, advice) =
                    refusal_lines(refusal, &confirm.branch, &tracking, &confirm.remote);
                body = body
                    .child(
                        Label::new(headline)
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new(advice)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    );
            }
            ForcePushConfirmState::Offered {
                commits, countdown, ..
            } => {
                let plural = if commits.len() == 1 { "" } else { "s" };
                body = body.child(
                    Label::new(format!(
                        "{} commit{plural} on {tracking} will be overwritten:",
                        commits.len()
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Error),
                );
                let mut rows = v_flex()
                    .id("push-dialog-force-confirm-overwritten")
                    .max_h(rems(12.))
                    .overflow_y_scroll();
                for (ix, commit) in commits.iter().take(MAX_OVERWRITTEN_ROWS).enumerate() {
                    rows = rows.child(render_overwritten_row(ix, commit));
                }
                body = body.child(rows);
                if commits.len() > MAX_OVERWRITTEN_ROWS {
                    body = body.child(
                        Label::new(format!(
                            "…and {} more.",
                            commits.len() - MAX_OVERWRITTEN_ROWS
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    );
                }
                let label: SharedString = if *countdown > 0 {
                    format!("{} ({countdown})", confirm.confirm_label).into()
                } else {
                    confirm.confirm_label.clone()
                };
                confirm_button = Some(
                    Button::new("push-dialog-force-confirm", label)
                        .style(ButtonStyle::Tinted(TintColor::Error))
                        .disabled(*countdown > 0)
                        .on_click(cx.listener(|this, _, _window, cx| this.confirm_force_push(cx))),
                );
            }
        }

        v_flex()
            .key_context("PushDialog")
            .on_action(cx.listener(Self::cancel))
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(rems(64.))
            .max_h(rems(40.))
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Error),
                    )
                    .child(Headline::new(confirm.title.clone()).size(HeadlineSize::Small)),
            )
            .child(
                Label::new(confirm.detail.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(body)
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("push-dialog-force-confirm-cancel", "Cancel").on_click(
                            cx.listener(|this, _, _window, cx| this.cancel_force_push_confirm(cx)),
                        ),
                    )
                    .when_some(confirm_button, |this, button| this.child(button)),
            )
            .into_any_element()
    }

    fn render_header(&self) -> impl IntoElement {
        let branch = self.branch.clone();
        let remote = if self.remote.is_empty() {
            SharedString::from("(no remote)")
        } else {
            self.remote.clone()
        };
        let create_hint = if self.preview.will_create_remote_branch {
            Some(
                Label::new("Will create new remote branch")
                    .size(LabelSize::XSmall)
                    .color(Color::Accent),
            )
        } else {
            None
        };
        h_flex()
            .gap_2()
            .child(Icon::new(IconName::ArrowUp).size(IconSize::Small))
            .child(Headline::new("Push").size(HeadlineSize::Small))
            .child(Label::new(branch).size(LabelSize::Small))
            .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
            .child(
                Label::new(remote)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new("/").size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .min_w(rems(16.))
                    .child(self.remote_branch_editor.clone()),
            )
            .when_some(create_hint, |this, hint| this.child(hint))
    }

    /// Renders the outcome of the last push / remediation. Git's own text
    /// is reproduced verbatim in a scrollable monospace block; the
    /// classification is only used to choose which remediation buttons to
    /// offer, never to replace the message.
    fn render_status(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if let Some(remediation) = self.remediation.clone() {
            return Some(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::ArrowCircle)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(remediation)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }

        if let Some(notice) = self.notice.clone() {
            return Some(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Check)
                            .size(IconSize::Small)
                            .color(Color::Success),
                    )
                    .child(
                        Label::new(notice)
                            .size(LabelSize::Small)
                            .color(Color::Success),
                    )
                    .into_any_element(),
            );
        }

        let failure = self.failure.as_ref()?;

        let mut detail = v_flex().gap_0p5();
        for line in failure.detail.lines() {
            detail = detail.child(
                Label::new(SharedString::from(line.to_string()))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .buffer_font(cx),
            );
        }

        let mut block = v_flex()
            .gap_1p5()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().status().error_border)
            .bg(cx.theme().status().error_background)
            .child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Icon::new(IconName::XCircle)
                                    .size(IconSize::Small)
                                    .color(Color::Error),
                            )
                            .child(
                                Label::new(failure.kind.headline())
                                    .size(LabelSize::Small)
                                    .color(Color::Error),
                            ),
                    )
                    .child(
                        Button::new("push-dialog-copy-error", "Copy")
                            .label_size(LabelSize::XSmall)
                            .tooltip(Tooltip::text("Copy git's output to the clipboard"))
                            .on_click(cx.listener(|this, _, _window, cx| this.copy_failure(cx))),
                    ),
            )
            .child(
                div()
                    .id("push-dialog-error-detail")
                    .max_h(rems(8.))
                    .overflow_y_scroll()
                    .child(detail),
            );

        if failure.kind.is_diverged() {
            let force_locked = self.force_locked_reason.clone();
            let mut force_button =
                Button::new("push-dialog-remediate-force-lease", "Force push with lease")
                    .style(ButtonStyle::Tinted(TintColor::Error))
                    .disabled(force_locked.is_some())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run_force_push_with_lease(window, cx)
                    }));
            force_button = match force_locked {
                Some(reason) => force_button.tooltip(Tooltip::text(reason)),
                None => force_button.tooltip(Tooltip::text(
                    "Overwrites the remote branch, but only if it still points at the commit \
                     you last fetched. Discards the remote-only commits listed above.",
                )),
            };

            block = block.child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("push-dialog-remediate-pull-rebase", "Pull with rebase")
                            .tooltip(Tooltip::text(
                                "git pull --rebase — replays your commits on top of the remote.",
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.run_pull(true, window, cx)),
                            ),
                    )
                    .child(
                        Button::new("push-dialog-remediate-pull-merge", "Pull (merge)")
                            .tooltip(Tooltip::text(
                                "git pull — merges the remote commits into your branch.",
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.run_pull(false, window, cx)),
                            ),
                    )
                    .child(force_button),
            );
        }

        Some(block.into_any_element())
    }

    fn render_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let commits = self.preview.ahead.clone();
        let total = commits.len();
        let selected = self.selected_commit;

        let mini = if commits.is_empty() {
            div()
                .py_4()
                .child(
                    // Pushing is not gated on having commits ahead: with `tags`
                    // on, `git push --tags` still has work to do. Saying
                    // "Nothing to push." there would contradict the Push button
                    // sitting right below it, enabled and about to push a tag.
                    Label::new(if self.refreshing {
                        "Loading…"
                    } else if self.push_tags {
                        "No commits to push — tags will still be pushed."
                    } else {
                        "Nothing to push."
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element()
        } else {
            let entity = cx.weak_entity();
            MiniGraph::new(commits)
                .with_selected(selected)
                .render(
                    move |ix, cx| {
                        if let Some(this) = entity.upgrade() {
                            this.update(cx, |this, cx| this.set_selected_commit(Some(ix), cx));
                        }
                    },
                    cx,
                )
                .into_any_element()
        };

        let detail: Vec<gpui::AnyElement> = if let Some(ix) = selected
            && let Some(commit) = self.preview.ahead.get(ix)
        {
            let header = h_flex()
                .gap_2()
                .child(
                    Label::new(commit.subject.clone())
                        .size(LabelSize::Small)
                        .color(Color::Default),
                )
                .child(
                    Label::new(commit.short_sha())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element();
            let mut rows: Vec<gpui::AnyElement> = vec![header];
            for file in &self.selected_files {
                rows.push(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(file.status.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(file.path.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Default)
                                .truncate(),
                        )
                        .child(
                            Label::new(format!("+{} −{}", file.additions, file.deletions))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .into_any_element(),
                );
            }
            if self.selected_files.is_empty() {
                rows.push(
                    Label::new("Loading file list…")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .into_any_element(),
                );
            }
            rows
        } else {
            vec![
                Label::new("Select a commit to see its file changes.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element(),
            ]
        };

        let context_menu_row = if let Some(ix) = selected {
            let entity = cx.weak_entity();
            let entity_for_squash = entity.clone();
            let entity_for_reword = entity.clone();
            let entity_for_drop = entity;
            Some(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("push-dialog-squash", "Squash with Previous").on_click(
                            move |_event: &ClickEvent, window, cx| {
                                if let Some(this) = entity_for_squash.upgrade() {
                                    this.update(cx, |this, cx| {
                                        this.run_squash_with_previous(ix, window, cx)
                                    });
                                }
                            },
                        ),
                    )
                    .child(Button::new("push-dialog-reword", "Reword").on_click(
                        move |_event: &ClickEvent, window, cx| {
                            if let Some(this) = entity_for_reword.upgrade() {
                                this.update(cx, |this, cx| this.run_reword(ix, window, cx));
                            }
                        },
                    ))
                    .child(Button::new("push-dialog-drop", "Drop").on_click(
                        move |_event: &ClickEvent, window, cx| {
                            if let Some(this) = entity_for_drop.upgrade() {
                                this.update(cx, |this, cx| this.run_drop(ix, window, cx));
                            }
                        },
                    )),
            )
        } else {
            None
        };

        let summary_label = format!(
            "{total} commit(s) ahead{}",
            if self.preview.divergence() {
                format!(", remote {} ahead", self.preview.behind.len())
            } else {
                String::new()
            }
        );

        v_flex()
            .gap_2()
            .child(
                Label::new(summary_label)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_3()
                    .h(rems(20.))
                    .child(div().w(rems(28.)).h_full().overflow_hidden().child(mini))
                    .child(div().w_px().h_full().bg(cx.theme().colors().border_variant))
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .gap_1()
                            .overflow_hidden()
                            .children(detail),
                    ),
            )
            .when_some(context_menu_row, |this, row| this.child(row))
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let force_locked = self.force_locked_reason.clone();
        // Any force posture is `--force-with-lease` here: the dialog no
        // longer offers a bare `--force`, and `run_push_cli` upgrades the
        // legacy variant to the leased form.
        let force_lease_state = if matches!(self.force_mode, ForceMode::None) {
            ToggleState::Unselected
        } else {
            ToggleState::Selected
        };
        let tags_state = if self.push_tags {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };
        let no_verify_state = if self.no_verify {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };

        let force_lease_box = Checkbox::new("push-dialog-force-with-lease", force_lease_state)
            .label("force-with-lease")
            .disabled(force_locked.is_some())
            .on_click(cx.listener(|this, _, _, cx| this.toggle_force_with_lease(cx)));
        // The toggle stays live even when the push will end up refused:
        // the only pre-press evidence available is `preview.behind`,
        // which is `--no-merges`-filtered and goes stale on a failed
        // refresh, so disabling on it would silently block a legitimate
        // force-push (a remote ahead only by a merge commit). The refusal
        // belongs at the press boundary, where the state is read fresh —
        // the tooltip is what makes that discoverable beforehand.
        let force_lease_box = match force_locked {
            Some(reason) => force_lease_box.tooltip(Tooltip::text(reason)),
            None => force_lease_box.tooltip(Tooltip::text(SharedString::from(
                "Checked against the remote when you press Push, and refused unless the \
                 remote really holds commits this branch does not.",
            ))),
        };

        let mut footer = v_flex().gap_2().child(
            h_flex()
                .gap_3()
                .child(force_lease_box)
                .child(
                    Checkbox::new("push-dialog-tags", tags_state)
                        .label("tags")
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_tags(cx))),
                )
                .child(
                    Checkbox::new("push-dialog-no-verify", no_verify_state)
                        .label("no-verify")
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_no_verify(cx))),
                ),
        );
        if self.preview.divergence() {
            let pull_rebase_state = if self.pull_rebase_first {
                ToggleState::Selected
            } else {
                ToggleState::Unselected
            };
            footer = footer.child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(format!(
                            "Remote has {} commits ahead",
                            self.preview.behind.len()
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                    )
                    .child(
                        Checkbox::new("push-dialog-pull-rebase", pull_rebase_state)
                            .label("Pull --rebase first")
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_pull_rebase(cx))),
                    ),
            );
        }
        let pushing = self.pushing;
        // Pressing again while the branch-protection prompt is up would
        // do nothing (`confirm_push` bails), but the button must not
        // look live either — and it is not "Pushing…" yet.
        let push_disabled = pushing || self.force_confirm.is_some();
        footer = footer.child(
            h_flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("push-dialog-cancel", "Cancel")
                        .on_click(cx.listener(|_this, _, _window, cx| cx.emit(DismissEvent))),
                )
                .child(
                    Button::new(
                        "push-dialog-push",
                        if pushing { "Pushing…" } else { "Push" },
                    )
                    .disabled(push_disabled)
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_push(window, cx))),
                ),
        );
        footer
    }
}

/// Modal launched from the dialog when the user picks "Reword" on a row.
struct RewordPromptModal {
    parent: WeakEntity<PushDialog>,
    work_dir: PathBuf,
    sha: String,
    editor: Entity<Editor>,
    focus_handle: FocusHandle,
}

impl RewordPromptModal {
    fn new(
        parent: WeakEntity<PushDialog>,
        work_dir: PathBuf,
        sha: String,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(initial, window, cx);
            editor
        });
        Self {
            parent,
            work_dir,
            sha,
            editor,
            focus_handle: cx.focus_handle(),
        }
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let new_message = self.editor.read(cx).text(cx);
        if new_message.trim().is_empty() {
            return;
        }
        let parent = self.parent.clone();
        let work_dir = self.work_dir.clone();
        let sha = self.sha.clone();
        let task = crate::handlers::edit_message::run(
            work_dir,
            sha,
            new_message,
            git::operations::rebase::RebaseCallbacks::default(),
            cx,
        );
        cx.spawn(async move |_, cx| {
            if let Err(err) = task.await {
                log::warn!("PushDialog: reword failed: {err}");
            }
            parent
                .update(cx, |parent, cx| parent.refresh_no_window(cx))
                .ok();
        })
        .detach();
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for RewordPromptModal {}
impl ModalView for RewordPromptModal {}
impl Focusable for RewordPromptModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for RewordPromptModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let short: String = self.sha.chars().take(7).collect();
        v_flex()
            .key_context("RewordPromptModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .track_focus(&self.focus_handle)
            .elevation_2(cx)
            .w(rems(40.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::Pencil).size(IconSize::XSmall))
                    .child(Headline::new(format!("Reword ({short})")).size(HeadlineSize::XSmall)),
            )
            .child(div().px_3().pb_3().w_full().child(self.editor.clone()))
    }
}

// =====================================================================
//  Helpers — git CLI wrappers used by the dialog and the MCP tools.
// =====================================================================

fn check_branch_protection(work_dir: &Path, branch: &str, op_name: &str) -> Option<SharedString> {
    // Real S-SOL-PRT lookup, run once when the dialog opens. Maps
    // `Forbidden` to a locked-with-reason string the dialog renders next
    // to the disabled force-push toggle. `RequiresConfirmation` does NOT
    // lock the toggle: that tier is answered at the press boundary in
    // `confirm_push`, which re-runs the check and puts up a
    // `PromptLevel::Critical` confirmation naming the branch, the remote
    // and the exact git command (see `force_push_gate`). Deciding it
    // here instead would ask before the user has committed to pushing,
    // and would go stale the moment the policy changed while the dialog
    // sat open. `Allowed` returns `None`.
    match solutions::branch_protection::check(work_dir, branch, op_name) {
        solutions::branch_protection::Decision::Forbidden { reason } => {
            Some(SharedString::from(reason))
        }
        solutions::branch_protection::Decision::RequiresConfirmation { .. }
        | solutions::branch_protection::Decision::Allowed => None,
    }
}

/// What the S-SOL-PRT policy says about the force-push the user just
/// pressed Push on. Kept as a pure function over the [`Decision`] so the
/// tier→UI mapping is testable: the live policy snapshot lives in a
/// process-global cache owned by `solutions` that a `git_ui` test can
/// neither install nor isolate from its neighbours.
///
/// [`Decision`]: solutions::branch_protection::Decision
#[derive(Debug, Clone, PartialEq, Eq)]
enum ForcePushGate {
    /// Nothing to add — run the push.
    Proceed,
    /// `Forbidden`: lock the force toggle with this reason, push nothing.
    Locked { reason: SharedString },
    /// `RequiresConfirmation`: ask first, push only if the user agrees.
    Confirm {
        title: String,
        detail: String,
        confirm_label: String,
    },
}

/// The half of a policy reason that is safe to show in a two-button
/// prompt. `solutions::branch_protection` writes its
/// `RequiresConfirmation` reasons for the MCP `confirmed: true` payload
/// flow, and ends the protected-branch ones with "confirm … by typing
/// the branch name" — an instruction for a type-the-name modal this fork
/// deliberately does not have. Keep the diagnosis, drop the stale
/// instruction; anything else is passed through whole.
fn policy_reason_headline(reason: &str) -> &str {
    match reason.split_once(" — ") {
        Some((head, tail)) if tail.contains("typing the branch name") => head.trim_end(),
        _ => reason,
    }
}

fn force_push_gate(
    decision: &solutions::branch_protection::Decision,
    force_mode: ForceMode,
    branch: &str,
    remote: &str,
    remote_branch: &str,
) -> ForcePushGate {
    use solutions::branch_protection::Decision;

    let force_flag = match force_mode {
        // Not a force push, so this gate has no say: a plain push is
        // `Allowed` by policy and must stay friction-free.
        ForceMode::None => return ForcePushGate::Proceed,
        // Both force postures run the leased form now — see `run_push_cli`.
        ForceMode::WithLease | ForceMode::Force => "--force-with-lease",
    };
    let reason = match decision {
        Decision::Allowed => return ForcePushGate::Proceed,
        Decision::Forbidden { reason } => {
            return ForcePushGate::Locked {
                reason: SharedString::from(reason.clone()),
            };
        }
        Decision::RequiresConfirmation { reason } => policy_reason_headline(reason),
    };
    ForcePushGate::Confirm {
        title: format!("Force-push “{branch}” to “{remote}”?"),
        detail: format!(
            "Branch protection: {reason}. Runs git push {force_flag} {remote} \
             {branch}:{remote_branch}, which overwrites {remote}/{remote_branch} with your \
             local history for everyone using this remote; commits that exist only there \
             cannot be restored from here."
        ),
        confirm_label: format!("Force-push to {remote}"),
    }
}

/// Seconds the confirmation's destructive button stays dead after it
/// appears. The dialog's Push button sits where this one does, so
/// without the delay a double-click on Push lands the second press on
/// "Force-push" — the exact accident the confirmation exists to stop.
const FORCE_PUSH_CONFIRM_DELAY_SECS: u8 = 5;

/// Rows rendered before the list is elided. A force-push that drops
/// hundreds of server-side commits is answered by the count, not by
/// scrolling all of them.
const MAX_OVERWRITTEN_ROWS: usize = 25;

/// Live state of the force-push confirmation. Owns its countdown and its
/// git query, so dropping it (Cancel / Escape) cancels both.
struct ForcePushConfirm {
    title: SharedString,
    detail: SharedString,
    confirm_label: SharedString,
    /// The gated push target, captured at the press boundary so what
    /// finally runs is what the user was shown — not whatever the
    /// remote-branch editor says by the time they answer.
    work_dir: PathBuf,
    branch: String,
    remote: String,
    remote_branch: String,
    state: ForcePushConfirmState,
    _load: Task<()>,
}

/// Where a confirmation is in its life: reading the remote, offering the
/// push, or refusing it outright.
enum ForcePushConfirmState {
    /// Reading what `<remote>/<remote_branch>` holds. No confirm control
    /// exists yet and no countdown runs — there is nothing to count down
    /// to until we know whether the push is offerable at all.
    Pending,
    /// The remote holds work this branch does not: the only state in
    /// which force-pushing means anything, and so the only one with a
    /// countdown and a confirm button.
    Offered {
        commits: Vec<MiniCommit>,
        /// Seconds left before the confirm button arms.
        countdown: u8,
        _countdown: Task<()>,
    },
    /// Refused. The confirm control is not merely disabled, it is absent.
    Refused(ForcePushRefusal),
}

/// Why a force push was refused outright. Both cases are decided from
/// the overwrite list read at the press boundary, so neither can be
/// slipped past by a dialog that has sat open while the world moved.
enum ForcePushRefusal {
    /// The remote holds nothing this branch is missing. Forcing would do
    /// exactly what an ordinary push does, so the flag can only matter
    /// if this reading is wrong — i.e. it can only do harm.
    NothingToOverwrite,
    /// What the server holds could not be read. Refusing beats warning:
    /// a confirmation the user cannot make an informed answer to is not
    /// a safeguard, it is a rubber stamp.
    Undeterminable { why: SharedString },
}

impl ForcePushConfirm {
    /// The destructive button arms only in [`ForcePushConfirmState::Offered`],
    /// and only once the reflex-click window has passed.
    fn is_armed(&self) -> bool {
        matches!(
            self.state,
            ForcePushConfirmState::Offered { countdown: 0, .. }
        )
    }
}

/// The two lines a refusal shows: what is wrong, and what to do instead.
/// Split out of the render so the wording is unit-testable and so the
/// two cases cannot drift into saying the same thing.
fn refusal_lines(
    refusal: &ForcePushRefusal,
    branch: &str,
    tracking: &str,
    remote: &str,
) -> (String, String) {
    match refusal {
        ForcePushRefusal::NothingToOverwrite => (
            format!("Nothing on {tracking} to overwrite — a force push here is pointless."),
            format!(
                "“{branch}” already contains everything {tracking} holds, so forcing can only \
                 differ from an ordinary push if this reading is wrong. Turn force-with-lease \
                 off and press Push; if nothing is ahead either, there is nothing to push."
            ),
        ),
        ForcePushRefusal::Undeterminable { why } => (
            format!("Cannot tell what {tracking} would lose: {why}."),
            format!(
                "Refusing to force-push blind — this dialog will not overwrite history it \
                 could not read. Fetch {remote}, then press Push again."
            ),
        ),
    }
}

/// What `<remote>/<remote_branch>` holds that the local branch does not.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OverwrittenCommits {
    /// Read successfully. Empty means the remote-tracking ref holds
    /// nothing the local branch is missing, as of the last fetch.
    Known(Vec<MiniCommit>),
    /// Could not be determined. Refused rather than warned about: an
    /// empty list would read as "nothing will be lost", and a warning the
    /// user cannot check is not a safeguard.
    Unknown { why: SharedString },
}

/// The commits a force-push would drop on the server: reachable from
/// `<remote>/<remote_branch>` but not from `branch`.
///
/// Deliberately re-derived instead of read off [`PushPreview::behind`]:
/// the preview's ranges are built with `--no-merges` (right for "what am
/// I about to send", wrong for "what am I about to destroy" — a merge
/// commit lost is still work lost), and a preview refresh that failed
/// leaves the previous, possibly empty, vec in place.
async fn overwritten_commits(
    work_dir: &Path,
    branch: &str,
    remote: &str,
    remote_branch: &str,
) -> OverwrittenCommits {
    let tracking = format!("{remote}/{remote_branch}");
    // No tracking ref is ambiguous — a brand-new remote branch looks
    // exactly like one this clone has never fetched — so it counts as
    // undeterminable, not as "nothing there". `--force-with-lease`
    // refuses on the same evidence.
    if let Err(err) = run_git_void(work_dir, &["rev-parse", "--verify", "--quiet", &tracking]).await
    {
        log::info!("PushDialog: no {tracking} ref to compare against: {err:#}");
        return OverwrittenCommits::Unknown {
            why: SharedString::from(format!(
                "this clone has no {tracking} ref, so what the server holds cannot be read \
                 from here — fetch {remote} first"
            )),
        };
    }
    match list_commits_in_range(work_dir, &format!("{branch}..{tracking}"), false).await {
        Ok(commits) => OverwrittenCommits::Known(commits),
        Err(err) => {
            log::warn!("PushDialog: listing {branch}..{tracking} failed: {err:#}");
            OverwrittenCommits::Unknown {
                why: SharedString::from(format!("git log {branch}..{tracking} failed: {err:#}")),
            }
        }
    }
}

/// One overwritten-commit row. Same shape as the dialog's own commit
/// rows (`mini_graph::render_row`) — subject on top, metadata muted
/// underneath — plus the author, since "who loses this work" is the
/// question this list exists to answer.
fn render_overwritten_row(ix: usize, commit: &MiniCommit) -> AnyElement {
    v_flex()
        .id(SharedString::from(format!(
            "push-dialog-overwritten-row-{ix}"
        )))
        .px_2()
        .py_1()
        .gap_0p5()
        .child(
            Label::new(commit.subject.clone())
                .size(LabelSize::Small)
                .truncate(),
        )
        .child(
            h_flex()
                .gap_2()
                .child(
                    Label::new(commit.short_sha())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(crate::mini_graph::format_relative(
                        commit.committer_date_unix,
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
                .child(
                    Label::new(commit.author_email.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        )
        .into_any_element()
}

/// The repository's configured remote names, newest-git-first order
/// preserved. An empty vec means `git remote` itself failed — callers
/// must not read that as "this repository has no remotes".
async fn configured_remotes(work_dir: &Path) -> Vec<SharedString> {
    run_git(work_dir, &["remote"])
        .await
        .map(|output| {
            output
                .lines()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(|name| SharedString::from(name.to_string()))
                .collect()
        })
        .unwrap_or_else(|error| {
            log::warn!("push preview: `git remote` failed, falling back to a first-slash split of the upstream: {error:#}");
            Vec::new()
        })
}

/// Split `<branch>@{upstream}` output into `(remote, remote branch)`.
///
/// A remote name may contain a `/` (`git remote add team/fork …` is
/// legal), so the boundary is resolved against the configured remotes by
/// [`split_remote_ref`] — the crate's one rule for this. The first-slash
/// split is kept **only** for the case where the remote list could not
/// be read at all: it is a guess, but it is a strictly better guess than
/// giving up on the upstream entirely.
fn split_upstream(upstream: &str, configured_remotes: &[SharedString]) -> Option<(String, String)> {
    if let Some((remote, remote_branch)) = split_remote_ref(upstream, configured_remotes) {
        return Some((remote.to_string(), remote_branch.to_string()));
    }
    if !configured_remotes.is_empty() {
        return None;
    }
    upstream
        .split_once('/')
        .map(|(remote, remote_branch)| (remote.to_string(), remote_branch.to_string()))
}

/// Build a `PushPreview` for the given branch by invoking git directly.
/// `remote_override` allows the dialog's remote-branch input to influence
/// which upstream we compare against; falls back to the configured
/// upstream when empty.
pub async fn build_preview(
    work_dir: &Path,
    branch: &str,
    remote_override: &str,
) -> Result<PushPreview> {
    let upstream = run_git(
        work_dir,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            &format!("{branch}@{{upstream}}"),
        ],
    )
    .await
    .ok()
    .map(|s| s.trim().to_string());

    let configured_remotes = configured_remotes(work_dir).await;

    let (remote, remote_branch_default, will_create) = match upstream {
        Some(upstream_str) if !upstream_str.is_empty() => {
            match split_upstream(&upstream_str, &configured_remotes) {
                Some((remote, rb)) => (remote, rb, false),
                None => ("origin".into(), branch.to_string(), false),
            }
        }
        _ => ("origin".into(), branch.to_string(), true),
    };

    let remote_branch = if remote_override.trim().is_empty() {
        remote_branch_default
    } else {
        remote_override.trim().to_string()
    };
    let upstream_full = format!("{remote}/{remote_branch}");
    let remote_ref_exists = run_git_void(
        work_dir,
        &["rev-parse", "--verify", "--quiet", &upstream_full],
    )
    .await
    .is_ok();

    let (ahead, behind) = if remote_ref_exists {
        let ahead = list_commits(work_dir, &format!("{upstream_full}..{branch}")).await?;
        let behind = list_commits(work_dir, &format!("{branch}..{upstream_full}")).await?;
        (ahead, behind)
    } else {
        let ahead = list_commits(work_dir, branch).await.unwrap_or_default();
        let ahead: Vec<MiniCommit> = ahead.into_iter().take(200).collect();
        (ahead, Vec::new())
    };

    Ok(PushPreview {
        branch: branch.to_string(),
        remote,
        remote_branch,
        ahead,
        behind,
        will_create_remote_branch: will_create || !remote_ref_exists,
    })
}

async fn list_commits(work_dir: &Path, range: &str) -> Result<Vec<MiniCommit>> {
    list_commits_in_range(work_dir, range, true).await
}

/// `git log <range>` as [`MiniCommit`]s. `skip_merges` is what the push
/// preview wants ("what am I about to send") but never what the
/// force-push confirmation wants ("what am I about to destroy") — a
/// merge commit that only exists on the server is still work lost.
async fn list_commits_in_range(
    work_dir: &Path,
    range: &str,
    skip_merges: bool,
) -> Result<Vec<MiniCommit>> {
    let mut args = vec!["log"];
    if skip_merges {
        args.push("--no-merges");
    }
    args.push("--pretty=format:%H%x09%s%x09%ae%x09%ct");
    args.push(range);
    let raw = run_git(work_dir, &args).await?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let mut cols = line.splitn(4, '\t');
        let sha = cols.next().unwrap_or("").to_string();
        if sha.is_empty() {
            continue;
        }
        let subject = cols.next().unwrap_or("").to_string();
        let author_email = cols.next().unwrap_or("").to_string();
        let ts: i64 = cols.next().unwrap_or("0").parse().unwrap_or(0);
        out.push(MiniCommit {
            sha,
            subject,
            author_email,
            committer_date_unix: ts,
        });
    }
    Ok(out)
}

async fn commit_file_summary(work_dir: &Path, sha: &str) -> Result<Vec<DiffFileSummary>> {
    let numstat = run_git(work_dir, &["show", "--numstat", "--format=", sha]).await?;
    let namestatus = run_git(work_dir, &["show", "--name-status", "--format=", sha]).await?;
    let mut files = Vec::new();
    let mut status_map = std::collections::HashMap::new();
    for line in namestatus.lines() {
        let mut cols = line.splitn(2, '\t');
        let status = cols.next().unwrap_or("").to_string();
        let path = cols.next().unwrap_or("").to_string();
        if path.is_empty() {
            continue;
        }
        status_map.insert(path, status);
    }
    for line in numstat.lines() {
        let mut cols = line.splitn(3, '\t');
        let additions: u32 = cols.next().unwrap_or("0").parse().unwrap_or(0);
        let deletions: u32 = cols.next().unwrap_or("0").parse().unwrap_or(0);
        let path = cols.next().unwrap_or("").to_string();
        if path.is_empty() {
            continue;
        }
        let status = status_map
            .get(&path)
            .cloned()
            .unwrap_or_else(|| "M".to_string());
        files.push(DiffFileSummary {
            path,
            status,
            additions,
            deletions,
        });
    }
    Ok(files)
}

/// Returns the list of remote refs that contain `sha` ("origin/main",
/// "upstream/dev", etc.) for the soft pre-edit guard.
pub async fn commit_remote_refs(work_dir: &Path, sha: &str) -> Result<Vec<String>> {
    let raw = run_git(work_dir, &["branch", "-r", "--contains", sha]).await?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((primary, _alias)) = trimmed.split_once(" -> ") {
            out.push(primary.to_string());
        } else {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

async fn run_push_cli(
    work_dir: &Path,
    branch: &str,
    remote: &str,
    remote_branch: &str,
    opts: &PushInvocation,
) -> Result<RemoteCommandOutput> {
    if opts.pull_rebase_first {
        run_git_void(work_dir, &["pull", "--rebase", remote, remote_branch])
            .await
            .context("pull --rebase before push")?;
    }
    let mut args: Vec<String> = vec!["push".into()];
    if opts.no_verify {
        args.push("--no-verify".into());
    }
    if opts.tags {
        args.push("--tags".into());
    }
    if opts.set_upstream {
        args.push("--set-upstream".into());
    }
    match opts.force_mode {
        ForceMode::None => {}
        // A bare `--force` is not offered here any more, and the legacy
        // variant is upgraded rather than rejected: `--force-with-lease`
        // does everything `--force` does *except* silently overwrite a
        // remote that moved since the last fetch, which is the only
        // difference and the whole accident. A caller that hits the
        // lease's "stale info" refusal has lost nothing and can fetch.
        ForceMode::WithLease | ForceMode::Force => args.push("--force-with-lease".into()),
    }
    args.push(remote.into());
    args.push(format!("{branch}:{remote_branch}"));
    let work_dir_buf: PathBuf = work_dir.to_path_buf();
    let mut command = new_command("git");
    command.current_dir(&work_dir_buf);
    command.args(args.iter().map(|s| s.as_str()));
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await.context("running `git push`")?;
    let stdout = String::from_utf8(output.stdout).unwrap_or_default();
    let stderr = String::from_utf8(output.stderr).unwrap_or_default();
    if !output.status.success() {
        return Err(anyhow!("git push failed: {}", stderr.trim_end()));
    }
    Ok(RemoteCommandOutput { stdout, stderr })
}

/// Invocation used by the `editor.git.push_force_with_lease` MCP tool.
/// When `expected_remote_sha` is `Some`, the lease is pinned to that
/// value via `--force-with-lease=<branch>:<sha>`, so git refuses if the
/// remote moved between preview and push. When `None`, falls back to
/// plain `--force-with-lease` (git auto-detects).
pub async fn run_force_with_lease(
    work_dir: &Path,
    branch: &str,
    remote: &str,
    remote_branch: &str,
    expected_remote_sha: Option<&str>,
    set_upstream: bool,
    tags: bool,
    no_verify: bool,
) -> Result<RemoteCommandOutput> {
    let mut args: Vec<String> = vec!["push".into()];
    if no_verify {
        args.push("--no-verify".into());
    }
    if tags {
        args.push("--tags".into());
    }
    if set_upstream {
        args.push("--set-upstream".into());
    }
    let lease = match expected_remote_sha {
        Some(sha) => format!("--force-with-lease={remote_branch}:{sha}"),
        None => "--force-with-lease".into(),
    };
    args.push(lease);
    args.push(remote.into());
    args.push(format!("{branch}:{remote_branch}"));
    let work_dir_buf: PathBuf = work_dir.to_path_buf();
    let mut command = new_command("git");
    command.current_dir(&work_dir_buf);
    command.args(args.iter().map(|s| s.as_str()));
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await.context("running `git push`")?;
    let stdout = String::from_utf8(output.stdout).unwrap_or_default();
    let stderr = String::from_utf8(output.stderr).unwrap_or_default();
    if !output.status.success() {
        return Err(anyhow!("git push failed: {}", stderr.trim_end()));
    }
    Ok(RemoteCommandOutput { stdout, stderr })
}

/// Invocation used by `editor.git.push` and `editor.git.push_force` —
/// just plain `git push` with the named flags. `force` adds
/// `--force-with-lease`, never a bare `--force`.
pub async fn run_plain_push(
    work_dir: &Path,
    branch: &str,
    remote: &str,
    remote_branch: &str,
    set_upstream: bool,
    tags: bool,
    no_verify: bool,
    force: bool,
) -> Result<RemoteCommandOutput> {
    let mut args: Vec<String> = vec!["push".into()];
    if no_verify {
        args.push("--no-verify".into());
    }
    if tags {
        args.push("--tags".into());
    }
    if set_upstream {
        args.push("--set-upstream".into());
    }
    if force {
        // Upgraded, not honoured verbatim: see `run_push_cli`. The wire
        // tool that sets this flag (`editor.git.push_force`) documents
        // the upgrade, so a subagent can't reintroduce the bare flag by
        // going around the dialog.
        args.push("--force-with-lease".into());
    }
    args.push(remote.into());
    args.push(format!("{branch}:{remote_branch}"));
    let work_dir_buf: PathBuf = work_dir.to_path_buf();
    let mut command = new_command("git");
    command.current_dir(&work_dir_buf);
    command.args(args.iter().map(|s| s.as_str()));
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await.context("running `git push`")?;
    let stdout = String::from_utf8(output.stdout).unwrap_or_default();
    let stderr = String::from_utf8(output.stderr).unwrap_or_default();
    if !output.status.success() {
        return Err(anyhow!("git push failed: {}", stderr.trim_end()));
    }
    Ok(RemoteCommandOutput { stdout, stderr })
}

/// Resolve the current branch name without going through `Repository`.
/// Used by MCP tools that operate on `work_dir` only.
pub async fn current_branch(work_dir: &Path) -> Result<String> {
    let raw = run_git(work_dir, &["symbolic-ref", "--short", "-q", "HEAD"]).await?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Err(anyhow!("HEAD is detached"))
    } else {
        Ok(trimmed.to_string())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use tempfile::TempDir;
    use util::path;
    use workspace::MultiWorkspace;

    /// Boots a tiny temp repo with a remote so the preview-builder has
    /// real `<remote>..<branch>` ranges to count.
    async fn boot_repo() -> Result<(TempDir, PathBuf, PathBuf)> {
        let tmp = TempDir::new()?;
        let local = tmp.path().join("local");
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&local)?;
        std::fs::create_dir_all(&remote)?;
        run_git_void(&remote, &["init", "--bare", "-b", "main"]).await?;
        run_git_void(&local, &["init", "-b", "main"]).await?;
        run_git_void(&local, &["config", "user.email", "test@example.com"]).await?;
        run_git_void(&local, &["config", "user.name", "Test"]).await?;
        std::fs::write(local.join("README"), "hello")?;
        run_git_void(&local, &["add", "README"]).await?;
        run_git_void(&local, &["commit", "-m", "init"]).await?;
        run_git_void(
            &local,
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().unwrap_or_default(),
            ],
        )
        .await?;
        run_git_void(&local, &["push", "-u", "origin", "main"]).await?;
        Ok((tmp, local, remote))
    }

    /// Same shape as [`boot_repo`], but the remote is named
    /// `team/fork` — legal in git, and exactly the case a first-slash
    /// split gets wrong (it would name a remote called `team`).
    async fn boot_repo_with_slashed_remote() -> Result<(TempDir, PathBuf)> {
        let tmp = TempDir::new()?;
        let local = tmp.path().join("local");
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&local)?;
        std::fs::create_dir_all(&remote)?;
        run_git_void(&remote, &["init", "--bare", "-b", "main"]).await?;
        run_git_void(&local, &["init", "-b", "main"]).await?;
        run_git_void(&local, &["config", "user.email", "test@example.com"]).await?;
        run_git_void(&local, &["config", "user.name", "Test"]).await?;
        std::fs::write(local.join("README"), "hello")?;
        run_git_void(&local, &["add", "README"]).await?;
        run_git_void(&local, &["commit", "-m", "init"]).await?;
        run_git_void(
            &local,
            &[
                "remote",
                "add",
                "team/fork",
                remote.to_str().unwrap_or_default(),
            ],
        )
        .await?;
        run_git_void(&local, &["push", "-u", "team/fork", "main"]).await?;
        Ok((tmp, local))
    }

    #[gpui::test]
    async fn preview_resolves_a_slash_bearing_remote(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (_tmp, local) = boot_repo_with_slashed_remote()
            .await
            .unwrap_or_else(|e| panic!("boot: {e}"));

        let preview = build_preview(&local, "main", "")
            .await
            .unwrap_or_else(|e| panic!("preview: {e}"));

        assert_eq!(
            preview.remote, "team/fork",
            "splitting the upstream on the first slash names a remote (“team”) that does not exist"
        );
        assert_eq!(preview.remote_branch, "main");
        assert!(!preview.will_create_remote_branch);
        assert_eq!(preview.ahead.len(), 0);
        assert_eq!(preview.behind.len(), 0);
    }

    fn remote_names(names: &[&str]) -> Vec<SharedString> {
        names
            .iter()
            .map(|name| SharedString::from(name.to_string()))
            .collect()
    }

    #[test]
    fn split_upstream_resolves_against_the_configured_remotes() {
        assert_eq!(
            split_upstream("team/fork/main", &remote_names(&["origin", "team/fork"])),
            Some(("team/fork".to_string(), "main".to_string()))
        );
        assert_eq!(
            split_upstream("origin/feature/FOO-1", &remote_names(&["origin"])),
            Some(("origin".to_string(), "feature/FOO-1".to_string()))
        );
    }

    /// An upstream no configured remote claims is not silently split at
    /// the first slash — that is how a preview ends up comparing against
    /// a ref that cannot exist.
    #[test]
    fn split_upstream_refuses_an_unclaimed_upstream() {
        assert_eq!(
            split_upstream("team/fork/main", &remote_names(&["origin"])),
            None
        );
    }

    /// The one case the first-slash guess survives: `git remote` itself
    /// failed, so there is no list to match against and a guess beats
    /// discarding the upstream.
    #[test]
    fn split_upstream_falls_back_to_the_first_slash_without_a_remote_list() {
        assert_eq!(
            split_upstream("origin/main", &[]),
            Some(("origin".to_string(), "main".to_string()))
        );
    }

    #[gpui::test]
    async fn preview_no_divergence(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (_tmp, local, _remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));
        let preview = build_preview(&local, "main", "")
            .await
            .unwrap_or_else(|e| panic!("preview: {e}"));
        assert_eq!(preview.ahead.len(), 0);
        assert_eq!(preview.behind.len(), 0);
        assert!(!preview.divergence());
        assert_eq!(preview.remote, "origin");
        assert_eq!(preview.remote_branch, "main");
        assert!(!preview.will_create_remote_branch);
    }

    #[gpui::test]
    async fn preview_local_ahead(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (_tmp, local, _remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));
        std::fs::write(local.join("a.txt"), "a").expect("write a");
        run_git_void(&local, &["add", "a.txt"])
            .await
            .expect("add a");
        run_git_void(&local, &["commit", "-m", "add a"])
            .await
            .expect("commit a");
        std::fs::write(local.join("b.txt"), "b").expect("write b");
        run_git_void(&local, &["add", "b.txt"])
            .await
            .expect("add b");
        run_git_void(&local, &["commit", "-m", "add b"])
            .await
            .expect("commit b");
        let preview = build_preview(&local, "main", "")
            .await
            .unwrap_or_else(|e| panic!("preview: {e}"));
        assert_eq!(preview.ahead.len(), 2);
        assert_eq!(preview.behind.len(), 0);
        assert!(!preview.divergence());
        assert_eq!(preview.ahead[0].subject, "add b");
        assert_eq!(preview.ahead[1].subject, "add a");
    }

    #[gpui::test]
    async fn preview_divergence_detection(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (_tmp, local, remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));
        let other = local
            .parent()
            .expect("parent of local exists")
            .join("other");
        std::fs::create_dir_all(&other).expect("mkdir other");
        run_git_void(&other, &["clone", remote.to_str().unwrap_or_default(), "."])
            .await
            .expect("clone");
        run_git_void(&other, &["config", "user.email", "test@example.com"])
            .await
            .expect("config email");
        run_git_void(&other, &["config", "user.name", "Test"])
            .await
            .expect("config name");
        std::fs::write(other.join("from-other.txt"), "hi").expect("write from-other");
        run_git_void(&other, &["add", "from-other.txt"])
            .await
            .expect("add");
        run_git_void(&other, &["commit", "-m", "from other"])
            .await
            .expect("commit");
        run_git_void(&other, &["push", "origin", "main"])
            .await
            .expect("push");

        std::fs::write(local.join("local-only.txt"), "x").expect("write local-only");
        run_git_void(&local, &["add", "local-only.txt"])
            .await
            .expect("add");
        run_git_void(&local, &["commit", "-m", "local commit"])
            .await
            .expect("commit");
        run_git_void(&local, &["fetch", "origin"])
            .await
            .expect("fetch");

        let preview = build_preview(&local, "main", "")
            .await
            .unwrap_or_else(|e| panic!("preview: {e}"));
        assert_eq!(preview.ahead.len(), 1);
        assert_eq!(preview.behind.len(), 1);
        assert!(preview.divergence());
    }

    #[gpui::test]
    async fn preview_handles_new_remote_branch(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (_tmp, local, _remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));
        run_git_void(&local, &["checkout", "-b", "feature"])
            .await
            .expect("checkout feature");
        std::fs::write(local.join("f.txt"), "f").expect("write f");
        run_git_void(&local, &["add", "f.txt"]).await.expect("add");
        run_git_void(&local, &["commit", "-m", "feature"])
            .await
            .expect("commit");
        let preview = build_preview(&local, "feature", "")
            .await
            .unwrap_or_else(|e| panic!("preview: {e}"));
        assert!(preview.will_create_remote_branch);
        assert!(!preview.ahead.is_empty());
    }

    #[gpui::test]
    async fn force_with_lease_rejects_stale_sha(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (_tmp, local, remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));
        let other = local
            .parent()
            .expect("parent of local exists")
            .join("other2");
        std::fs::create_dir_all(&other).expect("mkdir other2");
        run_git_void(&other, &["clone", remote.to_str().unwrap_or_default(), "."])
            .await
            .expect("clone");
        run_git_void(&other, &["config", "user.email", "test@example.com"])
            .await
            .expect("config email");
        run_git_void(&other, &["config", "user.name", "Test"])
            .await
            .expect("config name");

        let stale_sha = run_git(&local, &["rev-parse", "origin/main"])
            .await
            .expect("rev-parse origin");
        let stale_sha = stale_sha.trim().to_string();

        std::fs::write(local.join("local.txt"), "x").expect("write local.txt");
        run_git_void(&local, &["add", "local.txt"])
            .await
            .expect("add");
        run_git_void(&local, &["commit", "-m", "local"])
            .await
            .expect("commit");

        std::fs::write(other.join("other.txt"), "y").expect("write other.txt");
        run_git_void(&other, &["add", "other.txt"])
            .await
            .expect("add");
        run_git_void(&other, &["commit", "-m", "remote moved"])
            .await
            .expect("commit");
        run_git_void(&other, &["push", "origin", "main"])
            .await
            .expect("push");

        let result = run_force_with_lease(
            &local,
            "main",
            "origin",
            "main",
            Some(&stale_sha),
            false,
            false,
            false,
        )
        .await;
        assert!(result.is_err(), "stale lease should fail: {result:?}");
    }

    #[gpui::test]
    async fn commit_remote_refs_finds_origin(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (_tmp, local, _remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));
        let head = run_git(&local, &["rev-parse", "HEAD"])
            .await
            .expect("rev-parse HEAD");
        let refs = commit_remote_refs(&local, head.trim())
            .await
            .expect("commit_remote_refs");
        assert!(refs.iter().any(|r| r == "origin/main"));
    }

    /// Reproduces the reported bug: someone else pushed first, so our push
    /// is rejected. Asserts git's own words survive into the error that the
    /// dialog renders, and that the failure classifies as recoverable by a
    /// pull (which is what gates the remediation buttons).
    async fn diverge_remote(local: &Path, remote: &Path) -> Result<()> {
        let other = local
            .parent()
            .context("parent of local exists")?
            .join("other-pusher");
        std::fs::create_dir_all(&other)?;
        run_git_void(&other, &["clone", remote.to_str().unwrap_or_default(), "."]).await?;
        run_git_void(&other, &["config", "user.email", "test@example.com"]).await?;
        run_git_void(&other, &["config", "user.name", "Test"]).await?;
        std::fs::write(other.join("from-other.txt"), "hi")?;
        run_git_void(&other, &["add", "from-other.txt"]).await?;
        run_git_void(&other, &["commit", "-m", "from other"]).await?;
        run_git_void(&other, &["push", "origin", "main"]).await?;

        std::fs::write(local.join("local-only.txt"), "x")?;
        run_git_void(local, &["add", "local-only.txt"]).await?;
        run_git_void(local, &["commit", "-m", "local commit"]).await?;
        Ok(())
    }

    fn plain_push() -> PushInvocation {
        PushInvocation {
            force_mode: ForceMode::None,
            tags: false,
            no_verify: false,
            set_upstream: false,
            pull_rebase_first: false,
        }
    }

    #[gpui::test]
    async fn rejected_push_surfaces_git_message_verbatim(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (_tmp, local, remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));
        diverge_remote(&local, &remote)
            .await
            .unwrap_or_else(|e| panic!("diverge: {e}"));

        let err = run_push_cli(&local, "main", "origin", "main", &plain_push())
            .await
            .expect_err("push to a diverged remote must fail");
        let failure = PushFailure::from_error(&err);

        assert!(
            failure.detail.contains("[rejected]"),
            "git's rejection line must reach the UI verbatim, got: {}",
            failure.detail
        );
        assert!(
            failure.detail.contains("non-fast-forward") || failure.detail.contains("fetch first"),
            "git's reason must reach the UI verbatim, got: {}",
            failure.detail
        );
        assert_eq!(failure.kind, PushRejection::NonFastForward);
        assert!(
            failure.kind.is_diverged(),
            "a non-fast-forward rejection must offer the pull remediations"
        );
    }

    #[gpui::test]
    async fn pull_rebase_resolves_the_rejection(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (_tmp, local, remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));
        diverge_remote(&local, &remote)
            .await
            .unwrap_or_else(|e| panic!("diverge: {e}"));
        run_push_cli(&local, "main", "origin", "main", &plain_push())
            .await
            .expect_err("push to a diverged remote must fail");

        let opts = PushInvocation {
            pull_rebase_first: true,
            ..plain_push()
        };
        run_push_cli(&local, "main", "origin", "main", &opts)
            .await
            .unwrap_or_else(|e| panic!("push after rebase should succeed: {e:#}"));

        let log = run_git(&local, &["log", "--oneline", "origin/main"])
            .await
            .expect("log origin/main");
        assert!(log.contains("local commit"), "log was: {log}");
        assert!(log.contains("from other"), "log was: {log}");
    }

    #[gpui::test]
    async fn force_with_lease_resolves_the_rejection(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (_tmp, local, remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));
        diverge_remote(&local, &remote)
            .await
            .unwrap_or_else(|e| panic!("diverge: {e}"));
        run_push_cli(&local, "main", "origin", "main", &plain_push())
            .await
            .expect_err("push to a diverged remote must fail");

        // The lease is only valid once we know where the remote actually is;
        // before a fetch git reports `(stale info)` rather than overwriting.
        let stale = run_push_cli(
            &local,
            "main",
            "origin",
            "main",
            &PushInvocation {
                force_mode: ForceMode::WithLease,
                ..plain_push()
            },
        )
        .await
        .expect_err("force-with-lease before a fetch must be refused");
        assert_eq!(
            PushFailure::from_error(&stale).kind,
            PushRejection::StaleInfo
        );

        run_git_void(&local, &["fetch", "origin"])
            .await
            .expect("fetch");
        run_push_cli(
            &local,
            "main",
            "origin",
            "main",
            &PushInvocation {
                force_mode: ForceMode::WithLease,
                ..plain_push()
            },
        )
        .await
        .unwrap_or_else(|e| panic!("force-with-lease after fetch should succeed: {e:#}"));

        let log = run_git(&local, &["log", "--oneline", "origin/main"])
            .await
            .expect("log origin/main");
        assert!(log.contains("local commit"), "log was: {log}");
        assert!(!log.contains("from other"), "log was: {log}");
    }

    // =================================================================
    //  S-SOL-PRT — the force-push confirmation.
    // =================================================================

    fn requires_confirmation(reason: &str) -> solutions::branch_protection::Decision {
        solutions::branch_protection::Decision::RequiresConfirmation {
            reason: reason.to_string(),
        }
    }

    /// A plain push is `Allowed` by policy and must stay friction-free —
    /// the gate has nothing to say about it even when the branch is one
    /// the policy guards.
    #[test]
    fn a_plain_push_is_never_gated() {
        assert_eq!(
            force_push_gate(
                &requires_confirmation(
                    "'main' is protected — confirm force-push by typing the branch name"
                ),
                ForceMode::None,
                "main",
                "origin",
                "main",
            ),
            ForcePushGate::Proceed
        );
    }

    #[test]
    fn an_allowed_force_push_is_never_gated() {
        assert_eq!(
            force_push_gate(
                &solutions::branch_protection::Decision::Allowed,
                ForceMode::WithLease,
                "main",
                "origin",
                "main",
            ),
            ForcePushGate::Proceed
        );
    }

    /// `Forbidden` keeps doing exactly what it did before the
    /// confirmation existed: lock the toggle with the policy's reason and
    /// push nothing. It must never be downgraded into a question.
    #[test]
    fn a_forbidden_force_push_locks_instead_of_asking() {
        let gate = force_push_gate(
            &solutions::branch_protection::Decision::Forbidden {
                reason: "force-push to protected branch 'main' is forbidden".to_string(),
            },
            ForceMode::WithLease,
            "main",
            "origin",
            "main",
        );
        assert_eq!(
            gate,
            ForcePushGate::Locked {
                reason: SharedString::from("force-push to protected branch 'main' is forbidden"),
            }
        );
    }

    #[test]
    fn a_confirmable_force_push_names_the_branch_remote_and_command() {
        let gate = force_push_gate(
            &requires_confirmation("force-push to 'wip' rewrites remote history"),
            ForceMode::WithLease,
            "wip",
            "origin",
            "wip-remote",
        );
        let ForcePushGate::Confirm {
            title,
            detail,
            confirm_label,
        } = gate
        else {
            panic!("the middle tier must ask, got: {gate:?}");
        };
        assert_eq!(title, "Force-push “wip” to “origin”?");
        assert_eq!(confirm_label, "Force-push to origin");
        assert!(
            detail.contains("git push --force-with-lease origin wip:wip-remote"),
            "the confirmation must name the exact command, got: {detail}"
        );
        assert!(
            detail.contains("force-push to 'wip' rewrites remote history"),
            "the policy's own reason must survive into the prompt, got: {detail}"
        );
    }

    /// The legacy bare-`--force` posture is upgraded, so it must never
    /// advertise a flag the runner will not use.
    #[test]
    fn a_legacy_force_posture_is_described_as_leased() {
        let gate = force_push_gate(
            &requires_confirmation("force-push to 'wip' rewrites remote history"),
            ForceMode::Force,
            "wip",
            "origin",
            "wip",
        );
        let ForcePushGate::Confirm { detail, .. } = gate else {
            panic!("the middle tier must ask, got: {gate:?}");
        };
        assert!(detail.contains("--force-with-lease"), "got: {detail}");
        assert!(
            !detail.contains("git push --force "),
            "a bare --force must not be advertised: {detail}"
        );
    }

    /// The policy writes its reasons for the MCP `confirmed: true` flow
    /// and ends the protected-branch ones with an instruction for a
    /// type-the-name modal this fork does not have. Showing that verbatim
    /// next to two buttons would tell the user to do something
    /// impossible.
    #[test]
    fn the_confirmation_drops_the_type_the_name_instruction() {
        assert_eq!(
            policy_reason_headline(
                "'main' is protected — confirm force-push by typing the branch name"
            ),
            "'main' is protected"
        );
        assert_eq!(
            policy_reason_headline("force-push to 'wip' rewrites remote history"),
            "force-push to 'wip' rewrites remote history"
        );
        assert_eq!(
            policy_reason_headline("'main' is protected — ask the release owner"),
            "'main' is protected — ask the release owner",
            "an unrecognised tail is information, not boilerplate: keep it"
        );
    }

    fn confirm_with(state: ForcePushConfirmState) -> ForcePushConfirm {
        ForcePushConfirm {
            title: "Force-push “main” to “origin”?".into(),
            detail: "".into(),
            confirm_label: "Force-push to origin".into(),
            work_dir: PathBuf::from("/dir"),
            branch: "main".into(),
            remote: "origin".into(),
            remote_branch: "main".into(),
            state,
            _load: Task::ready(()),
        }
    }

    fn offered(countdown: u8) -> ForcePushConfirmState {
        ForcePushConfirmState::Offered {
            commits: vec![MiniCommit {
                sha: "deadbeefcafe".into(),
                subject: "server work".into(),
                author_email: "them@example.com".into(),
                committer_date_unix: 1_700_000_000,
            }],
            countdown,
            _countdown: Task::ready(()),
        }
    }

    /// Updated from the earlier version of this test, which let an
    /// undeterminable list arm the button once the countdown ran out.
    /// Neither refusal state is armable at all now.
    #[test]
    fn the_confirm_control_arms_only_for_an_offered_push() {
        assert!(
            !confirm_with(offered(1)).is_armed(),
            "the reflex-click window must gate the button"
        );
        assert!(confirm_with(offered(0)).is_armed());
        assert!(
            !confirm_with(ForcePushConfirmState::Pending).is_armed(),
            "confirming before the overwrite list resolves is confirming blind"
        );
        assert!(
            !confirm_with(ForcePushConfirmState::Refused(
                ForcePushRefusal::NothingToOverwrite
            ))
            .is_armed(),
            "a force push that overwrites nothing must be unreachable, not merely slow"
        );
        assert!(
            !confirm_with(ForcePushConfirmState::Refused(
                ForcePushRefusal::Undeterminable {
                    why: "no tracking ref".into()
                }
            ))
            .is_armed(),
            "a force push we cannot describe must be unreachable, not merely warned about"
        );
    }

    /// The two refusals must not read alike: one says "you do not need
    /// this", the other says "we could not look".
    #[test]
    fn the_two_refusals_say_different_things() {
        let (pointless_head, pointless_advice) = refusal_lines(
            &ForcePushRefusal::NothingToOverwrite,
            "main",
            "origin/main",
            "origin",
        );
        assert!(
            pointless_head.contains("origin/main") && pointless_head.contains("pointless"),
            "got: {pointless_head}"
        );
        assert!(
            pointless_advice.contains("force-with-lease") && pointless_advice.contains("Push"),
            "the empty case must point at the ordinary push: {pointless_advice}"
        );
        assert!(
            pointless_advice.contains("nothing to push"),
            "and at the case where there is nothing to send at all: {pointless_advice}"
        );

        let (blind_head, blind_advice) = refusal_lines(
            &ForcePushRefusal::Undeterminable {
                why: "this clone has no origin/main ref".into(),
            },
            "main",
            "origin/main",
            "origin",
        );
        assert!(
            blind_head.contains("Cannot tell") && blind_head.contains("no origin/main ref"),
            "got: {blind_head}"
        );
        assert!(
            blind_advice.contains("Fetch origin"),
            "the unknown case must send the user to fetch: {blind_advice}"
        );
        assert_ne!(pointless_head, blind_head);
        assert_ne!(pointless_advice, blind_advice);
    }

    fn init_ui_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    /// A dialog on a fake project, built field-by-field rather than
    /// through [`PushDialog::open`]: the press boundary is what is under
    /// test, and `open`'s async preview refresh would drag a real `git`
    /// invocation into every assertion.
    async fn init_push_dialog(
        force_mode: ForceMode,
        cx: &mut TestAppContext,
    ) -> (Entity<PushDialog>, VisualTestContext) {
        init_ui_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                ".git": {},
                "file.txt": "hello".to_string()
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let repository = cx
            .read(|cx| project.read(cx).active_repository(cx))
            .expect("fake project should expose a repository");
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");

        let dialog = window_handle
            .update(cx, |_, window, cx| {
                cx.new(|cx| {
                    let editor = cx.new(|cx| {
                        let mut editor = Editor::single_line(window, cx);
                        editor.set_text("main", window, cx);
                        editor
                    });
                    PushDialog {
                        workspace: workspace.downgrade(),
                        repository: repository.clone(),
                        work_dir: PathBuf::from(path!("/dir")),
                        branch: "main".into(),
                        remote: "origin".into(),
                        remote_branch_editor: editor,
                        preview: PushPreview::default(),
                        selected_commit: None,
                        selected_files: Vec::new(),
                        force_mode,
                        push_tags: false,
                        no_verify: false,
                        pull_rebase_first: false,
                        force_locked_reason: None,
                        force_confirm: None,
                        pushing: false,
                        refreshing: false,
                        failure: None,
                        notice: None,
                        remediation: None,
                        focus_handle: cx.focus_handle(),
                    }
                })
            })
            .expect("window should be open");

        (
            dialog,
            VisualTestContext::from_window(window_handle.into(), cx),
        )
    }

    #[gpui::test]
    async fn pressing_push_on_a_force_push_opens_the_confirmation(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (dialog, mut cx) = init_push_dialog(ForceMode::WithLease, cx).await;

        dialog.update_in(&mut cx, |dialog, window, cx| {
            dialog.confirm_push(window, cx)
        });

        dialog.update(&mut cx, |dialog, _| {
            let confirm = dialog
                .force_confirm
                .as_ref()
                .expect("a force push must be confirmed before it runs");
            assert!(
                confirm.title.contains("main") && confirm.title.contains("origin"),
                "the confirmation must name the branch and the remote, got: {}",
                confirm.title
            );
            assert!(
                confirm
                    .detail
                    .contains("git push --force-with-lease origin main:main"),
                "got: {}",
                confirm.detail
            );
            assert!(
                !dialog.pushing,
                "nothing may be pushed until the user answers"
            );
        });
    }

    #[gpui::test]
    async fn a_plain_push_opens_no_confirmation(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (dialog, mut cx) = init_push_dialog(ForceMode::None, cx).await;

        dialog.update_in(&mut cx, |dialog, window, cx| {
            dialog.confirm_push(window, cx)
        });

        dialog.update(&mut cx, |dialog, _| {
            assert!(
                dialog.force_confirm.is_none(),
                "a plain push must stay friction-free"
            );
            assert!(dialog.pushing, "it should have gone straight to the push");
        });
    }

    #[gpui::test]
    async fn cancelling_the_confirmation_aborts_the_push(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (dialog, mut cx) = init_push_dialog(ForceMode::WithLease, cx).await;

        dialog.update_in(&mut cx, |dialog, window, cx| {
            dialog.confirm_push(window, cx)
        });
        dialog.update_in(&mut cx, |dialog, window, cx| {
            // Escape, the same path the Cancel button takes.
            dialog.cancel(&Cancel, window, cx)
        });

        dialog.update(&mut cx, |dialog, _| {
            assert!(dialog.force_confirm.is_none(), "the confirmation is gone");
            assert!(!dialog.pushing, "cancelling must not push");
        });
        cx.run_until_parked();
        dialog.update(&mut cx, |dialog, _| {
            assert!(
                !dialog.pushing,
                "and it must not push once the timers drain either"
            );
        });
    }

    fn server_commit(subject: &str) -> MiniCommit {
        MiniCommit {
            sha: "0badc0ffee11".into(),
            subject: subject.into(),
            author_email: "them@example.com".into(),
            committer_date_unix: 1_700_000_000,
        }
    }

    /// The confirm control sits where the Push button was, so a
    /// double-click on Push would otherwise land its second press on the
    /// force-push. The countdown is the guard.
    ///
    /// Drives the overwrite list in by hand rather than waiting on the
    /// real `git` read `open_force_push_confirm` starts: this is the
    /// state machine's test, and `resolve_overwritten` is the same entry
    /// point the load task uses.
    #[gpui::test]
    async fn the_confirm_control_stays_dead_for_the_countdown(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (dialog, mut cx) = init_push_dialog(ForceMode::WithLease, cx).await;

        dialog.update_in(&mut cx, |dialog, window, cx| {
            dialog.confirm_push(window, cx)
        });
        dialog.update(&mut cx, |dialog, _| {
            let confirm = dialog.force_confirm.as_ref().expect("confirmation is up");
            assert!(
                matches!(confirm.state, ForcePushConfirmState::Pending),
                "it must not offer anything before it knows what is at stake"
            );
            assert!(!confirm.is_armed());
        });

        dialog.update(&mut cx, |dialog, cx| {
            dialog.resolve_overwritten(
                OverwrittenCommits::Known(vec![server_commit("server work")]),
                cx,
            )
        });
        dialog.update(&mut cx, |dialog, _| {
            let confirm = dialog.force_confirm.as_ref().expect("confirmation is up");
            let ForcePushConfirmState::Offered { countdown, .. } = confirm.state else {
                panic!("a non-empty overwrite list must offer the push");
            };
            assert_eq!(countdown, FORCE_PUSH_CONFIRM_DELAY_SECS);
            assert!(!confirm.is_armed(), "it must not arm on arrival");
        });

        // The reflex click.
        dialog.update(&mut cx, |dialog, cx| dialog.confirm_force_push(cx));
        dialog.update(&mut cx, |dialog, _| {
            assert!(
                dialog.force_confirm.is_some(),
                "an early press must not dismiss the confirmation"
            );
            assert!(!dialog.pushing, "and it must not start the push");
        });

        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        dialog.update(&mut cx, |dialog, _| {
            assert_eq!(
                dialog.force_confirm.as_ref().and_then(|c| match c.state {
                    ForcePushConfirmState::Offered { countdown, .. } => Some(countdown),
                    _ => None,
                }),
                Some(FORCE_PUSH_CONFIRM_DELAY_SECS - 1),
                "the countdown must tick on the executor clock"
            );
        });

        cx.executor()
            .advance_clock(Duration::from_secs(FORCE_PUSH_CONFIRM_DELAY_SECS as u64));
        cx.run_until_parked();
        dialog.update(&mut cx, |dialog, _| {
            let confirm = dialog.force_confirm.as_ref().expect("still up");
            assert!(confirm.is_armed(), "the wait must end and the button arm");
        });
    }

    /// Nothing on the remote to overwrite: the force push is pointless,
    /// so it is refused outright rather than offered after a wait.
    #[gpui::test]
    async fn an_empty_overwrite_list_refuses_the_force_push(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (dialog, mut cx) = init_push_dialog(ForceMode::WithLease, cx).await;

        dialog.update_in(&mut cx, |dialog, window, cx| {
            dialog.confirm_push(window, cx)
        });
        dialog.update(&mut cx, |dialog, cx| {
            dialog.resolve_overwritten(OverwrittenCommits::Known(Vec::new()), cx)
        });

        dialog.update(&mut cx, |dialog, _| {
            let confirm = dialog.force_confirm.as_ref().expect("confirmation is up");
            assert!(
                matches!(
                    confirm.state,
                    ForcePushConfirmState::Refused(ForcePushRefusal::NothingToOverwrite)
                ),
                "an empty overwrite list must refuse, not offer"
            );
            assert!(!confirm.is_armed());
        });

        // No countdown may be running: waiting cannot turn a refusal into
        // an offer.
        cx.executor().advance_clock(Duration::from_secs(
            FORCE_PUSH_CONFIRM_DELAY_SECS as u64 * 4,
        ));
        cx.run_until_parked();
        dialog.update(&mut cx, |dialog, cx| {
            let confirm = dialog.force_confirm.as_ref().expect("still up");
            assert!(!confirm.is_armed(), "no wait may arm a refusal");
            dialog.confirm_force_push(cx);
        });
        dialog.update(&mut cx, |dialog, _| {
            assert!(!dialog.pushing, "a refused force push must never run");
            assert!(
                dialog.force_confirm.is_some(),
                "and must not silently close"
            );
        });
    }

    /// The list could not be read: refused outright — a confirmation the
    /// user cannot answer informedly is not a safeguard.
    #[gpui::test]
    async fn an_undeterminable_overwrite_list_refuses_the_force_push(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (dialog, mut cx) = init_push_dialog(ForceMode::WithLease, cx).await;

        dialog.update_in(&mut cx, |dialog, window, cx| {
            dialog.confirm_push(window, cx)
        });
        dialog.update(&mut cx, |dialog, cx| {
            dialog.resolve_overwritten(
                OverwrittenCommits::Unknown {
                    why: "this clone has no origin/main ref".into(),
                },
                cx,
            )
        });

        dialog.update(&mut cx, |dialog, _| {
            let confirm = dialog.force_confirm.as_ref().expect("confirmation is up");
            let ForcePushConfirmState::Refused(ForcePushRefusal::Undeterminable { why }) =
                &confirm.state
            else {
                panic!("an unreadable overwrite list must refuse, not offer");
            };
            assert!(why.contains("origin/main"), "got: {why}");
            assert!(!confirm.is_armed());
        });

        cx.executor().advance_clock(Duration::from_secs(
            FORCE_PUSH_CONFIRM_DELAY_SECS as u64 * 4,
        ));
        cx.run_until_parked();
        dialog.update(&mut cx, |dialog, cx| {
            assert!(
                !dialog.force_confirm.as_ref().expect("still up").is_armed(),
                "no wait may arm a refusal"
            );
            dialog.confirm_force_push(cx);
        });
        dialog.update(&mut cx, |dialog, _| {
            assert!(!dialog.pushing, "a blind force push must never run");
        });
    }

    /// The list the confirmation shows is the server-side work a force
    /// push drops — and unlike the dialog's ahead/behind preview it must
    /// include merge commits, which `--no-merges` hides.
    #[gpui::test]
    async fn overwritten_commits_lists_server_side_work_including_merges(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let (_tmp, local, remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));
        let other = local
            .parent()
            .expect("parent of local exists")
            .join("other-merger");
        std::fs::create_dir_all(&other).expect("mkdir");
        run_git_void(&other, &["clone", remote.to_str().unwrap_or_default(), "."])
            .await
            .expect("clone");
        run_git_void(&other, &["config", "user.email", "test@example.com"])
            .await
            .expect("config email");
        run_git_void(&other, &["config", "user.name", "Test"])
            .await
            .expect("config name");
        run_git_void(&other, &["checkout", "-b", "side"])
            .await
            .expect("checkout side");
        std::fs::write(other.join("side.txt"), "s").expect("write side");
        run_git_void(&other, &["add", "side.txt"])
            .await
            .expect("add");
        run_git_void(&other, &["commit", "-m", "side work"])
            .await
            .expect("commit side");
        run_git_void(&other, &["checkout", "main"])
            .await
            .expect("checkout main");
        run_git_void(&other, &["merge", "--no-ff", "side", "-m", "Merge side"])
            .await
            .expect("merge");
        run_git_void(&other, &["push", "origin", "main"])
            .await
            .expect("push");

        // Our own commit, so the local branch is not simply behind.
        std::fs::write(local.join("local.txt"), "x").expect("write local");
        run_git_void(&local, &["add", "local.txt"])
            .await
            .expect("add");
        run_git_void(&local, &["commit", "-m", "local commit"])
            .await
            .expect("commit");
        run_git_void(&local, &["fetch", "origin"])
            .await
            .expect("fetch");

        let OverwrittenCommits::Known(commits) =
            overwritten_commits(&local, "main", "origin", "main").await
        else {
            panic!("a readable tracking ref must produce a list");
        };
        let subjects: Vec<&str> = commits.iter().map(|c| c.subject.as_str()).collect();
        assert!(
            subjects.contains(&"side work"),
            "the commits only on the server must be listed, got: {subjects:?}"
        );
        assert!(
            subjects.contains(&"Merge side"),
            "a merge commit lost is still work lost, got: {subjects:?}"
        );
        assert!(
            !subjects.contains(&"local commit"),
            "our own commits are not what the push overwrites, got: {subjects:?}"
        );
        assert!(
            commits.iter().all(|c| !c.author_email.is_empty()
                && c.committer_date_unix > 0
                && !c.subject.is_empty()),
            "each row needs a subject, a date and an author: {commits:?}"
        );

        // This is exactly why the list is re-derived rather than read off
        // the preview: the preview drops merges.
        let preview = build_preview(&local, "main", "")
            .await
            .unwrap_or_else(|e| panic!("preview: {e}"));
        let behind: Vec<&str> = preview.behind.iter().map(|c| c.subject.as_str()).collect();
        assert!(
            !behind.contains(&"Merge side"),
            "guard: if the preview ever starts including merges this test is moot"
        );
        for subject in &behind {
            assert!(
                subjects.contains(subject),
                "the overwrite list must be a superset of the behind set, missing {subject}"
            );
        }
    }

    /// A tracking ref this clone has never fetched is indistinguishable
    /// from a brand-new remote branch, so it must read as "cannot tell",
    /// never as an empty (reassuring) list.
    #[gpui::test]
    async fn overwritten_commits_treats_an_unreadable_ref_as_dangerous(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let (_tmp, local, _remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));

        let outcome = overwritten_commits(&local, "main", "origin", "never-fetched").await;
        let OverwrittenCommits::Unknown { why } = outcome else {
            panic!("an unreadable ref must not resolve to a list: {outcome:?}");
        };
        assert!(
            why.contains("origin/never-fetched") && why.contains("fetch"),
            "the reason must say what could not be read and what to do: {why}"
        );
    }

    /// The bare `--force` is gone from this crate: a caller that still
    /// asks for it gets the leased form, which refuses rather than
    /// silently overwriting a remote that moved.
    #[gpui::test]
    async fn a_legacy_force_request_still_runs_with_a_lease(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (_tmp, local, remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));
        diverge_remote(&local, &remote)
            .await
            .unwrap_or_else(|e| panic!("diverge: {e}"));

        let err = run_push_cli(
            &local,
            "main",
            "origin",
            "main",
            &PushInvocation {
                force_mode: ForceMode::Force,
                ..plain_push()
            },
        )
        .await
        .expect_err("a bare --force would have overwritten the remote here");
        assert_eq!(
            PushFailure::from_error(&err).kind,
            PushRejection::StaleInfo,
            "the lease must be what refuses it: {err:#}"
        );
    }
}
