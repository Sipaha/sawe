# Two ways a mutation-testing harness lies about its own results

**Date:** 2026-09-02 · **Status:** confirmed · **Applies to:** any script that mutates a source file, rebuilds, tests, restores

Both of these were hit while mutation-testing `git_ui` / `search`
(`docs/plans/2026-09-02-unify-the-two-diff-views.md`), and both produced *convincing* phantom
results — a clean-looking table of survivors and kills that described a tree and a binary
that were not what the harness thought they were.

**1. `shutil.move` restores the backup's mtime, so cargo reuses the mutant binary.**
`shutil.move` (and `mv`, and `shutil.copy2`) preserve the source file's modification time.
The `.bak` was written *before* the mutation, so the restored file is **older** than the
mutant object file cargo already built. Cargo's fingerprint is mtime-based: it sees nothing
newer than the artifact and does not rebuild. Every test run after the first restore then
exercises the mutant. Fix: `touch` — `os.utime(path, None)` — every file the restore loop
writes, or use `shutil.copyfile` into the existing inode and stamp it.

**2. A two-edit mutation on one file overwrites its own `.bak`.**
A harness that backs up per *edit* rather than per *file* writes `foo.rs.bak` for edit 1,
then re-reads the already-mutated `foo.rs` and writes it over `foo.rs.bak` for edit 2. The
restore loop then restores the mutant, and the working tree stays mutated after the run
reports "restored". Fix: key the backup map by path, back up only on first touch, and
de-duplicate paths in the restore loop.

Cheap detection for both, worth adding to any such harness as an assertion rather than
trusting the loop: after restoring, `git diff --quiet` must succeed, and the rebuild must
actually recompile (a build that finishes in fingerprint-check time after a source change is
the tell — the same signal as `script/run-mcp` launching a stale binary).
