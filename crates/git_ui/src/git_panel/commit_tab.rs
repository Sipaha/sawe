//! The building blocks of a *commit's* detail surface: the changed-files tree,
//! the commit-message split, the markdown identity line and the client-side
//! +/− totals.
//!
//! These were written for the git graph's inline detail sidebar and lived in
//! `git_graph.rs`; they are relocated here verbatim so the git panel's Commit
//! tab can host the same surface. `git_graph` depends on `git_ui` and never
//! the reverse, so down is the only direction they could move.
//!
//! The row renderers are the one thing that could not come across unchanged:
//! they were typed against `Context<GitGraph>` and are now hosted by two
//! unrelated views, so their host behaviour arrives as
//! [`ChangedFileRowHandlers`] — plain `&mut App` closures the host builds from
//! its own weak handle. Nothing about what they paint changed.

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
/// The git graph's sidebar steps its file rows in by
/// `18px + ProjectPanelSettings::indent_size` (38px at the shipped default).
/// `git_ui` cannot read that setting — `project_panel` depends on `git_ui`, so
/// the reverse is a dependency cycle — so the panel's own `TREE_INDENT` stands
/// in for the project panel's step. The leading 18px is the directory header's
/// chevron (14px) plus its gap (4px), which a file row has no equivalent of;
/// dropping it and using a bare `TREE_INDENT` would be a 22px regression
/// against the tree that ships today.
const COMMIT_TREE_INDENT: f32 = 18.0 + TREE_INDENT;

/// Cap on the Commit tab's message block. Without it a long commit message
/// pushes the changed-files tree out of a dock-width panel entirely; the block
/// scrolls past this height and a short message still takes only what it needs.
const COMMIT_MESSAGE_MAX_HEIGHT: f32 = 200.0;

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

/// One row of the sidebar's changed-files tree. IDEA renders the commit's
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
/// otherwise the sidebar renders a run of empty lines under short messages.
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

/// Escape the CommonMark inline markers that could turn an author name into
/// formatting. Only the markers that can *start* an inline construct are
/// escaped — over-escaping would show up verbatim in the text the user copies
/// out of the sidebar, since `markdown::Copy` copies from the source.
fn escape_markdown_inline(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// The markdown source of IDEA's identity line:
/// `550e4c28 antivanov <anton.ivanov@citeck.ru> on 14 Aug 2026 at 06:24`.
/// The email is a real `mailto:` link; the rest is plain text so the whole
/// line stays selectable as one run.
pub fn commit_identity_source(
    short_sha: &str,
    author_name: &str,
    author_email: &str,
    timestamp: Option<i64>,
) -> SharedString {
    let mut source = escape_markdown_inline(short_sha);
    if !author_name.is_empty() {
        source.push(' ');
        source.push_str(&escape_markdown_inline(author_name));
    }
    if !author_email.is_empty() {
        // The angle brackets are escaped and sit *outside* the link so the
        // parser cannot read them as an HTML tag or a nested autolink.
        source.push_str(&format!(
            " \\<[{}](mailto:{})\\>",
            escape_markdown_inline(author_email),
            author_email
        ));
    }
    if let Some(timestamp) = timestamp {
        source.push_str(" on ");
        source.push_str(&escape_markdown_inline(&format_detail_timestamp(timestamp)));
    }
    source.into()
}

/// `on <date> at <time>` reads better with the two halves separated, so the
/// detail sidebar spells the time out instead of reusing the log column's
/// compact `[day] [month] [year] [hour]:[minute]`.
fn detail_timestamp_format() -> &'static [BorrowedFormatItem<'static>] {
    static FORMAT: OnceLock<Vec<BorrowedFormatItem<'static>>> = OnceLock::new();
    FORMAT.get_or_init(|| {
        time::format_description::parse("[day] [month repr:short] [year] at [hour]:[minute]")
            .unwrap_or_default()
    })
}

fn format_detail_timestamp(timestamp: i64) -> String {
    let Ok(datetime) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return "Unknown".to_string();
    };

    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    datetime
        .to_offset(local_offset)
        .format(detail_timestamp_format())
        .unwrap_or_default()
}

/// Style for the selectable single-line text fields of the commit-details
/// sidebar, matching what the equivalent [`Label`] rendered before.
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
    /// `<short-sha> <author> <email> on <date> at <time>` — see
    /// [`commit_identity_source`].
    identity: Entity<Markdown>,
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
/// the author avatar. Deliberately not [`GitPanel::git_remote`], which resolves
/// against `active_repository`; see [`CommitSelection`].
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
    pub fn show_commit_selection(
        &mut self,
        selection: CommitSelection,
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
            self.activate_commit_tab_without_focus();
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

        self.activate_commit_tab_without_focus();
        cx.notify();
    }

    /// Activate the Commit tab without taking focus — see
    /// [`Self::show_commit_selection`] for why the graph's push must not steal
    /// it. Leaving History also drops what History was holding, because
    /// `set_active_tab` (the focusing, user-driven route) is bypassed here.
    fn activate_commit_tab_without_focus(&mut self) {
        self.active_tab = GitPanelTab::Commit;
        self.drop_history_state();
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
                        let identity = commit_identity_source(
                            &details.short_sha(),
                            &details.author_name,
                            &details.author_email,
                            Some(details.commit_timestamp),
                        );
                        let text = CommitDetailText {
                            subject: cx.new(|cx| Markdown::new_text(subject, cx)),
                            body: cx.new(|cx| Markdown::new_text(body, cx)),
                            identity: cx.new(|cx| Markdown::new(identity, None, None, cx)),
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
    /// Focus is left alone on the way out for the same reason
    /// [`Self::show_commit_selection`] does not take it: the close can arrive
    /// from the graph.
    pub fn close_commit_tab(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.commit_tab.take().is_none() {
            return;
        }
        // Only the Commit tab itself is yanked back to Changes; a user parked
        // on another tab keeps the tab they chose.
        if self.active_tab == GitPanelTab::Commit {
            self.active_tab = GitPanelTab::Changes;
        }
        cx.emit(Event::CommitTabClosed);
        cx.notify();
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
                let identity_style =
                    detail_text_style(TextSize::Small, Color::Muted, None, window, cx);
                let has_body = !text.body.read(cx).source().is_empty();

                let author_email =
                    (!details.author_email.is_empty()).then(|| details.author_email.clone());
                let remote = commit_remote(&state.selection.repository, cx);
                let avatar = CommitAvatar::new(&full_sha, author_email, remote.as_ref())
                    .size(px(16.))
                    .render(window, cx);

                body.child(
                    div()
                        .id("commit-tab-message")
                        .flex_shrink_0()
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
                // `flex_1` on the text, not just `min_w_0`: in a row flex a
                // child's width comes from its own content and `min_w_0` lets
                // it shrink to nothing, so a narrow dock could hand the
                // markdown a one-character width and wrap the identity line
                // vertically.
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .w_full()
                        .px_2()
                        .pb_1p5()
                        .gap_1p5()
                        .items_start()
                        .child(div().flex_shrink_0().pt_0p5().child(avatar))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(MarkdownElement::new(text.identity.clone(), identity_style)),
                        ),
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
    /// Changes tab's depth-1 file rows and as the graph sidebar's file rows.
    /// The headers themselves then sit 2px inside the Changes tab's 6px section
    /// headers; closing that last 2px would mean either changing the shared row
    /// renderer (which would move the graph's sidebar too) or a magic negative
    /// margin, and the file rows are the edge the eye actually tracks.
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
            .min_h_0()
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

    /// The identity line is the one place the detail surface parses real
    /// markdown, so the author name has to be escaped and the email has to
    /// come out as a `mailto:` link with the angle brackets outside it.
    #[test]
    fn test_commit_identity_source() {
        assert_eq!(
            commit_identity_source("550e4c28", "antivanov", "anton@citeck.ru", None).as_ref(),
            "550e4c28 antivanov \\<[anton@citeck.ru](mailto:anton@citeck.ru)\\>"
        );

        assert_eq!(
            commit_identity_source("550e4c28", "a_b*c", "", None).as_ref(),
            "550e4c28 a\\_b\\*c",
            "markdown inline markers in an author name must not become formatting"
        );

        assert_eq!(
            commit_identity_source("550e4c28", "", "", None).as_ref(),
            "550e4c28"
        );

        let with_date = commit_identity_source("550e4c28", "Ada", "ada@example.com", Some(0));
        assert!(
            with_date.contains(" on "),
            "a resolved timestamp gets an `on <date> at <time>` suffix: {with_date}"
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
