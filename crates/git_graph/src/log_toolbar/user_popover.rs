//! Multi-select user (author) filter popover for the Git Graph log
//! toolbar (S-FLT, User chip). Mirrors IntelliJ IDEA's git log User
//! filter UX: a fuzzy search input, scrollable list of authors with
//! checkboxes, sticky "Authors" group header, commit counts as tail
//! badges, and Apply / Clear all / Cancel footer. Selection is staged
//! locally and only committed to the parent `GitGraph` on Apply.
//!
//! Selection key is the author email (not display name) so identical
//! humans typed under multiple display names collapse — `LogFilters`'
//! `--author=<re>` matcher matches against `Author <email>` and emails
//! are stable identifiers.

use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::AtomicBool},
};

use editor::Editor;
use fuzzy::{StringMatch, StringMatchCandidate};
use git::repository::AuthorHistoryEntry;
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
struct AuthorRow {
    name: SharedString,
    email: SharedString,
    commit_count: usize,
    /// Pre-built haystack used by the fuzzy matcher: `"<name> <email>"`.
    /// Built once so candidates carry a stable string the matcher can
    /// score against; `positions` are returned in haystack-coordinates
    /// and only painted on the name span (any position past the name
    /// boundary is dropped).
    haystack: SharedString,
    name_len: usize,
}

#[derive(Clone)]
enum Row {
    Header(SharedString),
    Author { index: usize, positions: Vec<usize> },
}

enum LoadState {
    Loading,
    Ready,
    Error(SharedString),
}

pub struct UserFilterPopover {
    weak_graph: WeakEntity<GitGraph>,
    authors: Vec<AuthorRow>,
    selected: BTreeSet<SharedString>,
    query: Entity<Editor>,
    rows: Vec<Row>,
    match_task: Option<Task<()>>,
    cancel_flag: Arc<AtomicBool>,
    load_state: LoadState,
    _load_task: Option<Task<()>>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl UserFilterPopover {
    pub fn new(
        weak_graph: WeakEntity<GitGraph>,
        repository: Option<Entity<Repository>>,
        active: Vec<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter authors…", window, cx);
            editor
        });

        let on_query_changed =
            |this: &mut UserFilterPopover,
             _,
             event: &editor::EditorEvent,
             cx: &mut Context<UserFilterPopover>| {
                if matches!(
                    event,
                    editor::EditorEvent::BufferEdited | editor::EditorEvent::Edited { .. }
                ) {
                    this.refresh_matches(cx);
                }
            };
        let subscriptions = vec![cx.subscribe(&query, on_query_changed)];

        let focus_handle = cx.focus_handle();

        let load_task = repository.map(|repo| {
            let fetch_task = repo.update(cx, |repo, cx| repo.author_history(cx));
            cx.spawn(async move |this, cx| {
                let result = fetch_task.await;
                this.update(cx, |this, cx| {
                    match result {
                        Ok(entries) => {
                            this.authors = entries.into_iter().map(AuthorRow::from_entry).collect();
                            this.load_state = LoadState::Ready;
                        }
                        Err(err) => {
                            this.load_state = LoadState::Error(SharedString::from(err.to_string()));
                        }
                    }
                    this.refresh_matches(cx);
                    cx.notify();
                })
                .ok();
            })
        });

        let initial_load_state = if load_task.is_some() {
            LoadState::Loading
        } else {
            LoadState::Ready
        };

        let mut this = Self {
            weak_graph,
            authors: Vec::new(),
            selected: active.into_iter().collect(),
            query,
            rows: Vec::new(),
            match_task: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            load_state: initial_load_state,
            _load_task: load_task,
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

        if self.authors.is_empty() {
            self.rows.clear();
            self.match_task = None;
            return;
        }

        let query = self.query.read(cx).text(cx);
        let candidates: Vec<StringMatchCandidate> = self
            .authors
            .iter()
            .enumerate()
            .map(|(ix, a)| StringMatchCandidate::new(ix, a.haystack.as_ref()))
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
                self.authors
                    .get(m.candidate_id)
                    .map(|_| (m.candidate_id, m.positions))
            })
            .collect();

        // When the query is empty all entries score 0.0 — preserve the
        // shortlog's natural ordering (descending commit count). When a
        // query is active, fuzzy-match score order is what the user
        // expects to see, so leave that ordering alone.
        let user_typed_query = sorted.iter().any(|(_, positions)| !positions.is_empty());
        if !user_typed_query {
            sorted.sort_by(|a, b| {
                let ac = self.authors.get(a.0).map(|x| x.commit_count).unwrap_or(0);
                let bc = self.authors.get(b.0).map(|x| x.commit_count).unwrap_or(0);
                bc.cmp(&ac)
            });
        }

        let mut rows: Vec<Row> = Vec::with_capacity(sorted.len() + 1);
        if !sorted.is_empty() {
            rows.push(Row::Header(SharedString::from("Authors")));
            for (index, positions) in sorted {
                rows.push(Row::Author { index, positions });
            }
        }
        self.rows = rows;
    }

    fn toggle_author(&mut self, email: SharedString, cx: &mut Context<Self>) {
        if !self.selected.remove(&email) {
            self.selected.insert(email);
        }
        cx.notify();
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        let mut emails: Vec<SharedString> = self.selected.iter().cloned().collect();
        // Stable order: preserve the order authors appear in the
        // shortlog (descending commit count) so the resulting
        // `--author=<re>` argv is deterministic between sessions with
        // the same selection.
        let order: Vec<&SharedString> = self.authors.iter().map(|a| &a.email).collect();
        emails.sort_by_key(|e| order.iter().position(|r| *r == e).unwrap_or(usize::MAX));
        if let Some(graph) = self.weak_graph.upgrade() {
            graph.update(cx, |graph, cx| {
                graph.set_user_filter(emails, cx);
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

impl AuthorRow {
    fn from_entry(entry: AuthorHistoryEntry) -> Self {
        let haystack = format!("{} {}", entry.name, entry.email);
        let name_len = entry.name.chars().count();
        Self {
            name: entry.name,
            email: entry.email,
            commit_count: entry.commit_count,
            haystack: SharedString::from(haystack),
            name_len,
        }
    }
}

impl EventEmitter<DismissEvent> for UserFilterPopover {}

impl Focusable for UserFilterPopover {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for UserFilterPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let color = cx.theme().colors();
        let total_authors = self.authors.len();
        let selected_count = self.selected.len();

        let search_input = h_flex()
            .h_8()
            .px_2()
            .border_1()
            .border_color(color.border)
            .rounded_md()
            .bg(color.editor_background)
            .child(self.query.clone());

        let list_body: gpui::AnyElement = match &self.load_state {
            LoadState::Loading => v_flex()
                .py_4()
                .items_center()
                .child(
                    Label::new("Loading…")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element(),
            LoadState::Error(message) => v_flex()
                .py_4()
                .items_center()
                .child(
                    Label::new(message.clone())
                        .color(Color::Error)
                        .size(LabelSize::Small),
                )
                .into_any_element(),
            LoadState::Ready if total_authors == 0 => v_flex()
                .py_4()
                .items_center()
                .child(
                    Label::new("No authors")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element(),
            LoadState::Ready if self.rows.is_empty() => v_flex()
                .py_4()
                .items_center()
                .child(
                    Label::new("No authors match your query")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element(),
            LoadState::Ready => {
                let row_count = self.rows.len();
                let list_height =
                    rems((row_count as f32 * ROW_HEIGHT_REMS).min(LIST_MAX_HEIGHT_REMS));
                uniform_list(
                    "git-graph-user-filter-list",
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
                                    Row::Author { index, positions } => {
                                        let Some(entry) = this.authors.get(index).cloned() else {
                                            return gpui::Empty.into_any_element();
                                        };
                                        let is_selected = this.selected.contains(&entry.email);
                                        let toggle_state = if is_selected {
                                            ToggleState::Selected
                                        } else {
                                            ToggleState::Unselected
                                        };
                                        let row_id =
                                            SharedString::from(format!("git-graph-user-row-{ix}"));
                                        let email_for_click = entry.email.clone();
                                        // Highlight only the positions falling
                                        // inside the name span; positions on
                                        // the email tail would map to a
                                        // different label so we drop them.
                                        let name_positions: Vec<usize> = positions
                                            .into_iter()
                                            .filter(|p| *p < entry.name_len)
                                            .collect();
                                        let count_label =
                                            SharedString::from(format!("({})", entry.commit_count));
                                        ListItem::new(row_id)
                                            .inset(true)
                                            .spacing(ListItemSpacing::Sparse)
                                            .toggle_state(is_selected)
                                            .start_slot(
                                                Checkbox::new(
                                                    SharedString::from(format!(
                                                        "git-graph-user-check-{ix}"
                                                    )),
                                                    toggle_state,
                                                )
                                                .into_any_element(),
                                            )
                                            .child(
                                                h_flex()
                                                    .gap_2()
                                                    .flex_1()
                                                    .child(HighlightedLabel::new(
                                                        entry.name.clone(),
                                                        name_positions,
                                                    ))
                                                    .child(
                                                        Label::new(entry.email)
                                                            .color(Color::Muted)
                                                            .size(LabelSize::Small),
                                                    ),
                                            )
                                            .end_slot(
                                                Label::new(count_label)
                                                    .color(Color::Muted)
                                                    .size(LabelSize::Small),
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.toggle_author(email_for_click.clone(), cx);
                                            }))
                                            .into_any_element()
                                    }
                                })
                                .collect()
                        },
                    ),
                )
                // `uniform_list` virtualizes against its viewport height — with
                // only `max_h` and no concrete height in this unbounded popover
                // column it collapses to ~nothing, so size it to the rows
                // (capped). The empty case is handled by an earlier arm.
                .h(list_height)
                .into_any_element()
            }
        };

        let footer_left = h_flex().gap_1().child(
            Button::new("git-graph-user-clear-all", "Clear all")
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::Small)
                .disabled(selected_count == 0)
                .on_click(cx.listener(|this, _, _, cx| this.clear_all(cx))),
        );
        let footer_right = h_flex()
            .gap_1()
            .child(
                Button::new("git-graph-user-cancel", "Cancel")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
            )
            .child(
                Button::new("git-graph-user-apply", "Apply")
                    .style(ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
            );

        v_flex()
            .key_context("GitGraphUserFilterPopover")
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
