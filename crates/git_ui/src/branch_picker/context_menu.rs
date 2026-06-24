//! Per-branch context menu for the S-BRP Branches popup. Wires entries
//! to existing infrastructure: S-CTM compare/checkout/copy, S-BAK atomic-op
//! runner, S-DST rebase/merge (via `handlers::{rebase,merge}` with backup +
//! conflict-resolver routing), and S-PSH force push (`git::ForcePush` → the
//! push preview dialog in force mode, i.e. `--force-with-lease`).

use git::operations::{DeleteBranchOp, OpRunner, RunOutcome};
use gpui::{App, ClipboardItem, Entity, SharedString, WeakEntity, Window};
use notifications::status_toast::StatusToast;
use project::git_store::Repository;
use ui::{ContextMenu, Icon, IconName, IconSize, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use crate::branch_picker::favorites;
use crate::handlers::compare as compare_handlers;
use crate::handlers::{merge as merge_handler, rebase as rebase_handler};
use crate::project_diff::ProjectDiff;

/// Surrounding context for a branch row's context menu. Cheap to clone.
#[derive(Clone)]
pub struct BranchContext {
    pub workspace: WeakEntity<Workspace>,
    pub repository: Entity<Repository>,
    pub branch_name: SharedString,
    pub is_remote: bool,
    pub is_head: bool,
    pub is_favorite: bool,
}

/// Build the context menu for a regular branch row (Recent / Local /
/// Remote / Favorites sections). Tag rows use [`build_tag_menu`] instead.
pub fn build_branch_menu(
    ctx: BranchContext,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |menu, _window, _cx| {
        let mut menu = menu;

        if !ctx.is_head {
            let checkout_ctx = ctx.clone();
            menu = menu.entry("Checkout", None, move |_window, cx| {
                checkout(checkout_ctx.clone(), cx);
            });
        }

        let new_branch_ctx = ctx.clone();
        menu = menu
            .entry(
                "Checkout as New Branch From Here…",
                None,
                move |_window, cx| {
                    checkout_as_new(new_branch_ctx.clone(), cx);
                },
            )
            .separator();

        let cwc_ctx = ctx.clone();
        menu = menu.entry("Compare with Current", None, move |window, cx| {
            compare_with_current(cwc_ctx.clone(), window, cx);
        });
        let dwt_ctx = ctx.clone();
        menu = menu.entry("Show Diff with Working Tree", None, move |window, cx| {
            compare_with_current(dwt_ctx.clone(), window, cx);
        });

        // S-DST destructive sub-actions; menu shape locked so future
        // S-* work drops in handlers without rearranging entries.
        let rebase_ctx = ctx.clone();
        let merge_ctx = ctx.clone();
        let row_branch = ctx.branch_name.clone();
        menu = menu
            .separator()
            .entry(
                format!("Rebase Current Onto {row_branch}"),
                None,
                move |window, cx| run_rebase(rebase_ctx.clone(), window, cx),
            )
            .entry(
                format!("Merge {row_branch} Into Current"),
                None,
                move |window, cx| run_merge(merge_ctx.clone(), window, cx),
            );

        menu = menu
            .separator()
            .entry("Pull", None, |window, cx| {
                window.dispatch_action(Box::new(git::Pull), cx);
            })
            .entry("Push", None, |window, cx| {
                window.dispatch_action(Box::new(git::Push), cx);
            })
            // S-PSH — `git::ForcePush` opens the push preview dialog in force
            // mode, which pushes with `--force-with-lease` (see
            // `PushOptions::Force`). The dialog is the confirmation surface.
            .entry("Force Push", None, |window, cx| {
                window.dispatch_action(Box::new(git::ForcePush), cx);
            });

        let upstream_ctx = ctx.clone();
        menu = menu
            .separator()
            .entry("Set Upstream…", None, move |window, cx| {
                open_set_upstream_modal(upstream_ctx.clone(), window, cx);
            });

        if !ctx.is_remote {
            let rename_ctx = ctx.clone();
            menu = menu.entry("Rename…", None, move |window, cx| {
                open_rename_modal(rename_ctx.clone(), window, cx);
            });
        }

        if !ctx.is_head {
            let delete_ctx = ctx.clone();
            menu = menu.entry("Delete", None, move |_window, cx| {
                delete_branch(delete_ctx.clone(), cx);
            });
        }

        let star_label = if ctx.is_favorite {
            "Unfavorite Branch"
        } else {
            "Favorite Branch"
        };
        let fav_ctx = ctx.clone();
        menu = menu
            .separator()
            .entry(star_label, None, move |_window, cx| {
                toggle_favorite(fav_ctx.clone(), cx);
            });

        let copy_branch_name = ctx.branch_name.to_string();
        menu = menu.entry("Copy Branch Name", None, move |_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_branch_name.clone()));
        });
        menu
    })
}

/// Per-tag row context menu (Tags section).
#[derive(Clone)]
pub struct TagContext {
    pub workspace: WeakEntity<Workspace>,
    pub repository: Entity<Repository>,
    pub tag_name: SharedString,
}

pub fn build_tag_menu(ctx: TagContext, window: &mut Window, cx: &mut App) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |menu, _window, _cx| {
        let checkout_ctx = ctx.clone();
        let compare_ctx = ctx.clone();
        let push_ctx = ctx.clone();
        let delete_ctx = ctx.clone();
        let copy_ctx = ctx;
        menu.entry("Checkout", None, move |_window, cx| {
            let tag = checkout_ctx.tag_name.clone();
            let repo = checkout_ctx.repository.clone();
            cx.spawn(async move |cx| {
                let recv = repo.update(cx, |repo, _| repo.checkout_revision(tag.to_string()));
                recv.await??;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        })
        .entry("Compare with Working Tree", None, move |window, cx| {
            if let Some(workspace) = compare_ctx.workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    compare_handlers::compare_with_local_working_tree(
                        workspace,
                        compare_ctx.tag_name.as_ref(),
                        window,
                        cx,
                    );
                });
            }
        })
        .entry("Push Tag", None, move |_window, cx| {
            let tag = push_ctx.tag_name.clone();
            let repo = push_ctx.repository.clone();
            cx.spawn(async move |cx| {
                let recv = repo.update(cx, |repo, _| {
                    repo.push_tag("origin".into(), tag.to_string())
                });
                recv.await??;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        })
        .entry("Delete Tag", None, move |_window, cx| {
            let tag = delete_ctx.tag_name.clone();
            let repo = delete_ctx.repository.clone();
            cx.spawn(async move |cx| {
                let recv = repo.update(cx, |repo, _| repo.delete_tag(tag.to_string()));
                recv.await??;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        })
        .separator()
        .entry("Copy Tag Name", None, move |_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_ctx.tag_name.to_string()));
        })
    })
}

fn checkout(ctx: BranchContext, cx: &mut App) {
    let branch = ctx.branch_name;
    let repo = ctx.repository;
    let work_dir = repo.read(cx).work_directory_abs_path.clone();
    cx.spawn(async move |cx| {
        let recv = repo.update(cx, |repo, _| repo.change_branch(branch.to_string()));
        recv.await??;
        favorites::record_checkout(&work_dir, branch.as_ref()).log_err();
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn checkout_as_new(ctx: BranchContext, cx: &mut App) {
    // `git switch -c <new> <base>` accepts a branch name as the base, so
    // we don't need to resolve to a sha first. Pick a safe default name
    // — the user will rename via the rename modal afterward if desired.
    let branch = ctx.branch_name;
    let repo = ctx.repository;
    cx.spawn(async move |cx| {
        let new_name = format!("{}-copy", branch);
        let recv = repo.update(cx, |repo, _| {
            repo.create_branch(new_name, Some(branch.to_string()))
        });
        recv.await??;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn compare_with_current(ctx: BranchContext, window: &mut Window, cx: &mut App) {
    let workspace = ctx.workspace;
    let branch = ctx.branch_name;
    if let Some(workspace) = workspace.upgrade() {
        workspace.update(cx, |workspace, cx| {
            ProjectDiff::deploy_at_revision(
                workspace,
                SharedString::from(branch.to_string()),
                window,
                cx,
            );
        });
    }
}

fn open_set_upstream_modal(ctx: BranchContext, window: &mut Window, cx: &mut App) {
    if let Some(workspace) = ctx.workspace.upgrade() {
        let repo = ctx.repository;
        let branch = ctx.branch_name;
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                super::SetUpstreamModal::new(repo, branch, window, cx)
            });
        });
    }
}

fn open_rename_modal(ctx: BranchContext, window: &mut Window, cx: &mut App) {
    if let Some(workspace) = ctx.workspace.upgrade() {
        let repo = ctx.repository;
        let branch = ctx.branch_name;
        let work_dir = repo.read(cx).work_directory_abs_path.clone();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                super::RenameBranchPopupModal::new(repo, branch, work_dir, window, cx)
            });
        });
    }
}

fn delete_branch(ctx: BranchContext, cx: &mut App) {
    let repo = ctx.repository.clone();
    let work_dir = repo.read(cx).work_directory_abs_path.clone();
    let branch = ctx.branch_name.clone();
    let is_remote = ctx.is_remote;
    if is_remote {
        // Remote-branch delete still goes through the existing
        // `Repository::delete_branch` path — we don't backup-ref remote
        // refs, since the remote retains them.
        cx.spawn(async move |cx| {
            let recv = repo.update(cx, |repo, _| repo.delete_branch(true, branch.to_string()));
            recv.await??;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
        return;
    }

    // Local branch: route through `OpRunner` so we get a backup-ref
    // before the delete. Force-delete confirmation modal is owned by
    // S-DST; for now we attempt non-force `-d` and the caller surfaces
    // any "not fully merged" error.
    let work_dir_buf = work_dir.to_path_buf();
    let branch_string = branch.to_string();
    cx.background_spawn(async move {
        OpRunner::run(
            DeleteBranchOp {
                name: branch_string,
                force: false,
            },
            &work_dir_buf,
        )
        .log_err();
    })
    .detach();
}

fn toggle_favorite(ctx: BranchContext, cx: &mut App) {
    let work_dir = ctx.repository.read(cx).work_directory_abs_path.clone();
    let branch = ctx.branch_name.to_string();
    let work_dir_buf = work_dir.to_path_buf();
    cx.background_spawn(async move {
        favorites::toggle_favorite(&work_dir_buf, &branch).log_err();
    })
    .detach();
}

/// Pure decision over the outcome of a merge/rebase op. `RunOutcome` has
/// no failure variant — hard failures surface as `Result::Err`, so the
/// runner result is mapped to `PostOp::Failed` separately.
#[derive(Debug, PartialEq)]
enum PostOp {
    Done,
    Conflict,
    Failed(String),
}

/// Classify a successful `RunOutcome` (the `Result::Ok` arm). A
/// `PausedForExecFailure` can't occur for plain merge/rebase (no `exec`
/// steps), but we map it to `Failed` defensively rather than panicking.
fn classify(outcome: &RunOutcome) -> PostOp {
    match outcome {
        RunOutcome::Completed => PostOp::Done,
        RunOutcome::PausedForConflict { .. } => PostOp::Conflict,
        RunOutcome::PausedForExecFailure { stderr, .. } => PostOp::Failed(stderr.clone()),
    }
}

/// Map the full `Result<RunOutcome>` from a handler into a `PostOp`.
fn classify_result(result: &Result<RunOutcome, anyhow::Error>) -> PostOp {
    match result {
        Ok(outcome) => classify(outcome),
        Err(err) => PostOp::Failed(err.to_string()),
    }
}

fn notify(
    workspace: &WeakEntity<Workspace>,
    message: impl Into<SharedString>,
    icon: IconName,
    color: Color,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let message = message.into();
    workspace.update(cx, |workspace, cx| {
        let toast = StatusToast::new(message, cx, move |this, _cx| {
            this.icon(Icon::new(icon).size(IconSize::Small).color(color))
        });
        workspace.toggle_status_toast(toast, cx);
    });
}

/// Apply a `PostOp` to the UI: success/error toast, or conflict toast plus
/// routing into the existing conflict resolver via `git::OpenConflictResolver`.
fn handle_post_op(
    post: PostOp,
    workspace: &WeakEntity<Workspace>,
    success_message: String,
    window: &mut Window,
    cx: &mut App,
) {
    match post {
        PostOp::Done => {
            notify(
                workspace,
                success_message,
                IconName::Check,
                Color::Success,
                cx,
            );
        }
        PostOp::Conflict => {
            notify(
                workspace,
                "Conflicts — resolve them in the editor",
                IconName::Warning,
                Color::Warning,
                cx,
            );
            // The resolver action resolves the active repository/workspace
            // itself, sidestepping any borrow of entities we already hold.
            window.dispatch_action(Box::new(git_conflict_ui::OpenConflictResolver), cx);
        }
        PostOp::Failed(message) => {
            notify(workspace, message, IconName::XCircle, Color::Error, cx);
        }
    }
}

/// "Rebase Current Onto <branch>" — rebase the current HEAD branch onto the
/// row's branch. `LinearRebaseOp` runs `git rebase --autostash <branch>`
/// under `OpRunner` (backup-ref created before the op); `--autostash`
/// handles a dirty working tree so we don't need a separate stash pre-flight.
fn run_rebase(ctx: BranchContext, window: &mut Window, cx: &mut App) {
    let work_dir = ctx.repository.read(cx).work_directory_abs_path.clone();
    let workspace = ctx.workspace.clone();
    let target = ctx.branch_name;
    let success_message = format!("Rebased onto {target}");
    let task = rebase_handler::run(work_dir.to_path_buf(), target.to_string(), true, cx);
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let post = classify_result(&result);
            cx.update(|window, cx| {
                handle_post_op(post, &workspace, success_message, window, cx);
            })
            .log_err();
        })
        .detach();
}

/// "Merge <branch> Into Current" — merge the row's branch into the current
/// HEAD branch. `MergeOp` runs under `OpRunner` (backup-ref of the current
/// branch created before the op).
fn run_merge(ctx: BranchContext, window: &mut Window, cx: &mut App) {
    let work_dir = ctx.repository.read(cx).work_directory_abs_path.clone();
    let workspace = ctx.workspace.clone();
    let target = ctx.branch_name;
    let success_message = format!("Merged {target} into current branch");
    let task = merge_handler::run(
        work_dir.to_path_buf(),
        target.to_string(),
        false,
        false,
        None,
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let post = classify_result(&result);
            cx.update(|window, cx| {
                handle_post_op(post, &workspace, success_message, window, cx);
            })
            .log_err();
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn classify_completed_is_done() {
        assert_eq!(classify(&RunOutcome::Completed), PostOp::Done);
    }

    #[test]
    fn classify_conflict_is_conflict() {
        let outcome = RunOutcome::PausedForConflict {
            conflicted_files: vec![PathBuf::from("a.txt")],
        };
        assert_eq!(classify(&outcome), PostOp::Conflict);
    }

    #[test]
    fn classify_exec_failure_is_failed() {
        let outcome = RunOutcome::PausedForExecFailure {
            command: "make".into(),
            stderr: "boom".into(),
        };
        assert_eq!(classify(&outcome), PostOp::Failed("boom".into()));
    }

    #[test]
    fn classify_result_err_is_failed() {
        let result: Result<RunOutcome, anyhow::Error> = Err(anyhow::anyhow!("rebase failed: nope"));
        assert_eq!(
            classify_result(&result),
            PostOp::Failed("rebase failed: nope".into())
        );
    }

    #[test]
    fn classify_result_ok_delegates_to_classify() {
        let result: Result<RunOutcome, anyhow::Error> = Ok(RunOutcome::Completed);
        assert_eq!(classify_result(&result), PostOp::Done);
    }
}
