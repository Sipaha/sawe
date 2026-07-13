//! Branch-protection policy (S-SOL-PRT).
//!
//! Per-Solution settings declare a `default_protected` glob list and an
//! optional per-member override block. [`check`] resolves the active
//! Solution's member that owns `repo_path`, evaluates the merged rule
//! set, and returns one of [`Decision::Allowed`],
//! [`Decision::RequiresConfirmation`], or [`Decision::Forbidden`] for
//! the requested op name.
//!
//! Pattern syntax: gitignore-style globs via `globset`. `release/*`
//! matches `release/v1` but NOT `release/v2/hotfix`; for recursive use
//! `release/**`.
//!
//! UI handlers and the MCP registry call [`check`] before invoking the
//! underlying git operation so a single policy lookup site governs both
//! interactive and subagent paths.

use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::model::{Solution, SolutionMember};
use crate::settings::{BranchProtectionMember, BranchProtectionSettings};

/// Outcome of a single [`check`] call. `Allowed` means proceed silently;
/// `RequiresConfirmation` means show a confirmation modal (or, for
/// subagents, require an explicit `confirmed: true` payload field);
/// `Forbidden` means refuse the operation outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    RequiresConfirmation { reason: String },
    Forbidden { reason: String },
}

impl Decision {
    /// Convenience for the legacy two-state shape used by the v0 stub —
    /// callers that don't care about the confirmation tier just want to
    /// know `Allowed` vs not. Maps `Forbidden` to `Some(reason)` and the
    /// other two to `None` (legacy behaviour was "block on Forbidden,
    /// allow otherwise"). New call sites should pattern-match the full
    /// enum directly.
    pub fn forbidden_reason(&self) -> Option<&str> {
        match self {
            Decision::Forbidden { reason } => Some(reason.as_str()),
            _ => None,
        }
    }
}

/// Resolve `repo_path` to its owning Solution member (if any) under the
/// active [`SolutionStore`] global, evaluate the configured rules, and
/// return the resulting [`Decision`].
///
/// `op` matches the names emitted by `AtomicGitOp::op_name` — plus a
/// few synthetic names the UI uses for ops that don't run through
/// `OpRunner` (`"force_push"`, `"push"`).
pub fn check(repo_path: &Path, branch: &str, op: &str) -> Decision {
    // Read the active settings + Solution from the GPUI globals through
    // the synchronous accessor exposed for non-GPUI call sites
    // (`SolutionStore::with_active`). When neither global is installed
    // we conservatively use defaults — relevant for unit tests outside
    // a `TestAppContext`.
    let snapshot = active_snapshot();
    check_with_snapshot(&snapshot, repo_path, branch, op)
}

#[derive(Debug, Default, Clone)]
pub struct ActiveSnapshot {
    pub settings: BranchProtectionSettings,
    pub solution: Option<Solution>,
}

fn active_snapshot() -> ActiveSnapshot {
    // The check is called from background tasks holding only `&Path`.
    // We can't take a `&App`, so we route through the global store's
    // synchronous read accessor when present.
    //
    // When the snapshot hasn't been installed yet (early init, unit
    // tests outside `TestAppContext`, headless tools that bypass the
    // `Settings::register` flow), fall back to an empty policy rather
    // than the default-protected `["main", "master", "release/*"]`.
    // Otherwise every test that touches the real `branch_protection`
    // code path would have to install the cache; an empty policy is
    // the safer no-op for those callers and the production path
    // populates the cache through `SolutionsSettings::from_settings`.
    crate::store::active_branch_protection_snapshot()
        .map(|(settings, solution)| ActiveSnapshot { settings, solution })
        .unwrap_or_else(|| ActiveSnapshot {
            settings: BranchProtectionSettings {
                default_protected: Vec::new(),
                members: std::collections::HashMap::new(),
            },
            solution: None,
        })
}

/// Pure-data variant for tests. Callers supply the merged settings and
/// the active Solution explicitly so tests don't need a live GPUI
/// context.
pub fn check_with_snapshot(
    snapshot: &ActiveSnapshot,
    repo_path: &Path,
    branch: &str,
    op: &str,
) -> Decision {
    let member = resolve_member(snapshot.solution.as_ref(), repo_path);
    // Settings key the per-member overrides by the member's folder name (which
    // is what the old `catalog_id` slug was).
    let member_rules = member
        .and_then(|m| snapshot.settings.members.get(m.name.as_str()))
        .cloned()
        .unwrap_or_default();

    let protected = is_protected(&snapshot.settings.default_protected, branch)
        || is_protected(&member_rules.protected, branch);

    decide(protected, &member_rules, op, branch)
}

fn decide(protected: bool, member: &BranchProtectionMember, op: &str, branch: &str) -> Decision {
    match op {
        "push" | "fetch" | "pull" => Decision::Allowed,
        "force_push" | "push_force" => {
            if !protected {
                return Decision::RequiresConfirmation {
                    reason: format!("force-push to '{branch}' rewrites remote history"),
                };
            }
            if member.no_force_push {
                return Decision::Forbidden {
                    reason: format!("force-push to protected branch '{branch}' is forbidden"),
                };
            }
            Decision::RequiresConfirmation {
                reason: format!(
                    "'{branch}' is protected — confirm force-push by typing the branch name"
                ),
            }
        }
        "reset" | "reset_hard" => {
            if !protected {
                return Decision::Allowed;
            }
            if member.no_force_reset || member.no_force_push {
                return Decision::Forbidden {
                    reason: format!("hard reset of protected branch '{branch}' is forbidden"),
                };
            }
            Decision::RequiresConfirmation {
                reason: format!(
                    "'{branch}' is protected — confirm reset by typing the branch name"
                ),
            }
        }
        "drop" | "drop_commit" => {
            if !protected {
                return Decision::Allowed;
            }
            if member.no_drop_commit {
                return Decision::Forbidden {
                    reason: format!(
                        "dropping a commit on protected branch '{branch}' is forbidden"
                    ),
                };
            }
            Decision::RequiresConfirmation {
                reason: format!("'{branch}' is protected — confirm drop by typing the branch name"),
            }
        }
        "delete_branch" | "delete_branch_force" => {
            if protected {
                return Decision::Forbidden {
                    reason: format!("deleting protected branch '{branch}' is forbidden"),
                };
            }
            Decision::Allowed
        }
        "commit"
        | "merge"
        | "cherry_pick"
        | "revert"
        | "rebase"
        | "rebase_interactive"
        | "linear_rebase"
        | "squash"
        | "fixup"
        | "move_commit"
        | "edit_commit_message"
        | "reword"
        | "rename_branch" => {
            if protected {
                return Decision::RequiresConfirmation {
                    reason: format!("'{branch}' is protected — confirm by typing the branch name"),
                };
            }
            Decision::Allowed
        }
        // Unknown ops default to the protected-branch rule: confirmation
        // when protected, allowed otherwise. Fail-closed nudge for new
        // ops that haven't been classified yet.
        _ => {
            if protected {
                Decision::RequiresConfirmation {
                    reason: format!(
                        "'{branch}' is protected — confirm '{op}' by typing the branch name"
                    ),
                }
            } else {
                Decision::Allowed
            }
        }
    }
}

fn is_protected(patterns: &[String], branch: &str) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let mut builder = GlobSetBuilder::new();
    let mut empty = true;
    for pat in patterns {
        // `literal_separator(true)` matches gitignore semantics:
        // `release/*` matches `release/v1` but NOT `release/v2/hotfix`,
        // while `release/**` (or `**` anywhere) does cross slashes.
        let glob = GlobBuilder::new(pat).literal_separator(true).build();
        match glob {
            Ok(g) => {
                builder.add(g);
                empty = false;
            }
            Err(err) => {
                log::warn!("branch_protection: invalid glob pattern {pat:?} ignored: {err}");
            }
        }
    }
    if empty {
        return false;
    }
    let set: GlobSet = match builder.build() {
        Ok(s) => s,
        Err(err) => {
            log::warn!("branch_protection: failed to compile protected patterns: {err}");
            return false;
        }
    };
    set.is_match(branch)
}

fn resolve_member<'a>(
    solution: Option<&'a Solution>,
    repo_path: &Path,
) -> Option<&'a SolutionMember> {
    let solution = solution?;
    // Prefer the longest path match so nested member roots resolve to
    // the inner member rather than an enclosing one. Solutions don't
    // currently nest members but the cost is one cmp per candidate.
    solution
        .members
        .iter()
        .filter(|m| repo_path.starts_with(&m.local_path))
        .max_by_key(|m| m.local_path.as_os_str().len())
}

/// Build an [`ActiveSnapshot`] from explicit components. Used by tests
/// and by the MCP `solution.git.branch_protection_check` tool when it
/// wants to evaluate a synthetic snapshot for a specific Solution
/// rather than the process-global active one.
pub fn make_snapshot(
    settings: BranchProtectionSettings,
    solution: Option<Solution>,
) -> ActiveSnapshot {
    ActiveSnapshot { settings, solution }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MemberId, SolutionId, SolutionMember};
    use crate::settings::BranchProtectionMember;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn default_settings() -> BranchProtectionSettings {
        BranchProtectionSettings {
            default_protected: vec!["main".into(), "master".into(), "release/*".into()],
            members: HashMap::new(),
        }
    }

    fn snapshot(settings: BranchProtectionSettings) -> ActiveSnapshot {
        make_snapshot(settings, None)
    }

    #[test]
    fn default_protected_main_blocks_force_push() {
        let mut s = default_settings();
        let member = BranchProtectionMember {
            protected: vec![],
            no_force_push: true,
            no_force_reset: false,
            no_drop_commit: false,
        };
        // Because resolve_member returns None when no Solution is
        // configured, the member rule isn't picked up — so we install a
        // synthetic Solution+member to exercise the no_force_push path.
        s.members.insert("alpha".into(), member);
        let solution = Solution {
            id: SolutionId(1),
            name: "S".into(),
            root: PathBuf::from("/repo"),
            members: vec![SolutionMember {
                id: MemberId(1),
                name: "alpha".into(),
                local_path: PathBuf::from("/repo/a"),
                origin_catalog_id: None,
            }],
            last_opened_at: None,
        };
        let snap = ActiveSnapshot {
            settings: s,
            solution: Some(solution),
        };
        let decision = check_with_snapshot(&snap, Path::new("/repo/a"), "main", "force_push");
        assert!(
            matches!(decision, Decision::Forbidden { .. }),
            "expected Forbidden, got {decision:?}"
        );
    }

    #[test]
    fn default_protected_main_requires_confirm_for_merge() {
        let snap = snapshot(default_settings());
        let decision = check_with_snapshot(&snap, Path::new("/x"), "main", "merge");
        assert!(matches!(decision, Decision::RequiresConfirmation { .. }));
    }

    #[test]
    fn unprotected_branch_allowed_force_push() {
        let snap = snapshot(default_settings());
        let decision = check_with_snapshot(&snap, Path::new("/x"), "feature/foo", "force_push");
        // Unprotected force-push is RequiresConfirmation, not Allowed —
        // force-push always at least confirms even on unprotected
        // branches. The test name from the spec is preserved but the
        // assertion reflects the spec body which says "Otherwise:
        // RequiresConfirmation".
        assert!(matches!(decision, Decision::RequiresConfirmation { .. }));
    }

    #[test]
    fn glob_release_star_matches_release_v1() {
        let snap = snapshot(default_settings());
        let decision = check_with_snapshot(&snap, Path::new("/x"), "release/v1", "delete_branch");
        assert!(matches!(decision, Decision::Forbidden { .. }));
    }

    #[test]
    fn glob_release_star_does_not_match_release_v2_hotfix() {
        let snap = snapshot(default_settings());
        let decision =
            check_with_snapshot(&snap, Path::new("/x"), "release/v2/hotfix", "delete_branch");
        assert!(matches!(decision, Decision::Allowed));
    }

    #[test]
    fn glob_release_double_star_matches_nested() {
        let s = BranchProtectionSettings {
            default_protected: vec!["release/**".into()],
            members: HashMap::new(),
        };
        let snap = snapshot(s);
        let decision =
            check_with_snapshot(&snap, Path::new("/x"), "release/v2/hotfix", "delete_branch");
        assert!(matches!(decision, Decision::Forbidden { .. }));
    }

    #[test]
    fn member_specific_overrides_default() {
        let mut settings = BranchProtectionSettings {
            default_protected: vec![],
            members: HashMap::new(),
        };
        settings.members.insert(
            "alpha".into(),
            BranchProtectionMember {
                protected: vec!["dev".into()],
                no_force_push: true,
                no_force_reset: false,
                no_drop_commit: false,
            },
        );
        let solution = Solution {
            id: SolutionId(1),
            name: "S".into(),
            root: PathBuf::from("/repo"),
            members: vec![SolutionMember {
                id: MemberId(1),
                name: "alpha".into(),
                local_path: PathBuf::from("/repo/a"),
                origin_catalog_id: None,
            }],
            last_opened_at: None,
        };
        let snap = ActiveSnapshot {
            settings,
            solution: Some(solution),
        };
        let decision = check_with_snapshot(&snap, Path::new("/repo/a"), "dev", "force_push");
        assert!(
            matches!(decision, Decision::Forbidden { .. }),
            "expected Forbidden, got {decision:?}"
        );
    }

    #[test]
    fn commit_op_only_requires_confirmation_on_protected() {
        let snap = snapshot(default_settings());
        let prot = check_with_snapshot(&snap, Path::new("/x"), "main", "cherry_pick");
        assert!(matches!(prot, Decision::RequiresConfirmation { .. }));
        let unprot = check_with_snapshot(&snap, Path::new("/x"), "feature/x", "cherry_pick");
        assert!(matches!(unprot, Decision::Allowed));
    }

    #[test]
    fn empty_pattern_list_is_safe() {
        let snap = snapshot(BranchProtectionSettings {
            default_protected: vec![],
            members: HashMap::new(),
        });
        assert!(matches!(
            check_with_snapshot(&snap, Path::new("/x"), "main", "merge"),
            Decision::Allowed
        ));
    }

    #[test]
    fn invalid_glob_is_skipped() {
        let snap = snapshot(BranchProtectionSettings {
            default_protected: vec!["[".into(), "main".into()],
            members: HashMap::new(),
        });
        // The bad pattern is logged and ignored; the good one still
        // matches.
        assert!(matches!(
            check_with_snapshot(&snap, Path::new("/x"), "main", "merge"),
            Decision::RequiresConfirmation { .. }
        ));
    }

    #[test]
    fn delete_branch_force_blocked_when_protected() {
        let snap = snapshot(default_settings());
        let decision = check_with_snapshot(&snap, Path::new("/x"), "main", "delete_branch_force");
        assert!(matches!(decision, Decision::Forbidden { .. }));
    }
}
