# Mobile: session state stuck at `Running` after backgrounded transition

**Repo:** `sawe-mobile` (sibling)
**Reported:** 2026-05-18 (user, in-session)
**Status:** unreproduced in isolation; logged for next mobile-side pass.

## Symptom
The chat-header state pill stays at **Running** after a turn finishes —
the session has actually returned to **Idle** server-side (next message
sends fine, no actual turn in flight), but the phone UI never repaints
the badge.

## Hypothesis
Reproduces when the `Running → Idle` transition fires **while the
Android app is backgrounded**. Two plausible mechanisms, either or both:

1. **WS subscription gap.** Backgrounded Android may suspend the OkHttp
   pump or the StateFlow collector long enough for the
   `agent_session_state_changed` notification carrying the `Idle`
   transition to be dropped. On resume, the client never re-fetches
   session state, so it stays on the last cached `Running`. (R-6a
   added reconnect + subscription auto-restore for socket drops; this
   may be a separate gap for the "socket stayed open but the collector
   was paused" case.)
2. **Notification arrived but VM ignored.** If the notification did get
   buffered and replayed on resume but the VM's reducer treats it as
   stale (e.g. ordering by `last_activity_at_ms` and the live event
   carries an older timestamp than something else already applied),
   the Idle event silently no-ops.

## To investigate
- Repro: open a session on phone, send a long-running prompt, lock
  screen / switch app, wait for the desktop or another client to
  confirm the session went Idle, foreground the phone, check pill.
- Logcat filters: `agent_session_state_changed`, the `ConnectionState`
  flow, whatever VM owns `state` derivation.
- Server-side cross-check: on the live MCP socket, call
  `solution_agent.get_session({session_id})` after the supposed Idle
  transition — confirm server reports `Idle` so we know the gap is
  on the client.
- Audit `OkHttpRemoteTransport` / `RemoteClient` for any
  `ApplicationLifecycleObserver` / `ProcessLifecycleOwner` ties — if
  the WS pump is bound to UI scope rather than a service / coroutine
  scope that survives Stopped, the collector dies and rejoins late.

## Quick mitigation (if the deep fix is large)
On `onResume` / `Lifecycle.Event.ON_START`, re-issue
`solution_agent.get_session({session_id})` for the active session and
replace the local `state` with the server's truth. Cheap, narrow,
masks both hypotheses above.

## Why this matters
Stuck `Running` makes the send button disabled (the compose-bar gate
keys off the in-memory state). User can't see why their next message
isn't going through.

## Status
Deferred to the next mobile-side phase. Parallel Claude session is
mid-flight on an AGP-9 + connection-store refactor as of 2026-05-18
17:59 (PID 2601296) — fold this into their work or pick up after they
land.
