# Teammate completion via the authoritative Stop hook — shipped

**Date:** 2026-07-14
**Status:** shipped (merged to main)
**Crate:** `solution_agent` (+ the existing `claude_native` hook seam, unchanged)

## What shipped

The teammate/subagent tab lifecycle no longer GUESSES completion by tailing a
JSONL file (the root of the recurring zombie/stuck-tab bugs — a tab could sit
`Live` for up to an hour). Completion is now AUTHORITATIVE:

- **Async `Agent` teammate** closes on the Claude SDK `Stop` hook. The hook
  already reaches the editor (`claude_native/connection.rs` `Stop` callback,
  carrying the subagent `agent_id`); the store's existing `HookPull` closure now,
  when a subagent `Stop` fires with nothing left to deliver, resolves
  `agent_id → BackgroundAgentId → parent_tool_use_id → StreamId::Teammate` and
  calls `close_teammate_on_stop` → `close_stream` immediately. A `pending_stop`
  buffer covers the Stop-before-registration race.
- **Inline `Task` teammate** keeps closing on its tool-call terminal status
  (already authoritative — unchanged).
- **Killed teammate** (parent subprocess dropped → `mark_background_agents_killed`)
  closes immediately, no lingering `Done { killed }` tab.
- The JSONL `stop_reason` is no longer a close/reap trigger (still stored, still
  feeds `is_messageable`/supervisor gating/labels).
- The stale-mtime reaper is demoted to a lost-hook/dead-process **backstop** —
  and its live-parent window stays LONG (~3600 s), NOT short: the `Stop` hook
  fires only at end-of-turn, so a live subagent running a long silent tool call
  must not be mistaken for a lost hook (hardening #9). See the design spec's
  corrected "Collapse the reapers" section.

Design: `docs/superpowers/specs/2026-07-14-teammate-completion-signal-design.md`.
Plan: `docs/superpowers/plans/2026-07-14-teammate-completion-signal.md`.

## Verification chain

- **589 `solution_agent` unit tests** green, including the real production-seam
  test `native_pull_subagent_end_of_turn_closes_teammate_stream` (drives the
  actual `ClaudeNativeConnection` store-pull closure with the exact
  `Some(agent_id) + is_end_of_turn=true` combo → stream closed), the
  Stop-before-registration race through the real `apply_subagent_lifecycle`
  drain, kill-immediate-close, and JSONL-no-longer-closes.
- **Signal correlation confirmed against the live editor log:** every
  `solution_session_background_agent.agent_id` in the DB appeared verbatim as a
  `hook pull` `agent_id` — the hook hex IS the `BackgroundAgentId`.
- **Branch binary boots** headless (`script/run-mcp --debug --headless` →
  "mcp socket ready").
- **Whole-branch review (opus)** caught and forced the fix of a real regression
  (I-1/I-2: an earlier draft shortened the live-parent backstop, re-regressing
  hardening #9 — reverted to the long window).

The two halves of the wire (claude sends the `Stop` hook with the agent_id; the
store closes the stream) are each verified independently; their composition in a
real run validates naturally on the next launch of the built binary — dispatch a
background `Agent` that finishes with a trailing tool call (the exact old
stuck-tab shape) and its tab now leaves the strip promptly instead of after the
~1 h window.

## SDD execution

Built subagent-driven: 4 code tasks + a test-coverage fix (Task 1) + the I-1/I-2
regression fix, each with a per-task spec+quality review; then the whole-branch
review. Ledger: `.superpowers/sdd/progress.md`. Accumulated Minors (all
follow-up, none merge-blocking): `pending_stop` has no cleanup path besides the
registration drain; N+1 `rebuild_streams` in `mark_background_agents_killed`;
`seed_cold_session` debug tool can no longer paint `Done { killed }` (doc stale).
