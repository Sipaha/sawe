# Session handoff — 2026-07-13 (rename with folder move)

**Context for a fresh agent:** the operator renamed the Solution `spk-solutions`
to "Sawe" and it came back empty after a restart. That bug is fixed and the data
is restored; the session then turned into a design + planning run for "renaming a
Solution or a project also renames its folder". The plans are written and pushed;
**implementation has not started**.

## Commit chain (all on `origin/main`)

| Commit | What |
|---|---|
| `132e89f5a7` | `solutions`: `save_solution` is a real UPSERT — rename no longer cascade-deletes members |
| `f96c85defe` | finding: `docs/findings/2026-07-13-rename-solution-cascade-data-loss.md` |
| `d61e2c41b0` | plan 1 — identity model |
| `ccda012864` + `d8f84951dd` | plan 2 — folder move (+ worktree-repair refinement) |
| `2854fa4c9a` | plan 3 — claude settings + sockets |
| `31f4f1fde2` | `solution_agent`: stuck-session watchdog no longer kills a parent waiting on background agents |

## What shipped

1. **Data-loss fix.** `save_solution` used `INSERT OR REPLACE` on `solutions`,
   whose children (`solution_members`, `active_member`) are
   `ON DELETE CASCADE` with `PRAGMA foreign_keys=TRUE` — the REPLACE deleted the
   parent row and cascaded the members away. `rename_solution` was the only caller
   that re-saved an existing id, so renaming wiped the member list; the loss was
   invisible until the next app start. Regression test:
   `db::tests::resaving_solution_preserves_members_and_active_member`.
   The operator's live DB was repaired by hand (members `sawe`, `spk-editor-mobile`
   + `active_member`); backup at `/tmp/db.sqlite.bak-1783929090`.

2. **Watchdog false positive.** `tick_stuck_sessions` treated "Running, silent
   300s, no in-progress tool" as a hang. A parent awaiting background agents has
   exactly that shape (the spawning `Agent` call returns immediately), so it killed
   two live agent chains. `turn_is_wedged` now takes `background_alive`, computed
   from live background shells + background agents whose JSONL grew within
   `TOOL_OUTPUT_SILENCE_SECS`. A hung *foreground* tool is still recovered.

## The plan pool (nothing started)

Spec: `docs/superpowers/specs/2026-07-13-rename-with-folder-move-design.md`
(local only — `docs/superpowers/specs/` is gitignored; plans are tracked).

- `docs/superpowers/plans/2026-07-13-rename-1-identity.md` — 7 tasks. Counter ids
  for solutions/members/catalog, catalog becomes templates (`origin_catalog_id` is
  provenance only), `solution_sessions.member_id` replaces cwd-inference for the
  project label and console-tab scoping, cross-DB migration off slug ids.
- `docs/superpowers/plans/2026-07-13-rename-2-folder-move.md` — 12 tasks. Unicode
  folder-name derivation (no transliteration, hard error on collision), hot rename
  = `mv` + compat symlink + `pending_path_migrations`, cold reconcile at startup
  rewrites every path-bearing row in three DBs and moves the claude transcript
  bucket.
- `docs/superpowers/plans/2026-07-13-rename-3-claude-settings-and-sockets.md` —
  14 tasks. Sockets `config/` → `state_dir()`, an editor-owned claude settings file
  (`WorktreeCreate`/`WorktreeRemove` hooks → `<root>/.agents/worktrees`,
  `autoMemoryDirectory` → `<root>/.agents/memory`), optional `solution_id` in
  scoped MCP tools.

Order: 1 → 2 → 3 (2 depends on 1's types; 3 is independent of 2).

## Key decisions, so they are not relitigated

- Ids are surrogate counters, never derived from names. This is what makes rename
  cheap: the per-solution MCP socket dir and every FK survive it.
- A live process's cwd is an inode, so `mv` on the same filesystem does not break a
  running `claude` or shell. The compat symlink exists only to keep the *strings*
  they hold valid (transcript bucket, gitdir pointers, absolute paths in context)
  until the next cold start. Cross-filesystem rename is a hard error.
- The worktree self-heals (`ScanState::RootUpdated` → `update_abs_path_and_refresh`),
  so a rename must NOT remove/recreate it.
- Sessions keep cwd = the member path. Pinning every session to the solution root
  was considered and rejected: claude only loads a subdirectory's `CLAUDE.md`
  lazily, so a root-rooted session starts a task without the project's conventions.

## Open, not scheduled

- After a reconnect, background-agent tabs from the killed subprocess still render
  as live — no terminal state is set for them.
- The empty-solution "can't create a chat" guard is ineffective
  (`workspace_has_worktree` counts invisible worktrees) — see the data-loss finding.
- `./script/clippy` is red on pre-existing lints: `crates/git/src/backup.rs:114`
  (`unnecessary_sort_by`), `crates/solutions/src/add_member.rs:701`,
  `crates/solutions/src/cache.rs:166`.
