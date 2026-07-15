# Completed async-`Agent` teammate tabs linger ~1 h — missing `SubagentStop`

Date: 2026-07-15
Status: **root cause confirmed + fix approach (A) validated by a dev experiment; NO fix started** (awaiting user go-ahead to implement A)
Reporter: user pointed at ~12 sub-agent pills in the live session strip and asked "все активные? не баг?"

## Verdict: BUG (regression of the 2026-07-14 teammate-completion rework)

The tabs are **not active** — all were finished sub-agents. They should have vanished
promptly; instead they sit until a 1-hour reaper. This is the fragile lifecycle the user
flagged (memory `teammate-stream-lifecycle-architecture`).

## Root cause (confirmed by live log + two code traces)

Sub-agents dispatched via the `Agent` tool are classified as **async `Agent` teammates**
(`tool_name_is_agent`, `store.rs:443` — matches `"agent"` case-insensitive → `is_async_agent`).
By design such a tab closes **only** on the child's Claude SDK **`Stop` control-hook carrying
the sub-agent's `agent_id`**: `is_end_of_turn = callback_id == HOOK_CALLBACK_STOP`
(`connection.rs:1530`), `agent_id = input["agent_id"]` (`:1536`) → store pull
(`store.rs:3138-3140`) → `close_teammate_on_stop` (`store.rs:1729`) → `close_stream` inserts
`StreamId::Teammate` into `closed_streams` (`model.rs:934`) → `rebuild_streams` shift-removes
it (`model.rs:771`).

**That `Stop` signal never fires for sub-agents.** Live log of session `gf2kf7wn`
(`~/.spk/sawe/logs/sawe.log`, `store.rs:1608` "hook pull" line):
- `end_of_turn=true` fired **38×, ALL with `agent_id=None`** (Main session only).
- Across **all 25** distinct sub-agent `agent_id`s: `end_of_turn=true` = **0×**; only
  `end_of_turn=false` (those are `PostToolUse` hooks during the sub-agent's work).
- Zero teammate close/reap log events.

The editor registers only `Stop` + `PostToolUse` (`build_default_hooks`, `connection.rs:377-393`).
**`SubagentStop` is registered nowhere** (grep-clean across the tree). In Claude Code a Task/
Agent sub-agent's completion is the `SubagentStop` event; the plain `Stop` hook fires for the
top-level agent only (hence `agent_id=None`). So `close_teammate_on_stop` is never invoked for
these teammates, their streams stay out of `closed_streams`, and every `rebuild_streams`
re-materializes the pill from the sub-agent's `subagent_id`-tagged entries.

## Why they linger ~1 h instead of a few seconds

The same 2026-07-14 teammate-completion rework **removed** the older JSONL-`stop_reason` close
trigger (to stop false-reaping silently-working agents,
`teammate_reconciler.rs:1102-1105`). The only remaining close path is now the reaper backstop,
whose **live-parent** window is deliberately **3600 s** (`BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS`,
`store.rs:79`; branch `teammate_reconciler.rs:1129`). Dead-parent/killed = 420 s (120+300).
Reaper ticks every 5 s (`store.rs:592`) but ages from JSONL mtime / `registered_at`. Net: a
finished `Agent` tab clears ~1 h after its last output. Not gated on the parent turn — the
close would fire instantly if the `Stop` hook arrived; it simply never does.

Inline **`Task`** teammates are unaffected — they close on their spawn tool-call going terminal
(`teammate_reconciler.rs:917-924`), no hook needed. Only the async `Agent` path is broken.

## Experiment (2026-07-15, dev build, reverted): approach A CONFIRMED

Temporarily registered a `SubagentStop` hook + logged every hook callback's raw
input in a dev build, then drove a real editor claude session (scratch solution,
`Agent` tool) to dispatch one async `Agent` sub-agent. Result:

- **`SubagentStop` fires** on the sub-agent's completion (`connection.rs` hook
  callback, id we registered).
- Its raw input carries the sub-agent's **`agent_id`** — the SAME id the close
  path already keys on (matched the `hook pull agent_id=Some(...)` id). Full
  payload also had `agent_type`, `agent_transcript_path`, `last_assistant_message`,
  and `background_tasks:[{id, type:"subagent", status:"running", ...}]`.
- The Main **`Stop`** payload has **no** `agent_id` (confirms Stop = main-only).
- The session used the **`Agent`** tool (`subagent_id`-tagged entries absent →
  fully-detached async Agent = the exact buggy case), not `Task`.

Conclusion: **A is viable.** The handler already reads `input["agent_id"]`
(`connection.rs:1536`); registering `SubagentStop` and treating its callback as
`is_end_of_turn` routes straight into the existing
`close_teammate_on_stop(session, agent_id)` (`store.rs:3138-3140`), which resolves
`agent_id → BackgroundAgentId → parent_tool_use_id → StreamId::Teammate` via
`background_agents` (populated for async Agents). Nuances to handle in the fix,
not blockers: `background_tasks[0].status` reads `"running"` at SubagentStop time
(key off the event + agent_id, not that field); and confirm the detached-background
case (SubagentStop firing after the parent turn ends) still delivers.

## Fix directions (NOT started — user chooses)

- **(A)** Register `SubagentStop` and route it to `close_teammate_on_stop` (authoritative
  per-subagent completion) — IF Claude Code's `SubagentStop` payload carries an id that
  resolves to `parent_tool_use_id`. Verify with a dev-build experiment first.
- **(B)** Restore a terminal-state close for async `Agent` (tool-call terminal / JSONL
  `stop_reason`) — but that is exactly what the rework moved away from (false positives).
- **(C)** Hybrid: `SubagentStop` primary + a short reaper backstop instead of 1 h.

Related: [[teammate-stream-lifecycle-architecture]],
`findings/2026-07-14-teammate-completion-signal-shipped.md` (the rework this regresses).
