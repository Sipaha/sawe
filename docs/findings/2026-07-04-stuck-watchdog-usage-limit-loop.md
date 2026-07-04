# Stuck watchdog reconnect-loops on a usage/session-limit wall

**Date:** 2026-07-04
**Crates:** `solution_agent` (`store.rs`)
**FORK.md:** decision #34

## Symptom (reported live)

A session that hit claude's session limit (`You've hit your session limit · resets 8pm (Asia/Novosibirsk)`) got repeatedly "recovered" by the hang watchdog: a `system` note *"Агент не отвечал — переподключил сессию"* and a nudge *"Твой процесс завис, поэтому редактор перезапустил его … продолжай работу"*, followed by the limit message appearing again and again, until the session finally went `Errored`. The user's note: *"еще некорректно срабатывает отслеживание зависаний."*

## Root cause

`tick_stuck_sessions` reconnects a session that has been `Running` with no streaming / tool activity for `STUCK_TURN_SECS` (5 min) and no in-progress tool — the "hung subprocess" heuristic. But when a turn hits a usage/session/weekly limit, claude prints the wall as its last assistant message and the turn then **stalls without ending** — so the session sits `Running`, silent, with no tool. That is indistinguishable, by the silence heuristic alone, from a hang.

The watchdog therefore reconnected (respawn + replay transcript) and `maybe_send_reconnect_continuation` sent the "your process hung, carry on" prompt. That prompt starts a fresh turn that **immediately re-hits the same wall** → stalls → reconnect → … a loop that burns quota and spams the conversation.

## Fix

Teach the watchdog to distinguish a **wall** from a **hang** before recovering (`store::tick_stuck_sessions`):

- When a session is wedged, scan its latest assistant message; if its text matches `supervisor::is_usage_limit_error`, tag it as a usage-limit wall rather than a hang.
- A wall is routed to a new shared helper `apply_usage_limit_stop(id, message, cx)` instead of `reconnect_agent`: it stops the runaway turn (`Errored(<wall message>)`, so tick_stuck — which only fires on `Running` — can't re-fire), pushes a `system` note explaining the stop, and either schedules an auto-resume at the parsed reset time (if the observer is enabled and a reset time is parseable — stay `Watching`, gate `next_eligible_ms = reset + jitter`, hold a live timer) or parks at `Stopped(Quota)`.
- Genuine hangs (no usage-limit text) take the unchanged reconnect path.

`apply_usage_limit_stop` is extracted from `on_judge_failed`'s existing `JudgeFailure::Quota` arm (identical behaviour — the judge-failure path now calls the same helper), so both "the judge hit the wall" and "the agent's own turn hit the wall" are handled one way. The existing quota tests (`quota_error_stops_immediately`, `transient_error_advances_backoff_then_gives_up`) still pass unchanged, confirming the extraction is behaviour-preserving.

## Tests

- `store::tests::stuck_usage_limit_wall_stops_without_reconnect` — a `Running`, silent, tool-less session whose last assistant message is `You've hit your session limit` is stopped with the wall message (not `reconnecting…`) and the supervisor parks at `Stopped(Quota)` (no parseable reset).
- Refactor guarded by the pre-existing `quota_error_stops_immediately` / `transient_error_advances_backoff_then_gives_up`.
- Full `solution_agent` suite green (510 lib tests).

## Follow-up worth considering

The wall currently stops the turn as `Errored`; reconnecting to a *fresh* subprocess (to actively reap the hung one) while suppressing the continuation prompt would be cleaner, but the subprocess is reaped anyway on the next resume/kick, and `Errored` matches the state the session reaches on its own. Left as-is to keep the fix minimal.
