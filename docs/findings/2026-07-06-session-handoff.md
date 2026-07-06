# Session handoff — 2026-07-06 (per-source-streams phases 4+5 SHIPPED)

Pause snapshot for the next supervisor session. Phases 4 (server model+wire) and
5 (mobile) of the per-source-streams migration are **DONE and pushed in lockstep**
— the hard cutover (`wire_schema_version` 3) is live on both repos. Only **phase 6
(cleanup)** remains.

## Commit chain since the last handoff
`sawe` (repo `/home/spk/.spk/sawe/ss/spk-solutions/sawe`, `origin/main`):
- `0f6041b207` — 4a server model: per-stream `seq` + hydration-orphan close + async-Agent stream close.
- `e3bc093d8c` — 4b prep: per-stream `seq` = coalesce-aware max entry mod_seq (single wire-delta axis; dropped the 4a side counter).
- `8414f25191` — 4b wire: `get_session`/`get_session_changes` repointed onto `session.streams`; `wire_schema_version` 2→3.
- `89cb3fa069` — 4b fix: re-stamp survivor on a rewind that splits a coalesced group (reviewer-caught delta miss).
- `01d5c36bbe` (+ `cf3ff51bc9`, `e327dd5547`, `33bf465157`) — findings docs (4a, 4b, 5).
- **HEAD ≈ `01d5c36bbe`**, tree clean, `cargo test -p solution_agent --lib` = 554 green.

`sawe-mobile` (repo `/home/spk/.spk/sawe/ss/spk-solutions/spk-editor-mobile`, `origin/main`):
- `725f7ab` — migrate the session wire to per-source streams (hard cutover).
- `dc1977d` — offscreen Roborazzi snapshot of the streams tab strip. **HEAD**, pushed.

## What shipped
- **Server:** `session.streams` is the wire source. `get_session` = descriptors-for-all-streams + entries-for-the-selected `stream_id` (stream-local index, per-stream `seq` delta, `current_seq` = selected stream seq). `get_session_changes` = per-stream delta keyed on coalesce-aware `entry.mod_seq`, always-present `streams` list + `selected_stream_id`. `active_subagents` removed from both result DTOs (`SubagentDto` + the `agent_session_active_subagents_changed` dirty-poke notification KEPT). Deferred items from phase 3 closed: async-Agent stream close (via `BackgroundAgent.parent_tool_use_id` on the real stop_reason) + hydration orphans (reopenable `hydration_orphan_streams`+`hydration_watermark` overlay, distinct from the permanent `closed_streams` Done-close).
- **Mobile:** `:core` DTOs mirror the server (tagged `StreamIdDto`, `StreamDto`); `filterEntriesBySubagent` + render-time filters DELETED; tabs from `streams`; `SUPPORTED_WIRE_SCHEMA_VERSION`→3 + `isServerTooOld` reject. `:core:test` + `:app` compile green. **Render gate PASSED end-to-end on a headless Android emulator** (real v3 remote-control → seeded Main+teammate session → all three invariants confirmed; DNP-NX9 wasn't on adb so an emulator stood in — full repeatable recipe in the phase-5 findings). Plus a component-level Roborazzi tab-strip golden.

## Outstanding pool — PHASE 6 (cleanup), review-gated, the ONLY remaining work
Retire the compensating kludges now that `streams` fully replaces them. From the spec Phasing §6 + FORK.md #38/#39:
1. **Revert quick-fix #3** — `AcpThread::coalesce_target_index` backward-scan (FORK.md #39). Per-stream coalescing at ingest now owns coalescing, so `AcpThread` returns to naive `entries.last()`. DELICATE (touches upstream `AcpThread`); re-screenshot a torn-message repro. Was deferred through phase 5 because the flat wire (now gone) still read `session.entries`.
2. **Delete quick-fixes #1/#2** (FORK.md #38) and the `should_render_entry`/`filterEntriesBySubagent`-era leftovers.
3. **Retire flat `entries`** as the maintained source → make it a derived shim (or remove) now that `streams` is the truth. Every `entries` writer currently must `rebuild_streams()` (decision #1) — collapse that.
4. **Remove `SubagentView` variants / the `active_subagent_order` + `background_agent_order` parallel order-vecs / background-agent tab duplication** — unify onto `streams`.
5. **Unify shells/bg-agents into `streams`** (`StreamKind::Shell` descriptors on the wire) — deferred from 4b (decision iii kept them as separate tools). Mobile then drops the separate shell/bg-agent strips.
6. Update FORK.md (supersede #38/#39), spec, `.rules` as needed.

## Open architectural decisions (all LOCKED, for reference)
See `.agents/2ob64rrs/c08/decisions.md` (#1–8) + next.md decisions #9 (two-overlay hydration) and #10 (coalesce-aware single seq axis). Load-bearing gotchas:
- **Coalescing-in-the-mirror + per-entry-mod_seq gate**: a rewind splitting a coalesced same-source group must re-stamp the survivor (store.rs `EntriesRemoved`) or the delta silently misses the shrink. Any future edit to coalescing/rewind must preserve this.
- **Every `session.entries` mutation must `rebuild_streams()`** (or go through `set_entries`/`mutate_session` which do). Direct-assign seeds in tests need an explicit call.
- **`current_seq` is per-stream** (the selected stream's `seq`), NOT global `change_seq` — both wire tools + the mobile cursor rely on this being uniform.
- Async-Agent teammate close fires on the bg-agent `stop_reason` (NOT the spawn-ack tool-call terminal); DB-restored bg-agents have `parent_tool_use_id: None` (hydration orphans handle them).

## Active gotchas / environment
- Mobile: **NEVER touch** git-tracked `.superpowers/sdd/{progress.md,task-R-brief.md}` (an implementer touched `progress.md`; it was reverted). Device DNP-NX9 (`A3SQUT5902000367`) was NOT connected this session → render verified via Roborazzi instead (`./gradlew :app:testDebugUnitTest --tests "*StreamTabStripSnapshotTest"`; goldens in `app/src/test/snapshots/roborazzi/`). Mobile push needs a one-line user confirm (given this session).
- `target/release-fast/sawe` rebuilt at HEAD (v3 wire) — the release server the phone pairs to.
- Screenshot/verify infra for the DESKTOP editor unchanged (debug-only `solution_agent.seed_cold_session` + `windows.scroll_at` + headless `workspace.screenshot`).
