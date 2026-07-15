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

use collections::{BTreeMap, HashMap};
use editor::Editor;
use git::{
    BuildCommitPermalinkParams, GitHostingProviderRegistry, GitRemote, Oid, ParsedGitRemote,
    parse_git_remote_url,
    repository::{CommitDiff, CommitFile, InitialGraphCommitData, LogOrder, LogSource, RepoPath},
    status::{FileStatus, StatusCode, TrackedStatus},
};
use git_ui::{commit_tooltip::CommitAvatar, commit_view::CommitView, git_status_icon};
use gpui::{
    Anchor, AnyElement, App, Bounds, ClickEvent, ClipboardItem, DefiniteLength, DismissEvent,
    DragMoveEvent, ElementId, Empty, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    MouseButton, MouseDownEvent, PathBuilder, Pixels, Point, ScrollStrategy, ScrollWheelEvent,
    SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, actions,
    anchored, deferred, point, prelude::*, px, uniform_list,
};
use language::line_diff;
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
use smallvec::{SmallVec, smallvec};
use std::{
    cell::Cell,
    ops::Range,
    rc::Rc,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use theme::AccentColors;
use time::{OffsetDateTime, UtcOffset, format_description::BorrowedFormatItem};
use ui::{
    ButtonLike, Chip, ColumnWidthConfig, CommonAnimationExt as _, ContextMenu, DiffStat, Divider,
    HeaderResizeInfo, RedistributableColumnsState, ScrollableHandle, Table, TableInteractionState,
    TableRenderContext, TableResizeBehavior, Tooltip, WithScrollbar, bind_redistributable_columns,
    prelude::*, render_redistributable_columns_resize_handles, render_table_header,
    table_row::TableRow,
};
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabTooltipContent},
};

const COMMIT_CIRCLE_RADIUS: Pixels = px(3.0);
const COMMIT_CIRCLE_STROKE_WIDTH: Pixels = px(1.5);
// Tight IDEA-style lane spacing: pack several branch lanes into a narrow column
// so the graph stays compact and the description sits close to it, instead of a
// wide graph column shoving the message text far to the right.
const LANE_WIDTH: Pixels = px(10.0);
const LEFT_PADDING: Pixels = px(8.0);
// The commit-graph column has a fixed (non-user-resizable) width, IDEA-style:
// sized to the number of lanes in the loaded history, but always at least
// `MIN_GRAPH_LANES` (so even a linear history reserves sensible space) and
// never more than `MAX_GRAPH_LANES` (so a busy history's lanes don't crowd
// out the message column — overflow lanes are clipped).
const MIN_GRAPH_LANES: usize = 4;
const MAX_GRAPH_LANES: usize = 12;
const LINE_WIDTH: Pixels = px(1.5);
const RESIZE_HANDLE_WIDTH: f32 = 8.0;
const COPIED_STATE_DURATION: Duration = Duration::from_secs(2);
// Extra vertical breathing room added to the UI line height when computing
// the git graph's row height, so commit dots and lines have space around them.
const ROW_VERTICAL_PADDING: Pixels = px(4.0);

/// Whether a search string should be treated as a commit-hash lookup rather
/// than a message grep: all-hex and at least git's default short-hash length
/// (7), so ordinary words — even the odd 4-char hex word like `face`/`dead` —
/// still search commit messages.
fn is_hash_like(text: &str) -> bool {
    (7..=40).contains(&text.len()) && text.chars().all(|c| c.is_ascii_hexdigit())
}

struct CopiedState {
    copied_at: Option<Instant>,
}

impl CopiedState {
    fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { copied_at: None }
    }

    fn is_copied(&self) -> bool {
        self.copied_at
            .map(|t| t.elapsed() < COPIED_STATE_DURATION)
            .unwrap_or(false)
    }

    fn mark_copied(&mut self) {
        self.copied_at = Some(Instant::now());
    }
}

struct DraggedSplitHandle;

#[derive(Clone)]
struct ChangedFileEntry {
    status: FileStatus,
    file_name: SharedString,
    dir_path: SharedString,
    repo_path: RepoPath,
}

impl ChangedFileEntry {
    fn from_commit_file(file: &CommitFile, _cx: &App) -> Self {
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

    fn render(
        &self,
        ix: usize,
        commit_sha: SharedString,
        repository: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        _cx: &App,
    ) -> AnyElement {
        let file_name = self.file_name.clone();
        let dir_path = self.dir_path.clone();

        div()
            .w_full()
            .child(
                ButtonLike::new(("changed-file", ix))
                    .child(
                        h_flex()
                            .min_w_0()
                            .w_full()
                            .gap_1()
                            .overflow_hidden()
                            .child(git_status_icon(self.status))
                            .child(
                                Label::new(file_name.clone())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                            .when(!dir_path.is_empty(), |this| {
                                this.child(
                                    Label::new(dir_path.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate_start(),
                                )
                            }),
                    )
                    .tooltip({
                        let meta = if dir_path.is_empty() {
                            file_name
                        } else {
                            format!("{}/{}", dir_path, file_name).into()
                        };
                        move |_, cx| Tooltip::with_meta("View Changes", None, meta.clone(), cx)
                    })
                    .on_click({
                        let entry = self.clone();
                        move |_, window, cx| {
                            entry.open_file_diff(&commit_sha, &repository, &workspace, window, cx);
                        }
                    }),
            )
            .into_any_element()
    }
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

pub struct SplitState {
    left_ratio: f32,
    visible_left_ratio: f32,
}

impl SplitState {
    pub fn new() -> Self {
        Self {
            left_ratio: 1.0,
            visible_left_ratio: 1.0,
        }
    }

    pub fn right_ratio(&self) -> f32 {
        1.0 - self.visible_left_ratio
    }

    fn on_drag_move(
        &mut self,
        drag_event: &DragMoveEvent<DraggedSplitHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let drag_position = drag_event.event.position;
        let bounds = drag_event.bounds;
        let bounds_width = bounds.right() - bounds.left();

        let min_ratio = 0.1;
        let max_ratio = 0.9;

        let new_ratio = (drag_position.x - bounds.left()) / bounds_width;
        self.visible_left_ratio = new_ratio.clamp(min_ratio, max_ratio);
    }

    fn commit_ratio(&mut self) {
        self.left_ratio = self.visible_left_ratio;
    }

    fn on_double_click(&mut self) {
        self.left_ratio = 1.0;
        self.visible_left_ratio = 1.0;
    }
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
    max_lanes: usize,
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
        self.lines.reserve(commits.len() / 2);

        for commit in commits.iter() {
            let commit_row = self.commits.len();

            let commit_lane = self
                .parent_to_lanes
                .get(&commit.sha)
                .and_then(|lanes| lanes.first().copied());

            let commit_lane = commit_lane.unwrap_or_else(|| self.first_empty_lane_idx());

            let commit_color = self.get_lane_color(commit_lane);

            if let Some(lanes) = self.parent_to_lanes.remove(&commit.sha) {
                for lane_column in lanes {
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

            self.max_lanes = self.max_lanes.max(self.lane_states.len());

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
    git_graph_panel::init(cx);
    mcp::register(cx);

    cx.observe_new(|workspace: &mut workspace::Workspace, _, _| {
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
                graph.select_commit_by_sha(sha.as_str(), cx);
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
            graph.select_commit_by_sha(sha.as_str(), cx);
        }
        graph
    });
    workspace.add_item_to_active_pane(Box::new(git_graph), None, true, window, cx);
}

fn lane_center_x(bounds: Bounds<Pixels>, lane: f32) -> Pixels {
    bounds.origin.x + LEFT_PADDING + lane * LANE_WIDTH + LANE_WIDTH / 2.0
}

fn to_row_center(
    to_row: usize,
    row_height: Pixels,
    scroll_offset: Pixels,
    bounds: Bounds<Pixels>,
) -> Pixels {
    bounds.origin.y + to_row as f32 * row_height + row_height / 2.0 - scroll_offset
}

fn draw_commit_circle(center_x: Pixels, center_y: Pixels, color: Hsla, window: &mut Window) {
    let radius = COMMIT_CIRCLE_RADIUS;

    // A quad whose corner radius equals half its side renders as a filled circle.
    // This is reliable across GPU backends; a hand-built two-arc fill path
    // rasterized blocky ("square") at this tiny radius.
    let bounds = Bounds::new(
        point(center_x - radius, center_y - radius),
        gpui::Size {
            width: radius * 2.0,
            height: radius * 2.0,
        },
    );
    window.paint_quad(gpui::fill(bounds, color).corner_radii(gpui::Corners::all(radius)));
}

fn compute_diff_stats(diff: &CommitDiff) -> (usize, usize) {
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

pub struct GitGraph {
    focus_handle: FocusHandle,
    search_state: SearchState,
    graph_data: GraphData,
    git_store: Entity<GitStore>,
    workspace: WeakEntity<Workspace>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    table_interaction_state: Entity<TableInteractionState>,
    column_widths: Entity<RedistributableColumnsState>,
    selected_entry_idx: Option<usize>,
    hovered_entry_idx: Option<usize>,
    graph_canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    log_source: LogSource,
    log_order: LogOrder,
    /// Chip-based log filters (Branch / User / Date / Path / Query). S-FLT
    /// scaffolding — fields exist, chip UI + plumbing through
    /// `repository::initial_graph_data` lands per-chip in follow-ups.
    filters: filters::LogFilters,
    /// Row-decoration toggles (My commits / New since refresh). S-FLT
    /// scaffolding — wired when chip-Highlights toolbar lands.
    highlights: highlights::HighlightSet,
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
    selected_commit_diff: Option<CommitDiff>,
    selected_commit_diff_stats: Option<(usize, usize)>,
    _commit_diff_task: Option<Task<()>>,
    commit_details_split_state: Entity<SplitState>,
    repo_id: RepositoryId,
    changed_files_scroll_handle: UniformListScrollHandle,
    pending_select_sha: Option<Oid>,
}

impl GitGraph {
    fn invalidate_state(&mut self, cx: &mut Context<Self>) {
        self.graph_data.clear();
        cx.emit(ItemEvent::Edit);
        cx.notify();
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
    fn schedule_query_filter_update(&mut self, cx: &mut Context<Self>) {
        self.search_state.debounce_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            this.update(cx, |this, cx| {
                this.search_state.debounce_task = None;
                this.update_query_filter(cx);
            })
            .ok();
        }));
    }

    /// Build a `QueryFilter` from the search bar editor text + toggle flags
    /// and commit it to `filters.query`. Empty text → `None` (filter
    /// cleared). Called after the 250ms text-change debounce and on every
    /// toggle click so the active query stays in sync with UI state.
    fn update_query_filter(&mut self, cx: &mut Context<Self>) {
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
                self.select_commit_by_sha(oid, cx);
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
    /// detail panel). The caller resolves the `RepoPath` from a
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
        self.file_history_options.with_local_changes = on;
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

    /// Fixed width of the commit-graph column: `clamp(loaded lanes, MIN, MAX)`
    /// lanes' worth, plus the left/right padding the canvas reserves. Not
    /// user-resizable (IDEA-style); lanes past `MAX_GRAPH_LANES` are clipped.
    fn graph_column_width(&self) -> Pixels {
        let lanes = self
            .graph_data
            .max_lanes
            .clamp(MIN_GRAPH_LANES, MAX_GRAPH_LANES);
        (LANE_WIDTH * lanes as f32) + LEFT_PADDING * 2.0
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

        cx.subscribe(&git_store, |this, _, event, cx| match event {
            GitStoreEvent::RepositoryUpdated(updated_repo_id, repo_event, _) => {
                if this.repo_id == *updated_repo_id {
                    if let Some(repository) = this.get_repository(cx) {
                        this.on_repository_event(repository, repo_event, cx);
                    }
                }
            }
            _ => {}
        })
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
        // detail panel on click, and search-by-hash is server-side). The
        // commit-graph column is *not* a table column — it's rendered separately
        // at a fixed width to the left of the table (IDEA-style), no resize handle.
        let column_widths = cx.new(|_cx| {
            RedistributableColumnsState::new(
                3,
                vec![
                    DefiniteLength::Fraction(0.74),
                    DefiniteLength::Fraction(0.13),
                    DefiniteLength::Fraction(0.13),
                ],
                vec![
                    TableResizeBehavior::Resizable,
                    TableResizeBehavior::Resizable,
                    TableResizeBehavior::Resizable,
                ],
            )
        });
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
            |this, _editor, event: &editor::EditorEvent, _window, cx| {
                if let editor::EditorEvent::BufferEdited = event {
                    this.schedule_query_filter_update(cx);
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
            _commit_diff_task: None,
            context_menu: None,
            table_interaction_state,
            column_widths,
            selected_entry_idx: None,
            hovered_entry_idx: None,
            graph_canvas_bounds: Rc::new(Cell::new(None)),
            selected_commit_diff: None,
            selected_commit_diff_stats: None,
            log_source,
            log_order,
            filters: filters::LogFilters::default(),
            highlights: highlights::HighlightSet::default(),
            view_options: view_options::ViewOptions::default(),
            file_history_options: file_history::FileHistoryOptions::default(),
            local_user_email: None,
            commit_details_split_state: cx.new(|_cx| SplitState::new()),
            repo_id,
            changed_files_scroll_handle: UniformListScrollHandle::new(),
            pending_select_sha: None,
        };

        this.fetch_initial_graph_data(cx);
        this.fetch_local_user_email(cx);
        this
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
                        if let Some(pending_sha_data_index) =
                            self.pending_select_sha.take().and_then(|oid| {
                                repository
                                    .read(cx)
                                    .get_graph_data(
                                        source.clone(),
                                        *order,
                                        &extra_args,
                                        &extra_paths,
                                    )
                                    .and_then(|data| data.commit_oid_to_index.get(&oid).copied())
                            })
                        {
                            let view_index = self.data_to_view_idx(pending_sha_data_index);
                            self.select_entry(view_index, ScrollStrategy::Nearest, cx);
                        }
                    }
                    GitGraphEvent::LoadingError => {
                        // todo(git_graph): Wire this up with the UI
                    }
                    GitGraphEvent::CountUpdated(commit_count) => {
                        let old_count = self.graph_data.commits.len();

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
                            self.select_entry(view_index, ScrollStrategy::Nearest, cx);
                            self.pending_select_sha.take();
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
                    self.pending_select_sha = None;
                    self.invalidate_state(cx);
                }
            }
            RepositoryEvent::StashEntriesChanged if self.log_source == LogSource::All => {
                // Stash entries initial's scan id is 2, so we don't want to invalidate the graph before that
                if repository.read(cx).scan_id > 2 {
                    self.pending_select_sha = None;
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

    fn render_chip(
        &self,
        name: &SharedString,
        accent_color: gpui::Hsla,
        is_head: bool,
        work_dir: Option<&std::path::Path>,
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
            .truncate()
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

                vec![
                    div()
                        .id(ElementId::NamedInteger("commit-subject".into(), idx as u64))
                        .overflow_hidden()
                        .tooltip(Tooltip::text(subject))
                        .child(description_cell)
                        .into_any_element(),
                    column_label(formatted_time.into()),
                    column_label(author_name),
                ]
            })
            .collect()
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.selected_entry_idx = None;
        self.selected_commit_diff = None;
        self.selected_commit_diff_stats = None;
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    fn select_first(&mut self, _: &SelectFirst, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_entry(0, ScrollStrategy::Nearest, cx);
    }

    fn select_prev(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected_entry_idx) = &self.selected_entry_idx {
            self.select_entry(
                selected_entry_idx.saturating_sub(1),
                ScrollStrategy::Nearest,
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
                cx,
            );
        } else {
            self.select_prev(&SelectPrevious, window, cx);
        }
    }

    fn select_last(&mut self, _: &SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_entry(
            self.view_row_count().saturating_sub(1),
            ScrollStrategy::Nearest,
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
        cx: &mut Context<Self>,
    ) {
        if self.selected_entry_idx == Some(idx) {
            return;
        }

        self.selected_entry_idx = Some(idx);
        self.selected_commit_diff = None;
        self.selected_commit_diff_stats = None;
        self.changed_files_scroll_handle
            .scroll_to_item(0, ScrollStrategy::Top);
        self.table_interaction_state.update(cx, |state, cx| {
            state.scroll_handle.scroll_to_item(idx, scroll_strategy);
            cx.notify();
        });

        // The synthetic "local changes" row at view-index 0 has no commit
        // data — selecting it leaves the detail panel empty (this is by
        // design; v1 doesn't render a working-tree-vs-HEAD diff yet).
        if self.has_local_changes_row() && idx == 0 {
            cx.emit(ItemEvent::Edit);
            cx.notify();
            return;
        }
        let data_idx = if self.has_local_changes_row() {
            idx.saturating_sub(1)
        } else {
            idx
        };

        let Some(commit) = self.graph_data.commits.get(data_idx) else {
            return;
        };

        let sha = commit.data.sha.to_string();

        let Some(repository) = self.get_repository(cx) else {
            return;
        };

        let diff_receiver = repository.update(cx, |repo, _| repo.load_commit_diff(sha));

        self._commit_diff_task = Some(cx.spawn(async move |this, cx| {
            if let Ok(Ok(diff)) = diff_receiver.await {
                this.update(cx, |this, cx| {
                    let stats = compute_diff_stats(&diff);
                    this.selected_commit_diff = Some(diff);
                    this.selected_commit_diff_stats = Some(stats);
                    cx.notify();
                })
                .ok();
            }
        }));

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

    pub fn set_repo_id(&mut self, repo_id: RepositoryId, cx: &mut Context<Self>) {
        if repo_id != self.repo_id
            && self
                .git_store
                .read(cx)
                .repositories()
                .contains_key(&repo_id)
        {
            self.repo_id = repo_id;
            self.invalidate_state(cx);
        }
    }

    pub fn select_commit_by_sha(&mut self, sha: impl TryInto<Oid>, cx: &mut Context<Self>) {
        fn inner(this: &mut GitGraph, oid: Oid, cx: &mut Context<GitGraph>) {
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
            this.select_entry(view_index, ScrollStrategy::Center, cx);
        }

        if let Ok(oid) = sha.try_into() {
            inner(self, oid, cx);
        }
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

    fn get_remote(
        &self,
        repository: &Repository,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<GitRemote> {
        let remote_url = repository.default_remote_url()?;
        let provider_registry = GitHostingProviderRegistry::default_global(cx);
        let (provider, parsed) = parse_git_remote_url(provider_registry, &remote_url)?;
        Some(GitRemote {
            host: provider,
            owner: parsed.owner.into(),
            repo: parsed.repo.into(),
        })
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
        let (head_branch, local_branches) = {
            let repo = repository.read(cx);
            let head_branch = repo
                .branch
                .as_ref()
                .map(|b| SharedString::from(b.name().to_string()));
            let local_branches = repo
                .branch_list
                .iter()
                .filter(|b| !b.is_remote())
                .map(|b| SharedString::from(b.name().to_string()))
                .collect::<Vec<_>>();
            (head_branch, local_branches)
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
        };
        let menu = context_menu::build_commit_context_menu(ctx, window, cx);
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

    fn render_commit_detail_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(selected_idx) = self.selected_entry_idx else {
            return Empty.into_any_element();
        };

        let Some(commit_entry) = self.graph_data.commits.get(selected_idx) else {
            return Empty.into_any_element();
        };

        let Some(repository) = self.get_repository(cx) else {
            return Empty.into_any_element();
        };

        let data = repository.update(cx, |repository, cx| {
            repository
                .fetch_commit_data(commit_entry.data.sha, false, cx)
                .clone()
        });

        let full_sha: SharedString = commit_entry.data.sha.to_string().into();
        let ref_names = commit_entry.data.ref_names.clone();

        let head_branch_name: Option<SharedString> = repository
            .read(cx)
            .snapshot()
            .branch
            .as_ref()
            .map(|branch| SharedString::from(branch.name().to_string()));

        let work_dir = repository.read(cx).work_directory_abs_path.to_path_buf();

        let accent_colors = cx.theme().accents();
        let accent_color = accent_colors
            .0
            .get(commit_entry.color_idx)
            .copied()
            .unwrap_or_else(|| accent_colors.0.first().copied().unwrap_or_default());

        // todo(git graph): We should use the full commit message here
        let (author_name, author_email, commit_timestamp, commit_message) = match &data {
            CommitDataState::Loaded(data) => (
                data.author_name.clone(),
                data.author_email.clone(),
                Some(data.commit_timestamp),
                data.subject.clone(),
            ),
            CommitDataState::Loading(_) => ("Loading…".into(), "".into(), None, "Loading…".into()),
        };

        let date_string = commit_timestamp.map(format_timestamp).unwrap_or_default();
        let short_sha: SharedString = commit_entry.data.sha.display_short().into();

        let remote = repository.update(cx, |repo, cx| self.get_remote(repo, window, cx));

        let avatar = {
            let author_email_for_avatar = if author_email.is_empty() {
                None
            } else {
                Some(author_email.clone())
            };

            CommitAvatar::new(&full_sha, author_email_for_avatar, remote.as_ref())
                .size(px(16.))
                .render(window, cx)
        };

        let changed_files_count = self
            .selected_commit_diff
            .as_ref()
            .map(|diff| diff.files.len())
            .unwrap_or(0);

        let (total_lines_added, total_lines_removed) =
            self.selected_commit_diff_stats.unwrap_or((0, 0));

        let sorted_file_entries: Rc<Vec<ChangedFileEntry>> = Rc::new(
            self.selected_commit_diff
                .as_ref()
                .map(|diff| {
                    let mut files: Vec<_> = diff.files.iter().collect();
                    files.sort_by_key(|file| file.status());
                    files
                        .into_iter()
                        .map(|file| ChangedFileEntry::from_commit_file(file, cx))
                        .collect()
                })
                .unwrap_or_default(),
        );

        v_flex()
            .min_w(px(300.))
            .h_full()
            .bg(cx.theme().colors().editor_background)
            .flex_basis(DefiniteLength::Fraction(
                self.commit_details_split_state.read(cx).right_ratio(),
            ))
            .child(
                v_flex()
                    .relative()
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .gap_1p5()
                    .child(
                        div().absolute().top_1().right_1().child(
                            IconButton::new("close-detail", IconName::Close)
                                .icon_size(IconSize::Small)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.selected_entry_idx = None;
                                    this.selected_commit_diff = None;
                                    this.selected_commit_diff_stats = None;
                                    this._commit_diff_task = None;
                                    cx.notify();
                                })),
                        ),
                    )
                    // Subject — pad on the right so it doesn't run under the
                    // absolutely-positioned close button.
                    .child(
                        div()
                            .pr_5()
                            .child(Label::new(commit_message).weight(gpui::FontWeight::SEMIBOLD)),
                    )
                    // Author identity line: avatar · short SHA (click to copy) · name.
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(avatar)
                            .child({
                                let copy_sha = full_sha.clone();
                                let copied_state: Entity<CopiedState> =
                                    window.use_keyed_state("sha-copy", cx, CopiedState::new);
                                let is_copied = copied_state.read(cx).is_copied();

                                let (label_text, icon, icon_color, tooltip_label) = if is_copied {
                                    (
                                        SharedString::from("Copied!"),
                                        IconName::Check,
                                        Color::Success,
                                        "Commit SHA Copied!",
                                    )
                                } else {
                                    (short_sha, IconName::Hash, Color::Muted, "Copy Commit SHA")
                                };

                                Button::new("sha-button", label_text)
                                    .start_icon(
                                        Icon::new(icon).size(IconSize::Small).color(icon_color),
                                    )
                                    .label_size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .tooltip({
                                        let full_sha = full_sha.clone();
                                        move |_, cx| {
                                            Tooltip::with_meta(
                                                tooltip_label,
                                                None,
                                                full_sha.clone(),
                                                cx,
                                            )
                                        }
                                    })
                                    .on_click(move |_, _, cx| {
                                        copied_state.update(cx, |state, _cx| {
                                            state.mark_copied();
                                        });
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_sha.to_string(),
                                        ));
                                        let state_id = copied_state.entity_id();
                                        cx.spawn(async move |cx| {
                                            cx.background_executor()
                                                .timer(COPIED_STATE_DURATION)
                                                .await;
                                            cx.update(|cx| {
                                                cx.notify(state_id);
                                            })
                                        })
                                        .detach();
                                    })
                            })
                            .child(Label::new(author_name).size(LabelSize::Small).truncate()),
                    )
                    // Email · date line.
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .flex_wrap()
                            .when(!author_email.is_empty(), |this| {
                                let copied_state: Entity<CopiedState> = window.use_keyed_state(
                                    "author-email-copy",
                                    cx,
                                    CopiedState::new,
                                );
                                let is_copied = copied_state.read(cx).is_copied();

                                let (icon, icon_color, tooltip_label) = if is_copied {
                                    (IconName::Check, Color::Success, "Email Copied!")
                                } else {
                                    (IconName::Envelope, Color::Muted, "Copy Email")
                                };

                                let copy_email = author_email.clone();
                                let author_email_for_tooltip = author_email.clone();

                                this.child(
                                    Button::new("author-email-copy", author_email.clone())
                                        .start_icon(
                                            Icon::new(icon).size(IconSize::Small).color(icon_color),
                                        )
                                        .label_size(LabelSize::Small)
                                        .truncate(true)
                                        .color(Color::Muted)
                                        .tooltip(move |_, cx| {
                                            Tooltip::with_meta(
                                                tooltip_label,
                                                None,
                                                author_email_for_tooltip.clone(),
                                                cx,
                                            )
                                        })
                                        .on_click(move |_, _, cx| {
                                            copied_state.update(cx, |state, _cx| {
                                                state.mark_copied();
                                            });
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy_email.to_string(),
                                            ));
                                            let state_id = copied_state.entity_id();
                                            cx.spawn(async move |cx| {
                                                cx.background_executor()
                                                    .timer(COPIED_STATE_DURATION)
                                                    .await;
                                                cx.update(|cx| {
                                                    cx.notify(state_id);
                                                })
                                            })
                                            .detach();
                                        }),
                                )
                            })
                            .child(
                                Label::new(date_string)
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            ),
                    )
                    // Ref decorations.
                    .children((!ref_names.is_empty()).then(|| {
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .children(ref_names.iter().map(|name| {
                                let is_head = Self::is_head_ref(name.as_ref(), &head_branch_name);
                                self.render_chip(
                                    name,
                                    accent_color,
                                    is_head,
                                    Some(work_dir.as_path()),
                                )
                            }))
                    }))
                    .when_some(remote.clone(), |this, remote| {
                        let provider_name = remote.host.name();
                        let icon = match provider_name.as_str() {
                            "GitHub" => IconName::Github,
                            _ => IconName::Link,
                        };
                        let parsed_remote = ParsedGitRemote {
                            owner: remote.owner.as_ref().into(),
                            repo: remote.repo.as_ref().into(),
                        };
                        let params = BuildCommitPermalinkParams {
                            sha: full_sha.as_ref(),
                        };
                        let url = remote
                            .host
                            .build_commit_permalink(&parsed_remote, params)
                            .to_string();

                        this.child(
                            Button::new("view-on-provider", format!("View on {}", provider_name))
                                .start_icon(
                                    Icon::new(icon).size(IconSize::Small).color(Color::Muted),
                                )
                                .label_size(LabelSize::Small)
                                .truncate(true)
                                .color(Color::Muted)
                                .on_click(move |_, _, cx| {
                                    cx.open_url(&url);
                                }),
                        )
                    }),
            )
            .child(Divider::horizontal())
            .child(
                v_flex()
                    .min_w_0()
                    .p_2()
                    .flex_1()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_1()
                            .w_full()
                            .justify_between()
                            .child(
                                Label::new(format!(
                                    "{} Changed {}",
                                    changed_files_count,
                                    if changed_files_count == 1 {
                                        "File"
                                    } else {
                                        "Files"
                                    }
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                            .child(DiffStat::new(
                                "commit-diff-stat",
                                total_lines_added,
                                total_lines_removed,
                            )),
                    )
                    .child(
                        div()
                            .id("changed-files-container")
                            .flex_1()
                            .min_h_0()
                            .child({
                                let entries = sorted_file_entries;
                                let entry_count = entries.len();
                                let commit_sha = full_sha.clone();
                                let repository = repository.downgrade();
                                let workspace = self.workspace.clone();
                                uniform_list(
                                    "changed-files-list",
                                    entry_count,
                                    move |range, _window, cx| {
                                        range
                                            .map(|ix| {
                                                entries[ix].render(
                                                    ix,
                                                    commit_sha.clone(),
                                                    repository.clone(),
                                                    workspace.clone(),
                                                    cx,
                                                )
                                            })
                                            .collect()
                                    },
                                )
                                .size_full()
                                .ml_neg_1()
                                .track_scroll(&self.changed_files_scroll_handle)
                            })
                            .vertical_scrollbar_for(&self.changed_files_scroll_handle, window, cx),
                    ),
            )
            .child(Divider::horizontal())
            .child(
                h_flex().p_1p5().w_full().child(
                    Button::new("view-commit", "View Commit")
                        .full_width()
                        .style(ButtonStyle::OutlinedGhost)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_selected_commit_view(window, cx);
                        })),
                ),
            )
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

        let graph_width = self.graph_column_width();
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

        let hovered_entry_idx = self.hovered_entry_idx;
        let selected_entry_idx = self.selected_entry_idx;
        let is_focused = self.focus_handle.is_focused(window);
        let graph_canvas_bounds = self.graph_canvas_bounds.clone();

        gpui::canvas(
            move |_bounds, _window, _cx| {},
            move |bounds: Bounds<Pixels>, _: (), window: &mut Window, cx: &mut App| {
                graph_canvas_bounds.set(Some(bounds));

                window.paint_layer(bounds, |window| {
                    let accent_colors = cx.theme().accents();

                    let hover_bg = cx.theme().colors().element_hover.opacity(0.6);
                    let selected_bg = if is_focused {
                        cx.theme().colors().element_selected
                    } else {
                        cx.theme().colors().element_hover
                    };

                    for visible_row_idx in 0..rows.len() {
                        let absolute_row_idx = first_visible_row + visible_row_idx;
                        let is_hovered = hovered_entry_idx == Some(absolute_row_idx);
                        let is_selected = selected_entry_idx == Some(absolute_row_idx);

                        if is_hovered || is_selected {
                            let row_y = bounds.origin.y + visible_row_idx as f32 * row_height
                                - vertical_scroll_offset;

                            let row_bounds = Bounds::new(
                                point(bounds.origin.x, row_y),
                                gpui::Size {
                                    width: bounds.size.width,
                                    height: row_height,
                                },
                            );

                            let bg_color = if is_selected { selected_bg } else { hover_bg };
                            window.paint_quad(gpui::fill(row_bounds, bg_color));
                        }
                    }

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
                        let desired_curve_height = row_height / 3.0;
                        let desired_curve_width = LANE_WIDTH / 3.0;

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
                                    let mut to_column = lane_center_x(bounds, *to_column as f32);

                                    let mut to_row = to_row_center(
                                        *on_row - first_visible_row,
                                        row_height,
                                        vertical_scroll_offset,
                                        bounds,
                                    );

                                    // This means that this branch was a checkout
                                    let going_right = to_column > current_column;
                                    let column_shift = if going_right {
                                        COMMIT_CIRCLE_RADIUS + COMMIT_CIRCLE_STROKE_WIDTH
                                    } else {
                                        -COMMIT_CIRCLE_RADIUS - COMMIT_CIRCLE_STROKE_WIDTH
                                    };

                                    match curve_kind {
                                        CurveKind::Checkout => {
                                            if is_last {
                                                to_column -= column_shift;
                                            }

                                            let available_curve_width =
                                                (to_column - current_column).abs();
                                            let available_curve_height =
                                                (to_row - current_row).abs();
                                            let curve_width =
                                                desired_curve_width.min(available_curve_width);
                                            let curve_height =
                                                desired_curve_height.min(available_curve_height);
                                            let signed_curve_width = if going_right {
                                                curve_width
                                            } else {
                                                -curve_width
                                            };
                                            let curve_start =
                                                point(current_column, to_row - curve_height);
                                            let curve_end =
                                                point(current_column + signed_curve_width, to_row);
                                            let curve_control = point(current_column, to_row);

                                            builder.move_to(point(current_column, current_row));
                                            builder.line_to(curve_start);
                                            builder.move_to(curve_start);
                                            builder.curve_to(curve_end, curve_control);
                                            builder.move_to(curve_end);
                                            builder.line_to(point(to_column, to_row));
                                        }
                                        CurveKind::Merge => {
                                            if is_last {
                                                to_row -= COMMIT_CIRCLE_RADIUS;
                                            }

                                            let merge_start = point(
                                                current_column + column_shift,
                                                current_row - COMMIT_CIRCLE_RADIUS,
                                            );
                                            let available_curve_width =
                                                (to_column - merge_start.x).abs();
                                            let available_curve_height =
                                                (to_row - merge_start.y).abs();
                                            let curve_width =
                                                desired_curve_width.min(available_curve_width);
                                            let curve_height =
                                                desired_curve_height.min(available_curve_height);
                                            let signed_curve_width = if going_right {
                                                curve_width
                                            } else {
                                                -curve_width
                                            };
                                            let curve_start = point(
                                                to_column - signed_curve_width,
                                                merge_start.y,
                                            );
                                            let curve_end =
                                                point(to_column, merge_start.y + curve_height);
                                            let curve_control = point(to_column, merge_start.y);

                                            builder.move_to(merge_start);
                                            builder.line_to(curve_start);
                                            builder.move_to(curve_start);
                                            builder.curve_to(curve_end, curve_control);
                                            builder.move_to(curve_end);
                                            builder.line_to(point(to_column, to_row));
                                        }
                                    }
                                    current_row = to_row;
                                    current_column = to_column;
                                    builder.move_to(point(current_column, current_row));
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

    fn row_at_position(
        &self,
        position_y: Pixels,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<usize> {
        let canvas_bounds = self.graph_canvas_bounds.get()?;
        let table_state = self.table_interaction_state.read(cx);
        let scroll_offset_y = -table_state.scroll_offset().y;

        let local_y = position_y - canvas_bounds.origin.y;

        if local_y >= px(0.) && local_y < canvas_bounds.size.height {
            let absolute_y = local_y + scroll_offset_y;
            let row_height = Self::row_height(window, cx);
            let absolute_row = (absolute_y / row_height).floor() as usize;

            if absolute_row < self.graph_data.commits.len() {
                return Some(absolute_row);
            }
        }

        None
    }

    fn handle_graph_mouse_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.row_at_position(event.position.y, window, cx) {
            if self.hovered_entry_idx != Some(row) {
                self.hovered_entry_idx = Some(row);
                cx.notify();
            }
        } else if self.hovered_entry_idx.is_some() {
            self.hovered_entry_idx = None;
            cx.notify();
        }
    }

    fn handle_graph_click(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.row_at_position(event.position().y, window, cx) {
            self.select_entry(row, ScrollStrategy::Nearest, cx);
            if event.click_count() >= 2 {
                self.open_commit_view(row, window, cx);
            }
        }
    }

    fn handle_graph_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = window.line_height();
        let delta = event.delta.pixel_delta(line_height);

        let table_state = self.table_interaction_state.read(cx);
        let current_offset = table_state.scroll_offset();

        let viewport_height = table_state.scroll_handle.viewport().size.height;

        let commit_count = match self.graph_data.max_commit_count {
            AllCommitCount::Loaded(count) => count,
            AllCommitCount::NotLoaded => self.graph_data.commits.len(),
        };
        let content_height = Self::row_height(window, cx) * commit_count;
        let max_vertical_scroll = (viewport_height - content_height).min(px(0.));

        let new_y = (current_offset.y + delta.y).clamp(max_vertical_scroll, px(0.));
        let new_offset = Point::new(current_offset.x, new_y);

        if new_offset != current_offset {
            table_state.set_scroll_offset(new_offset);
            cx.notify();
        }
    }

    fn render_commit_view_resize_handle(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("commit-view-split-resize-container")
            .relative()
            .h_full()
            .flex_shrink_0()
            .w(px(1.))
            .bg(cx.theme().colors().border_variant)
            .child(
                div()
                    .id("commit-view-split-resize-handle")
                    .absolute()
                    .left(px(-RESIZE_HANDLE_WIDTH / 2.0))
                    .w(px(RESIZE_HANDLE_WIDTH))
                    .h_full()
                    .cursor_col_resize()
                    .block_mouse_except_scroll()
                    .on_click(cx.listener(|this, event: &ClickEvent, _window, cx| {
                        if event.click_count() >= 2 {
                            this.commit_details_split_state.update(cx, |state, _| {
                                state.on_double_click();
                            });
                        }
                        cx.stop_propagation();
                    }))
                    .on_drag(DraggedSplitHandle, |_, _, _, cx| cx.new(|_| gpui::Empty)),
            )
            .into_any_element()
    }
}

impl Render for GitGraph {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (mut commit_count, is_loading) = match self.graph_data.max_commit_count {
            AllCommitCount::Loaded(count) => (count, true),
            AllCommitCount::NotLoaded => {
                let extra_args = self.combined_extra_args();
                let extra_paths = self.filters.paths_args();
                let (commit_count, is_loading) = if let Some(repository) = self.get_repository(cx) {
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
                };

                (commit_count, is_loading)
            }
        };

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
            // Fixed width for the (non-resizable) commit-graph column.
            let graph_width = self.graph_column_width();

            h_flex()
                .size_full()
                .child(
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
                                .when(!is_file_history, |this| {
                                    this.child(
                                        div()
                                            .flex_none()
                                            .w(graph_width)
                                            .overflow_hidden()
                                            .border_b_1()
                                            .border_color(cx.theme().colors().border)
                                            .px_1()
                                            .py_0p5()
                                            .child(
                                                Label::new("Graph").color(Color::Muted).truncate(),
                                            ),
                                    )
                                })
                                .child(div().flex_1().min_w_0().child(render_table_header(
                                    TableRow::from_vec(
                                        vec![
                                                Label::new("Description")
                                                    .color(Color::Muted)
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
                                    Some(self.column_widths.entity_id()),
                                    cx,
                                ))),
                        )
                        .child({
                            let row_height = Self::row_height(window, cx);
                            let selected_entry_idx = self.selected_entry_idx;
                            let hovered_entry_idx = self.hovered_entry_idx;
                            let weak_self = cx.weak_entity();
                            let focus_handle = self.focus_handle.clone();

                            let graph_canvas = div()
                                .id("graph-canvas")
                                .size_full()
                                .overflow_hidden()
                                .child(
                                    div()
                                        .size_full()
                                        .child(self.render_graph_canvas(window, cx)),
                                )
                                .on_scroll_wheel(cx.listener(Self::handle_graph_scroll))
                                .on_mouse_move(cx.listener(Self::handle_graph_mouse_move))
                                .on_click(cx.listener(Self::handle_graph_click))
                                .on_hover(cx.listener(|this, &is_hovered: &bool, _, cx| {
                                    if !is_hovered && this.hovered_entry_idx.is_some() {
                                        this.hovered_entry_idx = None;
                                        cx.notify();
                                    }
                                }));

                            let commits_table = Table::new(3)
                                .interactable(&self.table_interaction_state)
                                .hide_row_borders()
                                .hide_row_hover()
                                .width_config(table_width_config)
                                .map_row(move |(index, row), window, cx| {
                                    let is_selected = selected_entry_idx == Some(index);
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
                                                    } else if this.hovered_entry_idx == Some(index)
                                                    {
                                                        this.hovered_entry_idx = None;
                                                        cx.notify();
                                                    }
                                                })
                                                .ok();
                                        })
                                        .on_click({
                                            let weak = weak.clone();
                                            move |event, window, cx| {
                                                let click_count = event.click_count();
                                                weak.update(cx, |this, cx| {
                                                    this.select_entry(
                                                        index,
                                                        ScrollStrategy::Center,
                                                        cx,
                                                    );
                                                    if click_count >= 2 {
                                                        this.open_commit_view(index, window, cx);
                                                    }
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
                                                    this.select_entry(
                                                        index,
                                                        ScrollStrategy::Center,
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
                                .flex_1()
                                .w_full()
                                .items_stretch()
                                .when(!is_file_history, |this| {
                                    this.child(
                                        div()
                                            .flex_none()
                                            .w(graph_width)
                                            .h_full()
                                            .overflow_hidden()
                                            .child(graph_canvas),
                                    )
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
                                ))
                        }),
                )
                .on_drag_move::<DraggedSplitHandle>(cx.listener(|this, event, window, cx| {
                    this.commit_details_split_state.update(cx, |state, cx| {
                        state.on_drag_move(event, window, cx);
                    });
                }))
                .on_drop::<DraggedSplitHandle>(cx.listener(|this, _event, _window, cx| {
                    this.commit_details_split_state.update(cx, |state, _cx| {
                        state.commit_ratio();
                    });
                }))
                .when(self.selected_entry_idx.is_some(), |this| {
                    this.child(self.render_commit_view_resize_handle(window, cx))
                        .child(self.render_commit_detail_panel(window, cx))
                })
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
            .on_action(cx.listener(|this, _: &ToggleCaseSensitive, _window, cx| {
                this.search_state.case_sensitive = !this.search_state.case_sensitive;
                this.update_query_filter(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleRegex, _window, cx| {
                this.search_state.regex = !this.search_state.regex;
                this.update_query_filter(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleSearchInDiffs, _window, cx| {
                this.search_state.search_in_diffs = !this.search_state.search_in_diffs;
                this.update_query_filter(cx);
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
                        graph.select_commit_by_sha(sha.as_str(), cx);
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

        let selected_sha = self
            .selected_entry_idx
            .and_then(|idx| self.graph_data.commits.get(idx))
            .map(|commit| commit.data.sha.to_string());

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
    use collections::{HashMap, HashSet};
    use fs::FakeFs;
    use git::Oid;
    use git::repository::InitialGraphCommitData;
    use gpui::{TestAppContext, UpdateGlobal};
    use project::Project;
    use project::git_store::{GitStoreEvent, RepositoryEvent};
    use rand::prelude::*;
    use serde_json::json;
    use settings::{SettingsStore, ThemeSettingsContent};
    use smallvec::{SmallVec, smallvec};
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

        git_graph.update(cx, |graph, cx| {
            graph.set_repo_id(second_repository.read(cx).id, cx)
        });
        cx.run_until_parked();

        let commit_count_after_clear =
            git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert_eq!(
            commit_count_after_clear, 0,
            "graph_data should be cleared after switching away"
        );

        git_graph.update(cx, |graph, cx| {
            graph.set_repo_id(first_repository.read(cx).id, cx)
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

        let selected_sha = git_graph.read_with(&*cx, |graph, _| {
            graph
                .selected_entry_idx
                .and_then(|idx| graph.graph_data.commits.get(idx))
                .map(|c| c.data.sha.to_string())
        });
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

            let restored_selected_sha = graph
                .selected_entry_idx
                .and_then(|idx| graph.graph_data.commits.get(idx))
                .map(|c| c.data.sha.to_string());
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
    async fn test_git_graph_row_at_position_rounding(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
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
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            assert!(
                graph.graph_data.commits.len() >= 10,
                "graph should load dummy commits"
            );

            let row_height = GitGraph::row_height(window, cx);
            let origin_y = px(100.0);
            graph.graph_canvas_bounds.set(Some(Bounds {
                origin: point(px(0.0), origin_y),
                size: gpui::size(px(100.0), row_height * 50.0),
            }));

            // Scroll down by half a row so the row under a position near the
            // top of the canvas is row 1 rather than row 0.
            let scroll_offset = row_height * 0.75;
            graph.table_interaction_state.update(cx, |state, _| {
                state.set_scroll_offset(point(px(0.0), -scroll_offset))
            });
            let pos_y = origin_y + row_height * 0.5;
            let absolute_calc_row = graph.row_at_position(pos_y, window, cx);

            assert_eq!(
                absolute_calc_row,
                Some(1),
                "Row calculation should yield absolute row exactly"
            );
        });
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

        // Column-count assertion: in file-history mode the table is the
        // four columns Description / Date / Author / Hash (the graph lane
        // is hidden).
        graph.read_with(cx, |graph, cx| {
            let widths = graph.column_widths.read(cx);
            assert_eq!(widths.cols(), 4);
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
}
