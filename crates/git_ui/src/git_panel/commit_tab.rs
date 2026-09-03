//! The building blocks of a *commit's* detail surface: the changed-files tree,
//! the commit-message split, the `short hash · author · date` identity line
//! and the client-side +/− totals.
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

use crate::commit_refs;
use git::repository::{CommitDetails, CommitDiff, CommitFile};
use git::status::{StatusCode, TrackedStatus};
use gpui::{
    AnyElement, ClipboardItem, FontWeight, StyleRefinement, TextStyleRefinement, UnderlineStyle,
};
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

/// Cap on the Commit tab's message block *while the user has never dragged the
/// divider*, and only then.
///
/// This is layout policy, not a safety rail: however tall the panel gets, an
/// undragged message block stops growing here and hands the surplus to the
/// changed-files tree. A drag is the user overriding that policy, so
/// [`GitPanel::commit_message_height`] being `Some` drops this cap entirely —
/// leaving it on would freeze the divider at 200px, broken exactly where the
/// user is pulling. The floor ([`COMMIT_MESSAGE_MIN_HEIGHT`]) stays on in both
/// cases. The block scrolls internally, so shrinking it costs only scrolling.
const COMMIT_MESSAGE_MAX_HEIGHT: f32 = 200.0;

/// Stop on a *dragged* message block, in logical pixels.
///
/// Not the divider's real upper bound. That is the changed-files tree's own
/// [`COMMIT_FILE_TREE_MIN_HEIGHT`], enforced by the flex pass: `.h()` is a
/// preferred height and `flex-shrink` defaults to 1, so once the tree bottoms
/// out the message block is squeezed back down whatever this says. All this
/// does is stop a pathological drag from parking an absurd number in the
/// panel's serialized state — the same job `MAX_COMPOSE_HEIGHT` does for the
/// solution session composer's handle, and the same number.
const COMMIT_MESSAGE_DRAG_MAX_HEIGHT: f32 = 400.0;

/// Half-height of the files↔message divider's grab area, either side of the
/// 1px rule it paints. A 1px rule with no slop is not grabbable; widening the
/// hitbox without widening the paint is the whole trick, and 3px is what the
/// solution band's top-edge handle uses for the same reason.
const COMMIT_MESSAGE_HANDLE_HIT_SLOP: f32 = 3.0;

/// Drag payload for the files↔message divider. Carries nothing: the height
/// comes from the message block's own hitbox and the cursor position, both of
/// which arrive on the `DragMoveEvent`.
#[derive(Clone)]
struct DraggedCommitMessageEdge;

/// Keep a message-block height inside the range the layout can use. Applied to
/// dragged values and to whatever comes back out of the panel's serialized
/// state, which is a hand-editable KVP row.
pub(super) fn clamp_commit_message_height(height: Pixels) -> Pixels {
    height.clamp(
        px(COMMIT_MESSAGE_MIN_HEIGHT),
        px(COMMIT_MESSAGE_DRAG_MAX_HEIGHT),
    )
}

/// Height to store for the message block while the divider is being dragged,
/// from the block's last painted `bounds` and the cursor's `pointer_y`.
///
/// Measured down from the block's painted BOTTOM edge, which the drag does not
/// move — the identity row and the containment line under it are
/// `flex_shrink_0` — rather than from an anchor captured at mouse-down. An
/// undragged block has no stored height to anchor on, and its automatic one is
/// content-derived and unknowable outside a layout pass, so an anchored delta
/// would make the very first grab jump.
///
/// Deliberately NOT capped at what the last frame actually granted. Once the
/// changed-files tree hits its floor the block paints shorter than the stored
/// height, and capping there looks like it would help the reversal — but this
/// value is absolute, not a delta, so reversal is already immediate: the moment
/// the cursor descends past the painted divider, `requested` is below the
/// painted height and the divider follows on that very event. The cap instead
/// oscillates, because `bounds` is the hitbox from the LAST PAINT and both X11
/// and Wayland dispatch a whole batch of motion events back to back with no
/// draw between them. Two moves in one frame then read the same stale bounds:
/// the first raises the height, the second sees the stale shortfall and puts it
/// back. An even number of motions per frame means an upward drag does not move
/// at all, while downward stays smooth. It also lets a temporarily short panel
/// permanently overwrite a taller stored height.
pub(super) fn dragged_commit_message_height(bounds: Bounds<Pixels>, pointer_y: Pixels) -> Pixels {
    clamp_commit_message_height(bounds.bottom() - pointer_y)
}

/// The stored height after a click on the divider. A double click hands the
/// block back to the automatic layout; anything else must leave the height
/// alone.
///
/// A drag never arrives here: crossing gpui's drag threshold takes the pending
/// mouse-down, so releasing after a real drag emits no click at all. The
/// single-click branch exists so that a stray press on the handle is not a
/// reset — and it is also why a fast second drag can never be misread as a
/// double click and discard the height the user just set.
pub(super) fn commit_message_height_after_click(
    click_count: usize,
    stored: Option<Pixels>,
) -> Option<Pixels> {
    if click_count >= 2 { None } else { stored }
}

/// Floor of the Commit tab's message block: enough for the first line of the
/// subject plus the block's own vertical padding, so that however short the
/// panel gets the user can still see *which* commit the tab is describing.
const COMMIT_MESSAGE_MIN_HEIGHT: f32 = 44.0;

/// Floor of the changed-files tree. The tree is the tab's payload, so it gets
/// a guaranteed share rather than absorbing every shortfall as the `flex_1`
/// child: without this the message block and the identity row together left it
/// zero pixels on a git panel at its shipped dock height.
///
/// Deliberately not re-derived from `changes_list::list_item_height`: it is a
/// pixel floor on the region, not a row count, and rounding it to a whole
/// number of rows would only make the tree stop scrolling at exactly the point
/// where a partly-visible row is the cue that there is more below.
const COMMIT_FILE_TREE_MIN_HEIGHT: f32 = 72.0;

/// Cap on an expanded containment list — the tag row's as well as the
/// containing-branches line's.
///
/// Collapsed, the line is one truncating row and costs the changed-files tree
/// nothing it was not already paying for the identity row above it. Expanded it
/// wraps, and a commit on a busy repository can be contained in hundreds of
/// branches — and, since `git tag --contains` answers with every tag whose
/// history reaches the commit, in hundreds of tags too — so without a cap the
/// tree would be pushed to its own floor by a single click. Four wrapped rows
/// of `LabelSize::Small` is enough to read a dozen names; past that the block
/// scrolls internally, which is the same bargain the message block above it
/// makes.
const COMMIT_CONTAINMENT_EXPANDED_MAX_HEIGHT: f32 = 64.0;

/// Settle time before the Commit tab asks git which branches *and tags*
/// contain the selected commit.
///
/// The tab is driven by the git graph's selection, arrow-key movement from row
/// to row included. Without a debounce, holding an arrow key queues one
/// `git branch --contains` per row onto the repository's job queue — ahead of
/// the commit diff, which is the thing the tab actually paints first. Dropping
/// the task on the next selection cancels the pending query before it is ever
/// sent, so only the row the user stops on costs a git invocation.
///
/// Named for branches only because it is spelled out by name in
/// `git_panel`'s tests; the one timer now gates both halves of
/// [`GitPanel::load_commit_tab_containment`].
pub(super) const BRANCHES_CONTAINING_DEBOUNCE: Duration = Duration::from_millis(150);

/// How many branch names the "In N branches" line spells out before it hides
/// the rest behind `Show all`.
const MAX_LISTED_BRANCHES: usize = 5;

/// How many tag names the tag row spells out before it hides the rest behind
/// `Show all`.
///
/// The row is fed by `git tag --points-at`, so it lists the commit's own tags:
/// zero for almost every commit, one for a release, and the cap will not fire
/// on either. It is kept anyway because the case that overflows it is real
/// rather than hypothetical — a monorepo release commit carries one tag per
/// published package (`pkg-a@1.2.3`, `pkg-b@4.5.6`, …), and a repo that keeps
/// moving aliases stacks `v1.4`, `v1.4.0`, `stable` and `latest` on the same
/// commit. The row deliberately carries no count (see
/// [`format_tags_pointing_at`]), so without the cap those names would push the
/// changed-files tree down with nothing telling the user why. Same five as
/// [`MAX_LISTED_BRANCHES`]: one truncating line's worth.
const MAX_LISTED_TAGS: usize = 5;

/// One of the stacked regions of the Commit tab body.
///
/// The order is a constant rather than the shape of `render_commit_tab`'s
/// statements because it is the one thing about the tab's layout that a unit
/// test can hold onto: the renderer returns an opaque `AnyElement` and needs a
/// live workspace to build, so a reorder is otherwise unguarded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CommitTabSection {
    /// The changed-files header and tree, or the placeholder standing in for
    /// them while the diff loads or after it failed.
    Diff,
    /// The commit message and the identity line under it, or the placeholder
    /// standing in for them while the details load or after they failed.
    Message,
    /// The refs *pointing at* the commit, as chips — the same chips the graph
    /// row one line below paints. Renders nothing at all when no ref points at
    /// the commit, which is the overwhelmingly common case; see
    /// [`GitPanel::render_commit_refs_row`].
    Refs,
    /// IDEA's tag row: a tag icon and the names of the tags *pointing at* the
    /// commit, with no `In N tags:` prose — a tag is a name, not a count.
    /// Renders nothing at all until the tags are loaded and non-empty — see
    /// [`format_tags_pointing_at`].
    Tags,
    /// IDEA's `In N branches: …` line. Renders nothing at all until the
    /// branches are loaded and non-empty — see [`format_branches_containing`].
    Branches,
}

/// Painted top-to-bottom order of the Commit tab body.
///
/// Files above, message below — mirroring the Changes tab, where the file list
/// is on top and the commit editor sits under it. The tab's single horizontal
/// rule follows from this order: it hangs off the *message*, the section
/// painted directly under the tree, so that it always separates the two.
///
/// The metadata rows come last because they are facts about the commit of the
/// same class as the identity row the message section ends with — IDEA puts
/// them under the sha/author/date line for the same reason — and because they
/// are the sections that can render nothing, which at the bottom costs no rule
/// and no gap. Tags sit above branches, matching IDEA: the tag row is the
/// shorter, more specific fact, and a commit that has one usually has exactly
/// one.
///
/// The ref chips lead that block. They name the refs that *are* this commit,
/// where the two rows under them describe what the commit is reachable from, so
/// they are the more specific fact again; and they are the row the user is
/// comparing against the graph, which paints the same chips at the same height
/// as the subject.
const COMMIT_TAB_SECTIONS: [CommitTabSection; 5] = [
    CommitTabSection::Diff,
    CommitTabSection::Message,
    CommitTabSection::Refs,
    CommitTabSection::Tags,
    CommitTabSection::Branches,
];

/// What a changed-files row does when the user acts on it, supplied by the
/// host as `&mut App` closures built from its own weak handle rather than the
/// rows naming a concrete view. The indirection dates from the tree having two
/// hosts that shared no type; the git graph's sidebar is gone and the Commit
/// tab is the only host left, but erasing the host is also what keeps these
/// closures safe to install into event callbacks under the panel's own lease
/// — see [`GitPanel::commit_file_row_handlers`].
#[derive(Clone)]
struct ChangedFileRowHandlers {
    /// Left click on a file row — marks it as the tree's selected file.
    select_file: Rc<dyn Fn(&RepoPath, &mut Window, &mut App)>,
    /// Right click on a file row, at the click position.
    deploy_file_context_menu: Rc<dyn Fn(&RepoPath, Point<Pixels>, &mut Window, &mut App)>,
    /// Click on a directory header row — collapses or expands the group.
    toggle_directory: Rc<dyn Fn(&SharedString, &mut Window, &mut App)>,
}

/// The two independent highlight states a changed-file row can be in.
///
/// They are different questions and a row can answer yes to both: `cursor` is
/// the row the user last put the pointer on, `open_in_pane` is the row whose
/// diff the centre pane is showing. Clicking a row only *retargets* an already
/// open single-file diff, so the two genuinely come apart — and activating an
/// unrelated tab moves `open_in_pane` without touching `cursor`.
#[derive(Clone, Copy)]
struct ChangedFileRowMarks {
    cursor: bool,
    open_in_pane: bool,
}

#[derive(Clone)]
struct ChangedFileEntry {
    status: FileStatus,
    file_name: SharedString,
    dir_path: SharedString,
    repo_path: RepoPath,
    /// The row's `+N −M`, or `None` when it has none to show: a binary file,
    /// or `git_panel.diff_stats` turned off. Passed in rather than derived
    /// here — see [`compute_diff_stats`] for why the row must never diff its
    /// own file.
    stat: Option<DiffLineCount>,
}

impl ChangedFileEntry {
    fn from_commit_file(file: &CommitFile, stat: Option<DiffLineCount>) -> Self {
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
            stat,
        }
    }

    /// The diff half of a left click on this row (the selection half is the
    /// caller's [`ChangedFileRowHandlers::select_file`]).
    ///
    /// Double click *summons* the shared single-file diff tab; a plain single
    /// click only *retargets* it, and does nothing when it is not open — the
    /// same rule the Changes tab follows, so mouse-walking either list cannot
    /// spray tabs across the pane. Neither gesture pins: a pinned tab leaves
    /// the pane's preview slot, and the next single click would then summon a
    /// *second* tab.
    fn handle_row_click(
        &self,
        click_count: usize,
        commit_sha: &SharedString,
        repository: &WeakEntity<Repository>,
        workspace: &WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(repository) = repository.upgrade() else {
            return;
        };
        let mode = if click_count >= 2 {
            // Focus stays in the panel, so the next click keeps landing on
            // file rows.
            DiffOpen::Summon { focus: false }
        } else {
            DiffOpen::Retarget
        };
        SoloDiffView::open_commit_file(
            commit_sha.clone(),
            repository,
            self.repo_path.clone(),
            workspace.clone(),
            mode,
            window,
            cx,
        )
        .detach_and_notify_err(workspace.clone(), window, cx);
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
    /// header. It stays a parameter rather than reading [`COMMIT_TREE_INDENT`]
    /// directly so the row renderer keeps no opinion about the tree it is laid
    /// out in.
    fn render(
        &self,
        ix: usize,
        indent: Pixels,
        commit_sha: SharedString,
        repository: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        handlers: ChangedFileRowHandlers,
        marks: ChangedFileRowMarks,
        cx: &App,
    ) -> AnyElement {
        let file_name = self.file_name.clone();
        let full_path = self.display_path();

        let handlers_for_click = handlers.clone();
        // The two states a Changes row can also be in, painted from the same
        // helper so the two tabs speak one visual language: a wash for the
        // click cursor, a stronger wash plus a bold name for the diff the
        // centre pane is actually showing.
        let (base_bg, _, _) =
            changes_list::row_background_colors(marks.cursor, marks.open_in_pane, cx);

        div()
            .w_full()
            .pl(indent)
            .bg(base_bg)
            .on_mouse_down(MouseButton::Right, {
                let repo_path = self.repo_path.clone();
                move |event: &MouseDownEvent, window, cx| {
                    (handlers.deploy_file_context_menu)(&repo_path, event.position, window, cx);
                    cx.stop_propagation();
                }
            })
            .child(
                ButtonLike::new(("changed-file", ix))
                    .height(changes_list::list_item_height().into())
                    .child(
                        h_flex()
                            .min_w_0()
                            .w_full()
                            .gap_1()
                            .overflow_hidden()
                            // Name and figures are laid out the way a Changes
                            // tab row lays them out: the name takes the slack
                            // and truncates, the figures never shrink. Without
                            // `flex_shrink_0` on the stat the numbers are the
                            // first thing a narrow dock drops, which is exactly
                            // backwards — the row already tells you the name is
                            // truncated by ellipsizing it.
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .flex_1()
                                    .gap_1()
                                    .child(git_status_icon(self.status))
                                    .child(
                                        Label::new(file_name)
                                            .when(marks.open_in_pane, |label| {
                                                label.weight(FontWeight::BOLD)
                                            })
                                            .truncate(),
                                    ),
                            )
                            .children(self.stat.map(|stat| {
                                div().flex_shrink_0().child(ui::DiffStat::new(
                                    ("changed-file-stat", ix),
                                    stat.added,
                                    stat.removed,
                                ))
                            })),
                    )
                    .tooltip({
                        let meta = full_path;
                        move |_, cx| Tooltip::with_meta("Open Diff", None, meta.clone(), cx)
                    })
                    // Every click selects; the diff half is
                    // `handle_row_click`, which documents the gesture split.
                    .on_click({
                        let entry = self.clone();
                        let handlers = handlers_for_click;
                        move |event: &ClickEvent, window, cx| {
                            (handlers.select_file)(&entry.repo_path, window, cx);
                            entry.handle_row_click(
                                event.click_count(),
                                &commit_sha,
                                &repository,
                                &workspace,
                                window,
                                cx,
                            );
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
enum ChangedFileRow {
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

/// Pair every file of a loaded commit diff with the +/− figures computed when
/// the diff landed. A file with no entry in [`CommitDiffStats::per_file`] — a
/// binary one — renders no figures, and `show_stats` (the `git_panel.diff_stats`
/// setting) drops them from every row at once.
fn changed_file_entries(loaded: &LoadedCommitDiff, show_stats: bool) -> Vec<ChangedFileEntry> {
    loaded
        .diff
        .files
        .iter()
        .map(|file| {
            let stat = show_stats
                .then(|| loaded.stats.per_file.get(&file.path).copied())
                .flatten();
            ChangedFileEntry::from_commit_file(file, stat)
        })
        .collect()
}

/// Flatten a commit's changed files into directory-grouped rows. Files under a
/// collapsed directory are dropped, but its header keeps the full count so the
/// user can see how much is hidden.
fn build_changed_file_rows(
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
fn render_changed_directory_row(
    ix: usize,
    key: SharedString,
    label: SharedString,
    file_count: usize,
    collapsed: bool,
    handlers: ChangedFileRowHandlers,
) -> AnyElement {
    let tooltip_label = label.clone();
    ButtonLike::new(("changed-dir", ix))
        .height(changes_list::list_item_height().into())
        .child(
            h_flex()
                .min_w_0()
                .w_full()
                .gap_1()
                .overflow_hidden()
                // A plain chevron rather than a `Disclosure`: that renders as
                // an `IconButton`, a nested button inside the row's own
                // `ButtonLike` that muddies the click target — the whole row is
                // the collapse affordance here, as it is on the Changes tab.
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
                .child(Label::new(label).truncate_start())
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
fn split_commit_message(message: &str) -> (SharedString, SharedString) {
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
        date: timestamp.map(|timestamp| crate::format_compact_date(timestamp).into()),
        tooltip: tooltip.into(),
    }
}

/// A containment row's text, split at the seam where the names stop and the
/// expand affordance begins. Both the containing-branches line and the tag row
/// are this shape.
///
/// The two halves are separate because the tail is a *button*: the deleted
/// `git_graph` version wrote `… and 3 more` as plain text, which told the user
/// there were more branches and then gave them no way to see them. That is the
/// gap this line exists to close, so the count and the control cannot be one
/// formatted string.
struct ContainmentLine {
    /// `In 1 branch: main` / `In 12 branches: a, b, c, d, e` for branches, the
    /// bare `v1.2.0, v1.2.1` for tags. Where there is a count it is always the
    /// *total*, never the number of names actually listed.
    line: SharedString,
    /// Label of the toggle that swaps the truncated list for the full one, or
    /// `None` when every name already fits and there is nothing to expand.
    toggle: Option<SharedString>,
}

/// Everything that distinguishes the tag row from the containing-branches
/// line in [`GitPanel::render_commit_containment_line`]. The rest of the row —
/// truncate-when-collapsed, scroll-when-expanded, the toggle's placement — is
/// identical, and duplicating it was how the two drifted apart the first time.
struct ContainmentRow {
    /// Stable element id of the name block, which is a scroll container when
    /// expanded and so cannot share an id with the other row's.
    list_id: &'static str,
    toggle_id: &'static str,
    /// The tag row is labelled by an icon instead of prose; the branches line
    /// carries its count in the text and takes none.
    icon: Option<IconName>,
    formatted: ContainmentLine,
    expanded: bool,
    /// Flips this row's own `*_expanded` flag. A plain fn pointer rather than
    /// a field name because the flag lives on `CommitTabState`, which the
    /// click handler only reaches through the panel's lease.
    toggle: fn(&mut CommitTabState),
}

/// The names half of a containment row: the comma-joined list, capped at `cap`
/// unless `expanded`, and the label of the toggle that swaps one for the other.
///
/// The toggle is keyed off the *total* rather than off what was listed, so a
/// list that exactly fits never paints a control that would change nothing.
fn listed_names(
    names: &[SharedString],
    cap: usize,
    expanded: bool,
) -> (String, Option<SharedString>) {
    let listed = if expanded {
        names.len()
    } else {
        names.len().min(cap)
    };
    let joined = names
        .iter()
        .take(listed)
        .map(|name| name.as_ref())
        .collect::<Vec<_>>()
        .join(", ");
    let toggle = (names.len() > cap).then(|| {
        if expanded {
            SharedString::new_static("Show less")
        } else {
            SharedString::new_static("Show all")
        }
    });
    (joined, toggle)
}

/// IDEA's `In 1 branch: main` / `In 12 branches: a, b, c, d, e` + `Show all`.
///
/// `None` when the commit is on no branch, and the line is then omitted rather
/// than rendered as `In 0 branches:`. That covers two cases that are worth
/// keeping indistinguishable here: a genuinely unreachable commit, and a
/// *remote* repository, where `Repository::branches_containing` answers with an
/// empty list because containment has no proto message (see its doc comment).
/// On a collab repo the line is therefore simply absent.
fn format_branches_containing(
    branches: &[SharedString],
    expanded: bool,
) -> Option<ContainmentLine> {
    if branches.is_empty() {
        return None;
    }
    let prefix = if branches.len() == 1 {
        "In 1 branch: ".to_string()
    } else {
        format!("In {} branches: ", branches.len())
    };
    let (names, toggle) = listed_names(branches, MAX_LISTED_BRANCHES, expanded);
    Some(ContainmentLine {
        line: format!("{prefix}{names}").into(),
        toggle,
    })
}

/// IDEA's tag row: `2.9.16`, or `pkg-a@1.2.3, pkg-b@4.5.6` + `Show all`.
///
/// The names are the commit's own tags — `git tag --points-at`, not
/// `--contains` — so `Release 2.9.16` shows `2.9.16` and not every release
/// tagged since. There can be more than one, and all of them belong on the row.
///
/// Deliberately no `In N tags:` prefix — IDEA labels this row with a tag icon
/// instead, and a tag is a name the user recognises rather than a count they
/// have to read. The row's icon is supplied by the renderer, so a caller that
/// gets `Some` here is holding text that means nothing on its own.
///
/// `None` for a commit with no tags, exactly as for branches — and with the
/// same two cases folded together: genuinely untagged, and a *remote*
/// repository, where `Repository::tags_pointing_at` answers `Ok(vec![])`
/// because the query has no proto message. "No tags" and "cannot ask" are
/// therefore indistinguishable here, and on a collab repo the row is simply
/// absent rather than empty.
fn format_tags_pointing_at(tags: &[SharedString], expanded: bool) -> Option<ContainmentLine> {
    if tags.is_empty() {
        return None;
    }
    let (names, toggle) = listed_names(tags, MAX_LISTED_TAGS, expanded);
    Some(ContainmentLine {
        line: names.into(),
        toggle,
    })
}

/// The tags `git tag --points-at` found that the commit's ref chips do not
/// already name.
///
/// Both describe the tags pointing at the commit, so painting both in full
/// would say the same thing twice a few pixels apart. Subtracting rather than
/// suppressing the row outright keeps it for the tag created since the graph
/// last decorated its rows, which is the only fact the row still has that the
/// chips do not.
fn uncharted_tags(tags: &[SharedString], ref_names: &[SharedString]) -> Vec<SharedString> {
    let charted: HashSet<&str> = commit_refs::tag_names(ref_names).collect();
    tags.iter()
        .filter(|tag| !charted.contains(tag.as_ref()))
        .cloned()
        .collect()
}

/// The line's three pieces share one style; a helper keeps them provably
/// identical rather than repeating the builder chain three times.
fn identity_label(text: SharedString) -> Label {
    Label::new(text).size(LabelSize::Small).color(Color::Muted)
}

fn identity_separator() -> Label {
    identity_label("·".into())
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

pub(crate) fn format_with(timestamp: i64, format: &[BorrowedFormatItem<'static>]) -> String {
    let Ok(datetime) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return "Unknown".to_string();
    };

    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    // `to_offset` panics rather than saturating when the shift would leave the
    // representable range, which a timestamp at either end of the epoch does.
    datetime
        .checked_to_offset(local_offset)
        .unwrap_or(datetime)
        .format(format)
        .unwrap_or_default()
}

fn format_detail_timestamp(timestamp: i64) -> String {
    format_with(timestamp, detail_timestamp_format())
}

/// Style for the Commit tab's selectable message text.
///
/// Built on [`git_commit_text_style`], the Changes tab's commit editor
/// typography, so that a commit message the user is *reading* here is the same
/// text as one they are *writing* over there. Before this it started from
/// `window.text_style()` and so rendered in the UI font, which is the whole of
/// why the two tabs disagreed.
fn detail_text_style(color: Color, weight: Option<gpui::FontWeight>, cx: &App) -> MarkdownStyle {
    let mut base_text_style = git_commit_text_style(
        ThemeSettings::get_global(cx).git_commit_buffer_font_size(cx),
        cx,
    );
    let font_size = base_text_style.font_size;
    let line_height = base_text_style.line_height;
    base_text_style.refine(&TextStyleRefinement {
        color: Some(color.color(cx)),
        font_weight: weight,
        ..Default::default()
    });

    // `base_text_style` alone is NOT enough for the two metrics below. Markdown
    // lowers its text to `TextRun`s (`markdown.rs::Renderer::push_text` →
    // `TextStyle::to_run`), and a `TextRun` carries font family / weight /
    // style / colour but neither a size nor a line height; `StyledText` reads
    // both from `window.text_style()` at layout time
    // (`gpui/src/elements/text.rs::TextLayout::layout`), i.e. from whatever the
    // containing div inherits. So those two have to be set on the container as
    // well — which is exactly what `MarkdownStyle::with_preview_overrides`
    // does. Without this the glyphs silently lay out at the window's UI size
    // and only the family, weight and colour take effect.
    let mut container_style = StyleRefinement::default();
    container_style.text.font_size = Some(font_size);
    container_style.text.line_height = Some(line_height);

    MarkdownStyle {
        base_text_style,
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

/// The `+N −M` of one file, or of a whole commit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DiffLineCount {
    added: usize,
    removed: usize,
}

/// A commit's +/− figures, whole and per file.
///
/// `CommitFile` carries no numstat — the counts are derived here from the
/// texts git already handed us, which is why the per-file half exists at all:
/// the diff that produces the header's total produces every row's figures on
/// the way, and throwing them away only to recompute them per frame would run
/// [`line_diff`] over every file of the commit on the render path.
#[derive(Default)]
struct CommitDiffStats {
    total: DiffLineCount,
    /// Keyed by the path of the `CommitFile` it was derived from. A **binary**
    /// file has no entry at all rather than a zero one — see
    /// [`compute_diff_stats`] — so `per_file` is not the same length as
    /// `CommitDiff::files` and a missing key means "this file has no line
    /// count", not "not computed yet".
    per_file: HashMap<RepoPath, DiffLineCount>,
}

/// Fold a commit's diff into its whole-commit and per-file +/− counts in a
/// single pass; the two must not be computed separately, or a future change to
/// one silently stops describing the other.
///
/// Binary files are skipped on both sides. `git --numstat` reports `-` rather
/// than a line count for one, and `load_commit_diff` gives us *empty* texts for
/// a binary file, so diffing it would only ever produce a truthful-looking
/// `+0 −0`. Skipping it from the total as well as from the rows keeps
/// `total == sum(per_file)` true by construction.
fn compute_diff_stats(diff: &CommitDiff) -> CommitDiffStats {
    let mut stats = CommitDiffStats::default();
    for file in &diff.files {
        if file.is_binary {
            continue;
        }
        let old_text = file.old_text.as_deref().unwrap_or("");
        let new_text = file.new_text.as_deref().unwrap_or("");
        let mut file_count = DiffLineCount::default();
        for (old_range, new_range) in line_diff(old_text, new_text) {
            file_count.added += (new_range.end - new_range.start) as usize;
            file_count.removed += (old_range.end - old_range.start) as usize;
        }
        stats.total.added += file_count.added;
        stats.total.removed += file_count.removed;
        stats.per_file.insert(file.path.clone(), file_count);
    }
    stats
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
    /// The refs pointing at `shas[0]`, as the graph row for that same commit
    /// paints them. Empty for a commit no ref points at, and for a caller that
    /// has no decorations to hand.
    pub refs: CommitRefs,
}

/// The ref decorations of one commit, travelling with the selection that names
/// it.
///
/// They come from the graph's own row data — git's `%D`, already fetched and
/// already laid out into lanes — rather than from a query the panel makes for
/// itself. Two reasons. The panel would otherwise run a third `git` process per
/// selection to re-derive what the row one line below it is already painting;
/// and the two surfaces are read together, so anything that let them answer
/// differently (a cache refreshed on one side only, a different `--decorate`
/// shape) would show up as the graph and its own detail pane disagreeing about
/// which branch a commit is on.
///
/// Only the first sha is described. A multi-commit selection renders a bare
/// count with no room for refs, so carrying every row's decorations would be
/// work for a surface that never paints them.
#[derive(Clone, Default)]
pub struct CommitRefs {
    /// Git's `%D` decorations verbatim and in git's order — `main`,
    /// `origin/main`, `HEAD -> feature/x`, `tag: 2.41.0`.
    pub names: Vec<SharedString>,
    /// Index into the theme's accents of the lane colour the graph painted this
    /// commit's chips with, so the detail pane's chips are the same colour as
    /// the row's.
    pub accent_idx: usize,
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
    stats: CommitDiffStats,
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
    /// Branches containing the commit. `Loading` covers the debounce window as
    /// well as the query itself, and `Loaded(vec![])` is both "on no branch"
    /// and "remote repository" — all three render nothing, so the distinction
    /// exists only for tests and for [`GitPanel::retry_failed_commit_loads`].
    pub(super) branches: LoadState<Vec<SharedString>>,
    /// Tags containing the commit, loaded by the same task as `branches` and
    /// therefore in lockstep with it. Same three-way collapse: loading, no
    /// tags and remote repository all render nothing.
    tags: LoadState<Vec<SharedString>>,
    /// Whether the containing-branches line is spelling out every branch.
    /// Lives here, so re-pointing the tab at another commit resets it with the
    /// rest of the state.
    branches_expanded: bool,
    /// The same, for the tag row. Separate from `branches_expanded` because
    /// the two rows hide different amounts and the user expands the one they
    /// are reading.
    tags_expanded: bool,
    text: Option<CommitDetailText>,
    pub(super) collapsed_dirs: HashSet<SharedString>,
    scroll_handle: UniformListScrollHandle,
    selected_file: Option<RepoPath>,
    _details_task: Option<Task<()>>,
    _diff_task: Option<Task<()>>,
    /// The one debounced task behind both `branches` and `tags` — see
    /// [`GitPanel::load_commit_tab_containment`].
    _containment_task: Option<Task<()>>,
}

impl CommitTabState {
    fn new(selection: CommitSelection) -> Self {
        Self {
            selection,
            details: LoadState::Idle,
            diff: LoadState::Idle,
            branches: LoadState::Idle,
            tags: LoadState::Idle,
            branches_expanded: false,
            tags_expanded: false,
            text: None,
            collapsed_dirs: HashSet::default(),
            scroll_handle: UniformListScrollHandle::new(),
            selected_file: None,
            _details_task: None,
            _diff_task: None,
            _containment_task: None,
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
            self.load_commit_tab_containment(sha, &repository, cx);
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
        // One task loads both, so either half having failed reruns the pair.
        let retry_containment = matches!(state.branches, LoadState::Failed(_))
            || matches!(state.tags, LoadState::Failed(_));
        if retry_details {
            self.load_commit_tab_details(sha, &repository, cx);
        }
        if retry_diff {
            self.load_commit_tab_diff(sha, &repository, cx);
        }
        // A remote repository answers with an *empty* list rather than an
        // error, so this retry cannot loop on collab; only a real
        // `git branch --contains` / `git tag --contains` failure reaches it.
        if retry_containment {
            self.load_commit_tab_containment(sha, &repository, cx);
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
                        let stats = compute_diff_stats(&diff);
                        LoadState::Loaded(LoadedCommitDiff { diff, stats })
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

    /// Load the branches containing the commit *and* the tags pointing at it
    /// into the open Commit tab, after [`BRANCHES_CONTAINING_DEBOUNCE`].
    ///
    /// The two halves are deliberately different git queries: `In N branches:`
    /// means reachability, matching IDEA, while the tag row means the commit's
    /// own tags. Only the branch half is containment, which is why the shared
    /// debounce constant keeps its branch-flavoured name.
    ///
    /// One task for both, not two. The debounce is the reason: the tab is
    /// driven by graph selection including arrow-key movement, so a second
    /// independent task would need its own timer and would queue its own job
    /// on every row the user travels — reintroducing exactly the queue-jamming
    /// the debounce was added to prevent. Folding them also collapses the
    /// staleness re-check to a single point: both fields are assigned inside
    /// one guarded block, so they cannot disagree about which commit they
    /// describe. The two queries are issued together and awaited with
    /// `join!` rather than in sequence, so the pair costs one round trip
    /// through the repository's job queue rather than two.
    ///
    /// Unlike the tab's other two loads this one cannot ask the repository up
    /// front — the whole point of the debounce is that the job is not queued
    /// until the selection has settled — so the task carries a *weak* handle
    /// and re-acquires it on the far side of the timer, rather than keeping a
    /// repository the user has since navigated away from alive for 150ms.
    fn load_commit_tab_containment(
        &mut self,
        sha: Oid,
        repository: &Entity<Repository>,
        cx: &mut Context<Self>,
    ) {
        let repository = repository.downgrade();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(BRANCHES_CONTAINING_DEBOUNCE)
                .await;
            let Ok((branches, tags)) = repository.update(cx, |repository, _| {
                (
                    repository.branches_containing(sha.to_string()),
                    repository.tags_pointing_at(sha.to_string()),
                )
            }) else {
                return;
            };
            let (branches, tags) = futures::join!(branches, tags);
            this.update(cx, |this, cx| {
                // Same guard as the other two loads: a response that resolved
                // after the selection moved on describes a commit that is no
                // longer on screen.
                if this.commit_tab_sha() != Some(sha) {
                    return;
                }
                // `what` is a whole noun phrase, not just the ref kind: the
                // two halves ask different questions and an error that says
                // "tags containing" would name a query we no longer run.
                let loaded = |what: &str, response| match response {
                    Ok(Ok(names)) => LoadState::Loaded(names),
                    Ok(Err(error)) => LoadState::Failed(SharedString::from(format!(
                        "Couldn't list the {what} commit {}: {error:#}",
                        sha.display_short()
                    ))),
                    Err(_) => LoadState::Failed(SharedString::from(format!(
                        "Listing the {what} commit {} was cancelled.",
                        sha.display_short()
                    ))),
                };
                let branches = loaded("branches containing", branches);
                let tags = loaded("tags on", tags);
                if let Some(state) = this.commit_tab.as_mut() {
                    state.branches = branches;
                    state.tags = tags;
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(state) = self.commit_tab.as_mut() {
            state.branches = LoadState::Loading;
            state.tags = LoadState::Loading;
            state.branches_expanded = false;
            state.tags_expanded = false;
            state._containment_task = Some(task);
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

    /// The Commit tab body, in [`COMMIT_TAB_SECTIONS`] order: the changed-files
    /// tree under a header carrying the file count and the commit's +/− totals,
    /// then the commit message with the
    /// `<short sha> <author> <email> on <date>` identity line under it.
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
        // `SoloDiffView::open_commit_file`, which fails to find the file in a
        // commit named by a short one.
        let full_sha: SharedString = sha.to_string().into();

        let mut body = v_flex().flex_1().size_full().min_h_0().overflow_hidden();

        let border = cx.theme().colors().border;
        // One switch for both halves of the tab's +/− figures. The setting is
        // documented as "the addition/deletion change count next to each file
        // in the Git panel", and the Changes tab hides its own header total
        // under it too; a Commit tab that kept the total while dropping the
        // rows would honour it half way.
        let show_diff_stats = GitPanelSettings::get_global(cx).diff_stats;
        // The tab's one horizontal rule belongs to the section painted second,
        // so it separates the two — but only when the first section actually
        // put something above it. `Idle` (a diff that was never asked for)
        // renders nothing, and a rule against the top of the body would then be
        // separating the message from the tab bar. It also gates the divider:
        // with nothing painted above the message there is nothing to resize
        // against, so the handle would only ever move the message against the
        // tab bar.
        let message_carries_rule = !matches!(state.diff, LoadState::Idle);

        for section in COMMIT_TAB_SECTIONS {
            body = match section {
                CommitTabSection::Diff => match &state.diff {
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
                                .child(
                                    Label::new(format!(
                                        "{file_count} Changed {}",
                                        if file_count == 1 { "File" } else { "Files" }
                                    ))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                )
                                // `ui::DiffStat`, fully qualified: `use super::*`
                                // brings `git::status::DiffStat`, the data type,
                                // into scope under the same bare name.
                                .when(show_diff_stats, |this| {
                                    this.child(ui::DiffStat::new(
                                        "commit-tab-diff-stat",
                                        loaded.stats.total.added,
                                        loaded.stats.total.removed,
                                    ))
                                }),
                        )
                        .child(self.render_commit_file_tree(
                            state,
                            loaded,
                            full_sha.clone(),
                            window,
                            cx,
                        ))
                    }
                    LoadState::Failed(error) => body.child(
                        div().flex_shrink_0().px_2().py_1p5().child(
                            Label::new(error.clone())
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        ),
                    ),
                    LoadState::Loading => body.child(
                        div().flex_shrink_0().px_2().py_1p5().child(
                            Label::new("Loading changed files…")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    ),
                    LoadState::Idle => body,
                },
                CommitTabSection::Message => match (&state.details, &state.text) {
                    (LoadState::Loaded(details), Some(text)) => {
                        // The two tabs' commit messages must look like the
                        // same kind of text, with the Changes tab as the
                        // reference: `detail_text_style` is now built on the
                        // buffer font at `git_commit_buffer_font_size`, the
                        // typography that tab's commit editor uses, rather than
                        // on the UI font this one used to inherit.
                        //
                        // `SEMIBOLD` stays on the subject. The Changes tab has
                        // no subject/body split to copy a weight from — it is
                        // one plain editor — so there is nothing here to match,
                        // and dropping it would flatten the subject into both
                        // the body below it and the `LabelSize::Small` /
                        // `Color::Muted` identity line and files header, which
                        // is the only hierarchy this block has left.
                        let subject_style =
                            detail_text_style(Color::Default, Some(gpui::FontWeight::SEMIBOLD), cx);
                        let body_style = detail_text_style(Color::Default, None, cx);
                        let has_body = !text.body.read(cx).source().is_empty();

                        let author_email = (!details.author_email.is_empty())
                            .then(|| details.author_email.clone());
                        let remote = commit_remote(&state.selection.repository, cx);
                        let avatar = CommitAvatar::new(&full_sha, author_email, remote.as_ref())
                            .size(px(16.))
                            .render(window, cx);

                        body.when(message_carries_rule, |this| {
                            this.child(self.render_commit_message_resize_handle(border, cx))
                        })
                        .child(
                            // Shrinkable between its floor and its cap rather
                            // than pinned at the cap: on a dock-height panel the
                            // tab body has ~282px to spend, and a fixed 200px
                            // message left the changed-files tree nothing. Flex
                            // distributes by factor and clamps by min/max, not
                            // by order, so this arithmetic is the same whether
                            // the block is painted above the tree or below it.
                            //
                            // Once dragged the cap goes and `.h()` takes over as
                            // a *preferred* height: `flex-shrink` still defaults
                            // to 1 and the floor is still on, so the tree's own
                            // floor squeezes the block back down through the
                            // flex pass rather than through any arithmetic here.
                            div()
                                .id("commit-tab-message")
                                .min_h(px(COMMIT_MESSAGE_MIN_HEIGHT))
                                .map(|this| match self.commit_message_height {
                                    Some(height) => this.h(height),
                                    None => this.max_h(px(COMMIT_MESSAGE_MAX_HEIGHT)),
                                })
                                // The divider's grab strip is `deferred` and
                                // `block_mouse_except_scroll`, so it is the
                                // topmost hitbox for the whole gesture and no
                                // ancestor's `on_drop` ever fires (FORK.md #84,
                                // #92). The height is therefore committed here,
                                // on every move, and there is no separate
                                // "visible" value waiting for a drop that never
                                // comes. Do not "fix" this back to `on_drop`.
                                .on_drag_move(cx.listener(
                                    |this,
                                     event: &DragMoveEvent<DraggedCommitMessageEdge>,
                                     _window,
                                     cx| {
                                        let height = dragged_commit_message_height(
                                            event.bounds,
                                            event.event.position.y,
                                        );
                                        if this.commit_message_height != Some(height) {
                                            this.commit_message_height = Some(height);
                                            this.serialize(cx);
                                            cx.notify();
                                        }
                                    },
                                ))
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
                                            div().min_w_0().child(MarkdownElement::new(
                                                text.body.clone(),
                                                body_style,
                                            ))
                                        })),
                                ),
                        )
                        // One line, always: only the author is allowed to
                        // shrink, and it truncates rather than wrapping. A
                        // wrapped identity row is vertical budget taken from the
                        // changed-files tree above it.
                        //
                        // `pt_1p5` is the gap between the message and this row,
                        // and it has to live *here* rather than on the message
                        // block: that block's own padding is inside its scroll
                        // container, so a message longer than
                        // `COMMIT_MESSAGE_MAX_HEIGHT` scrolls its bottom padding
                        // out of the viewport and clips mid-line flush against
                        // this row. It matches the message block's own `py_1p5`
                        // so that the seam reads the same whether the message
                        // overflowed or not. A `border_t_1` here instead would
                        // be the tab's second rule, and on a subject-only commit
                        // the two would sit ~44px apart and box a single line.
                        .child(
                            h_flex()
                                .id("commit-tab-identity")
                                .flex_shrink_0()
                                .w_full()
                                .px_2()
                                .pt_1p5()
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
                        div()
                            .flex_shrink_0()
                            .px_2()
                            .py_1p5()
                            .when(message_carries_rule, |this| {
                                this.border_t_1().border_color(border)
                            })
                            .child(
                                Label::new(error.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Error),
                            ),
                    ),
                    _ => body.child(
                        div()
                            .flex_shrink_0()
                            .px_2()
                            .py_1p5()
                            .when(message_carries_rule, |this| {
                                this.border_t_1().border_color(border)
                            })
                            .child(
                                Label::new("Loading commit…")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    ),
                },
                CommitTabSection::Refs => match self.render_commit_refs_row(state, cx) {
                    Some(row) => body.child(row),
                    None => body,
                },
                CommitTabSection::Tags => {
                    let expanded = state.tags_expanded;
                    let formatted = match &state.tags {
                        // Only the tags the chip row above did not already
                        // name. The two rows answer the same question — which
                        // tags point at this commit — from different sources:
                        // the chips from the graph's decorations, this row from
                        // a live `git tag --points-at`. Subtracting rather than
                        // suppressing keeps the row for the tag the decorations
                        // are too old to know about, which is the only case
                        // where it still has something to say.
                        LoadState::Loaded(tags) => format_tags_pointing_at(
                            &uncharted_tags(tags, &state.selection.refs.names),
                            expanded,
                        ),
                        _ => None,
                    };
                    match formatted {
                        // No child at all when there are no tags — not an
                        // empty row, which would still spend its padding. Most
                        // commits carry no tag, and that is what makes a third
                        // metadata row affordable in a tab whose body is
                        // already fighting two hard floors.
                        None => body,
                        Some(formatted) => body.child(self.render_commit_containment_line(
                            ContainmentRow {
                                list_id: "commit-tab-tags",
                                toggle_id: "commit-tab-tags-toggle",
                                icon: Some(IconName::Bookmark),
                                formatted,
                                expanded,
                                toggle: |state| state.tags_expanded = !state.tags_expanded,
                            },
                            cx,
                        )),
                    }
                }
                CommitTabSection::Branches => {
                    let expanded = state.branches_expanded;
                    let formatted = match &state.branches {
                        LoadState::Loaded(branches) => {
                            format_branches_containing(branches, expanded)
                        }
                        // Nothing is painted while the query is in flight or
                        // after it failed: reserving a row for a line that may
                        // never arrive would cost the changed-files tree the
                        // same height whether the commit is on a branch or not.
                        _ => None,
                    };
                    match formatted {
                        None => body,
                        Some(formatted) => body.child(self.render_commit_containment_line(
                            ContainmentRow {
                                list_id: "commit-tab-branches",
                                toggle_id: "commit-tab-branches-toggle",
                                // The count is in the text, so no icon: an
                                // icon plus `In 3 branches:` would say the
                                // same thing twice.
                                icon: None,
                                formatted,
                                expanded,
                                toggle: |state| state.branches_expanded = !state.branches_expanded,
                            },
                            cx,
                        )),
                    }
                }
            };
        }

        body.into_any_element()
    }

    /// The files↔message divider: the 1px rule the message block used to paint
    /// as its own `border_t_1`, now a flex child of its own so it can carry a
    /// grab area.
    ///
    /// The rule stays 1px; the grab strip is a `deferred` child straddling it,
    /// [`COMMIT_MESSAGE_HANDLE_HIT_SLOP`] either side. `deferred` because the
    /// strip overhangs the changed-files tree above it and has to be hit-tested
    /// on top of its rows, and `block_mouse_except_scroll` rather than
    /// `occlude` because `occlude` swallows the wheel too and the tree right
    /// above would lose three pixels of scrollable band.
    fn render_commit_message_resize_handle(
        &self,
        border: Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("commit-tab-message-resize")
            .flex_none()
            .w_full()
            .h(px(1.))
            .bg(border)
            .child(deferred(
                div()
                    .id("commit-tab-message-resize-grab")
                    .absolute()
                    .top(px(-COMMIT_MESSAGE_HANDLE_HIT_SLOP))
                    .left_0()
                    .right_0()
                    .h(px(COMMIT_MESSAGE_HANDLE_HIT_SLOP * 2. + 1.))
                    .cursor_row_resize()
                    .block_mouse_except_scroll()
                    .on_click(cx.listener(|this, event: &ClickEvent, _window, cx| {
                        let height = commit_message_height_after_click(
                            event.click_count(),
                            this.commit_message_height,
                        );
                        if height != this.commit_message_height {
                            this.commit_message_height = height;
                            this.serialize(cx);
                            cx.notify();
                        }
                        cx.stop_propagation();
                    }))
                    .on_drag(DraggedCommitMessageEdge, |_, _, _, cx| cx.new(|_| Empty)),
            ))
            .into_any_element()
    }

    /// The refs pointing at the commit, as the chips the graph's own row for it
    /// paints — see [`crate::commit_refs`], which both surfaces build them
    /// with, and [`CommitRefs`] for why the panel is handed them rather than
    /// asking git itself.
    ///
    /// `None`, and therefore no element at all, when nothing points at the
    /// commit. That is most commits, and an empty row would still spend its
    /// padding out of the changed-files tree's budget — the same reason the two
    /// containment rows below render nothing rather than an empty line.
    ///
    /// One line, never wrapped, for the reason the identity row above it gives:
    /// a wrapped row is vertical budget taken from the tree. What does not fit
    /// collapses into the graph's own `+N` chip, whose tooltip still names every
    /// ref, at the graph's own `git.log.compact_refs_threshold`. The graph
    /// applies that threshold only under its `compact_refs` view toggle and this
    /// row applies it always: the toggle is a control of the graph's, this row
    /// is narrower than the graph's Description column, and a chip row that can
    /// grow without bound is one a long-lived release branch would eat the tab
    /// with.
    fn render_commit_refs_row(&self, state: &CommitTabState, cx: &App) -> Option<AnyElement> {
        let names = &state.selection.refs.names;
        if names.is_empty() {
            return None;
        }

        let repository = state.selection.repository.read(cx);
        let head_branch_name: Option<SharedString> = repository
            .snapshot()
            .branch
            .as_ref()
            .map(|branch| SharedString::from(branch.name().to_string()));
        let work_dir = repository.work_directory_abs_path.to_path_buf();
        let accent_color = commit_refs::accent_color(state.selection.refs.accent_idx, cx);

        // A zero threshold would hide every chip behind a `+N`, which is a
        // worse row than no row; one chip is the least this can usefully paint.
        let threshold = (project::project_settings::ProjectSettings::get_global(cx)
            .git
            .log
            .compact_refs_threshold as usize)
            .max(1);
        let visible = names.len().min(threshold);

        Some(
            h_flex()
                .id("commit-tab-refs")
                .debug_selector(|| "COMMIT-TAB-REFS".into())
                .flex_shrink_0()
                .w_full()
                .px_2()
                .pb_1p5()
                .gap_1()
                .overflow_hidden()
                .children(names.iter().take(visible).map(|name| {
                    commit_refs::ref_chip(
                        name,
                        accent_color,
                        commit_refs::is_head_ref(name.as_ref(), head_branch_name.as_ref()),
                        Some(work_dir.as_path()),
                        true,
                    )
                }))
                .children(
                    (names.len() > visible)
                        .then(|| commit_refs::overflow_chip(&names[visible..], accent_color)),
                )
                .into_any_element(),
        )
    }

    /// IDEA's containment rows, under the identity row: the tag row and the
    /// `In N branches: …` line, which differ only by the fields of
    /// [`ContainmentRow`].
    ///
    /// Collapsed it is one row that truncates rather than wrapping, for the
    /// reason the identity row above it gives: a wrapped row is vertical budget
    /// taken from the changed-files tree. Expanded it wraps and scrolls inside
    /// [`COMMIT_CONTAINMENT_EXPANDED_MAX_HEIGHT`], so `Show all` can never push
    /// the tree past its floor however many branches or tags contain the
    /// commit. The toggle sits outside the scrolling block so that `Show less`
    /// stays reachable without scrolling back up to it.
    fn render_commit_containment_line(
        &self,
        row: ContainmentRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = row.expanded;
        let label = Label::new(row.formatted.line)
            .size(LabelSize::Small)
            .color(Color::Muted);
        let label = if expanded { label } else { label.truncate() };
        let toggle = row.toggle;

        h_flex()
            .flex_shrink_0()
            .w_full()
            .px_2()
            .pb_1p5()
            .gap_1()
            .when(expanded, |this| this.items_start())
            .children(row.icon.map(|icon| {
                div()
                    .flex_shrink_0()
                    .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
            }))
            .child(
                div()
                    .id(row.list_id)
                    .min_w_0()
                    .when(expanded, |this| {
                        this.max_h(px(COMMIT_CONTAINMENT_EXPANDED_MAX_HEIGHT))
                            .overflow_y_scroll()
                    })
                    .child(label),
            )
            .children(row.formatted.toggle.map(|label| {
                div().flex_shrink_0().child(
                    Button::new(row.toggle_id, label)
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .color(Color::Accent)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(state) = this.commit_tab.as_mut() {
                                toggle(state);
                                cx.notify();
                            }
                        })),
                )
            }))
            .into_any_element()
    }

    /// The commit's changed files, grouped by directory.
    ///
    /// The tree carries no left inset of its own: `ButtonLike`'s own 4px
    /// horizontal padding is the directory header's indent, and the file rows
    /// step in by [`COMMIT_TREE_INDENT`] on top of it so that their painted
    /// content edge lands on the Changes tab's — see that constant for the
    /// measurement. The directory headers are deliberately *not* aligned with
    /// the Changes tab's section headers (measured 4px apart): closing that
    /// would mean either changing the shared row renderer or a magic negative
    /// margin, and the file rows are the edge the eye actually tracks.
    fn render_commit_file_tree(
        &self,
        state: &CommitTabState,
        loaded: &LoadedCommitDiff,
        commit_sha: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entries = changed_file_entries(loaded, GitPanelSettings::get_global(cx).diff_stats);
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
        // Resolved once for the whole list rather than per row: the mark is
        // keyed by path, so it survives the tab's state being rebuilt on the
        // next commit selection, and it is only ever *read* here — never fed
        // back into which diff opens.
        let open_file = self.open_commit_file(&commit_sha).cloned();
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
                    move |range, _window, cx| {
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
                                        ChangedFileRowMarks {
                                            cursor: selected_file.as_ref()
                                                == Some(&entry.repo_path),
                                            open_in_pane: open_file.as_ref()
                                                == Some(&entry.repo_path),
                                        },
                                        cx,
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
    use gpui::{UpdateGlobal as _, VisualTestContext};

    #[test]
    fn test_commit_tab_paints_files_above_message() {
        let position = |wanted: CommitTabSection| {
            COMMIT_TAB_SECTIONS
                .iter()
                .position(|section| *section == wanted)
                .expect("every section must be painted exactly once")
        };

        assert_eq!(
            COMMIT_TAB_SECTIONS.len(),
            5,
            "a further section would need its own place in the rule's ordering"
        );
        assert_eq!(
            position(CommitTabSection::Message),
            position(CommitTabSection::Diff) + 1,
            "the changed-files tree must be painted above the commit message, \
             mirroring the Changes tab, and directly above it: \
             `message_carries_rule` in `render_commit_tab` hangs the tab's one \
             horizontal rule — and the resize handle on it — off the message, \
             and a divider only divides the two regions it sits between"
        );
        assert_eq!(
            position(CommitTabSection::Branches),
            COMMIT_TAB_SECTIONS.len() - 1,
            "the containing-branches line is metadata of the same class as the \
             identity row the message section ends with, and IDEA puts it under \
             that row; it is also one of the two sections that can render \
             nothing, which is only free at the bottom"
        );
        assert_eq!(
            position(CommitTabSection::Refs),
            position(CommitTabSection::Message) + 1,
            "the ref chips lead the metadata block: they name what the commit \
             *is*, where the two rows under them describe what it is reachable \
             from, and they are the row the user reads against the graph"
        );
        assert_eq!(
            position(CommitTabSection::Tags),
            position(CommitTabSection::Branches) - 1,
            "IDEA paints the tag row between the identity line and the \
             containing-branches line, and the tag row is the other section \
             that can render nothing — above the branches line it still costs \
             no rule and no gap when it does"
        );
    }

    /// The message block as the flex pass last painted it, with its bottom
    /// edge — the one the drag measures from — pinned at y=220.
    fn painted_message_bounds(height: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(0.), px(220.) - px(height)),
            size: size(px(300.), px(height)),
        }
    }

    #[test]
    fn test_dragged_commit_message_height_is_measured_from_the_bottom_edge() {
        let bounds = painted_message_bounds(120.);

        assert_eq!(
            dragged_commit_message_height(bounds, px(150.)),
            px(70.),
            "the height is the gap between the cursor and the block's bottom \
             edge, which the drag does not move — measuring from the top edge \
             would chase the value being changed"
        );

        assert_eq!(
            dragged_commit_message_height(bounds, px(-1000.)),
            px(COMMIT_MESSAGE_DRAG_MAX_HEIGHT),
            "dragging off the top of the window parks a pathological number in \
             the panel's serialized state unless the ceiling stops it"
        );
        assert_eq!(
            dragged_commit_message_height(bounds, px(1000.)),
            px(COMMIT_MESSAGE_MIN_HEIGHT),
            "dragging past the bottom leaves the floor, so the tab never loses \
             the line saying which commit it is describing"
        );
    }

    #[test]
    fn test_clamp_commit_message_height_guards_the_deserialized_value() {
        assert_eq!(
            clamp_commit_message_height(px(180.)),
            px(180.),
            "a height inside the range survives a restart unchanged"
        );
        assert_eq!(
            clamp_commit_message_height(px(0.)),
            px(COMMIT_MESSAGE_MIN_HEIGHT)
        );
        assert_eq!(
            clamp_commit_message_height(px(100_000.)),
            px(COMMIT_MESSAGE_DRAG_MAX_HEIGHT),
            "the KVP row is hand-editable, so a nonsense height must not reach \
             the layout"
        );
    }

    #[test]
    fn test_double_clicking_the_divider_restores_the_automatic_layout() {
        assert_eq!(
            commit_message_height_after_click(1, Some(px(180.))),
            Some(px(180.)),
            "a single click is how every grab of the handle ends; resetting on \
             one would destroy the split the user just dragged"
        );
        assert_eq!(
            commit_message_height_after_click(2, Some(px(180.))),
            None,
            "a double click hands the block back to the flex pass"
        );
        assert_eq!(
            commit_message_height_after_click(3, Some(px(180.))),
            None,
            "a triple click is still a double click that kept going"
        );
    }

    #[test]
    fn test_format_branches_containing() {
        assert!(
            format_branches_containing(&[], false).is_none(),
            "a commit on no branch — which is also every commit on a remote \
             repository — renders no line at all, never `In 0 branches:`"
        );

        let single = format_branches_containing(&["main".into()], false)
            .expect("one branch still gets a line");
        assert_eq!(single.line.as_ref(), "In 1 branch: main");
        assert_eq!(
            single.toggle, None,
            "nothing is hidden, so there is nothing to expand"
        );

        let at_threshold: Vec<SharedString> = (0..MAX_LISTED_BRANCHES)
            .map(|index| SharedString::from(format!("b{index}")))
            .collect();
        let at_threshold = format_branches_containing(&at_threshold, false)
            .expect("the threshold itself yields a line");
        assert_eq!(
            at_threshold.line.as_ref(),
            "In 5 branches: b0, b1, b2, b3, b4"
        );
        assert_eq!(
            at_threshold.toggle, None,
            "a list that exactly fits is not truncated, so it gets no toggle"
        );

        let many: Vec<SharedString> = (0..8)
            .map(|index| SharedString::from(format!("b{index}")))
            .collect();
        let collapsed =
            format_branches_containing(&many, false).expect("a long list still yields a line");
        assert_eq!(
            collapsed.line.as_ref(),
            "In 8 branches: b0, b1, b2, b3, b4",
            "a commit on a busy branch must not spell out every branch name, \
             but the count stays the total"
        );
        assert_eq!(
            collapsed.toggle.as_deref(),
            Some("Show all"),
            "the hidden branches have to be reachable — the tail is a button, \
             not the `and N more` text the git graph's version printed"
        );

        let expanded = format_branches_containing(&many, true).expect("expanding keeps the line");
        assert_eq!(
            expanded.line.as_ref(),
            "In 8 branches: b0, b1, b2, b3, b4, b5, b6, b7"
        );
        assert_eq!(
            expanded.toggle.as_deref(),
            Some("Show less"),
            "expanding must offer a way back"
        );
    }

    #[test]
    fn test_format_tags_pointing_at() {
        assert!(
            format_tags_pointing_at(&[], false).is_none(),
            "an untagged commit — which is most of them, and also every commit \
             on a remote repository — renders no row at all, not a bare tag icon"
        );

        // The maintainer's screenshot: `Release 2.9.16` shows `2.9.16` alone,
        // in a repo that has tagged plenty of releases since.
        let single =
            format_tags_pointing_at(&["2.9.16".into()], false).expect("one tag still gets a row");
        assert_eq!(
            single.line.as_ref(),
            "2.9.16",
            "the row is the tag name and nothing else: it is labelled by the \
             tag icon, not by `In 1 tag:` prose"
        );
        assert_eq!(
            single.toggle, None,
            "nothing is hidden, so there is nothing to expand"
        );

        // "And there can be more than one" — a monorepo release commit.
        let several = format_tags_pointing_at(&["pkg-a@1.2.3".into(), "pkg-b@4.5.6".into()], false)
            .expect("several tags still fit on one row");
        assert_eq!(several.line.as_ref(), "pkg-a@1.2.3, pkg-b@4.5.6");
        assert_eq!(several.toggle, None);

        let at_threshold: Vec<SharedString> = (0..MAX_LISTED_TAGS)
            .map(|index| SharedString::from(format!("v{index}")))
            .collect();
        let at_threshold = format_tags_pointing_at(&at_threshold, false)
            .expect("the threshold itself yields a row");
        assert_eq!(at_threshold.line.as_ref(), "v0, v1, v2, v3, v4");
        assert_eq!(
            at_threshold.toggle, None,
            "a list that exactly fits is not truncated, so it gets no toggle"
        );

        // The cap fires rarely now that the row lists only the commit's own
        // tags, but a release commit in a monorepo really does carry one tag
        // per published package.
        let many: Vec<SharedString> = (0..8)
            .map(|index| SharedString::from(format!("v{index}")))
            .collect();
        let collapsed =
            format_tags_pointing_at(&many, false).expect("a long list still yields a row");
        assert_eq!(
            collapsed.line.as_ref(),
            "v0, v1, v2, v3, v4",
            "the row must not push the changed-files tree down with a dozen \
             package tags"
        );
        assert_eq!(
            collapsed.toggle.as_deref(),
            Some("Show all"),
            "the row carries no count, so the toggle is the only thing telling \
             the user that names are hidden — and the only way to reach them"
        );

        let expanded = format_tags_pointing_at(&many, true).expect("expanding keeps the row");
        assert_eq!(expanded.line.as_ref(), "v0, v1, v2, v3, v4, v5, v6, v7");
        assert_eq!(
            expanded.toggle.as_deref(),
            Some("Show less"),
            "expanding must offer a way back"
        );
    }

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

    fn commit_file(
        path: &str,
        old_text: Option<&str>,
        new_text: Option<&str>,
        is_binary: bool,
    ) -> CommitFile {
        CommitFile {
            path: RepoPath::new(path).expect("valid repo path"),
            old_text: old_text.map(str::to_string),
            new_text: new_text.map(str::to_string),
            is_binary,
        }
    }

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).expect("valid repo path")
    }

    fn changed_file_entry(path: &str) -> ChangedFileEntry {
        ChangedFileEntry::from_commit_file(
            &commit_file(path, Some("old"), Some("new"), false),
            None,
        )
    }

    fn stat_of(stats: &CommitDiffStats, path: &str) -> Option<DiffLineCount> {
        stats
            .per_file
            .get(&RepoPath::new(path).expect("valid repo path"))
            .copied()
    }

    fn line_count(added: usize, removed: usize) -> Option<DiffLineCount> {
        Some(DiffLineCount { added, removed })
    }

    /// The header total and every row's figures come out of one pass over the
    /// commit, so this pins both halves and the identity between them.
    #[test]
    fn test_compute_diff_stats_counts_each_file_and_sums_to_the_total() {
        let diff = CommitDiff {
            files: vec![
                // Replaces a line and appends one: both columns non-zero.
                commit_file(
                    "src/mixed.rs",
                    Some("one\ntwo\nthree\n"),
                    Some("one\nTWO\nthree\nfour\n"),
                    false,
                ),
                // A modification that only adds.
                commit_file("src/appended.rs", Some("a\n"), Some("a\nb\n"), false),
                // A file the commit added: no old text at all.
                commit_file("src/added.rs", None, Some("x\ny\n"), false),
                // A file the commit deleted: no new text at all.
                commit_file("src/deleted.rs", Some("p\nq\nr\n"), None, false),
                // What `load_commit_diff` hands us for a binary file: the
                // status is real, the texts are empty stand-ins.
                commit_file("assets/icon.png", Some(""), Some(""), true),
            ],
        };

        let stats = compute_diff_stats(&diff);

        assert_eq!(stat_of(&stats, "src/mixed.rs"), line_count(2, 1));
        assert_eq!(stat_of(&stats, "src/appended.rs"), line_count(1, 0));
        assert_eq!(
            stat_of(&stats, "src/added.rs"),
            line_count(2, 0),
            "an added file is all additions, the way `git --numstat` reports it"
        );
        assert_eq!(
            stat_of(&stats, "src/deleted.rs"),
            line_count(0, 3),
            "a deleted file is all removals"
        );
        assert_eq!(
            stat_of(&stats, "assets/icon.png"),
            None,
            "a binary file has no line count at all — `+0 −0` would be a \
             truthful-looking lie about a file whose texts we never had"
        );

        let summed =
            stats
                .per_file
                .values()
                .fold(DiffLineCount::default(), |mut sum, file_count| {
                    sum.added += file_count.added;
                    sum.removed += file_count.removed;
                    sum
                });
        assert_eq!(
            stats.total, summed,
            "the header's total must be exactly what the rows add up to: \
             double-counting a file, or dropping one, shows up only here"
        );
        assert_eq!(
            stats.total,
            DiffLineCount {
                added: 5,
                removed: 4
            }
        );
    }

    /// The join between the map and the rows. A key that stops matching turns
    /// every row's figures off at once and nothing else fails.
    #[test]
    fn test_changed_file_entries_carry_their_figures() {
        let loaded = LoadedCommitDiff {
            stats: compute_diff_stats(&CommitDiff {
                files: vec![
                    commit_file("src/lib.rs", Some("a\n"), Some("a\nb\n"), false),
                    commit_file("assets/icon.png", Some(""), Some(""), true),
                ],
            }),
            diff: CommitDiff {
                files: vec![
                    commit_file("src/lib.rs", Some("a\n"), Some("a\nb\n"), false),
                    commit_file("assets/icon.png", Some(""), Some(""), true),
                ],
            },
        };

        let entries = changed_file_entries(&loaded, true);
        assert_eq!(
            entries.iter().map(|entry| entry.stat).collect::<Vec<_>>(),
            vec![line_count(1, 0), None],
            "each row is paired with the figures computed for its own path, \
             and a binary row gets none"
        );

        let entries = changed_file_entries(&loaded, false);
        assert_eq!(
            entries.iter().map(|entry| entry.stat).collect::<Vec<_>>(),
            vec![None, None],
            "`git_panel.diff_stats` off drops the figures from every row"
        );
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

    /// Drives `ChangedFileEntry::handle_row_click` — the diff half of a Commit
    /// tab file row's click handler — against a real workspace pane.
    ///
    /// The row's `on_click` closure itself cannot be driven here: it is built
    /// from the panel's weak handle inside `GitPanel::render` and may only be
    /// invoked from an event callback, so a test that called it during a
    /// `VisualTestContext` draw would prove nothing about the real lease
    /// anyway. Everything below the selection call is `handle_row_click`.
    struct CommitTabClickHarness {
        workspace: Entity<Workspace>,
        repository: WeakEntity<Repository>,
        sha: SharedString,
    }

    impl CommitTabClickHarness {
        fn click(&self, entry: &ChangedFileEntry, count: usize, cx: &mut VisualTestContext) {
            let workspace = self.workspace.downgrade();
            let repository = self.repository.clone();
            let sha = self.sha.clone();
            let entry = entry.clone();
            cx.update(move |window, cx| {
                entry.handle_row_click(count, &sha, &repository, &workspace, window, cx);
            });
            cx.run_until_parked();
        }

        /// Every single-file diff open in the workspace, as
        /// `(commit sha or `None` for a working-tree diff, path)`.
        fn open_diffs(&self, cx: &mut VisualTestContext) -> Vec<(Option<SharedString>, RepoPath)> {
            self.workspace.update_in(cx, |workspace, _window, cx| {
                workspace
                    .items_of_type::<SoloDiffView>(cx)
                    .map(|view| {
                        let source = view.read(cx).source();
                        (source.sha().cloned(), source.repo_path().clone())
                    })
                    .collect()
            })
        }

        /// The files the workspace's open *commit* diffs are showing.
        fn open_commit_diffs(&self, cx: &mut VisualTestContext) -> Vec<RepoPath> {
            self.open_diffs(cx)
                .into_iter()
                .filter_map(|(sha, path)| sha.map(|_| path))
                .collect()
        }

        /// Drive the *Changes* tab's half of the shared diff tab: `Summon`
        /// stands for a double click on a working-tree file, `Retarget` for a
        /// single one. The Changes tab's own plumbing is exercised in
        /// `git_panel`'s tests; here it only has to reach the same pane slot
        /// the Commit tab's clicks reach.
        async fn changes_click(&self, path: &str, mode: DiffOpen, cx: &mut VisualTestContext) {
            let entry = GitStatusEntry {
                repo_path: repo_path(path),
                status: FileStatus::Tracked(TrackedStatus {
                    index_status: StatusCode::Unmodified,
                    worktree_status: StatusCode::Modified,
                }),
                staging: StageStatus::Unstaged,
                diff_stat: None,
            };
            let repository = self
                .repository
                .upgrade()
                .expect("the fixture holds the repository alive");
            let workspace = self.workspace.downgrade();
            let open = cx.update(|window, cx| {
                SoloDiffView::open_or_focus(entry, repository, workspace, mode, window, cx)
            });
            open.await.expect("the Changes-tab gesture resolves");
            cx.run_until_parked();
        }
    }

    /// A one-repository workspace with its git repository already resolved —
    /// the setup both the click harness and the Commit tab's panel-level tests
    /// need before they diverge.
    async fn commit_tab_test_workspace(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<Workspace>, Entity<Repository>, VisualTestContext) {
        crate::git_panel::tests::init_test(cx);

        let fs = project::FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            util::path!("/project"),
            serde_json::json!({
                ".git": {},
                "a.rs": "a\n",
                "b.rs": "b\n",
            }),
        )
        .await;

        let project = Project::test(
            fs.clone(),
            [std::path::Path::new(util::path!("/project"))],
            cx,
        )
        .await;
        let window_handle = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
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

        (workspace, repository, cx)
    }

    async fn commit_tab_click_harness(
        cx: &mut gpui::TestAppContext,
    ) -> (CommitTabClickHarness, VisualTestContext) {
        let (workspace, repository, cx) = commit_tab_test_workspace(cx).await;
        // A full sha: it is forwarded verbatim to git, which is also how the
        // commit's files are registered below.
        let sha = "0123456789abcdef0123456789abcdef01234567";
        // Unlike the old `CommitView` path, opening a commit's file now fails
        // when the commit does not contain it — so the fake repository has to
        // actually have the commit.
        let fs = workspace.read_with(&cx, |workspace, cx| {
            fs::Fs::as_fake(workspace.project().read(cx).fs().as_ref())
        });
        fs.set_commit_diff(
            std::path::Path::new(util::path!("/project/.git")),
            sha,
            CommitDiff {
                files: ["a.rs", "b.rs"]
                    .into_iter()
                    .map(|path| CommitFile {
                        path: repo_path(path),
                        old_text: Some(format!("old {path}\n")),
                        new_text: Some(format!("new {path}\n")),
                        is_binary: false,
                    })
                    .collect(),
            },
        );
        (
            CommitTabClickHarness {
                workspace,
                repository: repository.downgrade(),
                sha: sha.into(),
            },
            cx,
        )
    }

    /// A git panel over [`commit_tab_test_workspace`] that is a *real dock
    /// panel*, so that what the Commit tab paints reaches
    /// [`VisualTestContext::debug_bounds`]. A panel that is merely constructed
    /// is never drawn and registers no bounds at all, which would let every
    /// paint assertion below pass for the wrong reason.
    async fn commit_tab_painted_panel(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GitPanel>,
        Entity<Repository>,
        Arc<project::FakeFs>,
        VisualTestContext,
    ) {
        let (workspace, repository, mut cx) = commit_tab_test_workspace(cx).await;
        let fs = workspace.read_with(&cx, |workspace, cx| {
            fs::Fs::as_fake(workspace.project().read(cx).fs().as_ref())
        });
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = GitPanel::new(workspace, window, cx);
            workspace.add_panel(panel.clone(), window, cx);
            workspace.open_panel::<GitPanel>(window, cx);
            panel
        });
        cx.run_until_parked();
        (panel, repository, fs, cx)
    }

    /// A git panel over [`commit_tab_test_workspace`], for the tab's
    /// load-lifecycle tests.
    async fn commit_tab_panel(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<GitPanel>,
        Entity<Repository>,
        Arc<project::FakeFs>,
        VisualTestContext,
    ) {
        let (workspace, repository, mut cx) = commit_tab_test_workspace(cx).await;
        let fs = workspace.read_with(&cx, |workspace, cx| {
            fs::Fs::as_fake(workspace.project().read(cx).fs().as_ref())
        });
        let panel = workspace.update_in(&mut cx, GitPanel::new);
        (panel, repository, fs, cx)
    }

    /// The gap this row closes: the graph labelled the selected row
    /// `origin/hotfix/2.41.1` and the detail pane beside it said nothing about
    /// any ref at all.
    ///
    /// Asserted against the painted tree rather than against
    /// `state.selection.refs`, which would pass just as happily with the row
    /// never rendered.
    #[gpui::test]
    async fn test_the_commit_tab_paints_the_refs_pointing_at_the_commit(
        cx: &mut gpui::TestAppContext,
    ) {
        let (panel, repository, _fs, mut cx) = commit_tab_painted_panel(cx).await;
        let cx = &mut cx;
        cx.update(|_window, cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .git
                        .get_or_insert_default()
                        .log
                        .get_or_insert_default()
                        .compact_refs_threshold = Some(2);
                });
            });
        });

        let decorated: Oid = "823a3f8a".parse().expect("valid abbreviated sha");
        let bare: Oid = "1a2b3c4d".parse().expect("valid abbreviated sha");

        cx.update_window_entity(&panel, |panel, window, cx| {
            panel.show_commit_selection(
                CommitSelection {
                    repository: repository.clone(),
                    shas: vec![decorated],
                    refs: CommitRefs {
                        names: vec![
                            "HEAD -> main".into(),
                            "origin/main".into(),
                            "tag: 2.41.0".into(),
                        ],
                        accent_idx: 0,
                    },
                },
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("COMMIT-TAB-REFS").is_some(),
            "a commit something points at must say so in the detail pane"
        );
        assert!(
            cx.debug_bounds("CHIP-HEAD -> main").is_some()
                && cx.debug_bounds("CHIP-origin/main").is_some(),
            "and it says so with the graph's own chips, verbatim — which is \
             what distinguishes the local branch from the remote one"
        );
        assert!(
            cx.debug_bounds("CHIP-tag: 2.41.0").is_none() && cx.debug_bounds("CHIP-+1").is_some(),
            "past `git.log.compact_refs_threshold` the rest collapses into the \
             graph's `+N` chip instead of widening the row"
        );

        cx.update_window_entity(&panel, |panel, window, cx| {
            panel.show_commit_selection(
                CommitSelection {
                    repository: repository.clone(),
                    shas: vec![bare],
                    refs: CommitRefs::default(),
                },
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("COMMIT-TAB-REFS").is_none(),
            "a commit no ref points at gets no row at all — an empty one would \
             still spend its padding out of the changed-files tree's budget"
        );
        assert!(
            cx.debug_bounds("CHIP-HEAD -> main").is_none(),
            "and the previous commit's chips go with it"
        );
    }

    /// The chip row and the tag row answer the same question from different
    /// sources. Whatever the chips already name must not be repeated.
    #[test]
    fn test_the_tag_row_drops_the_tags_the_chips_already_name() {
        let refs: Vec<SharedString> = vec!["origin/main".into(), "tag: 2.41.0".into()];
        assert_eq!(
            uncharted_tags(&["2.41.0".into(), "2.41.1".into()], &refs),
            vec![SharedString::from("2.41.1")],
            "the tag the chips show is dropped; the one they are too old to \
             know about is exactly what the row is still for"
        );
        assert_eq!(
            uncharted_tags(&["2.41.0".into()], &refs),
            Vec::<SharedString>::new(),
            "and when the chips name every tag the row has nothing left to say"
        );
        assert_eq!(
            uncharted_tags(&["2.41.0".into()], &[]),
            vec![SharedString::from("2.41.0")],
            "with no decorations to hand the row is the only thing showing the \
             tag, so it keeps it"
        );
    }

    /// Branches and tags load in one task, so the staleness re-check that task
    /// makes has to cover both: a response that resolved after the selection
    /// moved on must land on neither row, not on the row whose assignment
    /// happens to come first.
    ///
    /// The debounce widens the window in which the selection can move under an
    /// in-flight query, so this matters more here than for the tab's two
    /// undebounced loads, not less.
    #[gpui::test]
    async fn test_a_stale_containment_load_lands_on_neither_row(cx: &mut gpui::TestAppContext) {
        let (panel, repository, _fs, mut cx) = commit_tab_panel(cx).await;
        let cx = &mut cx;

        let first: Oid = "823a3f8a".parse().expect("valid abbreviated sha");
        let second: Oid = "1a2b3c4d".parse().expect("valid abbreviated sha");

        cx.update_window_entity(&panel, |panel, window, cx| {
            panel.show_commit_selection(
                CommitSelection {
                    repository: repository.clone(),
                    shas: vec![first],
                    refs: Default::default(),
                },
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        cx.update_window_entity(&panel, |panel, _window, _cx| {
            panel
                .commit_tab
                .as_mut()
                .expect("the tab is open")
                .selection
                .shas = vec![second];
        });
        cx.executor().advance_clock(BRANCHES_CONTAINING_DEBOUNCE);
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let state = panel.commit_tab.as_ref().expect("the tab is open");
            assert!(
                matches!(state.branches, LoadState::Loading),
                "the first commit's branches were pasted onto the second"
            );
            assert!(
                matches!(state.tags, LoadState::Loading),
                "the first commit's tags were pasted onto the second"
            );
        });
    }

    /// The tag row must ask for the tags *on* the commit. The fake answers
    /// [`GitRepository::tags_pointing_at`] from seeded state and leaves
    /// `tags_containing` on the trait's empty default, so a loader that asked
    /// the containment question would leave the row empty.
    #[gpui::test]
    async fn test_the_tag_row_loads_the_tags_pointing_at_the_commit(cx: &mut gpui::TestAppContext) {
        let (panel, repository, fs, mut cx) = commit_tab_panel(cx).await;
        let cx = &mut cx;

        let sha: Oid = "823a3f8a".parse().expect("valid abbreviated sha");
        fs.with_git_state(util::path!("/project/.git").as_ref(), true, |state| {
            state.tags_pointing_at.insert(
                sha.to_string(),
                vec!["pkg-a@1.2.3".into(), "pkg-b@4.5.6".into()],
            );
        })
        .expect("the fake project has a git repository");

        cx.update_window_entity(&panel, |panel, window, cx| {
            panel.show_commit_selection(
                CommitSelection {
                    repository: repository.clone(),
                    shas: vec![sha],
                    refs: Default::default(),
                },
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        cx.executor().advance_clock(BRANCHES_CONTAINING_DEBOUNCE);
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let state = panel.commit_tab.as_ref().expect("the tab is open");
            let LoadState::Loaded(tags) = &state.tags else {
                panic!("the tag row should have loaded by now");
            };
            assert_eq!(
                tags.as_slice(),
                &[
                    SharedString::from("pkg-a@1.2.3"),
                    SharedString::from("pkg-b@4.5.6"),
                ],
                "every tag on the commit reaches the row, and it is the \
                 points-at query that supplied them"
            );
        });
    }

    #[gpui::test]
    async fn test_single_click_retargets_the_shared_diff_tab(cx: &mut gpui::TestAppContext) {
        let (harness, mut cx) = commit_tab_click_harness(cx).await;
        let a = changed_file_entry("a.rs");
        let b = changed_file_entry("b.rs");

        harness.click(&a, 2, &mut cx);
        assert_eq!(
            harness.open_commit_diffs(&mut cx),
            vec![a.repo_path.clone()],
            "a double click summons the shared diff tab, showing the clicked file"
        );

        harness.click(&b, 1, &mut cx);
        assert_eq!(
            harness.open_commit_diffs(&mut cx),
            vec![b.repo_path.clone()],
            "a single click retargets that same tab instead of opening a second one"
        );
    }

    #[gpui::test]
    async fn test_single_click_opens_nothing_when_no_diff_tab_is_showing(
        cx: &mut gpui::TestAppContext,
    ) {
        let (harness, mut cx) = commit_tab_click_harness(cx).await;
        let a = changed_file_entry("a.rs");

        harness.click(&a, 1, &mut cx);
        assert_eq!(
            harness.open_commit_diffs(&mut cx),
            Vec::<RepoPath>::new(),
            "a single click never summons a diff from nothing"
        );
    }

    /// With previews turned off there is no shared slot to retarget, so the
    /// gestures fall back to what the tab did before: double click opens a
    /// permanent tab, single click only selects. Doing nothing at all would
    /// leave the tab with no way to open a diff.
    #[gpui::test]
    async fn test_previews_disabled_falls_back_to_permanent_tabs(cx: &mut gpui::TestAppContext) {
        let (harness, mut cx) = commit_tab_click_harness(cx).await;
        cx.update(|_window, cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.preview_tabs.get_or_insert_default().enabled = Some(false);
                });
            });
        });

        let a = changed_file_entry("a.rs");
        let b = changed_file_entry("b.rs");

        harness.click(&a, 2, &mut cx);
        assert_eq!(
            harness.open_commit_diffs(&mut cx),
            vec![a.repo_path.clone()],
            "double click still opens the file's diff with previews off"
        );

        harness.click(&b, 1, &mut cx);
        assert_eq!(
            harness.open_commit_diffs(&mut cx),
            vec![a.repo_path.clone()],
            "with no preview slot to retarget, a single click only selects"
        );

        harness.click(&b, 2, &mut cx);
        assert_eq!(
            harness.open_commit_diffs(&mut cx),
            vec![a.repo_path.clone(), b.repo_path.clone()],
            "each double click gets its own permanent tab — the pre-preview behaviour"
        );
    }

    /// Regression guard for the tab-identity bug this path used to carry: the
    /// "which item do I replace" lookup keyed off the sha alone, so re-opening
    /// a whole-commit view closed whichever tab for that commit came first —
    /// including a single-file diff sitting to its left.
    ///
    /// A single file's diff is now a `SoloDiffView`, so the lookup cannot
    /// reach it at all; what this pins is that re-opening the whole-commit
    /// view still replaces *itself* and still leaves the file diff — and its
    /// hold on the pane's preview slot — alone.
    #[gpui::test]
    async fn test_reopening_the_commit_view_spares_the_file_diff_tab(
        cx: &mut gpui::TestAppContext,
    ) {
        let (harness, mut cx) = commit_tab_click_harness(cx).await;
        let a = changed_file_entry("a.rs");
        harness.click(&a, 2, &mut cx);

        for _ in 0..2 {
            let workspace = harness.workspace.downgrade();
            let repository = harness.repository.clone();
            let sha = harness.sha.to_string();
            cx.update(move |window, cx| {
                CommitView::open(sha, repository, workspace, None, None, window, cx);
            });
            cx.run_until_parked();
        }

        let commit_views = harness
            .workspace
            .update_in(&mut cx, |workspace, _window, cx| {
                workspace.items_of_type::<CommitView>(cx).count()
            });
        assert_eq!(
            commit_views, 1,
            "re-opening the whole-commit view replaces itself"
        );
        assert_eq!(
            harness.open_commit_diffs(&mut cx),
            vec![a.repo_path.clone()],
            "and leaves the single-file diff tab standing"
        );
        harness
            .workspace
            .update_in(&mut cx, |workspace, _window, cx| {
                let preview = workspace.active_pane().read(cx).preview_item_id();
                let diff = workspace
                    .items_of_type::<SoloDiffView>(cx)
                    .next()
                    .expect("the file diff is open");
                assert_eq!(
                    preview,
                    Some(diff.entity_id()),
                    "the file diff also keeps the preview slot, so the next \
                     single click still retargets it"
                );
            });
    }

    #[gpui::test]
    async fn test_repeated_double_clicks_reuse_the_shared_diff_tab(cx: &mut gpui::TestAppContext) {
        let (harness, mut cx) = commit_tab_click_harness(cx).await;
        let a = changed_file_entry("a.rs");
        let b = changed_file_entry("b.rs");

        harness.click(&a, 2, &mut cx);
        harness.click(&b, 2, &mut cx);
        assert_eq!(
            harness.open_commit_diffs(&mut cx),
            vec![b.repo_path.clone()],
            "double clicking a second file replaces the preview rather than \
             pinning the first tab and opening a second"
        );
    }

    /// The maintainer's model is *one* shared diff tab across both git-panel
    /// tabs, not one per tab: «двойной клик на любом файле в changes и в commit
    /// её открывает. Дальше любой клик на файле в changes и в commit меняет
    /// содержимое вкладки.» Before the two views were unified each tab could
    /// only retarget its own item type, so this could not hold.
    #[gpui::test]
    async fn test_the_shared_diff_tab_is_shared_across_both_tabs(cx: &mut gpui::TestAppContext) {
        let (harness, mut cx) = commit_tab_click_harness(cx).await;
        let a = changed_file_entry("a.rs");

        harness
            .changes_click("b.rs", DiffOpen::Summon { focus: false }, &mut cx)
            .await;
        assert_eq!(
            harness.open_diffs(&mut cx),
            vec![(None, repo_path("b.rs"))],
            "a Changes-tab double click summons the shared diff"
        );

        harness.click(&a, 1, &mut cx);
        assert_eq!(
            harness.open_diffs(&mut cx),
            vec![(Some(harness.sha.clone()), a.repo_path.clone())],
            "a single click in the Commit tab retargets the diff the Changes \
             tab opened, rather than opening a second tab"
        );

        harness
            .changes_click("b.rs", DiffOpen::Retarget, &mut cx)
            .await;
        assert_eq!(
            harness.open_diffs(&mut cx),
            vec![(None, repo_path("b.rs"))],
            "and a single click in the Changes tab retargets the diff the \
             Commit tab opened"
        );
    }
}
