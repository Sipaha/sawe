# Renaming a Solution cascade-deleted its members

**Date:** 2026-07-13
**Status:** fixed (root cause) / one gap open (see below)
**Repo:** editor (`crates/solutions`)

## Symptom (user, live)

User renamed the solution `spk-solutions` to "Sawe". After restarting the
editor the solution showed **"0 projects" / "Solution is empty"**, even
though the member directories (`sawe`, `spk-editor-mobile`) were still on
disk untouched. The in-memory session survived the rename itself — the
loss only became visible on the next cold load. Additionally, despite the
solution being empty, **a chat could still be created** from the "+" menu,
landing with cwd `ROOT` instead of a real project directory.

## Root cause

`crates/solutions/src/db.rs::save_solution` used `INSERT OR REPLACE INTO
solutions ...`. Both `solution_members` and `active_member` declare
`solution_id ... REFERENCES solutions(id) ON DELETE CASCADE`, and
`PRAGMA foreign_keys=TRUE` is set globally for every connection
(`crates/db/src/db.rs:126`, `CONNECTION_INITIALIZE_QUERY`). SQLite's
`REPLACE` conflict resolution **deletes** the conflicting parent row before
re-inserting it — that delete fires the cascade, wiping every
`solution_members` row and the `active_member` row for that solution.

`store/lifecycle.rs::rename_solution` is the only call site that re-saves
an **existing** id (every other `save_solution` call is a first-time
insert with a fresh id, so no conflict ever occurred there — which is why
this went unnoticed until a rename). The in-memory `SolutionStore` state is
untouched by the save, so the wipe is invisible until the next
`load_all_solutions_with_members` — which now returns the solution with
zero members.

## Fix

`132e89f5a7` — `save_solution` is now a real UPSERT:

```sql
INSERT INTO solutions (id, name, root, last_opened_at)
VALUES (?, ?, ?, ?)
ON CONFLICT(id) DO UPDATE SET
    name = excluded.name,
    root = excluded.root,
    last_opened_at = excluded.last_opened_at
```

updating the parent row in place instead of deleting it, so the cascade
never fires. New regression test
`db::tests::resaving_solution_preserves_members_and_active_member`
(`crates/solutions/src/db.rs:303`) reproduces the wipe against the old
`INSERT OR REPLACE` and passes against the fix. `cargo test -p solutions`
= 155 passed.

**Audit of siblings:** `set_solution_member`, `set_active_member`, and
`save_catalog_project` also use `INSERT OR REPLACE`, but their tables are
leaves — nothing references them via a foreign key — so they're safe.
`solutions` was the only parent table with cascading children.

## Data repair (user's live DB)

`~/.spk/sawe/data/db/0-stable/db.sqlite`: the 2 lost `solution_members`
rows for `spk-solutions` (`sawe` pos 0, `spk-editor-mobile` pos 1) and
`active_member = sawe` were re-inserted by hand. Backup taken first at
`/tmp/db.sqlite.bak-1783929090`. Checked every other solution's DB member
rows against its on-disk dirs — only `spk-solutions` was affected.

## Open gap — NOT fixed: empty-solution chat-creation guard is ineffective

The "can't start a chat in an empty solution" guard doesn't actually
guard. `console_panel::panel::workspace_has_worktree` (`panel.rs:63-64`):

```rust
pub fn workspace_has_worktree(workspace: &Workspace, cx: &App) -> bool {
    workspace.project().read(cx).worktrees(cx).next().is_some()
}
```

but `solutions_ui/src/open.rs:145,173` opens an empty solution's workspace
with `solution.root` as an **invisible** worktree (`OpenVisible::None`,
gated on `info.is_empty`). `worktrees()` counts invisible worktrees too —
only `visible_worktrees()` filters them out — so the guard sees a
worktree and passes, "New AI Chat" stays enabled. The session then falls
back to `solution.root` in `solution_agent/src/store.rs:840`
(`cwd.unwrap_or_else(|| solution.root.clone())`) and renders as `ROOT`.

Also: `solution_agent.create_session` (`mcp/lifecycle.rs`) has **no**
`members.is_empty()` check at all — the MCP path bypasses the UI guard
entirely.

Fix direction (not applied this session): switch the guard to
`visible_worktrees`, or gate chat creation directly on
`solution.members.is_empty()` at both the UI action and the MCP tool.

## Lessons

- Any `INSERT OR REPLACE` on a table that is an FK **parent** with `ON
  DELETE CASCADE` children is a silent data-loss bug — SQLite's REPLACE
  deletes before re-inserting, and the cascade doesn't care that the row
  is "coming right back". Use `INSERT ... ON CONFLICT(id) DO UPDATE SET`
  instead; it never deletes the row.
- DB-layer tests that only ever `INSERT` once won't catch this — the
  **re-save** path (update an existing row) needs its own explicit test,
  and that test needs child rows already seeded so the cascade would show.
