use collections::HashMap;
use editor::{
    Editor, EditorEvent, EditorSettings, HighlightKey, SelectionEffects, scroll::Autoscroll,
};
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Render, ScrollStrategy, StatefulInteractiveElement, Styled,
    Subscription, Task, UniformListScrollHandle, Window, actions, uniform_list,
};
use language::{Anchor, Buffer, BufferSnapshot, Point};
use project::{
    Project, SearchResults,
    search::{SearchQuery, SearchResult},
};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::Settings as _;
use std::{
    ops::Range,
    path::{Path, PathBuf},
    pin::pin,
    sync::Arc,
    time::Duration,
};
use ui::prelude::*;
use util::paths::PathMatcher;
use workspace::{ModalView, Workspace};

use crate::{
    SearchOption, SearchOptions, SearchSource, ToggleCaseSensitive, ToggleRegex, ToggleWholeWord,
    search_bar,
};

#[cfg(test)]
#[path = "find_in_path_tests.rs"]
mod find_in_path_tests;

/// Opens the Find in Path modal (project-wide search overlay).
#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Default, gpui::Action)]
#[action(namespace = find_in_path)]
#[serde(deny_unknown_fields)]
pub struct Toggle {
    #[serde(default)]
    pub replace_enabled: bool,
}

actions!(
    find_in_path,
    [
        /// Opens the Find in Path modal with the replace field revealed.
        ToggleReplace
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(register).detach();
}

fn register(workspace: &mut Workspace, _window: Option<&mut Window>, _cx: &mut Context<Workspace>) {
    workspace.register_action(|workspace, action: &Toggle, window, cx| {
        FindInPath::toggle(workspace, action.replace_enabled, window, cx);
    });
    workspace.register_action(|workspace, _: &ToggleReplace, window, cx| {
        FindInPath::toggle(workspace, true, window, cx);
    });
}

/// What part of the Solution a Find in Path search is restricted to.
#[derive(Clone, Debug, PartialEq)]
pub enum Scope {
    /// No restriction — search every visible worktree.
    Solution,
    /// Restrict to the active member's worktree (`member_root`, resolved by the caller).
    Project,
    /// Restrict to one directory (and everything below it).
    Directory(PathBuf),
}

/// Resolve the active member's root path for the Solution that owns `workspace`'s project, if any.
///
/// Called from `FindInPath::toggle`, which stores the result on `FindInPath` for `Scope::Project`
/// to consume via `include_patterns_for_scope`'s `member_root`.
fn active_member_root(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    // `try_global`, not `global` — this runs for every `FindInPath::toggle`, including plain
    // (non-Solution) workspaces and tests that never call `solutions::init`, where the
    // `SolutionStore` global is never installed at all.
    let store = solutions::SolutionStore::try_global(cx)?;
    let project = workspace.project().read(cx);
    let first_root = project.visible_worktrees(cx).next()?.read(cx).abs_path();
    let solution = store.read(cx).solution_for_path(&first_root)?;
    store.read(cx).active_member_path(solution.id)
}

/// Build include globs restricting a search to `scope`. Empty ⇒ whole Solution.
///
/// A Solution is one `project::Project` with each member mounted as a separate
/// worktree, so `In Project` / `Directory` restrictions are expressed as
/// worktree-relative (or, when the project has multiple visible worktrees,
/// root-name-prefixed) recursive globs rather than as a different project.
fn include_patterns_for_scope(
    scope: &Scope,
    member_root: Option<&Path>,
    project: &Entity<Project>,
    cx: &App,
) -> Vec<String> {
    let project = project.read(cx);
    let match_full_paths = project.visible_worktrees(cx).count() > 1;
    let root_glob = |abs: &Path| -> Option<String> {
        for worktree in project.visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            let worktree_abs_path = worktree.abs_path();
            let Ok(relative) = abs.strip_prefix(&*worktree_abs_path) else {
                continue;
            };
            let mut glob = if match_full_paths {
                format!("{}/", worktree.root_name_str())
            } else {
                String::new()
            };
            if relative.as_os_str().is_empty() {
                glob.push_str("**");
            } else {
                glob.push_str(&relative.to_string_lossy());
                glob.push_str("/**");
            }
            return Some(glob);
        }
        None
    };

    match scope {
        Scope::Solution => Vec::new(),
        Scope::Project => {
            let owned_root = match member_root {
                Some(root) => Some(root.to_path_buf()),
                None => project
                    .visible_worktrees(cx)
                    .next()
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf()),
            };
            owned_root
                .as_deref()
                .and_then(root_glob)
                .into_iter()
                .collect()
        }
        Scope::Directory(dir) => root_glob(dir).into_iter().collect(),
    }
}

/// Split a comma-separated glob list into individual pattern strings, respecting `{...}` brace groups.
///
/// Copied from `project_search::split_glob_patterns` (kept private to each module — sharing it
/// would mean threading a new public export through `project_search` for one helper function).
fn split_glob_patterns(text: &str) -> Vec<&str> {
    let mut patterns = Vec::new();
    let mut pattern_start = 0;
    let mut brace_depth: usize = 0;
    let mut escaped = false;

    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            ',' if brace_depth == 0 => {
                patterns.push(&text[pattern_start..index]);
                pattern_start = index + 1;
            }
            _ => {}
        }
    }
    patterns.push(&text[pattern_start..]);
    patterns
}

fn parse_glob_patterns(text: &str) -> Vec<String> {
    split_glob_patterns(text)
        .into_iter()
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Build a `SearchQuery` from raw editor text plus a `Scope` restriction.
///
/// `scope`'s include patterns are merged in front of `include_text`'s user-typed patterns so an
/// empty `Scope::Solution` leaves the user's own include filter untouched. Returns `None` when the
/// query text is empty or when either glob list fails to parse.
fn build_query(
    query_text: &str,
    options: SearchOptions,
    include_text: &str,
    exclude_text: &str,
    scope: &Scope,
    member_root: Option<&Path>,
    project: &Entity<Project>,
    cx: &App,
) -> Option<SearchQuery> {
    if query_text.is_empty() {
        return None;
    }

    let path_style = project.read(cx).path_style(cx);

    let mut include_patterns = include_patterns_for_scope(scope, member_root, project, cx);
    include_patterns.extend(parse_glob_patterns(include_text));
    let included_files = PathMatcher::new(&include_patterns, path_style).ok()?;

    let exclude_patterns = parse_glob_patterns(exclude_text);
    let excluded_files = PathMatcher::new(&exclude_patterns, path_style).ok()?;

    let match_full_paths = project.read(cx).visible_worktrees(cx).count() > 1;

    let query = if options.contains(SearchOptions::REGEX) {
        SearchQuery::regex(
            query_text,
            options.contains(SearchOptions::WHOLE_WORD),
            options.contains(SearchOptions::CASE_SENSITIVE),
            options.contains(SearchOptions::INCLUDE_IGNORED),
            options.contains(SearchOptions::ONE_MATCH_PER_LINE),
            included_files,
            excluded_files,
            match_full_paths,
            None,
        )
        .ok()?
    } else {
        SearchQuery::text(
            query_text,
            options.contains(SearchOptions::WHOLE_WORD),
            options.contains(SearchOptions::CASE_SENSITIVE),
            options.contains(SearchOptions::INCLUDE_IGNORED),
            included_files,
            excluded_files,
            match_full_paths,
            None,
        )
        .ok()?
    };

    if query.is_empty() { None } else { Some(query) }
}

/// A single search match, resolved to a snapshot-relative line and a trimmed preview snippet.
pub struct MatchRow {
    pub range: Range<Anchor>,
    pub line: u32,
    pub snippet: SharedString,
}

/// All matches found in one buffer.
pub struct FileGroup {
    pub path: Arc<Path>,
    pub buffer: Entity<Buffer>,
    pub matches: Vec<MatchRow>,
}

/// One row of the flattened (group header / match) result list, as consumed by the results view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Header(usize),
    Match(usize, usize),
}

/// Compute the line + trimmed snippet preview for each range in `ranges`.
///
/// Pure and `cx`-free so it can run on a background thread — `BufferSnapshot` is `Send`, unlike the
/// `Entity<Buffer>`/`cx` pair needed to obtain it. Callers grab the snapshot on the foreground and
/// offload this (the actual per-range work) to `cx.background_executor()`.
fn compute_match_rows(snapshot: &BufferSnapshot, ranges: &[Range<Anchor>]) -> Vec<MatchRow> {
    ranges
        .iter()
        .map(|range| {
            let line = snapshot.summary_for_anchor::<Point>(&range.start).row;
            let line_end = snapshot.line_len(line);
            let snippet = snapshot
                .text_for_range(Point::new(line, 0)..Point::new(line, line_end))
                .collect::<String>();
            MatchRow {
                range: range.clone(),
                line,
                snippet: snippet.trim().to_string().into(),
            }
        })
        .collect()
}

/// Streaming search results grouped by file, plus the flattened row list the results view renders.
#[derive(Default)]
pub struct MatchList {
    pub groups: Vec<FileGroup>,
    pub rows: Vec<Row>,
    index_by_buffer: HashMap<EntityId, usize>,
}

impl MatchList {
    /// Merge a batch of already-computed `rows` for `buffer` into the existing group for that
    /// buffer (creating one, using `path`, if this is the first batch seen for it). `cx`-free and
    /// does no snippet computation — callers produce `rows` via `compute_match_rows`, typically on
    /// a background thread. Does not touch `rows` (the flattened list) — call `rebuild_rows` after.
    pub fn push_matches(&mut self, buffer: Entity<Buffer>, path: Arc<Path>, rows: Vec<MatchRow>) {
        if rows.is_empty() {
            return;
        }

        if let Some(&group_index) = self.index_by_buffer.get(&buffer.entity_id()) {
            self.groups[group_index].matches.extend(rows);
        } else {
            let group_index = self.groups.len();
            self.index_by_buffer.insert(buffer.entity_id(), group_index);
            self.groups.push(FileGroup {
                path,
                buffer,
                matches: rows,
            });
        }
    }

    /// Convenience wrapper around `compute_match_rows` + `push_matches` for callers (tests, and any
    /// non-streaming caller) that don't need to offload snippet computation to a background thread.
    /// The streaming `spawn_search` path does NOT use this — it offloads `compute_match_rows` itself.
    #[cfg(test)]
    pub fn push_result(&mut self, buffer: Entity<Buffer>, ranges: Vec<Range<Anchor>>, cx: &App) {
        if ranges.is_empty() {
            return;
        }
        let snapshot = buffer.read(cx).snapshot();
        let path: Arc<Path> = snapshot
            .file()
            .map(|file| Arc::from(file.full_path(cx)))
            .unwrap_or_else(|| Arc::from(Path::new("")));
        let rows = compute_match_rows(&snapshot, &ranges);
        self.push_matches(buffer, path, rows);
    }

    /// Flatten `groups` into `rows` (one `Header` per group followed by its `Match` rows).
    pub fn rebuild_rows(&mut self) {
        self.rows.clear();
        for (group_index, group) in self.groups.iter().enumerate() {
            self.rows.push(Row::Header(group_index));
            for match_index in 0..group.matches.len() {
                self.rows.push(Row::Match(group_index, match_index));
            }
        }
    }

    pub fn total_matches(&self) -> usize {
        self.groups.iter().map(|group| group.matches.len()).sum()
    }

    pub fn file_count(&self) -> usize {
        self.groups.len()
    }
}

/// Status line for the in-progress / completed search, folded from the `SearchResult` stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SearchStatus {
    #[default]
    Idle,
    Searching,
    Done,
    LimitReached,
}

pub struct FindInPath {
    focus_handle: FocusHandle,
    replace_enabled: bool,
    project: Entity<Project>,
    query_editor: Entity<Editor>,
    search_options: SearchOptions,
    scope: Scope,
    member_root: Option<PathBuf>,
    results: MatchList,
    status: SearchStatus,
    search_task: Option<Task<()>>,
    selected_row: usize,
    list_scroll_handle: UniformListScrollHandle,
    /// The editor previewing the currently-selected match, keyed by the previewed buffer's
    /// `EntityId` so `update_preview` can tell whether it needs to rebuild (selection moved to a
    /// different file) or can just re-highlight + re-autoscroll (selection moved within the same
    /// file).
    preview_editor: Option<(EntityId, Entity<Editor>)>,
    /// Set instead of calling `update_preview` directly from the streaming-search batch handler,
    /// which runs in `spawn_search`'s async task and has no `&mut Window`. Consumed (and cleared)
    /// at the top of `render`, which does have one.
    preview_dirty: bool,
    /// Identity of the match currently shown in `preview_editor` (previewed buffer's `EntityId` +
    /// the match's anchor range), so `update_preview` can no-op when the selection is re-resolved
    /// to the same match it already displays — e.g. a streaming batch that runs `clamp_selection`
    /// without moving `selected_row`. Without this, every batch would re-highlight and
    /// re-autoscroll the preview even though nothing the user is looking at changed.
    previewed_match: Option<(EntityId, Range<Anchor>)>,
    _subscriptions: Vec<Subscription>,
}

impl FindInPath {
    fn toggle(
        workspace: &mut Workspace,
        replace_enabled: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(existing) = workspace.active_modal::<Self>(cx) {
            existing.update(cx, |this, cx| {
                this.replace_enabled |= replace_enabled;
                this.query_editor.focus_handle(cx).focus(window, cx);
                cx.notify();
            });
            return;
        }
        let project = workspace.project().clone();
        let member_root = active_member_root(workspace, cx);
        workspace.toggle_modal(window, cx, |window, cx| {
            let query_editor = cx.new(|cx| Editor::single_line(window, cx));
            // Re-run the search on every query edit, mirroring `ProjectSearchView`'s
            // `cx.subscribe(&query_editor, ...)` in `project_search.rs`.
            let query_editor_subscription = cx.subscribe(
                &query_editor,
                |this: &mut Self, _, event: &EditorEvent, cx| {
                    if let EditorEvent::Edited { .. } = event {
                        this.update_search(cx);
                    }
                },
            );
            Self {
                focus_handle: cx.focus_handle(),
                replace_enabled,
                project,
                query_editor,
                search_options: SearchOptions::from_settings(
                    &EditorSettings::get_global(cx).search,
                ),
                scope: Scope::Solution,
                member_root,
                results: MatchList::default(),
                status: SearchStatus::Idle,
                search_task: None,
                selected_row: 0,
                list_scroll_handle: UniformListScrollHandle::new(),
                preview_editor: None,
                preview_dirty: false,
                previewed_match: None,
                _subscriptions: vec![query_editor_subscription],
            }
        });
    }

    /// Rebuild the `SearchQuery` from the current editor/option/scope state and (re)run it, or
    /// clear the results when the query text is empty / fails to parse (`build_query` returns
    /// `None`). Include/exclude masks are wired in Task 7; empty text means "no restriction" here.
    fn update_search(&mut self, cx: &mut Context<Self>) {
        let query_text = self.query_editor.read(cx).text(cx);
        if let Some(query) = build_query(
            &query_text,
            self.search_options,
            "",
            "",
            &self.scope,
            self.member_root.as_deref(),
            &self.project,
            cx,
        ) {
            self.spawn_search(query, cx);
        } else {
            self.results = MatchList::default();
            self.selected_row = 0;
            self.status = SearchStatus::Idle;
            self.search_task = None;
            self.preview_editor = None;
            self.preview_dirty = false;
            self.previewed_match = None;
            cx.notify();
        }
    }

    fn toggle_case_sensitive(
        &mut self,
        _: &ToggleCaseSensitive,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_options.toggle(SearchOptions::CASE_SENSITIVE);
        self.update_search(cx);
    }

    fn toggle_whole_word(
        &mut self,
        _: &ToggleWholeWord,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_options.toggle(SearchOptions::WHOLE_WORD);
        self.update_search(cx);
    }

    fn toggle_regex(&mut self, _: &ToggleRegex, _window: &mut Window, cx: &mut Context<Self>) {
        self.search_options.toggle(SearchOptions::REGEX);
        self.update_search(cx);
    }

    /// Short summary of `self.results`/`self.status` for the modal's status bar.
    fn status_label(&self) -> SharedString {
        match self.status {
            SearchStatus::Idle if self.results.file_count() == 0 => "Type to search".into(),
            SearchStatus::Searching => "Searching…".into(),
            _ => {
                let matches = self.results.total_matches();
                if matches == 0 {
                    "No results".into()
                } else {
                    let files = self.results.file_count();
                    let suffix = if self.status == SearchStatus::LimitReached {
                        " (limit reached)"
                    } else {
                        ""
                    };
                    format!("{matches} matches in {files} files{suffix}").into()
                }
            }
        }
    }

    /// Debounce, then run `query` against `self.project` and stream results into `self.results`.
    ///
    /// Replacing `self.search_task` drops (and thus cancels) any in-flight search — including one
    /// still sitting in the debounce timer — before this one starts.
    fn spawn_search(&mut self, query: SearchQuery, cx: &mut Context<Self>) {
        let project = self.project.clone();
        self.results = MatchList::default();
        self.selected_row = 0;
        self.status = SearchStatus::Searching;
        self.preview_editor = None;
        self.preview_dirty = false;
        self.previewed_match = None;
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;

            let SearchResults { rx, _task_handle } =
                project.update(cx, |project, cx| project.search(query, cx));
            let mut chunks = pin!(rx.ready_chunks(1024));

            let mut limit_reached = false;
            while let Some(batch) = chunks.next().await {
                let mut buffers_with_ranges = Vec::with_capacity(batch.len());
                for result in batch {
                    match result {
                        SearchResult::Buffer { buffer, ranges } => {
                            buffers_with_ranges.push((buffer, ranges));
                        }
                        SearchResult::LimitReached => limit_reached = true,
                        SearchResult::WaitingForScan | SearchResult::Searching => {}
                    }
                }

                // Snippet computation is offloaded per buffer (not batched as one background job)
                // so no single buffer's snippet extraction blocks the others. `rebuild_rows` is
                // O(total-accumulated-rows), so it (and the foreground yield) happen once per
                // drained batch rather than once per buffer — with up to `MAX_SEARCH_RESULT_FILES`
                // buffers per search, a per-buffer rebuild would be quadratic in the result count.
                for (buffer, ranges) in buffers_with_ranges {
                    let (snapshot, path) = buffer.read_with(cx, |buffer, cx| {
                        let snapshot = buffer.snapshot();
                        let path: Arc<Path> = snapshot
                            .file()
                            .map(|file| Arc::from(file.full_path(cx)))
                            .unwrap_or_else(|| Arc::from(Path::new("")));
                        (snapshot, path)
                    });
                    let rows = cx
                        .background_executor()
                        .spawn(async move { compute_match_rows(&snapshot, &ranges) })
                        .await;

                    let update_result = this.update(cx, |this, _cx| {
                        this.results.push_matches(buffer, path, rows);
                    });
                    if update_result.is_err() {
                        return;
                    }
                }

                let update_result = this.update(cx, |this, cx| {
                    this.results.rebuild_rows();
                    this.status = SearchStatus::Searching;
                    // Only mark the preview dirty when this batch actually moved the selection
                    // (e.g. the first batch landing on the first match). A later batch that
                    // `clamp_selection` leaves untouched must not re-trigger `update_preview` —
                    // that would snap the preview's scroll back to center on every streamed
                    // batch, fighting a user who has manually scrolled the preview pane.
                    if this.clamp_selection() {
                        this.preview_dirty = true;
                    }
                    cx.notify();
                });
                if update_result.is_err() {
                    return;
                }
                futures_lite::future::yield_now().await;
            }

            this.update(cx, |this, cx| {
                this.status = if limit_reached {
                    SearchStatus::LimitReached
                } else {
                    SearchStatus::Done
                };
                this.search_task = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Index of the first `Row::Match` in `rows`, or `None` if `rows` has no matches (e.g. it's
    /// empty, or every group somehow ended up with zero matches).
    fn first_match_row(rows: &[Row]) -> Option<usize> {
        rows.iter().position(|row| matches!(row, Row::Match(_, _)))
    }

    fn last_match_row(rows: &[Row]) -> Option<usize> {
        rows.iter().rposition(|row| matches!(row, Row::Match(_, _)))
    }

    /// Called after every `rebuild_rows()` (streaming search grows `rows` batch by batch) so
    /// `selected_row` always lands on a `Row::Match`, never a `Row::Header` or an index past the
    /// end. Leaves an already-valid `Match` selection untouched, so incoming batches don't yank
    /// the highlight away from a row the user is looking at. Returns whether `selected_row`
    /// actually changed, so callers (the streaming batch handler) can tell a genuine
    /// first-match-lands-in-view transition from a no-op reclamp.
    fn clamp_selection(&mut self) -> bool {
        let previous = self.selected_row;
        let rows = &self.results.rows;
        if rows.is_empty() {
            self.selected_row = 0;
        } else if self.selected_row >= rows.len()
            || !matches!(rows[self.selected_row], Row::Match(_, _))
        {
            self.selected_row = Self::first_match_row(rows).unwrap_or(0);
        }
        self.selected_row != previous
    }

    /// Move `selected_row` to the next/previous `Row::Match`, skipping over `Row::Header` rows,
    /// clamping (not wrapping) at the ends. Scrolls the list to keep the new selection visible.
    fn move_selection(&mut self, direction: isize, window: &mut Window, cx: &mut Context<Self>) {
        let rows = &self.results.rows;
        if rows.is_empty() {
            return;
        }
        let mut ix = self.selected_row.min(rows.len() - 1) as isize;
        loop {
            ix += direction;
            if ix < 0 || ix >= rows.len() as isize {
                return;
            }
            if matches!(rows[ix as usize], Row::Match(_, _)) {
                break;
            }
        }
        self.selected_row = ix as usize;
        self.list_scroll_handle
            .scroll_to_item(self.selected_row, ScrollStrategy::Center);
        self.update_preview(window, cx);
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, window, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_selection(-1, window, cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = Self::first_match_row(&self.results.rows) {
            self.selected_row = ix;
            self.list_scroll_handle
                .scroll_to_item(self.selected_row, ScrollStrategy::Center);
            self.update_preview(window, cx);
            cx.notify();
        }
    }

    fn select_last(&mut self, _: &menu::SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = Self::last_match_row(&self.results.rows) {
            self.selected_row = ix;
            self.list_scroll_handle
                .scroll_to_item(self.selected_row, ScrollStrategy::Center);
            self.update_preview(window, cx);
            cx.notify();
        }
    }

    /// Resolve `selected_row` to a `Row::Match` and (re)build/refresh the read-only preview
    /// editor: a fresh `Editor::for_buffer` when the selection just moved to a different file,
    /// otherwise the existing preview editor is reused and just re-highlighted/re-scrolled. A
    /// selection that isn't (or is no longer) a `Row::Match` — empty results, or a `Row::Header`
    /// — leaves any existing preview in place rather than tearing it down, so a stray
    /// `clamp_selection` mid-stream doesn't flash the pane empty.
    ///
    /// Early-returns before touching the editor at all when the resolved match is identical to
    /// `previewed_match` — otherwise every call (including ones triggered by a streaming batch
    /// that didn't move the selection) would re-highlight and re-autoscroll the preview, snapping
    /// a user's manual scroll back to center.
    fn update_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Row::Match(group_index, match_index)) =
            self.results.rows.get(self.selected_row).copied()
        else {
            return;
        };
        let Some(group) = self.results.groups.get(group_index) else {
            return;
        };
        let Some(match_row) = group.matches.get(match_index) else {
            return;
        };
        let buffer = group.buffer.clone();
        let buffer_id = buffer.entity_id();
        let range = match_row.range.clone();

        let target = (buffer_id, range.clone());
        if self.previewed_match.as_ref() == Some(&target) {
            return;
        }

        let needs_rebuild = self
            .preview_editor
            .as_ref()
            .is_none_or(|(existing_id, _)| *existing_id != buffer_id);
        if needs_rebuild {
            let project = self.project.clone();
            let editor = cx.new(|cx| {
                let mut editor = Editor::for_buffer(buffer, Some(project), window, cx);
                editor.set_read_only(true);
                editor
            });
            self.preview_editor = Some((buffer_id, editor));
        }

        let Some((_, editor)) = self.preview_editor.clone() else {
            return;
        };
        let displayed = editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let Some(multibuffer_range) = snapshot.anchor_range_in_buffer(range) else {
                return false;
            };
            editor.highlight_background(
                HighlightKey::FindInPathPreview,
                &[multibuffer_range.clone()],
                |_, theme| theme.colors().search_match_background,
                cx,
            );
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_ranges([multibuffer_range]),
            );
            true
        });
        // Only remember this match as "displayed" once it's actually been highlighted/scrolled to
        // — if `anchor_range_in_buffer` failed to resolve, leave `previewed_match` as-is so a
        // later call (e.g. once the buffer has caught up) still retries instead of being
        // short-circuited by the identity check above.
        if displayed {
            self.previewed_match = Some(target);
        }
    }

    /// Render one row of `self.results.rows` for the `uniform_list`. Bounds-checked against
    /// `groups`/`matches` since streaming updates and click closures can race a row's index
    /// against a shrinking/rebuilt result set — an out-of-range row renders as an empty element
    /// rather than panicking.
    fn render_row(&self, ix: usize, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.results.rows.get(ix).copied() else {
            return div().into_any_element();
        };
        match row {
            Row::Header(group_index) => {
                let Some(group) = self.results.groups.get(group_index) else {
                    return div().into_any_element();
                };
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .child(
                        Label::new(group.path.to_string_lossy().into_owned())
                            .size(LabelSize::Small)
                            .color(Color::Default),
                    )
                    .child(
                        Label::new(group.matches.len().to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element()
            }
            Row::Match(group_index, match_index) => {
                let Some(group) = self.results.groups.get(group_index) else {
                    return div().into_any_element();
                };
                let Some(match_row) = group.matches.get(match_index) else {
                    return div().into_any_element();
                };
                let selected = ix == self.selected_row;
                h_flex()
                    .id(("find-in-path-row", ix))
                    .w_full()
                    .pl_6()
                    .pr_2()
                    .py_0p5()
                    .gap_2()
                    .cursor_pointer()
                    .when(selected, |this| {
                        this.bg(cx.theme().colors().element_selected)
                    })
                    .child(
                        Label::new((match_row.line + 1).to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(match_row.snippet.clone()).size(LabelSize::Small))
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.selected_row = ix;
                        this.update_preview(window, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            }
        }
    }

    /// The left pane of the results split: the flattened `MatchList` row list, one file-header
    /// row per group followed by its indented match rows.
    fn render_results(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let row_count = self.results.rows.len();
        uniform_list(
            "find-in-path-results",
            row_count,
            cx.processor(move |this, range: Range<usize>, window, cx| {
                range
                    .map(|ix| this.render_row(ix, window, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.list_scroll_handle)
        .size_full()
    }
}

impl Focusable for FindInPath {
    // Delegate to the query editor (mirroring `BufferSearchBar`) so the modal layer's
    // "focus after being shown" (`ModalLayer::show_modal`) lands the caret in the query field.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query_editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for FindInPath {}

impl ModalView for FindInPath {
    fn fade_out_background(&self) -> bool {
        true
    }

    fn debug_kind(&self) -> &'static str {
        "FindInPath"
    }
}

impl Render for FindInPath {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.preview_dirty {
            self.preview_dirty = false;
            self.update_preview(window, cx);
        }
        v_flex()
            .key_context("FindInPath")
            .track_focus(&self.focus_handle)
            .w(relative(0.85))
            .h(relative(0.80))
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_lg()
            .on_action(cx.listener(Self::toggle_case_sensitive))
            .on_action(cx.listener(Self::toggle_whole_word))
            .on_action(cx.listener(Self::toggle_regex))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .child(
                        search_bar::input_base_styles(cx.theme().colors().border, |d| d)
                            .child(search_bar::render_text_input(&self.query_editor, None, cx)),
                    )
                    .child(SearchOption::CaseSensitive.as_button(
                        self.search_options,
                        SearchSource::Buffer,
                        self.focus_handle.clone(),
                    ))
                    .child(SearchOption::WholeWord.as_button(
                        self.search_options,
                        SearchSource::Buffer,
                        self.focus_handle.clone(),
                    ))
                    .child(SearchOption::Regex.as_button(
                        self.search_options,
                        SearchSource::Buffer,
                        self.focus_handle.clone(),
                    )),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        div()
                            .w(relative(0.4))
                            .h_full()
                            .border_r_1()
                            .border_color(cx.theme().colors().border)
                            .child(self.render_results(cx)),
                    )
                    // Live read-only preview of the selected match, scrolled/highlighted by
                    // `update_preview`. Falls back to a placeholder until a match is selected.
                    .child(if let Some((_, editor)) = self.preview_editor.as_ref() {
                        div().flex_1().h_full().child(editor.clone())
                    } else {
                        div()
                            .flex_1()
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Label::new("Select a match")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                    }),
            )
            .child(h_flex().p_1().child(self.status_label()))
    }
}
