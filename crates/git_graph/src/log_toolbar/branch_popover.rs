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
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Task, WeakEntity, Window, rems, uniform_list,
};
use project::git_store::Repository;
use ui::{Checkbox, Divider, HighlightedLabel, ListItem, ListItemSpacing, ToggleState, prelude::*};

use super::filter_cursor::FilterCursor;
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
    /// Keyboard cursor into `rows` — where Enter will toggle. Orthogonal to
    /// `selected`, which is the checked set `Apply` commits.
    cursor: FilterCursor,
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
            cursor: FilterCursor::new(),
            match_task: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            focus_handle,
            _subscriptions: subscriptions,
        };
        this.refresh_matches(cx);
        // `PopoverMenu::show_menu` focuses `Focusable::focus_handle` two frames
        // later, and that now resolves to the query editor, so this call only
        // covers hosts that focus the view at construction time. It cannot race
        // the deferred one — they aim at the same handle.
        this.query.focus_handle(cx).focus(window, cx);
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
        let rows = &self.rows;
        self.cursor
            .reset(rows.len(), |ix| Self::is_actionable(rows, ix));
    }

    fn is_actionable(rows: &[Row], index: usize) -> bool {
        matches!(rows.get(index), Some(Row::Branch { .. }))
    }

    fn toggle_branch(&mut self, ref_name: SharedString, cx: &mut Context<Self>) {
        if !self.selected.remove(&ref_name) {
            self.selected.insert(ref_name);
        }
        cx.notify();
    }

    /// Enter toggles the cursored row's checkbox and leaves the popover open —
    /// the same thing a click on that row does. Applying and closing here would
    /// make the checkboxes unreachable from the keyboard, since the popover
    /// stages a whole set behind an explicit Apply button.
    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let Some(Row::Branch { index, .. }) = self.rows.get(self.cursor.index()) else {
            return;
        };
        let Some(ref_name) = self
            .branches
            .get(*index)
            .map(|entry| entry.ref_name.clone())
        else {
            return;
        };
        self.toggle_branch(ref_name, cx);
    }

    /// `ctrl-enter` — the keyboard route to the Apply button.
    fn secondary_confirm(
        &mut self,
        _: &menu::SecondaryConfirm,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply(cx);
    }

    fn handle_cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel(cx);
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let rows = &self.rows;
        if self
            .cursor
            .select_next(rows.len(), |ix| Self::is_actionable(rows, ix))
        {
            cx.notify();
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = &self.rows;
        if self
            .cursor
            .select_previous(|ix| Self::is_actionable(rows, ix))
        {
            cx.notify();
        }
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        let rows = &self.rows;
        if self
            .cursor
            .select_first(rows.len(), |ix| Self::is_actionable(rows, ix))
        {
            cx.notify();
        }
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        let rows = &self.rows;
        if self
            .cursor
            .select_last(rows.len(), |ix| Self::is_actionable(rows, ix))
        {
            cx.notify();
        }
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
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // Hand out the query editor's handle, not the container's: this is what
        // `PopoverMenu::show_menu` focuses (two frames after opening), so
        // returning the container would leave the caret nowhere and force a
        // click into the field before typing. The container element still
        // declares the key_context and the `on_action` handlers, and it is an
        // ancestor of the focused editor, so menu actions still reach it.
        self.query.focus_handle(cx)
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
                                        // Checked rows get the selected
                                        // background; the keyboard cursor is a
                                        // focus border, so the two states stay
                                        // tellable apart on the same row.
                                        .focused(ix == this.cursor.index())
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
                                            // Keep the keyboard cursor on the
                                            // row the mouse just acted on, so a
                                            // following arrow key continues
                                            // from there rather than jumping
                                            // back to the top match.
                                            this.cursor.move_to(ix);
                                            this.toggle_branch(ref_name_for_click.clone(), cx);
                                        }))
                                        .into_any_element()
                                }
                            })
                            .collect()
                    },
                ),
            )
            .track_scroll(self.cursor.scroll_handle())
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
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::secondary_confirm))
            .on_action(cx.listener(Self::handle_cancel))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_toolbar::test_support::init_test;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::Project;
    use std::{cell::Cell, rc::Rc};

    fn entry(display_name: &str, is_remote: bool) -> BranchEntry {
        let ref_name = if is_remote {
            format!("refs/remotes/{display_name}")
        } else {
            format!("refs/heads/{display_name}")
        };
        BranchEntry {
            ref_name: SharedString::from(ref_name),
            display_name: SharedString::from(display_name.to_string()),
            is_remote,
        }
    }

    /// The `ref_name` of the row the keyboard cursor is parked on, or `None`
    /// when it is on a header / out of range.
    fn cursored(popover: &BranchFilterPopover) -> Option<SharedString> {
        match popover.rows.get(popover.cursor.index()) {
            Some(Row::Branch { index, .. }) => {
                popover.branches.get(*index).map(|b| b.ref_name.clone())
            }
            _ => None,
        }
    }

    #[gpui::test]
    async fn test_branch_filter_popover_keyboard_selection(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let popover = window
            .update(cx, |_, window, cx| {
                let popover = cx.new(|cx| {
                    BranchFilterPopover::new(
                        WeakEntity::<GitGraph>::new_invalid(),
                        None,
                        Vec::new(),
                        window,
                        cx,
                    )
                });
                // Bug #2: opening the popover must land the caret in the search
                // field. `PopoverMenu::show_menu` focuses whatever
                // `Focusable::focus_handle` returns, so that has to resolve to
                // the query editor and not the container.
                let query_handle = popover.read(cx).query.focus_handle(cx);
                assert_eq!(
                    popover.focus_handle(cx),
                    query_handle,
                    "the popover must hand `PopoverMenu` the query editor's focus handle"
                );
                assert!(
                    query_handle.is_focused(window),
                    "the search field must be focused as soon as the popover opens"
                );
                popover
            })
            .expect("window is open");
        cx.run_until_parked();

        let dismissed = Rc::new(Cell::new(false));
        let _dismiss_subscription = cx.update(|cx| {
            let dismissed = dismissed.clone();
            cx.subscribe(&popover, move |_, _: &DismissEvent, _| dismissed.set(true))
        });

        // rows: 0 Header(Local) | 1 feature | 2 main | 3 Header(Remote) | 4 origin/main
        window
            .update(cx, |_, _window, cx| {
                popover.update(cx, |popover, cx| {
                    popover.branches = vec![
                        entry("main", false),
                        entry("feature", false),
                        entry("origin/main", true),
                    ];
                    popover.refresh_matches(cx);
                });
            })
            .expect("window is open");
        cx.run_until_parked();

        popover.update(cx, |popover, _| {
            assert_eq!(popover.rows.len(), 5, "two headers plus three branches");
            assert_eq!(
                cursored(popover).as_deref(),
                Some("refs/heads/feature"),
                "the cursor must start on the first matching row, not the header"
            );
        });

        let press = |action: &'static str, cx: &mut TestAppContext| {
            window
                .update(cx, |_, window, cx| {
                    popover.update(cx, |popover, cx| match action {
                        "next" => popover.select_next(&menu::SelectNext, window, cx),
                        "previous" => popover.select_previous(&menu::SelectPrevious, window, cx),
                        "confirm" => popover.confirm(&menu::Confirm, window, cx),
                        "cancel" => popover.handle_cancel(&menu::Cancel, window, cx),
                        _ => popover.secondary_confirm(&menu::SecondaryConfirm, window, cx),
                    });
                })
                .expect("window is open");
        };

        press("next", cx);
        popover.update(cx, |popover, _| {
            assert_eq!(cursored(popover).as_deref(), Some("refs/heads/main"));
        });
        press("next", cx);
        popover.update(cx, |popover, _| {
            assert_eq!(
                cursored(popover).as_deref(),
                Some("refs/remotes/origin/main"),
                "select_next must step over the Remote header"
            );
        });
        press("next", cx);
        popover.update(cx, |popover, _| {
            assert_eq!(
                cursored(popover).as_deref(),
                Some("refs/remotes/origin/main"),
                "the cursor must not run off the end of the list"
            );
        });

        press("previous", cx);
        press("previous", cx);
        popover.update(cx, |popover, _| {
            assert_eq!(
                cursored(popover).as_deref(),
                Some("refs/heads/feature"),
                "select_previous must step back over the Remote header"
            );
        });
        press("previous", cx);
        popover.update(cx, |popover, _| {
            assert_eq!(
                cursored(popover).as_deref(),
                Some("refs/heads/feature"),
                "the cursor must not run off the front of the list"
            );
        });

        // Confirm toggles the cursored checkbox and leaves the popover open.
        press("confirm", cx);
        popover.update(cx, |popover, _| {
            assert!(
                popover
                    .selected
                    .contains(&SharedString::from("refs/heads/feature")),
                "Confirm must check the cursored branch"
            );
        });
        assert!(
            !dismissed.get(),
            "Confirm must not dismiss the popover — Apply is a separate, explicit step"
        );
        press("confirm", cx);
        popover.update(cx, |popover, _| {
            assert!(
                popover.selected.is_empty(),
                "a second Confirm on the same row must uncheck it"
            );
        });

        // Narrowing the query rebuilds the list under the cursor: it must land
        // back on the first match rather than dangling past the shorter list.
        press("next", cx);
        press("next", cx);

        // A query that still matches everything: the list keeps its length, so
        // the old index stays in range and merely clamping it would be enough.
        // The cursor must still snap back to the first match — after a rebuild
        // the rows are a different ranking, so the old index means nothing.
        window
            .update(cx, |_, window, cx| {
                popover.read(cx).query.clone().update(cx, |editor, cx| {
                    editor.set_text("a", window, cx);
                });
            })
            .expect("window is open");
        cx.run_until_parked();
        popover.update(cx, |popover, _| {
            assert_eq!(popover.rows.len(), 5, "every entry still matches \"a\"");
            assert_eq!(
                popover.cursor.index(),
                1,
                "rebuilding the rows must re-park the cursor on the first match, \
                 not leave it wherever it happened to be"
            );
        });

        window
            .update(cx, |_, window, cx| {
                popover.read(cx).query.clone().update(cx, |editor, cx| {
                    editor.set_text("origin", window, cx);
                });
            })
            .expect("window is open");
        cx.run_until_parked();

        popover.update(cx, |popover, _| {
            assert_eq!(popover.rows.len(), 2, "one Remote header plus one match");
            assert_eq!(
                cursored(popover).as_deref(),
                Some("refs/remotes/origin/main"),
                "a narrowed list must re-park the cursor on the first match"
            );
        });
        press("confirm", cx);
        popover.update(cx, |popover, _| {
            assert!(
                popover
                    .selected
                    .contains(&SharedString::from("refs/remotes/origin/main")),
                "Confirm after narrowing must toggle the row now under the cursor"
            );
        });

        // Cancel (escape) dismisses — none of these popovers had that route.
        assert!(!dismissed.get());
        press("cancel", cx);
        assert!(dismissed.get(), "Cancel must dismiss the popover");
        dismissed.set(false);

        // SecondaryConfirm (ctrl-enter) is the keyboard route to Apply, which
        // commits the staged set and closes.
        press("apply", cx);
        assert!(
            dismissed.get(),
            "SecondaryConfirm must apply and dismiss the popover"
        );
    }
}
