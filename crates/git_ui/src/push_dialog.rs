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

use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use git::repository::{Remote, RemoteCommandOutput};
use gpui::{
    AppContext, ClickEvent, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, ParentElement, Render, SharedString, Styled, Task, WeakEntity, Window, div,
};
use menu::Cancel;
use ui::{
    App, Button, Checkbox, Clickable, Color, Context, Headline, HeadlineSize, Icon, IconName,
    IconSize, IntoElement, Label, LabelCommon, LabelSize, ToggleState, Tooltip, h_flex, prelude::*,
    rems, v_flex,
};
use util::ResultExt as _;
use util::command::new_command;
use workspace::{ModalView, Workspace};

use crate::mini_graph::{MiniCommit, MiniGraph};
use crate::remote_output::{RemoteAction, format_output};

/// Force-push posture chosen in the dialog footer.
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
    pushing: bool,
    refreshing: bool,
    focus_handle: FocusHandle,
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
            let _repo = repo;
            let mut dialog = PushDialog {
                workspace: workspace_handle,
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
                pushing: false,
                refreshing: false,
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

    fn toggle_force(&mut self, cx: &mut Context<Self>) {
        if self.force_locked_reason.is_some() {
            return;
        }
        self.force_mode = match self.force_mode {
            ForceMode::Force => ForceMode::None,
            _ => ForceMode::Force,
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

    fn confirm_push(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.pushing {
            return;
        }
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
        // S-SOL-PRT — refuse force-push if the policy says `Forbidden`.
        // The dialog's `force_locked_reason` already disabled the
        // toggle in this case, but a stale snapshot or a settings
        // change between dialog-open and push-confirm could still let
        // the toggle stay on; double-check at the press boundary.
        if !matches!(self.force_mode, ForceMode::None) {
            let op = "force_push";
            if let solutions::branch_protection::Decision::Forbidden { reason } =
                solutions::branch_protection::check(&work_dir, &branch, op)
            {
                log::warn!("PushDialog: force-push refused by branch protection: {reason}");
                self.force_locked_reason = Some(SharedString::from(reason));
                self.force_mode = ForceMode::None;
                cx.notify();
                return;
            }
        }
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
                        cx.emit(DismissEvent);
                    }
                    Err(err) => {
                        log::warn!("PushDialog: push failed: {err}");
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
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
        let header = self.render_header().into_any_element();
        let body = self.render_body(cx).into_any_element();
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
            .child(footer)
    }
}

impl PushDialog {
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

    fn render_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let commits = self.preview.ahead.clone();
        let total = commits.len();
        let selected = self.selected_commit;

        let mini = if commits.is_empty() {
            div()
                .py_4()
                .child(
                    Label::new(if self.refreshing {
                        "Loading…"
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
        let force_lease_state = if matches!(self.force_mode, ForceMode::WithLease) {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };
        let force_state = if matches!(self.force_mode, ForceMode::Force) {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
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
        let force_lease_box = if let Some(reason) = force_locked.clone() {
            force_lease_box.tooltip(Tooltip::text(reason))
        } else {
            force_lease_box
        };

        let force_box = Checkbox::new("push-dialog-force", force_state)
            .label("force")
            .disabled(force_locked.is_some())
            .on_click(cx.listener(|this, _, _, cx| this.toggle_force(cx)));
        let force_box = if let Some(reason) = force_locked {
            force_box.tooltip(Tooltip::text(reason))
        } else {
            force_box.tooltip(Tooltip::text(SharedString::from(
                "Plain --force overwrites without atomic check.",
            )))
        };

        let mut footer = v_flex().gap_2().child(
            h_flex()
                .gap_3()
                .child(force_lease_box)
                .child(force_box)
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
                    .disabled(pushing)
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
    // Real S-SOL-PRT lookup. Maps `Forbidden` to a locked-with-reason
    // string the dialog renders next to the disabled force-push toggle.
    // `RequiresConfirmation` does NOT lock the toggle here — confirming
    // a force-push happens via the dialog's own "type the branch name"
    // modal flow (deferred polish; the toggle is enabled and the actual
    // push goes through the same handler-level check). `Allowed`
    // returns `None`.
    match solutions::branch_protection::check(work_dir, branch, op_name) {
        solutions::branch_protection::Decision::Forbidden { reason } => {
            Some(SharedString::from(reason))
        }
        solutions::branch_protection::Decision::RequiresConfirmation { .. }
        | solutions::branch_protection::Decision::Allowed => None,
    }
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

    let (remote, remote_branch_default, will_create) = match upstream {
        Some(upstream_str) if !upstream_str.is_empty() => {
            if let Some((remote, rb)) = upstream_str.split_once('/') {
                (remote.to_string(), rb.to_string(), false)
            } else {
                ("origin".into(), branch.to_string(), false)
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
    let raw = run_git(
        work_dir,
        &[
            "log",
            "--no-merges",
            "--pretty=format:%H%x09%s%x09%ae%x09%ct",
            range,
        ],
    )
    .await?;
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
        ForceMode::WithLease => args.push("--force-with-lease".into()),
        ForceMode::Force => args.push("--force".into()),
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
/// just plain `git push` with the named flags. `force` adds `--force`.
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
        args.push("--force".into());
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
    use tempfile::TempDir;

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
}
