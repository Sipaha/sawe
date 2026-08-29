//! The building blocks of a *commit's* detail surface: the changed-files tree,
//! the commit-message split, the markdown identity line and the client-side
//! +/− totals.
//!
//! These were written for the git graph's inline detail sidebar, which the
//! Commit tab replaced; they moved down here from `git_graph.rs` because
//! `git_graph` depends on `git_ui` and never the reverse, so down was the only
//! direction they could go.
//!
//! The row renderers are the one thing that could not come across unchanged:
//! they were typed against `Context<GitGraph>` and take their host behaviour
//! as [`ChangedFileRowHandlers`] instead — plain `&mut App` closures the host
//! builds from its own weak handle.

use super::*;

use git::repository::{CommitDetails, CommitDiff, CommitFile};
use git::status::{StatusCode, TrackedStatus};
use gpui::{AnyElement, ClipboardItem, StyleRefinement, TextStyleRefinement, UnderlineStyle};
use language::line_diff;
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use std::rc::Rc;
use std::sync::OnceLock;
use time::{UtcOffset, format_description::BorrowedFormatItem};

/// Left step of a Commit tab file row, measured from its directory header.
///
/// The number is chosen so the file row's *painted* content edge — the status
/// glyph — lands on the same column as the Changes tab's, which is what the
/// eye actually tracks. Matching the two padding boxes is not enough: a
/// Changes row spends a 14px chevron slot and a 6px gap between its padding
/// edge and its first glyph, while a Commit row paints straight at its own.
/// So the step is the Changes tab's content edge, less the 4px `ButtonLike`
/// puts inside every Commit row:
///
/// `1px row border + content_row_padding(0) + 14px chevron + 6px gap - 4px`.
///
/// The `ButtonLike` 4px is `DynamicSpacing::Base04`, which is rems-based, so
/// the two trees line up exactly on stock density and font size and drift a
/// couple of pixels otherwise. `content_row_padding(0)` rather than `(1)`:
/// `tree_view` ships `false`, so every Changes file row is at depth 0.
const COMMIT_TREE_INDENT: f32 = 1.0 + ROW_LEFT_PADDING + SECTION_CONTENT_INDENT + 14.0 + 6.0 - 4.0;

/// Cap on the Commit tab's message block *when there is room for it*.
///
/// The block is not pinned at this height: it is a flex child with
/// [`COMMIT_MESSAGE_MIN_HEIGHT`] as its floor, so on a dock-height panel it
/// gives its space back to the changed-files tree instead of pushing the tree
/// off the bottom. It scrolls internally, so shrinking it costs only scrolling.
const COMMIT_MESSAGE_MAX_HEIGHT: f32 = 200.0;

/// Floor of the Commit tab's message block: enough for the first line of the
/// subject plus the block's own vertical padding, so that however short the
/// panel gets the user can still see *which* commit the tab is describing.
const COMMIT_MESSAGE_MIN_HEIGHT: f32 = 44.0;

/// Floor of the changed-files tree. The tree is the tab's payload, so it gets
/// a guaranteed share rather than absorbing every shortfall as the `flex_1`
/// child: without this the message block and the identity row together left it
/// zero pixels on a git panel at its shipped dock height.
const COMMIT_FILE_TREE_MIN_HEIGHT: f32 = 72.0;

/// What a changed-files row does when the user acts on it. The tree is hosted
/// by two views that share no type, so each host supplies its own behaviour as
/// `&mut App` closures built from its own weak handle rather than the rows
/// naming a concrete view.
#[derive(Clone)]
pub struct ChangedFileRowHandlers {
    /// Left click on a file row — marks it as the tree's selected file.
    pub select_file: Rc<dyn Fn(&RepoPath, &mut Window, &mut App)>,
    /// Right click on a file row, at the click position.
    pub deploy_file_context_menu: Rc<dyn Fn(&RepoPath, Point<Pixels>, &mut Window, &mut App)>,
    /// Click on a directory header row — collapses or expands the group.
    pub toggle_directory: Rc<dyn Fn(&SharedString, &mut Window, &mut App)>,
}

#[derive(Clone)]
pub struct ChangedFileEntry {
    pub status: FileStatus,
    pub file_name: SharedString,
    pub dir_path: SharedString,
    pub repo_path: RepoPath,
}

impl ChangedFileEntry {
    pub fn from_commit_file(file: &CommitFile) -> Self {
        let file_name: SharedString = file
            .path
            .file_name()
            .map(|n| n.to_string())
            .unwrap_or_default()
            .into();
        let dir_path: SharedString = file
            .path
            .parent()
            .map(|p| p.as_unix_str().to_string())
            .unwrap_or_default()
            .into();

        let status_code = match (&file.old_text, &file.new_text) {
            (None, Some(_)) => StatusCode::Added,
            (Some(_), None) => StatusCode::Deleted,
            _ => StatusCode::Modified,
        };

        let status = FileStatus::Tracked(TrackedStatus {
            index_status: status_code,
            worktree_status: StatusCode::Unmodified,
        });

        Self {
            status,
            file_name,
            dir_path,
            repo_path: file.path.clone(),
        }
    }

    fn open_file_diff(
        &self,
        commit_sha: &SharedString,
        repository: &WeakEntity<Repository>,
        workspace: &WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        CommitView::open_file_diff(
            commit_sha.to_string(),
            repository.clone(),
            workspace.clone(),
            self.repo_path.clone(),
            window,
            cx,
        );
    }

    /// Full repo-relative path, used for tooltips and the copy-path menu.
    fn display_path(&self) -> SharedString {
        if self.dir_path.is_empty() {
            self.file_name.clone()
        } else {
            format!("{}/{}", self.dir_path, self.file_name).into()
        }
    }

    /// A file row of the changed-files tree. The directory is carried by the
    /// header row above, so the row itself only shows the file name — the
    /// panel is narrow and IDEA's tree does the same.
    /// `indent` is the row's left padding: one step in from the directory
    /// header. It is passed in rather than read here because it comes from
    /// `ProjectPanelSettings`, and `project_panel` depends on `git_ui` — the
    /// host reads the setting and hands the result down.
    pub fn render(
        &self,
        ix: usize,
        indent: Pixels,
        commit_sha: SharedString,
        repository: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        handlers: ChangedFileRowHandlers,
        is_selected: bool,
    ) -> AnyElement {
        let file_name = self.file_name.clone();
        let full_path = self.display_path();

        let handlers_for_click = handlers.clone();

        div()
            .w_full()
            .pl(indent)
            .on_mouse_down(MouseButton::Right, {
                let repo_path = self.repo_path.clone();
                move |event: &MouseDownEvent, window, cx| {
                    (handlers.deploy_file_context_menu)(&repo_path, event.position, window, cx);
                    cx.stop_propagation();
                }
            })
            .child(
                ButtonLike::new(("changed-file", ix))
                    .toggle_state(is_selected)
                    .child(
                        h_flex()
                            .min_w_0()
                            .w_full()
                            .gap_1()
                            .overflow_hidden()
                            .child(git_status_icon(self.status))
                            .child(Label::new(file_name).size(LabelSize::Small).truncate()),
                    )
                    .tooltip({
                        let meta = full_path;
                        move |_, cx| Tooltip::with_meta("Open Diff", None, meta.clone(), cx)
                    })
                    // Single click only selects; the diff opens on double
                    // click, so walking the file list with the mouse does not
                    // spray tabs across the pane.
                    .on_click({
                        let entry = self.clone();
                        let handlers = handlers_for_click;
                        move |event: &ClickEvent, window, cx| {
                            (handlers.select_file)(&entry.repo_path, window, cx);
                            if event.click_count() >= 2 {
                                entry.open_file_diff(
                                    &commit_sha,
                                    &repository,
                                    &workspace,
                                    window,
                                    cx,
                                );
                            }
                        }
                    }),
            )
            .into_any_element()
    }
}

/// One row of the Commit tab's changed-files tree. IDEA renders the commit's
/// files grouped under their directory rather than as a flat list of
/// `name  dir/path` pairs; a directory chain with a single child is compacted
/// into one header (`docs/plans/completed`) instead of nesting a row per path
/// component.
#[derive(Clone)]
pub enum ChangedFileRow {
    Directory {
        /// Raw directory path — the key into `collapsed_changed_dirs`. Empty
        /// for files sitting at the repository root.
        key: SharedString,
        /// What the row shows: the directory path, or the repository name for
        /// the root group.
        label: SharedString,
        file_count: usize,
        collapsed: bool,
    },
    File(ChangedFileEntry),
}

/// Flatten a commit's changed files into directory-grouped rows. Files under a
/// collapsed directory are dropped, but its header keeps the full count so the
/// user can see how much is hidden.
pub fn build_changed_file_rows(
    entries: &[ChangedFileEntry],
    root_label: &SharedString,
    collapsed: &HashSet<SharedString>,
) -> Vec<ChangedFileRow> {
    let mut by_directory: BTreeMap<SharedString, Vec<&ChangedFileEntry>> = BTreeMap::new();
    for entry in entries {
        by_directory
            .entry(entry.dir_path.clone())
            .or_default()
            .push(entry);
    }

    let mut rows = Vec::new();
    for (directory, mut files) in by_directory {
        files.sort_by(|left, right| left.file_name.cmp(&right.file_name));
        let is_collapsed = collapsed.contains(&directory);
        rows.push(ChangedFileRow::Directory {
            key: directory.clone(),
            label: if directory.is_empty() {
                root_label.clone()
            } else {
                directory
            },
            file_count: files.len(),
            collapsed: is_collapsed,
        });
        if !is_collapsed {
            rows.extend(files.into_iter().cloned().map(ChangedFileRow::File));
        }
    }
    rows
}

/// Header row of one directory group in the changed-files tree. Clicking it
/// collapses / expands the group.
pub fn render_changed_directory_row(
    ix: usize,
    key: SharedString,
    label: SharedString,
    file_count: usize,
    collapsed: bool,
    handlers: ChangedFileRowHandlers,
) -> AnyElement {
    let tooltip_label = label.clone();
    ButtonLike::new(("changed-dir", ix))
        .child(
            h_flex()
                .min_w_0()
                .w_full()
                .gap_1()
                .overflow_hidden()
                // A plain chevron rather than a `Disclosure`: that renders as
                // an `IconButton`, and a nested button inside the row's own
                // `ButtonLike` both muddies the click target and makes the row
                // taller than a file row — `uniform_list` sizes every row from
                // the first one, so the two must match exactly.
                .child(
                    Icon::new(if collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .size(IconSize::Small)
                    .color(Color::Muted),
                )
                // Default (16px), matching the project panel's folder glyph and
                // the 16px status glyph on the file rows below it.
                .child(Icon::new(IconName::Folder).color(Color::Muted))
                .child(Label::new(label).size(LabelSize::Small).truncate_start())
                .child(
                    Label::new(format!(
                        "{file_count} {}",
                        if file_count == 1 { "file" } else { "files" }
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
        )
        .tooltip(move |_, cx| Tooltip::simple(tooltip_label.clone(), cx))
        .on_click(move |_, window, cx| {
            (handlers.toggle_directory)(&key, window, cx);
        })
        .into_any_element()
}

/// Split a raw commit message into its subject (first line) and body. The
/// blank line git puts between them is dropped, as is trailing whitespace —
/// otherwise the tab renders a run of empty lines under short messages.
pub fn split_commit_message(message: &str) -> (SharedString, SharedString) {
    let mut lines = message.lines();
    let subject = lines.next().unwrap_or_default().trim_end().to_string();
    let body = lines
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches(|c: char| c == '\n' || c == '\r')
        .trim_end()
        .to_string();
    (subject.into(), body.into())
}

/// Spec §5's `short hash · author · date`, pre-split so that a panel too
/// narrow for the whole line truncates the author rather than dropping the
/// date off the end.
///
/// The sidebar this tab replaced rendered
/// `<sha> <author> <email> on <date> at <time>` as one markdown run. At a
/// dock's width that wraps to three lines, and those lines came straight out
/// of the changed-files tree's vertical budget — measured at 63px against a
/// 282px tab body. The email and the time of day therefore live in
/// [`CommitIdentity::tooltip`] instead of on the line.
struct CommitIdentity {
    short_sha: SharedString,
    author: Option<SharedString>,
    date: Option<SharedString>,
    /// The full identity, including everything the line itself drops.
    tooltip: SharedString,
}

fn commit_identity(
    short_sha: &str,
    author_name: &str,
    author_email: &str,
    timestamp: Option<i64>,
) -> CommitIdentity {
    let mut tooltip = short_sha.to_string();
    if !author_name.is_empty() {
        tooltip.push_str(" · ");
        tooltip.push_str(author_name);
    }
    if !author_email.is_empty() {
        tooltip.push_str(if author_name.is_empty() { " · " } else { " " });
        tooltip.push('<');
        tooltip.push_str(author_email);
        tooltip.push('>');
    }
    if let Some(timestamp) = timestamp {
        tooltip.push_str(" · ");
        tooltip.push_str(&format_detail_timestamp(timestamp));
    }

    CommitIdentity {
        short_sha: short_sha.to_string().into(),
        author: (!author_name.is_empty()).then(|| author_name.to_string().into()),
        date: timestamp.map(|timestamp| format_identity_date(timestamp).into()),
        tooltip: tooltip.into(),
    }
}

/// The line's three pieces share one style; a helper keeps them provably
/// identical rather than repeating the builder chain three times.
fn identity_label(text: SharedString) -> Label {
    Label::new(text).size(LabelSize::Small).color(Color::Muted)
}

fn identity_separator() -> Label {
    identity_label("·".into())
}

/// The line's date carries no time of day: it is the half of the timestamp a
/// user scanning a commit list is actually reading, and the panel is narrow.
fn identity_date_format() -> &'static [BorrowedFormatItem<'static>] {
    static FORMAT: OnceLock<Vec<BorrowedFormatItem<'static>>> = OnceLock::new();
    FORMAT.get_or_init(|| {
        time::format_description::parse("[day] [month repr:short] [year]").unwrap_or_default()
    })
}

/// `on <date> at <time>` reads better with the two halves separated, so the
/// tooltip spells the time out instead of reusing the log column's compact
/// `[day] [month] [year] [hour]:[minute]`.
fn detail_timestamp_format() -> &'static [BorrowedFormatItem<'static>] {
    static FORMAT: OnceLock<Vec<BorrowedFormatItem<'static>>> = OnceLock::new();
    FORMAT.get_or_init(|| {
        time::format_description::parse("[day] [month repr:short] [year] at [hour]:[minute]")
            .unwrap_or_default()
    })
}

fn format_with(timestamp: i64, format: &[BorrowedFormatItem<'static>]) -> String {
    let Ok(datetime) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return "Unknown".to_string();
    };

    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    datetime
        .to_offset(local_offset)
        .format(format)
        .unwrap_or_default()
}

fn format_identity_date(timestamp: i64) -> String {
    format_with(timestamp, identity_date_format())
}

fn format_detail_timestamp(timestamp: i64) -> String {
    format_with(timestamp, detail_timestamp_format())
}

/// Style for the selectable single-line text fields of the Commit tab,
/// matching what the equivalent [`Label`] rendered before.
pub fn detail_text_style(
    text_size: TextSize,
    color: Color,
    weight: Option<gpui::FontWeight>,
    window: &Window,
    cx: &App,
) -> MarkdownStyle {
    let refinement = TextStyleRefinement {
        font_size: Some(text_size.rems(cx).into()),
        color: Some(color.color(cx)),
        font_weight: weight,
        ..Default::default()
    };
    let mut base_text_style = window.text_style();
    base_text_style.refine(&refinement);

    let container_style = StyleRefinement::default();

    MarkdownStyle {
        base_text_style,
        // `base_text_style` alone is NOT enough: markdown's text runs carry
        // `HighlightStyle`, which has no font size, so the glyphs are laid out
        // at whatever size the containing div inherits — the window's UI size.
        // Setting the size on the container too is what actually shrinks the
        // text, and is what `MarkdownStyle::with_preview_overrides` does.
        container_style,
        selection_background_color: cx.theme().colors().element_selection_background,
        link: TextStyleRefinement {
            color: Some(cx.theme().colors().link_text_hover),
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(cx.theme().colors().link_text_hover),
                wavy: false,
            }),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn compute_diff_stats(diff: &CommitDiff) -> (usize, usize) {
    diff.files.iter().fold((0, 0), |(added, removed), file| {
        let old_text = file.old_text.as_deref().unwrap_or("");
        let new_text = file.new_text.as_deref().unwrap_or("");
        let hunks = line_diff(old_text, new_text);
        hunks
            .iter()
            .fold((added, removed), |(a, r), (old_range, new_range)| {
                (
                    a + (new_range.end - new_range.start) as usize,
                    r + (old_range.end - old_range.start) as usize,
                )
            })
    })
}

/// A git-graph selection handed to the git panel's Commit tab.
///
/// The selection carries its own repository rather than letting the panel
/// resolve `active_repository`: the panel follows the Solution's active member
/// and the two can disagree transiently, and a Commit tab describing a
/// different repository than the graph row the user clicked is exactly the bug
/// this avoids.
#[derive(Clone)]
pub struct CommitSelection {
    pub repository: Entity<Repository>,
    /// Non-empty. A single sha renders the commit's details; more than one
    /// renders a bare "N commits selected" summary.
    pub shas: Vec<Oid>,
}

/// What made the git graph push a selection at the Commit tab.
///
/// The graph re-anchors its selection by sha after every refetch, and a
/// refetch is triggered by repository events the user never asked for — a
/// `git fetch` landing in a terminal, a branch checked out elsewhere. Those
/// re-anchors reach [`GitPanel::show_commit_selection`] through exactly the
/// same call as a click does, so without this distinction one of them would
/// swap the panel body out from under a user who had gone back to Changes to
/// stage files and write a commit message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommitSelectionSource {
    /// A click, a keyboard selection, or another gesture the user made in the
    /// graph. Brings the Commit tab back to the front, which is what makes
    /// "select a commit, switch to Changes, click that same row again" work.
    UserGesture,
    /// A re-anchor after a refetch. Refreshes what an already-open Commit tab
    /// is describing and does nothing at all when the tab is closed; never
    /// changes which tab is active.
    Background,
}

/// Where one of the Commit tab's two background loads has got to.
///
/// `Failed` exists so that a diff which errored is not indistinguishable from
/// a commit that genuinely touched no files; `Idle` is a multi-commit
/// selection, which loads nothing at all.
pub(super) enum LoadState<T> {
    Idle,
    Loading,
    Loaded(T),
    Failed(SharedString),
}

pub(super) struct LoadedCommitDiff {
    diff: CommitDiff,
    lines_added: usize,
    lines_removed: usize,
}

/// Selectable text of the commit message. `ui::Label` has no selection
/// support, so the subject / body / identity line are `MarkdownElement`s. The
/// entities are built once when the details load rather than per frame: a
/// `Markdown` owns the user's in-progress selection, and rebuilding it every
/// frame would wipe a selection mid-drag.
struct CommitDetailText {
    subject: Entity<Markdown>,
    /// Everything after the subject line, verbatim. Parsed as plain text (only
    /// bare URLs become links) so that the `#`, `*` and backticks commit
    /// messages are full of are not swallowed as markdown syntax.
    body: Entity<Markdown>,
    /// `<short sha> · <author> · <date>` — see [`commit_identity`].
    identity: CommitIdentity,
}

/// Everything the Commit tab shows.
///
/// It hangs off [`GitPanel`] as an `Option` because the tab exists only while
/// something is in it: `Some` is exactly what puts the tab in the tab bar, so
/// "is the tab open" and "is there anything to show" cannot drift apart the
/// way a parallel `bool` would let them.
pub(super) struct CommitTabState {
    pub(super) selection: CommitSelection,
    pub(super) details: LoadState<CommitDetails>,
    pub(super) diff: LoadState<LoadedCommitDiff>,
    text: Option<CommitDetailText>,
    pub(super) collapsed_dirs: HashSet<SharedString>,
    scroll_handle: UniformListScrollHandle,
    selected_file: Option<RepoPath>,
    _details_task: Option<Task<()>>,
    _diff_task: Option<Task<()>>,
}

impl CommitTabState {
    fn new(selection: CommitSelection) -> Self {
        Self {
            selection,
            details: LoadState::Idle,
            diff: LoadState::Idle,
            text: None,
            collapsed_dirs: HashSet::default(),
            scroll_handle: UniformListScrollHandle::new(),
            selected_file: None,
            _details_task: None,
            _diff_task: None,
        }
    }
}

/// The hosting provider of the repository the Commit tab is showing — used for
/// the author avatar. Deliberately resolved from the pushed repository rather
/// than the panel's own `active_repository`, which can transiently disagree;
/// see [`CommitSelection`].
fn commit_remote(repository: &Entity<Repository>, cx: &mut App) -> Option<GitRemote> {
    let remote_url = repository.read(cx).default_remote_url()?;
    let provider_registry = GitHostingProviderRegistry::default_global(cx);
    let (provider, parsed) = parse_git_remote_url(provider_registry, &remote_url)?;
    Some(GitRemote {
        host: provider,
        owner: parsed.owner.into(),
        repo: parsed.repo.into(),
    })
}

impl GitPanel {
    /// Whether the Commit tab is present in the tab bar.
    pub fn commit_tab_is_open(&self) -> bool {
        self.commit_tab.is_some()
    }

    /// The commits the Commit tab is describing, in the order the git graph
    /// pushed them. Empty while the tab is closed.
    pub fn commit_tab_shas(&self) -> &[Oid] {
        self.commit_tab
            .as_ref()
            .map_or(&[], |state| state.selection.shas.as_slice())
    }

    /// Show a git-graph selection in the Commit tab, opening the tab if it is
    /// closed. One sha renders that commit's details; more render a bare count.
    ///
    /// Activating the tab here deliberately does **not** move focus: the caller
    /// is the graph, mid-click or mid-arrow-key, and pulling focus into the
    /// panel would break the graph's own keyboard navigation. The user-driven
    /// routes into the tab (its tab-bar row, `git_panel::ActivateCommitTab`) go
    /// through `set_active_tab`, which does focus.
    ///
    /// A [`CommitSelectionSource::Background`] push refreshes an open tab in
    /// place and stops there — see that variant for why.
    pub fn show_commit_selection(
        &mut self,
        selection: CommitSelection,
        source: CommitSelectionSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(&sha) = selection.shas.first() else {
            // An empty selection is a deselection. Callers are expected to say
            // that with `close_commit_tab`, but a tab left describing nothing
            // is worse than closing one they meant to keep.
            self.close_commit_tab(window, cx);
            return;
        };

        if source == CommitSelectionSource::Background && !self.commit_tab_is_open() {
            return;
        }

        let already_showing = self.commit_tab.as_ref().is_some_and(|state| {
            state.selection.repository.entity_id() == selection.repository.entity_id()
                && state.selection.shas == selection.shas
        });
        if already_showing {
            // Re-selecting the same row must not restart a load that worked, or
            // throw away the tree's scroll position and collapsed directories.
            // A load that FAILED is the exception: this selection is the only
            // gesture that reaches back here, so refusing it too would make the
            // error permanent until the user picks some other commit.
            self.retry_failed_commit_loads(sha, cx);
            if source == CommitSelectionSource::UserGesture {
                self.activate_commit_tab_without_focus(cx);
            }
            cx.notify();
            return;
        }

        let repository = selection.repository.clone();
        let is_single_commit = selection.shas.len() == 1;
        self.commit_tab = Some(CommitTabState::new(selection));

        if is_single_commit {
            self.load_commit_tab_details(sha, &repository, cx);
            self.load_commit_tab_diff(sha, &repository, cx);
        }

        if source == CommitSelectionSource::UserGesture {
            self.activate_commit_tab_without_focus(cx);
        }
        cx.notify();
    }

    /// Activate the Commit tab without taking focus — see
    /// [`Self::show_commit_selection`] for why the graph's push must not steal
    /// it.
    ///
    /// It notifies for itself rather than trusting callers to: the panel's
    /// action registrations now depend on the active tab, so a missed notify
    /// leaves the *old* tab's actions palette-reachable, not merely a stale
    /// frame. Callers that notify anyway cost nothing — GPUI coalesces.
    fn activate_commit_tab_without_focus(&mut self, cx: &mut Context<Self>) {
        self.active_tab = GitPanelTab::Commit;
        cx.notify();
    }

    /// Restart whichever of the Commit tab's two loads failed. A multi-commit
    /// selection loads nothing, so neither state can be `Failed` and `sha` —
    /// the first of several — is never used as a commit to reload.
    fn retry_failed_commit_loads(&mut self, sha: Oid, cx: &mut Context<Self>) {
        let Some(state) = self.commit_tab.as_ref() else {
            return;
        };
        let repository = state.selection.repository.clone();
        let retry_details = matches!(state.details, LoadState::Failed(_));
        let retry_diff = matches!(state.diff, LoadState::Failed(_));
        if retry_details {
            self.load_commit_tab_details(sha, &repository, cx);
        }
        if retry_diff {
            self.load_commit_tab_diff(sha, &repository, cx);
        }
    }

    /// Load the commit's message and identity into the open Commit tab.
    /// Assigning the task also cancels whatever load it replaces.
    fn load_commit_tab_details(
        &mut self,
        sha: Oid,
        repository: &Entity<Repository>,
        cx: &mut Context<Self>,
    ) {
        let details = repository.update(cx, |repository, _| repository.show(sha.to_string()));
        let task = cx.spawn(async move |this, cx| {
            let loaded = details.await;
            this.update(cx, |this, cx| {
                // Drop a load that resolved after the selection moved on,
                // rather than pairing it with whatever is shown now.
                if this.commit_tab_sha() != Some(sha) {
                    return;
                }
                let loaded = match loaded {
                    Ok(Ok(details)) => {
                        let (subject, body) = split_commit_message(&details.message);
                        let text = CommitDetailText {
                            subject: cx.new(|cx| Markdown::new_text(subject, cx)),
                            body: cx.new(|cx| Markdown::new_text(body, cx)),
                            identity: commit_identity(
                                &details.short_sha(),
                                &details.author_name,
                                &details.author_email,
                                Some(details.commit_timestamp),
                            ),
                        };
                        Ok((details, text))
                    }
                    Ok(Err(error)) => Err(SharedString::from(format!(
                        "Couldn't load commit {}: {error:#}",
                        sha.display_short()
                    ))),
                    Err(_) => Err(SharedString::from(format!(
                        "Loading commit {} was cancelled.",
                        sha.display_short()
                    ))),
                };
                if let Some(state) = this.commit_tab.as_mut() {
                    match loaded {
                        Ok((details, text)) => {
                            state.text = Some(text);
                            state.details = LoadState::Loaded(details);
                        }
                        Err(message) => state.details = LoadState::Failed(message),
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(state) = self.commit_tab.as_mut() {
            state.details = LoadState::Loading;
            state._details_task = Some(task);
        }
    }

    /// Load the commit's changed files into the open Commit tab.
    fn load_commit_tab_diff(
        &mut self,
        sha: Oid,
        repository: &Entity<Repository>,
        cx: &mut Context<Self>,
    ) {
        let diff = repository.update(cx, |repository, _| {
            repository.load_commit_diff(sha.to_string())
        });
        let task = cx.spawn(async move |this, cx| {
            let loaded = diff.await;
            this.update(cx, |this, cx| {
                if this.commit_tab_sha() != Some(sha) {
                    return;
                }
                let loaded = match loaded {
                    Ok(Ok(diff)) => {
                        let (lines_added, lines_removed) = compute_diff_stats(&diff);
                        LoadState::Loaded(LoadedCommitDiff {
                            diff,
                            lines_added,
                            lines_removed,
                        })
                    }
                    Ok(Err(error)) => LoadState::Failed(SharedString::from(format!(
                        "Couldn't load the changes of commit {}: {error:#}",
                        sha.display_short()
                    ))),
                    Err(_) => LoadState::Failed(SharedString::from(format!(
                        "Loading the changes of commit {} was cancelled.",
                        sha.display_short()
                    ))),
                };
                if let Some(state) = this.commit_tab.as_mut() {
                    state.diff = loaded;
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(state) = self.commit_tab.as_mut() {
            state.diff = LoadState::Loading;
            state._diff_task = Some(task);
        }
    }

    /// Close the Commit tab and drop everything it was showing, emitting
    /// [`Event::CommitTabClosed`] so the git graph can clear the row selection
    /// that opened it.
    ///
    /// The event carries the shas the tab was describing: the event reaches
    /// every git graph in the window, and one pinned to another repository (or
    /// a second graph with its own selection) must not lose its rows because
    /// somebody else's tab closed.
    ///
    /// Focus is left alone on the way out for the same reason
    /// [`Self::show_commit_selection`] does not take it: the close can arrive
    /// from the graph.
    pub fn close_commit_tab(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(closed) = self.commit_tab.take() else {
            return;
        };
        // Only the Commit tab itself is yanked back to Changes; a user parked
        // on another tab keeps the tab they chose. With Changes the only other
        // tab today the check is tautological — it is kept so that adding a
        // third tab does not silently start stealing it.
        if self.active_tab == GitPanelTab::Commit {
            self.active_tab = GitPanelTab::Changes;
        }
        cx.emit(Event::CommitTabClosed(closed.selection.shas));
        cx.notify();
    }

    /// Whether the Commit tab is the one being rendered, as opposed to merely
    /// open in the tab bar. The git graph's tests assert against it: a
    /// background re-anchor has to leave the active tab alone, and only the
    /// panel knows which tab that is.
    #[cfg(any(test, feature = "test-support"))]
    pub fn commit_tab_is_active(&self) -> bool {
        self.active_tab == GitPanelTab::Commit
    }

    /// Leave the Commit tab for Changes the way clicking the Changes tab
    /// would, for tests that live outside this crate.
    #[cfg(any(test, feature = "test-support"))]
    pub fn activate_changes_tab_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_active_tab(GitPanelTab::Changes, window, cx);
    }

    /// The sha the Commit tab's *loaded* details were fetched for, as opposed
    /// to the one [`Self::commit_tab_shas`] says it is describing. The git
    /// graph's tests assert the two agree across a log refetch: when the
    /// header and the file list drifted apart, clicking a file asked
    /// `CommitView` for a path the displayed commit never touched, which
    /// silently opened an empty tab.
    #[cfg(any(test, feature = "test-support"))]
    pub fn commit_tab_loaded_details_sha(&self) -> Option<SharedString> {
        match &self.commit_tab.as_ref()?.details {
            LoadState::Loaded(details) => Some(details.sha.clone()),
            _ => None,
        }
    }

    /// Whether the Commit tab's changed-files load finished. False while it is
    /// loading, when it failed, and when the tab is closed.
    #[cfg(any(test, feature = "test-support"))]
    pub fn commit_tab_diff_is_loaded(&self) -> bool {
        self.commit_tab
            .as_ref()
            .is_some_and(|state| matches!(state.diff, LoadState::Loaded(_)))
    }

    /// The sha whose details the Commit tab is showing, if it is showing a
    /// single commit rather than a multi-row selection summary.
    fn commit_tab_sha(&self) -> Option<Oid> {
        match self.commit_tab.as_ref()?.selection.shas.as_slice() {
            [sha] => Some(*sha),
            _ => None,
        }
    }

    pub(super) fn activate_commit_tab(
        &mut self,
        _: &ActivateCommitTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.commit_tab_is_open() {
            return;
        }
        self.set_active_tab(GitPanelTab::Commit, window, cx);
    }

    fn toggle_commit_directory(&mut self, key: &SharedString, cx: &mut Context<Self>) {
        let Some(state) = self.commit_tab.as_mut() else {
            return;
        };
        if !state.collapsed_dirs.remove(key) {
            state.collapsed_dirs.insert(key.clone());
        }
        cx.notify();
    }

    fn deploy_commit_file_context_menu(
        &mut self,
        path: RepoPath,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = path.as_unix_str().to_string();
        let focus_handle = self.focus_handle.clone();
        let context_menu = ContextMenu::build(window, cx, move |context_menu, _window, _cx| {
            context_menu
                .context(focus_handle)
                .entry("Copy Path", None, move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                })
        });
        self.set_context_menu(context_menu, position, window, cx);
    }

    /// How the Commit tab's file rows reach back into the panel.
    ///
    /// This bundle erases the host entity behind `Rc<dyn Fn(…, &mut App)>`, so
    /// invoking one of these closures while the panel is already leased is a
    /// runtime double-lease panic that the compiler cannot see and a
    /// `VisualTestContext` draw will not reproduce. `GitPanel::render` holds
    /// such a lease for its whole duration — unlike the git graph, which
    /// renders the same tree outside its own update — so the bundle must be
    /// built here from `cx.weak_entity()` and its closures may only ever be
    /// installed into event callbacks, never called during layout or paint.
    fn commit_file_row_handlers(&self, cx: &Context<Self>) -> ChangedFileRowHandlers {
        let panel = cx.weak_entity();
        ChangedFileRowHandlers {
            select_file: Rc::new({
                let panel = panel.clone();
                move |repo_path, _window, cx| {
                    panel
                        .update(cx, |panel, cx| {
                            if let Some(state) = panel.commit_tab.as_mut() {
                                state.selected_file = Some(repo_path.clone());
                                cx.notify();
                            }
                        })
                        .ok();
                }
            }),
            deploy_file_context_menu: Rc::new({
                let panel = panel.clone();
                move |repo_path, position, window, cx| {
                    panel
                        .update(cx, |panel, cx| {
                            panel.deploy_commit_file_context_menu(
                                repo_path.clone(),
                                position,
                                window,
                                cx,
                            );
                        })
                        .ok();
                }
            }),
            toggle_directory: Rc::new(move |key, _window, cx| {
                panel
                    .update(cx, |panel, cx| {
                        panel.toggle_commit_directory(key, cx);
                    })
                    .ok();
            }),
        }
    }

    /// The Commit tab body, in spec §5's order: the commit message, the
    /// `<short sha> <author> <email> on <date>` identity line, then the
    /// changed-files tree under a header carrying the file count and the
    /// commit's +/− totals.
    pub(super) fn render_commit_tab(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(state) = self.commit_tab.as_ref() else {
            return Empty.into_any_element();
        };

        let selected_commits = state.selection.shas.len();
        if selected_commits > 1 {
            return v_flex()
                .flex_1()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    Label::new(format!("{selected_commits} commits selected")).color(Color::Muted),
                )
                .into_any_element();
        }

        let Some(&sha) = state.selection.shas.first() else {
            return Empty.into_any_element();
        };
        // The FULL sha, never `display_short()`: it is forwarded verbatim to
        // `CommitView::open_file_diff`, which opens an empty tab for a short one.
        let full_sha: SharedString = sha.to_string().into();

        let mut body = v_flex().flex_1().size_full().min_h_0().overflow_hidden();

        body = match (&state.details, &state.text) {
            (LoadState::Loaded(details), Some(text)) => {
                // The message matches the log rows beside it —
                // `TextSize::Default`. The identity line stays small: it is
                // metadata about the commit, not the commit.
                let subject_style = detail_text_style(
                    TextSize::Default,
                    Color::Default,
                    Some(gpui::FontWeight::SEMIBOLD),
                    window,
                    cx,
                );
                let body_style =
                    detail_text_style(TextSize::Default, Color::Default, None, window, cx);
                let has_body = !text.body.read(cx).source().is_empty();

                let author_email =
                    (!details.author_email.is_empty()).then(|| details.author_email.clone());
                let remote = commit_remote(&state.selection.repository, cx);
                let avatar = CommitAvatar::new(&full_sha, author_email, remote.as_ref())
                    .size(px(16.))
                    .render(window, cx);

                body.child(
                    // Shrinkable between its floor and its cap rather than
                    // pinned at the cap: on a dock-height panel the tab body
                    // has ~282px to spend, and a fixed 200px message left the
                    // changed-files tree nothing. Flex does the arithmetic, so
                    // nothing here has to read the available height — which it
                    // could only do from layout, where a `cx.notify()` would be
                    // discarded and a re-derive-and-notify would spin.
                    div()
                        .id("commit-tab-message")
                        .min_h(px(COMMIT_MESSAGE_MIN_HEIGHT))
                        .max_h(px(COMMIT_MESSAGE_MAX_HEIGHT))
                        .overflow_y_scroll()
                        .child(
                            v_flex()
                                .min_w_0()
                                .px_2()
                                .py_1p5()
                                .gap_1p5()
                                .child(div().min_w_0().child(MarkdownElement::new(
                                    text.subject.clone(),
                                    subject_style,
                                )))
                                .children(has_body.then(|| {
                                    div()
                                        .min_w_0()
                                        .child(MarkdownElement::new(text.body.clone(), body_style))
                                })),
                        ),
                )
                // One line, always: only the author is allowed to shrink, and
                // it truncates rather than wrapping. A wrapped identity row is
                // vertical budget taken from the changed-files tree below it.
                .child(
                    h_flex()
                        .id("commit-tab-identity")
                        .flex_shrink_0()
                        .w_full()
                        .px_2()
                        .pb_1p5()
                        .gap_1()
                        .items_center()
                        .child(div().flex_shrink_0().child(avatar))
                        .child(identity_label(text.identity.short_sha.clone()))
                        .children(text.identity.author.clone().map(|author| {
                            h_flex()
                                .min_w_0()
                                .gap_1()
                                .child(identity_separator())
                                .child(identity_label(author).truncate())
                        }))
                        .children(text.identity.date.clone().map(|date| {
                            h_flex()
                                .flex_shrink_0()
                                .gap_1()
                                .child(identity_separator())
                                .child(identity_label(date))
                        }))
                        .tooltip({
                            let identity = text.identity.tooltip.clone();
                            move |_, cx| Tooltip::simple(identity.clone(), cx)
                        }),
                )
            }
            (LoadState::Failed(error), _) => body.child(
                div().px_2().py_1p5().child(
                    Label::new(error.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                ),
            ),
            _ => body.child(
                div().px_2().py_1p5().child(
                    Label::new("Loading commit…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            ),
        };

        let border = cx.theme().colors().border;
        body = match &state.diff {
            LoadState::Loaded(loaded) => {
                let file_count = loaded.diff.files.len();
                body.child(
                    h_flex()
                        .flex_shrink_0()
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .justify_between()
                        .border_t_1()
                        .border_color(border)
                        .child(
                            Label::new(format!(
                                "{file_count} Changed {}",
                                if file_count == 1 { "File" } else { "Files" }
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        // `ui::DiffStat`, fully qualified: `use super::*`
                        // brings `git::status::DiffStat`, the data type, into
                        // scope under the same bare name.
                        .child(ui::DiffStat::new(
                            "commit-tab-diff-stat",
                            loaded.lines_added,
                            loaded.lines_removed,
                        )),
                )
                .child(self.render_commit_file_tree(state, loaded, full_sha, window, cx))
            }
            LoadState::Failed(error) => body.child(
                div()
                    .flex_shrink_0()
                    .px_2()
                    .py_1p5()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    ),
            ),
            LoadState::Loading => body.child(
                div()
                    .flex_shrink_0()
                    .px_2()
                    .py_1p5()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        Label::new("Loading changed files…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            ),
            LoadState::Idle => body,
        };

        body.into_any_element()
    }

    /// The commit's changed files, grouped by directory.
    ///
    /// The tree carries no left inset of its own: `ButtonLike`'s own 4px
    /// horizontal padding is the directory header's indent, which puts the file
    /// rows at `4 + COMMIT_TREE_INDENT` = 38px — the same left edge as the
    /// Changes tab's depth-1 file rows, and as the graph sidebar's file rows
    /// sat at before it was deleted. The headers themselves then sit 2px inside
    /// the Changes tab's 6px section headers; closing that last 2px would mean
    /// either changing the shared row renderer or a magic negative margin, and
    /// the file rows are the edge the eye actually tracks.
    fn render_commit_file_tree(
        &self,
        state: &CommitTabState,
        loaded: &LoadedCommitDiff,
        commit_sha: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entries: Vec<ChangedFileEntry> = loaded
            .diff
            .files
            .iter()
            .map(ChangedFileEntry::from_commit_file)
            .collect();
        let repo_label: SharedString = state
            .selection
            .repository
            .read(cx)
            .work_directory_abs_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "Repository".to_string())
            .into();
        let rows: Rc<Vec<ChangedFileRow>> = Rc::new(build_changed_file_rows(
            &entries,
            &repo_label,
            &state.collapsed_dirs,
        ));
        let row_count = rows.len();
        let repository = state.selection.repository.downgrade();
        let workspace = self.workspace.clone();
        let selected_file = state.selected_file.clone();
        let scroll_handle = state.scroll_handle.clone();
        let handlers = self.commit_file_row_handlers(cx);

        div()
            .id("commit-tab-files")
            .flex_1()
            // An explicit floor, not `min_h_0()`: as the only `flex_1` child of
            // the tab body the tree would otherwise absorb the whole shortfall
            // on a short panel and render zero rows.
            .min_h(px(COMMIT_FILE_TREE_MIN_HEIGHT))
            .child(
                uniform_list(
                    "commit-tab-files-list",
                    row_count,
                    move |range, _window, _cx| {
                        range
                            .filter_map(|ix| {
                                let row = rows.get(ix)?;
                                Some(match row {
                                    ChangedFileRow::Directory {
                                        key,
                                        label,
                                        file_count,
                                        collapsed,
                                    } => render_changed_directory_row(
                                        ix,
                                        key.clone(),
                                        label.clone(),
                                        *file_count,
                                        *collapsed,
                                        handlers.clone(),
                                    ),
                                    ChangedFileRow::File(entry) => entry.render(
                                        ix,
                                        px(COMMIT_TREE_INDENT),
                                        commit_sha.clone(),
                                        repository.clone(),
                                        workspace.clone(),
                                        handlers.clone(),
                                        selected_file.as_ref() == Some(&entry.repo_path),
                                    ),
                                })
                            })
                            .collect()
                    },
                )
                .size_full()
                .track_scroll(&scroll_handle),
            )
            .vertical_scrollbar_for(&scroll_handle, window, cx)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_commit_message() {
        let (subject, body) = split_commit_message("Just a subject");
        assert_eq!(subject, "Just a subject");
        assert_eq!(body, "");

        let (subject, body) =
            split_commit_message("Subject line\n\nFirst paragraph.\n\nSecond paragraph.\n\n");
        assert_eq!(subject, "Subject line");
        assert_eq!(
            body, "First paragraph.\n\nSecond paragraph.",
            "the blank line git puts after the subject and the trailing newlines are dropped, \
             but blank lines inside the body are kept"
        );

        let (subject, body) = split_commit_message("Subject\nBody starts immediately");
        assert_eq!(subject, "Subject");
        assert_eq!(body, "Body starts immediately");

        let (subject, body) = split_commit_message("");
        assert_eq!(subject, "");
        assert_eq!(body, "");
    }

    /// Spec §5's line is `short hash · author · date`; the email and the time
    /// of day are the tooltip's job, because on the line they wrapped the row
    /// to three lines in a dock-width panel.
    #[test]
    fn test_commit_identity() {
        let identity = commit_identity("550e4c28", "antivanov", "anton@citeck.ru", None);
        assert_eq!(identity.short_sha.as_ref(), "550e4c28");
        assert_eq!(identity.author.as_deref(), Some("antivanov"));
        assert_eq!(identity.date, None);
        assert_eq!(
            identity.tooltip.as_ref(),
            "550e4c28 · antivanov <anton@citeck.ru>",
            "the email the line drops has to survive in the tooltip"
        );

        let anonymous = commit_identity("550e4c28", "", "", None);
        assert_eq!(anonymous.author, None);
        assert_eq!(anonymous.tooltip.as_ref(), "550e4c28");

        let email_only = commit_identity("550e4c28", "", "ada@example.com", None);
        assert_eq!(
            email_only.tooltip.as_ref(),
            "550e4c28 · <ada@example.com>",
            "with no author name the email takes the separator the name would have used"
        );

        let dated = commit_identity("550e4c28", "Ada", "ada@example.com", Some(0));
        let date = dated.date.expect("a resolved timestamp yields a date");
        assert!(
            !date.contains(':'),
            "the line's date carries no time of day: {date}"
        );
        assert!(
            dated.tooltip.contains(" at "),
            "the tooltip keeps the time of day: {}",
            dated.tooltip
        );
    }

    fn changed_file_entry(path: &str) -> ChangedFileEntry {
        ChangedFileEntry::from_commit_file(&CommitFile {
            path: RepoPath::new(path).expect("valid repo path"),
            old_text: Some("old".to_string()),
            new_text: Some("new".to_string()),
            is_binary: false,
        })
    }

    fn describe_changed_file_row(row: &ChangedFileRow) -> String {
        match row {
            ChangedFileRow::Directory {
                key,
                label,
                file_count,
                ..
            } => format!("dir[key={key}, label={label}, {file_count}]"),
            ChangedFileRow::File(entry) => format!("file[{}]", entry.file_name),
        }
    }

    #[test]
    fn test_build_changed_file_rows_groups_by_directory() {
        let entries = vec![
            changed_file_entry("docs/plans/b.md"),
            changed_file_entry("README.md"),
            changed_file_entry("docs/plans/a.md"),
        ];
        let root: SharedString = "my-repo".into();

        let rows = build_changed_file_rows(&entries, &root, &HashSet::default());
        let described: Vec<String> = rows.iter().map(describe_changed_file_row).collect();
        assert_eq!(
            described,
            vec![
                "dir[key=, label=my-repo, 1]".to_string(),
                "file[README.md]".to_string(),
                "dir[key=docs/plans, label=docs/plans, 2]".to_string(),
                "file[a.md]".to_string(),
                "file[b.md]".to_string(),
            ],
            "root-level files sit under a header named after the repository, \
             and each directory group is sorted by file name"
        );

        let mut collapsed = HashSet::default();
        collapsed.insert(SharedString::from("docs/plans"));
        let rows = build_changed_file_rows(&entries, &root, &collapsed);
        let described: Vec<String> = rows.iter().map(describe_changed_file_row).collect();
        assert_eq!(
            described,
            vec![
                "dir[key=, label=my-repo, 1]".to_string(),
                "file[README.md]".to_string(),
                "dir[key=docs/plans, label=docs/plans, 2]".to_string(),
            ],
            "a collapsed directory hides its files but keeps its count"
        );
    }
}
