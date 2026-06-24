//! Multi-select path filter popover for the Git Graph log toolbar
//! (S-FLT, Path chip). Mirrors IntelliJ IDEA's git log Path filter UX:
//! a fuzzy search input over a flat list of paths under the repo's
//! working tree, sticky "Paths" group header, and Apply / Clear all /
//! Cancel footer. Selection is staged locally and only committed to
//! the parent `GitGraph` on Apply, where it lands in `LogFilters::paths`
//! and is appended *after* the `--` separator on the next `git log`
//! invocation.
//!
//! For v1 we render a single flat list (files + dirs both selectable)
//! ordered alphabetically by repo-relative path. Selecting a directory
//! is git-natural — `git log -- some/dir/` includes any commit that
//! touched anything under it.

use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::AtomicBool},
};

use editor::Editor;
use fuzzy::{StringMatch, StringMatchCandidate};
use git::repository::RepoPath;
use gpui::{
    Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, WeakEntity, Window,
    rems, uniform_list,
};
use project::git_store::Repository;
use ui::{Checkbox, Divider, HighlightedLabel, ListItem, ListItemSpacing, ToggleState, prelude::*};

use crate::GitGraph;

pub(super) const POPOVER_WIDTH_REMS: f32 = 28.0;
const ROW_HEIGHT_REMS: f32 = 1.75;
const LIST_MAX_HEIGHT_REMS: f32 = 22.0;

#[derive(Clone, Debug)]
struct PathRow {
    repo_path: RepoPath,
    display: SharedString,
    is_dir: bool,
}

#[derive(Clone)]
enum Row {
    Header(SharedString),
    Path { index: usize, positions: Vec<usize> },
}

pub struct PathFilterPopover {
    weak_graph: WeakEntity<GitGraph>,
    paths: Vec<PathRow>,
    selected: BTreeSet<RepoPath>,
    query: Entity<Editor>,
    rows: Vec<Row>,
    match_task: Option<Task<()>>,
    cancel_flag: Arc<AtomicBool>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl PathFilterPopover {
    pub fn new(
        weak_graph: WeakEntity<GitGraph>,
        repository: Option<Entity<Repository>>,
        active: Vec<RepoPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let paths = collect_repo_paths(repository.as_ref(), cx);

        let query = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter paths…", window, cx);
            editor
        });

        let on_query_changed =
            |this: &mut PathFilterPopover,
             _,
             event: &editor::EditorEvent,
             cx: &mut Context<PathFilterPopover>| {
                if matches!(
                    event,
                    editor::EditorEvent::BufferEdited | editor::EditorEvent::Edited { .. }
                ) {
                    this.refresh_matches(cx);
                }
            };
        let subscriptions = vec![cx.subscribe(&query, on_query_changed)];

        let focus_handle = cx.focus_handle();
        let mut this = Self {
            weak_graph,
            paths,
            selected: active.into_iter().collect(),
            query,
            rows: Vec::new(),
            match_task: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            focus_handle,
            _subscriptions: subscriptions,
        };
        this.refresh_matches(cx);
        this
    }

    fn refresh_matches(&mut self, cx: &mut Context<Self>) {
        // Cancel any in-flight match task before kicking off a new one.
        self.cancel_flag
            .store(true, std::sync::atomic::Ordering::Release);
        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.cancel_flag = cancel_flag.clone();

        if self.paths.is_empty() {
            self.rows.clear();
            self.match_task = None;
            return;
        }

        let query = self.query.read(cx).text(cx);
        let candidates: Vec<StringMatchCandidate> = self
            .paths
            .iter()
            .enumerate()
            .map(|(ix, p)| StringMatchCandidate::new(ix, p.display.as_ref()))
            .collect();
        let executor = cx.background_executor().clone();
        let task = cx.spawn(async move |this, cx| {
            let matches: Vec<StringMatch> = if query.is_empty() {
                candidates
                    .iter()
                    .map(|c| StringMatch {
                        candidate_id: c.id,
                        score: 0.0,
                        positions: Vec::new(),
                        string: c.string.clone(),
                    })
                    .collect()
            } else {
                fuzzy::match_strings(
                    &candidates,
                    &query,
                    true,
                    true,
                    candidates.len().max(1),
                    &cancel_flag,
                    executor,
                )
                .await
            };
            if cancel_flag.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            this.update(cx, |this, cx| {
                this.rebuild_rows(matches);
                cx.notify();
            })
            .ok();
        });
        self.match_task = Some(task);
    }

    fn rebuild_rows(&mut self, matches: Vec<StringMatch>) {
        let mut sorted: Vec<(usize, Vec<usize>)> = matches
            .into_iter()
            .filter_map(|m| {
                self.paths
                    .get(m.candidate_id)
                    .map(|_| (m.candidate_id, m.positions))
            })
            .collect();

        // When the query is empty, all entries score 0.0 and `match_strings`
        // doesn't impose an order — preserve the alphabetic ordering from
        // `collect_repo_paths`. With a query active, fuzzy score order is
        // what the user expects, so leave it alone.
        let user_typed_query = sorted.iter().any(|(_, positions)| !positions.is_empty());
        if !user_typed_query {
            sorted.sort_by(|a, b| {
                let ad = self
                    .paths
                    .get(a.0)
                    .map(|p| p.display.as_ref())
                    .unwrap_or("");
                let bd = self
                    .paths
                    .get(b.0)
                    .map(|p| p.display.as_ref())
                    .unwrap_or("");
                ad.cmp(bd)
            });
        }

        let mut rows: Vec<Row> = Vec::with_capacity(sorted.len() + 1);
        if !sorted.is_empty() {
            rows.push(Row::Header(SharedString::from("Paths")));
            for (index, positions) in sorted {
                rows.push(Row::Path { index, positions });
            }
        }
        self.rows = rows;
    }

    fn toggle_path(&mut self, repo_path: RepoPath, cx: &mut Context<Self>) {
        if !self.selected.remove(&repo_path) {
            self.selected.insert(repo_path);
        }
        cx.notify();
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        let mut paths: Vec<RepoPath> = self.selected.iter().cloned().collect();
        // Stable order: alphabetic by display (matches BTreeSet iteration
        // order anyway, but spelled out so future changes don't accidentally
        // flip determinism).
        paths.sort();
        if let Some(graph) = self.weak_graph.upgrade() {
            graph.update(cx, |graph, cx| {
                graph.set_path_filter(paths, cx);
            });
        }
        cx.emit(DismissEvent);
    }

    fn clear_all(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            return;
        }
        self.selected.clear();
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

fn collect_repo_paths(
    repository: Option<&Entity<Repository>>,
    cx: &mut Context<PathFilterPopover>,
) -> Vec<PathRow> {
    let Some(repository) = repository else {
        return Vec::new();
    };
    let repo_abs_path = repository.read(cx).work_directory_abs_path.clone();

    // Reach out to the matching project worktree via GitStore -> WorktreeStore.
    // We pick the worktree whose `abs_path` equals the repo's working tree —
    // anything else (e.g. a sub-worktree rooted inside the repo) would yield
    // path strings missing the path prefix git would expect.
    let git_store = match repository.read(cx).git_store() {
        Some(git_store) => git_store,
        None => return Vec::new(),
    };
    let worktree_store = git_store.read(cx).worktree_store().clone();
    let matching_worktree = worktree_store
        .read(cx)
        .worktrees()
        .find(|wt| wt.read(cx).abs_path().as_ref() == repo_abs_path.as_ref());
    let Some(worktree) = matching_worktree else {
        return Vec::new();
    };

    let snapshot = worktree.read(cx).snapshot();
    let mut rows: Vec<PathRow> = snapshot
        .entries(false, 0)
        .filter(|entry| !entry.path.is_empty())
        .map(|entry| {
            let display = entry.path.as_unix_str().to_string();
            let repo_path = RepoPath::from_rel_path(entry.path.as_ref());
            PathRow {
                repo_path,
                display: SharedString::from(display),
                is_dir: entry.is_dir(),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.display.as_ref().cmp(b.display.as_ref()));
    rows
}

impl EventEmitter<DismissEvent> for PathFilterPopover {}

impl Focusable for PathFilterPopover {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PathFilterPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let color = cx.theme().colors();
        let total_paths = self.paths.len();
        let selected_count = self.selected.len();

        let search_input = h_flex()
            .h_8()
            .px_2()
            .border_1()
            .border_color(color.border)
            .rounded_md()
            .bg(color.editor_background)
            .child(self.query.clone());

        let list_body: gpui::AnyElement = if total_paths == 0 {
            v_flex()
                .py_4()
                .items_center()
                .child(
                    Label::new("No paths")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element()
        } else if self.rows.is_empty() {
            v_flex()
                .py_4()
                .items_center()
                .child(
                    Label::new("No paths match your query")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element()
        } else {
            let row_count = self.rows.len();
            let list_height = rems((row_count as f32 * ROW_HEIGHT_REMS).min(LIST_MAX_HEIGHT_REMS));
            uniform_list(
                "git-graph-path-filter-list",
                row_count,
                cx.processor(
                    move |this: &mut Self, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|ix| this.rows.get(ix).cloned().map(|row| (ix, row)))
                            .map(|(ix, row)| match row {
                                Row::Header(label) => h_flex()
                                    .h(rems(ROW_HEIGHT_REMS))
                                    .px_2()
                                    .items_end()
                                    .child(
                                        Label::new(label)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .into_any_element(),
                                Row::Path { index, positions } => {
                                    let Some(entry) = this.paths.get(index).cloned() else {
                                        return gpui::Empty.into_any_element();
                                    };
                                    let is_selected = this.selected.contains(&entry.repo_path);
                                    let toggle_state = if is_selected {
                                        ToggleState::Selected
                                    } else {
                                        ToggleState::Unselected
                                    };
                                    let row_id =
                                        SharedString::from(format!("git-graph-path-row-{ix}"));
                                    let path_for_click = entry.repo_path.clone();
                                    let icon = if entry.is_dir {
                                        IconName::Folder
                                    } else {
                                        IconName::FileGeneric
                                    };
                                    ListItem::new(row_id)
                                        .inset(true)
                                        .spacing(ListItemSpacing::Sparse)
                                        .toggle_state(is_selected)
                                        .start_slot(
                                            Checkbox::new(
                                                SharedString::from(format!(
                                                    "git-graph-path-check-{ix}"
                                                )),
                                                toggle_state,
                                            )
                                            .into_any_element(),
                                        )
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .flex_1()
                                                .child(
                                                    Icon::new(icon)
                                                        .color(Color::Muted)
                                                        .size(IconSize::Small),
                                                )
                                                .child(HighlightedLabel::new(
                                                    entry.display,
                                                    positions,
                                                )),
                                        )
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_path(path_for_click.clone(), cx);
                                        }))
                                        .into_any_element()
                                }
                            })
                            .collect()
                    },
                ),
            )
            // See user_popover: `uniform_list` needs a concrete height in this
            // unbounded popover column or it collapses.
            .h(list_height)
            .into_any_element()
        };

        let footer_left = h_flex().gap_1().child(
            Button::new("git-graph-path-clear-all", "Clear all")
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::Small)
                .disabled(selected_count == 0)
                .on_click(cx.listener(|this, _, _, cx| this.clear_all(cx))),
        );
        let footer_right = h_flex()
            .gap_1()
            .child(
                Button::new("git-graph-path-cancel", "Cancel")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
            )
            .child(
                Button::new("git-graph-path-apply", "Apply")
                    .style(ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
            );

        v_flex()
            .key_context("GitGraphPathFilterPopover")
            .track_focus(&self.focus_handle)
            .w(rems(POPOVER_WIDTH_REMS))
            .p_2()
            .gap_2()
            .bg(color.elevated_surface_background)
            .border_1()
            .border_color(color.border)
            .rounded_md()
            .child(search_input)
            .child(Divider::horizontal())
            .child(list_body)
            .child(Divider::horizontal())
            .child(
                h_flex()
                    .justify_between()
                    .child(footer_left)
                    .child(footer_right),
            )
    }
}
