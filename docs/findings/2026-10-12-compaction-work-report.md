# Compaction must preserve the work report

At inspected baseline `fef1b2b22c`, the compaction template required technical
state but no explicit user-facing result report. Its exploratory continuation
only asked to read the files and ask what to tackle next. The latest visible
reply could therefore be a handoff acknowledgement and Git-state check,
leaving a returning reader unable to tell what the preceding work achieved.

Shared manual/automatic notices now ask for a substantive report at the natural
boundary. `state.md` preserves the report in the user's language, including
outcome, concrete changes/findings, verification and limits, delivery status,
remaining work and exact stopping point. Before rotation it is visible in chat;
`continue.md` requires the resumed context to restore it. Completed and paused
boundaries get a final result report; ongoing work gets a short recap and
continues only authorized work. Newer user instructions take precedence.

The existing handoff file set and scope validation remain unchanged. No new
provider/model fallback, retry, runtime restart or phase authorization is added.
The success instructions now follow actual adapter behavior: in-place rotation
is deferred until the turn ends, while other adapters can rotate immediately.

Verification: `solution_agent` library 1020 passed, 0 failed, 1 ignored,
including actual manual/automatic notice queue assertions and the preserved
report contract. Debug build passed in 3m04s. A native headless screenshot
shows the full restored report and pause boundary in a clearly labelled cold
fixture; it verifies rendering, not model behavior or an actual provider
rotation. The private editor is stopped. Full release verification is pending.
Evidence is retained under Solution `.tmp/handoff-report`. Offline checks
prove prompt content and transport, not model compliance. No paid model turn
or user-editor restart was performed.

The changed template's placeholder/host replacement check passes, including the
previously omitted `compact_note` inventory entry. The global prompt checker has
pre-existing unrelated drift: system prompt platform/terminal expressions,
supervisor diary/intent placeholders, renamed edit-prediction teacher template,
and synthetic evaluator values. Baseline extraction reproduces those failures.
Those are not fixed by this scoped change. Maintenance finding
`p-8c9abd5989cd58c88d2c6917748796b9` is pending; report
`todo-ea31932cd60222fde79beddc9c25be0c` remains open.

Knowledge context returned no applicable documents or revisions; source and
native evidence are used. Observation `p-314bf0f52c4d90026910e970b2167fa7` is pending
curation; linked report `todo-8d170c93be035169acd716890b89a60f` remains open.
Related: [native context shrink](2026-10-12-native-compaction-handoff.md),
[prompt checks](../workflow/prompt-checks.md), [fork contract](../../FORK.md).
