# The Observer parked the session at the exact moment it promised to resume it

**Reported:** 2026-09-19, from the mobile client — "супервизор выдал это когда
лимит истек вместо продолжения работы".

## What the operator saw

Session `citeck-forge 3` (`xjrn2pmv`), in order:

```
system   claude usage limit reached — the current turn was stopped.
system   claude usage limit reached. The Observer will resume the session
         automatically around 21:10.
observer ⏸ Parked — awaiting you: сессия упёрлась в недельный лимит токенов …
         ни агент, ни супервизор этого сдвинуть не могут, вмешательство не
         требуется. … в citeck-migration-toolkit осталось 8 незакоммиченных
         файлов … работа была прервана лимитом на середине.
```

The session then sat at `Errored: You've hit your weekly limit · resets 9pm`
until the operator noticed by hand.

## Evidence

The observer's own breadcrumbs under
`~/.spk/sawe/ss/citeck-forge/.agents/xjrn2pmv/supervisor/`:

- `verdicts.jsonl`, `ts_ms 1789827097650` = **2026-09-19 21:11:37 +07** —
  `"action":"done"`, reasoning starting with the `PARK:` token.
- `diary.md`, same wake: «Вердикт `done` (PARK), а не `wait`: блокирует квота с
  горизонтом ~9 часов, `wait_seconds` упирается в 1800 с — проснулся бы в то же
  errored-состояние. **Автовозобновление редактором уже запланировано
  (~21:10 local).**»

That last sentence is the whole bug. The judge parked *because* it believed the
editor would resume the session — and it **was** the editor's resume: the
scheduled `next_eligible_ms` wake had just fired and spawned it. Its verdict
cancelled the recovery it was counting on.

## Root cause

`apply_usage_limit_stop` promises the operator a resume in the chat and arms
`next_eligible_ms = reset + jitter`. That gate expired into an ordinary judge
fire, so keeping the promise depended on an LLM re-deriving it — from a
transcript whose last event IS the wall, with `observation_context` saying only
"Idle/errored session quiet for at least 60 seconds". Concluding "blocked,
nothing anyone here can move" is the *reasonable* read of that input. The judge
was not wrong; it was asked the wrong question.

## Fix

`tick_supervisor` gained a one-shot resume branch that wakes the **worker**
(`USAGE_LIMIT_RESUME_PROMPT`) instead of spawning a judge, mirroring the
one-shot `wait` deadline. Conditions: a scheduled wake now due (consumed on
fire), the session in `Errored(<claude's limit line>)`, status `Watching`, no
typing in the last 60 s. Reading the wall off `SessionState` rather than a flag
set at scheduling time keeps the decision on the condition that actually
matters and survives a restart. A wall that outlasts its announced reset
re-errors the woken turn → `apply_usage_limit_stop` re-arms at the new reset, so
this is one nudge per wall window, not a poll.

`supervisor_judge_instructions.md` gained the matching rule for paths that still
reach a judge: a provider wall is neither a park nor a `wait`.

See FORK.md #190. Tests:
`scheduled_usage_limit_resume_wakes_the_worker_instead_of_judging`,
`pending_usage_limit_resume_does_not_wake_the_worker_early`.

## The general trap

When the editor writes a commitment into the operator's chat on a timer it owns,
the timer's expiry must **perform** the action. Consulting a model at that point
re-opens a question the editor already answered, and the model's input is the
failure state itself, which reads as "blocked".
