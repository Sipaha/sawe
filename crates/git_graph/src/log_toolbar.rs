//! Chip-based log filter toolbar for the Git Graph view (S-FLT in
//! `docs/superpowers/plans/git-panel-plan.md`).
//!
//! `LogToolbar` is a render-only widget the `GitGraph` view embeds between
//! its search bar and the graph content. It renders one chip per filter
//! dimension; each chip opens a popover for editing that dimension. Date,
//! Branch, User, and Path chips are wired today; Query lands in
//! follow-ups alongside.

mod branch_popover;
mod path_popover;
mod user_popover;

use chrono::{Datelike, Local, NaiveDate, TimeZone};
use editor::Editor;
use git::repository::RepoPath;
use gpui::{
    AnyElement, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, WeakEntity, Window, rems,
};
use project::git_store::Repository;
use ui::{Divider, ListItem, ListItemSpacing, PopoverMenu, TintColor, Tooltip, prelude::*};

use crate::GitGraph;
use crate::GraphMode;
use crate::file_history::FileHistoryOptions;
use crate::filters::DateRange;
use branch_popover::BranchFilterPopover;
use path_popover::PathFilterPopover;
use user_popover::UserFilterPopover;

const POPOVER_WIDTH_REMS: f32 = 18.0;
const CUSTOM_DATE_PLACEHOLDER: &str = "YYYY-MM-DD";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DatePreset {
    Today,
    Yesterday,
    ThisWeek,
    Last30Days,
    AllTime,
}

impl DatePreset {
    fn label(self) -> &'static str {
        match self {
            DatePreset::Today => "Today",
            DatePreset::Yesterday => "Yesterday",
            DatePreset::ThisWeek => "This Week",
            DatePreset::Last30Days => "Last 30 days",
            DatePreset::AllTime => "All time",
        }
    }

    fn all() -> [DatePreset; 5] {
        [
            DatePreset::Today,
            DatePreset::Yesterday,
            DatePreset::ThisWeek,
            DatePreset::Last30Days,
            DatePreset::AllTime,
        ]
    }

    /// Resolve the preset against the current local clock. `None` means the
    /// preset clears the date filter (All time).
    fn resolve(self, now: chrono::DateTime<Local>) -> Option<DateRange> {
        match self {
            DatePreset::Today => start_of_local_day(now).map(DateRange::Since),
            DatePreset::Yesterday => {
                let yesterday = now.date_naive().pred_opt()?;
                let since = local_naive_at_midnight(yesterday)?;
                let until = local_naive_at_midnight(now.date_naive())?;
                Some(DateRange::Between { since, until })
            }
            DatePreset::ThisWeek => {
                let weekday_idx = now.weekday().num_days_from_monday() as i64;
                let monday = now
                    .date_naive()
                    .checked_sub_days(chrono::Days::new(u64::try_from(weekday_idx).unwrap_or(0)))?;
                local_naive_at_midnight(monday).map(DateRange::Since)
            }
            DatePreset::Last30Days => Some(DateRange::Since(now.timestamp() - 30 * 86_400)),
            DatePreset::AllTime => None,
        }
    }

    fn matches(self, range: Option<DateRange>, now: chrono::DateTime<Local>) -> bool {
        // Re-resolve against `now` and compare; this means the checkmark
        // *can* drift past midnight (a "Today" filter set last night reads
        // as "since yesterday's midnight" today), but follow-up commits
        // will replace this with a persisted active-preset marker.
        match (self.resolve(now), range) {
            (None, None) => true,
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }
}

fn start_of_local_day(now: chrono::DateTime<Local>) -> Option<i64> {
    local_naive_at_midnight(now.date_naive())
}

fn local_naive_at_midnight(date: NaiveDate) -> Option<i64> {
    let naive = date.and_hms_opt(0, 0, 0)?;
    Local
        .from_local_datetime(&naive)
        .single()
        .map(|dt| dt.timestamp())
}

fn parse_iso_date(input: &str) -> Option<i64> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let date = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").ok()?;
    local_naive_at_midnight(date)
}

fn date_chip_label(range: Option<DateRange>) -> SharedString {
    match range {
        None => SharedString::from("Date"),
        Some(DateRange::Since(unix)) => {
            let now = Local::now().timestamp();
            let elapsed_days = (now - unix).max(0) / 86_400;
            // Match common preset shapes back to their labels for the chip
            // surface; arbitrary `Since` (custom range that happens to omit
            // an end date) falls through to a generic "Since …" label.
            if elapsed_days <= 0 {
                SharedString::from("Date: Today")
            } else if elapsed_days <= 7 {
                SharedString::from("Date: This Week")
            } else if elapsed_days <= 30 {
                SharedString::from("Date: Last 30 days")
            } else if let Some(date) = chrono::DateTime::from_timestamp(unix, 0) {
                SharedString::from(format!(
                    "Date: Since {}",
                    date.with_timezone(&Local).format("%Y-%m-%d")
                ))
            } else {
                SharedString::from("Date: Since …")
            }
        }
        Some(DateRange::Until(unix)) => chrono::DateTime::from_timestamp(unix, 0)
            .map(|date| {
                SharedString::from(format!(
                    "Date: Until {}",
                    date.with_timezone(&Local).format("%Y-%m-%d")
                ))
            })
            .unwrap_or_else(|| SharedString::from("Date: Until …")),
        Some(DateRange::Between { since, until }) => {
            // A 1-day window anchored at "yesterday's" midnight — the
            // Yesterday preset.
            if until - since == 86_400 {
                let yesterday = chrono::DateTime::from_timestamp(since, 0)
                    .map(|d| d.with_timezone(&Local).date_naive());
                if let Some(date) = yesterday
                    && Some(date) == Local::now().date_naive().pred_opt()
                {
                    return SharedString::from("Date: Yesterday");
                }
            }
            let label = chrono::DateTime::from_timestamp(since, 0)
                .zip(chrono::DateTime::from_timestamp(until, 0))
                .map(|(a, b)| {
                    format!(
                        "{} – {}",
                        a.with_timezone(&Local).format("%Y-%m-%d"),
                        b.with_timezone(&Local).format("%Y-%m-%d"),
                    )
                });
            SharedString::from(format!(
                "Date: {}",
                label.as_deref().unwrap_or("Custom range"),
            ))
        }
    }
}

pub struct LogToolbar {
    weak_graph: WeakEntity<GitGraph>,
    date_range: Option<DateRange>,
    branches: Vec<SharedString>,
    authors: Vec<SharedString>,
    paths: Vec<RepoPath>,
    repository: Option<Entity<Repository>>,
    all_refs: bool,
    my_commits: bool,
    new_since_refresh: bool,
    compact_refs: bool,
    group_by_date: bool,
    mode: GraphMode,
    file_history_options: FileHistoryOptions,
    /// Optional leading element rendered at the start of the toolbar row,
    /// before the filter chips. The git-graph panel uses this to inline its
    /// "Search commits…" box into the toolbar row (IDEA-style).
    leading: Option<AnyElement>,
}

impl LogToolbar {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        weak_graph: WeakEntity<GitGraph>,
        date_range: Option<DateRange>,
        branches: Vec<SharedString>,
        authors: Vec<SharedString>,
        paths: Vec<RepoPath>,
        repository: Option<Entity<Repository>>,
        all_refs: bool,
        my_commits: bool,
        new_since_refresh: bool,
        compact_refs: bool,
        group_by_date: bool,
        mode: GraphMode,
        file_history_options: FileHistoryOptions,
    ) -> Self {
        Self {
            weak_graph,
            date_range,
            branches,
            authors,
            paths,
            repository,
            all_refs,
            my_commits,
            new_since_refresh,
            compact_refs,
            group_by_date,
            mode,
            file_history_options,
            leading: None,
        }
    }

    /// Prepend a leading element to the toolbar row (rendered before the
    /// filter chips, taking the remaining horizontal width). Used to inline
    /// the commit-search box into the log toolbar.
    pub fn with_leading(mut self, leading: impl IntoElement) -> Self {
        self.leading = Some(leading.into_any_element());
        self
    }

    pub fn render(mut self, cx: &mut App) -> impl IntoElement + use<> {
        let color = cx.theme().colors();
        let leading = self.leading.take();
        let branch_chip = self.render_branch_chip();
        let user_chip = self.render_user_chip();
        let path_chip = self.render_path_chip();
        let date_chip = self.render_date_chip();
        let toggles = self.render_toggles();
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(color.border_variant)
            .bg(color.toolbar_background)
            .when_some(leading, |this, leading| {
                this.child(div().flex_1().min_w_0().mr_1().child(leading))
            })
            .child(branch_chip)
            .child(user_chip)
            .child(path_chip)
            .child(date_chip)
            .child(div().px_1().child(Divider::vertical()))
            .child(toggles)
    }

    fn render_toggles(&self) -> impl IntoElement {
        // Mirrors IntelliJ's git log toolbar: icon-only IconButton toggles
        // sit to the right of the chip group. Each toggle uses a tinted
        // accent style when active and a subtle style when inactive.
        let weak = self.weak_graph.clone();
        let all_refs = self.all_refs;
        let branch_active = !self.branches.is_empty();
        let my_commits = self.my_commits;
        let new_since_refresh = self.new_since_refresh;
        let compact_refs = self.compact_refs;
        let group_by_date = self.group_by_date;
        let is_file_history = matches!(self.mode, GraphMode::FileHistory);
        let follow_renames = self.file_history_options.follow_renames;
        let with_local_changes = self.file_history_options.with_local_changes;
        let show_inline_diff = self.file_history_options.show_inline_diff;

        let toggle_style = |active: bool| {
            if active {
                ButtonStyle::Tinted(TintColor::Accent)
            } else {
                ButtonStyle::Subtle
            }
        };

        // --all toggle: precedence rule says explicit branch chip selection
        // wins, so the toggle is visually disabled while branches is non-
        // empty (LogFilters::to_git_args also drops `--all` in that case).
        let all_refs_button = {
            let weak = weak.clone();
            let tooltip_text: SharedString = if branch_active {
                "All refs (disabled while Branch filter is active)".into()
            } else if all_refs {
                "Showing all refs (--all). Click to disable.".into()
            } else {
                "Show all refs (--all)".into()
            };
            IconButton::new("git-graph-toggle-all-refs", IconName::GitBranch)
                .icon_size(IconSize::Small)
                .style(toggle_style(all_refs && !branch_active))
                .toggle_state(all_refs && !branch_active)
                .disabled(branch_active)
                .tooltip(Tooltip::text(tooltip_text))
                .on_click(move |_, _, cx| {
                    let weak = weak.clone();
                    if let Some(graph) = weak.upgrade() {
                        graph.update(cx, |graph, cx| {
                            graph.set_all_refs(!all_refs, cx);
                        });
                    }
                })
        };

        let my_commits_button = {
            let weak = weak.clone();
            IconButton::new("git-graph-toggle-my-commits", IconName::Person)
                .icon_size(IconSize::Small)
                .style(toggle_style(my_commits))
                .toggle_state(my_commits)
                .tooltip(Tooltip::text("Highlight my commits"))
                .on_click(move |_, _, cx| {
                    let weak = weak.clone();
                    if let Some(graph) = weak.upgrade() {
                        graph.update(cx, |graph, cx| {
                            graph.set_my_commits(!my_commits, cx);
                        });
                    }
                })
        };

        let new_since_refresh_button = {
            let weak = weak.clone();
            IconButton::new("git-graph-toggle-new-since-refresh", IconName::Sparkle)
                .icon_size(IconSize::Small)
                .style(toggle_style(new_since_refresh))
                .toggle_state(new_since_refresh)
                .tooltip(Tooltip::text("Highlight new commits since last refresh"))
                .on_click(move |_, _, cx| {
                    let weak = weak.clone();
                    if let Some(graph) = weak.upgrade() {
                        graph.update(cx, |graph, cx| {
                            graph.set_new_since_refresh(!new_since_refresh, cx);
                        });
                    }
                })
        };

        let compact_refs_button = {
            let weak = weak.clone();
            IconButton::new("git-graph-toggle-compact-refs", IconName::ListCollapse)
                .icon_size(IconSize::Small)
                .style(toggle_style(compact_refs))
                .toggle_state(compact_refs)
                .tooltip(Tooltip::text("Compact references"))
                .on_click(move |_, _, cx| {
                    let weak = weak.clone();
                    if let Some(graph) = weak.upgrade() {
                        graph.update(cx, |graph, cx| {
                            graph.set_compact_refs(!compact_refs, cx);
                        });
                    }
                })
        };

        let group_by_date_button = {
            let weak = weak.clone();
            IconButton::new("git-graph-toggle-group-by-date", IconName::Clock)
                .icon_size(IconSize::Small)
                .style(toggle_style(group_by_date))
                .toggle_state(group_by_date)
                .tooltip(Tooltip::text("Group by date"))
                .on_click(move |_, _, cx| {
                    let weak = weak.clone();
                    if let Some(graph) = weak.upgrade() {
                        graph.update(cx, |graph, cx| {
                            graph.set_group_by_date(!group_by_date, cx);
                        });
                    }
                })
        };

        // File-history mode toggles. Built up-front so the conditional
        // `.when` block below can move them by value.
        let follow_renames_button = {
            let weak = weak.clone();
            IconButton::new("git-graph-toggle-follow-renames", IconName::ArrowRightLeft)
                .icon_size(IconSize::Small)
                .style(toggle_style(follow_renames))
                .toggle_state(follow_renames)
                .tooltip(Tooltip::text("Follow file across renames (--follow)"))
                .on_click(move |_, _, cx| {
                    let weak = weak.clone();
                    if let Some(graph) = weak.upgrade() {
                        graph.update(cx, |graph, cx| {
                            graph.set_follow_renames(!follow_renames, cx);
                        });
                    }
                })
        };

        let with_local_changes_button = {
            let weak = weak.clone();
            IconButton::new("git-graph-toggle-with-local-changes", IconName::Pencil)
                .icon_size(IconSize::Small)
                .style(toggle_style(with_local_changes))
                .toggle_state(with_local_changes)
                .tooltip(Tooltip::text("Show uncommitted changes as a synthetic row"))
                .on_click(move |_, _, cx| {
                    let weak = weak.clone();
                    if let Some(graph) = weak.upgrade() {
                        graph.update(cx, |graph, cx| {
                            graph.set_with_local_changes(!with_local_changes, cx);
                        });
                    }
                })
        };

        // Show Inline Diff is wired up to persist toggle state but the
        // per-row hunk rendering is deferred — the v1 surface is the toggle
        // only, with a "coming soon" tooltip.
        let show_inline_diff_button =
            IconButton::new("git-graph-toggle-show-inline-diff", IconName::ListTree)
                .icon_size(IconSize::Small)
                .style(toggle_style(show_inline_diff))
                .toggle_state(show_inline_diff)
                .tooltip(Tooltip::text("Show inline diff per row (coming soon)"))
                .on_click(move |_, _, cx| {
                    let weak = weak.clone();
                    if let Some(graph) = weak.upgrade() {
                        graph.update(cx, |graph, cx| {
                            graph.set_show_inline_diff(!show_inline_diff, cx);
                        });
                    }
                });

        h_flex()
            .gap_0p5()
            .child(all_refs_button)
            .child(my_commits_button)
            .child(new_since_refresh_button)
            .child(compact_refs_button)
            .child(group_by_date_button)
            .when(is_file_history, |this| {
                this.child(div().px_1().child(Divider::vertical()))
                    .child(follow_renames_button)
                    .child(with_local_changes_button)
                    .child(show_inline_diff_button)
            })
    }

    fn render_date_chip(&self) -> impl IntoElement {
        let active = self.date_range.is_some();
        let label = date_chip_label(self.date_range);
        let weak = self.weak_graph.clone();
        let initial_range = self.date_range;

        let trigger = Button::new("git-graph-filter-date", label)
            .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
            .label_size(LabelSize::Small)
            .color(if active { Color::Default } else { Color::Muted })
            .style(if active {
                ButtonStyle::Tinted(TintColor::Accent)
            } else {
                ButtonStyle::Subtle
            });

        PopoverMenu::new("git-graph-filter-date-popover")
            .trigger(trigger)
            .menu(move |window, cx| {
                let weak = weak.clone();
                Some(cx.new(|cx| DateFilterPopover::new(weak, initial_range, window, cx)))
            })
            .anchor(gpui::Anchor::TopLeft)
            .attach(gpui::Anchor::BottomLeft)
    }

    fn render_branch_chip(&self) -> impl IntoElement {
        let active = !self.branches.is_empty();
        let label = branch_chip_label(&self.branches);
        let weak = self.weak_graph.clone();
        let initial = self.branches.clone();
        let repository = self.repository.clone();

        let trigger = Button::new("git-graph-filter-branch", label)
            .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
            .label_size(LabelSize::Small)
            .color(if active { Color::Default } else { Color::Muted })
            .style(if active {
                ButtonStyle::Tinted(TintColor::Accent)
            } else {
                ButtonStyle::Subtle
            });

        PopoverMenu::new("git-graph-filter-branch-popover")
            .trigger(trigger)
            .menu(move |window, cx| {
                let weak = weak.clone();
                let initial = initial.clone();
                let repository = repository.clone();
                Some(cx.new(|cx| BranchFilterPopover::new(weak, repository, initial, window, cx)))
            })
            .anchor(gpui::Anchor::TopLeft)
            .attach(gpui::Anchor::BottomLeft)
    }

    fn render_user_chip(&self) -> impl IntoElement {
        let active = !self.authors.is_empty();
        let label = user_chip_label(&self.authors);
        let weak = self.weak_graph.clone();
        let initial = self.authors.clone();
        let repository = self.repository.clone();

        let trigger = Button::new("git-graph-filter-user", label)
            .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
            .label_size(LabelSize::Small)
            .color(if active { Color::Default } else { Color::Muted })
            .style(if active {
                ButtonStyle::Tinted(TintColor::Accent)
            } else {
                ButtonStyle::Subtle
            });

        PopoverMenu::new("git-graph-filter-user-popover")
            .trigger(trigger)
            .menu(move |window, cx| {
                let weak = weak.clone();
                let initial = initial.clone();
                let repository = repository.clone();
                Some(cx.new(|cx| UserFilterPopover::new(weak, repository, initial, window, cx)))
            })
            .anchor(gpui::Anchor::TopLeft)
            .attach(gpui::Anchor::BottomLeft)
    }

    fn render_path_chip(&self) -> impl IntoElement {
        let active = !self.paths.is_empty();
        let label = path_chip_label(&self.paths);
        let weak = self.weak_graph.clone();
        let initial = self.paths.clone();
        let repository = self.repository.clone();

        let trigger = Button::new("git-graph-filter-path", label)
            .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
            .label_size(LabelSize::Small)
            .color(if active { Color::Default } else { Color::Muted })
            .style(if active {
                ButtonStyle::Tinted(TintColor::Accent)
            } else {
                ButtonStyle::Subtle
            });

        PopoverMenu::new("git-graph-filter-path-popover")
            .trigger(trigger)
            .menu(move |window, cx| {
                let weak = weak.clone();
                let initial = initial.clone();
                let repository = repository.clone();
                Some(cx.new(|cx| PathFilterPopover::new(weak, repository, initial, window, cx)))
            })
            .anchor(gpui::Anchor::TopLeft)
            .attach(gpui::Anchor::BottomLeft)
    }
}

fn path_chip_label(paths: &[RepoPath]) -> SharedString {
    match paths.len() {
        0 => SharedString::from("Path"),
        1 => SharedString::from(format!("Path: {}", path_display_segment(&paths[0]))),
        n => {
            let first = path_display_segment(&paths[0]);
            SharedString::from(format!("Path: {first}, +{}", n - 1))
        }
    }
}

/// Shorten a repo-relative path to its trailing segment so the chip stays
/// narrow. `crates/foo/bar.rs` → `bar.rs`; a directory like `crates/foo`
/// → `foo`.
fn path_display_segment(path: &RepoPath) -> String {
    let unix = path.as_unix_str();
    unix.rsplit('/').next().unwrap_or(unix).to_string()
}

fn user_chip_label(authors: &[SharedString]) -> SharedString {
    match authors.len() {
        0 => SharedString::from("User"),
        1 => {
            let display = author_display_name(&authors[0]);
            SharedString::from(format!("User: {display}"))
        }
        n => {
            let first = author_display_name(&authors[0]);
            SharedString::from(format!("User: {first}, +{}", n - 1))
        }
    }
}

/// Selection is keyed by email; the chip surface shows just the local
/// part (`alice` for `alice@example.com`) so the chip stays narrow.
/// Falls back to the raw value when there's no `@`.
fn author_display_name(email: &SharedString) -> String {
    email
        .split_once('@')
        .map(|(local, _)| local.to_string())
        .unwrap_or_else(|| email.to_string())
}

fn branch_chip_label(branches: &[SharedString]) -> SharedString {
    match branches.len() {
        0 => SharedString::from("Branch"),
        1 => {
            let display = branch_display_name(&branches[0]);
            SharedString::from(format!("Branch: {display}"))
        }
        n => {
            let first = branch_display_name(&branches[0]);
            SharedString::from(format!("Branch: {first}, +{}", n - 1))
        }
    }
}

fn branch_display_name(ref_name: &SharedString) -> String {
    ref_name
        .strip_prefix("refs/heads/")
        .or_else(|| ref_name.strip_prefix("refs/remotes/"))
        .unwrap_or(ref_name.as_ref())
        .to_string()
}

#[derive(Clone, Copy)]
enum PopoverMode {
    Presets,
    Custom,
}

pub struct DateFilterPopover {
    weak_graph: WeakEntity<GitGraph>,
    mode: PopoverMode,
    active_range: Option<DateRange>,
    custom_since: Entity<Editor>,
    custom_until: Entity<Editor>,
    custom_error: Option<SharedString>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl DateFilterPopover {
    fn new(
        weak_graph: WeakEntity<GitGraph>,
        active_range: Option<DateRange>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let custom_since = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(CUSTOM_DATE_PLACEHOLDER, window, cx);
            if let Some(DateRange::Between { since, .. }) = active_range
                && let Some(date) = chrono::DateTime::from_timestamp(since, 0)
            {
                editor.set_text(
                    date.with_timezone(&Local).format("%Y-%m-%d").to_string(),
                    window,
                    cx,
                );
            }
            editor
        });
        let custom_until = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(CUSTOM_DATE_PLACEHOLDER, window, cx);
            if let Some(DateRange::Between { until, .. }) = active_range
                && let Some(date) = chrono::DateTime::from_timestamp(until, 0)
            {
                editor.set_text(
                    date.with_timezone(&Local).format("%Y-%m-%d").to_string(),
                    window,
                    cx,
                );
            }
            editor
        });

        let clear_error = |this: &mut DateFilterPopover,
                           _,
                           event: &editor::EditorEvent,
                           cx: &mut Context<DateFilterPopover>| {
            if matches!(
                event,
                editor::EditorEvent::BufferEdited | editor::EditorEvent::Edited { .. }
            ) && this.custom_error.is_some()
            {
                this.custom_error = None;
                cx.notify();
            }
        };
        let subscriptions = vec![
            cx.subscribe(&custom_since, clear_error),
            cx.subscribe(&custom_until, clear_error),
        ];

        let focus_handle = cx.focus_handle();
        Self {
            weak_graph,
            mode: PopoverMode::Presets,
            active_range,
            custom_since,
            custom_until,
            custom_error: None,
            focus_handle,
            _subscriptions: subscriptions,
        }
    }

    fn apply_preset(&mut self, preset: DatePreset, cx: &mut Context<Self>) {
        let resolved = preset.resolve(Local::now());
        self.commit_range(resolved, cx);
    }

    fn apply_custom(&mut self, cx: &mut Context<Self>) {
        let since_text = self.custom_since.read(cx).text(cx);
        let until_text = self.custom_until.read(cx).text(cx);
        let since = parse_iso_date(&since_text);
        let until = parse_iso_date(&until_text);
        let range = match (since, until) {
            (Some(s), Some(u)) if u >= s => DateRange::Between { since: s, until: u },
            (Some(_), Some(_)) => {
                self.custom_error = Some(SharedString::from("End date is before start date"));
                cx.notify();
                return;
            }
            (Some(s), None) => DateRange::Since(s),
            (None, Some(u)) => DateRange::Until(u),
            (None, None) => {
                self.custom_error =
                    Some(SharedString::from("Enter at least one date as YYYY-MM-DD"));
                cx.notify();
                return;
            }
        };
        self.commit_range(Some(range), cx);
    }

    fn commit_range(&mut self, range: Option<DateRange>, cx: &mut Context<Self>) {
        let weak = self.weak_graph.clone();
        if let Some(graph) = weak.upgrade() {
            graph.update(cx, |graph, cx| {
                graph.set_date_filter(range, cx);
            });
        }
        cx.emit(DismissEvent);
    }

    fn switch_to_custom(&mut self, cx: &mut Context<Self>) {
        self.mode = PopoverMode::Custom;
        self.custom_error = None;
        cx.notify();
    }

    fn cancel_custom(&mut self, cx: &mut Context<Self>) {
        self.mode = PopoverMode::Presets;
        self.custom_error = None;
        cx.notify();
    }

    fn render_preset_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Local::now();
        let mut list = v_flex().gap_0p5();
        for preset in DatePreset::all() {
            let is_active = preset.matches(self.active_range, now);
            let row = ListItem::new(SharedString::from(format!(
                "git-graph-date-preset-{:?}",
                preset
            )))
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(is_active)
            .start_slot(if is_active {
                Icon::new(IconName::Check)
                    .color(Color::Accent)
                    .size(IconSize::Small)
                    .into_any_element()
            } else {
                gpui::Empty.into_any_element()
            })
            .child(Label::new(preset.label()))
            .on_click(cx.listener(move |this, _, _, cx| this.apply_preset(preset, cx)));
            list = list.child(row);
        }
        list = list.child(Divider::horizontal()).child(
            ListItem::new("git-graph-date-custom")
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .child(Label::new("Custom range…").color(Color::Accent))
                .on_click(cx.listener(|this, _, _, cx| this.switch_to_custom(cx))),
        );
        list
    }

    fn render_custom_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_box = |label: &'static str, editor: Entity<Editor>| {
            v_flex()
                .gap_0p5()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                .child(
                    h_flex()
                        .h_8()
                        .px_2()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .rounded_md()
                        .bg(cx.theme().colors().editor_background)
                        .child(editor),
                )
        };

        v_flex()
            .gap_2()
            .child(editor_box("From", self.custom_since.clone()))
            .child(editor_box("To", self.custom_until.clone()))
            .when_some(self.custom_error.clone(), |this, error| {
                this.child(Label::new(error).color(Color::Error).size(LabelSize::Small))
            })
            .child(
                h_flex()
                    .gap_1()
                    .justify_end()
                    .child(
                        Button::new("git-graph-date-custom-cancel", "Cancel")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_custom(cx))),
                    )
                    .child(
                        Button::new("git-graph-date-custom-ok", "OK")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, _, cx| this.apply_custom(cx))),
                    ),
            )
    }
}

impl EventEmitter<DismissEvent> for DateFilterPopover {}

impl Focusable for DateFilterPopover {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DateFilterPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: gpui::AnyElement = match self.mode {
            PopoverMode::Presets => self.render_preset_list(cx).into_any_element(),
            PopoverMode::Custom => self.render_custom_form(cx).into_any_element(),
        };

        v_flex()
            .key_context("GitGraphDateFilterPopover")
            .track_focus(&self.focus_handle)
            .w(rems(POPOVER_WIDTH_REMS))
            .p_2()
            .gap_1()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(body)
    }
}
