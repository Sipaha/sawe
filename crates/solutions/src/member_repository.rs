//! One place that answers "which git repository is the user working on".
//!
//! In a multi-member Solution every member is a worktree of ONE `Project`, so
//! `Project::active_repository` follows whatever buffer was focused last and
//! not the member selected in the tab strip. Each git surface used to carry its
//! own copy of a member-scoped lookup, and every copy did the same thing:
//! `repositories().values().find(|repo| repo.work_directory_abs_path
//! .starts_with(&member.local_path))`.
//!
//! That `find` is wrong twice over. `repositories()` is a `HashMap`, so
//! iteration order is arbitrary, and `starts_with` matches a repository nested
//! *inside* another member's worktree just as happily as the member's own
//! repository — a vendored plugin with its own `.git` is a first-class
//! `Repository` (the worktree scanner only rejects a `.git` inside a `.git`).
//! With ecos-ui and `ecos-ui/src/plugins/…-plugin` both matching, each surface
//! could land on a different one, which is exactly how the title bar came to
//! show a detached-HEAD sha from a plugin while the log showed another repo.
//!
//! So: candidates are ordered (outermost first, then by path), the default is
//! the outermost — the member's own repository — and the user's explicit pick
//! is remembered per member and honoured by every surface.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{App, Entity, Global};
use project::{Project, git_store::Repository};

use crate::model::{MemberId, SolutionId};
use crate::store::SolutionStore;

/// Explicit per-member repository choices, keyed by the repository work
/// directory rather than by `RepositoryId` because ids are minted fresh on
/// every rescan while the path is stable.
#[derive(Default)]
struct MemberRepositoryChoices {
    by_member: HashMap<(SolutionId, MemberId), PathBuf>,
}

impl Global for MemberRepositoryChoices {}

/// The Solution member the tab strip currently has selected, as
/// `(solution, member, member root)`. `None` for a plain (non-Solution)
/// project, which is the signal for callers to fall back to
/// `Project::active_repository`.
pub fn active_member_context(
    project: &Entity<Project>,
    cx: &App,
) -> Option<(SolutionId, MemberId, PathBuf)> {
    let store = SolutionStore::try_global(cx)?;
    let store = store.read(cx);
    let solution = project
        .read(cx)
        .worktrees(cx)
        .find_map(|worktree| store.solution_for_path(&worktree.read(cx).abs_path()))?;
    let member_id = store.active_member(solution.id)?;
    let member = solution.member(member_id)?;
    Some((solution.id, member_id, member.local_path.clone()))
}

/// Every repository whose work directory lives under `member_path`, outermost
/// first. The order is what makes the default pick deterministic: the member's
/// own repository has the shortest path, anything vendored inside it is longer.
pub fn repositories_under(
    project: &Entity<Project>,
    member_path: &Path,
    cx: &App,
) -> Vec<Entity<Repository>> {
    let mut repositories: Vec<_> = project
        .read(cx)
        .repositories(cx)
        .values()
        .filter(|repo| {
            repo.read(cx)
                .work_directory_abs_path
                .starts_with(member_path)
        })
        .cloned()
        .collect();
    repositories.sort_by(|left, right| {
        let left = left.read(cx).work_directory_abs_path.clone();
        let right = right.read(cx).work_directory_abs_path.clone();
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(&right))
    });
    repositories
}

/// Repositories the active member owns, outermost first. Empty outside a
/// Solution.
pub fn active_member_repositories(project: &Entity<Project>, cx: &App) -> Vec<Entity<Repository>> {
    let Some((_, _, member_path)) = active_member_context(project, cx) else {
        return Vec::new();
    };
    repositories_under(project, &member_path, cx)
}

/// The repository every git surface should act on: the user's explicit choice
/// for the active member when it is still present, else the member's outermost
/// repository. `None` outside a Solution — callers fall back to
/// `Project::active_repository`.
pub fn active_member_repository(project: &Entity<Project>, cx: &App) -> Option<Entity<Repository>> {
    let (solution_id, member_id, member_path) = active_member_context(project, cx)?;
    let repositories = repositories_under(project, &member_path, cx);
    let chosen = cx
        .try_global::<MemberRepositoryChoices>()
        .and_then(|choices| choices.by_member.get(&(solution_id, member_id)))
        .and_then(|chosen| {
            repositories
                .iter()
                .find(|repo| repo.read(cx).work_directory_abs_path.as_ref() == chosen.as_path())
        });
    chosen.or_else(|| repositories.first()).cloned()
}

/// Record the user's pick for the active member and make it the project-wide
/// active repository, so surfaces that only know about `GitStore` follow along
/// and everyone re-renders off the existing `ActiveRepositoryChanged` event.
pub fn set_active_member_repository(
    project: &Entity<Project>,
    repository: &Entity<Repository>,
    cx: &mut App,
) {
    if let Some((solution_id, member_id, _)) = active_member_context(project, cx) {
        let work_directory = repository.read(cx).work_directory_abs_path.to_path_buf();
        cx.default_global::<MemberRepositoryChoices>()
            .by_member
            .insert((solution_id, member_id), work_directory);
    }
    repository.update(cx, |repository, cx| {
        repository.set_as_active_repository(cx);
    });
}

/// Forget every explicit pick. Only used by tests, which share one process-wide
/// global across cases.
#[cfg(any(test, feature = "test-support"))]
pub fn clear_repository_choices_for_test(cx: &mut App) {
    cx.default_global::<MemberRepositoryChoices>()
        .by_member
        .clear();
}
