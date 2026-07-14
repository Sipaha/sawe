# Session handoff — 2026-07-14

**READ FIRST on resume** (supersedes `2026-07-13-session-handoff.md`).

Previous handoff tip: `05958a2a82`. This session's chain: `2c57564fc1 … 22ac330ac6`
(all pushed to `origin/main`). Mobile (`Sipaha/sawe-mobile`) tip `c58d88a`, pushed.

## What shipped this session (all pushed, all reviewed)

1. **Session-purge-on-rename fix** (`2c57564fc1`). A folder-moving
   `rename_solution`/`rename_member` hard-purged every open AI session:
   `gc_orphan_members` (fired on `SolutionStoreEvent::Changed`) compared each
   hydrated session's stale-cwd against the just-moved root/members and deleted
   it as a false orphan. Fix: new `SolutionStoreEvent::PathsMoved{id,old,new}`
   emitted **before** `Changed`; `SolutionAgentStore::rewrite_session_cwds_for_move`
   rewrites live + persisted cwds first. Finding:
   `findings/2026-07-14-rename-purges-open-sessions.md`.

2. **Mobile numeric-id wire mismatch → wire v6** (`1679c533d0` editor +
   `c58d88a` mobile). The identity migration made Solution/member/catalog ids
   numeric on the wire but never bumped the version, and the mobile DTOs still
   said `String` → kotlinx crash `"String literal … should be quoted"`. Editor
   `wire_schema_version` 5→6; mobile migrated every solution/catalog id
   `String`→`Long` + `SUPPORTED_WIRE_SCHEMA_VERSION`=6 (26 files, `:core:test`
   332, `:app` 41). **v6 editor and v6 mobile must run together** (mutual gate).
   Finding: `findings/2026-07-14-mobile-wire-numeric-ids-v6.md`.

3. **Teammate-completion architecture rework** (`677e0f3d55 … 67470c90be`, full
   subagent-driven build). Root fix for the recurring zombie/stuck teammate-tab
   class: instead of guessing "done" by tailing the agent JSONL (tab could stick
   Live ~1 h), the async `Agent` teammate closes on the authoritative Claude SDK
   `Stop` hook (`close_teammate_on_stop`, driven by the existing `subscribe_to_session`
   HookPull closure), inline `Task` on tool-call terminal, killed on thread-drop
   (immediate). JSONL dropped as a close trigger; reaper demoted to a lost-hook
   backstop whose live-parent window STAYS LONG (~3600 s — hardening #9: a live
   silent long tool call must not be mistaken for a lost hook). Spec
   `specs/2026-07-14-teammate-completion-signal-design.md`, plan
   `plans/2026-07-14-teammate-completion-signal.md`, finding
   `findings/2026-07-14-teammate-completion-signal-shipped.md`. The opus
   whole-branch review caught an I-1/I-2 #9 re-regression (a shortened backstop)
   and forced its revert (`4e0663e9d0`). Follow-ups: `pending_stop` cleanup + a
   stale `seed_cold_session` doc were then closed (`22ac330ac6`). 590 tests.

4. **Worktree-hook "(deleted)" fix** (`3b98c64da7`). Rebuilding the binary while
   the editor runs makes `current_exe()` resolve to `…/sawe (deleted)` on Linux;
   that path was baked into the claude WorktreeCreate hook + the `--nc` MCP
   bridge → background agents failed "not found". Extracted the existing
   " (deleted)"-strip into `util::current_exe_resolved` and used it at both
   self-exec sites.

**Deploy state:** `release-fast` rebuilt at 18:02 (verified live: editor now
advertises `wire_schema_version: 6`, `binary_path` has no "(deleted)"). User
restarted the editor; v6 + hook-path confirmed on the running instance. Mobile
v6 APK built + installed on the connected phone (`DNP_NX9`). NOTE: the 18:02
binary predates `22ac330ac6` (the two Minor follow-ups) — harmless, they ride
the next rebuild.

## Outstanding pool — IN PROGRESS (resume here)

**Feature: restore ALL dialog tabs (AI sessions + terminals) on Solution
close → reopen.** User also reported a bug: opening a Solution shows no dialogs
first, then a terminal appears alone. Mid-**brainstorming** (design not yet
written). Grounding is done — the diagnosis (verified read-only):

- It is ONE bug, not two. `ConsolePanel` (one per window, `console_panel/src/panel.rs`)
  restores tabs via `restore_from_db` from the workspace-keyed `console_panel_state`
  table. **Terminals restore unconditionally** (fresh shell at saved `origin_cwd`);
  **chat (AI) rows are SKIPPED** (`continue`) when `session_exists` is false —
  `restore_from_db` chat branch, `panel.rs:477-491`. Restore is detached/async →
  empty strip first, then the terminal materialises alone.
- Why chats are skipped: `restore_from_db` awaits `hydrate_open_tabs_lazy(solution_id,…)`
  first (`panel.rs:400-417`), but only when `active_solution_id_for_workspace(ws)`
  (`panel.rs` top helper) returns `Some`. **PAUSED mid-read of the exact root**:
  is that helper `None` at restore time (so hydration is skipped → `session_exists`
  false), or does `hydrate_open_tabs_lazy` not make `session(id)` return `Some`?
  `active_solution_id_for_workspace` matches a worktree abs_path via
  `store.solution_for_path` — verify it's populated before `restore_from_db` runs.
- Separate dead seam: `SolutionStoreEvent::Opened` handler (`solution_agent/src/store.rs:3508`)
  calls only `hydrate_all_for_solution`, which INTENTIONALLY skips `by_solution` +
  `SessionCreated` + `persist_tab_order` → emits nothing `ConsolePanel` subscribes
  to. `restore_open_tabs` (the only method that emits `SessionCreated` + fills
  `by_solution`) has NO production caller — effectively dead on desktop.
- Reopen reruns `restore_from_db` (that's why the terminal reappears), so the fix
  locus is the `restore_from_db` chat-skip race, NOT the `Opened` handler — but
  confirm the window model (`solutions/src/event_sources.rs:145-167` MultiWorkspace
  reconciliation) during design.

**Scope agreed with user:** terminal cwd-restore depth is sufficient (no
scrollback/command re-run); focus on making AI tabs restore reliably ALONGSIDE
terminals in the right order. Next brainstorming steps: propose 2-3 fix
approaches (fix the hydration-before-session_exists race in `restore_from_db`
vs. make chat restore hydrate-on-demand vs. wire `Opened`→panel), then design →
spec → plan → subagent-driven implementation.

## Open architectural decisions / gotchas still live

- **Teammate `StreamState::Done` is now near-vestigial** (killed closes
  immediately) but KEPT — removing it is a wire v7 + mobile lockstep change.
- **N+1 `rebuild_streams` in `mark_background_agents_killed`** left as-is (a DRY
  refactor to collapse it risks duplicating `close_stream`'s body; two reviewers
  called it harmless).
- **Two parallel tab-persistence stores**: terminals → `console_panel_state`
  (workspace-keyed); chats → `console_panel_state` AND `solution_agent.tab_order`.
  This split is the root of the restore asymmetry above.
- **The user's frustration signal is on record** (memory
  `teammate-stream-lifecycle-architecture`): prefers root/architectural fixes
  over band-aids in the teammate/stream-lifecycle area.
- A session lost to the original rename bug is **not recoverable** in-app
  (hard-purged); raw claude JSONL may exist — offer stands.

## SDD scratch

`.superpowers/sdd/progress.md` (git-ignored) holds the teammate-work ledger.
