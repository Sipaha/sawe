//! `ConflictResolverView` — the v0 layout from the spike: three (or four
//! when Base is shown) standalone Editors in one parent View, mirroring
//! `editor::SplittableEditor`. Buffers are synthetic `Buffer::local`
//! handles populated from `git show :N:<path>`, plus a writable Result
//! buffer carrying the working-tree text.

use anyhow::Result;
use editor::{Editor, EditorEvent, MinimapVisibility, MultiBufferOffset};
use gpui::{
    AnyElement, App, AppContext as _, Context, DragMoveEvent, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, div, px,
};
use language::Buffer;
use project::Project;
use std::any::TypeId;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use theme::ActiveTheme as _;
use ui::{Color, Icon, IconName, Label, LabelCommon as _, h_flex, v_flex};
use util::ResultExt as _;
use workspace::{
    Item, Workspace,
    item::{ItemEvent, TabContentParams},
};

use crate::binary_view::BinaryConflictView;
use crate::chunks::{ConflictChunk, extract_conflict_chunks};
use crate::conflict_parser::{
    ConflictedFile, InProgressOp, ThreeWayContent, detect_in_progress_op, list_conflicts_async,
    load_three_way_async,
};
use crate::sidebar::ConflictSidebar;
use crate::toolbar;

const RESIZE_HANDLE_WIDTH: f32 = 8.0;
const MIN_RATIO: f32 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolverPane {
    Local,
    Result,
    Their,
    Base,
}

#[derive(Debug, Clone)]
struct DraggedHandle {
    boundary: Boundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
    LocalResult,
    ResultTheir,
    TheirBase,
}

/// Pane width ratios summing to 1.0. Always tracks four slots (local,
/// result, their, base); when Base is hidden the fourth slot is 0.0 and
/// rendering skips it.
#[derive(Debug, Clone)]
pub struct ThreeWaySplitState {
    ratios: [f32; 4],
    show_base: bool,
}

impl ThreeWaySplitState {
    pub fn new(show_base: bool) -> Self {
        let ratios = if show_base {
            [0.25, 0.25, 0.25, 0.25]
        } else {
            [1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0, 0.0]
        };
        Self { ratios, show_base }
    }

    fn flex(&self, idx: usize) -> f32 {
        self.ratios[idx].max(0.0)
    }

    fn move_boundary(&mut self, boundary: Boundary, delta_ratio: f32) {
        let (left_idx, right_idx) = match boundary {
            Boundary::LocalResult => (0, 1),
            Boundary::ResultTheir => (1, 2),
            Boundary::TheirBase => (2, 3),
        };
        let total = self.ratios[left_idx] + self.ratios[right_idx];
        let mut new_left = self.ratios[left_idx] + delta_ratio;
        new_left = new_left.clamp(MIN_RATIO, total - MIN_RATIO);
        self.ratios[left_idx] = new_left;
        self.ratios[right_idx] = total - new_left;
    }

    pub fn toggle_base(&mut self) {
        self.show_base = !self.show_base;
        if self.show_base {
            // shrink result by 1/4 to make room
            let chunk = self.ratios[1] * 0.5;
            self.ratios[1] -= chunk;
            self.ratios[3] = chunk;
        } else {
            let returning = self.ratios[3];
            self.ratios[1] += returning;
            self.ratios[3] = 0.0;
        }
    }
}

/// The resolver itself. Holds three (or four) Editor entities, the source
/// data describing each side, and bookkeeping for the toolbar and
/// sidebar. Constructed via [`ConflictResolverView::open`] from outside.
pub struct ConflictResolverView {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    work_dir: Arc<Path>,
    git_dir: PathBuf,
    /// Conflicted files known to this resolver session. Populated by the
    /// initial `list_conflicts` and refreshed on Mark Resolved /
    /// repository status changes.
    conflicts: Vec<ConflictedFile>,
    /// Index of the currently-open file inside `conflicts`. `None` if
    /// the conflict list is empty (e.g. all resolved → continue prompt).
    active_file: Option<usize>,
    /// Resolved files this session — used to drive the sidebar progress
    /// counter and the "all done? continue?" auto-prompt.
    resolved_files: Vec<git::repository::RepoPath>,

    sidebar: Entity<ConflictSidebar>,

    /// Per-file editor state. `None` when no file is selected or the
    /// active file is binary (binary view is rendered instead).
    text_state: Option<TextResolverState>,
    binary_view: Option<Entity<BinaryConflictView>>,

    last_focused: ResolverPane,
    show_base: bool,
    lock_scroll: bool,

    op: Option<InProgressOp>,
    chunks: Vec<ConflictChunk>,
    current_chunk: Option<usize>,

    /// The three-way content for the currently active file. Cached so
    /// the toolbar can compute the AI-suggest button's enabled state
    /// (size cap, presence of base/ours/theirs) without re-spawning
    /// `git show :N:<path>` on every render. Cleared when the active
    /// file changes or is binary.
    last_three_way_content: Option<ThreeWayContent>,
    /// True while an AI merge suggestion is in flight. Used to render
    /// the toolbar button as a spinner and to suppress duplicate clicks.
    ai_suggest_pending: bool,
    _ai_suggest_task: Option<Task<()>>,

    split_state: ThreeWaySplitState,
    title: SharedString,
    _subscriptions: Vec<Subscription>,
}

struct TextResolverState {
    path: git::repository::RepoPath,
    local_editor: Entity<Editor>,
    result_editor: Entity<Editor>,
    their_editor: Entity<Editor>,
    base_editor: Option<Entity<Editor>>,
    _subscriptions: Vec<Subscription>,
}

impl ConflictResolverView {
    /// Open a fresh resolver against the active repository's working
    /// directory. Spawns the initial `git ls-files -u` enumeration and
    /// then loads the first conflicted file. Adds the resolver to the
    /// workspace's active pane on completion.
    pub fn open(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        work_dir: Arc<Path>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            let conflicts = cx
                .background_spawn({
                    let dir = work_dir.to_path_buf();
                    async move { list_conflicts_async(&dir).await }
                })
                .await?;

            let view = workspace.update_in(cx, |workspace, window, cx| {
                let view = cx.new(|cx| {
                    ConflictResolverView::new(
                        project.clone(),
                        workspace.weak_handle(),
                        work_dir.clone(),
                        conflicts.clone(),
                        window,
                        cx,
                    )
                });
                workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
                view
            })?;

            // Open the first file (if any) once we're back on the main thread
            // — avoids holding the editor across the await boundary.
            view.update_in(cx, |this, window, cx| {
                if !this.conflicts.is_empty() {
                    this.activate_file(0, window, cx);
                }
            })
            .ok();

            Ok(view)
        })
    }

    fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        work_dir: Arc<Path>,
        conflicts: Vec<ConflictedFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let git_dir = compute_git_dir(&work_dir);
        let op = detect_in_progress_op(&git_dir);
        let sidebar = cx.new(|cx| ConflictSidebar::new(conflicts.clone(), cx));

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &sidebar,
            window,
            |this, _, event: &crate::sidebar::FileSelected, window, cx| {
                this.activate_file(event.index, window, cx);
            },
        ));

        let title: SharedString = match op {
            Some(InProgressOp::Merge) => "Resolve merge conflicts".into(),
            Some(InProgressOp::Rebase) => "Resolve rebase conflicts".into(),
            Some(InProgressOp::CherryPick) => "Resolve cherry-pick conflicts".into(),
            Some(InProgressOp::Revert) => "Resolve revert conflicts".into(),
            None => "Resolve conflicts".into(),
        };

        Self {
            project,
            workspace,
            work_dir,
            git_dir,
            conflicts,
            active_file: None,
            resolved_files: Vec::new(),
            sidebar,
            text_state: None,
            binary_view: None,
            last_focused: ResolverPane::Result,
            show_base: false,
            lock_scroll: false,
            op,
            chunks: Vec::new(),
            current_chunk: None,
            last_three_way_content: None,
            ai_suggest_pending: false,
            _ai_suggest_task: None,
            split_state: ThreeWaySplitState::new(false),
            title,
            _subscriptions: subscriptions,
        }
    }

    pub fn conflicts(&self) -> &[ConflictedFile] {
        &self.conflicts
    }

    pub fn op(&self) -> Option<InProgressOp> {
        self.op
    }

    pub fn lock_scroll(&self) -> bool {
        self.lock_scroll
    }

    pub fn show_base(&self) -> bool {
        self.show_base
    }

    pub fn current_chunk(&self) -> Option<usize> {
        self.current_chunk
    }

    pub fn chunks(&self) -> &[ConflictChunk] {
        &self.chunks
    }

    /// Switch focus to a given conflicted file and load its three-way
    /// content. Spawns `git show :N:<path>` requests in the background.
    pub fn activate_file(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.conflicts.len() {
            return;
        }
        let entry = self.conflicts[idx].clone();
        self.active_file = Some(idx);

        if entry.is_binary {
            self.text_state = None;
            self.last_three_way_content = None;
            self.binary_view = Some(
                cx.new(|_| BinaryConflictView::new(entry.path.clone(), self.work_dir.clone())),
            );
            cx.notify();
            return;
        }

        self.binary_view = None;
        self.last_three_way_content = None;
        let work_dir = self.work_dir.clone();
        let path = entry.path.clone();
        let task = cx.background_spawn(async move { load_three_way_async(&work_dir, &path).await });

        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(content) => this.populate_text_state(entry.path.clone(), content, window, cx),
                Err(err) => log::warn!("conflict resolver: load three-way failed: {err:?}"),
            })
            .log_err();
        })
        .detach();
    }

    fn populate_text_state(
        &mut self,
        path: git::repository::RepoPath,
        content: ThreeWayContent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Cache the unmodified ThreeWayContent for downstream consumers
        // (AI suggest, eventual chunk-by-chunk re-merge tooling).
        self.last_three_way_content = Some(content.clone());
        let local_text = content.ours.unwrap_or_default();
        let their_text = content.theirs.unwrap_or_default();
        let base_text = content.base.unwrap_or_default();
        let working_text = content.working;

        let local_editor = make_readonly_editor(&local_text, window, cx);
        let result_editor = make_result_editor(&working_text, window, cx);
        let their_editor = make_readonly_editor(&their_text, window, cx);
        let base_editor = if self.show_base {
            Some(make_readonly_editor(&base_text, window, cx))
        } else {
            None
        };

        let mut subs = Vec::new();
        let local_handle = local_editor.read(cx).focus_handle(cx);
        subs.push(cx.on_focus_in(&local_handle, window, |this, _, cx| {
            if this.last_focused != ResolverPane::Local {
                this.last_focused = ResolverPane::Local;
                cx.notify();
            }
        }));
        let result_handle = result_editor.read(cx).focus_handle(cx);
        subs.push(cx.on_focus_in(&result_handle, window, |this, _, cx| {
            if this.last_focused != ResolverPane::Result {
                this.last_focused = ResolverPane::Result;
                cx.notify();
            }
        }));
        let their_handle = their_editor.read(cx).focus_handle(cx);
        subs.push(cx.on_focus_in(&their_handle, window, |this, _, cx| {
            if this.last_focused != ResolverPane::Their {
                this.last_focused = ResolverPane::Their;
                cx.notify();
            }
        }));
        if let Some(base) = &base_editor {
            let base_handle = base.read(cx).focus_handle(cx);
            subs.push(cx.on_focus_in(&base_handle, window, |this, _, cx| {
                if this.last_focused != ResolverPane::Base {
                    this.last_focused = ResolverPane::Base;
                    cx.notify();
                }
            }));
        }
        // recompute current chunk on cursor move
        subs.push(cx.subscribe(
            &result_editor,
            |this, _, event: &EditorEvent, cx| match event {
                EditorEvent::SelectionsChanged { .. } | EditorEvent::Edited { .. } => {
                    this.recompute_current_chunk(cx);
                }
                _ => {}
            },
        ));

        self.text_state = Some(TextResolverState {
            path,
            local_editor,
            result_editor,
            their_editor,
            base_editor,
            _subscriptions: subs,
        });

        self.chunks = extract_conflict_chunks(&working_text);
        self.current_chunk = if self.chunks.is_empty() {
            None
        } else {
            Some(0)
        };
        cx.notify();
    }

    fn recompute_current_chunk(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.text_state.as_ref() else {
            return;
        };
        let current_text = state.result_editor.read(cx).text(cx);
        self.chunks = extract_conflict_chunks(&current_text);
        if self.chunks.is_empty() {
            self.current_chunk = None;
        } else if self.current_chunk.map_or(true, |c| c >= self.chunks.len()) {
            self.current_chunk = Some(0);
        }
        cx.notify();
    }

    pub fn focused_pane(&self) -> ResolverPane {
        self.last_focused
    }

    pub fn focused_editor(&self) -> Option<Entity<Editor>> {
        let state = self.text_state.as_ref()?;
        let editor = match self.last_focused {
            ResolverPane::Local => state.local_editor.clone(),
            ResolverPane::Result => state.result_editor.clone(),
            ResolverPane::Their => state.their_editor.clone(),
            ResolverPane::Base => state.base_editor.clone()?,
        };
        Some(editor)
    }

    /// Replace the current chunk's marker region in the Result buffer
    /// with `replacement`. Re-extracts chunks afterward.
    pub fn replace_current_chunk_with(
        &mut self,
        replacement: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.current_chunk else {
            return;
        };
        let Some(state) = self.text_state.as_ref() else {
            return;
        };
        let chunk = match self.chunks.get(idx) {
            Some(c) => c.clone(),
            None => return,
        };

        let result_editor = state.result_editor.clone();
        let _ = window;
        result_editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let max = snapshot.len().0;
            let start = chunk.range.start.min(max);
            let end = chunk.range.end.min(max);
            let anchor_start = snapshot.anchor_before(MultiBufferOffset(start));
            let anchor_end = snapshot.anchor_after(MultiBufferOffset(end));
            editor.edit([(anchor_start..anchor_end, replacement.to_string())], cx);
        });
        self.recompute_current_chunk(cx);
    }

    pub fn accept_yours(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self.current_chunk else {
            return;
        };
        let Some(chunk) = self.chunks.get(idx).cloned() else {
            return;
        };
        self.replace_current_chunk_with(&chunk.ours, window, cx);
    }

    pub fn accept_theirs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self.current_chunk else {
            return;
        };
        let Some(chunk) = self.chunks.get(idx).cloned() else {
            return;
        };
        self.replace_current_chunk_with(&chunk.theirs, window, cx);
    }

    pub fn accept_both(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self.current_chunk else {
            return;
        };
        let Some(chunk) = self.chunks.get(idx).cloned() else {
            return;
        };
        let mut combined = chunk.ours.clone();
        if !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&chunk.theirs);
        self.replace_current_chunk_with(&combined, window, cx);
    }

    pub fn accept_base(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self.current_chunk else {
            return;
        };
        let Some(chunk) = self.chunks.get(idx).cloned() else {
            return;
        };
        let base = chunk.base.unwrap_or_default();
        self.replace_current_chunk_with(&base, window, cx);
    }

    pub fn next_chunk(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.chunks.is_empty() {
            return;
        }
        let next = match self.current_chunk {
            Some(c) if c + 1 < self.chunks.len() => c + 1,
            Some(c) => c,
            None => 0,
        };
        self.current_chunk = Some(next);
        self.scroll_to_current_chunk(cx);
        cx.notify();
    }

    pub fn prev_chunk(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.chunks.is_empty() {
            return;
        }
        let prev = match self.current_chunk {
            Some(0) => 0,
            Some(c) => c - 1,
            None => 0,
        };
        self.current_chunk = Some(prev);
        self.scroll_to_current_chunk(cx);
        cx.notify();
    }

    fn scroll_to_current_chunk(&self, _cx: &mut Context<Self>) {
        // Editor scroll APIs operate on Anchors; we leave precise scroll for
        // a follow-up. Toolbar Prev/Next still updates the highlighted chunk
        // and provides feedback via the chunk counter in the toolbar.
    }

    /// Toolbar action: rebuild the result buffer by stripping every
    /// chunk that has identical ours/theirs (i.e. trivially auto-merged
    /// regions). The remaining chunks stay marker-delimited so the user
    /// resolves them explicitly.
    pub fn apply_non_conflicting_hunks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.text_state.as_ref() else {
            return;
        };
        let editor = state.result_editor.clone();
        let text = editor.read(cx).text(cx);
        let chunks = extract_conflict_chunks(&text);
        if chunks.is_empty() {
            return;
        }

        let mut new_text = String::with_capacity(text.len());
        let mut cursor = 0;
        for chunk in &chunks {
            new_text.push_str(&text[cursor..chunk.range.start]);
            if chunk.ours == chunk.theirs {
                new_text.push_str(&chunk.ours);
            } else {
                new_text.push_str(&text[chunk.range.clone()]);
            }
            cursor = chunk.range.end;
        }
        new_text.push_str(&text[cursor..]);

        editor.update(cx, |editor, cx| {
            editor.set_text(new_text, window, cx);
        });
        self.recompute_current_chunk(cx);
    }

    pub fn toggle_show_base(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_base = !self.show_base;
        self.split_state.toggle_base();

        // Rebuild base editor lazily next time we activate a file with base
        // content. For now, if we have a path active, just reopen it.
        if let Some(idx) = self.active_file {
            self.activate_file(idx, window, cx);
        } else {
            cx.notify();
        }
    }

    pub fn toggle_lock_scroll(&mut self, cx: &mut Context<Self>) {
        self.lock_scroll = !self.lock_scroll;
        cx.notify();
    }

    /// Save the result buffer to disk + `git add <path>`. Marks the file
    /// as resolved in the sidebar.
    pub fn mark_resolved(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(state) = self.text_state.as_ref() else {
            return Task::ready(Err(anyhow::anyhow!("no active file")));
        };
        let path = state.path.clone();
        let text = state.result_editor.read(cx).text(cx);
        let work_dir = self.work_dir.clone();

        cx.spawn(async move |this, cx| {
            cx.background_spawn({
                let path = path.clone();
                let work_dir = work_dir.to_path_buf();
                let text = text.clone();
                async move {
                    let abs = work_dir.join(path.as_std_path());
                    if let Some(parent) = abs.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&abs, text)?;
                    let path_str = path.as_std_path().to_string_lossy().into_owned();
                    crate::operations::run_git_void(&work_dir, &["add", "--", &path_str]).await
                }
            })
            .await?;

            this.update(cx, |this, cx| {
                this.resolved_files.push(path);
                this.refresh_conflict_list(cx);
            })
            .ok();
            Ok(())
        })
    }

    /// Re-run `git ls-files -u --stage` to refresh the conflicts list.
    /// Drives the sidebar progress counter and triggers the auto-prompt
    /// when zero files remain.
    pub fn refresh_conflict_list(&mut self, cx: &mut Context<Self>) {
        let work_dir = self.work_dir.clone();
        cx.spawn(async move |this, cx| {
            let conflicts = cx
                .background_spawn(async move { list_conflicts_async(&work_dir).await })
                .await
                .log_err()
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                this.conflicts = conflicts.clone();
                this.sidebar
                    .update(cx, |sidebar, _| sidebar.set_files(conflicts));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-load the working content from index, dropping any local edits
    /// to the result buffer. Mirrors `git checkout --merge -- <path>`
    /// which restores the conflict markers.
    pub fn revert_to_original(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self.active_file else { return };
        let work_dir = self.work_dir.clone();
        let path = self.conflicts[idx].path.clone();
        let path_str = path.as_std_path().to_string_lossy().into_owned();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_spawn(async move {
                crate::operations::run_git_void(
                    &work_dir,
                    &["checkout", "--merge", "--", &path_str],
                )
                .await
            })
            .await
            .log_err();
            this.update_in(cx, |this, window, cx| {
                if let Some(idx) = this.active_file {
                    this.activate_file(idx, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Three-way content for the active file, cached at file-open time.
    /// Used by the AI suggest button to compute size/binary eligibility
    /// without re-running `git show`.
    pub fn current_three_way_content(&self) -> Option<&ThreeWayContent> {
        self.last_three_way_content.as_ref()
    }

    /// True when the AI merge button should currently be in its
    /// "spinner" state (i.e. a suggestion is being generated). The
    /// toolbar reads this to disable the button during the in-flight
    /// turn.
    pub fn ai_suggest_pending(&self) -> bool {
        self.ai_suggest_pending
    }

    /// Whether an AI merge suggestion is feasible for the active file
    /// right now: text (not binary), under the size cap, and there is
    /// an active Solution to host the ephemeral session. The toolbar
    /// uses this for the button's `disabled` state.
    pub fn ai_suggest_eligible(&self, cx: &App) -> bool {
        let Some(entry) = self.active_file.and_then(|i| self.conflicts.get(i)) else {
            return false;
        };
        let Some(content) = self.last_three_way_content.as_ref() else {
            return false;
        };
        crate::ai_suggest::is_eligible(content, entry.is_binary) && has_active_solution(cx)
    }

    /// Tooltip text explaining why the AI merge button is disabled, or
    /// `None` when it would be enabled.
    pub fn ai_suggest_disabled_reason(&self, cx: &App) -> Option<&'static str> {
        let entry = self.active_file.and_then(|i| self.conflicts.get(i))?;
        let content = self.last_three_way_content.as_ref()?;
        crate::ai_suggest::ineligibility_reason(content, entry.is_binary, has_active_solution(cx))
    }

    /// Replace the entire Result buffer with `text`. Called by the AI
    /// suggest modal when the user clicks Apply. Does NOT save to disk
    /// — `mark_resolved` still has to be triggered explicitly.
    pub fn replace_result_with(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.text_state.as_ref() else {
            return;
        };
        let editor = state.result_editor.clone();
        editor.update(cx, |editor, cx| {
            editor.set_text(text.to_string(), window, cx);
        });
        self.recompute_current_chunk(cx);
    }

    /// Toolbar action: kick off an AI merge suggestion for the active
    /// file. On success, opens [`crate::ai_suggest_modal::AiSuggestModal`]
    /// with the proposed content; the user explicitly Applies or
    /// Cancels. On failure, shows a toast via the workspace.
    pub fn request_ai_suggest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_suggest_pending {
            return;
        }
        let Some(state) = self.text_state.as_ref() else {
            return;
        };
        let Some(content) = self.last_three_way_content.clone() else {
            return;
        };
        let path = state.path.clone();
        let result_text = state.result_editor.read(cx).text(cx);
        let project = self.project.clone();
        let work_dir = self.work_dir.clone();
        let workspace = self.workspace.clone();
        let resolver_weak = cx.weak_entity();

        self.ai_suggest_pending = true;
        cx.notify();

        let task = cx.spawn_in(window, async move |this, cx| {
            let outcome = crate::ai_suggest::suggest_merge(
                &path,
                &content,
                &project,
                work_dir.as_ref(),
                &mut cx.clone(),
            )
            .await;

            this.update(cx, |this, cx| {
                this.ai_suggest_pending = false;
                cx.notify();
            })
            .ok();

            match outcome {
                Ok(suggestion) => {
                    let Some(workspace) = workspace.upgrade() else {
                        return;
                    };
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            let resolver = resolver_weak.clone();
                            workspace.toggle_modal(window, cx, move |window, cx| {
                                crate::ai_suggest_modal::AiSuggestModal::new(
                                    resolver,
                                    result_text,
                                    suggestion,
                                    window,
                                    cx,
                                )
                            });
                        })
                        .ok();
                }
                Err(err) => {
                    log::warn!("AI merge suggestion failed: {err:#}");
                    let Some(workspace) = workspace.upgrade() else {
                        return;
                    };
                    let message = format!("AI merge unavailable: {err}");
                    cx.update(|_window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace.show_notification(
                                workspace::notifications::NotificationId::unique::<
                                    AiMergeFailureNotification,
                                >(),
                                cx,
                                |cx| {
                                    cx.new(|cx| {
                                        workspace::notifications::simple_message_notification::MessageNotification::new(
                                            message,
                                            cx,
                                        )
                                    })
                                },
                            );
                        });
                    })
                    .ok();
                }
            }
        });
        self._ai_suggest_task = Some(task);
    }

    pub fn work_dir(&self) -> &Arc<Path> {
        &self.work_dir
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub fn project(&self) -> &Entity<Project> {
        &self.project
    }

    pub fn workspace(&self) -> WeakEntity<Workspace> {
        self.workspace.clone()
    }

    fn render_resize_handle(&self, boundary: Boundary, cx: &Context<Self>) -> AnyElement {
        let separator_color = cx.theme().colors().border_variant;
        let id = match boundary {
            Boundary::LocalResult => "cfl-handle-lr",
            Boundary::ResultTheir => "cfl-handle-rt",
            Boundary::TheirBase => "cfl-handle-tb",
        };
        div()
            .id(id)
            .relative()
            .h_full()
            .flex_shrink_0()
            .w(px(1.0))
            .bg(separator_color)
            .child(
                div()
                    .id("cfl-handle-thumb")
                    .absolute()
                    .left(px(-RESIZE_HANDLE_WIDTH / 2.0))
                    .w(px(RESIZE_HANDLE_WIDTH))
                    .h_full()
                    .cursor_col_resize()
                    .on_drag(DraggedHandle { boundary }, |_, _, _, cx| {
                        cx.new(|_| gpui::Empty)
                    }),
            )
            .into_any_element()
    }

    fn pane(editor: Entity<Editor>, flex: f32, label: &'static str, cx: &App) -> AnyElement {
        v_flex()
            .h_full()
            .flex_shrink(1.)
            .min_w_0()
            .flex_basis(gpui::DefiniteLength::Fraction(flex))
            .overflow_hidden()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .bg(cx.theme().colors().elevated_surface_background)
                    .child(
                        Label::new(label)
                            .size(ui::LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(div().flex_1().min_h_0().child(editor))
            .into_any_element()
    }
}

fn make_result_editor(
    text: &str,
    window: &mut Window,
    cx: &mut Context<ConflictResolverView>,
) -> Entity<Editor> {
    let buffer = cx.new(|cx| Buffer::local(text.to_string(), cx));
    cx.new(|cx| {
        let mut editor = Editor::for_buffer(buffer, None, window, cx);
        editor.set_minimap_visibility(MinimapVisibility::Disabled, window, cx);
        editor
    })
}

fn make_readonly_editor(
    text: &str,
    window: &mut Window,
    cx: &mut Context<ConflictResolverView>,
) -> Entity<Editor> {
    let buffer = cx.new(|cx| Buffer::local(text.to_string(), cx));
    cx.new(|cx| {
        let mut editor = Editor::for_buffer(buffer, None, window, cx);
        editor.set_read_only(true);
        editor.disable_inline_diagnostics();
        editor.disable_diagnostics(cx);
        editor.set_show_vertical_scrollbar(false, cx);
        editor.set_minimap_visibility(MinimapVisibility::Disabled, window, cx);
        editor
    })
}

/// Marker type for the failure notification produced when an AI merge
/// suggestion fails. `NotificationId::unique::<T>()` keys notifications
/// by type, so each fork-local notification needs a distinct empty
/// struct. The marker is private — only this file produces these.
struct AiMergeFailureNotification;

/// Returns true when there is at least one Solution registered in the
/// SolutionStore (regardless of whether one is currently focused). The
/// AI suggest button needs *some* Solution to host the ephemeral
/// session — without one, `pick_active_solution` errors and the toast
/// path fires anyway, but disabling the button up front is friendlier.
fn has_active_solution(cx: &App) -> bool {
    solutions::SolutionStore::try_global(cx)
        .map(|store| !store.read(cx).solutions().is_empty())
        .unwrap_or(false)
}

/// Resolve the .git directory for `work_dir`. For a regular checkout this
/// is `<work_dir>/.git`. For a worktree, `.git` is a file containing
/// `gitdir: <abs_path>` — we follow that one level.
fn compute_git_dir(work_dir: &Path) -> PathBuf {
    let dot_git = work_dir.join(".git");
    if dot_git.is_file() {
        if let Ok(contents) = std::fs::read_to_string(&dot_git) {
            for line in contents.lines() {
                if let Some(rest) = line.strip_prefix("gitdir:") {
                    let p = Path::new(rest.trim());
                    if p.is_absolute() {
                        return p.to_path_buf();
                    } else {
                        return work_dir.join(p);
                    }
                }
            }
        }
    }
    dot_git
}

impl Render for ConflictResolverView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let main = if let Some(state) = &self.text_state {
            let local = state.local_editor.clone().into_any_element();
            let result = state.result_editor.clone().into_any_element();
            let their = state.their_editor.clone().into_any_element();

            let mut row = h_flex()
                .id("conflict-resolver-row")
                .size_full()
                .min_h_0()
                .on_drag_move::<DraggedHandle>(cx.listener(
                    |this, event: &DragMoveEvent<DraggedHandle>, _window, cx| {
                        let bounds = event.bounds;
                        let bounds_width = bounds.right() - bounds.left();
                        if bounds_width <= Pixels::ZERO {
                            return;
                        }
                        let pos = event.event.position.x - bounds.left();
                        let total: f32 = this.split_state.ratios.iter().sum();
                        let new_left_ratio = (pos / bounds_width) * total;
                        let bound = event.drag(cx).boundary;
                        let target_left_total = match bound {
                            Boundary::LocalResult => this.split_state.flex(0),
                            Boundary::ResultTheir => {
                                this.split_state.flex(0) + this.split_state.flex(1)
                            }
                            Boundary::TheirBase => {
                                this.split_state.flex(0)
                                    + this.split_state.flex(1)
                                    + this.split_state.flex(2)
                            }
                        };
                        let delta = new_left_ratio - target_left_total;
                        this.split_state.move_boundary(bound, delta);
                        cx.notify();
                    },
                ))
                .child(Self::pane(
                    state.local_editor.clone(),
                    self.split_state.flex(0),
                    "Yours",
                    cx,
                ))
                .child(self.render_resize_handle(Boundary::LocalResult, cx))
                .child(Self::pane(
                    state.result_editor.clone(),
                    self.split_state.flex(1),
                    "Result",
                    cx,
                ))
                .child(self.render_resize_handle(Boundary::ResultTheir, cx))
                .child(Self::pane(
                    state.their_editor.clone(),
                    self.split_state.flex(2),
                    "Their",
                    cx,
                ));

            if self.show_base {
                if let Some(base_editor) = &state.base_editor {
                    row = row
                        .child(self.render_resize_handle(Boundary::TheirBase, cx))
                        .child(Self::pane(
                            base_editor.clone(),
                            self.split_state.flex(3),
                            "Base",
                            cx,
                        ));
                }
            }

            // Suppress unused locals when text_state is preserved across renders.
            let _ = (local, result, their);
            row.into_any_element()
        } else if let Some(binary) = &self.binary_view {
            binary.clone().into_any_element()
        } else {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(Label::new("Select a conflicted file from the sidebar"))
                .into_any_element()
        };

        let toolbar = toolbar::render_toolbar(self, window, cx);
        let bottom = toolbar::render_bottom_bar(self, window, cx);
        let header = render_header(&self.title, &self.work_dir, self.active_path(), cx);

        h_flex()
            .id("conflict-resolver")
            .size_full()
            .child(self.sidebar.clone())
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(header)
                    .child(toolbar)
                    .child(div().flex_1().min_h_0().w_full().child(main))
                    .child(bottom),
            )
    }
}

fn render_header(
    title: &SharedString,
    work_dir: &Arc<Path>,
    path: Option<&git::repository::RepoPath>,
    cx: &App,
) -> AnyElement {
    let work_dir_str = work_dir.display().to_string();
    let detail = match path {
        Some(p) => format!("{}/{}", work_dir_str, p.as_std_path().display()),
        None => work_dir_str,
    };
    h_flex()
        .px_3()
        .py_2()
        .gap_2()
        .border_b_1()
        .border_color(cx.theme().colors().border)
        .child(Icon::new(IconName::Warning).color(Color::Warning))
        .child(Label::new(title.clone()))
        .child(
            Label::new(detail)
                .color(Color::Muted)
                .size(ui::LabelSize::Small),
        )
        .into_any_element()
}

impl ConflictResolverView {
    pub fn active_path(&self) -> Option<&git::repository::RepoPath> {
        self.text_state.as_ref().map(|s| &s.path)
    }

    pub fn resolved_count(&self) -> usize {
        self.resolved_files.len()
    }

    pub fn total_count(&self) -> usize {
        self.conflicts.len() + self.resolved_files.len()
    }
}

impl EventEmitter<()> for ConflictResolverView {}

impl Focusable for ConflictResolverView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if let Some(state) = &self.text_state {
            let editor = match self.last_focused {
                ResolverPane::Local => &state.local_editor,
                ResolverPane::Result => &state.result_editor,
                ResolverPane::Their => &state.their_editor,
                ResolverPane::Base => state.base_editor.as_ref().unwrap_or(&state.result_editor),
            };
            editor.read(cx).focus_handle(cx)
        } else {
            // Fall back to the sidebar handle.
            self.sidebar.read(cx).focus_handle(cx)
        }
    }
}

impl Item for ConflictResolverView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Warning).color(Color::Warning))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
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
        Some("Conflict Resolver Opened")
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
        } else if type_id == TypeId::of::<Editor>() {
            self.text_state
                .as_ref()
                .map(|s| match self.last_focused {
                    ResolverPane::Local => s.local_editor.clone(),
                    ResolverPane::Result => s.result_editor.clone(),
                    ResolverPane::Their => s.their_editor.clone(),
                    ResolverPane::Base => s
                        .base_editor
                        .clone()
                        .unwrap_or_else(|| s.result_editor.clone()),
                })
                .map(Into::into)
        } else {
            None
        }
    }
}
