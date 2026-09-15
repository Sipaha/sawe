# A parent parked on its own background agents cannot be reached by the queue

Date: 2026-09-15
Status: fixed (delivery over stdin, and the status row now names the state)

## The report

The user watched an idle-looking session swallow two messages. Reconstructed
from their own editor log (`solution_agent::store`), session `16sq3qml`:

| message | enqueued | delivered | latency | why |
|---|---|---|---|---|
| "приостановись" | 13:08:42 | 13:13:37 | 4m55s | main agent inside a long foreground tool — the documented path; the queued bubble's "Delivered when Bash finishes" hint was truthful |
| a `/compact` prompt | 13:17:44 | 13:26:17 | 8m33s | **parked** |

In that second window the main agent fired **zero** hooks; its sub-agents fired
eight. That is the whole bug: `pending_messages` is drained only by the hook
pull, and the main agent's hooks fire at a tool boundary (`PostToolUse`) or at
end of turn (`Stop`). A parent that has dispatched async Agents and is waiting
for their `<task-notification>` runs no tools and ends no turn, so for
`agent_id=None` the only delivery channel never fires.

## Measured on a probe editor

Isolated `SAWE_HOME`, one background agent dispatched with
`run_in_background: true`, parent told to wait quietly.

| step | before | after |
|---|---|---|
| `send_message` while parked | queued; still undelivered after 60s | user entry in the transcript at **1.0s**, agent answered at **2.0s** |
| `send_message` inside a foreground `Bash` | queued, hint "Delivered when Bash finishes" | unchanged |
| status row while parked | `Thinking… 8m` | `Waiting on 2 agents` |
| status row while the parent is Idle and a teammate runs | `Done in 3s` | `Waiting on 1 agent` |

## The fix, and the constraint that shapes it

`ClaudeNativeConnection::write_user_message_to_stdin` writes the same
`InputMessage::user_blocks` frame `prompt()` writes, without arming `prompt_tx`
— the message joins the turn already in flight. `store::queue::inject_while_parked`
calls it **instead of** enqueuing when all of these hold: origin `User`, target
`Main`, claude-native, no permission prompt was just unblocked, and the session
is parked.

The narrowing is the entire safety argument, and it is not stylistic. Stdin has
no recall: measured in the previous session, injecting and then pressing Stop
0.3s later still got the injected ask answered after `Stop`. `Stop` discards
`pending_messages`; it cannot unwrite a line the subprocess has read. So
draining the QUEUE into stdin would silently break "Stop discards what I typed".
Because an injected message never enters the queue, that guarantee is untouched,
and the residual exposure shrinks to "the user pressed Stop within about a
second of sending" — the same window they already have on an idle session.

The transcript push (`push_user_message_entry`) is not cosmetic. Measured
without it: the assistant's answer appears and the question does not.

## The bug inside the bug — `in_progress_tool` counted teammates

The parked predicate is `Running && no in-progress MAIN tool && has_live_background_work`.
It never fired on the probe. `status_row::in_progress_tool` scans `s.entries`,
which is the flat ingest buffer carrying **every** source, and it did not filter
`subagent_id` — so it reported the teammate's `Bash` as the parent's:

```
PROBE session=d30c3ot2 not parked: state=Running in_progress=Some(("Bash", …)) live=true
```

Two user-visible lies followed from the same line, both live before this change:

- the status row said `Running Bash · 2m` for a parent that was running nothing;
- the queued bubble promised "Delivered when **Bash** finishes" — a boundary
  that fires the TEAMMATE's hook, which by design does not drain a
  `QueueTarget::Main` bundle. The hint named the one event that could not
  deliver the message.

Both callers ask about the main agent (a teammate tab gets its status from
`subagent_status`), so the scan now filters to `subagent_id.is_none()`.

## Two halves of "the session itself is idle"

The user asked for «отдельный статус … когда основная сессия сама по себе
простаивает, но есть фоновая работа». Measuring it showed the state has two
halves, and the common one was not the one the plan assumed. Over one 150s
teammate run the parent was `Running` (parked) for a handful of two-second
samples and `Idle` for all the rest: claude usually ENDS its turn after
dispatching an async Agent and is woken by the notification. So the label lives
in both arms — `Waiting on 2 agents · 3m12s` while Running, `Waiting on 1 agent`
while Idle — and `Done in 3s` no longer claims completion while work continues.

## Gotchas for the next probe session

- The probe editor's log rotated every ~60s under a flood of
  `gpui/window.rs … window not found` ERRORs in headless mode, which threw away
  the INFO lines being measured. `ZED_LOG="solution_agent=info,gpui=off"`.
  (`ZED_LOG` / `RUST_LOG`; without either, only ERROR is recorded.)
- `solution_agent.get_session`'s `entries` are the demuxed **Main** stream, so a
  teammate's in-flight tool is invisible there — which is exactly why the
  first three probe runs could not see why the predicate refused to fire. Read
  `has_live_background_work` from `solution_agent.get_background_agents` and the
  parent's tool state from the editor's own log, not from the Main stream.
- `workspace.screenshot` is per-solution-socket only; on the global socket it
  returns an error frame with no image.
- A background agent dispatched with `run_in_background: true` does NOT get a
  `teammate` stream in `get_session` until it has produced parent-visible
  output — do not use "a teammate stream exists" as a parked detector.
