# Clear, single-use agent approval controls

Status: implementation

## Goal
Make tool approval controls readable, expose Codex's session-scoped approval,
and prevent repeated clicks after an answer has been sent.

## Scope
Root owns shared conversation rendering and live authorization lifecycle checks.
An isolated backend agent owns native Codex approval option/decision translation,
including session approval and protocol-specific supported choices. Do not add
persistent global permission grants or treat session approval as blanket access.
Keep Claude's existing options and both user-interface and MCP reply paths valid.

## Verification
Test protocol option/response mappings and single-use authorization behavior.
Inspect the visible approval control in a debug editor. Run affected tests and
clippy, build debug for smoke and release-fast for handoff. Document final
results, commit/push relevant files; exclude temporary screenshot/report files.
