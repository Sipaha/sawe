# Observer + AI-session hang-detection audit (2026-07-09)

Deep audit (fable) of the supervisor/observer and hang-detection subsystems.
Each finding is a fable CLAIM — the controller independently verifies it against
the code before fixing (some may not hold). Status tracked here as fixes land.

Theme: `apply_verdict` got the full send-time double-check (FORK #31), but its
siblings — `apply_audit_verdict`, the judge-spawn failure paths, the
judge-timeout classifier, the reconnect↔observer boundary — did not.

## Real bugs / gaps

| # | Title | Status |
|---|---|---|
| 1 | `apply_audit_verdict` has no send-time gate → a late audit `escalate` clobbers `Held`/`Disabled`, violating the manual-stop rule (FORK #44) | **DONE** |
| 2 | Any judge-spawn early-return leaves status pinned `Judging` → backoff spiral → false `Stopped(ProviderError)` (breaks scheduled quota resume across restart; first-enable of a cold session) | **DONE** |
| 3 | A judge that stalls on the usage wall is classified Transient not Quota → supervision dies as `ProviderError` instead of scheduling the reset-time resume | **DONE** |
| 4 | `is_usage_limit_error` over the whole last assistant message → prose false-positive turns a real hang into `Stopped(Quota)` + skips reconnect | **DONE** |
| 5 | Close-then-reopen (tab/solution) loses supervision in-process, and resurrects it on the next restart (`enabled=true` row never reloaded in rehydrate paths) | **DONE** |
| 6 | Stale wall text + 5-min stuck delay → reset time rolls to tomorrow (~24h over-park) | **DONE** |
| 7 | Observer and reconnect watchdog interleave (no mutual exclusion; reconnect doesn't bump `last_activity_at`) → judge fires mid-reconnect; double-resume race | **DONE** |
| 8 | `wait_until_ms` not cleared by agent activity → mechanical wake nudge fires at a session that already resumed AND finished | **DONE** |
| 9 | Observer-issued `compact` resets the consecutive-continue cap (goes through the `from_user:true` funnel) | **DONE** |
| 10 | Compact refusal invisible to the mechanism (only `log::warn!`) → cap-exempt `compact` re-issued every ~60s | **DONE** |

## Hardening ideas

- Verdict tools unauthenticated — any client on the per-solution socket (incl. the worker) can submit verdicts for any session. (nonce in briefing + require in-flight `judge_sessions`)
- Double-submit likely by-design (bridge exits on stdin EOF; prompt says retry on empty) → duplicate `apply_verdict`. (idempotency key / nonce)
- Judge briefing can bake in the global socket where the verdict tool doesn't exist → guaranteed timeout → spiral (compounds #2). (skip-with-diary, or add verdict tools to `GLOBAL_TOOLS`)
- **DONE** Reconnect continuation prompts are unmarked user-role messages → judge may distill them into `user_intent.md`; `tail_is_unanswered_user_message` misclassifies a prior continuation. (fixed: `spk_editor_recovery` marker; excluded in tail-detector + judge filter via separate `editor_recovery` DTO field)
- `JUDGE_TIMEOUT_SECS` measures wall-clock from fire, not judge liveness → a thorough judge is killed mid-verdict. (check judge session `last_activity_at`/streaming before declaring dead)
- `TOOL_STUCK_SECS`=20min hard-kills legitimately long foreground tools → possible duplicate build/deploy. (check process/terminal liveness before reconnect)
- No watchdog on the reconnect itself → a failed/hung `resume_session` strands the session `Errored("reconnecting…")` forever. (retry-once-then-notify)
- **DONE** Agent-side wall as an `Error` event loses its text (`Errored("agent error")`) → reset time unrecoverable. (fixed `284435a46b`: split `Error`/`LoadError` arms; new `session_wall_message` helper — turn-boundary-anchored so a stale prior-turn wall can't reclassify a later transient error — routes a supervised wall to `apply_usage_limit_stop`, surfaces the text for an unsupervised session)
- Dropped verdicts logged indistinguishably from acted ones → auditor miscounts. (add `dropped: true` to `VerdictRecord`)
- `VerdictRecord.tokens` always `None` in production → `total_tokens` stat permanently 0. (fill from judge token usage, or drop the field)
- Zero-output long background shells reaped as stale at ~7min → lose `has_live_background_work` suppression. (degrades gracefully to a `wait`)
- **DONE** Empty `question` passes validation (`is_some()` not non-empty) → empty banner + toast. (now requires a non-empty trimmed question)

## Correctly handled (credited by the audit — do NOT "fix")

`apply_verdict` send-time triple check; `judge_superseded` lifecycle; `pending_nudge`
staleness seams; backoff hygiene; restart reconciliation (phantom Judging→Watching,
`watch_started_ms`); teardown coverage (judge+auditor reaped on every close path);
no tick re-entrancy; wall-vs-hang routing for the worker (FORK #34); notification-gate
parity; reconnect-prompt selection (FORK #45); tail-capped logs.
