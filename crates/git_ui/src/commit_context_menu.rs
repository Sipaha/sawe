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

use crate::handlers::{
    branch, checkout, cherry_pick, compare, copy, drop as drop_handler, edit_message, fixup, merge,
    patch as patch_handler, reset, revert, show_at_revision, squash, tag,
};
use editor::Editor;
use gpui::{
    App, ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, WeakEntity, Window,
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
    /// Local branch names known to the repository. Used to tell a local
    /// branch token in [`Self::refs`] apart from a remote-tracking ref
    /// like `origin/feature` — both are slash-bearing in `%D`.
    pub local_branches: Vec<SharedString>,
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
//  Parses the commit's `%D` ref decorations into local branches and
//  tags, then exposes per-ref submenus: branches get Checkout / Merge
//  into <head> / Delete; tags get Checkout / Delete. Remote-tracking
//  refs are intentionally omitted — operating on them from here would be
//  surprising. The head branch is listed but shown as a non-actionable
//  label (you can't checkout / merge / `git branch -d` the current
//  branch).
// =====================================================================

struct RefsAtCommit {
    branches: Vec<SharedString>,
    tags: Vec<SharedString>,
}

fn refs_at_commit(ctx: &CommitContext) -> RefsAtCommit {
    let mut branches: Vec<SharedString> = Vec::new();
    let mut tags: Vec<SharedString> = Vec::new();
    for token in &ctx.refs {
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
        // `%D` lists local branches as bare names and remote-tracking refs
        // as `<remote>/<branch>` — both are slash-bearing when the local
        // branch name itself contains a `/` (GitFlow-style). Disambiguate
        // against the repository's known local-branch set.
        let is_local = !name.contains('/') || ctx.local_branches.iter().any(|b| b.as_ref() == name);
        if !is_local {
            continue;
        }
        if !branches.iter().any(|b| b.as_ref() == name) {
            branches.push(name.to_string().into());
        }
    }
    RefsAtCommit { branches, tags }
}

fn build_branch_tag_section(menu: ContextMenu, ctx: CommitContext) -> ContextMenu {
    let RefsAtCommit { branches, tags } = refs_at_commit(&ctx);
    if branches.is_empty() && tags.is_empty() {
        return menu;
    }
    let mut menu = menu.separator();
    if !branches.is_empty() {
        menu = menu.header("Branches at This Commit");
        for branch in branches {
            let entry_ctx = ctx.clone();
            menu = menu.submenu_with_icon(
                branch.clone(),
                IconName::GitBranch,
                move |submenu, _window, _cx| {
                    build_branch_ref_submenu(submenu, entry_ctx.clone(), branch.clone())
                },
            );
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

fn build_branch_ref_submenu(
    menu: ContextMenu,
    ctx: CommitContext,
    branch: SharedString,
) -> ContextMenu {
    let is_head = ctx
        .head_branch
        .as_ref()
        .is_some_and(|h| h.as_ref() == branch.as_ref());
    if is_head {
        return menu.label("Currently checked out");
    }

    let menu = {
        let repo = ctx.repository.clone();
        let branch = branch.clone();
        menu.entry("Checkout", None, move |window, cx| {
            run_checkout_branch(repo.clone(), branch.clone(), window, cx);
        })
    };

    let menu = if let Some(head) = ctx.head_branch.clone() {
        let merge_ctx = ctx.clone();
        let target = branch.clone();
        menu.entry(format!("Merge into {head}"), None, move |window, cx| {
            let Some(work_dir) = repo_work_dir(&merge_ctx, cx) else {
                return;
            };
            run_merge_branch(work_dir, target.clone(), window, cx);
        })
    } else {
        menu
    };

    let repo = ctx.repository;
    menu.entry("Delete", None, move |window, cx| {
        run_delete_branch(repo.clone(), branch.clone(), window, cx);
    })
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

fn run_delete_branch(
    repo: Entity<Repository>,
    branch: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let recv = repo.update(cx, |repo, _| {
        repo.delete_branch(false, branch.to_string(), false)
    });
    await_repo_recv(
        recv,
        "delete branch was canceled",
        "Delete branch failed",
        window,
        cx,
    );
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
    window.dispatch_action(Box::new(crate::fork_actions::InteractiveRebaseFromHere { sha }), cx);
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
