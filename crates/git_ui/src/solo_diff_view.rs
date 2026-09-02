use crate::{
    commit_blob::{LoadedBlob, load_commit_file_blob},
    git_panel::GitStatusEntry,
    git_status_icon, soft_wrap_button,
};
use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use editor::{
    Direction, Editor, EditorEvent, EditorSettings, SplittableEditor, ToggleSplitDiff,
    actions::{GoToHunk, GoToPreviousHunk, ToggleSoftWrap},
    multibuffer_context_lines,
};
use fs::Fs;
use git::repository::RepoPath;
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, Render, Task, WeakEntity, Window,
};
use language::{Buffer, Capability, HighlightedText};
use multi_buffer::{MultiBuffer, MultiBufferSnapshot};
use project::{
    Project,
    git_store::{Repository, RepositoryId},
};
use settings::{DiffViewStyle, Settings, update_settings_file};
use std::{
    any::{Any, TypeId},
    cell::Cell,
    sync::Arc,
};
use ui::{
    Color, DiffStat, Icon, IconButton, IconName, Label, LabelCommon as _, SharedString, Tooltip,
    prelude::*, vertical_divider,
};
use util::paths::{PathExt as _, PathStyle};
use workspace::{
    Item, ItemHandle, ItemNavHistory, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace,
    item::{ItemEvent, PreviewTabsSettings, SaveOptions, TabContentParams, TabTooltipContent},
    searchable::SearchableItemHandle,
};

/// Memoized `MultiBufferSnapshot::diff_hunks().count()`.
///
/// Counting hunks walks every excerpt and every hunk of the multibuffer with a
/// couple of O(log n) anchor resolutions per hunk. That is nothing for a
/// single-file diff, but a project diff over several hundred files repaints its
/// toolbar often enough that redoing the walk each time is worth avoiding.
///
/// Every component of the key is an O(1) sum-tree summary read: `edit_count`
/// moves on any buffer edit, `non_text_state_update_count` on any buffer
/// non-text change, and the changed-row totals on any change to the diff itself
/// (including the async arrival of the initial diff). Staging does not move the
/// key, and correctly so — it only rewrites secondary hunk status, never the
/// hunk count.
#[derive(Default)]
pub(crate) struct HunkCountCache(Cell<Option<(HunkCountKey, usize)>>);

#[derive(Clone, Copy, PartialEq, Eq)]
struct HunkCountKey {
    edit_count: usize,
    non_text_state_update_count: usize,
    added_rows: u32,
    removed_rows: u32,
}

impl HunkCountCache {
    pub(crate) fn count(&self, snapshot: &MultiBufferSnapshot) -> usize {
        let (added_rows, removed_rows) = snapshot.total_changed_lines();
        let key = HunkCountKey {
            edit_count: snapshot.edit_count(),
            non_text_state_update_count: snapshot.non_text_state_update_count(),
            added_rows,
            removed_rows,
        };
        if let Some((cached_key, count)) = self.0.get()
            && cached_key == key
        {
            return count;
        }
        let count = snapshot.diff_hunks().count();
        self.0.set(Some((key, count)));
        count
    }
}

/// What the user did to ask for a diff, as opposed to where the resulting tab
/// should land.
///
/// The git panel's two tabs share one diff tab per pane, so the placement is
/// never the caller's decision: a summoned diff always takes the pane's
/// preview slot and a retarget always reuses whatever is already in it.
/// Naming the gesture instead of the destination is what keeps the two tabs
/// from drifting apart again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffOpen {
    /// Double click, or Enter. Opens the shared diff if it is not open.
    /// `focus` moves the keyboard into it, which only Enter does — a click
    /// leaves focus in the panel so the next click keeps landing on file rows.
    Summon { focus: bool },
    /// Single click, or an arrow-key step through the list. Only ever changes
    /// what an already-open shared diff is showing.
    Retarget,
}

impl DiffOpen {
    fn focuses(self) -> bool {
        matches!(self, Self::Summon { focus: true })
    }
}

/// What [`SoloDiffView::resolve_gesture`] decided about a gesture, before
/// anything has been loaded.
enum GestureOutcome {
    /// A view of this exact source was already open; it has been activated.
    Reused(Entity<SoloDiffView>),
    /// The gesture may only retarget, and there is no shared diff tab open to
    /// retarget. Nothing happens at all.
    Declined,
    /// Load the source and build a view for it.
    Load,
}

/// What a [`SoloDiffView`] is showing, and therefore what it can do.
///
/// Every difference between the two modes is derived from this value, so the
/// view can never end up half-configured for one of them.
#[derive(Clone)]
pub enum DiffSource {
    /// Uncommitted changes to a file in the working tree. The right-hand side
    /// is the live project buffer, so the view is editable and its hunks can
    /// be staged.
    WorkingTree {
        repository: Entity<Repository>,
        repo_path: RepoPath,
    },
    /// A file as of `sha`, diffed against its parent. Both sides are detached
    /// historic blobs: read-only, no staging.
    Commit {
        repository: Entity<Repository>,
        repo_path: RepoPath,
        sha: SharedString,
    },
}

impl DiffSource {
    pub fn repository(&self) -> &Entity<Repository> {
        match self {
            Self::WorkingTree { repository, .. } | Self::Commit { repository, .. } => repository,
        }
    }

    pub fn repo_path(&self) -> &RepoPath {
        match self {
            Self::WorkingTree { repo_path, .. } | Self::Commit { repo_path, .. } => repo_path,
        }
    }

    /// The commit this diff is taken from, or `None` for a working-tree diff.
    pub fn sha(&self) -> Option<&SharedString> {
        match self {
            Self::WorkingTree { .. } => None,
            Self::Commit { sha, .. } => Some(sha),
        }
    }

    /// Whether the right-hand side is a file the user can edit and save.
    fn is_editable(&self) -> bool {
        matches!(self, Self::WorkingTree { .. })
    }

    /// Whether two views show the same thing, and so should be one tab.
    ///
    /// A working-tree diff is identified by its repository — the same relative
    /// path can exist in a second repository in the window — and a commit diff
    /// by its sha, so the same file at two revisions gets two tabs.
    fn matches(&self, other: &Self, cx: &App) -> bool {
        match (self, other) {
            (
                Self::WorkingTree {
                    repository,
                    repo_path,
                },
                Self::WorkingTree {
                    repository: other_repository,
                    repo_path: other_repo_path,
                },
            ) => {
                repository.read(cx).id == other_repository.read(cx).id
                    && repo_path == other_repo_path
            }
            (
                Self::Commit { sha, repo_path, .. },
                Self::Commit {
                    sha: other_sha,
                    repo_path: other_repo_path,
                    ..
                },
            ) => sha == other_sha && repo_path == other_repo_path,
            _ => false,
        }
    }

    /// The revision `git blame` should annotate the left-hand pane's text
    /// with, or `None` to leave left-pane blame off.
    fn blame_base(&self) -> Option<SharedString> {
        match self {
            // The left pane holds the file's content at HEAD.
            Self::WorkingTree { .. } => Some("HEAD".into()),
            // Deliberately unwired, not merely unset: the left pane's text is
            // a detached historic blob, and `SplittableEditor::
            // sync_lhs_blame_sources` resolves the `(repository, repo_path)`
            // to blame through `repository_and_path_for_buffer_id` on the
            // *right-hand* buffer id. A blob that is not in the project's
            // buffer store cannot answer that, so every source it builds is
            // dropped. Wiring it needs an explicit repository override on
            // `SplittableEditor`.
            Self::Commit { .. } => None,
        }
    }

    fn tab_icon(&self) -> IconName {
        match self {
            Self::WorkingTree { .. } => IconName::Diff,
            Self::Commit { .. } => IconName::GitCommit,
        }
    }

    /// The file's basename, which is what the tab is titled with for either
    /// source. Falls back to the whole path for a path with no last component.
    fn tab_title(&self) -> SharedString {
        let repo_path = self.repo_path();
        match repo_path.file_name() {
            Some(file_name) => file_name.to_string().into(),
            None => repo_path
                .as_ref()
                .display(PathStyle::local())
                .into_owned()
                .into(),
        }
    }
}

/// The buffers a [`DiffSource`] resolves to, as produced by whichever entry
/// point loaded it. Kept apart from [`DiffSource`] because the two shapes are
/// genuinely different: a working-tree file is one project buffer plus its
/// uncommitted diff, while a commit file is a pair of detached blobs that also
/// carry the excerpt ranges and the binary flag the loader worked out.
enum LoadedDiff {
    WorkingTree {
        buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
    },
    Commit(LoadedBlob),
}

impl LoadedDiff {
    fn buffer(&self) -> &Entity<Buffer> {
        match self {
            Self::WorkingTree { buffer, .. } => buffer,
            Self::Commit(blob) => &blob.buffer,
        }
    }
}

pub struct SoloDiffView {
    source: DiffSource,
    repository_id: RepositoryId,
    buffer: Entity<Buffer>,
    editor: Entity<SplittableEditor>,
    workspace: WeakEntity<Workspace>,
    hunk_count_cache: HunkCountCache,
}

impl SoloDiffView {
    /// Open (or retarget) the shared diff of an uncommitted change.
    ///
    /// `Ok(None)` is the gesture legitimately doing nothing: a retarget with
    /// no shared diff tab open to retarget.
    pub fn open_or_focus(
        entry: GitStatusEntry,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        mode: DiffOpen,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<Entity<Self>>>> {
        let Some(workspace_entity) = workspace.upgrade() else {
            return Task::ready(Err(anyhow::anyhow!("workspace was dropped")));
        };

        let source = DiffSource::WorkingTree {
            repository: repository.clone(),
            repo_path: entry.repo_path.clone(),
        };
        match Self::resolve_gesture(&workspace_entity, &source, mode, window, cx) {
            GestureOutcome::Reused(existing) => return Task::ready(Ok(Some(existing))),
            GestureOutcome::Declined => return Task::ready(Ok(None)),
            GestureOutcome::Load => {}
        }

        let Some(project_path) = repository
            .read(cx)
            .repo_path_to_project_path(&entry.repo_path, cx)
        else {
            return Task::ready(Err(anyhow::anyhow!(
                "could not resolve repository path {:?}",
                entry.repo_path
            )));
        };

        let project = workspace_entity.read(cx).project().clone();
        window.spawn(cx, async move |cx| {
            let buffer = project
                .update(cx, |project, cx| {
                    project.open_buffer(project_path.clone(), cx)
                })
                .await?;
            let diff = project
                .update(cx, |project, cx| {
                    project.open_uncommitted_diff(buffer.clone(), cx)
                })
                .await?;

            workspace_entity.update_in(cx, |workspace, window, cx| {
                let workspace_handle = cx.entity();
                let view = cx.new(|cx| {
                    Self::new(
                        project,
                        source,
                        LoadedDiff::WorkingTree { buffer, diff },
                        workspace_handle,
                        window,
                        cx,
                    )
                });
                Self::add_to_pane(workspace, &view, mode, window, cx);
                Some(view)
            })
        })
    }

    /// Open (or retarget) the shared diff to a read-only view of `repo_path`
    /// as of `sha`, against its parent.
    ///
    /// Reuses an already-open view of the same `(sha, repo_path)` rather than
    /// loading the commit a second time. `Ok(None)` is the gesture
    /// legitimately doing nothing: a retarget with no shared diff tab open to
    /// retarget.
    pub fn open_commit_file(
        sha: SharedString,
        repository: Entity<Repository>,
        repo_path: RepoPath,
        workspace: WeakEntity<Workspace>,
        mode: DiffOpen,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<Entity<Self>>>> {
        let Some(workspace_entity) = workspace.upgrade() else {
            return Task::ready(Err(anyhow::anyhow!("workspace was dropped")));
        };

        let source = DiffSource::Commit {
            repository: repository.clone(),
            repo_path: repo_path.clone(),
            sha: sha.clone(),
        };
        match Self::resolve_gesture(&workspace_entity, &source, mode, window, cx) {
            GestureOutcome::Reused(existing) => return Task::ready(Ok(Some(existing))),
            GestureOutcome::Declined => return Task::ready(Ok(None)),
            GestureOutcome::Load => {}
        }

        let project = workspace_entity.read(cx).project().clone();
        let language_registry = project.read(cx).languages().clone();
        // A file the commit deleted has no project path to resolve, so the
        // loader needs a worktree to attach its synthetic file to.
        let fallback_worktree_id = project
            .read(cx)
            .worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).id());
        let commit_diff = repository.update(cx, |repository, _| {
            repository.load_commit_diff(sha.to_string())
        });

        window.spawn(cx, async move |cx| {
            let commit_diff = commit_diff
                .await
                .context("loading the commit's diff was cancelled")??;
            let file = commit_diff
                .files
                .into_iter()
                .find(|file| file.path == repo_path)
                .with_context(|| {
                    format!(
                        "commit {sha} does not contain {}",
                        repo_path.as_ref().display(PathStyle::local())
                    )
                })?;
            let blob = load_commit_file_blob(
                file,
                &sha,
                &repository,
                fallback_worktree_id,
                &language_registry,
                cx,
            )
            .await?;

            workspace_entity.update_in(cx, |workspace, window, cx| {
                let workspace_handle = cx.entity();
                let view = cx.new(|cx| {
                    Self::new(
                        project,
                        source,
                        LoadedDiff::Commit(blob),
                        workspace_handle,
                        window,
                        cx,
                    )
                });
                Self::add_to_pane(workspace, &view, mode, window, cx);
                Some(view)
            })
        })
    }

    /// Whether the active pane's preview slot holds a single-file diff — i.e.
    /// whether there is a shared diff tab for a retarget gesture to point
    /// somewhere else.
    ///
    /// One guard for both of the git panel's tabs. They summon the same item
    /// type into the same slot, so "is the shared diff open?" is the whole
    /// question either of them has to ask, and a Changes click retargeting a
    /// commit's diff (or the reverse) is the point rather than an accident.
    fn preview_holds_a_diff(workspace: &Entity<Workspace>, cx: &App) -> bool {
        let workspace = workspace.read(cx);
        let Some(preview_id) = workspace.active_pane().read(cx).preview_item_id() else {
            return false;
        };
        workspace
            .items_of_type::<SoloDiffView>(cx)
            .any(|view| view.entity_id() == preview_id)
    }

    /// The prologue of the one open algorithm both git-panel tabs run —
    /// everything that can be decided before the source is loaded.
    ///
    /// Nothing here pins: `unpreview_item_if_preview` would promote the shared
    /// diff out of the preview slot, and the next single click would then find
    /// the slot empty and summon a *second* tab. Pinning stays reachable
    /// through the editor's own double-click-on-the-tab gesture.
    fn resolve_gesture(
        workspace: &Entity<Workspace>,
        source: &DiffSource,
        mode: DiffOpen,
        window: &mut Window,
        cx: &mut App,
    ) -> GestureOutcome {
        // The guard comes before the reuse search, and the order matters: a
        // retarget with no shared diff tab open must do *nothing*, not quietly
        // activate a matching view that happens to be pinned somewhere. That
        // is what the pre-unification code did — the Changes tab checked the
        // preview slot in `move_diff_to_entry` and never reached the open call
        // at all — and it is the difference between arrow-stepping down the
        // list and arrow-stepping down the list while a pane jumps to a pinned
        // diff and never jumps back.
        if mode == DiffOpen::Retarget && !Self::preview_holds_a_diff(workspace, cx) {
            return GestureOutcome::Declined;
        }

        // A duplicate tab for the same source is never what the user asked
        // for, whichever gesture got them here — and the search is
        // workspace-wide, so a diff parked in another pane is found too.
        let existing = workspace
            .read(cx)
            .items_of_type::<SoloDiffView>(cx)
            .find(|item| item.read(cx).source.matches(source, cx));
        if let Some(existing) = existing {
            let focus = mode.focuses();
            workspace.update(cx, |workspace, cx| {
                workspace.activate_item(&existing, true, focus, window, cx);
            });
            if focus {
                existing.focus_handle(cx).focus(window, cx);
            }
            return GestureOutcome::Reused(existing);
        }

        GestureOutcome::Load
    }

    /// Put a freshly-built view into the active pane.
    fn add_to_pane(
        workspace: &mut Workspace,
        view: &Entity<Self>,
        mode: DiffOpen,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let item: Box<dyn ItemHandle> = Box::new(view.clone());
        workspace.active_pane().update(cx, |pane, cx| {
            if PreviewTabsSettings::get_global(cx).enabled {
                // FORK.md #54: there is no `allow_preview` flag on `add_item`;
                // claiming the shared slot means replacing its current
                // occupant and reusing the index it vacated.
                let destination_index = pane.replace_preview_item_id(item.item_id(), window, cx);
                pane.add_item(item, true, mode.focuses(), destination_index, window, cx);
            } else {
                // With previews off there is no shared slot to keep clicking
                // into, so every open is a deliberate permanent tab and takes
                // focus rather than being stranded behind the panel.
                pane.add_item(item, true, true, None, window, cx);
            }
        });
    }

    fn new(
        project: Entity<Project>,
        source: DiffSource,
        loaded: LoadedDiff,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The pair decides the capability, the multibuffer shape and the hunk
        // controls between them, so a mismatched pair would build a view that
        // is read-only in one respect and editable in another. Both entry
        // points construct the two together; this is the invariant that keeps
        // a third one from getting it wrong.
        debug_assert!(
            matches!(
                (&source, &loaded),
                (
                    DiffSource::WorkingTree { .. },
                    LoadedDiff::WorkingTree { .. }
                ) | (DiffSource::Commit { .. }, LoadedDiff::Commit(_))
            ),
            "a DiffSource must be built with the buffers that were loaded for it"
        );
        let repository_id = source.repository().read(cx).id;
        let buffer = loaded.buffer().clone();
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = match &loaded {
                LoadedDiff::WorkingTree { buffer, diff } => {
                    // A live project buffer brings its own capability, which is
                    // what makes this side editable.
                    let mut multibuffer = MultiBuffer::singleton(buffer.clone(), cx);
                    multibuffer.add_diff(diff.clone(), cx);
                    multibuffer
                }
                // Read-only-ness lives here, not on the buffer: the historic
                // blob itself is built `ReadWrite`. The file's name is already
                // in the tab, so a path header would be redundant chrome.
                LoadedDiff::Commit(_) => MultiBuffer::without_headers(Capability::ReadOnly),
            };
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            let mut editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer,
                project.clone(),
                workspace.clone(),
                window,
                cx,
            );

            if let LoadedDiff::Commit(blob) = &loaded {
                // History has nothing to stage or revert.
                editor.disable_diff_hunk_controls(cx);
                editor.rhs_editor().update(cx, |editor, cx| {
                    editor.set_show_diff_review_button(true, cx);
                });
                editor.update_excerpts_for_path(
                    blob.path_key.clone(),
                    blob.buffer.clone(),
                    blob.excerpt_ranges.clone(),
                    multibuffer_context_lines(cx),
                    blob.diff.clone(),
                    cx,
                );
                if blob.is_binary {
                    // The excerpt is a "(binary file not shown)" placeholder;
                    // folding it says so without pretending to be a diff.
                    let buffer_id = blob.buffer.read(cx).remote_id();
                    editor.rhs_editor().update(cx, |editor, cx| {
                        editor.fold_buffers([buffer_id], cx);
                    });
                }
            }

            // After the excerpts exist: `sync_lhs_blame_sources` drops entries
            // whose base buffer is not excerpted.
            editor.set_lhs_blame_base(source.blame_base(), cx);

            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_should_serialize(false, cx);
                let snapshot = editor.snapshot(window, cx);
                editor.go_to_hunk_before_or_after_position(
                    &snapshot,
                    language::Point::new(0, 0),
                    Direction::Next,
                    true,
                    window,
                    cx,
                );
            });
            editor
        });

        // No `SettingsStore` observer here: `SplittableEditor::new` installs
        // one that toggles the split whenever the setting stops matching the
        // editor's own style, which is strictly what a second one here could
        // do.
        Self {
            source,
            repository_id,
            buffer,
            editor,
            workspace: workspace.downgrade(),
            hunk_count_cache: HunkCountCache::default(),
        }
    }

    /// What this view is showing. Callers that need to tell a working-tree
    /// diff from a commit's diff branch on this rather than on the tab title.
    pub fn source(&self) -> &DiffSource {
        &self.source
    }

    /// The repository this diff belongs to. Read by the git panel to decide
    /// whether one of *its* rows is the diff the pane is showing; the same
    /// relative path can exist in a second repository in the window.
    pub fn repository_id(&self) -> RepositoryId {
        self.repository_id
    }

    /// The file this diff is showing, relative to the repository root.
    pub fn repo_path(&self) -> &RepoPath {
        self.source.repo_path()
    }

    /// Number of diff hunks in this file's diff. Also gates the prev/next-hunk
    /// buttons: with a single hunk there is nowhere to navigate to.
    fn hunk_count(&self, cx: &App) -> usize {
        let editor = self.editor.read(cx).rhs_editor().read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        self.hunk_count_cache.count(&snapshot)
    }

    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut App) {
        self.focus_handle(cx).focus(window, cx);
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        });
    }
}

impl EventEmitter<EditorEvent> for SoloDiffView {}

impl Focusable for SoloDiffView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for SoloDiffView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(self.source.tab_icon()).color(Color::Muted))
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
        self.source.tab_title()
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let repo_relative = || {
            self.source
                .repo_path()
                .as_ref()
                .display(PathStyle::local())
                .into_owned()
        };
        Some(match &self.source {
            // The working tree's file is on disk, so name it the way the rest
            // of the editor does: an absolute path with `~` folded back in.
            DiffSource::WorkingTree { .. } => self
                .buffer
                .read(cx)
                .file()
                .map(|file| file.full_path(cx).compact().to_string_lossy().into_owned())
                .unwrap_or_else(repo_relative)
                .into(),
            // A commit's file has no location on disk to name.
            DiffSource::Commit { .. } => repo_relative().into(),
        })
    }

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        let text = self.tab_tooltip_text(cx)?;
        let DiffSource::Commit { sha, .. } = &self.source else {
            return Some(TabTooltipContent::Text(text));
        };
        // Which revision the file is from is the whole point of this tab, and
        // the title carries only the basename — so say it on a second line.
        let sha = sha.get(0..16).unwrap_or(sha).to_string();
        Some(TabTooltipContent::Custom(Box::new(Tooltip::element(
            move |_, _| {
                v_flex()
                    .child(Label::new(text.clone()))
                    .child(
                        Label::new(format!("at {sha}"))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            },
        ))))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Solo Diff View Opened")
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.deactivated(window, cx);
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else {
            self.editor.act_as_type(type_id, cx)
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.for_each_project_item(cx, f)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            })
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor.update(cx, |editor, cx| {
            editor
                .rhs_editor()
                .update(cx, |editor, cx| editor.navigate(data, window, cx))
        })
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        // Defer to the embedded editor: respects `toolbar.breadcrumbs`
        // (hidden by default in this fork — the tab already names the file).
        self.editor.breadcrumb_location(cx)
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<gpui::Font>)> {
        self.editor.breadcrumbs(cx)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.added_to_workspace(workspace, window, cx)
            })
        });
    }

    fn can_save(&self, cx: &App) -> bool {
        // A commit's file is history: both sides are detached blobs, with
        // nowhere on disk to write back to.
        self.source.is_editable() && self.editor.read(cx).rhs_editor().read(cx).can_save(cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if !self.source.is_editable() {
            return Task::ready(Ok(()));
        }
        self.editor.save(options, project, window, cx)
    }
}

impl Render for SoloDiffView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.editor.clone()
    }
}

pub struct SoloDiffStyleToolbar {
    solo_diff: Option<WeakEntity<SoloDiffView>>,
}

pub struct SoloDiffGitToolbar {
    solo_diff: Option<WeakEntity<SoloDiffView>>,
}

impl SoloDiffStyleToolbar {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self { solo_diff: None }
    }

    fn solo_diff(&self) -> Option<Entity<SoloDiffView>> {
        self.solo_diff.as_ref()?.upgrade()
    }

    fn set_diff_view_style(
        &mut self,
        diff_view_style: DiffViewStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(solo_diff) = self.solo_diff() else {
            return;
        };
        let workspace = solo_diff.read(cx).workspace.clone();

        update_settings_file(<dyn Fs>::global(cx), cx, move |settings, _| {
            settings.editor.diff_view_style = Some(diff_view_style);
        });

        if let Some(workspace) = workspace.upgrade() {
            let splittable_editors = {
                workspace
                    .read(cx)
                    .items(cx)
                    .filter_map(|item| item.act_as_type(TypeId::of::<SplittableEditor>(), cx))
                    .filter_map(|item| item.downcast::<SplittableEditor>().ok())
                    .collect::<Vec<_>>()
            };

            for editor in splittable_editors {
                editor.update(cx, |editor, cx| {
                    if editor.diff_view_style() != diff_view_style {
                        editor.toggle_split(&ToggleSplitDiff, window, cx);
                    }
                });
            }
        }

        cx.notify();
    }
}

impl EventEmitter<ToolbarItemEvent> for SoloDiffStyleToolbar {}

impl ToolbarItemView for SoloDiffStyleToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.solo_diff = active_pane_item
            .and_then(|item| item.act_as::<SoloDiffView>(cx))
            .map(|entity| entity.downgrade());
        if self.solo_diff.is_some() {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl SoloDiffStyleToolbar {
    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(solo_diff) = self.solo_diff() {
            solo_diff.update(cx, |solo_diff, cx| {
                solo_diff.dispatch_action(action, window, cx);
            });
        }
    }
}

impl Render for SoloDiffStyleToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(solo_diff) = self.solo_diff() else {
            return div();
        };
        let focus_handle = solo_diff.focus_handle(cx);
        let prev_next = solo_diff.read(cx).hunk_count(cx) > 1;
        let editor_entity = solo_diff.read(cx).editor.clone();
        let editor = editor_entity.read(cx);
        let diff_view_style = editor.diff_view_style();
        let is_soft_wrap_enabled = editor.is_soft_wrap_enabled(cx);
        let is_split_set = diff_view_style == DiffViewStyle::Split;
        let split_icon = if is_split_set && !editor.is_split() {
            IconName::DiffSplitAuto
        } else {
            IconName::DiffSplit
        };

        h_flex()
            .h_8()
            .items_center()
            .gap_1()
            // IDEA puts change navigation first in the diff toolbar.
            .child(
                IconButton::new("solo-diff-prev", IconName::ArrowUp)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title_in(
                        "Go to previous hunk",
                        &GoToPreviousHunk,
                        &focus_handle,
                    ))
                    .disabled(!prev_next)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch_action(&GoToPreviousHunk, window, cx)
                    })),
            )
            .child(
                IconButton::new("solo-diff-next", IconName::ArrowDown)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title_in(
                        "Go to next hunk",
                        &GoToHunk,
                        &focus_handle,
                    ))
                    .disabled(!prev_next)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch_action(&GoToHunk, window, cx)
                    })),
            )
            .child(vertical_divider())
            .child(
                IconButton::new("solo-diff-unified", IconName::DiffUnified)
                    .icon_size(IconSize::Small)
                    .toggle_state(diff_view_style == DiffViewStyle::Unified)
                    .tooltip(Tooltip::text("Unified"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_diff_view_style(DiffViewStyle::Unified, window, cx);
                    })),
            )
            .child(
                IconButton::new("solo-diff-split", split_icon)
                    .icon_size(IconSize::Small)
                    .toggle_state(diff_view_style == DiffViewStyle::Split)
                    .tooltip(Tooltip::text("Split"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_diff_view_style(DiffViewStyle::Split, window, cx);
                    })),
            )
            .child(vertical_divider())
            .child(
                soft_wrap_button("solo-diff-soft-wrap", is_soft_wrap_enabled, &focus_handle)
                    .icon_size(IconSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch_action(&ToggleSoftWrap, window, cx)
                    })),
            )
            .child(vertical_divider())
            .child(div().w_1())
    }
}

impl SoloDiffGitToolbar {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self { solo_diff: None }
    }

    fn solo_diff(&self) -> Option<Entity<SoloDiffView>> {
        self.solo_diff.as_ref()?.upgrade()
    }
}

impl EventEmitter<ToolbarItemEvent> for SoloDiffGitToolbar {}

impl ToolbarItemView for SoloDiffGitToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.solo_diff = active_pane_item
            .and_then(|item| item.act_as::<SoloDiffView>(cx))
            .map(|entity| entity.downgrade());
        if self.solo_diff.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl Render for SoloDiffGitToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(solo_diff) = self.solo_diff() else {
            return div();
        };
        let solo_diff = solo_diff.read(cx);
        let hunk_count = solo_diff.hunk_count(cx);
        let status_entry = solo_diff
            .source
            .repository()
            .read(cx)
            .status_for_path(solo_diff.source.repo_path());
        let status = status_entry.as_ref().map(|entry| entry.status);
        let diff_stat = status_entry.and_then(|entry| entry.diff_stat);

        h_group_xl()
            .my_neg_1()
            .py_1()
            .items_center()
            .flex_wrap()
            .justify_between()
            .children(status.map(|status| git_status_icon(status).into_any_element()))
            .children(diff_stat.map(|stat| {
                DiffStat::new("solo-diff-stat", stat.added as usize, stat.deleted as usize)
                    .into_any_element()
            }))
            .child(
                Label::new(difference_count_label(hunk_count))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().w_1())
    }
}

/// IntelliJ IDEA's diff toolbar ends with a bare count of differences, worded
/// exactly like this. Zero is rendered rather than hidden so the toolbar slot
/// keeps a stable width and an empty diff reads as "nothing left to review"
/// instead of "the count is missing".
pub(crate) fn difference_count_label(count: usize) -> String {
    if count == 1 {
        "1 difference".to_string()
    } else {
        format!("{count} differences")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::repository::{CommitDiff, CommitFile, repo_path};
    use git::status::{FileStatus, StageStatus, StatusCode, TrackedStatus};
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext};
    use project::FakeFs;
    use settings::SettingsStore;
    use std::path::Path;
    use util::path;
    use workspace::MultiWorkspace;

    /// Two full shas — `load_commit_file_blob` shortens them itself, so a
    /// stub-length sha would not exercise the same code.
    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";

    #[test]
    fn difference_count_label_is_singular_only_at_one() {
        assert_eq!(difference_count_label(0), "0 differences");
        assert_eq!(difference_count_label(1), "1 difference");
        assert_eq!(difference_count_label(2), "2 differences");
        assert_eq!(difference_count_label(17), "17 differences");
    }

    fn init_test(cx: &mut TestAppContext) {
        zlog::init_test();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
            let store = solutions::SolutionStore::for_test(std::path::PathBuf::new(), cx);
            solutions::install_global_for_test(store, cx);
        });
    }

    struct DiffTestContext {
        workspace: Entity<Workspace>,
        repository: Entity<Repository>,
        fs: Arc<FakeFs>,
    }

    /// A one-repository workspace whose `a.rs` has an uncommitted change, so
    /// both entry points have something real to open.
    async fn diff_test_context(cx: &mut TestAppContext) -> (DiffTestContext, VisualTestContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/project"),
            serde_json::json!({
                ".git": {},
                "a.rs": "one\nTWO\nthree\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            path!("/project/.git").as_ref(),
            &[("a.rs", "one\ntwo\nthree\n".into())],
        );

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the test window holds a workspace");
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.run_until_parked();

        let repository = workspace
            .update_in(&mut cx, |workspace, _window, cx| {
                workspace.project().read(cx).active_repository(cx)
            })
            .expect("the fake project exposes its repository");

        (
            DiffTestContext {
                workspace,
                repository,
                fs,
            },
            cx,
        )
    }

    fn commit_file(path: &str, old_text: Option<&str>, new_text: Option<&str>) -> CommitFile {
        CommitFile {
            path: repo_path(path),
            old_text: old_text.map(str::to_string),
            new_text: new_text.map(str::to_string),
            is_binary: false,
        }
    }

    fn set_commit(context: &DiffTestContext, sha: &str, files: Vec<CommitFile>) {
        context
            .fs
            .set_commit_diff(path!("/project/.git").as_ref(), sha, CommitDiff { files });
    }

    async fn open_commit_with(
        context: &DiffTestContext,
        sha: &str,
        path: &str,
        mode: DiffOpen,
        cx: &mut VisualTestContext,
    ) -> Result<Option<Entity<SoloDiffView>>> {
        let open = cx.update(|window, cx| {
            SoloDiffView::open_commit_file(
                sha.into(),
                context.repository.clone(),
                repo_path(path),
                context.workspace.downgrade(),
                mode,
                window,
                cx,
            )
        });
        let view = open.await;
        cx.run_until_parked();
        view
    }

    async fn open_commit(
        context: &DiffTestContext,
        sha: &str,
        path: &str,
        cx: &mut VisualTestContext,
    ) -> Result<Option<Entity<SoloDiffView>>> {
        open_commit_with(context, sha, path, DiffOpen::Summon { focus: false }, cx).await
    }

    async fn open_working_tree(
        context: &DiffTestContext,
        path: &str,
        cx: &mut VisualTestContext,
    ) -> Result<Option<Entity<SoloDiffView>>> {
        let entry = GitStatusEntry {
            repo_path: repo_path(path),
            status: FileStatus::Tracked(TrackedStatus {
                index_status: StatusCode::Unmodified,
                worktree_status: StatusCode::Modified,
            }),
            staging: StageStatus::Unstaged,
            diff_stat: None,
        };
        let open = cx.update(|window, cx| {
            SoloDiffView::open_or_focus(
                entry,
                context.repository.clone(),
                context.workspace.downgrade(),
                DiffOpen::Summon { focus: false },
                window,
                cx,
            )
        });
        let view = open.await;
        cx.run_until_parked();
        view
    }

    fn open_view_count(context: &DiffTestContext, cx: &mut VisualTestContext) -> usize {
        context.workspace.update_in(cx, |workspace, _window, cx| {
            workspace.items_of_type::<SoloDiffView>(cx).count()
        })
    }

    #[gpui::test]
    async fn test_a_commit_source_opens_a_read_only_view(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(
            &context,
            SHA,
            vec![commit_file(
                "src/lib.rs",
                Some("one\ntwo\nthree\n"),
                Some("one\nTWO\nthree\n"),
            )],
        );

        let view = open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");

        view.read_with(&cx, |view, cx| {
            assert!(matches!(view.source(), DiffSource::Commit { .. }));
            assert_eq!(view.source().sha().map(SharedString::as_ref), Some(SHA));
            assert_eq!(view.tab_content_text(0, cx), "lib.rs");
            assert_eq!(view.source().tab_icon(), IconName::GitCommit);
            assert_eq!(
                view.tab_tooltip_text(cx).as_deref(),
                Some("src/lib.rs"),
                "a commit's file has no on-disk path, so the tooltip names the \
                 repo-relative one"
            );
            assert!(
                matches!(
                    view.tab_tooltip_content(cx),
                    Some(TabTooltipContent::Custom(_))
                ),
                "the commit tooltip is two lines — the path and the revision"
            );
            assert!(
                !view.can_save(cx),
                "a commit's file has nowhere on disk to be saved back to"
            );
            assert!(
                view.editor
                    .read(cx)
                    .rhs_editor()
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .read_only(),
                "the commit source's multibuffer must be read-only"
            );
            assert_eq!(
                view.hunk_count(cx),
                1,
                "the loaded blob's excerpts must actually reach the editor"
            );
        });
    }

    #[gpui::test]
    async fn test_a_working_tree_source_stays_editable(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;

        let view = open_working_tree(&context, "a.rs", &mut cx)
            .await
            .expect("the working-tree file opens")
            .expect("the gesture opened a view");

        view.read_with(&cx, |view, cx| {
            assert!(matches!(view.source(), DiffSource::WorkingTree { .. }));
            assert_eq!(view.source().sha(), None);
            assert_eq!(view.tab_content_text(0, cx), "a.rs");
            assert_eq!(view.source().tab_icon(), IconName::Diff);
            assert!(
                matches!(
                    view.tab_tooltip_content(cx),
                    Some(TabTooltipContent::Text(_))
                ),
                "a working-tree diff has no revision to name on a second line"
            );
            assert!(view.can_save(cx), "an uncommitted diff stays saveable");
            assert!(
                !view
                    .editor
                    .read(cx)
                    .rhs_editor()
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .read_only(),
                "the working-tree source's multibuffer must stay writable"
            );
        });
    }

    #[gpui::test]
    async fn test_the_same_commit_file_reuses_one_view(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(
            &context,
            SHA,
            vec![commit_file("src/lib.rs", Some("one\n"), Some("two\n"))],
        );

        let first = open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");
        let second = open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the second open resolves")
            .expect("the gesture opened a view");

        assert_eq!(first, second, "the same (sha, path) is one tab");
        assert_eq!(open_view_count(&context, &mut cx), 1);
    }

    #[gpui::test]
    async fn test_a_second_sha_opens_a_second_view(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(
            &context,
            SHA,
            vec![commit_file("src/lib.rs", Some("one\n"), Some("two\n"))],
        );
        set_commit(
            &context,
            OTHER_SHA,
            vec![commit_file("src/lib.rs", Some("two\n"), Some("three\n"))],
        );

        // Previews off, so each open gets its own tab: with the shared preview
        // slot in play the second open would evict the first, which says
        // nothing about whether the two sources are considered distinct.
        cx.update(|_window, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.preview_tabs.get_or_insert_default().enabled = Some(false);
                });
            });
        });

        let first = open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the first commit's file opens")
            .expect("the gesture opened a view");
        let second = open_commit(&context, OTHER_SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the second commit's file opens")
            .expect("the gesture opened a view");

        assert_ne!(
            first, second,
            "the same file at two revisions is two different diffs"
        );
        assert_eq!(open_view_count(&context, &mut cx), 2);
    }

    #[gpui::test]
    async fn test_a_commit_that_does_not_touch_the_path_is_an_error(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(
            &context,
            SHA,
            vec![commit_file("src/other.rs", Some("one\n"), Some("two\n"))],
        );

        let error = open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect_err("a file the commit never touched cannot be shown");

        assert!(
            error.to_string().contains("src/lib.rs"),
            "the error must name the file that was asked for, got {error}"
        );
        assert_eq!(
            open_view_count(&context, &mut cx),
            0,
            "a failed open must not leave an empty tab behind"
        );
    }

    #[gpui::test]
    async fn test_the_blame_base_follows_the_source(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(
            &context,
            SHA,
            vec![commit_file("src/lib.rs", Some("one\n"), Some("two\n"))],
        );

        let working_tree = open_working_tree(&context, "a.rs", &mut cx)
            .await
            .expect("the working-tree file opens")
            .expect("the gesture opened a view");
        let commit = open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");

        working_tree.read_with(&cx, |view, cx| {
            assert_eq!(
                view.editor
                    .read(cx)
                    .lhs_blame_base()
                    .map(SharedString::as_ref),
                Some("HEAD"),
                "the left pane of an uncommitted diff is the file at HEAD"
            );
        });
        commit.read_with(&cx, |view, cx| {
            assert_eq!(
                view.editor.read(cx).lhs_blame_base(),
                None,
                "commit-mode blame is deliberately unwired"
            );
        });
    }

    /// History has nothing to stage or restore, so a commit source must take
    /// the hunk controls away — and a working-tree source must not.
    #[gpui::test]
    async fn test_only_the_commit_source_takes_the_hunk_controls_away(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(
            &context,
            SHA,
            vec![commit_file("src/lib.rs", Some("one\n"), Some("two\n"))],
        );

        let working_tree = open_working_tree(&context, "a.rs", &mut cx)
            .await
            .expect("the working-tree file opens")
            .expect("the gesture opened a view");
        working_tree.read_with(&cx, |view, cx| {
            assert!(
                !view.editor.read(cx).diff_hunk_controls_disabled(),
                "an uncommitted hunk can still be staged or restored"
            );
        });

        let commit = open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");
        commit.read_with(&cx, |view, cx| {
            assert!(
                view.editor.read(cx).diff_hunk_controls_disabled(),
                "a commit's hunks have nothing to stage or restore"
            );
        });
    }

    /// The flag behind [`SplittableEditor::diff_hunk_controls_disabled`] must
    /// track the renderer, not merely the first call: a view that disables the
    /// controls at construction and installs its own later still paints them.
    #[gpui::test]
    async fn test_installing_a_renderer_undoes_disabled_hunk_controls(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(
            &context,
            SHA,
            vec![commit_file("src/lib.rs", Some("one\n"), Some("two\n"))],
        );

        let commit = open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");

        commit.update(&mut cx, |view, cx| {
            view.editor.update(cx, |editor, cx| {
                assert!(editor.diff_hunk_controls_disabled());
                editor.set_render_diff_hunk_controls(
                    Arc::new(|_, _: &_, _, _, _, _: &_, _: &mut _, _: &mut _| {
                        gpui::Empty.into_any_element()
                    }),
                    cx,
                );
                assert!(
                    !editor.diff_hunk_controls_disabled(),
                    "a renderer installed after the fact takes the controls back"
                );
            });
        });
    }

    /// The half of the gesture model that lives in the open algorithm itself:
    /// a retarget with no shared diff tab open is not an error and not a
    /// summon, it is nothing at all.
    #[gpui::test]
    async fn test_a_retarget_with_no_open_diff_does_nothing(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(
            &context,
            SHA,
            vec![
                commit_file("src/lib.rs", Some("one\n"), Some("two\n")),
                commit_file("src/other.rs", Some("three\n"), Some("four\n")),
            ],
        );

        let declined = open_commit_with(&context, SHA, "src/lib.rs", DiffOpen::Retarget, &mut cx)
            .await
            .expect("a declined gesture is not an error");
        assert!(declined.is_none(), "there was nothing to retarget");
        assert_eq!(open_view_count(&context, &mut cx), 0);

        open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");
        // A *different* file, so the retarget has to go through the shared
        // slot rather than short-circuiting on the already-open view.
        let retargeted =
            open_commit_with(&context, SHA, "src/other.rs", DiffOpen::Retarget, &mut cx)
                .await
                .expect("the retarget resolves")
                .expect("with the shared diff open, a retarget reaches it");
        assert_eq!(
            retargeted.read_with(&cx, |view, _| view.repo_path().clone()),
            repo_path("src/other.rs"),
        );
        assert_eq!(
            open_view_count(&context, &mut cx),
            1,
            "and reuses the one tab rather than adding a second"
        );
    }

    /// `SoloDiffView` used to install its own `SettingsStore` observer on top
    /// of the one `SplittableEditor::new` already has. Removing ours must
    /// leave the unified/split toggle following the setting.
    #[gpui::test]
    async fn test_the_diff_view_style_setting_still_drives_the_split(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;

        let view = open_working_tree(&context, "a.rs", &mut cx)
            .await
            .expect("the working-tree file opens")
            .expect("the gesture opened a view");
        view.read_with(&cx, |view, cx| {
            assert_eq!(
                view.editor.read(cx).diff_view_style(),
                DiffViewStyle::Split,
                "this fork defaults to a side-by-side diff"
            );
            assert!(view.editor.read(cx).is_split());
        });

        cx.update(|_window, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Unified);
                })
            });
        });
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            assert_eq!(
                view.editor.read(cx).diff_view_style(),
                DiffViewStyle::Unified,
                "the setting must still reach the editor with no observer of our own"
            );
            assert!(
                !view.editor.read(cx).is_split(),
                "and actually collapse the second pane"
            );
        });
    }
}
