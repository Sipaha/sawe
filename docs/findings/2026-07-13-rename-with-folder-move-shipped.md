# Rename-with-folder-move: the three-plan arc shipped

**Status:** all three plans implemented, verified end-to-end and pushed to
`origin/main`. This doc records what landed, the real-DB rehearsal numbers, and
the follow-ups the work surfaced but did not close.

Design spec: `docs/superpowers/specs/2026-07-13-rename-with-folder-move-design.md`
(local only — `docs/superpowers/specs/` is gitignored). Plans (tracked):
`docs/superpowers/plans/2026-07-13-rename-{1-identity,2-folder-move,3-claude-settings-and-sockets}.md`.

Architectural decisions extracted into `FORK.md`: **#50** (surrogate counter
ids), **#51** (editor-owned claude settings + worktree hook), **#52** (the
hot/cold rename split).

## Plan 1 — identity model

`a81166f241` `b22518b815` `f573212060` `9c3e0e9d1e` `0a1407c8a3` `defd92e20b`
`a1917ea87c` `03ec37164d`

Slug ids became surrogate counters: `SolutionId(i64)` / `MemberId(i64)` /
`CatalogId(i64)`, backed by SQLite rowids. `name`, `root` and `local_path` are
ordinary mutable columns, which is what makes rename cheap. Catalog entries are
now **templates** — `origin_catalog_id` is provenance only, and a member owns
its own `name`. `solution_sessions.member_id` replaces cwd-inference for the
session's project label and for console-tab scoping. Per-solution MCP socket
dirs are keyed by the numeric id; stale slug-named dirs are swept at startup.

A cross-DB migration (`solution_legacy_ids`) remaps the slugs held in
`solution_agent.db`.

`03ec37164d` closed a regression the type flip introduced: the per-solution
socket injects `solution_id` as a JSON *number*, but the scoped tools still
declared it `String`, so every `workspace.*` / `project.*` / `diagnostics.*` /
`solution.git.*` call failed with `invalid type: integer 1, expected a string`
until they were flipped to `i64`.

### Real-database rehearsal

The migration was rehearsed against the operator's **real** databases
(`crates/solutions/tests/identity_migration_rehearsal.rs`):

| In | Out |
|---|---|
| 16 solutions / 43 members / 14 active / 47 catalog projects | same counts, same rows |
| 176 agent sessions | all preserved; **169 remapped**, **70 `member_id`s backfilled** |
| 4 slugs belonging to long-deleted solutions | left unmapped — rows kept, not destroyed |
| — | **0** dangling `active_member` rows |

**The operator's real databases migrate on the next launch of the new binary.**
Pre-migration backups:

- `/tmp/db.sqlite.pre-identity-1783948879`
- `/tmp/solution_agent.db.pre-identity-1783948879`

## Plan 2 — rename moves the folder

`37763a1897` `5a3e92e403` `8012b06d8d` `107d47a40f` `27110814b5` `0a7843b2d4`
`5a010d842d` `b60545b59b` `22e071d5ce` `eb3eb809f4` `5030f40efc` `5e2b6d661c`

**Hot half** (editor live): derive a Unicode folder name from the display name
(no transliteration), collision-check, same-filesystem check (cross-device is a
hard error — never a copy fallback), `rename(2)`, drop a compat symlink old →
new so in-flight *string* paths keep resolving, update the owning DB rows, queue
a `pending_path_migrations` row.

**Cold half** (`solutions::path_migrations::drain_and_apply`, runs in
`SolutionStore::init_with_db` before any window opens): rewrite every
path-bearing row across the app DB (`solutions`, `solution_members`,
`workspaces`, `console_panel_state`, `editors`, `terminals`, `breakpoints`,
`bookmarks`, `trusted_worktrees`; stale `toolchains` rows are dropped because
their key *is* the path) and the agent DB (`solution_sessions.cwd`,
background-agent JSONL paths), move/merge the claude transcript bucket, repair
git worktrees, remove the compat symlink, delete the row. Idempotent and
crash-safe.

`workspaces.paths` is rewritten **in place**, so the window keeps its
`workspace_id` and its panes / tabs / docks survive the rename. Verified live:
renaming solution `btest` → `Проект Тест` moved the directory, and after a
restart the reconcile drained cleanly with `workspace_id` unchanged.

Also new: the `solutions.rename_member` MCP tool, a rename-member modal, and
inline error surfacing in the rename-solution modal.

## Plan 3 — claude settings + sockets

`47c38dbca7` `4573ee11b3` `307eea9bde` `710361bb66` `8a60552b22` `7ad284513f`
`8b21dc3fdd` `69a83c8918` `16d9f9295d` `399eb2e471` `781b484308` `48a5b7b04c`
`6ec94dbcb1`

Sockets, lock file and upload spool moved `config/` → `state_dir()`
(`~/.spk/sawe[-dev]/state/`), with a one-time sweep of the old location.

The editor now owns a claude settings layer passed as `--settings <file>` (at
`<state>/solutions/<id>/claude-settings.json`). Its `WorktreeCreate` /
`WorktreeRemove` hooks point at the running binary itself
(`sawe --worktree-hook`), so agent worktrees land in
`<solution_root>/.agents/worktrees/` instead of `<member>/.claude/worktrees/`,
and `autoMemoryDirectory` → `<solution_root>/.agents/memory`. Verified with a
real claude subagent.

`solution_id` is now **optional** in the solution-scoped MCP tools: the
per-solution socket injects it and overrides any foreign id the caller sends; on
the global socket, omitting it is a clear error, never a silent
wrong-solution success.

## Open follow-ups

Discovered during the work, not closed:

- **`file_folds` is not rewritten by the cold reconcile.** The editor's fold
  persistence is keyed by path, and the table is missing from
  `path_migrations::rewrite_app_db`, so a folded region is orphaned by a rename.
  Low impact; the fix is one more row in the rewriter.
- **`crates/editor_mcp/tests/solutions_e2e_test.rs`'s two tests collide.** They
  share the process-global runtime-dir `OnceLock` + `SingleInstanceLock` — each
  passes alone, the pair fails. Needs a per-`App` runtime dir, or one test per
  binary.
- **`workspace.list_buffers` on a per-solution socket reports buffers from a
  *different* Solution** when both are open in the same window — buffer
  collection is window-level. Orthogonal to the identity work.

Pre-existing, untouched by this arc:

- 42 `open_listener` test failures in the `zed` bin.
- Clippy lints at `crates/solutions/src/cache.rs:166`,
  `crates/editor_mcp/src/tools/subscribe.rs:67`, `crates/zed/src/zed.rs:76`,
  `crates/zed/src/main.rs:1733` and `:1744`.
- Unused variable at `crates/project/src/git_store.rs:10404`.

Carried over from the previous handoff and still open:

- After a reconnect, background-agent tabs from a killed subprocess still render
  as live — no terminal state is set for them.
- The empty-solution "can't create a chat" guard is ineffective:
  `console_panel::panel::workspace_has_worktree` counts invisible worktrees. See
  `docs/findings/2026-07-13-rename-solution-cascade-data-loss.md`.
