//! The git panel's Diff tab: the files changed between two revisions, each
//! opening its own single-file diff (FORK.md #208).
//!
//! Opened by the git graph's "Compare Versions" on a two-commit selection and
//! by a commit's "Compare with Local Working Tree". It replaces the one editor
//! tab that used to hold every changed file of the comparison in a single
//! multibuffer: the tab lists the files the way the Commit tab lists a
//! commit's, and a file row opens that file's diff alone in the shared diff
//! tab, with the same click gestures. There is one Diff tab; a new comparison
//! re-points it.

use super::commit_tab::{
    ChangedFileEntry, ChangedFileTree, DiffLineCount, FileDiffTarget, FileTreeOwner, LoadState,
    LoadedCommitDiff, changed_file_entries, compute_diff_stats,
};
use super::*;
use git::status::{DiffTreeType, TreeDiff};
use gpui::{AsyncApp, EntityId};

/// The right side of the comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffTabHead {
    /// Another commit — the newer of the two "Compare Versions" selected.
    Commit(Oid),
    /// The working tree, as it is now.
    WorkingTree,
}

/// What the tab lists, once loaded.
pub(super) enum LoadedComparison {
    /// Two commits: every file with both texts, so rows carry `+N −M`.
    Commits(LoadedCommitDiff),
    /// A commit and the working tree: names and statuses only. The working
    /// tree side changes under the tab, and re-reading every file's text on
    /// each status event to redraw figures would be the expensive half.
    Local(TreeDiff),
}

impl LoadedComparison {
    fn file_count(&self) -> usize {
        match self {
            Self::Commits(loaded) => loaded.diff.files.len(),
            Self::Local(tree_diff) => tree_diff.entries.len(),
        }
    }

    fn total(&self) -> Option<DiffLineCount> {
        match self {
            Self::Commits(loaded) => Some(loaded.stats.total),
            Self::Local(_) => None,
        }
    }

    fn entries(&self, show_stats: bool) -> Vec<ChangedFileEntry> {
        match self {
            Self::Commits(loaded) => changed_file_entries(loaded, show_stats),
            Self::Local(tree_diff) => tree_diff
                .entries
                .iter()
                .map(|(path, status)| ChangedFileEntry::from_tree_diff(path, status))
                .collect(),
        }
    }

    #[cfg(test)]
    pub(super) fn paths(&self) -> Vec<RepoPath> {
        let mut paths: Vec<RepoPath> = match self {
            Self::Commits(loaded) => loaded.diff.files.iter().map(|f| f.path.clone()).collect(),
            Self::Local(tree_diff) => tree_diff.entries.keys().cloned().collect(),
        };
        paths.sort();
        paths
    }
}

/// Everything the Diff tab shows.
///
/// Like the Commit tab's state it hangs off [`GitPanel`] as an `Option`, and
/// `Some` is exactly what puts the tab in the tab bar.
pub(super) struct DiffTabState {
    pub(super) repository: Entity<Repository>,
    /// The left side — the older commit.
    pub(super) base: Oid,
    pub(super) head: DiffTabHead,
    /// The commits' subjects, for the header; empty when the caller had none.
    base_subject: SharedString,
    head_subject: SharedString,
    pub(super) files: LoadState<LoadedComparison>,
    pub(super) collapsed_dirs: HashSet<SharedString>,
    pub(super) selected_file: Option<RepoPath>,
    scroll_handle: UniformListScrollHandle,
    _load_task: Option<Task<()>>,
}

impl DiffTabState {
    fn target(&self) -> FileDiffTarget {
        let base: SharedString = self.base.to_string().into();
        match self.head {
            DiffTabHead::Commit(head) => FileDiffTarget::Range {
                base,
                head: head.to_string().into(),
            },
            DiffTabHead::WorkingTree => FileDiffTarget::Local { base },
        }
    }
}

impl GitPanel {
    pub fn diff_tab_is_open(&self) -> bool {
        self.diff_tab.is_some()
    }

    /// `(base, head)` of the open Diff tab.
    pub fn diff_tab_comparison(&self) -> Option<(Oid, DiffTabHead)> {
        self.diff_tab.as_ref().map(|state| (state.base, state.head))
    }

    /// Point the Diff tab at `base` against `head` in `repository` and make it
    /// the active tab. Re-showing the comparison the tab already holds keeps
    /// its state — the files, the cursor, the collapsed directories — and a
    /// different one replaces it: there is only ever one Diff tab.
    pub fn show_comparison(
        &mut self,
        repository: Entity<Repository>,
        base: Oid,
        head: DiffTabHead,
        base_subject: SharedString,
        head_subject: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let already_shown = self.diff_tab.as_ref().is_some_and(|state| {
            state.base == base
                && state.head == head
                && state.repository.entity_id() == repository.entity_id()
                && !matches!(state.files, LoadState::Failed(_))
        });
        if !already_shown {
            self.diff_tab = Some(DiffTabState {
                repository,
                base,
                head,
                base_subject,
                head_subject,
                files: LoadState::Idle,
                collapsed_dirs: HashSet::default(),
                selected_file: None,
                scroll_handle: UniformListScrollHandle::new(),
                _load_task: None,
            });
            self.load_diff_tab(cx);
        } else if head == DiffTabHead::WorkingTree {
            // The working tree may have moved on since the tab last listed it.
            self.load_diff_tab(cx);
        }
        self.set_active_tab(GitPanelTab::Diff, window, cx);
        cx.notify();
    }

    /// Re-list a comparison with the working tree after `repository_id`'s
    /// files changed. Two commits never change, so that tab is left alone.
    pub(super) fn refresh_local_diff_tab(
        &mut self,
        repository_id: RepositoryId,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.diff_tab.as_ref() else {
            return;
        };
        if state.head == DiffTabHead::WorkingTree && state.repository.read(cx).id == repository_id {
            self.load_diff_tab(cx);
        }
    }

    fn load_diff_tab(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.diff_tab.as_mut() else {
            return;
        };
        let (base, head) = (state.base, state.head);
        let target = (state.repository.entity_id(), base, head);
        let task = match head {
            DiffTabHead::Commit(head_sha) => {
                let diff = state.repository.update(cx, |repository, _| {
                    repository.load_commit_range(base.to_string(), head_sha.to_string())
                });
                cx.spawn(async move |this, cx| {
                    let loaded = match diff.await {
                        Ok(Ok(diff)) => {
                            let stats = compute_diff_stats(&diff);
                            Ok(LoadedComparison::Commits(LoadedCommitDiff { diff, stats }))
                        }
                        Ok(Err(error)) => Err(format!("{error:#}")),
                        Err(_) => Err("cancelled".to_string()),
                    };
                    Self::finish_diff_tab_load(this, target, loaded, cx).await;
                })
            }
            DiffTabHead::WorkingTree => {
                let tree_diff = state.repository.update(cx, |repository, cx| {
                    repository.diff_tree(
                        DiffTreeType::SinceWithWorktree {
                            base: base.to_string().into(),
                        },
                        cx,
                    )
                });
                cx.spawn(async move |this, cx| {
                    let loaded = match tree_diff.await {
                        Ok(Ok(tree_diff)) => Ok(LoadedComparison::Local(tree_diff)),
                        Ok(Err(error)) => Err(format!("{error:#}")),
                        Err(_) => Err("cancelled".to_string()),
                    };
                    Self::finish_diff_tab_load(this, target, loaded, cx).await;
                })
            }
        };
        // A refresh keeps the rows it already shows until the new ones land,
        // rather than flashing "Loading…" on every status event.
        if !matches!(state.files, LoadState::Loaded(_)) {
            state.files = LoadState::Loading;
        }
        state._load_task = Some(task);
    }

    async fn finish_diff_tab_load(
        this: WeakEntity<Self>,
        target: (EntityId, Oid, DiffTabHead),
        loaded: std::result::Result<LoadedComparison, String>,
        cx: &mut AsyncApp,
    ) {
        this.update(cx, |this, cx| {
            let Some(state) = this.diff_tab.as_mut() else {
                return;
            };
            // The tab may have been re-pointed while this was loading.
            if (state.repository.entity_id(), state.base, state.head) != target {
                return;
            }
            state.files = match loaded {
                Ok(loaded) => LoadState::Loaded(loaded),
                Err(error) => LoadState::Failed(SharedString::from(format!(
                    "Couldn't compare {} with {}: {error}",
                    state.base.display_short(),
                    match state.head {
                        DiffTabHead::Commit(head) => head.display_short(),
                        DiffTabHead::WorkingTree => "the local changes".to_string(),
                    }
                ))),
            };
            cx.notify();
        })
        .ok();
    }

    pub fn close_diff_tab(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.diff_tab.take().is_none() {
            return;
        }
        // Only the Diff tab itself is yanked back to Changes; a user parked on
        // another tab keeps the tab they chose.
        if self.active_tab == GitPanelTab::Diff {
            self.active_tab = GitPanelTab::Changes;
        }
        cx.notify();
    }

    pub(super) fn activate_diff_tab(
        &mut self,
        _: &ActivateDiffTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_tab(GitPanelTab::Diff, window, cx);
    }

    /// The row a Diff tab file's open diff marks, if the centre pane is
    /// showing one of this comparison's files.
    fn diff_tab_open_file(&self, state: &DiffTabState) -> Option<RepoPath> {
        let open_diff = self.open_diff.as_ref()?;
        let base = state.base.to_string();
        match state.head {
            DiffTabHead::Commit(head) => open_diff.range_file(&base, &head.to_string()).cloned(),
            DiffTabHead::WorkingTree => open_diff.local_file(&base).cloned(),
        }
    }

    /// The Diff tab body: what is being compared with what, the changed
    /// files' count and +/− totals, then the directory-grouped file tree.
    pub(super) fn render_diff_tab(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(state) = self.diff_tab.as_ref() else {
            return Empty.into_any_element();
        };
        let (head_sha, head_subject): (Option<Oid>, SharedString) = match state.head {
            DiffTabHead::Commit(head) => (Some(head), state.head_subject.clone()),
            DiffTabHead::WorkingTree => (None, "Local changes".into()),
        };
        let mut body = v_flex()
            .debug_selector(|| "DIFF-TAB-BODY".into())
            .flex_1()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .child(
                div().flex_shrink_0().px_2().pt_1p5().child(
                    Label::new("Diff between")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(render_revision_row(
                "From",
                Some(state.base),
                &state.base_subject,
            ))
            .child(render_revision_row("To", head_sha, &head_subject))
            .child(div().pt_1().child(Divider::horizontal()));

        let show_diff_stats = GitPanelSettings::get_global(cx).diff_stats;
        body = match &state.files {
            LoadState::Loaded(loaded) => {
                let file_count = loaded.file_count();
                body.child(
                    h_flex()
                        .flex_shrink_0()
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .justify_between()
                        .child(
                            Label::new(format!(
                                "{file_count} Changed {}",
                                if file_count == 1 { "File" } else { "Files" }
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        .when_some(loaded.total().filter(|_| show_diff_stats), |this, total| {
                            this.child(ui::DiffStat::new(
                                "diff-tab-diff-stat",
                                total.added,
                                total.removed,
                            ))
                        }),
                )
                .when(file_count == 0, |this| {
                    this.child(
                        div().flex_shrink_0().px_2().py_1p5().child(
                            Label::new("No differences.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                })
                .when(file_count > 0, |this| {
                    this.child(self.render_changed_file_tree(
                        ChangedFileTree {
                            owner: FileTreeOwner::Diff,
                            id: "diff-tab-files",
                            repository: &state.repository,
                            entries: loaded.entries(show_diff_stats),
                            collapsed_dirs: &state.collapsed_dirs,
                            selected_file: state.selected_file.clone(),
                            scroll_handle: state.scroll_handle.clone(),
                            target: state.target(),
                            open_file: self.diff_tab_open_file(state),
                        },
                        window,
                        cx,
                    ))
                })
            }
            LoadState::Failed(error) => body.child(
                div().flex_shrink_0().px_2().py_1p5().child(
                    Label::new(error.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                ),
            ),
            LoadState::Loading | LoadState::Idle => body.child(
                div().flex_shrink_0().px_2().py_1p5().child(
                    Label::new("Loading changed files…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            ),
        };
        body.into_any_element()
    }
}

/// One side of the header: `From  1a2b3c4  Subject of the commit`, or
/// `To  Local changes` for the working tree.
fn render_revision_row(
    side: &'static str,
    sha: Option<Oid>,
    subject: &SharedString,
) -> impl IntoElement {
    h_flex()
        .flex_shrink_0()
        .w_full()
        .min_w_0()
        .px_2()
        .gap_2()
        .child(
            div()
                .w(rems(2.5))
                .flex_none()
                .child(Label::new(side).size(LabelSize::Small).color(Color::Muted)),
        )
        .children(sha.map(|sha| {
            Label::new(sha.display_short())
                .size(LabelSize::Small)
                .color(Color::Accent)
        }))
        .child(
            Label::new(subject.clone())
                .size(LabelSize::Small)
                .truncate(),
        )
}
