//! Auto-trust hook: any project whose worktree lives under a Solution's
//! root is implicitly trusted on the spot. Catalog membership IS the
//! trust signal — the user vetted the remote URL when adding it to the
//! catalog and explicitly added it to a Solution, so prompting again at
//! LSP start is noise.
//!
//! `project::trusted_worktrees` exposes a path-hierarchy: trusting a
//! parent path implicitly trusts every current and future child worktree
//! under it. So we trust `solution.root` once per workspace and every
//! member project (current and future, since `add_member` clones into
//! `solution.root/<catalog_id>`) inherits trust automatically.

use collections::HashSet;
use gpui::{App, Subscription};
use project::trusted_worktrees::{PathTrust, TrustedWorktrees};

use crate::SolutionStore;

pub fn init(cx: &mut App) -> Subscription {
    cx.observe_new::<workspace::Workspace>(|workspace, _window, cx| {
        let project = workspace.project().clone();
        // Defer to the next App tick: the workspace observer fires before
        // the worktree store has hooked into the trust system, and the
        // surrounding `observe_new` update borrows the workspace, so a
        // synchronous read into project paths can also panic.
        cx.defer(move |cx| {
            trust_solution_roots_for_project(&project, cx);
        });
    })
}

fn trust_solution_roots_for_project(project: &gpui::Entity<project::Project>, cx: &mut App) {
    let Some(store) = SolutionStore::try_global(cx) else {
        return;
    };
    let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
        return;
    };

    // Collect every Solution root that this project sits inside of.
    // Iterate ALL worktrees (visible + hidden) — empty Solutions attach
    // their root as a hidden worktree so the project panel stays clean,
    // and we still want to trust them.
    let worktree_paths: Vec<std::sync::Arc<std::path::Path>> = project
        .read(cx)
        .worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path())
        .collect();

    let roots_to_trust: HashSet<PathTrust> = store.read_with(cx, |store, _| {
        let mut roots = HashSet::default();
        for solution in store.solutions() {
            if worktree_paths
                .iter()
                .any(|path| path.starts_with(&solution.root))
            {
                roots.insert(PathTrust::AbsPath(solution.root.clone()));
            }
        }
        roots
    });

    if roots_to_trust.is_empty() {
        return;
    }

    let worktree_store = project.read(cx).worktree_store();
    trusted_worktrees.update(cx, |trusted_worktrees, cx| {
        trusted_worktrees.trust(&worktree_store, roots_to_trust, cx);
    });
}
