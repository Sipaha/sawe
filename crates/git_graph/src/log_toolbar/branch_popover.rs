//! Multi-select branch filter popover for the Git Graph log toolbar
//! (S-FLT, branch chip). Mirrors IntelliJ IDEA's git log Branch filter UX:
//! a fuzzy search input, scrollable list with local branches first then
//! remote, sticky group separators, and Apply / Clear all / Cancel
//! footer. Selection is staged locally and only committed to the
//! parent `GitGraph` on Apply.

use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::AtomicBool},
};

use editor::Editor;
use fuzzy::{StringMatch, StringMatchCandidate};
use git::repository::Branch;
use gpui::{
    Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, WeakEntity, Window,
    rems, uniform_list,
};
use project::git_store::Repository;
use ui::{Checkbox, Divider, HighlightedLabel, ListItem, ListItemSpacing, ToggleState, prelude::*};

use crate::GitGraph;

pub(super) const POPOVER_WIDTH_REMS: f32 = 24.0;
const ROW_HEIGHT_REMS: f32 = 1.75;
const LIST_MAX_HEIGHT_REMS: f32 = 22.0;

#[derive(Clone, Debug)]
struct BranchEntry {
    ref_name: SharedString,
    display_name: SharedString,
    is_remote: bool,
}

impl BranchEntry {
    fn from_branch(branch: &Branch) -> Self {
        Self {
            ref_name: branch.ref_name.clone(),
            display_name: SharedString::from(branch.name().to_string()),
            is_remote: branch.is_remote(),
        }
    }
}

#[derive(Clone)]
enum Row {
    Header(SharedString),
    Branch { index: usize, positions: Vec<usize> },
}

pub struct BranchFilterPopover {
    weak_graph: WeakEntity<GitGraph>,
    branches: Vec<BranchEntry>,
    selected: BTreeSet<SharedString>,
    query: Entity<Editor>,
    rows: Vec<Row>,
    match_task: Option<Task<()>>,
    cancel_flag: Arc<AtomicBool>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl BranchFilterPopover {
    pub fn new(
        weak_graph: WeakEntity<GitGraph>,
        repository: Option<Entity<Repository>>,
        active: Vec<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branches = repository
            .as_ref()
            .map(|repo| {
                repo.read(cx)
                    .branch_list
                    .iter()
                    .map(BranchEntry::from_branch)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let query = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter branches…", window, cx);
            editor
        });

        let on_query_changed =
            |this: &mut BranchFilterPopover,
             _,
             event: &editor::EditorEvent,
             cx: &mut Context<BranchFilterPopover>| {
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
            branches,
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

        let query = self.query.read(cx).text(cx);
        let candidates: Vec<StringMatchCandidate> = self
            .branches
            .iter()
            .enumerate()
            .map(|(ix, b)| StringMatchCandidate::new(ix, b.display_name.as_ref()))
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
        let mut local: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut remote: Vec<(usize, Vec<usize>)> = Vec::new();
        for m in matches {
            let Some(entry) = self.branches.get(m.candidate_id) else {
                continue;
            };
            if entry.is_remote {
                remote.push((m.candidate_id, m.positions));
            } else {
                local.push((m.candidate_id, m.positions));
            }
        }

        let sort_key = |index: &usize, branches: &[BranchEntry]| {
            branches
                .get(*index)
                .map(|b| b.display_name.to_lowercase())
                .unwrap_or_default()
        };
        local.sort_by(|a, b| sort_key(&a.0, &self.branches).cmp(&sort_key(&b.0, &self.branches)));
        remote.sort_by(|a, b| sort_key(&a.0, &self.branches).cmp(&sort_key(&b.0, &self.branches)));

        let mut rows: Vec<Row> = Vec::with_capacity(local.len() + remote.len() + 2);
        if !local.is_empty() {
            rows.push(Row::Header(SharedString::from("Local")));
            for (index, positions) in local {
                rows.push(Row::Branch { index, positions });
            }
        }
        if !remote.is_empty() {
            rows.push(Row::Header(SharedString::from("Remote")));
            for (index, positions) in remote {
                rows.push(Row::Branch { index, positions });
            }
        }
        self.rows = rows;
    }

    fn toggle_branch(&mut self, ref_name: SharedString, cx: &mut Context<Self>) {
        if !self.selected.remove(&ref_name) {
            self.selected.insert(ref_name);
        }
        cx.notify();
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        let mut branches: Vec<SharedString> = self.selected.iter().cloned().collect();
        // Stable order: preserve the order branches appear in the
        // repository's branch list so the resulting `git log` argv is
        // deterministic between sessions with the same selection.
        let order: Vec<&SharedString> = self.branches.iter().map(|b| &b.ref_name).collect();
        branches.sort_by_key(|b| order.iter().position(|r| *r == b).unwrap_or(usize::MAX));
        if let Some(graph) = self.weak_graph.upgrade() {
            graph.update(cx, |graph, cx| {
                graph.set_branch_filter(branches, cx);
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

impl EventEmitter<DismissEvent> for BranchFilterPopover {}

impl Focusable for BranchFilterPopover {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BranchFilterPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let color = cx.theme().colors();
        let total_branches = self.branches.len();
        let selected_count = self.selected.len();

        let search_input = h_flex()
            .h_8()
            .px_2()
            .border_1()
            .border_color(color.border)
            .rounded_md()
            .bg(color.editor_background)
            .child(self.query.clone());

        let list_body: gpui::AnyElement = if total_branches == 0 {
            v_flex()
                .py_4()
                .items_center()
                .child(
                    Label::new("No branches")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element()
        } else if self.rows.is_empty() {
            v_flex()
                .py_4()
                .items_center()
                .child(
                    Label::new("No branches match your query")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element()
        } else {
            let row_count = self.rows.len();
            let list_height = rems((row_count as f32 * ROW_HEIGHT_REMS).min(LIST_MAX_HEIGHT_REMS));
            uniform_list(
                "git-graph-branch-filter-list",
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
                                Row::Branch { index, positions } => {
                                    let Some(entry) = this.branches.get(index).cloned() else {
                                        return gpui::Empty.into_any_element();
                                    };
                                    let is_selected = this.selected.contains(&entry.ref_name);
                                    let toggle_state = if is_selected {
                                        ToggleState::Selected
                                    } else {
                                        ToggleState::Unselected
                                    };
                                    let row_id =
                                        SharedString::from(format!("git-graph-branch-row-{ix}"));
                                    let ref_name_for_click = entry.ref_name.clone();
                                    ListItem::new(row_id)
                                        .inset(true)
                                        .spacing(ListItemSpacing::Sparse)
                                        .toggle_state(is_selected)
                                        .start_slot(
                                            Checkbox::new(
                                                SharedString::from(format!(
                                                    "git-graph-branch-check-{ix}"
                                                )),
                                                toggle_state,
                                            )
                                            .into_any_element(),
                                        )
                                        .child(HighlightedLabel::new(entry.display_name, positions))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_branch(ref_name_for_click.clone(), cx);
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
            Button::new("git-graph-branch-clear-all", "Clear all")
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::Small)
                .disabled(selected_count == 0)
                .on_click(cx.listener(|this, _, _, cx| this.clear_all(cx))),
        );
        let footer_right = h_flex()
            .gap_1()
            .child(
                Button::new("git-graph-branch-cancel", "Cancel")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
            )
            .child(
                Button::new("git-graph-branch-apply", "Apply")
                    .style(ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
            );

        v_flex()
            .key_context("GitGraphBranchFilterPopover")
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
