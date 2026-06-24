//! S-CTM "Checkout Revision" handler — pre-checks dirty working tree and
//! invokes `git checkout <sha>`.

use anyhow::{Result, anyhow};
use gpui::{App, Entity, Task};
use project::git_store::Repository;

/// Check out `sha` directly. When `force_dirty` is `false`, errors if the
/// working tree has uncommitted changes — the UI is expected to show a
/// confirmation in that case and re-invoke with `force_dirty: true` when
/// the user explicitly accepts losing those changes.
pub fn checkout_revision(
    repository: Entity<Repository>,
    sha: String,
    force_dirty: bool,
    cx: &mut App,
) -> Task<Result<()>> {
    cx.spawn(async move |cx| {
        if !force_dirty {
            let is_dirty = match repository.update(cx, |repo, _| repo.is_dirty()).await {
                Ok(Ok(dirty)) => dirty,
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(anyhow!("is_dirty was canceled")),
            };
            if is_dirty {
                return Err(anyhow!(
                    "working tree has uncommitted changes — stash, commit, \
                     or invoke with force_dirty=true to discard"
                ));
            }
        }
        match repository
            .update(cx, |repo, _| repo.checkout_revision(sha))
            .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow!("checkout_revision was canceled")),
        }
    })
}
