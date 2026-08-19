//! S-CTM Commit row context menu — IDEA-style right-click on a commit
//! in the Git Graph view (also used by the S-ANN blame gutter
//! right-click). Non-destructive operations only; destructive ops
//! (cherry-pick / revert / reset / drop / squash / merge / rebase) land
//! via the S-DST work and are stubbed here with disabled placeholder
//! entries so the menu shape stays stable across releases.
//!
//! The builder takes the surrounding context (workspace, repository,
//! commit SHA, the loaded subject when available, and a flag for whether
//! the active remote is hosted) and constructs the full menu eagerly.
//!
//! The "Show Affected Paths in Log" entry dispatches the
//! `git_graph::ShowAffectedPathsInLog` action via the action registry —
//! this lets `git_ui` (which sits below `git_graph` in the dependency
//! DAG) emit the action without taking a build-time dep on the graph
//! crate. When the action is not registered (e.g. `git_graph` was
//! disabled at build time), the dispatch is a no-op.

use std::path::PathBuf;

use crate::askpass_modal::AskPassModal;
use crate::handlers::branch::{
    FORCE_DELETE_BRANCH_ANSWER, ForceDeleteDecision, force_delete_decision,
};
use crate::handlers::{
    branch, checkout, cherry_pick, compare, copy, drop as drop_handler, edit_message, fixup, merge,
    patch as patch_handler, protection, rebase, reset, revert, show_at_revision, squash, tag,
};
use anyhow::Context as _;
use askpass::AskPassDelegate;
use editor::Editor;
use git::operations::RunOutcome;
use git::repository::PushOptions;
use gpui::{
    App, AsyncWindowContext, ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, PromptLevel, Render, SharedString, WeakEntity, Window,
};
use menu::{Cancel, Confirm};
use project::git_store::Repository;
use serde_json::json;
use ui::ContextMenu;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    ModalView, Toast, Workspace,
    notifications::{DetachAndPromptErr, NotificationId},
};
use zed_actions::NewWorktreeBranchTarget;

#[derive(Clone)]
pub struct CommitContext {
    pub workspace: WeakEntity<Workspace>,
    pub repository: Entity<Repository>,
    pub sha: SharedString,
    /// Subject line of the commit message. May be empty when the commit
    /// data hasn't loaded yet — in that case "Copy Subject" / "Copy
    /// Subject + Hash" copy the SHA alone, which is the least surprising
    /// fallback for an unknown subject.
    pub subject: SharedString,
    /// `Some((provider_name, _base_url))` when the active remote is
    /// hosted on a recognized provider (GitHub / GitLab / Bitbucket /
    /// Gitea / etc). Drives the External submenu visibility.
    pub provider: Option<(String, String)>,
    /// Working directory of the active repository, for "Show in File
    /// Manager" entries.
    pub work_dir: Option<PathBuf>,
    /// `Some(<catalog id>)` when this row was sourced from a Solution-wide
    /// aggregated log (S-SOL-LOG). Drives the "Cherry-pick to Other
    /// Member…" entry (S-SOL-CHP). `None` for plain single-repo log rows
    /// — no cross-member entry is shown.
    pub member_id: Option<SharedString>,
    /// Raw `%D` ref-decoration tokens for this commit — e.g.
    /// `HEAD -> main`, `tag: v1.0`, `origin/main`, `feat-x`. Drives the
    /// "Branches / Tags at This Commit" submenus. Empty when the source
    /// (e.g. the blame gutter) doesn't carry decoration info.
    pub refs: Vec<SharedString>,
    /// Name of the currently checked-out branch (`None` on detached
    /// HEAD). Labels "Merge into <head>" and suppresses checkout / merge /
    /// delete on the head branch itself.
    pub head_branch: Option<SharedString>,
    /// Local branches known to the repository. Used to tell a local
    /// branch token in [`Self::refs`] apart from a remote-tracking ref
    /// like `origin/feature` (both are slash-bearing in `%D`), to build
    /// the "Tracked Branch" submenu, and to decide whether checking out
    /// a remote ref would strand local commits.
    pub local_branches: Vec<LocalBranchInfo>,
    /// Remote-tracking branch names known to the repository, in the same
    /// `<remote>/<branch>` spelling `%D` uses (`refs/remotes/` stripped).
    /// A token that appears here is a remote-tracking ref even when the
    /// configured remote names aren't known yet.
    pub remote_branches: Vec<SharedString>,
    /// Configured remote names (`origin`, `upstream`, …). A remote name
    /// may itself contain a `/`, so splitting a `%D` token on the first
    /// one names the wrong remote; the split is resolved against this
    /// list instead. Empty when the source can't supply it — the
    /// remote-tracking refs are still listed, only the server-side
    /// actions that need a remote name are withheld.
    pub remotes: Vec<SharedString>,
}

/// A local branch as the repository currently knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalBranchInfo {
    pub name: SharedString,
    /// Remote-tracking ref this branch follows, in `%D` spelling
    /// (`origin/main`). `None` when the branch tracks nothing.
    pub upstream: Option<SharedString>,
    /// Commits on this branch that [`Self::upstream`] does not have.
    /// `0` when the branch tracks nothing or the upstream ref is gone —
    /// i.e. "no known divergence", never "definitely none".
    pub ahead: u32,
}

pub fn build_commit_context_menu(
    ctx: CommitContext,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |menu, _window, _cx| {
        let copy_ctx = ctx.clone();
        let new_branch_ctx = ctx.clone();
        let new_tag_ctx = ctx.clone();
        let checkout_ctx = ctx.clone();
        let compare_ctx = ctx.clone();
        let show_ctx = ctx.clone();
        let destructive_ctx = ctx.clone();
        let irb_ctx = ctx.clone();
        let patch_ctx = ctx.clone();
        let refs_ctx = ctx.clone();
        let external_ctx = ctx;
        let has_provider = external_ctx.provider.is_some();

        let menu = menu
            .submenu("Copy", move |menu, _window, _cx| {
                build_copy_submenu(menu, copy_ctx.clone())
            })
            .separator()
            .entry("New Branch from Here…", None, {
                let ctx = new_branch_ctx;
                move |window, cx| open_new_branch_modal(ctx.clone(), window, cx)
            })
            .entry("New Tag at Here…", None, {
                let ctx = new_tag_ctx;
                move |window, cx| open_new_tag_modal(ctx.clone(), window, cx)
            })
            .entry("Checkout Revision", None, {
                let ctx = checkout_ctx;
                move |window, cx| open_checkout_confirmation(ctx.clone(), window, cx)
            });

        // S-CTM refs section — branches / tags pointing at this commit,
        // each with checkout / merge / delete actions. Hidden when the
        // commit carries no ref decorations (or the source didn't supply
        // them, e.g. the blame gutter).
        let menu = build_branch_tag_section(menu, refs_ctx);

        let menu = menu
            .separator()
            .submenu("Compare", move |menu, _window, _cx| {
                build_compare_submenu(menu, compare_ctx.clone())
            })
            .submenu("Show", move |menu, _window, _cx| {
                build_show_submenu(menu, show_ctx.clone())
            });

        let menu = if has_provider {
            menu.submenu("Open on Host", move |menu, _window, _cx| {
                build_external_submenu(menu, external_ctx.clone())
            })
        } else {
            menu
        };

        // S-DST destructive section.
        let menu = menu.separator().entry("Cherry-pick", None, {
            let ctx = destructive_ctx.clone();
            move |window, cx| run_cherry_pick(ctx.clone(), window, cx)
        });

        // S-SOL-CHP — show "Cherry-pick to Other Member…" only for rows
        // that came from the Solution-wide aggregated log (i.e.
        // `member_id` is set). Builds the action dynamically by name so
        // this module doesn't pull in a build-time dep on `solution_git`
        // (mirrors the `Show Affected Paths in Log` pattern). When the
        // action isn't registered the dispatch is silently skipped.
        let menu = if let Some(member_id) = destructive_ctx.member_id.clone() {
            let sha = destructive_ctx.sha.clone();
            menu.entry("Cherry-pick to Other Member…", None, move |window, cx| {
                if let Ok(action) = cx.build_action(
                    "solution_git::CrossCherryPick",
                    Some(json!({
                        "source_member": member_id.to_string(),
                        "source_sha": sha.to_string(),
                    })),
                ) {
                    window.dispatch_action(action, cx);
                }
            })
        } else {
            menu
        };

        let menu = menu
            .entry("Revert", None, {
                let ctx = destructive_ctx.clone();
                move |window, cx| run_revert(ctx.clone(), window, cx)
            })
            .submenu("Reset Current Branch to Here", {
                let ctx = destructive_ctx.clone();
                move |menu, _window, _cx| build_reset_submenu(menu, ctx.clone())
            })
            .entry("Edit Commit Message…", None, {
                let ctx = destructive_ctx.clone();
                move |window, cx| open_edit_message_prompt(ctx.clone(), window, cx)
            })
            .entry("Drop Commit", None, {
                let ctx = destructive_ctx.clone();
                move |window, cx| run_drop_commit(ctx.clone(), window, cx)
            })
            .entry("Squash with Previous", None, {
                let ctx = destructive_ctx.clone();
                move |window, cx| run_squash_with_previous(ctx.clone(), window, cx)
            })
            .entry("Fixup with Previous", None, {
                let ctx = destructive_ctx.clone();
                move |window, cx| run_fixup_with_previous(ctx.clone(), window, cx)
            })
            .entry("Reword Commit", None, {
                let ctx = destructive_ctx;
                move |window, cx| open_edit_message_prompt(ctx.clone(), window, cx)
            })
            .entry("Move Commit", None, |_, _| {
                // Picker UX deferred — see `git_ui::handlers::move_commit`
                // for the underlying op. Wired up alongside S-IRB.
            })
            .entry("Interactive Rebase from Here", None, {
                let ctx = irb_ctx;
                move |window, cx| open_interactive_rebase(ctx.clone(), window, cx)
            });

        let menu = menu.separator().submenu("Patch", {
            let ctx = patch_ctx;
            move |menu, _window, _cx| build_patch_submenu(menu, ctx.clone())
        });

        menu.separator().entry("Show in Terminal", None, |_, _| {})
    })
}

fn build_copy_submenu(menu: ContextMenu, ctx: CommitContext) -> ContextMenu {
    let CommitContext {
        sha,
        subject,
        repository,
        provider,
        ..
    } = ctx;

    let sha_for_hash = sha.clone();
    let sha_for_short = sha.clone();
    let subject_for_subject = subject.clone();
    let sha_for_combo = sha.clone();
    let subject_for_combo = subject;
    let sha_for_patch_id = sha.clone();
    let repository_for_patch_id = repository.clone();
    let menu = menu
        .entry("Copy Hash", None, move |_, cx| {
            copy::copy_hash(&sha_for_hash, cx);
        })
        .entry("Copy Short Hash", None, move |_, cx| {
            copy::copy_short_hash(&sha_for_short, cx);
        })
        .entry("Copy Subject", None, move |_, cx| {
            copy::copy_subject(&subject_for_subject, cx);
        })
        .entry("Copy Subject + Hash", None, move |_, cx| {
            copy::copy_subject_and_hash(&sha_for_combo, &subject_for_combo, cx);
        })
        .entry("Copy Patch ID", None, move |_, cx| {
            copy::copy_patch_id(
                repository_for_patch_id.clone(),
                sha_for_patch_id.to_string(),
                cx,
            )
            .detach_and_log_err(cx);
        });
    if provider.is_some() {
        menu.entry("Copy Permalink", None, move |_, cx| {
            let sha = sha.clone();
            repository.update(cx, |repo, cx| {
                copy::copy_permalink(repo, &sha, cx).log_err();
            });
        })
    } else {
        menu
    }
}

fn build_compare_submenu(menu: ContextMenu, ctx: CommitContext) -> ContextMenu {
    let CommitContext { sha, workspace, .. } = ctx;

    let menu = menu.entry(
        "Compare with Local Working Tree",
        None,
        move |window, cx| {
            let sha = sha.clone();
            workspace
                .update(cx, |workspace, cx| {
                    compare::compare_with_local_working_tree(workspace, &sha, window, cx);
                })
                .ok();
        },
    );
    // "Compare with HEAD / Branch / Commit" need a true commit-vs-commit
    // diff that the existing `branch_diff::DiffBase` enum can't express
    // (it always diffs the working tree against a base ref). Stubbed
    // until that infrastructure lands.
    menu.entry("Compare with HEAD", None, |_, _| {})
        .entry("Compare with Branch…", None, |_, _| {})
        .entry("Compare with Commit…", None, |_, _| {})
}

fn build_show_submenu(menu: ContextMenu, ctx: CommitContext) -> ContextMenu {
    // S-SAR — capture before destructuring; the bare-repo pre-check
    // wants `work_dir` and the dispatch wants `sha`, both of which are
    // moved out below.
    let work_dir_for_sar = ctx.work_dir.clone();
    let sha_for_sar = ctx.sha.to_string();

    let CommitContext {
        sha,
        repository,
        work_dir,
        ..
    } = ctx;

    let menu = menu.entry("Show Affected Paths in Log", None, move |window, cx| {
        // Cross-link to S-FLT: collect the paths the commit touches via
        // `Repository::load_commit_diff` and emit
        // `git_graph::ShowAffectedPathsInLog { paths }`. The handler in
        // `GitGraph::on_action` calls `set_path_filter`.
        let repository = repository.clone();
        let sha_string = sha.to_string();
        window
            .spawn(cx, async move |cx| {
                let diff = match repository
                    .update(cx, |repo, _| repo.load_commit_diff(sha_string))
                    .await
                {
                    Ok(Ok(diff)) => diff,
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        return Err(anyhow::anyhow!("load_commit_diff was canceled"));
                    }
                };
                let paths: Vec<String> = diff
                    .files
                    .iter()
                    .map(|f| f.path.as_unix_str().to_string())
                    .collect();
                cx.update(|window, cx| {
                    // Build the action dynamically by name so this module
                    // doesn't take a static dep on `git_graph` (which itself
                    // depends on `git_ui`). When the action isn't
                    // registered the dispatch is silently skipped.
                    if let Ok(action) = cx.build_action(
                        "git_graph::ShowAffectedPathsInLog",
                        Some(json!({ "paths": paths })),
                    ) {
                        window.dispatch_action(action, cx);
                    }
                })?;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
    });
    // S-SAR — open the repo state at this commit in a read-only
    // snapshot window. Disabled (with a clarifying label) when the
    // source is a bare clone — `git worktree add` semantics on bare
    // repos differ enough that v1 refuses up front rather than
    // letting the user discover the failure mid-operation.
    let is_bare_source = work_dir_for_sar
        .as_ref()
        .map(|p| !show_at_revision::source_repo_is_normal(p))
        .unwrap_or(true);
    let menu = if is_bare_source {
        menu.item(
            ui::ContextMenuEntry::new("Show Repository at Revision (bare repo)").disabled(true),
        )
    } else {
        menu.entry("Show Repository at Revision", None, move |window, cx| {
            window.dispatch_action(
                Box::new(crate::fork_actions::ShowAtRevision {
                    sha: sha_for_sar.clone(),
                }),
                cx,
            );
        })
    };
    if let Some(work_dir) = work_dir {
        menu.entry("Show in File Manager", None, move |_, cx| {
            cx.reveal_path(&work_dir);
        })
    } else {
        menu
    }
}

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

struct RefsAtCommit {
    branches: Vec<BranchRef>,
    tags: Vec<SharedString>,
}

/// A branch decoration on a commit, classified against the refs the
/// repository actually knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BranchRef {
    Local(SharedString),
    Remote(RemoteBranchRef),
}

impl BranchRef {
    /// The ref as git spells it — both the menu label and the argument
    /// every operation in the submenu takes.
    fn name(&self) -> &SharedString {
        match self {
            Self::Local(name) => name,
            Self::Remote(remote) => &remote.full,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteBranchRef {
    /// `%D` spelling — `origin/main`, i.e. `refs/remotes/` stripped.
    full: SharedString,
    /// `Some((remote, branch))` only when [`Self::full`] starts with one
    /// of the repository's configured remote names. Server-side actions
    /// are withheld when this is `None`: we can't say which remote a
    /// delete or a pull would go to, and guessing means hitting the
    /// wrong one.
    split: Option<(SharedString, SharedString)>,
}

fn refs_at_commit(ctx: &CommitContext) -> RefsAtCommit {
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
fn classify_refs(
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
fn classify_branch_token(
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
    }))
}

/// Split a remote-tracking ref into `(remote, branch)` against the
/// repository's configured remotes. The **longest** matching remote name
/// wins: `git remote add team/fork …` is legal, so splitting on the
/// first `/` can name a remote that doesn't exist.
fn split_remote_ref(name: &str, remotes: &[SharedString]) -> Option<(SharedString, SharedString)> {
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

/// What checking out a remote-tracking ref would do to the local branch
/// of the same name — the question IDEA answers with its "Checkout
/// \<remote ref\>" dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CheckoutDivergence {
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

fn checkout_divergence(
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

fn build_branch_tag_section(menu: ContextMenu, ctx: CommitContext) -> ContextMenu {
    let RefsAtCommit { branches, tags } = refs_at_commit(&ctx);
    if branches.is_empty() && tags.is_empty() {
        return menu;
    }
    let mut menu = menu.separator();
    if !branches.is_empty() {
        menu = menu.header("Branches at This Commit");
        // Locals first — they are the refs you can act on without
        // talking to a server.
        let (locals, remotes): (Vec<_>, Vec<_>) = branches
            .into_iter()
            .partition(|branch| matches!(branch, BranchRef::Local(_)));
        for branch in locals.into_iter().chain(remotes) {
            let entry_ctx = ctx.clone();
            let icon = match branch {
                BranchRef::Local(_) => IconName::GitBranch,
                BranchRef::Remote(_) => IconName::Screen,
            };
            menu =
                menu.submenu_with_icon(branch.name().clone(), icon, move |submenu, _window, cx| {
                    build_branch_ref_submenu(submenu, entry_ctx.clone(), branch.clone(), cx)
                });
        }
    }
    if !tags.is_empty() {
        menu = menu.header("Tags at This Commit");
        for tag in tags {
            let entry_ctx = ctx.clone();
            menu = menu.submenu_with_icon(
                tag.clone(),
                IconName::Hash,
                move |submenu, _window, _cx| {
                    build_tag_ref_submenu(submenu, entry_ctx.clone(), tag.clone())
                },
            );
        }
    }
    menu
}

/// A row IDEA offers that this fork cannot back with a real operation:
/// rendered disabled with the reason on the info aside instead of being
/// dropped, so the menu keeps its familiar shape and says why.
fn unavailable_entry(
    label: impl Into<SharedString>,
    reason: impl Into<SharedString>,
) -> ui::ContextMenuEntry {
    let reason = reason.into();
    ui::ContextMenuEntry::new(label)
        .disabled(true)
        .documentation_aside(ui::DocumentationSide::Left, move |_cx| {
            Label::new(reason.clone()).into_any_element()
        })
}

/// What a row of the per-ref submenu does when clicked. Payloads are
/// resolved while planning (`plan_branch_submenu`) rather than at click
/// time so the plan is a complete description of the menu — the renderer
/// only attaches handlers.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BranchAction {
    Checkout,
    NewBranchFrom,
    CheckoutAndRebase {
        head: SharedString,
    },
    CheckoutAndUpdate {
        remote: SharedString,
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
enum SubmenuRow {
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
    fn enabled(action: BranchAction, label: impl Into<SharedString>) -> Self {
        Self::Row {
            action,
            label: label.into(),
            unavailable: None,
        }
    }

    fn unavailable(
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
///   confirm. Unavailable when neither an upstream nor a single
///   unambiguous remote resolves the destination.
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
fn plan_branch_submenu(
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
    let upstream = match branch {
        BranchRef::Local(local_name) => local_branches
            .iter()
            .find(|info| info.name == *local_name)
            .and_then(|info| info.upstream.clone()),
        BranchRef::Remote(_) => None,
    };
    let upstream_ref = upstream.map(|upstream| RemoteBranchRef {
        split: split_remote_ref(&upstream, remotes),
        full: upstream,
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
            Some(upstream) => match upstream.split.clone() {
                Some((remote, _)) => SubmenuRow::enabled(
                    BranchAction::CheckoutAndUpdate { remote },
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
    rows
}

/// "Push…" for a branch that isn't the current one. `PushDialog` only
/// knows how to push the checked-out branch, but `Repository::push`
/// takes any branch — as long as we can name the destination.
fn plan_push_row(
    branch: &SharedString,
    upstream: Option<&RemoteBranchRef>,
    remotes: &[SharedString],
) -> SubmenuRow {
    let destination = match upstream.and_then(|upstream| upstream.split.clone()) {
        Some((remote, remote_branch)) => Some((remote, remote_branch, false)),
        // No upstream yet: only auto-resolve when the repository leaves
        // no room for doubt, i.e. it has exactly one remote. That is a
        // `--set-upstream` push, like IDEA's first push of a branch.
        None => match remotes {
            [only_remote] => Some((only_remote.clone(), branch.clone(), true)),
            _ => None,
        },
    };
    match destination {
        Some((remote, remote_branch, set_upstream)) => SubmenuRow::enabled(
            BranchAction::Push {
                remote,
                remote_branch,
                set_upstream,
            },
            "Push…",
        ),
        None => SubmenuRow::unavailable(
            BranchAction::Push {
                remote: SharedString::default(),
                remote_branch: branch.clone(),
                set_upstream: false,
            },
            "Push…",
            format!(
                "“{branch}” tracks no upstream and this repository has {} remotes, so the \
                 destination is ambiguous. Check the branch out and use the Push dialog, which \
                 resolves it interactively.",
                remotes.len()
            ),
        ),
    }
}

fn plan_pull_rows(branch: &RemoteBranchRef, head: &SharedString) -> Vec<SubmenuRow> {
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

fn plan_delete_row(branch: &BranchRef, delete_forbidden: Option<SharedString>) -> SubmenuRow {
    let action = match branch {
        BranchRef::Local(_) => BranchAction::Delete,
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
fn protected_branch_name(branch: &BranchRef) -> Option<SharedString> {
    match branch {
        BranchRef::Local(name) => Some(name.clone()),
        BranchRef::Remote(remote) => remote.split.as_ref().map(|(_, branch)| branch.clone()),
    }
}

fn build_branch_ref_submenu(
    menu: ContextMenu,
    ctx: CommitContext,
    branch: BranchRef,
    cx: &App,
) -> ContextMenu {
    let work_dir = repo_work_dir(&ctx, cx);
    let delete_forbidden = work_dir
        .as_deref()
        .zip(protected_branch_name(&branch))
        .and_then(|(work_dir, name)| {
            match protection::enforce(work_dir, &name, "delete_branch", true) {
                Err(protection::BranchProtectionError::Forbidden { reason }) => {
                    Some(SharedString::from(reason))
                }
                _ => None,
            }
        });
    let rows = plan_branch_submenu(
        &branch,
        ctx.head_branch.as_ref(),
        &ctx.local_branches,
        &ctx.remotes,
        delete_forbidden,
    );

    let mut menu = menu;
    for row in rows {
        let (action, label, unavailable) = match row {
            SubmenuRow::Separator => {
                menu = menu.separator();
                continue;
            }
            SubmenuRow::Row {
                action,
                label,
                unavailable,
            } => (action, label, unavailable),
        };
        if let Some(reason) = unavailable {
            menu = menu.item(unavailable_entry(label, reason));
            continue;
        }
        menu = render_branch_action(menu, &ctx, &branch, action, label);
    }
    menu
}

fn render_branch_action(
    menu: ContextMenu,
    ctx: &CommitContext,
    branch: &BranchRef,
    action: BranchAction,
    label: SharedString,
) -> ContextMenu {
    let ctx = ctx.clone();
    let name = branch.name().clone();
    match action {
        BranchAction::Checkout => {
            let remote_ref = match branch {
                BranchRef::Remote(remote) => Some(remote.clone()),
                BranchRef::Local(_) => None,
            };
            menu.entry(label, None, move |window, cx| match &remote_ref {
                Some(remote_ref) => {
                    run_checkout_remote_branch(ctx.clone(), remote_ref.clone(), window, cx)
                }
                None => run_checkout_branch(ctx.repository.clone(), name.clone(), window, cx),
            })
        }
        BranchAction::NewBranchFrom => menu.entry(label, None, move |window, cx| {
            open_new_branch_from_ref_modal(ctx.clone(), name.clone(), window, cx)
        }),
        BranchAction::CheckoutAndRebase { head } => menu.entry(label, None, move |window, cx| {
            run_checkout_and_rebase(ctx.clone(), name.clone(), head.clone(), window, cx)
        }),
        BranchAction::CheckoutAndUpdate { remote } => menu.entry(label, None, move |window, cx| {
            run_checkout_and_update(ctx.clone(), name.clone(), remote.clone(), window, cx)
        }),
        BranchAction::ShowDiffWithWorkingTree => menu.entry(label, None, move |window, cx| {
            run_show_diff_with_working_tree(ctx.clone(), window, cx)
        }),
        BranchAction::RebaseHeadOnto => menu.entry(label, None, move |window, cx| {
            let Some(work_dir) = repo_work_dir(&ctx, cx) else {
                return;
            };
            run_rebase_onto(work_dir, name.clone(), window, cx);
        }),
        BranchAction::MergeIntoHead => menu.entry(label, None, move |window, cx| {
            let Some(work_dir) = repo_work_dir(&ctx, cx) else {
                return;
            };
            run_merge_branch(work_dir, name.clone(), window, cx);
        }),
        BranchAction::NewWorktree { target } => menu.entry(label, None, move |window, cx| {
            open_new_worktree_modal(ctx.clone(), target.clone(), window, cx)
        }),
        BranchAction::Push {
            remote,
            remote_branch,
            set_upstream,
        } => menu.entry(label, None, move |window, cx| {
            run_push_branch(
                ctx.clone(),
                name.clone(),
                remote.clone(),
                remote_branch.clone(),
                set_upstream.then_some(PushOptions::SetUpstream),
                window,
                cx,
            )
        }),
        BranchAction::TrackedBranch { upstream } => {
            menu.submenu(label, move |submenu, _window, cx| {
                build_branch_ref_submenu(
                    submenu,
                    ctx.clone(),
                    BranchRef::Remote(upstream.clone()),
                    cx,
                )
            })
        }
        BranchAction::Pull {
            remote,
            remote_branch,
            rebase,
        } => menu.entry(label, None, move |window, cx| {
            run_pull_into_head(
                ctx.clone(),
                remote.clone(),
                remote_branch.clone(),
                rebase,
                window,
                cx,
            )
        }),
        BranchAction::Rename => menu.entry(label, None, move |window, cx| {
            open_rename_branch_modal(ctx.clone(), name.clone(), window, cx)
        }),
        BranchAction::Delete => menu.entry(label, None, move |window, cx| {
            run_delete_branch(ctx.clone(), name.clone(), false, window, cx);
        }),
        BranchAction::DeleteOnRemote { remote, branch } => menu.item(
            ui::ContextMenuEntry::new(label)
                .icon(IconName::Trash)
                .icon_color(Color::Error)
                .handler(move |window, cx| {
                    run_delete_remote_branch(
                        ctx.clone(),
                        remote.clone(),
                        branch.clone(),
                        window,
                        cx,
                    );
                }),
        ),
        // Planned unavailable in every case they are emitted, so the
        // renderer never reaches them with a handler to attach.
        BranchAction::CompareWithHead | BranchAction::Update => {
            menu.item(ui::ContextMenuEntry::new(label).disabled(true))
        }
    }
}

fn build_tag_ref_submenu(menu: ContextMenu, ctx: CommitContext, tag: SharedString) -> ContextMenu {
    let menu = {
        let repo = ctx.repository.clone();
        let tag = tag.clone();
        menu.entry("Checkout", None, move |window, cx| {
            run_checkout_tag(repo.clone(), tag.clone(), window, cx);
        })
    };
    menu.entry("Delete", None, move |window, cx| {
        run_delete_tag(ctx.clone(), tag.clone(), window, cx);
    })
}

fn await_repo_recv(
    recv: futures::channel::oneshot::Receiver<anyhow::Result<()>>,
    canceled_msg: &'static str,
    label: &'static str,
    window: &mut Window,
    cx: &mut App,
) {
    let task = cx.spawn(async move |_cx| match recv.await {
        Ok(Ok(())) => anyhow::Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(anyhow::anyhow!("{canceled_msg}")),
    });
    task.detach_and_prompt_err(label, window, cx, |e, _, _| Some(format!("{e}")));
}

fn run_checkout_branch(
    repo: Entity<Repository>,
    branch: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let recv = repo.update(cx, |repo, _| repo.change_branch(branch.to_string()));
    await_repo_recv(recv, "checkout was canceled", "Checkout failed", window, cx);
}

/// Await a `change_branch` job, flattening its two failure channels.
async fn checkout_branch(
    repository: &Entity<Repository>,
    branch: &SharedString,
    cx: &mut AsyncWindowContext,
) -> anyhow::Result<()> {
    match repository
        .update(cx, |repo, _| repo.change_branch(branch.to_string()))
        .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(anyhow::anyhow!("checkout was canceled")),
    }
}

/// Turn a paused rebase / reset into an error the UI actually shows.
/// Reporting "done" while the worktree sits mid-rebase is worse than a
/// loud failure.
fn describe_outcome(outcome: RunOutcome) -> anyhow::Result<()> {
    match outcome {
        RunOutcome::Completed => Ok(()),
        RunOutcome::PausedForConflict { conflicted_files } => Err(anyhow::anyhow!(
            "paused on {} conflicted file(s) — resolve them, then continue from the Changes panel",
            conflicted_files.len()
        )),
        RunOutcome::PausedForExecFailure { command, stderr } => {
            Err(anyhow::anyhow!("`{command}` failed: {stderr}"))
        }
    }
}

/// "Checkout" on a remote-tracking ref. `change_branch` creates (or
/// re-points) the matching local branch with `--track` and checks that
/// out, so when the local branch already carries commits the remote
/// doesn't have, doing it silently would strand them on a branch the
/// user believes they just synced. IDEA asks first; so do we.
fn run_checkout_remote_branch(
    ctx: CommitContext,
    branch: RemoteBranchRef,
    window: &mut Window,
    cx: &mut App,
) {
    let CheckoutDivergence::Diverged { local_branch, .. } =
        checkout_divergence(&branch, &ctx.local_branches)
    else {
        run_checkout_branch(ctx.repository, branch.full, window, cx);
        return;
    };

    let full = branch.full;
    let work_dir = repo_work_dir(&ctx, cx);
    let answer = window.prompt(
        PromptLevel::Info,
        &format!("Checkout {full}"),
        Some(&format!(
            "Local branch '{local_branch}' has commits that do not exist in '{full}'. \
             Rebase '{local_branch}' onto '{full}', or drop local commits?"
        )),
        // Order is load-bearing: the first answer is the default one,
        // and throwing commits away must never be what Enter does.
        &["Rebase onto Remote", "Drop Local Commits", "Cancel"],
        cx,
    );

    let repository = ctx.repository;
    let task = window.spawn(cx, async move |cx| {
        let drop_local = match answer.await.ok() {
            Some(0) => false,
            Some(1) => true,
            _ => return anyhow::Ok(()),
        };
        let work_dir = work_dir.context("repository has no working directory")?;
        if drop_local {
            // Same gate as the server-side delete: dropping commits is a
            // hard reset of the local branch, which the policy forbids
            // outright on a protected one.
            protection::enforce(&work_dir, &local_branch, "reset_hard", true)
                .map_err(|error| anyhow::anyhow!("branch protection: {error}"))?;
        }
        checkout_branch(&repository, &full, cx).await?;
        let outcome = cx.update(|_window, cx| {
            if drop_local {
                reset::run_with_confirmation(
                    work_dir.clone(),
                    full.to_string(),
                    git::operations::reset::ResetMode::Hard,
                    true,
                    cx,
                )
            } else {
                rebase::run(work_dir.clone(), full.to_string(), false, cx)
            }
        })?;
        describe_outcome(outcome.await?)
    });
    task.detach_and_prompt_err("Checkout failed", window, cx, |e, _, _| {
        Some(format!("{e}"))
    });
}

/// "New Branch from '\<ref\>'…" — `git switch -c <name> <ref>`, which
/// creates *and* checks out, matching the default state of IDEA's
/// create-branch dialog.
fn open_new_branch_from_ref_modal(
    ctx: CommitContext,
    base_ref: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = ctx.workspace.upgrade() else {
        return;
    };
    let repository = ctx.repository;
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            NameInputModal::new(
                format!("Create Branch from '{base_ref}'"),
                "Branch name",
                IconName::GitBranch,
                window,
                cx,
                move |name, window, cx| {
                    let recv = repository.update(cx, |repo, _| {
                        repo.create_branch(name, Some(base_ref.to_string()))
                    });
                    await_repo_recv(
                        recv,
                        "create branch was canceled",
                        "Failed to create branch",
                        window,
                        cx,
                    );
                },
            )
        });
    });
}

/// "Checkout and Rebase onto '\<head\>'" — check the ref out, then
/// rebase it onto the branch we were on. Both halves are real
/// operations; `rebase` always rewrites whatever is current, which after
/// the checkout is the ref itself.
fn run_checkout_and_rebase(
    ctx: CommitContext,
    branch: SharedString,
    head: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(work_dir) = repo_work_dir(&ctx, cx) else {
        return;
    };
    let repository = ctx.repository;
    let task = window.spawn(cx, async move |cx| {
        checkout_branch(&repository, &branch, cx).await?;
        let outcome =
            cx.update(|_window, cx| rebase::run(work_dir.clone(), head.to_string(), false, cx))?;
        describe_outcome(outcome.await?)
    });
    task.detach_and_prompt_err("Checkout and rebase failed", window, cx, |e, _, _| {
        Some(format!("{e}"))
    });
}

/// "Checkout and Update" — check the branch out, then pull its own
/// upstream into it.
fn run_checkout_and_update(
    ctx: CommitContext,
    branch: SharedString,
    remote: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let workspace = ctx.workspace;
    let repository = ctx.repository;
    let task = window.spawn(cx, async move |cx| {
        checkout_branch(&repository, &branch, cx).await?;
        let askpass = cx.update(|window, cx| {
            askpass_delegate(workspace.clone(), format!("git pull {remote}"), window, cx)
        })?;
        let pull = repository.update(cx, |repo, cx| {
            repo.pull(Some(branch.clone()), remote.clone(), false, askpass, cx)
        });
        match pull.await {
            Ok(Ok(_output)) => anyhow::Ok(()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow::anyhow!("pull was canceled")),
        }
    });
    task.detach_and_prompt_err("Checkout and update failed", window, cx, |e, _, _| {
        Some(format!("{e}"))
    });
}

/// "Rebase '\<head\>' onto '\<ref\>'" — plain `git rebase <ref>` on the
/// current branch.
fn run_rebase_onto(work_dir: PathBuf, target: SharedString, window: &mut Window, cx: &mut App) {
    let rebase = rebase::run(work_dir, target.to_string(), false, cx);
    let task = cx.spawn(async move |_cx| describe_outcome(rebase.await?));
    task.detach_and_prompt_err("Rebase failed", window, cx, |e, _, _| Some(format!("{e}")));
}

/// "Pull into '\<head\>' Using Rebase / Using Merge" — `git pull
/// [--rebase] <remote> <branch>` into the current branch.
fn run_pull_into_head(
    ctx: CommitContext,
    remote: SharedString,
    remote_branch: SharedString,
    rebase: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let workspace = ctx.workspace;
    let repository = ctx.repository;
    let task = window.spawn(cx, async move |cx| {
        let operation = if rebase {
            format!("git pull --rebase {remote} {remote_branch}")
        } else {
            format!("git pull {remote} {remote_branch}")
        };
        let askpass =
            cx.update(|window, cx| askpass_delegate(workspace.clone(), operation, window, cx))?;
        let pull = repository.update(cx, |repo, cx| {
            repo.pull(
                Some(remote_branch.clone()),
                remote.clone(),
                rebase,
                askpass,
                cx,
            )
        });
        match pull.await {
            Ok(Ok(_output)) => anyhow::Ok(()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow::anyhow!("pull was canceled")),
        }
    });
    task.detach_and_prompt_err("Pull failed", window, cx, |e, _, _| Some(format!("{e}")));
}

/// "New Worktree…" — asks for the worktree name, then hands the ref to
/// the existing `CreateWorktree` action as the new worktree's branch
/// target.
fn open_new_worktree_modal(
    ctx: CommitContext,
    target: NewWorktreeBranchTarget,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = ctx.workspace.upgrade() else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            NameInputModal::new(
                "New Worktree",
                "Worktree name",
                IconName::GitWorktree,
                window,
                cx,
                move |name, window, cx| {
                    window.dispatch_action(
                        Box::new(zed_actions::CreateWorktree {
                            worktree_name: Some(name),
                            branch_target: target,
                        }),
                        cx,
                    );
                },
            )
        });
    });
}

/// Marker type for the "pushed \<branch\>" toast's [`NotificationId`].
struct BranchPushedToast;

/// "Push…" for a branch that isn't the current one. `PushDialog` only
/// knows how to push the checked-out branch, but `Repository::push`
/// takes any branch, so this confirms the exact refspec and runs it.
fn run_push_branch(
    ctx: CommitContext,
    branch: SharedString,
    remote: SharedString,
    remote_branch: SharedString,
    options: Option<PushOptions>,
    window: &mut Window,
    cx: &mut App,
) {
    let detail = match options {
        Some(PushOptions::SetUpstream) => format!(
            "Runs git push --set-upstream {remote} {branch}:{remote_branch} — “{branch}” has no \
             upstream yet, so this also makes “{remote}/{remote_branch}” its upstream."
        ),
        _ => format!("Runs git push {remote} {branch}:{remote_branch}."),
    };
    let answer = window.prompt(
        PromptLevel::Info,
        &format!("Push '{branch}' to '{remote}/{remote_branch}'?"),
        Some(&detail),
        &["Push", "Cancel"],
        cx,
    );

    let workspace = ctx.workspace;
    let repository = ctx.repository;
    let task = window.spawn(cx, async move |cx| {
        if answer.await.ok() != Some(0) {
            return anyhow::Ok(());
        }
        let askpass = cx.update(|window, cx| {
            askpass_delegate(workspace.clone(), format!("git push {remote}"), window, cx)
        })?;
        let push = repository.update(cx, |repo, cx| {
            repo.push(
                branch.clone(),
                remote_branch.clone(),
                remote.clone(),
                options,
                askpass,
                cx,
            )
        });
        match push.await {
            Ok(Ok(_output)) => {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.show_toast(
                            Toast::new(
                                NotificationId::unique::<BranchPushedToast>(),
                                format!("Pushed “{branch}” to “{remote}/{remote_branch}”."),
                            )
                            .autohide(),
                            cx,
                        );
                    })
                    .ok();
                anyhow::Ok(())
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow::anyhow!("push was canceled")),
        }
    });
    task.detach_and_prompt_err("Push failed", window, cx, |e, _, _| Some(format!("{e}")));
}

/// "Rename…" — reuses the git panel's rename modal, which pre-fills the
/// editor with the current name and runs `git branch -m`.
fn open_rename_branch_modal(
    ctx: CommitContext,
    branch: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = ctx.workspace.upgrade() else {
        return;
    };
    let repository = ctx.repository;
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            crate::RenameBranchModal::new(branch.to_string(), repository, window, cx)
        });
    });
}

/// "Show Diff with Working Tree" — the ref's tip *is* this commit (that
/// is why the ref decorates the row), so the existing commit-vs-working
/// -tree compare is exactly the right operation.
fn run_show_diff_with_working_tree(ctx: CommitContext, window: &mut Window, cx: &mut App) {
    let sha = ctx.sha.clone();
    ctx.workspace
        .update(cx, |workspace, cx| {
            compare::compare_with_local_working_tree(workspace, &sha, window, cx);
        })
        .ok();
}

/// "Delete" on a branch decoration. A plain `git branch -d` refuses an
/// unmerged branch; rather than dead-ending on git's stderr the warning
/// carries a [`FORCE_DELETE_BRANCH_ANSWER`] answer that re-runs the
/// delete with `force = true`.
///
/// With `is_remote` the delete becomes `git branch -dr <remote>/<branch>`
/// — it removes this clone's remote-tracking ref, never anything on the
/// server (that is [`run_delete_remote_branch`]).
///
/// This uses a two-answer prompt rather than a toast because the failure
/// already surfaces as a modal here (`detach_and_prompt_err`), so adding
/// the escape hatch to that same modal keeps one surface instead of two,
/// and matches how the branch picker (entry A) and every other
/// destructive git confirm in this crate is spelled.
fn run_delete_branch(
    ctx: CommitContext,
    branch: SharedString,
    is_remote: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let repo = ctx.repository;
    let work_dir = ctx.work_dir;
    let recv = repo.update(cx, |repo, _| {
        repo.delete_branch(is_remote, branch.to_string(), false)
    });
    let task = window.spawn(cx, async move |cx| {
        let error = match recv.await {
            Ok(Ok(())) => return anyhow::Ok(()),
            Ok(Err(error)) => error,
            Err(_) => anyhow::bail!("delete branch was canceled"),
        };

        match force_delete_decision(&error, work_dir.as_deref(), &branch) {
            ForceDeleteDecision::NotApplicable => Err(error),
            ForceDeleteDecision::Forbidden { message } => Err(anyhow::anyhow!(message)),
            ForceDeleteDecision::Offer { warning } => {
                let answer = cx.update(|window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        &warning,
                        None,
                        &[FORCE_DELETE_BRANCH_ANSWER, "Cancel"],
                        cx,
                    )
                })?;
                if answer.await != Ok(0) {
                    return Ok(());
                }
                repo.update(cx, |repo, _| {
                    repo.delete_branch(is_remote, branch.to_string(), true)
                })
                .await?
            }
        }
    });
    task.detach_and_prompt_err("Delete branch failed", window, cx, |e, _, _| {
        Some(format!("{e}"))
    });
}

/// Marker type for the "branch deleted on <remote>" toast's
/// [`NotificationId`].
struct RemoteBranchDeletedToast;

/// "Delete on \<remote\>…" — the server-side delete of a remote branch.
///
/// Spelled as a push of an empty source refspec (`git push <remote>
/// :<branch>`, the wire form of `--delete`) because [`Repository::push`]
/// is the only path that carries the askpass delegate, the collab
/// proxying and the post-push branch rescan; there is no
/// delete-branch-on-remote API to call instead, and a bare
/// `git push --delete` spawned here would block forever the first time a
/// credential helper wanted input.
///
/// Guarded twice: branch protection refuses the delete outright on a
/// protected branch (`delete_branch` is `Forbidden`, not merely
/// confirmable, there), and everything else goes through a
/// [`PromptLevel::Critical`] confirm naming both the branch and the
/// remote.
fn run_delete_remote_branch(
    ctx: CommitContext,
    remote: SharedString,
    branch: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(work_dir) = repo_work_dir(&ctx, cx)
        && let Err(error) = protection::enforce(&work_dir, &branch, "delete_branch", true)
    {
        let refusal = window.prompt(
            PromptLevel::Critical,
            &format!("Cannot delete “{branch}” on “{remote}”"),
            Some(&error.to_string()),
            &["OK"],
            cx,
        );
        cx.spawn(async move |_cx| {
            refusal.await.ok();
        })
        .detach();
        return;
    }

    let confirm_answer = format!("Delete on {remote}");
    let answer = window.prompt(
        PromptLevel::Critical,
        &format!("Delete branch “{branch}” on “{remote}”?"),
        Some(&format!(
            "Runs git push {remote} --delete {branch}. The branch disappears \
             on the server for everyone using this remote and cannot be \
             restored from here. Your local branches and the \
             {remote}/{branch} tracking ref are left alone."
        )),
        &[confirm_answer.as_str(), "Cancel"],
        cx,
    );

    let workspace = ctx.workspace;
    let repository = ctx.repository;
    let task = window.spawn(cx, async move |cx| {
        if answer.await.ok() != Some(0) {
            return anyhow::Ok(());
        }
        let askpass = cx.update(|window, cx| {
            askpass_delegate(
                workspace.clone(),
                format!("git push {remote} --delete {branch}"),
                window,
                cx,
            )
        })?;
        let push = repository.update(cx, |repo, cx| {
            // Empty local side of the refspec == delete the remote ref.
            repo.push(
                SharedString::default(),
                branch.clone(),
                remote.clone(),
                None,
                askpass,
                cx,
            )
        });
        match push.await {
            Ok(Ok(_output)) => {
                // The branch is gone on the server, but this clone's
                // `refs/remotes/<remote>/<branch>` survives until a
                // pruning fetch — leaving the ref chip painted on the
                // row, which reads as "the delete didn't work". Drop it
                // here; failing to is a cosmetic problem, not a reason
                // to report the delete as failed.
                let tracking_ref = format!("{remote}/{branch}");
                match repository
                    .update(cx, |repo, _| repo.delete_branch(true, tracking_ref, false))
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        log::warn!(
                            "deleted {branch} on {remote}, but the tracking ref remains: {error}"
                        )
                    }
                    Err(_) => {}
                }
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.show_toast(
                            Toast::new(
                                NotificationId::unique::<RemoteBranchDeletedToast>(),
                                format!("Deleted “{branch}” on “{remote}”."),
                            )
                            .autohide(),
                            cx,
                        );
                    })
                    .ok();
                anyhow::Ok(())
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow::anyhow!("delete on {remote} was canceled")),
        }
    });
    task.detach_and_prompt_err("Delete on remote failed", window, cx, |e, _, _| {
        Some(format!("{e}"))
    });
}

/// Mirrors `GitPanel::askpass_delegate`: a push can trip the credential
/// helper, and without a delegate the git subprocess waits forever on a
/// prompt nobody can see.
fn askpass_delegate(
    workspace: WeakEntity<Workspace>,
    operation: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) -> AskPassDelegate {
    let operation = operation.into();
    let window = window.window_handle();
    AskPassDelegate::new(&mut cx.to_async(), move |prompt, tx, cx| {
        window
            .update(cx, |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.toggle_modal(window, cx, |window, cx| {
                            AskPassModal::new(operation.clone(), prompt.into(), tx, window, cx)
                        });
                    })
                    .ok();
            })
            .ok();
    })
}

fn run_checkout_tag(
    repo: Entity<Repository>,
    tag: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let recv = repo.update(cx, |repo, _| repo.checkout_revision(tag.to_string()));
    await_repo_recv(recv, "checkout was canceled", "Checkout failed", window, cx);
}

/// Marker type for the post-delete "tag deleted — also delete on origin?"
/// toast's [`NotificationId`].
struct TagDeletedToast;

fn run_delete_tag(ctx: CommitContext, tag: SharedString, window: &mut Window, cx: &mut App) {
    let repo = ctx.repository;
    let workspace = ctx.workspace;
    let recv = repo.update(cx, |repo, _| repo.delete_tag(tag.to_string()));
    let task = cx.spawn({
        let tag = tag.clone();
        let repo = repo.clone();
        async move |cx| match recv.await {
            Ok(Ok(())) => {
                offer_remote_tag_delete(workspace, repo, tag, cx);
                anyhow::Ok(())
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow::anyhow!("delete tag was canceled")),
        }
    });
    task.detach_and_prompt_err("Delete tag failed", window, cx, |e, _, _| {
        Some(format!("{e}"))
    });
}

/// IDEA-style: after a local tag is deleted, drop a toast offering to
/// delete the tag on `origin` too. The remote may not actually have the
/// tag — in that case `git push --delete` errors and the error surfaces
/// through the toast handler's log (no notification, to avoid noise).
fn offer_remote_tag_delete(
    workspace: WeakEntity<Workspace>,
    repo: Entity<Repository>,
    tag: SharedString,
    cx: &mut gpui::AsyncApp,
) {
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<TagDeletedToast>(),
                    format!("Deleted tag “{tag}”."),
                )
                .autohide()
                .on_click("Also delete on origin", move |_window, cx| {
                    let recv = repo.update(cx, |repo, _| {
                        repo.delete_remote_tag("origin".into(), tag.to_string())
                    });
                    cx.spawn(async move |_| match recv.await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => log::error!("delete remote tag failed: {error}"),
                        Err(_) => {}
                    })
                    .detach();
                }),
                cx,
            );
        })
        .ok();
}

fn run_merge_branch(
    work_dir: PathBuf,
    target_branch: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let task = merge::run(work_dir, target_branch.to_string(), false, false, None, cx);
    task.detach_and_prompt_err("Merge failed", window, cx, |e, _, _| Some(format!("{e}")));
}

fn build_external_submenu(menu: ContextMenu, ctx: CommitContext) -> ContextMenu {
    let CommitContext {
        sha,
        repository,
        provider,
        ..
    } = ctx;
    let provider_name = provider
        .as_ref()
        .map(|(name, _)| name.clone())
        .unwrap_or_default();

    let open_label: SharedString = if provider_name.is_empty() {
        "Open Commit on Host".into()
    } else {
        format!("Open Commit on {provider_name}").into()
    };

    let sha_for_open = sha.clone();
    let repository_for_open = repository.clone();
    let menu = menu
        .entry(open_label, None, move |_, cx| {
            let sha = sha_for_open.clone();
            repository_for_open.update(cx, |repo, cx| {
                if let Some(url) = copy::build_permalink(repo, &sha, cx) {
                    cx.open_url(&url);
                }
            });
        })
        .entry("Copy Web URL", None, move |_, cx| {
            let sha = sha.clone();
            repository.update(cx, |repo, cx| {
                if let Some(url) = copy::build_permalink(repo, &sha, cx) {
                    cx.write_to_clipboard(ClipboardItem::new_string(url));
                }
            });
        });
    menu.entry("Open Compare with HEAD on Host", None, |_, _| {
        // Deferred: providers' permalink trait doesn't yet expose a
        // commit-compare URL builder; lands alongside S-DST's
        // host-aware compare when the trait is extended.
    })
}

fn open_new_branch_modal(ctx: CommitContext, window: &mut Window, cx: &mut App) {
    let Some(workspace) = ctx.workspace.upgrade() else {
        return;
    };
    let repository = ctx.repository;
    let sha = ctx.sha;
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            NameInputModal::new(
                "Create Branch",
                "Branch name",
                IconName::GitBranch,
                window,
                cx,
                move |name, window, cx| {
                    let task =
                        branch::create_branch_at(repository, sha.to_string(), name, true, cx);
                    task.detach_and_prompt_err("Failed to create branch", window, cx, |e, _, _| {
                        Some(format!("{e}"))
                    });
                },
            )
        });
    });
}

fn open_new_tag_modal(ctx: CommitContext, window: &mut Window, cx: &mut App) {
    let Some(workspace) = ctx.workspace.upgrade() else {
        return;
    };
    let repository = ctx.repository;
    let sha = ctx.sha;
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            NameInputModal::new(
                "Create Tag",
                "Tag name",
                IconName::Hash,
                window,
                cx,
                move |name, window, cx| {
                    let task = tag::create_tag_at(repository, sha.to_string(), name, None, cx);
                    task.detach_and_prompt_err("Failed to create tag", window, cx, |e, _, _| {
                        Some(format!("{e}"))
                    });
                },
            )
        });
    });
}

fn open_checkout_confirmation(ctx: CommitContext, window: &mut Window, cx: &mut App) {
    let repository = ctx.repository;
    let sha = ctx.sha.to_string();
    let short: String = sha.chars().take(7).collect();

    let answer = window.prompt(
        gpui::PromptLevel::Warning,
        &format!("Checkout {short}?"),
        Some(
            "You will be in a detached HEAD state. Any uncommitted \
             changes that don't conflict are kept; changes that conflict \
             with the target revision will fail the checkout. Use \
             'Discard and Checkout' if you want to throw away local \
             changes first.",
        ),
        &["Checkout", "Discard and Checkout", "Cancel"],
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            let force = match answer.await.ok() {
                Some(0) => false,
                Some(1) => true,
                _ => return anyhow::Ok(()),
            };
            cx.update(|window, cx| {
                let task = checkout::checkout_revision(repository, sha, force, cx);
                task.detach_and_prompt_err("Failed to checkout revision", window, cx, |e, _, _| {
                    Some(format!("{e}"))
                });
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

// =====================================================================
//  S-DST destructive-section drivers — invoked from the context menu.
//
//  Each driver collects user confirmation via `window.prompt`, looks up
//  the repo work-dir, and dispatches the matching handler. Errors land
//  through `detach_and_prompt_err` so the user sees a notification
//  rather than a silent log entry.
// =====================================================================

fn repo_work_dir(ctx: &CommitContext, cx: &App) -> Option<PathBuf> {
    if let Some(dir) = ctx.work_dir.clone() {
        return Some(dir);
    }
    let repo = ctx.repository.read(cx);
    Some(repo.work_directory_abs_path.to_path_buf())
}

fn run_cherry_pick(ctx: CommitContext, window: &mut Window, cx: &mut App) {
    let Some(work_dir) = repo_work_dir(&ctx, cx) else {
        return;
    };
    let sha = ctx.sha.to_string();
    let task = cherry_pick::run(work_dir, vec![sha], false, None, false, cx);
    task.detach_and_prompt_err("Cherry-pick failed", window, cx, |e, _, _| {
        Some(format!("{e}"))
    });
}

fn run_revert(ctx: CommitContext, window: &mut Window, cx: &mut App) {
    let Some(work_dir) = repo_work_dir(&ctx, cx) else {
        return;
    };
    let sha = ctx.sha.to_string();
    let task = revert::run(work_dir, vec![sha], false, None, cx);
    task.detach_and_prompt_err("Revert failed", window, cx, |e, _, _| Some(format!("{e}")));
}

fn build_reset_submenu(menu: ContextMenu, ctx: CommitContext) -> ContextMenu {
    use git::operations::reset::ResetMode;
    let soft_ctx = ctx.clone();
    let mixed_ctx = ctx.clone();
    let hard_ctx = ctx.clone();
    let keep_ctx = ctx;
    menu.entry("Soft (--soft)", None, move |window, cx| {
        run_reset(soft_ctx.clone(), ResetMode::Soft, false, window, cx);
    })
    .entry("Mixed (--mixed)", None, move |window, cx| {
        run_reset(mixed_ctx.clone(), ResetMode::Mixed, false, window, cx);
    })
    .entry("Hard (--hard)", None, move |window, cx| {
        run_reset(hard_ctx.clone(), ResetMode::Hard, true, window, cx);
    })
    .entry("Keep (--keep)", None, move |window, cx| {
        run_reset(keep_ctx.clone(), ResetMode::Keep, false, window, cx);
    })
}

fn run_reset(
    ctx: CommitContext,
    mode: git::operations::reset::ResetMode,
    require_double_confirm: bool,
    window: &mut Window,
    cx: &mut App,
) {
    use git::operations::reset::ResetMode;
    let Some(work_dir) = repo_work_dir(&ctx, cx) else {
        return;
    };
    let sha = ctx.sha.to_string();
    let short: String = sha.chars().take(7).collect();
    let label = match mode {
        ResetMode::Soft => "soft",
        ResetMode::Mixed => "mixed",
        ResetMode::Hard => "HARD",
        ResetMode::Keep => "keep",
    };
    let level = if require_double_confirm {
        gpui::PromptLevel::Critical
    } else {
        gpui::PromptLevel::Warning
    };
    let detail = if require_double_confirm {
        "Hard reset DROPS commits AND working-tree changes. \
         The branch tip will be backed up — use Undo Last Operation to \
         recover. Working-tree edits are NOT recoverable. Confirm twice."
            .to_string()
    } else {
        format!("git reset --{label} {short} on the current branch.")
    };
    let answer = window.prompt(
        level,
        &format!("Reset --{label} to {short}?"),
        Some(&detail),
        &["Reset", "Cancel"],
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            match answer.await.ok() {
                Some(0) => {}
                _ => return anyhow::Ok(()),
            }
            if require_double_confirm {
                let second = cx.update(|window, cx| {
                    window.prompt(
                        gpui::PromptLevel::Critical,
                        "Are you absolutely sure?",
                        Some("This destroys uncommitted edits."),
                        &["Yes, hard reset", "Cancel"],
                        cx,
                    )
                })?;
                if second.await.ok() != Some(0) {
                    return anyhow::Ok(());
                }
            }
            cx.update(|window, cx| {
                let task = reset::run(work_dir, sha, mode, cx);
                task.detach_and_prompt_err("Reset failed", window, cx, |e, _, _| {
                    Some(format!("{e}"))
                });
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

fn run_drop_commit(ctx: CommitContext, window: &mut Window, cx: &mut App) {
    let Some(work_dir) = repo_work_dir(&ctx, cx) else {
        return;
    };
    let sha = ctx.sha.to_string();
    let short: String = sha.chars().take(7).collect();
    let answer = window.prompt(
        gpui::PromptLevel::Warning,
        &format!("Drop commit {short}?"),
        Some(
            "Rewrites history above this commit. The branch tip is \
             backed up — use Undo Last Operation to recover.",
        ),
        &["Drop", "Cancel"],
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            if answer.await.ok() != Some(0) {
                return anyhow::Ok(());
            }
            cx.update(|window, cx| {
                let task = drop_handler::run(
                    work_dir,
                    sha,
                    git::operations::rebase::RebaseCallbacks::default(),
                    cx,
                );
                task.detach_and_prompt_err("Drop commit failed", window, cx, |e, _, _| {
                    Some(format!("{e}"))
                });
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

fn run_squash_with_previous(ctx: CommitContext, window: &mut Window, cx: &mut App) {
    let Some(work_dir) = repo_work_dir(&ctx, cx) else {
        return;
    };
    let sha = ctx.sha.to_string();
    let subject = ctx.subject.to_string();
    // Squash <sha> onto its predecessor: the previous commit becomes
    // the base pick, this commit becomes the squash target. Re-uses
    // the existing commit subject as the final message; user can amend
    // in a follow-up commit if they need different wording.
    let prev = format!("{sha}^");
    let task = squash::run(
        work_dir,
        vec![prev, sha],
        subject,
        git::operations::rebase::RebaseCallbacks::default(),
        cx,
    );
    task.detach_and_prompt_err("Squash failed", window, cx, |e, _, _| Some(format!("{e}")));
}

fn run_fixup_with_previous(ctx: CommitContext, window: &mut Window, cx: &mut App) {
    let Some(work_dir) = repo_work_dir(&ctx, cx) else {
        return;
    };
    let sha = ctx.sha.to_string();
    let prev = format!("{sha}^");
    let task = fixup::run(
        work_dir,
        vec![prev, sha],
        git::operations::rebase::RebaseCallbacks::default(),
        cx,
    );
    task.detach_and_prompt_err("Fixup failed", window, cx, |e, _, _| Some(format!("{e}")));
}

fn open_interactive_rebase(ctx: CommitContext, window: &mut Window, cx: &mut App) {
    let sha = ctx.sha.to_string();
    window.dispatch_action(
        Box::new(crate::fork_actions::InteractiveRebaseFromHere { sha }),
        cx,
    );
}

fn open_edit_message_prompt(ctx: CommitContext, window: &mut Window, cx: &mut App) {
    let Some(workspace) = ctx.workspace.upgrade() else {
        return;
    };
    let work_dir = match repo_work_dir(&ctx, cx) {
        Some(dir) => dir,
        None => return,
    };
    let sha = ctx.sha.to_string();
    let initial = ctx.subject.to_string();
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            EditMessageModal::new(sha, initial, work_dir, window, cx)
        });
    });
}

struct EditMessageModal {
    sha: String,
    work_dir: PathBuf,
    editor: Entity<Editor>,
}

impl EditMessageModal {
    fn new(
        sha: String,
        initial: String,
        work_dir: PathBuf,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(initial, window, cx);
            editor
        });
        Self {
            sha,
            work_dir,
            editor,
        }
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut gpui::Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let new_message = self.editor.read(cx).text(cx);
        if new_message.trim().is_empty() {
            return;
        }
        let task = edit_message::run(
            self.work_dir.clone(),
            self.sha.clone(),
            new_message,
            git::operations::rebase::RebaseCallbacks::default(),
            cx,
        );
        task.detach_and_prompt_err("Edit message failed", window, cx, |e, _, _| {
            Some(format!("{e}"))
        });
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for EditMessageModal {}
impl ModalView for EditMessageModal {}
impl Focusable for EditMessageModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for EditMessageModal {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let short: String = self.sha.chars().take(7).collect();
        v_flex()
            .key_context("EditMessageModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(40.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::Pencil).size(IconSize::XSmall))
                    .child(
                        Headline::new(format!("Edit Message ({short})")).size(HeadlineSize::XSmall),
                    ),
            )
            .child(div().px_3().pb_3().w_full().child(self.editor.clone()))
    }
}

/// Tiny single-line input modal — gives the user a place to type a name
/// for "New Branch" / "New Tag". Mirrors `RenameBranchModal` in `git_ui`
/// (modal + Editor::single_line + Confirm/Cancel actions). Kept local to
/// the context-menu submodule because it has no other callers; if a
/// third caller appears we'll promote it to `git_ui`.
pub struct NameInputModal {
    title: SharedString,
    icon: IconName,
    editor: Entity<Editor>,
    on_confirm: Option<Box<dyn FnOnce(String, &mut Window, &mut App) + 'static>>,
}

impl NameInputModal {
    fn new<F>(
        title: impl Into<SharedString>,
        placeholder: &str,
        icon: IconName,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
        on_confirm: F,
    ) -> Self
    where
        F: FnOnce(String, &mut Window, &mut App) + 'static,
    {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(placeholder, window, cx);
            editor
        });
        Self {
            title: title.into(),
            icon,
            editor,
            on_confirm: Some(Box::new(on_confirm)),
        }
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut gpui::Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let name = self.editor.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            return;
        }
        if let Some(callback) = self.on_confirm.take() {
            callback(name, window, cx);
        }
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for NameInputModal {}
impl ModalView for NameInputModal {}

impl Focusable for NameInputModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for NameInputModal {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("NameInputModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(34.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(
                        Icon::new(self.icon)
                            .size(IconSize::XSmall)
                            .color(Color::Default),
                    )
                    .child(Headline::new(self.title.clone()).size(HeadlineSize::XSmall)),
            )
            .child(div().px_3().pb_3().w_full().child(self.editor.clone()))
    }
}

// =====================================================================
//  S-PCH — Patch submenu (create patch from a commit row).
// =====================================================================

fn build_patch_submenu(menu: ContextMenu, ctx: CommitContext) -> ContextMenu {
    let single_ctx = ctx.clone();
    let range_ctx = ctx;
    menu.entry("Create Patch from Here…", None, move |window, cx| {
        run_create_patch(single_ctx.clone(), /*range_to_head*/ false, window, cx);
    })
    .entry(
        "Create Patch (range to HEAD)…",
        None,
        move |window, cx| {
            run_create_patch(range_ctx.clone(), /*range_to_head*/ true, window, cx);
        },
    )
}

fn run_create_patch(ctx: CommitContext, range_to_head: bool, window: &mut Window, cx: &mut App) {
    let Some(work_dir) = repo_work_dir(&ctx, cx) else {
        return;
    };
    let sha = ctx.sha.to_string();
    let sha_to = if range_to_head {
        Some("HEAD".to_string())
    } else {
        None
    };
    patch_handler::create_patch_action(ctx.workspace, work_dir, sha, sha_to, window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    /// Fake repository holding a single branch that `git branch -d`
    /// refuses to delete, plus the window the delete prompt is driven
    /// from.
    async fn init_delete_branch_test(
        branch: &str,
        cx: &mut TestAppContext,
    ) -> (CommitContext, VisualTestContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                ".git": {},
                "file.txt": "buffer_text".to_string()
            }),
        )
        .await;
        fs.set_head_for_repo(
            path!("/dir/.git").as_ref(),
            &[("file.txt", "test".to_string())],
            "deadbeef",
        );

        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let repository = cx
            .read(|cx| project.read(cx).active_repository(cx))
            .expect("fake project should expose a repository");

        repository
            .update(cx, |repo, _| repo.create_branch(branch.to_string(), None))
            .await
            .expect("create_branch was canceled")
            .expect("create_branch failed");
        cx.run_until_parked();

        fs.with_git_state(path!("/dir/.git").as_ref(), true, |state| {
            state
                .branches_requiring_force_delete
                .insert(branch.to_string());
        })
        .expect("failed to mark test branch as requiring force delete");

        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");

        let ctx = CommitContext {
            workspace: workspace.downgrade(),
            repository,
            sha: "deadbeef".into(),
            subject: "".into(),
            provider: None,
            work_dir: Some(path!("/dir").into()),
            member_id: None,
            refs: vec![branch.into()],
            head_branch: Some("main".into()),
            local_branches: vec![LocalBranchInfo {
                name: branch.into(),
                upstream: None,
                ahead: 0,
            }],
            remote_branches: Vec::new(),
            remotes: vec!["origin".into()],
        };

        (
            ctx,
            VisualTestContext::from_window(window_handle.into(), cx),
        )
    }

    async fn branch_names(ctx: &CommitContext, cx: &mut VisualTestContext) -> Vec<String> {
        ctx.repository
            .update(cx, |repo, _| repo.branches())
            .await
            .expect("branches was canceled")
            .expect("branches failed")
            .branches
            .into_iter()
            .map(|branch| branch.name().to_string())
            .collect()
    }

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
            ahead,
        }
    }

    fn remote_ref(full: &str, split: Option<(&str, &str)>) -> RemoteBranchRef {
        RemoteBranchRef {
            full: full.into(),
            split: split.map(|(remote, branch)| (remote.into(), branch.into())),
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

    #[gpui::test]
    async fn test_delete_unmerged_branch_offers_delete_anyway(cx: &mut TestAppContext) {
        let (ctx, mut cx) = init_delete_branch_test("feature-auth", cx).await;

        cx.update(|window, cx| {
            run_delete_branch(ctx.clone(), "feature-auth".into(), false, window, cx)
        });
        cx.run_until_parked();
        assert!(
            cx.has_pending_prompt(),
            "an unmerged branch delete must warn instead of dead-ending on git stderr"
        );

        cx.simulate_prompt_answer(FORCE_DELETE_BRANCH_ANSWER);
        cx.run_until_parked();

        assert!(
            !branch_names(&ctx, &mut cx)
                .await
                .iter()
                .any(|name| name == "feature-auth"),
            "\"{FORCE_DELETE_BRANCH_ANSWER}\" must force delete the branch"
        );
    }

    #[gpui::test]
    async fn test_delete_unmerged_branch_cancel_keeps_branch(cx: &mut TestAppContext) {
        let (ctx, mut cx) = init_delete_branch_test("feature-auth", cx).await;

        cx.update(|window, cx| {
            run_delete_branch(ctx.clone(), "feature-auth".into(), false, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());

        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(
            !cx.has_pending_prompt(),
            "cancelling must not chain into the raw git error prompt"
        );

        assert!(
            branch_names(&ctx, &mut cx)
                .await
                .iter()
                .any(|name| name == "feature-auth"),
            "cancelling must leave the branch in place"
        );
    }
}
