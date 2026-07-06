# Phase 6c — desktop strip + selection onto `session.streams`

**Date:** 2026-07-06 · **Commits:** `7aeeee7470` (6c), `7144dc94d2` (seed-tool
gate helper) · **Crate:** `solution_agent` (desktop-only; no wire, no mobile).
**Status:** ✅ shipped to `sawe` `origin/main`. `sawe-mobile` unchanged (`dc1977d`).

## What shipped

The desktop sub-agent tab strip and its selection reconcile now read the maintained
per-source `session.streams` mirror instead of the parallel `active_subagent_order`
Vec, plus a real cold-load orphan bug found while here.

1. **Strip teammate (`tabs`) loop** (`session_view/task_subagent_strip.rs`) iterates
   `session.streams` in map order, filtered to `StreamId::Teammate(id)` whose `id` is
   present in `active_subagents`. Ordering source of truth is now `streams`; the
   desktop no longer reads `active_subagent_order`.
2. **`next_selection_after_change`** (`session_view.rs`) takes `&session.streams` and
   snaps a removed teammate to **Main only** (spec: "force back to Main only on
   stream_removed"), dropping the old hop-to-next-teammate.
3. **`hydrate_streams_main_only`** (`model.rs`) derives hydration orphans from a direct
   `demux(&self.entries)` instead of the stale `self.streams` mirror.

## Scope decision — teammates-only, staged (LOCKED)

A FULL `SubagentView`→`StreamId` collapse and removal of the `active_subagents*` /
`background_*_order` fields is **deferred to 6d**, because:

- `StreamId` has only `Main | Teammate | Shell` — no `Background` variant, so an
  async-`Agent` teammate view can't be a bare `StreamId` yet.
- An async `Agent` teammate is **double-represented**: a live
  `StreamId::Teammate(parent_tool_use_id)` stream in `session.streams` AND a separate
  `bg_agents` pill (JSONL-sourced). Removing the double representation is 6d.
- `active_subagents` + `active_subagent_order` are **still on the wire**
  (`SessionSummary.active_subagents` via `build_active_subagents_vec`, and the
  `agent_session_active_subagents_changed` notification). Removing them is a
  wire-format change → belongs to 6d's `wire_schema_version` bump.

So every field and enum variant is KEPT; 6c changes only how the DESKTOP reads them.

### Why the `∈ active_subagents` filter is behavior-preserving (not a hack)

It is the exact bridge that keeps 6c equivalent to the old strip until 6d:
- Live inline `Task`: in `active_subagents` AND has a teammate stream → shown.
- Async `Agent` post-spawn-ack: dropped from `active_subagents` at ack
  (`store.rs:6957`) but keeps its teammate stream open → the filter **excludes** it
  here so it is NOT double-pilled (it renders as its `bg_agents` pill).
- Completed inline `Task`: removed from `active_subagents` AND `close_stream`'d
  together (`store.rs:6957-6965`) → gone from both.

The one intended behavior change: a just-spawned inline Task's pill appears on its
**first tagged entry** (stream creation) rather than the tool-call's first
`InProgress` — exactly the spec's "teammate on first tagged entry" (Decision 3).

## Two bugs fixed (one latent-prod, one review-caught)

### A. decision-#16 cold-load orphan bug (latent prod — decision-#9 zombie tabs)

`hydrate_streams_main_only` recorded `hydration_orphan_streams` from
`self.streams.keys()`, but all 4 cold-load sites (`store.rs:3614, 4421, 4717, 5049`)
do `s.entries = entries; s.hydrate_streams_main_only();` with **no `rebuild_streams()`
between** — so `self.streams` was the stale pre-load (Main-only) mirror at that read →
**zero orphans recorded**. The trailing `rebuild_streams()` then demux'd the legacy
teammate-tagged rows into visible `Live` teammate streams with nothing suppressing
them → **finished teammates resurrected as zombie tabs after a restart** (decision-#9
suppression silently no-op'd in production; the model tests hid it by calling
`set_entries` — which rebuilds — first). Fixed by deriving orphans from
`demux(&self.entries)`. Regression test:
`model::tests::hydrate_records_orphans_from_directly_assigned_entries`.

### B. review-caught: →Idle strip GC stranded a viewer on a frozen tab

The `→Idle` subagent-strip GC (`store.rs:8805`) clears `active_subagents` /
`active_subagent_order` for stranded inline Tasks but never called `close_stream`.
Pre-6c the selection snap checked `active_subagents` (cleared here → snapped to Main);
WI-2's streams-only check meant the teammate stream survived (re-demux'd `Live` from
still-tagged `entries`) → a viewer pinned to that tab stranded on a frozen, pill-less
view — the exact 14h-stuck-tab class this GC exists to prevent. Fixed: the GC now
`close_stream`s each cleared teammate in lockstep (async `Agent` teammates are already
out of `active_subagents` at ack, so only inline Tasks close — never a live async
stream). Regression test:
`store::tests::idle_transition_gc_closes_stranded_teammate_stream`.

## Verification

- `cargo test -p solution_agent --lib` → **558 passed** (556 base + orphan-fix +
  GC-close regressions).
- `cargo build --bin sawe` clean; `cargo clippy -p solution_agent --all-targets` — no
  findings in touched files (`script/clippy`'s `--deny warnings` gate is blocked by a
  pre-existing unrelated lint in `crates/git/src/backup.rs:114`).
- **Offscreen strip screenshot gate PASSED** (headless, `streams-gate` dev solution):
  (1) Main selected → strip `Main`(active)+`toolu_scout1` pill from streams, Main body
  excludes teammate; (2) teammate selected → pill active, body = teammate stream;
  (3) after-close (Main-only session) → strip collapses to nothing. Screenshots at
  `/tmp/6c-shot-{main2,teammate,closed}.png`.
- release-fast rebuilt at `7aeeee7470` for the user's hands-on test (the seed-tool
  extension is `#[cfg(debug_assertions)]`, inert in release).

## Gate tooling

`solution_agent.seed_cold_session` gained an opt-in `live_teammates` flag
(`7144dc94d2`) that registers seeded teammates into `active_subagents` so the debug
tool can paint a LIVE teammate pill (a cold seed otherwise leaves `active_subagents`
empty and the phase-6c strip correctly hides the teammate). Default false = the
finished/cold-load render state.

## Next: 6d

Fold shells / background-agents into `streams` (wire bump `wire_schema_version` 3→4 +
`sawe-mobile` lockstep + emulator gate). THEN the full `SubagentView`→`StreamId`
collapse, removal of `active_subagents*` / `background_*_order`, and the
double-representation removal become clean. Mobile push needs a one-line user confirm.
