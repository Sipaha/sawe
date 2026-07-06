# Phase 6d-tail-2 — Stream.label as the single label source + retire active_subagents (wire v4→v5)

**Date:** 2026-07-07. **Character:** the "beautiful architecture" cross-repo cutover (v4→v5) — make
`Stream.label` the single source of truth for a stream's display label and delete the entire
`active_subagents`/`SubagentTab`/`SubagentDto` machinery + its wire field. Server + mobile ship together; the
v5 server breaks a v3/v4 mobile client → coordinated push after user confirm.

## The design
A teammate stream's friendly label used to live in `active_subagents: HashMap<toolu, SubagentTab{label,
started_at}>` (+ a parallel `active_subagent_order` Vec), serialized onto the wire as
`SessionSummary.active_subagents: Vec<SubagentDto>`, and looked up ad-hoc by the desktop strip. Replaced with:

1. **`teammate_labels: HashMap<toolu, SharedString>`** on `SolutionSession` — the stable friendly label
   captured at registration for EVERY teammate (inline Task AND async Agent), keyed by the demux toolu. No
   `started_at`, no order-Vec, no struct.
2. **`rebuild_streams` enriches** each `StreamId::Teammate(toolu)` stream's `label` from `teammate_labels`
   (fallback: raw toolu). After this, **`Stream.label` is the single truth** for the teammate label on BOTH
   the desktop strip AND the mobile wire (`StreamDto.label`) — the strip and mobile just read `stream.label`.
3. Lifecycle: label captured at registration (`store.rs`, `is_task_like`+`is_in_progress`, locked at first
   observation); NOT dropped at async spawn-ack (the async stream stays open past ack); reclaimed in
   `close_stream(Teammate(toolu))` — the single reclaim point reached by both the inline-Task terminal path
   and the async agent's real-`stop_reason` path.
4. Removed: `active_subagents`/`active_subagent_order`/`SubagentTab`/`SubagentDto`/`build_active_subagents_vec`
   /`SessionSummary.active_subagents` (the wire field)/`started_at`/`seed_subagent_tabs`. The
   `agent_session_active_subagents_changed` notification stays as a lean `{session_id}`-only dirty-poke
   (mobile ignores the list and just re-polls `streams`). `wire_schema_version` 4→5.

## The →Idle GC (delicate — reworked + regression-tested)
The →Idle catch-all GC that closes stranded teammate streams was rewritten: it now sources stranded ids from
`session.streams` teammate keys **EXCLUDING async-agent parents** (`background_agents[*].parent_tool_use_id`),
instead of the old `active_subagents.keys()` (which was inline-Task-only because async was dropped from
active_subagents at spawn-ack). Behaviour-equivalent to the old inline-Task-only cleanup, but streams-driven
and label-safe: a still-streaming async teammate is NOT killed (decision #5) and keeps its label. New
regression test `idle_transition_gc_excludes_live_async_agent_teammate` guards exactly this.

## Mobile (`spk-editor-mobile`, v5)
`SessionSummaryDto.activeSubagents` was decoded-but-never-rendered (dead field) → deleted, with `SubagentDto`.
`SessionActiveSubagentsChangedPayload` slimmed to `{sessionId}` (the handler already ignored the list —
`onActiveSubagentsChanged` just `scheduleDeltaPoll`s). `SUPPORTED_WIRE_SCHEMA_VERSION` 4→5. Teammate pills
already render `StreamDto.label`, so mobile gets **friendly teammate labels for free** (previously raw toolu
ids). Roborazzi goldens unchanged (fixtures already drove `label`). Version tests bumped to v5.

## Verification (both v5 gates PASSED; controller re-verified diffs + pixels)
- Server: `cargo test -p solution_agent --lib` **531** (530 + the new GC regression test); `editor_mcp`
  server_e2e v5 assert green; clippy byte-identical to base (no new warnings/dead_code). Subagent reviewer:
  no blockers; its two findings (missing async-exclusion regression test; a stale `on_background_agents_changed`
  doc comment) both addressed in the same commit.
- Mobile: `:core:test` + `:app:compileDebugKotlin` + `:app:testDebugUnitTest` (Roborazzi Compare) green;
  grep-confirmed `SubagentDto`/`activeSubagents` gone from live code.
- **Desktop offscreen gate:** seeded `live_teammates` (now stamps a `task-<toolu>` label into
  `teammate_labels`) — the strip pill reads **`task-toolu_zz9`** (the friendly label via
  `teammate_labels`→`Stream.label`→pill), NOT the raw id; Main/shell render.
- **Android emulator v5↔v5 gate:** `editor.capabilities` wire_schema_version 5; mobile
  `SUPPORTED_WIRE_SCHEMA_VERSION` 5; live `ESTAB :21773`. Proofs (controller-inspected screencaps):
  (1) v5↔v5 handshake accepted; (2) the session LIST decodes cleanly with `SessionSummary` carrying NO
  `active_subagents` (11 sessions rendered); (3) **headline:** the mobile teammate pill shows the FRIENDLY
  **`task-toolu_v5a`** label (rides `StreamDto.label` on the v5 wire), NOT the raw id; (4) teammate body
  scoped under its own tab.

## Remaining
- **6e** — final docs (supersede FORK.md #38/#39, spec Phasing §6 fully ✅), `.rules` refresh, whole-branch
  migration review (constraint #6). Optional cosmetic `SubagentView`→`StreamId` rename (the enum is already
  isomorphic — `{Main,Task,Shell}`). Optionally drop the now-purposeless `on_background_agents_changed`
  repaint (the label no longer varies with `background_agents`).

## Coordinated push
v5 is a HARD CUTOVER — `sawe` + `sawe-mobile` push TOGETHER after the user's one-line confirm.
