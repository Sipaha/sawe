//! Solutions: catalog of remote git projects + named groups (Solutions) that
//! open as a single editor window with all members mounted as worktrees.

mod add_member;
mod auto_trust;
pub mod branch_protection;
mod cache;
pub mod db;
mod event_sources;
pub mod folder_name;
pub mod git;
pub mod mcp;
pub mod migrate;
mod model;
mod persistence;
pub mod rename;
mod settings;
mod slug;
mod store;
mod tabs_snapshot;

pub use add_member::{AddProgressCallback, PendingAddView};
pub use cache::{default_cache_root, refresh_cache};
pub use event_sources::install as install_event_sources_for_test;
pub use folder_name::FolderNameError;
pub use folder_name::derive as derive_folder_name;
pub use model::{CatalogId, CatalogProject, MemberId, Solution, SolutionId, SolutionMember};
pub use settings::{BranchProtectionMember, BranchProtectionSettings, SolutionsSettings};
pub use store::{
    SolutionStore, SolutionStoreEvent, install_global_for_test,
    refresh_active_solution_for_branch_protection,
};
pub use tabs_snapshot::{SolutionTabsSnapshot, TabSnapshots};

use ::settings::Settings;
use gpui::App;

pub fn init(cx: &mut App) {
    SolutionsSettings::register(cx);
    SolutionStore::init_global(cx);
    mcp::register(cx);
    event_sources::install(cx);
    // Auto-trust the root of any Solution whose member opens in a
    // workspace. Catalog membership IS the trust signal — see the
    // `auto_trust` module docs.
    auto_trust::init(cx).detach();

    // S-SOL-PRT — keep the process-global branch-protection snapshot
    // in sync with the active Solution. The settings half is updated
    // synchronously inside `SolutionsSettings::from_settings`; the
    // active-Solution half follows `ActiveSolutionChanged`.
    refresh_active_solution_for_branch_protection(cx);
    if let Some(store) = SolutionStore::try_global(cx) {
        cx.subscribe(
            &store,
            |_store, event: &SolutionStoreEvent, cx| match event {
                SolutionStoreEvent::ActiveSolutionChanged(_) | SolutionStoreEvent::Changed => {
                    refresh_active_solution_for_branch_protection(cx);
                }
                _ => {}
            },
        )
        .detach();
    }

    // S-SOL-PRT — install the registry-level branch-protection
    // checker so MCP tools that registered an `affects_branch`
    // extractor get their target evaluated against the same policy
    // the UI handlers use.
    editor_mcp::set_branch_protection_checker(Some(Box::new(|target| {
        let decision = branch_protection::check(&target.repo_path, &target.branch, target.op_name);
        match decision {
            branch_protection::Decision::Allowed => editor_mcp::BranchProtectionDecision::Allowed,
            branch_protection::Decision::RequiresConfirmation { reason } => {
                editor_mcp::BranchProtectionDecision::RequiresConfirmation { reason }
            }
            branch_protection::Decision::Forbidden { reason } => {
                editor_mcp::BranchProtectionDecision::Forbidden { reason }
            }
        }
    })));
}

#[cfg(test)]
mod tests {
    mod persistence_e2e;
}
