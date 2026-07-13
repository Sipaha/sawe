# Session handoff — 2026-07-13 (rename with folder move)

**Status: the three-plan rename arc is IMPLEMENTED, verified end-to-end and
pushed to `origin/main`.** The session started from a data-loss bug (renaming a
Solution cascade-deleted its members), turned into a design + planning run for
"renaming a Solution or a project also renames its folder", and then shipped all
three plans. What remains is a short list of follow-ups, none of them blocking.

Full write-up:
[`2026-07-13-rename-with-folder-move-shipped.md`](2026-07-13-rename-with-folder-move-shipped.md).

## Commit chain (all on `origin/main`)

| Commits | What |
|---|---|
| `132e89f5a7` | `solutions`: `save_solution` is a real UPSERT — rename no longer cascade-deletes members |
| `f96c85defe` | finding: `2026-07-13-rename-solution-cascade-data-loss.md` |
| `31f4f1fde2` | `solution_agent`: stuck-session watchdog no longer kills a parent waiting on background agents |
| `d61e2c41b0` `ccda012864` `d8f84951dd` `2854fa4c9a` | the three plan docs |
| `a81166f241` `b22518b815` `f573212060` `9c3e0e9d1e` `0a1407c8a3` `defd92e20b` `a1917ea87c` `03ec37164d` | **plan 1 — identity model** |
| `37763a1897` `5a3e92e403` `8012b06d8d` `107d47a40f` `27110814b5` `0a7843b2d4` `5a010d842d` `b60545b59b` `22e071d5ce` `eb3eb809f4` `5030f40efc` `5e2b6d661c` | **plan 2 — rename moves the folder** |
| `47c38dbca7` `4573ee11b3` `307eea9bde` `710361bb66` `8a60552b22` `7ad284513f` `8b21dc3fdd` `69a83c8918` `16d9f9295d` `399eb2e471` `781b484308` `48a5b7b04c` `6ec94dbcb1` | **plan 3 — claude settings + sockets** |

## What shipped

1. **Data-loss fix.** `save_solution` used `INSERT OR REPLACE` on `solutions`,
   whose children (`solution_members`, `active_member`) are `ON DELETE CASCADE`
   with `PRAGMA foreign_keys=TRUE` — the REPLACE deleted the parent row and
   cascaded the members away, invisible until the next app start. Now an
   `ON CONFLICT(id) DO UPDATE`.

2. **Watchdog false positive.** `tick_stuck_sessions` treated "Running, silent
   300s, no in-progress tool" as a hang, which is exactly the shape of a parent
   awaiting background agents. `turn_is_wedged` now takes `background_alive`. A
   hung *foreground* tool is still recovered.

3. **Plan 1 — identity model.** `SolutionId(i64)` / `MemberId(i64)` /
   `CatalogId(i64)` surrogate counters replace slug ids; catalog entries became
   templates; `solution_sessions.member_id` replaces cwd-inference; per-solution
   MCP socket dirs are keyed by the numeric id. Cross-DB migration
   (`solution_legacy_ids`) remaps `solution_agent.db`'s slugs, rehearsed against
   the operator's real databases (16 solutions / 43 members / 14 active / 47
   catalog in → same out; 176 sessions preserved, 169 remapped, 70 `member_id`s
   backfilled, 4 slugs of long-deleted solutions left unmapped, 0 dangling
   `active_member` rows).

4. **Plan 2 — rename moves the folder.** Hot half: Unicode folder-name
   derivation, collision + same-filesystem check (cross-device = hard error),
   `rename(2)`, compat symlink, `pending_path_migrations` row. Cold half
   (`path_migrations::drain_and_apply`, before any window opens): rewrite every
   path-bearing row in the app + agent DBs, move/merge the claude transcript
   bucket, repair git worktrees, remove the symlink, delete the row —
   idempotent and crash-safe. `workspaces.paths` is rewritten **in place**, so
   the window keeps its `workspace_id` and its panes/tabs/docks. Verified live:
   `btest` → `Проект Тест` moved the directory and drained cleanly on restart.
   New `solutions.rename_member` MCP tool + rename-member modal.

5. **Plan 3 — claude settings + sockets.** Sockets / lock / upload spool moved
   `config/` → `state_dir()` (`~/.spk/sawe[-dev]/state/`) with a one-time sweep
   of the old location. The editor owns a claude settings layer
   (`--settings <file>`) whose `WorktreeCreate` / `WorktreeRemove` hooks are the
   running binary itself, so agent worktrees land in
   `<solution_root>/.agents/worktrees/` and auto memory in
   `<solution_root>/.agents/memory` (verified with a real claude subagent).
   `solution_id` is optional in solution-scoped MCP tools — injected and
   overridden by the per-solution socket, a clear error on the global socket.

**Operator action:** the real databases migrate **on the next launch of the new
binary**. Pre-migration backups: `/tmp/db.sqlite.pre-identity-1783948879` and
`/tmp/solution_agent.db.pre-identity-1783948879`.

## Outstanding pool

- `file_folds` (editor fold persistence, keyed by path) is **not** in
  `path_migrations::rewrite_app_db` — a folded region is orphaned by a rename.
  Low impact, one row in the rewriter.
- `crates/editor_mcp/tests/solutions_e2e_test.rs`'s two tests collide on the
  process-global runtime-dir `OnceLock` + `SingleInstanceLock`: each passes
  alone, the pair fails. Needs a per-`App` runtime dir or one test per binary.
- `workspace.list_buffers` on a per-solution socket reports buffers from a
  *different* Solution when both are open in the same window (buffer collection
  is window-level).
- After a reconnect, background-agent tabs from a killed subprocess still render
  as live.
- The empty-solution "can't create a chat" guard is ineffective
  (`workspace_has_worktree` counts invisible worktrees).
- Pre-existing, untouched: 42 `open_listener` test failures in the `zed` bin;
  clippy lints at `crates/solutions/src/cache.rs:166`,
  `crates/editor_mcp/src/tools/subscribe.rs:67`, `crates/zed/src/zed.rs:76`,
  `crates/zed/src/main.rs:1733`/`:1744`; unused variable at
  `crates/project/src/git_store.rs:10404`.

## Decisions, so they are not relitigated

Now recorded as FORK.md **#50** (surrogate counter ids), **#51** (editor-owned
claude settings + worktree hook) and **#52** (the hot/cold rename split). Short
form:

- Ids are surrogate counters, never derived from names. That is what makes
  rename cheap: the per-solution MCP socket dir and every FK survive it.
- A live process's cwd is an inode, so a same-filesystem `mv` does not break a
  running `claude` or shell. The compat symlink exists only to keep the
  *strings* they hold valid until the next cold start. Cross-filesystem rename
  is a hard error, never a copy fallback.
- `workspaces.paths` is the workspace identity key — rewrite it in place, never
  delete-and-reinsert, or the window comes back empty.
- The worktree self-heals (`ScanState::RootUpdated` →
  `update_abs_path_and_refresh`), so a rename must NOT remove/recreate it.
- Sessions keep cwd = the member path. Pinning every session to the solution
  root was rejected: claude only loads a subdirectory's `CLAUDE.md` lazily, so a
  root-rooted session starts a task without the project's conventions.

## Resume recipe

The arc is closed; there is no in-flight phase to pick up. Take the next item
from the outstanding pool per `docs/workflow/supervisor-mode.md` § 7 — the
`file_folds` row and the `solutions_e2e_test.rs` test-isolation fix are both
LIGHT and self-contained; the `list_buffers` cross-solution leak is the only one
with real design surface.
