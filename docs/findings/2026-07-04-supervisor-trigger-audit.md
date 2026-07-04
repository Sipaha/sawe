# Supervisor / observer TRIGGER audit

**Date:** 2026-07-04
**Crate:** `solution_agent` (`store.rs`, `supervisor.rs`, `background_agent.rs`)
**FORK.md:** #31, #32, #33

Full audit of the observer trigger mechanism (when a judge fires, when its verdict is acted on), prompted by "supervisor reacted while the agent was still alive." Done as my own read + an independent adversarial audit agent. Findings and dispositions:

## Fixed

**1 (HIGH) — a dropped verdict left the supervisor pinned in `Judging`.** `apply_verdict`'s send-time drop (session no longer idle — the agent self-resumed mid-judge) called `finish_judge` and returned WITHOUT resetting `status`. `finish_judge` doesn't touch `status`, so it stayed `Judging` with no live judge → the judge-stuck watchdog (5 min) mistook it for a crashed judge → bogus transient backoff → over repeated benign self-resumes, compounded to a false `Stopped(ProviderError)` that silently killed supervision. Fix: on the drop, if still `Judging`, reset to `Watching`. Test: `continue_verdict_dropped_when_agent_already_running` (extended).

**3 (MEDIUM-HIGH) — a parked `pending_nudge` flushed even after the user paused the session.** The hold-on-typing flush in `tick_supervisor` had no `status == Watching` gate (the wait-wake path right below it does). `hold_supervisor` / `escalate_to_user` / the `Done` arm don't clear `pending_nudge`, so a nudge held while the user was typing would be delivered after the user hit Stop (`Held`) / the supervisor escalated (`WaitingUser`) — dragging the agent back to work it was explicitly paused from. Fix: gate the flush on `Watching`; drop the stale nudge otherwise. Test: `pending_nudge_dropped_when_paused_before_flush`.

**4 (MEDIUM) — background AGENT completion didn't reset the silence clock** (same over-eager-trigger race as background shells). Already fixed earlier this session (`refresh_background_agent_snapshot` bumps `last_activity_at` on the terminal transition; test `background_agent_terminal_transition_resets_silence_clock`).

**5 (LOW-MEDIUM) — a scheduled auto-resume was defeated by a restart.** `watch_started_ms` (inherited-idle gate) blocked a session whose `last_activity_at` predates the process start forever — including a usage-limit auto-resume or a transient-failure backoff with `next_eligible_ms` set, silently breaking the "observer will auto-continue at HH:MM" promise across a restart. Fix: `eligible_for_watch` now exempts any session with a scheduled `next_eligible_ms` (an explicit schedule is not plain inherited idle). The manual-kick rule still applies to unscheduled idle.

**6 (LOW, defensive) — the judge-stuck watchdog couldn't time out a `None` `last_fired_at`.** `unwrap_or(now_ms)` → `stuck_ms = 0` → never trips. Currently unreachable (every fire sets it; DB load coerces `Judging`+`None` → `Watching`), but a future path that sets `Judging` without `last_fired_at` would wedge permanently. Fix: `unwrap_or(0)` (treat `None` as already-stuck → un-wedges immediately).

## Deferred (documented, narrow)

**2 — `spawn_judge`'s no-project bail leaves `Judging` orphaned** (cold/prebuilt session, no cached project/thread). Same wedge class as #1: pinned `Judging` → 5-min watchdog → bogus backoff → `Stopped(ProviderError)`. Narrow reachability (a cold session that is supervised + eligible; restart-cold is normally gated by `watch_started_ms`, though #5's exemption re-opens it for a cold quota-resume). The correct fix is a fire-precondition ("don't fire a judge you can't spawn"), but that changes the observable for the whole tick-test suite (all fire-tests use projectless seeded sessions and assert `Judging` as the fire signal), and a naive status-reset on the bail creates a 1 Hz fire→reset loop (worse than the self-limiting original). Deferred until the tick-test harness can seed a spawnable session; the original behaviour is self-limiting, not a loop.

## Verified sound (no change)

Inline Task subagents (`active_subagents`) — parent is `Running` while they run, so `should_fire`'s idle gate covers it. Foreground tool calls — parent `Running`; completion bumps `last_activity`. `AwaitingInput` → resume. `hold_supervisor` / `reset_supervisor_continue_counter` / `set_supervision_enabled` all clear transient `wait_until_ms`/`pending_nudge`/`next_eligible_ms`/backoff. One-judge-at-a-time (status set to `Judging` synchronously before `spawn_judge`). `consecutive_continues` is not inflated by dropped verdicts (drop returns before the Continue arm). Delivery-time races on `deliver_nudge_now`/wait-wake are same-tick (µs window) with the `apply_verdict` backstop.
