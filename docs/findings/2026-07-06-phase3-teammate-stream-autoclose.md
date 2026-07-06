# Phase 3 — teammate stream auto-close (and the async-Agent spawn-ack gotcha)

**Date:** 2026-07-06
**Area:** `crates/solution_agent` (`model.rs` streams overlay, `store.rs`
`apply_subagent_lifecycle`)
**Spec:** `docs/superpowers/specs/2026-07-06-per-source-streams-design.md`

## What shipped

`SolutionSession.closed_streams: HashMap<StreamId, SharedString>` — an overlay
`rebuild_streams()` consults: after `demux(&entries)` it `shift_remove`s every
closed id, so a finished teammate's stream does not reappear from its
still-present tagged entries (the transcript archives to the on-disk JSONL —
decision #6). `close_stream(id, reason)` (no-op for Main) inserts + rebuilds;
`clear_closed_streams()` empties it on a context reset (`/compact`, `/clear`).
Inert: nothing user-visible reads auto-close yet (the strip + selection still run
off `active_subagents`); groundwork so the phase-4 wire emits accurate
per-stream add/remove.

## The load-bearing gotcha: async `Agent` tool-call goes terminal AT SPAWN

The teammate "done" signal is `apply_subagent_lifecycle`'s terminal-status
branch (the `Agent`/`Task` tool-call reaching `Completed|Failed|Rejected|
Canceled`). **This is only a genuine "done" for an inline `Task`** — its
tool-call stays `InProgress` for the whole run and completes when the Task
finishes.

An async `Agent` teammate is different (traced through
`claude_native/src/translate.rs` + `connection.rs`): its spawn tool-call emits
`tool_use` → `InProgress` (a `NewEntry`), then **moments later** the spawn-ack
`tool_result` ("Async agent launched successfully… agentId:… output_file:…") →
`Completed` (an `EntryUpdated`) — **while the teammate then streams
`subagent_id`-tagged entries into the parent thread for minutes afterward.** So
the `Agent` spawn call is tracked-then-immediately-removed in `active_subagents`
(this is what FORK.md #38's "a background agent leaves `active_subagents` empty"
actually means — removed at spawn-ack, not never-tracked).

Naively closing the teammate stream at that terminal status therefore **kills
the async teammate's demux stream at spawn** and the `closed_streams` overlay
then suppresses every later tagged entry — exactly the live content decision #5
says the parent-thread demux should OWN. (The reviewer pass missed this; a
follow-up trace of the tool-call lifecycle caught it.)

**Fix:** gate auto-close to `!tool_name_is_agent(tool_name)` — inline `Task`
only. The async `Agent`'s real done-signal (stop_reason / completion, already
tracked via the separate `background_agents` JSONL tail) will drive its stream
close in a later phase. The `active_subagents` removal (the pill) still fires for
both, unchanged. Pinned by `store::…::stream_auto_close_on_terminal_excludes_async_agent`.

## Deferred to phase 4 (wire) — do not forget

- **Async `Agent` stream close**: needs the completion signal, not the
  spawn-ack. Wire it when the per-stream wire lands.
- **Hydration orphans**: a session restored from the DB re-demuxes finished
  teammates' persisted tagged entries into fresh `Live` streams (`closed_streams`
  is ephemeral → empty on load). Invisible in phase 3 (strip/selection run off
  the empty `active_subagents`), but the phase-4 wire must not emit these — mark
  hydrated secondary streams closed on load, or filter at the wire.

## Verify

`cargo test -p solution_agent --lib` → 543 pass (5 new: 4 overlay-mechanics +
the Task/Agent gate). `./script/clippy -p solution_agent --no-deps` clean. No
screenshot (inert).
