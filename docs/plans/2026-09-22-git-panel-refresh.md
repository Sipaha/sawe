# Git panel refresh correctness

**Status:** complete

## Goal
Keep Changes, Commit and the history graph current after external Git operations and Solution repository changes.

## Evidence
The UI can track the active Solution member while GitStore tracks the last focused buffer. Filtering status events by GitStore's active flag misses updates. Commit message loading starts before the panel resolves its repository. Containing branches are loaded once. The graph ignores repository discovery/removal, and local repository removal emits no removal event. Partial status refreshes remove exact paths but can query entire directories.

## Scope
Fix these event routing and asynchronous refresh boundaries in git_ui, git_graph, project, git, fs and worktree. Track tag object IDs and register loose-ref directory watches, including newly-created namespaces.

## Acceptance
- Changes responds to its displayed repository even when the GitStore active repository differs.
- Commit message buffers follow repository switches and discard stale loads.
- Containing branches refresh after ref changes without resetting commit content.
- Repository discovery/removal updates the graph and respects members with no repository.
- Directory status refreshes clear clean descendants and preserve unrelated statuses.

- Native non-recursive watchers detect standalone external ref changes and newly-created ref namespaces.

## Verification
Run regression tests plus affected crate suites, build the editor, drive an isolated headless editor and inspect screenshots. Build release-fast for the user's next launch.

## Documentation
Record confirmed mechanisms and verification here and in FORK.md. Keep unrelated work untouched.

## Verification notes

The first strict Clippy run hit one redundant clone in a new test (removed)
and three existing findings in `project/src/lsp_workspace_cache.rs` (redundant
clones / cloned references used as single-element slices). These unrelated
findings prevent a clean `--deny warnings` gate. A follow-up warning-mode run
checks all changed crates without broadening this fix into unrelated cleanup.

The native UI probe additionally exposed missing Linux loose-ref watches.
Regression coverage includes existing refs, nested namespaces created later,
and deletion/recreation at the same directory path. FakeFs alone cannot prove
native non-recursive watch registration.

Native fixtures must live outside any member's Git checkout. Setting TMPDIR to
`sawe/target` makes ancestor discovery and inherited ignore rules contaminate
unrelated integration cases. The verified run uses the Solution-local
`../.git-refresh-test-temp` directory (outside both member repositories).

## Test results

`cargo test -p git_ui -p git_graph -p project -p git -p worktree --lib --tests --no-fail-fast`
with the isolated TMPDIR completed successfully: **1046 passed, 0 failed,
4 ignored**. This includes all 439 git_ui tests, 100 git_graph tests,
project's 56 unit and 258 integration tests, and worktree's native watch test.
The standalone native watcher regression also passed (3.08 s).

The removal/restore UI probe also reproduced a renamed `.git` directory being
ignored as a bare `Changed` event. That suppression now applies only to known
repositories; newly discovered repositories renew metadata-root watches.
The native regression includes moving `.git` out and back and observing a
subsequent HEAD edit. This follow-up is checked after the full-suite result
above.

The expanded native test passed after that follow-up (3.77 s), and all
**56 worktree integration tests passed again** (37.89 s). Clippy over the
six affected packages completed with lints capped at warnings: only the three
pre-existing `lsp_workspace_cache.rs` warnings remained. The final worktree
follow-up passed its scoped Clippy check with no warnings.

## Native UI verification

A fresh headless debug editor used two isolated member repositories. Screenshots
confirmed external file restore clearing Changes, member switching retargeting
Changes and history and closing the previous Commit tab, and standalone
`git tag -f` moving the tag chip in both history and Commit without Refresh.
Creating a descendant branch refreshed the selected ancestor's containing-branch
line. Removing the active member's `.git` showed no repository rather than the
other member's history. After the final filter fix, renaming `.git` back restored
both panels automatically; the resulting screenshot was inspected.

Local evidence is retained under `target/git-refresh-*.png` and build/test logs
under `target/git-refresh-*.log`. These generated probe artifacts are not shipped.
The final debug build completed successfully in 2m 45s.

The final `cargo build --bin sawe --profile release-fast` completed successfully
(20m 07s). Both debug and release-fast binaries contain the final fixes. The
maintainer's running editor was left open; the updated binary takes effect on
its next launch.

## Upstream comparison (2026-09-22)

Compared the current upstream main at
[`b54cc1d0acc8fe3f7581721ee1195516e7581f9d`](https://github.com/zed-industries/zed/commit/b54cc1d0acc8fe3f7581721ee1195516e7581f9d)
and the latest stable release
[`v1.20.2`](https://github.com/zed-industries/zed/releases/tag/v1.20.2)
(published September 17) against the inherited v1.7.2 code. This was a source
and commit-history audit, not an execution of upstream Zed's tests or UI.

The following fixes are present in both current main and v1.20.2:

| Inherited mechanism | Upstream change |
|---|---|
| Commit buffer opened before updating the active repository; detached stale loads | [June 27, bfe0d7c8f696 / #58180](https://github.com/zed-industries/zed/commit/bfe0d7c8f696669dded40df535840621d4756a88): resolve the repository first and retain a cancellable reopen task. |
| Partial directory refresh retains clean descendants | [June 29, 33473c1cd3b6 / #59934](https://github.com/zed-industries/zed/commit/33473c1cd3b6555145e12e39590c6790077088a1): reconcile old statuses below each queried prefix. |
| Native non-recursive watchers miss loose refs | [July 10, 2b9b3c7ea212 / #60660](https://github.com/zed-industries/zed/commit/2b9b3c7ea21293c9f271d993503e6d8dba2c6891): watch refs directories, including newly-created namespaces. |
| Bare .git Changed events are unconditionally discarded | [July 23, 137c981cb03b / #59876](https://github.com/zed-industries/zed/commit/137c981cb03b38034ce727f4055f7423c051627f): reload Git state for standalone bare events; #61636 subsequently excludes events explained by ignored siblings. |

The local `for id in removed_ids` path still removes repository entities without
emitting `RepositoryRemoved` in both inspected versions (main git_store.rs,
lines 2723-2735). The remaining emitter is in the remote removal handler.
That establishes the code-level omission, not that current upstream UI has the
same visible failure as our Solution-scoped panels.

Our Solution-member routing, separate Commit tab, and tag-name-only invalidation
were fork-specific changes. Their defects must not be attributed to current
upstream. No upstream changes were merged or cherry-picked during this audit.
