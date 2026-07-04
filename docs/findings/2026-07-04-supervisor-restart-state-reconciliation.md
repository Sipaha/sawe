# Supervisor restart-state reconciliation (phantom Judging + instant-fire on inherited idle)

**Date:** 2026-07-04
**Crates:** `solution_agent` (`store.rs`, `db.rs`, `supervisor.rs`)
**FORK.md:** decision #33

## Symptoms (reported live)

1. *"только открыл редактор и сразу обсервер затригерился (мигать иконка стала), даже ничего ещё не писал"* — reopening the editor fired a judge instantly (status-row eye pulsing = `Judging`), before the user typed anything.
2. *"reviewing висит всё ещё, хотя я уже сам пнул агента"* — the "reviewing" indicator stayed stuck even after the user manually sent the agent a message to resume.

## Root cause

The supervisor persists its **row** (`enabled`, `status`, `last_fired_at`, `next_eligible_ms`, `trigger_count`, `custom_prompt`, counters) but its **in-flight** state lives only in transient runtime maps/fields that are `None`/empty after a restart:

- `judge_sessions` / `auditor_sessions` (the actual ephemeral judge handles),
- `last_user_input_ms`, `pending_nudge`, `wait_until_ms`, `judge_superseded`.

Two independent wrong behaviours followed from restoring persisted `status` without reconciling it against the (now-empty) transient state:

**(1) Instant fire on inherited idle.** The idle-nudge measures silence from the session's `last_activity_at`. After a restart that timestamp is stale (from the previous run, possibly hours old), so `should_fire` computes `silent_ms ≫ IDLE_THRESHOLD_SECS` and fires a judge on the *very first* `tick_supervisor` pass — a session restored as `Watching` (the resting state most idle supervised sessions are in at shutdown) fires immediately on open.

**(2) Phantom `Judging`.** A judge exists only inside the transient `judge_sessions` map. A row persisted mid-`Judging` restores with `status == Judging` but **no judge actually running**:
- `supersede_judge_on_user_reply` is gated on `judge_sessions.contains_key(&id)` → no-ops on the user's next message, so the user's manual kick can't clear it;
- the judge-stuck watchdog only fires when `now - last_fired_at >= JUDGE_TIMEOUT_SECS`; if the persisted `last_fired_at` is recent (or `None → now`, giving `stuck_ms = 0`) the watchdog never trips.

So the status row sits at "reviewing" indefinitely.

## Fix (two reconciliations at load)

- **`db::load_supervisor_states`:** coerce a persisted `Judging → Watching` and drop the stale `last_fired_at`. A cold-loaded row can never have a live judge, so `Judging` is always a phantom.
- **`SupervisorState.watch_started_ms` (new transient field):** the restart/load path (`store::set_persistence`) stamps `watch_started_ms = now` on every loaded row; `tick_supervisor` fires only when `last_activity_ms > watch_started_ms` — i.e. the session produced genuinely-new activity under THIS process's watch. A **fresh in-session enable** (`set_supervision_enabled`) leaves the field `None` (always eligible) → immediate-idle semantics unchanged (existing in-session tests and behaviour untouched).

## Product decision: inherited idle is left alone until a manual kick

The operator's explicit rule: *"Если я закрывал редактор и потом открыл, то ожидаю, что все сессии только с ручного пинка пойдут дальше работать."* After a restart **nothing auto-resumes** — every session parked before the restart waits for a manual kick. Once the operator kicks a session and its turn completes (`last_activity_at` moves past the baseline), the normal autonomous idle-nudge cycle re-engages for it.

This is a stronger stance than "delay the first judge by an idle window" (the initially-shipped variant): the earlier variant would still auto-resume a parked task after the grace window, which the operator did not want. Gating on `last_activity_ms > watch_started_ms` (activity *after* we started watching) rather than flooring the silence clock is what enforces "manual kick only".

## Tests

- `db::tests::judging_status_coerced_to_watching_on_load` — phantom `Judging` restored as `Watching`, `last_fired_at` cleared, other statuses preserved.
- `store::tests::restart_leaves_inherited_idle_until_fresh_activity` — three cases: (A) restored row + stale `last_activity_at` → no fire; (B) restored row whose activity now postdates the baseline (a completed manual kick) → fires; (C) fresh in-session enable (no baseline) → fires on idle as before.
- `store::tests::supervisor_states_loaded_at_persistence_init` — extended to assert the load path stamps `watch_started_ms`.
- Full `solution_agent` suite green.
