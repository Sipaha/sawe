# Compaction killed running sub-agents, and their work was lost

**Date:** 2026-09-28 · **Status:** fixed. Rotation now happens in place with `/clear`; the old guard remains as the fallback.

## Symptom

In the `citeck-forge` session `xjrn2pmv`, six background executors (Agent-tool sub-agents)
stopped mid-work. The observer then told the agent their notifications would never come, and
resuming them by name failed ("their logs are not in this session").

## Mechanism

- A compaction (`SolutionAgentStore::rotate_context`) opened a **fresh claude session**: a new
  process, a new session id, a new transcript. It then `close_session`d the old one, which kills
  the old `claude` process.
- Every Agent-tool sub-agent is a **child of that process**. It died with it, and its final
  report could only ever have reached the old session.
- `has_live_background_work` did not gate any of this.
- Timeline (`sawe.log.old`):
  - 15:31: the observer asks for a handoff.
  - 15:41: the agent calls `start_compact` itself.
  - 15:42:34: `compact_session` rotates. All six sub-agent transcripts stop in that same second.

## Fix: rotate inside the running process

**`/clear` in stream-json mode.** Verified against claude 2.1.282, both standalone and through
Sawe:
- `/clear` sent as an ordinary user message wipes the conversation **inside the running
  process**. claude answers with `conversation_reset`, then an `init` carrying a **new session
  id**, then the command's own empty `result`.
- Background sub-agents and background shells keep running. Their `task_notification` arrives
  in the new conversation, which answers it on its own.

**`claude_native`.**
- `AgentConnection::clear_session_in_place` (default: unsupported) sends `/clear`.
- The update pump consumes the `init` and `result` that belong to the clear. It then **holds the
  stream until the new `AcpThread` is installed**. Without that hold, output produced right after
  the clear, such as a sub-agent's completion turn, raced into the retiring thread.
- The session is re-keyed to the new id. `SessionShared.session_id` and `.thread` are
  `RefCell`s, and the desired model and effort entries move with it. A later respawn therefore
  `--resume`s the new conversation.
- The slash-command list is copied over, because claude does not re-announce it after a clear.

**`solution_agent`.**
- `rotate_context` and `reset_context` (`/clear` from the UI) both prefer the in-place path
  whenever the live connection supports it. The graft is shared (`commit_rotation`,
  `commit_reset`) with `same_process = true`, which means:
  - nothing is closed;
  - no pool slot is released;
  - background agents are not marked killed (`set_acp_thread_same_process`).
- If the in-place clear fails, the old process-replacing path runs instead
  (`*_replacing_process`).
- **The agent calls `compact_session` from inside its own turn**, and a process can only clear
  itself between turns:
  - The tool parks the handoff on the session (`pending_rotation`). It tells the agent that the
    rotation happens when its turn ends, and that work done after that point is not in the
    handoff. It does not order the agent to stop at once: the user found that too harsh. The
    agent still decides for itself when its context is saved and calls `compact_session`.
  - The `Stopped` that ends the turn runs the rotation and sends the continuation prompt
    (`start_deferred_rotation`).
  - A `Stopped(Cancelled)`, meaning the user pressed Stop, abandons it.
- **Whether a turn is running** is read from `AcpThread::status()`, not `SessionState`. A
  background sub-agent's streaming entries flip the session to `Running`
  (`resume_on_activity`) while the main agent is idle. Gating on `SessionState` refused exactly
  the case this exists for, and a live run hit that refusal.
- **Guard.** The earlier guard (`compact::background_agents_block_reason`: refuse to compact while
  sub-agents run) now applies only to agents that cannot clear in place. There, rotating still
  replaces the process.
- **Prompts.** The handoff instructions, the ladder's ask and the judge's instructions say that
  sub-agents survive the handoff. Running sub-agents should be listed in the handoff; they are
  not a reason to wait.

## Caveat: an in-flight foreground command is interrupted

`/clear` aborts whatever tool call a sub-agent is running **in the foreground** at that moment.
In the live probe, a sub-agent's `python3 -c "time.sleep(150)…"` got exit 137 in the very second
of the clear. The sub-agent itself carries on: it saw the kill, answered, and its report reached
the new context. Background shells (`run_in_background`) are not interrupted. This is claude's
behaviour, not Sawe's. The handoff instructions tell the next context that such a failure may
just need the step repeated.

## Tests

- `claude_native/tests/mock_claude.rs`:
  - `clear_in_place_keeps_the_process_and_moves_to_the_new_session`
  - `clear_in_place_refuses_while_a_turn_is_running`
  - The mock answers `/clear` the way the real binary does.
- `solution_agent` `store::tests::in_place_rotation::*`:
  - rotation and `/clear` in place;
  - the fallback path;
  - deferral to the end of the turn;
  - background activity does not hold the rotation;
  - Stop abandons the parked handoff.
- `mcp::tests::in_place_compaction_does_not_wait_for_background_agents`
- `mcp::tests::compaction_waits_for_running_background_agents` covers the fallback guard.
- Live probe (debug build, headless, real claude): `/clear` while a sub-agent ran. The `claude`
  pid was unchanged, the session moved to the new id, the agent was not marked killed, and its
  completion was answered in the new context.
