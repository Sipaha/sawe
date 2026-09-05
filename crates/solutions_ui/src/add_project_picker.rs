//! "Add project to this solution" popover hosted by the project tab strip's + button.
//!
//! Shape: search input → "+ Create new project in solution…" and
//! "Add new project from git…" entries → catalog rows (filtered to
//! projects not already members). Confirm on an action row dispatches
//! `CreateNewProjectInSolution` / `AddCatalogProject`; confirm on a
//! catalog row takes the `SolutionStore::add_member` clone path.
//!
//! `up`/`down` move the selection and `enter` confirms it; typing parks
//! the selection on the first matching catalog row so `enter` adds a
//! match rather than one of the two action rows.

use gpui::{
    AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Task, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use solutions::{CatalogId, CatalogProject, SolutionId, SolutionStore, default_cache_root};
use std::sync::Arc;
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;

/// Cap for the scrollable match list — the registry can hold dozens of
/// projects, which otherwise blows the popover open to full screen height.
const LIST_MAX_HEIGHT_REMS: f32 = 18.0;

pub struct AddProjectPicker {
    picker: Entity<Picker<AddProjectDelegate>>,
}

impl AddProjectPicker {
    pub fn new(solution_id: SolutionId, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = AddProjectDelegate::new(solution_id, cx.entity().downgrade(), cx);
        let picker = cx.new(|cx| {
            Picker::list(delegate, window, cx)
                // `modal(true)` would stack an `elevation_3` shell inside
                // the popover this view already paints, and would make the
                // search editor losing focus dismiss the popover.
                // `PopoverMenu` already dismisses on an outside mouse-down.
                .modal(false)
                .show_scrollbar(true)
                .max_height(Some(rems(LIST_MAX_HEIGHT_REMS).into()))
        });
        Self { picker }
    }
}

impl EventEmitter<DismissEvent> for AddProjectPicker {}

impl Focusable for AddProjectPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for AddProjectPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ActiveProjectAddPicker")
            .w(rems(34.))
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(self.picker.clone())
    }
}

/// The two action rows live *inside* the match list rather than in
/// [`PickerDelegate::render_header`] for two reasons: a header is only
/// painted while `match_count() > 0`, so they would vanish exactly when the
/// filter matched nothing and the user most needs them; and as matches they
/// are reachable with the arrow keys. They are pinned at candidate indices
/// 0 and 1 so they still render above the catalog, as they always have.
enum PickerEntry {
    CreateEmptyProject,
    AddProjectFromGit,
    Catalog(CatalogProject),
}

/// What `enter` would act on. Split out of `confirm` so the "which row does
/// Enter target" contract is assertable without actually mutating the store
/// or dispatching an action.
#[derive(Debug, PartialEq, Eq)]
enum ConfirmTarget {
    CreateEmptyProject,
    AddProjectFromGit,
    AddCatalog(CatalogId),
}

pub struct AddProjectDelegate {
    solution_id: SolutionId,
    popover: WeakEntity<AddProjectPicker>,
    /// Indices 0 and 1 are always the two action rows.
    candidates: Vec<PickerEntry>,
    matches: Vec<usize>,
    selected_index: usize,
}

impl AddProjectDelegate {
    fn new(solution_id: SolutionId, popover: WeakEntity<AddProjectPicker>, cx: &mut App) -> Self {
        let store = SolutionStore::global(cx);
        let catalog_entries = store.read_with(cx, |s, _| {
            let already_member: collections::HashSet<CatalogId> = s
                .find_solution(solution_id)
                .map(|sol| {
                    sol.members
                        .iter()
                        .filter_map(|m| m.origin_catalog_id)
                        .collect()
                })
                .unwrap_or_default();
            s.catalog()
                .iter()
                .filter(|catalog_project| !already_member.contains(&catalog_project.id))
                .cloned()
                .collect::<Vec<_>>()
        });
        let candidates: Vec<PickerEntry> = [
            PickerEntry::CreateEmptyProject,
            PickerEntry::AddProjectFromGit,
        ]
        .into_iter()
        .chain(catalog_entries.into_iter().map(PickerEntry::Catalog))
        .collect();
        let matches = (0..candidates.len()).collect();
        let mut this = Self {
            solution_id,
            popover,
            candidates,
            matches,
            selected_index: 0,
        };
        this.selected_index = this.first_catalog_match();
        this
    }

    /// Position within `matches` of the first catalog row, falling back to
    /// the first action row when the filter matched no catalog entry.
    fn first_catalog_match(&self) -> usize {
        self.matches
            .iter()
            .position(|index| matches!(self.candidates.get(*index), Some(PickerEntry::Catalog(_))))
            .unwrap_or(0)
    }

    fn confirm_target(&self) -> Option<ConfirmTarget> {
        let candidate_index = *self.matches.get(self.selected_index)?;
        match self.candidates.get(candidate_index)? {
            PickerEntry::CreateEmptyProject => Some(ConfirmTarget::CreateEmptyProject),
            PickerEntry::AddProjectFromGit => Some(ConfirmTarget::AddProjectFromGit),
            PickerEntry::Catalog(catalog_project) => {
                Some(ConfirmTarget::AddCatalog(catalog_project.id))
            }
        }
    }

    fn add_catalog(&mut self, catalog_id: CatalogId, cx: &mut Context<Picker<Self>>) {
        let cache_root = default_cache_root();
        let solution_id = self.solution_id;
        let store = SolutionStore::global(cx);
        let task = store.update(cx, |s, cx| {
            s.add_member(solution_id, catalog_id, cache_root, cx)
        });
        cx.spawn(async move |_, _| task.await)
            .detach_and_log_err(cx);
    }
}

impl PickerDelegate for AddProjectDelegate {
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

    // Divider between the action rows and the catalog below them.
    fn separators_after_indices(&self) -> Vec<usize> {
        vec![1]
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
                // The action rows are the escape hatch when the query
                // matches nothing, so they never filter out.
                PickerEntry::CreateEmptyProject | PickerEntry::AddProjectFromGit => true,
                // Name only. Matching the remote URL too made every query
                // that happened to appear in a host or group path
                // ("gitlab", "citeck") return the whole registry, which is
                // exactly when the filter is needed most. The URL is still
                // shown in the row's end slot.
                PickerEntry::Catalog(catalog_project) => {
                    query.is_empty() || catalog_project.name.to_lowercase().contains(&query)
                }
            })
            .map(|(index, _)| index)
            .collect();
        // Park the selection on the first matching catalog row so `enter`
        // adds a match rather than one of the action rows.
        self.selected_index = self.first_catalog_match();
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(target) = self.confirm_target() else {
            return;
        };
        let solution_id = self.solution_id.0;
        // Dismiss before dispatching: the action rows hand off to a modal
        // that opens on the workspace's modal layer, and leaving the
        // popover mounted stacks the two.
        self.dismissed(window, cx);
        match target {
            ConfirmTarget::CreateEmptyProject => window.dispatch_action(
                Box::new(crate::actions::CreateNewProjectInSolution { solution_id }),
                cx,
            ),
            ConfirmTarget::AddProjectFromGit => window.dispatch_action(
                Box::new(crate::actions::AddCatalogProject {
                    solution_id: Some(solution_id),
                }),
                cx,
            ),
            ConfirmTarget::AddCatalog(catalog_id) => self.add_catalog(catalog_id, cx),
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.popover
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let candidate_index = *self.matches.get(ix)?;
        let item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected);
        Some(match self.candidates.get(candidate_index)? {
            PickerEntry::CreateEmptyProject => item
                .start_slot(
                    Icon::new(IconName::Plus)
                        .color(Color::Accent)
                        .size(IconSize::Small),
                )
                .child(Label::new("Create new project in solution…").color(Color::Accent)),
            PickerEntry::AddProjectFromGit => item
                .start_slot(
                    Icon::new(IconName::GitBranch)
                        .color(Color::Accent)
                        .size(IconSize::Small),
                )
                .child(Label::new("Add new project from git…").color(Color::Accent)),
            PickerEntry::Catalog(catalog_project) => {
                let label = SharedString::from(catalog_project.name.clone());
                let url = SharedString::from(catalog_project.remote_url.clone());
                item.child(Label::new(label).truncate()).end_slot(
                    Label::new(url)
                        .color(Color::Muted)
                        .size(LabelSize::Small)
                        .truncate(),
                )
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use tempfile::TempDir;

    fn catalog(id: u64, name: &str) -> PickerEntry {
        PickerEntry::Catalog(CatalogProject {
            id: CatalogId(id as i64),
            name: name.into(),
            remote_url: format!("git@example.com:group/{name}.git"),
            default_branch: None,
        })
    }

    /// Builds a popover over an empty registry, then installs `entries`
    /// after the two action rows by hand — the catalog has no non-test
    /// mutator, and the keyboard path only cares about the entry sequence.
    fn build_picker(
        entries: Vec<PickerEntry>,
        cx: &mut TestAppContext,
    ) -> (
        Entity<Picker<AddProjectDelegate>>,
        TempDir,
        &mut VisualTestContext,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            let store = SolutionStore::for_test(dir.path().join("catalog.json"), cx);
            solutions::install_global_for_test(store, cx);
        });

        let (popover, cx) =
            cx.add_window_view(|window, cx| AddProjectPicker::new(SolutionId(1), window, cx));
        let picker = popover.read_with(cx, |popover, _| popover.picker.clone());
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.candidates.extend(entries);
            picker.refresh(window, cx);
        });
        (picker, dir, cx)
    }

    #[gpui::test]
    async fn enter_confirms_the_first_filtered_match(cx: &mut TestAppContext) {
        let (picker, _dir, cx) = build_picker(
            vec![
                catalog(1, "alpha"),
                catalog(2, "bundles"),
                catalog(3, "bunker"),
            ],
            cx,
        );

        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches("bun".into(), window, cx);
        });

        picker.update(cx, |picker, _| {
            assert_eq!(
                picker.delegate.match_count(),
                4,
                "the two action rows plus the two `bun` catalog entries survive the filter"
            );
            assert_eq!(
                picker.delegate.confirm_target(),
                Some(ConfirmTarget::AddCatalog(CatalogId(2))),
                "enter must add the FIRST matching catalog entry, not an action row"
            );
        });
    }

    #[gpui::test]
    async fn down_then_enter_confirms_the_second_match(cx: &mut TestAppContext) {
        let (picker, _dir, cx) = build_picker(
            vec![
                catalog(1, "alpha"),
                catalog(2, "bundles"),
                catalog(3, "bunker"),
            ],
            cx,
        );

        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches("bun".into(), window, cx);
            picker.select_next(&menu::SelectNext, window, cx);
        });

        picker.update(cx, |picker, _| {
            assert_eq!(
                picker.delegate.confirm_target(),
                Some(ConfirmTarget::AddCatalog(CatalogId(3))),
                "down must move from the first matching catalog entry to the second"
            );
        });
    }

    #[gpui::test]
    async fn up_from_the_first_match_lands_on_the_last_action_row(cx: &mut TestAppContext) {
        let (picker, _dir, cx) = build_picker(vec![catalog(1, "bundles")], cx);

        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches("bun".into(), window, cx);
            picker.editor_move_up(&Default::default(), window, cx);
        });

        picker.update(cx, |picker, _| {
            assert_eq!(
                picker.delegate.selected_index(),
                1,
                "up from the first catalog row must land on the git action row, not off the list"
            );
            assert_eq!(
                picker.delegate.confirm_target(),
                Some(ConfirmTarget::AddProjectFromGit)
            );
        });
    }

    #[gpui::test]
    async fn enter_with_no_matches_targets_the_first_action_row(cx: &mut TestAppContext) {
        let (picker, _dir, cx) = build_picker(vec![catalog(1, "alpha"), catalog(2, "bundles")], cx);

        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches("zzz-no-such-project".into(), window, cx);
            // Arrow keys over a list holding only the action rows must not
            // wander off them (or panic on an out-of-range index).
            picker.select_next(&menu::SelectNext, window, cx);
            picker.editor_move_up(&Default::default(), window, cx);
        });

        picker.update(cx, |picker, _| {
            assert_eq!(picker.delegate.match_count(), 2);
            assert_eq!(
                picker.delegate.confirm_target(),
                Some(ConfirmTarget::CreateEmptyProject),
                "with nothing matched, enter must fall back to the create row rather than adding an arbitrary project"
            );
        });
    }
}
