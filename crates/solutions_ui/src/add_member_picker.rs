use std::sync::Arc;

use gpui::{
    AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity,
};
use picker::{Picker, PickerDelegate};
use settings::Settings as _;
use solutions::{CatalogId, SolutionId, SolutionStore, SolutionsSettings, default_cache_root};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::actions::AddCatalogProject;

pub struct AddMemberPicker {
    picker: Entity<Picker<AddMemberDelegate>>,
}

impl AddMemberPicker {
    pub fn open(
        workspace: &mut Workspace,
        solution_id: SolutionId,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let weak = cx.weak_entity();
        workspace.toggle_modal(window, cx, move |window, cx| {
            AddMemberPicker::new(weak, solution_id, window, cx)
        });
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        solution_id: SolutionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = AddMemberDelegate::new(workspace, cx.entity().downgrade(), solution_id, cx);
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for AddMemberPicker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

impl Focusable for AddMemberPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for AddMemberPicker {}
impl ModalView for AddMemberPicker {
    fn debug_kind(&self) -> &'static str {
        "AddMember"
    }
}

struct CatalogEntry {
    id: CatalogId,
    name: String,
    remote_url: String,
}

// The picker shows real catalog entries plus a synthetic "+" row so the
// user can jump to AddCatalogProject without first dismissing this modal.
// Without it, an empty catalog would dead-end the flow ("nothing to pick"
// with no path forward).
enum PickerEntry {
    Catalog(CatalogEntry),
    AddNew,
}

pub struct AddMemberDelegate {
    workspace: WeakEntity<Workspace>,
    modal: WeakEntity<AddMemberPicker>,
    solution_id: SolutionId,
    candidates: Vec<PickerEntry>,
    matches: Vec<usize>,
    selected_index: usize,
}

impl AddMemberDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        modal: WeakEntity<AddMemberPicker>,
        solution_id: SolutionId,
        cx: &mut App,
    ) -> Self {
        let store = SolutionStore::global(cx);
        let mut candidates: Vec<PickerEntry> = store.read_with(cx, |s, _| {
            let already_in_solution: std::collections::HashSet<CatalogId> = s
                .find_solution(solution_id)
                .map(|sol| sol.members.iter().filter_map(|m| m.origin_catalog_id).collect())
                .unwrap_or_default();
            s.catalog()
                .iter()
                .filter(|c| !already_in_solution.contains(&c.id))
                .map(|c| {
                    PickerEntry::Catalog(CatalogEntry {
                        id: c.id,
                        name: c.name.clone(),
                        remote_url: c.remote_url.clone(),
                    })
                })
                .collect()
        });
        candidates.push(PickerEntry::AddNew);
        let matches = (0..candidates.len()).collect();
        Self {
            workspace,
            modal,
            solution_id,
            candidates,
            matches,
            selected_index: 0,
        }
    }
}

impl PickerDelegate for AddMemberDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search catalog…".into()
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

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        self.matches = if query.is_empty() {
            (0..self.candidates.len()).collect()
        } else {
            self.candidates
                .iter()
                .enumerate()
                .filter(|(_, e)| match e {
                    PickerEntry::Catalog(c) => {
                        c.name.to_lowercase().contains(&query)
                            || c.remote_url.to_lowercase().contains(&query)
                    }
                    // Always keep the "+ Add new project to catalog" entry
                    // visible — it is the escape hatch when the search misses
                    // and the user wants to add what they were looking for.
                    PickerEntry::AddNew => true,
                })
                .map(|(i, _)| i)
                .collect()
        };
        if self.selected_index >= self.matches.len() {
            self.selected_index = 0;
        }
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(&idx) = self.matches.get(self.selected_index) else {
            return;
        };
        let Some(entry) = self.candidates.get(idx) else {
            return;
        };
        match entry {
            PickerEntry::Catalog(catalog) => {
                let cat_id = catalog.id;
                let sol_id = self.solution_id;
                let cache_root = default_cache_root();
                let solutions_root = SolutionsSettings::get_global(cx).root.clone();
                let _ = solutions_root;
                let store = SolutionStore::global(cx);
                let task = store.update(cx, |s, cx| s.add_member(sol_id, cat_id, cache_root, cx));
                cx.spawn(async move |_, _cx| task.await)
                    .detach_and_log_err(cx);
                self.dismissed(window, cx);
            }
            PickerEntry::AddNew => {
                // Hand off to the AddCatalogProject modal, carrying this
                // solution so the new project is added to it (cloning in the
                // background) as soon as it's in the catalog.
                let solution_id = self.solution_id.0;
                self.dismissed(window, cx);
                let Some(workspace) = self.workspace.upgrade() else {
                    return;
                };
                workspace.update(cx, |_, cx| {
                    window.dispatch_action(
                        Box::new(AddCatalogProject {
                            solution_id: Some(solution_id),
                        }),
                        cx,
                    );
                });
            }
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry_idx = *self.matches.get(ix)?;
        let entry = self.candidates.get(entry_idx)?;
        let item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected);
        let item = match entry {
            PickerEntry::Catalog(c) => item.child(Label::new(c.name.clone())).end_slot(
                Label::new(c.remote_url.clone())
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            ),
            PickerEntry::AddNew => item
                .start_slot(
                    Icon::new(IconName::Plus)
                        .color(Color::Muted)
                        .size(IconSize::Small),
                )
                .child(Label::new("Add new project to catalog…").color(Color::Accent)),
        };
        Some(item)
    }
}
