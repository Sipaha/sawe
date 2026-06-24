//! S-SAR — open the state of a repository at a specific commit in a
//! read-only snapshot worktree, in a brand-new top-level workspace
//! window.
//!
//! Architecture:
//! 1. Pre-check: refuse if the source repo is bare (`git worktree add`
//!    semantics on a bare repo are awkward; the spec calls for an
//!    explicit "Source is a bare repository" error rather than a
//!    surprise mid-operation failure).
//! 2. Pick a target dir under [`paths::temp_dir()`]`/worktrees/<repo>-at-<short-sha>-<rand>`.
//!    The 8-hex random suffix prevents path collision when the same
//!    commit is opened twice.
//! 3. `git worktree add --detach <target> <sha>` against the source repo.
//! 4. Drop a `.spke-readonly.json` marker at the target root with
//!    `{ base_sha, branch_template, created_at_unix, source_repo }`.
//!    The `Project` constructor's `WorktreeAdded` hook reads this
//!    marker and registers the path in `Project::read_only_roots`.
//! 5. Open the target as a new top-level workspace via
//!    [`workspace::open_paths`] with `OpenMode::NewWindow`. The new
//!    window does **not** inherit Solution membership: the snapshot
//!    is always a single-project window per the S-SAR spec — there is
//!    no "Solution at revision" concept.
//!
//! Cleanup is handled in two places:
//! - On window close: `crate::handlers::show_at_revision::cleanup_for_workspace`
//!   runs `git worktree remove --force` against the source repo (best
//!   effort — orphan cleanup at startup catches anything left behind).
//! - At editor startup: [`cleanup_orphan_worktrees`] walks the
//!   worktrees dir and removes anything older than the configured
//!   threshold.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AppContext as _, Context, Entity, Task, TaskExt as _, WindowHandle};
use project::git_store::Repository;
use rand::Rng;
use serde::{Deserialize, Serialize};
use util::ResultExt as _;
use workspace::{MultiWorkspace, OpenMode, OpenOptions, Workspace, WorkspaceMatching};

const WORKTREES_SUBDIR: &str = "worktrees";
const SHORT_SHA_LEN: usize = 7;
const RANDOM_SUFFIX_LEN: usize = 8;
/// Default age threshold (hours) for orphan-worktree cleanup at startup.
/// Mirrored in `crates/settings_content` as
/// `git.show_at_revision.cleanup_orphans_older_than_h`.
pub const DEFAULT_CLEANUP_ORPHANS_OLDER_THAN_H: u32 = 24;

/// On-disk marker dropped at the root of every snapshot worktree.
/// `Project::on_worktree_added` reads it back to recognise the worktree
/// as read-only.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadOnlyMarker {
    /// SHA the worktree was checked out at.
    pub base_sha: String,
    /// Default branch name used by the "Create Branch and Open
    /// Writable" affordance. Stored as a template so the modal can
    /// pre-fill an editable name (e.g. `snapshot/<short-sha>`).
    pub branch_template: String,
    /// Unix timestamp the worktree was created at — read by
    /// [`cleanup_orphan_worktrees`] to age out stale dirs from
    /// previous editor runs that crashed before close-cleanup ran.
    pub created_at_unix: u64,
    /// Absolute path of the source repository the snapshot was
    /// branched from. The on-close handler runs `git worktree remove`
    /// here, and "Create Branch and Open Writable" runs
    /// `git branch <name> <sha>` here.
    pub source_repo: PathBuf,
}

/// Open `sha` as a read-only snapshot in a new top-level workspace
/// window. Always opens a single-project window — never propagates
/// Solution context, even if invoked from a Solution-wide log.
///
/// Returns the new window handle (or an error if pre-checks fail or
/// `git worktree add` errors out). The caller is expected to hand the
/// task off via `.detach_and_log_err(...)` or surface the error
/// through the standard toast mechanism.
pub fn show_at_revision(
    workspace: &mut Workspace,
    repo: Entity<Repository>,
    sha: String,
    _window: &mut gpui::Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<WindowHandle<MultiWorkspace>>> {
    let app_state = workspace.app_state().clone();
    let source_repo_path = repo.read(cx).work_directory_abs_path.to_path_buf();
    // The re-fork's `Repository` no longer carries the donor's
    // `original_repo_abs_path` field; for a normal checkout it equalled
    // `work_directory_abs_path`, which is all we need to derive the
    // snapshot's display name.
    let original_repo_path = source_repo_path.clone();

    cx.spawn(async move |_workspace_handle, cx| {
        // Pre-check: bare-repo source. `git worktree add` against a
        // bare clone does work in some configurations, but the spec
        // explicitly calls for a reject + clear error so the
        // context-menu item can stay disabled-with-tooltip in the bare
        // case. We approximate "is bare" by checking whether the
        // source's `.git` is a directory: a normal checkout has
        // `<repo>/.git/`, a bare clone has the dot-git contents
        // directly at the root and no `.git` entry under it.
        if !source_repo_is_normal(&source_repo_path) {
            return Err(anyhow!(
                "Source is a bare repository, cannot create snapshot worktree."
            ));
        }

        let repo_name = original_repo_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| sanitise_repo_name(s))
            .unwrap_or_else(|| "repo".to_string());

        let short_sha: String = sha.chars().take(SHORT_SHA_LEN).collect();
        let random_suffix = generate_hex_suffix();
        let dir_name = format!("{repo_name}-at-{short_sha}-{random_suffix}");
        let target = paths::temp_dir().join(WORKTREES_SUBDIR).join(&dir_name);

        std::fs::create_dir_all(target.parent().unwrap_or(target.as_path())).with_context(
            || {
                format!(
                    "creating snapshot worktree parent {}",
                    target.parent().unwrap_or(target.as_path()).display()
                )
            },
        )?;

        // Run `git worktree add --detach <target> <sha>`. We invoke
        // `git` directly (rather than going through
        // `Repository::create_worktree`) so the new worktree path is
        // not adopted into the source project's worktree settings —
        // it's a transient snapshot, not part of the user's worktree
        // catalogue.
        let target_for_add = target.clone();
        let sha_for_add = sha.clone();
        let source_for_add = source_repo_path.clone();
        cx.background_spawn(async move {
            run_git_worktree_add(&source_for_add, &target_for_add, &sha_for_add).await
        })
        .await?;

        // Write the marker. If this fails we still try to roll back
        // the worktree — leaving an unmarked snapshot worktree behind
        // would mean the new window opens read-write, defeating the
        // whole point.
        let now_unix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let marker = ReadOnlyMarker {
            base_sha: sha.clone(),
            branch_template: format!("snapshot/{short_sha}"),
            created_at_unix: now_unix,
            source_repo: source_repo_path.clone(),
        };
        if let Err(err) = write_marker(&target, &marker) {
            let source_for_rollback = source_repo_path.clone();
            let target_for_rollback = target.clone();
            cx.background_spawn(async move {
                run_git_worktree_remove(&source_for_rollback, &target_for_rollback)
                    .await
                    .log_err();
            })
            .detach();
            return Err(err.context("writing .spke-readonly.json marker"));
        }

        // Open the snapshot as a new top-level window. We use
        // `OpenMode::NewWindow` + `WorkspaceMatching::None` so it
        // doesn't try to dock into the active MultiWorkspace's
        // sidebar — Solution context should never leak into a
        // snapshot window.
        let paths_to_open = vec![target.clone()];
        let open_task = cx.update(|cx| {
            workspace::open_paths(
                &paths_to_open,
                app_state.clone(),
                OpenOptions {
                    open_mode: OpenMode::NewWindow,
                    workspace_matching: WorkspaceMatching::None,
                    focus: Some(true),
                    ..Default::default()
                },
                cx,
            )
        });
        let open_result = open_task.await.with_context(|| {
            format!("opening snapshot worktree window for {}", target.display())
        })?;

        // Hook cleanup-on-close. `observe_release` on the new
        // workspace's Project fires once the workspace is dropped
        // (i.e. the user closed the window or quit the editor).
        // Best-effort: if the editor crashes before this fires,
        // `cleanup_orphan_worktrees` at next startup catches it.
        let target_for_cleanup = target.clone();
        let workspace_entity = open_result.workspace.clone();
        cx.update(|cx| {
            workspace_entity
                .read(cx)
                .project()
                .clone()
                .update(cx, |_project, cx| {
                    cx.on_release(move |_project, cx| {
                        cleanup_for_worktree_path(target_for_cleanup, cx);
                    })
                    .detach();
                });
        });

        Ok(open_result.window)
    })
}

/// Best-effort cleanup of the snapshot worktree backing `worktree_path`
/// when its workspace window is closing. Reads the marker to find the
/// source repo, then runs `git worktree remove --force`. Failures are
/// logged — the orphan-cleanup-at-startup pass catches anything we
/// miss here.
pub fn cleanup_for_worktree_path(worktree_path: PathBuf, cx: &mut App) {
    let marker_path = worktree_path.join(project::READ_ONLY_MARKER_FILE);
    cx.background_spawn(async move {
        let marker = match read_marker(&marker_path) {
            Ok(m) => m,
            Err(err) => {
                log::warn!(
                    "show_at_revision::cleanup: cannot read marker {}: {err}",
                    marker_path.display()
                );
                // No marker -> not our worktree. Bail.
                return;
            }
        };
        if let Err(err) = run_git_worktree_remove(&marker.source_repo, &worktree_path).await {
            log::warn!(
                "show_at_revision::cleanup: git worktree remove failed for {}: {err}",
                worktree_path.display()
            );
            // Last-ditch: blow the directory away so it doesn't come back
            // as an orphan. Git will still have stale admin metadata in
            // `.git/worktrees/<name>/` on the source repo; the user can
            // run `git worktree prune` themselves.
            std::fs::remove_dir_all(&worktree_path).log_err();
        }
    })
    .detach();
}

/// Walk `<temp_dir>/worktrees/` at editor startup and remove any
/// snapshot worktree dir whose `.spke-readonly.json` marker is older
/// than `older_than_hours` hours. Best-effort: every failure is
/// logged, none aborts the scan.
pub fn cleanup_orphan_worktrees(older_than_hours: u32) {
    let root = paths::temp_dir().join(WORKTREES_SUBDIR);
    cleanup_orphan_worktrees_in(&root, older_than_hours);
}

/// Test seam for [`cleanup_orphan_worktrees`].
pub fn cleanup_orphan_worktrees_in(root: &Path, older_than_hours: u32) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!("show_at_revision: cannot scan {}: {err}", root.display());
            }
            return;
        }
    };
    let now_unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff_secs = u64::from(older_than_hours) * 3600;
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.metadata().map(|m| m.is_dir()).unwrap_or(false) {
            continue;
        }
        let marker_path = path.join(project::READ_ONLY_MARKER_FILE);
        let marker = match read_marker(&marker_path) {
            Ok(m) => m,
            Err(_) => {
                // No marker — could be a half-created dir or someone
                // else's content. Leave it alone unless its mtime is
                // also old, which would suggest a half-failed attempt.
                if let Ok(metadata) = std::fs::metadata(&path)
                    && let Ok(mtime) = metadata.modified()
                    && let Ok(age) = SystemTime::now().duration_since(mtime)
                    && age.as_secs() > cutoff_secs
                {
                    std::fs::remove_dir_all(&path).log_err();
                }
                continue;
            }
        };
        if now_unix.saturating_sub(marker.created_at_unix) <= cutoff_secs {
            continue;
        }
        // Drop the working directory. We deliberately don't shell out
        // to `git worktree remove` here — orphan cleanup runs
        // synchronously at editor startup before the smol runtime is
        // wired in (mirrors `git::operations::rebase::cleanup_orphan_sessions`),
        // and the source repo's `.git/worktrees/<name>/` admin entry is
        // harmless on its own (`git worktree prune` reaps it whenever
        // the user next runs git). The on-close handler does the
        // proper `git worktree remove --force` for the common path; this
        // routine only catches the post-crash leftover case.
        std::fs::remove_dir_all(&path).log_err();
        // Avoid the unused-variable warning for the marker we still
        // had to read for the TTL check.
        let _ = marker.source_repo;
    }
}

/// Workspace action handler for `crate::fork_actions::ShowAtRevision { sha }`. Picks
/// the workspace's active repository and dispatches to
/// [`show_at_revision`].
pub fn show_at_revision_action(
    workspace: &mut Workspace,
    sha: String,
    window: &mut gpui::Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let Some(repo) = project.read(cx).active_repository(cx) else {
        log::warn!("crate::fork_actions::ShowAtRevision: no active repository");
        return;
    };
    show_at_revision(workspace, repo, sha, window, cx).detach_and_log_err(cx);
}

const DOT_GIT: &str = ".git";

/// Whether the source repo is a normal (non-bare) checkout, i.e. has a
/// `.git/` directory at its root. Bare clones store the contents of
/// `.git` directly and have no `<repo>/.git` entry — `git worktree add`
/// against them needs a different invocation pattern that's not worth
/// implementing for the v1 of this feature, so we reject up front with
/// a clear error.
pub fn source_repo_is_normal(source: &Path) -> bool {
    let dot_git = source.join(DOT_GIT);
    std::fs::metadata(&dot_git)
        .map(|m| m.is_dir())
        .unwrap_or(false)
}

fn sanitise_repo_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn generate_hex_suffix() -> String {
    let mut rng = rand::rng();
    let mut s = String::with_capacity(RANDOM_SUFFIX_LEN);
    for _ in 0..RANDOM_SUFFIX_LEN {
        let nibble: u8 = rng.random_range(0..16);
        s.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
    }
    s
}

fn write_marker(target: &Path, marker: &ReadOnlyMarker) -> Result<()> {
    let marker_path = target.join(project::READ_ONLY_MARKER_FILE);
    let json = serde_json::to_vec_pretty(marker)?;
    std::fs::write(&marker_path, json)
        .with_context(|| format!("writing {}", marker_path.display()))?;
    Ok(())
}

fn read_marker(marker_path: &Path) -> Result<ReadOnlyMarker> {
    let bytes =
        std::fs::read(marker_path).with_context(|| format!("reading {}", marker_path.display()))?;
    let marker: ReadOnlyMarker = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", marker_path.display()))?;
    Ok(marker)
}

async fn run_git_worktree_add(source: &Path, target: &Path, sha: &str) -> Result<()> {
    let output = util::command::new_command("git")
        .arg("-C")
        .arg(source)
        .args(["worktree", "add", "--detach"])
        .arg(target)
        .arg(sha)
        .output()
        .await
        .context("spawning git worktree add")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("git worktree add failed: {stderr}"));
    }
    Ok(())
}

async fn run_git_worktree_remove(source: &Path, target: &Path) -> Result<()> {
    let output = util::command::new_command("git")
        .arg("-C")
        .arg(source)
        .args(["worktree", "remove", "--force"])
        .arg(target)
        .output()
        .await
        .context("spawning git worktree remove")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("git worktree remove failed: {stderr}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn cleanup_removes_only_old_worktrees() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        // Old: created 25h ago.
        let old_dir = root.join("repo-at-aaaaaaa-deadbeef");
        fs::create_dir_all(&old_dir).expect("create old");
        let old_marker = ReadOnlyMarker {
            base_sha: "a".repeat(40),
            branch_template: "snapshot/aaaaaaa".into(),
            created_at_unix: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .saturating_sub(25 * 3600),
            source_repo: PathBuf::from("/nonexistent-source-repo-for-test"),
        };
        write_marker(&old_dir, &old_marker).expect("write old marker");

        // Young: created 1h ago.
        let young_dir = root.join("repo-at-bbbbbbb-cafef00d");
        fs::create_dir_all(&young_dir).expect("create young");
        let young_marker = ReadOnlyMarker {
            base_sha: "b".repeat(40),
            branch_template: "snapshot/bbbbbbb".into(),
            created_at_unix: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .saturating_sub(3600),
            source_repo: PathBuf::from("/nonexistent-source-repo-for-test"),
        };
        write_marker(&young_dir, &young_marker).expect("write young marker");

        cleanup_orphan_worktrees_in(root, 24);

        assert!(!old_dir.exists(), "old dir should be cleaned up");
        assert!(young_dir.exists(), "young dir should be retained");
    }

    #[test]
    fn marker_round_trip_preserves_fields() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let original = ReadOnlyMarker {
            base_sha: "deadbeef".repeat(5),
            branch_template: "snapshot/dead".into(),
            created_at_unix: 1_700_000_000,
            source_repo: PathBuf::from("/path/to/source"),
        };
        write_marker(tmp.path(), &original).expect("write");
        let parsed = read_marker(&tmp.path().join(project::READ_ONLY_MARKER_FILE)).expect("read");
        assert_eq!(parsed.base_sha, original.base_sha);
        assert_eq!(parsed.branch_template, original.branch_template);
        assert_eq!(parsed.created_at_unix, original.created_at_unix);
        assert_eq!(parsed.source_repo, original.source_repo);
    }

    #[test]
    fn sanitise_repo_name_strips_path_separators() {
        assert_eq!(sanitise_repo_name("foo bar"), "foo_bar");
        assert_eq!(sanitise_repo_name("a/b\\c"), "a_b_c");
        assert_eq!(sanitise_repo_name("ok.repo-1_x"), "ok.repo-1_x");
    }

    #[test]
    fn hex_suffix_is_correct_length() {
        let suffix = generate_hex_suffix();
        assert_eq!(suffix.len(), RANDOM_SUFFIX_LEN);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Bare-repo rejection: source_repo_is_normal returns false when
    /// there's no `.git` directory at the repo root.
    #[test]
    fn bare_repo_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Bare clone shape: contents directly at root, no `.git/`
        // subdirectory.
        fs::create_dir_all(tmp.path().join("objects")).expect("objects");
        fs::create_dir_all(tmp.path().join("refs")).expect("refs");
        fs::write(tmp.path().join("HEAD"), b"ref: refs/heads/main\n").expect("HEAD");
        assert!(!source_repo_is_normal(tmp.path()));
    }

    /// Mirror: a normal checkout (one with `<repo>/.git/`) is accepted.
    #[test]
    fn normal_repo_is_accepted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join(".git")).expect("create .git");
        assert!(source_repo_is_normal(tmp.path()));
    }

    /// Cleanup ignores subdirectories that don't carry a marker, but
    /// will scrub them if they're also stale on the filesystem (older
    /// than the cutoff). This guards against a half-failed `worktree
    /// add` leaving an unrecoverable directory.
    #[test]
    fn cleanup_preserves_unmarked_recent_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("unmarked-recent");
        fs::create_dir_all(&dir).expect("create");
        // No marker — and mtime is current. Should be left alone.
        cleanup_orphan_worktrees_in(tmp.path(), 24);
        assert!(dir.exists());
    }
}
