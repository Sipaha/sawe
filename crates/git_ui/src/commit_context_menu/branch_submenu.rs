//! S-CTM per-ref submenu: what decorates a commit, and what the
//! "Branches / Tags at This Commit" submenu offers for it.
//!
//! Split out of `commit_context_menu.rs` unchanged. Everything here is
//! pure — no `App`, no entity handles, no `Window` — which is what lets
//! the exact submenu (including which rows are unavailable and why) be
//! unit-tested without a live repository. Rendering the plan and running
//! the chosen row live in the parent module and in
//! [`super::branch_actions`] respectively.

use gpui::SharedString;
use zed_actions::NewWorktreeBranchTarget;

use crate::handlers::branch::split_remote_ref;

use super::{CommitContext, LocalBranchInfo};

// =====================================================================
//  S-CTM "Branches / Tags at This Commit" section.
//
//  Parses the commit's `%D` ref decorations into branches (local and
//  remote-tracking) and tags, then exposes a per-ref submenu modelled
//  entry-for-entry on IntelliJ IDEA's `Branch '<name>'` submenu — see
//  [`plan_branch_submenu`], which owns the entry list and records which
//  rows this fork cannot back with a real operation (those render
//  disabled with the reason on their info aside rather than vanishing).
//
//  **The current branch is the only ref filtered out** — you cannot
//  check out, merge into, or delete the branch you are standing on.
//  Everything else decorating the commit gets a submenu, local or
//  remote.
//
//  Remote-tracking refs used to be dropped here on the theory that
//  acting on them from a commit row would be surprising. That was the
//  wrong call: a commit whose only decorations are remote (nothing
//  merged locally yet — the common case for the tip of `origin/master`)
//  lost the entire "Branches at This Commit" section, so the ref chips
//  painted on the row and in the detail panel had no menu counterpart at
//  all. They are listed now, marked with the same `IconName::Screen` the
//  branch picker uses for remote entries and labelled with their full
//  `<remote>/<branch>` name.
//
//  `<remote>/HEAD` is the one remaining skip: it is a symbolic ref
//  pointing at the remote's default branch, not a branch of its own, and
//  offering to check it out or delete it would be a lie.
// =====================================================================

pub(super) struct RefsAtCommit {
    pub(super) branches: Vec<BranchRef>,
    pub(super) tags: Vec<SharedString>,
}

/// A branch decoration on a commit, classified against the refs the
/// repository actually knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BranchRef {
    Local(SharedString),
    Remote(RemoteBranchRef),
}

impl BranchRef {
    /// The ref as git spells it — both the menu label and the argument
    /// every operation in the submenu takes.
    pub(super) fn name(&self) -> &SharedString {
        match self {
            Self::Local(name) => name,
            Self::Remote(remote) => &remote.full,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoteBranchRef {
    /// `%D` spelling — `origin/main`, i.e. `refs/remotes/` stripped.
    pub(super) full: SharedString,
    /// `Some((remote, branch))` only when [`Self::full`] starts with one
    /// of the repository's configured remote names. Server-side actions
    /// are withheld when this is `None`: we can't say which remote a
    /// delete or a pull would go to, and guessing means hitting the
    /// wrong one.
    pub(super) split: Option<(SharedString, SharedString)>,
    /// This clone no longer has the ref — git's `[gone]` upstream state.
    ///
    /// Only ever `true` for a ref reached as a *local* branch's
    /// configured upstream: a ref that decorates a commit is still in
    /// `refs/remotes/**` by definition, that being where the decoration
    /// came from.
    pub(super) gone: bool,
}

pub(super) fn refs_at_commit(ctx: &CommitContext) -> RefsAtCommit {
    let local_branch_names: Vec<SharedString> = ctx
        .local_branches
        .iter()
        .map(|branch| branch.name.clone())
        .collect();
    classify_refs(
        &ctx.refs,
        &local_branch_names,
        &ctx.remote_branches,
        &ctx.remotes,
        ctx.head_branch.as_deref(),
    )
}

/// Pure half of [`refs_at_commit`], kept free of [`CommitContext`] so
/// the classification is unit-testable without a live repository.
pub(super) fn classify_refs(
    refs: &[SharedString],
    local_branches: &[SharedString],
    remote_branches: &[SharedString],
    remotes: &[SharedString],
    head_branch: Option<&str>,
) -> RefsAtCommit {
    let mut branches: Vec<BranchRef> = Vec::new();
    let mut tags: Vec<SharedString> = Vec::new();
    for token in refs {
        let token = token.as_ref().trim();
        if token.is_empty() || token == "HEAD" {
            continue;
        }
        if let Some(tag) = token.strip_prefix("tag: ") {
            let tag = tag.trim();
            if !tag.is_empty() && !tags.iter().any(|t| t.as_ref() == tag) {
                tags.push(tag.to_string().into());
            }
            continue;
        }
        let name = token
            .strip_prefix("HEAD -> ")
            .map(str::trim)
            .unwrap_or(token);
        if name.is_empty() {
            continue;
        }
        let Some(branch) = classify_branch_token(name, local_branches, remote_branches, remotes)
        else {
            continue;
        };
        // The current branch is the one ref with nothing to offer:
        // every operation in the submenu is either impossible on it
        // (checkout, merge into itself, `git branch -d`) or already
        // reachable from the top level of this menu.
        if matches!(&branch, BranchRef::Local(name) if Some(name.as_ref()) == head_branch) {
            continue;
        }
        if !branches.iter().any(|known| known.name() == branch.name()) {
            branches.push(branch);
        }
    }
    RefsAtCommit { branches, tags }
}

/// Classify one `%D` branch token. `None` means "don't list it at all",
/// which today only happens for a remote's `HEAD` pointer.
///
/// `%D` lists local branches as bare names and remote-tracking refs as
/// `<remote>/<branch>`, and both are slash-bearing when the local branch
/// name itself contains a `/` (GitFlow `feature/FOO`). The repository's
/// own ref sets settle it: a token the repository lists as a local
/// branch is local no matter how many slashes it has, and the remote is
/// resolved by [`split_remote_ref`].
pub(super) fn classify_branch_token(
    name: &str,
    local_branches: &[SharedString],
    remote_branches: &[SharedString],
    remotes: &[SharedString],
) -> Option<BranchRef> {
    if local_branches.iter().any(|b| b.as_ref() == name) {
        return Some(BranchRef::Local(name.into()));
    }

    if let Some((remote, branch)) = split_remote_ref(name, remotes) {
        if branch == "HEAD" {
            return None;
        }
        return Some(BranchRef::Remote(RemoteBranchRef {
            full: name.into(),
            split: Some((remote, branch)),
            gone: false,
        }));
    }

    let is_known_remote_ref = remote_branches.iter().any(|b| b.as_ref() == name);
    if !is_known_remote_ref && !name.contains('/') {
        return Some(BranchRef::Local(name.into()));
    }
    // Slash-bearing and unclaimed by any known local branch or configured
    // remote: still a remote-tracking ref (that is what `%D` means here),
    // but with no trustworthy remote name, so it gets the subset of the
    // submenu that doesn't need one.
    if name.rsplit('/').next() == Some("HEAD") {
        return None;
    }
    Some(BranchRef::Remote(RemoteBranchRef {
        full: name.into(),
        split: None,
        gone: false,
    }))
}

/// What checking out a remote-tracking ref would do to the local branch
/// of the same name — the question IDEA answers with its "Checkout
/// \<remote ref\>" dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CheckoutDivergence {
    /// No local branch of that name yet: `change_branch` will create it
    /// with `--track` and nothing can be lost. Check out silently.
    NoLocalBranch,
    /// The local branch exists and holds no commits the remote lacks
    /// (or holds commits we can't count, because it tracks something
    /// else). Checking out keeps them either way, so: silent.
    NoKnownDivergence,
    /// The local branch tracks this remote ref and is `ahead` commits
    /// in front of it — checking out silently would leave those commits
    /// stranded on a branch the user thinks they just synced.
    Diverged {
        local_branch: SharedString,
        ahead: u32,
    },
}

pub(super) fn checkout_divergence(
    branch: &RemoteBranchRef,
    local_branches: &[LocalBranchInfo],
) -> CheckoutDivergence {
    // `change_branch` creates/checks out the *branch* half of the ref,
    // so that is the local branch whose commits are at stake. Without a
    // trustworthy split we can't name it.
    let Some((_, local_name)) = branch.split.as_ref() else {
        return CheckoutDivergence::NoLocalBranch;
    };
    let Some(local) = local_branches
        .iter()
        .find(|local| local.name == *local_name)
    else {
        return CheckoutDivergence::NoLocalBranch;
    };
    let tracks_this_ref = local
        .upstream
        .as_ref()
        .is_some_and(|upstream| upstream == &branch.full);
    if tracks_this_ref && local.ahead > 0 {
        CheckoutDivergence::Diverged {
            local_branch: local.name.clone(),
            ahead: local.ahead,
        }
    } else {
        CheckoutDivergence::NoKnownDivergence
    }
}
/// What a row of the per-ref submenu does when clicked. Payloads are
/// resolved while planning (`plan_branch_submenu`) rather than at click
/// time so the plan is a complete description of the menu — the renderer
/// only attaches handlers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BranchAction {
    Checkout,
    NewBranchFrom,
    CheckoutAndRebase {
        head: SharedString,
    },
    CheckoutAndUpdate {
        remote: SharedString,
        /// Branch name **on the remote**, which is not necessarily the
        /// local branch's own name — `git pull <remote> <ref>` names the
        /// remote side, so a local `release` tracking
        /// `origin/release-1.2` must pull `release-1.2`.
        remote_branch: SharedString,
    },
    CompareWithHead,
    ShowDiffWithWorkingTree,
    RebaseHeadOnto,
    MergeIntoHead,
    NewWorktree {
        target: NewWorktreeBranchTarget,
    },
    Update,
    Push {
        remote: SharedString,
        remote_branch: SharedString,
        set_upstream: bool,
    },
    TrackedBranch {
        upstream: RemoteBranchRef,
    },
    Pull {
        remote: SharedString,
        remote_branch: SharedString,
        rebase: bool,
    },
    Rename,
    Delete,
    DeleteOnRemote {
        remote: SharedString,
        branch: SharedString,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SubmenuRow {
    Separator,
    Row {
        action: BranchAction,
        label: SharedString,
        /// `Some(reason)` renders the row disabled with `reason` on its
        /// info aside — the row exists in IDEA but has nothing to call
        /// here (or is refused by policy).
        unavailable: Option<SharedString>,
    },
}

impl SubmenuRow {
    pub(super) fn enabled(action: BranchAction, label: impl Into<SharedString>) -> Self {
        Self::Row {
            action,
            label: label.into(),
            unavailable: None,
        }
    }

    pub(super) fn unavailable(
        action: BranchAction,
        label: impl Into<SharedString>,
        reason: impl Into<SharedString>,
    ) -> Self {
        Self::Row {
            action,
            label: label.into(),
            unavailable: Some(reason.into()),
        }
    }
}

/// The per-ref submenu's entry list, modelled row for row on IntelliJ
/// IDEA's `Branch '<name>'` submenu — same entries, same order, same
/// separators, same interpolation of the branch and head names. Local
/// and remote-tracking refs share everything except five rows: locals
/// get Checkout and Update / Update / Push… / Tracked Branch / Rename…,
/// remotes get the two "Pull into" rows.
///
/// Kept pure (no `App`, no entity handles) so the exact list — including
/// which rows are unavailable and why — is unit-testable.
///
/// Row → operation, and why the unavailable ones are unavailable:
/// - **Checkout** → `Repository::change_branch`, which for a remote ref
///   creates or re-points the matching local branch with `--track` and
///   checks *that* out. Guarded by [`checkout_divergence`].
/// - **New Branch from '\<ref\>'…** → `Repository::create_branch(name,
///   Some(ref))` = `git switch -c <name> <ref>` (IDEA's dialog checks
///   the new branch out by default too).
/// - **Checkout and Rebase onto '\<head\>'** → checkout, then
///   [`rebase`], which rebases the now-current branch.
/// - **Checkout and Update** → checkout, then `Repository::pull` on the
///   branch's own upstream. Unavailable when it tracks nothing.
/// - **Compare with '\<head\>'** → *unavailable*: comparing two branches
///   needs a commit-vs-commit diff, and `ProjectDiff` only diffs the
///   working tree against one base ref.
/// - **Show Diff with Working Tree** → [`compare`], keyed on the commit
///   sha — which is this ref's tip, that being why it decorates the row.
/// - **Rebase '\<head\>' onto '\<ref\>'** / **Merge '\<ref\>' into
///   '\<head\>'** → [`rebase`] / [`merge`].
/// - **New Worktree…** → `zed_actions::CreateWorktree` with the ref as
///   the new worktree's branch target.
/// - **Update** (local) → *unavailable*: advancing a branch you are not
///   on is `git fetch <remote> <branch>:<branch>`, and this fork's fetch
///   API takes no refspec. "Checkout and Update" is the wired
///   equivalent.
/// - **Push…** (local) → `Repository::push` on that branch behind a
///   confirm. Unavailable when its upstream names no configured remote,
///   and when it tracks nothing while the repository has anything other
///   than exactly one remote.
/// - **Tracked Branch '\<upstream\>'** (local) → this same submenu for
///   the upstream ref. Omitted — as in IDEA — when nothing is tracked.
/// - **Pull into '\<head\>' Using Rebase / Using Merge** (remote) →
///   `Repository::pull` with `rebase` true / false.
/// - **Rename…** (local) → `RenameBranchModal` → `rename_branch`.
/// - **Delete** → local `git branch -d` (with the force escape hatch) or
///   the server-side remote delete. Unavailable when branch protection
///   forbids it, which is why IDEA greys this row out on its own
///   protected branches.
///
/// Rows whose label names the current branch are omitted on a detached
/// HEAD: there is no branch to name, and every one of them (checkout and
/// rebase onto, compare with, rebase/merge, pull into) is meaningless
/// without one.
pub(super) fn plan_branch_submenu(
    branch: &BranchRef,
    head: Option<&SharedString>,
    local_branches: &[LocalBranchInfo],
    remotes: &[SharedString],
    delete_forbidden: Option<SharedString>,
) -> Vec<SubmenuRow> {
    let name = branch.name().clone();
    let remote_ref = match branch {
        BranchRef::Remote(remote) => Some(remote),
        BranchRef::Local(_) => None,
    };
    let local_info = match branch {
        BranchRef::Local(local_name) => local_branches.iter().find(|info| info.name == *local_name),
        BranchRef::Remote(_) => None,
    };
    let upstream_gone = local_info.is_some_and(|info| info.upstream_gone);
    let upstream = local_info.and_then(|info| info.upstream.clone());
    let upstream_ref = upstream.map(|upstream| RemoteBranchRef {
        split: split_remote_ref(&upstream, remotes),
        full: upstream,
        gone: upstream_gone,
    });

    let mut groups: Vec<Vec<SubmenuRow>> = Vec::new();

    let mut checkout_group = vec![
        SubmenuRow::enabled(BranchAction::Checkout, "Checkout"),
        SubmenuRow::enabled(
            BranchAction::NewBranchFrom,
            format!("New Branch from '{name}'…"),
        ),
    ];
    if let Some(head) = head {
        checkout_group.push(SubmenuRow::enabled(
            BranchAction::CheckoutAndRebase { head: head.clone() },
            format!("Checkout and Rebase onto '{head}'"),
        ));
    }
    if remote_ref.is_none() {
        checkout_group.push(match &upstream_ref {
            // A `[gone]` upstream is a tracking ref git has pruned, so
            // `git pull <remote> <branch>` answers `couldn't find remote ref`.
            // Only the delete row survives a gone upstream, and it says so
            // itself — see `plan_delete_row`.
            Some(upstream) if upstream.gone => SubmenuRow::unavailable(
                BranchAction::Update,
                "Checkout and Update",
                format!(
                    "“{}” is gone from this clone — git still lists it as the configured \
                     upstream, but the tracking ref itself has been pruned, so there is \
                     nothing to update from. Fetch first.",
                    upstream.full
                ),
            ),
            Some(upstream) => match upstream.split.clone() {
                Some((remote, remote_branch)) => SubmenuRow::enabled(
                    BranchAction::CheckoutAndUpdate {
                        remote,
                        remote_branch,
                    },
                    "Checkout and Update",
                ),
                None => SubmenuRow::unavailable(
                    BranchAction::Update,
                    "Checkout and Update",
                    format!(
                        "“{}” is not one of this repository's configured remotes, so there is \
                         no remote to pull from.",
                        upstream.full
                    ),
                ),
            },
            None => SubmenuRow::unavailable(
                BranchAction::Update,
                "Checkout and Update",
                format!(
                    "“{name}” tracks no upstream branch, so there is nothing to update it from."
                ),
            ),
        });
    }
    groups.push(checkout_group);

    let mut compare_group = Vec::new();
    if let Some(head) = head {
        compare_group.push(SubmenuRow::unavailable(
            BranchAction::CompareWithHead,
            format!("Compare with '{head}'"),
            "Comparing two branches needs a commit-vs-commit diff. This fork's ProjectDiff only \
             diffs the working tree against a single base ref — that is “Show Diff with Working \
             Tree” below.",
        ));
    }
    compare_group.push(SubmenuRow::enabled(
        BranchAction::ShowDiffWithWorkingTree,
        "Show Diff with Working Tree",
    ));
    groups.push(compare_group);

    if let Some(head) = head {
        groups.push(vec![
            SubmenuRow::enabled(
                BranchAction::RebaseHeadOnto,
                format!("Rebase '{head}' onto '{name}'"),
            ),
            SubmenuRow::enabled(
                BranchAction::MergeIntoHead,
                format!("Merge '{name}' into '{head}'"),
            ),
        ]);
    }

    groups.push(vec![match branch {
        BranchRef::Local(local_name) => SubmenuRow::enabled(
            BranchAction::NewWorktree {
                target: NewWorktreeBranchTarget::ExistingBranch {
                    name: local_name.to_string(),
                },
            },
            "New Worktree…",
        ),
        BranchRef::Remote(remote) => match remote.split.clone() {
            Some((remote_name, branch_name)) => SubmenuRow::enabled(
                BranchAction::NewWorktree {
                    target: NewWorktreeBranchTarget::RemoteBranch {
                        remote_name: remote_name.to_string(),
                        branch_name: branch_name.to_string(),
                    },
                },
                "New Worktree…",
            ),
            None => SubmenuRow::unavailable(
                BranchAction::NewWorktree {
                    target: NewWorktreeBranchTarget::CurrentBranch,
                },
                "New Worktree…",
                format!(
                    "“{name}” doesn't resolve to one of this repository's configured remotes, \
                     so the new worktree's branch target can't be named."
                ),
            ),
        },
    }]);

    let mut traffic_group = Vec::new();
    match remote_ref {
        None => {
            traffic_group.push(SubmenuRow::unavailable(
                BranchAction::Update,
                "Update",
                format!(
                    "Advancing “{name}” without checking it out needs `git fetch <remote> \
                     {name}:{name}`, and this fork's fetch API takes no refspec. Use “Checkout \
                     and Update”."
                ),
            ));
            traffic_group.push(plan_push_row(&name, upstream_ref.as_ref(), remotes));
            if let Some(upstream) = upstream_ref {
                traffic_group.push(SubmenuRow::enabled(
                    BranchAction::TrackedBranch {
                        upstream: upstream.clone(),
                    },
                    format!("Tracked Branch '{}'", upstream.full),
                ));
            }
        }
        Some(remote) => {
            if let Some(head) = head {
                traffic_group.extend(plan_pull_rows(remote, head));
            }
        }
    }
    groups.push(traffic_group);

    let mut final_group = Vec::new();
    if remote_ref.is_none() {
        final_group.push(SubmenuRow::enabled(BranchAction::Rename, "Rename…"));
    }
    final_group.push(plan_delete_row(branch, delete_forbidden));
    groups.push(final_group);

    let mut rows = Vec::new();
    for group in groups.into_iter().filter(|group| !group.is_empty()) {
        if !rows.is_empty() {
            rows.push(SubmenuRow::Separator);
        }
        rows.extend(group);
    }
    // Every row above acts on the ref it names, and a `[gone]` tracking ref is
    // not in this clone any more: `Checkout`, `New Branch from`, `New
    // Worktree`, the diff and the rebase/merge pair would each hand git a
    // pathspec it cannot resolve. Disabling them here rather than at each
    // construction site keeps the submenu's shape identical to a live ref's —
    // IDEA shows the rows and greys them — and cannot miss a row added later.
    // Rows that already carry their own reason keep it; `Delete` is the one
    // that stays meaningful, and `plan_delete_row` writes its own.
    if let BranchRef::Remote(remote) = branch
        && remote.gone
    {
        let reason = SharedString::from(format!(
            "“{}” is gone from this clone — git still lists it as the configured upstream, \
             but the tracking ref itself has been pruned. Fetch to find out whether it is \
             still on the remote.",
            remote.full
        ));
        for row in &mut rows {
            if let SubmenuRow::Row {
                action,
                unavailable,
                ..
            } = row
                && unavailable.is_none()
                && !matches!(action, BranchAction::Delete)
            {
                *unavailable = Some(reason.clone());
            }
        }
    }
    rows
}

/// "Push…" for a branch that isn't the current one. `PushDialog` only
/// knows how to push the checked-out branch, but `Repository::push`
/// takes any branch — as long as we can name the destination.
pub(super) fn plan_push_row(
    branch: &SharedString,
    upstream: Option<&RemoteBranchRef>,
    remotes: &[SharedString],
) -> SubmenuRow {
    let unavailable = |reason: String| {
        SubmenuRow::unavailable(
            BranchAction::Push {
                remote: SharedString::default(),
                remote_branch: branch.clone(),
                set_upstream: false,
            },
            "Push…",
            reason,
        )
    };
    let (remote, remote_branch, set_upstream) = match upstream {
        Some(upstream) => match upstream.split.clone() {
            Some((remote, remote_branch)) => (remote, remote_branch, false),
            // An upstream *is* configured, it just doesn't resolve
            // against any configured remote. Falling through to the
            // single-remote guess below would push to a remote this
            // branch does not track and, with `--set-upstream`, silently
            // re-point its tracking config — while the confirmation
            // claims the branch has no upstream yet.
            None => {
                return unavailable(format!(
                    "“{}” is not one of this repository's configured remotes, so there is no \
                     remote to push to.",
                    upstream.full
                ));
            }
        },
        // No upstream at all: only auto-resolve when the repository
        // leaves no room for doubt, i.e. it has exactly one remote. That
        // is a `--set-upstream` push, like IDEA's first push of a branch.
        None => match remotes {
            [only_remote] => (only_remote.clone(), branch.clone(), true),
            _ => {
                return unavailable(format!(
                    "“{branch}” tracks no upstream and this repository has {} remotes, so the \
                     destination is ambiguous. Check the branch out and use the Push dialog, \
                     which resolves it interactively.",
                    remotes.len()
                ));
            }
        },
    };
    SubmenuRow::enabled(
        BranchAction::Push {
            remote,
            remote_branch,
            set_upstream,
        },
        "Push…",
    )
}

pub(super) fn plan_pull_rows(branch: &RemoteBranchRef, head: &SharedString) -> Vec<SubmenuRow> {
    let Some((remote, remote_branch)) = branch.split.clone() else {
        let reason = format!(
            "“{}” doesn't resolve to one of this repository's configured remotes, so there is \
             no remote to pull from.",
            branch.full
        );
        return vec![
            SubmenuRow::unavailable(
                BranchAction::Pull {
                    remote: SharedString::default(),
                    remote_branch: branch.full.clone(),
                    rebase: true,
                },
                format!("Pull into '{head}' Using Rebase"),
                reason.clone(),
            ),
            SubmenuRow::unavailable(
                BranchAction::Pull {
                    remote: SharedString::default(),
                    remote_branch: branch.full.clone(),
                    rebase: false,
                },
                format!("Pull into '{head}' Using Merge"),
                reason,
            ),
        ];
    };
    vec![
        SubmenuRow::enabled(
            BranchAction::Pull {
                remote: remote.clone(),
                remote_branch: remote_branch.clone(),
                rebase: true,
            },
            format!("Pull into '{head}' Using Rebase"),
        ),
        SubmenuRow::enabled(
            BranchAction::Pull {
                remote,
                remote_branch,
                rebase: false,
            },
            format!("Pull into '{head}' Using Merge"),
        ),
    ]
}

pub(super) fn plan_delete_row(
    branch: &BranchRef,
    delete_forbidden: Option<SharedString>,
) -> SubmenuRow {
    let action = match branch {
        BranchRef::Local(_) => BranchAction::Delete,
        // Reached through a local branch's "Tracked Branch" submenu for
        // an upstream git reports as `[gone]`: the ref this row would
        // delete is not in this clone any more, and offering a
        // server-side delete for it is how a row stays live for a branch
        // that provably is not there. Nothing here proves it is gone on
        // the *server* — that is a separate question, answered by the
        // delete itself — so the row is disabled and says why rather
        // than silently disappearing.
        BranchRef::Remote(remote) if remote.gone => {
            return SubmenuRow::unavailable(
                BranchAction::Delete,
                "Delete",
                format!(
                    "“{}” is gone from this clone — git still lists it as the configured \
                     upstream, but the tracking ref itself has been pruned. Fetch to find out \
                     whether it is still on the remote.",
                    remote.full
                ),
            );
        }
        BranchRef::Remote(remote) => match remote.split.clone() {
            Some((remote_name, branch_name)) => BranchAction::DeleteOnRemote {
                remote: remote_name,
                branch: branch_name,
            },
            None => {
                return SubmenuRow::unavailable(
                    BranchAction::Delete,
                    "Delete",
                    format!(
                        "“{}” doesn't resolve to one of this repository's configured remotes, \
                         so there is no remote to delete it on.",
                        remote.full
                    ),
                );
            }
        },
    };
    match delete_forbidden {
        Some(reason) => SubmenuRow::unavailable(action, "Delete", reason),
        None => SubmenuRow::enabled(action, "Delete"),
    }
}

/// The branch name branch protection is keyed on. A remote-tracking ref
/// is checked under its branch half (`master`, not `origin/master`), so
/// a protected `master` protects both sides.
pub(super) fn protected_branch_name(branch: &BranchRef) -> Option<SharedString> {
    match branch {
        BranchRef::Local(name) => Some(name.clone()),
        BranchRef::Remote(remote) => remote.split.as_ref().map(|(_, branch)| branch.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<SharedString> {
        values
            .iter()
            .map(|value| SharedString::from(value.to_string()))
            .collect()
    }

    fn local(name: &str) -> BranchRef {
        BranchRef::Local(name.into())
    }

    fn remote(full: &str, split: Option<(&str, &str)>) -> BranchRef {
        BranchRef::Remote(RemoteBranchRef {
            full: full.into(),
            split: split.map(|(remote, branch)| (remote.into(), branch.into())),
            gone: false,
        })
    }

    /// The bug that motivated listing remote refs: the commit carried
    /// only `origin/master` + `origin/HEAD`, so the old local-only filter
    /// emptied the whole "Branches at This Commit" section.
    #[test]
    fn test_commit_with_only_remote_refs_still_lists_branches() {
        let refs = classify_refs(
            &strings(&["tag: 3.29.0", "origin/master", "origin/HEAD"]),
            &strings(&["main"]),
            &strings(&["origin/master", "origin/HEAD"]),
            &strings(&["origin"]),
            Some("main"),
        );
        assert_eq!(
            refs.branches,
            vec![remote("origin/master", Some(("origin", "master")))]
        );
        assert_eq!(refs.tags, strings(&["3.29.0"]));
    }

    #[test]
    fn test_mixed_local_and_remote_refs_are_classified_apart() {
        let refs = classify_refs(
            &strings(&["HEAD -> main", "origin/main", "upstream/main"]),
            &strings(&["main"]),
            &strings(&["origin/main", "upstream/main"]),
            &strings(&["origin", "upstream"]),
            None,
        );
        assert_eq!(
            refs.branches,
            vec![
                local("main"),
                remote("origin/main", Some(("origin", "main"))),
                remote("upstream/main", Some(("upstream", "main"))),
            ]
        );
    }

    /// A GitFlow-style local branch is slash-bearing like a
    /// remote-tracking ref; only the repository's local-branch list tells
    /// them apart, and it must win over a remote name that happens to
    /// prefix it.
    #[test]
    fn test_gitflow_local_branch_with_slash_stays_local() {
        let refs = classify_refs(
            &strings(&["HEAD -> feature/COREDEV-432", "origin/feature/COREDEV-432"]),
            &strings(&["feature/COREDEV-432"]),
            &strings(&["origin/feature/COREDEV-432"]),
            &strings(&["origin", "feature"]),
            None,
        );
        assert_eq!(
            refs.branches,
            vec![
                local("feature/COREDEV-432"),
                remote(
                    "origin/feature/COREDEV-432",
                    Some(("origin", "feature/COREDEV-432"))
                ),
            ]
        );
    }

    #[test]
    fn test_non_origin_remote_is_split_against_the_configured_remotes() {
        let refs = classify_refs(
            &strings(&["fork/main", "team/fork/main"]),
            &[],
            &strings(&["fork/main", "team/fork/main"]),
            // `team/fork` is a legal (if pathological) remote name, and
            // the longest matching remote has to win: splitting on the
            // first `/` would push to a "team" remote that doesn't exist.
            &strings(&["fork", "team/fork"]),
            None,
        );
        assert_eq!(
            refs.branches,
            vec![
                remote("fork/main", Some(("fork", "main"))),
                remote("team/fork/main", Some(("team/fork", "main"))),
            ]
        );
    }

    /// `<remote>/HEAD` is a symbolic ref for the remote's default branch,
    /// not a branch — offering Checkout / Delete on it would be a lie.
    #[test]
    fn test_remote_head_pointer_is_never_listed() {
        let known = classify_refs(
            &strings(&["origin/HEAD"]),
            &[],
            &strings(&["origin/HEAD"]),
            &strings(&["origin"]),
            None,
        );
        assert!(known.branches.is_empty());

        // Same call with nothing known about the repository: the
        // trailing-segment check has to catch it too.
        let unknown = classify_refs(&strings(&["origin/HEAD"]), &[], &[], &[], None);
        assert!(unknown.branches.is_empty());
    }

    /// With no configured-remote list, a slash-bearing unknown token is
    /// still shown as a remote-tracking ref — but unsplit, which is what
    /// withholds the server-side "Delete on <remote>…" entry.
    #[test]
    fn test_unsplittable_remote_ref_keeps_local_only_actions() {
        let refs = classify_refs(&strings(&["origin/main", "main"]), &[], &[], &[], None);
        assert_eq!(
            refs.branches,
            vec![remote("origin/main", None), local("main")]
        );
    }

    fn local_info(name: &str, upstream: Option<&str>, ahead: u32) -> LocalBranchInfo {
        LocalBranchInfo {
            name: name.into(),
            upstream: upstream.map(Into::into),
            upstream_gone: false,
            ahead,
        }
    }

    /// A local branch git reports as `<name> [gone]`: the upstream is
    /// still configured, the remote-tracking ref is not there any more.
    fn local_info_with_gone_upstream(name: &str, upstream: &str) -> LocalBranchInfo {
        LocalBranchInfo {
            name: name.into(),
            upstream: Some(upstream.into()),
            upstream_gone: true,
            ahead: 0,
        }
    }

    fn remote_ref(full: &str, split: Option<(&str, &str)>) -> RemoteBranchRef {
        RemoteBranchRef {
            full: full.into(),
            split: split.map(|(remote, branch)| (remote.into(), branch.into())),
            gone: false,
        }
    }

    /// Labels in menu order, with separators as `"---"` and the
    /// unavailable rows marked, so a test reads like the screenshot.
    fn row_labels(rows: &[SubmenuRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                SubmenuRow::Separator => "---".to_string(),
                SubmenuRow::Row {
                    label, unavailable, ..
                } => match unavailable {
                    Some(_) => format!("{label} (disabled)"),
                    None => label.to_string(),
                },
            })
            .collect()
    }

    /// The current branch is the *only* ref that gets filtered out of
    /// the section — everything else at the commit is listed.
    #[test]
    fn test_only_the_current_branch_is_filtered_out() {
        let refs = classify_refs(
            &strings(&["HEAD -> master222", "master", "origin/master"]),
            &strings(&["master222", "master"]),
            &strings(&["origin/master"]),
            &strings(&["origin"]),
            Some("master222"),
        );
        assert_eq!(
            refs.branches,
            vec![
                local("master"),
                remote("origin/master", Some(("origin", "master"))),
            ]
        );
    }

    /// The IDEA reference for a local branch (`master`, while on
    /// `master222`, tracking `origin/master`).
    #[test]
    fn test_local_branch_submenu_matches_idea_layout() {
        let rows = plan_branch_submenu(
            &BranchRef::Local("master".into()),
            Some(&"master222".into()),
            &[local_info("master", Some("origin/master"), 0)],
            &strings(&["origin"]),
            None,
        );
        assert_eq!(
            row_labels(&rows),
            vec![
                "Checkout",
                "New Branch from 'master'…",
                "Checkout and Rebase onto 'master222'",
                "Checkout and Update",
                "---",
                "Compare with 'master222' (disabled)",
                "Show Diff with Working Tree",
                "---",
                "Rebase 'master222' onto 'master'",
                "Merge 'master' into 'master222'",
                "---",
                "New Worktree…",
                "---",
                "Update (disabled)",
                "Push…",
                "Tracked Branch 'origin/master'",
                "---",
                "Rename…",
                "Delete",
            ]
        );
    }

    /// The IDEA reference for a remote branch (`origin/master`, while on
    /// `master222`). Note Delete is *enabled* here — the reference shot
    /// greys it out because IDEA protects `master` by default, which is
    /// the `delete_forbidden` case covered separately below.
    #[test]
    fn test_remote_branch_submenu_matches_idea_layout() {
        let rows = plan_branch_submenu(
            &BranchRef::Remote(remote_ref("origin/master", Some(("origin", "master")))),
            Some(&"master222".into()),
            &[local_info("master", Some("origin/master"), 0)],
            &strings(&["origin"]),
            None,
        );
        assert_eq!(
            row_labels(&rows),
            vec![
                "Checkout",
                "New Branch from 'origin/master'…",
                "Checkout and Rebase onto 'master222'",
                "---",
                "Compare with 'master222' (disabled)",
                "Show Diff with Working Tree",
                "---",
                "Rebase 'master222' onto 'origin/master'",
                "Merge 'origin/master' into 'master222'",
                "---",
                "New Worktree…",
                "---",
                "Pull into 'master222' Using Rebase",
                "Pull into 'master222' Using Merge",
                "---",
                "Delete",
            ]
        );
    }

    /// Branch protection greys Delete out rather than removing it —
    /// same as the reference screenshot, where IDEA's protected
    /// `master` leaves a disabled row.
    #[test]
    fn test_protected_branch_disables_delete_instead_of_hiding_it() {
        let rows = plan_branch_submenu(
            &BranchRef::Remote(remote_ref("origin/master", Some(("origin", "master")))),
            Some(&"master222".into()),
            &[],
            &strings(&["origin"]),
            Some("deleting protected branch 'master' is forbidden".into()),
        );
        assert_eq!(
            rows.last(),
            Some(&SubmenuRow::unavailable(
                BranchAction::DeleteOnRemote {
                    remote: "origin".into(),
                    branch: "master".into(),
                },
                "Delete",
                "deleting protected branch 'master' is forbidden",
            ))
        );
    }

    /// Re-enter the planner the way `build_branch_ref_submenu` does for
    /// a "Tracked Branch '<upstream>'" row: the nested submenu is this
    /// same function called with `BranchRef::Remote(upstream)`.
    fn tracked_branch_submenu(rows: &[SubmenuRow], remotes: &[SharedString]) -> Vec<SubmenuRow> {
        let upstream = rows
            .iter()
            .find_map(|row| match row {
                SubmenuRow::Row {
                    action: BranchAction::TrackedBranch { upstream },
                    ..
                } => Some(upstream.clone()),
                _ => None,
            })
            .expect("a tracking local branch must offer a Tracked Branch row");
        plan_branch_submenu(
            &BranchRef::Remote(upstream),
            Some(&"master222".into()),
            &[],
            remotes,
            None,
        )
    }

    /// The direct route to the server-side delete: the upstream ref is
    /// really there, so the row is live.
    #[test]
    fn test_a_live_upstream_offers_delete_on_remote() {
        let remotes = strings(&["origin"]);
        let rows = plan_branch_submenu(
            &BranchRef::Local("feature".into()),
            Some(&"master222".into()),
            &[local_info("feature", Some("origin/feature"), 0)],
            &remotes,
            None,
        );
        assert_eq!(
            tracked_branch_submenu(&rows, &remotes).last(),
            Some(&SubmenuRow::enabled(
                BranchAction::DeleteOnRemote {
                    remote: "origin".into(),
                    branch: "feature".into(),
                },
                "Delete",
            ))
        );
    }

    /// A pruned tracking ref is not in this clone, so every row that would
    /// hand git that ref as a pathspec — `Checkout`, `New Branch from`,
    /// `New Worktree…`, the diff, the rebase/merge pair — is greyed with a
    /// reason instead of failing at the git call. `Delete` is the exception:
    /// it stays the one meaningful action and carries its own wording.
    #[test]
    fn test_a_gone_upstream_greys_every_row_that_needs_the_ref() {
        let remotes = strings(&["origin"]);
        let rows = plan_branch_submenu(
            &BranchRef::Local("feature".into()),
            Some(&"master222".into()),
            &[local_info_with_gone_upstream("feature", "origin/feature")],
            &remotes,
            None,
        );
        for row in tracked_branch_submenu(&rows, &remotes) {
            let SubmenuRow::Row {
                action,
                label,
                unavailable,
            } = row
            else {
                continue;
            };
            assert!(
                unavailable.is_some(),
                "“{label}” acts on a ref this clone no longer has ({action:?})"
            );
        }
    }

    /// The local branch's own group asks the same question from the other
    /// side: `Checkout and Update` is `git pull <remote> <branch>`, which
    /// answers `couldn't find remote ref` once the upstream is pruned.
    #[test]
    fn test_a_gone_upstream_withholds_checkout_and_update() {
        let rows = plan_branch_submenu(
            &BranchRef::Local("feature".into()),
            Some(&"master222".into()),
            &[local_info_with_gone_upstream("feature", "origin/feature")],
            &strings(&["origin"]),
            None,
        );
        let update = rows
            .iter()
            .find_map(|row| match row {
                SubmenuRow::Row {
                    label,
                    unavailable,
                    action,
                } if label.as_ref() == "Checkout and Update" => Some((action, unavailable)),
                _ => None,
            })
            .expect("the row is shown for a tracking local branch");
        assert!(
            update.1.is_some(),
            "a [gone] upstream has nothing to pull from"
        );
        assert!(
            !matches!(update.0, BranchAction::CheckoutAndUpdate { .. }),
            "and must not carry the action that would run the pull"
        );
    }

    /// Git keeps reporting the configured upstream as `[gone]` after a
    /// pruning fetch removed the tracking ref, so "tracks origin/feature"
    /// alone would leave a live Delete row for a branch this clone
    /// provably no longer has.
    #[test]
    fn test_a_gone_upstream_withholds_delete_on_remote() {
        let remotes = strings(&["origin"]);
        let rows = plan_branch_submenu(
            &BranchRef::Local("feature".into()),
            Some(&"master222".into()),
            &[local_info_with_gone_upstream("feature", "origin/feature")],
            &remotes,
            None,
        );
        let delete_row = tracked_branch_submenu(&rows, &remotes)
            .last()
            .cloned()
            .expect("the submenu always ends in a Delete row");
        let SubmenuRow::Row {
            action,
            label,
            unavailable,
        } = delete_row
        else {
            panic!("expected a Delete row, got a separator");
        };
        assert_eq!(label, "Delete");
        assert_ne!(
            action,
            BranchAction::DeleteOnRemote {
                remote: "origin".into(),
                branch: "feature".into(),
            },
            "a [gone] upstream must not carry a server-side delete action"
        );
        assert!(
            unavailable.is_some_and(|reason| reason.contains("gone from this clone")),
            "the disabled row must say why it is disabled"
        );
    }

    /// A branch that tracks nothing can neither be updated nor pushed
    /// without guessing — both rows stay, disabled, and "Tracked
    /// Branch" disappears exactly as it does in IDEA.
    #[test]
    fn test_untracked_local_branch_disables_the_rows_that_need_a_remote() {
        let rows = plan_branch_submenu(
            &BranchRef::Local("scratch".into()),
            Some(&"master222".into()),
            &[local_info("scratch", None, 0)],
            &strings(&["origin", "upstream"]),
            None,
        );
        let labels = row_labels(&rows);
        assert!(labels.contains(&"Checkout and Update (disabled)".to_string()));
        assert!(labels.contains(&"Push… (disabled)".to_string()));
        assert!(
            !labels
                .iter()
                .any(|label| label.starts_with("Tracked Branch")),
            "a branch with no upstream has no tracked-branch submenu: {labels:?}"
        );
    }

    /// With exactly one remote configured, a branch with no upstream can
    /// still be pushed — it becomes the `--set-upstream` first push.
    #[test]
    fn test_single_remote_resolves_push_for_an_untracked_branch() {
        let rows = plan_branch_submenu(
            &BranchRef::Local("scratch".into()),
            Some(&"master222".into()),
            &[local_info("scratch", None, 0)],
            &strings(&["origin"]),
            None,
        );
        assert!(rows.contains(&SubmenuRow::enabled(
            BranchAction::Push {
                remote: "origin".into(),
                remote_branch: "scratch".into(),
                set_upstream: true,
            },
            "Push…",
        )));
    }

    /// "no upstream at all" and "an upstream that names no configured
    /// remote" are different states, and only the first one may take the
    /// single-remote `--set-upstream` shortcut. Pushing the second one to
    /// the lone remote would both target a remote the branch does not
    /// track and re-point its tracking config, behind a confirmation that
    /// asserts the branch has no upstream.
    #[test]
    fn test_unresolvable_upstream_does_not_take_the_single_remote_fallback() {
        let row = plan_push_row(
            &"release".into(),
            Some(&remote_ref("gone/release", None)),
            &strings(&["origin"]),
        );
        assert_eq!(
            row,
            SubmenuRow::unavailable(
                BranchAction::Push {
                    remote: SharedString::default(),
                    remote_branch: "release".into(),
                    set_upstream: false,
                },
                "Push…",
                "“gone/release” is not one of this repository's configured remotes, so there is \
                 no remote to push to.",
            )
        );
    }

    /// The same unresolvable upstream with several remotes: still
    /// unavailable, and still for the upstream's reason rather than the
    /// ambiguity one — the branch's problem is that its upstream names
    /// nothing, not that the menu can't pick between remotes.
    #[test]
    fn test_unresolvable_upstream_reports_the_upstream_reason_not_ambiguity() {
        let rows = plan_branch_submenu(
            &BranchRef::Local("release".into()),
            Some(&"master222".into()),
            &[local_info("release", Some("gone/release"), 0)],
            &strings(&["origin", "upstream"]),
            None,
        );
        let reason = rows
            .iter()
            .find_map(|row| match row {
                SubmenuRow::Row {
                    label, unavailable, ..
                } if label.as_ref() == "Push…" => unavailable.clone(),
                _ => None,
            })
            .expect("Push… must be present and unavailable");
        assert!(
            reason.contains("is not one of this repository's configured remotes"),
            "unexpected reason: {reason}"
        );
    }

    /// A branch with no upstream at all keeps the ambiguity wording when
    /// the repository has more than one remote.
    #[test]
    fn test_untracked_branch_with_several_remotes_is_ambiguous() {
        let row = plan_push_row(&"scratch".into(), None, &strings(&["origin", "upstream"]));
        assert_eq!(
            row,
            SubmenuRow::unavailable(
                BranchAction::Push {
                    remote: SharedString::default(),
                    remote_branch: "scratch".into(),
                    set_upstream: false,
                },
                "Push…",
                "“scratch” tracks no upstream and this repository has 2 remotes, so the \
                 destination is ambiguous. Check the branch out and use the Push dialog, which \
                 resolves it interactively.",
            )
        );
    }

    /// "Checkout and Update" runs `git pull <remote> <ref>`, whose
    /// trailing positional names the branch **on the remote** — which is
    /// the local branch's name only by coincidence. Every other fixture
    /// in this file tracks `master` → `origin/master`, where passing the
    /// local name happens to work, which is exactly why the mismatch
    /// went unnoticed.
    #[test]
    fn test_checkout_and_update_pulls_the_upstream_branch_name() {
        let rows = plan_branch_submenu(
            &BranchRef::Local("release".into()),
            Some(&"master222".into()),
            &[local_info("release", Some("origin/release-1.2"), 0)],
            &strings(&["origin"]),
            None,
        );
        assert!(
            rows.contains(&SubmenuRow::enabled(
                BranchAction::CheckoutAndUpdate {
                    remote: "origin".into(),
                    remote_branch: "release-1.2".into(),
                },
                "Checkout and Update",
            )),
            "Checkout and Update must pull the upstream's own branch name: {:?}",
            row_labels(&rows)
        );
    }

    /// The same row against a slash-bearing remote: the remote/branch
    /// boundary comes from [`split_remote_ref`], not from the first `/`.
    #[test]
    fn test_checkout_and_update_splits_a_slash_bearing_remote() {
        let rows = plan_branch_submenu(
            &BranchRef::Local("release".into()),
            Some(&"master222".into()),
            &[local_info("release", Some("team/fork/release-1.2"), 0)],
            &strings(&["origin", "team/fork"]),
            None,
        );
        assert!(rows.contains(&SubmenuRow::enabled(
            BranchAction::CheckoutAndUpdate {
                remote: "team/fork".into(),
                remote_branch: "release-1.2".into(),
            },
            "Checkout and Update",
        )));
    }

    /// On a detached HEAD every row that would name the current branch
    /// is dropped; the rest of the submenu still works.
    #[test]
    fn test_detached_head_drops_the_rows_that_name_a_branch() {
        let rows = plan_branch_submenu(
            &BranchRef::Remote(remote_ref("origin/master", Some(("origin", "master")))),
            None,
            &[],
            &strings(&["origin"]),
            None,
        );
        assert_eq!(
            row_labels(&rows),
            vec![
                "Checkout",
                "New Branch from 'origin/master'…",
                "---",
                "Show Diff with Working Tree",
                "---",
                "New Worktree…",
                "---",
                "Delete",
            ]
        );
    }

    /// Checking out `origin/master` while local `master` holds commits
    /// the remote doesn't have must ask before doing anything.
    #[test]
    fn test_diverged_local_branch_is_detected_before_checkout() {
        assert_eq!(
            checkout_divergence(
                &remote_ref("origin/master", Some(("origin", "master"))),
                &[local_info("master", Some("origin/master"), 3)],
            ),
            CheckoutDivergence::Diverged {
                local_branch: "master".into(),
                ahead: 3,
            }
        );
    }

    /// A local branch that is level with (or merely behind) its
    /// upstream fast-forwards: checkout stays silent.
    #[test]
    fn test_fast_forwardable_local_branch_checks_out_silently() {
        assert_eq!(
            checkout_divergence(
                &remote_ref("origin/master", Some(("origin", "master"))),
                &[local_info("master", Some("origin/master"), 0)],
            ),
            CheckoutDivergence::NoKnownDivergence
        );
    }

    /// Nothing local to strand — `change_branch` will create the branch.
    #[test]
    fn test_checkout_without_a_local_branch_is_silent() {
        assert_eq!(
            checkout_divergence(
                &remote_ref("origin/master", Some(("origin", "master"))),
                &[local_info("other", Some("origin/other"), 5)],
            ),
            CheckoutDivergence::NoLocalBranch
        );
    }

    /// A same-named local branch that tracks a *different* upstream has
    /// no ahead-count against this ref. Checking out keeps its commits
    /// either way, so the silent path is the safe one.
    #[test]
    fn test_local_branch_tracking_another_remote_does_not_prompt() {
        assert_eq!(
            checkout_divergence(
                &remote_ref("origin/master", Some(("origin", "master"))),
                &[local_info("master", Some("upstream/master"), 4)],
            ),
            CheckoutDivergence::NoKnownDivergence
        );
    }
}
