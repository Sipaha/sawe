# The Observer went silent because "is this background work alive?" had three answers

**Date:** 2026-09-15
**Status:** fixed — `ef782084ad`, `b893fd340b`, `8ad756f76f`, `dc828a5285`
**Symptom:** session `qv09rxtm` ("Columns Migration", solution 38) sat idle for 39
minutes with supervision enabled and the Observer never fired. The operator also
reported seeing no background agents in the UI.

## What was actually wrong

Three things, of which only the first was the trigger.

### 1. The managed-agent watcher was pinned to a directory the session had left

`SolutionAgentStore::ensure_background_agent_watcher` arms one `fs.watch` per
session on

```
~/.claude/projects/<encoded-cwd>/<acp-session-id>/subagents/
```

and guarded itself with `has_agent_watcher(session_id)` — keyed on the *session*.
That path embeds the **ACP session id**, which is not stable for the life of a
session: `rotate_context` (compaction), `reset_context` (`/clear`) and
`resume_session` each mint a new one, and claude then writes into a new
`subagents/` directory. Nothing ever disarmed the watcher, so from the first
compaction onward it watched a directory that would never change again. The
incident session had six ACP-session directories; the live one was created at
10:35:53, at dispatch time.

Registration is followed by an inline `refresh_background_agent_snapshot`, so the
agent could still pick up a first snapshot — but every subsequent line, **including
the terminal `stop_reason`**, was lost. The agent finished at 10:46:43 with
`stop_reason: end_turn` in its JSONL and the editor never learned it.

### 2. `fs.watch` was the only source of snapshots

`tick_background_agents` already walked every registered agent every 5 s, but only
to reap. A missed watch event therefore cost an hour, not a tick.

### 3. "Live background work" was asked three different ways

| caller | predicate | answer for "registered, never observed" |
|---|---|---|
| Observer gate (`tick_supervisor`) | `is_messageable()` | **alive, forever** (`latest.map_or(true, …)`) |
| "Agent finished" notifier | `is_messageable()` | **alive, forever** |
| stuck-turn watchdog (`turn_is_wedged`) | `is_messageable() && background_work_shows_liveness(quiet)` | a 15-minute cutoff measured from a snapshot that, in that state, does not exist |

So one agent whose JSONL was never tailed held `has_live_background_work` true
until the 1-hour reaper removed its map entry. **Prediction made and confirmed:**
`BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS` from `registered_at` = 11:35:53, and the
Observer fired at 11:35:55, `trigger_count` 7→8.

## Two claims from the first pass that did not survive

Recorded because both were stated with more confidence than the evidence carried.

**"The DB proves it was never observed."** The diagnosis cited
`last_seen_label` / `last_mtime_ms` / `stop_reason` being NULL in
`solution_session_background_agent`. Those columns are written exactly once, at
registration, always NULL — no refresh path persists a snapshot. Every agent
looks like that. The conclusion happened to be right, but only the reaper-timing
prediction actually supported it. This is what
`solution_agent.get_background_agents` now exists to fix.

**"The stream fold is the third divergent predicate, failing closed."** It does
not. A registered, never-observed agent IS folded into its teammate stream and
renders "Starting… / no output yet" (now pinned by
`rebuild_streams_folds_an_agent_that_has_never_been_snapshotted`). What the
operator saw was the tab already closed by the subagent `Stop` hook after the
agent completed, while its registry entry lingered unobserved.

**"`watcher.add` fails permanently on a missing directory."** Also wrong, and it
briefly shipped as a `fs.create_dir` before watching. `FsWatcher::add` files a
non-existent path under `pending_registrations` and polls until it appears, then
promotes it to a real native registration (`poll_path_until_created`,
`crates/fs/src/fs_watcher.rs`). Arming ahead of claude was already safe; the
pre-creation was removed in `dc828a5285`.

## The fix

**Re-arm on rotation.** The arm-once guard keys on the watched **path**, not the
session, so a session whose ACP id changed re-arms on its current directory at the
next dispatch. `TeammateWatchers` stores `(PathBuf, Task<()>)` per session and
exposes `agent_watcher_path`.

**The tick is a second source.** `tail_unobserved_background_agents` runs at the
top of every 5 s pass and tails the JSONL of any agent that is not terminal, not
killed, and has no snapshot newer than `BACKGROUND_AGENT_TAIL_FALLBACK_SECS`
(30 s). `tail_jsonl` reads forward from `last_offset`, so a re-tail with nothing
new is a `stat`; a healthy agent is refreshed by the watcher within ~200 ms and is
skipped. `fs.watch` is now an optimisation.

**One liveness predicate.** Liveness is a property of the *evidence*, not of the
caller, so it is no longer a per-caller number:

```rust
BackgroundAgent::vouches_for_parent(now)
  killed / walled / terminal stop_reason -> false at once
  observed        -> snapshot mtime aged against BACKGROUND_AGENT_QUIET_MAX_SECS
                     (60 min — hardening #9: a long quiet tool call inside a
                     teammate is not death; the same budget the reaper uses, so
                     "vouches" and "tracked" expire together)
  never observed  -> registered_at aged against
                     BACKGROUND_AGENT_UNOBSERVED_GRACE_SECS (5 min), because
                     nothing has corroborated this agent yet. Short is safe only
                     because the tick now tails every agent itself.
```

`SolutionSession::has_live_background_work(now)` wraps that together with the
running-shell half, and the Observer gate, the watchdog and the notifier all read
that one function. `background_work_shows_liveness` is deleted.

`is_messageable` is renamed **`transcript_is_open`** and documented as what it
actually answers: a bookkeeping fact ("the transcript has not ended") that never
expires and decides tab rendering, reconnect kill-marking and the
permission-mode gate. It must not be mistaken for liveness again — an agent whose
JSONL was never readable satisfies it forever.

## Verification

Unit: 895 tests green, `./script/clippy` clean, `cargo fmt --all --check` clean.

End-to-end, against a live headless editor driven over MCP. Same scenario on both
binaries — dispatch a background `Agent`, `reset_context` to mint a new ACP
session id, dispatch another, and ask whether the editor ever learns the
sub-agent finished:

| binary | scenario | result |
|---|---|---|
| pre-fix | before rotation | terminal `stop_reason` seen |
| pre-fix | **after rotation** | **never seen** — 200 s later still `observed=false`, `vouches_for_parent=true`, `has_live_background_work=true`, with the sub-agent long finished |
| fixed | before rotation | seen at +17 s |
| fixed | **after rotation** | seen at +17 s, `vouches_for_parent` flips false |

The pre-fix binary for that A/B was the current tree with exactly the two
production hunks reverted, so the only variable is the fix.

## New observability

`solution_agent.get_background_agents {session_id}` (solution-scoped) reports, per
tracked agent: `observed` (was a snapshot ever parsed out of its JSONL),
`last_activity_label`, `last_mtime_ms`, `stop_reason`, `killed`, `usage_limited`,
`vouches_for_parent`, `parent_tool_use_id` — plus the session-level
`has_live_background_work` roll-up. Nothing else on the wire distinguished "this
teammate is working" from "we registered it and have never read a line of it",
which is why the first diagnosis had to fall back on SQLite rows that could not
answer the question.
