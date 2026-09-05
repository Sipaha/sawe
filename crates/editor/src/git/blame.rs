use crate::display_map::DisplaySnapshot;
use crate::git::blame_colors::ColorMode;
use crate::git::blame_filters::AuthorFilter;
use crate::{DisplayRow, Editor};
use anyhow::{Context as _, Result};
use collections::HashMap;

use git::{
    GitHostingProviderRegistry, Oid,
    blame::{Blame, BlameEntry},
    commit::ParsedCommitMessage,
    repository::RepoPath,
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Hsla, ScrollHandle, SharedString,
    Subscription, Task, TextStyle, WeakEntity, Window,
};
use itertools::Itertools;
use language::{Bias, BufferSnapshot, Edit};
use markdown::Markdown;
use multi_buffer::{MultiBuffer, RowInfo};
use project::{
    Project, ProjectItem as _,
    git_store::{GitStoreEvent, Repository},
};
use smallvec::SmallVec;
use std::{sync::Arc, time::Duration};
use sum_tree::SumTree;
use text::BufferId;
use workspace::Workspace;

/// S-ANN — toolbar / persistence state controlling how the blame gutter
/// renders and which lines are emphasized. Toggles for `ignore_whitespace`
/// and `follow_renames` are honored by the `editor.git.blame` MCP tool;
/// the editor-side gutter pipeline goes through
/// [`project::Project::blame_buffer`] which doesn't yet plumb these
/// flags down to the trait method (would require touching the
/// remote-blame proto), so v1 of the toolbar tracks the state and
/// re-runs `generate()` for parity with the MCP path. See `S-ANN`
/// follow-ups.
#[derive(Clone, Debug, Default)]
pub struct BlameOptions {
    pub ignore_whitespace: bool,
    pub follow_renames: bool,
    pub color_mode: ColorMode,
    pub author_filter: AuthorFilter,
}

/// Tells [`GitBlame`] how to annotate a buffer that is not a project file.
///
/// The left-hand pane of a split diff is built from
/// `BufferDiff::base_text_buffer()`, a detached in-memory buffer with no
/// `File`, so `GitStore::repository_and_path_for_buffer_id` cannot find a
/// repository for it. Rather than faking a `File` on that buffer — which would
/// make unrelated code treat it as a real project file — the owner of the
/// editor registers this side-channel entry, which also carries the revision
/// the base text was taken from. Blaming HEAD would be wrong whenever the diff
/// base is not HEAD.
#[derive(Clone, Debug)]
pub struct BlameBaseSource {
    pub repository: Entity<Repository>,
    pub repo_path: RepoPath,
    pub revision: SharedString,
}

#[derive(Clone, Debug, Default)]
pub struct GitBlameEntry {
    pub rows: u32,
    pub blame: Option<BlameEntry>,
}

#[derive(Clone, Debug, Default)]
pub struct GitBlameEntrySummary {
    rows: u32,
}

impl sum_tree::Item for GitBlameEntry {
    type Summary = GitBlameEntrySummary;

    fn summary(&self, _cx: ()) -> Self::Summary {
        GitBlameEntrySummary { rows: self.rows }
    }
}

impl sum_tree::ContextLessSummary for GitBlameEntrySummary {
    fn zero() -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &Self) {
        self.rows += summary.rows;
    }
}

impl<'a> sum_tree::Dimension<'a, GitBlameEntrySummary> for u32 {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a GitBlameEntrySummary, _cx: ()) {
        *self += summary.rows;
    }
}

struct GitBlameBuffer {
    entries: SumTree<GitBlameEntry>,
    buffer_snapshot: BufferSnapshot,
    buffer_edits: text::Subscription<usize>,
    commit_details: HashMap<Oid, ParsedCommitMessage>,
}

pub struct GitBlame {
    project: Entity<Project>,
    multi_buffer: WeakEntity<MultiBuffer>,
    buffers: HashMap<BufferId, GitBlameBuffer>,
    base_sources: HashMap<BufferId, BlameBaseSource>,
    task: Task<Result<()>>,
    focused: bool,
    changed_while_blurred: bool,
    user_triggered: bool,
    regenerate_on_edit_task: Task<Result<()>>,
    _regenerate_subscriptions: Vec<Subscription>,
    options: BlameOptions,
    /// Bumped whenever the blame entries change, so that
    /// [`RunPredecessorCache`] can tell a still-valid answer from one taken
    /// against blame data that has since been re-generated or re-synced.
    blame_generation: usize,
    run_predecessor_cache: Option<RunPredecessorCache>,
    /// Display rows the last [`GitBlame::run_predecessor_above`] actually
    /// read: zero when the memo answered, and never more than
    /// [`MAX_RUN_PREDECESSOR_LOOKBACK`] because each widening only reads the
    /// rows it has not read yet. Both are properties a test cannot see from
    /// the classification alone.
    #[cfg(test)]
    last_predecessor_scan_rows: u32,
}

/// Where a blamed gutter row sits inside a run of consecutive lines that came
/// from the same commit.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlameRunPosition {
    /// The row opens a run and something precedes it — another commit's run,
    /// a block row, an unblamed line. It identifies the commit, and the
    /// boundary above it is real enough to draw.
    Head,
    /// The row opens the first run of the display, with nothing above it at
    /// all. It identifies its commit like any head, but there is no boundary
    /// there: a line drawn on the top edge would frame the gutter rather than
    /// separate two commits.
    DocumentHead,
    /// The row continues the run opened above it.
    Continuation,
}

/// What sits immediately above the first row of a slice — the one thing a
/// slice of rows cannot see about itself, and the reason the classification
/// used to be viewport-relative: a run that starts above the visible rows has
/// to keep its label up there and leave the rows below it blank, exactly as a
/// run whose head is on screen does.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlameRunPredecessor {
    /// Nothing does: the slice starts the display.
    DisplayStart,
    /// Something a run cannot be continued from: a block row, an unblamed
    /// line, or a blamed row that a later row of the slice already severed.
    Severed,
    /// A blamed row whose run the slice's first row may continue.
    Blamed {
        buffer_id: BufferId,
        sha: Oid,
        buffer_row: u32,
    },
    /// The scan above the slice ran out of budget
    /// ([`MAX_RUN_PREDECESSOR_LOOKBACK`]) without reaching anything that
    /// settles the question, and everything it did see was a soft-wrap
    /// continuation or an alignment spacer — neither of which can start a run.
    ///
    /// Distinct from [`Self::DisplayStart`] precisely because "I could not
    /// see far enough" is not "there is nothing up there": reading it as the
    /// latter relabels a still-continuing run's first visible row as a head,
    /// redrawing the date and author mid-run and making the two panes of a
    /// split diff disagree about where the label sits.
    Unsettled,
}

/// Classifies each row as opening or continuing a run of lines that share a
/// commit, and reports what the rows *below* this slice would see above them
/// — the same [`BlameRunPredecessor`] this call takes, so the classification
/// of one slice and the classification of the rows above it compose.
///
/// Deliberately keyed on `RowInfo::buffer_row` adjacency rather than
/// `BlameEntry::range`: `blame_for_rows` hands out a clone of the same entry
/// for every line of a hunk, and `GitBlame::sync` leaves a stale `range` in
/// both halves when an edit splits an entry, so two rows carrying equal ranges
/// are not necessarily adjacent lines of one run.
///
/// `blamed_rows` is expected to be `GitBlame::blame_for_rows(rows)`, i.e. the
/// same length as `rows` and positionally aligned with it; a row with no blame
/// entry gets no position. Rows past the end of `blamed_rows` are treated as
/// unblamed rather than panicking on the index.
///
/// `alignment_rows` is aligned with `rows` too and flags the rows that belong
/// to a block the block map inserted purely to keep this pane level with its
/// companion — see [`crate::display_map::Block::is_alignment_only`] and
/// [`alignment_rows_in_range`], which derives it. A shorter slice reads as
/// "not an alignment row", which is the pre-split behaviour.
pub fn blame_run_positions(
    rows: &[RowInfo],
    blamed_rows: &[Option<(BufferId, BlameEntry)>],
    alignment_rows: &[bool],
    predecessor: BlameRunPredecessor,
) -> (Vec<Option<BlameRunPosition>>, BlameRunPredecessor) {
    // Positional alignment is the whole basis of the classification: a
    // `blamed_rows` that drifted out of step with `rows` would silently make
    // every row a `Head` again rather than fail anywhere visible.
    debug_assert_eq!(
        rows.len(),
        blamed_rows.len(),
        "blame_run_positions expects `blame_for_rows(rows)`, aligned with `rows`"
    );
    debug_assert_eq!(
        rows.len(),
        alignment_rows.len(),
        "blame_run_positions expects one alignment flag per row of `rows`"
    );

    // The previous *blamed* row's identity, or `None` when the run was broken
    // (a block row in between, or nothing above at all).
    let mut previous = match predecessor {
        BlameRunPredecessor::Blamed {
            buffer_id,
            sha,
            buffer_row,
        } => Some((buffer_id, sha, buffer_row)),
        BlameRunPredecessor::DisplayStart
        | BlameRunPredecessor::Severed
        | BlameRunPredecessor::Unsettled => None,
    };
    // Whether nothing at all has been seen yet. Soft-wrap continuations and
    // alignment spacers do not clear it: a wrap is the same line still, and a
    // spacer stands for text on the *other* side of a split diff, so letting
    // either count as "something above" would make the two panes disagree
    // about whether their first row opens the display.
    let mut nothing_above = matches!(predecessor, BlameRunPredecessor::DisplayStart);
    // Same lifetime as `nothing_above` — a wrap or a spacer leaves it standing,
    // anything else clears it — but the opposite classification: the run above
    // is unreadable rather than absent, so the first blamed row continues it
    // instead of opening a new one. Choosing a head here would be inventing a
    // run boundary out of a scan budget.
    let mut unsettled_above = matches!(predecessor, BlameRunPredecessor::Unsettled);

    let mut positions = Vec::with_capacity(rows.len());
    for (ix, info) in rows.iter().enumerate() {
        let is_alignment_row = alignment_rows.get(ix).copied().unwrap_or(false);
        let Some((buffer_id, entry)) = blamed_rows.get(ix).and_then(Option::as_ref) else {
            // A block row (excerpt header, diff-hunk controls, folded-buffer
            // header) is the one unblamed row that severs a run: it is real
            // vertical space standing for something between the two lines.
            // An alignment spacer is the exception — it stands for text on
            // the *other* side of a split diff, so severing on it would let
            // the two panes cut one commit's run in different places. A
            // soft-wrap continuation is the same line still, so it must not
            // sever either. Every other unblamed row keeps its `buffer_row`,
            // so the adjacency check below already sees the gap it leaves.
            let is_block_row = info.buffer_id.is_none() && info.wrapped_buffer_row.is_none();
            let is_wrap_row = info.wrapped_buffer_row.is_some();
            if is_block_row && !is_alignment_row {
                previous = None;
            }
            if !is_wrap_row && !(is_block_row && is_alignment_row) {
                nothing_above = false;
                unsettled_above = false;
            }
            positions.push(None);
            continue;
        };

        let position = match (previous, info.buffer_row) {
            (Some((previous_buffer, previous_sha, previous_row)), Some(row))
                if previous_buffer == *buffer_id
                    && previous_sha == entry.sha
                    && previous_row.checked_add(1) == Some(row) =>
            {
                BlameRunPosition::Continuation
            }
            _ if unsettled_above => BlameRunPosition::Continuation,
            _ if nothing_above => BlameRunPosition::DocumentHead,
            _ => BlameRunPosition::Head,
        };

        previous = info.buffer_row.map(|row| (*buffer_id, entry.sha, row));
        nothing_above = false;
        unsettled_above = false;
        positions.push(Some(position));
    }

    let trailing = match (nothing_above, previous) {
        (true, _) => BlameRunPredecessor::DisplayStart,
        (false, Some((buffer_id, sha, buffer_row))) => BlameRunPredecessor::Blamed {
            buffer_id,
            sha,
            buffer_row,
        },
        (false, None) if unsettled_above => BlameRunPredecessor::Unsettled,
        (false, None) => BlameRunPredecessor::Severed,
    };
    (positions, trailing)
}

/// The alignment flags [`blame_run_positions`] wants for `len` display rows
/// starting at `start_row`, read off the block map rather than restated by a
/// caller.
fn alignment_rows_in_range(
    snapshot: &DisplaySnapshot,
    start_row: DisplayRow,
    len: usize,
) -> Vec<bool> {
    let mut alignment_rows = vec![false; len];
    let end_row = DisplayRow(start_row.0.saturating_add(len as u32));
    for (block_row, block) in snapshot.blocks_in_range(start_row..end_row) {
        if !block.is_alignment_only() {
            continue;
        }
        // `blocks_in_range` yields a block whose *start* may sit above the
        // range, and a block spans `height()` rows from there, so neither end
        // of the span can be assumed to be in range.
        for row in block_row.0..block_row.0.saturating_add(block.height()) {
            if let Some(offset) = row.checked_sub(start_row.0)
                && let Some(flag) = alignment_rows.get_mut(offset as usize)
            {
                *flag = true;
            }
        }
    }
    alignment_rows
}

/// How far above the visible rows the search for a run's real start gives up.
///
/// The scan doubles its reach and stops at the first row that settles the
/// question, so it costs one row in the ordinary case; it only keeps widening
/// while every row above is a soft-wrap continuation or an alignment spacer,
/// neither of which settles anything. A stretch of those longer than this is
/// pathological, and giving up reads as [`BlameRunPredecessor::Unsettled`] —
/// which keeps the run going rather than inventing a boundary where the scan
/// merely ran out of budget.
///
/// This is a budget on rows *scanned in total*, not on how far above the
/// viewport the last window starts: each widening only reads the rows it has
/// not read yet (see [`GitBlame::scan_run_predecessor_above`]).
const MAX_RUN_PREDECESSOR_LOOKBACK: u32 = 1024;

/// Everything about a [`DisplaySnapshot`] that can change the answer
/// [`GitBlame::run_predecessor_above`] gives for a fixed start row, cheap
/// enough to recompute every frame.
#[derive(Copy, Clone, PartialEq, Eq)]
struct DisplayFingerprint {
    display_map_id: gpui::EntityId,
    max_display_row: u32,
    max_display_column: u32,
    edit_count: usize,
    non_text_state_update_count: usize,
    trailing_excerpt_update_count: usize,
}

impl DisplayFingerprint {
    fn of(snapshot: &DisplaySnapshot) -> Self {
        let max_point = snapshot.max_point();
        let buffer = snapshot.buffer_snapshot();
        Self {
            display_map_id: snapshot.display_map_id,
            max_display_row: max_point.row().0,
            max_display_column: max_point.column(),
            edit_count: buffer.edit_count(),
            non_text_state_update_count: buffer.non_text_state_update_count(),
            trailing_excerpt_update_count: buffer.trailing_excerpt_update_count(),
        }
    }
}

/// Memo for [`GitBlame::run_predecessor_above`]. The gutter asks the same
/// question on every frame it is shown, and in the pathological case (a huge
/// alignment spacer, or a line wrapping into hundreds of display rows) that
/// question costs a scan of up to [`MAX_RUN_PREDECESSOR_LOOKBACK`] rows.
struct RunPredecessorCache {
    start_row: DisplayRow,
    display: DisplayFingerprint,
    blame_generation: usize,
    predecessor: BlameRunPredecessor,
}

impl GitBlame {
    /// [`blame_run_positions`] for the rows a renderer is about to lay out,
    /// with the alignment flags and the run's real start read off `snapshot`
    /// rather than restated by the caller.
    ///
    /// `rows` must be `snapshot.row_infos(start_row)` truncated to its own
    /// length, which is what makes a viewport index addressable as a display
    /// row, and `blamed_rows` must be `self.blame_for_rows(rows)`.
    pub fn run_positions_in_viewport(
        &mut self,
        snapshot: &DisplaySnapshot,
        start_row: DisplayRow,
        rows: &[RowInfo],
        blamed_rows: &[Option<(BufferId, BlameEntry)>],
        cx: &mut App,
    ) -> Vec<Option<BlameRunPosition>> {
        let predecessor = self.run_predecessor_above(snapshot, start_row, cx);
        let alignment_rows = alignment_rows_in_range(snapshot, start_row, rows.len());
        blame_run_positions(rows, blamed_rows, &alignment_rows, predecessor).0
    }

    /// Classifies the rows above `start_row` — as many of them as it takes to
    /// answer what the first visible row is looking at — and hands back what
    /// they leave for it.
    ///
    /// The rows above are run through [`blame_run_positions`] rather than
    /// through a second reading of the same rule: whether a block row severs,
    /// whether a spacer or a wrap does not, and what identity survives to the
    /// next row are all decided in exactly one place.
    fn run_predecessor_above(
        &mut self,
        snapshot: &DisplaySnapshot,
        start_row: DisplayRow,
        cx: &mut App,
    ) -> BlameRunPredecessor {
        let display = DisplayFingerprint::of(snapshot);
        if let Some(cache) = self.run_predecessor_cache.as_ref()
            && cache.start_row == start_row
            && cache.display == display
            && cache.blame_generation == self.blame_generation
        {
            #[cfg(test)]
            {
                self.last_predecessor_scan_rows = 0;
            }
            return cache.predecessor;
        }

        let predecessor = self.scan_run_predecessor_above(snapshot, start_row, cx);
        self.run_predecessor_cache = Some(RunPredecessorCache {
            start_row,
            display,
            blame_generation: self.blame_generation,
            predecessor,
        });
        predecessor
    }

    /// Walks upward from `start_row` in widening segments until one of them
    /// settles what the first visible row is looking at.
    ///
    /// Each segment covers only the rows not already scanned. That is sound
    /// because a segment that comes back [`BlameRunPredecessor::DisplayStart`]
    /// held nothing but wraps and spacers, and those pass whatever is above
    /// them through unchanged — so the answer for `start_row` is exactly what
    /// the newest segment settles. Restarting the whole window on every
    /// doubling (which is what this used to do) re-read the same rows up to
    /// eleven times, on every frame the gutter was shown.
    fn scan_run_predecessor_above(
        &mut self,
        snapshot: &DisplaySnapshot,
        start_row: DisplayRow,
        cx: &mut App,
    ) -> BlameRunPredecessor {
        let mut scanned = 0u32;
        let mut segment_len = 1u32;
        loop {
            let segment_end = start_row.0.saturating_sub(scanned);
            let budget = MAX_RUN_PREDECESSOR_LOOKBACK.saturating_sub(scanned);
            let count = segment_len.min(budget).min(segment_end);
            if count == 0 {
                return if segment_end == 0 {
                    BlameRunPredecessor::DisplayStart
                } else {
                    BlameRunPredecessor::Unsettled
                };
            }
            let segment_start = DisplayRow(segment_end - count);
            scanned += count;
            #[cfg(test)]
            {
                self.last_predecessor_scan_rows = scanned;
            }
            let rows = snapshot
                .row_infos(segment_start)
                .take(count as usize)
                .collect::<Vec<_>>();
            let blamed_rows = self.blame_for_rows(&rows, cx).collect::<Vec<_>>();
            let alignment_rows = alignment_rows_in_range(snapshot, segment_start, rows.len());
            // Seeded as if the scanned segment started the display: coming back
            // out the other end still saying so is precisely "these rows
            // settled nothing", which is the signal to widen.
            let (_, predecessor) = blame_run_positions(
                &rows,
                &blamed_rows,
                &alignment_rows,
                BlameRunPredecessor::DisplayStart,
            );
            if predecessor != BlameRunPredecessor::DisplayStart {
                return predecessor;
            }
            segment_len = segment_len.saturating_mul(2);
        }
    }
}

pub trait BlameRenderer {
    /// The widest author name the gutter will draw, in monospace columns.
    /// Names wider than this are truncated by the renderer, so the gutter's
    /// width reservation can clamp to it.
    fn max_author_columns(&self) -> usize;

    /// Monospace columns the gutter row spends on everything that is not the
    /// author name — the date, any avatar, the gaps between them. The
    /// renderer owns the row's layout, so it is the only thing that can
    /// answer this; the editor just adds it to the author budget.
    fn gutter_fixed_columns(&self, cx: &App) -> usize;

    fn render_blame_entry(
        &self,
        _: &TextStyle,
        _: BlameEntry,
        _: Option<ParsedCommitMessage>,
        _: Entity<Repository>,
        _: WeakEntity<Workspace>,
        _: Entity<Editor>,
        _: usize,
        _: Hsla,
        _: BlameRunPosition,
        window: &mut Window,
        _: &mut App,
    ) -> Option<AnyElement>;

    /// S-ANN — options-aware variant. The default implementation
    /// forwards to `render_blame_entry`, ignoring `options`. Renderers
    /// that opt into the annotate-toolbar pipeline override this to
    /// honor color modes / author filters / absolute-date toggles.
    #[allow(clippy::too_many_arguments)]
    fn render_blame_entry_with_options(
        &self,
        style: &TextStyle,
        blame_entry: BlameEntry,
        details: Option<ParsedCommitMessage>,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        editor: Entity<Editor>,
        ix: usize,
        sha_color: Hsla,
        run_position: BlameRunPosition,
        _options: &BlameOptions,
        _date_range: Option<(i64, i64)>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        self.render_blame_entry(
            style,
            blame_entry,
            details,
            repository,
            workspace,
            editor,
            ix,
            sha_color,
            run_position,
            window,
            cx,
        )
    }

    fn render_inline_blame_entry(
        &self,
        _: &TextStyle,
        _: BlameEntry,
        _: &mut App,
    ) -> Option<AnyElement>;

    fn render_blame_entry_popover(
        &self,
        _: BlameEntry,
        _: ScrollHandle,
        _: Option<ParsedCommitMessage>,
        _: Entity<Markdown>,
        _: Entity<Repository>,
        _: WeakEntity<Workspace>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<AnyElement>;

    fn open_blame_commit(
        &self,
        _: BlameEntry,
        _: Entity<Repository>,
        _: WeakEntity<Workspace>,
        _: &mut Window,
        _: &mut App,
    );
}

impl BlameRenderer for () {
    fn max_author_columns(&self) -> usize {
        0
    }

    fn gutter_fixed_columns(&self, _: &App) -> usize {
        0
    }

    fn render_blame_entry(
        &self,
        _: &TextStyle,
        _: BlameEntry,
        _: Option<ParsedCommitMessage>,
        _: Entity<Repository>,
        _: WeakEntity<Workspace>,
        _: Entity<Editor>,
        _: usize,
        _: Hsla,
        _: BlameRunPosition,
        _: &mut Window,
        _: &mut App,
    ) -> Option<AnyElement> {
        None
    }

    fn render_inline_blame_entry(
        &self,
        _: &TextStyle,
        _: BlameEntry,
        _: &mut App,
    ) -> Option<AnyElement> {
        None
    }

    fn render_blame_entry_popover(
        &self,
        _: BlameEntry,
        _: ScrollHandle,
        _: Option<ParsedCommitMessage>,
        _: Entity<Markdown>,
        _: Entity<Repository>,
        _: WeakEntity<Workspace>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<AnyElement> {
        None
    }

    fn open_blame_commit(
        &self,
        _: BlameEntry,
        _: Entity<Repository>,
        _: WeakEntity<Workspace>,
        _: &mut Window,
        _: &mut App,
    ) {
    }
}

pub(crate) struct GlobalBlameRenderer(pub Arc<dyn BlameRenderer>);

impl gpui::Global for GlobalBlameRenderer {}

impl GitBlame {
    pub fn new(
        multi_buffer: Entity<MultiBuffer>,
        project: Entity<Project>,
        base_sources: HashMap<BufferId, BlameBaseSource>,
        user_triggered: bool,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let multi_buffer_subscription = cx.subscribe(
            &multi_buffer,
            |git_blame, multi_buffer, event, cx| match event {
                multi_buffer::Event::DirtyChanged => {
                    if !multi_buffer.read(cx).is_dirty(cx) {
                        git_blame.generate(cx);
                    }
                }
                multi_buffer::Event::BufferRangesUpdated { .. }
                | multi_buffer::Event::BuffersEdited { .. } => git_blame.regenerate_on_edit(cx),
                _ => {}
            },
        );

        let project_subscription = cx.subscribe(&project, {
            let multi_buffer = multi_buffer.downgrade();

            move |git_blame, _, event, cx| {
                if let project::Event::WorktreeUpdatedEntries(_, updated) = event {
                    let Some(multi_buffer) = multi_buffer.upgrade() else {
                        return;
                    };
                    let project_entry_id = multi_buffer
                        .read(cx)
                        .as_singleton()
                        .and_then(|it| it.read(cx).entry_id(cx));
                    if updated
                        .iter()
                        .any(|(_, entry_id, _)| project_entry_id == Some(*entry_id))
                    {
                        log::debug!("Updated buffers. Regenerating blame data...",);
                        git_blame.generate(cx);
                    }
                }
            }
        });

        let git_store = project.read(cx).git_store().clone();
        let git_store_subscription =
            cx.subscribe(&git_store, move |this, _, event, cx| match event {
                GitStoreEvent::RepositoryUpdated(_, _, _)
                | GitStoreEvent::RepositoryAdded
                | GitStoreEvent::RepositoryRemoved(_) => {
                    log::debug!("Status of git repositories updated. Regenerating blame data...",);
                    this.generate(cx);
                }
                _ => {}
            });

        let mut this = Self {
            project,
            multi_buffer: multi_buffer.downgrade(),
            buffers: HashMap::default(),
            base_sources,
            user_triggered,
            focused,
            changed_while_blurred: false,
            task: Task::ready(Ok(())),
            regenerate_on_edit_task: Task::ready(Ok(())),
            _regenerate_subscriptions: vec![
                multi_buffer_subscription,
                project_subscription,
                git_store_subscription,
            ],
            options: BlameOptions::default(),
            blame_generation: 0,
            run_predecessor_cache: None,
            #[cfg(test)]
            last_predecessor_scan_rows: 0,
        };
        this.generate(cx);
        this
    }

    pub fn repository(&self, cx: &App, id: BufferId) -> Option<Entity<Repository>> {
        if let Some(source) = self.base_sources.get(&id) {
            return Some(source.repository.clone());
        }
        self.project
            .read(cx)
            .git_store()
            .read(cx)
            .repository_and_path_for_buffer_id(id, cx)
            .map(|(repo, _)| repo)
    }

    /// Replaces the detached-buffer blame sources (see [`BlameBaseSource`])
    /// and re-runs blame if they changed.
    pub fn set_base_sources(
        &mut self,
        base_sources: HashMap<BufferId, BlameBaseSource>,
        cx: &mut Context<Self>,
    ) {
        let unchanged = base_sources.len() == self.base_sources.len()
            && base_sources.iter().all(|(id, source)| {
                self.base_sources.get(id).is_some_and(|existing| {
                    existing.repository == source.repository
                        && existing.repo_path == source.repo_path
                        && existing.revision == source.revision
                })
            });
        if unchanged {
            return;
        }
        self.base_sources = base_sources;
        self.generate(cx);
    }

    pub fn has_generated_entries(&self) -> bool {
        !self.buffers.is_empty()
    }

    pub fn options(&self) -> &BlameOptions {
        &self.options
    }

    /// Replace the current options. When the parts that affect the
    /// underlying `git blame` invocation change (currently
    /// `ignore_whitespace` / `follow_renames`), `regenerate` is set
    /// — callers may want to call `generate(cx)` afterward to refresh.
    pub fn set_options(&mut self, options: BlameOptions, cx: &mut Context<Self>) -> bool {
        let regenerate = self.options.ignore_whitespace != options.ignore_whitespace
            || self.options.follow_renames != options.follow_renames;
        self.options = options;
        cx.notify();
        if regenerate {
            self.generate(cx);
        }
        regenerate
    }

    /// Iterator over every loaded blame entry across all buffers — used
    /// by the toolbar's author-filter dropdown to enumerate
    /// contributors.
    pub fn all_entries(&self) -> impl Iterator<Item = &BlameEntry> + '_ {
        self.buffers
            .values()
            .flat_map(|b| b.entries.iter().filter_map(|e| e.blame.as_ref()))
    }

    /// Min/max committer-time across all loaded blame entries — used by
    /// the date-heatmap color mode.
    pub fn date_range(&self) -> Option<(i64, i64)> {
        let times = self
            .all_entries()
            .filter_map(|e| e.author_time)
            .collect::<Vec<_>>();
        let min = *times.iter().min()?;
        let max = *times.iter().max()?;
        Some((min, max))
    }

    pub fn details_for_entry(
        &self,
        buffer: BufferId,
        entry: &BlameEntry,
    ) -> Option<ParsedCommitMessage> {
        self.buffers
            .get(&buffer)?
            .commit_details
            .get(&entry.sha)
            .cloned()
    }

    pub fn blame_for_rows<'a>(
        &'a mut self,
        rows: &'a [RowInfo],
        cx: &'a mut App,
    ) -> impl Iterator<Item = Option<(BufferId, BlameEntry)>> + use<'a> {
        rows.iter().map(move |info| {
            let buffer_id = info.buffer_id?;
            self.sync(cx, buffer_id);

            let buffer_row = info.buffer_row?;
            let mut cursor = self.buffers.get(&buffer_id)?.entries.cursor::<u32>(());
            cursor.seek_forward(&buffer_row, Bias::Right);
            Some((buffer_id, cursor.item()?.blame.clone()?))
        })
    }

    /// How many monospace columns the widest author name in the file needs
    /// once the gutter has shortened it.
    ///
    /// Columns, not bytes and not `char`s: the gutter reserves
    /// `columns * ch_advance` pixels, so measuring the name any other way
    /// makes the reservation wrong — under for a wide CJK glyph, and roughly
    /// double for Cyrillic, whose letters are two bytes and one column.
    pub fn max_author_display_columns(&mut self, cx: &mut App) -> usize {
        self.sync_all(cx);

        let mut max_columns = 0;
        for buffer in self.buffers.values() {
            for entry in buffer.entries.iter() {
                let Some(blame_entry) = entry.blame.as_ref() else {
                    continue;
                };
                let author = ::git::blame::display_author(blame_entry.author.as_deref());
                max_columns = max_columns.max(unicode_width::UnicodeWidthStr::width(author));
            }
        }

        max_columns
    }

    pub fn blur(&mut self, _: &mut Context<Self>) {
        self.focused = false;
    }

    pub fn focus(&mut self, cx: &mut Context<Self>) {
        if self.focused {
            return;
        }
        self.focused = true;
        if self.changed_while_blurred {
            self.changed_while_blurred = false;
            self.generate(cx);
        }
    }

    fn sync_all(&mut self, cx: &mut App) {
        let Some(multi_buffer) = self.multi_buffer.upgrade() else {
            return;
        };
        let snapshot = multi_buffer.read(cx).snapshot(cx);
        for id in snapshot.all_buffer_ids() {
            self.sync(cx, id)
        }
    }

    fn sync(&mut self, cx: &mut App, buffer_id: BufferId) {
        let Some(blame_buffer) = self.buffers.get_mut(&buffer_id) else {
            return;
        };
        let Some(buffer) = self
            .multi_buffer
            .upgrade()
            .and_then(|multi_buffer| multi_buffer.read(cx).buffer(buffer_id))
        else {
            return;
        };
        let edits = blame_buffer.buffer_edits.consume();
        let had_edits = !edits.is_empty();
        let new_snapshot = buffer.read(cx).snapshot();

        let mut row_edits = edits
            .into_iter()
            .map(|edit| {
                let old_point_range = blame_buffer.buffer_snapshot.offset_to_point(edit.old.start)
                    ..blame_buffer.buffer_snapshot.offset_to_point(edit.old.end);
                let new_point_range = new_snapshot.offset_to_point(edit.new.start)
                    ..new_snapshot.offset_to_point(edit.new.end);

                if old_point_range.start.column
                    == blame_buffer
                        .buffer_snapshot
                        .line_len(old_point_range.start.row)
                    && (new_snapshot.chars_at(edit.new.start).next() == Some('\n')
                        || blame_buffer
                            .buffer_snapshot
                            .line_len(old_point_range.end.row)
                            == 0)
                {
                    Edit {
                        old: old_point_range.start.row + 1..old_point_range.end.row + 1,
                        new: new_point_range.start.row + 1..new_point_range.end.row + 1,
                    }
                } else if old_point_range.start.column == 0
                    && old_point_range.end.column == 0
                    && new_point_range.end.column == 0
                {
                    Edit {
                        old: old_point_range.start.row..old_point_range.end.row,
                        new: new_point_range.start.row..new_point_range.end.row,
                    }
                } else {
                    Edit {
                        old: old_point_range.start.row..old_point_range.end.row + 1,
                        new: new_point_range.start.row..new_point_range.end.row + 1,
                    }
                }
            })
            .peekable();

        let mut new_entries = SumTree::default();
        let mut cursor = blame_buffer.entries.cursor::<u32>(());

        while let Some(mut edit) = row_edits.next() {
            while let Some(next_edit) = row_edits.peek() {
                if edit.old.end >= next_edit.old.start {
                    edit.old.end = next_edit.old.end;
                    edit.new.end = next_edit.new.end;
                    row_edits.next();
                } else {
                    break;
                }
            }

            new_entries.append(cursor.slice(&edit.old.start, Bias::Right), ());

            if edit.new.start > new_entries.summary().rows {
                new_entries.push(
                    GitBlameEntry {
                        rows: edit.new.start - new_entries.summary().rows,
                        blame: cursor.item().and_then(|entry| entry.blame.clone()),
                    },
                    (),
                );
            }

            cursor.seek(&edit.old.end, Bias::Right);
            if !edit.new.is_empty() {
                new_entries.push(
                    GitBlameEntry {
                        rows: edit.new.len() as u32,
                        blame: None,
                    },
                    (),
                );
            }

            let old_end = cursor.end();
            if row_edits
                .peek()
                .is_none_or(|next_edit| next_edit.old.start >= old_end)
                && let Some(entry) = cursor.item()
            {
                if old_end > edit.old.end {
                    new_entries.push(
                        GitBlameEntry {
                            rows: cursor.end() - edit.old.end,
                            blame: entry.blame.clone(),
                        },
                        (),
                    );
                }

                cursor.next();
            }
        }
        new_entries.append(cursor.suffix(), ());
        drop(cursor);

        blame_buffer.buffer_snapshot = new_snapshot;
        blame_buffer.entries = new_entries;
        if had_edits {
            self.blame_generation = self.blame_generation.wrapping_add(1);
        }
    }

    #[cfg(test)]
    fn check_invariants(&mut self, cx: &mut Context<Self>) {
        self.sync_all(cx);
        for (&id, buffer) in &self.buffers {
            assert_eq!(
                buffer.entries.summary().rows,
                self.multi_buffer
                    .upgrade()
                    .unwrap()
                    .read(cx)
                    .buffer(id)
                    .unwrap()
                    .read(cx)
                    .max_point()
                    .row
                    + 1
            );
        }
    }

    #[ztracing::instrument(skip_all)]
    fn generate(&mut self, cx: &mut Context<Self>) {
        if !self.focused {
            self.changed_while_blurred = true;
            return;
        }
        let buffers_to_blame = self
            .multi_buffer
            .update(cx, |multi_buffer, cx| {
                let snapshot = multi_buffer.snapshot(cx);
                snapshot
                    .all_buffer_ids()
                    .filter_map(|id| Some(multi_buffer.buffer(id)?.downgrade()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let project = self.project.downgrade();
        let base_sources = self.base_sources.clone();

        self.task = cx.spawn(async move |this, cx| {
            let mut all_results = Vec::new();
            let mut all_errors = Vec::new();

            for buffers in buffers_to_blame.chunks(4) {
                let span = ztracing::debug_span!("for each chunk of buffers");
                let _enter = span.enter();
                let blame = cx.update(|cx| {
                    buffers
                        .iter()
                        .map(|buffer| {
                            let buffer = buffer.upgrade().context("buffer was dropped")?;
                            let project = project.upgrade().context("project was dropped")?;
                            let id = buffer.read(cx).remote_id();
                            let snapshot = buffer.read(cx).snapshot();
                            let buffer_edits = buffer.update(cx, |buffer, _| buffer.subscribe());

                            let base_source = base_sources.get(&id);
                            let repository = match base_source {
                                Some(source) => Some(source.repository.clone()),
                                None => project
                                    .read(cx)
                                    .git_store()
                                    .read(cx)
                                    .repository_and_path_for_buffer_id(id, cx)
                                    .map(|(repo, _)| repo),
                            };

                            let remote_url = repository
                                .as_ref()
                                .and_then(|repo| repo.read(cx).default_remote_url());

                            let blame_buffer = match base_source {
                                Some(source) => project.update(cx, |project, cx| {
                                    project.blame_path_at_revision(
                                        &source.repository,
                                        source.repo_path.clone(),
                                        source.revision.to_string(),
                                        cx,
                                    )
                                }),
                                None if repository.is_some() => project
                                    .update(cx, |project, cx| {
                                        project.blame_buffer(&buffer, None, cx)
                                    }),
                                None => Task::ready(Ok(None)),
                            };

                            Ok(async move {
                                (id, snapshot, buffer_edits, blame_buffer.await, remote_url)
                            })
                        })
                        .collect::<Result<Vec<_>>>()
                })?;
                let provider_registry =
                    cx.update(|cx| GitHostingProviderRegistry::default_global(cx));
                let (results, errors) = cx
                    .background_spawn({
                        async move {
                            let blame = futures::future::join_all(blame).await;
                            let mut res = vec![];
                            let mut errors = vec![];
                            for (id, snapshot, buffer_edits, blame, remote_url) in blame {
                                match blame {
                                    Ok(Some(Blame { entries, messages })) => {
                                        let entries = build_blame_entry_sum_tree(
                                            entries,
                                            snapshot.max_point().row,
                                        );
                                        let commit_details = messages
                                            .into_iter()
                                            .map(|(oid, message)| {
                                                let parsed_commit_message =
                                                    ParsedCommitMessage::parse(
                                                        oid.to_string(),
                                                        message,
                                                        remote_url.as_deref(),
                                                        Some(provider_registry.clone()),
                                                    );
                                                (oid, parsed_commit_message)
                                            })
                                            .collect();
                                        res.push((
                                            id,
                                            snapshot,
                                            buffer_edits,
                                            Some(entries),
                                            commit_details,
                                        ));
                                    }
                                    Ok(None) => res.push((
                                        id,
                                        snapshot,
                                        buffer_edits,
                                        None,
                                        Default::default(),
                                    )),
                                    Err(e) => errors.push(e),
                                }
                            }
                            (res, errors)
                        }
                    })
                    .await;
                all_results.extend(results);
                all_errors.extend(errors)
            }

            this.update(cx, |this, cx| {
                this.buffers.clear();
                this.blame_generation = this.blame_generation.wrapping_add(1);
                for (id, snapshot, buffer_edits, entries, commit_details) in all_results {
                    let Some(entries) = entries else {
                        continue;
                    };
                    this.buffers.insert(
                        id,
                        GitBlameBuffer {
                            buffer_edits,
                            buffer_snapshot: snapshot,
                            entries,
                            commit_details,
                        },
                    );
                }
                cx.notify();
                if !all_errors.is_empty() {
                    this.project.update(cx, |_, cx| {
                        let all_errors = all_errors
                            .into_iter()
                            .map(|e| format!("{e:#}"))
                            .dedup()
                            .collect::<Vec<_>>();
                        let all_errors = all_errors.join(", ");
                        if this.user_triggered {
                            log::error!("failed to get git blame data: {all_errors}");
                            cx.emit(project::Event::Toast {
                                notification_id: "git-blame".into(),
                                message: all_errors,
                                link: None,
                            });
                        } else {
                            // If we weren't triggered by a user, we just log errors in the background, instead of sending
                            // notifications.
                            log::debug!("failed to get git blame data: {all_errors}");
                        }
                    })
                }
            })
        });
    }

    fn regenerate_on_edit(&mut self, cx: &mut Context<Self>) {
        // todo(lw): hot foreground spawn
        self.regenerate_on_edit_task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(REGENERATE_ON_EDIT_DEBOUNCE_INTERVAL)
                .await;

            this.update(cx, |this, cx| {
                this.generate(cx);
            })
        });
    }
}

const REGENERATE_ON_EDIT_DEBOUNCE_INTERVAL: Duration = Duration::from_secs(2);

fn build_blame_entry_sum_tree(entries: Vec<BlameEntry>, max_row: u32) -> SumTree<GitBlameEntry> {
    let mut current_row = 0;
    let mut entries = SumTree::from_iter(
        entries.into_iter().flat_map(|entry| {
            let mut entries = SmallVec::<[GitBlameEntry; 2]>::new();

            if entry.range.start > current_row {
                let skipped_rows = entry.range.start - current_row;
                entries.push(GitBlameEntry {
                    rows: skipped_rows,
                    blame: None,
                });
            }
            entries.push(GitBlameEntry {
                rows: entry.range.len() as u32,
                blame: Some(entry.clone()),
            });

            current_row = entry.range.end;
            entries
        }),
        (),
    );

    if max_row >= current_row {
        entries.push(
            GitBlameEntry {
                rows: (max_row + 1) - current_row,
                blame: None,
            },
            (),
        );
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::repository::repo_path;
    use gpui::Context;
    use language::{Point, Rope};
    use project::FakeFs;
    use rand::prelude::*;
    use serde_json::json;
    use settings::SettingsStore;
    use std::{cmp, env, ops::Range, path::Path, sync::Mutex};
    use text::BufferId;
    use unindent::Unindent as _;
    use util::{RandomCharIter, path};

    // macro_rules! assert_blame_rows {
    //     ($blame:expr, $rows:expr, $expected:expr, $cx:expr) => {
    //         assert_eq!(
    //             $blame
    //                 .blame_for_rows($rows.map(MultiBufferRow).map(Some), $cx)
    //                 .collect::<Vec<_>>(),
    //             $expected
    //         );
    //     };
    // }

    #[track_caller]
    fn assert_blame_rows(
        blame: &mut GitBlame,
        buffer_id: BufferId,
        rows: Range<u32>,
        expected: Vec<Option<BlameEntry>>,
        cx: &mut Context<GitBlame>,
    ) {
        pretty_assertions::assert_eq!(
            blame
                .blame_for_rows(
                    &rows
                        .map(|row| RowInfo {
                            buffer_row: Some(row),
                            buffer_id: Some(buffer_id),
                            ..Default::default()
                        })
                        .collect::<Vec<_>>(),
                    cx
                )
                .collect::<Vec<_>>(),
            expected
                .into_iter()
                .map(|it| Some((buffer_id, it?)))
                .collect::<Vec<_>>()
        );
    }

    fn init_test(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);

            theme_settings::init(theme::LoadThemes::JustBase, cx);

            crate::init(cx);
        });
    }

    #[gpui::test]
    async fn test_blame_error_notifications(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/my-repo",
            json!({
                ".git": {},
                "file.txt": r#"
                    irrelevant contents
                "#
                .unindent()
            }),
        )
        .await;

        // Creating a GitBlame without a corresponding blame state
        // will result in an error.

        let project = Project::test(fs, ["/my-repo".as_ref()], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/my-repo/file.txt", cx)
            })
            .await
            .unwrap();
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        let blame = cx.new(|cx| {
            GitBlame::new(
                buffer.clone(),
                project.clone(),
                HashMap::default(),
                true,
                true,
                cx,
            )
        });

        let event = project.next_event(cx).await;
        assert_eq!(
            event,
            project::Event::Toast {
                notification_id: "git-blame".into(),
                message: "Failed to blame \"file.txt\": failed to get blame for \"file.txt\""
                    .to_string(),
                link: None
            }
        );

        blame.update(cx, |blame, cx| {
            assert_eq!(
                blame
                    .blame_for_rows(
                        &(0..1)
                            .map(|row| RowInfo {
                                buffer_row: Some(row),
                                ..Default::default()
                            })
                            .collect::<Vec<_>>(),
                        cx
                    )
                    .collect::<Vec<_>>(),
                vec![None]
            );
        });
    }

    #[gpui::test]
    async fn test_blame_ignores_buffers_outside_git_repositories(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());

        fs.insert_tree(
            "/not-a-repo",
            json!({
                "foo": "bar",
            }),
        )
        .await;

        let project = Project::test(fs, ["/not-a-repo".as_ref()], cx).await;

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/not-a-repo/foo", cx)
            })
            .await
            .unwrap();

        let buffer_id = buffer.read_with(cx, |buffer, _| buffer.remote_id());

        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        let events = Arc::new(Mutex::new(Vec::new()));

        let _subscription = project.update(cx, |_, cx| {
            cx.subscribe(&project, {
                let events = events.clone();

                move |_, _, event: &project::Event, _| {
                    events
                        .lock()
                        .expect("events mutex poisoned")
                        .push(event.clone());
                }
            })
        });

        let blame = cx.new(|cx| {
            GitBlame::new(
                buffer.clone(),
                project.clone(),
                HashMap::default(),
                true,
                true,
                cx,
            )
        });

        cx.executor().run_until_parked();

        assert!(events.lock().expect("events mutex poisoned").is_empty());

        blame.update(cx, |blame, cx| {
            assert_eq!(
                blame
                    .blame_for_rows(
                        &(0..1)
                            .map(|row| RowInfo {
                                buffer_row: Some(row),
                                buffer_id: Some(buffer_id),
                                ..Default::default()
                            })
                            .collect::<Vec<_>>(),
                        cx
                    )
                    .collect::<Vec<_>>(),
                vec![None]
            );
        });
    }

    #[gpui::test]
    async fn test_blame_for_rows(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/my-repo",
            json!({
                ".git": {},
                "file.txt": r#"
                    AAA Line 1
                    BBB Line 2 - Modified 1
                    CCC Line 3 - Modified 2
                    modified in memory 1
                    modified in memory 1
                    DDD Line 4 - Modified 2
                    EEE Line 5 - Modified 1
                    FFF Line 6 - Modified 2
                "#
                .unindent()
            }),
        )
        .await;

        fs.set_blame_for_repo(
            Path::new("/my-repo/.git"),
            vec![(
                repo_path("file.txt"),
                Blame {
                    entries: vec![
                        blame_entry("1b1b1b", 0..1),
                        blame_entry("0d0d0d", 1..2),
                        blame_entry("3a3a3a", 2..3),
                        blame_entry("3a3a3a", 5..6),
                        blame_entry("0d0d0d", 6..7),
                        blame_entry("3a3a3a", 7..8),
                    ],
                    ..Default::default()
                },
            )],
        );
        let project = Project::test(fs, ["/my-repo".as_ref()], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/my-repo/file.txt", cx)
            })
            .await
            .unwrap();
        let buffer_id = buffer.read_with(cx, |buffer, _| buffer.remote_id());
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        let git_blame = cx
            .new(|cx| GitBlame::new(buffer.clone(), project, HashMap::default(), false, true, cx));

        cx.executor().run_until_parked();

        git_blame.update(cx, |blame, cx| {
            // All lines
            pretty_assertions::assert_eq!(
                blame
                    .blame_for_rows(
                        &(0..8)
                            .map(|buffer_row| RowInfo {
                                buffer_row: Some(buffer_row),
                                buffer_id: Some(buffer_id),
                                ..Default::default()
                            })
                            .collect::<Vec<_>>(),
                        cx
                    )
                    .collect::<Vec<_>>(),
                vec![
                    Some((buffer_id, blame_entry("1b1b1b", 0..1))),
                    Some((buffer_id, blame_entry("0d0d0d", 1..2))),
                    Some((buffer_id, blame_entry("3a3a3a", 2..3))),
                    None,
                    None,
                    Some((buffer_id, blame_entry("3a3a3a", 5..6))),
                    Some((buffer_id, blame_entry("0d0d0d", 6..7))),
                    Some((buffer_id, blame_entry("3a3a3a", 7..8))),
                ]
            );
            // Subset of lines
            pretty_assertions::assert_eq!(
                blame
                    .blame_for_rows(
                        &(1..4)
                            .map(|buffer_row| RowInfo {
                                buffer_row: Some(buffer_row),
                                buffer_id: Some(buffer_id),
                                ..Default::default()
                            })
                            .collect::<Vec<_>>(),
                        cx
                    )
                    .collect::<Vec<_>>(),
                vec![
                    Some((buffer_id, blame_entry("0d0d0d", 1..2))),
                    Some((buffer_id, blame_entry("3a3a3a", 2..3))),
                    None
                ]
            );
            // Subset of lines, with some not displayed
            pretty_assertions::assert_eq!(
                blame
                    .blame_for_rows(
                        &[
                            RowInfo {
                                buffer_row: Some(1),
                                buffer_id: Some(buffer_id),
                                ..Default::default()
                            },
                            Default::default(),
                            Default::default(),
                        ],
                        cx
                    )
                    .collect::<Vec<_>>(),
                vec![Some((buffer_id, blame_entry("0d0d0d", 1..2))), None, None]
            );
        });
    }

    /// A left-hand diff pane is backed by a detached base-text buffer. Without
    /// a `BlameBaseSource` it must produce nothing (it has no repository), and
    /// with one it must be annotated from the base revision — not from HEAD.
    #[gpui::test]
    async fn test_blame_for_detached_base_text_buffer(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/my-repo"),
            json!({
                ".git": {},
                "file.txt": "Line 1\nLine 2\nLine 3\n"
            }),
        )
        .await;

        fs.set_blame_for_repo(
            Path::new(path!("/my-repo/.git")),
            vec![(
                repo_path("file.txt"),
                Blame {
                    entries: vec![blame_entry("aaaaaa", 0..3)],
                    ..Default::default()
                },
            )],
        );
        fs.set_blame_at_revision_for_repo(
            Path::new(path!("/my-repo/.git")),
            "HEAD",
            vec![(
                repo_path("file.txt"),
                Blame {
                    entries: vec![blame_entry("bbbbbb", 0..2)],
                    ..Default::default()
                },
            )],
        );

        let project = Project::test(fs, [path!("/my-repo").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let base_text_buffer = cx.new(|cx| language::Buffer::local("Base 1\nBase 2\n", cx));
        let base_text_buffer_id = base_text_buffer.read_with(cx, |buffer, _| buffer.remote_id());
        let multi_buffer = cx.new(|cx| MultiBuffer::singleton(base_text_buffer, cx));

        let without_source = cx.new(|cx| {
            GitBlame::new(
                multi_buffer.clone(),
                project.clone(),
                HashMap::default(),
                false,
                true,
                cx,
            )
        });
        cx.executor().run_until_parked();
        without_source.update(cx, |blame, cx| {
            assert_blame_rows(blame, base_text_buffer_id, 0..2, vec![None, None], cx);
        });

        let repository = project.read_with(cx, |project, cx| {
            project
                .git_store()
                .read(cx)
                .repositories()
                .values()
                .next()
                .cloned()
                .expect("git store should have discovered /my-repo")
        });
        let mut base_sources = HashMap::default();
        base_sources.insert(
            base_text_buffer_id,
            BlameBaseSource {
                repository,
                repo_path: repo_path("file.txt"),
                revision: "HEAD".into(),
            },
        );

        let with_source = cx.new(|cx| {
            GitBlame::new(
                multi_buffer.clone(),
                project.clone(),
                base_sources,
                false,
                true,
                cx,
            )
        });
        cx.executor().run_until_parked();
        with_source.update(cx, |blame, cx| {
            assert_blame_rows(
                blame,
                base_text_buffer_id,
                0..2,
                vec![
                    Some(blame_entry("bbbbbb", 0..2)),
                    Some(blame_entry("bbbbbb", 0..2)),
                ],
                cx,
            );
        });
    }

    #[gpui::test]
    async fn test_blame_for_rows_with_edits(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/my-repo"),
            json!({
                ".git": {},
                "file.txt": r#"
                    Line 1
                    Line 2
                    Line 3
                "#
                .unindent()
            }),
        )
        .await;

        fs.set_blame_for_repo(
            Path::new(path!("/my-repo/.git")),
            vec![(
                repo_path("file.txt"),
                Blame {
                    entries: vec![blame_entry("1b1b1b", 0..4)],
                    ..Default::default()
                },
            )],
        );

        let project = Project::test(fs, [path!("/my-repo").as_ref()], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/my-repo/file.txt"), cx)
            })
            .await
            .unwrap();
        let buffer_id = buffer.read_with(cx, |buffer, _| buffer.remote_id());
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        let git_blame = cx
            .new(|cx| GitBlame::new(buffer.clone(), project, HashMap::default(), false, true, cx));

        cx.executor().run_until_parked();

        git_blame.update(cx, |blame, cx| {
            // Sanity check before edits: make sure that we get the same blame entry for all
            // lines.
            assert_blame_rows(
                blame,
                buffer_id,
                0..4,
                vec![
                    Some(blame_entry("1b1b1b", 0..4)),
                    Some(blame_entry("1b1b1b", 0..4)),
                    Some(blame_entry("1b1b1b", 0..4)),
                    Some(blame_entry("1b1b1b", 0..4)),
                ],
                cx,
            );
        });

        // Modify a single line, at the start of the line
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(0, 0)..Point::new(0, 0), "X")], None, cx);
        });
        git_blame.update(cx, |blame, cx| {
            assert_blame_rows(
                blame,
                buffer_id,
                0..2,
                vec![None, Some(blame_entry("1b1b1b", 0..4))],
                cx,
            );
        });
        // Modify a single line, in the middle of the line
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(1, 2)..Point::new(1, 2), "X")], None, cx);
        });
        git_blame.update(cx, |blame, cx| {
            assert_blame_rows(
                blame,
                buffer_id,
                1..4,
                vec![
                    None,
                    Some(blame_entry("1b1b1b", 0..4)),
                    Some(blame_entry("1b1b1b", 0..4)),
                ],
                cx,
            );
        });

        // Before we insert a newline at the end, sanity check:
        git_blame.update(cx, |blame, cx| {
            assert_blame_rows(
                blame,
                buffer_id,
                3..4,
                vec![Some(blame_entry("1b1b1b", 0..4))],
                cx,
            );
        });
        // Insert a newline at the end
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(3, 6)..Point::new(3, 6), "\n")], None, cx);
        });
        // Only the new line is marked as edited:
        git_blame.update(cx, |blame, cx| {
            assert_blame_rows(
                blame,
                buffer_id,
                3..5,
                vec![Some(blame_entry("1b1b1b", 0..4)), None],
                cx,
            );
        });

        // Before we insert a newline at the start, sanity check:
        git_blame.update(cx, |blame, cx| {
            assert_blame_rows(
                blame,
                buffer_id,
                2..3,
                vec![Some(blame_entry("1b1b1b", 0..4))],
                cx,
            );
        });

        // Usage example
        // Insert a newline at the start of the row
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(2, 0)..Point::new(2, 0), "\n")], None, cx);
        });
        // Only the new line is marked as edited:
        git_blame.update(cx, |blame, cx| {
            assert_blame_rows(
                blame,
                buffer_id,
                2..4,
                vec![None, Some(blame_entry("1b1b1b", 0..4))],
                cx,
            );
        });
    }

    #[gpui::test(iterations = 100)]
    async fn test_blame_random(mut rng: StdRng, cx: &mut gpui::TestAppContext) {
        let operations = env::var("OPERATIONS")
            .map(|i| i.parse().expect("invalid `OPERATIONS` variable"))
            .unwrap_or(10);
        let max_edits_per_operation = env::var("MAX_EDITS_PER_OPERATION")
            .map(|i| {
                i.parse()
                    .expect("invalid `MAX_EDITS_PER_OPERATION` variable")
            })
            .unwrap_or(5);

        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let buffer_initial_text_len = rng.random_range(5..15);
        let mut buffer_initial_text = Rope::from(
            RandomCharIter::new(&mut rng)
                .take(buffer_initial_text_len)
                .collect::<String>()
                .as_str(),
        );

        let mut newline_ixs = (0..buffer_initial_text_len).choose_multiple(&mut rng, 5);
        newline_ixs.sort_unstable();
        for newline_ix in newline_ixs.into_iter().rev() {
            let newline_ix = buffer_initial_text.clip_offset(newline_ix, Bias::Right);
            buffer_initial_text.replace(newline_ix..newline_ix, "\n");
        }
        log::info!("initial buffer text: {:?}", buffer_initial_text);

        fs.insert_tree(
            path!("/my-repo"),
            json!({
                ".git": {},
                "file.txt": buffer_initial_text.to_string()
            }),
        )
        .await;

        let blame_entries = gen_blame_entries(buffer_initial_text.max_point().row, &mut rng);
        log::info!("initial blame entries: {:?}", blame_entries);
        fs.set_blame_for_repo(
            Path::new(path!("/my-repo/.git")),
            vec![(
                repo_path("file.txt"),
                Blame {
                    entries: blame_entries,
                    ..Default::default()
                },
            )],
        );

        let project = Project::test(fs.clone(), [path!("/my-repo").as_ref()], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/my-repo/file.txt"), cx)
            })
            .await
            .unwrap();
        let mbuffer = cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx));

        let git_blame = cx.new(|cx| {
            GitBlame::new(
                mbuffer.clone(),
                project,
                HashMap::default(),
                false,
                true,
                cx,
            )
        });
        cx.executor().run_until_parked();
        git_blame.update(cx, |blame, cx| blame.check_invariants(cx));

        for _ in 0..operations {
            match rng.random_range(0..100) {
                0..=19 => {
                    log::info!("quiescing");
                    cx.executor().run_until_parked();
                }
                20..=69 => {
                    log::info!("editing buffer");
                    buffer.update(cx, |buffer, cx| {
                        buffer.randomly_edit(&mut rng, max_edits_per_operation, cx);
                        log::info!("buffer text: {:?}", buffer.text());
                    });

                    let blame_entries = gen_blame_entries(
                        buffer.read_with(cx, |buffer, _| buffer.max_point().row),
                        &mut rng,
                    );
                    log::info!("regenerating blame entries: {:?}", blame_entries);

                    fs.set_blame_for_repo(
                        Path::new(path!("/my-repo/.git")),
                        vec![(
                            repo_path("file.txt"),
                            Blame {
                                entries: blame_entries,
                                ..Default::default()
                            },
                        )],
                    );
                }
                _ => {
                    git_blame.update(cx, |blame, cx| blame.check_invariants(cx));
                }
            }
        }

        git_blame.update(cx, |blame, cx| blame.check_invariants(cx));
    }

    fn gen_blame_entries(max_row: u32, rng: &mut StdRng) -> Vec<BlameEntry> {
        let mut last_row = 0;
        let mut blame_entries = Vec::new();
        for ix in 0..5 {
            if last_row < max_row {
                let row_start = rng.random_range(last_row..max_row);
                let row_end = rng.random_range(row_start + 1..cmp::min(row_start + 3, max_row) + 1);
                blame_entries.push(blame_entry(&ix.to_string(), row_start..row_end));
                last_row = row_end;
            } else {
                break;
            }
        }
        blame_entries
    }

    fn blame_entry(sha: &str, range: Range<u32>) -> BlameEntry {
        BlameEntry {
            sha: sha.parse().unwrap(),
            range,
            original_line_number: 0,
            author: None,
            author_mail: None,
            author_time: None,
            author_tz: None,
            committer_name: None,
            committer_email: None,
            committer_time: None,
            committer_tz: None,
            summary: None,
            previous: None,
            filename: String::new(),
        }
    }

    fn test_buffer_id(id: u64) -> BufferId {
        BufferId::new(id).unwrap()
    }

    fn text_row(buffer_id: BufferId, buffer_row: u32) -> RowInfo {
        RowInfo {
            buffer_id: Some(buffer_id),
            buffer_row: Some(buffer_row),
            ..Default::default()
        }
    }

    /// A soft-wrap continuation as `WrapRows` emits it: no buffer id, no
    /// buffer row, but the wrapped row it belongs to.
    fn soft_wrapped_row(buffer_row: u32) -> RowInfo {
        RowInfo {
            wrapped_buffer_row: Some(buffer_row),
            ..Default::default()
        }
    }

    /// A block row (excerpt header, diff-hunk controls, folded-buffer header)
    /// as `BlockRows` emits it: every field `None`. Not every block row looks
    /// like this — the first output row of a `Replace` custom block (a
    /// collapsed block crease) forwards its real row info and so reads as
    /// ordinary blamed text here.
    fn block_row() -> RowInfo {
        RowInfo::default()
    }

    /// Runs one fixture through the classification. The third element of each
    /// entry is the row's alignment flag, as `alignment_rows_in_range` derives
    /// it from the block map: `true` only for the rows of a `Block::Spacer`.
    fn run_positions(
        spec: &[(RowInfo, Option<(BufferId, &str)>, bool)],
        predecessor: BlameRunPredecessor,
    ) -> (Vec<Option<BlameRunPosition>>, BlameRunPredecessor) {
        let rows = spec.iter().map(|(info, _, _)| *info).collect::<Vec<_>>();
        let blamed_rows = spec
            .iter()
            .map(|(_, blame, _)| blame.map(|(buffer_id, sha)| (buffer_id, blame_entry(sha, 0..1))))
            .collect::<Vec<_>>();
        let alignment_rows = spec
            .iter()
            .map(|(_, _, alignment)| *alignment)
            .collect::<Vec<_>>();

        blame_run_positions(&rows, &blamed_rows, &alignment_rows, predecessor)
    }

    /// Every entry is built with the same `range`, so an implementation that
    /// grouped on `BlameEntry::range` instead of row adjacency would collapse
    /// these fixtures into one run — failing most of the tests below (the
    /// header-block case survives it, being pinned by the separate block-row
    /// clause rather than by the adjacency test).
    ///
    /// The fixture is taken to start the display, so its first blamed row is a
    /// `DocumentHead`: it opens a run with nothing above it.
    #[track_caller]
    fn assert_run_positions(
        spec: &[(RowInfo, Option<(BufferId, &str)>)],
        expected: &[Option<BlameRunPosition>],
    ) {
        let spec = spec
            .iter()
            .map(|(info, blame)| (*info, *blame, false))
            .collect::<Vec<_>>();
        assert_run_positions_with_alignment(&spec, expected);
    }

    #[track_caller]
    fn assert_run_positions_with_alignment(
        spec: &[(RowInfo, Option<(BufferId, &str)>, bool)],
        expected: &[Option<BlameRunPosition>],
    ) {
        pretty_assertions::assert_eq!(
            run_positions(spec, BlameRunPredecessor::DisplayStart).0,
            expected
        );
    }

    /// The composition the gutter actually performs: `context` are the display
    /// rows above the visible ones, classified first, and what they leave
    /// behind is what `spec` — the visible slice — is classified against. This
    /// is `GitBlame::run_predecessor_above` followed by
    /// `GitBlame::run_positions_in_viewport`, with the display-row scan (which
    /// needs a real `DisplaySnapshot`) standing in as a literal fixture.
    #[track_caller]
    fn assert_run_positions_below(
        context: &[(RowInfo, Option<(BufferId, &str)>, bool)],
        spec: &[(RowInfo, Option<(BufferId, &str)>, bool)],
        expected: &[Option<BlameRunPosition>],
    ) {
        let (_, predecessor) = run_positions(context, BlameRunPredecessor::DisplayStart);
        pretty_assertions::assert_eq!(run_positions(spec, predecessor).0, expected);
    }

    /// Shorthand for a fixture with no alignment spacers in it.
    fn plain<'a>(
        spec: &[(RowInfo, Option<(BufferId, &'a str)>)],
    ) -> Vec<(RowInfo, Option<(BufferId, &'a str)>, bool)> {
        spec.iter()
            .map(|(info, blame)| (*info, *blame, false))
            .collect()
    }

    #[test]
    fn blame_run_positions_group_consecutive_rows_of_one_commit() {
        let buffer = test_buffer_id(1);
        assert_run_positions(
            &[
                (text_row(buffer, 0), Some((buffer, "1a1a1a"))),
                (text_row(buffer, 1), Some((buffer, "1a1a1a"))),
                (text_row(buffer, 2), Some((buffer, "1a1a1a"))),
            ],
            &[
                Some(BlameRunPosition::DocumentHead),
                Some(BlameRunPosition::Continuation),
                Some(BlameRunPosition::Continuation),
            ],
        );
    }

    #[test]
    fn blame_run_positions_break_when_the_sha_changes() {
        let buffer = test_buffer_id(1);
        assert_run_positions(
            &[
                (text_row(buffer, 0), Some((buffer, "1a1a1a"))),
                (text_row(buffer, 1), Some((buffer, "2b2b2b"))),
                (text_row(buffer, 2), Some((buffer, "2b2b2b"))),
            ],
            &[
                Some(BlameRunPosition::DocumentHead),
                Some(BlameRunPosition::Head),
                Some(BlameRunPosition::Continuation),
            ],
        );
    }

    #[test]
    fn blame_run_positions_break_when_buffer_rows_jump() {
        let buffer = test_buffer_id(1);
        assert_run_positions(
            &[
                (text_row(buffer, 0), Some((buffer, "1a1a1a"))),
                (text_row(buffer, 5), Some((buffer, "1a1a1a"))),
                (text_row(buffer, 6), Some((buffer, "1a1a1a"))),
            ],
            &[
                Some(BlameRunPosition::DocumentHead),
                Some(BlameRunPosition::Head),
                Some(BlameRunPosition::Continuation),
            ],
        );
    }

    #[test]
    fn blame_run_positions_break_when_the_buffer_changes() {
        let first = test_buffer_id(1);
        let second = test_buffer_id(2);
        assert_run_positions(
            &[
                (text_row(first, 0), Some((first, "1a1a1a"))),
                (text_row(second, 1), Some((second, "1a1a1a"))),
            ],
            &[
                Some(BlameRunPosition::DocumentHead),
                Some(BlameRunPosition::Head),
            ],
        );
    }

    /// A header block stands for something between the two lines — the end of
    /// one excerpt and the start of the next, or a buffer that was folded
    /// away — so the run has to be cut there even though the two blamed rows
    /// around it happen to be buffer-adjacent.
    #[test]
    fn blame_run_positions_break_across_a_header_block_row() {
        let buffer = test_buffer_id(1);
        assert_run_positions(
            &[
                (text_row(buffer, 0), Some((buffer, "1a1a1a"))),
                (block_row(), None),
                (text_row(buffer, 1), Some((buffer, "1a1a1a"))),
            ],
            &[
                Some(BlameRunPosition::DocumentHead),
                None,
                Some(BlameRunPosition::Head),
            ],
        );
    }

    /// The sibling of the header case, and the whole point of the alignment
    /// flag: a split diff pads the shorter pane with a spacer that stands for
    /// text on the *other* side. Cutting there would make the two panes label
    /// one commit in different places — the left pane, which has no spacer,
    /// keeps a single run over the same lines.
    #[test]
    fn blame_run_positions_survive_an_alignment_spacer_row() {
        let buffer = test_buffer_id(1);
        assert_run_positions_with_alignment(
            &[
                (text_row(buffer, 0), Some((buffer, "1a1a1a")), false),
                (block_row(), None, true),
                (text_row(buffer, 1), Some((buffer, "1a1a1a")), false),
            ],
            &[
                Some(BlameRunPosition::DocumentHead),
                None,
                Some(BlameRunPosition::Continuation),
            ],
        );
    }

    /// A spacer stands for other-side text, not for a jump in this buffer, so
    /// it does not paper over one: the rows around it still have to be
    /// consecutive lines of the same commit to stay in one run.
    #[test]
    fn blame_run_positions_still_break_when_buffer_rows_jump_across_a_spacer() {
        let buffer = test_buffer_id(1);
        assert_run_positions_with_alignment(
            &[
                (text_row(buffer, 0), Some((buffer, "1a1a1a")), false),
                (block_row(), None, true),
                (text_row(buffer, 9), Some((buffer, "1a1a1a")), false),
            ],
            &[
                Some(BlameRunPosition::DocumentHead),
                None,
                Some(BlameRunPosition::Head),
            ],
        );
    }

    #[test]
    fn blame_run_positions_survive_a_soft_wrapped_row() {
        let buffer = test_buffer_id(1);
        assert_run_positions(
            &[
                (text_row(buffer, 0), Some((buffer, "1a1a1a"))),
                (soft_wrapped_row(0), None),
                (text_row(buffer, 1), Some((buffer, "1a1a1a"))),
            ],
            &[
                Some(BlameRunPosition::DocumentHead),
                None,
                Some(BlameRunPosition::Continuation),
            ],
        );
    }

    #[test]
    fn blame_run_positions_break_across_an_unblamed_row() {
        let buffer = test_buffer_id(1);
        assert_run_positions(
            &[
                (text_row(buffer, 0), Some((buffer, "1a1a1a"))),
                (text_row(buffer, 1), None),
                (text_row(buffer, 2), Some((buffer, "1a1a1a"))),
            ],
            &[
                Some(BlameRunPosition::DocumentHead),
                None,
                Some(BlameRunPosition::Head),
            ],
        );
    }

    /// The rule the gutter is scrolled through: a slice that begins in the
    /// middle of a run is a slice of continuations. The label stays on the row
    /// the run really starts on — off screen, and so unwritten — instead of
    /// sliding down to whichever row the scroll left on top.
    #[test]
    fn blame_run_positions_continue_a_run_the_rows_above_started() {
        let buffer = test_buffer_id(1);
        assert_run_positions_below(
            &plain(&[(text_row(buffer, 6), Some((buffer, "1a1a1a")))]),
            &plain(&[
                (text_row(buffer, 7), Some((buffer, "1a1a1a"))),
                (text_row(buffer, 8), Some((buffer, "1a1a1a"))),
            ]),
            &[
                Some(BlameRunPosition::Continuation),
                Some(BlameRunPosition::Continuation),
            ],
        );
    }

    /// The other side of it: a slice whose first row is where a run really
    /// begins keeps its head, and its boundary. Scrolling one line further
    /// must not cost the label or the hairline.
    #[test]
    fn blame_run_positions_head_a_slice_that_starts_at_a_run() {
        let buffer = test_buffer_id(1);
        assert_run_positions_below(
            &plain(&[(text_row(buffer, 6), Some((buffer, "1a1a1a")))]),
            &plain(&[
                (text_row(buffer, 7), Some((buffer, "2b2b2b"))),
                (text_row(buffer, 8), Some((buffer, "2b2b2b"))),
            ]),
            &[
                Some(BlameRunPosition::Head),
                Some(BlameRunPosition::Continuation),
            ],
        );
    }

    /// The block row above the slice severs the run there just as it would
    /// inside it, so the first visible row opens a run of its own — and,
    /// having something above it, marks a boundary.
    #[test]
    fn blame_run_positions_head_below_a_block_row_above_the_slice() {
        let buffer = test_buffer_id(1);
        assert_run_positions_below(
            &plain(&[
                (text_row(buffer, 6), Some((buffer, "1a1a1a"))),
                (block_row(), None),
            ]),
            &plain(&[(text_row(buffer, 7), Some((buffer, "1a1a1a")))]),
            &[Some(BlameRunPosition::Head)],
        );
    }

    /// A soft-wrap continuation above the slice is the tail of the line above
    /// it, not a line of its own, so the run reaches across it — the case a
    /// one-row look above would get wrong every time a wrapped line is the
    /// first thing scrolled off.
    #[test]
    fn blame_run_positions_continue_across_a_soft_wrap_above_the_slice() {
        let buffer = test_buffer_id(1);
        assert_run_positions_below(
            &plain(&[
                (text_row(buffer, 6), Some((buffer, "1a1a1a"))),
                (soft_wrapped_row(6), None),
            ]),
            &plain(&[(text_row(buffer, 7), Some((buffer, "1a1a1a")))]),
            &[Some(BlameRunPosition::Continuation)],
        );
    }

    /// And an alignment spacer above the slice stands for text on the other
    /// side of a split diff, so it does not sever the run either: the pane
    /// with the spacer and the pane without it have to agree about where the
    /// run's head is, on screen or above it.
    #[test]
    fn blame_run_positions_continue_across_an_alignment_spacer_above_the_slice() {
        let buffer = test_buffer_id(1);
        assert_run_positions_below(
            &[
                (text_row(buffer, 6), Some((buffer, "1a1a1a")), false),
                (block_row(), None, true),
            ],
            &plain(&[(text_row(buffer, 7), Some((buffer, "1a1a1a")))]),
            &[Some(BlameRunPosition::Continuation)],
        );
    }

    /// An unblamed line above the slice is a line all the same: the run cannot
    /// reach across it, and the first visible row has something above it to be
    /// separated from.
    #[test]
    fn blame_run_positions_head_below_an_unblamed_row_above_the_slice() {
        let buffer = test_buffer_id(1);
        assert_run_positions_below(
            &plain(&[
                (text_row(buffer, 6), Some((buffer, "1a1a1a"))),
                (text_row(buffer, 7), None),
            ]),
            &plain(&[(text_row(buffer, 8), Some((buffer, "1a1a1a")))]),
            &[Some(BlameRunPosition::Head)],
        );
    }

    /// The first row of the display opens a run with nothing above it. It is
    /// the one head that marks no boundary — a hairline there separates the
    /// gutter from the toolbar, not one commit from another.
    #[test]
    fn blame_run_positions_open_the_display_at_its_first_row() {
        let buffer = test_buffer_id(1);
        assert_run_positions_below(
            &[],
            &plain(&[
                (text_row(buffer, 0), Some((buffer, "1a1a1a"))),
                (text_row(buffer, 1), Some((buffer, "1a1a1a"))),
            ]),
            &[
                Some(BlameRunPosition::DocumentHead),
                Some(BlameRunPosition::Continuation),
            ],
        );
    }

    /// A split diff whose first line is padded on one side puts a spacer above
    /// that pane's first blamed row. It stands for the other pane's text, so
    /// the row still opens the display: otherwise one pane would draw a
    /// hairline across its top edge and the other would not.
    #[test]
    fn blame_run_positions_open_the_display_across_a_leading_spacer() {
        let buffer = test_buffer_id(1);
        assert_run_positions_below(
            &[(block_row(), None, true)],
            &plain(&[(text_row(buffer, 0), Some((buffer, "1a1a1a")))]),
            &[Some(BlameRunPosition::DocumentHead)],
        );
    }

    /// The display-space half of the rule, which the fixtures above cannot
    /// reach: `run_predecessor_above` has to *find* the rows it classifies,
    /// and the row directly above the visible ones is very often a soft-wrap
    /// continuation that settles nothing. A scan that looked up exactly one
    /// row would read that as "nothing above", relabel the middle of a run,
    /// and draw a hairline across the top of the gutter — the very defect
    /// this is all here to remove — so the scan widens until it finds a row
    /// that answers the question.
    #[gpui::test]
    async fn test_run_positions_reach_past_wrapped_rows_above_the_viewport(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::display_map::{DisplayMap, DisplayRow, FoldPlaceholder};
        use gpui::{font, px};
        use project::project_settings::DiagnosticSeverity;

        init_test(cx);

        let long_line = "wrapped ".repeat(40);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/my-repo"),
            json!({
                ".git": {},
                "file.txt": format!("first\n{long_line}\nthird\nfourth\n"),
            }),
        )
        .await;
        // One commit over every line, so anything but `Continuation` below
        // the wrap is the classification losing the run, not the fixture.
        fs.set_blame_for_repo(
            Path::new(path!("/my-repo/.git")),
            vec![(
                repo_path("file.txt"),
                Blame {
                    entries: vec![blame_entry("1a1a1a", 0..4)],
                    ..Default::default()
                },
            )],
        );

        let project = Project::test(fs, [path!("/my-repo").as_ref()], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/my-repo/file.txt"), cx)
            })
            .await
            .expect("the fixture file opens");
        let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        let display_map = cx.new(|cx| {
            DisplayMap::new(
                multi_buffer.clone(),
                font("Helvetica"),
                px(14.),
                Some(px(120.)),
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });
        let snapshot = display_map.update(cx, |map, cx| map.snapshot(cx));

        let blame =
            cx.new(|cx| GitBlame::new(multi_buffer, project, HashMap::default(), false, true, cx));
        cx.executor().run_until_parked();

        // The second buffer line has to actually wrap, or the rows above the
        // viewport would be plain text rows and one look up would do.
        let start_row = snapshot
            .row_infos(DisplayRow(0))
            .position(|info| info.buffer_row == Some(2))
            .expect("the third line is on screen somewhere") as u32;
        assert!(
            start_row > 2,
            "the long line must occupy more than one display row, else the \
             scan above the viewport is never exercised: {start_row}"
        );

        let start_row = DisplayRow(start_row);
        let rows = snapshot.row_infos(start_row).take(2).collect::<Vec<_>>();
        blame.update(cx, |blame, cx| {
            let blamed_rows = blame.blame_for_rows(&rows, cx).collect::<Vec<_>>();
            pretty_assertions::assert_eq!(
                blame.run_positions_in_viewport(&snapshot, start_row, &rows, &blamed_rows, cx),
                vec![
                    Some(BlameRunPosition::Continuation),
                    Some(BlameRunPosition::Continuation),
                ],
                "the run started on the first line and the rows between are \
                 wrap continuations, so the visible rows continue it"
            );
        });
    }

    /// What a slice leaves for the rows below it, which is what
    /// `GitBlame::run_predecessor_above` reads and what its widening loop
    /// keys on: `DisplayStart` back out of a scan means those rows settled
    /// nothing, so the scan has to reach further up.
    #[test]
    fn blame_run_positions_report_what_the_rows_below_look_at() {
        let buffer = test_buffer_id(1);
        let sha = blame_entry("1a1a1a", 0..1).sha;

        assert_eq!(
            run_positions(
                &plain(&[(text_row(buffer, 6), Some((buffer, "1a1a1a")))]),
                BlameRunPredecessor::DisplayStart,
            )
            .1,
            BlameRunPredecessor::Blamed {
                buffer_id: buffer,
                sha,
                buffer_row: 6,
            },
            "a blamed row is what the row under it may continue"
        );
        assert_eq!(
            run_positions(
                &plain(&[
                    (text_row(buffer, 6), Some((buffer, "1a1a1a"))),
                    (block_row(), None),
                ]),
                BlameRunPredecessor::DisplayStart,
            )
            .1,
            BlameRunPredecessor::Severed,
            "a block row cuts the run before the rows below it ever see it"
        );
        assert_eq!(
            run_positions(
                &[
                    (block_row(), None, true),
                    (soft_wrapped_row(0), None, false),
                ],
                BlameRunPredecessor::DisplayStart,
            )
            .1,
            BlameRunPredecessor::DisplayStart,
            "spacers and wraps settle nothing, so the scan must widen past them"
        );
    }

    /// A scan that ran out of budget knows one thing for certain: everything
    /// it saw was a wrap or a spacer, so nothing up there *started* a run.
    /// Reading that as `DisplayStart` drew the date and author again in the
    /// middle of a run — and only in the pane that had the huge spacer, so the
    /// two panes of a split diff put the label on different rows.
    #[test]
    fn blame_run_positions_continue_a_run_the_scan_could_not_reach() {
        let buffer = test_buffer_id(1);
        pretty_assertions::assert_eq!(
            run_positions(
                &plain(&[
                    (text_row(buffer, 900), Some((buffer, "1a1a1a"))),
                    (text_row(buffer, 901), Some((buffer, "1a1a1a"))),
                ]),
                BlameRunPredecessor::Unsettled,
            )
            .0,
            vec![
                Some(BlameRunPosition::Continuation),
                Some(BlameRunPosition::Continuation),
            ],
            "the run above is unreadable, not absent, so its first visible row \
             must not be relabelled as a head"
        );
    }

    /// `Unsettled` only survives the same rows `DisplayStart` survives — a
    /// wrap or a spacer. Anything that genuinely breaks a run still breaks it,
    /// so the fallback cannot glue two commits together.
    #[test]
    fn blame_run_positions_stop_continuing_an_unreachable_run_at_a_block_row() {
        let buffer = test_buffer_id(1);
        pretty_assertions::assert_eq!(
            run_positions(
                &plain(&[
                    (block_row(), None),
                    (text_row(buffer, 900), Some((buffer, "1a1a1a"))),
                ]),
                BlameRunPredecessor::Unsettled,
            )
            .0,
            vec![None, Some(BlameRunPosition::Head)],
            "a block row severs whatever the scan could not see"
        );
        pretty_assertions::assert_eq!(
            run_positions(
                &[(soft_wrapped_row(900), None, false)],
                BlameRunPredecessor::Unsettled,
            )
            .1,
            BlameRunPredecessor::Unsettled,
            "a slice of nothing but wraps hands the same unreadable state down"
        );
    }

    /// The two halves of the lookback cap, on a real display snapshot: a line
    /// wrapping into more display rows than the scan is allowed to read.
    ///
    /// 1. The scan must not invent a run boundary — the row below the wrap
    ///    continues the run rather than opening one, which is also what the
    ///    companion pane of a split diff says about the same line.
    /// 2. It must not cost more than its budget, and the *same* question asked
    ///    again on the same frame must cost nothing: this runs from
    ///    `EditorElement::layout_blame_entries` on every frame the gutter is
    ///    shown, and it used to restart the whole widening window each time.
    #[gpui::test]
    async fn test_run_positions_at_the_lookback_cap(cx: &mut gpui::TestAppContext) {
        use crate::display_map::{DisplayMap, DisplayRow, FoldPlaceholder};
        use gpui::{font, px};
        use project::project_settings::DiagnosticSeverity;

        init_test(cx);

        // Long enough that the wrapped rows above the third line outnumber
        // `MAX_RUN_PREDECESSOR_LOOKBACK`, which is the only way to reach the
        // cap without a companion pane and a giant alignment spacer.
        let long_line = "wrapped ".repeat(1400);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/my-repo"),
            json!({
                ".git": {},
                "file.txt": format!("first\n{long_line}\nthird\nfourth\n"),
            }),
        )
        .await;
        // One commit over every line: anything but `Continuation` below the
        // wrap is the classification losing the run, not the fixture.
        fs.set_blame_for_repo(
            Path::new(path!("/my-repo/.git")),
            vec![(
                repo_path("file.txt"),
                Blame {
                    entries: vec![blame_entry("1a1a1a", 0..4)],
                    ..Default::default()
                },
            )],
        );

        let project = Project::test(fs, [path!("/my-repo").as_ref()], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/my-repo/file.txt"), cx)
            })
            .await
            .expect("the fixture file opens");
        let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        let display_map = cx.new(|cx| {
            DisplayMap::new(
                multi_buffer.clone(),
                font("Helvetica"),
                px(14.),
                Some(px(60.)),
                1,
                1,
                FoldPlaceholder::test(),
                DiagnosticSeverity::Warning,
                cx,
            )
        });
        cx.executor().run_until_parked();
        let snapshot = display_map.update(cx, |map, cx| map.snapshot(cx));

        let blame =
            cx.new(|cx| GitBlame::new(multi_buffer, project, HashMap::default(), false, true, cx));
        cx.executor().run_until_parked();

        let start_row = snapshot
            .row_infos(DisplayRow(0))
            .position(|info| info.buffer_row == Some(2))
            .expect("the third line is on screen somewhere") as u32;
        assert!(
            start_row > MAX_RUN_PREDECESSOR_LOOKBACK,
            "the wrapped line must occupy more display rows than the scan is \
             allowed to read, or the cap is never reached: {start_row}"
        );

        let start_row = DisplayRow(start_row);
        let rows = snapshot.row_infos(start_row).take(2).collect::<Vec<_>>();
        blame.update(cx, |blame, cx| {
            let blamed_rows = blame.blame_for_rows(&rows, cx).collect::<Vec<_>>();
            pretty_assertions::assert_eq!(
                blame.run_positions_in_viewport(&snapshot, start_row, &rows, &blamed_rows, cx),
                vec![
                    Some(BlameRunPosition::Continuation),
                    Some(BlameRunPosition::Continuation),
                ],
                "the scan gave up without reaching the run's head, which is not \
                 a reason to draw a new one"
            );
            assert_eq!(
                blame.last_predecessor_scan_rows, MAX_RUN_PREDECESSOR_LOOKBACK,
                "each widening must read only the rows it has not read yet — \
                 restarting the window every doubling scanned about twice this"
            );

            blame.run_positions_in_viewport(&snapshot, start_row, &rows, &blamed_rows, cx);
            assert_eq!(
                blame.last_predecessor_scan_rows, 0,
                "the gutter asks this on every frame, so the same question \
                 against the same snapshot must be answered from the memo"
            );
        });
    }
}
