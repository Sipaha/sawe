# Phase 4b — per-source-streams server wire (hard cutover)

**Date:** 2026-07-06
**Commits:** `e3bc093d8c` (seq single-axis prep), `8414f25191` (wire repoint), `89cb3fa069` (coalesce-split delta-miss fix).
**Character:** wire change, HARD CUTOVER (`wire_schema_version` 2→3). Server-only — NOT user-visible on desktop (desktop reads the in-process model). Consumed by the mobile client (phase 5, ships in lockstep).
**Crate:** `solution_agent` (+ one line in `editor_mcp`). 554 lib tests.

Review-gated (implementer → reviewer → controller re-verify). The controller caught a `current_seq` consistency bug the cut-off implementer left; the reviewer caught a coalescing delta-miss (Finding 1 below). Both fixed before the phase closed.

## Seq axis correction (`e3bc093d8c`, prep)
Phase 4a maintained `Stream.seq` via a SEPARATE counter + fingerprint map. That's a stream-level "changed?" watermark — it can't drive an incremental per-ENTRY delta (the client needs a per-entry cursor on the same axis). Revised to a SINGLE axis: `Stream.seq = max(entry.mod_seq)` and `push_coalesced` bumps a merged entry's `mod_seq` to the incoming fragment's. This one axis is BOTH the descriptor watermark and the per-entry delta cursor. Dropped the dead `stream_seq_counter`/`stream_seqs` fields.

## Wire shape (`8414f25191`)
Model = **descriptors-for-ALL-streams + entries-for-the-SELECTED-stream** (decision #7).
- DTOs: `StreamIdDto` (tagged `{type: main|teammate|shell}`, also `Deserialize` — it's the `stream_id` request param replacing `subagent_filter`), `StreamDto` (id/kind/label/state/seq/total_count), `StreamKindDto`, `StreamStateDto`, `build_streams_vec`.
- `get_session`: returns the full `streams` descriptor list + paginates the SELECTED stream's own entries (stream-local index); image cursor is per-stream. `current_seq` = the SELECTED stream's watermark (its descriptor `seq`), NOT the global `change_seq` — so the per-stream cursor is uniform + monotonic (a global seed would start above the stream's watermark then step DOWN on the first delta poll).
- `get_session_changes`: per-stream delta (`entry.mod_seq > since_seq` over the selected stream, coalesce-aware), always-present `streams` descriptor list + `selected_stream_id`; caught-up `current_seq` = the selected stream's `seq`. Reset carries the descriptor list too.
- Removed `active_subagents` from both result DTOs (the `streams` list is the tab strip now). `SubagentDto` + the `agent_session_active_subagents_changed` notification STAY (live dirty-poke channel; mobile re-polls on the poke). `wire_schema_version` 2→3.
- Shells/bg-agents stay separate tools (decision iii); `StreamKind::Shell` won't appear in `streams` yet.

## Finding 1 — coalesce-split delta miss (`89cb3fa069`, review-caught)
Two consecutive parent `AssistantMessage`s coalesce into ONE stream entry keeping the FIRST fragment's `mod_seq`. A rewind (`EntriesRemoved`) that removes only the LATER fragment shrinks that entry's content while leaving `total_count` unchanged AND lowering the coalesced entry's `mod_seq` back to the first fragment's. A per-stream delta client caught up past the coalesced seq would then silently render the stale (longer) text — the flat wire was immune (separate rows dropped via `total_count`). This is a NEW failure class from coalescing-in-the-mirror + per-entry-mod_seq gating; bounded to the `acp_thread` rewind/refusal-truncate path (`/clear` is safe via epoch reset).

**Fix:** the `EntriesRemoved` handler re-stamps the surviving boundary entry's `mod_seq` via `bump_change_seq()` before `rebuild_streams` (so the stream watermark rises above every issued cursor and the next delta re-delivers the shorter entry) and re-persists that row to keep the DB `mod_seq` in lockstep. Regression: `entries_removed_restamps_survivor_on_coalesce_split`.

## Test-shape shifts (for future readers)
The wire now serves COALESCED streams, so tests that built consecutive assistant messages got 1 merged entry — several delta/pagination tests were repointed to non-coalescing (user) messages, and `EntrySummary.index` is now stream-local. Any test that assigns `session.entries` directly MUST `rebuild_streams()` after (the `mutate_session` helper now does; the cold-row + created_ms seeds gained explicit calls). Selecting a teammate's entries needs `stream_id: Some(Teammate{toolu})` — Main no longer contains tagged entries.

## Next
Phase 5 (mobile, sawe-mobile repo): Kotlin `StreamDto`/`StreamIdDto` mirroring these exact shapes; `SessionDetailStore` reads `streams` (tabs) + selected-stream entries; delete `filterEntriesBySubagent` + render-time filters; `subagent_filter`→`stream_id`; `applySessionDelta` per-stream (stream-local index + per-stream seq cursor); `SUPPORTED_WIRE_SCHEMA_VERSION`→3 + add a server-too-OLD reject. Gate: `:core` tests → build+install DNP-NX9 → offscreen device screenshot (Main intact + teammate excluded + a teammate tab from `streams`) → push `sawe-mobile` (one-line confirm).
