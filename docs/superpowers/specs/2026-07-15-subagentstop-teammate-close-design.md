# Close async-`Agent` teammate tabs on `SubagentStop`

Date: 2026-07-15
Status: approved direction (A), pending spec review
Crate: `claude_native` (fix is one file); `solution_agent` unchanged

## Problem

Completed async-`Agent` teammate tabs linger in the session strip for up to ~1 h
instead of vanishing on completion. Full diagnosis + experiment:
`docs/findings/2026-07-15-teammate-tab-lingering-subagentstop.md`.

Root cause: an async `Agent` teammate closes only via a per-subagent Claude SDK
`Stop`-hook carrying the sub-agent's `agent_id` → `close_teammate_on_stop`
(`store.rs:1729`). That hook never fires: the editor registers only `Stop`
(main-agent, `agent_id=None`) + `PostToolUse`. A sub-agent's completion is the
**`SubagentStop`** event, which is registered nowhere (`build_default_hooks`,
`connection.rs:377-393`). The 2026-07-14 rework also removed the JSONL-`stop_reason`
close trigger, so the only remaining close is the 3600 s live-parent reaper.

A dev experiment (2026-07-15) confirmed: registering `SubagentStop` makes it fire
on async-`Agent` completion, and its hook input carries the sub-agent's `agent_id`
(the exact id `close_teammate_on_stop` resolves). Main `Stop` carries no `agent_id`.

## Design (approach A)

The store side already does the right thing. The pull closure
(`store.rs:3124-3147`) calls, for every hook callback:
`take_pending_for_delivery(session_id, agent_id, is_end_of_turn)` and then
`if delivered.is_none() && is_end_of_turn { if let Some(agent_id) =>
close_teammate_on_stop(session_id, agent_id) }`. So a callback that (a) sets
`is_end_of_turn = true` and (b) carries `agent_id` closes the teammate — **no
store change needed.** The entire fix registers `SubagentStop` and routes its
callback as end-of-turn.

Changes, all in `crates/claude_native/src/connection.rs`:

1. **New callback id.** Add `const HOOK_CALLBACK_SUBAGENT_STOP: &str = "sub_stop";`
   next to `HOOK_CALLBACK_STOP` (`connection.rs:63`).

2. **Register the hook.** In `build_default_hooks` (`connection.rs:375-393`), add a
   `"SubagentStop"` entry with `matcher: None`, `hook_callback_ids:
   [HOOK_CALLBACK_SUBAGENT_STOP]`, `timeout: 30_000` — mirroring the `Stop` entry.

3. **Treat it as end-of-turn.** In the `HookCallback` handler
   (`connection.rs:1530`), change
   `let is_end_of_turn = callback_id.as_str() == HOOK_CALLBACK_STOP;`
   to also match the SubagentStop callback:
   `let is_end_of_turn = matches!(callback_id.as_str(), HOOK_CALLBACK_STOP | HOOK_CALLBACK_SUBAGENT_STOP);`
   `agent_id` is already read from `input["agent_id"]` (`:1536`), which the
   `SubagentStop` payload carries. This routes straight into the store pull →
   `close_teammate_on_stop`. The degenerate-tool-call nudge (`:1554`) stays
   guarded by `agent_id.is_none()`, so a SubagentStop (always has `agent_id`)
   never triggers it.

4. **Correct response event name; do NOT block a sub-agent's stop.** In
   `build_hook_response` (`connection.rs:410-448`): today `is_stop = callback_id ==
   HOOK_CALLBACK_STOP` drives both the `hookEventName` (`"Stop"` vs `"PostToolUse"`)
   and the `decision: "block"` injection. Extend it so the SubagentStop callback
   maps `hookEventName` to `"SubagentStop"`, but **keep `decision: "block"` only
   for the main `Stop`** (we must not force a finished sub-agent to keep
   generating). Concretely: compute `event_name` from the callback id
   (`HOOK_CALLBACK_STOP => "Stop"`, `HOOK_CALLBACK_SUBAGENT_STOP => "SubagentStop"`,
   else `"PostToolUse"`), and gate the `decision:block` insert on `callback_id ==
   HOOK_CALLBACK_STOP` only. In practice a sub-agent almost never has a pending
   follow-up, so the `pending.is_none()` no-op path runs and the response is a
   plain success — but this keeps the targeted-delivery path correct if a message
   was ever aimed at a specific teammate.

That is the whole fix. `solution_agent` (`close_teammate_on_stop`,
`rebuild_streams`, the reaper) is unchanged — the reaper stays as the lost-hook
backstop for the genuinely-lost case.

## Edge cases / nuances

- **`background_tasks[0].status: "running"` at SubagentStop time.** Ignore that
  field; the authoritative signal is the `SubagentStop` event + its `agent_id`.
  We key off the event, not the status.
- **Registration race.** If the async-`Agent` `BackgroundAgent` isn't registered
  yet when SubagentStop fires, `close_teammate_on_stop` buffers into `pending_stop`
  and drains at registration (`teammate_reconciler.rs`) — existing behavior,
  unchanged.
- **Inline `Task` unaffected.** A `Task` never registers a `BackgroundAgent`, so a
  SubagentStop for a Task parks harmlessly in `pending_stop`; Task already closes
  on tool-call terminal. No regression.
- **Detached-background completion.** Must verify (test/e2e) that SubagentStop is
  delivered — and the tab closes — when a background agent finishes AFTER the
  parent turn has gone idle, not only when it finishes within the parent turn (the
  experiment only covered the in-turn case).

## Testing

- **Unit (`connection.rs`):**
  - Extend `build_default_hooks_registers_post_tool_use_and_stop` (or add a sibling)
    to assert a `"SubagentStop"` entry is registered with the SubagentStop callback id.
  - `build_hook_response` for the SubagentStop callback with a pending message emits
    `hookSpecificOutput.hookEventName == "SubagentStop"` and **no** `decision` key;
    the main `Stop` still emits `"Stop"` **with** `decision: "block"`.
- **Behavioral e2e (dev headless MCP):** create a session, dispatch one async
  `Agent` sub-agent (echo), confirm its teammate pill appears while running and
  **vanishes promptly** on completion (screenshot before/after; dev log shows the
  SubagentStop callback → a teammate close, no 1 h wait). Then repeat with a
  background agent that finishes after the parent turn goes idle to cover the
  detached case.

## Out of scope

- Approaches B (restore JSONL/terminal-state close) and C (shorten the reaper) —
  A makes them unnecessary; the reaper stays as the lost-hook backstop.
- Any change to the reaper windows / `close_teammate_on_stop` internals.
