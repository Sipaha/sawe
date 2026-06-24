//! S-IRB Interactive rebase UI.
//!
//! Visual editor over a `RebaseTodoBuilder` from S-RBL: the user toggles
//! per-row actions (`pick`/`reword`/`edit`/`squash`/`fixup`/`drop`/`exec`),
//! reorders rows, and (for `reword`/`squash`) supplies the new message
//! up-front. Click Start Rebase, then translate rows to a `RebaseTodo`
//! and call `run_rebase`. The handle is kept on the view; subsequent
//! state transitions (`PausedForConflict`, `PausedForEdit`,
//! `PausedForExecFailure`, `Completed`, `Aborted`, `Failed`) drive a
//! footer with Continue / Abort / Skip / Retry / Close buttons.
//!
//! The view is a workspace pane Item — multi-step rebases can coexist
//! with the conflict resolver in another tab. No mid-rebase modal asks
//! for a reword message; reword is pre-supplied (the builder translates
//! it internally to `pick + helper-exec`).

pub mod ai_planner;

use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use git::operations::rebase::{
    RebaseCallbacks, RebaseHandle, RebaseState, RebaseTodo, RebaseTodoBuilder, run_rebase,
};
use git_conflict_ui::ConflictResolverView;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Task, WeakEntity,
    Window, div,
};
use notifications::status_toast::StatusToast;
use project::Project;
use project::git_store::Repository;
use std::any::TypeId;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use theme::ActiveTheme as _;
use ui::{
    Button, ButtonCommon as _, Clickable as _, Color, Disableable as _, FluentBuilder as _,
    Headline, HeadlineSize, Icon, IconName, IconSize, Label, LabelCommon as _, Tooltip, h_flex,
    prelude::*, v_flex,
};
use workspace::{
    Item, Workspace,
    item::{ItemEvent, TabContentParams},
};

use crate::interactive_rebase::ai_planner::{CommitInfo, PlannedAction, plan_rebase};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoAction {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    Drop,
    Exec,
}

impl TodoAction {
    fn label(self) -> &'static str {
        match self {
            TodoAction::Pick => "pick",
            TodoAction::Reword => "reword",
            TodoAction::Edit => "edit",
            TodoAction::Squash => "squash",
            TodoAction::Fixup => "fixup",
            TodoAction::Drop => "drop",
            TodoAction::Exec => "exec",
        }
    }

    fn all() -> [TodoAction; 7] {
        [
            TodoAction::Pick,
            TodoAction::Reword,
            TodoAction::Edit,
            TodoAction::Squash,
            TodoAction::Fixup,
            TodoAction::Drop,
            TodoAction::Exec,
        ]
    }
}

#[derive(Debug, Clone)]
pub struct TodoRow {
    pub sha: String,
    pub original_message: String,
    pub action: TodoAction,
    pub message_override: Option<String>,
    pub shell_command: Option<String>,
}

impl TodoRow {
    pub fn new(sha: String, original_message: String) -> Self {
        Self {
            sha,
            original_message,
            action: TodoAction::Pick,
            message_override: None,
            shell_command: None,
        }
    }

    fn short_sha(&self) -> String {
        self.sha.chars().take(7).collect()
    }

    fn subject(&self) -> &str {
        self.original_message
            .lines()
            .next()
            .unwrap_or(&self.original_message)
    }
}

#[derive(Debug, Clone)]
pub enum ViewState {
    Editing,
    Running,
    PausedForConflict,
    PausedForEdit { current_sha: String },
    PausedForExecFailure { command: String, stderr: String },
    Completed,
    Aborted,
    Failed(String),
}

pub struct InteractiveRebaseView {
    repo: Entity<Repository>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    repo_path: PathBuf,
    base_sha: String,
    branch_name: SharedString,
    state: ViewState,
    rows: Vec<TodoRow>,
    handle: Option<Arc<RebaseHandle>>,
    pending: bool,
    ai_pending: bool,
    editing_field: Option<EditingField>,
    title: SharedString,
    focus_handle: FocusHandle,
}

#[derive(Clone)]
struct EditingField {
    row_index: usize,
    kind: EditingKind,
    editor: Entity<Editor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditingKind {
    Message,
    ShellCmd,
}

impl InteractiveRebaseView {
    /// Open the view against `repo`, starting from `base_sha`. The
    /// caller is responsible for ensuring `base_sha` is reachable from
    /// HEAD on the current branch (use [`base_is_ancestor_of_head`]).
    /// Commits between `base_sha` and HEAD are loaded eagerly to populate
    /// the editable row list.
    pub fn open(
        workspace: Entity<Workspace>,
        repo: Entity<Repository>,
        project: Entity<Project>,
        base_sha: String,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let repo_path = repo.read(cx).work_directory_abs_path.to_path_buf();
        let workspace_weak = workspace.downgrade();
        let base_for_load = base_sha.clone();
        let repo_path_for_load = repo_path.clone();

        window.spawn(cx, async move |cx| {
            let load = cx
                .background_spawn(
                    async move { load_rows_to_pick(&repo_path_for_load, &base_for_load) },
                )
                .await?;
            let branch_name = cx
                .background_spawn({
                    let path = repo_path.clone();
                    async move { current_branch_name(&path) }
                })
                .await
                .unwrap_or_else(|_| "HEAD".to_string());

            workspace.update_in(cx, |workspace, window, cx| {
                let view = cx.new(|cx| {
                    InteractiveRebaseView::new(
                        repo.clone(),
                        project.clone(),
                        workspace_weak,
                        repo_path,
                        base_sha,
                        branch_name.into(),
                        load,
                        cx,
                    )
                });
                workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
                view
            })
        })
    }

    fn new(
        repo: Entity<Repository>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        repo_path: PathBuf,
        base_sha: String,
        branch_name: SharedString,
        rows: Vec<TodoRow>,
        cx: &mut Context<Self>,
    ) -> Self {
        let short_base: String = base_sha.chars().take(7).collect();
        let title: SharedString = format!("Rebase {short_base}… onto {branch_name}").into();
        Self {
            repo,
            project,
            workspace,
            repo_path,
            base_sha,
            branch_name,
            state: ViewState::Editing,
            rows,
            handle: None,
            pending: false,
            ai_pending: false,
            editing_field: None,
            title,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn rows(&self) -> &[TodoRow] {
        &self.rows
    }

    pub fn state(&self) -> &ViewState {
        &self.state
    }

    /// Translate the current rows to a [`RebaseTodo`].
    pub fn build_todo(&self) -> RebaseTodo {
        build_todo_from_rows(&self.rows)
    }

    fn set_action(&mut self, idx: usize, action: TodoAction, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(idx) {
            row.action = action;
            cx.notify();
        }
    }

    fn move_up(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx == 0 || idx >= self.rows.len() {
            return;
        }
        self.rows.swap(idx, idx - 1);
        cx.notify();
    }

    fn move_down(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx + 1 >= self.rows.len() {
            return;
        }
        self.rows.swap(idx, idx + 1);
        cx.notify();
    }

    fn remove_row(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.rows.len() {
            return;
        }
        self.rows.remove(idx);
        if let Some(field) = self.editing_field.as_ref() {
            if field.row_index == idx {
                self.editing_field = None;
            }
        }
        cx.notify();
    }

    fn insert_exec_row(&mut self, cx: &mut Context<Self>) {
        let mut row = TodoRow::new(String::new(), String::new());
        row.action = TodoAction::Exec;
        row.shell_command = Some(String::new());
        self.rows.push(row);
        cx.notify();
    }

    fn reset_to_pick(&mut self, cx: &mut Context<Self>) {
        for row in &mut self.rows {
            if !row.sha.is_empty() {
                row.action = TodoAction::Pick;
                row.message_override = None;
                row.shell_command = None;
            }
        }
        cx.notify();
    }

    fn ai_disabled_reason(&self, cx: &App) -> Option<&'static str> {
        if !matches!(self.state, ViewState::Editing) {
            return Some("AI auto-organize is only available before Start Rebase");
        }
        if self.ai_pending || self.pending {
            return Some("AI auto-organize already in progress");
        }
        let commit_rows = self.rows.iter().filter(|r| !r.sha.is_empty()).count();
        if commit_rows == 0 {
            return Some("No commits to plan");
        }
        if commit_rows > ai_planner::MAX_COMMITS {
            return Some("Too many commits for AI auto-organize (limit 50)");
        }
        if !has_active_solution(cx) {
            return Some("AI auto-organize requires an active Solution");
        }
        None
    }

    /// Triggered by the "AI Auto-organize" button. Opens a confirmation
    /// modal first, then runs the planner asynchronously and replaces
    /// the current rows on success.
    fn run_ai_auto_organize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_disabled_reason(cx).is_some() {
            return;
        }

        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            "Replace current todo with AI-organized version?",
            Some(
                "The AI planner will propose a fresh rebase todo (squash / reorder / \
                 reword / drop) for the listed commits. Any edits you've made — \
                 reorderings, custom messages, exec lines — will be lost. You can \
                 still tweak the result before clicking Start Rebase.",
            ),
            &["Replace", "Cancel"],
            cx,
        );

        cx.spawn_in(window, async move |this, cx| {
            let choice = answer.await.ok();
            if choice != Some(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.start_ai_planner(cx);
            });
        })
        .detach();
    }

    fn start_ai_planner(&mut self, cx: &mut Context<Self>) {
        let commits: Vec<CommitInfo> = self
            .rows
            .iter()
            .filter(|r| !r.sha.is_empty())
            .map(|r| CommitInfo {
                sha: r.sha.clone(),
                subject: r
                    .original_message
                    .lines()
                    .next()
                    .unwrap_or(&r.original_message)
                    .to_string(),
                body: r
                    .original_message
                    .split_once('\n')
                    .map(|(_, rest)| rest)
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                diff_stat: String::new(),
            })
            .collect();
        if commits.is_empty() {
            return;
        }
        self.ai_pending = true;
        cx.notify();

        let project = self.project.clone();
        let repo_work_dir = self.repo_path.clone();

        cx.spawn(async move |this, cx| {
            // Compute diff stats off the foreground; surface failures as
            // empty stats rather than aborting the whole plan — the
            // prompt still has the subject + body, which is enough for
            // the agent to do something useful.
            let with_stats = cx
                .background_spawn({
                    let repo_work_dir = repo_work_dir.clone();
                    async move {
                        let mut filled = commits;
                        for commit in filled.iter_mut() {
                            commit.diff_stat =
                                git_diff_stat(&repo_work_dir, &commit.sha).unwrap_or_default();
                        }
                        filled
                    }
                })
                .await;
            let result = plan_rebase(&with_stats, &project, &repo_work_dir, &mut cx.clone()).await;
            let _ = this.update(cx, |this, cx| {
                this.ai_pending = false;
                match result {
                    Ok(actions) => {
                        this.apply_planned_actions(actions, cx);
                    }
                    Err(err) => {
                        let message = format!("AI auto-organize failed: {err:#}");
                        log::warn!("{message}");
                        this.show_ai_failure_toast(message, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_planned_actions(&mut self, actions: Vec<PlannedAction>, cx: &mut Context<Self>) {
        let original_by_sha: std::collections::HashMap<String, TodoRow> = self
            .rows
            .iter()
            .filter(|r| !r.sha.is_empty())
            .map(|r| (r.sha.clone(), r.clone()))
            .collect();

        // Build the replacement sequence respecting `insert_after` when
        // present. We start by laying out actions in the order they
        // arrived from the planner; then any action with `insert_after`
        // pointing at a sha earlier in the list is moved into place.
        let mut ordered: Vec<PlannedAction> = actions;
        let mut i = 0;
        while i < ordered.len() {
            let target = ordered[i].insert_after.clone();
            if let Some(target_sha) = target {
                let anchor = ordered.iter().take(i).position(|a| a.sha == target_sha);
                if let Some(anchor_idx) = anchor
                    && anchor_idx + 1 != i
                {
                    let entry = ordered.remove(i);
                    ordered.insert(anchor_idx + 1, entry);
                    // Re-process the slot we just filled.
                    continue;
                }
            }
            i += 1;
        }

        let mut new_rows: Vec<TodoRow> = Vec::with_capacity(ordered.len());
        for action in ordered {
            // Sanitization should have already rejected unknown shas;
            // this is the second line of defense — if a sha somehow
            // slipped through and we have no original row to clone, skip.
            let Some(mut row) = original_by_sha.get(&action.sha).cloned() else {
                log::warn!(
                    "interactive rebase: planner returned sha {} not present in original rows",
                    action.sha
                );
                continue;
            };
            row.action = action.action;
            row.message_override = action.new_message.filter(|m| !m.trim().is_empty());
            // exec actions can never reach this branch — sanitize_response
            // drops them in ai_planner. The shell_command field is left
            // as-is on the cloned row (which was always None for a
            // commit row anyway).
            new_rows.push(row);
        }

        if new_rows.is_empty() {
            self.show_ai_failure_toast("AI auto-organize returned no commits to replay".into(), cx);
            return;
        }
        self.rows = new_rows;
        self.editing_field = None;
        cx.notify();
    }

    fn show_ai_failure_toast(&self, message: String, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, |this, _cx| {
                this.icon(
                    Icon::new(IconName::XCircle)
                        .size(IconSize::Small)
                        .color(Color::Error),
                )
                .dismiss_button(true)
            });
            workspace.toggle_status_toast(toast, cx);
        });
    }

    fn open_message_editor(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let initial = self
            .rows
            .get(idx)
            .map(|r| {
                r.message_override
                    .clone()
                    .unwrap_or_else(|| r.original_message.clone())
            })
            .unwrap_or_default();
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text(initial, window, cx);
            editor
        });
        self.editing_field = Some(EditingField {
            row_index: idx,
            kind: EditingKind::Message,
            editor,
        });
        cx.notify();
    }

    fn open_shell_editor(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let initial = self
            .rows
            .get(idx)
            .and_then(|r| r.shell_command.clone())
            .unwrap_or_default();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(initial, window, cx);
            editor
        });
        self.editing_field = Some(EditingField {
            row_index: idx,
            kind: EditingKind::ShellCmd,
            editor,
        });
        cx.notify();
    }

    fn save_editing_field(&mut self, cx: &mut Context<Self>) {
        let Some(field) = self.editing_field.take() else {
            return;
        };
        let value = field.editor.read(cx).text(cx);
        if let Some(row) = self.rows.get_mut(field.row_index) {
            match field.kind {
                EditingKind::Message => {
                    row.message_override = Some(value);
                }
                EditingKind::ShellCmd => {
                    row.shell_command = Some(value);
                }
            }
        }
        cx.notify();
    }

    fn cancel_editing_field(&mut self, cx: &mut Context<Self>) {
        self.editing_field = None;
        cx.notify();
    }

    /// Render the rows into a textual todo (matches what S-RBL would
    /// hand to git). Used by the Show preview button.
    pub fn preview_todo_text(&self) -> String {
        self.build_todo()
            .serialize_with_helper("<sawe-helper> --git-message-set")
    }

    fn start_rebase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending || self.handle.is_some() {
            return;
        }
        self.pending = true;
        self.state = ViewState::Running;
        cx.notify();

        let todo = self.build_todo();
        let repo_path = self.repo_path.clone();
        let base = self.base_sha.clone();

        let conflict_signal = Arc::new(Mutex::new(false));
        let edit_signal: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let shell_signal: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));

        let conflict_for_cb = conflict_signal.clone();
        let edit_for_cb = edit_signal.clone();
        let shell_for_cb = shell_signal.clone();

        let callbacks = RebaseCallbacks {
            on_conflict: Box::new(move |_, _files| {
                if let Ok(mut g) = conflict_for_cb.lock() {
                    *g = true;
                }
            }),
            on_paused_for_edit: Box::new(move |_, sha| {
                if let Ok(mut g) = edit_for_cb.lock() {
                    *g = Some(sha);
                }
            }),
            on_exec_failure: Box::new(move |_, command, stderr| {
                if let Ok(mut g) = shell_for_cb.lock() {
                    *g = Some((command, stderr));
                }
            }),
            on_completed: Box::new(|_| {}),
        };

        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(
                    async move { run_rebase(&repo_path, &base, todo, callbacks).await },
                )
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.pending = false;
                match result {
                    Ok(handle) => {
                        let handle = Arc::new(handle);
                        this.handle = Some(handle);
                        this.refresh_state_from_handle(window, cx);
                        let conflict = conflict_signal.lock().map(|g| *g).unwrap_or(false);
                        if conflict {
                            this.open_conflict_resolver(window, cx);
                        }
                        let _ = (edit_signal, shell_signal);
                    }
                    Err(err) => {
                        this.state = ViewState::Failed(format!("{err}"));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn refresh_state_from_handle(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(handle) = self.handle.clone() else {
            return;
        };
        self.state = match handle.state() {
            RebaseState::Running => ViewState::Running,
            RebaseState::PausedForConflict { .. } => ViewState::PausedForConflict,
            RebaseState::PausedForEdit { current_sha } => ViewState::PausedForEdit { current_sha },
            RebaseState::PausedForExecFailure { command, stderr } => {
                ViewState::PausedForExecFailure { command, stderr }
            }
            RebaseState::Completed => ViewState::Completed,
            RebaseState::Aborted => ViewState::Aborted,
            RebaseState::Failed(msg) => ViewState::Failed(msg),
        };
        cx.notify();
    }

    fn open_conflict_resolver(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = self.project.clone();
        let work_dir: Arc<std::path::Path> = self.repo.read(cx).work_directory_abs_path.clone();
        let weak = workspace.downgrade();
        ConflictResolverView::open(project, weak, work_dir, window, cx).detach_and_log_err(cx);
    }

    fn run_continuation_command(
        &mut self,
        op: ContinuationOp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(handle) = self.handle.clone() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    match op {
                        ContinuationOp::Continue => handle.continue_(),
                        ContinuationOp::Abort => handle.abort(),
                        ContinuationOp::Skip => handle.skip(),
                        ContinuationOp::RetryShell => handle.retry_exec(),
                    }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Err(err) = outcome {
                    log::warn!("interactive rebase: continuation failed: {err}");
                }
                this.refresh_state_from_handle(window, cx);
            });
        })
        .detach();
    }

    fn close_view(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // Drop the handle so the session dir + repo lock release before
        // the workspace removes the item.
        self.handle = None;
        cx.notify();
    }

    fn render_header(&self, cx: &Context<Self>) -> AnyElement {
        let short: String = self.base_sha.chars().take(7).collect();
        let detail: SharedString = format!("from {short}… onto {}", self.branch_name).into();
        h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Icon::new(IconName::ListTree))
            .child(Headline::new("Interactive Rebase").size(HeadlineSize::Small))
            .child(
                Label::new(detail)
                    .color(Color::Muted)
                    .size(ui::LabelSize::Small),
            )
            .into_any_element()
    }

    fn render_status(&self, _cx: &Context<Self>) -> AnyElement {
        let (icon, color, text): (IconName, Color, SharedString) = match &self.state {
            ViewState::Editing => (
                IconName::Pencil,
                Color::Muted,
                "Editing — adjust actions below, then click Start Rebase".into(),
            ),
            ViewState::Running => {
                if self.pending {
                    (
                        IconName::ArrowCircle,
                        Color::Info,
                        "Starting rebase…".into(),
                    )
                } else {
                    (IconName::ArrowCircle, Color::Info, "Rebase running".into())
                }
            }
            ViewState::PausedForConflict => (
                IconName::Warning,
                Color::Warning,
                "Paused on conflict — resolve files in the conflict resolver, then click Continue"
                    .into(),
            ),
            ViewState::PausedForEdit { current_sha } => {
                let short: String = current_sha.chars().take(7).collect();
                (
                    IconName::Warning,
                    Color::Warning,
                    format!(
                        "Paused at {short} for edit — modify and `git commit --amend`, then click Continue"
                    )
                    .into(),
                )
            }
            ViewState::PausedForExecFailure { command, .. } => (
                IconName::XCircle,
                Color::Error,
                format!("Exec failed: {command}").into(),
            ),
            ViewState::Completed => (IconName::Check, Color::Success, "Rebase completed".into()),
            ViewState::Aborted => (IconName::XCircle, Color::Muted, "Rebase aborted".into()),
            ViewState::Failed(msg) => (
                IconName::XCircle,
                Color::Error,
                format!("Rebase failed: {msg}").into(),
            ),
        };
        h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .child(Icon::new(icon).color(color))
            .child(Label::new(text).color(color))
            .into_any_element()
    }

    fn render_row(&self, idx: usize, cx: &mut Context<Self>) -> AnyElement {
        let row = match self.rows.get(idx) {
            Some(r) => r.clone(),
            None => return div().into_any_element(),
        };
        let is_editable = matches!(self.state, ViewState::Editing);
        let action_label: SharedString = row.action.label().to_string().into();

        let mut action_buttons = h_flex().gap_1();
        for action in TodoAction::all() {
            let selected = action == row.action;
            let label = action.label();
            let disabled = !is_editable || (row.sha.is_empty() && action != TodoAction::Exec);
            let btn = Button::new(SharedString::from(format!("irb-act-{idx}-{label}")), label)
                .toggle_state(selected)
                .disabled(disabled)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_action(idx, action, cx);
                }));
            action_buttons = action_buttons.child(btn);
        }

        let move_up = Button::new(SharedString::from(format!("irb-up-{idx}")), "▲")
            .disabled(!is_editable || idx == 0)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.move_up(idx, cx);
            }));
        let move_down = Button::new(SharedString::from(format!("irb-down-{idx}")), "▼")
            .disabled(!is_editable || idx + 1 >= self.rows.len())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.move_down(idx, cx);
            }));
        let remove = Button::new(SharedString::from(format!("irb-rm-{idx}")), "Remove")
            .disabled(!is_editable)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.remove_row(idx, cx);
            }));

        let summary = if row.sha.is_empty() {
            let cmd = row.shell_command.clone().unwrap_or_default();
            format!("exec: {}", if cmd.is_empty() { "<command>" } else { &cmd })
        } else {
            format!("{} {}", row.short_sha(), row.subject())
        };
        let summary_label = Label::new(SharedString::from(summary)).color(
            if matches!(row.action, TodoAction::Drop) {
                Color::Disabled
            } else {
                Color::Default
            },
        );

        let needs_message = matches!(row.action, TodoAction::Reword | TodoAction::Squash);
        let edit_message_btn = if needs_message {
            let label = if row.message_override.is_some() {
                "Edit message ✓"
            } else {
                "Edit message"
            };
            Some(
                Button::new(SharedString::from(format!("irb-msg-{idx}")), label)
                    .disabled(!is_editable)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_message_editor(idx, window, cx);
                    })),
            )
        } else {
            None
        };
        let edit_shell_btn = if matches!(row.action, TodoAction::Exec) {
            Some(
                Button::new(
                    SharedString::from(format!("irb-shell-{idx}")),
                    "Edit command",
                )
                .disabled(!is_editable)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_shell_editor(idx, window, cx);
                })),
            )
        } else {
            None
        };

        let main_line = h_flex()
            .gap_2()
            .px_3()
            .py_1()
            .child(Label::new(action_label).color(Color::Accent))
            .child(summary_label)
            .child(div().flex_1())
            .child(move_up)
            .child(move_down)
            .child(remove);

        let action_line = h_flex()
            .px_3()
            .pb_1()
            .gap_2()
            .child(action_buttons)
            .when_some(edit_message_btn, |this, btn| this.child(btn))
            .when_some(edit_shell_btn, |this, btn| this.child(btn));

        let inline_editor = if let Some(field) =
            self.editing_field.as_ref().filter(|f| f.row_index == idx)
        {
            let kind_label: SharedString = match field.kind {
                EditingKind::Message => "New commit message".into(),
                EditingKind::ShellCmd => "Shell command (runs verbatim)".into(),
            };
            let warn = matches!(field.kind, EditingKind::ShellCmd);
            let editor = field.editor.clone();
            Some(
                v_flex()
                    .px_3()
                    .pb_2()
                    .gap_1()
                    .child(
                        Label::new(kind_label)
                            .color(Color::Muted)
                            .size(ui::LabelSize::Small),
                    )
                    .when(warn, |this| {
                        this.child(
                            Label::new("Custom commands run in your shell.")
                                .color(Color::Warning)
                                .size(ui::LabelSize::Small),
                        )
                    })
                    .child(editor)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(SharedString::from(format!("irb-save-{idx}")), "Save")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.save_editing_field(cx);
                                    })),
                            )
                            .child(
                                Button::new(
                                    SharedString::from(format!("irb-cancel-{idx}")),
                                    "Cancel",
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.cancel_editing_field(cx);
                                    },
                                )),
                            ),
                    ),
            )
        } else {
            None
        };

        v_flex()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(main_line)
            .child(action_line)
            .when_some(inline_editor, |this, editor| this.child(editor))
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let editing = matches!(self.state, ViewState::Editing);
        let running = matches!(self.state, ViewState::Running) && !self.pending;
        let paused = matches!(
            self.state,
            ViewState::PausedForConflict
                | ViewState::PausedForEdit { .. }
                | ViewState::PausedForExecFailure { .. }
        );
        let shell_paused = matches!(self.state, ViewState::PausedForExecFailure { .. });
        let terminal = matches!(
            self.state,
            ViewState::Completed | ViewState::Aborted | ViewState::Failed(_)
        );
        let has_handle = self.handle.is_some();

        let ai_disabled_reason = self.ai_disabled_reason(cx);
        let ai_disabled = ai_disabled_reason.is_some();
        let mut ai_button = Button::new("irb-ai-auto-organize", "AI Auto-organize")
            .start_icon(Icon::new(IconName::Sparkle).size(IconSize::Small))
            .loading(self.ai_pending)
            .disabled(ai_disabled || self.ai_pending)
            .on_click(cx.listener(|this, _, window, cx| {
                this.run_ai_auto_organize(window, cx);
            }));
        ai_button = if let Some(reason) = ai_disabled_reason {
            let reason_str: SharedString = SharedString::from(reason);
            ai_button.tooltip(move |_, cx| Tooltip::simple(reason_str.clone(), cx))
        } else if self.ai_pending {
            ai_button.tooltip(Tooltip::text("AI auto-organize in progress"))
        } else {
            ai_button.tooltip(Tooltip::text(
                "Ask the AI to propose a clean rebase todo (squash / reorder / reword / drop)",
            ))
        };

        let editing_buttons = h_flex()
            .gap_2()
            .child(
                Button::new("irb-insert-exec", "Insert exec line")
                    .disabled(!editing)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.insert_exec_row(cx);
                    })),
            )
            .child(
                Button::new("irb-reset", "Reset to default")
                    .disabled(!editing)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reset_to_pick(cx);
                    })),
            )
            .child(ai_button)
            .child(
                Button::new("irb-preview", "Show preview")
                    .disabled(!editing)
                    .on_click(cx.listener(|this, _, _, cx| {
                        log::info!("interactive rebase preview:\n{}", this.preview_todo_text());
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("irb-start", "Start Rebase")
                    .disabled(!editing || self.rows.is_empty() || self.pending)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.start_rebase(window, cx);
                    })),
            )
            .child(
                Button::new("irb-cancel-editing", "Cancel").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.close_view(window, cx);
                    },
                )),
            );

        let running_buttons = h_flex()
            .gap_2()
            .child(div().flex_1())
            .child(
                Button::new("irb-abort", "Abort")
                    .disabled(!has_handle || (!running && !paused))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run_continuation_command(ContinuationOp::Abort, window, cx);
                    })),
            )
            .child(
                Button::new("irb-skip", "Skip")
                    .disabled(!has_handle || !paused)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run_continuation_command(ContinuationOp::Skip, window, cx);
                    })),
            )
            .child(
                Button::new("irb-retry", "Retry")
                    .disabled(!has_handle || !shell_paused)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run_continuation_command(ContinuationOp::RetryShell, window, cx);
                    })),
            )
            .child(
                Button::new("irb-continue", "Continue")
                    .disabled(!has_handle || !paused)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run_continuation_command(ContinuationOp::Continue, window, cx);
                    })),
            );

        let close_button =
            Button::new("irb-close", "Close").on_click(cx.listener(|this, _, window, cx| {
                this.close_view(window, cx);
            }));

        if editing {
            editing_buttons.into_any_element()
        } else if terminal {
            h_flex()
                .gap_2()
                .child(div().flex_1())
                .child(close_button)
                .into_any_element()
        } else {
            running_buttons.into_any_element()
        }
    }

    fn render_shell_failure_detail(&self, _cx: &Context<Self>) -> Option<AnyElement> {
        let ViewState::PausedForExecFailure { command, stderr } = &self.state else {
            return None;
        };
        let command_text: SharedString = format!("$ {command}").into();
        let stderr_text: SharedString = stderr.clone().into();
        Some(
            v_flex()
                .px_3()
                .py_2()
                .gap_1()
                .child(Label::new(command_text).color(Color::Warning))
                .child(
                    Label::new(stderr_text)
                        .color(Color::Error)
                        .size(ui::LabelSize::Small),
                )
                .into_any_element(),
        )
    }
}

#[derive(Debug, Clone, Copy)]
enum ContinuationOp {
    Continue,
    Abort,
    Skip,
    RetryShell,
}

impl Render for InteractiveRebaseView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_header(cx);
        let status = self.render_status(cx);
        let footer = self.render_footer(cx);
        let shell_detail = self.render_shell_failure_detail(cx);
        let row_count = self.rows.len();
        let body = v_flex().w_full().children(
            (0..row_count)
                .map(|idx| self.render_row(idx, cx))
                .collect::<Vec<_>>(),
        );

        v_flex()
            .id("interactive-rebase")
            .key_context("InteractiveRebase")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(header)
            .child(status)
            .when_some(shell_detail, |this, detail| this.child(detail))
            .child(
                div()
                    .id("interactive-rebase-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
            .child(
                div()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .p_2()
                    .child(footer),
            )
    }
}

impl Focusable for InteractiveRebaseView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for InteractiveRebaseView {}

impl Item for InteractiveRebaseView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, _cx: &App) -> AnyElement {
        Label::new(self.title.clone())
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Interactive Rebase Opened")
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else {
            None
        }
    }
}

#[allow(clippy::disallowed_methods)]
fn current_branch_name(repo_path: &std::path::Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))?;
    if !output.status.success() {
        return Ok("HEAD".to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[allow(clippy::disallowed_methods)]
fn run_git(repo_path: &std::path::Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))
}

/// Returns commits between `base` (exclusive) and HEAD, oldest first —
/// i.e. the order `git rebase -i` writes them in the todo. `base` must
/// be reachable from HEAD or the load fails.
pub(crate) fn load_rows_to_pick(repo_path: &std::path::Path, base: &str) -> Result<Vec<TodoRow>> {
    let ancestor = run_git(repo_path, &["merge-base", "--is-ancestor", base, "HEAD"])?;
    if !ancestor.status.success() {
        return Err(anyhow!(
            "{base} is not an ancestor of HEAD; refusing to rebase from a non-linear point"
        ));
    }
    let listing = run_git(
        repo_path,
        &[
            "log",
            "--reverse",
            "--format=%H%x09%s",
            &format!("{base}..HEAD"),
        ],
    )?;
    if !listing.status.success() {
        return Err(anyhow!(
            "git log {base}..HEAD failed: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        ));
    }
    let body = String::from_utf8_lossy(&listing.stdout);
    let mut rows = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, '\t');
        let sha = parts.next().unwrap_or("").trim().to_string();
        let subject = parts.next().unwrap_or("").to_string();
        if sha.is_empty() {
            continue;
        }
        rows.push(TodoRow::new(sha, subject));
    }
    if rows.is_empty() {
        return Err(anyhow!(
            "no commits between {base} and HEAD — nothing to rebase"
        ));
    }
    Ok(rows)
}

/// Verify `base` is an ancestor of HEAD without loading the full row
/// list. Used by the context-menu wiring so we can reject the action
/// early before constructing the view.
pub fn base_is_ancestor_of_head(repo_path: &std::path::Path, base: &str) -> Result<()> {
    let output = run_git(repo_path, &["merge-base", "--is-ancestor", base, "HEAD"])
        .with_context(|| format!("checking ancestry of {base}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "{base} is not reachable from HEAD on the current branch"
        ));
    }
    Ok(())
}

/// True when there is at least one Solution registered in the
/// SolutionStore. Mirrors `git_conflict_ui::resolver_view::has_active_solution`
/// — the AI Auto-organize button needs *some* Solution to host the
/// ephemeral session.
fn has_active_solution(cx: &App) -> bool {
    solutions::SolutionStore::try_global(cx)
        .map(|store| !store.read(cx).solutions().is_empty())
        .unwrap_or(false)
}

/// Compute a short `--shortstat` for `sha` to feed the AI planner. Best
/// effort: any failure produces an empty string and the planner runs
/// without that commit's diff summary.
#[allow(clippy::disallowed_methods)]
fn git_diff_stat(repo_path: &std::path::Path, sha: &str) -> Result<String> {
    let attempt = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["diff", "--shortstat", &format!("{sha}~..{sha}")])
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))?;
    if attempt.status.success() {
        return Ok(String::from_utf8_lossy(&attempt.stdout).trim().to_string());
    }
    // Root-commit fallback.
    let fallback = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["show", "--shortstat", "--format=", sha])
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))?;
    if !fallback.status.success() {
        return Err(anyhow!(
            "git diff/show --shortstat failed for {sha}: {}",
            String::from_utf8_lossy(&fallback.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&fallback.stdout).trim().to_string())
}

/// Builder used by both the view and unit tests; pulled out as a
/// free function so it can be exercised without constructing a
/// full GPUI context.
pub(crate) fn build_todo_from_rows(rows: &[TodoRow]) -> RebaseTodo {
    let mut builder = RebaseTodoBuilder::new();
    for row in rows {
        builder = match row.action {
            TodoAction::Pick => builder.pick(row.sha.clone()),
            TodoAction::Reword => {
                let message = row
                    .message_override
                    .clone()
                    .unwrap_or_else(|| row.original_message.clone());
                builder.reword(row.sha.clone(), message)
            }
            TodoAction::Edit => builder.edit(row.sha.clone()),
            TodoAction::Squash => builder.squash(row.sha.clone()),
            TodoAction::Fixup => builder.fixup(row.sha.clone()),
            TodoAction::Drop => builder.drop(row.sha.clone()),
            TodoAction::Exec => {
                let cmd = row.shell_command.clone().unwrap_or_default();
                builder.exec(cmd)
            }
        };
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sha: &str, subject: &str) -> TodoRow {
        TodoRow::new(sha.to_string(), subject.to_string())
    }

    #[test]
    fn build_todo_translates_actions() {
        let mut rows = vec![
            row("aaaa", "first"),
            row("bbbb", "second"),
            row("cccc", "third"),
        ];
        rows[0].action = TodoAction::Pick;
        rows[1].action = TodoAction::Drop;
        rows[2].action = TodoAction::Reword;
        rows[2].message_override = Some("rewritten".into());

        let todo = build_todo_from_rows(&rows);
        let body = todo.serialize_with_helper("/helper --git-message-set");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            lines.len(),
            4,
            "expected pick + drop + pick+exec, got {body:?}"
        );
        assert!(
            lines[0].starts_with("pick aaaa"),
            "expected pick first, got {body:?}"
        );
        assert!(
            lines[1].starts_with("drop bbbb"),
            "expected drop second, got {body:?}"
        );
        assert!(
            lines[2].starts_with("pick cccc"),
            "expected pick (reword translation) third, got {body:?}"
        );
        assert!(
            lines[3].starts_with("exec /helper --git-message-set "),
            "expected reword's exec line, got {body:?}"
        );
    }

    #[test]
    fn build_todo_handles_shell_command() {
        let mut rows = vec![row("aaaa", "msg")];
        rows[0].action = TodoAction::Pick;
        rows.push({
            let mut r = TodoRow::new(String::new(), String::new());
            r.action = TodoAction::Exec;
            r.shell_command = Some("make test".into());
            r
        });
        let todo = build_todo_from_rows(&rows);
        let body = todo.serialize_with_helper("/x");
        assert_eq!(body.trim_end(), "pick aaaa\nexec make test");
    }

    #[test]
    fn drop_middle_row_yields_pick_drop_pick() {
        let mut rows = vec![row("1111", "a"), row("2222", "b"), row("3333", "c")];
        rows[1].action = TodoAction::Drop;
        let todo = build_todo_from_rows(&rows);
        let body = todo.serialize_with_helper("/x");
        assert_eq!(body, "pick 1111\ndrop 2222\npick 3333\n");
    }
}
