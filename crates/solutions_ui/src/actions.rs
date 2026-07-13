use gpui::{Action, actions};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

actions!(
    solutions_ui,
    [
        /// Switch to the next open Solution in the window.
        SwitchToNextSolution,
        /// Switch to the previous open Solution in the window.
        SwitchToPrevSolution,
    ]
);

actions!(
    solutions,
    [
        /// Open the picker to switch to a Solution.
        OpenSolution,
        /// Create a new Solution.
        NewSolution,
        /// Refresh the cache for every catalog project referenced by the active Solution.
        RefreshCacheForCurrent,
    ]
);

/// Delete a Solution (with disk cleanup) by id. Triggered from the welcome
/// list's row trash icon; opens a confirmation modal that does the work.
#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = solutions)]
pub struct DeleteSolution {
    pub id: i64,
}

/// Close a Solution from the title-bar tab strip — stops AI sessions and
/// closes any retained / active workspaces hosting it. Mirrors what the
/// (retired) dock panel's `close_solution` button used to do.
#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = solutions)]
pub struct CloseSolutionFromTabBar {
    pub id: i64,
}

/// Open the destructive-action confirmation modal for the given Solution
/// from the title-bar tab strip's right-click menu. The modal lists the
/// registry entry and on-disk folder; on confirm it dispatches
/// `DeleteSolution { id }` (which performs the actual deletion).
#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = solutions)]
pub struct DeleteSolutionFromTabBar {
    pub id: i64,
}

/// Reveal the on-disk root folder of a Solution in the OS file manager.
/// Triggered from the title-bar tab strip's right-click menu.
#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = solutions)]
pub struct RevealSolutionFolder {
    pub id: i64,
}

/// Open the rename modal for a Solution by id. Triggered from the title-bar
/// tab strip's right-click menu (and, eventually, the picker dropdown).
#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = solutions)]
pub struct RenameSolution {
    pub id: i64,
}

/// Open the edit modal for a catalog project (Name / Remote URL / default
/// branch). Triggered from the failed in-flight add row in the Solutions
/// panel — the most common reason an add fails is a wrong URL, and this
/// is the path the user clicks to fix it before retrying.
#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = solutions)]
pub struct EditCatalogProject {
    pub id: i64,
}

/// Open the delete-confirmation modal for a catalog project. Triggered
/// from the trash icon on a Catalog row. The modal lists every solution
/// that references the project so the user can see the cascade impact
/// before confirming.
#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = solutions)]
pub struct DeleteCatalogProject {
    pub id: i64,
}

/// Open the modal that creates a new empty member inside the named
/// solution. Dispatched from the panel selector's `+` dropdown.
#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = solutions)]
pub struct CreateNewProjectInSolution {
    pub solution_id: i64,
}

/// Open the modal to add a new git project to the catalog. When dispatched
/// from a Solution's project-strip `+` (or the member picker), `solution_id`
/// carries that Solution so the new project is also added as a member once it
/// finishes cloning. `None` (the default, e.g. a bare keymap binding) adds to
/// the catalog only.
#[derive(PartialEq, Clone, Debug, Default, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = solutions)]
#[serde(default)]
pub struct AddCatalogProject {
    pub solution_id: Option<i64>,
}

/// Open the destructive-action confirmation modal for removing a member
/// from a solution. Dispatched from the trash icon on a member-picker
/// row. The modal lists the registry entry + on-disk folder; on confirm
/// it calls `SolutionStore::remove_member` and rm-rfs the folder. The
/// member id alone identifies the target — the owning solution is
/// recovered via `SolutionStore::member_of`.
#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = solutions)]
pub struct RemoveMember {
    pub member_id: i64,
}

/// Cycle the solution-wide active project forward (next member) within
/// the active solution. The `panel_kind` field is retained for keymap
/// stability but is now ignored (selection is solution-wide, not
/// per-panel). Ships without a default keymap; users bind themselves.
#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = solutions)]
pub struct SwitchToNextProjectInPanel {
    pub panel_kind: String,
}

/// Cycle the solution-wide active project backward (previous member) within
/// the active solution. The `panel_kind` field is retained for keymap
/// stability but is now ignored (selection is solution-wide, not
/// per-panel). Ships without a default keymap; users bind themselves.
#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = solutions)]
pub struct SwitchToPrevProjectInPanel {
    pub panel_kind: String,
}
