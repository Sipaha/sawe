# Push used another Solution member's repository

At 15:34:22 +0700, a Push intended for `ecos-model-enterprise` went to
`ecos-community/ecos-data.git`. The subsequent pull/rebase stopped at a conflict
in commit `204f057`. Further attempts to open Push logged only `no current branch`.

The Solution contains separate library projects. Its internal `Project` owns
worktrees for all members. `Project::active_repository` follows the last focused
buffer, while Changes and the project selector use
`solutions::active_member_repository`. Push incorrectly used the former.
It now uses the shared member resolver, including explicit nested-repository
choices; a member without a repository never falls back to another member.
The dialog names the repository alongside the branch route. A detached HEAD or
an interrupted rebase produces a visible explanation instead of a silent return.

The live `ecos-model-enterprise` worktree is clean on `master` at `599d8f2`.
A read-only `ls-remote` confirmed server `master` at `2ac90e14`, which is its
ancestor; the library is one commit ahead and does not require rebase for this
push. Diagnosis used read-only commands; the later user-approved recovery only
aborted the accidental rebase in `ecos-data`.

## User-approved recovery outside this Solution

The user explicitly approved `git rebase --abort` in
`/home/spk/.spk/sawe/ss/ui-bugs-release-26-3/ecos-data`. On 2026-10-08 at
17:01 +0700, the interrupted rebase was checked against its original master
`204f0570e8fcb4301fe575983c6c6ab53984ca5c` and `refs/heads/master` before any write.
A fresh snapshot of all eleven affected paths, index and rebase metadata was
saved inside this Solution at
`.tmp/push-recovery/ecos-data-approved-20261008-170153`. The earlier diagnosis
snapshot remains in `.tmp/push-recovery/ecos-data-before-abort`.

The approved abort restored `master` to `204f0570e8fcb4301fe575983c6c6ab53984ca5c`.
Verification confirmed a clean worktree, no unmerged index entries and no
remaining rebase metadata. The result and snapshot path are recorded in
`.tmp/push-recovery/approved-recovery-result.json`. No push of either live
library was performed during recovery. This approval covered that exact abort;
it does not authorize other changes outside `Sawe1`.

## Preview layout

The old body used a 16–20 rem height derived from at most seven files. The new
modal uses a wider 64 rem maximum and a viewport-dependent height; the body fills
the remaining space, independent of whether there are zero or one commits.
Lists scroll inside the two panes. The common 12px content inset remains.
`ChangedFileContent` supplies the complete configured UI font in both Changes
and Push, preserving their common label sizes and row spacing.

## Verification

The target-selection regression failed before the fix and passed after it.
All 441 Git UI tests passed, including explicit nested-repository choice and
no fallback from a member without Git. Native screenshots reproduced the old
mismatch (Changes showed one ahead; Push showed zero from another member).
The fixed UI pushed the intended test library to its local bare remote while
leaving the other repository's remote refs unchanged. A real conflicting rebase
in a third isolated member produced the visible diagnostic. Screenshots checked
both themes, 20 files with one commit, zero commits after push, a 768×480 modal
inside an 800×600 window, and preservation on outside click. The normal modal
is 1024×736 at 1920×1080. Evidence is in `.tmp/push-recovery`.

A pixel check of the same `DbBatchTaskAdminDao.kt` label in Push and Changes
showed 11px glyph height in both captures at the normal UI scale. File labels
keep the common Default label size; the fix does not shrink Push text separately.
