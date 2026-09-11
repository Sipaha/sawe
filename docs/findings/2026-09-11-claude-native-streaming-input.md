# Claude native streaming input and cancellation

Status: verified against Claude CLI 2.1.258; retain the existing main-agent hook delivery

## Question and result
Can native stream-json user input replace the editor's PostToolUse/Stop follow-up injection?

The installed CLI does accept a second user message while a tool is running. A bounded synthetic test delivered that message at the tool boundary in the same turn. Its `--replay-user-messages` echo preserved the exact input UUID, so a correlated receipt is possible without replacing the original prompt resolver.

However, a second test found a cancellation behavior incompatible with Sawe's Stop semantics: an input already submitted to stdin survived `interrupt` and started another turn after the cancelled result. Replacing the hook with unconditional native streaming would therefore allow stopped work to resume.

## Probe conditions
- Claude CLI 2.1.258, Sonnet, isolated empty temporary working directory.
- `--print --input-format stream-json --output-format stream-json --verbose --include-partial-messages --replay-user-messages`.
- Safe mode, only Bash allowed, empty strict MCP configuration, no session persistence, no settings sources, maximum budget USD 0.20 per probe.
- Initial instruction: run `sleep 3; printf FIRST_TOOL_DONE`, then answer `FIRST_FINAL`.
- On the tool-use event, submit a second user message requesting `SECOND_SEEN` instead. Both inputs carry UUIDs; the verification probe derives deterministic UUIDs from each input and checks replay equality.
- The cancellation variant sends the native `interrupt` control request immediately after the follow-up.
- Every probe terminates and waits for its process in `finally`; temporary working directories are removed. Scripts and output stay outside Git under `/tmp`.

## Observations
The ordinary probe emitted the follow-up's exact UUID after the tool result, then `SECOND_SEEN`, and exactly one successful terminal result. This establishes that active input works for the tested version and tool boundary; it does not establish arbitrary interrupt timing guarantees.

In the cancellation probe, relative to process start:

| Time | Event |
| --- | --- |
| 2.87 s | Tool use; follow-up submitted, then interrupt submitted |
| 2.94 s | Tool rejected/interrupted; first result `error_during_execution` |
| 3.90 s | Previously queued follow-up echoed with its original UUID |
| 4.02 s | Assistant emitted `SECOND_SEEN` |
| 4.03 s | Second result, `success` |

The cancellation output is `/tmp/sawe-claude-steer-cancel-result.json`; the exact-UUID success output is `/tmp/sawe-claude-steer-echo-result.json`. These are local diagnostic artifacts, not required runtime inputs.

## Decision
Keep Claude main-agent follow-ups in the editor-owned queue and deliver through the existing hook. This queue can be discarded on Stop before the native runtime receives it. Keep the existing subagent hook routing and bounded self-healing behavior as well.

Codex's native `turn/steer` includes an expected active turn ID and an explicit request response. Claude stdin user input does not provide the same expected-turn boundary in the tested protocol: a boundary race or interrupt may leave it queued for a later turn. A replay UUID proves acceptance, but does not prove cancellation removed pending work.

A future migration needs verified cancellation of queued native input, or a carefully tested process-recovery policy that preserves the transcript while preventing post-Stop continuation. This finding is specific to the tested version and behavior, not a claim that native streaming can never be supported safely. The experimental receipt implementation was not merged.
