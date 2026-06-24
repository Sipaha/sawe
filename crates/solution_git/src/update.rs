//! Solution-wide "Update Project" orchestrator — fetch + pull every git
//! member of the active Solution.
//!
//! Implements [`SolutionUpdateProvider`] from `git_ui`. Mirrors the
//! `push::SolutionPushOrchestrator` member-resolution + spawn pattern and
//! reuses `dashboard::{run_git_fetch, run_git_pull}` for the per-member
//! work. Surfaces a single summary `StatusToast` on completion.

use std::path::PathBuf;

use git_ui::providers::SolutionUpdateProvider;
use gpui::{App, AppContext, WeakEntity};
use notifications::status_toast::StatusToast;
use solutions::{Solution, SolutionStore};
use ui::{Color, Icon, IconName, IconSize};
use util::ResultExt as _;
use workspace::Workspace;

use crate::dashboard::{run_git_fetch, run_git_pull};

/// Holds a `WeakEntity<SolutionStore>` so the provider always resolves
/// against whichever Solution is currently active without re-registering
/// on solution switch (mirrors `push::SolutionPushOrchestrator`).
pub struct SolutionUpdateOrchestrator {
    store: WeakEntity<SolutionStore>,
}

impl SolutionUpdateOrchestrator {
    pub fn new(store: WeakEntity<SolutionStore>) -> Self {
        Self { store }
    }

    fn active_solution(&self, cx: &App) -> Option<Solution> {
        let store = self.store.upgrade()?;
        crate::active_solution_from_store(&store, cx)
    }
}

/// Build an orchestrator wired to the global `SolutionStore`. Returns
/// `None` when the store global is missing — same pattern as push/commit.
pub fn build_global_orchestrator(cx: &App) -> Option<SolutionUpdateOrchestrator> {
    let store = SolutionStore::try_global(cx)?;
    Some(SolutionUpdateOrchestrator::new(store.downgrade()))
}

/// Work dirs of every git member of `solution`. Drops non-git members so
/// per-member `git fetch`/`pull` don't surface "fatal: not a git repository"
/// — mirrors `dashboard::resolve_targets`.
fn git_member_targets(solution: &Solution) -> Vec<PathBuf> {
    solution
        .members
        .iter()
        .filter(|m| m.local_path.join(".git").exists())
        .map(|m| m.local_path.clone())
        .collect()
}

/// Per-member update outcome — pure data so the summary formatting below
/// is unit-testable without real git.
#[derive(Debug, Clone)]
struct MemberUpdateOutcome {
    ok: bool,
}

/// Summarize per-member outcomes into a toast `(text, success)`. Pure
/// function — unit-tested below.
fn summarize_outcomes(total: usize, failed: usize) -> (String, bool) {
    let updated = total.saturating_sub(failed);
    if failed == 0 {
        (
            format!(
                "Updated {updated} project{}",
                if updated == 1 { "" } else { "s" }
            ),
            true,
        )
    } else {
        (format!("Updated {updated}, {failed} failed"), false)
    }
}

impl SolutionUpdateProvider for SolutionUpdateOrchestrator {
    fn is_active(&self) -> bool {
        // Reading the store to count git members needs an `&App`, which the
        // trait surface doesn't carry. Mirror push's cheap "store exists"
        // check; `update_solution` re-validates (no targets ⇒ no-op).
        self.store.upgrade().is_some()
    }

    fn update_solution(&self, workspace: WeakEntity<Workspace>, cx: &mut App) {
        let Some(solution) = self.active_solution(cx) else {
            log::info!("solution_git::update: no active Solution");
            return;
        };
        let targets = git_member_targets(&solution);
        if targets.is_empty() {
            log::info!("solution_git::update: active Solution has no git members");
            return;
        }

        cx.spawn(async move |cx| {
            let mut tasks = Vec::with_capacity(targets.len());
            for work_dir in targets {
                tasks.push(cx.background_spawn(async move {
                    // Fetch first, then fast-forward pull. A fetch failure
                    // counts as a failed member even if pull would no-op.
                    let fetch = run_git_fetch(&work_dir).await;
                    let pull = if fetch.is_ok() {
                        run_git_pull(&work_dir).await
                    } else {
                        Ok(())
                    };
                    MemberUpdateOutcome {
                        ok: fetch.is_ok() && pull.is_ok(),
                    }
                }));
            }
            let mut outcomes = Vec::with_capacity(tasks.len());
            for task in tasks {
                outcomes.push(task.await);
            }
            let total = outcomes.len();
            let failed = outcomes.iter().filter(|o| !o.ok).count();
            let (text, success) = summarize_outcomes(total, failed);

            // No entity lease is held across the awaits above — we only
            // reach for the workspace here, after the work is done.
            workspace
                .update(cx, |workspace, cx| {
                    let toast = StatusToast::new(text, cx, move |this, _cx| {
                        this.icon(
                            Icon::new(if success {
                                IconName::Check
                            } else {
                                IconName::XCircle
                            })
                            .size(IconSize::Small)
                            .color(if success {
                                Color::Success
                            } else {
                                Color::Error
                            }),
                        )
                        .dismiss_button(true)
                    });
                    workspace.toggle_status_toast(toast, cx);
                })
                .log_err();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_all_succeeded_plural() {
        let (text, success) = summarize_outcomes(3, 0);
        assert_eq!(text, "Updated 3 projects");
        assert!(success);
    }

    #[test]
    fn summarize_single_success_singular() {
        let (text, success) = summarize_outcomes(1, 0);
        assert_eq!(text, "Updated 1 project");
        assert!(success);
    }

    #[test]
    fn summarize_some_failed() {
        let (text, success) = summarize_outcomes(4, 1);
        assert_eq!(text, "Updated 3, 1 failed");
        assert!(!success);
    }

    #[test]
    fn summarize_all_failed() {
        let (text, success) = summarize_outcomes(2, 2);
        assert_eq!(text, "Updated 0, 2 failed");
        assert!(!success);
    }
}
