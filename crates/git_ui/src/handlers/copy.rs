//! S-CTM Copy submenu handlers — copy commit metadata to clipboard.

use anyhow::{Result, anyhow};
use git::{BuildCommitPermalinkParams, GitHostingProviderRegistry, parse_git_remote_url};
use gpui::{App, ClipboardItem, Entity, Task};
use project::git_store::Repository;

const SHORT_SHA_LEN: usize = 7;

/// Write the full commit hash to the clipboard.
pub fn copy_hash(sha: &str, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(sha.to_string()));
}

/// Write a 7-char short hash to the clipboard. Falls back to the full SHA
/// when shorter than 7 chars.
pub fn copy_short_hash(sha: &str, cx: &mut App) {
    let truncated: String = sha.chars().take(SHORT_SHA_LEN).collect();
    cx.write_to_clipboard(ClipboardItem::new_string(truncated));
}

/// Write the commit subject (first line of the message) to the clipboard.
pub fn copy_subject(subject: &str, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(subject.to_string()));
}

/// Write `<short_sha> <subject>` to the clipboard — IDEA's "Copy Subject
/// + Hash" format.
pub fn copy_subject_and_hash(sha: &str, subject: &str, cx: &mut App) {
    let short: String = sha.chars().take(SHORT_SHA_LEN).collect();
    cx.write_to_clipboard(ClipboardItem::new_string(format!("{short} {subject}")));
}

/// Compute `git patch-id` for `sha` and write it to the clipboard.
pub fn copy_patch_id(
    repository: Entity<Repository>,
    sha: String,
    cx: &mut App,
) -> Task<Result<()>> {
    cx.spawn(async move |cx| {
        let patch_id = match repository
            .update(cx, |repo, _| repo.compute_patch_id(sha))
            .await
        {
            Ok(Ok(patch_id)) => patch_id,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(anyhow!("compute_patch_id was canceled")),
        };
        cx.update(|cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(patch_id));
        });
        Ok(())
    })
}

/// Build a hosting-provider permalink for `sha` and write it to the
/// clipboard. Returns `Ok(false)` when no hosted remote is configured —
/// the menu disables this entry in that case, but the handler is
/// defensive.
pub fn copy_permalink(repository: &Repository, sha: &str, cx: &mut App) -> Result<bool> {
    let Some(remote_url) = repository.default_remote_url() else {
        return Ok(false);
    };
    let registry = GitHostingProviderRegistry::default_global(cx);
    let Some((provider, parsed)) = parse_git_remote_url(registry, &remote_url) else {
        return Ok(false);
    };
    let url = provider
        .build_commit_permalink(&parsed, BuildCommitPermalinkParams { sha })
        .to_string();
    cx.write_to_clipboard(ClipboardItem::new_string(url));
    Ok(true)
}

/// Same as [`copy_permalink`] but used when "Open on $HOST" is invoked —
/// returns the URL string to feed `cx.open_url`. Returns `None` when no
/// hosted remote is configured.
pub fn build_permalink(repository: &Repository, sha: &str, cx: &mut App) -> Option<String> {
    let remote_url = repository.default_remote_url()?;
    let registry = GitHostingProviderRegistry::default_global(cx);
    let (provider, parsed) = parse_git_remote_url(registry, &remote_url)?;
    Some(
        provider
            .build_commit_permalink(&parsed, BuildCommitPermalinkParams { sha })
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::{BuildCommitPermalinkParams, GitHostingProvider, ParsedGitRemote};
    use git_hosting_providers::{Bitbucket, Gitea, Github, Gitlab};

    #[test]
    fn copy_short_hash_truncates_to_seven() {
        // Standalone unit logic — we don't exercise `App` here, just the
        // string slicing the helper relies on.
        let full = "abcdef1234567890";
        let short: String = full.chars().take(SHORT_SHA_LEN).collect();
        assert_eq!(short, "abcdef1");
    }

    #[test]
    fn copy_short_hash_keeps_short_inputs_intact() {
        let full = "abc";
        let short: String = full.chars().take(SHORT_SHA_LEN).collect();
        assert_eq!(short, "abc");
    }

    /// S-CTM permalink construction — verifies that the four hosted
    /// providers we expose in the External submenu produce the expected
    /// commit URL shape. Drives the URL contract the menu's "Open Commit
    /// on $HOST" / "Copy Web URL" entries depend on.
    #[test]
    fn permalink_for_github_commit() {
        let parsed = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };
        let url = Github::public_instance()
            .build_commit_permalink(&parsed, BuildCommitPermalinkParams { sha: "abc123" })
            .to_string();
        assert_eq!(url, "https://github.com/zed-industries/zed/commit/abc123");
    }

    #[test]
    fn permalink_for_gitlab_commit() {
        let parsed = ParsedGitRemote {
            owner: "owner".into(),
            repo: "repo".into(),
        };
        let url = Gitlab::public_instance()
            .build_commit_permalink(&parsed, BuildCommitPermalinkParams { sha: "deadbeef" })
            .to_string();
        // GitLab uses `/-/commit/<sha>` rather than `/commit/<sha>`.
        assert_eq!(url, "https://gitlab.com/owner/repo/-/commit/deadbeef");
    }

    #[test]
    fn permalink_for_bitbucket_commit() {
        let parsed = ParsedGitRemote {
            owner: "team".into(),
            repo: "project".into(),
        };
        let url = Bitbucket::public_instance()
            .build_commit_permalink(&parsed, BuildCommitPermalinkParams { sha: "0123abcd" })
            .to_string();
        // Bitbucket uses `/commits/<sha>` (note the trailing `s`).
        assert_eq!(url, "https://bitbucket.org/team/project/commits/0123abcd");
    }

    #[test]
    fn permalink_for_gitea_commit() {
        let parsed = ParsedGitRemote {
            owner: "org".into(),
            repo: "repo".into(),
        };
        let url = Gitea::public_instance()
            .build_commit_permalink(&parsed, BuildCommitPermalinkParams { sha: "feedface" })
            .to_string();
        assert_eq!(url, "https://gitea.com/org/repo/commit/feedface");
    }
}
