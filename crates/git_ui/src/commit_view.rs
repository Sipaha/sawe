//! S-DET commit view — IDEA-style metadata surface (header, parents bar,
//! refs bar, contains panel, affected files, footer) plus the existing
//! diff editor. Decomposed into per-section modules under
//! [`crate::commit_view::*`] so each part has a clear scope. Public API
//! (`CommitView`, `CommitViewToolbar`, `CommitView::open`) is unchanged.

mod affected_files;
pub mod ai_explain;
mod contains_panel;
mod footer;
mod header;
pub(crate) mod mcp;
mod mentions;
mod parents_bar;
mod refs_bar;

use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use collections::HashMap;
use editor::{
    Addon, Editor, EditorEvent, EditorSettings, MultiBuffer, SplittableEditor,
    hover_markdown_style, multibuffer_context_lines,
};
use futures_lite::future::yield_now;
use git::repository::{CommitDetails, CommitDiff, RepoPath, is_binary_content};
use git::status::{FileStatus, StatusCode, TrackedStatus};
use git::{
    BuildCommitPermalinkParams, GitHostingProviderRegistry, GitRemote, ParsedGitRemote,
    parse_git_remote_url,
};
use gpui::{
    AnyElement, App, AppContext as _, AsyncApp, AsyncWindowContext, ClipboardItem, Context, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    PromptLevel, Render, ScrollHandle, StatefulInteractiveElement as _, Styled, Task, WeakEntity,
    Window, actions,
};
use language::{
    Buffer, Capability, DiskState, File, LanguageRegistry, LineEnding, OffsetRangeExt as _,
    ReplicaId, Rope, TextBuffer,
};
use markdown::{Markdown, MarkdownElement};
use multi_buffer::PathKey;
use project::{Project, ProjectPath, WorktreeId, git_store::Repository};
use settings::{DiffViewStyle, Settings};
use std::{
    any::{Any, TypeId},
    collections::HashSet,
    path::PathBuf,
    sync::Arc,
};
use theme::ActiveTheme;
use ui::{ContextMenu, DiffStat, Disclosure, Divider, Tooltip, WithScrollbar, prelude::*};
use util::{ResultExt, paths::PathStyle, rel_path::RelPath, truncate_and_trailoff};
use workspace::item::TabTooltipContent;
use workspace::{
    Item, ItemHandle, ItemNavHistory, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace,
    item::{ItemEvent, TabContentParams},
    notifications::NotifyTaskExt,
    pane::SaveIntent,
    searchable::SearchableItemHandle,
};

use crate::commit_view::affected_files::CommitAffectedFiles;
use crate::commit_view::contains_panel::CommitContainsPanel;
use crate::git_panel::GitPanel;
use crate::git_panel_settings::GitPanelSettings;
use settings::Settings as _;

actions!(
    git,
    [
        ApplyCurrentStash,
        PopCurrentStash,
        DropCurrentStash,
        OpenFileAtHead,
        /// S-AI-EXP — kick off an AI explanation for the open commit
        /// (or expand/collapse the section when one already exists).
        ExplainCommit,
    ]
);

/// Action emitted by the footer's "Open in New Tab" button. The workspace
/// handler resolves the target repository, fetches the commit, and adds a
/// fresh `CommitView` pane item.
#[derive(Clone, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = git)]
pub struct OpenCommitInNewTab {
    pub sha: String,
}

pub fn init(cx: &mut App) {
    mcp::register(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ApplyCurrentStash, window, cx| {
            CommitView::apply_stash(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &DropCurrentStash, window, cx| {
            CommitView::remove_stash(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &PopCurrentStash, window, cx| {
            CommitView::pop_stash(workspace, window, cx);
        });
        workspace.register_action(|workspace, action: &OpenCommitInNewTab, window, cx| {
            let Some(repo) = workspace.project().read(cx).active_repository(cx) else {
                return;
            };
            CommitView::open(
                action.sha.clone(),
                repo.downgrade(),
                workspace.weak_handle(),
                None,
                None,
                window,
                cx,
            );
        });
    })
    .detach();
}

pub struct CommitView {
    commit: CommitDetails,
    editor: Entity<SplittableEditor>,
    message: Entity<Markdown>,
    message_expanded: bool,
    message_scroll_handle: ScrollHandle,
    stash: Option<usize>,
    multibuffer: Entity<MultiBuffer>,
    repository: Entity<Repository>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    remote: Option<GitRemote>,
    /// Parents in display order. Loaded asynchronously after `new`.
    parents: Vec<SharedString>,
    /// Ref decorations attached to this commit (branches / tags). The
    /// `git_graph` callers already know these from the log row; for
    /// standalone opens we re-derive them via `git log --decorate=full`.
    ref_names: Vec<SharedString>,
    /// `Some((name, email))` only when the committer differs from the
    /// author. Surfaced as a second line under the author tile.
    extra_committer: Option<(SharedString, SharedString)>,
    /// Currently selected merge-parent index (1-based; 1 = first parent).
    /// Hidden in the toolbar when the commit has a single parent.
    selected_parent_index: usize,
    /// Cached commit diff (for the currently selected merge-parent index)
    /// — used by the affected-files component.
    diff_files: Vec<git::repository::CommitFile>,
    affected_files: CommitAffectedFiles,
    contains_panel: CommitContainsPanel,
    /// Whether this view is the standalone-tab variant. Drives the
    /// "Open in New Tab" footer button visibility.
    in_pane: bool,
    /// S-AI-EXP — Explain button state. The body / cache flag are only
    /// `Some` once an explanation has been produced (or pulled from
    /// disk). `pending` is the spinner driver.
    explain_body: Option<SharedString>,
    explain_from_cache: bool,
    explain_pending: bool,
    explain_expanded: bool,
    explain_error: Option<SharedString>,
    _explain_task: Option<Task<()>>,
    /// `Some` when this view was opened as a focused single-file diff
    /// (clicking a file in the git-graph commit-detail panel) — render
    /// only the diff editor, no metadata panel / commit-message excerpt,
    /// and title the tab with the file name. Also keys the open-view
    /// dedup so a single-file diff and the full commit view coexist.
    single_file: Option<RepoPath>,
}

struct GitBlob {
    path: RepoPath,
    worktree_id: WorktreeId,
    is_deleted: bool,
    is_binary: bool,
    display_name: String,
}

struct CommitDiffAddon {
    file_statuses: HashMap<language::BufferId, FileStatus>,
    commit_view: WeakEntity<CommitView>,
}

impl Addon for CommitDiffAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn override_status_for_buffer_id(
        &self,
        buffer_id: language::BufferId,
        _cx: &App,
    ) -> Option<FileStatus> {
        self.file_statuses.get(&buffer_id).copied()
    }

    fn extend_buffer_header_context_menu(
        &self,
        menu: ContextMenu,
        buffer: &language::BufferSnapshot,
        _window: &mut Window,
        cx: &mut App,
    ) -> ContextMenu {
        let file_to_open = buffer.file().and_then(|file| {
            let commit_view = self.commit_view.upgrade()?;
            let commit_view = commit_view.read(cx);
            let project_path = commit_view
                .repository
                .read(cx)
                .repo_path_to_project_path(&RepoPath::from_rel_path(file.path()), cx)?;
            let exists_at_head = commit_view
                .workspace
                .upgrade()?
                .read(cx)
                .project()
                .read(cx)
                .entry_for_path(&project_path, cx)
                .is_some();
            exists_at_head.then(|| file.clone())
        });

        menu.when_some(file_to_open, |menu, file| {
            let commit_view = self.commit_view.clone();
            menu.entry(
                "Open File in Project",
                Some(Box::new(OpenFileAtHead)),
                move |window, cx| {
                    commit_view
                        .update(cx, |view, cx| view.open_file_at_head(&file, window, cx))
                        .log_err();
                },
            )
        })
    }
}

const FILE_NAMESPACE_SORT_PREFIX: u64 = 1;

impl CommitView {
    pub fn open(
        commit_sha: String,
        repo: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        stash: Option<usize>,
        file_filter: Option<RepoPath>,
        window: &mut Window,
        cx: &mut App,
    ) {
        Self::open_internal(
            commit_sha,
            repo,
            workspace,
            stash,
            file_filter,
            false,
            window,
            cx,
        );
    }

    /// Open a focused diff of a single file's changes in `commit_sha`
    /// (its content at the commit vs. at the parent) — no commit
    /// metadata chrome, tab titled with the file name. Used by the
    /// changed-files list in the git-graph commit-detail panel.
    pub fn open_file_diff(
        commit_sha: String,
        repo: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        file: RepoPath,
        window: &mut Window,
        cx: &mut App,
    ) {
        Self::open_internal(
            commit_sha,
            repo,
            workspace,
            None,
            Some(file),
            true,
            window,
            cx,
        );
    }

    fn open_internal(
        commit_sha: String,
        repo: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        stash: Option<usize>,
        file_filter: Option<RepoPath>,
        single_file_mode: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let commit_diff = repo
            .update(cx, |repo, _| repo.load_commit_diff(commit_sha.clone()))
            .ok();
        let commit_details = repo
            .update(cx, |repo, _| repo.show(commit_sha.clone()))
            .ok();

        window
            .spawn(cx, async move |cx| {
                let commit_diff = commit_diff?;
                let commit_details = commit_details?;
                let (commit_diff, commit_details) = futures::join!(commit_diff, commit_details);
                let mut commit_diff = commit_diff.log_err()?.log_err()?;
                let commit_details = commit_details.log_err()?.log_err()?;

                if let Some(ref filter_path) = file_filter {
                    commit_diff.files.retain(|f| &f.path == filter_path);
                }

                let single_file = if single_file_mode {
                    file_filter.clone()
                } else {
                    None
                };

                let repo = repo.upgrade()?;

                workspace
                    .update_in(cx, |workspace, window, cx| {
                        let project = workspace.project();
                        let workspace_entity = cx.entity();
                        let workspace_handle = cx.weak_entity();
                        let single_file_for_view = single_file.clone();
                        let commit_view = cx.new(|cx| {
                            CommitView::new(
                                commit_details,
                                commit_diff,
                                repo,
                                project.clone(),
                                workspace_entity,
                                workspace_handle,
                                stash,
                                single_file_for_view,
                                window,
                                cx,
                            )
                        });

                        let pane = workspace.active_pane();
                        pane.update(cx, |pane, cx| {
                            let ix = pane.items().position(|item| {
                                let commit_view = item.downcast::<CommitView>();
                                commit_view.is_some_and(|view| {
                                    let view = view.read(cx);
                                    view.commit.sha == commit_sha && view.single_file == single_file
                                })
                            });
                            if let Some(ix) = ix {
                                let existing = pane
                                    .items()
                                    .filter_map(|item| item.downcast::<CommitView>())
                                    .find(|view| view.read(cx).commit.sha == commit_sha)
                                    .unwrap();

                                pane.remove_item(existing.item_id(), false, false, window, cx);
                                pane.add_item(
                                    Box::new(commit_view),
                                    true,
                                    true,
                                    Some(ix),
                                    window,
                                    cx,
                                );
                            } else {
                                pane.add_item(Box::new(commit_view), true, true, None, window, cx);
                            }
                        })
                    })
                    .log_err()
            })
            .detach();
    }

    fn new(
        commit: CommitDetails,
        commit_diff: CommitDiff,
        repository: Entity<Repository>,
        project: Entity<Project>,
        workspace_entity: Entity<Workspace>,
        workspace: WeakEntity<Workspace>,
        stash: Option<usize>,
        single_file: Option<RepoPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Single-file diff mode: render only the file's diff editor — skip
        // the commit-message excerpt (and the metadata panel, see `render`).
        let compact = single_file.is_some();
        let language_registry = project.read(cx).languages().clone();
        let multibuffer = cx.new(|cx| {
            // Single-file mode shows exactly one file whose name is already in
            // the tab — the multibuffer path header would be redundant chrome.
            let mut multibuffer = if compact {
                MultiBuffer::without_headers(Capability::ReadOnly)
            } else {
                MultiBuffer::new(Capability::ReadOnly)
            };
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });

        let message = cx.new(|cx| {
            Markdown::new(
                commit.message.clone(),
                Some(language_registry.clone()),
                None,
                cx,
            )
        });

        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer.clone(),
                project.clone(),
                workspace_entity.clone(),
                window,
                cx,
            );
            editor.disable_diff_hunk_controls(cx);

            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_show_bookmarks(false, cx);
                editor.set_show_breakpoints(false, cx);
                editor.set_show_diff_review_button(true, cx);
            });

            editor
        });
        let commit_sha = Arc::<str>::from(commit.sha.as_ref());

        let first_worktree_id = project
            .read(cx)
            .worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).id());

        let repository_clone = repository.clone();
        let diff_files = commit_diff.files.iter().map(clone_commit_file).collect();

        cx.spawn_in(window, async move |this, cx| {
            let mut binary_buffer_ids: HashSet<language::BufferId> = HashSet::default();
            let mut file_statuses: HashMap<language::BufferId, FileStatus> = HashMap::default();

            for file in commit_diff.files {
                let is_created = file.old_text.is_none();
                let is_deleted = file.new_text.is_none();
                let raw_new_text = file.new_text.unwrap_or_default();
                let raw_old_text = file.old_text;

                let is_binary = file.is_binary
                    || is_binary_content(raw_new_text.as_bytes())
                    || raw_old_text
                        .as_ref()
                        .is_some_and(|text| is_binary_content(text.as_bytes()));

                let new_text = if is_binary {
                    "(binary file not shown)".to_string()
                } else {
                    raw_new_text
                };
                let old_text = if is_binary { None } else { raw_old_text };
                let worktree_id = repository_clone
                    .update(cx, |repository, cx| {
                        repository
                            .repo_path_to_project_path(&file.path, cx)
                            .map(|path| path.worktree_id)
                            .or(first_worktree_id)
                    })
                    .context("project has no worktrees")?;
                let short_sha = commit_sha
                    .get(0..git::SHORT_SHA_LENGTH)
                    .unwrap_or(&commit_sha);
                let file_name = file
                    .path
                    .file_name()
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| file.path.display(PathStyle::local()).to_string());
                let display_name = format!("{short_sha} - {file_name}");

                let file = Arc::new(GitBlob {
                    path: file.path.clone(),
                    is_deleted,
                    is_binary,
                    worktree_id,
                    display_name,
                }) as Arc<dyn language::File>;

                let buffer = build_buffer(new_text, file, &language_registry, cx).await?;
                let buffer_id = cx.update(|_, cx| buffer.read(cx).remote_id())?;

                let status_code = if is_created {
                    StatusCode::Added
                } else if is_deleted {
                    StatusCode::Deleted
                } else {
                    StatusCode::Modified
                };
                file_statuses.insert(
                    buffer_id,
                    FileStatus::Tracked(TrackedStatus {
                        index_status: status_code,
                        worktree_status: StatusCode::Unmodified,
                    }),
                );

                if is_binary {
                    binary_buffer_ids.insert(buffer_id);
                }

                let buffer_diff = if is_binary {
                    cx.update(|_, cx| {
                        let snapshot = buffer.read(cx).snapshot();
                        cx.new(|cx| {
                            BufferDiff::new_unchanged(
                                &snapshot,
                                snapshot.language().cloned(),
                                Some(language_registry.clone()),
                                cx,
                            )
                        })
                    })?
                } else {
                    build_buffer_diff(old_text, &buffer, &language_registry, cx).await?
                };

                let (excerpt_ranges, path) = cx.update(|_, cx| {
                    let snapshot = buffer.read(cx).snapshot();
                    let path = PathKey::with_sort_prefix(
                        FILE_NAMESPACE_SORT_PREFIX,
                        snapshot.file().unwrap().path().clone(),
                    );
                    let ranges = if is_binary {
                        vec![language::Point::zero()..snapshot.max_point()]
                    } else {
                        let diff_snapshot = buffer_diff.read(cx).snapshot(cx);
                        let mut hunks = diff_snapshot.hunks(&snapshot).peekable();
                        if hunks.peek().is_none() {
                            vec![language::Point::zero()..snapshot.max_point()]
                        } else {
                            hunks
                                .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                                .collect::<Vec<_>>()
                        }
                    };
                    (ranges, path)
                })?;

                // Batch the insertion of excerpts and yield between batches, to avoid blocking the main thread when a single file has many hunks.
                const EXCERPT_BATCH_SIZE: usize = 10;
                let total = excerpt_ranges.len();
                let mut batch_end = 0;
                while batch_end < total {
                    let is_first_batch = batch_end == 0;
                    batch_end = (batch_end + EXCERPT_BATCH_SIZE).min(total);
                    let ranges = excerpt_ranges[..batch_end].to_vec();
                    this.update_in(cx, |this, window, cx| {
                        this.editor.update(cx, |editor, cx| {
                            editor.update_excerpts_for_path(
                                path.clone(),
                                buffer.clone(),
                                ranges,
                                multibuffer_context_lines(cx),
                                buffer_diff.clone(),
                                cx,
                            );
                            if is_first_batch && editor.diff_view_style() == DiffViewStyle::Split {
                                editor.split(window, cx);
                            }
                        });
                    })?;
                    if batch_end < total {
                        yield_now().await;
                    }
                }
            }

            this.update(cx, |this, cx| {
                let commit_view = cx.weak_entity();
                this.editor.update(cx, |editor, cx| {
                    editor.rhs_editor().update(cx, |editor, _cx| {
                        editor.register_addon(CommitDiffAddon {
                            file_statuses,
                            commit_view,
                        });
                    });
                });
                if !binary_buffer_ids.is_empty() {
                    this.editor.update(cx, |editor, cx| {
                        editor.rhs_editor().update(cx, |editor, cx| {
                            editor.fold_buffers(binary_buffer_ids, cx);
                        });
                    });
                }
            })?;

            anyhow::Ok(())
        })
        .detach();

        let snapshot = repository.read(cx).snapshot();
        let remote_url = snapshot
            .remote_upstream_url
            .as_ref()
            .or(snapshot.remote_origin_url.as_ref());

        let remote = remote_url.and_then(|url| {
            let provider_registry = GitHostingProviderRegistry::default_global(cx);
            parse_git_remote_url(provider_registry, url).map(|(host, parsed)| GitRemote {
                host,
                owner: parsed.owner.into(),
                repo: parsed.repo.into(),
            })
        });

        let lazy_threshold = GitPanelSettings::get_global(cx)
            .commit_view
            .affected_files_lazy_threshold;
        let affected_files = CommitAffectedFiles::new(lazy_threshold, window, cx);

        let mut view = Self {
            commit,
            editor,
            message,
            message_expanded: false,
            message_scroll_handle: ScrollHandle::new(),
            multibuffer,
            stash,
            repository: repository.clone(),
            project,
            workspace,
            remote,
            parents: Vec::new(),
            ref_names: Vec::new(),
            extra_committer: None,
            selected_parent_index: 1,
            diff_files,
            affected_files,
            contains_panel: CommitContainsPanel::new(),
            in_pane: true,
            explain_body: None,
            explain_from_cache: false,
            explain_pending: false,
            explain_expanded: false,
            explain_error: None,
            _explain_task: None,
            single_file,
        };
        // The metadata panel (parents / refs / contains) isn't rendered in
        // single-file diff mode, so don't pay for the git calls that fill it.
        if !compact {
            let sha_for_meta = view.commit.sha.to_string();
            view.contains_panel
                .load(sha_for_meta.clone(), repository.clone(), cx);
            view.spawn_load_metadata(sha_for_meta, repository, cx);
        }
        view
    }

    fn spawn_load_metadata(
        &mut self,
        sha: String,
        repository: Entity<Repository>,
        cx: &mut Context<Self>,
    ) {
        let work_dir = repository.read(cx).work_directory_abs_path.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(
                    async move { load_commit_metadata(work_dir.as_ref(), &sha).await },
                )
                .await;
            if let Some(metadata) = result.log_err() {
                this.update(cx, |view, cx| {
                    view.parents = metadata.parents;
                    view.ref_names = metadata.ref_names;
                    view.extra_committer = metadata.extra_committer;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn calculate_changed_lines(&self, cx: &App) -> (u32, u32) {
        self.multibuffer.read(cx).snapshot(cx).total_changed_lines()
    }

    fn open_file_at_head(
        &mut self,
        file: &Arc<dyn language::File>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rel_path = file.path().clone();
        let worktree_id = file.worktree_id(cx);
        let repo_path = RepoPath::from_rel_path(&rel_path);
        let project_path = self
            .repository
            .read(cx)
            .repo_path_to_project_path(&repo_path, cx)
            .unwrap_or(project::ProjectPath {
                worktree_id,
                path: rel_path,
            });

        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_path_preview(project_path, None, false, false, true, window, cx)
                    .detach_and_log_err(cx);
            })
            .log_err();
    }

    fn open_file_at_head_action(
        &mut self,
        _: &OpenFileAtHead,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self
            .editor
            .read(cx)
            .focused_editor()
            .read(cx)
            .active_buffer(cx)
            .and_then(|buffer| buffer.read(cx).file().cloned())
        else {
            return;
        };
        self.open_file_at_head(&file, window, cx);
    }


    /// True if there's a Solution available to host the ephemeral AI
    /// session. Without one, [`ai_explain::explain_commit`] would
    /// surface the same error via the toast path; disabling the button
    /// up front is friendlier — and keeps the affordance consistent
    /// with the conflict-resolver AI button.
    fn has_active_solution(cx: &App) -> bool {
        solutions::SolutionStore::try_global(cx)
            .map(|store| !store.read(cx).solutions().is_empty())
            .unwrap_or(false)
    }

    /// Reason the Explain button is disabled (or `None` when active).
    /// Surfaced as the button's tooltip so the user knows why the
    /// affordance is greyed out.
    pub(crate) fn explain_disabled_reason(&self, cx: &App) -> Option<&'static str> {
        if self.commit.sha.as_ref().is_empty() {
            return Some("This commit has no SHA");
        }
        if self.stash.is_some() {
            return Some("Explain is only available for non-stash commits");
        }
        if !Self::has_active_solution(cx) {
            return Some("Explain requires an active Solution");
        }
        None
    }

    /// Toolbar / header click handler for the Explain button. If an
    /// explanation is already produced, just toggles the expandable
    /// section; otherwise kicks off the ephemeral AI task.
    pub(crate) fn toggle_or_request_explain(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.explain_pending {
            return;
        }
        if self.explain_body.is_some() {
            self.explain_expanded = !self.explain_expanded;
            cx.notify();
            return;
        }
        if self.explain_disabled_reason(cx).is_some() {
            return;
        }
        let work_dir: PathBuf = self
            .repository
            .read(cx)
            .work_directory_abs_path
            .as_ref()
            .to_path_buf();
        let sha = self.commit.sha.to_string();
        let project = self.project.clone();
        let cache_ttl_days = GitPanelSettings::get_global(cx)
            .commit_explanations
            .cache_ttl_days;

        self.explain_pending = true;
        self.explain_expanded = true;
        self.explain_error = None;
        cx.notify();

        let task = cx.spawn_in(window, async move |this, cx| {
            let outcome = ai_explain::explain_commit(
                &work_dir,
                &sha,
                &project,
                cache_ttl_days,
                &mut cx.clone(),
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                view.explain_pending = false;
                match outcome {
                    Ok(out) => {
                        view.explain_body = Some(SharedString::from(out.text));
                        view.explain_from_cache = out.source == ai_explain::ExplainSource::Cached;
                        view.explain_expanded = true;
                    }
                    Err(err) => {
                        log::warn!("AI commit explain failed: {err:#}");
                        view.explain_error = Some(SharedString::from(format!(
                            "Couldn't explain commit: {err}"
                        )));
                    }
                }
                cx.notify();
            });
        });
        self._explain_task = Some(task);
    }

    /// Reload the diff for a different parent (1-based merge-parent toggle).
    fn select_parent_index(&mut self, parent_index: usize, cx: &mut Context<Self>) {
        if parent_index == self.selected_parent_index {
            return;
        }
        self.selected_parent_index = parent_index;
        let sha = self.commit.sha.to_string();
        let task = self.repository.update(cx, |repo, _| {
            repo.load_commit_diff_against_parent(sha, parent_index)
        });
        cx.spawn(async move |this, cx| {
            if let Some(diff) = task.await.log_err().and_then(|res| res.log_err()) {
                this.update(cx, |view, cx| {
                    view.diff_files = diff.files.iter().map(clone_commit_file).collect();
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn render_metadata_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let gutter_width = self.editor.update(cx, |editor, cx| {
            let editor = editor.rhs_editor().clone();
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                let style = editor.style(cx);
                let font_id = window.text_system().resolve_font(&style.text.font());
                let font_size = style.text.font_size.to_pixels(window.rem_size());
                snapshot
                    .gutter_dimensions(font_id, font_size, style, window, cx)
                    .full_width()
            })
        });

        let head_branch_name = self
            .repository
            .read(cx)
            .snapshot()
            .branch
            .as_ref()
            .map(|branch| SharedString::from(branch.name().to_string()));

        let explain_state = header::ExplainHeaderState {
            pending: self.explain_pending,
            body: self.explain_body.clone(),
            error: self.explain_error.clone(),
            from_cache: self.explain_from_cache,
            expanded: self.explain_expanded,
            disabled_reason: self.explain_disabled_reason(cx),
            on_click: Arc::new(cx.listener(|view, _event, window, cx| {
                view.toggle_or_request_explain(window, cx);
            })),
        };
        let header = header::render_header(
            &self.commit,
            self.remote.as_ref(),
            self.extra_committer.clone(),
            self.stash.is_some(),
            gutter_width,
            explain_state,
            window,
            cx,
        );

        let parents = parents_bar::render_parents_bar(&self.parents);
        let refs = refs_bar::render_refs_bar(&self.ref_names, head_branch_name.as_ref());
        let contains = self.contains_panel.render(cx);

        let affected = if self.diff_files.is_empty() {
            None
        } else {
            Some(affected_files::render_affected_files(
                &self.diff_files,
                &self.affected_files,
                cx,
            ))
        };

        v_flex()
            .gap_1()
            .child(header)
            .when_some(parents, |this, el| {
                this.child(div().px_2().pt_1p5().child(el))
            })
            .when_some(refs, |this, el| this.child(div().px_2().child(el)))
            .when_some(contains, |this, el| this.child(div().px_2().child(el)))
            .when_some(affected, |this, el| {
                this.child(div().px_2().pt_1p5().child(el))
            })
    }

    fn render_inline_footer(&self, cx: &mut App) -> impl IntoElement {
        footer::render_footer(&self.commit.sha, self.stash.is_some(), self.in_pane, cx)
    }

    fn apply_stash(workspace: &mut Workspace, window: &mut Window, cx: &mut App) {
        Self::stash_action(
            workspace,
            "Apply",
            window,
            cx,
            async move |repository, sha, stash, commit_view, workspace, cx| {
                let result = repository.update(cx, |repo, cx| {
                    if !stash_matches_index(&sha, stash, repo) {
                        return Err(anyhow::anyhow!("Stash has changed, not applying"));
                    }
                    Ok(repo.stash_apply(Some(stash), cx))
                });

                match result {
                    Ok(task) => task.await?,
                    Err(err) => {
                        Self::close_commit_view(commit_view, workspace, cx).await?;
                        return Err(err);
                    }
                };
                Self::close_commit_view(commit_view, workspace, cx).await?;
                anyhow::Ok(())
            },
        );
    }

    fn pop_stash(workspace: &mut Workspace, window: &mut Window, cx: &mut App) {
        Self::stash_action(
            workspace,
            "Pop",
            window,
            cx,
            async move |repository, sha, stash, commit_view, workspace, cx| {
                let result = repository.update(cx, |repo, cx| {
                    if !stash_matches_index(&sha, stash, repo) {
                        return Err(anyhow::anyhow!("Stash has changed, pop aborted"));
                    }
                    Ok(repo.stash_pop(Some(stash), cx))
                });

                match result {
                    Ok(task) => task.await?,
                    Err(err) => {
                        Self::close_commit_view(commit_view, workspace, cx).await?;
                        return Err(err);
                    }
                };
                Self::close_commit_view(commit_view, workspace, cx).await?;
                anyhow::Ok(())
            },
        );
    }

    fn remove_stash(workspace: &mut Workspace, window: &mut Window, cx: &mut App) {
        Self::stash_action(
            workspace,
            "Drop",
            window,
            cx,
            async move |repository, sha, stash, commit_view, workspace, cx| {
                let result = repository.update(cx, |repo, cx| {
                    if !stash_matches_index(&sha, stash, repo) {
                        return Err(anyhow::anyhow!("Stash has changed, drop aborted"));
                    }
                    Ok(repo.stash_drop(Some(stash), cx))
                });

                match result {
                    Ok(task) => task.await??,
                    Err(err) => {
                        Self::close_commit_view(commit_view, workspace, cx).await?;
                        return Err(err);
                    }
                };
                Self::close_commit_view(commit_view, workspace, cx).await?;
                anyhow::Ok(())
            },
        );
    }

    fn stash_action<AsyncFn>(
        workspace: &mut Workspace,
        str_action: &str,
        window: &mut Window,
        cx: &mut App,
        callback: AsyncFn,
    ) where
        AsyncFn: AsyncFnOnce(
                Entity<Repository>,
                &SharedString,
                usize,
                Entity<CommitView>,
                WeakEntity<Workspace>,
                &mut AsyncWindowContext,
            ) -> anyhow::Result<()>
            + 'static,
    {
        let Some(commit_view) = workspace.active_item_as::<CommitView>(cx) else {
            return;
        };
        let Some(stash) = commit_view.read(cx).stash else {
            return;
        };
        let sha = commit_view.read(cx).commit.sha.clone();
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("{} stash@{{{}}}?", str_action, stash),
            None,
            &[str_action, "Cancel"],
            cx,
        );

        let workspace_weak = workspace.weak_handle();
        let commit_view_entity = commit_view;

        window
            .spawn(cx, async move |cx| {
                if answer.await != Ok(0) {
                    return anyhow::Ok(());
                }

                let Some(workspace) = workspace_weak.upgrade() else {
                    return Ok(());
                };

                let repo = workspace.update(cx, |workspace, cx| {
                    workspace
                        .panel::<GitPanel>(cx)
                        .and_then(|p| p.read(cx).active_repository.clone())
                });

                let Some(repo) = repo else {
                    return Ok(());
                };

                callback(repo, &sha, stash, commit_view_entity, workspace_weak, cx).await?;
                anyhow::Ok(())
            })
            .detach_and_notify_err(workspace.weak_handle(), window, cx);
    }

    async fn close_commit_view(
        commit_view: Entity<CommitView>,
        workspace: WeakEntity<Workspace>,
        cx: &mut AsyncWindowContext,
    ) -> anyhow::Result<()> {
        workspace
            .update_in(cx, |workspace, window, cx| {
                let active_pane = workspace.active_pane();
                let commit_view_id = commit_view.entity_id();
                active_pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(commit_view_id, SaveIntent::Skip, window, cx)
                })
            })?
            .await?;
        anyhow::Ok(())
    }
}

fn clone_commit_file(file: &git::repository::CommitFile) -> git::repository::CommitFile {
    git::repository::CommitFile {
        path: file.path.clone(),
        old_text: file.old_text.clone(),
        new_text: file.new_text.clone(),
        is_binary: file.is_binary,
    }
}

struct LoadedCommitMetadata {
    parents: Vec<SharedString>,
    ref_names: Vec<SharedString>,
    extra_committer: Option<(SharedString, SharedString)>,
}

async fn load_commit_metadata(
    work_dir: &std::path::Path,
    sha: &str,
) -> Result<LoadedCommitMetadata> {
    use util::command::new_command;
    // %H<NUL>%P<NUL>%D<NUL>%an<NUL>%ae<NUL>%cn<NUL>%ce
    let format = "--format=%H%x00%P%x00%D%x00%an%x00%ae%x00%cn%x00%ce";
    let mut cmd = new_command("git");
    cmd.current_dir(work_dir);
    cmd.args(["show", "--no-patch", "--decorate=full", format, sha]);
    let output = cmd
        .output()
        .await
        .context("spawning git show for metadata")?;
    if !output.status.success() {
        anyhow::bail!(
            "git show --format= failed: {}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .context("git show metadata output not utf-8")?
        .trim_end_matches('\n');
    let mut parts = stdout.splitn(7, '\x00');
    let _full_sha = parts.next().unwrap_or("");
    let parents = parts
        .next()
        .unwrap_or("")
        .split_whitespace()
        .map(|s| SharedString::from(s.to_string()))
        .collect();
    let refs_raw = parts.next().unwrap_or("");
    let ref_names: Vec<SharedString> = if refs_raw.is_empty() {
        Vec::new()
    } else {
        refs_raw
            .split(", ")
            .map(|s| SharedString::from(s.to_string()))
            .collect()
    };
    let author_name = parts.next().unwrap_or("");
    let author_email = parts.next().unwrap_or("");
    let committer_name = parts.next().unwrap_or("");
    let committer_email = parts.next().unwrap_or("");
    let extra_committer = if !committer_name.is_empty()
        && (committer_name != author_name || committer_email != author_email)
    {
        Some((
            SharedString::from(committer_name.to_string()),
            SharedString::from(committer_email.to_string()),
        ))
    } else {
        None
    };
    Ok(LoadedCommitMetadata {
        parents,
        ref_names,
        extra_committer,
    })
}

impl language::File for GitBlob {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic {
            was_deleted: self.is_deleted,
        }
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::local()
    }

    fn path(&self) -> &Arc<RelPath> {
        self.path.as_ref()
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.path.as_std_path().to_path_buf()
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        self.display_name.as_ref()
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, _cx: &App) -> language::proto::File {
        // Synthetic CommitView buffers never travel over the collab wire —
        // collab is disabled in this fork (.rules § "What's disabled"), so
        // `to_proto` is unreachable. If collab is ever re-enabled,
        // CommitView's read-only synthetic blobs would need a real
        // serialization shape; until then `unreachable!` is correct.
        unreachable!("CommitView synthetic File never serializes — collab disabled")
    }

    fn is_private(&self) -> bool {
        false
    }

    fn can_open(&self) -> bool {
        !self.is_binary
    }
}

async fn build_buffer(
    mut text: String,
    blob: Arc<dyn File>,
    language_registry: &Arc<language::LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Buffer>> {
    let line_ending = LineEnding::detect(&text);
    LineEnding::normalize(&mut text);
    let text = Rope::from(text);
    let language =
        cx.update(|_, cx| language_registry.language_for_file(&blob, Some(&text), cx))?;
    let language = if let Some(language) = language {
        language_registry
            .load_language(&language)
            .await
            .ok()
            .and_then(|e| e.log_err())
    } else {
        None
    };
    let buffer = cx.new(|cx| {
        let buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            text,
        );
        let mut buffer = Buffer::build(buffer, Some(blob), Capability::ReadWrite);
        buffer.set_language_async(language, cx);
        buffer
    });
    Ok(buffer)
}

async fn build_buffer_diff(
    mut old_text: Option<String>,
    buffer: &Entity<Buffer>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<BufferDiff>> {
    if let Some(old_text) = &mut old_text {
        LineEnding::normalize(old_text);
    }

    let language = cx.update(|_, cx| buffer.read(cx).language().cloned())?;
    let buffer = cx.update(|_, cx| buffer.read(cx).snapshot())?;

    let diff =
        cx.new(|cx| BufferDiff::new(&buffer.text, language, Some(language_registry.clone()), cx));

    diff.update(cx, |diff, cx| {
        diff.set_base_text(
            old_text.map(|old_text| Arc::from(old_text.as_str())),
            buffer.text.clone(),
            cx,
        )
    })
    .await;

    Ok(diff)
}

impl EventEmitter<EditorEvent> for CommitView {}

impl Focusable for CommitView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for CommitView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitCommit).color(Color::Muted))
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
        if let Some(path) = &self.single_file {
            return path
                .file_name()
                .map(|name| name.to_string())
                .unwrap_or_else(|| path.as_unix_str().to_string())
                .into();
        }
        let short_sha = self.commit.sha.get(0..7).unwrap_or(&*self.commit.sha);
        let subject = truncate_and_trailoff(self.commit.message.split('\n').next().unwrap(), 20);
        format!("{short_sha} — {subject}").into()
    }

    fn tab_tooltip_content(&self, _: &App) -> Option<TabTooltipContent> {
        let short_sha = self.commit.sha.get(0..16).unwrap_or(&*self.commit.sha);
        let subject = self.commit.message.split('\n').next().unwrap();

        if let Some(path) = &self.single_file {
            let path = path.as_unix_str().to_string();
            let short_sha = short_sha.to_string();
            return Some(TabTooltipContent::Custom(Box::new(Tooltip::element(
                move |_, _| {
                    v_flex()
                        .child(Label::new(path.clone()))
                        .child(
                            Label::new(format!("at {short_sha}"))
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                        .into_any_element()
                },
            ))));
        }

        Some(TabTooltipContent::Custom(Box::new(Tooltip::element({
            let subject = subject.to_string();
            let short_sha = short_sha.to_string();

            move |_, _| {
                v_flex()
                    .child(Label::new(subject.clone()))
                    .child(
                        Label::new(short_sha.clone())
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            }
        }))))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Commit View Opened")
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.editor.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.read(cx).rhs_editor().clone().into())
        } else {
            None
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
        self.editor.read(cx).for_each_project_item(cx, f)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
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
            });
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        let file_statuses = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .addon::<CommitDiffAddon>()
            .map(|addon| addon.file_statuses.clone())
            .unwrap_or_default();
        let Some(workspace_entity) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let project = self.project.clone();
        let diff_view_style = self.editor.read(cx).diff_view_style();
        let multibuffer = self.multibuffer.clone();
        let parents = self.parents.clone();
        let ref_names = self.ref_names.clone();
        let extra_committer = self.extra_committer.clone();
        let diff_files = self.diff_files.iter().map(clone_commit_file).collect();
        let lazy_threshold = self.affected_files.lazy_threshold;
        Task::ready(Some(cx.new(|cx| {
            let commit_view = cx.weak_entity();
            let editor = cx.new({
                let file_statuses = file_statuses.clone();
                let project = project.clone();
                let workspace_entity = workspace_entity.clone();
                let multibuffer = multibuffer.clone();
                // Reborrow `window` so the `move` closure consumes the reborrow
                // (which ends when `cx.new` returns) rather than the caller's
                // `&mut Window`, leaving it usable afterwards.
                let window = &mut *window;
                move |cx| {
                    let editor = SplittableEditor::new(
                        diff_view_style,
                        multibuffer.clone(),
                        project.clone(),
                        workspace_entity.clone(),
                        window,
                        cx,
                    );
                    editor.disable_diff_hunk_controls(cx);
                    editor.rhs_editor().update(cx, |editor, cx| {
                        editor.set_show_bookmarks(false, cx);
                        editor.set_show_breakpoints(false, cx);
                        editor.set_show_diff_review_button(true, cx);
                        editor.register_addon(CommitDiffAddon {
                            file_statuses,
                            commit_view,
                        });
                    });
                    editor
                }
            });
            let language_registry = project.read(cx).languages().clone();
            let message = cx.new(|cx| {
                Markdown::new(
                    self.commit.message.clone(),
                    Some(language_registry),
                    None,
                    cx,
                )
            });
            let affected_files = CommitAffectedFiles::new(lazy_threshold, window, cx);
            Self {
                editor,
                message,
                message_expanded: self.message_expanded,
                message_scroll_handle: ScrollHandle::new(),
                multibuffer: self.multibuffer.clone(),
                commit: self.commit.clone(),
                stash: self.stash,
                repository: self.repository.clone(),
                project: self.project.clone(),
                workspace: self.workspace.clone(),
                remote: self.remote.clone(),
                parents,
                ref_names,
                extra_committer,
                selected_parent_index: self.selected_parent_index,
                diff_files,
                affected_files,
                contains_panel: CommitContainsPanel::new(),
                in_pane: true,
                explain_body: self.explain_body.clone(),
                explain_from_cache: self.explain_from_cache,
                explain_pending: false,
                explain_expanded: self.explain_expanded,
                explain_error: self.explain_error.clone(),
                _explain_task: None,
                single_file: self.single_file.clone(),
            }
        })))
    }
}

impl Render for CommitView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_stash = self.stash.is_some();

        let base = v_flex()
            .key_context(if is_stash { "StashDiff" } else { "CommitDiff" })
            .on_action(cx.listener(Self::open_file_at_head_action))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(|view, _: &ExplainCommit, window, cx| {
                view.toggle_or_request_explain(window, cx);
            }));

        // Single-file diff mode — just the diff editor, no commit metadata.
        if self.single_file.is_some() {
            return base.when(!self.editor.read(cx).rhs_editor().read(cx).is_empty(cx), |this| {
                this.child(div().flex_grow(1.).child(self.editor.clone()))
            });
        }

        base.child(self.render_metadata_panel(window, cx))
            .when(!self.editor.read(cx).rhs_editor().read(cx).is_empty(cx), |this| {
                this.child(div().flex_grow(1.).child(self.editor.clone()))
            })
            .child(self.render_inline_footer(cx))
    }
}

pub struct CommitViewToolbar {
    commit_view: Option<WeakEntity<CommitView>>,
}

impl CommitViewToolbar {
    pub fn new() -> Self {
        Self { commit_view: None }
    }
}

impl EventEmitter<ToolbarItemEvent> for CommitViewToolbar {}

impl Render for CommitViewToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(commit_view) = self.commit_view.as_ref().and_then(|w| w.upgrade()) else {
            return div();
        };

        let commit_view_ref = commit_view.read(cx);
        let is_stash = commit_view_ref.stash.is_some();

        let (additions, deletions) = commit_view_ref.calculate_changed_lines(cx);

        let commit_sha = commit_view_ref.commit.sha.clone();
        let parents = commit_view_ref.parents.clone();
        let selected_parent_index = commit_view_ref.selected_parent_index;

        let remote_info = commit_view_ref.remote.as_ref().map(|remote| {
            let provider = remote.host.name();
            let parsed_remote = ParsedGitRemote {
                owner: remote.owner.as_ref().into(),
                repo: remote.repo.as_ref().into(),
            };
            let params = BuildCommitPermalinkParams { sha: &commit_sha };
            let url = remote
                .host
                .build_commit_permalink(&parsed_remote, params)
                .to_string();
            (provider, url)
        });

        let sha_for_graph = commit_sha.to_string();
        let commit_view_for_parent = commit_view.downgrade();

        h_flex()
            .gap_1()
            .when(additions > 0 || deletions > 0, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(DiffStat::new(
                            "toolbar-diff-stat",
                            additions as usize,
                            deletions as usize,
                        ))
                        .child(Divider::vertical()),
                )
            })
            .when(parents.len() > 1, |this| {
                this.child(render_parent_toggle(
                    selected_parent_index,
                    parents.len(),
                    commit_view_for_parent.clone(),
                ))
                .child(Divider::vertical())
            })
            .child(
                IconButton::new("buffer-search", IconName::MagnifyingGlass)
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
            .when(!is_stash, |this| {
                this.child(
                    IconButton::new("show-in-git-graph", IconName::GitGraph)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Show in Git Graph"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::git_panel::OpenAtCommit {
                                    sha: sha_for_graph.clone(),
                                }),
                                cx,
                            );
                        }),
                )
                .children(remote_info.map(|(provider_name, url)| {
                    let icon = crate::get_provider_icon(provider_name.as_str());

                    IconButton::new("view_on_provider", icon)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text(format!("View on {}", provider_name)))
                        .on_click(move |_, _, cx| cx.open_url(&url))
                }))
            })
    }
}

fn render_parent_toggle(
    selected: usize,
    parent_count: usize,
    commit_view: WeakEntity<CommitView>,
) -> AnyElement {
    let label = format!("Diff vs parent: {}", selected.max(1).min(parent_count));
    Button::new("merge-parent-toggle", label)
        .style(ButtonStyle::Subtle)
        .label_size(LabelSize::Small)
        .tooltip(Tooltip::text(
            "Cycle through merge-commit parents to diff against",
        ))
        .on_click(move |_, _, cx| {
            let next = if selected >= parent_count {
                1
            } else {
                selected + 1
            };
            commit_view
                .update(cx, |view, cx| {
                    view.select_parent_index(next, cx);
                })
                .ok();
        })
        .into_any_element()
}

impl ToolbarItemView for CommitViewToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        if let Some(entity) = active_pane_item.and_then(|i| i.act_as::<CommitView>(cx)) {
            self.commit_view = Some(entity.downgrade());
            return ToolbarItemLocation::PrimaryRight;
        }
        self.commit_view = None;
        ToolbarItemLocation::Hidden
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

fn stash_matches_index(sha: &str, stash_index: usize, repo: &Repository) -> bool {
    repo.stash_entries
        .entries
        .get(stash_index)
        .map(|entry| entry.oid.to_string() == sha)
        .unwrap_or(false)
}
