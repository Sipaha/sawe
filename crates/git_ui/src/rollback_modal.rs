//! IDEA's "Rollback Changes" dialog — the confirmation that stands between a
//! rollback and the working-tree changes it would throw away.
//!
//! Opened by [`crate::git_panel::GitPanel::open_rollback_modal`] (the
//! `Rollback...` row of the Changes list's context menu, the `Rollback Tracked
//! Changes...` panel entry, and the `git::RestoreFile` action). The modal *is*
//! the confirmation — there is no second two-button prompt behind it — and it
//! narrows the set: only the files whose checkbox is checked are handed to
//! [`crate::git_panel::GitPanel::perform_rollback`].
//!
//! The tree itself is the git panel's own tree builder
//! ([`TreeViewState::build_tree_entries`]) rather than a second implementation:
//! the modal owns a private `TreeViewState`, feeds it the candidate entries and
//! renders the rows it gets back under a synthetic `Changes` root and a
//! repository row. That also buys the directory→file descendant map the
//! check-state propagation needs, for free.

use std::ops::Range;

use collections::HashSet;
use git::repository::RepoPath;
use git::status::FileStatus;
use gpui::{
    AnyElement, DismissEvent, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window,
    div, px, uniform_list,
};
use ui::{
    App, Button, ButtonCommon, ButtonStyle, Checkbox, Clickable, Color, Context, Disableable,
    Disclosure, Headline, HeadlineSize, Icon, IconButton, IconName, IconSize, IntoElement, Label,
    LabelCommon, LabelSize, StyledExt, TintColor, ToggleState, Tooltip, h_flex, rems, v_flex,
};
use util::ResultExt as _;
use workspace::ModalView;

use crate::git_panel::{
    GitListEntry, GitPanel, GitStatusEntry, Section, TreeKey, TreeViewState, file_count_label,
};
use crate::git_status_icon;

/// The dialog shows one changelist, not the panel's tracked / untracked /
/// conflicted split, so every entry is built under a single section. `Section`
/// is nothing but the namespace half of a [`TreeKey`] here.
const ROLLBACK_SECTION: Section = Section::Tracked;

/// What a row of the rollback tree stands for.
#[derive(Clone)]
pub(crate) enum RollbackRow {
    /// The changelist root — `Changes  N files`.
    Root,
    /// The repository the changes belong to.
    Repository,
    Directory {
        key: TreeKey,
        name: SharedString,
        expanded: bool,
    },
    File(GitStatusEntry),
}

pub(crate) struct RollbackRowEntry {
    row: RollbackRow,
    depth: usize,
    /// False when an ancestor directory is collapsed.
    visible: bool,
}

/// The checkable tree behind the dialog. Deliberately free of `Window` and
/// `Entity`, so the selection rules it encodes are unit-testable on their own.
pub(crate) struct RollbackTree {
    entries: Vec<GitStatusEntry>,
    tree_state: TreeViewState,
    rows: Vec<RollbackRowEntry>,
    visible_indices: Vec<usize>,
    checked: HashSet<RepoPath>,
}

impl RollbackTree {
    pub(crate) fn new(mut entries: Vec<GitStatusEntry>) -> Self {
        // The tree builder sorts internally; sorting the backing list too keeps
        // `checked_entries` in the same order the user sees, which is the order
        // the rollback then reports on.
        entries.sort_by(|a, b| a.repo_path.cmp(&b.repo_path));
        let checked = entries
            .iter()
            .map(|entry| entry.repo_path.clone())
            .collect();
        let mut this = Self {
            entries,
            tree_state: TreeViewState::default(),
            rows: Vec::new(),
            visible_indices: Vec::new(),
            checked,
        };
        this.rebuild();
        this
    }

    /// Re-flatten the rows from `tree_state`. Cheap enough to run on every
    /// expand/collapse: `build_tree_entries` preserves the expansion map, so
    /// rebuilding is what applies it.
    fn rebuild(&mut self) {
        let mut seen_directories = HashSet::default();
        let entries = self.entries.clone();
        let flattened =
            self.tree_state
                .build_tree_entries(ROLLBACK_SECTION, entries, &mut seen_directories);
        self.tree_state
            .expanded_dirs
            .retain(|key, _| seen_directories.contains(key));

        let mut rows = vec![
            RollbackRowEntry {
                row: RollbackRow::Root,
                depth: 0,
                visible: true,
            },
            RollbackRowEntry {
                row: RollbackRow::Repository,
                depth: 1,
                visible: true,
            },
        ];
        for (entry, visible) in flattened {
            // The panel's depths start at the section's content; the dialog
            // hangs them under its own root and repository rows.
            let depth = entry.depth() + 2;
            let row = match entry {
                GitListEntry::Directory(dir) => RollbackRow::Directory {
                    key: dir.key,
                    name: dir.name,
                    expanded: dir.expanded,
                },
                GitListEntry::TreeStatus(status) => RollbackRow::File(status.entry),
                GitListEntry::Status(status) => RollbackRow::File(status),
                GitListEntry::Header(_) => continue,
            };
            rows.push(RollbackRowEntry {
                row,
                depth,
                visible,
            });
        }

        self.visible_indices = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.visible)
            .map(|(ix, _)| ix)
            .collect();
        self.rows = rows;
    }

    /// Every file a row stands for: itself for a file row, the whole subtree
    /// for a directory, everything for the root and repository rows.
    fn descendants(&self, row_ix: usize) -> Vec<RepoPath> {
        let Some(row) = self.rows.get(row_ix) else {
            return Vec::new();
        };
        match &row.row {
            RollbackRow::Root | RollbackRow::Repository => self
                .entries
                .iter()
                .map(|entry| entry.repo_path.clone())
                .collect(),
            RollbackRow::Directory { key, .. } => self
                .tree_state
                .directory_descendants
                .get(key)
                .map(|descendants| {
                    descendants
                        .iter()
                        .map(|entry| entry.repo_path.clone())
                        .collect()
                })
                .unwrap_or_default(),
            RollbackRow::File(entry) => vec![entry.repo_path.clone()],
        }
    }

    /// A parent is checked only when every file under it is, and goes
    /// indeterminate as soon as the subtree is split.
    pub(crate) fn check_state(&self, row_ix: usize) -> ToggleState {
        let paths = self.descendants(row_ix);
        if paths.is_empty() {
            return ToggleState::Unselected;
        }
        let checked = paths
            .iter()
            .filter(|path| self.checked.contains(*path))
            .count();
        if checked == 0 {
            ToggleState::Unselected
        } else if checked == paths.len() {
            ToggleState::Selected
        } else {
            ToggleState::Indeterminate
        }
    }

    /// Toggling any row applies to its whole subtree: a fully-checked row
    /// clears it, anything else (including a partially-checked parent) checks
    /// all of it.
    pub(crate) fn toggle(&mut self, row_ix: usize) {
        let check = self.check_state(row_ix) != ToggleState::Selected;
        for path in self.descendants(row_ix) {
            if check {
                self.checked.insert(path);
            } else {
                self.checked.remove(&path);
            }
        }
    }

    pub(crate) fn toggle_expanded(&mut self, row_ix: usize) {
        let Some(RollbackRow::Directory { key, .. }) = self.rows.get(row_ix).map(|row| &row.row)
        else {
            return;
        };
        let key = key.clone();
        let expanded = self.tree_state.expanded_dirs.entry(key).or_insert(true);
        *expanded = !*expanded;
        self.rebuild();
    }

    pub(crate) fn set_all_expanded(&mut self, expanded: bool) {
        for value in self.tree_state.expanded_dirs.values_mut() {
            *value = expanded;
        }
        self.rebuild();
    }

    /// The files the Rollback button will actually act on.
    pub(crate) fn checked_entries(&self) -> Vec<GitStatusEntry> {
        self.entries
            .iter()
            .filter(|entry| self.checked.contains(&entry.repo_path))
            .cloned()
            .collect()
    }

    /// Whether "Delete local copies of added files" has anything to control.
    pub(crate) fn checked_contains_added(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.status.is_created() && self.checked.contains(&entry.repo_path))
    }

    /// IDEA's summary line, over the *checked* set — that is what Rollback acts
    /// on, so counting the unchecked files here would be a lie.
    pub(crate) fn summary(&self) -> String {
        let checked = self.checked_entries();
        if checked.is_empty() {
            return "Nothing selected".to_string();
        }
        let added = checked
            .iter()
            .filter(|entry| entry.status.is_created())
            .count();
        let deleted = checked
            .iter()
            .filter(|entry| !entry.status.is_created() && entry.status.is_deleted())
            .count();
        let modified = checked.len() - added - deleted;

        let mut parts = Vec::new();
        if modified > 0 {
            parts.push(format!("{modified} modified"));
        }
        if added > 0 {
            parts.push(format!("{added} added"));
        }
        if deleted > 0 {
            parts.push(format!("{deleted} deleted"));
        }
        parts.join(", ")
    }
}

pub(crate) struct RollbackModal {
    panel: WeakEntity<GitPanel>,
    repository_name: SharedString,
    tree: RollbackTree,
    delete_local_copies: bool,
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for RollbackModal {}
impl ModalView for RollbackModal {}
impl Focusable for RollbackModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl RollbackModal {
    pub(crate) fn new(
        panel: WeakEntity<GitPanel>,
        repository_name: SharedString,
        entries: Vec<GitStatusEntry>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            panel,
            repository_name,
            tree: RollbackTree::new(entries),
            delete_local_copies: false,
            focus_handle: cx.focus_handle(),
        }
    }

    #[cfg(test)]
    pub(crate) fn tree(&self) -> &RollbackTree {
        &self.tree
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.rollback(window, cx);
    }

    /// Hand the checked set to the panel and close. Nothing is touched when
    /// nothing is checked, and the "delete local copies" flag is only passed on
    /// when the checked set actually contains an added file — the checkbox is
    /// disabled in that case, but a stale `true` must not leak through.
    fn rollback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entries = self.tree.checked_entries();
        if entries.is_empty() {
            return;
        }
        let delete_local_copies = self.delete_local_copies && self.tree.checked_contains_added();
        self.panel
            .update(cx, |panel, cx| {
                panel.perform_rollback(entries, delete_local_copies, window, cx);
            })
            .log_err();
        cx.emit(DismissEvent);
    }

    fn render_row(&self, row_ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(entry) = self.tree.rows.get(row_ix) else {
            return div().into_any_element();
        };
        let check_state = self.tree.check_state(row_ix);
        let this = cx.entity().downgrade();

        let disclosure = match &entry.row {
            RollbackRow::Directory { expanded, .. } => {
                let this = this.clone();
                Disclosure::new(("rollback-disclosure", row_ix), *expanded)
                    .on_click(move |_, _, cx| {
                        this.update(cx, |modal, cx| {
                            modal.tree.toggle_expanded(row_ix);
                            cx.notify();
                        })
                        .log_err();
                    })
                    .into_any_element()
            }
            _ => div().size(IconSize::Small.rems()).into_any_element(),
        };

        let content = match &entry.row {
            RollbackRow::Root => h_flex()
                .gap_1p5()
                .child(Label::new("Changes").size(LabelSize::Small))
                .child(
                    Label::new(file_count_label(self.tree.entries.len()))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            RollbackRow::Repository => h_flex()
                .gap_1p5()
                .child(
                    Icon::new(IconName::GitBranch)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(Label::new(self.repository_name.clone()).size(LabelSize::Small))
                .into_any_element(),
            RollbackRow::Directory { name, .. } => h_flex()
                .gap_1p5()
                .child(
                    Icon::new(IconName::Folder)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(Label::new(name.clone()).size(LabelSize::Small))
                .into_any_element(),
            RollbackRow::File(status_entry) => h_flex()
                .gap_1p5()
                .child(git_status_icon(status_entry.status))
                .child(
                    Label::new(file_name_label(status_entry))
                        .size(LabelSize::Small)
                        .color(status_color(status_entry.status)),
                )
                .into_any_element(),
        };

        let toggle_from_checkbox = this.clone();
        let toggle_from_label = this;
        h_flex()
            .id(("rollback-row", row_ix))
            .w_full()
            .h(rems(1.5))
            .gap_1p5()
            .pl(px(4. + entry.depth as f32 * 16.))
            .child(disclosure)
            .child(
                Checkbox::new(("rollback-check", row_ix), check_state).on_click(move |_, _, cx| {
                    toggle_from_checkbox
                        .update(cx, |modal, cx| {
                            modal.tree.toggle(row_ix);
                            cx.notify();
                        })
                        .log_err();
                }),
            )
            .child(
                div()
                    .id(("rollback-label", row_ix))
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        toggle_from_label
                            .update(cx, |modal, cx| {
                                modal.tree.toggle(row_ix);
                                cx.notify();
                            })
                            .log_err();
                    })
                    .child(content),
            )
            .into_any_element()
    }
}

impl Render for RollbackModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_added = self.tree.checked_contains_added();
        let nothing_checked = self.tree.checked_entries().is_empty();
        let summary = self.tree.summary();
        let row_count = self.tree.visible_indices.len();
        let delete_local_copies = self.delete_local_copies;
        let this = cx.entity().downgrade();

        let toolbar = h_flex()
            .gap_0p5()
            .child(
                IconButton::new("rollback-expand-all", IconName::ChevronUpDown)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Expand All"))
                    .on_click(cx.listener(|modal, _, _, cx| {
                        modal.tree.set_all_expanded(true);
                        cx.notify();
                    })),
            )
            .child(
                IconButton::new("rollback-collapse-all", IconName::ChevronDownUp)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Collapse All"))
                    .on_click(cx.listener(|modal, _, _, cx| {
                        modal.tree.set_all_expanded(false);
                        cx.notify();
                    })),
            );

        v_flex()
            .key_context("RollbackModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(rems(44.))
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .child(Headline::new("Rollback Changes").size(HeadlineSize::Small))
                    .child(toolbar),
            )
            .child(
                div().h(rems(18.)).w_full().child(
                    uniform_list(
                        "rollback-tree",
                        row_count,
                        cx.processor(|modal, range: Range<usize>, _window, cx| {
                            let row_indices = range
                                .filter_map(|ix| modal.tree.visible_indices.get(ix).copied())
                                .collect::<Vec<_>>();
                            row_indices
                                .into_iter()
                                .map(|row_ix| modal.render_row(row_ix, cx))
                                .collect()
                        }),
                    )
                    .size_full(),
                ),
            )
            .child(
                Label::new(summary)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Checkbox::new(
                    "rollback-delete-local-copies",
                    if delete_local_copies && has_added {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .disabled(!has_added)
                .label("Delete local copies of added files")
                .label_size(LabelSize::Small)
                .on_click(move |state, _, cx| {
                    let selected = *state == ToggleState::Selected;
                    this.update(cx, |modal, cx| {
                        modal.delete_local_copies = selected;
                        cx.notify();
                    })
                    .log_err();
                }),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(Button::new("rollback-close", "Close").on_click(cx.listener(
                        |_, _, _, cx| {
                            cx.emit(DismissEvent);
                        },
                    )))
                    .child(
                        Button::new("rollback-confirm", "Rollback")
                            .style(ButtonStyle::Tinted(TintColor::Error))
                            .disabled(nothing_checked)
                            .on_click(cx.listener(|modal, _, window, cx| {
                                modal.rollback(window, cx);
                            })),
                    ),
            )
    }
}

fn file_name_label(entry: &GitStatusEntry) -> SharedString {
    entry
        .repo_path
        .file_name()
        .unwrap_or(entry.repo_path.as_unix_str())
        .to_string()
        .into()
}

fn status_color(status: FileStatus) -> Color {
    if status.is_conflicted() {
        Color::Conflict
    } else if status.is_created() {
        Color::Created
    } else if status.is_deleted() {
        Color::Deleted
    } else {
        Color::Modified
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::status::{StageStatus, StatusCode, TrackedStatus, UnmergedStatus};

    fn tracked_modified() -> FileStatus {
        FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Unmodified,
            worktree_status: StatusCode::Modified,
        })
    }

    fn entry(path: &str, status: FileStatus) -> GitStatusEntry {
        GitStatusEntry {
            repo_path: RepoPath::new(path).expect("valid repo path"),
            status,
            staging: StageStatus::Unstaged,
            diff_stat: None,
        }
    }

    fn modified(path: &str) -> GitStatusEntry {
        entry(path, tracked_modified())
    }

    fn added(path: &str) -> GitStatusEntry {
        entry(path, FileStatus::Untracked)
    }

    fn deleted(path: &str) -> GitStatusEntry {
        entry(
            path,
            FileStatus::Tracked(TrackedStatus {
                index_status: StatusCode::Unmodified,
                worktree_status: StatusCode::Deleted,
            }),
        )
    }

    /// Row index of the first file row whose name matches, in the flattened
    /// (visible-or-not) row list.
    fn file_row(tree: &RollbackTree, path: &str) -> usize {
        tree.rows
            .iter()
            .position(|row| match &row.row {
                RollbackRow::File(entry) => entry.repo_path.as_unix_str() == path,
                _ => false,
            })
            .unwrap_or_else(|| panic!("no file row for {path}"))
    }

    fn directory_row(tree: &RollbackTree, name: &str) -> usize {
        tree.rows
            .iter()
            .position(|row| match &row.row {
                RollbackRow::Directory { name: row_name, .. } => row_name.as_ref() == name,
                _ => false,
            })
            .unwrap_or_else(|| panic!("no directory row for {name}"))
    }

    #[test]
    fn everything_starts_checked() {
        let tree = RollbackTree::new(vec![modified("src/main.rs"), modified("README.md")]);
        assert_eq!(tree.check_state(0), ToggleState::Selected);
        assert_eq!(tree.checked_entries().len(), 2);
    }

    #[test]
    fn root_row_toggles_the_whole_subtree() {
        let mut tree = RollbackTree::new(vec![modified("src/main.rs"), modified("README.md")]);
        tree.toggle(0);
        assert_eq!(tree.check_state(0), ToggleState::Unselected);
        assert!(tree.checked_entries().is_empty());
        tree.toggle(0);
        assert_eq!(tree.check_state(0), ToggleState::Selected);
        assert_eq!(tree.checked_entries().len(), 2);
    }

    #[test]
    fn directory_row_toggles_only_its_own_subtree() {
        let mut tree = RollbackTree::new(vec![
            modified("src/main.rs"),
            modified("src/lib.rs"),
            modified("README.md"),
        ]);
        let src = directory_row(&tree, "src");
        tree.toggle(src);
        assert_eq!(tree.check_state(src), ToggleState::Unselected);
        assert_eq!(
            tree.checked_entries()
                .iter()
                .map(|entry| entry.repo_path.as_unix_str().to_string())
                .collect::<Vec<_>>(),
            vec!["README.md".to_string()]
        );
    }

    #[test]
    fn unchecking_one_child_makes_every_ancestor_indeterminate() {
        let mut tree = RollbackTree::new(vec![
            modified("src/main.rs"),
            modified("src/lib.rs"),
            modified("README.md"),
        ]);
        tree.toggle(file_row(&tree, "src/main.rs"));

        let src = directory_row(&tree, "src");
        assert_eq!(tree.check_state(src), ToggleState::Indeterminate);
        // Repository row and root row both sit above it.
        assert_eq!(tree.check_state(1), ToggleState::Indeterminate);
        assert_eq!(tree.check_state(0), ToggleState::Indeterminate);
    }

    #[test]
    fn only_checked_files_are_handed_to_the_rollback() {
        let mut tree = RollbackTree::new(vec![
            modified("src/main.rs"),
            modified("src/lib.rs"),
            modified("README.md"),
        ]);
        tree.toggle(file_row(&tree, "src/lib.rs"));

        let checked = tree
            .checked_entries()
            .iter()
            .map(|entry| entry.repo_path.as_unix_str().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            checked,
            vec!["README.md".to_string(), "src/main.rs".to_string()]
        );
    }

    #[test]
    fn added_files_control_the_delete_local_copies_checkbox() {
        let mut tree = RollbackTree::new(vec![modified("src/main.rs"), modified("README.md")]);
        assert!(
            !tree.checked_contains_added(),
            "a set with no added files must leave the checkbox disabled"
        );

        tree = RollbackTree::new(vec![modified("src/main.rs"), added("src/new.rs")]);
        assert!(tree.checked_contains_added());

        // Unchecking the only added file disables it again.
        tree.toggle(file_row(&tree, "src/new.rs"));
        assert!(!tree.checked_contains_added());
    }

    #[test]
    fn summary_counts_the_checked_set_only() {
        let mut tree = RollbackTree::new(vec![
            modified("a.rs"),
            modified("b.rs"),
            added("c.rs"),
            deleted("d.rs"),
        ]);
        assert_eq!(tree.summary(), "2 modified, 1 added, 1 deleted");

        tree.toggle(file_row(&tree, "c.rs"));
        assert_eq!(tree.summary(), "2 modified, 1 deleted");

        // The root is indeterminate now, and toggling an indeterminate parent
        // checks its whole subtree rather than clearing it.
        tree.toggle(0);
        assert_eq!(tree.summary(), "2 modified, 1 added, 1 deleted");

        tree.toggle(0);
        assert_eq!(tree.summary(), "Nothing selected");
    }

    #[test]
    fn collapsing_hides_child_rows_but_keeps_their_check_state() {
        let mut tree = RollbackTree::new(vec![modified("src/main.rs"), modified("README.md")]);
        let visible_before = tree.visible_indices.len();

        tree.set_all_expanded(false);
        assert!(
            tree.visible_indices.len() < visible_before,
            "collapsing everything must hide the files under `src`"
        );

        let src = directory_row(&tree, "src");
        assert_eq!(tree.check_state(src), ToggleState::Selected);
        assert_eq!(tree.checked_entries().len(), 2);

        tree.set_all_expanded(true);
        assert_eq!(tree.visible_indices.len(), visible_before);
    }

    #[test]
    fn conflicted_files_are_coloured_as_conflicts() {
        let conflicted = FileStatus::Unmerged(UnmergedStatus {
            first_head: git::status::UnmergedStatusCode::Updated,
            second_head: git::status::UnmergedStatusCode::Updated,
        });
        assert_eq!(status_color(conflicted), Color::Conflict);
        assert_eq!(status_color(FileStatus::Untracked), Color::Created);
        assert_eq!(status_color(tracked_modified()), Color::Modified);
    }
}
