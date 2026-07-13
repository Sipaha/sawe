# Session handoff — 2026-07-13 (rename with folder move)

**Status: the three-plan rename arc is IMPLEMENTED and verified end-to-end, and
the entire follow-up pool has been CLEARED. All on `origin/main`.** The session
started from a data-loss bug (renaming a Solution cascade-deleted its members),
turned into a design + planning run for "renaming a Solution or a project also
renames its folder", shipped all three plans, and then closed out every
follow-up they surfaced — including several pre-existing red test suites left by
the v1.7.2 refork. Every touched crate is green.

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
| `0473cabd97` `c8d20768bb` `0a3a666753` `c1a327602e` `df1ff88091` `86a6a1b616` `d52b8b67b9` `cb1e64b915` `b846537798` `4ba55c1e35` `c6dc8f33f9` `7b8734b94a` `5ec2f23d91` | **follow-up cleanup** (see below) |

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

## Follow-up cleanup (all CLOSED this session)

Every item that was outstanding is now fixed, and the cleanup surfaced further
real bugs that were fixed too:

- **Cold reconcile completeness** (`0473cabd97`, `b846537798`). `file_folds` was
  the reported gap; an audit of the whole app DB found **five more** missed
  path-bearing tables (`vim_marks`, `vim_global_marks_paths`, `image_viewers`,
  `git_graphs`, `undo_entries`) — all now rewritten. Four tables keyed by a
  `DefaultHasher` digest of the repo path (`shelf_entries`, `branch_favorites`,
  `branch_recent`, `pre_commit_configs`) are now re-keyed by recomputing the hash
  for the moved repo (the hash fn is hoisted to `git::repo_hash`).
- **`workspace.list_buffers` cross-solution leak** (`c8d20768bb`). Root cause:
  two Solutions really share one window, and the tool read the window's *active*
  workspace. Scoped to the socket's own Solution via `Solution::member_for_path`;
  the same window-active bug in `diagnostics.get` and `project.open_file` (the
  latter failed outright) was fixed in the same commit.
- **Phantom-live background-agent tabs** (`df1ff88091`). On reconnect,
  `set_acp_thread(None)` now marks background agents `killed` (not a completion,
  not `Stopped`) — the strip paints them struck-through with a red X, and the
  stuck-session watchdog stops being suppressed by a dead child holding
  `background_alive`.
- **Empty-solution chat guard** (`86a6a1b616`, `4ba55c1e35`). The UI guard now
  tests `Solution::members` (`console_panel::workspace_has_project`); the MCP
  `solution_agent.create_session` gained the matching member check it lacked.
- **The colliding e2e tests** (`0a3a666753`). `set_runtime_dir_for_test` now
  *panics* on a conflicting second call instead of silently dropping it;
  `solutions_e2e_test.rs` was split one-server-per-binary; `run_config.*` was
  correctly promoted to `GLOBAL_TOOLS`; the add-member clone cache is pinned to a
  tempdir via `set_cache_root_for_test`.
- **Pre-existing lints** (`c1a327602e`). All cleared; `git_store.rs`'s
  `branches_future` was a merge-orphaned dead binding (branches are still fetched
  by the inlined `try_join4`), removed.
- **Long-red `cargo test -p zed`** (`cb1e64b915`, `d52b8b67b9`). 42
  `open_listener` failures were the test harness not installing the fork-local
  `SolutionStore`/`SolutionAgentStore` globals. Fixing it exposed two **real
  production bugs**: `CommitDetails::short_sha` did `sha[..7]` (panicked the
  render pass on a short sha), and the `--dev-container` flag was only consumed on
  a late worktree event (stale-flag race) — both fixed.
- **Long-red `cargo test -p workspace`** (`c6dc8f33f9`, `7b8734b94a`) and **3
  `cargo test -p project` failures** (`5ec2f23d91`). Stale tests encoding upstream
  defaults the fork changed (autosave-on-close; the `.zed/` → `.sawe/` per-project
  config dir); expectations updated to the fork's reality. One
  feature-unification `match` non-exhaustiveness fixed alongside.

## Outstanding pool

Empty. The arc and its follow-ups are closed; every touched crate's tests and
clippy are green. `cargo test -p project` still lists a couple of settings tests
that only fail under cross-crate *parallel* port contention (pass standalone) —
a test-harness nuisance, not a code defect, and not introduced here.

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

The arc and its follow-up pool are both closed; there is no in-flight phase and
no outstanding pool to pick up. On the next resume, pick a fresh top-of-table
item from `docs/INDEX.md` per `docs/workflow/supervisor-mode.md` § 7 rather than
looking for leftovers here.
