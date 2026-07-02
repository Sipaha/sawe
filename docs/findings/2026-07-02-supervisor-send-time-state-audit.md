# Supervisor: send-time state audit + hold-on-typing + disable/interrupt

**Date:** 2026-07-02 · **Status:** fixed · **Crate:** `solution_agent`

## Problem

Every condition that gates the "supervisor" (aka Observer) was evaluated only at
judge-**start** (`should_fire` / `tick_supervisor`). An ephemeral judge turn then
runs for seconds→minutes; by the time its verdict is delivered
(`apply_verdict` → `send_supervisor_nudge`) the world has moved on, yet the only
send-time re-check was the bug-#1 `judge_superseded` marker. Concrete gaps the
maintainer reported:

1. **Typing didn't stop a nudge already in flight.** The "reset the timer while
   I type" feature only worked if the user started typing *before* the judge
   fired (the silence clock blocks a NEW fire). A judge that fired while the user
   was idle and finished *after* they started composing still barged its nudge
   into the middle of the user's half-written message.
2. **Disabling the supervisor didn't interrupt a running one.** `set_supervision_enabled(false)`
   flipped status to `Disabled` but did NOT tear down an in-flight judge/auditor,
   and `apply_verdict` had no `enabled` re-check — so a judge that was already
   running delivered its nudge anyway, after the user had switched supervision off.
3. **Latent: a verdict racing in after a manual Stop still nudged.** `hold_supervisor`
   tore the judge down but never set `judge_superseded`, and `apply_verdict` didn't
   check for `Held`, so a verdict that left the judge just before teardown nudged
   the agent post-Stop.

## Fix (double-check: start AND send; interrupt known-stale runs)

- **Send-time gate (`apply_verdict`).** Drop the verdict (log for audit, no nudge,
  no counter bump, no escalation) when the live state says supervision isn't
  actively running: `!enabled` (Disabled), `Held` (manual Stop), or `Stopped`
  (usage wall / provider death). `Watching`/`Judging`/`WaitingUser` pass through so
  the direct-apply paths (e.g. a `Done` verdict from `Watching`, and the unit
  tests) still act. Consumes `judge_superseded` in the same read, subsuming the
  bug-#1 check.
- **Interrupt on known-stale (synchronous judge teardown), not run-to-drop.** Where
  a state change makes the in-flight verdict useless, tear the judge down now and
  set `judge_superseded` (belt-and-suspenders with the send gate):
  - `set_supervision_enabled(false)` → `finish_judge` + `finish_auditor` + discard
    held nudge (was the reported bug).
  - `set_supervisor_prompt` (instruction changed mid-review) → the judge reviewed
    under the old instruction; tear it down and re-arm `Watching` so the next tick
    re-fires under the new one.
  - `supersede_judge_on_user_reply` (user reply) and `hold_supervisor` (manual Stop)
    already tore the judge down; the send gate now also covers their racing verdict.
- **Hold-on-typing (`send_supervisor_nudge` + `tick_supervisor` flush).** If the user
  typed within `IDLE_THRESHOLD_SECS` when the nudge is about to send, park it in the
  new transient `SupervisorState.pending_nudge` instead of delivering. The verdict
  is still accepted (continue-counter bumps). `tick_supervisor` flushes it once the
  user has been quiet for the idle window ("changed my mind, stopped writing"); a
  genuine user SEND discards it in the `from_user` funnel (unconditionally — the
  judge is already gone once a nudge is held, so `supersede_*`'s no-judge early-return
  can't be relied on). While a nudge is parked no fresh judge fires.

## Where checked (start vs send)

| Condition | Start (`should_fire`/`tick`) | Send (`apply_verdict`) | Interrupt on change |
|---|---|---|---|
| `enabled` | ✅ | ✅ (`!enabled`→drop) | ✅ `set_supervision_enabled(false)`→finish_judge/auditor |
| status Held/Stopped | ✅ (needs `Watching`) | ✅ (drop) | ✅ `hold_supervisor` |
| user typing | ✅ (silence clock) | ✅ (hold-on-typing park) | — (held, not interrupted) |
| user reply/send | — | ✅ (superseded/status) | ✅ `supersede_judge_on_user_reply` + discard pending |
| instruction changed | — | ✅ (superseded) | ✅ `set_supervisor_prompt`→finish_judge |
| session closed | — | ✅ (state gone→drop) | ✅ `teardown_session_runtime` |

## UI

The status-row eye **pulses** (opacity `pulsating_between(0.35, 1.0)`, 1 s repeat —
the same idiom as `agent_ui`'s loading rows) while `status == Judging`, tooltip
"Supervisor reviewing…", so "the observer is working right now" is visible at a
glance. The pulse is driven off the unit-tested `judging` bool; it isn't
headless-screenshottable (the `Judging` state needs a live judge subprocess, and a
still frame can't show a pulse) — eyeball it live by leaving a supervised session
idle ~60 s.

## Tests

`crates/solution_agent/src/store/tests.rs` (all green, full suite 505/505):
`observer_nudge_held_while_typing_then_flushed`, `disabling_supervision_interrupts_running_judge`,
`held_supervisor_drops_racing_verdict`, `user_send_discards_held_nudge`,
`changing_instruction_interrupts_running_judge`. FORK.md decision #31.
