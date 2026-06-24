//! S-CTM "New Tag at Here…" handler.

use anyhow::{Result, anyhow};
use gpui::{App, Entity, Task};
use project::git_store::Repository;

/// Create a tag `name` at `sha`. When `message` is `Some`, the tag is
/// annotated; otherwise it's lightweight.
pub fn create_tag_at(
    repository: Entity<Repository>,
    sha: String,
    name: String,
    message: Option<String>,
    cx: &mut App,
) -> Task<Result<()>> {
    cx.spawn(async move |cx| {
        match repository
            .update(cx, |repo, _| repo.tag_at_sha(name, sha, message))
            .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow!("tag_at_sha was canceled")),
        }
    })
}
