# Phase 6b — retire flat `entries` as persist authority + revert #3

**Date:** 2026-07-06
**Commit:** `306ca1af5f` (sawe `origin/main`)
**Scope:** `solution_agent` (persistence/ingest/model) + `acp_thread` (#3 revert).

## What shipped

The per-source demux (`SolutionSession.streams`) is now the **persistence**
source of truth, as it already was for desktop render (phase 2c) and the mobile
wire (phase 4b). Flat `session.entries` is **kept** as the 1:1 `AcpThread` ingest
mirror + demux input — it is no longer the persisted authority and, after the #3
revert, may be torn/interleaved in memory. That is harmless: render, wire, and
persist all read the coalesced `streams`; the few remaining flat-`entries`
readers (`get_session_entry`, `read_session_history`, the
`agent_session_message_appended` push, `has_in_progress_tool_call`, cold
list-sizing) stay 1:1-aligned with `AcpThread` and behave exactly as they did
before quick-fix #39 shipped.

Concretely:
- **Persist `streams[Main].entries` at Main-LOCAL indices** (subagent_id always
  `None`) via a seq-watermark incremental persist (`persisted_main_seq`):
  `persist_main_stream` upserts only Main entries with `mod_seq > watermark`,
  advances the watermark synchronously, and `delete_entries_from(main_len)`.
  `persist_all_rows` rewrites the whole Main stream. The flat-index helpers
  `persist_upsert_range`/`_entry`/`_delete_from` were removed.
- **Revert #3 (FORK.md #39):** `AcpThread::coalesce_target_index` (the
  interleave backward-scan) deleted; `AcpThread` is back to naive `entries.last()`
  coalescing. Its flat interleaving is now invisible — the store's demux
  (`stream::push_coalesced`) reunites each stream. The un-tear guarantee lives in
  `stream.rs::demux_reunites_a_parent_message_split_by_an_interleaved_teammate`.
- **Decision-#11 rewind re-stamp re-homed onto the Main stream** (was flat
  `entries.last()`): after a coalesce-split truncate, bump the last `streams[Main]`
  entry's `mod_seq` + `streams[Main].seq` so the wire delta re-delivers it and
  `persist_main_stream` re-upserts it.

## Two bugs caught in review (both real, both fixed here)

### Bug 1 — append-race via unconditional `delete_entries_from` (controller-caught)
`persist_main_stream` issues `delete_entries_from(main_len)` on every persist.
GPUI's detached DB tasks are **NOT FIFO** (see memory
`solution-agent-detached-db-writes-race`). Two rapid appends capture `main_len`
in event order, but if the earlier link's stale `delete_entries_from(N)` runs
*after* the later link's `upsert(row N)`, the just-written row N is deleted — and
because its `mod_seq` is already ≤ the watermark it is never re-persisted →
permanent row loss → truncated transcript on the next cold load. Even a FIFO DB
channel wouldn't help: the stale `main_len` is captured at issue time but its
delete executes later.
**Fix:** serialize per-session entry-row persist writes — `entries_persist_chain:
HashMap<SolutionSessionId, Task<()>>`. Each helper captures its plan
synchronously (issue order) then `prev.await`s the previous link before touching
the DB, so upsert+delete pairs apply in issue order. Removed on
`evict_session_runtime_maps`. NOTE: this is the one place we chose *enforced
ordering* over the codebase's usual *order-independence* remedy — order-independence
is impossible here because `delete_entries_from` is inherently stateful.

### Bug 2 — legacy global-indexed rows not realigned on cold-load (reviewer-caught)
A pre-6b session persisted teammate-tagged rows at **global flat indices**,
interleaved with Main rows (`entries_from_rows` preserves `subagent_id`,
store.rs:664). Under 6b, persistence keys on **Main-local** indices. On cold-load,
seeding `persisted_main_seq = streams[Main].seq` (the skip-optimization) means the
first incremental persist writes a new Main entry at a Main-local index that
physically holds a *different* row → overwrites/loses a Main message and strands
the stale tagged row (which resurrects as a phantom teammate tab on the next load).
Manifests in the crash/force-kill window before a full-flush self-heals — very
plausible in this repo's `pkill sawe` dev loop.
**Fix (`hydrate_streams_main_only`):** when the flat mirror is LONGER than the
Main stream (`entries.len() != streams[Main].len()` — i.e. legacy tagged/uncoalesced
rows present), seed `persisted_main_seq = 0` so the first persist rewrites the
ENTIRE Main stream at Main-local indices and `delete_entries_from(Main.len)` trims
the leftovers — a one-time realign. Keyed on the length (NOT
`hydration_orphan_streams`, which is populated from the *pre-rebuild* `streams`
snapshot and so is empty on a direct-`entries`-assign cold-load — see the open
note below). Regression test:
`store::tests::legacy_teammate_tagged_rows_realign_to_main_local_on_cold_load`.

## Open note (pre-existing, NOT a 6b regression — flagged for follow-up)
`hydrate_streams_main_only` records `hydration_orphan_streams` from
`self.streams.keys()` **before** it rebuilds — but the real cold-load sites assign
`s.entries = …` directly (no prior rebuild), so `streams` is still Main-only-empty
at that point and **no orphans are recorded** on the production path. The model
tests exercise it via `set_entries` first (which rebuilds), hiding this. If the
orphan set is genuinely empty on real cold-loads, the decision-#9 zombie-teammate
suppression may not fire in production (finished teammates could show as tabs after
a restart). Out of 6b scope; 6b's realign trigger was deliberately made
independent of the orphan set. **Verify in 6c/6d.**

## Verification
- `cargo test -p solution_agent --lib` → 556 passed (554 base +1 implementer
  torn-persist test +1 legacy-realign regression).
- `cargo test -p acp_thread --lib` → 80 passed, 1 pre-existing unrelated failure
  (`test_checkpoint_shows_when_file_changes_during_pending_message`, documented in
  `ff50156359`).
- clippy: no findings in the touched files (pre-existing lints only, in `git`/`mcp.rs`).
- Live render gate (offscreen, `seed_cold_session` + `workspace.screenshot`): an
  interleaved parent/teammate seed renders Main as ONE coalesced bubble
  ("Three " + "scouts were dispatched…" reunited), teammate on its own
  `toolu_gate1` stream; `get_session` confirms Main `total_count=2` (user + 1
  coalesced assistant), teammate a separate stream.
