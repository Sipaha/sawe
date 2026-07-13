use solutions::{CatalogId, SolutionId, SolutionStore};
use ui::prelude::*;
use workspace::Workspace;

use crate::actions::{
    AddCatalogProject, CreateNewProjectInSolution, DeleteCatalogProject, DeleteSolution,
    EditCatalogProject, NewSolution,
};

mod add_catalog_project;
mod delete_catalog_project;
mod delete_solution;
mod edit_catalog_project;
mod new_project_in_solution;
mod new_solution;
mod rename_solution;

pub(crate) use new_solution::NewSolutionModal;
pub(crate) use rename_solution::open_rename_solution;

use add_catalog_project::AddCatalogProjectModal;
use add_catalog_project::humanize_catalog_error;
use delete_catalog_project::DeleteCatalogProjectModal;
use delete_solution::DeleteSolutionModal;
use edit_catalog_project::{EditCatalogPrefill, EditCatalogProjectModal};
use new_project_in_solution::open_new_project_in_solution;

pub fn register(workspace: &mut Workspace, _: Option<&mut Window>, _: &mut Context<Workspace>) {
    workspace.register_action(|workspace, _: &NewSolution, window, cx| {
        let weak = cx.weak_entity();
        workspace.toggle_modal(window, cx, |window, cx| {
            NewSolutionModal::new(weak, window, cx)
        });
    });
    workspace.register_action(|workspace, action: &AddCatalogProject, window, cx| {
        let weak = cx.weak_entity();
        let solution_id = action.solution_id.map(SolutionId);
        workspace.toggle_modal(window, cx, move |window, cx| {
            AddCatalogProjectModal::new(weak, solution_id, window, cx)
        });
    });
    workspace.register_action(|workspace, action: &EditCatalogProject, window, cx| {
        let id = CatalogId(action.id);
        let store = SolutionStore::global(cx);
        let Some(prefill) = store.read_with(cx, |s, _| {
            s.catalog()
                .iter()
                .find(|p| p.id == id)
                .map(|p| EditCatalogPrefill {
                    name: p.name.clone(),
                    remote_url: p.remote_url.clone(),
                    default_branch: p.default_branch.clone().unwrap_or_default(),
                })
        }) else {
            return;
        };
        let weak = cx.weak_entity();
        workspace.toggle_modal(window, cx, move |window, cx| {
            EditCatalogProjectModal::new(weak, id, prefill, window, cx)
        });
    });
    workspace.register_action(|workspace, action: &DeleteCatalogProject, window, cx| {
        let id = CatalogId(action.id);
        let store = SolutionStore::global(cx);
        let Some((name, references)) = store.read_with(cx, |s, _| {
            let project = s.catalog().iter().find(|p| p.id == id)?;
            Some((project.name.clone(), s.solutions_referencing(id)))
        }) else {
            return;
        };
        workspace.toggle_modal(window, cx, move |_window, cx| {
            DeleteCatalogProjectModal::new(id, name, references, cx)
        });
    });
    workspace.register_action(|workspace, action: &DeleteSolution, window, cx| {
        let id = SolutionId(action.id);
        let store = SolutionStore::global(cx);
        // Look up the solution's display name + root for the modal copy.
        // If the id is unknown (stale action / already-deleted), do nothing.
        let Some((name, root)) = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|sol| sol.id == id)
                .map(|sol| (sol.name.clone(), sol.root.clone()))
        }) else {
            return;
        };
        workspace.toggle_modal(window, cx, |_window, cx| {
            DeleteSolutionModal::new(id, name, root, cx)
        });
    });
    workspace.register_action(
        |workspace, action: &CreateNewProjectInSolution, window, cx| {
            let id = SolutionId(action.solution_id);
            open_new_project_in_solution(workspace, id, window, cx);
        },
    );
}
