//! S-CTM "New Branch from Here…" handler.

use anyhow::{Result, anyhow};
use gpui::{App, Entity, Task};
use project::git_store::Repository;

/// Create a branch named `name` pointing at `sha`. When `checkout` is
/// `true`, additionally check the branch out via `change_branch`.
///
/// `git branch <name> <sha>` errors when a branch with that name already
/// exists, so we get collision detection for free.
pub fn create_branch_at(
    repository: Entity<Repository>,
    sha: String,
    name: String,
    checkout: bool,
    cx: &mut App,
) -> Task<Result<()>> {
    cx.spawn(async move |cx| {
        match repository
            .update(cx, |repo, _| repo.branch_at_sha(name.clone(), sha))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(anyhow!("branch_at_sha was canceled")),
        }
        if checkout {
            match repository
                .update(cx, |repo, _| repo.change_branch(name.clone()))
                .await
            {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(anyhow!("change_branch was canceled")),
            }
        } else {
            Ok(())
        }
    })
}
