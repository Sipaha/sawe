//! S-CTM "New Branch from Here…" handler, plus the branch-delete
//! failure classifier shared by every UI delete entry point (branch
//! picker rows, the Branches popup context menu, and the commit / blame
//! ref submenu).

use std::path::Path;

use anyhow::{Result, anyhow};
use gpui::{App, Entity, SharedString, Task};
use project::git_store::Repository;

use crate::git_panel::format_git_error_toast_message;
use crate::handlers::protection;

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

/// Label of the affirmative answer on the "this branch can only be
/// deleted by forcing" warning. Shared so every delete entry point
/// spells the escape hatch the same way.
pub const FORCE_DELETE_BRANCH_ANSWER: &str = "Delete anyway";

struct BranchDeleteForceDeletePrompt {
    required_error_substrings: &'static [&'static str],
    message: fn(&str) -> String,
}

impl BranchDeleteForceDeletePrompt {
    fn matches(&self, normalized_error_message: &str) -> bool {
        self.required_error_substrings
            .iter()
            .all(|substring| normalized_error_message.contains(substring))
    }
}

const BRANCH_DELETE_FORCE_DELETE_PROMPTS: &[BranchDeleteForceDeletePrompt] =
    &[BranchDeleteForceDeletePrompt {
        required_error_substrings: &["not fully merged"],
        message: unmerged_branch_force_delete_prompt,
    }];

fn unmerged_branch_force_delete_prompt(branch_name: &str) -> String {
    format!("Branch \"{branch_name}\" is not fully merged. Force delete it?")
}

/// Haystack the substring table is matched against. Over collab the git
/// stderr travels inside an `RpcError` whose payload
/// `anyhow::Error::to_string` does not render, so the raw RPC message
/// has to be searched too — neither string alone covers both the local
/// and the remote repository case.
fn branch_delete_error_haystack(error: &anyhow::Error) -> String {
    let mut haystack = format_git_error_toast_message(error);
    haystack.push('\n');
    haystack.push_str(&error.to_string());
    haystack.to_lowercase()
}

// Git only reports these cases via localized stderr, so this best-effort
// check may miss some locales and fall back to the raw error toast.
pub fn force_delete_prompt_for_branch_delete_error(
    error: &anyhow::Error,
    branch_name: &str,
) -> Option<String> {
    let haystack = branch_delete_error_haystack(error);
    BRANCH_DELETE_FORCE_DELETE_PROMPTS
        .iter()
        .find(|prompt| prompt.matches(&haystack))
        .map(|prompt| (prompt.message)(branch_name))
}

/// What a UI entry point should do after a non-force branch delete
/// failed.
#[derive(Debug, PartialEq, Eq)]
pub enum ForceDeleteDecision {
    /// The failure is recoverable by forcing and policy permits it —
    /// show `warning` next to a [`FORCE_DELETE_BRANCH_ANSWER`] button.
    Offer { warning: String },
    /// Recoverable by forcing, but branch protection refuses the forced
    /// delete, so no escape hatch may be offered.
    Forbidden { message: String },
    /// Not a force-recoverable failure — surface the original error.
    NotApplicable,
}

/// Classify a failed non-force branch delete. `work_dir` is the
/// repository the branch lives in; `None` skips the branch-protection
/// lookup (there is nothing to resolve the owning Solution member
/// against). `RequiresConfirmation` is treated as permitted: the
/// explicit "Delete anyway" button *is* the confirmation, and no other
/// destructive path in this fork stacks a second confirm on top of one.
pub fn force_delete_decision(
    error: &anyhow::Error,
    work_dir: Option<&Path>,
    branch_name: &str,
) -> ForceDeleteDecision {
    let forbidden_reason = work_dir.and_then(|work_dir| {
        match protection::enforce(work_dir, branch_name, "delete_branch_force", false) {
            Err(protection::BranchProtectionError::Forbidden { reason }) => Some(reason),
            _ => None,
        }
    });
    classify_force_delete(error, forbidden_reason, branch_name)
}

/// The classification itself, with branch protection already resolved. Split out
/// because `protection::enforce` reads a process-global whose setters belong to
/// `solutions`, so the `Forbidden` arm — the one that withholds a destructive
/// affordance — is not reachable from a `git_ui` unit test otherwise.
fn classify_force_delete(
    error: &anyhow::Error,
    forbidden_reason: Option<String>,
    branch_name: &str,
) -> ForceDeleteDecision {
    let Some(warning) = force_delete_prompt_for_branch_delete_error(error, branch_name) else {
        return ForceDeleteDecision::NotApplicable;
    };
    if let Some(reason) = forbidden_reason {
        return ForceDeleteDecision::Forbidden {
            message: format!("Branch \"{branch_name}\" cannot be force deleted: {reason}"),
        };
    }
    ForceDeleteDecision::Offer { warning }
}

/// Split a remote-tracking ref into `(remote, branch)` against the
/// repository's configured remotes. The **longest** matching remote name
/// wins: `git remote add team/fork …` is legal, so splitting on the
/// first `/` can name a remote that doesn't exist.
///
/// `None` means the ref is not claimed by any configured remote — the
/// caller decides whether that is a hard stop (the commit menu withholds
/// every server-side action) or a cue to fall back to a guess.
///
/// This is the crate's one remote/branch boundary rule. `%D`
/// decorations, `<branch>@{upstream}` output and `refs/remotes/…` paths
/// all reach it; strip any `refs/remotes/` prefix before calling.
pub fn split_remote_ref(
    name: &str,
    remotes: &[SharedString],
) -> Option<(SharedString, SharedString)> {
    remotes
        .iter()
        .filter_map(|remote| {
            let branch = name.strip_prefix(remote.as_ref())?.strip_prefix('/')?;
            if branch.is_empty() {
                return None;
            }
            Some((remote.clone(), SharedString::from(branch.to_string())))
        })
        .max_by_key(|(remote, _)| remote.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remotes(names: &[&str]) -> Vec<SharedString> {
        names
            .iter()
            .map(|name| SharedString::from(name.to_string()))
            .collect()
    }

    #[test]
    fn splits_a_plain_remote_ref() {
        assert_eq!(
            split_remote_ref("origin/main", &remotes(&["origin"])),
            Some(("origin".into(), "main".into()))
        );
    }

    /// The reason this rule exists: `git remote add team/fork …` is
    /// legal, and splitting on the first `/` names a remote that does
    /// not exist.
    #[test]
    fn the_longest_matching_remote_wins() {
        assert_eq!(
            split_remote_ref("team/fork/main", &remotes(&["team", "team/fork"])),
            Some(("team/fork".into(), "main".into()))
        );
    }

    /// A GitFlow branch name keeps its slashes — only the remote half is
    /// consumed.
    #[test]
    fn a_slash_bearing_branch_name_survives_the_split() {
        assert_eq!(
            split_remote_ref("origin/feature/FOO-1", &remotes(&["origin"])),
            Some(("origin".into(), "feature/FOO-1".into()))
        );
    }

    #[test]
    fn a_ref_no_configured_remote_claims_does_not_split() {
        assert_eq!(
            split_remote_ref("upstream/main", &remotes(&["origin"])),
            None
        );
        assert_eq!(split_remote_ref("main", &remotes(&["origin"])), None);
        assert_eq!(split_remote_ref("origin/", &remotes(&["origin"])), None);
        assert_eq!(split_remote_ref("origin/main", &[]), None);
    }

    fn unmerged_error(branch: &str) -> anyhow::Error {
        anyhow!(
            "error: The branch '{branch}' is not fully merged.\nIf you are sure you want to delete it, run 'git branch -D {branch}'."
        )
    }

    #[test]
    fn detects_unmerged_branch_in_plain_git_stderr() {
        assert_eq!(
            force_delete_prompt_for_branch_delete_error(&unmerged_error("feature"), "feature"),
            Some("Branch \"feature\" is not fully merged. Force delete it?".to_string())
        );
    }

    #[test]
    fn detects_unmerged_branch_inside_an_rpc_error() {
        let rpc_error = proto::RpcError::from_proto(
            &proto::Error {
                message: "error: The branch 'feature' is not fully merged.".to_string(),
                code: proto::ErrorCode::Internal as i32,
                tags: Default::default(),
            },
            "DeleteBranch",
        );
        let wrapped = rpc_error.context("deleting branch");

        assert_eq!(
            force_delete_prompt_for_branch_delete_error(&wrapped, "feature"),
            Some("Branch \"feature\" is not fully merged. Force delete it?".to_string()),
            "collab-wrapped git stderr must still match the force-delete table"
        );
    }

    #[test]
    fn unrelated_failures_are_not_force_recoverable() {
        let error = anyhow!("error: branch 'feature' not found.");
        assert!(force_delete_prompt_for_branch_delete_error(&error, "feature").is_none());
        assert_eq!(
            force_delete_decision(&error, None, "feature"),
            ForceDeleteDecision::NotApplicable
        );
    }

    #[test]
    fn unmerged_failure_offers_force_delete() {
        assert_eq!(
            force_delete_decision(&unmerged_error("feature"), None, "feature"),
            ForceDeleteDecision::Offer {
                warning: "Branch \"feature\" is not fully merged. Force delete it?".to_string(),
            }
        );
    }

    #[test]
    fn a_protected_branch_is_not_offered_the_escape_hatch() {
        assert_eq!(
            classify_force_delete(
                &unmerged_error("release/2.28"),
                Some("release branches are protected".to_string()),
                "release/2.28",
            ),
            ForceDeleteDecision::Forbidden {
                message: "Branch \"release/2.28\" cannot be force deleted: release branches are \
                          protected"
                    .to_string(),
            }
        );
    }

    #[test]
    fn protection_does_not_turn_an_unrelated_failure_into_a_force_offer() {
        assert_eq!(
            classify_force_delete(
                &anyhow!("error: branch 'feature' not found."),
                Some("protected".to_string()),
                "feature",
            ),
            ForceDeleteDecision::NotApplicable,
            "protection must not manufacture a force-delete decision out of an unrelated error"
        );
    }
}
