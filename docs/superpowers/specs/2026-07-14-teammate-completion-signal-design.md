# Teammate completion via an authoritative signal — design

**Date:** 2026-07-14
**Status:** design (approved core + decisions; ready for implementation plan)
**Crate:** `solution_agent` (+ a small seam in `claude_native`)

## Problem

The teammate/subagent **stream (tab) lifecycle** has been the single largest
source of recurring bugs in `solution_agent`. Of ~10 distinct fixes in this
area, **6 were "a tab stayed Live after it should have closed"** (zombie tabs on
restart, frozen tabs after GC, killed agents still painting "running", a tab
stuck Live for ~1 hour). The user's framing — "костыли поверх костылей" — is
correct: this is one root re-surfacing, not twenty independent bugs.

Concrete current instance: a background `Agent` sub-agent finished and its
subprocess exited, but its teammate tab stayed `state: live`.

## Root cause

For an async `Agent` teammate, **"done" is never observed — it is inferred**,
badly, from tailing the agent's JSONL file:

- The primary detector (`refresh_background_agent_snapshot`,
  `store/teammate_reconciler.rs:243-333`) tails the JSONL's **last line** and
  matches `stop_reason` against a 4-value allow-list
  (`is_terminal_stop_reason`, `background_agent.rs:303-308`). It misses whenever
  the terminal `assistant` message is not literally the file's last line (a
  trailing `tool_result`/`user` row overwrites the terminal snapshot — this is
  the observed stuck tab, since the agent ended by running `git commit`), when
  the fs-watch event is coalesced (200 ms debounce), or when the reason is
  outside the allow-list.
- The DB column `solution_session_background_agent.stop_reason` is **dead
  capacity**: written once at registration as `None`
  (`teammate_reconciler.rs:1024-1037`) and thereafter only *deleted*, never
  updated. It carries no liveness information.
- When the primary detector misses, the backstops (`tick_background_agents`,
  `reconcile_finished_teammate_streams`) close on stale mtime — but under a live
  parent that window is **up to 3600 s** (`store.rs:79`). So a missed completion
  = a tab stuck Live for up to an hour.

There are **five independent heuristic detectors** all racing to populate the
`closed_streams` overlay; none observes the actual completion event. Stream
*liveness* is otherwise cleanly derived (`rebuild_streams` rebuilds the whole
map each mutation); *completion* is the weak, guessed input.

## The enabling fact

An **authoritative, per-subagent "finished" signal already reaches the editor**
and is only half-used.

The Claude Agent SDK control-protocol `Stop` hook (registered today in
`claude_native/connection.rs:375-394`, callback id `stop_inj`) fires over the
subprocess's stdio control channel when the model produces a final message
without invoking another tool — the exact terminal boundary. It is handled at
`connection.rs:1517-1575`:

```rust
let is_end_of_turn = callback_id.as_str() == HOOK_CALLBACK_STOP;
let agent_id = input.get("agent_id").and_then(|v| v.as_str());
```

- `agent_id` is `Some(<hex>)` for an Agent-Teams **subagent**, `None` for the
  main agent.
- The `<hex>` is the **same id space** as `background_agent.rs`'s parsed
  `agentId:` announcement (`BackgroundAgentId`). Verified against the live log:
  every `solution_session_background_agent.agent_id` in the DB
  (`a3f0bd2978e3b0434`, `ae5c775877152a5dd`) appears verbatim as a hook-pull
  `agent_id`.
- The mapping to the stream already exists as data:
  `hook agent_id → BackgroundAgentId → BackgroundAgent.parent_tool_use_id
  (toolu…) → StreamId::Teammate(toolu…)` (`background_agent.rs:129-134`,
  `model.rs:524`).
- The signal is synchronous and in-process (the SDK **blocks** on the editor's
  `ControlResponse`) — strictly more reliable and timely than the JSONL tail.
- It is already consumed, but only for message routing: the `Stop` hook drives
  `take_pending_for_delivery` (`store.rs:1583-1650`), which injects a queued
  follow-up for the subagent (and blocks the stop) **or does nothing**. The
  "does nothing" branch is exactly "this subagent is idle with no more work" —
  today it just fails to close the tab.

## Design

**One authority per teammate type, both firing an immediate close; one backstop
only for the case no signal can exist.**

### Core — async `Agent`: close on the subagent `Stop` hook

Extend the existing `Stop`-hook seam (do **not** add a `SubagentStop` hook — the
`Stop` hook already carries the subagent `agent_id`; a dedicated registration is
redundant). In the handler, when `is_end_of_turn && agent_id.is_some()`:

1. As today, `take_pending_for_delivery`: if a queued message targets this
   subagent, inject it and block — the teammate is **not** done, stays `Live`.
2. **New:** if nothing is delivered (the subagent is idle), resolve
   `background_agents[BackgroundAgentId::new(agent_id)] → parent_tool_use_id` and
   `close_stream(StreamId::Teammate(toolu), "done")` **immediately**.

Wiring: add a store callback sibling to `HookPull` (e.g. `HookAgentStop`), set in
`subscribe_to_session` (`store.rs:3065-3096`) next to `set_store_pull`, so the
close path is not tangled with message-queue routing. It resolves the session
via `session_id_for_acp` (`store.rs:1516`) and reuses the existing
`close_teammate`/`close_stream` path (`model.rs:898`).

### Inline `Task`: unchanged (already authoritative)

Inline `Task` subagents already close on their spawn tool-call reaching a
terminal status (`apply_subagent_lifecycle`, `teammate_reconciler.rs:905-945`).
That is a real signal, not a heuristic — keep it. Result: two authoritative
close signals (tool-call-terminal for `Task`, `Stop`-hook for `Agent`), both
funnelling into the single `close_stream`.

### Remove the JSONL "done" detector

Delete the JSONL tail as a **close trigger**: the terminal-branch close in
`refresh_background_agent_snapshot` (`teammate_reconciler.rs:296-318`) and the
`is_terminal_stop_reason` gating that feeds it. The JSONL tail stays only for
**content** (labels, progress snapshot). This removes the last-line fragility
and the allow-list entirely.

### Kill path: unchanged trigger, immediate close

`mark_background_agents_killed` (reconnect/crash via `set_acp_thread(None)`,
`model.rs:698`, commit `df1ff88091`) stays — a dropped thread is its own
authoritative signal with no hook analogue (a dead process sends no more control
messages). Per the "done closes immediately" principle, close the killed
teammate's stream on the kill event itself rather than waiting for the reaper.

### Collapse the reapers into one dead-process backstop

Replace the two stale-mtime reapers (`tick_background_agents`,
`reconcile_finished_teammate_streams`) with a **single backstop** that closes a
teammate when the authoritative signal is absent, in two cases:

1. **Parent subprocess confirmed dead** — close immediately (no hook can arrive;
   this overlaps the kill path).
2. **Stale mtime past a short window** — safety net for a *lost* `Stop` hook
   while the parent is still alive. Because the hook now handles every normal
   completion, this window can be short (order of a minute or two), not the old
   3600 s live-parent cap — but it must remain non-zero so a dropped hook cannot
   strand a tab forever.

The backstop is a safety net, not a primary path; the hook closes the common
case immediately.

### Race: `Stop` before registration

The `Stop` hook can arrive before the `agentId:` announcement has registered the
`BackgroundAgent`. Buffer it: a small `pending_stop: HashSet<BackgroundAgentId>`
(or map to reason) on the session; the registration site
(`apply_subagent_lifecycle`, `teammate_reconciler.rs:972-1063`) checks the buffer
and closes immediately if a stop is already pending.

## Decisions (resolved)

1. **JSONL "done" detector — removed** (kept only for content). No second path
   to "done".
2. **No cold-load persistence.** No teammate tab survives a restart — the
   existing `hydrate_streams_main_only` collapse to Main-only is kept as-is; done
   tabs never reappear. "Done" = immediate close while live; nothing to persist.
3. **`pending_stop` buffer** for the Stop-before-registration race — accepted.

## Out of scope

- Persisting done-ness / restoring finished teammate tabs across restart
  (explicitly not wanted).
- Removing the `StreamState::Done` wire variant. With killed teammates also
  closing immediately, `Done{reason}` becomes nearly vestigial, but dropping it
  is a wire change (v6→v7 + mobile lock-step) — a possible later cleanup, not
  this change.
- Wiring targeted-message-to-subagent to a UI/MCP entry point
  (`QueueTarget::Subagent` plumbing exists but is unused).

## Affected code (anchors)

- `claude_native/connection.rs:1517-1575` — invoke the new `HookAgentStop`
  callback on subagent `Stop`; `:54-56` — new callback type beside `HookPull`.
- `solution_agent/store.rs:3065-3096` — set the callback in
  `subscribe_to_session`; `:1516`,`:1583` — resolution + delivery seam.
- `solution_agent/store/teammate_reconciler.rs:243-333` — drop JSONL terminal
  close; `:1082`,`:1258` — collapse the two reapers into one dead-process
  backstop; `:972-1063` — `pending_stop` check at registration.
- `solution_agent/background_agent.rs:303-308` — `is_terminal_stop_reason` no
  longer a close trigger (retain only if still needed for content labelling).
- `solution_agent/model.rs:698`,`:898`,`:524` — kill-path immediate close;
  `close_stream`; `background_agents` map + new `pending_stop` field.

## Testing / verification

- **Unit:** hook-driven close (subagent Stop with nothing to deliver →
  `Teammate` stream closed); Stop-with-pending-delivery keeps it Live;
  `pending_stop` race (Stop before registration → closed on registration); kill →
  immediate close; dead-process backstop still closes an agent that never sent a
  Stop.
- **Live capture** (debug MCP): dispatch a background `Agent`, confirm its tab
  closes promptly on completion (not after the old ~1 h window). This resolves
  the last correlation doubt (hook `agent_id` ↔ teammate stream) end-to-end.
- Existing teammate/streams tests must stay green.

## Note

This does not resurrect any tab already stuck under the old code; the current
stuck tab clears on the next editor restart (needed for wire v6 anyway). The
change prevents recurrence.
