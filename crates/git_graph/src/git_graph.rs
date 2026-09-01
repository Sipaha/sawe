mod canvas_geometry;
pub mod context_menu;
pub mod file_history;
pub mod filters;
pub mod git_graph_panel;
pub mod highlights;
pub mod log_toolbar;
pub mod mcp;
pub mod view_options;

/// Re-export of the mini-graph component, owned by `git_ui` to break the
/// `git_graph → git_ui` dep direction. Anyone in this crate that wants
/// the small commit-chain widget should reach for [`mini::MiniGraph`].
pub use git_ui::mini_graph as mini;

#[cfg(any(test, feature = "test-support"))]
pub use test_support::generate_random_commit_dag;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use git::Oid;
    use git::repository::InitialGraphCommitData;
    use rand::prelude::*;
    use smallvec::{SmallVec, smallvec};
    use std::sync::Arc;

    /// Generates a random commit DAG suitable for testing git graph rendering.
    ///
    /// The commits are ordered newest-first (like git log output), so:
    /// - Index 0 = most recent commit (HEAD)
    /// - Last index = oldest commit (root, has no parents)
    /// - Parents of commit at index I must have index > I
    ///
    /// When `adversarial` is true, generates complex topologies with many branches
    /// and octopus merges. Otherwise generates more realistic linear histories
    /// with occasional branches.
    pub fn generate_random_commit_dag(
        rng: &mut StdRng,
        num_commits: usize,
        adversarial: bool,
    ) -> Vec<Arc<InitialGraphCommitData>> {
        if num_commits == 0 {
            return Vec::new();
        }

        let mut commits: Vec<Arc<InitialGraphCommitData>> = Vec::with_capacity(num_commits);
        let oids: Vec<Oid> = (0..num_commits).map(|_| Oid::random(rng)).collect();

        for i in 0..num_commits {
            let sha = oids[i];

            let parents = if i == num_commits - 1 {
                smallvec![]
            } else {
                generate_parents_from_oids(rng, &oids, i, num_commits, adversarial)
            };

            let ref_names = if i == 0 {
                vec!["HEAD".into(), "main".into()]
            } else if adversarial && rng.random_bool(0.1) {
                vec![format!("branch-{}", i).into()]
            } else {
                Vec::new()
            };

            commits.push(Arc::new(InitialGraphCommitData {
                sha,
                parents,
                ref_names,
            }));
        }

        commits
    }

    fn generate_parents_from_oids(
        rng: &mut StdRng,
        oids: &[Oid],
        current_idx: usize,
        num_commits: usize,
        adversarial: bool,
    ) -> SmallVec<[Oid; 1]> {
        let remaining = num_commits - current_idx - 1;
        if remaining == 0 {
            return smallvec![];
        }

        if adversarial {
            let merge_chance = 0.4;
            let octopus_chance = 0.15;

            if remaining >= 3 && rng.random_bool(octopus_chance) {
                let num_parents = rng.random_range(3..=remaining.min(5));
                let mut parent_indices: Vec<usize> = (current_idx + 1..num_commits).collect();
                parent_indices.shuffle(rng);
                parent_indices
                    .into_iter()
                    .take(num_parents)
                    .map(|idx| oids[idx])
                    .collect()
            } else if remaining >= 2 && rng.random_bool(merge_chance) {
                let mut parent_indices: Vec<usize> = (current_idx + 1..num_commits).collect();
                parent_indices.shuffle(rng);
                parent_indices
                    .into_iter()
                    .take(2)
                    .map(|idx| oids[idx])
                    .collect()
            } else {
                let parent_idx = rng.random_range(current_idx + 1..num_commits);
                smallvec![oids[parent_idx]]
            }
        } else {
            let merge_chance = 0.15;
            let skip_chance = 0.1;

            if remaining >= 2 && rng.random_bool(merge_chance) {
                let first_parent = current_idx + 1;
                let second_parent = rng.random_range(current_idx + 2..num_commits);
                smallvec![oids[first_parent], oids[second_parent]]
            } else if rng.random_bool(skip_chance) && remaining >= 2 {
                let skip = rng.random_range(1..remaining.min(3));
                smallvec![oids[current_idx + 1 + skip]]
            } else {
                smallvec![oids[current_idx + 1]]
            }
        }
    }
}

use canvas_geometry::{
    COMMIT_CIRCLE_RADIUS, COMMIT_CIRCLE_STROKE_WIDTH, LINE_WIDTH, clear_lane_transition_dots,
    draw_commit_circle, graph_column_width_for, graph_row_extent, lane_center_x,
    lane_transition_height, stroke_lane_transition, to_row_center,
};
use collections::{BTreeMap, HashMap, HashSet};
use editor::Editor;
use git::{
    GitHostingProviderRegistry, Oid, parse_git_remote_url,
    repository::{InitialGraphCommitData, LogOrder, LogSource},
};
use git_ui::{
    commit_context_menu::{MultiCommitContext, build_multi_commit_context_menu},
    commit_view::CommitView,
    git_panel::{CommitSelection, CommitSelectionSource, Event as GitPanelEvent, GitPanel},
};
use gpui::{
    Anchor, AnyElement, App, Bounds, ClickEvent, DefiniteLength, DismissEvent, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, Hsla, Modifiers, MouseButton, MouseDownEvent,
    PathBuilder, Pixels, Point, Rems, ScrollStrategy, SharedString, Subscription, Task, TextRun,
    WeakEntity, Window, actions, anchored, deferred, point, prelude::*, px,
};
use menu::{Cancel, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use project::{
    ProjectPath,
    git_store::{
        CommitDataState, GitGraphEvent, GitStore, GitStoreEvent, GraphDataResponse, Repository,
        RepositoryEvent, RepositoryId,
    },
};
use project_panel::ProjectPanel;
use search::{
    SearchOption, SearchOptions, SearchSource, ToggleCaseSensitive, ToggleRegex, buffer_search,
};
use settings::Settings as _;
use smallvec::{SmallVec, smallvec};
use std::{
    ops::Range,
    rc::Rc,
    sync::{Arc, OnceLock},
    time::Duration,
};
use theme::AccentColors;
use theme_settings::ThemeSettings;
use time::{OffsetDateTime, UtcOffset, format_description::BorrowedFormatItem};
use ui::{
    Chip, ColumnWidthConfig, CommonAnimationExt as _, ContextMenu, HeaderResizeInfo,
    RedistributableColumnsState, Table, TableInteractionState, TableRenderContext,
    TableResizeBehavior, Tooltip, bind_redistributable_columns, prelude::*,
    render_redistributable_columns_resize_handles, render_table_header, table_row::TableRow,
};
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabTooltipContent},
};

/// Index of the Description column, which the commit graph is drawn over.
const DESCRIPTION_COLUMN_IDX: usize = 0;

/// Column shares used before the table has been measured — the very first
/// frame, where `cached_container_width` is still zero. Every later frame
/// re-derives them from the content (see [`default_column_fractions`]).
const UNMEASURED_COLUMN_FRACTIONS: [f32; 3] = [0.74, 0.13, 0.13];

/// Smallest share of the log table the Description column keeps. Below this the
/// Date and Author content widths are scaled back together: on a container too
/// narrow for all three, squeezing the two short columns is better than
/// starving the one that is actually being read.
const MIN_DESCRIPTION_FRACTION: f32 = 0.4;

/// The Date column's content is fixed-width: [`format_timestamp`] renders
/// `[day] [month repr:short] [year] [hour]:[minute]`, which is always a
/// two-digit day, a three-letter month, a four-digit year and `HH:MM`. So any
/// instance of that shape measures the column exactly, and `May` is picked for
/// the month because `M` is the widest glyph in the set of twelve.
/// `test_date_column_sample_matches_the_formatter` fails if the format changes
/// out from under this.
const DATE_COLUMN_SAMPLE: &str = "30 May 2026 12:04";

/// Author names have no fixed width, so unlike the Date sample this is a
/// *default* rather than a measurement: room for a full "Firstname Lastname"
/// and no more, with the divider left draggable for the repository where every
/// author is `dependabot[bot]`. It is measured in the UI font rather than
/// hard-coded in pixels so it tracks `ui_font_size` the way the row height does.
const AUTHOR_COLUMN_SAMPLE: &str = "Firstname Lastname";

/// `ui::render_cell` wraps every cell in `px_1()` and gpui lays out with a
/// border box, so a column sized to its text alone clips it by the padding.
const COLUMN_CELL_PADDING: Rems = Rems(0.5);
// Extra vertical breathing room added to the UI line height when computing
// the git graph's row height, so commit dots and lines have space around them.
const ROW_VERTICAL_PADDING: Pixels = px(4.0);

/// Share of the log table each column takes by default, for a table
/// `container` wide. `date` and `author` are the measured widths of the two
/// columns' content.
///
/// A flat fraction cannot serve both widths this view is used at. The Date
/// string is a constant ~132px, which is ~20% of the Solution band's compact
/// half but only ~7% of a full-window pane: the previous flat 0.13 therefore
/// truncated *every* row in the band while leaving ~118px of dead whitespace
/// beside the same text at full width. Sizing Date and Author to their content
/// and letting Description absorb the remainder is both correct at every width
/// and what IDEA's log does.
///
/// Returns `[description, date, author]`, summing to 1.
fn default_column_fractions(date: Pixels, author: Pixels, container: Pixels) -> [f32; 3] {
    if container <= px(0.) {
        return UNMEASURED_COLUMN_FRACTIONS;
    }

    let date = (date / container).max(0.0);
    let author = (author / container).max(0.0);
    let sides = date + author;
    let sides_budget = 1.0 - MIN_DESCRIPTION_FRACTION;
    let (date, author) = if sides > sides_budget {
        let scale = sides_budget / sides;
        (date * scale, author * scale)
    } else {
        (date, author)
    };

    [1.0 - date - author, date, author]
}

/// Everything [`default_column_fractions`] derives from: the table's width and
/// the two measured content widths.
///
/// The derived columns are cached against all three rather than against the
/// table width alone, because the two measurements move with `rem_size` and the
/// UI font while the table's width does not. A width-only key therefore stays
/// satisfied across a font-size or theme change, and the columns keep the
/// previous font's sizing -- which is the same truncation the derivation exists
/// to remove, and is repaired by nothing short of a resize. Keying on the
/// function's own arguments cannot go stale by construction; keying on whatever
/// [`GitGraph::measured_column_width`] happens to read today would have to be
/// revisited every time it reads something new.
#[derive(Clone, Copy, PartialEq)]
struct ColumnWidthInputs {
    container: Pixels,
    date: Pixels,
    author: Pixels,
}

fn new_column_widths_state(fractions: [f32; 3]) -> RedistributableColumnsState {
    RedistributableColumnsState::new(
        3,
        fractions.map(DefiniteLength::Fraction).to_vec(),
        vec![
            TableResizeBehavior::Resizable,
            TableResizeBehavior::Resizable,
            TableResizeBehavior::Resizable,
        ],
    )
}

/// Whether a search string should be treated as a commit-hash lookup rather
/// than a message grep: all-hex and at least git's default short-hash length
/// (7), so ordinary words — even the odd 4-char hex word like `face`/`dead` —
/// still search commit messages.
fn is_hash_like(text: &str) -> bool {
    (7..=40).contains(&text.len()) && text.chars().all(|c| c.is_ascii_hexdigit())
}

struct SearchState {
    case_sensitive: bool,
    regex: bool,
    search_in_diffs: bool,
    editor: Entity<Editor>,
    /// Debounce timer for re-fetching the log when the query input changes.
    /// Replaced (and dropped, cancelling its timer) on every keystroke so
    /// only the last edit within the debounce window triggers a refetch.
    debounce_task: Option<Task<()>>,
    _editor_subscription: Subscription,
}

actions!(
    git_graph,
    [
        /// Opens the commit view for the selected commit.
        OpenCommitView,
        /// Focuses the search field.
        FocusSearch,
        /// Toggles whether the Query filter searches commit content (`-G`)
        /// instead of just commit messages (`--grep`). Slow on large
        /// histories.
        ToggleSearchInDiffs,
        /// Re-runs `git log` for the current filters, picking up commits that
        /// landed outside the editor.
        Refresh,
        /// Shows, focuses or hides the commit graph in the Solution band's
        /// utility section (`ctrl-alt-\``). Tri-state, the same one
        /// `console_panel::ToggleFocus` and `debug_panel::ToggleFocus`
        /// implement for the other two occupants of that section: hidden (or
        /// showing another kind) → select the graph, show it and focus it;
        /// shown but unfocused → focus it; shown and focused → hide the
        /// section. See `git_graph_panel::handle_toggle_focus`.
        ///
        /// Bound in the `"Workspace"` context on all three platforms.
        /// `ctrl-alt-\`` was picked because every mnemonic chord for "git"
        /// is taken by something more useful in a context that would shadow
        /// it: `ctrl-shift-g` is `git_panel::ToggleFocus` in the same
        /// `"Workspace"` block, and `ctrl-alt-g` / `ctrl-alt-shift-g` are
        /// `search::Select{Next,Previous}Match` in `"Pane"`, which is in the
        /// focus path almost always. This chord is the terminal sibling's
        /// (`ctrl-\``) neighbour on the same physical key, and it is unbound
        /// in every keymap this repo ships, including the vim and
        /// jetbrains/sublime/emacs alternatives — so it fires even from
        /// inside the terminal or an editor, unlike `ctrl-shift-a`.
        ToggleFocus,
    ]
);

/// S-CTM cross-link to S-FLT — emitted when the commit context menu's
/// "Show Affected Paths in Log" entry is invoked. The handler in
/// `GitGraph::on_action` calls `set_path_filter(paths, cx)`, scoping the
/// log to commits that touch one of the listed paths.
#[derive(
    Clone, PartialEq, Debug, Default, serde::Deserialize, schemars::JsonSchema, gpui::Action,
)]
#[action(namespace = git_graph)]
pub struct ShowAffectedPathsInLog {
    pub paths: Vec<String>,
}

/// View-level mode for the [`GitGraph`] surface. Derived from `log_source` —
/// `LogSource::Path(_)` projects to [`GraphMode::FileHistory`], everything
/// else projects to [`GraphMode::Full`]. Code that needs to switch behavior
/// based on the preset (e.g. toolbar toggle visibility) calls
/// [`GitGraph::mode`] instead of pattern-matching `log_source` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphMode {
    Full,
    FileHistory,
}

/// Strip the prefixes git can emit in `%D` decorations
/// (`HEAD -> `, `tag: `, `refs/heads/`, `refs/remotes/<remote>/`) so
/// the bare branch name is what reaches downstream callers — matters
/// for branch-protection glob matching, where `release/*` should
/// match the branch `release/v1` regardless of upstream-tracking
/// shape.
fn strip_ref_namespace(name: &str) -> &str {
    let s = name.trim();
    if let Some(after) = s.strip_prefix("HEAD -> ") {
        return strip_ref_namespace(after);
    }
    if let Some(after) = s.strip_prefix("tag: ") {
        return after;
    }
    if let Some(after) = s.strip_prefix("refs/heads/") {
        return after;
    }
    if let Some(after) = s.strip_prefix("refs/remotes/") {
        // refs/remotes/<remote>/<branch> — drop the remote segment so
        // the policy match is on the branch portion alone.
        if let Some((_remote, rest)) = after.split_once('/') {
            return rest;
        }
        return after;
    }
    s
}

fn timestamp_format() -> &'static [BorrowedFormatItem<'static>] {
    static FORMAT: OnceLock<Vec<BorrowedFormatItem<'static>>> = OnceLock::new();
    FORMAT.get_or_init(|| {
        time::format_description::parse("[day] [month repr:short] [year] [hour]:[minute]")
            .unwrap_or_default()
    })
}

fn format_timestamp(timestamp: i64) -> String {
    let Ok(datetime) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return "Unknown".to_string();
    };

    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let local_datetime = datetime.to_offset(local_offset);

    local_datetime
        .format(timestamp_format())
        .unwrap_or_default()
}

/// Local-day label used by the "Group by date" view option to insert a
/// header above the first commit of each day. Returns `None` when the
/// timestamp is unparseable. Two commits whose labels are equal share a
/// header.
fn local_day_label(timestamp: i64) -> Option<String> {
    let datetime = OffsetDateTime::from_unix_timestamp(timestamp).ok()?;
    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let local = datetime.to_offset(local_offset);
    let format = time::format_description::parse("[year]-[month]-[day]").ok()?;
    local.format(&format).ok()
}

fn accent_colors_count(accents: &AccentColors) -> usize {
    accents.0.len()
}

#[derive(Copy, Clone, Debug)]
struct BranchColor(u8);

#[derive(Debug)]
enum LaneState {
    Empty,
    Active {
        child: Oid,
        parent: Oid,
        color: Option<BranchColor>,
        starting_row: usize,
        starting_col: usize,
        destination_column: Option<usize>,
        segments: SmallVec<[CommitLineSegment; 1]>,
    },
}

impl LaneState {
    fn to_commit_lines(
        &mut self,
        ending_row: usize,
        lane_column: usize,
        parent_column: usize,
        parent_color: BranchColor,
    ) -> Option<CommitLine> {
        let state = std::mem::replace(self, LaneState::Empty);

        match state {
            LaneState::Active {
                #[cfg_attr(not(test), allow(unused_variables))]
                parent,
                #[cfg_attr(not(test), allow(unused_variables))]
                child,
                color,
                starting_row,
                starting_col,
                destination_column,
                mut segments,
            } => {
                let final_destination = destination_column.unwrap_or(parent_column);
                let final_color = color.unwrap_or(parent_color);

                Some(CommitLine {
                    #[cfg(test)]
                    child,
                    #[cfg(test)]
                    parent,
                    child_column: starting_col,
                    full_interval: starting_row..ending_row,
                    color_idx: final_color.0 as usize,
                    segments: {
                        match segments.last_mut() {
                            Some(CommitLineSegment::Straight { to_row })
                                if *to_row == usize::MAX =>
                            {
                                if final_destination != lane_column {
                                    *to_row = ending_row - 1;

                                    let curved_line = CommitLineSegment::Curve {
                                        to_column: final_destination,
                                        on_row: ending_row,
                                        curve_kind: CurveKind::Checkout,
                                    };

                                    if *to_row == starting_row {
                                        let last_index = segments.len() - 1;
                                        segments[last_index] = curved_line;
                                    } else {
                                        segments.push(curved_line);
                                    }
                                } else {
                                    *to_row = ending_row;
                                }
                            }
                            Some(CommitLineSegment::Curve {
                                on_row,
                                to_column,
                                curve_kind,
                            }) if *on_row == usize::MAX => {
                                if *to_column == usize::MAX {
                                    *to_column = final_destination;
                                }
                                if matches!(curve_kind, CurveKind::Merge) {
                                    *on_row = starting_row + 1;
                                    if *on_row < ending_row {
                                        if *to_column != final_destination {
                                            segments.push(CommitLineSegment::Straight {
                                                to_row: ending_row - 1,
                                            });
                                            segments.push(CommitLineSegment::Curve {
                                                to_column: final_destination,
                                                on_row: ending_row,
                                                curve_kind: CurveKind::Checkout,
                                            });
                                        } else {
                                            segments.push(CommitLineSegment::Straight {
                                                to_row: ending_row,
                                            });
                                        }
                                    } else if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    }
                                } else {
                                    *on_row = ending_row;
                                    if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row,
                                        });
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    }
                                }
                            }
                            Some(CommitLineSegment::Curve {
                                on_row, to_column, ..
                            }) => {
                                if *on_row < ending_row {
                                    if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row - 1,
                                        });
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    } else {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row,
                                        });
                                    }
                                } else if *to_column != final_destination {
                                    segments.push(CommitLineSegment::Curve {
                                        to_column: final_destination,
                                        on_row: ending_row,
                                        curve_kind: CurveKind::Checkout,
                                    });
                                }
                            }
                            _ => {}
                        }

                        segments
                    },
                })
            }
            LaneState::Empty => None,
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            LaneState::Empty => true,
            LaneState::Active { .. } => false,
        }
    }
}

struct CommitEntry {
    data: Arc<InitialGraphCommitData>,
    lane: usize,
    color_idx: usize,
}

type ActiveLaneIdx = usize;

enum AllCommitCount {
    NotLoaded,
    Loaded(usize),
}

#[derive(Debug)]
enum CurveKind {
    Merge,
    Checkout,
}

#[derive(Debug)]
enum CommitLineSegment {
    Straight {
        to_row: usize,
    },
    Curve {
        to_column: usize,
        on_row: usize,
        curve_kind: CurveKind,
    },
}

#[derive(Debug)]
struct CommitLine {
    #[cfg(test)]
    child: Oid,
    #[cfg(test)]
    parent: Oid,
    child_column: usize,
    full_interval: Range<usize>,
    color_idx: usize,
    segments: SmallVec<[CommitLineSegment; 1]>,
}

impl CommitLine {
    fn get_first_visible_segment_idx(&self, first_visible_row: usize) -> Option<(usize, usize)> {
        if first_visible_row > self.full_interval.end {
            return None;
        } else if first_visible_row <= self.full_interval.start {
            return Some((0, self.child_column));
        }

        let mut current_column = self.child_column;

        for (idx, segment) in self.segments.iter().enumerate() {
            match segment {
                CommitLineSegment::Straight { to_row } => {
                    if *to_row >= first_visible_row {
                        return Some((idx, current_column));
                    }
                }
                CommitLineSegment::Curve {
                    to_column, on_row, ..
                } => {
                    if *on_row >= first_visible_row {
                        return Some((idx, current_column));
                    }
                    current_column = *to_column;
                }
            }
        }

        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CommitLineKey {
    child: Oid,
    parent: Oid,
}

struct GraphData {
    lane_states: SmallVec<[LaneState; 8]>,
    lane_colors: HashMap<ActiveLaneIdx, BranchColor>,
    parent_to_lanes: HashMap<Oid, SmallVec<[usize; 1]>>,
    next_color: BranchColor,
    accent_colors_count: usize,
    commits: Vec<Rc<CommitEntry>>,
    max_commit_count: AllCommitCount,
    /// Widest row's occupancy over all loaded commits — the width the graph
    /// column has to reserve. Monotonically non-decreasing; reset by `clear`.
    max_lanes: usize,
    /// Number of lane columns occupied *at each row* — the commit's own lane,
    /// every lane still live across that row, and every lane that terminates
    /// on it. Parallel to `commits`. This is what indents each row's subject
    /// text, so it must be the row's own occupancy, not the running maximum.
    max_column_at_row: Vec<u16>,
    lines: Vec<Rc<CommitLine>>,
    active_commit_lines: HashMap<CommitLineKey, usize>,
    active_commit_lines_by_parent: HashMap<Oid, SmallVec<[usize; 1]>>,
}

impl GraphData {
    fn new(accent_colors_count: usize) -> Self {
        GraphData {
            lane_states: SmallVec::default(),
            lane_colors: HashMap::default(),
            parent_to_lanes: HashMap::default(),
            next_color: BranchColor(0),
            accent_colors_count,
            commits: Vec::default(),
            max_commit_count: AllCommitCount::NotLoaded,
            max_lanes: 0,
            max_column_at_row: Vec::default(),
            lines: Vec::default(),
            active_commit_lines: HashMap::default(),
            active_commit_lines_by_parent: HashMap::default(),
        }
    }

    fn clear(&mut self) {
        self.lane_states.clear();
        self.lane_colors.clear();
        self.parent_to_lanes.clear();
        self.commits.clear();
        self.lines.clear();
        self.active_commit_lines.clear();
        self.active_commit_lines_by_parent.clear();
        self.next_color = BranchColor(0);
        self.max_commit_count = AllCommitCount::NotLoaded;
        self.max_lanes = 0;
        self.max_column_at_row.clear();
    }

    /// Lane columns occupied at `row`, floored at 1 so a row that somehow has
    /// no recorded occupancy still indents past its own commit dot.
    fn columns_at_row(&self, row: usize) -> usize {
        self.max_column_at_row
            .get(row)
            .copied()
            .unwrap_or(1)
            .max(1)
            .into()
    }

    fn first_empty_lane_idx(&mut self) -> ActiveLaneIdx {
        self.lane_states
            .iter()
            .position(LaneState::is_empty)
            .unwrap_or_else(|| {
                self.lane_states.push(LaneState::Empty);
                self.lane_states.len() - 1
            })
    }

    fn get_lane_color(&mut self, lane_idx: ActiveLaneIdx) -> BranchColor {
        let accent_colors_count = self.accent_colors_count;
        *self.lane_colors.entry(lane_idx).or_insert_with(|| {
            let color_idx = self.next_color;
            self.next_color = BranchColor((self.next_color.0 + 1) % accent_colors_count as u8);
            color_idx
        })
    }

    fn add_commits(&mut self, commits: &[Arc<InitialGraphCommitData>]) {
        self.commits.reserve(commits.len());
        self.max_column_at_row.reserve(commits.len());
        self.lines.reserve(commits.len() / 2);

        for commit in commits.iter() {
            let commit_row = self.commits.len();
            // Lanes that end on this row still draw into it, so they count
            // towards its occupancy even though they are `Empty` by the time
            // the row's width is computed below.
            let mut terminated_columns = 0usize;

            let commit_lane = self
                .parent_to_lanes
                .get(&commit.sha)
                .and_then(|lanes| lanes.first().copied());

            let commit_lane = commit_lane.unwrap_or_else(|| self.first_empty_lane_idx());

            let commit_color = self.get_lane_color(commit_lane);

            if let Some(lanes) = self.parent_to_lanes.remove(&commit.sha) {
                for lane_column in lanes {
                    terminated_columns = terminated_columns.max(lane_column + 1);
                    let state = &mut self.lane_states[lane_column];

                    if let LaneState::Active {
                        starting_row,
                        segments,
                        ..
                    } = state
                    {
                        if let Some(CommitLineSegment::Curve {
                            to_column,
                            curve_kind: CurveKind::Merge,
                            ..
                        }) = segments.first_mut()
                        {
                            let curve_row = *starting_row + 1;
                            let would_overlap =
                                if lane_column != commit_lane && curve_row < commit_row {
                                    self.commits[curve_row..commit_row]
                                        .iter()
                                        .any(|c| c.lane == commit_lane)
                                } else {
                                    false
                                };

                            if would_overlap {
                                *to_column = lane_column;
                            }
                        }
                    }

                    if let Some(commit_line) =
                        state.to_commit_lines(commit_row, lane_column, commit_lane, commit_color)
                    {
                        self.lines.push(Rc::new(commit_line));
                    }
                }
            }

            commit
                .parents
                .iter()
                .enumerate()
                .for_each(|(parent_idx, parent)| {
                    if parent_idx == 0 {
                        self.lane_states[commit_lane] = LaneState::Active {
                            parent: *parent,
                            child: commit.sha,
                            color: Some(commit_color),
                            starting_col: commit_lane,
                            starting_row: commit_row,
                            destination_column: None,
                            segments: smallvec![CommitLineSegment::Straight { to_row: usize::MAX }],
                        };

                        self.parent_to_lanes
                            .entry(*parent)
                            .or_default()
                            .push(commit_lane);
                    } else {
                        let new_lane = self.first_empty_lane_idx();

                        self.lane_states[new_lane] = LaneState::Active {
                            parent: *parent,
                            child: commit.sha,
                            color: None,
                            starting_col: commit_lane,
                            starting_row: commit_row,
                            destination_column: None,
                            segments: smallvec![CommitLineSegment::Curve {
                                to_column: usize::MAX,
                                on_row: usize::MAX,
                                curve_kind: CurveKind::Merge,
                            },],
                        };

                        self.parent_to_lanes
                            .entry(*parent)
                            .or_default()
                            .push(new_lane);
                    }
                });

            // `lane_states` never shrinks — a freed lane stays in the vector as
            // `Empty` so its index can be reused — so its length overstates how
            // far right the row actually reaches. Measure the last *occupied*
            // slot instead.
            let live_columns = self
                .lane_states
                .iter()
                .rposition(|lane| !lane.is_empty())
                .map_or(0, |last| last + 1);
            let occupied_columns = live_columns
                .max(terminated_columns)
                .max(commit_lane + 1)
                .min(u16::MAX as usize);

            self.max_lanes = self.max_lanes.max(occupied_columns);
            self.max_column_at_row.push(occupied_columns as u16);

            self.commits.push(Rc::new(CommitEntry {
                data: commit.clone(),
                lane: commit_lane,
                color_idx: commit_color.0 as usize,
            }));
        }

        self.max_commit_count = AllCommitCount::Loaded(self.commits.len());
    }
}

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<GitGraph>(cx);
    mcp::register(cx);

    cx.observe_new(|workspace: &mut workspace::Workspace, _, _| {
        workspace.register_action(git_graph_panel::handle_toggle_focus);
        workspace.register_action_renderer(|div, workspace, window, cx| {
            div.when_some(
                resolve_file_history_target(workspace, window, cx),
                |div, (repo_id, log_source)| {
                    let git_store = workspace.project().read(cx).git_store().clone();
                    let workspace = workspace.weak_handle();

                    div.on_action(move |_: &git::FileHistory, window, cx| {
                        let git_store = git_store.clone();
                        workspace
                            .update(cx, |workspace, cx| {
                                open_or_reuse_graph(
                                    workspace,
                                    repo_id,
                                    git_store,
                                    log_source.clone(),
                                    None,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    })
                },
            )
            .when(
                workspace.project().read(cx).active_repository(cx).is_some(),
                |div| {
                    let workspace = workspace.weak_handle();

                    div.on_action({
                        let workspace = workspace.clone();
                        move |_: &git_ui::git_panel::Open, window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    let Some(repo) =
                                        workspace.project().read(cx).active_repository(cx)
                                    else {
                                        return;
                                    };
                                    let selected_repo_id = repo.read(cx).id;

                                    let git_store =
                                        workspace.project().read(cx).git_store().clone();
                                    open_or_reuse_graph(
                                        workspace,
                                        selected_repo_id,
                                        git_store,
                                        LogSource::All,
                                        None,
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        }
                    })
                    .on_action(
                        move |action: &git_ui::git_panel::OpenAtCommit, window, cx| {
                            let sha = action.sha.clone();
                            workspace
                                .update(cx, |workspace, cx| {
                                    let Some(repo) =
                                        workspace.project().read(cx).active_repository(cx)
                                    else {
                                        return;
                                    };
                                    let selected_repo_id = repo.read(cx).id;

                                    let git_store =
                                        workspace.project().read(cx).git_store().clone();
                                    open_or_reuse_graph(
                                        workspace,
                                        selected_repo_id,
                                        git_store,
                                        LogSource::All,
                                        Some(sha),
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        },
                    )
                },
            )
        });
    })
    .detach();
}

fn resolve_file_history_target(
    workspace: &Workspace,
    window: &Window,
    cx: &App,
) -> Option<(RepositoryId, LogSource)> {
    if let Some(panel) = workspace.panel::<ProjectPanel>(cx)
        && panel.read(cx).focus_handle(cx).contains_focused(window, cx)
        && let Some(project_path) = panel.read(cx).selected_file_project_path(cx)
    {
        let git_store = workspace.project().read(cx).git_store();
        let (repo, repo_path) = git_store
            .read(cx)
            .repository_and_path_for_project_path(&project_path, cx)?;
        return Some((repo.read(cx).id, LogSource::Path(repo_path)));
    }

    if let Some(panel) = workspace.panel::<git_ui::git_panel::GitPanel>(cx)
        && panel.read(cx).focus_handle(cx).contains_focused(window, cx)
        && let Some((repository, repo_path)) = panel.read(cx).selected_file_history_target()
    {
        return Some((repository.read(cx).id, LogSource::Path(repo_path)));
    }

    let editor = workspace.active_item_as::<Editor>(cx)?;

    let file = editor
        .read(cx)
        .file_at(editor.read(cx).selections.newest_anchor().head(), cx)?;
    let project_path = ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    };

    let git_store = workspace.project().read(cx).git_store();
    let (repo, repo_path) = git_store
        .read(cx)
        .repository_and_path_for_project_path(&project_path, cx)?;
    Some((repo.read(cx).id, LogSource::Path(repo_path)))
}

fn open_or_reuse_graph(
    workspace: &mut Workspace,
    repo_id: RepositoryId,
    git_store: Entity<GitStore>,
    log_source: LogSource,
    sha: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<GitGraph>(cx).find(|graph| {
        let graph = graph.read(cx);
        graph.repo_id == repo_id && graph.log_source == log_source
    });

    if let Some(existing) = existing {
        if let Some(sha) = sha {
            existing.update(cx, |graph, cx| {
                graph.select_commit_by_sha(
                    sha.as_str(),
                    CommitSelectionSource::UserGesture,
                    window,
                    cx,
                );
            });
        }
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }

    let workspace_handle = workspace.weak_handle();
    let git_graph = cx.new(|cx| {
        let mut graph = GitGraph::new(
            repo_id,
            git_store,
            workspace_handle,
            Some(log_source),
            window,
            cx,
        );
        if let Some(sha) = sha {
            graph.select_commit_by_sha(
                sha.as_str(),
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
        }
        graph
    });
    workspace.add_item_to_active_pane(Box::new(git_graph), None, true, window, cx);
}

pub struct GitGraph {
    focus_handle: FocusHandle,
    search_state: SearchState,
    graph_data: GraphData,
    git_store: Entity<GitStore>,
    workspace: WeakEntity<Workspace>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    table_interaction_state: Entity<TableInteractionState>,
    column_widths: Entity<RedistributableColumnsState>,
    /// Width the log table was last laid out at, from
    /// [`GitGraph::observe_table_width`].
    table_width: Pixels,
    /// Inputs the current `column_widths` defaults were derived from, or `None`
    /// before the first derivation. See [`GitGraph::sync_default_column_widths`].
    auto_column_widths_for: Option<ColumnWidthInputs>,
    selected_entry_idx: Option<usize>,
    /// Every selected row, in view space. Multi-row selections only come from
    /// Ctrl/Shift clicks on commit rows; every other path into
    /// [`GitGraph::select_entry`] collapses this back to the one active row.
    /// Invariants: empty exactly while [`GitGraph::selected_entry_idx`] is
    /// `None`, and otherwise always contains it.
    selected_entry_idxs: HashSet<usize>,
    /// Origin row of a Shift+click range, in view space. Moved by every
    /// plain/Ctrl click on a row and cleared by [`GitGraph::select_entry`], so
    /// keyboard navigation restarts the range from wherever it lands.
    selection_anchor_idx: Option<usize>,
    hovered_entry_idx: Option<usize>,
    log_source: LogSource,
    log_order: LogOrder,
    /// Chip-based log filters (Branch / User / Date / Path / Query). S-FLT
    /// scaffolding — fields exist, chip UI + plumbing through
    /// `repository::initial_graph_data` lands per-chip in follow-ups.
    filters: filters::LogFilters,
    /// Row-decoration toggles (My commits / New since refresh). S-FLT
    /// scaffolding — wired when chip-Highlights toolbar lands.
    highlights: highlights::HighlightSet,
    /// Watches the git panel for [`GitPanelEvent::CommitTabClosed`]. Installed
    /// on the first selection push (see [`GitGraph::observe_git_panel`]).
    _git_panel_subscription: Option<Subscription>,
    /// Render-only toggles (Compact refs / Group by date) applied at row
    /// rendering time without re-running `git log`.
    view_options: view_options::ViewOptions,
    /// Toggle state specific to the file-history (S-FHT) preset. Only
    /// surfaced in the toolbar when [`Self::mode`] is
    /// [`GraphMode::FileHistory`]; otherwise unused.
    file_history_options: file_history::FileHistoryOptions,
    /// Email reported by `git config user.email`, captured at view init.
    /// Used by the My-commits highlight to compare against per-commit
    /// `author_email`. `None` until the background fetch resolves.
    local_user_email: Option<SharedString>,
    /// Configured remote names (`origin`, `upstream`, …), captured at
    /// view init. The commit context menu splits a `%D` remote-tracking
    /// token against this list instead of on the first `/`, since a
    /// remote name may itself contain one. Empty until the background
    /// fetch resolves, and not refreshed afterwards: a remote added
    /// mid-session just means the menu withholds the server-side
    /// actions for its refs until the view is reopened.
    remote_names: Vec<SharedString>,
    repo_id: RepositoryId,
    pending_select_sha: Option<Oid>,
}

/// How a click on a commit row folds into the multi-row selection. Derived
/// from the click's modifiers at the call site so that [`fold_row_click`]
/// stays a pure function a unit test can drive without a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowSelectionGesture {
    /// Plain click: the clicked row becomes the whole selection.
    Replace,
    /// Ctrl (Cmd on macOS): add the clicked row to the selection, or drop it.
    Toggle,
    /// Shift: select the inclusive range between the anchor and the click.
    Range,
}

/// Outcome of folding a click into the current selection. `active` is the row
/// whose commit the git panel's Commit tab describes (it becomes
/// [`GitGraph::selected_entry_idx`]) and is always a member of `selected`.
#[derive(Debug, PartialEq, Eq)]
struct RowSelection {
    active: usize,
    selected: HashSet<usize>,
    anchor: Option<usize>,
}

/// Selection algebra behind [`GitGraph::apply_row_click_selection`].
///
/// `local_changes_row` says whether view-index 0 is the synthetic
/// working-tree row. That row has no commit behind it, so it can never take
/// part in a multi-row selection: a Ctrl/Shift click that would involve it
/// degrades to a plain click.
///
/// Toggling the last selected row off would leave the graph with nothing
/// selected and the Commit tab with no commit to describe, so the clicked row
/// stays selected instead. Toggling a row off while others remain moves
/// `active` to the nearest surviving row rather than to the row just
/// deselected — the tab has to describe a *selected* commit — while the
/// anchor still follows the click, so a subsequent Shift+click ranges from
/// where the user last clicked, as it does in IDEA.
fn fold_row_click(
    clicked_idx: usize,
    gesture: RowSelectionGesture,
    selected: &HashSet<usize>,
    anchor: Option<usize>,
    local_changes_row: bool,
) -> RowSelection {
    let is_local_changes_row = |idx: usize| local_changes_row && idx == 0;

    let gesture = match gesture {
        RowSelectionGesture::Replace => RowSelectionGesture::Replace,
        _ if is_local_changes_row(clicked_idx) => RowSelectionGesture::Replace,
        RowSelectionGesture::Range if anchor.is_none_or(is_local_changes_row) => {
            RowSelectionGesture::Replace
        }
        other => other,
    };

    match gesture {
        RowSelectionGesture::Replace => RowSelection {
            active: clicked_idx,
            selected: HashSet::from_iter([clicked_idx]),
            anchor: Some(clicked_idx),
        },
        RowSelectionGesture::Toggle => {
            let mut selected = selected.clone();
            selected.retain(|idx| !is_local_changes_row(*idx));
            let active;
            if selected.remove(&clicked_idx) {
                match nearest_selected_row(&selected, clicked_idx) {
                    Some(nearest) => active = nearest,
                    None => {
                        selected.insert(clicked_idx);
                        active = clicked_idx;
                    }
                }
            } else {
                selected.insert(clicked_idx);
                active = clicked_idx;
            }
            RowSelection {
                active,
                selected,
                anchor: Some(clicked_idx),
            }
        }
        RowSelectionGesture::Range => {
            let anchor_idx = anchor.unwrap_or(clicked_idx);
            let (first, last) = if anchor_idx <= clicked_idx {
                (anchor_idx, clicked_idx)
            } else {
                (clicked_idx, anchor_idx)
            };
            RowSelection {
                active: clicked_idx,
                selected: (first..=last)
                    .filter(|idx| !is_local_changes_row(*idx))
                    .collect(),
                anchor: Some(anchor_idx),
            }
        }
    }
}

/// The selected row closest to `idx`, preferring the row above on a tie.
fn nearest_selected_row(selected: &HashSet<usize>, idx: usize) -> Option<usize> {
    selected
        .iter()
        .copied()
        .min_by_key(|candidate| (candidate.abs_diff(idx), *candidate))
}

/// True when `selection` — commit shas paired with their first parent,
/// **oldest first** — is an unbroken first-parent chain: every commit but the
/// oldest has the entry before it as its first parent. A single commit is
/// trivially a chain; an empty selection is too.
fn is_first_parent_chain(selection: &[(Oid, Option<Oid>)]) -> bool {
    selection
        .windows(2)
        .all(|pair| pair[1].1 == Some(pair[0].0))
}

impl GitGraph {
    /// Drop the loaded log (and everything derived from it) so the caller can
    /// refetch it.
    ///
    /// The selection is re-anchored by sha rather than kept as a row index:
    /// the refetched log may insert, drop or reorder commits, so the row that
    /// was selected can end up pointing at a different commit. Keeping the
    /// index paired the newly-arrived commit's header with the previous
    /// commit's file list, and clicking one of those files opened an empty
    /// tab. An explicit pending request (`select_commit_by_sha` for a commit
    /// that isn't loaded yet) wins over the re-anchor.
    ///
    /// Nothing is pushed at the git panel from here — the clear is a transient
    /// step of the refetch, not a deselection. The Commit tab is left standing
    /// until the re-anchor lands ([`CommitSelectionSource::Background`], so it
    /// cannot yank a user who has moved on to Changes) or gives up, which
    /// closes the tab through [`Self::close_vanished_commit_tab`].
    fn invalidate_state(&mut self, cx: &mut Context<Self>) {
        if self.pending_select_sha.is_none() {
            self.pending_select_sha = self.selected_commit_sha();
        }
        self.selected_entry_idx = None;
        self.selected_entry_idxs.clear();
        self.selection_anchor_idx = None;
        self.graph_data.clear();
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    /// Sha of the commit the git panel's Commit tab is currently describing,
    /// if any. `None` for the synthetic local-changes row, which has no commit
    /// data.
    fn selected_commit_sha(&self) -> Option<Oid> {
        let data_idx = self.view_to_data_idx(self.selected_entry_idx?)?;
        Some(self.graph_data.commits.get(data_idx)?.data.sha)
    }

    /// Shas of every selected row, in graph order.
    ///
    /// The selection is held in view space, where row 0 can be the synthetic
    /// local-changes row; that row has no commit and drops out here rather
    /// than being reported as one. An empty result therefore means "nothing
    /// commit-shaped is selected", which is what the git panel's Commit tab
    /// treats as a deselection.
    fn selected_commit_shas(&self) -> Vec<Oid> {
        let mut view_idxs: Vec<usize> = self.selected_entry_idxs.iter().copied().collect();
        view_idxs.sort_unstable();
        view_idxs
            .into_iter()
            .filter_map(|view_idx| self.view_to_data_idx(view_idx))
            .filter_map(|data_idx| self.graph_data.commits.get(data_idx))
            .map(|commit| commit.data.sha)
            .collect()
    }

    /// Mirror the row selection into the git panel's Commit tab, or close that
    /// tab when nothing commit-shaped is selected.
    ///
    /// Deferred rather than called inline for two reasons. `select_entry` is
    /// reachable from the repository-event and deserialize paths, where the
    /// workspace is already leased and a synchronous `workspace.update` would
    /// panic; and the deferred closure re-reads the selection after the whole
    /// gesture has settled, so `apply_row_click_selection` writing its
    /// multi-row set back after `select_entry` returns is seen, not missed.
    fn push_selection_to_git_panel(
        &self,
        source: CommitSelectionSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.defer_in(window, move |this, window, cx| {
            let Some(workspace) = this.workspace.upgrade() else {
                return;
            };
            let Some(panel) = workspace.read(cx).panel::<GitPanel>(cx) else {
                return;
            };
            this.observe_git_panel(&panel, window, cx);

            let shas = this.selected_commit_shas();
            let repository = this.get_repository(cx);
            panel.update(cx, |panel, cx| match repository {
                Some(repository) if !shas.is_empty() => {
                    panel.show_commit_selection(
                        CommitSelection { repository, shas },
                        source,
                        window,
                        cx,
                    );
                }
                _ => panel.close_commit_tab(window, cx),
            });
        });
    }

    /// Close the git panel's Commit tab when the commit it was opened for has
    /// vanished from the refetched log.
    ///
    /// `git commit --amend` or a rebase run in a terminal rewrites the sha, so
    /// the re-anchor in [`Self::on_repository_event`] finds nothing to select
    /// and pushes nothing. The graph then shows no selection while the tab
    /// still describes the old commit and its file rows still ask
    /// [`CommitView`] for diffs of a sha that no longer resolves.
    ///
    /// Scoped to a tab describing exactly that one commit: a pending sha can
    /// also come from a [`Self::select_commit_by_sha`] request for a commit
    /// that never loaded, which says nothing about whatever tab is open.
    fn close_vanished_commit_tab(&self, sha: Oid, window: &mut Window, cx: &mut Context<Self>) {
        cx.defer_in(window, move |this, window, cx| {
            let Some(workspace) = this.workspace.upgrade() else {
                return;
            };
            let Some(panel) = workspace.read(cx).panel::<GitPanel>(cx) else {
                return;
            };
            panel.update(cx, |panel, cx| {
                if panel.commit_tab_shas() == [sha] {
                    panel.close_commit_tab(window, cx);
                }
            });
        });
    }

    /// Watch the git panel so that closing the Commit tab drops the row
    /// selection that opened it.
    ///
    /// Idempotent and installed from the first push rather than eagerly in
    /// [`GitGraph::new`]: the panel is registered on the workspace by a
    /// spawned task, so a graph built during workspace deserialization can run
    /// its constructor before the panel exists. The push resolves the panel
    /// anyway, and no Commit tab can be open to close before one has happened.
    fn observe_git_panel(
        &mut self,
        panel: &Entity<GitPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self._git_panel_subscription.is_some() {
            return;
        }
        self._git_panel_subscription = Some(cx.subscribe_in(
            panel,
            window,
            |this, _panel, event, _window, cx| match event {
                GitPanelEvent::CommitTabClosed(closed_shas) => {
                    // Only the graph whose own selection the tab was describing
                    // drops it. Another graph in the window — one pinned to a
                    // different repository, or just holding a different row —
                    // keeps what it had. This also covers the local-changes
                    // row, which closes the tab (it is not a commit) while the
                    // graph's own shas are empty: the mismatch stops the close
                    // bouncing back and deselecting the row just clicked.
                    if *closed_shas != this.selected_commit_shas() {
                        return;
                    }
                    this.clear_selection();
                    cx.notify();
                }
                GitPanelEvent::Focus => {}
            },
        ));
    }

    /// Re-run `git log` for the current filters, keeping the selection
    /// anchored by sha (see [`GitGraph::invalidate_state`]).
    ///
    /// The repository memoises each log result keyed by its filter args and
    /// only evicts that entry for moves it can observe (head, branch list,
    /// tag list). Commits that arrive some other way — a fetch or rebase run
    /// in a terminal, a worktree edited by another process — stay invisible
    /// until the entry is dropped, so an explicit refresh has to evict it
    /// before asking for the data again.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        // "Highlight new commits since last refresh" is anchored on the head
        // of the log as it was last seen. Re-anchor on the commit currently at
        // the top *before* the reload throws it away, so anything the reload
        // brings in above it is decorated as new.
        if self.highlights.new_since_refresh {
            self.highlights.last_seen_sha = self.graph_data.commits.first().map(|c| c.data.sha);
        }
        if let Some(repository) = self.get_repository(cx) {
            repository.update(cx, |repository, _| repository.clear_graph_data_cache());
        }
        self.invalidate_state(cx);
        self.fetch_initial_graph_data(cx);
    }

    pub fn set_date_filter(&mut self, range: Option<filters::DateRange>, cx: &mut Context<Self>) {
        if self.filters.date_range == range {
            return;
        }
        self.filters.date_range = range;
        self.invalidate_state(cx);
        self.fetch_initial_graph_data(cx);
    }

    pub fn set_branch_filter(&mut self, branches: Vec<SharedString>, cx: &mut Context<Self>) {
        if self.filters.branches == branches {
            return;
        }
        self.filters.branches = branches;
        self.invalidate_state(cx);
        self.fetch_initial_graph_data(cx);
    }

    pub fn set_user_filter(&mut self, authors: Vec<SharedString>, cx: &mut Context<Self>) {
        if self.filters.authors == authors {
            return;
        }
        self.filters.authors = authors;
        self.invalidate_state(cx);
        self.fetch_initial_graph_data(cx);
    }

    pub fn set_path_filter(
        &mut self,
        paths: Vec<git::repository::RepoPath>,
        cx: &mut Context<Self>,
    ) {
        if self.filters.paths == paths {
            return;
        }
        self.filters.paths = paths;
        self.invalidate_state(cx);
        self.fetch_initial_graph_data(cx);
    }

    pub fn set_query_filter(
        &mut self,
        query: Option<filters::QueryFilter>,
        cx: &mut Context<Self>,
    ) {
        if self.filters.query == query {
            return;
        }
        self.filters.query = query;
        self.invalidate_state(cx);
        self.fetch_initial_graph_data(cx);
    }

    /// Debounce text-input changes — overwriting the prior task drops it,
    /// which cancels the in-flight timer so only the last keystroke within
    /// a 250ms window triggers a `git log` re-run.
    fn schedule_query_filter_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_state.debounce_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            this.update_in(cx, |this, window, cx| {
                this.search_state.debounce_task = None;
                this.update_query_filter(window, cx);
            })
            .ok();
        }));
    }

    /// Build a `QueryFilter` from the search bar editor text + toggle flags
    /// and commit it to `filters.query`. Empty text → `None` (filter
    /// cleared). Called after the 250ms text-change debounce and on every
    /// toggle click so the active query stays in sync with UI state.
    fn update_query_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.search_state.editor.read(cx).text(cx);
        let trimmed = text.trim();

        // A hex string (>= 7 chars, git's short-hash length) is a commit-hash
        // lookup, not a message grep: `git --grep` never matches a SHA, so we
        // instead jump to and highlight the matching commit. This keeps commits
        // findable by hash even though the hash column was removed from the
        // table. Message search still works for any non-hash text.
        if is_hash_like(trimmed) {
            // Don't also grep — that would empty the list.
            if self.filters.query.is_some() {
                self.set_query_filter(None, cx);
            }
            if let Some(oid) = self.find_loaded_commit_by_prefix(trimmed) {
                self.select_commit_by_sha(oid, CommitSelectionSource::UserGesture, window, cx);
            }
            return;
        }

        let query = if trimmed.is_empty() {
            None
        } else {
            Some(filters::QueryFilter {
                text: text.into(),
                regex: self.search_state.regex,
                case_sensitive: self.search_state.case_sensitive,
                search_in_diffs: self.search_state.search_in_diffs,
            })
        };
        self.set_query_filter(query, cx);
    }

    /// First loaded commit whose full SHA starts with `prefix` (case-insensitive
    /// hex). Used to resolve a hash typed into the search box to a concrete
    /// commit to select. Only loaded commits are matched — a hash for a commit
    /// below the currently-fetched window won't resolve here.
    fn find_loaded_commit_by_prefix(&self, prefix: &str) -> Option<Oid> {
        let needle = prefix.to_ascii_lowercase();
        self.graph_data.commits.iter().find_map(|commit| {
            let oid = commit.data.sha;
            oid.to_string().starts_with(&needle).then_some(oid)
        })
    }

    pub fn set_all_refs(&mut self, all_refs: bool, cx: &mut Context<Self>) {
        if self.filters.all_refs == all_refs {
            return;
        }
        self.filters.all_refs = all_refs;
        self.invalidate_state(cx);
        self.fetch_initial_graph_data(cx);
    }

    pub fn set_my_commits(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.highlights.my_commits == on {
            return;
        }
        self.highlights.my_commits = on;
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    pub fn set_new_since_refresh(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.highlights.new_since_refresh == on {
            return;
        }
        self.highlights.new_since_refresh = on;
        // Anchor the "new" boundary at the currently-visible HEAD the first
        // time the toggle flips on. Subsequent commits loaded above this
        // anchor get the decoration. Anchor is in-memory only — clearing
        // the toggle resets it so re-enabling re-anchors at HEAD.
        if on {
            self.highlights.last_seen_sha = self.graph_data.commits.first().map(|c| c.data.sha);
        } else {
            self.highlights.last_seen_sha = None;
        }
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    pub fn set_compact_refs(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.view_options.compact_refs == on {
            return;
        }
        self.view_options.compact_refs = on;
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    pub fn set_group_by_date(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.view_options.group_by_date == on {
            return;
        }
        self.view_options.group_by_date = on;
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    /// View-level mode derived from [`Self::log_source`]. See [`GraphMode`].
    pub fn mode(&self) -> GraphMode {
        match self.log_source {
            LogSource::Path(_) => GraphMode::FileHistory,
            _ => GraphMode::Full,
        }
    }

    pub fn file_history_options(&self) -> file_history::FileHistoryOptions {
        self.file_history_options
    }

    /// File-history preset constructor. Equivalent to
    /// [`Self::new`] with `LogSource::Path(repo_path)` plus the implicit
    /// file-history rendering preset (no graph column; per-file diff in the
    /// Commit tab). The caller resolves the `RepoPath` from a
    /// `ProjectPath` via `git_store.repository_and_path_for_project_path`.
    pub fn for_file_history(
        repo_id: RepositoryId,
        repo_path: git::repository::RepoPath,
        git_store: Entity<GitStore>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(
            repo_id,
            git_store,
            workspace,
            Some(LogSource::Path(repo_path)),
            window,
            cx,
        )
    }

    pub fn set_follow_renames(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.file_history_options.follow_renames == on {
            return;
        }
        self.file_history_options.follow_renames = on;
        self.invalidate_state(cx);
        self.fetch_initial_graph_data(cx);
    }

    pub fn set_with_local_changes(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.file_history_options.with_local_changes == on {
            return;
        }
        // Adding or removing the synthetic local-changes row shifts every
        // view index by one, so a selection held as an index would silently
        // slide onto the neighbouring commit while the Commit tab kept the
        // old one. Re-derive it from the sha the tab is actually describing.
        let selected_data_idx = self
            .selected_entry_idx
            .and_then(|view_idx| self.view_to_data_idx(view_idx));
        self.file_history_options.with_local_changes = on;
        if let Some(data_idx) = selected_data_idx {
            self.selected_entry_idx = Some(self.data_to_view_idx(data_idx));
        }
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    pub fn set_show_inline_diff(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.file_history_options.show_inline_diff == on {
            return;
        }
        self.file_history_options.show_inline_diff = on;
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    /// True when the file-history view should render a synthetic "local
    /// changes" row at index 0. Used by both the rendering path (to widen
    /// `commit_count`) and the row-render code (to short-circuit the
    /// commit-data fetch for the synthetic row).
    pub fn has_local_changes_row(&self) -> bool {
        matches!(self.mode(), GraphMode::FileHistory)
            && self.file_history_options.with_local_changes
    }

    fn render_log_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let search_input = self.render_search_input(cx).into_any_element();
        log_toolbar::LogToolbar::new(
            cx.weak_entity(),
            self.filters.date_range,
            self.filters.branches.clone(),
            self.filters.authors.clone(),
            self.filters.paths.clone(),
            self.get_repository(cx),
            self.filters.all_refs,
            self.highlights.my_commits,
            self.highlights.new_since_refresh,
            self.view_options.compact_refs,
            self.view_options.group_by_date,
            self.mode(),
            self.file_history_options,
        )
        .with_leading(search_input)
        .render(cx)
    }

    /// Computes the height of a single commit row in the git graph.
    ///
    /// The returned value is snapped to the nearest physical pixel. This is
    /// required so that the canvas's float math and the `uniform_list` layout
    /// (which snaps to device pixels) agree on row positions; otherwise rows
    /// drift apart as the user scrolls when `ui_font_size` is fractional.
    fn row_height(window: &Window, _cx: &App) -> Pixels {
        let rem_size = window.rem_size();
        let line_height = window.text_style().line_height_in_pixels(rem_size);
        let raw = line_height + ROW_VERTICAL_PADDING;
        let scale = window.scale_factor();

        (raw * scale).round() / scale
    }

    /// Width of `text` as the table would paint it in a cell, plus the cell's
    /// own padding.
    fn measured_column_width(text: &str, window: &Window, cx: &App) -> Pixels {
        let font_size = TextSize::default().rems(cx).to_pixels(window.rem_size());
        let run = TextRun {
            len: text.len(),
            font: ThemeSettings::get_global(cx).ui_font.clone(),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window
            .text_system()
            .layout_line(text, font_size, &[run], None)
            .width
            + COLUMN_CELL_PADDING.to_pixels(window.rem_size())
    }

    /// Record the log table's laid-out width and ask for one more frame when it
    /// has changed, so [`GitGraph::sync_default_column_widths`] can re-derive
    /// the columns against it.
    ///
    /// The width is only knowable once the table has been laid out, i.e. during
    /// the draw, and gpui discards invalidation raised inside a draw — so the
    /// redraw has to be deferred out of it. Reading the width instead off
    /// `RedistributableColumnsState::cached_container_width` (which the table
    /// fills in at the same point) is not enough on its own: nothing notifies
    /// when that value lands, so after a window or band resize the table kept
    /// the previous width's column shares until some unrelated event happened
    /// to redraw it.
    ///
    /// The equality guard is load-bearing. An unconditional deferred notify
    /// would re-arm itself on every frame and spin the view forever.
    fn observe_table_width(&mut self, bounds: &[Bounds<Pixels>], cx: &mut Context<Self>) {
        let Some(width) = bounds.first().map(|bounds| bounds.size.width) else {
            return;
        };
        if self.table_width == width {
            return;
        }
        self.table_width = width;

        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |_, cx| cx.notify()).ok();
        });
    }

    /// Re-derive the default column widths whenever anything they are derived
    /// from changes, so Date and Author stay sized to their content at both the
    /// Solution band's compact width and a full-window pane item, and across a
    /// change of UI font (see [`ColumnWidthInputs`]).
    ///
    /// This installs a *new* state entity rather than editing the existing one
    /// because `RedistributableColumnsState` exposes no width mutator. Going
    /// through the entity is not optional: the header, the rows, the resize
    /// handles' positions and the drag arithmetic all read their widths from
    /// it, so overriding only the rendered widths would paint the dividers
    /// where the drag math does not believe they are, and the first grab would
    /// jump the column sideways.
    ///
    /// A drag writes into the state, so "the state still holds exactly the
    /// widths we installed" is what separates an untouched table from a tuned
    /// one: while it differs, the user's widths stand and the derivation keeps
    /// out of the way. Because that is re-checked rather than latched,
    /// double-clicking a divider can also hand the table back -- but only when
    /// that restores `preview_widths` to `initial_widths` exactly, which
    /// `reset_column_to_initial_width` only does by redistributing the
    /// difference onto the reset column's *neighbours*. Once several dividers
    /// have been tuned, no sequence of double-clicks is guaranteed to get back.
    ///
    /// Swapping the entity is safe because nothing observes or subscribes to
    /// `RedistributableColumnsState`, with one narrow exception: between a
    /// divider's MouseDown and its first drag-move the preview still equals the
    /// initial widths, so a swap in that window is not gated out, and the
    /// in-flight `DraggedColumn`'s `state_id` then no longer matches the entity
    /// `bind_redistributable_columns` guards on -- killing that drag until the
    /// button is released and the divider re-grabbed.
    fn sync_default_column_widths(&mut self, window: &Window, cx: &mut Context<Self>) {
        let container = self.table_width;
        if container <= px(0.) {
            return;
        }
        let inputs = ColumnWidthInputs {
            container,
            date: Self::measured_column_width(DATE_COLUMN_SAMPLE, window, cx),
            author: Self::measured_column_width(AUTHOR_COLUMN_SAMPLE, window, cx),
        };
        if self.auto_column_widths_for == Some(inputs) {
            return;
        }

        let is_untouched = {
            let state = self.column_widths.read(cx);
            state.preview_widths().as_slice() == state.initial_widths().as_slice()
        };
        if !is_untouched {
            return;
        }

        let fractions = default_column_fractions(inputs.date, inputs.author, inputs.container);
        self.column_widths = cx.new(|_cx| {
            let mut state = new_column_widths_state(fractions);
            // Carried over so this frame's `graph_column_width` still sees a
            // measured Description column; the fresh entity would otherwise
            // report zero until its first prepaint and the graph would snap to
            // its uncapped natural width for one frame on every resize.
            state.set_cached_container_width(container);
            state
        });
        self.auto_column_widths_for = Some(inputs);
    }

    /// Width of the commit-graph column: enough for the widest loaded row, so
    /// the DAG is never clipped, bounded by the share of the Description column
    /// the graph may take (see `graph_column_width_for`).
    ///
    /// The Description column's width comes from the column state, which caches
    /// it during prepaint — so it lags one frame behind a resize. That is safe
    /// here: the value only feeds the graph's own width, which does not feed
    /// back into the container width, and any resize that changes it also
    /// triggers the redraw that picks the new value up. Notifying from the
    /// draw phase to force the issue would be dropped anyway.
    fn graph_column_width(&self, window: &Window, cx: &App) -> Pixels {
        let available = self
            .column_widths
            .read(cx)
            .preview_column_width(DESCRIPTION_COLUMN_IDX, window)
            .unwrap_or(px(0.));
        graph_column_width_for(self.graph_data.max_lanes, available)
    }

    fn table_column_width_config(&self, _window: &Window, cx: &App) -> ColumnWidthConfig {
        // The four text columns (Description / Date / Author / Commit) live in
        // `column_widths`; the graph column is rendered separately at a fixed
        // width to the left of the table, so it's no longer a table column.
        ColumnWidthConfig::explicit(
            self.column_widths
                .read(cx)
                .preview_widths()
                .as_slice()
                .to_vec(),
        )
    }

    pub fn new(
        repo_id: RepositoryId,
        git_store: Entity<GitStore>,
        workspace: WeakEntity<Workspace>,
        log_source: Option<LogSource>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        cx.on_focus(&focus_handle, window, |_, _, cx| cx.notify())
            .detach();

        let accent_colors = cx.theme().accents();
        let graph = GraphData::new(accent_colors_count(accent_colors));
        let log_source = log_source.unwrap_or_default();
        let log_order = LogOrder::default();

        cx.subscribe_in(
            &git_store,
            window,
            |this, _, event, window, cx| match event {
                GitStoreEvent::RepositoryUpdated(updated_repo_id, repo_event, _) => {
                    if this.repo_id == *updated_repo_id {
                        if let Some(repository) = this.get_repository(cx) {
                            this.on_repository_event(repository, repo_event, window, cx);
                        }
                    }
                }
                _ => {}
            },
        )
        .detach();

        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search commits…", window, cx);
            editor
        });

        let table_interaction_state = cx.new(|cx| TableInteractionState::new(cx));

        // The table holds only the three text columns (Description / Date /
        // Author); they're user-resizable. The commit hash is intentionally not
        // a column here (it's noise while scanning the graph — it lives in the
        // git panel's Commit tab on click, and search-by-hash is server-side). The
        // commit-graph column is *not* a table column — it's rendered separately
        // at a fixed width to the left of the table (IDEA-style), no resize handle.
        //
        // These fractions only survive the first frame: the table has not been
        // measured yet, and `sync_default_column_widths` re-derives them from
        // the columns' content as soon as it has a width to derive them for.
        let column_widths = cx.new(|_cx| new_column_widths_state(UNMEASURED_COLUMN_FRACTIONS));
        let mut row_height = Self::row_height(window, cx);

        cx.observe_global_in::<settings::SettingsStore>(window, move |this, window, cx| {
            let new_row_height = Self::row_height(window, cx);
            if new_row_height != row_height {
                // The `uniform_list` powering the table caches the item size
                // from its last layout; invalidate it so it re-measures with
                // the new row height on the next frame.
                this.table_interaction_state.update(cx, |state, _cx| {
                    state.scroll_handle.0.borrow_mut().last_item_size = None;
                });
                row_height = new_row_height;
                cx.notify();
            }
        })
        .detach();

        let editor_subscription = cx.subscribe_in(
            &search_editor,
            window,
            |this, _editor, event: &editor::EditorEvent, window, cx| {
                if let editor::EditorEvent::BufferEdited = event {
                    this.schedule_query_filter_update(window, cx);
                }
            },
        );

        let mut this = GitGraph {
            focus_handle,
            git_store,
            search_state: SearchState {
                case_sensitive: false,
                regex: false,
                search_in_diffs: false,
                editor: search_editor,
                debounce_task: None,
                _editor_subscription: editor_subscription,
            },
            workspace,
            graph_data: graph,
            context_menu: None,
            table_interaction_state,
            column_widths,
            table_width: px(0.),
            auto_column_widths_for: None,
            selected_entry_idx: None,
            selected_entry_idxs: HashSet::default(),
            selection_anchor_idx: None,
            hovered_entry_idx: None,
            log_source,
            log_order,
            filters: filters::LogFilters::default(),
            highlights: highlights::HighlightSet::default(),
            _git_panel_subscription: None,
            view_options: view_options::ViewOptions::default(),
            file_history_options: file_history::FileHistoryOptions::default(),
            local_user_email: None,
            remote_names: Vec::new(),
            repo_id,
            pending_select_sha: None,
        };

        this.fetch_initial_graph_data(cx);
        this.fetch_local_user_email(cx);
        this.fetch_remote_names(cx);
        this
    }

    fn fetch_remote_names(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let remotes = match repository
                .update(cx, |repo, _| repo.get_remotes(None, false))
                .await
            {
                Ok(Ok(remotes)) => remotes,
                Ok(Err(error)) => {
                    log::warn!("git graph: failed to list remotes: {error}");
                    return anyhow::Ok(());
                }
                Err(_) => return anyhow::Ok(()),
            };
            this.update(cx, |this, _cx| {
                this.remote_names = remotes.into_iter().map(|remote| remote.name).collect();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn fetch_local_user_email(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let committer = git::repository::get_git_committer(cx).await;
            this.update(cx, |this, cx| {
                if let Some(email) = committer.email {
                    let email = SharedString::from(email);
                    if this.local_user_email.as_ref() != Some(&email) {
                        this.local_user_email = Some(email);
                        if this.highlights.my_commits {
                            cx.notify();
                        }
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn on_repository_event(
        &mut self,
        repository: Entity<Repository>,
        event: &RepositoryEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            RepositoryEvent::GraphEvent((source, order, extra_args, extra_paths), event)
                if source == &self.log_source
                    && order == &self.log_order
                    && extra_args == &self.combined_extra_args()
                    && extra_paths == &self.filters.paths_args() =>
            {
                let extra_args = extra_args.clone();
                let extra_paths = extra_paths.clone();
                match event {
                    GitGraphEvent::FullyLoaded => {
                        // Pull any commits that finished loading but weren't
                        // delivered as a `CountUpdated` we observed (e.g. the
                        // fetch resolved in a window where our local count was
                        // still 0), then repaint. Without this the graph can
                        // stay stuck on "Loading" on a freshly-created solution:
                        // the render fast-path won't re-read the repository once
                        // `max_commit_count` is cached, and no other event would
                        // arrive to nudge it.
                        repository.update(cx, |repository, cx| {
                            let GraphDataResponse { commits, .. } = repository.graph_data(
                                source.clone(),
                                *order,
                                extra_args.clone(),
                                extra_paths.clone(),
                                self.graph_data.commits.len()..usize::MAX,
                                cx,
                            );
                            self.graph_data.add_commits(commits);
                        });

                        let pending_sha = self.pending_select_sha.take();
                        let pending_sha_data_index = pending_sha.and_then(|oid| {
                            repository
                                .read(cx)
                                .get_graph_data(source.clone(), *order, &extra_args, &extra_paths)
                                .and_then(|data| data.commit_oid_to_index.get(&oid).copied())
                        });
                        match (pending_sha, pending_sha_data_index) {
                            (_, Some(data_index)) => {
                                let view_index = self.data_to_view_idx(data_index);
                                self.select_entry(
                                    view_index,
                                    ScrollStrategy::Nearest,
                                    CommitSelectionSource::Background,
                                    window,
                                    cx,
                                );
                            }
                            // The log finished loading without the commit we
                            // were holding a place for: it is gone.
                            (Some(vanished), None) => {
                                self.close_vanished_commit_tab(vanished, window, cx);
                            }
                            (None, None) => {}
                        }
                        cx.notify();
                    }
                    GitGraphEvent::LoadingError => {
                        // todo(git_graph): Wire this up with the UI
                    }
                    GitGraphEvent::CountUpdated(commit_count) => {
                        let old_count = self.graph_data.commits.len();
                        // Kept to spot the give-up below: the closure clears
                        // `pending_select_sha` once the load has finished
                        // without producing the commit it was holding.
                        let pending_sha = self.pending_select_sha;

                        if let Some(pending_selection_index) =
                            repository.update(cx, |repository, cx| {
                                let GraphDataResponse {
                                    commits,
                                    is_loading,
                                    error: _,
                                } = repository.graph_data(
                                    source.clone(),
                                    *order,
                                    extra_args.clone(),
                                    extra_paths.clone(),
                                    old_count..*commit_count,
                                    cx,
                                );
                                self.graph_data.add_commits(commits);

                                let pending_sha_index = self.pending_select_sha.and_then(|oid| {
                                    repository
                                        .get_graph_data(
                                            source.clone(),
                                            *order,
                                            &extra_args,
                                            &extra_paths,
                                        )
                                        .and_then(|data| {
                                            data.commit_oid_to_index.get(&oid).copied()
                                        })
                                });

                                if !is_loading && pending_sha_index.is_none() {
                                    self.pending_select_sha.take();
                                }

                                pending_sha_index
                            })
                        {
                            let view_index = self.data_to_view_idx(pending_selection_index);
                            self.select_entry(
                                view_index,
                                ScrollStrategy::Nearest,
                                CommitSelectionSource::Background,
                                window,
                                cx,
                            );
                            self.pending_select_sha.take();
                        } else if let Some(vanished) =
                            pending_sha.filter(|_| self.pending_select_sha.is_none())
                        {
                            self.close_vanished_commit_tab(vanished, window, cx);
                        }

                        cx.notify();
                    }
                }
            }
            RepositoryEvent::HeadChanged
            | RepositoryEvent::BranchListChanged
            | RepositoryEvent::TagListChanged => {
                // Only invalidate if we scanned atleast once,
                // meaning we are not inside the initial repo loading state
                // NOTE: this fixes an loading performance regression
                if repository.read(cx).scan_id > 1 {
                    self.invalidate_state(cx);
                }
            }
            RepositoryEvent::StashEntriesChanged if self.log_source == LogSource::All => {
                // Stash entries initial's scan id is 2, so we don't want to invalidate the graph before that
                if repository.read(cx).scan_id > 2 {
                    self.invalidate_state(cx);
                }
            }
            RepositoryEvent::GraphEvent(_, _) => {}
            _ => {}
        }
    }

    fn fetch_initial_graph_data(&mut self, cx: &mut App) {
        if let Some(repository) = self.get_repository(cx) {
            let extra_args = self.combined_extra_args();
            let extra_paths = self.filters.paths_args();
            repository.update(cx, |repository, cx| {
                let commits = repository
                    .graph_data(
                        self.log_source.clone(),
                        self.log_order,
                        extra_args,
                        extra_paths,
                        0..usize::MAX,
                        cx,
                    )
                    .commits;
                self.graph_data.add_commits(commits);
            });
        }
    }

    /// `git log` extra-args produced by the chip filters plus the
    /// file-history preset's toggles. Kept in one place so the cache key
    /// the repository uses (`extra_args`) stays consistent across all call
    /// sites — `fetch_initial_graph_data`, the `RepositoryEvent::GraphEvent`
    /// match, and any other code that has to thread args back through.
    fn combined_extra_args(&self) -> Vec<String> {
        let mut args = self.filters.to_git_args();
        if matches!(self.log_source, LogSource::Path(_)) {
            args.extend(self.file_history_options.extra_git_args());
        }
        args
    }

    fn get_repository(&self, cx: &App) -> Option<Entity<Repository>> {
        let git_store = self.git_store.read(cx);
        git_store.repositories().get(&self.repo_id).cloned()
    }

    /// Checks whether a ref name from git's `%D` decoration
    ///  format refers to the currently checked-out branch.
    fn is_head_ref(ref_name: &str, head_branch_name: &Option<SharedString>) -> bool {
        head_branch_name.as_ref().is_some_and(|head| {
            ref_name == head.as_ref() || ref_name.strip_prefix("HEAD -> ") == Some(head.as_ref())
        })
    }

    /// Resolve the active repository's working-directory path. Reads
    /// once per render pass — the result is fed into [`render_chip`]
    /// for the protected-branch indicator.
    fn current_work_dir(&self, cx: &App) -> Option<std::path::PathBuf> {
        self.get_repository(cx)
            .map(|repo| repo.read(cx).work_directory_abs_path.to_path_buf())
    }

    /// `truncate` belongs to the caller, not the chip. Today the graph's
    /// Description column is the only caller and it always passes `true`,
    /// because a long ref there must shrink so the commit subject stays
    /// visible. The flag survives because `Chip::truncate` sets `min_w_0`,
    /// which is wrong in any row that wraps: every chip would collapse to a
    /// bare ellipsis rather than wrap to the next line. The `false` caller was
    /// the commit-detail sidebar's wrapping ref-chip row, deleted when the
    /// Commit tab replaced it.
    fn render_chip(
        &self,
        name: &SharedString,
        accent_color: gpui::Hsla,
        is_head: bool,
        work_dir: Option<&std::path::Path>,
        truncate: bool,
    ) -> impl IntoElement {
        // S-SOL-PRT — render protected refs with a lock glyph in
        // place of the standard chip icon. We strip the ref-namespace
        // prefix git emits in `%D` decorations (`refs/heads/`,
        // `HEAD -> `, `refs/remotes/origin/`, etc.) before consulting
        // the policy so the glob patterns match the bare branch name.
        let bare = strip_ref_namespace(name.as_ref());
        let is_protected = work_dir
            .map(|wd| {
                matches!(
                    solutions::branch_protection::check(wd, bare, "delete_branch"),
                    solutions::branch_protection::Decision::Forbidden { .. }
                )
            })
            .unwrap_or(false);
        Chip::new(name.clone())
            .label_size(LabelSize::Small)
            .map(|chip| if truncate { chip.truncate() } else { chip })
            .map(|chip| {
                if is_head {
                    chip.icon(IconName::Check)
                        .bg_color(accent_color.opacity(0.25))
                        .border_color(accent_color.opacity(0.5))
                } else if is_protected {
                    chip.icon(IconName::LockOutlined)
                        .bg_color(accent_color.opacity(0.12))
                        .border_color(accent_color.opacity(0.5))
                } else {
                    chip.bg_color(accent_color.opacity(0.08))
                        .border_color(accent_color.opacity(0.25))
                }
            })
    }

    fn render_table_rows(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Vec<AnyElement>> {
        let repository = self.get_repository(cx);

        let head_branch_name: Option<SharedString> = repository.as_ref().and_then(|repo| {
            repo.read(cx)
                .snapshot()
                .branch
                .as_ref()
                .map(|branch| SharedString::from(branch.name().to_string()))
        });

        let work_dir = self.current_work_dir(cx);

        let row_height = Self::row_height(window, cx);
        // The graph is painted over the left edge of the Description column, so
        // every subject has to be indented past the lanes on its own row.
        let shows_graph = !matches!(self.log_source, LogSource::Path(_));
        let graph_width = self.graph_column_width(window, cx);
        // The synthetic "local changes" row, when active, occupies row 0 in
        // the view but has no backing commit. Real commit indices shift by
        // 1 — `data_idx = view_idx.checked_sub(1)`.
        let has_local_row = self.has_local_changes_row();

        // We fetch data outside the visible viewport to avoid loading entries when
        // users scroll through the git graph
        if let Some(repository) = repository.as_ref() {
            const FETCH_RANGE: usize = 100;
            repository.update(cx, |repository, cx| {
                self.graph_data.commits[range.start.saturating_sub(FETCH_RANGE)
                    ..(range.end + FETCH_RANGE)
                        .min(self.graph_data.commits.len().saturating_sub(1))]
                    .iter()
                    .for_each(|commit| {
                        repository.fetch_commit_data(commit.data.sha, false, cx);
                    });
            });
        }

        // Index of the "new since refresh" anchor (last seen sha at the
        // moment the toggle was first enabled). Commits at indices strictly
        // less than this — i.e. above the anchor in the log — are "new".
        let new_anchor_idx: Option<usize> = if self.highlights.new_since_refresh {
            self.highlights.last_seen_sha.and_then(|anchor| {
                self.graph_data
                    .commits
                    .iter()
                    .position(|c| c.data.sha == anchor)
            })
        } else {
            None
        };
        let local_user_email = self.local_user_email.clone();
        let my_commits_active = self.highlights.my_commits;
        let compact_refs = self.view_options.compact_refs;
        let compact_threshold = view_options::compact_refs_threshold(cx);
        let group_by_date = self.view_options.group_by_date;
        let highlight_color = cx.theme().colors().text_accent;

        range
            .map(|idx| {
                if has_local_row && idx == 0 {
                    return vec![
                        div()
                            .h(row_height)
                            .id(("local-changes-row", 0_u32))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Icon::new(IconName::Pencil).size(IconSize::Small))
                                    .child(Label::new("Local Changes").color(Color::Accent)),
                            )
                            .into_any_element(),
                        div().h(row_height).into_any_element(),
                        div().h(row_height).into_any_element(),
                        div().h(row_height).into_any_element(),
                    ];
                }
                // `view_idx` is the row index in the view (used by selection
                // and hover state); `data_idx` is the index into
                // `graph_data.commits` (shifted by 1 when the synthetic
                // local-changes row is at view 0).
                let view_idx = idx;
                let data_idx = if has_local_row {
                    idx.saturating_sub(1)
                } else {
                    idx
                };
                let Some((commit, repository)) = self
                    .graph_data
                    .commits
                    .get(data_idx)
                    .zip(repository.as_ref())
                else {
                    return vec![
                        div().h(row_height).into_any_element(),
                        div().h(row_height).into_any_element(),
                        div().h(row_height).into_any_element(),
                        div().h(row_height).into_any_element(),
                    ];
                };
                // The remaining code originally indexed by `idx` against
                // `graph_data` (group-by-date prev lookup). Shadow `idx`
                // with `data_idx` so those lookups stay correct, and use
                // `view_idx` explicitly for selection comparisons.
                let idx = data_idx;

                let data = repository.update(cx, |repository, cx| {
                    repository
                        .fetch_commit_data(commit.data.sha, false, cx)
                        .clone()
                });

                let mut formatted_time = String::new();
                let subject: SharedString;
                let author_name: SharedString;
                let mut author_email: SharedString = SharedString::default();
                let mut commit_timestamp: i64 = 0;

                if let CommitDataState::Loaded(data) = data {
                    subject = data.subject.clone();
                    author_name = data.author_name.clone();
                    author_email = data.author_email.clone();
                    commit_timestamp = data.commit_timestamp;
                    formatted_time = format_timestamp(commit_timestamp);
                } else {
                    subject = "Loading…".into();
                    author_name = "".into();
                }

                let is_my_commit = my_commits_active
                    && local_user_email
                        .as_ref()
                        .is_some_and(|me| !me.is_empty() && me.as_ref() == author_email.as_ref());
                let is_new_commit = new_anchor_idx.is_some_and(|anchor| idx < anchor);
                let date_header_label: Option<SharedString> = if group_by_date {
                    let current_day = local_day_label(commit_timestamp);
                    let prev_day: Option<String> = idx.checked_sub(1).and_then(|prev_idx| {
                        let prev_commit = self.graph_data.commits.get(prev_idx)?;
                        let prev_state = repository.update(cx, |repository, cx| {
                            repository
                                .fetch_commit_data(prev_commit.data.sha, false, cx)
                                .clone()
                        });
                        match prev_state {
                            CommitDataState::Loaded(prev) => local_day_label(prev.commit_timestamp),
                            _ => None,
                        }
                    });
                    match (current_day, prev_day) {
                        (Some(today), Some(prev)) if today == prev => None,
                        (Some(today), _) => Some(SharedString::from(today)),
                        _ => None,
                    }
                } else {
                    None
                };

                let accent_colors = cx.theme().accents();
                let accent_color = accent_colors
                    .0
                    .get(commit.color_idx)
                    .copied()
                    .unwrap_or_else(|| accent_colors.0.first().copied().unwrap_or_default());

                let is_selected = self.selected_entry_idx == Some(view_idx);
                let column_label = |label: SharedString| {
                    Label::new(label)
                        .when(!is_selected, |c| c.color(Color::Muted))
                        .truncate()
                        .into_any_element()
                };

                let subject_label = column_label(subject.clone());

                let ref_chips_element = (!commit.data.ref_names.is_empty()).then(|| {
                    let total = commit.data.ref_names.len();
                    let visible = if compact_refs && total > compact_threshold {
                        compact_threshold
                    } else {
                        total
                    };
                    let hidden = total.saturating_sub(visible);
                    let mut row = h_flex().gap_1();
                    for name in commit.data.ref_names.iter().take(visible) {
                        let is_head = Self::is_head_ref(name.as_ref(), &head_branch_name);
                        row = row.child(self.render_chip(
                            name,
                            accent_color,
                            is_head,
                            work_dir.as_deref(),
                            true,
                        ));
                    }
                    if hidden > 0 {
                        let hidden_names = commit
                            .data
                            .ref_names
                            .iter()
                            .skip(visible)
                            .map(|n| n.to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        row = row.child(
                            Chip::new(SharedString::from(format!("+{hidden}")))
                                .label_size(LabelSize::Small)
                                .bg_color(accent_color.opacity(0.08))
                                .border_color(accent_color.opacity(0.25))
                                .tooltip(Tooltip::text(SharedString::from(hidden_names))),
                        );
                    }
                    row
                });

                let highlight_marker = if is_my_commit || is_new_commit {
                    Some(
                        div()
                            .w(px(2.0))
                            .h_full()
                            .bg(highlight_color)
                            .into_any_element(),
                    )
                } else {
                    None
                };

                // The table is a fixed-row-height `uniform_list`, so the
                // group-by-date marker can't be a stacked second line (it'd
                // overflow the row and clip / overlap the next one). Render it
                // inline as a leading pill instead.
                let date_pill = date_header_label.map(|label| {
                    div()
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .bg(cx.theme().colors().element_background)
                        .child(
                            Label::new(label)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                });

                let description_cell = h_flex()
                    .gap_2()
                    .overflow_hidden()
                    .children(date_pill)
                    .children(highlight_marker)
                    .children(ref_chips_element)
                    .child(subject_label)
                    .into_any_element();

                // IDEA-style per-row indent: the subject starts right after
                // *this* row's own lanes, so a narrow stretch of history pulls
                // the text back left instead of every row being indented by the
                // widest row in the log.
                let subject_indent = if shows_graph {
                    graph_row_extent(self.graph_data.columns_at_row(idx)).min(graph_width)
                } else {
                    px(0.)
                };

                vec![
                    h_flex()
                        .id(ElementId::NamedInteger("commit-subject".into(), idx as u64))
                        .overflow_hidden()
                        .tooltip(Tooltip::text(subject))
                        .child(div().flex_none().w(subject_indent))
                        .child(description_cell)
                        .into_any_element(),
                    column_label(formatted_time.into()),
                    column_label(author_name),
                ]
            })
            .collect()
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_selection();
        self.push_selection_to_git_panel(CommitSelectionSource::UserGesture, window, cx);
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    /// Drop the row selection. Deliberately silent towards the git panel: the
    /// panel's own `CommitTabClosed` is one of the callers, and pushing from
    /// here would bounce a redundant close straight back at it. Deselection
    /// gestures that originate in the graph pair this with
    /// [`Self::push_selection_to_git_panel`].
    fn clear_selection(&mut self) {
        self.selected_entry_idx = None;
        self.selected_entry_idxs.clear();
        self.selection_anchor_idx = None;
    }

    fn select_first(&mut self, _: &SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        self.select_entry(
            0,
            ScrollStrategy::Nearest,
            CommitSelectionSource::UserGesture,
            window,
            cx,
        );
    }

    fn select_prev(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected_entry_idx) = &self.selected_entry_idx {
            self.select_entry(
                selected_entry_idx.saturating_sub(1),
                ScrollStrategy::Nearest,
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
        } else {
            self.select_first(&SelectFirst, window, cx);
        }
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected_entry_idx) = &self.selected_entry_idx {
            self.select_entry(
                selected_entry_idx
                    .saturating_add(1)
                    .min(self.view_row_count().saturating_sub(1)),
                ScrollStrategy::Nearest,
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
        } else {
            self.select_prev(&SelectPrevious, window, cx);
        }
    }

    fn select_last(&mut self, _: &SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        self.select_entry(
            self.view_row_count().saturating_sub(1),
            ScrollStrategy::Nearest,
            CommitSelectionSource::UserGesture,
            window,
            cx,
        );
    }

    /// Total number of rows visible in the table — the data commits plus
    /// the synthetic "local changes" row if active.
    fn view_row_count(&self) -> usize {
        self.graph_data.commits.len() + if self.has_local_changes_row() { 1 } else { 0 }
    }

    /// Translate a view-space row index into a data-space index. Returns
    /// `None` for the synthetic local-changes row (it has no commit data).
    fn view_to_data_idx(&self, view_idx: usize) -> Option<usize> {
        if self.has_local_changes_row() {
            view_idx.checked_sub(1)
        } else {
            Some(view_idx)
        }
    }

    fn data_to_view_idx(&self, data_idx: usize) -> usize {
        if self.has_local_changes_row() {
            data_idx.saturating_add(1)
        } else {
            data_idx
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.open_selected_commit_view(window, cx);
    }

    fn select_entry(
        &mut self,
        idx: usize,
        scroll_strategy: ScrollStrategy,
        source: CommitSelectionSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target_sha = self
            .view_to_data_idx(idx)
            .and_then(|data_idx| self.graph_data.commits.get(data_idx))
            .map(|commit| commit.data.sha);

        // Every route into `select_entry` — keyboard navigation, an
        // `OpenCommitView` jump, the restore-by-sha after a refetch — is a
        // single-row selection, so it collapses whatever multi-row selection a
        // Ctrl/Shift click had built. Done before the no-op early return
        // below: re-selecting the already-active row still has to drop the
        // other rows' highlight.
        let collapsed_multi_selection =
            self.selected_entry_idxs.len() > 1 || !self.selected_entry_idxs.contains(&idx);
        self.selected_entry_idxs.clear();
        self.selected_entry_idxs.insert(idx);
        self.selection_anchor_idx = None;

        // Scheduled here, above every early return below, because the push is
        // deferred and re-reads the selection once it has settled: a re-click
        // on the already-active row still has to re-activate the panel's
        // Commit tab, and `apply_row_click_selection` writes the multi-row set
        // back on top of this call after it returns.
        self.push_selection_to_git_panel(source, window, cx);

        // Re-selecting the same row only has to repaint the highlight — the
        // commit's details live in the git panel's Commit tab, and the push
        // above has already refreshed them. Nothing below this guard runs on a
        // re-click.
        if self.selected_entry_idx == Some(idx) {
            if collapsed_multi_selection {
                cx.notify();
            }
            return;
        }

        self.selected_entry_idx = Some(idx);
        self.table_interaction_state.update(cx, |state, cx| {
            state.scroll_handle.scroll_to_item(idx, scroll_strategy);
            cx.notify();
        });

        // The synthetic "local changes" row at view-index 0 has no commit
        // data — selecting it leaves the Commit tab empty (this is by
        // design; v1 doesn't render a working-tree-vs-HEAD diff yet).
        if self.has_local_changes_row() && idx == 0 {
            cx.emit(ItemEvent::Edit);
            cx.notify();
            return;
        }
        // `ItemEvent::Edit` is the "re-serialize me" signal, and what gets
        // serialized is the selected commit's sha — a row with no commit
        // behind it has nothing to write.
        if target_sha.is_none() {
            return;
        }

        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn log_source_for_test(&self) -> &LogSource {
        &self.log_source
    }

    /// Snapshot of the currently-loaded commits as their source
    /// [`InitialGraphCommitData`] (sha / parents / ref names), in graph
    /// (newest-first) order. Used by integration tests to assert that the
    /// fetched graph matches the commits seeded into the repository.
    #[cfg(any(test, feature = "test-support"))]
    pub fn initial_commit_data_for_test(&self) -> Vec<std::sync::Arc<InitialGraphCommitData>> {
        self.graph_data
            .commits
            .iter()
            .map(|entry| entry.data.clone())
            .collect()
    }

    /// Drives a search the same way typing into the search bar would, but
    /// without the 250ms input debounce: apply the query filter and
    /// immediately re-run the filtered `git log`. Pair with
    /// `run_until_parked` + [`Self::search_matches_for_test`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn search_for_test(&mut self, query: SharedString, cx: &mut Context<Self>) {
        // Keep the search-bar editor in sync without needing a `Window`: the
        // singleton buffer text drives the visible input, and the filter
        // below is what actually re-runs `git log`.
        if let Some(buffer) = self
            .search_state
            .editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
        {
            buffer.update(cx, |buffer, cx| buffer.set_text(query.clone(), cx));
        }
        let query = if query.is_empty() {
            None
        } else {
            Some(filters::QueryFilter {
                text: query,
                regex: self.search_state.regex,
                case_sensitive: self.search_state.case_sensitive,
                search_in_diffs: self.search_state.search_in_diffs,
            })
        };
        self.set_query_filter(query, cx);
    }

    /// SHAs of the commits remaining after the active search/query filter, in
    /// graph order. Returned as bare [`Oid`]s (rather than
    /// `InitialGraphCommitData`, which is not `PartialEq`) so tests can compare
    /// the local and remote match sets directly with `assert_eq!`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn search_matches_for_test(&self) -> Vec<Oid> {
        self.graph_data
            .commits
            .iter()
            .map(|entry| entry.data.sha)
            .collect()
    }

    pub fn set_repo_id(
        &mut self,
        repo_id: RepositoryId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if repo_id != self.repo_id
            && self
                .git_store
                .read(cx)
                .repositories()
                .contains_key(&repo_id)
        {
            self.repo_id = repo_id;
            // A selection belongs to the repository it was made in — don't let
            // `invalidate_state` re-anchor it onto the incoming one.
            self.clear_selection();
            self.push_selection_to_git_panel(CommitSelectionSource::UserGesture, window, cx);
            self.pending_select_sha = None;
            self.invalidate_state(cx);
        }
    }

    pub fn select_commit_by_sha(
        &mut self,
        sha: impl TryInto<Oid>,
        source: CommitSelectionSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        fn inner(
            this: &mut GitGraph,
            oid: Oid,
            source: CommitSelectionSource,
            window: &mut Window,
            cx: &mut Context<GitGraph>,
        ) {
            let Some(selected_repository) = this.get_repository(cx) else {
                return;
            };

            let extra_args = this.combined_extra_args();
            let extra_paths = this.filters.paths_args();
            let Some(data_index) = selected_repository
                .read(cx)
                .get_graph_data(
                    this.log_source.clone(),
                    this.log_order,
                    &extra_args,
                    &extra_paths,
                )
                .and_then(|data| data.commit_oid_to_index.get(&oid))
                .copied()
            else {
                this.pending_select_sha = Some(oid);
                return;
            };

            this.pending_select_sha = None;
            // Convert the data-space index back to view-space (the synthetic
            // local-changes row, when active, occupies view-index 0).
            let view_index = if this.has_local_changes_row() {
                data_index.saturating_add(1)
            } else {
                data_index
            };
            this.select_entry(view_index, ScrollStrategy::Center, source, window, cx);
        }

        if let Ok(oid) = sha.try_into() {
            inner(self, oid, source, window, cx);
        }
    }

    /// A click on a commit row. Double-click deliberately does *nothing beyond
    /// selecting*: it used to open the synthetic `CommitView` tab, which is a
    /// pseudo-file holding the commit description — redundant now that the git
    /// panel's Commit tab carries the full message, and far too easy to trigger
    /// by accident while walking the log. The commit view is still reachable
    /// explicitly (`menu::Confirm` and the `git_graph::OpenCommitView` action).
    fn on_row_click(
        &mut self,
        index: usize,
        _click_count: usize,
        modifiers: Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Shift wins over the secondary modifier when both are held, matching
        // every other multi-select list: Ctrl+Shift+click extends the range.
        let gesture = if modifiers.shift {
            RowSelectionGesture::Range
        } else if modifiers.secondary() {
            RowSelectionGesture::Toggle
        } else {
            RowSelectionGesture::Replace
        };
        self.apply_row_click_selection(index, gesture, window, cx);
    }

    /// Fold a row click into the selection and re-point the git panel's Commit
    /// tab at the resulting active row.
    ///
    /// The tab is re-pointed by going through [`Self::select_entry`], which
    /// also collapses the selection; the multi-row set is written back on top
    /// of it afterwards.
    fn apply_row_click_selection(
        &mut self,
        index: usize,
        gesture: RowSelectionGesture,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A stale anchor (the log shrank under us) would range over rows that
        // no longer exist, so it degrades to "no anchor" instead.
        let anchor = self
            .selection_anchor_idx
            .filter(|anchor| *anchor < self.view_row_count());
        let selection = fold_row_click(
            index,
            gesture,
            &self.selected_entry_idxs,
            anchor,
            self.has_local_changes_row(),
        );
        self.select_entry(
            selection.active,
            ScrollStrategy::Center,
            CommitSelectionSource::UserGesture,
            window,
            cx,
        );
        self.selected_entry_idxs = selection.selected;
        self.selection_anchor_idx = selection.anchor;
        cx.notify();
    }

    fn open_selected_commit_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected_entry_index) = self.selected_entry_idx else {
            return;
        };

        self.open_commit_view(selected_entry_index, window, cx);
    }

    fn open_commit_view(
        &mut self,
        entry_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(data_index) = self.view_to_data_idx(entry_index) else {
            return;
        };
        let Some(commit_entry) = self.graph_data.commits.get(data_index) else {
            return;
        };

        let Some(repository) = self.get_repository(cx) else {
            return;
        };

        CommitView::open(
            commit_entry.data.sha.to_string(),
            repository.downgrade(),
            self.workspace.clone(),
            None,
            None,
            window,
            cx,
        );
    }

    /// S-CTM right-click handler — assemble [`context_menu::CommitContext`]
    /// from the row at `index` and deploy a [`ContextMenu`] anchored at
    /// `position`. Subscribes to the menu's `DismissEvent` to drop the
    /// menu state when it closes.
    fn deploy_commit_context_menu(
        &mut self,
        index: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(data_index) = self.view_to_data_idx(index) else {
            return;
        };
        let Some(commit_entry) = self.graph_data.commits.get(data_index) else {
            return;
        };
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        let sha: SharedString = commit_entry.data.sha.to_string().into();
        let refs: Vec<SharedString> = commit_entry.data.ref_names.clone();
        let subject: SharedString = {
            let data = repository.update(cx, |repo, cx| {
                repo.fetch_commit_data(commit_entry.data.sha, false, cx)
                    .clone()
            });
            match data {
                CommitDataState::Loaded(data) => data.subject.clone(),
                _ => SharedString::default(),
            }
        };
        let provider = repository.read(cx).default_remote_url().and_then(|url| {
            let registry = GitHostingProviderRegistry::default_global(cx);
            parse_git_remote_url(registry, &url)
                .map(|(provider, _)| (provider.name(), provider.base_url().to_string()))
        });
        let work_dir = Some(
            repository
                .read(cx)
                .work_directory_abs_path
                .as_ref()
                .to_path_buf(),
        );
        let (head_branch, local_branches, remote_branches) = {
            let repo = repository.read(cx);
            let head_branch = repo
                .branch
                .as_ref()
                .map(|b| SharedString::from(b.name().to_string()));
            let mut local_branches = Vec::new();
            let mut remote_branches = Vec::new();
            for branch in repo.branch_list.iter() {
                let name = SharedString::from(branch.name().to_string());
                if branch.is_remote() {
                    remote_branches.push(name);
                    continue;
                }
                // `ahead` is what tells the commit menu whether checking
                // out the upstream would strand local commits.
                let upstream = branch.upstream.as_ref();
                local_branches.push(context_menu::LocalBranchInfo {
                    name,
                    upstream: upstream
                        .and_then(|upstream| upstream.stripped_ref_name())
                        .map(|ref_name| SharedString::from(ref_name.to_string())),
                    upstream_gone: upstream
                        .is_some_and(|upstream| upstream.tracking.is_gone()),
                    ahead: upstream
                        .and_then(|upstream| upstream.tracking.status())
                        .map_or(0, |status| status.ahead),
                });
            }
            (head_branch, local_branches, remote_branches)
        };

        let ctx = context_menu::CommitContext {
            workspace: self.workspace.clone(),
            repository,
            sha,
            subject,
            provider,
            work_dir,
            // S-SOL-CHP: the GraphView consumes per-repo data, not the
            // Solution-wide aggregated log, so member_id is always None
            // here. The Solution-aggregated log view (S-SOL-LOG) sets
            // this when constructing its own context.
            member_id: None,
            refs,
            head_branch,
            local_branches,
            remote_branches,
            remotes: self.remote_names.clone(),
        };
        let menu = context_menu::build_commit_context_menu(ctx, window, cx);
        self.show_context_menu(menu, position, window, cx);
    }

    /// Right-click inside a multi-row selection: build the menu for the whole
    /// selection rather than for the single row under the cursor.
    fn deploy_multi_commit_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.get_repository(cx) else {
            return;
        };

        // Rows run newest-first, so oldest-first — the order the multi-commit
        // menu (and every git range it builds) wants — is descending data
        // index. The synthetic local-changes row maps to no data index and
        // drops out here.
        let mut data_indices: Vec<usize> = self
            .selected_entry_idxs
            .iter()
            .filter_map(|view_idx| self.view_to_data_idx(*view_idx))
            .collect();
        data_indices.sort_unstable();
        data_indices.reverse();

        let mut shas = Vec::with_capacity(data_indices.len());
        let mut subjects = Vec::with_capacity(data_indices.len());
        let mut first_parents = Vec::with_capacity(data_indices.len());
        for data_index in data_indices {
            let Some(commit_entry) = self.graph_data.commits.get(data_index).cloned() else {
                continue;
            };
            shas.push(SharedString::from(commit_entry.data.sha.to_string()));
            let data = repository.update(cx, |repo, cx| {
                repo.fetch_commit_data(commit_entry.data.sha, false, cx)
                    .clone()
            });
            subjects.push(match data {
                CommitDataState::Loaded(data) => data.subject.clone(),
                _ => SharedString::default(),
            });
            first_parents.push((
                commit_entry.data.sha,
                commit_entry.data.parents.first().copied(),
            ));
        }
        if shas.is_empty() {
            return;
        }

        let work_dir = Some(
            repository
                .read(cx)
                .work_directory_abs_path
                .as_ref()
                .to_path_buf(),
        );
        let ctx = MultiCommitContext {
            workspace: self.workspace.clone(),
            repository,
            shas,
            subjects,
            work_dir,
            contiguous: is_first_parent_chain(&first_parents),
        };
        let menu = build_multi_commit_context_menu(ctx, window, cx);
        self.show_context_menu(menu, position, window, cx);
    }

    /// Anchor `menu` at `position` and keep it alive until it dismisses,
    /// returning focus to the graph when the menu had it.
    fn show_context_menu(
        &mut self,
        menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subscription = cx.subscribe_in(
            &menu,
            window,
            |this, _menu, _: &DismissEvent, window, cx| {
                if this
                    .context_menu
                    .as_ref()
                    .is_some_and(|cm| cm.0.focus_handle(cx).contains_focused(window, cx))
                {
                    this.focus_handle.focus(window, cx);
                }
                this.context_menu.take();
                cx.notify();
            },
        );
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    /// The "Search commits…" input box (text editor + case-sensitive / regex /
    /// search-in-diffs toggles), styled as a rounded bordered field. Rendered
    /// inline at the start of the log toolbar row (see `render_log_toolbar`).
    fn render_search_input(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let color = cx.theme().colors();
        let query_focus_handle = self.search_state.editor.focus_handle(cx);
        let search_options = {
            let mut options = SearchOptions::NONE;
            options.set(
                SearchOptions::CASE_SENSITIVE,
                self.search_state.case_sensitive,
            );
            options.set(SearchOptions::REGEX, self.search_state.regex);
            options
        };
        let search_in_diffs = self.search_state.search_in_diffs;
        let in_diffs_focus_handle = query_focus_handle.clone();

        h_flex()
            // The whole search field sits inside the `GitGraph` key context,
            // so vim's commit-list bindings (`j`/`k`/`shift-g`/`g g`, keyed on
            // `GitGraph && !GitGraphSearchBar`) would otherwise swallow those
            // characters instead of letting them reach the query editor. The
            // negated clause needs this identifier to actually be emitted.
            .key_context("GitGraphSearchBar")
            .h_7()
            .w_full()
            .min_w_0()
            .px_1p5()
            .gap_1()
            .border_1()
            .border_color(color.border_variant)
            .rounded_md()
            .bg(color.editor_background)
            .child(self.search_state.editor.clone())
            .child(SearchOption::CaseSensitive.as_button(
                search_options,
                SearchSource::Buffer,
                query_focus_handle.clone(),
            ))
            .child(SearchOption::Regex.as_button(
                search_options,
                SearchSource::Buffer,
                query_focus_handle,
            ))
            .child(
                IconButton::new("git-graph-search-in-diffs", IconName::FileDiff)
                    .shape(ui::IconButtonShape::Square)
                    .style(ButtonStyle::Subtle)
                    .toggle_state(search_in_diffs)
                    .tooltip(move |_, cx| {
                        Tooltip::for_action_in(
                            "Search in commit content (slower)",
                            &ToggleSearchInDiffs,
                            &in_diffs_focus_handle,
                            cx,
                        )
                    })
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(Box::new(ToggleSearchInDiffs), cx);
                    })),
            )
    }

    fn render_loading_spinner(&self, cx: &App) -> AnyElement {
        let rems = TextSize::Large.rems(cx);
        Icon::new(IconName::LoadCircle)
            .size(IconSize::Custom(rems))
            .color(Color::Accent)
            .with_rotate_animation(3)
            .into_any_element()
    }

    fn render_graph_canvas(&self, window: &Window, cx: &mut Context<GitGraph>) -> impl IntoElement {
        let row_height = Self::row_height(window, cx);
        let table_state = self.table_interaction_state.read(cx);
        let viewport_height = table_state
            .scroll_handle
            .0
            .borrow()
            .last_item_size
            .map(|size| size.item.height)
            .unwrap_or(window.viewport_size().height);
        let loaded_commit_count = self.graph_data.commits.len();

        let content_height = row_height * loaded_commit_count;
        let max_scroll = (content_height - viewport_height).max(px(0.));
        let scroll_offset_y = (-table_state.scroll_offset().y).clamp(px(0.), max_scroll);

        let first_visible_row = (scroll_offset_y / row_height).floor() as usize;
        let vertical_scroll_offset = scroll_offset_y - (first_visible_row as f32 * row_height);

        let graph_width = self.graph_column_width(window, cx);
        let last_visible_row =
            first_visible_row + (viewport_height / row_height).ceil() as usize + 1;

        let viewport_range = first_visible_row.min(loaded_commit_count.saturating_sub(1))
            ..(last_visible_row).min(loaded_commit_count);
        let rows = self.graph_data.commits[viewport_range.clone()].to_vec();
        let commit_lines: Vec<_> = self
            .graph_data
            .lines
            .iter()
            .filter(|line| {
                line.full_interval.start <= viewport_range.end
                    && line.full_interval.end >= viewport_range.start
            })
            .cloned()
            .collect();

        let mut lines: BTreeMap<usize, Vec<_>> = BTreeMap::new();

        gpui::canvas(
            move |_bounds, _window, _cx| {},
            move |bounds: Bounds<Pixels>, _: (), window: &mut Window, cx: &mut App| {
                window.paint_layer(bounds, |window| {
                    let accent_colors = cx.theme().accents();

                    // No row background is painted here: it belongs to the
                    // table row underneath, which now spans the graph too.
                    // Painting it here as well would double-blend the
                    // translucent hover colour and leave a seam at the graph's
                    // edge.

                    for (row_idx, row) in rows.into_iter().enumerate() {
                        let row_color = accent_colors.color_for_index(row.color_idx as u32);
                        let row_y_center =
                            bounds.origin.y + row_idx as f32 * row_height + row_height / 2.0
                                - vertical_scroll_offset;

                        let commit_x = lane_center_x(bounds, row.lane as f32);

                        draw_commit_circle(commit_x, row_y_center, row_color, window);
                    }

                    for line in commit_lines {
                        let Some((start_segment_idx, start_column)) =
                            line.get_first_visible_segment_idx(first_visible_row)
                        else {
                            continue;
                        };

                        let line_x = lane_center_x(bounds, start_column as f32);

                        let start_row = line.full_interval.start as i32 - first_visible_row as i32;

                        let from_y =
                            bounds.origin.y + start_row as f32 * row_height + row_height / 2.0
                                - vertical_scroll_offset
                                + COMMIT_CIRCLE_RADIUS;

                        let mut current_row = from_y;
                        let mut current_column = line_x;

                        let mut builder = PathBuilder::stroke(LINE_WIDTH);
                        builder.move_to(point(line_x, from_y));

                        let segments = &line.segments[start_segment_idx..];
                        // How far short of a commit dot an edge stops, so it
                        // never paints over a dot of a different colour.
                        let dot_clearance =
                            f32::from(COMMIT_CIRCLE_RADIUS + COMMIT_CIRCLE_STROKE_WIDTH);

                        for (segment_idx, segment) in segments.iter().enumerate() {
                            let is_last = segment_idx + 1 == segments.len();

                            match segment {
                                CommitLineSegment::Straight { to_row } => {
                                    let mut dest_row = to_row_center(
                                        to_row - first_visible_row,
                                        row_height,
                                        vertical_scroll_offset,
                                        bounds,
                                    );
                                    if is_last {
                                        dest_row -= COMMIT_CIRCLE_RADIUS;
                                    }

                                    let dest_point = point(current_column, dest_row);

                                    current_row = dest_point.y;
                                    builder.line_to(dest_point);
                                    builder.move_to(dest_point);
                                }
                                CommitLineSegment::Curve {
                                    to_column,
                                    on_row,
                                    curve_kind,
                                } => {
                                    let to_column = lane_center_x(bounds, *to_column as f32);

                                    let mut to_row = to_row_center(
                                        *on_row - first_visible_row,
                                        row_height,
                                        vertical_scroll_offset,
                                        bounds,
                                    );

                                    // Both kinds change lanes over a single
                                    // cubic that leaves and arrives parallel to
                                    // the lanes, spreading the bend across the
                                    // whole transition. Its height comes from
                                    // the row height and the number of lanes
                                    // crossed, so a wide fan-in gets the room to
                                    // stay a curve; the elbow this replaced put
                                    // the entire lane change in one corner.
                                    let pen_end = match curve_kind {
                                        // A branch runs down its parent's lane and
                                        // only crosses into its own on the row it
                                        // is drawn at.
                                        CurveKind::Checkout => {
                                            let travel = to_row - current_row;
                                            let height = lane_transition_height(
                                                row_height,
                                                to_column - current_column,
                                                travel,
                                            ) * travel.signum();
                                            let curve_start =
                                                point(current_column, to_row - height);
                                            let landing = point(to_column, to_row);
                                            // The curve arrives along the target
                                            // lane, so it clears the dot by backing
                                            // up that lane, not along a diagonal.
                                            let (curve_start, curve_end) = if is_last {
                                                clear_lane_transition_dots(
                                                    curve_start,
                                                    landing,
                                                    0.,
                                                    dot_clearance,
                                                )
                                            } else {
                                                (curve_start, landing)
                                            };

                                            builder.line_to(curve_start);
                                            stroke_lane_transition(
                                                &mut builder,
                                                curve_start,
                                                curve_end,
                                            );
                                            curve_end
                                        }
                                        // A merge edge leaves its commit's dot for
                                        // the parent's lane, then drops down it.
                                        CurveKind::Merge => {
                                            if is_last {
                                                to_row -= COMMIT_CIRCLE_RADIUS;
                                            }

                                            // The line entered this segment already
                                            // clear of the dot; the curve has to
                                            // leave from the dot's centre instead,
                                            // or it starts off-axis.
                                            let origin = point(
                                                current_column,
                                                current_row - COMMIT_CIRCLE_RADIUS,
                                            );
                                            let travel = to_row - origin.y;
                                            let height = lane_transition_height(
                                                row_height,
                                                to_column - origin.x,
                                                travel,
                                            ) * travel.signum();
                                            let (curve_start, curve_end) =
                                                clear_lane_transition_dots(
                                                    origin,
                                                    point(to_column, origin.y + height),
                                                    dot_clearance,
                                                    0.,
                                                );

                                            stroke_lane_transition(
                                                &mut builder,
                                                curve_start,
                                                curve_end,
                                            );
                                            let landing = point(to_column, to_row);
                                            builder.line_to(landing);
                                            landing
                                        }
                                    };
                                    current_row = pen_end.y;
                                    current_column = pen_end.x;
                                    builder.move_to(pen_end);
                                }
                            }
                        }

                        builder.close();
                        lines.entry(line.color_idx).or_default().push(builder);
                    }

                    for (color_idx, builders) in lines {
                        let line_color = accent_colors.color_for_index(color_idx as u32);

                        for builder in builders {
                            if let Ok(path) = builder.build() {
                                // we paint each color on it's own layer to stop overlapping lines
                                // of different colors changing the color of a line
                                window.paint_layer(bounds, |window| {
                                    window.paint_path(path, line_color);
                                });
                            }
                        }
                    }
                })
            },
        )
        .w(graph_width)
        .h_full()
    }
}

impl GitGraph {
    /// Commit count to render plus whether the underlying fetch is still in
    /// flight. Extracted from `render` so tests can assert the loading state
    /// (a stuck "Loading" is otherwise invisible to assertions).
    fn resolve_commit_count(&mut self, cx: &mut Context<Self>) -> (usize, bool) {
        match self.graph_data.max_commit_count {
            // A locally-cached `Loaded(count)` with `count > 0` is the steady
            // state — render from the cache. But `Loaded(0)` is NOT terminal:
            // the very first paint calls `add_commits(&[])` with an empty batch
            // (the async fetch has only just been kicked off, nothing streamed
            // yet), and `add_commits` unconditionally sets `Loaded(0)`. If we
            // treated that as terminal the graph would stick on "Loading"
            // forever — the fast-path never re-reads the repository, so a fetch
            // that finishes later (or a freshly-created solution whose repo only
            // becomes ready after mount) is never picked up. Fall through to the
            // re-read path, which reports the repository's real `is_loading` and
            // pulls in whatever has loaded.
            AllCommitCount::Loaded(count) if count > 0 => (count, true),
            AllCommitCount::Loaded(_) | AllCommitCount::NotLoaded => {
                let extra_args = self.combined_extra_args();
                let extra_paths = self.filters.paths_args();
                if let Some(repository) = self.get_repository(cx) {
                    repository.update(cx, |repository, cx| {
                        // Start loading the graph data if we haven't started already
                        let GraphDataResponse {
                            commits,
                            is_loading,
                            error: _,
                        } = repository.graph_data(
                            self.log_source.clone(),
                            self.log_order,
                            extra_args.clone(),
                            extra_paths.clone(),
                            0..usize::MAX,
                            cx,
                        );
                        self.graph_data.add_commits(&commits);
                        (commits.len(), is_loading)
                    })
                } else {
                    (0, false)
                }
            }
        }
    }
}

impl Render for GitGraph {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_default_column_widths(window, cx);
        let (mut commit_count, is_loading) = self.resolve_commit_count(cx);

        // S-FHT: when "With Local Changes" is enabled, prepend a synthetic
        // row at index 0 representing the uncommitted state. The row has no
        // backing `Oid` and is rendered with a distinct `local-changes`
        // marker; downstream click / select logic treats it as a no-op.
        if self.has_local_changes_row() {
            commit_count = commit_count.saturating_add(1);
        }

        let extra_args = self.combined_extra_args();
        let extra_paths = self.filters.paths_args();
        let error = self.get_repository(cx).and_then(|repo| {
            repo.read(cx)
                .get_graph_data(
                    self.log_source.clone(),
                    self.log_order,
                    &extra_args,
                    &extra_paths,
                )
                .and_then(|data| data.error.clone())
        });

        let content = if commit_count == 0 {
            let message = if let Some(error) = &error {
                format!("Error loading: {}", error)
            } else if is_loading {
                "Loading".to_string()
            } else {
                "No commits found".to_string()
            };
            let label = Label::new(message)
                .color(Color::Muted)
                .size(LabelSize::Large);
            div()
                .size_full()
                .h_flex()
                .gap_1()
                .items_center()
                .justify_center()
                .child(label)
                .when(is_loading && error.is_none(), |this| {
                    this.child(self.render_loading_spinner(cx))
                })
        } else {
            let is_file_history = matches!(self.log_source, LogSource::Path(_));
            let header_resize_info =
                HeaderResizeInfo::from_redistributable(&self.column_widths, cx);
            let header_context = TableRenderContext::for_column_widths(
                Some(self.column_widths.read(cx).widths_to_render()),
                true,
            );
            let table_width_config = self.table_column_width_config(window, cx);
            // Width of the (non-resizable) commit-graph region. The graph is
            // painted *over* the left edge of the Description column rather
            // than living in a column of its own: only then can each row's
            // subject start at that row's own graph extent (IDEA-style) instead
            // of every row being pushed behind the widest row in the log.
            let graph_width = self.graph_column_width(window, cx);
            // Where the widest row's subject starts, so the "Description"
            // caption lines up with the leftmost subject text in the table.
            let widest_row_indent = graph_row_extent(self.graph_data.max_lanes).min(graph_width);

            h_flex().size_full().child(
                div()
                    .flex_1()
                    .min_w_0()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(
                        h_flex()
                            .w_full()
                            .items_stretch()
                            .child(div().w_full().child(render_table_header(
                                TableRow::from_vec(
                                    vec![
                                                h_flex()
                                                    .when(!is_file_history, |this| {
                                                        this.child(
                                                            div()
                                                                .flex_none()
                                                                .w(widest_row_indent)
                                                                .overflow_hidden()
                                                                .child(
                                                                    Label::new("Graph")
                                                                        .color(Color::Muted)
                                                                        .truncate(),
                                                                ),
                                                        )
                                                    })
                                                    .child(
                                                        Label::new("Description")
                                                            .color(Color::Muted)
                                                            .truncate(),
                                                    )
                                                    .into_any_element(),
                                                Label::new("Date")
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                                Label::new("Author")
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                            ],
                                    3,
                                ),
                                header_context,
                                Some(header_resize_info),
                                // Only seeds the header cells' `ElementId`s, so
                                // it wants the one entity that lives as long as
                                // the view -- which is what `data_table`'s own
                                // call site passes. `column_widths` is replaced
                                // on every re-derivation, and keying off it
                                // would throw away each cell's retained
                                // interactivity state whenever the table is
                                // resized.
                                Some(self.table_interaction_state.entity_id()),
                                cx,
                            ))),
                    )
                    .child({
                        let row_height = Self::row_height(window, cx);
                        let selected_entry_idxs = self.selected_entry_idxs.clone();
                        let hovered_entry_idx = self.hovered_entry_idx;
                        let weak_self = cx.weak_entity();
                        let focus_handle = self.focus_handle.clone();

                        // No `id`, no listeners: the overlay must not
                        // register a hitbox, or it would swallow hover,
                        // clicks and ref-chip presses on the table rows it
                        // covers. All row interaction (including the
                        // context menu, which the old graph column never
                        // had) comes from the table underneath.
                        let graph_canvas = div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .h_full()
                            .w(graph_width)
                            .overflow_hidden()
                            .child(self.render_graph_canvas(window, cx));

                        let commits_table = Table::new(3)
                            .interactable(&self.table_interaction_state)
                            .hide_row_borders()
                            .hide_row_hover()
                            .width_config(table_width_config)
                            .map_row(move |(index, row), window, cx| {
                                let is_selected = selected_entry_idxs.contains(&index);
                                let is_hovered = hovered_entry_idx == Some(index);
                                let is_focused = focus_handle.is_focused(window);
                                let weak = weak_self.clone();
                                let weak_for_hover = weak.clone();

                                let hover_bg = cx.theme().colors().element_hover.opacity(0.6);
                                let selected_bg = if is_focused {
                                    cx.theme().colors().element_selected
                                } else {
                                    cx.theme().colors().element_hover
                                };

                                row.h(row_height)
                                    .when(is_selected, |row| row.bg(selected_bg))
                                    .when(is_hovered && !is_selected, |row| row.bg(hover_bg))
                                    .on_hover(move |&is_hovered, _, cx| {
                                        weak_for_hover
                                            .update(cx, |this, cx| {
                                                if is_hovered {
                                                    if this.hovered_entry_idx != Some(index) {
                                                        this.hovered_entry_idx = Some(index);
                                                        cx.notify();
                                                    }
                                                } else if this.hovered_entry_idx == Some(index) {
                                                    this.hovered_entry_idx = None;
                                                    cx.notify();
                                                }
                                            })
                                            .ok();
                                    })
                                    .on_click({
                                        let weak = weak.clone();
                                        move |event: &ClickEvent, window, cx| {
                                            let click_count = event.click_count();
                                            let modifiers = event.modifiers();
                                            weak.update(cx, |this, cx| {
                                                this.on_row_click(
                                                    index,
                                                    click_count,
                                                    modifiers,
                                                    window,
                                                    cx,
                                                );
                                            })
                                            .ok();
                                        }
                                    })
                                    .on_mouse_down(MouseButton::Right, {
                                        move |event: &MouseDownEvent, window, cx| {
                                            if event.button != MouseButton::Right {
                                                return;
                                            }
                                            weak.update(cx, |this, cx| {
                                                // A right-click inside an
                                                // existing multi-row
                                                // selection keeps it and
                                                // acts on all of it;
                                                // anywhere else it selects
                                                // the row first, exactly
                                                // as a plain click would.
                                                if this.selected_entry_idxs.len() >= 2
                                                    && this.selected_entry_idxs.contains(&index)
                                                {
                                                    this.deploy_multi_commit_context_menu(
                                                        event.position,
                                                        window,
                                                        cx,
                                                    );
                                                    return;
                                                }
                                                this.select_entry(
                                                    index,
                                                    ScrollStrategy::Center,
                                                    CommitSelectionSource::UserGesture,
                                                    window,
                                                    cx,
                                                );
                                                this.deploy_commit_context_menu(
                                                    index,
                                                    event.position,
                                                    window,
                                                    cx,
                                                );
                                            })
                                            .ok();
                                            cx.stop_propagation();
                                        }
                                    })
                                    .into_any_element()
                            })
                            .uniform_list(
                                "git-graph-commits",
                                commit_count,
                                cx.processor(Self::render_table_rows),
                            );

                        h_flex()
                            .relative()
                            .flex_1()
                            .w_full()
                            .items_stretch()
                            // A wrapper purely so the table's laid-out width can
                            // be observed: `bind_redistributable_columns`
                            // installs its own `on_children_prepainted` on the
                            // div it is handed, and a div holds only one such
                            // listener, so a second one has to sit outside it.
                            // The wrapper's only child is the table, making the
                            // reported bounds unambiguous.
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .on_children_prepainted({
                                        let this = cx.weak_entity();
                                        move |bounds, _window, cx| {
                                            this.update(cx, |this, cx| {
                                                this.observe_table_width(&bounds, cx);
                                            })
                                            .ok();
                                        }
                                    })
                                    .child(bind_redistributable_columns(
                                        div()
                                            .relative()
                                            .flex_1()
                                            .min_w_0()
                                            .h_full()
                                            .overflow_hidden()
                                            .child(commits_table)
                                            .child(render_redistributable_columns_resize_handles(
                                                &self.column_widths,
                                                window,
                                                cx,
                                            )),
                                        self.column_widths.clone(),
                                    )),
                            )
                            // Last child, so the DAG paints on top of the
                            // table's row background instead of being
                            // covered by it on the hovered/selected row.
                            .when(!is_file_history, |this| this.child(graph_canvas))
                    }),
            )
        };

        div()
            .key_context("GitGraph")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(|this, _: &OpenCommitView, window, cx| {
                this.open_selected_commit_view(window, cx);
            }))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.search_state
                    .editor
                    .update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
            }))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(|this, _: &ToggleCaseSensitive, window, cx| {
                this.search_state.case_sensitive = !this.search_state.case_sensitive;
                this.update_query_filter(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleRegex, window, cx| {
                this.search_state.regex = !this.search_state.regex;
                this.update_query_filter(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Refresh, _window, cx| {
                this.refresh(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleSearchInDiffs, window, cx| {
                this.search_state.search_in_diffs = !this.search_state.search_in_diffs;
                this.update_query_filter(window, cx);
                cx.notify();
            }))
            .on_action(
                cx.listener(|this, action: &ShowAffectedPathsInLog, _window, cx| {
                    let paths: Vec<git::repository::RepoPath> = action
                        .paths
                        .iter()
                        .filter_map(|p| git::repository::RepoPath::new(p).ok())
                        .collect();
                    this.set_path_filter(paths, cx);
                }),
            )
            .child(
                v_flex()
                    .size_full()
                    .child(self.render_log_toolbar(cx))
                    .child(div().flex_1().child(content)),
            )
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
            .on_action(cx.listener(|_, _: &buffer_search::Deploy, window, cx| {
                window.dispatch_action(Box::new(FocusSearch), cx);
                cx.stop_propagation();
            }))
    }
}

impl EventEmitter<ItemEvent> for GitGraph {}

impl Focusable for GitGraph {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for GitGraph {
    type Event = ItemEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitGraph))
    }

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        let repo_name = self.get_repository(cx).and_then(|repo| {
            repo.read(cx)
                .work_directory_abs_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        });
        let file_history_path = match &self.log_source {
            LogSource::Path(path) => Some(path.as_unix_str().to_string()),
            _ => None,
        };

        Some(TabTooltipContent::Custom(Box::new(Tooltip::element({
            move |_, _| {
                v_flex()
                    .child(Label::new(if file_history_path.is_some() {
                        "File History"
                    } else {
                        "Git Graph"
                    }))
                    .when_some(file_history_path.clone(), |this, path| {
                        this.child(Label::new(path).color(Color::Muted).size(LabelSize::Small))
                    })
                    .when_some(repo_name.clone(), |this, name| {
                        this.child(Label::new(name).color(Color::Muted).size(LabelSize::Small))
                    })
                    .into_any_element()
            }
        }))))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        if let LogSource::Path(path) = &self.log_source {
            return path
                .as_ref()
                .file_name()
                .map(|name| SharedString::from(name.to_string()))
                .unwrap_or_else(|| SharedString::from(path.as_unix_str().to_string()));
        }

        self.get_repository(cx)
            .and_then(|repo| {
                repo.read(cx)
                    .work_directory_abs_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .map_or_else(|| "Git Graph".into(), |name| SharedString::from(name))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl workspace::SerializableItem for GitGraph {
    fn serialized_item_kind() -> &'static str {
        "GitGraph"
    }

    fn cleanup(
        workspace_id: workspace::WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<gpui::Result<()>> {
        workspace::delete_unloaded_items(
            alive_items,
            workspace_id,
            "git_graphs",
            &persistence::GitGraphsDb::global(cx),
            cx,
        )
    }

    fn deserialize(
        project: Entity<project::Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<gpui::Result<Entity<Self>>> {
        let db = persistence::GitGraphsDb::global(cx);
        let Some((
            repo_work_path,
            log_source_type,
            log_source_value,
            log_order,
            selected_sha,
            search_query,
            search_case_sensitive,
            search_regex,
            search_in_diffs,
            filter_branches,
            filter_authors,
            filter_paths,
            filter_date_since,
            filter_date_until,
            filter_all_refs,
            highlight_my_commits,
            highlight_new_since_refresh,
            highlight_last_seen_sha,
            view_compact_refs,
            view_group_by_date,
            view_follow_renames,
            view_with_local_changes,
            view_show_inline_diff,
        )) = db.get_git_graph(item_id, workspace_id).ok().flatten()
        else {
            return Task::ready(Err(anyhow::anyhow!("No git graph to deserialize")));
        };

        let state = persistence::SerializedGitGraphState {
            log_source_type,
            log_source_value,
            log_order,
            selected_sha,
            search_query,
            search_case_sensitive,
            search_regex,
            search_in_diffs,
            filter_branches,
            filter_authors,
            filter_paths,
            filter_date_since,
            filter_date_until,
            filter_all_refs,
            highlight_my_commits,
            highlight_new_since_refresh,
            highlight_last_seen_sha,
            view_compact_refs,
            view_group_by_date,
            view_follow_renames,
            view_with_local_changes,
            view_show_inline_diff,
        };

        let window_handle = window.window_handle();
        let project = project.read(cx);
        let git_store = project.git_store().clone();
        let wait = project.wait_for_initial_scan(cx);

        cx.spawn(async move |cx| {
            wait.await;

            cx.update_window(window_handle, |_, window, cx| {
                let path = repo_work_path.as_path();

                let repositories = git_store.read(cx).repositories();
                let repo_id = repositories.iter().find_map(|(&repo_id, repo)| {
                    if repo.read(cx).snapshot().work_directory_abs_path.as_ref() == path {
                        Some(repo_id)
                    } else {
                        None
                    }
                });

                let Some(repo_id) = repo_id else {
                    return Err(anyhow::anyhow!("Repository not found for path: {:?}", path));
                };

                let log_source = persistence::deserialize_log_source(&state);
                let log_order = persistence::deserialize_log_order(&state);
                let filters = persistence::deserialize_log_filters(&state);
                let highlights = persistence::deserialize_highlights(&state);
                let view_options = persistence::deserialize_view_options(&state);
                let file_history_options = persistence::deserialize_file_history_options(&state);

                let case_sensitive = state.search_case_sensitive.unwrap_or(false);
                let regex = state.search_regex.unwrap_or(false);
                let search_in_diffs = state.search_in_diffs.unwrap_or(false);
                let mut filters = filters;
                filters.query =
                    state
                        .search_query
                        .as_deref()
                        .filter(|q| !q.is_empty())
                        .map(|text| filters::QueryFilter {
                            text: text.to_string().into(),
                            regex,
                            case_sensitive,
                            search_in_diffs,
                        });

                let git_graph = cx.new(|cx| {
                    let mut graph =
                        GitGraph::new(repo_id, git_store, workspace, Some(log_source), window, cx);
                    graph.log_order = log_order;
                    graph.filters = filters;
                    graph.highlights = highlights;
                    graph.view_options = view_options;
                    graph.file_history_options = file_history_options;
                    graph.search_state.case_sensitive = case_sensitive;
                    graph.search_state.regex = regex;
                    graph.search_state.search_in_diffs = search_in_diffs;
                    // `GitGraph::new` already kicked off a fetch with default
                    // filters and (if the empty-args cache was already
                    // populated by another `GitGraph` for the same repo)
                    // synchronously copied those commits into `graph_data`.
                    // Reset so the subsequent fetch's `CountUpdated` handler
                    // computes a correct `old_count..commit_count` slice
                    // against the now-active filtered cache instead of
                    // collapsing the range against the leftover unfiltered
                    // count.
                    graph.graph_data.clear();
                    graph.fetch_initial_graph_data(cx);

                    if let Some(sha) = &state.selected_sha {
                        // Restoring a persisted selection is not a gesture: the
                        // Commit tab is ephemeral and the panel boots on
                        // Changes, so a restore must not open one.
                        graph.select_commit_by_sha(
                            sha.as_str(),
                            CommitSelectionSource::Background,
                            window,
                            cx,
                        );
                    }

                    graph
                });

                if let Some(query_text) = state.search_query.as_deref().filter(|q| !q.is_empty()) {
                    git_graph.update(cx, |graph, cx| {
                        graph
                            .search_state
                            .editor
                            .update(cx, |editor, cx| editor.set_text(query_text, window, cx));
                        // The text-edit subscription would otherwise schedule
                        // a redundant 250ms-debounced refetch with the exact
                        // same query we already hydrated into filters.query.
                        graph.search_state.debounce_task = None;
                    });
                }

                Ok(git_graph)
            })?
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<gpui::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let repo = self.get_repository(cx)?;
        let repo_working_path = repo
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .to_string_lossy()
            .to_string();

        // `selected_entry_idx` is view-space: when the synthetic
        // local-changes row occupies view 0 every commit sits one row below
        // its index in `graph_data.commits`, and that row is not a commit at
        // all. `selected_commit_sha` is the conversion.
        let selected_sha = self.selected_commit_sha().map(|sha| sha.to_string());

        let search_query = self.search_state.editor.read(cx).text(cx);
        let search_query = if search_query.is_empty() {
            None
        } else {
            Some(search_query)
        };

        let log_source_type = Some(persistence::serialize_log_source_type(&self.log_source));
        let log_source_value = persistence::serialize_log_source_value(&self.log_source);
        let log_order = Some(persistence::serialize_log_order(&self.log_order));
        let search_case_sensitive = Some(self.search_state.case_sensitive);
        let search_regex = if self.search_state.regex {
            Some(true)
        } else {
            None
        };
        let search_in_diffs = if self.search_state.search_in_diffs {
            Some(true)
        } else {
            None
        };

        let filter_columns = persistence::serialize_log_filters(&self.filters);
        let highlight_columns = persistence::serialize_highlights(&self.highlights);
        let view_columns = persistence::serialize_view_options(&self.view_options);
        let file_history_columns =
            persistence::serialize_file_history_options(&self.file_history_options);

        let db = persistence::GitGraphsDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_git_graph(
                item_id,
                workspace_id,
                repo_working_path,
                log_source_type,
                log_source_value,
                log_order,
                selected_sha,
                search_query,
                search_case_sensitive,
                search_regex,
                search_in_diffs,
                filter_columns.branches,
                filter_columns.authors,
                filter_columns.paths,
                filter_columns.date_since,
                filter_columns.date_until,
                filter_columns.all_refs,
                highlight_columns.my_commits,
                highlight_columns.new_since_refresh,
                highlight_columns.last_seen_sha,
                view_columns.compact_refs,
                view_columns.group_by_date,
                file_history_columns.follow_renames,
                file_history_columns.with_local_changes,
                file_history_columns.show_inline_diff,
            )
            .await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        match event {
            ItemEvent::UpdateTab | ItemEvent::Edit => true,
            _ => false,
        }
    }
}

mod persistence {
    use std::{path::PathBuf, str::FromStr};

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use git::{
        Oid,
        repository::{LogOrder, LogSource, RepoPath},
    };
    use gpui::SharedString;
    use workspace::WorkspaceDb;

    use crate::{
        file_history::FileHistoryOptions,
        filters::{DateRange, LogFilters},
        highlights::HighlightSet,
        view_options::ViewOptions,
    };

    pub struct GitGraphsDb(ThreadSafeConnection);

    impl Domain for GitGraphsDb {
        const NAME: &str = stringify!(GitGraphsDb);

        const MIGRATIONS: &[&str] = &[
            sql!(
                CREATE TABLE git_graphs (
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,
                    is_open INTEGER DEFAULT FALSE,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN repo_working_path TEXT;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN log_source_type TEXT;
                ALTER TABLE git_graphs ADD COLUMN log_source_value TEXT;
                ALTER TABLE git_graphs ADD COLUMN log_order TEXT;
                ALTER TABLE git_graphs ADD COLUMN selected_sha TEXT;
                ALTER TABLE git_graphs ADD COLUMN search_query TEXT;
                ALTER TABLE git_graphs ADD COLUMN search_case_sensitive INTEGER;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN filter_branches TEXT;
                ALTER TABLE git_graphs ADD COLUMN filter_authors TEXT;
                ALTER TABLE git_graphs ADD COLUMN filter_paths TEXT;
                ALTER TABLE git_graphs ADD COLUMN filter_date_since INTEGER;
                ALTER TABLE git_graphs ADD COLUMN filter_date_until INTEGER;
                ALTER TABLE git_graphs ADD COLUMN filter_all_refs INTEGER;
                ALTER TABLE git_graphs ADD COLUMN highlight_my_commits INTEGER;
                ALTER TABLE git_graphs ADD COLUMN highlight_new_since_refresh INTEGER;
                ALTER TABLE git_graphs ADD COLUMN highlight_last_seen_sha TEXT;
                ALTER TABLE git_graphs ADD COLUMN view_compact_refs INTEGER;
                ALTER TABLE git_graphs ADD COLUMN view_group_by_date INTEGER;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN search_regex INTEGER;
                ALTER TABLE git_graphs ADD COLUMN search_in_diffs INTEGER;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN view_follow_renames INTEGER;
                ALTER TABLE git_graphs ADD COLUMN view_with_local_changes INTEGER;
                ALTER TABLE git_graphs ADD COLUMN view_show_inline_diff INTEGER;
            ),
        ];
    }

    db::static_connection!(GitGraphsDb, [WorkspaceDb]);

    pub const LOG_SOURCE_ALL: i32 = 0;
    pub const LOG_SOURCE_BRANCH: i32 = 1;
    pub const LOG_SOURCE_SHA: i32 = 2;
    pub const LOG_SOURCE_FILE: i32 = 3;

    pub const LOG_ORDER_DATE: i32 = 0;
    pub const LOG_ORDER_TOPO: i32 = 1;
    pub const LOG_ORDER_AUTHOR_DATE: i32 = 2;
    pub const LOG_ORDER_REVERSE: i32 = 3;

    pub fn serialize_log_source_type(log_source: &LogSource) -> i32 {
        match log_source {
            LogSource::All => LOG_SOURCE_ALL,
            LogSource::Branch(_) => LOG_SOURCE_BRANCH,
            LogSource::Sha(_) => LOG_SOURCE_SHA,
            LogSource::Path(_) => LOG_SOURCE_FILE,
        }
    }

    pub fn serialize_log_source_value(log_source: &LogSource) -> Option<String> {
        match log_source {
            LogSource::All => None,
            LogSource::Branch(branch) => Some(branch.to_string()),
            LogSource::Sha(oid) => Some(oid.to_string()),
            LogSource::Path(path) => Some(path.as_unix_str().to_string()),
        }
    }

    pub fn serialize_log_order(log_order: &LogOrder) -> i32 {
        match log_order {
            LogOrder::DateOrder => LOG_ORDER_DATE,
            LogOrder::TopoOrder => LOG_ORDER_TOPO,
            LogOrder::AuthorDateOrder => LOG_ORDER_AUTHOR_DATE,
            LogOrder::ReverseChronological => LOG_ORDER_REVERSE,
        }
    }

    pub fn deserialize_log_source(state: &SerializedGitGraphState) -> LogSource {
        match state.log_source_type {
            Some(LOG_SOURCE_ALL) => LogSource::All,
            Some(LOG_SOURCE_BRANCH) => state
                .log_source_value
                .as_ref()
                .map(|v| LogSource::Branch(v.clone().into()))
                .unwrap_or_default(),
            Some(LOG_SOURCE_SHA) => state
                .log_source_value
                .as_ref()
                .and_then(|v| Oid::from_str(v).ok())
                .map(LogSource::Sha)
                .unwrap_or_default(),
            Some(LOG_SOURCE_FILE) => state
                .log_source_value
                .as_ref()
                .and_then(|v| RepoPath::new(v).ok())
                .map(LogSource::Path)
                .unwrap_or_default(),
            None | Some(_) => LogSource::default(),
        }
    }

    pub fn deserialize_log_order(state: &SerializedGitGraphState) -> LogOrder {
        match state.log_order {
            Some(LOG_ORDER_DATE) => LogOrder::DateOrder,
            Some(LOG_ORDER_TOPO) => LogOrder::TopoOrder,
            Some(LOG_ORDER_AUTHOR_DATE) => LogOrder::AuthorDateOrder,
            Some(LOG_ORDER_REVERSE) => LogOrder::ReverseChronological,
            _ => LogOrder::default(),
        }
    }

    #[derive(Debug, Default, Clone)]
    pub struct SerializedGitGraphState {
        pub log_source_type: Option<i32>,
        pub log_source_value: Option<String>,
        pub log_order: Option<i32>,
        pub selected_sha: Option<String>,
        pub search_query: Option<String>,
        pub search_case_sensitive: Option<bool>,
        pub search_regex: Option<bool>,
        pub search_in_diffs: Option<bool>,
        pub filter_branches: Option<String>,
        pub filter_authors: Option<String>,
        pub filter_paths: Option<String>,
        pub filter_date_since: Option<i64>,
        pub filter_date_until: Option<i64>,
        pub filter_all_refs: Option<bool>,
        pub highlight_my_commits: Option<bool>,
        pub highlight_new_since_refresh: Option<bool>,
        pub highlight_last_seen_sha: Option<String>,
        pub view_compact_refs: Option<bool>,
        pub view_group_by_date: Option<bool>,
        pub view_follow_renames: Option<bool>,
        pub view_with_local_changes: Option<bool>,
        pub view_show_inline_diff: Option<bool>,
    }

    /// Column values produced from a [`LogFilters`] for the `save_git_graph`
    /// query. Bundled to keep the function signature manageable.
    #[derive(Debug, Default, Clone)]
    pub struct SerializedFilterColumns {
        pub branches: Option<String>,
        pub authors: Option<String>,
        pub paths: Option<String>,
        pub date_since: Option<i64>,
        pub date_until: Option<i64>,
        pub all_refs: Option<bool>,
    }

    #[derive(Debug, Default, Clone)]
    pub struct SerializedHighlightColumns {
        pub my_commits: Option<bool>,
        pub new_since_refresh: Option<bool>,
        pub last_seen_sha: Option<String>,
    }

    #[derive(Debug, Default, Clone)]
    pub struct SerializedViewColumns {
        pub compact_refs: Option<bool>,
        pub group_by_date: Option<bool>,
    }

    /// Persisted columns for [`FileHistoryOptions`]. Optional shape so
    /// pre-S-FHT rows hydrate to defaults via `unwrap_or` in the load
    /// path.
    #[derive(Debug, Default, Clone)]
    pub struct SerializedFileHistoryColumns {
        pub follow_renames: Option<bool>,
        pub with_local_changes: Option<bool>,
        pub show_inline_diff: Option<bool>,
    }

    pub fn serialize_log_filters(filters: &LogFilters) -> SerializedFilterColumns {
        let branches = if filters.branches.is_empty() {
            None
        } else {
            let raw: Vec<&str> = filters.branches.iter().map(|s| s.as_ref()).collect();
            serde_json::to_string(&raw).ok()
        };
        let authors = if filters.authors.is_empty() {
            None
        } else {
            let raw: Vec<&str> = filters.authors.iter().map(|s| s.as_ref()).collect();
            serde_json::to_string(&raw).ok()
        };
        let paths = if filters.paths.is_empty() {
            None
        } else {
            let raw: Vec<String> = filters
                .paths
                .iter()
                .map(|p| p.as_unix_str().to_string())
                .collect();
            serde_json::to_string(&raw).ok()
        };
        let (date_since, date_until) = match filters.date_range {
            Some(DateRange::Since(s)) => (Some(s), None),
            Some(DateRange::Until(u)) => (None, Some(u)),
            Some(DateRange::Between { since, until }) => (Some(since), Some(until)),
            None => (None, None),
        };
        let all_refs = if filters.all_refs { Some(true) } else { None };

        SerializedFilterColumns {
            branches,
            authors,
            paths,
            date_since,
            date_until,
            all_refs,
        }
    }

    pub fn deserialize_log_filters(state: &SerializedGitGraphState) -> LogFilters {
        let branches = decode_string_vec(state.filter_branches.as_deref(), "filter_branches")
            .into_iter()
            .map(SharedString::from)
            .collect();
        let authors = decode_string_vec(state.filter_authors.as_deref(), "filter_authors")
            .into_iter()
            .map(SharedString::from)
            .collect();
        let paths = decode_string_vec(state.filter_paths.as_deref(), "filter_paths")
            .into_iter()
            .filter_map(|s| match RepoPath::new(&s) {
                Ok(p) => Some(p),
                Err(err) => {
                    log::warn!("git_graph: skipping invalid persisted path {s:?}: {err}");
                    None
                }
            })
            .collect();
        let date_range = match (state.filter_date_since, state.filter_date_until) {
            (Some(since), Some(until)) => Some(DateRange::Between { since, until }),
            (Some(since), None) => Some(DateRange::Since(since)),
            (None, Some(until)) => Some(DateRange::Until(until)),
            (None, None) => None,
        };

        LogFilters {
            branches,
            authors,
            date_range,
            paths,
            query: None,
            all_refs: state.filter_all_refs.unwrap_or(false),
            sha: None,
        }
    }

    pub fn serialize_highlights(h: &HighlightSet) -> SerializedHighlightColumns {
        SerializedHighlightColumns {
            my_commits: if h.my_commits { Some(true) } else { None },
            new_since_refresh: if h.new_since_refresh {
                Some(true)
            } else {
                None
            },
            last_seen_sha: h.last_seen_sha.map(|oid| oid.to_string()),
        }
    }

    pub fn deserialize_highlights(state: &SerializedGitGraphState) -> HighlightSet {
        let last_seen_sha =
            state
                .highlight_last_seen_sha
                .as_deref()
                .and_then(|s| match Oid::from_str(s) {
                    Ok(oid) => Some(oid),
                    Err(err) => {
                        log::warn!(
                            "git_graph: dropping invalid persisted last_seen_sha {s:?}: {err}"
                        );
                        None
                    }
                });
        HighlightSet {
            my_commits: state.highlight_my_commits.unwrap_or(false),
            new_since_refresh: state.highlight_new_since_refresh.unwrap_or(false),
            last_seen_sha,
        }
    }

    pub fn serialize_view_options(v: &ViewOptions) -> SerializedViewColumns {
        SerializedViewColumns {
            compact_refs: if v.compact_refs { Some(true) } else { None },
            group_by_date: if v.group_by_date { Some(true) } else { None },
        }
    }

    pub fn deserialize_view_options(state: &SerializedGitGraphState) -> ViewOptions {
        ViewOptions {
            compact_refs: state.view_compact_refs.unwrap_or(false),
            group_by_date: state.view_group_by_date.unwrap_or(false),
        }
    }

    pub fn serialize_file_history_options(
        opts: &FileHistoryOptions,
    ) -> SerializedFileHistoryColumns {
        // `follow_renames` defaults to `true`, so persist it as `Some(false)`
        // when off (and `None` when on, since absence == default). The other
        // two default to `false`, so the convention is the inverse.
        SerializedFileHistoryColumns {
            follow_renames: if opts.follow_renames {
                None
            } else {
                Some(false)
            },
            with_local_changes: if opts.with_local_changes {
                Some(true)
            } else {
                None
            },
            show_inline_diff: if opts.show_inline_diff {
                Some(true)
            } else {
                None
            },
        }
    }

    pub fn deserialize_file_history_options(state: &SerializedGitGraphState) -> FileHistoryOptions {
        FileHistoryOptions {
            // Default-on: missing column hydrates to `true`.
            follow_renames: state.view_follow_renames.unwrap_or(true),
            with_local_changes: state.view_with_local_changes.unwrap_or(false),
            show_inline_diff: state.view_show_inline_diff.unwrap_or(false),
        }
    }

    fn decode_string_vec(raw: Option<&str>, column: &str) -> Vec<String> {
        match raw {
            None | Some("") => Vec::new(),
            Some(s) => match serde_json::from_str::<Vec<String>>(s) {
                Ok(v) => v,
                Err(err) => {
                    log::warn!(
                        "git_graph: malformed JSON in column {column}: {err}; resetting to empty"
                    );
                    Vec::new()
                }
            },
        }
    }

    /// Column tuples for `save_git_graph` — split into chunks because the
    /// `Bind`/`Column` trait impls only cover tuples of up to 10 elements,
    /// and the full row is wider than that. Tuples nest naturally, so the
    /// `query!` macro can still bind/select into them as one composite row.
    pub type CoreSaveTuple = (
        workspace::ItemId,
        workspace::WorkspaceId,
        String,
        Option<i32>,
        Option<String>,
        Option<i32>,
        Option<String>,
        Option<String>,
        Option<bool>,
    );

    pub type FilterSaveTuple = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<bool>,
    );

    pub type HighlightViewSaveTuple = (
        Option<bool>,
        Option<bool>,
        Option<String>,
        Option<bool>,
        Option<bool>,
    );

    /// Search-bar toggle flags that don't fit in [`CoreSaveTuple`] (already
    /// at the 9-element mark, leaving headroom under sqlez's 10-tuple cap).
    /// Kept as its own sub-tuple so `search_query` / `search_case_sensitive`
    /// can stay co-located with the rest of the core columns.
    pub type SearchExtraSaveTuple = (Option<bool>, Option<bool>);

    /// File-history (S-FHT) toggles persisted alongside the rest of the
    /// view state. Three nullable booleans — see
    /// [`SerializedFileHistoryColumns`] for default semantics.
    pub type FileHistorySaveTuple = (Option<bool>, Option<bool>, Option<bool>);

    pub type FileHistoryLoadTuple = (Option<bool>, Option<bool>, Option<bool>);

    /// Result row for `get_git_graph` — same chunking rationale as
    /// [`CoreSaveTuple`].
    pub type CoreLoadTuple = (
        PathBuf,
        Option<i32>,
        Option<String>,
        Option<i32>,
        Option<String>,
        Option<String>,
        Option<bool>,
    );

    pub type FilterLoadTuple = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<bool>,
    );

    pub type HighlightViewLoadTuple = (
        Option<bool>,
        Option<bool>,
        Option<String>,
        Option<bool>,
        Option<bool>,
    );

    pub type SearchExtraLoadTuple = (Option<bool>, Option<bool>);

    impl GitGraphsDb {
        query! {
            pub async fn save_git_graph_raw(
                core: CoreSaveTuple,
                filters: FilterSaveTuple,
                highlights_view: HighlightViewSaveTuple,
                search_extra: SearchExtraSaveTuple,
                file_history: FileHistorySaveTuple
            ) -> Result<()> {
                INSERT OR REPLACE INTO git_graphs(
                    item_id, workspace_id, repo_working_path,
                    log_source_type, log_source_value, log_order,
                    selected_sha, search_query, search_case_sensitive,
                    filter_branches, filter_authors, filter_paths,
                    filter_date_since, filter_date_until, filter_all_refs,
                    highlight_my_commits, highlight_new_since_refresh, highlight_last_seen_sha,
                    view_compact_refs, view_group_by_date,
                    search_regex, search_in_diffs,
                    view_follow_renames, view_with_local_changes, view_show_inline_diff
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            }
        }

        #[allow(clippy::too_many_arguments)]
        pub async fn save_git_graph(
            &self,
            item_id: workspace::ItemId,
            workspace_id: workspace::WorkspaceId,
            repo_working_path: String,
            log_source_type: Option<i32>,
            log_source_value: Option<String>,
            log_order: Option<i32>,
            selected_sha: Option<String>,
            search_query: Option<String>,
            search_case_sensitive: Option<bool>,
            search_regex: Option<bool>,
            search_in_diffs: Option<bool>,
            filter_branches: Option<String>,
            filter_authors: Option<String>,
            filter_paths: Option<String>,
            filter_date_since: Option<i64>,
            filter_date_until: Option<i64>,
            filter_all_refs: Option<bool>,
            highlight_my_commits: Option<bool>,
            highlight_new_since_refresh: Option<bool>,
            highlight_last_seen_sha: Option<String>,
            view_compact_refs: Option<bool>,
            view_group_by_date: Option<bool>,
            view_follow_renames: Option<bool>,
            view_with_local_changes: Option<bool>,
            view_show_inline_diff: Option<bool>,
        ) -> anyhow::Result<()> {
            let core: CoreSaveTuple = (
                item_id,
                workspace_id,
                repo_working_path,
                log_source_type,
                log_source_value,
                log_order,
                selected_sha,
                search_query,
                search_case_sensitive,
            );
            let filters: FilterSaveTuple = (
                filter_branches,
                filter_authors,
                filter_paths,
                filter_date_since,
                filter_date_until,
                filter_all_refs,
            );
            let highlights_view: HighlightViewSaveTuple = (
                highlight_my_commits,
                highlight_new_since_refresh,
                highlight_last_seen_sha,
                view_compact_refs,
                view_group_by_date,
            );
            let search_extra: SearchExtraSaveTuple = (search_regex, search_in_diffs);
            let file_history: FileHistorySaveTuple = (
                view_follow_renames,
                view_with_local_changes,
                view_show_inline_diff,
            );
            self.save_git_graph_raw(core, filters, highlights_view, search_extra, file_history)
                .await
        }

        query! {
            fn get_git_graph_raw(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId
            ) -> Result<Option<(
                CoreLoadTuple,
                FilterLoadTuple,
                HighlightViewLoadTuple,
                SearchExtraLoadTuple,
                FileHistoryLoadTuple
            )>> {
                SELECT
                    repo_working_path,
                    log_source_type,
                    log_source_value,
                    log_order,
                    selected_sha,
                    search_query,
                    search_case_sensitive,
                    filter_branches,
                    filter_authors,
                    filter_paths,
                    filter_date_since,
                    filter_date_until,
                    filter_all_refs,
                    highlight_my_commits,
                    highlight_new_since_refresh,
                    highlight_last_seen_sha,
                    view_compact_refs,
                    view_group_by_date,
                    search_regex,
                    search_in_diffs,
                    view_follow_renames,
                    view_with_local_changes,
                    view_show_inline_diff
                FROM git_graphs
                WHERE item_id = ? AND workspace_id = ?
            }
        }

        #[allow(clippy::type_complexity)]
        pub fn get_git_graph(
            &self,
            item_id: workspace::ItemId,
            workspace_id: workspace::WorkspaceId,
        ) -> anyhow::Result<
            Option<(
                PathBuf,
                Option<i32>,
                Option<String>,
                Option<i32>,
                Option<String>,
                Option<String>,
                Option<bool>,
                Option<bool>,
                Option<bool>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<i64>,
                Option<i64>,
                Option<bool>,
                Option<bool>,
                Option<bool>,
                Option<String>,
                Option<bool>,
                Option<bool>,
                Option<bool>,
                Option<bool>,
                Option<bool>,
            )>,
        > {
            let row = self.get_git_graph_raw(item_id, workspace_id)?;
            Ok(row.map(
                |(core, filters, highlights_view, search_extra, file_history)| {
                    let (
                        repo_working_path,
                        log_source_type,
                        log_source_value,
                        log_order,
                        selected_sha,
                        search_query,
                        search_case_sensitive,
                    ) = core;
                    let (
                        filter_branches,
                        filter_authors,
                        filter_paths,
                        filter_date_since,
                        filter_date_until,
                        filter_all_refs,
                    ) = filters;
                    let (
                        highlight_my_commits,
                        highlight_new_since_refresh,
                        highlight_last_seen_sha,
                        view_compact_refs,
                        view_group_by_date,
                    ) = highlights_view;
                    let (search_regex, search_in_diffs) = search_extra;
                    let (view_follow_renames, view_with_local_changes, view_show_inline_diff) =
                        file_history;
                    (
                        repo_working_path,
                        log_source_type,
                        log_source_value,
                        log_order,
                        selected_sha,
                        search_query,
                        search_case_sensitive,
                        search_regex,
                        search_in_diffs,
                        filter_branches,
                        filter_authors,
                        filter_paths,
                        filter_date_since,
                        filter_date_until,
                        filter_all_refs,
                        highlight_my_commits,
                        highlight_new_since_refresh,
                        highlight_last_seen_sha,
                        view_compact_refs,
                        view_group_by_date,
                        view_follow_renames,
                        view_with_local_changes,
                        view_show_inline_diff,
                    )
                },
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas_geometry::MIN_GRAPH_LANES;
    use anyhow::{Context, Result, bail};

    #[test]
    fn test_is_hash_like() {
        // Full and short (>= 7) hex → hash lookup.
        assert!(is_hash_like("9509ee5"));
        assert!(is_hash_like("63ecdb1a2f"));
        assert!(is_hash_like(&"a".repeat(40)));
        assert!(is_hash_like("ABCDEF0")); // case-insensitive
        // Too short, over-long, or non-hex → treated as a message grep.
        assert!(!is_hash_like("face")); // 4-char hex word stays a message search
        assert!(!is_hash_like("abc")); // < 7
        assert!(!is_hash_like("")); //
        assert!(!is_hash_like(&"a".repeat(41))); // > 40
        assert!(!is_hash_like("fix bug")); // non-hex
        assert!(!is_hash_like("9509ee5z")); // trailing non-hex
    }

    /// The Date column is sized by measuring [`DATE_COLUMN_SAMPLE`] instead of
    /// the rows' own text, which is only sound while every date the formatter
    /// produces has the sample's shape. Changing `timestamp_format` without
    /// changing the sample must fail here rather than silently start truncating
    /// again.
    #[test]
    fn test_date_column_sample_matches_the_formatter() {
        let shape = |text: &str| {
            text.chars()
                .map(|c| {
                    if c.is_ascii_digit() {
                        'd'
                    } else if c.is_ascii_alphabetic() {
                        'a'
                    } else {
                        c
                    }
                })
                .collect::<String>()
        };

        // A leap day, a single-digit day/hour/minute, and the epoch: every case
        // where a non-padded field would shorten the string.
        for timestamp in [0, 1_709_164_800, 1_800_000_000, 2_147_483_647] {
            let formatted = format_timestamp(timestamp);
            assert_eq!(
                shape(&formatted),
                shape(DATE_COLUMN_SAMPLE),
                "{formatted:?} does not have the shape of DATE_COLUMN_SAMPLE {DATE_COLUMN_SAMPLE:?}"
            );
        }
    }

    #[test]
    fn test_default_column_fractions_size_date_and_author_to_their_content() {
        let date = px(130.);
        let author = px(140.);

        // The Solution band's compact half of a 1920 window: the old flat 0.13
        // gave Date 125px for 130px of text, so every row truncated.
        let [description, date_fraction, author_fraction] =
            default_column_fractions(date, author, px(960.));
        assert!(px(960.) * date_fraction >= date);
        assert!(px(960.) * author_fraction >= author);
        assert!((description + date_fraction + author_fraction - 1.0).abs() < 0.001);

        // A full-window pane item: the same two columns must not keep growing
        // into whitespace, so Description takes everything they don't need.
        let [wide_description, wide_date, wide_author] =
            default_column_fractions(date, author, px(1920.));
        assert!((px(1920.) * wide_date - date).abs() < px(1.));
        assert!((px(1920.) * wide_author - author).abs() < px(1.));
        assert!(wide_description > description);

        // Too narrow for all three: Date and Author are scaled back together
        // rather than starving the column being read.
        let [narrow_description, narrow_date, narrow_author] =
            default_column_fractions(date, author, px(300.));
        // Two assertions, because either alone is weak. The first pins the
        // clamp to land *exactly* on the floor rather than merely near it, and
        // so is what fails if the clamp is dropped. But it compares against the
        // constant under test, so lowering that constant satisfies it just as
        // happily -- hence the second, where the floor is spelled out as a
        // literal on purpose. Both carry the same slack: the scale-back leaves
        // the sum a few f32 ulps under an exact 0.4.
        assert!((narrow_description - MIN_DESCRIPTION_FRACTION).abs() < 0.001);
        assert!(narrow_description >= 0.4 - 0.001);
        assert!(narrow_date > narrow_author * 0.9 && narrow_date < narrow_author);

        // Unmeasured first frame.
        assert_eq!(
            default_column_fractions(date, author, px(0.)),
            UNMEASURED_COLUMN_FRACTIONS
        );
    }

    use collections::{HashMap, HashSet};
    use fs::FakeFs;
    use git::Oid;
    use git::repository::{InitialGraphCommitData, RepoPath};
    use gpui::{TestAppContext, UpdateGlobal};
    use project::Project;
    use project::git_store::{GitStoreEvent, RepositoryEvent};
    use rand::prelude::*;
    use serde_json::json;
    use settings::{SettingsStore, ThemeSettingsContent};
    use smallvec::smallvec;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            git_ui::init(cx);
            project_panel::init(cx);
            init(cx);
        });
    }

    fn build_oid_to_row_map(graph: &GraphData) -> HashMap<Oid, usize> {
        graph
            .commits
            .iter()
            .enumerate()
            .map(|(idx, entry)| (entry.data.sha, idx))
            .collect()
    }

    fn verify_commit_order(
        graph: &GraphData,
        commits: &[Arc<InitialGraphCommitData>],
    ) -> Result<()> {
        if graph.commits.len() != commits.len() {
            bail!(
                "Commit count mismatch: graph has {} commits, expected {}",
                graph.commits.len(),
                commits.len()
            );
        }

        for (idx, (graph_commit, expected_commit)) in
            graph.commits.iter().zip(commits.iter()).enumerate()
        {
            if graph_commit.data.sha != expected_commit.sha {
                bail!(
                    "Commit order mismatch at index {}: graph has {:?}, expected {:?}",
                    idx,
                    graph_commit.data.sha,
                    expected_commit.sha
                );
            }
        }

        Ok(())
    }

    fn verify_line_endpoints(graph: &GraphData, oid_to_row: &HashMap<Oid, usize>) -> Result<()> {
        for line in &graph.lines {
            let child_row = *oid_to_row
                .get(&line.child)
                .context("Line references non-existent child commit")?;

            let parent_row = *oid_to_row
                .get(&line.parent)
                .context("Line references non-existent parent commit")?;

            if child_row >= parent_row {
                bail!(
                    "child_row ({}) must be < parent_row ({})",
                    child_row,
                    parent_row
                );
            }

            if line.full_interval.start != child_row {
                bail!(
                    "full_interval.start ({}) != child_row ({})",
                    line.full_interval.start,
                    child_row
                );
            }

            if line.full_interval.end != parent_row {
                bail!(
                    "full_interval.end ({}) != parent_row ({})",
                    line.full_interval.end,
                    parent_row
                );
            }

            if let Some(last_segment) = line.segments.last() {
                let segment_end_row = match last_segment {
                    CommitLineSegment::Straight { to_row } => *to_row,
                    CommitLineSegment::Curve { on_row, .. } => *on_row,
                };

                if segment_end_row != line.full_interval.end {
                    bail!(
                        "last segment ends at row {} but full_interval.end is {}",
                        segment_end_row,
                        line.full_interval.end
                    );
                }
            }
        }

        Ok(())
    }

    fn verify_column_correctness(
        graph: &GraphData,
        oid_to_row: &HashMap<Oid, usize>,
    ) -> Result<()> {
        for line in &graph.lines {
            let child_row = *oid_to_row
                .get(&line.child)
                .context("Line references non-existent child commit")?;

            let parent_row = *oid_to_row
                .get(&line.parent)
                .context("Line references non-existent parent commit")?;

            let child_lane = graph.commits[child_row].lane;
            if line.child_column != child_lane {
                bail!(
                    "child_column ({}) != child's lane ({})",
                    line.child_column,
                    child_lane
                );
            }

            let mut current_column = line.child_column;
            for segment in &line.segments {
                if let CommitLineSegment::Curve { to_column, .. } = segment {
                    current_column = *to_column;
                }
            }

            let parent_lane = graph.commits[parent_row].lane;
            if current_column != parent_lane {
                bail!(
                    "ending column ({}) != parent's lane ({})",
                    current_column,
                    parent_lane
                );
            }
        }

        Ok(())
    }

    fn verify_segment_continuity(graph: &GraphData) -> Result<()> {
        for line in &graph.lines {
            if line.segments.is_empty() {
                bail!("Line has no segments");
            }

            let mut current_row = line.full_interval.start;

            for (idx, segment) in line.segments.iter().enumerate() {
                let segment_end_row = match segment {
                    CommitLineSegment::Straight { to_row } => *to_row,
                    CommitLineSegment::Curve { on_row, .. } => *on_row,
                };

                if segment_end_row < current_row {
                    bail!(
                        "segment {} ends at row {} which is before current row {}",
                        idx,
                        segment_end_row,
                        current_row
                    );
                }

                current_row = segment_end_row;
            }
        }

        Ok(())
    }

    fn verify_line_overlaps(graph: &GraphData) -> Result<()> {
        for line in &graph.lines {
            let child_row = line.full_interval.start;

            let mut current_column = line.child_column;
            let mut current_row = child_row;

            for segment in &line.segments {
                match segment {
                    CommitLineSegment::Straight { to_row } => {
                        for row in (current_row + 1)..*to_row {
                            if row < graph.commits.len() {
                                let commit_at_row = &graph.commits[row];
                                if commit_at_row.lane == current_column {
                                    bail!(
                                        "straight segment from row {} to {} in column {} passes through commit {:?} at row {}",
                                        current_row,
                                        to_row,
                                        current_column,
                                        commit_at_row.data.sha,
                                        row
                                    );
                                }
                            }
                        }
                        current_row = *to_row;
                    }
                    CommitLineSegment::Curve {
                        to_column, on_row, ..
                    } => {
                        current_column = *to_column;
                        current_row = *on_row;
                    }
                }
            }
        }

        Ok(())
    }

    fn verify_coverage(graph: &GraphData) -> Result<()> {
        let mut expected_edges: HashSet<(Oid, Oid)> = HashSet::default();
        for entry in &graph.commits {
            for parent in &entry.data.parents {
                expected_edges.insert((entry.data.sha, *parent));
            }
        }

        let mut found_edges: HashSet<(Oid, Oid)> = HashSet::default();
        for line in &graph.lines {
            let edge = (line.child, line.parent);

            if !found_edges.insert(edge) {
                bail!(
                    "Duplicate line found for edge {:?} -> {:?}",
                    line.child,
                    line.parent
                );
            }

            if !expected_edges.contains(&edge) {
                bail!(
                    "Orphan line found: {:?} -> {:?} is not in the commit graph",
                    line.child,
                    line.parent
                );
            }
        }

        for (child, parent) in &expected_edges {
            if !found_edges.contains(&(*child, *parent)) {
                bail!("Missing line for edge {:?} -> {:?}", child, parent);
            }
        }

        assert_eq!(
            expected_edges.symmetric_difference(&found_edges).count(),
            0,
            "The symmetric difference should be zero"
        );

        Ok(())
    }

    fn verify_merge_line_optimality(
        graph: &GraphData,
        oid_to_row: &HashMap<Oid, usize>,
    ) -> Result<()> {
        for line in &graph.lines {
            let first_segment = line.segments.first();
            let is_merge_line = matches!(
                first_segment,
                Some(CommitLineSegment::Curve {
                    curve_kind: CurveKind::Merge,
                    ..
                })
            );

            if !is_merge_line {
                continue;
            }

            let child_row = *oid_to_row
                .get(&line.child)
                .context("Line references non-existent child commit")?;

            let parent_row = *oid_to_row
                .get(&line.parent)
                .context("Line references non-existent parent commit")?;

            let parent_lane = graph.commits[parent_row].lane;

            let Some(CommitLineSegment::Curve { to_column, .. }) = first_segment else {
                continue;
            };

            let curves_directly_to_parent = *to_column == parent_lane;

            if !curves_directly_to_parent {
                continue;
            }

            let curve_row = child_row + 1;
            let has_commits_in_path = graph.commits[curve_row..parent_row]
                .iter()
                .any(|c| c.lane == parent_lane);

            if has_commits_in_path {
                bail!(
                    "Merge line from {:?} to {:?} curves directly to parent lane {} but there are commits in that lane between rows {} and {}",
                    line.child,
                    line.parent,
                    parent_lane,
                    curve_row,
                    parent_row
                );
            }

            let curve_ends_at_parent = curve_row == parent_row;

            if curve_ends_at_parent {
                if line.segments.len() != 1 {
                    bail!(
                        "Merge line from {:?} to {:?} curves directly to parent (curve_row == parent_row), but has {} segments instead of 1 [MergeCurve]",
                        line.child,
                        line.parent,
                        line.segments.len()
                    );
                }
            } else {
                if line.segments.len() != 2 {
                    bail!(
                        "Merge line from {:?} to {:?} curves directly to parent lane without overlap, but has {} segments instead of 2 [MergeCurve, Straight]",
                        line.child,
                        line.parent,
                        line.segments.len()
                    );
                }

                let is_straight_segment = matches!(
                    line.segments.get(1),
                    Some(CommitLineSegment::Straight { .. })
                );

                if !is_straight_segment {
                    bail!(
                        "Merge line from {:?} to {:?} curves directly to parent lane without overlap, but second segment is not a Straight segment",
                        line.child,
                        line.parent
                    );
                }
            }
        }

        Ok(())
    }

    fn verify_all_invariants(
        graph: &GraphData,
        commits: &[Arc<InitialGraphCommitData>],
    ) -> Result<()> {
        let oid_to_row = build_oid_to_row_map(graph);

        verify_commit_order(graph, commits).context("commit order")?;
        verify_line_endpoints(graph, &oid_to_row).context("line endpoints")?;
        verify_column_correctness(graph, &oid_to_row).context("column correctness")?;
        verify_segment_continuity(graph).context("segment continuity")?;
        verify_merge_line_optimality(graph, &oid_to_row).context("merge line optimality")?;
        verify_coverage(graph).context("coverage")?;
        verify_line_overlaps(graph).context("line overlaps")?;
        Ok(())
    }

    #[test]
    fn test_git_graph_merge_commits() {
        let mut rng = StdRng::seed_from_u64(42);

        let oid1 = Oid::random(&mut rng);
        let oid2 = Oid::random(&mut rng);
        let oid3 = Oid::random(&mut rng);
        let oid4 = Oid::random(&mut rng);

        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: oid1,
                parents: smallvec![oid2, oid3],
                ref_names: vec!["HEAD".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid2,
                parents: smallvec![oid4],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid3,
                parents: smallvec![oid4],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid4,
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];

        let mut graph_data = GraphData::new(8);
        graph_data.add_commits(&commits);

        if let Err(error) = verify_all_invariants(&graph_data, &commits) {
            panic!("Graph invariant violation for merge commits:\n{}", error);
        }
    }

    #[test]
    fn test_graph_per_row_occupancy() {
        let mut rng = StdRng::seed_from_u64(7);

        // A linear head, a branch that forks and re-merges, then a linear tail:
        //   row 0  oid1          (1 lane)
        //   row 1  oid2  fork    (2 lanes)
        //   row 2  oid3          (2 lanes)
        //   row 3  oid4          (2 lanes)
        //   row 4  oid5  merge   (2 lanes — the second lane ends *on* this row)
        //   row 5  oid6          (1 lane)
        let oid1 = Oid::random(&mut rng);
        let oid2 = Oid::random(&mut rng);
        let oid3 = Oid::random(&mut rng);
        let oid4 = Oid::random(&mut rng);
        let oid5 = Oid::random(&mut rng);
        let oid6 = Oid::random(&mut rng);

        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: oid1,
                parents: smallvec![oid2],
                ref_names: vec!["HEAD".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid2,
                parents: smallvec![oid3, oid4],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid3,
                parents: smallvec![oid5],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid4,
                parents: smallvec![oid5],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid5,
                parents: smallvec![oid6],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid6,
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];

        let mut graph_data = GraphData::new(8);
        graph_data.add_commits(&commits);

        if let Err(error) = verify_all_invariants(&graph_data, &commits) {
            panic!(
                "Graph invariant violation for fork/merge commits:\n{}",
                error
            );
        }

        assert_eq!(graph_data.max_lanes, 2);
        assert_eq!(graph_data.max_column_at_row, vec![1, 2, 2, 2, 2, 1]);

        // This history never exceeds the `MIN_GRAPH_LANES` floor, so every row
        // indents its subject by the same reserved four lanes: the text neither
        // hugs the left edge on the linear rows nor jitters sideways as the fork
        // opens and closes.
        for row in 0..commits.len() {
            assert_eq!(
                graph_row_extent(graph_data.columns_at_row(row)),
                graph_row_extent(MIN_GRAPH_LANES),
                "row {row} should indent by the reserved minimum"
            );
        }

        // A row index past the loaded commits still indents past a commit dot.
        assert_eq!(graph_data.columns_at_row(999), 1);

        // Loading the same batch again on a cleared graph must not leave the
        // per-row vector stale or double-length.
        graph_data.clear();
        assert!(graph_data.max_column_at_row.is_empty());
        graph_data.add_commits(&commits);
        assert_eq!(graph_data.max_column_at_row.len(), commits.len());
    }

    #[test]
    fn test_git_graph_linear_commits() {
        let mut rng = StdRng::seed_from_u64(42);

        let oid1 = Oid::random(&mut rng);
        let oid2 = Oid::random(&mut rng);
        let oid3 = Oid::random(&mut rng);

        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: oid1,
                parents: smallvec![oid2],
                ref_names: vec!["HEAD".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid2,
                parents: smallvec![oid3],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid3,
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];

        let mut graph_data = GraphData::new(8);
        graph_data.add_commits(&commits);

        if let Err(error) = verify_all_invariants(&graph_data, &commits) {
            panic!("Graph invariant violation for linear commits:\n{}", error);
        }
    }

    #[test]
    fn test_git_graph_random_commits() {
        for seed in 0..100 {
            let mut rng = StdRng::seed_from_u64(seed);

            let adversarial = rng.random_bool(0.2);
            let num_commits = if adversarial {
                rng.random_range(10..100)
            } else {
                rng.random_range(5..50)
            };

            let commits = generate_random_commit_dag(&mut rng, num_commits, adversarial);

            assert_eq!(
                num_commits,
                commits.len(),
                "seed={}: Generate random commit dag didn't generate the correct amount of commits",
                seed
            );

            let mut graph_data = GraphData::new(8);
            graph_data.add_commits(&commits);

            if let Err(error) = verify_all_invariants(&graph_data, &commits) {
                panic!(
                    "Graph invariant violation (seed={}, adversarial={}, num_commits={}):\n{:#}",
                    seed, adversarial, num_commits, error
                );
            }
        }
    }

    // The full integration test has less iterations because it's significantly slower
    // than the random commit test
    #[gpui::test(iterations = 10)]
    async fn test_git_graph_random_integration(mut rng: StdRng, cx: &mut TestAppContext) {
        init_test(cx);

        let adversarial = rng.random_bool(0.2);
        let num_commits = if adversarial {
            rng.random_range(10..100)
        } else {
            rng.random_range(5..50)
        };

        let commits = generate_random_commit_dag(&mut rng, num_commits, adversarial);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        repository.update(cx, |repo, cx| {
            repo.graph_data(
                crate::LogSource::default(),
                crate::LogOrder::default(),
                Vec::new(),
                Vec::new(),
                0..usize::MAX,
                cx,
            );
        });
        cx.run_until_parked();

        let graph_commits: Vec<Arc<InitialGraphCommitData>> = repository.update(cx, |repo, cx| {
            repo.graph_data(
                crate::LogSource::default(),
                crate::LogOrder::default(),
                Vec::new(),
                Vec::new(),
                0..usize::MAX,
                cx,
            )
            .commits
            .to_vec()
        });

        let mut graph_data = GraphData::new(8);
        graph_data.add_commits(&graph_commits);

        if let Err(error) = verify_all_invariants(&graph_data, &commits) {
            panic!(
                "Graph invariant violation (adversarial={}, num_commits={}):\n{:#}",
                adversarial, num_commits, error
            );
        }
    }

    #[gpui::test]
    async fn test_initial_graph_data_not_cleared_on_initial_loading(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        let observed_repository_events = Arc::new(Mutex::new(Vec::new()));
        project.update(cx, |project, cx| {
            let observed_repository_events = observed_repository_events.clone();
            cx.subscribe(project.git_store(), move |_, _, event, _| {
                if let GitStoreEvent::RepositoryUpdated(_, repository_event, true) = event {
                    observed_repository_events
                        .lock()
                        .expect("repository event mutex should be available")
                        .push(repository_event.clone());
                }
            })
            .detach();
        });

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        repository.update(cx, |repo, cx| {
            repo.graph_data(
                crate::LogSource::default(),
                crate::LogOrder::default(),
                Vec::new(),
                Vec::new(),
                0..usize::MAX,
                cx,
            );
        });

        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let observed_repository_events = observed_repository_events
            .lock()
            .expect("repository event mutex should be available");
        assert!(
            observed_repository_events
                .iter()
                .any(|event| matches!(event, RepositoryEvent::HeadChanged)),
            "initial repository scan should emit HeadChanged"
        );
        let commit_count_after = repository.read_with(cx, |repo, _| {
            repo.get_graph_data(
                crate::LogSource::default(),
                crate::LogOrder::default(),
                &[],
                &[],
            )
            .map(|data| data.commit_data.len())
            .unwrap()
        });
        assert_eq!(
            commits.len(),
            commit_count_after,
            "initial_graph_data should remain populated after events emitted by initial repository scan"
        );
    }

    /// The counterpart to the test above: the initial-load scan must NOT drop
    /// the cached log, but an explicit post-push rescan must. The push dialog
    /// shells out to `git push`, so `refresh_branches` is the only thing that
    /// tells the graph its `origin/…` decorations moved — and it has to do so
    /// even here, where `scan_id` is still at its initial value and the generic
    /// `HeadChanged` cache-clear is therefore guarded off.
    #[gpui::test]
    async fn test_refresh_branches_clears_cached_graph_data(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        repository.update(cx, |repo, cx| {
            repo.graph_data(
                crate::LogSource::default(),
                crate::LogOrder::default(),
                Vec::new(),
                Vec::new(),
                0..usize::MAX,
                cx,
            );
        });
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let cached = repository.read_with(cx, |repo, _| {
            repo.get_graph_data(
                crate::LogSource::default(),
                crate::LogOrder::default(),
                &[],
                &[],
            )
            .is_some()
        });
        assert!(cached, "graph data should be cached before the rescan");

        let rescan = repository.update(cx, |repo, cx| repo.refresh_branches(cx));
        cx.run_until_parked();
        rescan
            .await
            .expect("rescan job should report back")
            .expect("rescan should succeed");

        let cached = repository.read_with(cx, |repo, _| {
            repo.get_graph_data(
                crate::LogSource::default(),
                crate::LogOrder::default(),
                &[],
                &[],
            )
            .is_some()
        });
        assert!(
            !cached,
            "refresh_branches should drop the cached log so the graph re-reads its ref decorations"
        );
    }

    #[gpui::test]
    async fn test_initial_graph_data_propagates_error(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        fs.set_graph_error(
            Path::new("/project/.git"),
            Some("fatal: bad default revision 'HEAD'".to_string()),
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        repository.update(cx, |repo, cx| {
            repo.graph_data(
                crate::LogSource::default(),
                crate::LogOrder::default(),
                Vec::new(),
                Vec::new(),
                0..usize::MAX,
                cx,
            );
        });

        cx.run_until_parked();

        let error = repository.read_with(cx, |repo, _| {
            repo.get_graph_data(
                crate::LogSource::default(),
                crate::LogOrder::default(),
                &[],
                &[],
            )
            .and_then(|data| data.error.clone())
        });

        assert!(
            error.is_some(),
            "graph data should contain an error after initial_graph_data fails"
        );
        let error_message = error.unwrap();
        assert!(
            error_message.contains("bad default revision"),
            "error should contain the git error message, got: {}",
            error_message
        );
    }

    #[gpui::test]
    async fn test_graph_data_repopulated_from_cache_after_repo_switch(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project_a"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        fs.insert_tree(
            Path::new("/project_b"),
            json!({
                ".git": {},
                "other.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project_a/.git"), commits.clone());

        let project = Project::test(
            fs.clone(),
            [Path::new("/project_a"), Path::new("/project_b")],
            cx,
        )
        .await;
        cx.run_until_parked();

        let (first_repository, second_repository) = project.read_with(cx, |project, cx| {
            let mut first_repository = None;
            let mut second_repository = None;

            for repository in project.repositories(cx).values() {
                let work_directory_abs_path = &repository.read(cx).work_directory_abs_path;
                if work_directory_abs_path.as_ref() == Path::new("/project_a") {
                    first_repository = Some(repository.clone());
                } else if work_directory_abs_path.as_ref() == Path::new("/project_b") {
                    second_repository = Some(repository.clone());
                }
            }

            (
                first_repository.expect("should have repository for /project_a"),
                second_repository.expect("should have repository for /project_b"),
            )
        });
        first_repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
        cx.run_until_parked();

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                first_repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Verify initial graph data is loaded
        let initial_commit_count =
            git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert!(
            initial_commit_count > 0,
            "graph data should have been loaded, got 0 commits"
        );

        git_graph.update_in(cx, |graph, window, cx| {
            graph.set_repo_id(second_repository.read(cx).id, window, cx)
        });
        cx.run_until_parked();

        let commit_count_after_clear =
            git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert_eq!(
            commit_count_after_clear, 0,
            "graph_data should be cleared after switching away"
        );

        git_graph.update_in(cx, |graph, window, cx| {
            graph.set_repo_id(first_repository.read(cx).id, window, cx)
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        // Verify graph data is reloaded from repository cache on switch back
        let reloaded_commit_count =
            git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert_eq!(
            reloaded_commit_count,
            commits.len(),
            "graph data should be reloaded after switching back"
        );
    }

    /// Regression: a paint that lands while the initial fetch is still in
    /// flight caches `max_commit_count = Loaded(0)` (via `add_commits(&[])`).
    /// That state must NOT be terminal — once the fetch resolves the graph
    /// must show the commits instead of sticking on "Loading" forever
    /// (reported on a freshly-created Solution: the git graph opened right
    /// after add_member showed an eternal loader until a search query nudged
    /// an invalidate).
    #[gpui::test]
    async fn test_graph_not_stuck_loading_after_empty_first_paint(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        let mut rng = StdRng::seed_from_u64(7);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });

        // Kick off the fetch and let it fully resolve into the repository's
        // cache.
        git_graph.update(cx, |graph, cx| {
            graph.resolve_commit_count(cx);
        });
        cx.run_until_parked();

        // Force the production race's end state: the repository has all the
        // commits, but the graph's LOCAL cache is an empty `Loaded(0)` (an
        // empty paint landed and the delivery events were missed). The next
        // resolve must fall through to re-read the repository instead of
        // treating `Loaded(0)` as terminal.
        git_graph.update(cx, |graph, cx| {
            graph.graph_data.clear();
            graph.graph_data.add_commits(&[]);
            assert!(matches!(
                graph.graph_data.max_commit_count,
                AllCommitCount::Loaded(0)
            ));

            let (count, is_loading) = graph.resolve_commit_count(cx);
            assert_eq!(
                count,
                commits.len(),
                "graph must pick up the fetched commits after an empty first paint"
            );
            assert!(count > 0);
            let _ = is_loading;
        });
    }

    /// Regression: a repository with NO commits must render "No commits
    /// found", not an eternal "Loading" — `Loaded(0)` used to hard-code
    /// `is_loading = true` without ever re-reading the repository.
    #[gpui::test]
    async fn test_commitless_repo_reports_not_loading(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        // No set_graph_commits: the repo has zero commits.

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });

        // Kick off the fetch (first paint) and let it resolve.
        git_graph.update(cx, |graph, cx| {
            graph.resolve_commit_count(cx);
        });
        cx.run_until_parked();

        git_graph.update(cx, |graph, cx| {
            let (count, is_loading) = graph.resolve_commit_count(cx);
            assert_eq!(count, 0);
            assert!(
                !is_loading,
                "a commitless repo must settle on 'No commits found', not loading forever"
            );
        });
    }

    /// FileHistory dispatched while a project-panel selection in a NON-git
    /// worktree is focused must not open a graph (no fall-back source). Lives
    /// here (not in `project_panel`) because it exercises git_graph's
    /// FileHistory handler; keeping it in project_panel's tests forced a
    /// project_panel <-> git_graph dev-dependency cycle that linked two copies
    /// of project_panel into the test binary and double-registered every
    /// project_panel action.
    #[gpui::test]
    async fn test_file_history_action_does_not_open_graph_for_non_git_selection(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/git-project"),
            json!({ ".git": {}, "tracked.txt": "tracked" }),
        )
        .await;
        fs.insert_tree(Path::new("/plain-project"), json!({ "plain.txt": "plain" }))
            .await;
        fs.set_graph_commits(
            Path::new("/git-project/.git"),
            vec![Arc::new(InitialGraphCommitData {
                sha: Oid::from_bytes(&[1; 20]).unwrap(),
                parents: smallvec![],
                ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
            })],
        );

        let project = Project::test(
            fs.clone(),
            [Path::new("/git-project"), Path::new("/plain-project")],
            cx,
        )
        .await;
        cx.run_until_parked();

        let plain_worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktree_for_root_name("plain-project", cx)
                .expect("plain worktree should exist")
                .read(cx)
                .id()
        });
        let plain_project_path = project::ProjectPath {
            worktree_id: plain_worktree_id,
            path: util::rel_path::rel_path("plain.txt").into(),
        };

        let workspace_window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = workspace_window
            .read_with(cx, |multi, _| multi.workspace().clone())
            .expect("workspace should exist");

        let (weak_workspace, async_window_cx) = workspace_window
            .update(cx, |multi, window, cx| {
                (multi.workspace().downgrade(), window.to_async(cx))
            })
            .expect("window should be available");
        cx.background_executor.allow_parking();
        let project_panel = cx
            .foreground_executor()
            .clone()
            .block_test(ProjectPanel::load(weak_workspace, async_window_cx))
            .expect("project panel should load");
        cx.background_executor.forbid_parking();

        workspace_window
            .update(cx, |multi, window, cx| {
                multi.workspace().update(cx, |workspace, cx| {
                    workspace.add_panel(project_panel.clone(), window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace_window
            .update(cx, |multi, window, cx| {
                project_panel.update(cx, |panel, cx| {
                    panel.select_path_for_test(plain_project_path.clone(), cx)
                });
                multi.workspace().update(cx, |workspace, cx| {
                    workspace.focus_panel::<ProjectPanel>(window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace_window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(git::FileHistory), cx);
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<GitGraph>(cx).count(), 0);
        });
    }

    #[gpui::test]
    async fn test_file_history_action_uses_focused_source_and_reuses_matching_graph(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "tracked1.txt": "tracked 1",
                "tracked2.txt": "tracked 2",
            }),
        )
        .await;

        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: Oid::from_bytes(&[1; 20]).unwrap(),
            parents: smallvec![],
            ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have active repository")
        });
        let tracked1_repo_path = RepoPath::new(&"tracked1.txt").unwrap();
        let tracked2_repo_path = RepoPath::new(&"tracked2.txt").unwrap();
        let tracked1 = repository
            .read_with(cx, |repository, cx| {
                repository.repo_path_to_project_path(&tracked1_repo_path, cx)
            })
            .expect("tracked1 should resolve to project path");
        let tracked2 = repository
            .read_with(cx, |repository, cx| {
                repository.repo_path_to_project_path(&tracked2_repo_path, cx)
            })
            .expect("tracked2 should resolve to project path");

        let workspace_window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = workspace_window
            .read_with(cx, |multi, _| multi.workspace().clone())
            .expect("workspace should exist");

        let (weak_workspace, async_window_cx) = workspace_window
            .update(cx, |multi, window, cx| {
                (multi.workspace().downgrade(), window.to_async(cx))
            })
            .expect("window should be available");
        cx.background_executor.allow_parking();
        let project_panel = cx
            .foreground_executor()
            .clone()
            .block_test(ProjectPanel::load(
                weak_workspace.clone(),
                async_window_cx.clone(),
            ))
            .expect("project panel should load");
        let git_panel = cx
            .foreground_executor()
            .clone()
            .block_test(git_ui::git_panel::GitPanel::load(
                weak_workspace,
                async_window_cx,
            ))
            .expect("git panel should load");
        cx.background_executor.forbid_parking();

        workspace_window
            .update(cx, |multi, window, cx| {
                let workspace = multi.workspace();
                workspace.update(cx, |workspace, cx| {
                    workspace.add_panel(project_panel.clone(), window, cx);
                    workspace.add_panel(git_panel.clone(), window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace_window
            .update(cx, |multi, window, cx| {
                let workspace = multi.workspace();
                project_panel.update(cx, |panel, cx| {
                    panel.select_path_for_test(tracked1.clone(), cx)
                });
                workspace.update(cx, |workspace, cx| {
                    workspace.focus_panel::<ProjectPanel>(window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();
        workspace_window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(git::FileHistory), cx);
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 1);
            assert_eq!(
                graphs[0].read(cx).log_source,
                LogSource::Path(tracked1_repo_path.clone())
            );
        });

        workspace_window
            .update(cx, |multi, window, cx| {
                let workspace = multi.workspace();
                git_panel.update(cx, |panel, cx| {
                    panel.select_entry_by_path(tracked1.clone(), window, cx);
                });
                workspace.update(cx, |workspace, cx| {
                    workspace.focus_panel::<git_ui::git_panel::GitPanel>(window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();
        workspace_window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(git::FileHistory), cx);
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 1);
            assert_eq!(
                graphs[0].read(cx).log_source,
                LogSource::Path(tracked1_repo_path.clone())
            );
        });

        let tracked1_buffer = project
            .update(cx, |project, cx| project.open_buffer(tracked1.clone(), cx))
            .await
            .expect("tracked1 buffer should open");
        let tracked2_buffer = project
            .update(cx, |project, cx| project.open_buffer(tracked2.clone(), cx))
            .await
            .expect("tracked2 buffer should open");
        workspace_window
            .update(cx, |multi, window, cx| {
                let workspace = multi.workspace();
                let multibuffer = cx.new(|cx| {
                    let mut multibuffer = editor::MultiBuffer::new(language::Capability::ReadWrite);
                    multibuffer.set_excerpts_for_buffer(
                        tracked1_buffer.clone(),
                        [Default::default()..tracked1_buffer.read(cx).max_point()],
                        0,
                        cx,
                    );
                    multibuffer.set_excerpts_for_buffer(
                        tracked2_buffer.clone(),
                        [Default::default()..tracked2_buffer.read(cx).max_point()],
                        0,
                        cx,
                    );
                    multibuffer
                });
                let editor = cx.new(|cx| {
                    Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx)
                });
                workspace.update(cx, |workspace, cx| {
                    workspace.add_item_to_active_pane(
                        Box::new(editor.clone()),
                        None,
                        true,
                        window,
                        cx,
                    );
                });
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let second_excerpt_point = snapshot
                        .range_for_buffer(tracked2_buffer.read(cx).remote_id())
                        .expect("tracked2 excerpt should exist")
                        .start;
                    let anchor = snapshot.anchor_before(second_excerpt_point);
                    editor.change_selections(
                        editor::SelectionEffects::no_scroll(),
                        window,
                        cx,
                        |selections| {
                            selections.select_anchor_ranges([anchor..anchor]);
                        },
                    );
                    window.focus(&editor.focus_handle(cx), cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace_window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(git::FileHistory), cx);
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 2);
            let latest = graphs
                .into_iter()
                .max_by_key(|graph| graph.entity_id())
                .expect("expected a git graph");
            assert_eq!(
                latest.read(cx).log_source,
                LogSource::Path(tracked2_repo_path)
            );
        });
    }

    #[gpui::test]
    fn test_serialized_state_roundtrip(_cx: &mut TestAppContext) {
        use persistence::SerializedGitGraphState;

        let file_path = RepoPath::new(&"src/main.rs").unwrap();
        let sha = Oid::from_bytes(&[0xab; 20]).unwrap();

        let state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_FILE),
            log_source_value: Some("src/main.rs".to_string()),
            log_order: Some(persistence::LOG_ORDER_TOPO),
            selected_sha: Some(sha.to_string()),
            search_query: Some("fix bug".to_string()),
            search_case_sensitive: Some(true),
            ..Default::default()
        };

        assert_eq!(
            persistence::deserialize_log_source(&state),
            LogSource::Path(file_path)
        );
        assert!(matches!(
            persistence::deserialize_log_order(&state),
            LogOrder::TopoOrder
        ));
        assert_eq!(
            state.selected_sha.as_deref(),
            Some(sha.to_string()).as_deref()
        );
        assert_eq!(state.search_query.as_deref(), Some("fix bug"));
        assert_eq!(state.search_case_sensitive, Some(true));

        let all_state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_ALL),
            log_source_value: None,
            log_order: Some(persistence::LOG_ORDER_DATE),
            selected_sha: None,
            search_query: None,
            search_case_sensitive: None,
            ..Default::default()
        };
        assert_eq!(
            persistence::deserialize_log_source(&all_state),
            LogSource::All
        );
        assert!(matches!(
            persistence::deserialize_log_order(&all_state),
            LogOrder::DateOrder
        ));

        let branch_state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_BRANCH),
            log_source_value: Some("refs/heads/main".to_string()),
            ..Default::default()
        };
        assert_eq!(
            persistence::deserialize_log_source(&branch_state),
            LogSource::Branch("refs/heads/main".into())
        );

        let sha_state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_SHA),
            log_source_value: Some(sha.to_string()),
            ..Default::default()
        };
        assert_eq!(
            persistence::deserialize_log_source(&sha_state),
            LogSource::Sha(sha)
        );

        let empty_state = SerializedGitGraphState::default();
        assert_eq!(
            persistence::deserialize_log_source(&empty_state),
            LogSource::All
        );
        assert!(matches!(
            persistence::deserialize_log_order(&empty_state),
            LogOrder::DateOrder
        ));
    }

    #[gpui::test]
    fn test_filter_state_roundtrip(_cx: &mut TestAppContext) {
        use crate::filters::{DateRange, LogFilters};
        use crate::highlights::HighlightSet;
        use crate::view_options::ViewOptions;
        use persistence::SerializedGitGraphState;

        let last_seen = Oid::from_bytes(&[0xcd; 20]).unwrap();
        let filters = LogFilters {
            branches: vec!["main".into(), "feature/x".into()],
            authors: vec!["alice@example.com".into()],
            date_range: Some(DateRange::Between {
                since: 100,
                until: 200,
            }),
            paths: vec![
                RepoPath::new(&"src/main.rs").unwrap(),
                RepoPath::new(&"docs/readme.md").unwrap(),
            ],
            query: None,
            all_refs: true,
            sha: None,
        };
        let highlights = HighlightSet {
            my_commits: true,
            new_since_refresh: true,
            last_seen_sha: Some(last_seen),
        };
        let view = ViewOptions {
            compact_refs: true,
            group_by_date: false,
        };

        let filter_cols = persistence::serialize_log_filters(&filters);
        let hl_cols = persistence::serialize_highlights(&highlights);
        let view_cols = persistence::serialize_view_options(&view);

        let state = SerializedGitGraphState {
            filter_branches: filter_cols.branches,
            filter_authors: filter_cols.authors,
            filter_paths: filter_cols.paths,
            filter_date_since: filter_cols.date_since,
            filter_date_until: filter_cols.date_until,
            filter_all_refs: filter_cols.all_refs,
            highlight_my_commits: hl_cols.my_commits,
            highlight_new_since_refresh: hl_cols.new_since_refresh,
            highlight_last_seen_sha: hl_cols.last_seen_sha,
            view_compact_refs: view_cols.compact_refs,
            view_group_by_date: view_cols.group_by_date,
            ..Default::default()
        };

        let restored_filters = persistence::deserialize_log_filters(&state);
        assert_eq!(restored_filters, filters);

        let restored_highlights = persistence::deserialize_highlights(&state);
        assert_eq!(restored_highlights, highlights);

        let restored_view = persistence::deserialize_view_options(&state);
        assert_eq!(restored_view, view);

        let empty = SerializedGitGraphState::default();
        assert_eq!(
            persistence::deserialize_log_filters(&empty),
            LogFilters::default()
        );
        assert_eq!(
            persistence::deserialize_highlights(&empty),
            HighlightSet::default()
        );
        assert_eq!(
            persistence::deserialize_view_options(&empty),
            ViewOptions::default()
        );

        let since_only = LogFilters {
            date_range: Some(DateRange::Since(42)),
            ..LogFilters::default()
        };
        let since_cols = persistence::serialize_log_filters(&since_only);
        let since_state = SerializedGitGraphState {
            filter_date_since: since_cols.date_since,
            filter_date_until: since_cols.date_until,
            ..Default::default()
        };
        assert_eq!(
            persistence::deserialize_log_filters(&since_state).date_range,
            Some(DateRange::Since(42))
        );

        let until_only = LogFilters {
            date_range: Some(DateRange::Until(99)),
            ..LogFilters::default()
        };
        let until_cols = persistence::serialize_log_filters(&until_only);
        let until_state = SerializedGitGraphState {
            filter_date_since: until_cols.date_since,
            filter_date_until: until_cols.date_until,
            ..Default::default()
        };
        assert_eq!(
            persistence::deserialize_log_filters(&until_state).date_range,
            Some(DateRange::Until(99))
        );

        let malformed = SerializedGitGraphState {
            filter_branches: Some("not json".into()),
            filter_paths: Some("\"oops\"".into()),
            ..Default::default()
        };
        let restored_malformed = persistence::deserialize_log_filters(&malformed);
        assert!(restored_malformed.branches.is_empty());
        assert!(restored_malformed.paths.is_empty());

        let bad_sha_state = SerializedGitGraphState {
            highlight_last_seen_sha: Some("not-a-sha".into()),
            ..Default::default()
        };
        assert_eq!(
            persistence::deserialize_highlights(&bad_sha_state).last_seen_sha,
            None
        );
    }

    #[gpui::test]
    async fn test_git_graph_state_persists_across_serialization_roundtrip(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(99);
        let commits = generate_random_commit_dag(&mut rng, 20, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak.clone(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let commit_count = git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert!(commit_count > 0, "graph should have loaded commits, got 0");

        let target_sha = commits[5].sha;
        git_graph.update(cx, |graph, _| {
            graph.selected_entry_idx = Some(5);
        });

        let selected_sha = git_graph
            .read_with(&*cx, |graph, _| graph.selected_commit_sha())
            .map(|sha| sha.to_string());
        assert_eq!(selected_sha, Some(target_sha.to_string()));

        let item_id = workspace::ItemId::from(999_u64);
        let workspace_db = cx.read(|cx| workspace::WorkspaceDb::global(cx));
        let workspace_id = workspace_db
            .next_id()
            .await
            .expect("should create workspace id");
        let db = cx.read(|cx| persistence::GitGraphsDb::global(cx));
        db.save_git_graph(
            item_id,
            workspace_id,
            "/project".to_string(),
            Some(persistence::LOG_SOURCE_ALL),
            None,
            Some(persistence::LOG_ORDER_DATE),
            selected_sha.clone(),
            Some("some query".to_string()),
            Some(true),
            Some(true),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("save should succeed");

        let restored_graph = cx
            .update(|window, cx| {
                <GitGraph as workspace::SerializableItem>::deserialize(
                    project.clone(),
                    workspace_weak,
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await
            .expect("deserialization should succeed");
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| restored_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let restored_commit_count =
            restored_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert_eq!(
            restored_commit_count, commit_count,
            "restored graph should have the same number of commits"
        );

        restored_graph.read_with(&*cx, |graph, _| {
            assert_eq!(
                graph.log_source,
                LogSource::All,
                "log_source should be restored"
            );

            let restored_selected_sha = graph.selected_commit_sha().map(|sha| sha.to_string());
            assert_eq!(
                restored_selected_sha, selected_sha,
                "selected commit should be restored via pending_select_sha"
            );

            assert_eq!(
                graph.search_state.case_sensitive, true,
                "search case sensitivity should be restored"
            );
            assert_eq!(
                graph.search_state.regex, true,
                "search regex flag should be restored"
            );
            assert_eq!(
                graph.search_state.search_in_diffs, false,
                "search-in-diffs flag should default to false when persisted as NULL"
            );
            assert_eq!(
                graph.filters.query,
                Some(filters::QueryFilter {
                    text: "some query".into(),
                    regex: true,
                    case_sensitive: true,
                    search_in_diffs: false,
                }),
                "filters.query should be hydrated from persisted text + flags"
            );
        });

        restored_graph.read_with(&*cx, |graph, cx| {
            let editor_text = graph.search_state.editor.read(cx).text(cx);
            assert_eq!(
                editor_text, "some query",
                "search query text should be restored in editor"
            );
        });
    }

    #[gpui::test]
    async fn test_graph_data_reloaded_after_stash_change(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let initial_head = Oid::from_bytes(&[1; 20]).unwrap();
        let initial_stash = Oid::from_bytes(&[2; 20]).unwrap();
        let updated_head = Oid::from_bytes(&[3; 20]).unwrap();
        let updated_stash = Oid::from_bytes(&[4; 20]).unwrap();

        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: initial_head,
                    parents: smallvec![initial_stash],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: initial_stash,
                    parents: smallvec![],
                    ref_names: vec!["refs/stash".into()],
                }),
            ],
        );
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state.stash_entries = git::stash::GitStash {
                entries: vec![git::stash::StashEntry {
                    index: 0,
                    oid: initial_stash,
                    message: "initial stash".to_string(),
                    branch: Some("main".to_string()),
                    timestamp: 1,
                }]
                .into(),
            };
        })
        .unwrap();

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let initial_shas = git_graph.read_with(&*cx, |graph, _| {
            graph
                .graph_data
                .commits
                .iter()
                .map(|commit| commit.data.sha)
                .collect::<Vec<_>>()
        });
        assert_eq!(initial_shas, vec![initial_head, initial_stash]);

        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: updated_head,
                    parents: smallvec![updated_stash],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: updated_stash,
                    parents: smallvec![],
                    ref_names: vec!["refs/stash".into()],
                }),
            ],
        );
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state.stash_entries = git::stash::GitStash {
                entries: vec![git::stash::StashEntry {
                    index: 0,
                    oid: updated_stash,
                    message: "updated stash".to_string(),
                    branch: Some("main".to_string()),
                    timestamp: 1,
                }]
                .into(),
            };
        })
        .unwrap();

        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let reloaded_shas = git_graph.read_with(&*cx, |graph, _| {
            graph
                .graph_data
                .commits
                .iter()
                .map(|commit| commit.data.sha)
                .collect::<Vec<_>>()
        });
        assert_eq!(reloaded_shas, vec![updated_head, updated_stash]);
    }

    #[gpui::test]
    async fn test_row_height_matches_uniform_list_item_height(cx: &mut TestAppContext) {
        init_test(cx);

        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    *settings.theme = ThemeSettingsContent {
                        ui_font_size: Some(12.7.into()),
                        ..Default::default()
                    }
                });
            })
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(99);
        let commits = generate_random_commit_dag(&mut rng, 20, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            let commit_count = graph.graph_data.commits.len();
            assert!(
                commit_count > 0,
                "need at least one commit to measure item height"
            );

            let table_state = graph.table_interaction_state.read(cx);
            let item_size = table_state.scroll_handle.0.borrow().last_item_size.expect(
                "uniform_list should have populated last_item_size after draw(); \
                     the table has not been laid out",
            );

            let measured_item_height = item_size.contents.height / commit_count as f32;
            let computed_row_height = GitGraph::row_height(window, cx);

            assert_eq!(
                computed_row_height, measured_item_height,
                "GitGraph::row_height ({}) must exactly match the height that \
                 uniform_list measured for each table row ({}). \
                 A mismatch means the canvas and table rows will drift when scrolling.",
                computed_row_height, measured_item_height,
            );
        });
    }

    /// The property the derived columns exist for is that Date is never
    /// narrower than the date it holds. The two pure-function tests above only
    /// check `default_column_fractions`' arithmetic; neither reaches either of
    /// the inputs it is called with, so neither notices the columns being
    /// derived against a stale table width or a stale font.
    ///
    /// So this one goes through a real window: the graph is a workspace pane
    /// item and the width comes from `simulate_resize`, which is what makes the
    /// deferred re-derivation in `observe_table_width` run at all. Driving
    /// frames with `VisualTestContext::draw` instead would not do — it never
    /// makes the views live window entities, so an invalidation that the app
    /// drops mid-draw passes there (#62).
    #[gpui::test]
    async fn test_default_column_widths_follow_the_table_width_and_the_ui_font(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let mut rng = StdRng::seed_from_u64(7);
        let commits = generate_random_commit_dag(&mut rng, 12, false);
        let (_project, workspace, git_graph, cx) =
            setup_graph_with_workspace(&fs, commits, cx).await;

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        // The Date column as it was last laid out, beside the width its text
        // actually needs. A derivation that has gone stale shows up as the
        // former falling behind the latter.
        let date_column = |cx: &mut gpui::VisualTestContext| {
            git_graph.update_in(cx, |graph, window, cx| {
                let table_width = graph.table_width;
                assert!(
                    table_width > px(0.),
                    "the table was never laid out, so nothing below tests anything"
                );
                let DefiniteLength::Fraction(fraction) =
                    graph.column_widths.read(cx).initial_widths()[1]
                else {
                    panic!("column widths are installed as fractions");
                };
                let needed = GitGraph::measured_column_width(DATE_COLUMN_SAMPLE, window, cx);
                assert!(
                    needed > COLUMN_CELL_PADDING.to_pixels(window.rem_size()),
                    "the text system measured {DATE_COLUMN_SAMPLE:?} as empty, so a Date \
                     column of any width would satisfy the assertions below"
                );
                (table_width * fraction, needed, table_width, fraction)
            })
        };

        // Wide enough for both content columns outright: Date is sized to its
        // text and nothing is clamped.
        cx.simulate_resize(gpui::size(px(1600.), px(900.)));
        cx.run_until_parked();
        let (wide_date, wide_needed, wide_table, wide_fraction) = date_column(cx);
        assert!(
            wide_date >= wide_needed,
            "Date was laid out {wide_date:?} wide for {wide_needed:?} of text \
             in a {wide_table:?} table"
        );

        // The Solution band's case. Same text, much less room, so the same
        // column has to claim a bigger share than it did above — and than the
        // flat 0.13 that truncated it before this was derived at all.
        cx.simulate_resize(gpui::size(px(700.), px(900.)));
        cx.run_until_parked();
        let (_, _, narrow_table, narrow_fraction) = date_column(cx);
        assert!(
            narrow_table < wide_table,
            "the window resize never reached the table: still {narrow_table:?}"
        );
        assert!(
            narrow_fraction > wide_fraction && narrow_fraction > UNMEASURED_COLUMN_FRACTIONS[1],
            "Date kept {narrow_fraction} of a {narrow_table:?} table after holding \
             {wide_fraction} of a {wide_table:?} one, so it was not re-derived"
        );

        // Now change the font without touching the window. The table's width is
        // unchanged, so a derivation cached against the width alone stands pat
        // and the columns keep the previous font's sizing — the truncation this
        // whole derivation exists to remove, and one no resize provokes.
        cx.simulate_resize(gpui::size(px(1600.), px(900.)));
        cx.run_until_parked();
        let (_, small_font_needed, _, _) = date_column(cx);

        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    *settings.theme = ThemeSettingsContent {
                        ui_font_size: Some(24.0.into()),
                        ..Default::default()
                    }
                });
            });
        });
        cx.run_until_parked();

        let (big_font_date, big_font_needed, big_font_table, _) = date_column(cx);
        assert!(
            big_font_needed > small_font_needed,
            "the larger UI font did not widen the Date text ({small_font_needed:?} -> \
             {big_font_needed:?}), so the assertion below proves nothing"
        );
        assert!(
            big_font_date >= big_font_needed,
            "after the UI font grew, Date was still laid out {big_font_date:?} wide for \
             {big_font_needed:?} of text in a {big_font_table:?} table"
        );
    }

    #[gpui::test]
    async fn test_for_file_history_preset_uses_file_log_source(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "src": { "main.rs": "fn main() {}" },
            }),
        )
        .await;
        fs.set_graph_commits(Path::new("/project/.git"), Vec::new());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("active repository should exist")
        });
        let repo_id = repository.read_with(cx, |repo, _| repo.id);
        let git_store = project.read_with(cx, |project, _| project.git_store().clone());
        let repo_path = RepoPath::new(&"src/main.rs").unwrap();

        let workspace_window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = workspace_window
            .read_with(cx, |multi, _| multi.workspace().clone())
            .expect("workspace should exist");
        let weak_workspace = workspace.downgrade();

        let graph = workspace_window
            .update(cx, |_multi, window, cx| {
                let git_store = git_store.clone();
                let repo_path = repo_path.clone();
                cx.new(|cx| {
                    GitGraph::for_file_history(
                        repo_id,
                        repo_path,
                        git_store,
                        weak_workspace.clone(),
                        window,
                        cx,
                    )
                })
            })
            .expect("graph should construct");

        graph.read_with(cx, |graph, _| {
            assert_eq!(graph.log_source, LogSource::Path(repo_path.clone()));
            assert_eq!(graph.mode(), GraphMode::FileHistory);
            assert!(graph.file_history_options().follow_renames);
            // Default-off: view_row_count == commits.len() (no synthetic
            // row).
            assert_eq!(graph.view_row_count(), graph.graph_data.commits.len());
            assert!(graph.view_to_data_idx(0).is_some());
        });

        // Toggle "With Local Changes" on; the view widens by 1 and view-
        // index 0 is now the synthetic row (returns `None` from
        // `view_to_data_idx`).
        graph.update(cx, |graph, cx| {
            graph.set_with_local_changes(true, cx);
            assert!(graph.has_local_changes_row());
            let commits = graph.graph_data.commits.len();
            assert_eq!(graph.view_row_count(), commits + 1);
            assert_eq!(graph.view_to_data_idx(0), None);
            assert_eq!(graph.view_to_data_idx(1), Some(0));
            assert_eq!(graph.data_to_view_idx(0), 1);
        });

        // Column-count assertion: three columns Description / Date / Author —
        // the hash column was dropped everywhere (decision #56; the SHA lives
        // in the git panel's Commit tab), file-history mode included.
        graph.read_with(cx, |graph, cx| {
            let widths = graph.column_widths.read(cx);
            assert_eq!(widths.cols(), 3);
        });

        // Toggle Follow Renames off; combined_extra_args picks up
        // `--no-follow` so subsequent fetches stop walking renames.
        graph.update(cx, |graph, cx| {
            graph.set_follow_renames(false, cx);
            let args = graph.combined_extra_args();
            assert!(args.iter().any(|a| a == "--no-follow"));
        });
    }

    #[gpui::test]
    fn test_file_history_options_persistence_roundtrip(_cx: &mut TestAppContext) {
        use file_history::FileHistoryOptions;
        use persistence::SerializedGitGraphState;

        let opts = FileHistoryOptions {
            follow_renames: false,
            with_local_changes: true,
            show_inline_diff: true,
        };
        let cols = persistence::serialize_file_history_options(&opts);
        let state = SerializedGitGraphState {
            view_follow_renames: cols.follow_renames,
            view_with_local_changes: cols.with_local_changes,
            view_show_inline_diff: cols.show_inline_diff,
            ..Default::default()
        };
        assert_eq!(persistence::deserialize_file_history_options(&state), opts);

        // Default-on follow_renames hydrates from missing column.
        let empty = SerializedGitGraphState::default();
        let restored = persistence::deserialize_file_history_options(&empty);
        assert!(restored.follow_renames);
        assert!(!restored.with_local_changes);
        assert!(!restored.show_inline_diff);
    }

    /// Boilerplate shared by the selection tests: a project on a fake fs whose
    /// repository serves `commits`, plus a drawn `GitGraph`.
    async fn setup_graph_with_commits<'a>(
        fs: &Arc<FakeFs>,
        commits: Vec<Arc<InitialGraphCommitData>>,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<Project>,
        Entity<GitGraph>,
        &'a mut gpui::VisualTestContext,
    ) {
        let (project, _workspace, git_graph, cx) =
            setup_graph_with_workspace(fs, commits, cx).await;
        (project, git_graph, cx)
    }

    /// As [`setup_graph_with_commits`], but also hands back the workspace so a
    /// test can assert what the graph did (or did not) open in it.
    async fn setup_graph_with_workspace<'a>(
        fs: &Arc<FakeFs>,
        commits: Vec<Arc<InitialGraphCommitData>>,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<Project>,
        Entity<workspace::Workspace>,
        Entity<GitGraph>,
        &'a mut gpui::VisualTestContext,
    ) {
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        (project, workspace, git_graph, cx)
    }

    fn draw_graph(git_graph: &Entity<GitGraph>, cx: &mut gpui::VisualTestContext) {
        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();
    }

    /// The toolbar's refresh button has to evict the repository's memoised
    /// `git log` result, not just drop the view's copy of it: the cache is
    /// keyed by filter args and is only evicted by head/branch/tag events, so
    /// a commit that arrives without one (a fetch into a bare ref, an amend in
    /// a terminal that the watcher misses) would otherwise be re-served from
    /// the stale snapshot and the button would look like a no-op.
    #[gpui::test]
    async fn test_refresh_reloads_log_without_a_head_event(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());

        let head = Oid::from_bytes(&[1; 20]).expect("valid oid");
        let parent = Oid::from_bytes(&[2; 20]).expect("valid oid");
        let external = Oid::from_bytes(&[3; 20]).expect("valid oid");

        let head_entry = Arc::new(InitialGraphCommitData {
            sha: head,
            parents: smallvec![parent],
            ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
        });
        let parent_entry = Arc::new(InitialGraphCommitData {
            sha: parent,
            parents: smallvec![],
            ref_names: vec![],
        });

        let (_project, git_graph, cx) =
            setup_graph_with_commits(&fs, vec![head_entry.clone(), parent_entry.clone()], cx).await;
        draw_graph(&git_graph, cx);

        // A new commit lands on disk. No ref is rewritten, so nothing emits
        // `HeadChanged` and the cached log keeps its pre-commit contents.
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: external,
                    parents: smallvec![head],
                    ref_names: vec![],
                }),
                head_entry,
                parent_entry,
            ],
        );
        cx.run_until_parked();
        draw_graph(&git_graph, cx);

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(
                graph.graph_data.commits.first().map(|entry| entry.data.sha),
                Some(head),
                "without an explicit refresh the cached log should still be served"
            );
        });

        git_graph.update(cx, |graph, cx| graph.refresh(cx));
        cx.run_until_parked();
        draw_graph(&git_graph, cx);

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(
                graph.graph_data.commits.first().map(|entry| entry.data.sha),
                Some(external),
                "refresh should re-run the log and surface the new commit"
            );
            assert_eq!(graph.graph_data.commits.len(), 3);
        });
    }

    /// Refreshing re-anchors the "new since last refresh" highlight on the
    /// commit that was at the top *before* the reload, so the commits the
    /// reload pulls in are the ones decorated as new. Anchoring after the
    /// reload (or not at all) leaves the highlight pointing at a commit that
    /// is still row 0, and nothing is ever marked new.
    #[gpui::test]
    async fn test_refresh_reanchors_new_since_refresh(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());

        let head = Oid::from_bytes(&[1; 20]).expect("valid oid");
        let parent = Oid::from_bytes(&[2; 20]).expect("valid oid");
        let external = Oid::from_bytes(&[3; 20]).expect("valid oid");

        let head_entry = Arc::new(InitialGraphCommitData {
            sha: head,
            parents: smallvec![parent],
            ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
        });
        let parent_entry = Arc::new(InitialGraphCommitData {
            sha: parent,
            parents: smallvec![],
            ref_names: vec![],
        });

        let (_project, git_graph, cx) =
            setup_graph_with_commits(&fs, vec![head_entry.clone(), parent_entry.clone()], cx).await;
        draw_graph(&git_graph, cx);

        git_graph.update(cx, |graph, cx| graph.set_new_since_refresh(true, cx));
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.highlights.last_seen_sha, Some(head));
        });

        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: external,
                    parents: smallvec![head],
                    ref_names: vec![],
                }),
                head_entry,
                parent_entry,
            ],
        );
        cx.run_until_parked();

        git_graph.update(cx, |graph, cx| graph.refresh(cx));
        cx.run_until_parked();
        draw_graph(&git_graph, cx);

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(
                graph.highlights.last_seen_sha,
                Some(head),
                "the anchor should stay on the commit that headed the log before the reload"
            );
            assert_eq!(
                graph.graph_data.commits.first().map(|entry| entry.data.sha),
                Some(external),
                "the newly-arrived commit should now sit above the anchor"
            );
        });
    }

    /// Double-clicking a commit row used to open the `CommitView` tab — a
    /// synthetic pseudo-file whose first screen is the commit description.
    /// It must now do nothing beyond selecting; the same view is still one
    /// explicit `open_selected_commit_view` away, which is what the second
    /// half of this test pins down (without it, the assertion above would
    /// also pass if `CommitView::open` had simply stopped working).
    #[gpui::test]
    async fn test_double_click_selects_without_opening_the_commit_view(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let sha = Oid::from_bytes(&[9; 20]).expect("valid oid");

        let (_project, workspace, git_graph, cx) = setup_graph_with_workspace(
            &fs,
            vec![Arc::new(InitialGraphCommitData {
                sha,
                parents: smallvec![],
                ref_names: vec!["HEAD".into()],
            })],
            cx,
        )
        .await;

        let commit_views = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(&*cx, |workspace, cx| {
                workspace
                    .active_pane()
                    .read(cx)
                    .items()
                    .filter(|item| item.downcast::<CommitView>().is_some())
                    .count()
            })
        };

        git_graph.update_in(cx, |graph, window, cx| {
            graph.on_row_click(0, 2, Modifiers::none(), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            git_graph.read_with(&*cx, |graph, _| graph.selected_entry_idx),
            Some(0),
            "a double click still selects the row"
        );
        assert_eq!(
            commit_views(cx),
            0,
            "a double click must not open the commit pseudo-file"
        );

        git_graph.update_in(cx, |graph, window, cx| {
            graph.open_selected_commit_view(window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            commit_views(cx),
            1,
            "the commit view is still reachable explicitly"
        );
    }

    fn rows(indices: impl IntoIterator<Item = usize>) -> HashSet<usize> {
        HashSet::from_iter(indices)
    }

    #[test]
    fn test_fold_row_click_plain_click_replaces_the_selection() {
        let folded = fold_row_click(
            4,
            RowSelectionGesture::Replace,
            &rows([1, 2, 3]),
            Some(1),
            false,
        );
        assert_eq!(folded.active, 4);
        assert_eq!(folded.selected, rows([4]));
        assert_eq!(
            folded.anchor,
            Some(4),
            "a plain click re-anchors, so the next Shift+click ranges from it"
        );
    }

    #[test]
    fn test_fold_row_click_ctrl_toggles_rows() {
        let folded = fold_row_click(4, RowSelectionGesture::Toggle, &rows([1]), Some(1), false);
        assert_eq!(folded.selected, rows([1, 4]), "Ctrl+click adds a row");
        assert_eq!(folded.active, 4);
        assert_eq!(folded.anchor, Some(4));

        // Ctrl+clicking a selected row drops it. The Commit tab has to keep
        // describing a *selected* commit, so the active row moves to the
        // nearest survivor while the anchor still follows the click.
        let folded = fold_row_click(
            4,
            RowSelectionGesture::Toggle,
            &rows([1, 4, 9]),
            Some(4),
            false,
        );
        assert_eq!(folded.selected, rows([1, 9]));
        assert_eq!(folded.active, 1);
        assert_eq!(folded.anchor, Some(4));

        // Toggling the only selected row off would leave nothing selected.
        let folded = fold_row_click(4, RowSelectionGesture::Toggle, &rows([4]), Some(4), false);
        assert_eq!(folded.selected, rows([4]));
        assert_eq!(folded.active, 4);
    }

    #[test]
    fn test_fold_row_click_shift_selects_a_range() {
        let folded = fold_row_click(6, RowSelectionGesture::Range, &rows([2]), Some(2), false);
        assert_eq!(folded.selected, rows([2, 3, 4, 5, 6]));
        assert_eq!(folded.active, 6);
        assert_eq!(folded.anchor, Some(2), "the anchor survives a Shift+click");

        // Shrinking the range and reversing direction both range from the
        // untouched anchor.
        let folded = fold_row_click(
            3,
            RowSelectionGesture::Range,
            &folded.selected,
            Some(2),
            false,
        );
        assert_eq!(folded.selected, rows([2, 3]));
        let folded = fold_row_click(
            0,
            RowSelectionGesture::Range,
            &folded.selected,
            Some(2),
            false,
        );
        assert_eq!(folded.selected, rows([0, 1, 2]));
        assert_eq!(folded.active, 0);

        // Ctrl+click moves the anchor, so a following Shift+click ranges from
        // the Ctrl-clicked row and discards the rows picked before it.
        let ctrl = fold_row_click(7, RowSelectionGesture::Toggle, &rows([1]), Some(1), false);
        let shift = fold_row_click(
            9,
            RowSelectionGesture::Range,
            &ctrl.selected,
            ctrl.anchor,
            false,
        );
        assert_eq!(shift.selected, rows([7, 8, 9]));
        assert_eq!(shift.anchor, Some(7));

        // With no anchor at all a Shift+click is just a plain click.
        let folded = fold_row_click(5, RowSelectionGesture::Range, &rows([1, 2]), None, false);
        assert_eq!(folded.selected, rows([5]));
        assert_eq!(folded.anchor, Some(5));
    }

    /// View-index 0 is the synthetic "local changes" row when it is shown. It
    /// has no commit behind it, so it must never end up in a multi-row
    /// selection that the multi-commit menu would then act on.
    #[test]
    fn test_fold_row_click_never_multi_selects_the_local_changes_row() {
        let folded = fold_row_click(0, RowSelectionGesture::Toggle, &rows([2, 3]), Some(2), true);
        assert_eq!(
            folded.selected,
            rows([0]),
            "Ctrl+clicking the local-changes row falls back to a plain click"
        );
        assert_eq!(folded.active, 0);
        assert_eq!(folded.anchor, Some(0));

        let folded = fold_row_click(0, RowSelectionGesture::Range, &rows([2, 3]), Some(3), true);
        assert_eq!(folded.selected, rows([0]));
        assert_eq!(folded.active, 0);

        // Anchored on the local-changes row, a Shift+click can't range out of
        // it either.
        let folded = fold_row_click(3, RowSelectionGesture::Range, &rows([0]), Some(0), true);
        assert_eq!(folded.selected, rows([3]));
        assert_eq!(folded.anchor, Some(3));

        // The same clicks on a commit row still multi-select normally.
        let folded = fold_row_click(3, RowSelectionGesture::Range, &rows([1]), Some(1), true);
        assert_eq!(folded.selected, rows([1, 2, 3]));
    }

    #[test]
    fn test_is_first_parent_chain() {
        let oid = |byte: u8| Oid::from_bytes(&[byte; 20]).expect("valid oid");

        // Oldest first: c1 <- c2 <- c3.
        let contiguous = [
            (oid(1), None),
            (oid(2), Some(oid(1))),
            (oid(3), Some(oid(2))),
        ];
        assert!(is_first_parent_chain(&contiguous));

        // c3's first parent is c2, which was not picked.
        let with_a_gap = [(oid(1), None), (oid(3), Some(oid(2)))];
        assert!(!is_first_parent_chain(&with_a_gap));

        // A merge whose *second* parent is the selected commit is not a
        // first-parent chain either.
        let second_parent_only = [(oid(1), None), (oid(4), Some(oid(9)))];
        assert!(!is_first_parent_chain(&second_parent_only));

        assert!(is_first_parent_chain(&[(oid(1), Some(oid(2)))]));
        assert!(is_first_parent_chain(&[]));
    }

    /// As [`setup_graph_with_workspace`], but the workspace also carries a git
    /// panel, so the graph's selection push has somewhere to land. The project
    /// comes back too, for tests that let commits land from outside the editor
    /// and wait on `git_scans_complete`.
    async fn setup_graph_with_git_panel(
        fs: &Arc<FakeFs>,
        commits: Vec<Arc<InitialGraphCommitData>>,
        cx: &mut TestAppContext,
    ) -> (
        Entity<Project>,
        Entity<GitGraph>,
        Entity<GitPanel>,
        gpui::VisualTestContext,
    ) {
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let window_handle = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = window_handle
            .read_with(cx, |multi, _| multi.workspace().clone())
            .expect("workspace should exist");

        let (weak_workspace, async_window_cx) = window_handle
            .update(cx, |_, window, cx| {
                (workspace.downgrade(), window.to_async(cx))
            })
            .expect("window should be available");
        cx.background_executor.allow_parking();
        let git_panel = cx
            .foreground_executor()
            .clone()
            .block_test(GitPanel::load(weak_workspace, async_window_cx))
            .expect("git panel should load");
        cx.background_executor.forbid_parking();

        window_handle
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.add_panel(git_panel.clone(), window, cx);
                });
            })
            .expect("window should be available");
        cx.run_until_parked();

        let workspace_weak = workspace.downgrade();
        let mut cx = gpui::VisualTestContext::from_window(window_handle.into(), cx);
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        (project, git_graph, git_panel, cx)
    }

    fn three_commits() -> Vec<Arc<InitialGraphCommitData>> {
        let oid = |byte: u8| Oid::from_bytes(&[byte; 20]).expect("valid oid");
        vec![
            Arc::new(InitialGraphCommitData {
                sha: oid(1),
                parents: smallvec![oid(2)],
                ref_names: vec!["HEAD".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid(2),
                parents: smallvec![oid(3)],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid(3),
                parents: smallvec![],
                ref_names: vec![],
            }),
        ]
    }

    /// The graph pushes its selection into the git panel's Commit tab: one row
    /// is a commit to describe, several are a count, and collapsing back to one
    /// row has to narrow the tab again rather than leave the stale set behind.
    #[gpui::test]
    async fn test_graph_selection_drives_the_commit_tab(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let oid = |byte: u8| Oid::from_bytes(&[byte; 20]).expect("valid oid");
        let (_project, git_graph, git_panel, mut cx) =
            setup_graph_with_git_panel(&fs, three_commits(), cx).await;
        let cx = &mut cx;
        draw_graph(&git_graph, cx);

        git_panel.read_with(&*cx, |panel, _| {
            assert!(
                !panel.commit_tab_is_open(),
                "nothing is selected yet, so there is no Commit tab"
            );
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(0, RowSelectionGesture::Replace, window, cx);
        });
        cx.run_until_parked();
        git_panel.read_with(&*cx, |panel, _| {
            assert!(panel.commit_tab_is_open());
            assert_eq!(panel.commit_tab_shas(), [oid(1)]);
        });

        // Ctrl-click a second row: the multi-row set is written back *after*
        // `select_entry` returns, so a push that read the selection eagerly
        // would report only the active row here.
        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(2, RowSelectionGesture::Toggle, window, cx);
        });
        cx.run_until_parked();
        git_panel.read_with(&*cx, |panel, _| {
            assert_eq!(panel.commit_tab_shas(), [oid(1), oid(3)]);
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(1, RowSelectionGesture::Replace, window, cx);
        });
        cx.run_until_parked();
        git_panel.read_with(&*cx, |panel, _| {
            assert_eq!(panel.commit_tab_shas(), [oid(2)]);
        });
    }

    /// The shipped vim keymap binds bare `j` / `k` / `shift-g` / `g g` to
    /// commit-list navigation under `GitGraph && !GitGraphSearchBar`, with no
    /// `vim_mode` gate. The search input lives inside the `GitGraph` context,
    /// so unless it emits `GitGraphSearchBar` those bindings win over plain
    /// text input and typing `j` into the search box moves the selection —
    /// `shift-g` even jumps the list to the oldest commit — instead of
    /// filtering. `gpui::KeyBindingContextPredicate`'s `Not` scans the whole
    /// stack, so emitting the identifier anywhere above the focused editor is
    /// enough to withhold the block.
    #[gpui::test]
    async fn test_search_input_withholds_the_vim_commit_list_bindings(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let (_project, git_graph, _git_panel, mut cx) =
            setup_graph_with_git_panel(&fs, three_commits(), cx).await;
        let cx = &mut cx;

        let commit_list_bindings =
            gpui::KeyBindingContextPredicate::parse("GitGraph && !GitGraphSearchBar")
                .expect("the shipped keymap's predicate parses");

        // `VisualTestContext::draw` paints into `next_frame` and never swaps
        // it in, so `Window::context_stack` would still describe the workspace.
        // Put the graph in a pane and let the real draw build the tree.
        let workspace = git_graph
            .read_with(&*cx, |graph, _| graph.workspace.clone())
            .upgrade()
            .expect("the fixture's workspace outlives the graph");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            graph.focus_handle.focus(window, cx);
        });
        cx.run_until_parked();
        let graph_stack = cx.update(|window, _| window.context_stack());
        assert!(
            graph_stack
                .iter()
                .any(|context| context.contains("GitGraph")),
            "precondition: focusing the graph enters the GitGraph context"
        );
        // `depth_of`, not `eval`: `eval` passes the full stack to both sides of
        // an `And`, so `Identifier("GitGraph")` is only ever tested against the
        // deepest context — the focused editor's. That makes `!eval` true even
        // when the binding is live, which is exactly the bug this test guards.
        // The real keymap resolves through `depth_of` (`keymap.rs`), which
        // scans every prefix of the stack.
        assert!(
            commit_list_bindings.depth_of(&graph_stack).is_some(),
            "precondition: `j` still navigates the commit list when the graph holds focus"
        );

        git_graph.update_in(cx, |graph, window, cx| {
            graph
                .search_state
                .editor
                .update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.run_until_parked();
        let search_stack = cx.update(|window, _| window.context_stack());
        assert!(
            search_stack
                .iter()
                .any(|context| context.contains("GitGraphSearchBar")),
            "the focused search input must emit GitGraphSearchBar, got {search_stack:?}"
        );
        assert!(
            commit_list_bindings.depth_of(&search_stack).is_none(),
            "no commit-list binding may resolve while the search input holds focus"
        );
    }

    /// Escape deselects in the graph, which has to close the tab the selection
    /// opened rather than leave it describing a row that is no longer lit.
    #[gpui::test]
    async fn test_escape_in_the_graph_closes_the_commit_tab(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let (_project, git_graph, git_panel, mut cx) =
            setup_graph_with_git_panel(&fs, three_commits(), cx).await;
        let cx = &mut cx;
        draw_graph(&git_graph, cx);

        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(0, RowSelectionGesture::Replace, window, cx);
        });
        cx.run_until_parked();
        git_panel.read_with(&*cx, |panel, _| assert!(panel.commit_tab_is_open()));

        git_graph.update_in(cx, |graph, window, cx| {
            graph.cancel(&Cancel, window, cx);
        });
        cx.run_until_parked();
        git_panel.read_with(&*cx, |panel, _| assert!(!panel.commit_tab_is_open()));
    }

    /// The panel's ✕ closes the tab, which clears the graph row that opened it.
    /// That clear must not push a second close back at the panel — the loop is
    /// broken by `clear_selection` being silent, so the tab stays closed and
    /// the graph stays deselected instead of the two ping-ponging.
    #[gpui::test]
    async fn test_closing_the_commit_tab_clears_the_graph_selection(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let (_project, git_graph, git_panel, mut cx) =
            setup_graph_with_git_panel(&fs, three_commits(), cx).await;
        let cx = &mut cx;
        draw_graph(&git_graph, cx);

        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(1, RowSelectionGesture::Replace, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(1));
        });

        cx.update_window_entity(&git_panel, |panel, window, cx| {
            panel.close_commit_tab(window, cx);
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, None);
            assert!(graph.selected_entry_idxs.is_empty());
            assert_eq!(graph.selection_anchor_idx, None);
        });
        git_panel.read_with(&*cx, |panel, _| {
            assert!(!panel.commit_tab_is_open());
        });

        // Re-clicking the row the ✕ deselected has to reopen the tab: the
        // no-op early return in `select_entry` is keyed on the row still being
        // selected, and it no longer is.
        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(1, RowSelectionGesture::Replace, window, cx);
        });
        cx.run_until_parked();
        git_panel.read_with(&*cx, |panel, _| assert!(panel.commit_tab_is_open()));
    }

    /// The selection is held in view space, where row 0 can be the synthetic
    /// local-changes row. That row is not a commit, so selecting it closes the
    /// Commit tab — and the resulting `CommitTabClosed` must not bounce back
    /// and deselect the row the user just clicked.
    #[gpui::test]
    async fn test_local_changes_row_is_never_pushed_as_a_commit(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let oid = |byte: u8| Oid::from_bytes(&[byte; 20]).expect("valid oid");
        let (_project, git_graph, git_panel, mut cx) =
            setup_graph_with_git_panel(&fs, three_commits(), cx).await;
        let cx = &mut cx;

        git_graph.update_in(cx, |graph, window, cx| {
            graph.log_source = LogSource::Path(RepoPath::new(&"file.txt").expect("valid path"));
            graph.file_history_options.with_local_changes = true;
            assert!(graph.has_local_changes_row());
            // View row 1 is data row 0 once the synthetic row takes view 0.
            graph.select_entry(
                1,
                ScrollStrategy::Nearest,
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        git_panel.read_with(&*cx, |panel, _| {
            assert_eq!(panel.commit_tab_shas(), [oid(1)]);
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_entry(
                0,
                ScrollStrategy::Nearest,
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        git_panel.read_with(&*cx, |panel, _| {
            assert!(
                !panel.commit_tab_is_open(),
                "the local-changes row has no commit to describe"
            );
        });
        git_graph.read_with(&*cx, |graph, _| {
            assert!(graph.selected_commit_shas().is_empty());
            assert_eq!(
                graph.selected_entry_idx,
                Some(0),
                "closing the tab for a non-commit row must not deselect that row"
            );
        });
    }

    /// Re-clicking the row that is already selected takes `select_entry`'s
    /// no-op early return, so the only thing that can bring the panel back to
    /// the Commit tab is the push scheduled ABOVE that return. With the push
    /// below it every other test here stays green while "select a commit, read
    /// the Changes tab, click that row again" silently stops working.
    #[gpui::test]
    async fn test_reclicking_the_selected_row_reactivates_the_commit_tab(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let oid = |byte: u8| Oid::from_bytes(&[byte; 20]).expect("valid oid");
        let (_project, git_graph, git_panel, mut cx) =
            setup_graph_with_git_panel(&fs, three_commits(), cx).await;
        let cx = &mut cx;
        draw_graph(&git_graph, cx);

        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(1, RowSelectionGesture::Replace, window, cx);
        });
        cx.run_until_parked();
        git_panel.read_with(&*cx, |panel, _| {
            assert!(panel.commit_tab_is_active());
            assert_eq!(panel.commit_tab_shas(), [oid(2)]);
        });

        cx.update_window_entity(&git_panel, |panel, window, cx| {
            panel.activate_changes_tab_for_test(window, cx);
        });
        cx.run_until_parked();
        git_panel.read_with(&*cx, |panel, _| assert!(!panel.commit_tab_is_active()));

        git_graph.read_with(&*cx, |graph, _| {
            // The early return's whole condition: the row about to be
            // re-clicked is still the selected one.
            assert_eq!(graph.selected_entry_idx, Some(1));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(1, RowSelectionGesture::Replace, window, cx);
        });
        cx.run_until_parked();
        git_panel.read_with(&*cx, |panel, _| {
            assert!(
                panel.commit_tab_is_active(),
                "re-clicking the selected row is the way back to the Commit tab"
            );
            assert_eq!(panel.commit_tab_shas(), [oid(2)]);
        });
    }

    /// A commit landing from outside the editor (a background agent, a
    /// terminal `git commit`) invalidates the loaded log and shifts every row
    /// index down by one. The Commit tab must keep describing the commit the
    /// user selected, header *and* file list together: when they drifted
    /// apart, clicking a file asked `CommitView` for a path the displayed
    /// commit never touched, which silently opened an empty tab.
    ///
    /// This used to be a `GitGraph` invariant, back when the graph owned an
    /// inline detail sidebar; the surface moved to the panel but the pairing
    /// still has to hold across the refetch.
    #[gpui::test]
    async fn test_commit_details_survive_external_commit(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let head = Oid::from_bytes(&[1; 20]).expect("valid oid");
        let parent = Oid::from_bytes(&[2; 20]).expect("valid oid");
        let external = Oid::from_bytes(&[3; 20]).expect("valid oid");
        let parent_entry = Arc::new(InitialGraphCommitData {
            sha: parent,
            parents: smallvec![],
            ref_names: vec![],
        });

        let (project, git_graph, git_panel, mut cx) = setup_graph_with_git_panel(
            &fs,
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: head,
                    parents: smallvec![parent],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                parent_entry.clone(),
            ],
            cx,
        )
        .await;
        let cx = &mut cx;
        draw_graph(&git_graph, cx);

        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(0, RowSelectionGesture::Replace, window, cx);
        });
        cx.run_until_parked();

        git_panel.read_with(&*cx, |panel, _| {
            assert_eq!(panel.commit_tab_shas(), [head]);
            assert_eq!(
                panel.commit_tab_loaded_details_sha().as_deref(),
                Some(head.to_string().as_str())
            );
            assert!(
                panel.commit_tab_diff_is_loaded(),
                "the selected commit's changed files should have loaded"
            );
        });

        // An external commit lands on top of `head`, pushing it from row 0 to
        // row 1 once the log is refetched.
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: external,
                    parents: smallvec![head],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: head,
                    parents: smallvec![parent],
                    ref_names: vec![],
                }),
                parent_entry,
            ],
        );
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state.refs.insert("HEAD".into(), external.to_string());
        })
        .expect("fake git state should be writable");

        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();
        draw_graph(&git_graph, cx);

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(
                graph.graph_data.commits.first().map(|entry| entry.data.sha),
                Some(external),
                "the refetched log should start with the externally-created commit"
            );
        });
        git_panel.read_with(&*cx, |panel, _| {
            assert_eq!(
                panel.commit_tab_shas(),
                [head],
                "the tab should still describe the commit the user selected, \
                 not whatever commit inherited its row index"
            );
            assert_eq!(
                panel.commit_tab_loaded_details_sha().as_deref(),
                Some(head.to_string().as_str()),
                "the loaded details must be the ones fetched for the displayed commit"
            );
            assert!(
                panel.commit_tab_diff_is_loaded(),
                "the file list of the re-anchored commit must still be there"
            );
        });
    }

    /// A commit landing from outside the editor makes the graph refetch and
    /// re-anchor its selection, which pushes it at the panel again. That push
    /// must not drag a user who has gone back to Changes — staging files, a
    /// commit message half typed — onto the Commit tab.
    #[gpui::test]
    async fn test_an_external_commit_does_not_yank_the_panel_onto_the_commit_tab(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let head = Oid::from_bytes(&[1; 20]).expect("valid oid");
        let parent = Oid::from_bytes(&[2; 20]).expect("valid oid");
        let external = Oid::from_bytes(&[3; 20]).expect("valid oid");
        let parent_entry = Arc::new(InitialGraphCommitData {
            sha: parent,
            parents: smallvec![],
            ref_names: vec![],
        });

        let (project, git_graph, git_panel, mut cx) = setup_graph_with_git_panel(
            &fs,
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: head,
                    parents: smallvec![parent],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                parent_entry.clone(),
            ],
            cx,
        )
        .await;
        let cx = &mut cx;
        draw_graph(&git_graph, cx);

        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(0, RowSelectionGesture::Replace, window, cx);
        });
        cx.run_until_parked();
        cx.update_window_entity(&git_panel, |panel, window, cx| {
            panel.activate_changes_tab_for_test(window, cx);
        });
        cx.run_until_parked();

        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: external,
                    parents: smallvec![head],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: head,
                    parents: smallvec![parent],
                    ref_names: vec![],
                }),
                parent_entry,
            ],
        );
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state.refs.insert("HEAD".into(), external.to_string());
        })
        .expect("fake git state should be writable");
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();
        draw_graph(&git_graph, cx);

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(
                graph.selected_commit_shas(),
                vec![head],
                "the selection should have been re-anchored onto its new row"
            );
        });
        git_panel.read_with(&*cx, |panel, _| {
            assert!(
                !panel.commit_tab_is_active(),
                "a background refetch must leave the user on the tab they chose"
            );
            assert!(panel.commit_tab_is_open());
            assert_eq!(panel.commit_tab_shas(), [head]);
        });
    }

    /// `git commit --amend` in a terminal rewrites the sha, so the re-anchor
    /// finds nothing and the graph ends up with no selection. The Commit tab
    /// must not be left describing the commit that no longer exists — its file
    /// rows would ask `CommitView` for diffs of a sha git cannot resolve.
    #[gpui::test]
    async fn test_an_amended_commit_closes_the_commit_tab(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let head = Oid::from_bytes(&[1; 20]).expect("valid oid");
        let parent = Oid::from_bytes(&[2; 20]).expect("valid oid");
        let amended = Oid::from_bytes(&[3; 20]).expect("valid oid");
        let parent_entry = Arc::new(InitialGraphCommitData {
            sha: parent,
            parents: smallvec![],
            ref_names: vec![],
        });

        let (project, git_graph, git_panel, mut cx) = setup_graph_with_git_panel(
            &fs,
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: head,
                    parents: smallvec![parent],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                parent_entry.clone(),
            ],
            cx,
        )
        .await;
        let cx = &mut cx;
        draw_graph(&git_graph, cx);

        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(0, RowSelectionGesture::Replace, window, cx);
        });
        cx.run_until_parked();
        git_panel.read_with(&*cx, |panel, _| {
            assert_eq!(panel.commit_tab_shas(), [head]);
        });

        // The amend replaces `head` outright: nothing in the refetched log
        // carries that sha any more.
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: amended,
                    parents: smallvec![parent],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                parent_entry,
            ],
        );
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state.refs.insert("HEAD".into(), amended.to_string());
        })
        .expect("fake git state should be writable");
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();
        draw_graph(&git_graph, cx);

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, None);
            assert!(graph.selected_commit_shas().is_empty());
        });
        git_panel.read_with(&*cx, |panel, _| {
            assert!(
                !panel.commit_tab_is_open(),
                "the tab was describing a commit that no longer exists"
            );
        });
    }

    /// `CommitTabClosed` reaches every graph in the window, so it carries the
    /// shas the closing tab was describing. A graph holding a different row —
    /// a pane-item graph on another repository, or simply a second graph — must
    /// keep its selection when somebody else's tab closes.
    #[gpui::test]
    async fn test_closing_a_tab_for_other_commits_keeps_this_selection(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let oid = |byte: u8| Oid::from_bytes(&[byte; 20]).expect("valid oid");
        let (project, git_graph, git_panel, mut cx) =
            setup_graph_with_git_panel(&fs, three_commits(), cx).await;
        let cx = &mut cx;
        draw_graph(&git_graph, cx);

        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(1, RowSelectionGesture::Replace, window, cx);
        });
        cx.run_until_parked();

        let repository = project.read_with(&*cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        // Stand in for a second graph pushing its own row at the shared panel.
        cx.update_window_entity(&git_panel, |panel, window, cx| {
            panel.show_commit_selection(
                CommitSelection {
                    repository,
                    shas: vec![oid(9)],
                },
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
            panel.close_commit_tab(window, cx);
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(
                graph.selected_entry_idx,
                Some(1),
                "the close described another graph's commit"
            );
        });
    }

    /// Keyboard navigation, restore-by-sha and every other programmatic
    /// selection go through `select_entry`, which has to drop a multi-row
    /// selection built by Ctrl/Shift clicks — including when it re-selects the
    /// row that was already active and takes its no-op early return.
    #[gpui::test]
    async fn test_select_entry_collapses_a_multi_row_selection(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let oid = |byte: u8| Oid::from_bytes(&[byte; 20]).expect("valid oid");
        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: oid(1),
                parents: smallvec![oid(2)],
                ref_names: vec!["HEAD".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid(2),
                parents: smallvec![oid(3)],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid(3),
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];

        let (_project, git_graph, cx) = setup_graph_with_commits(&fs, commits, cx).await;
        draw_graph(&git_graph, cx);

        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(0, RowSelectionGesture::Replace, window, cx);
            graph.apply_row_click_selection(2, RowSelectionGesture::Toggle, window, cx);
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idxs, rows([0, 2]));
            assert_eq!(graph.selected_entry_idx, Some(2));
            assert_eq!(graph.selection_anchor_idx, Some(2));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_entry(
                1,
                ScrollStrategy::Nearest,
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idxs, rows([1]));
            assert_eq!(graph.selected_entry_idx, Some(1));
            assert_eq!(graph.selection_anchor_idx, None);
        });

        // Re-select the active row while a multi-row selection is live: the
        // early return still has to leave a single-row selection behind.
        git_graph.update_in(cx, |graph, window, cx| {
            graph.apply_row_click_selection(1, RowSelectionGesture::Replace, window, cx);
            graph.apply_row_click_selection(0, RowSelectionGesture::Range, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idxs, rows([0, 1]));
            assert_eq!(graph.selected_entry_idx, Some(0));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_entry(
                0,
                ScrollStrategy::Nearest,
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idxs, rows([0]));
            assert_eq!(graph.selection_anchor_idx, None);
        });
    }

    /// `serialize` persists the selection as a sha, and the selection index is
    /// view-space: with the synthetic local-changes row at view 0 every commit
    /// sits one row below its index in `graph_data.commits`. Reading the
    /// commit list with the raw view index therefore persisted the
    /// *neighbouring* commit — and turned a selection of the synthetic row,
    /// which is not a commit at all, into a selection of the newest one.
    #[gpui::test]
    async fn test_serialize_persists_the_selected_sha_past_the_local_changes_row(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let oid = |byte: u8| Oid::from_bytes(&[byte; 20]).expect("valid oid");
        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: oid(1),
                parents: smallvec![oid(2)],
                ref_names: vec!["HEAD".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid(2),
                parents: smallvec![oid(3)],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid(3),
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];

        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "src": { "main.rs": "fn main() {}" },
            }),
        )
        .await;
        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        multi_workspace.update(cx, |multi, cx| multi.set_random_database_id(cx));
        let workspace_id = workspace.read_with(&*cx, |workspace, _| {
            workspace
                .database_id()
                .expect("the test workspace was just given a database id")
        });

        // `git_graphs.workspace_id` is a foreign key into `workspaces`, and
        // the id a test workspace invents has no row behind it.
        let db = cx.read(|cx| persistence::GitGraphsDb::global(cx));
        let raw_workspace_id = i64::from(workspace_id);
        db.write(move |connection| -> anyhow::Result<()> {
            connection.exec(&format!(
                "INSERT INTO workspaces(workspace_id) VALUES ({raw_workspace_id})"
            ))?()?;
            Ok(())
        })
        .await
        .expect("seeding the workspace row should succeed");

        let repo_path = RepoPath::new(&"src/main.rs").expect("valid repo path");
        let workspace_weak = workspace.downgrade();
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::for_file_history(
                repository.read(cx).id,
                repo_path,
                project.read(cx).git_store().clone(),
                workspace_weak,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        draw_graph(&git_graph, cx);

        git_graph.update(cx, |graph, cx| {
            assert_eq!(
                graph.graph_data.commits.len(),
                commits.len(),
                "the file-history graph should have loaded the fake repository's commits"
            );
            graph.set_with_local_changes(true, cx);
            assert!(graph.has_local_changes_row());
        });

        // View row 1 is the newest commit; view row 0 is the synthetic
        // local-changes row.
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_entry(
                1,
                ScrollStrategy::Nearest,
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let item_id = workspace::ItemId::from(4242_u64);
        let persist = |cx: &mut gpui::VisualTestContext| {
            let save = workspace.update_in(cx, |workspace, window, cx| {
                git_graph.update(cx, |graph, cx| {
                    <GitGraph as workspace::SerializableItem>::serialize(
                        graph, workspace, item_id, false, window, cx,
                    )
                })
            });
            save.expect("serialize should produce a save task")
        };

        persist(cx).await.expect("save should succeed");
        let persisted_sha = db
            .get_git_graph(item_id, workspace_id)
            .expect("reading the persisted row should succeed")
            .expect("serialize should have written a row")
            .4;
        assert_eq!(
            persisted_sha.as_deref(),
            Some(commits[0].sha.to_string().as_str()),
            "the selected commit's own sha should be persisted, not its neighbour's"
        );

        // The synthetic row has no commit data, so selecting it persists no
        // selection at all rather than the commit that shares its view index.
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_entry(
                0,
                ScrollStrategy::Nearest,
                CommitSelectionSource::UserGesture,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        persist(cx).await.expect("save should succeed");
        let persisted_sha = db
            .get_git_graph(item_id, workspace_id)
            .expect("reading the persisted row should succeed")
            .expect("serialize should have written a row")
            .4;
        assert_eq!(
            persisted_sha, None,
            "the synthetic local-changes row is not a commit and must not be persisted as one"
        );
    }
}
