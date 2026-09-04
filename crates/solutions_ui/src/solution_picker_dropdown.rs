//! Popover behind the title-bar `+` button.
//!
//! Lists solutions in the catalog that are not currently open in any
//! window (sorted by `last_opened_at` desc, nulls last). Row 0 is a
//! "Create new solution…" entry; every solution row carries a trash icon
//! that opens [`crate::delete_confirm_modal::DeleteConfirmModal`]. The
//! search input is autofocused on open and filters rows case-insensitively
//! as the user types; `up`/`down` move the selection and `enter` confirms
//! it.
//!
//! Wired into the title-bar by Task 7 (`SolutionTabStrip`); kept in its
//! own modal-style entity so the strip can `toggle_modal` it without
//! rebuilding the picker on every rerender.

use gpui::{
    AnyElement, DismissEvent, Div, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    Task, WeakEntity, px,
};
use picker::{Picker, PickerDelegate};
use solutions::{Solution, SolutionId, SolutionStore, SolutionStoreEvent};
use std::path::PathBuf;
use std::sync::Arc;
use ui::{Divider, IconButtonShape, ListItem, ListItemSpacing, Tooltip, prelude::*};
use ui_input::ErasedEditor;
use util::ResultExt as _;
use workspace::{ModalView, MultiWorkspace, Workspace};

use crate::delete_confirm_modal::{DeleteConfirmItem, open_delete_confirm};
use crate::modals::NewSolutionModal;
use crate::open::{OpenIntent, open_solution};
use crate::window_helpers::is_solution_open_anywhere;

/// Width of the popover. Rows fill this width so the trash icon sits
/// flush against the right edge instead of hugging the (short) label.
const POPOVER_WIDTH: f32 = 320.0;
/// Cap for the scrollable match list alone (the search row and the
/// "no matches" footer sit outside it).
const LIST_MAX_HEIGHT: f32 = 320.0;

pub struct SolutionPickerDropdown {
    picker: Entity<Picker<SolutionPickerDelegate>>,
    _store_subscription: Subscription,
}

#[derive(Clone)]
struct ClosedSolutionRow {
    id: SolutionId,
    name: SharedString,
    root: PathBuf,
}

/// The create row lives *inside* the match list rather than in
/// [`PickerDelegate::render_header`] for two reasons: a header is only
/// painted while `match_count() > 0`, so it would vanish exactly when the
/// filter matched nothing and the user most needs it; and as a match it is
/// reachable with the arrow keys. It is pinned at candidate index 0 so it
/// still renders above the solutions, as it always has.
enum PickerEntry {
    CreateSolution,
    Solution(ClosedSolutionRow),
}

/// What `enter` would act on. Split out of `confirm` so the "which row
/// does Enter target" contract is assertable without actually opening a
/// solution or a modal.
#[derive(Debug, PartialEq, Eq)]
enum ConfirmTarget {
    CreateSolution,
    Open(SolutionId),
}

impl SolutionPickerDropdown {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        multi_workspace: WeakEntity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate =
            SolutionPickerDelegate::new(workspace, multi_workspace, cx.entity().downgrade(), cx);
        let picker = cx.new(|cx| {
            Picker::list(delegate, window, cx)
                // `modal(true)` would wrap the list in `elevation_3`, a
                // modal shell stacked inside the popover this view already
                // paints, and would make the search editor losing focus
                // dismiss the popover. `PopoverMenu` already dismisses on
                // an outside mouse-down.
                .modal(false)
                .show_scrollbar(true)
                .max_height(Some(px(LIST_MAX_HEIGHT).into()))
        });

        // Refresh the closed-solutions list whenever the store mutates
        // (solutions added / removed / renamed, or members changing in a
        // way that flips a solution's open-anywhere status).
        let store = SolutionStore::global(cx);
        let store_subscription = cx.subscribe_in(
            &store,
            window,
            |this, _, _event: &SolutionStoreEvent, window, cx| {
                this.picker.update(cx, |picker, cx| {
                    picker.delegate.reload(cx);
                    picker.refresh(window, cx);
                });
            },
        );

        Self {
            picker,
            _store_subscription: store_subscription,
        }
    }
}

impl ModalView for SolutionPickerDropdown {
    fn debug_kind(&self) -> &'static str {
        "SolutionPickerDropdown"
    }
}

impl EventEmitter<DismissEvent> for SolutionPickerDropdown {}

impl Focusable for SolutionPickerDropdown {
    // Hand the picker's (i.e. the search editor's) focus handle out so the
    // popover layer can park focus on it on open — that's the autofocus
    // contract.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for SolutionPickerDropdown {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("SolutionPickerDropdown")
            .w(px(POPOVER_WIDTH))
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(self.picker.clone())
    }
}

pub struct SolutionPickerDelegate {
    workspace: WeakEntity<Workspace>,
    multi_workspace: WeakEntity<MultiWorkspace>,
    dropdown: WeakEntity<SolutionPickerDropdown>,
    /// Index 0 is always [`PickerEntry::CreateSolution`].
    candidates: Vec<PickerEntry>,
    matches: Vec<usize>,
    selected_index: usize,
}

impl SolutionPickerDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        multi_workspace: WeakEntity<MultiWorkspace>,
        dropdown: WeakEntity<SolutionPickerDropdown>,
        cx: &mut App,
    ) -> Self {
        let mut this = Self {
            workspace,
            multi_workspace,
            dropdown,
            candidates: vec![PickerEntry::CreateSolution],
            matches: vec![0],
            selected_index: 0,
        };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut App) {
        let rows = closed_solution_rows(&self.multi_workspace, cx);
        self.candidates = std::iter::once(PickerEntry::CreateSolution)
            .chain(rows.into_iter().map(PickerEntry::Solution))
            .collect();
        self.matches = (0..self.candidates.len()).collect();
        self.selected_index = self.first_solution_match();
    }

    /// Position within `matches` of the first real solution, falling back
    /// to the create row when the filter matched no solution at all.
    fn first_solution_match(&self) -> usize {
        self.matches
            .iter()
            .position(|index| matches!(self.candidates.get(*index), Some(PickerEntry::Solution(_))))
            .unwrap_or(0)
    }

    fn confirm_target(&self) -> Option<ConfirmTarget> {
        let candidate_index = *self.matches.get(self.selected_index)?;
        match self.candidates.get(candidate_index)? {
            PickerEntry::CreateSolution => Some(ConfirmTarget::CreateSolution),
            PickerEntry::Solution(row) => Some(ConfirmTarget::Open(row.id)),
        }
    }

    /// True when nothing but the create row survived the filter.
    fn no_solution_matches(&self) -> bool {
        self.matches.len() <= 1
    }

    fn open_create(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        // cx.dispatch_action(&NewSolution) used to be the implementation,
        // but the dropdown is rendered as a popover and isn't in the
        // workspace's focus tree — so the workspace's register_action
        // handler never fires and the click silently does nothing.
        // Open the modal directly via the workspace handle we already
        // hold (same approach used by the welcome-window delete flow
        // fix in 8c7d87c931).
        self.dismissed(window, cx);
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

    fn ask_delete(
        &mut self,
        row: ClosedSolutionRow,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        // Dismiss the dropdown first — the confirm modal toggles on the
        // workspace's modal layer, and leaving this picker mounted while
        // a confirm modal opens above it stacks two modals on the same
        // layer. Dispatching through the `DeleteSolutionFromTabBar`
        // action handler would do the same modal but force us to keep
        // the dropdown around long enough for the action to fire; calling
        // `open_delete_confirm` directly lets us dismiss first.
        self.dismissed(window, cx);
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

/// Solutions the user could still open from this window: everything in the
/// registry that isn't already open somewhere, most-recently-opened first.
fn closed_solution_rows(
    multi_workspace: &WeakEntity<MultiWorkspace>,
    cx: &App,
) -> Vec<ClosedSolutionRow> {
    // `is_solution_open_anywhere` skips the window currently on the
    // stack, so solutions only-open-in-our-window slip through. Build
    // an explicit "open in this window's MW" set from the source MW
    // handle and exclude those too.
    let open_in_this_window: std::collections::HashSet<SolutionId> = multi_workspace
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
                                .map(|sol| sol.id)
                        })
                })
                .collect()
        })
        .unwrap_or_default();

    let store = SolutionStore::global(cx);
    let mut rows: Vec<(Option<i64>, ClosedSolutionRow)> = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .filter(|sol: &&Solution| {
                !is_solution_open_anywhere(sol.id, cx) && !open_in_this_window.contains(&sol.id)
            })
            .map(|sol| {
                (
                    sol.last_opened_at,
                    ClosedSolutionRow {
                        id: sol.id,
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
    rows.into_iter().map(|(_, row)| row).collect()
}

impl PickerDelegate for SolutionPickerDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    // Draws the divider that has always separated the create row from the
    // solutions below it.
    fn separators_after_indices(&self) -> Vec<usize> {
        vec![0]
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        self.matches = self
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, entry)| match entry {
                // The create row is the escape hatch when the query matches
                // nothing, so it never filters out.
                PickerEntry::CreateSolution => true,
                PickerEntry::Solution(row) => {
                    query.is_empty() || row.name.to_lowercase().contains(&query)
                }
            })
            .map(|(index, _)| index)
            .collect();
        // Park the selection on the first matching solution so `enter`
        // opens a match rather than the create row; the create row is the
        // fallback only when nothing else matched.
        self.selected_index = self.first_solution_match();
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        match self.confirm_target() {
            Some(ConfirmTarget::Open(id)) => {
                self.dismissed(window, cx);
                let source = window.window_handle().downcast();
                open_solution(id, source, OpenIntent::SameWindow, cx);
            }
            Some(ConfirmTarget::CreateSolution) => self.open_create(window, cx),
            None => {}
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.dropdown
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_editor(
        &self,
        editor: &Arc<dyn ErasedEditor>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Div {
        // Compact search row. The h_flex carries fixed height + the
        // editor's background/border — the picker's single-line editor
        // paints on a transparent background, so without this wrapper the
        // typed text overlaid on `elevated_surface_background` was
        // barely visible. It also has to restate `flex_none().h_7()`,
        // which the default `render_editor` supplies and which guarantees
        // the EditorElement gets a non-zero height even when the popover's
        // max height clamps the column.
        v_flex()
            .child(
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
                    .child(div().flex_1().min_w_0().child(editor.render(window, cx)))
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(Divider::horizontal())
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let candidate_index = *self.matches.get(ix)?;
        let item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected);
        Some(match self.candidates.get(candidate_index)? {
            PickerEntry::CreateSolution => item
                .start_slot(
                    Icon::new(IconName::Plus)
                        .size(IconSize::Small)
                        .color(Color::Accent),
                )
                .child(Label::new("Create new solution…").color(Color::Accent)),
            PickerEntry::Solution(row) => {
                let row = row.clone();
                item.child(Label::new(row.name.clone()).truncate())
                    .end_slot_on_hover(
                        IconButton::new(("solution-picker-delete", ix), IconName::Trash)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Delete solution"))
                            .on_click(cx.listener(move |picker, _, window, cx| {
                                picker.delegate.ask_delete(row.clone(), window, cx);
                            })),
                    )
            }
        })
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        if !self.no_solution_matches() {
            return None;
        }
        let message = if self.candidates.len() == 1 {
            "No closed solutions"
        } else {
            "No matches"
        };
        Some(
            div()
                .w_full()
                .px_2()
                .pb_1p5()
                .child(
                    Label::new(message)
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use gpui::{TestAppContext, VisualTestContext};
    use tempfile::TempDir;

    /// Mirrors the sort logic inside `closed_solution_rows` so we can
    /// validate it in isolation. `(last_opened, name)` pairs in / `name`s
    /// in expected order out.
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
    /// the picker's single-line editor paints on a transparent background,
    /// so the container needs to supply contrast). Overriding
    /// `render_editor` also drops the framework's own `flex_none()/h_9()`,
    /// so this row has to restate an explicit height.
    #[test]
    fn search_row_has_magnifier_after_editor_and_uses_editor_background() {
        let src = include_str!("solution_picker_dropdown.rs");
        // The search row is the body of `render_editor`; end the segment at
        // the next method in the impl block so the boundary is
        // whitespace-insensitive.
        let row_start = src
            .find("// Compact search row")
            .expect("search row comment exists");
        let row_segment = &src[row_start..];
        let end_marker = row_segment
            .find("\n    fn ")
            .expect("render_editor is followed by another method");
        let row = &row_segment[..end_marker];
        let editor_pos = row
            .find("editor.render(window, cx)")
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
            "search row must be flex_none so the popover's max height doesn't collapse it"
        );
    }

    #[test]
    fn filter_matches_substring_case_insensitive() {
        let rows = [
            ClosedSolutionRow {
                id: SolutionId(1),
                name: "Citeck Core".into(),
                root: PathBuf::from("/x/1"),
            },
            ClosedSolutionRow {
                id: SolutionId(2),
                name: "ECOS Records".into(),
                root: PathBuf::from("/x/2"),
            },
            ClosedSolutionRow {
                id: SolutionId(3),
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

    /// Builds a dropdown over a throwaway registry holding `names`, in
    /// registry order (no `last_opened_at`, so the sort keeps that order).
    /// The workspace / multi-workspace handles are deliberately invalid:
    /// nothing in the keyboard path needs them, and an invalid handle makes
    /// an accidental dependency on one fail loudly.
    fn build_dropdown<'a>(
        names: &[&str],
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<SolutionPickerDropdown>,
        Entity<Picker<SolutionPickerDelegate>>,
        TempDir,
        &'a mut VisualTestContext,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);

            let store = SolutionStore::for_test(dir.path().join("catalog.json"), cx);
            for name in names {
                store
                    .update(cx, |s, cx| {
                        s.create_solution(name, dir.path().join(name), cx)
                    })
                    .expect("create solution");
            }
            solutions::install_global_for_test(store, cx);
        });

        let (dropdown, cx) = cx.add_window_view(|window, cx| {
            SolutionPickerDropdown::new(
                WeakEntity::new_invalid(),
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        });
        let picker = dropdown.read_with(cx, |dropdown, _| dropdown.picker.clone());
        (dropdown, picker, dir, cx)
    }

    #[gpui::test]
    async fn enter_confirms_the_first_filtered_match(cx: &mut TestAppContext) {
        let (_dropdown, picker, _dir, cx) = build_dropdown(&["Alpha", "Bundles", "Bunker"], cx);

        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches("bun".into(), window, cx);
        });

        picker.update(cx, |picker, _| {
            assert_eq!(
                picker.delegate.match_count(),
                3,
                "the create row plus the two `bun` solutions survive the filter"
            );
            let id = match picker.delegate.confirm_target() {
                Some(ConfirmTarget::Open(id)) => id,
                other => panic!("enter must target a solution, got {other:?}"),
            };
            let ConfirmTarget::Open(expected) = first_solution_target(&picker.delegate) else {
                panic!("expected a solution among the matches");
            };
            assert_eq!(
                id, expected,
                "enter must open the FIRST matching solution, not the create row"
            );
        });
    }

    /// The id of the first solution in `matches`, i.e. what "the first
    /// match" means independently of `selected_index`.
    fn first_solution_target(delegate: &SolutionPickerDelegate) -> ConfirmTarget {
        for candidate_index in &delegate.matches {
            if let Some(PickerEntry::Solution(row)) = delegate.candidates.get(*candidate_index) {
                return ConfirmTarget::Open(row.id);
            }
        }
        ConfirmTarget::CreateSolution
    }

    #[gpui::test]
    async fn down_then_enter_confirms_the_second_match(cx: &mut TestAppContext) {
        let (_dropdown, picker, _dir, cx) = build_dropdown(&["Alpha", "Bundles", "Bunker"], cx);

        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches("bun".into(), window, cx);
        });
        let first = picker.update(cx, |picker, _| picker.delegate.confirm_target());

        picker.update_in(cx, |picker, window, cx| {
            picker.select_next(&menu::SelectNext, window, cx);
        });
        let second = picker.update(cx, |picker, _| picker.delegate.confirm_target());

        assert!(
            matches!(second, Some(ConfirmTarget::Open(_))),
            "down must land on the second matching solution, got {second:?}"
        );
        assert_ne!(
            first, second,
            "down must move the selection off the first match"
        );
    }

    #[gpui::test]
    async fn up_from_the_first_match_lands_on_the_create_row(cx: &mut TestAppContext) {
        let (_dropdown, picker, _dir, cx) = build_dropdown(&["Bundles", "Bunker"], cx);

        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches("bun".into(), window, cx);
            picker.editor_move_up(&Default::default(), window, cx);
        });

        picker.update(cx, |picker, _| {
            assert_eq!(
                picker.delegate.selected_index(),
                0,
                "up from the first solution must land on the create row, not off the list"
            );
            assert_eq!(
                picker.delegate.confirm_target(),
                Some(ConfirmTarget::CreateSolution)
            );
        });
    }

    #[gpui::test]
    async fn enter_with_no_matches_targets_the_create_row(cx: &mut TestAppContext) {
        let (_dropdown, picker, _dir, cx) = build_dropdown(&["Alpha", "Bundles"], cx);

        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches("zzz-no-such-solution".into(), window, cx);
            // Arrow keys on a list holding only the create row must not
            // wander off it (or panic on an out-of-range index).
            picker.select_next(&menu::SelectNext, window, cx);
            picker.editor_move_up(&Default::default(), window, cx);
        });

        picker.update(cx, |picker, _| {
            assert_eq!(picker.delegate.match_count(), 1);
            assert_eq!(
                picker.delegate.confirm_target(),
                Some(ConfirmTarget::CreateSolution),
                "with nothing matched, enter must fall back to the create row rather than opening an arbitrary solution"
            );
        });
    }
}
