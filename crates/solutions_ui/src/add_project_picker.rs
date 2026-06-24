//! "Add project to this solution" popover hosted by the project tab strip's + button.
//!
//! Shape: search input → "+ Create new empty project in solution…"
//! entry → catalog rows (filtered to projects not already members).
//! Click create → dispatches CreateNewProjectInSolution { solution_id }.
//! Click catalog row → SolutionStore::add_member clone path.

use editor::Editor;
use gpui::{
    AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Window,
};
use solutions::{CatalogId, CatalogProject, SolutionId, SolutionStore, default_cache_root};
use ui::{ListItem, ListItemSpacing, prelude::*};

pub struct AddProjectPicker {
    solution_id: SolutionId,
    pub(crate) catalog_entries: Vec<CatalogProject>,
    search_editor: Entity<Editor>,
    query: String,
    focus_handle: FocusHandle,
    _editor_subscription: Subscription,
}

impl AddProjectPicker {
    pub fn new(solution_id: SolutionId, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = SolutionStore::global(cx);
        let catalog_entries = store.read_with(cx, |s, _| {
            let already_member: collections::HashSet<CatalogId> = s
                .solutions()
                .iter()
                .find(|sol| sol.id == solution_id)
                .map(|sol| sol.members.iter().map(|m| m.catalog_id.clone()).collect())
                .unwrap_or_default();
            s.catalog()
                .iter()
                .filter(|catalog_project| !already_member.contains(&catalog_project.id))
                .cloned()
                .collect::<Vec<_>>()
        });
        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search…", window, cx);
            editor
        });
        let editor_subscription = cx.subscribe(&search_editor, |this, _, event, cx| {
            if matches!(
                event,
                editor::EditorEvent::BufferEdited | editor::EditorEvent::Edited { .. }
            ) {
                let query = this.search_editor.read(cx).text(cx);
                this.query = query.trim().to_lowercase();
                cx.notify();
            }
        });
        let focus_handle = search_editor.focus_handle(cx);
        Self {
            solution_id,
            catalog_entries,
            search_editor,
            query: String::new(),
            focus_handle,
            _editor_subscription: editor_subscription,
        }
    }

    pub fn create_empty(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let solution_id = self.solution_id.clone();
        cx.emit(DismissEvent);
        window.dispatch_action(
            Box::new(crate::actions::CreateNewProjectInSolution {
                solution_id: solution_id.0,
            }),
            cx,
        );
    }

    pub fn add_from_git(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        window.dispatch_action(Box::new(crate::modals::AddCatalogProject), cx);
    }

    pub fn add_catalog(
        &mut self,
        catalog_project: CatalogProject,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cache_root = default_cache_root();
        let solution_id = self.solution_id.clone();
        let store = SolutionStore::global(cx);
        let task = store.update(cx, |s, cx| {
            s.add_member(solution_id, catalog_project.id, cache_root, cx)
        });
        cx.spawn(async move |_, _| task.await)
            .detach_and_log_err(cx);
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for AddProjectPicker {}

impl Focusable for AddProjectPicker {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AddProjectPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let create_row = ListItem::new("create-empty-project")
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .start_slot(
                Icon::new(IconName::Plus)
                    .color(Color::Accent)
                    .size(IconSize::Small),
            )
            .child(Label::new("Create new project in solution…").color(Color::Accent))
            .on_click(cx.listener(|this, _, window, cx| this.create_empty(window, cx)));

        let add_from_git_row = ListItem::new("add-from-git")
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .start_slot(
                Icon::new(IconName::GitBranch)
                    .color(Color::Accent)
                    .size(IconSize::Small),
            )
            .child(Label::new("Add new project from git…").color(Color::Accent))
            .on_click(cx.listener(|this, _, window, cx| this.add_from_git(window, cx)));

        let query = self.query.clone();
        // Cap the catalog list height and scroll the overflow — the registry
        // can hold dozens of projects, which otherwise blows the popover open
        // to full screen height. Content-sized up to the cap (not `flex_1`),
        // so short lists stay compact.
        let mut list = v_flex()
            .id("add-project-catalog-list")
            .gap_0p5()
            .max_h(rems(18.))
            .overflow_y_scroll();
        for catalog_project in self.catalog_entries.iter() {
            if !query.is_empty()
                && !catalog_project.name.to_lowercase().contains(&query)
                && !catalog_project.remote_url.to_lowercase().contains(&query)
            {
                continue;
            }
            let catalog_project = catalog_project.clone();
            let label: SharedString = catalog_project.name.clone().into();
            let url: SharedString = catalog_project.remote_url.clone().into();
            list = list.child(
                ListItem::new(SharedString::from(catalog_project.id.0.clone()))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .child(Label::new(label))
                    .end_slot(Label::new(url).color(Color::Muted).size(LabelSize::Small))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_catalog(catalog_project.clone(), window, cx);
                    })),
            );
        }

        v_flex()
            .key_context("ActiveProjectAddPicker")
            .track_focus(&self.focus_handle)
            .w(rems(34.))
            .p_2()
            .gap_2()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(self.search_editor.clone())
            .child(create_row)
            .child(add_from_git_row)
            .child(list)
    }
}
