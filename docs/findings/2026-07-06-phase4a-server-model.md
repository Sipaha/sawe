# Phase 4a — per-source-streams server model (seq + both deferred closes)

**Date:** 2026-07-06
**Commit:** `0f6041b207`
**Character:** INERT (nothing on the wire reads it yet — phase 4b does).
**Crate:** `solution_agent`. 552 lib tests (+9).

Closes the two items phase 3 deferred and adds the per-stream `seq` watermark
the phase-4b wire delta needs. Review-gated (implementer → reviewer → controller
re-verify); the reviewer caught a real resurrection bug in the first hydration
attempt, fixed before commit.

## 1. Per-stream `seq` maintained across the full-replace `rebuild_streams`

`demux()` rebuilds `streams` from scratch each call, discarding `Stream.seq`. So
seq is re-derived and preserved via two `SolutionSession` side fields:
`stream_seq_counter: u64` (monotonic allocator) + `stream_seqs: HashMap<StreamId,
(u64 seq, (usize count, u64 max_mod_seq))>` (prior fingerprint + assigned seq).

In `rebuild_streams`, each stream is fingerprinted `(source_entry_count,
max_source_mod_seq)` computed over the **pre-coalesce** `self.entries` (routing
mirrors demux: None→Main, Some(toolu)→Teammate). Unchanged fingerprint ⇒ keep the
prior seq; any change ⇒ `stream_seq_counter += 1`. Result: a stream's seq is
monotonic and advances iff that stream changed.

**Why the fingerprint is over source entries, not the coalesced stream
(decision #5, load-bearing):** `push_coalesced` keeps the FIRST fragment's
`mod_seq` on the merged entry, so a coalesced entry's `mod_seq` does NOT advance
when a new chunk merges in — a delta keyed on `entry.mod_seq` (the old flat wire)
would MISS coalesced-message updates. But every append (store.rs ~8053) and every
in-place `EntryUpdated` (store.rs ~8596) re-stamps the affected source entry's
`mod_seq` with a fresh monotonic `bump_change_seq()`, so `max_source_mod_seq` over
the pre-coalesce entries advances on a merge even though the coalesced entry's own
mod_seq is frozen. `source_entry_count` additionally catches truncate/rewind.

## 2. Hydration-orphan close — TWO overlays, not one (the fix)

A DB-restored session's persisted rows retain `subagent_id` (store.rs:664), so
`rebuild_streams` re-demuxes finished teammates into fresh Live streams. Phase 4a
collapses a cold-restored session to Main-only via `hydrate_streams_main_only()`,
called at the 4 cold-load sites (store.rs ~3603/4407/4703/5035).

**The bug the reviewer caught, and why one overlay is wrong:** the first attempt
reused `closed_streams` + cleared it unconditionally in `set_acp_thread` on the
cold→live transition (to un-strand a genuinely-resumed live teammate). But a
DB-restored async `Agent` carries `parent_tool_use_id: None`, so the part-3 close
path can NEVER re-close it, and it's already terminal (no `!was_terminal` edge) —
so clearing on attach resurrected finished teammates into permanent zombie tabs,
the exact regression deferred #2 removes.

**Resolution — two distinct suppression categories:**
- `closed_streams` (permanent Done-close): phase-3 Task terminal + phase-4a
  async-Agent stop_reason. Stays closed until context reset. `rebuild_streams`
  always `shift_remove`s these.
- `hydration_orphan_streams: HashSet<StreamId>` + `hydration_watermark: usize`
  (= `entries.len()` at hydration): reopenable. `rebuild_streams` suppresses an
  orphan UNLESS an entry at index >= watermark carries its `subagent_id` (a live
  resume is streaming it anew). Keyed on a pure index watermark, not the thread
  handle, so cold == "no entries past the boundary yet" and it is unit-testable
  without fabricating an `AcpThread`.
- `close_stream` removes the id from the orphan set first (permanent close
  outranks — a Done stream is never reopenable). `clear_closed_streams` (context
  reset) clears both overlays + resets the watermark. `set_acp_thread` is
  UNTOUCHED (reverted) — the watermark rebuild handles resume.

A resumed teammate that claude replays as new appends (index >= watermark)
reopens, then gets a live bg-agent registration (parent toolu captured) whose real
stop_reason permanently closes it if finished; a genuinely-live one stays open.

## 3. Async-Agent stream close via `BackgroundAgentId → parent toolu` (deferred #1)

Id spaces differ: `BackgroundAgentId` = hex `agentId` from the launch
announcement; the demux `Teammate` key = the parent `Agent` spawn tool-call's
`tool_use` id (= `snapshot.id` in `apply_subagent_lifecycle`, = the teammate
entries' `subagent_id`/parentToolUseId). Plumbed a `parent_tool_use_id:
Option<SharedString>` onto `BackgroundAgent`, captured at LIVE registration
(store.rs ~7043, before the `BackgroundAgentId::new` shadow of `id`); `None` on DB
cold-restore (not persisted — those are hydration orphans anyway). The teammate's
demux stream is `close_stream`d on the managed agent's real terminal stop_reason:
the `now_terminal && !was_terminal` edge in `refresh_background_agent_snapshot`
(primary) + a safety-net in the `tick_background_agents` reap (covers a missed
edge / stale-dead). Both collect the (toolu, reason) inside the `ba` borrow then
`close_stream` after it ends (close needs `&mut s`).

## Next

Phase 4b (server wire): `StreamDto`/`StreamIdDto`, repoint `get_session` +
`get_session_changes` onto `session.streams` (descriptors-all + entries-for-
selected, stream-local index, per-stream `seq` delta), bump
`wire_schema_version` 2→3. Then phase 5 (mobile, lockstep).
