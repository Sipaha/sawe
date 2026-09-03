//! S-CTM per-ref submenu commands — the `run_*` / `open_*` handlers
//! behind every row [`super::branch_submenu`] plans, plus the small
//! await/outcome adapters they share.
//!
//! Split out of `commit_context_menu.rs` unchanged. These are the
//! functions that actually touch the repository: checkout, pull, push,
//! rename, the local and server-side deletes, and the tag pair.

use std::path::PathBuf;

use anyhow::Context as _;
use git::operations::RunOutcome;
use git::repository::PushOptions;
use gpui::{App, AsyncWindowContext, Entity, PromptLevel, SharedString, WeakEntity, Window};
use project::git_store::Repository;
use ui::IconName;
use workspace::{
    Toast, Workspace,
    notifications::{DetachAndPromptErr, NotificationId},
};
use zed_actions::NewWorktreeBranchTarget;

use crate::handlers::askpass::askpass_delegate;
use crate::handlers::branch::{
    FORCE_DELETE_BRANCH_ANSWER, ForceDeleteDecision, force_delete_decision,
    is_remote_ref_already_absent_error,
};
use crate::handlers::{compare, merge, protection, rebase, reset};

use super::branch_submenu::{CheckoutDivergence, RemoteBranchRef, checkout_divergence};
use super::{CommitContext, NameInputModal, repo_work_dir};

pub(super) fn await_repo_recv(
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

pub(super) fn run_checkout_branch(
    repo: Entity<Repository>,
    branch: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let recv = repo.update(cx, |repo, _| repo.change_branch(branch.to_string()));
    await_repo_recv(recv, "checkout was canceled", "Checkout failed", window, cx);
}

/// Await a `change_branch` job, flattening its two failure channels.
pub(super) async fn checkout_branch(
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
pub(super) fn describe_outcome(outcome: RunOutcome) -> anyhow::Result<()> {
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

/// Answers offered by the diverged-checkout prompt, in the order the
/// platform dialog shows them.
///
/// **Order is load-bearing and is pinned by a test.** The first answer
/// is the one Enter activates, so throwing commits away must never sit
/// at index 0. The indices are matched positionally below, so reordering
/// this array alone silently rewires the handlers too.
pub(super) const CHECKOUT_DIVERGENCE_ANSWERS: [&str; 3] =
    ["Rebase onto Remote", "Drop Local Commits", "Cancel"];

/// "Checkout" on a remote-tracking ref. `change_branch` creates (or
/// re-points) the matching local branch with `--track` and checks that
/// out, so when the local branch already carries commits the remote
/// doesn't have, doing it silently would strand them on a branch the
/// user believes they just synced. IDEA asks first; so do we.
pub(super) fn run_checkout_remote_branch(
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
        &CHECKOUT_DIVERGENCE_ANSWERS,
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
pub(super) fn open_new_branch_from_ref_modal(
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
pub(super) fn run_checkout_and_rebase(
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
///
/// `branch` is the local branch to stand on; `remote_branch` is the
/// branch half of its upstream, which is what `git pull <remote> <ref>`
/// names. The two differ whenever a branch tracks an upstream of another
/// name, so passing the local name here pulls the wrong ref (or fails).
pub(super) fn run_checkout_and_update(
    ctx: CommitContext,
    branch: SharedString,
    remote: SharedString,
    remote_branch: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let workspace = ctx.workspace;
    let repository = ctx.repository;
    let task = window.spawn(cx, async move |cx| {
        checkout_branch(&repository, &branch, cx).await?;
        let askpass = cx.update(|window, cx| {
            askpass_delegate(
                workspace.clone(),
                format!("git pull {remote} {remote_branch}"),
                window,
                cx,
            )
        })?;
        let pull = repository.update(cx, |repo, cx| {
            repo.pull(
                Some(remote_branch.clone()),
                remote.clone(),
                false,
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
    task.detach_and_prompt_err("Checkout and update failed", window, cx, |e, _, _| {
        Some(format!("{e}"))
    });
}

/// "Rebase '\<head\>' onto '\<ref\>'" — plain `git rebase <ref>` on the
/// current branch.
pub(super) fn run_rebase_onto(
    work_dir: PathBuf,
    target: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let rebase = rebase::run(work_dir, target.to_string(), false, cx);
    let task = cx.spawn(async move |_cx| describe_outcome(rebase.await?));
    task.detach_and_prompt_err("Rebase failed", window, cx, |e, _, _| Some(format!("{e}")));
}

/// "Pull into '\<head\>' Using Rebase / Using Merge" — `git pull
/// [--rebase] <remote> <branch>` into the current branch.
pub(super) fn run_pull_into_head(
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
pub(super) fn open_new_worktree_modal(
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
pub(super) struct BranchPushedToast;

/// "Push…" for a branch that isn't the current one. `PushDialog` only
/// knows how to push the checked-out branch, but `Repository::push`
/// takes any branch, so this confirms the exact refspec and runs it.
pub(super) fn run_push_branch(
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
pub(super) fn open_rename_branch_modal(
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
pub(super) fn run_show_diff_with_working_tree(
    ctx: CommitContext,
    window: &mut Window,
    cx: &mut App,
) {
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
/// Local branches only: `plan_delete_row` routes a remote-tracking
/// decoration to [`BranchAction::DeleteOnRemote`], so this never has to
/// spell `git branch -dr`. (The tracking ref is pruned by
/// [`run_delete_remote_branch`] right after the server-side delete
/// succeeds, which is the only moment this menu has a reason to touch
/// it.)
///
/// This uses a two-answer prompt rather than a toast because the failure
/// already surfaces as a modal here (`detach_and_prompt_err`), so adding
/// the escape hatch to that same modal keeps one surface instead of two,
/// and matches how the branch picker (entry A) and every other
/// destructive git confirm in this crate is spelled.
pub(super) fn run_delete_branch(
    ctx: CommitContext,
    branch: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let repo = ctx.repository;
    let work_dir = ctx.work_dir;
    let recv = repo.update(cx, |repo, _| {
        repo.delete_branch(false, branch.to_string(), false)
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
                    repo.delete_branch(false, branch.to_string(), true)
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
pub(super) struct RemoteBranchDeletedToast;

/// Re-read the branch list so the graph's ref chips match reality again.
///
/// Every arm of the remote delete needs one of these: the fs watcher does
/// not report `refs/remotes/**`, so neither the delete-push nor the
/// tracking-ref prune repaints anything on its own.
async fn refresh_branches_after_remote_delete(
    repository: &Entity<Repository>,
    cx: &mut AsyncWindowContext,
) {
    match repository
        .update(cx, |repo, cx| repo.refresh_branches(cx))
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => log::warn!("branch rescan after a remote delete failed: {error}"),
        Err(_) => {
            log::warn!("branch rescan after a remote delete was dropped before it ran")
        }
    }
}

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
pub(super) fn run_delete_remote_branch(
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
            "Runs git push {remote} --delete {branch}, then prunes this clone's \
             now-dangling {remote}/{branch} tracking ref. The branch disappears \
             on the server for everyone using this remote and cannot be restored \
             from here. Your local branches are left alone."
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
            //
            // The destination is spelled fully qualified on purpose. A
            // *short* destination makes git resolve the name against the
            // remote's advertised refs and abort with "unable to delete
            // '<branch>': remote ref does not exist" when the branch is
            // already gone — the maintainer's bug. `refs/heads/<branch>`
            // needs no resolution, so the delete succeeds whether or not
            // the ref is still there, and still deletes it when it is.
            // That is what makes this idempotent without an `ls-remote`
            // probe, which would cost a network round trip (and possibly
            // a credential prompt) on every delete and still be racy.
            repo.push(
                SharedString::default(),
                SharedString::from(format!("refs/heads/{branch}")),
                remote.clone(),
                None,
                askpass,
                cx,
            )
        });
        let was_already_absent = match push.await {
            Ok(Ok(_output)) => false,
            // Safety net for a git that refuses the qualified refspec
            // too: the requested end state is the actual one, so this is
            // a success, not a failure to report.
            Ok(Err(error)) if is_remote_ref_already_absent_error(&error) => true,
            Ok(Err(error)) => {
                // The push failed for a reason we cannot interpret. Do
                // NOT prune the tracking ref — a network, auth or
                // protected-branch refusal says nothing about whether
                // the branch is still on the server, and dropping the
                // ref would hide a branch that really does exist. Only
                // re-read the branch list, so the graph behind the
                // failure modal shows whatever the truth now is.
                refresh_branches_after_remote_delete(&repository, cx).await;
                return Err(error);
            }
            Err(_) => return Err(anyhow::anyhow!("delete on {remote} was canceled")),
        };

        // The branch is gone on the server, but this clone's
        // `refs/remotes/<remote>/<branch>` survives until a pruning
        // fetch — leaving the ref chip painted on the row, which reads
        // as "the delete didn't work". Drop it here; failing to is a
        // cosmetic problem, not a reason to report the delete as failed.
        let tracking_ref = format!("{remote}/{branch}");
        match repository
            .update(cx, |repo, _| repo.delete_branch(true, tracking_ref, false))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                log::warn!("deleted {branch} on {remote}, but the tracking ref remains: {error}")
            }
            Err(_) => log::warn!(
                "deleted {branch} on {remote}, but the tracking-ref prune was dropped \
                 before it ran; the ref remains"
            ),
        }
        // `Repository::push` rescans the branch list itself, but from
        // inside the push job — i.e. before the prune above ran, so that
        // rescan still sees the dangling tracking ref. The prune's own
        // write under `.git` does eventually schedule a snapshot
        // recompute, so this second rescan is belt and braces rather
        // than the only thing that repaints; it makes the republish
        // immediate instead of waiting on a debounced fs event.
        refresh_branches_after_remote_delete(&repository, cx).await;

        workspace
            .update(cx, |workspace, cx| {
                let message = if was_already_absent {
                    format!(
                        "“{branch}” was already gone on “{remote}”. \
                         Removed this clone's “{remote}/{branch}” tracking ref."
                    )
                } else {
                    format!("Deleted “{branch}” on “{remote}”.")
                };
                workspace.show_toast(
                    Toast::new(
                        NotificationId::unique::<RemoteBranchDeletedToast>(),
                        message,
                    )
                    .autohide(),
                    cx,
                );
            })
            .ok();
        anyhow::Ok(())
    });
    task.detach_and_prompt_err("Delete on remote failed", window, cx, |e, _, _| {
        Some(format!("{e}"))
    });
}

pub(super) fn run_checkout_tag(
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
pub(super) struct TagDeletedToast;

pub(super) fn run_delete_tag(
    ctx: CommitContext,
    tag: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
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
/// tag, which is harmless: `delete_remote_tag` spells the destination
/// `refs/tags/<tag>`, and a fully-qualified delete refspec needs no
/// resolution against the remote's advertised refs, so git exits 0 with
/// a `deleting a non-existent ref` warning rather than failing. Genuine
/// failures surface through the toast handler's log (no notification, to
/// avoid noise).
pub(super) fn offer_remote_tag_delete(
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
                        Err(_) => log::warn!(
                            "delete remote tag was dropped before it ran; the tag may still \
                             exist on origin"
                        ),
                    })
                    .detach();
                }),
                cx,
            );
        })
        .ok();
}

pub(super) fn run_merge_branch(
    work_dir: PathBuf,
    target_branch: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let task = merge::run(work_dir, target_branch.to_string(), false, false, None, cx);
    task.detach_and_prompt_err("Merge failed", window, cx, |e, _, _| Some(format!("{e}")));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit_context_menu::LocalBranchInfo;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use std::sync::Arc;
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

    /// Boots a fake single-repository project plus the window prompts
    /// are driven from, and hands back a [`CommitContext`] carrying the
    /// defaults every test here shares. Fields that describe the *row*
    /// (`refs`, `local_branches`, `head_branch`) are plain data the
    /// caller overwrites — they need not match what the fake repository
    /// actually holds, which is what lets a test claim a diverged local
    /// branch without building one.
    async fn init_commit_context_test(
        cx: &mut TestAppContext,
    ) -> (Arc<FakeFs>, CommitContext, VisualTestContext) {
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
            refs: Vec::new(),
            head_branch: Some("main".into()),
            local_branches: Vec::new(),
            remote_branches: Vec::new(),
            remotes: vec!["origin".into()],
        };

        (
            fs,
            ctx,
            VisualTestContext::from_window(window_handle.into(), cx),
        )
    }

    /// The branch the fake repository currently has checked out.
    /// `change_branch` is the only thing that moves it, so this is how a
    /// test tells "the checkout ran" from "it didn't".
    fn current_branch(fs: &FakeFs) -> Option<String> {
        fs.with_git_state(path!("/dir/.git").as_ref(), false, |state| {
            state.current_branch_name.clone()
        })
        .expect("fake repository should exist")
    }

    /// Fake repository holding a single branch that `git branch -d`
    /// refuses to delete, plus the window the delete prompt is driven
    /// from.
    async fn init_delete_branch_test(
        branch: &str,
        cx: &mut TestAppContext,
    ) -> (CommitContext, VisualTestContext) {
        let (fs, mut ctx, mut cx) = init_commit_context_test(cx).await;

        ctx.repository
            .update(&mut cx, |repo, _| {
                repo.create_branch(branch.to_string(), None)
            })
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

        ctx.refs = vec![branch.into()];
        ctx.local_branches = vec![LocalBranchInfo {
            name: branch.into(),
            upstream: None,
            upstream_gone: false,
            ahead: 0,
        }];

        (ctx, cx)
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

    fn local_info(name: &str, upstream: Option<&str>, ahead: u32) -> LocalBranchInfo {
        LocalBranchInfo {
            name: name.into(),
            upstream: upstream.map(Into::into),
            upstream_gone: false,
            ahead,
        }
    }

    fn remote_ref(full: &str, split: Option<(&str, &str)>) -> RemoteBranchRef {
        RemoteBranchRef {
            full: full.into(),
            split: split.map(|(remote, branch)| (remote.into(), branch.into())),
            gone: false,
        }
    }

    /// The one property of the diverged-checkout prompt that is a
    /// safety invariant rather than a matter of taste: the platform
    /// activates the *first* answer on Enter, so the answer that throws
    /// commits away must never sit there. The dialog is shown by the
    /// platform, so the order can only be pinned at its source.
    #[test]
    fn test_divergence_prompt_defaults_to_rebase_not_dropping_commits() {
        assert_eq!(
            CHECKOUT_DIVERGENCE_ANSWERS[0], "Rebase onto Remote",
            "Enter activates the first answer — it must never discard local commits"
        );
        assert_eq!(
            CHECKOUT_DIVERGENCE_ANSWERS,
            ["Rebase onto Remote", "Drop Local Commits", "Cancel"],
            "run_checkout_remote_branch matches these answers positionally, so reordering \
             them rewires the handlers"
        );
    }

    /// A [`CommitContext`] whose row claims local `master` is three
    /// commits ahead of `origin/master` — the case the checkout prompt
    /// exists for. The fake repository deliberately holds no such
    /// branch: divergence is decided from the row's data.
    fn diverged_on_master(ctx: &mut CommitContext) {
        ctx.head_branch = Some("master222".into());
        ctx.refs = vec!["origin/master".into()];
        ctx.remote_branches = vec!["origin/master".into()];
        ctx.local_branches = vec![local_info("master", Some("origin/master"), 3)];
    }

    #[gpui::test]
    async fn test_diverged_checkout_asks_before_touching_the_branch(cx: &mut TestAppContext) {
        let (fs, mut ctx, mut cx) = init_commit_context_test(cx).await;
        diverged_on_master(&mut ctx);
        let branch = remote_ref("origin/master", Some(("origin", "master")));

        cx.update(|window, cx| run_checkout_remote_branch(ctx.clone(), branch.clone(), window, cx));
        cx.run_until_parked();

        let (title, detail) = cx
            .pending_prompt()
            .expect("a diverged checkout must ask before running");
        assert_eq!(title, "Checkout origin/master");
        assert!(
            detail.contains(
                "Local branch 'master' has commits that do not exist in \
                 'origin/master'"
            ),
            "the prompt must name the branch whose commits are at stake: {detail}"
        );
        assert_eq!(
            current_branch(&fs),
            None,
            "nothing may be checked out before the user answers"
        );
    }

    #[gpui::test]
    async fn test_diverged_checkout_cancel_leaves_the_repository_alone(cx: &mut TestAppContext) {
        let (fs, mut ctx, mut cx) = init_commit_context_test(cx).await;
        diverged_on_master(&mut ctx);
        let branch = remote_ref("origin/master", Some(("origin", "master")));

        cx.update(|window, cx| run_checkout_remote_branch(ctx.clone(), branch, window, cx));
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());

        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        assert_eq!(
            current_branch(&fs),
            None,
            "cancelling must not check the remote ref out"
        );
        assert!(
            !cx.has_pending_prompt(),
            "cancelling must not chain into an error prompt"
        );
    }

    /// A ref with no known divergence skips the prompt entirely and
    /// checks out in one step — the common path, which must not
    /// regress into an extra confirmation.
    #[gpui::test]
    async fn test_undiverged_checkout_runs_without_a_prompt(cx: &mut TestAppContext) {
        let (fs, mut ctx, mut cx) = init_commit_context_test(cx).await;
        diverged_on_master(&mut ctx);
        ctx.local_branches = vec![local_info("master", Some("origin/master"), 0)];
        let branch = remote_ref("origin/master", Some(("origin", "master")));

        cx.update(|window, cx| run_checkout_remote_branch(ctx.clone(), branch, window, cx));
        cx.run_until_parked();

        assert!(!cx.has_pending_prompt());
        assert_eq!(current_branch(&fs), Some("origin/master".to_string()));
    }

    /// The server-side delete is a `Critical` confirm, and its body has
    /// to describe what actually happens: the push *and* the local
    /// prune of the now-dangling tracking ref that follows it.
    #[gpui::test]
    async fn test_delete_remote_branch_confirm_describes_the_tracking_ref_prune(
        cx: &mut TestAppContext,
    ) {
        let (_fs, ctx, mut cx) = init_commit_context_test(cx).await;

        cx.update(|window, cx| {
            run_delete_remote_branch(
                ctx.clone(),
                "origin".into(),
                "feature-auth".into(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let (title, detail) = cx
            .pending_prompt()
            .expect("deleting a branch on a remote must confirm first");
        assert_eq!(title, "Delete branch “feature-auth” on “origin”?");
        assert!(
            detail.contains("git push origin --delete feature-auth"),
            "the confirm must name the command it runs: {detail}"
        );
        assert!(
            detail.contains("prunes this clone's now-dangling origin/feature-auth tracking ref"),
            "the confirm must not claim the tracking ref is left alone — it is deleted right \
             after the push: {detail}"
        );

        // Cancelling is the only outcome this fixture can carry through:
        // `FakeGitRepository::push` is `unimplemented!()`, so confirming
        // would panic rather than exercise the delete.
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
    }

    /// Refspecs the fake repository has been asked to push, as
    /// `(local side, destination, remote)`.
    fn recorded_pushes(fs: &FakeFs) -> Vec<(String, String, String)> {
        fs.with_git_state(path!("/dir/.git").as_ref(), false, |state| {
            state
                .pushes
                .iter()
                .map(|push| {
                    (
                        push.branch.clone(),
                        push.remote_branch.clone(),
                        push.remote.clone(),
                    )
                })
                .collect()
        })
        .expect("fake repository should exist")
    }

    /// The branch list as the *UI* currently sees it — the snapshot the
    /// graph paints its ref chips from, which only a rescan updates.
    fn published_branch_refs(ctx: &CommitContext, cx: &mut VisualTestContext) -> Vec<String> {
        ctx.repository.read_with(cx, |repo, _| {
            repo.branch_list
                .iter()
                .map(|branch| branch.ref_name.to_string())
                .collect::<Vec<_>>()
        })
    }

    /// Drive a confirmed "Delete on origin…" against a clone that still
    /// has the `origin/feature-auth` tracking ref, with `push` either
    /// succeeding or failing with `push_error`.
    async fn run_confirmed_remote_delete(
        push_error: Option<&str>,
        cx: &mut TestAppContext,
    ) -> (Arc<FakeFs>, CommitContext, VisualTestContext) {
        let (fs, ctx, mut cx) = init_commit_context_test(cx).await;

        let push_error = push_error.map(str::to_string);
        fs.with_git_state(path!("/dir/.git").as_ref(), true, |state| {
            state.branches.insert("origin/feature-auth".to_string());
            state.simulated_push_error = push_error;
        })
        .expect("fake repository should exist");
        cx.run_until_parked();

        cx.update(|window, cx| {
            run_delete_remote_branch(
                ctx.clone(),
                "origin".into(),
                "feature-auth".into(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("Delete on origin");
        cx.run_until_parked();

        (fs, ctx, cx)
    }

    /// The whole of fix (a): the destination is spelled
    /// `refs/heads/<branch>`, which git resolves without asking the
    /// remote what it advertises — so the delete succeeds whether or not
    /// the branch is still there, with no `ls-remote` probe.
    #[gpui::test]
    async fn test_delete_on_remote_pushes_a_fully_qualified_refspec(cx: &mut TestAppContext) {
        let (fs, _ctx, _cx) = run_confirmed_remote_delete(None, cx).await;

        assert_eq!(
            recorded_pushes(&fs),
            vec![(
                String::new(),
                "refs/heads/feature-auth".to_string(),
                "origin".to_string()
            )],
            "a short destination makes git resolve the name against the remote's advertised \
             refs, which is what fails when the branch is already gone"
        );
    }

    /// After a successful delete the graph must not still paint a chip
    /// for the branch. Guards the tracking-ref prune; it does *not*
    /// distinguish the extra post-prune rescan, because pruning writes
    /// under `.git` and the resulting fs event schedules a full snapshot
    /// recompute of its own.
    #[gpui::test]
    async fn test_a_successful_remote_delete_republishes_the_pruned_branch_list(
        cx: &mut TestAppContext,
    ) {
        let (_fs, ctx, mut cx) = run_confirmed_remote_delete(None, cx).await;

        assert!(
            !cx.has_pending_prompt(),
            "a successful delete must not raise the failure modal"
        );
        assert!(
            !published_branch_refs(&ctx, &mut cx)
                .iter()
                .any(|ref_name| ref_name == "refs/remotes/origin/feature-auth"),
            "the branch list the graph paints from must no longer carry the pruned tracking ref"
        );
    }

    /// The maintainer's bug, end to end: a git that still refuses the
    /// qualified refspec must not dead-end on a modal — the requested
    /// end state is the actual one.
    #[gpui::test]
    async fn test_an_already_absent_remote_ref_is_not_reported_as_a_failure(
        cx: &mut TestAppContext,
    ) {
        let (_fs, ctx, mut cx) = run_confirmed_remote_delete(
            Some(
                "error: unable to delete 'feature-auth': remote ref does not exist\n\
                 error: failed to push some refs to 'gitlab.example.com:group/repo.git'",
            ),
            cx,
        )
        .await;

        assert!(
            !cx.has_pending_prompt(),
            "deleting a branch that is already gone on the remote is a no-op, not a failure"
        );
        assert!(
            !published_branch_refs(&ctx, &mut cx)
                .iter()
                .any(|ref_name| ref_name == "refs/remotes/origin/feature-auth"),
            "the stale tracking ref must be pruned, or the chip stays painted in the graph"
        );
    }

    /// Fix (c)'s other half, and its safety limit: an uninterpretable
    /// failure still raises the modal, and must NOT prune — a network or
    /// auth error says nothing about whether the branch is still there.
    #[gpui::test]
    async fn test_an_unrelated_push_failure_keeps_the_tracking_ref(cx: &mut TestAppContext) {
        let (_fs, ctx, mut cx) = run_confirmed_remote_delete(
            Some(
                "fatal: Authentication failed for 'https://gitlab.example.com/group/repo.git/'\n\
                 error: failed to push some refs to 'origin'",
            ),
            cx,
        )
        .await;

        let (title, _detail) = cx
            .pending_prompt()
            .expect("an uninterpretable push failure must still be reported");
        assert_eq!(title, "Delete on remote failed");
        assert!(
            published_branch_refs(&ctx, &mut cx)
                .iter()
                .any(|ref_name| ref_name == "refs/remotes/origin/feature-auth"),
            "a failed delete must leave the tracking ref alone — the branch may well still exist"
        );
    }

    /// "Push…" confirms the exact refspec before running, so a
    /// mis-resolved destination is visible before anything leaves the
    /// machine.
    #[gpui::test]
    async fn test_push_branch_confirms_the_exact_refspec(cx: &mut TestAppContext) {
        let (_fs, ctx, mut cx) = init_commit_context_test(cx).await;

        cx.update(|window, cx| {
            run_push_branch(
                ctx.clone(),
                "release".into(),
                "origin".into(),
                "release-1.2".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let (title, detail) = cx
            .pending_prompt()
            .expect("pushing a non-current branch must confirm first");
        assert_eq!(title, "Push 'release' to 'origin/release-1.2'?");
        assert_eq!(detail, "Runs git push origin release:release-1.2.");

        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
    }

    /// The first push of an untracked branch says so, because it also
    /// sets the upstream — a side effect the plain form doesn't have.
    #[gpui::test]
    async fn test_push_branch_set_upstream_confirm_says_so(cx: &mut TestAppContext) {
        let (_fs, ctx, mut cx) = init_commit_context_test(cx).await;

        cx.update(|window, cx| {
            run_push_branch(
                ctx.clone(),
                "scratch".into(),
                "origin".into(),
                "scratch".into(),
                Some(PushOptions::SetUpstream),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let (_title, detail) = cx.pending_prompt().expect("push must confirm first");
        assert!(
            detail.contains("git push --set-upstream origin scratch:scratch"),
            "{detail}"
        );

        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_delete_unmerged_branch_offers_delete_anyway(cx: &mut TestAppContext) {
        let (ctx, mut cx) = init_delete_branch_test("feature-auth", cx).await;

        cx.update(|window, cx| run_delete_branch(ctx.clone(), "feature-auth".into(), window, cx));
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

        cx.update(|window, cx| run_delete_branch(ctx.clone(), "feature-auth".into(), window, cx));
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
