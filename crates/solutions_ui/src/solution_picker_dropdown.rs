//! Popover behind the title-bar `+` button.
//!
//! Lists solutions in the catalog that are not currently open in any
//! window (sorted by `last_opened_at` desc, nulls last). Has a leading
//! "Create new solution…" row and a trash icon on each row that opens
//! [`crate::delete_confirm_modal::DeleteConfirmModal`]. The search input
//! is autofocused on open and filters rows case-insensitively as the
//! user types.
//!
//! Wired into the title-bar by Task 7 (`SolutionTabStrip`); kept in its
//! own modal-style entity so the strip can `toggle_modal` it without
//! rebuilding the picker on every rerender.

use editor::{Editor, EditorEvent};
use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, WeakEntity, px,
};
use solutions::{Solution, SolutionId, SolutionStore, SolutionStoreEvent};
use std::path::PathBuf;
use ui::{IconButtonShape, Tooltip, prelude::*};
use workspace::{ModalView, MultiWorkspace, Workspace};

use crate::delete_confirm_modal::{DeleteConfirmItem, open_delete_confirm};
use crate::modals::NewSolutionModal;
use crate::open::{OpenIntent, open_solution};
use crate::window_helpers::is_solution_open_anywhere;

/// Width of the popover. Rows fill this width so the trash icon sits
/// flush against the right edge instead of hugging the (short) label.
const POPOVER_WIDTH: f32 = 320.0;

pub struct SolutionPickerDropdown {
    workspace: WeakEntity<Workspace>,
    multi_workspace: WeakEntity<MultiWorkspace>,
    search_editor: Entity<Editor>,
    /// Cached at construction time so render reads it without re-borrowing
    /// the search editor (and so `track_focus` on the outer container can
    /// be set without calling `search_editor.focus_handle(cx)` during the
    /// render pass).
    focus_handle: FocusHandle,
    closed_solutions: Vec<ClosedSolutionRow>,
    _store_subscription: Subscription,
    _search_subscription: Subscription,
}

#[derive(Clone)]
struct ClosedSolutionRow {
    id: SolutionId,
    name: SharedString,
    root: PathBuf,
}

impl SolutionPickerDropdown {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        multi_workspace: WeakEntity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search…", window, cx);
            editor
        });

        // Re-render whenever the search query changes — the filter is
        // applied in-place on the `closed_solutions` snapshot so the
        // store subscription doesn't have to rerun for every keystroke.
        let search_subscription = cx.subscribe(&search_editor, |_, _, event: &EditorEvent, cx| {
            if matches!(
                event,
                EditorEvent::Edited { .. } | EditorEvent::BufferEdited
            ) {
                cx.notify();
            }
        });

        // Refresh the closed-solutions list whenever the store mutates
        // (solutions added / removed / renamed, or members changing in a
        // way that flips a solution's open-anywhere status).
        let store = SolutionStore::global(cx);
        let store_subscription =
            cx.subscribe(&store, |this, _, _event: &SolutionStoreEvent, cx| {
                this.refresh(cx);
            });

        let focus_handle = search_editor.focus_handle(cx);
        let mut this = Self {
            workspace,
            multi_workspace,
            search_editor,
            focus_handle,
            closed_solutions: Vec::new(),
            _store_subscription: store_subscription,
            _search_subscription: search_subscription,
        };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        // `is_solution_open_anywhere` skips the window currently on the
        // stack, so solutions only-open-in-our-window slip through. Build
        // an explicit "open in this window's MW" set from the source MW
        // handle and exclude those too.
        let open_in_this_window: std::collections::HashSet<SolutionId> = self
            .multi_workspace
            .upgrade()
            .map(|mw| {
                mw.read(cx)
                    .workspaces()
                    .filter_map(|ws| {
                        let store = SolutionStore::try_global(cx)?;
                        let store = store.read(cx);
                        ws.read(cx)
                            .project()
                            .read(cx)
                            .worktrees(cx)
                            .find_map(|tree| {
                                store
                                    .solution_for_path(&tree.read(cx).abs_path())
                                    .map(|sol| sol.id.clone())
                            })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let store = SolutionStore::global(cx);
        let mut rows: Vec<(Option<chrono::DateTime<chrono::Utc>>, ClosedSolutionRow)> = store
            .read_with(cx, |s, _| {
                s.solutions()
                    .iter()
                    .filter(|sol: &&Solution| {
                        !is_solution_open_anywhere(&sol.id, cx)
                            && !open_in_this_window.contains(&sol.id)
                    })
                    .map(|sol| {
                        (
                            sol.last_opened_at,
                            ClosedSolutionRow {
                                id: sol.id.clone(),
                                name: SharedString::from(sol.name.clone()),
                                root: sol.root.clone(),
                            },
                        )
                    })
                    .collect()
            });
        // Most-recently-opened first; never-opened solutions go last in
        // their natural store order. Mirrors `welcome::all_solutions`
        // so the dropdown's row order matches what the user already
        // sees on the launcher.
        rows.sort_by(|a, b| match (a.0, b.0) {
            (Some(ts_a), Some(ts_b)) => ts_b.cmp(&ts_a),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
        self.closed_solutions = rows.into_iter().map(|(_, row)| row).collect();
        cx.notify();
    }

    fn filter_query(&self, cx: &App) -> String {
        self.search_editor.read(cx).text(cx).trim().to_lowercase()
    }

    fn filtered_rows<'a>(&'a self, cx: &App) -> Vec<&'a ClosedSolutionRow> {
        let query = self.filter_query(cx);
        if query.is_empty() {
            self.closed_solutions.iter().collect()
        } else {
            self.closed_solutions
                .iter()
                .filter(|row| row.name.to_lowercase().contains(&query))
                .collect()
        }
    }

    fn open_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // cx.dispatch_action(&NewSolution) used to be the implementation,
        // but the dropdown is rendered as a popover and isn't in the
        // workspace's focus tree — so the workspace's register_action
        // handler never fires and the click silently does nothing.
        // Open the modal directly via the workspace handle we already
        // hold (same approach used by the welcome-window delete flow
        // fix in 8c7d87c931).
        cx.emit(DismissEvent);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let weak = workspace.downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                NewSolutionModal::new(weak, window, cx)
            });
        });
    }

    fn open_row(&mut self, id: SolutionId, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        let source = window.window_handle().downcast();
        open_solution(id, source, OpenIntent::SameWindow, cx);
    }

    fn ask_delete(&mut self, row: ClosedSolutionRow, window: &mut Window, cx: &mut Context<Self>) {
        // Dismiss the dropdown first — the confirm modal toggles on the
        // workspace's modal layer, and leaving this picker mounted while
        // a confirm modal opens above it stacks two modals on the same
        // layer. Dispatching through the `DeleteSolutionFromTabBar`
        // action handler would do the same modal but force us to keep
        // the dropdown around long enough for the action to fire; calling
        // `open_delete_confirm` directly lets us emit `DismissEvent`
        // first.
        cx.emit(DismissEvent);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let ClosedSolutionRow { id, name, root } = row;
        workspace.update(cx, |workspace, cx| {
            let folder_label = SharedString::from(format!("Folder {}", root.display()));
            let root_for_cleanup = root.clone();
            open_delete_confirm(
                workspace,
                SharedString::from(format!("Delete solution \"{name}\"?")),
                "This will permanently delete:",
                vec![
                    DeleteConfirmItem {
                        label: "Registry entry".into(),
                        path: None,
                    },
                    DeleteConfirmItem {
                        label: folder_label,
                        path: Some(root),
                    },
                ],
                move |_window, cx| {
                    crate::delete_solution_with_cleanup(id, root_for_cleanup, cx);
                },
                window,
                cx,
            );
        });
    }
}

impl ModalView for SolutionPickerDropdown {
    fn debug_kind(&self) -> &'static str {
        "SolutionPickerDropdown"
    }
}

impl EventEmitter<DismissEvent> for SolutionPickerDropdown {}

impl Focusable for SolutionPickerDropdown {
    // Hand the search editor's focus handle out so the modal layer can
    // park focus on it on open — that's the autofocus contract.
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SolutionPickerDropdown {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<ClosedSolutionRow> = self.filtered_rows(cx).into_iter().cloned().collect();
        let row_count = rows.len();

        let row_elements: Vec<gpui::AnyElement> = rows
            .into_iter()
            .map(|row| {
                let row_id = SharedString::from(format!("solution-picker-row-{}", row.id.as_str()));
                let group_id =
                    SharedString::from(format!("solution-picker-group-{}", row.id.as_str()));
                let trash_id =
                    SharedString::from(format!("solution-picker-delete-{}", row.id.as_str()));
                let id_for_open = row.id.clone();
                let label = row.name.clone();
                h_flex()
                    .id(row_id)
                    .group(group_id.clone())
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_row(id_for_open.clone(), window, cx);
                    }))
                    .child(div().flex_1().min_w_0().child(Label::new(label).truncate()))
                    .child(
                        IconButton::new(trash_id, IconName::Trash)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .visible_on_hover(group_id)
                            .tooltip(Tooltip::text("Delete solution"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.ask_delete(row.clone(), window, cx);
                            })),
                    )
                    .into_any_element()
            })
            .chain((row_count == 0).then(|| {
                div()
                    .w_full()
                    .px_2()
                    .py_2()
                    .child(
                        Label::new(if self.closed_solutions.is_empty() {
                            "No closed solutions"
                        } else {
                            "No matches"
                        })
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                    )
                    .into_any_element()
            }))
            .collect();

        v_flex()
            .key_context("SolutionPickerDropdown")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| {
                cx.emit(DismissEvent);
            }))
            .w(px(POPOVER_WIDTH))
            .max_h(px(360.0))
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(
                // Compact search row. The h_flex carries fixed height + the
                // editor's background/border — `Editor::single_line` paints
                // on a transparent background, so without this wrapper the
                // typed text overlaid on `elevated_surface_background` was
                // barely visible. Also matches the `picker::render_editor`
                // pattern (`flex_none().h_7().overflow_hidden()`), which
                // guarantees the EditorElement gets a non-zero height even
                // when the popover's `max_h` clamps the column.
                h_flex()
                    .m_1p5()
                    .px_2()
                    .h_7()
                    .gap_1p5()
                    .flex_none()
                    .items_center()
                    .overflow_hidden()
                    .rounded_sm()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(div().flex_1().min_w_0().child(self.search_editor.clone()))
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(div().h_px().bg(cx.theme().colors().border))
            .child(
                h_flex()
                    .id("solution-picker-create")
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                    .child(
                        Icon::new(IconName::Plus)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .child(Label::new("Create new solution…").color(Color::Accent))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_create(window, cx);
                    })),
            )
            .child(div().h_px().bg(cx.theme().colors().border))
            .child(
                div()
                    .id("solution-picker-list")
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(row_elements),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};

    /// Mirrors the sort logic inside `refresh()` so we can validate it in
    /// isolation. `(last_opened, name)` pairs in / `name`s in expected
    /// order out.
    fn sort_rows(
        mut rows: Vec<(Option<chrono::DateTime<chrono::Utc>>, &'static str)>,
    ) -> Vec<&'static str> {
        rows.sort_by(|a, b| match (a.0, b.0) {
            (Some(ts_a), Some(ts_b)) => ts_b.cmp(&ts_a),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
        rows.into_iter().map(|(_, name)| name).collect()
    }

    #[test]
    fn closed_solutions_sort_by_last_opened_desc_with_nulls_last() {
        let now = Utc.with_ymd_and_hms(2024, 1, 10, 12, 0, 0).unwrap();
        let earlier = now - Duration::hours(1);
        let earliest = now - Duration::days(1);
        let order = sort_rows(vec![
            (Some(earlier), "b-middle"),
            (None, "d-never-1"),
            (Some(now), "a-newest"),
            (None, "e-never-2"),
            (Some(earliest), "c-oldest"),
        ]);
        assert_eq!(
            order,
            vec!["a-newest", "b-middle", "c-oldest", "d-never-1", "e-never-2"]
        );
    }

    /// Pinned guarantee for the playtest tweak: the magnifier icon must
    /// render to the RIGHT of the editor inside the search row, AND the
    /// editor must sit inside a wrapper with an explicit `editor_background`
    /// (so the typed text doesn't sink into the popover's elevated surface,
    /// which was the cause of the "filter fires but text invisible" bug —
    /// `EditorMode::SingleLine` paints on a transparent background, so the
    /// container needs to supply contrast).
    #[test]
    fn search_row_has_magnifier_after_editor_and_uses_editor_background() {
        let src = include_str!("solution_picker_dropdown.rs");
        // The search row spans from the `// Compact search row` comment up to
        // the next sibling child — a 1px divider. The divider is the first
        // `.h_px()` after the comment (the search row itself never calls it),
        // so we use that as the end boundary — whitespace-insensitive, unlike
        // matching the divider's exact multi-line layout.
        let row_start = src
            .find("// Compact search row")
            .expect("search row comment exists");
        let row_segment = &src[row_start..];
        let end_marker = row_segment
            .find(".h_px()")
            .expect("search row ends before the divider div");
        let row = &row_segment[..end_marker];
        let editor_pos = row
            .find("self.search_editor.clone()")
            .expect("editor must be a child of the search row");
        let magnifier_pos = row
            .find("IconName::MagnifyingGlass")
            .expect("magnifier icon must be a child of the search row");
        assert!(
            magnifier_pos > editor_pos,
            "magnifier icon must come AFTER the editor in the children chain so it renders on the right edge of the row"
        );
        assert!(
            row.contains("bg(cx.theme().colors().editor_background)"),
            "search row must paint editor_background for typed-text contrast"
        );
        assert!(
            row.contains(".h_7()"),
            "search row must pin an explicit height so EditorElement gets a non-zero layout"
        );
        assert!(
            row.contains(".flex_none()"),
            "search row must be flex_none so the popover's max_h doesn't collapse it"
        );
    }

    #[test]
    fn filter_matches_substring_case_insensitive() {
        let rows = [
            ClosedSolutionRow {
                id: SolutionId("1".into()),
                name: "Citeck Core".into(),
                root: PathBuf::from("/x/1"),
            },
            ClosedSolutionRow {
                id: SolutionId("2".into()),
                name: "ECOS Records".into(),
                root: PathBuf::from("/x/2"),
            },
            ClosedSolutionRow {
                id: SolutionId("3".into()),
                name: "sawe".into(),
                root: PathBuf::from("/x/3"),
            },
        ];
        let query = "ECOS".to_lowercase();
        let matched: Vec<&str> = rows
            .iter()
            .filter(|r| r.name.to_lowercase().contains(&query))
            .map(|r| r.name.as_ref())
            .collect();
        assert_eq!(matched, vec!["ECOS Records"]);

        // Uppercase query matches a lowercase name case-insensitively.
        let query = "AW".to_lowercase();
        let matched: Vec<&str> = rows
            .iter()
            .filter(|r| r.name.to_lowercase().contains(&query))
            .map(|r| r.name.as_ref())
            .collect();
        assert_eq!(matched, vec!["sawe"]);
    }
}
