# Renaming a Solution hard-purged its open AI sessions

**Date:** 2026-07-14
**Status:** fixed
**Repo:** editor (`crates/solutions`, `crates/solution_agent`)

## Symptom (user, live)

User renamed a Solution (folder `spk-solutions` → `Sawe1`) while its window
was open. "Всё слетело." After reloading the editor the tab strip came back,
**but the AI session the user had been chatting in was gone** — not soft-closed,
gone. The editor binary was still running from the pre-rename build path
(`…/ss/spk-solutions/sawe/target/release-fast/sawe`), which is what tipped off
that a folder-moving rename of *this* Solution was the trigger.

This is the **second** data-loss bug in the rename-with-folder-move arc; the
first was the cascade wipe of members
([`2026-07-13-rename-solution-cascade-data-loss.md`](2026-07-13-rename-solution-cascade-data-loss.md)).

## Root cause

`SolutionAgentStore::on_solution_event` runs `gc_orphan_members` on **every**
`SolutionStoreEvent::Changed` (`crates/solution_agent/src/store.rs`).
`gc_orphan_members` (`store/teardown.rs`) hard-purges — `purge_session_hard`,
which deletes the DB row + transcript — any **hydrated** session whose in-memory
`cwd` is neither the solution root nor under a live member directory.

A folder-moving `rename_solution` / `rename_member`
(`crates/solutions/src/store/lifecycle.rs`, `store/members.rs`):

1. `rename(2)`s the folder and rewrites the store's `root` / member
   `local_path` to the **new** paths (+ queues one `pending_path_migration`),
2. emits `SolutionStoreEvent::Changed`.

But the **live `solution_agent` sessions are deliberately not touched** — their
`cwd` still points under the **old** root. The persisted DB cwds are only
rewritten by the cold reconcile (`path_migrations::rewrite_agent_db`) at the
*next* startup. So at the instant `Changed` fires, every open session's
(old-root) `cwd` fails both the `at_root` and `under_member` checks against the
just-updated (new-root) paths → each is classified as a false orphan and
**hard-deleted**. Every session open in the renamed Solution is lost.

Member-less "root/supervisor" sessions (cwd == solution root) are hit too: on a
move the old-root cwd no longer equals the new root.

## Fix

Give the folder move an explicit signal so the path holder can repair itself
before the GC looks.

- New event `SolutionStoreEvent::PathsMoved { id, old_prefix, new_prefix }`
  (`crates/solutions/src/store.rs`), emitted by both `rename_solution` and
  `rename_member` **before** their `Changed` (same-subscriber events deliver
  FIFO, so the repair lands first).
- `SolutionAgentStore::rewrite_session_cwds_for_move` handles it:
  - **live** hydrated sessions — rewrite the in-memory `cwd` prefix
    old→new (reusing `solutions::path_migrations::PathRewrite::apply_str`) and
    re-persist, so the `gc_orphan_members` on the trailing `Changed` sees a
    valid cwd;
  - **cold** (un-hydrated) sessions — `SolutionAgentDb::rewrite_session_cwds`
    rewrites their persisted cwd straight in the DB, so a *same-process*
    Solution reopen (`Opened → hydrate_all_for_solution → gc_orphan_members`)
    doesn't re-hydrate a stale cwd into the same purge.

The cold-reconcile DB rewrite at the next startup is unchanged and idempotent
against the hot rewrite.

## Tests

`crates/solution_agent/src/store/tests/teardown.rs`:

- `rename_solution_folder_move_keeps_open_sessions` — reproduces the loss
  (fails pre-fix: "the open session must survive a folder-moving rename"),
  asserts survival + cwd rewrite.
- `rename_member_folder_move_keeps_open_sessions` — same for the member path.
- `rewrite_session_cwds_rewrites_cold_db_rows` — the cold DB branch (also
  guards the `solution_id` TEXT-column vs numeric-id SQLite bind).

`gc_orphan_members_purges_only_removed_member_sessions` still passes — a genuine
removed-member session is still purged (member removal doesn't emit
`PathsMoved`). Full `solution_agent` (584) + `solutions` (220) +
`editor_mcp::rename_folder_move_e2e_test` green.

## Not recovered

Sessions already hard-purged before this fix are gone from the app DB. Their raw
`claude` transcripts may still exist as `~/.claude/projects/<bucket>/<acp_session_id>.jsonl`,
but the app has no row to hang them on — re-import is out of scope / speculative.
