//! Standalone 3-way merge conflict resolver (S-CFL).
//!
//! `init(cx)` registers MCP tools (`editor.git.list_conflicts`,
//! `editor.git.resolve_conflict`, `editor.git.mark_resolved`,
//! `editor.git.continue_merge`, `editor.git.abort_merge`) and an action
//! handler so workspaces can request the resolver be opened against the
//! active repository.
//!
//! The visual surface (`ConflictResolverView`) is a workspace pane Item:
//! three (or four with Base) Editors + sidebar + toolbar + bottom bar,
//! mirroring the structural pattern from `editor::SplittableEditor` (see
//! `docs/superpowers/specs/git-panel/cfl-spike.md` for the rationale).

pub mod ai_suggest;
mod ai_suggest_modal;
pub mod binary_view;
pub mod chunks;
pub mod conflict_parser;
mod mcp_tools;
pub mod operations;
pub mod resolver_view;
pub mod sidebar;
mod toolbar;

pub use conflict_parser::{ConflictedFile, InProgressOp, ThreeWayContent, detect_in_progress_op};
pub use resolver_view::{ConflictResolverView, ResolverPane, ThreeWaySplitState};

use gpui::{App, TaskExt as _};
use std::sync::Arc;
use workspace::Workspace;

/// Workspace action: open the resolver against the active repository.
#[derive(Default, Clone, PartialEq, Eq, gpui::Action)]
#[action(namespace = git, name = "OpenConflictResolver")]
pub struct OpenConflictResolver;

/// Put the selected path back into conflict — the reverse of `Mark Resolved`.
///
/// It is a separate action from `git::ToggleStaged` because the two run
/// different git commands and only one of them is reversible. Staging a
/// conflict resolves it (`git update-index --add`); *un*staging it is
/// `git reset`, which replaces the unmerged stages with HEAD's blob and loses
/// the incoming side outright. The way back is
/// [`operations::RestoreConflictOp`] (`git checkout --merge`), and it needs its
/// own action so no keybinding, menu row or checkbox can dispatch the
/// destructive spelling by accident.
///
/// The git panel is the handler; it is the surface that knows which row is
/// selected. See `GitPanel::mark_unresolved_for_selected`.
#[derive(Default, Clone, PartialEq, Eq, gpui::Action)]
#[action(namespace = git, name = "MarkUnresolved")]
pub struct MarkUnresolved;

pub fn init(cx: &mut App) {
    mcp_tools::register(cx);

    cx.observe_new(|workspace: &mut Workspace, _, _cx| {
        workspace.register_action(
            |workspace: &mut Workspace, _: &OpenConflictResolver, window, cx| {
                let project = workspace.project().clone();
                let Some(repo) = project.read(cx).active_repository(cx) else {
                    log::warn!("OpenConflictResolver: no active repository");
                    return;
                };
                let work_dir: Arc<std::path::Path> = repo.read(cx).work_directory_abs_path.clone();
                let weak = workspace.weak_handle();
                ConflictResolverView::open(project, weak, work_dir, window, cx)
                    .detach_and_log_err(cx);
            },
        );
    })
    .detach();
}
