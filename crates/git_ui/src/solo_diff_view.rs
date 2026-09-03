use crate::{
    commit_blob::{LoadedBlob, load_commit_file_blob},
    git_panel::GitStatusEntry,
    git_status_icon, soft_wrap_button,
};
use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use editor::{
    DiffBlameBase, Direction, Editor, EditorEvent, EditorSettings, SplittableEditor,
    ToggleSplitDiff,
    actions::{GoToHunk, GoToPreviousHunk, ToggleSoftWrap},
    multibuffer_context_lines,
};
use fs::Fs;
use git::{
    BuildCommitPermalinkParams, GitHostingProviderRegistry, GitRemote, ParsedGitRemote,
    parse_git_remote_url, repository::RepoPath, status::FileStatus,
};
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
    ///
    /// The asymmetry is deliberate, not an omission: the commit arm ignores
    /// the repository because `(sha, path)` already identifies the *content*.
    /// A sha is content-addressed, so two repositories that both contain it
    /// contain the same blob at that path. The one visible consequence is in a
    /// Solution whose members are two clones of one repository: a Commit-tab
    /// click in member B reuses member A's open tab. The diff shown is
    /// identical, which is why this is a dedupe win rather than a bug — but if
    /// the tab ever grows a working-tree-relative affordance (reveal in the
    /// project panel, "open the file at HEAD"), the repository has to join the
    /// key at that point.
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

/// What `git blame` should annotate each pane's text with.
///
/// Not a method on [`DiffSource`] — per the rule in FORK.md #136, the source
/// owns an answer only when it can give it alone, and this one cannot. A
/// commit's file may not exist on both sides of its own diff (it was added, or
/// deleted), and it may be binary, and the pane text is then a placeholder
/// rather than the file. Those are facts of the *loaded blob*, so the
/// derivation lives where the blob's facts are in hand.
fn blame_base_for_source(
    source: &DiffSource,
    commit_file: Option<&CommitFileFacts>,
) -> Option<DiffBlameBase> {
    match source {
        // The left pane holds the file's content at HEAD; the right pane is
        // the live project buffer, which blame resolves on its own.
        DiffSource::WorkingTree { .. } => Some(DiffBlameBase::RhsFilesAt("HEAD".into())),
        DiffSource::Commit {
            repository,
            repo_path,
            sha,
        } => {
            let facts = commit_file?;
            if facts.binary_buffer_id.is_some() {
                // Both panes show "(binary file not shown)". There are no
                // lines of the file there to attribute to anyone.
                return None;
            }
            // The `None`s below state provenance — that side's text is not
            // any revision's — and one of them is load-bearing, for a
            // narrower reason than "git blame would fail". Where the missing
            // thing is the *path* (a deleted file at `sha`, an added file at
            // an ordinary commit's `<sha>^`) git answers `fatal: no such
            // path`, which `git::blame::run_git_blame` maps to an empty
            // blame, so an ungated call would only waste a subprocess. Where
            // the missing thing is the *revision* it would not: a root commit
            // has no `<sha>^`, git answers `fatal: bad revision`, that matches
            // no sentinel, and `GitBlame` raises it as a toast — once per
            // file, since `git show --name-status` reports every file of a
            // root commit as added.
            Some(DiffBlameBase::Blob {
                repository: repository.clone(),
                repo_path: repo_path.clone(),
                rhs_revision: (!facts.status.is_deleted()).then(|| sha.clone()),
                // `<sha>^` is the first parent, which is exactly what
                // `GitRepository::load_commit` diffs against (`git show
                // --first-parent`, `parent_sha = format!("{commit}^")`), so a
                // merge commit's two sides agree about which parent they mean.
                lhs_revision: (!facts.status.is_created())
                    .then(|| SharedString::from(format!("{sha}^"))),
            })
        }
    }
}

/// Everything a freshly-built [`SplittableEditor`] needs on top of
/// [`SplittableEditor::new`] to be the editor this source calls for.
///
/// [`SoloDiffView::new`] and `SoloDiffView::clone_on_split` both build one
/// over the same multibuffer, and a split pane that staged hunks the original
/// would not — or blamed a revision the original would not — is exactly the
/// drift this shared helper exists to prevent. The excerpts are *not* here:
/// they belong to the multibuffer, so the clone already has them.
fn configure_editor_for_source(
    editor: &mut SplittableEditor,
    source: &DiffSource,
    commit_file: Option<&CommitFileFacts>,
    cx: &mut Context<SplittableEditor>,
) {
    if let DiffSource::Commit { .. } = source {
        // History has nothing to stage or revert.
        editor.disable_diff_hunk_controls(cx);
        editor.rhs_editor().update(cx, |editor, cx| {
            editor.set_show_diff_review_button(true, cx);
        });
    }
    if let Some(buffer_id) = commit_file.and_then(|facts| facts.binary_buffer_id) {
        // The excerpt is a "(binary file not shown)" placeholder; folding it
        // says so without pretending to be a diff.
        editor.rhs_editor().update(cx, |editor, cx| {
            editor.fold_buffers([buffer_id], cx);
        });
    }
    // Hunk navigation and Unified/Split are painted by
    // `SoloDiffStyleToolbar`, which sits in the same `PrimaryLeft` slot as
    // `BufferSearchBar`'s copy of the same four buttons. Without this the
    // commit source draws both sets side by side — the working-tree source
    // escapes only because its multibuffer is a singleton, which is a
    // coincidence rather than a reason.
    editor.set_style_controls_painted_by_consumer(true);
    editor.set_blame_base(blame_base_for_source(source, commit_file), cx);
    editor.rhs_editor().update(cx, |editor, cx| {
        editor.set_should_serialize(false, cx);
    });
}

/// The repository's hosting provider, as `CommitView` parses it: the upstream
/// remote if there is one, otherwise `origin`.
fn parse_repository_remote(repository: &Entity<Repository>, cx: &mut App) -> Option<GitRemote> {
    let snapshot = repository.read(cx).snapshot();
    let remote_url = snapshot
        .remote_upstream_url
        .as_ref()
        .or(snapshot.remote_origin_url.as_ref())?;
    let provider_registry = GitHostingProviderRegistry::default_global(cx);
    parse_git_remote_url(provider_registry, remote_url).map(|(host, parsed)| GitRemote {
        host,
        owner: parsed.owner.into(),
        repo: parsed.repo.into(),
    })
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

/// What the commit-file loader worked out that the view still needs once the
/// `CommitFile` itself is gone.
#[derive(Clone)]
struct CommitFileFacts {
    /// Whether the commit added, modified or deleted the file. The toolbar's
    /// status icon has to come from here rather than from
    /// `Repository::status_for_path`, which answers about the *working tree*
    /// and would happily describe an unrelated uncommitted edit to the same
    /// path.
    status: FileStatus,
    /// `Some` when the excerpt is the "(binary file not shown)" placeholder.
    /// Folding is per-editor state, so a second editor over the same
    /// multibuffer has to fold it again for itself.
    binary_buffer_id: Option<language::BufferId>,
}

pub struct SoloDiffView {
    source: DiffSource,
    repository_id: RepositoryId,
    buffer: Entity<Buffer>,
    multibuffer: Entity<MultiBuffer>,
    editor: Entity<SplittableEditor>,
    /// Held rather than read back off the workspace: `clone_on_split` runs
    /// inside `Workspace::split_and_clone`, i.e. while the workspace entity is
    /// already being updated, and reading it there panics.
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    hunk_count_cache: HunkCountCache,
    /// `Some` exactly when the source is [`DiffSource::Commit`].
    commit_file: Option<CommitFileFacts>,
    /// The repository's hosting provider, parsed once at construction the way
    /// `CommitView` does it, for the "View on <provider>" permalink. `None`
    /// for a working-tree diff, which has no commit to link to.
    remote: Option<GitRemote>,
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
        // Always the *first* parent, which is what pairs with the hard-coded
        // `<sha>^` in `blame_base_for_source`: this surface has no merge-parent
        // toggle, so it never reaches `load_commit_diff_against_parent` and the
        // diff and its left-pane blame cannot name different parents. If it
        // ever gains that toggle, `lhs_revision` is already a parameter of
        // `DiffBlameBase::Blob` — chasing the choice through to `<sha>^N` is a
        // call-site change here, not a redesign of the blame seam.
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
        let commit_file = match &loaded {
            LoadedDiff::WorkingTree { .. } => None,
            LoadedDiff::Commit(blob) => Some(CommitFileFacts {
                status: blob.status,
                binary_buffer_id: blob.is_binary.then(|| blob.buffer.read(cx).remote_id()),
            }),
        };
        let remote = match &source {
            DiffSource::WorkingTree { .. } => None,
            DiffSource::Commit { repository, .. } => parse_repository_remote(repository, cx),
        };
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
                multibuffer.clone(),
                project.clone(),
                workspace.clone(),
                window,
                cx,
            );

            // The excerpts have to exist before the source's configuration is
            // applied: `sync_blame_sources` drops entries whose base
            // buffer is not excerpted. A clone skips this step — the excerpts
            // belong to the multibuffer, which it shares.
            if let LoadedDiff::Commit(blob) = &loaded {
                editor.update_excerpts_for_path(
                    blob.path_key.clone(),
                    blob.buffer.clone(),
                    blob.excerpt_ranges.clone(),
                    multibuffer_context_lines(cx),
                    blob.diff.clone(),
                    cx,
                );
            }
            configure_editor_for_source(&mut editor, &source, commit_file.as_ref(), cx);

            editor.rhs_editor().update(cx, |editor, cx| {
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
            multibuffer,
            editor,
            project,
            workspace: workspace.downgrade(),
            hunk_count_cache: HunkCountCache::default(),
            commit_file,
            remote,
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

    /// Everything [`SoloDiffGitToolbar`] paints about *what* this view is
    /// showing, resolved without a frame so the per-source rules can be
    /// tested without asserting on rendered elements.
    pub(crate) fn git_toolbar_content(&self, cx: &App) -> GitToolbarContent {
        let hunk_count = self.hunk_count(cx);
        match &self.source {
            DiffSource::WorkingTree {
                repository,
                repo_path,
            } => {
                // Read fresh on every render, with no subscription of our own:
                // the toolbar is repainted because the pane repaints it, so a
                // status that changed while this tab sat in the background is
                // only as fresh as the next repaint. That was already true
                // before this task; the commit branch below deliberately reads
                // nothing that can go stale.
                let status_entry = repository.read(cx).status_for_path(repo_path);
                GitToolbarContent {
                    status: status_entry.as_ref().map(|entry| entry.status),
                    diff_stat: status_entry.and_then(|entry| entry.diff_stat),
                    hunk_count,
                    commit: None,
                }
            }
            DiffSource::Commit { sha, .. } => {
                // Not `status_for_path`: the working tree may have its own,
                // unrelated change to this path, and describing a historic
                // diff with it would be a lie rather than merely stale.
                let (added, deleted) = self.multibuffer.read(cx).snapshot(cx).total_changed_lines();
                GitToolbarContent {
                    status: self.commit_file.as_ref().map(|facts| facts.status),
                    diff_stat: Some(git::status::DiffStat { added, deleted }),
                    hunk_count,
                    commit: Some(CommitToolbarInfo {
                        sha: sha.clone(),
                        short_sha: sha
                            .get(0..git::SHORT_SHA_LENGTH)
                            .unwrap_or(sha)
                            .to_string()
                            .into(),
                        permalink: self.remote.as_ref().map(|remote| {
                            let parsed_remote = ParsedGitRemote {
                                owner: remote.owner.as_ref().into(),
                                repo: remote.repo.as_ref().into(),
                            };
                            let url = remote
                                .host
                                .build_commit_permalink(
                                    &parsed_remote,
                                    BuildCommitPermalinkParams { sha },
                                )
                                .to_string();
                            (remote.host.name().into(), url)
                        }),
                    }),
                }
            }
        }
    }

    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut App) {
        self.focus_handle(cx).focus(window, cx);
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        });
    }
}

/// What [`SoloDiffGitToolbar`] shows, resolved from a [`SoloDiffView`].
///
/// The status and the `+N −M` figures are the same two slots for either
/// source but are read from completely different places, and only a commit
/// source contributes [`Self::commit`]. Keeping that as data rather than as
/// branches inside `render` is what makes it testable.
pub(crate) struct GitToolbarContent {
    pub(crate) status: Option<FileStatus>,
    pub(crate) diff_stat: Option<git::status::DiffStat>,
    pub(crate) hunk_count: usize,
    pub(crate) commit: Option<CommitToolbarInfo>,
}

/// The part of the toolbar only a [`DiffSource::Commit`] has.
pub(crate) struct CommitToolbarInfo {
    /// The full sha, which is what `git_panel::OpenAtCommit` and the
    /// permalink need — a short one finds nothing.
    pub(crate) sha: SharedString,
    /// The abbreviated sha, so the user can see *which* commit they are
    /// reading; the tab title carries only the file's basename.
    pub(crate) short_sha: SharedString,
    /// `(provider name, permalink)`, absent when the repository has no remote
    /// this fork recognises a hosting provider for.
    pub(crate) permalink: Option<(SharedString, String)>,
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
        // Forwarded to the `SplittableEditor`, not straight to its right pane:
        // its own hook re-points the handle it splits with and reaches *both*
        // panes, which is what every sibling diff view does. Reaching past it
        // left the left pane on whatever workspace it was born with.
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let project = self.project.clone();
        let diff_view_style = self.editor.read(cx).diff_view_style();
        // The same multibuffer, not a second one built from the same buffers:
        // the excerpts, the expanded-hunk state and the diff all live on it,
        // and a clone that rebuilt them would drift from the original the
        // moment either side changed.
        let multibuffer = self.multibuffer.clone();
        let source = self.source.clone();
        let commit_file = self.commit_file.clone();
        let repository_id = self.repository_id;
        let buffer = self.buffer.clone();
        let remote = self.remote.clone();

        Task::ready(Some(cx.new(|cx| {
            let editor = cx.new({
                let source = source.clone();
                let commit_file = commit_file.clone();
                // Reborrow `window` so the `move` closure consumes the
                // reborrow (which ends when `cx.new` returns) rather than the
                // caller's `&mut Window`.
                let window = &mut *window;
                move |cx| {
                    let mut editor = SplittableEditor::new(
                        diff_view_style,
                        multibuffer,
                        project,
                        workspace,
                        window,
                        cx,
                    );
                    configure_editor_for_source(&mut editor, &source, commit_file.as_ref(), cx);
                    editor
                }
            });
            Self {
                source,
                repository_id,
                buffer,
                multibuffer: self.multibuffer.clone(),
                editor,
                project: self.project.clone(),
                workspace: self.workspace.clone(),
                // Deliberately not shared: the memo is a `Cell` keyed on the
                // multibuffer's own summaries, so the clone repopulates it on
                // its first render and the two views never write to one cell.
                hunk_count_cache: HunkCountCache::default(),
                commit_file,
                remote,
            }
        })))
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
        let content = solo_diff.read(cx).git_toolbar_content(cx);

        h_group_xl()
            .my_neg_1()
            .py_1()
            .items_center()
            .flex_wrap()
            .justify_between()
            .children(
                content
                    .status
                    .map(|status| git_status_icon(status).into_any_element()),
            )
            .children(content.diff_stat.map(|stat| {
                DiffStat::new("solo-diff-stat", stat.added as usize, stat.deleted as usize)
                    .into_any_element()
            }))
            .child(
                Label::new(difference_count_label(content.hunk_count))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(content.commit.as_ref().map(|commit| {
                Label::new(commit.short_sha.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element()
            }))
            .child(vertical_divider())
            // Buffer search is offered for *either* source: `as_searchable`
            // already returns this view's editor whatever it is showing, so
            // the button is an affordance for something both sources can
            // already do, and one shared tab whose search button blinked out
            // whenever a click retargeted it from an uncommitted change to a
            // commit's file would be the exact chrome drift this refactor is
            // undoing. (`QuickActionBar`, which offers this button for a plain
            // editor, downcasts to `Editor` and so is hidden here.)
            .child(
                IconButton::new("solo-diff-buffer-search", IconName::MagnifyingGlass)
                    .icon_size(IconSize::Small)
                    .tooltip(move |_, cx| {
                        Tooltip::for_action(
                            "Buffer Search",
                            &zed_actions::buffer_search::Deploy::find(),
                            cx,
                        )
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(
                            Box::new(zed_actions::buffer_search::Deploy::find()),
                            cx,
                        );
                    }),
            )
            .children(content.commit.as_ref().map(|commit| {
                let sha = commit.sha.to_string();
                IconButton::new("solo-diff-show-in-git-graph", IconName::GitGraph)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Show in Git Graph"))
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(
                            Box::new(crate::git_panel::OpenAtCommit { sha: sha.clone() }),
                            cx,
                        );
                    })
                    .into_any_element()
            }))
            .children(
                content
                    .commit
                    .as_ref()
                    .and_then(|commit| commit.permalink.clone())
                    .map(|(provider_name, url)| {
                        IconButton::new(
                            "solo-diff-view-on-provider",
                            crate::get_provider_icon(&provider_name),
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text(format!("View on {provider_name}")))
                        .on_click(move |_, _, cx| cx.open_url(&url))
                        .into_any_element()
                    }),
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
    use search::BufferSearchBar;
    use settings::SettingsStore;
    use std::path::Path;
    use util::path;
    use workspace::ToolbarItemEvent;
    use workspace::{MultiWorkspace, SplitDirection};

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
        diff_test_context_with_remote(cx, None).await
    }

    /// The same fixture, with `origin` pointed at `remote_url` before the
    /// project is built — `SoloDiffView::new` parses the remote once, so it
    /// has to be in place before the view exists.
    async fn diff_test_context_with_remote(
        cx: &mut TestAppContext,
        remote_url: Option<&str>,
    ) -> (DiffTestContext, VisualTestContext) {
        init_test(cx);
        if remote_url.is_some() {
            cx.update(|cx| {
                git::GitHostingProviderRegistry::default_global(cx).register_hosting_provider(
                    Arc::new(git_hosting_providers::Github::public_instance()),
                );
            });
        }

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
        if let Some(remote_url) = remote_url {
            fs.set_remote_for_repo(path!("/project/.git").as_ref(), "origin", remote_url);
        }

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

    fn blame_entry_at(row: u32, author: &str) -> ::git::blame::BlameEntry {
        blame_entry_at_sha(row, author, run_sha(0xaa))
    }

    /// A sha built from one repeated byte, so a test can name two distinct
    /// commits without carrying two forty-character literals around.
    fn run_sha(byte: u8) -> ::git::Oid {
        ::git::Oid::from_bytes(&[byte; 20]).expect("twenty bytes is a valid oid")
    }

    fn blame_entry_at_sha(row: u32, author: &str, sha: ::git::Oid) -> ::git::blame::BlameEntry {
        ::git::blame::BlameEntry {
            sha,
            range: row..row + 1,
            original_line_number: row + 1,
            author: Some(author.to_string()),
            author_mail: None,
            author_time: Some(1_700_000_000),
            author_tz: None,
            committer_name: Some(author.to_string()),
            committer_email: None,
            committer_time: Some(1_700_000_000),
            committer_tz: None,
            summary: Some("a commit".to_string()),
            previous: None,
            filename: "a.rs".to_string(),
        }
    }

    fn binary_commit_file(path: &str) -> CommitFile {
        CommitFile {
            path: repo_path(path),
            old_text: Some("one\ntwo\n".to_string()),
            new_text: Some("one\nTWO\n".to_string()),
            is_binary: true,
        }
    }

    /// Teaches the fake repository to answer `git blame <revision> -- <path>`
    /// with `rows` annotated lines, at each of `revisions`.
    fn set_blame_at_revisions(
        context: &DiffTestContext,
        path: &str,
        revisions: impl IntoIterator<Item = String>,
        rows: u32,
    ) {
        for revision in revisions {
            context.fs.set_blame_at_revision_for_repo(
                Path::new(path!("/project/.git")),
                &revision,
                vec![(
                    repo_path(path),
                    ::git::blame::Blame {
                        entries: (0..rows).map(|row| blame_entry_at(row, "Tester")).collect(),
                        ..Default::default()
                    },
                )],
            );
        }
    }

    /// Like `set_blame_at_revisions`, but annotates row `i` with `row_shas[i]`,
    /// so a test can lay out explicit runs of consecutive lines per commit.
    fn set_blame_runs_at_revisions(
        context: &DiffTestContext,
        path: &str,
        revisions: impl IntoIterator<Item = String>,
        row_shas: &[::git::Oid],
    ) {
        for revision in revisions {
            context.fs.set_blame_at_revision_for_repo(
                Path::new(path!("/project/.git")),
                &revision,
                vec![(
                    repo_path(path),
                    ::git::blame::Blame {
                        entries: row_shas
                            .iter()
                            .enumerate()
                            .map(|(row, sha)| blame_entry_at_sha(row as u32, "Tester", *sha))
                            .collect(),
                        ..Default::default()
                    },
                )],
            );
        }
    }

    /// Turns blame on in both panes of an open commit diff — one at a time,
    /// each with the keyboard in it, because `GitBlame::generate` defers
    /// everything while the editor is blurred.
    fn blame_both_panes(view: &Entity<SoloDiffView>, cx: &mut VisualTestContext) {
        let splittable = view.read_with(&*cx, |view, _| view.editor.clone());
        let lhs = splittable
            .read_with(&*cx, |editor, _| editor.lhs_editor().cloned())
            .expect("a commit diff opens split");
        let rhs = splittable.read_with(&*cx, |editor, _| editor.rhs_editor().clone());
        for editor in [&lhs, &rhs] {
            editor.update_in(cx, |editor, window, cx| {
                editor.focus_handle(cx).focus(window, cx);
                editor.toggle_git_blame(&::git::Blame, window, cx);
            });
            cx.run_until_parked();
        }
    }

    /// Blame on the left pane of a working-tree diff has to *paint*, not merely
    /// be permitted: the gutter reserves the blame column from
    /// `GitBlame::max_author_display_columns` while the annotations themselves
    /// are laid out by `EditorElement::layout_blame_entries`, which bails out
    /// when the editor has no workspace. The two used to disagree here — the
    /// column appeared, ~170px of text shifted right, and nothing was drawn in
    /// it — so this asserts the painted tree rather than the predicate.
    #[gpui::test]
    async fn test_the_left_pane_paints_blame_for_a_working_tree_diff(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        context.fs.set_blame_at_revision_for_repo(
            Path::new(path!("/project/.git")),
            "HEAD",
            vec![(
                repo_path("a.rs"),
                ::git::blame::Blame {
                    entries: vec![
                        blame_entry_at(0, "Tester"),
                        blame_entry_at(1, "Tester"),
                        blame_entry_at(2, "Tester"),
                    ],
                    ..Default::default()
                },
            )],
        );

        let view = open_working_tree(&context, "a.rs", &mut cx)
            .await
            .expect("the working-tree file opens")
            .expect("the gesture opened a view");

        let splittable = view.read_with(&cx, |view, _| view.editor.clone());
        let lhs = splittable
            .read_with(&cx, |editor, _| editor.lhs_editor().cloned())
            .expect("a working-tree diff opens split");

        assert!(
            cx.debug_bounds("GIT-BLAME-ENTRY-LEFT").is_none(),
            "nothing is blamed before the user asks for it"
        );

        lhs.update_in(&mut cx, |editor, window, cx| {
            editor.focus_handle(cx).focus(window, cx);
            editor.toggle_git_blame(&::git::Blame, window, cx);
        });
        cx.run_until_parked();

        assert!(
            lhs.read_with(&cx, |editor, cx| editor
                .blame()
                .is_some_and(|blame| blame.read(cx).has_generated_entries())),
            "the fixture's blame at HEAD must have reached the left pane's GitBlame"
        );
        assert!(
            cx.debug_bounds("GIT-BLAME-ENTRY-LEFT").is_some(),
            "the left pane reserves the blame column, so it must also paint in it"
        );
        assert!(
            cx.debug_bounds("GIT-BLAME-ENTRY-RIGHT").is_none(),
            "and that has to be a claim about the left pane, not about either \
             one: blame was toggled on only that side"
        );
    }

    /// The same assertion for a left pane created *after* the view joined the
    /// workspace: Unified and back to Split rebuilds it from scratch, and the
    /// `Item::added_to_workspace` hook has long since fired, so the rebuilt pane
    /// only has a workspace if `SplittableEditor::split` gives it one. Every
    /// consumer of a split diff reaches this path through the toolbar's
    /// Unified/Split toggle, not just this view.
    #[gpui::test]
    async fn test_a_re_split_left_pane_still_paints_blame(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        context.fs.set_blame_at_revision_for_repo(
            Path::new(path!("/project/.git")),
            "HEAD",
            vec![(
                repo_path("a.rs"),
                ::git::blame::Blame {
                    entries: vec![
                        blame_entry_at(0, "Tester"),
                        blame_entry_at(1, "Tester"),
                        blame_entry_at(2, "Tester"),
                    ],
                    ..Default::default()
                },
            )],
        );

        let view = open_working_tree(&context, "a.rs", &mut cx)
            .await
            .expect("the working-tree file opens")
            .expect("the gesture opened a view");

        let splittable = view.read_with(&cx, |view, _| view.editor.clone());
        splittable.update_in(&mut cx, |editor, window, cx| {
            editor.toggle_split(&editor::ToggleSplitDiff, window, cx);
        });
        cx.run_until_parked();
        assert!(
            splittable.read_with(&cx, |editor, _| editor.lhs_editor().is_none()),
            "the first toggle collapses the diff to a single pane"
        );

        splittable.update_in(&mut cx, |editor, window, cx| {
            editor.toggle_split(&editor::ToggleSplitDiff, window, cx);
        });
        cx.run_until_parked();
        let lhs = splittable
            .read_with(&cx, |editor, _| editor.lhs_editor().cloned())
            .expect("the second toggle rebuilds the left pane");

        lhs.update_in(&mut cx, |editor, window, cx| {
            editor.focus_handle(cx).focus(window, cx);
            editor.toggle_git_blame(&::git::Blame, window, cx);
        });
        cx.run_until_parked();

        assert!(
            lhs.read_with(&cx, |editor, cx| editor
                .blame()
                .is_some_and(|blame| blame.read(cx).has_generated_entries())),
            "the rebuilt pane's GitBlame must still resolve the base text's blame"
        );
        assert!(
            cx.debug_bounds("GIT-BLAME-ENTRY-LEFT").is_some(),
            "a pane split after the view joined the workspace must paint blame too"
        );
        assert!(
            cx.debug_bounds("GIT-BLAME-ENTRY-RIGHT").is_none(),
            "and that has to be the rebuilt left pane, not the right one"
        );
    }

    /// Both panes of a commit diff are detached historic blobs, so neither can
    /// resolve a repository through the project's buffer store. Each therefore
    /// needs its revision declared: the right pane is the file at the sha, the
    /// left pane is the file at the sha's parent.
    #[gpui::test]
    async fn test_both_panes_paint_blame_for_a_commit_diff(cx: &mut TestAppContext) {
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
        set_blame_at_revisions(
            &context,
            "src/lib.rs",
            [SHA.to_string(), format!("{SHA}^")],
            3,
        );

        let view = open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");

        assert!(
            cx.debug_bounds("GIT-BLAME-ENTRY-LEFT").is_none()
                && cx.debug_bounds("GIT-BLAME-ENTRY-RIGHT").is_none(),
            "nothing is blamed before the user asks for it"
        );

        blame_both_panes(&view, &mut cx);

        assert!(
            cx.debug_bounds("GIT-BLAME-ENTRY-RIGHT").is_some(),
            "the right pane holds the file at the commit, so it must be \
             annotated from that revision"
        );
        assert!(
            cx.debug_bounds("GIT-BLAME-ENTRY-LEFT").is_some(),
            "and the left pane holds the file at the commit's parent"
        );
    }

    /// The fixture both run tests share: four lines whose first two came from
    /// one commit and whose last two came from another, opened as a commit
    /// diff so both panes are blamed. Two runs rather than one is the point —
    /// it is what separates "the gutter labels the first row of each run" from
    /// "the gutter labels the first row of the file".
    async fn open_two_run_commit_diff(
        context: &DiffTestContext,
        cx: &mut VisualTestContext,
    ) -> Entity<SoloDiffView> {
        set_commit(
            context,
            SHA,
            vec![commit_file(
                "src/lib.rs",
                Some("one\ntwo\nthree\nfour\n"),
                Some("one\nTWO\nthree\nfour\n"),
            )],
        );
        set_blame_runs_at_revisions(
            context,
            "src/lib.rs",
            [SHA.to_string(), format!("{SHA}^")],
            &[run_sha(0xaa), run_sha(0xaa), run_sha(0xbb), run_sha(0xbb)],
        );

        let view = open_commit(context, SHA, "src/lib.rs", cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");
        blame_both_panes(&view, cx);
        view
    }

    /// The date and the author identify a commit, so repeating them on every
    /// line of a run says nothing the row above did not. Asserted on the
    /// painted tree, and on both sides of it: the metadata is there on the row
    /// that opens each run and gone on the row that continues it. Row 2 opening
    /// the second run is what keeps this from passing for a renderer that only
    /// ever labels the top row of the viewport.
    #[gpui::test]
    async fn test_only_the_head_of_a_run_draws_its_blame_metadata(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        let _view = open_two_run_commit_diff(&context, &mut cx).await;

        assert!(
            cx.debug_bounds("GIT-BLAME-META-RIGHT-0").is_some(),
            "row 0 opens the first run, so it names its commit"
        );
        assert!(
            cx.debug_bounds("GIT-BLAME-META-RIGHT-1").is_none(),
            "row 1 came from the same commit as the row above it, so repeating \
             the date and the author there is noise"
        );
        assert!(
            cx.debug_bounds("GIT-BLAME-META-RIGHT-2").is_some(),
            "row 2 came from a different commit, so it opens a run of its own"
        );
        assert!(
            cx.debug_bounds("GIT-BLAME-META-RIGHT-3").is_none(),
            "and row 3 continues that second run"
        );
        assert!(
            cx.debug_bounds("GIT-BLAME-META-LEFT-0").is_some()
                && cx.debug_bounds("GIT-BLAME-META-LEFT-1").is_none(),
            "the left pane groups its own rows, not the right pane's"
        );
    }

    /// A continuation row draws nothing, and an empty flex is zero pixels tall:
    /// left at that, the row would keep its hover background, its tooltip, its
    /// context menu and its click-to-open listener and none of them could ever
    /// fire, because there would be no hit area to land on. The container has
    /// to stay the size of the line it annotates.
    #[gpui::test]
    async fn test_a_continuation_row_keeps_a_full_height_hit_area(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        let _view = open_two_run_commit_diff(&context, &mut cx).await;

        let head = cx
            .debug_bounds("GIT-BLAME-ROW-RIGHT-0")
            .expect("the row that opens the run paints its container");
        let continuation = cx
            .debug_bounds("GIT-BLAME-ROW-RIGHT-1")
            .expect("a continuation row paints its container even with no children");
        let next_head = cx
            .debug_bounds("GIT-BLAME-ROW-RIGHT-2")
            .expect("the row that opens the second run paints its container");

        // Derived from the gutter itself rather than restated as a constant:
        // consecutive rows are one line apart, so the gap between them is the
        // height a row has to cover to leave no dead band between two lines.
        let line_height = next_head.origin.y - continuation.origin.y;
        assert!(line_height > gpui::px(0.), "rows are laid out one line apart");
        assert_eq!(
            continuation.size.height, line_height,
            "a continuation row must cover its whole line, or the mouse falls \
             between the rows above and below it"
        );
        assert!(head.size.width > gpui::px(0.), "the gutter has a width");
        assert_eq!(
            continuation.size.width, head.size.width,
            "and a continuation row spans it just as the head row does"
        );
    }

    /// A file the commit added is not at the parent revision, so the left
    /// pane must simply go unannotated, and must not take the right pane's
    /// annotations down with it. This is also the shape of every file of a
    /// root commit — which has no `<sha>^` at all, and is the case where the
    /// gate is what stops `git blame` from erroring into a toast.
    #[gpui::test]
    async fn test_an_added_file_blames_only_the_right_pane(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(
            &context,
            SHA,
            vec![commit_file("src/new.rs", None, Some("one\ntwo\n"))],
        );
        // The fixture answers at the parent revision too, which real git would
        // refuse: the left assertion below has to be able to fail when the
        // `is_created` gate is removed, and it cannot if the fixture is the
        // thing withholding the annotations.
        set_blame_at_revisions(
            &context,
            "src/new.rs",
            [SHA.to_string(), format!("{SHA}^")],
            2,
        );

        let view = open_commit(&context, SHA, "src/new.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");

        blame_both_panes(&view, &mut cx);

        assert!(
            cx.debug_bounds("GIT-BLAME-ENTRY-RIGHT").is_some(),
            "the added file exists at the commit, so the right pane is blamed"
        );
        assert!(
            cx.debug_bounds("GIT-BLAME-ENTRY-LEFT").is_none(),
            "the file does not exist at the parent, so there is nothing to \
             annotate the empty left pane with"
        );
    }

    /// The mirror of the added file: one the commit **deleted** is not at the
    /// commit itself, so the right pane goes unannotated while the left pane —
    /// which holds the file as of the parent — still paints.
    #[gpui::test]
    async fn test_a_deleted_file_blames_only_the_left_pane(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(
            &context,
            SHA,
            vec![commit_file("src/gone.rs", Some("one\ntwo\n"), None)],
        );
        // Both revisions answerable, for the same reason as above: the right
        // assertion has to fail when the `is_deleted` gate is removed.
        set_blame_at_revisions(
            &context,
            "src/gone.rs",
            [SHA.to_string(), format!("{SHA}^")],
            2,
        );

        let view = open_commit(&context, SHA, "src/gone.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");

        blame_both_panes(&view, &mut cx);

        assert!(
            cx.debug_bounds("GIT-BLAME-ENTRY-LEFT").is_some(),
            "the deleted file still exists at the commit's parent, so the \
             left pane is blamed"
        );
        assert!(
            cx.debug_bounds("GIT-BLAME-ENTRY-RIGHT").is_none(),
            "and it does not exist at the commit, so there is nothing to \
             annotate the empty right pane with"
        );
    }

    /// A binary file's panes hold a `(binary file not shown)` placeholder
    /// rather than the file, so there is no line of it to attribute to anyone
    /// on either side and the diff declares no blame base at all.
    #[gpui::test]
    async fn test_a_binary_file_blames_neither_pane(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(&context, SHA, vec![binary_commit_file("src/logo.png")]);
        // Answerable at both revisions, so that dropping the binary gate would
        // actually annotate the placeholder rather than fail to find a blame.
        set_blame_at_revisions(
            &context,
            "src/logo.png",
            [SHA.to_string(), format!("{SHA}^")],
            2,
        );

        let view = open_commit(&context, SHA, "src/logo.png", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");

        view.read_with(&cx, |view, cx| {
            assert!(
                view.editor.read(cx).blame_base().is_none(),
                "a placeholder is not the file, so neither pane has a \
                 revision its text came from"
            );
        });

        blame_both_panes(&view, &mut cx);

        assert!(
            cx.debug_bounds("GIT-BLAME-ENTRY-LEFT").is_none()
                && cx.debug_bounds("GIT-BLAME-ENTRY-RIGHT").is_none(),
            "and so neither pane paints an annotation"
        );
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
            assert!(
                matches!(
                    view.editor.read(cx).blame_base(),
                    Some(DiffBlameBase::RhsFilesAt(revision)) if revision == "HEAD"
                ),
                "the left pane of an uncommitted diff is the file at HEAD, \
                 and its right pane is a project file blame resolves on its own"
            );
        });
        commit.read_with(&cx, |view, cx| {
            let Some(DiffBlameBase::Blob {
                repo_path: blamed_path,
                rhs_revision,
                lhs_revision,
                ..
            }) = view.editor.read(cx).blame_base()
            else {
                panic!("both panes of a commit diff are detached blobs");
            };
            assert_eq!(blamed_path, &repo_path("src/lib.rs"));
            assert_eq!(
                rhs_revision.as_ref().map(SharedString::as_ref),
                Some(SHA),
                "the right pane holds the file as of the commit"
            );
            assert_eq!(
                lhs_revision.as_ref().map(SharedString::as_ref),
                Some(format!("{SHA}^").as_str()),
                "and the left pane holds it as of the commit's first parent"
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

    /// Both toolbars key off `act_as::<SoloDiffView>`, which resolves through
    /// `Item::act_as_type` — so a source that stopped answering there would
    /// silently lose its whole toolbar rather than fail to compile.
    #[gpui::test]
    async fn test_both_toolbars_claim_either_source_and_nothing_else(cx: &mut TestAppContext) {
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
        let unrelated: Box<dyn ItemHandle> =
            Box::new(cx.update(|window, cx| cx.new(|cx| Editor::single_line(window, cx))));

        let git_toolbar = cx.update(|_window, cx| cx.new(SoloDiffGitToolbar::new));
        let style_toolbar = cx.update(|_window, cx| cx.new(SoloDiffStyleToolbar::new));

        for (source_name, view) in [("working tree", &working_tree), ("commit", &commit)] {
            let item: Box<dyn ItemHandle> = Box::new(view.clone());
            assert_eq!(
                git_toolbar.update_in(&mut cx, |toolbar, window, cx| {
                    toolbar.set_active_pane_item(Some(item.as_ref()), window, cx)
                }),
                ToolbarItemLocation::PrimaryRight,
                "the git toolbar must serve a {source_name} diff"
            );
            assert_eq!(
                style_toolbar.update_in(&mut cx, |toolbar, window, cx| {
                    toolbar.set_active_pane_item(Some(item.as_ref()), window, cx)
                }),
                ToolbarItemLocation::PrimaryLeft,
                "the style toolbar must serve a {source_name} diff"
            );
        }

        assert_eq!(
            git_toolbar.update_in(&mut cx, |toolbar, window, cx| {
                toolbar.set_active_pane_item(Some(unrelated.as_ref()), window, cx)
            }),
            ToolbarItemLocation::Hidden,
        );
        assert_eq!(
            style_toolbar.update_in(&mut cx, |toolbar, window, cx| {
                toolbar.set_active_pane_item(Some(unrelated.as_ref()), window, cx)
            }),
            ToolbarItemLocation::Hidden,
        );
    }

    /// An uncommitted diff describes the working tree: the status icon and the
    /// `+N −M` figures come from the repository's status entry, and there is
    /// no revision to name or link to.
    #[gpui::test]
    async fn test_the_git_toolbar_describes_the_working_tree(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;

        let view = open_working_tree(&context, "a.rs", &mut cx)
            .await
            .expect("the working-tree file opens")
            .expect("the gesture opened a view");

        view.read_with(&cx, |view, cx| {
            let content = view.git_toolbar_content(cx);
            let status_entry = context
                .repository
                .read(cx)
                .status_for_path(&repo_path("a.rs"))
                .expect("a.rs is modified in the working tree");
            assert_eq!(content.status, Some(status_entry.status));
            assert_eq!(content.diff_stat, status_entry.diff_stat);
            assert_eq!(
                content.diff_stat,
                Some(git::status::DiffStat {
                    added: 3,
                    deleted: 3
                }),
                "the repository's numstat — the view's own diff of a.rs is one \
                 changed line, so these figures could not have come from it"
            );
            assert_eq!(content.hunk_count, view.hunk_count(cx));
            assert!(
                content.commit.is_none(),
                "a working-tree diff has no revision to name, jump to or link to"
            );
        });
    }

    /// A commit's diff describes the commit: the status is the one the commit
    /// gave the file, the `+N −M` figures are the view's own, and the short
    /// sha plus the git-graph jump are present.
    #[gpui::test]
    async fn test_the_git_toolbar_describes_the_commit(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(
            &context,
            SHA,
            vec![commit_file("src/lib.rs", None, Some("one\ntwo\n"))],
        );

        let view = open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");

        view.read_with(&cx, |view, cx| {
            let content = view.git_toolbar_content(cx);
            assert_eq!(
                content.status,
                Some(FileStatus::Tracked(TrackedStatus {
                    index_status: StatusCode::Added,
                    worktree_status: StatusCode::Unmodified,
                })),
                "the commit created this file, so the icon must say added"
            );
            assert_eq!(
                content.diff_stat,
                Some(git::status::DiffStat {
                    added: 2,
                    deleted: 0
                }),
                "the figures are the view's own diff, not any numstat"
            );
            let commit = content.commit.expect("a commit source names its revision");
            assert_eq!(
                commit.sha.as_ref(),
                SHA,
                "the graph jump needs the full sha"
            );
            assert_eq!(
                commit.short_sha.as_ref(),
                &SHA[0..git::SHORT_SHA_LENGTH],
                "but the label is the abbreviated one"
            );
            assert_eq!(
                commit.permalink, None,
                "this fixture's repository has no remote"
            );
        });
    }

    /// The regression this task exists to close: before it, the toolbar read
    /// `status_for_path` for *every* source, so a commit's diff of a path that
    /// also has an uncommitted change described the uncommitted change.
    #[gpui::test]
    async fn test_the_commit_toolbar_ignores_a_dirty_working_tree(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        // The very path the working tree has modified, at a commit that
        // *deleted* it — so the two answers cannot be confused for each other.
        set_commit(
            &context,
            SHA,
            vec![commit_file("a.rs", Some("gone\n"), None)],
        );

        let view = open_commit(&context, SHA, "a.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");

        view.read_with(&cx, |view, cx| {
            let working_tree_status = context
                .repository
                .read(cx)
                .status_for_path(&repo_path("a.rs"))
                .expect("a.rs is also modified in the working tree");
            let content = view.git_toolbar_content(cx);
            assert_eq!(
                content.status,
                Some(FileStatus::Tracked(TrackedStatus {
                    index_status: StatusCode::Deleted,
                    worktree_status: StatusCode::Unmodified,
                })),
                "the commit deleted the file; the working tree merely modified it"
            );
            assert_ne!(
                content.status,
                Some(working_tree_status.status),
                "the two answers must not be the same one"
            );
            assert_ne!(
                content.diff_stat, working_tree_status.diff_stat,
                "and neither must the figures beside them"
            );
        });
    }

    /// The "View on <provider>" permalink is built from the repository's
    /// parsed remote, the way `CommitViewToolbar` built it.
    #[gpui::test]
    async fn test_a_commit_with_a_recognised_remote_gets_a_permalink(cx: &mut TestAppContext) {
        let (context, mut cx) =
            diff_test_context_with_remote(cx, Some("https://github.com/owner/repo.git")).await;
        set_commit(
            &context,
            SHA,
            vec![commit_file("src/lib.rs", Some("one\n"), Some("two\n"))],
        );

        let commit = open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");
        let working_tree = open_working_tree(&context, "a.rs", &mut cx)
            .await
            .expect("the working-tree file opens")
            .expect("the gesture opened a view");

        commit.read_with(&cx, |view, cx| {
            let commit = view
                .git_toolbar_content(cx)
                .commit
                .expect("a commit source names its revision");
            assert_eq!(
                commit.permalink,
                Some((
                    "GitHub".into(),
                    format!("https://github.com/owner/repo/commit/{SHA}")
                )),
            );
        });
        working_tree.read_with(&cx, |view, cx| {
            assert!(
                view.git_toolbar_content(cx).commit.is_none(),
                "an uncommitted change has no commit to permalink to"
            );
        });
    }

    /// `CommitViewToolbar` recounted every hunk on every render. The unified
    /// toolbar must not carry that over for the source it inherited.
    #[gpui::test]
    async fn test_the_hunk_count_is_memoised_for_a_commit_source(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(
            &context,
            SHA,
            vec![commit_file(
                "src/lib.rs",
                Some("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n"),
                Some("1 changed\n2\n3\n4\n5\n6\n7\n8\n9\n10 changed\n"),
            )],
        );

        let view = open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");

        view.read_with(&cx, |view, cx| {
            let counted = view.hunk_count(cx);
            assert_eq!(counted, 2, "the fixture's commit has two hunks");
            let (key, cached) = view
                .hunk_count_cache
                .0
                .get()
                .expect("the first count populates the memo");
            assert_eq!(cached, counted);
            // Plant a value the walk could never produce: a second call that
            // still returns it never walked the multibuffer again.
            view.hunk_count_cache.0.set(Some((key, counted + 100)));
            assert_eq!(
                view.hunk_count(cx),
                counted + 100,
                "a commit source's hunk count must be memoised, not recounted"
            );
        });
    }

    /// The other side of the same wiring. A `SplittableEditor` whose consumer
    /// paints no style controls of its own must get them from the search bar
    /// — and must get them *whether or not* it has anything to collapse.
    ///
    /// The editor here is headerless and holds no buffers, which is not a
    /// contrived shape: it is exactly what `CommitView` builds for a
    /// compare-range diff, and exactly the state the pane sees, because
    /// `add_item` runs synchronously while the commit's blobs are still
    /// loading. Gating the quartet on "has files to collapse" made both of
    /// those answer false and dropped the controls; nothing recomputes the
    /// toolbar location afterwards, so they never came back.
    #[gpui::test]
    async fn test_a_consumer_without_its_own_toolbar_gets_the_style_controls(
        cx: &mut TestAppContext,
    ) {
        let (context, mut cx) = diff_test_context(cx).await;
        let project = context
            .workspace
            .read_with(&cx, |workspace, _| workspace.project().clone());
        let workspace = context.workspace.clone();

        let search_bar =
            cx.update(|window, cx| cx.new(|cx| BufferSearchBar::new(None, window, cx)));

        // Both shapes that answer "nothing to collapse": a headerless
        // multibuffer with no excerpts yet (compare-range `CommitView`, and
        // every commit view at the instant `add_item` runs), and a singleton
        // (`file_diff_view`, which `buffer_kind` excludes outright).
        for headerless in [true, false] {
            let project = project.clone();
            let workspace = workspace.clone();
            let editor = cx.update(|window, cx| {
                let multibuffer = cx.new(|cx| {
                    if headerless {
                        MultiBuffer::without_headers(Capability::ReadOnly)
                    } else {
                        MultiBuffer::singleton(cx.new(|cx| Buffer::local("one\n", cx)), cx)
                    }
                });
                cx.new(|cx| {
                    SplittableEditor::new(
                        DiffViewStyle::Split,
                        multibuffer,
                        project,
                        workspace,
                        window,
                        cx,
                    )
                })
            });
            cx.run_until_parked();
            assert!(
                !editor.read_with(&cx, |editor, _| editor.style_controls_painted_by_consumer()),
                "this consumer has no toolbar of its own"
            );

            let item: Box<dyn ItemHandle> = Box::new(editor);
            let (location, paints) = search_bar.update_in(&mut cx, |search_bar, window, cx| {
                let location = search_bar.set_active_pane_item(Some(item.as_ref()), window, cx);
                (location, search_bar.paints_diff_style_controls(cx))
            });

            assert!(
                paints,
                "the search bar is the one that paints them (headerless={headerless})"
            );
            assert_eq!(
                location,
                ToolbarItemLocation::PrimaryLeft,
                "and it has to take the slot even with nothing to collapse \
                 (headerless={headerless})"
            );

            // The slot has to survive a search, too. `show` and `dismiss`
            // re-emit the location themselves and nothing recomputes it
            // afterwards, so a bar that answered differently there would drop
            // the controls on the first ctrl-f and never bring them back.
            let mut events = cx.events::<ToolbarItemEvent, BufferSearchBar>(&search_bar);
            search_bar.update_in(&mut cx, |search_bar, window, cx| {
                search_bar.show(window, cx);
            });
            assert_eq!(
                events.try_recv().expect("show emits a location"),
                ToolbarItemEvent::ChangeLocation(ToolbarItemLocation::PrimaryLeft),
                "ctrl-f must not evict the diff controls (headerless={headerless})"
            );
            search_bar.update_in(&mut cx, |search_bar, window, cx| {
                search_bar.dismiss(&search::buffer_search::Dismiss, window, cx);
            });
            assert_eq!(
                events.try_recv().expect("dismiss emits a location"),
                ToolbarItemEvent::ChangeLocation(ToolbarItemLocation::PrimaryLeft),
                "and neither must escaping out of it (headerless={headerless})"
            );
        }
    }

    /// `set_active_pane_item` saying `PrimaryLeft` is not the same claim as
    /// the bar actually painting the four buttons — the two were gated
    /// separately, and that gap is what shipped the compare-range regression.
    /// So assert the painted element tree: `IconButton` registers a
    /// `debug_selector` of `ICON-{icon:?}`, which `VisualTestContext::
    /// debug_bounds` can look up after a draw.
    #[gpui::test]
    async fn test_the_search_bar_paints_the_style_controls_and_nothing_else(
        cx: &mut TestAppContext,
    ) {
        let (context, mut cx) = diff_test_context(cx).await;
        let project = context
            .workspace
            .read_with(&cx, |workspace, _| workspace.project().clone());
        let workspace = context.workspace.clone();

        // Headerless and empty — a compare-range `CommitView` at `add_item`.
        let editor = cx.update(|window, cx| {
            let multibuffer = cx.new(|_| MultiBuffer::without_headers(Capability::ReadOnly));
            cx.new(|cx| {
                SplittableEditor::new(
                    DiffViewStyle::Split,
                    multibuffer,
                    project,
                    workspace,
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();

        let search_bar =
            cx.update(|window, cx| cx.new(|cx| BufferSearchBar::new(None, window, cx)));
        let item: Box<dyn ItemHandle> = Box::new(editor);
        search_bar.update_in(&mut cx, |search_bar, window, cx| {
            search_bar.set_active_pane_item(Some(item.as_ref()), window, cx);
        });
        cx.run_until_parked();

        // Dismissed and shown are two different element trees — the dismissed
        // bar returns early with just the leading group — so both have to be
        // drawn. The shown one is what the user gets after ctrl-f, which is
        // exactly where the controls used to disappear.
        for shown in [false, true] {
            if shown {
                assert!(
                    search_bar.update_in(&mut cx, |search_bar, window, cx| {
                        search_bar.show(window, cx)
                    }),
                    "the bar opens over a diff"
                );
                cx.run_until_parked();
            }

            cx.draw(
                gpui::point(gpui::px(0.), gpui::px(0.)),
                gpui::size(gpui::px(1200.), gpui::px(200.)),
                |_window, _cx| search_bar.clone().into_any_element(),
            );

            for icon in ["ICON-ArrowUp", "ICON-ArrowDown", "ICON-DiffUnified"] {
                assert!(
                    cx.debug_bounds(icon).is_some(),
                    "{icon} must actually be painted, not merely permitted (shown={shown})"
                );
            }
            assert!(
                cx.debug_bounds("ICON-ChevronDownUp").is_none(),
                "and there is nothing here to collapse, so no chevron beside them \
                 (shown={shown})"
            );
        }
    }

    /// Splitting the pane was a gesture the commit source had through
    /// `CommitView` and the working-tree source never had at all. Both keep
    /// it, and the clone has to be configured for its own source rather than
    /// for the default.
    ///
    /// One source per test, deliberately: a *second* split in the same
    /// workspace narrows the panes enough to unsplit one of the diffs, and
    /// unsplitting a clone that shares its multibuffer trips a debug assertion
    /// in `editor::display_map`. That is not this task's bug — an untouched
    /// `CommitView`, split twice, hits the same assertion — but there is no
    /// reason for these tests to walk into it.
    async fn assert_splits_into_a_working_clone(
        context: &DiffTestContext,
        view: &Entity<SoloDiffView>,
        cx: &mut VisualTestContext,
    ) {
        assert!(view.read_with(cx, |view, _| Item::can_split(view)));
        let pane = context
            .workspace
            .read_with(cx, |workspace, _| workspace.active_pane().clone());
        // Through `Workspace::split_and_clone`, never `clone_on_split`
        // directly: the real gesture calls it *while the workspace entity is
        // being updated*, so a clone that read anything back off the workspace
        // would double-lease and panic. Calling it directly from a test is
        // exactly the shape that hides that — it took a live editor to find.
        let new_pane = context
            .workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.activate_item(view, true, false, window, cx);
                workspace.split_and_clone(pane, SplitDirection::Right, window, cx)
            })
            .await
            .expect("a splittable item produces a second pane");
        cx.run_until_parked();
        let clone = new_pane
            .read_with(cx, |pane, _| {
                pane.active_item()
                    .and_then(|item| item.downcast::<SoloDiffView>())
            })
            .expect("the new pane holds the cloned diff");

        let (source_matches, same_multibuffer, distinct_editor, controls, blame, hunks) = cx
            .update(|_window, cx| {
                let original = view.read(cx);
                let clone = clone.read(cx);
                (
                    clone.source().matches(original.source(), cx),
                    clone.multibuffer == original.multibuffer,
                    clone.editor != original.editor,
                    clone.editor.read(cx).diff_hunk_controls_disabled()
                        == original.editor.read(cx).diff_hunk_controls_disabled(),
                    clone.editor.read(cx).blame_base().cloned()
                        == original.editor.read(cx).blame_base().cloned(),
                    (clone.hunk_count(cx), original.hunk_count(cx)),
                )
            });
        assert!(source_matches, "the clone shows what the original showed");
        assert!(same_multibuffer, "over the very same multibuffer");
        assert!(distinct_editor, "through an editor of its own");
        assert!(controls, "with the source's hunk-control rule re-applied");
        assert!(blame, "and the source's blame base re-applied");
        assert_eq!(hunks.0, hunks.1, "so it counts the same hunks");
        assert!(hunks.0 > 0, "and actually has excerpts to count");
    }

    /// `SoloDiffStyleToolbar` and `BufferSearchBar` both live in
    /// `PrimaryLeft` and both know how to paint hunk navigation and
    /// Unified/Split. The view has to tell its editor which one is doing it,
    /// or a commit diff draws two identical pairs side by side — the
    /// working-tree source escapes only because its multibuffer is a
    /// singleton, which `BufferSearchBar` gates on for unrelated reasons.
    #[gpui::test]
    async fn test_neither_source_lets_the_search_bar_paint_the_style_controls(
        cx: &mut TestAppContext,
    ) {
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

        // The far end of the wiring, not just the flag: a real
        // `BufferSearchBar` told about this item must decline to paint the
        // quartet `SoloDiffStyleToolbar` is already painting.
        let search_bar =
            cx.update(|window, cx| cx.new(|cx| BufferSearchBar::new(None, window, cx)));
        for view in [&working_tree, &commit] {
            assert!(
                view.read_with(&cx, |view, cx| view
                    .editor
                    .read(cx)
                    .style_controls_painted_by_consumer()),
                "this view has a style toolbar of its own"
            );
            let item: Box<dyn ItemHandle> = Box::new(view.clone());
            let (location, paints) = search_bar.update_in(&mut cx, |search_bar, window, cx| {
                let location = search_bar.set_active_pane_item(Some(item.as_ref()), window, cx);
                (location, search_bar.paints_diff_style_controls(cx))
            });
            assert!(
                !paints,
                "the search bar must leave the style controls to that toolbar"
            );
            assert_ne!(
                location,
                ToolbarItemLocation::PrimaryLeft,
                "and must not take the slot beside it either"
            );
        }

        // And a split pane's clone, which builds a second editor from scratch.
        let pane = context
            .workspace
            .read_with(&cx, |workspace, _| workspace.active_pane().clone());
        let new_pane = context
            .workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.activate_item(&commit, true, false, window, cx);
                workspace.split_and_clone(pane, SplitDirection::Right, window, cx)
            })
            .await
            .expect("a splittable item produces a second pane");
        cx.run_until_parked();
        let clone = new_pane
            .read_with(&cx, |pane, _| {
                pane.active_item()
                    .and_then(|item| item.downcast::<SoloDiffView>())
            })
            .expect("the new pane holds the cloned diff");
        assert!(
            clone.read_with(&cx, |view, cx| view
                .editor
                .read(cx)
                .style_controls_painted_by_consumer()),
            "the clone is served by the same toolbar as the original"
        );
    }

    #[gpui::test]
    async fn test_a_working_tree_diff_splits_into_a_second_view(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;

        let view = open_working_tree(&context, "a.rs", &mut cx)
            .await
            .expect("the working-tree file opens")
            .expect("the gesture opened a view");

        assert_splits_into_a_working_clone(&context, &view, &mut cx).await;
    }

    #[gpui::test]
    async fn test_a_commit_diff_splits_into_a_second_view(cx: &mut TestAppContext) {
        let (context, mut cx) = diff_test_context(cx).await;
        set_commit(
            &context,
            SHA,
            vec![commit_file("src/lib.rs", Some("one\n"), Some("two\n"))],
        );

        let view = open_commit(&context, SHA, "src/lib.rs", &mut cx)
            .await
            .expect("the commit's file opens")
            .expect("the gesture opened a view");

        assert_splits_into_a_working_clone(&context, &view, &mut cx).await;
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
