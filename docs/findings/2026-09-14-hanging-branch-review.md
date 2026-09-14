# Branch review 2026-09-14 — every hanging local branch, both members

**Scope:** all local branches in `sawe` and `spk-editor-mobile` whose commits are
authored by Pavel Simonov. **Verdict: nothing left to merge.** Every branch is either
already in `main` content-wise (the work was rebased/squashed in, then refactored
further by later commits, which is why `git branch --merged` still hid them), or
superseded by a later phase of the same refactor, or archived on a remote. All local
branches were deleted; `main` was not touched, so no build/test run was needed —
no tree changed.

## Method (why "not merged" ≠ "unmerged work")

`git branch --no-merged main` listed 21 sawe branches, which looks alarming. It is an
ancestry check, not a content check. Three passes were used instead:

1. `git cherry -v main <branch>` — patch-id equivalence. Marked `-` (already upstream)
   for every commit on every branch except two.
2. `git merge-tree --write-tree main <branch>` — merge result identical to `main`'s tree
   ⇒ branch adds literally nothing. Clean for 10 branches; the rest reported conflicts,
   which only means `main` edited the same lines *after* absorbing the work.
3. Line-level: for each branch, every non-blank added line of `git diff main...<branch>`
   was grepped in `main`'s copy of that file. Hit rate 95–99%; every miss was manually
   traced to a later refactor in `main`, not to lost work.

## sawe — deleted (restore with `git branch <name> <sha>`)

| branch | tip | own commits | why it is gone |
|---|---|---|---|
| approval-runtime-20260911 | `75f9ae2a9a` | 4 | all patches already upstream |
| chat-scroll-anchor | `21d3be9ba5` | 3 | merge-tree no-op |
| claude-stream-steer-20260911 | `c0424c9d60` | 1 | merge-tree no-op |
| codex-native-backend | `0eef7d7e88` | 4 | all patches already upstream |
| codex-native-ui | `93af4dd003` | 1 | merge-tree no-op |
| codex-steering-20260911 | `79d917be7b` | 4 | see "two apparent exceptions" below |
| error-compaction-fix | `172e6dfcfe` | 2 | see "two apparent exceptions" below |
| live-compact-20260911 | `b4397bc179` | 1 | all patches already upstream |
| observer-headroom-20260911 | `60e1ae37cb` | 2 | merge-tree no-op |
| observer-triggers-20260911 | `ba558a34b9` | 1 | all patches already upstream |
| peer-messaging-api-20260911 | `9b573c93ac` | 2 | all patches already upstream |
| peer-messaging-delivery-20260911 | `d693cbd6a4` | 1 | all patches already upstream |
| permission-default-20260911 | `55d890e787` | 1 | merge-tree no-op |
| prompt-checks-20260911 | `2820dc0405` | 1 | all patches already upstream |
| prompt-evidence-20260911 | `93882191eb` | 1 | all patches already upstream |
| prompt-generation-20260911 | `2e3b306c34` | 1 | all patches already upstream |
| remaining-prompt-audit | `59596f6d7e` | 1 | merge-tree no-op |
| session-permissions-20260911 | `ec78080f87` | 4 | all patches already upstream |
| solution-prompt-audit | `70bfe71bc4` | 3 | all patches already upstream |
| status-bar-model-selector | `190d4cbdc4` | 905 | pre-re-fork fork; archived on the OLD remote |
| wip-phase2b-render-source | `54fdd73bda` | 1 | superseded by the phase-2c render flip |

### The two apparent exceptions

`git cherry` flagged one commit on each of these as *not* upstream. Both are false
alarms — the patch-ids differ only because the surrounding context moved:

- `codex-steering-20260911` / `3087d475` *Preserve steering receipts across handoff and
  reconnect races* — `steerable_bundles`, `rotation_steering_ready`,
  `newer_intent_stays_queued_while_handoff_is_pending`,
  `request_timeout_does_not_retry_or_keep_a_pending_receiver` and the "Agent follow-up
  delivery could not be confirmed" wording are all present in `main`.
- `error-compaction-fix` / `172e6dfc` *Handle cold slash clear locally and finish cleanup
  test turns* — `slash_clear_recovers_cold_error_without_sending_a_prompt`, the
  "Wait for the current turn to finish before clearing context" toast and the
  "Could not clear context:" toast are all in `main` (`session_view/compose.rs:149,162`).

### Line-level misses that were checked by hand

`solution-prompt-audit` and `error-compaction-fix` had the highest miss rate (14% / 4%),
all of it in shell-quoting and compaction-gate code. `main` still has every behaviour —
it moved the helpers: `quote_shell_argument` now lives in
`crates/solution_agent/src/prompt_template.rs:25` (used by `compact.rs` and
`supervisor/briefing.rs`), the ad-hoc `{key}` substitution loop became
`prompt_template::render`, and `compact_unavailable_reason` is at `compact.rs:47`.
`wip-phase2b-render-source` missed 72% of its lines because `main` went past it:
phase 2b's `main_stream_entries_for_render` doc block was rewritten by the phase-2c
render flip and phase 6d-A shell streams (`session_view.rs:434–460`).

### `status-bar-model-selector` — the 905-commit pre-re-fork line

This is the old SPK Editor fork (2026-04-25 … 2026-06-24), rooted on a different zed
line than today's `main` (merge-base `8a119a51`, 2021-03-28; 38 205 commits apart). Its
work was not merged — it was **re-implemented** by the 2026-06-24 re-fork onto zed
v1.7.2, which is why `docs/findings/2026-08-26-…-double-lease-crash.md` can blame
"the re-fork commit `3249e3f33e`". The history is not lost: the old repo
`git@github.com-sipaha:Sipaha/spk-editor.git` still holds
`refs/heads/status-bar-model-selector` at the identical tip `190d4cbdc4`
(plus `main` and `feature/unified-workspace-wire`). The local copy was redundant.

## spk-editor-mobile — deleted

All ten were already ancestors of `main` (`git branch --merged` listed every one,
`ahead=0`). Nothing to review.

`feature/banner-quiet-grace`, `feature/cli-workspace-smoke`,
`feature/unified-workspace-mobile`, `feature/workspace-compact-rows`,
`feature/workspace-new-console-in-kebab`, `fix/create-solution-auto-open`,
`fix/drop-create-chain`, `fix/reconnect-loop`, `phase6-mobile-delta-sync`,
`refactor/banner-under-header`.

Two of them also existed on `origin` (`feature/unified-workspace-mobile` `d8d73bb45d`,
`phase6-mobile-delta-sync` `32a96bfed8`), both already merged into `origin/main`. Deleted
on the maintainer's go-ahead — `Sipaha/sawe-mobile` now carries only `main`.

## Agent worktrees — also removed

The five `worktree-agent-*` branches (`.agents/worktrees/sawe/agent-*`, 2026-08-12) were
fully merged with zero own commits, but each was checked out in a live registered
worktree. Removed with `git worktree remove --force` on the maintainer's go-ahead,
discarding four dirty `Cargo.lock`s and a 7-line `SAWE_GUTTER_DEBUG` `log::info!` scratch
patch in `crates/editor/src/editor.rs` (throwaway gutter-measurement tracing, nothing to
salvage). `.agents/worktrees/sawe/` is now empty and `git worktree list` shows only the
main checkout.

Tips, if a worktree branch is ever wanted back: `a5fafb7c24`, `610837611c`,
`e6fe9c2b48`, `c70d5061dc`, `6cfbb5dbc5`.

## Left standing

- Uncommitted working-tree changes in `spk-editor-mobile` (the `NewSessionDialog`
  snapshot-test rework). Out of scope for a branch review.
- `sawe`'s untracked `.agents/reports/*` artefacts were deleted, and the 51 that had
  been committed there by accident were removed from the repo as well — see the
  `.gitignore` entry for the commit they stay retrievable at.

Both members now have exactly one local branch: `main`.
