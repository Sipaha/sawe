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

## Session permission modes
The user clarified that agents should have full access by default across the
Solution and an optional read-only mode. Full access matches Claude's existing
unrestricted mode and Codex's no-sandbox/no-confirmation policy. The Solution is
the working/task scope, not an operating-system security boundary in this mode.
Persist the choice per session; switch only while idle with no queued input or
pending compaction/authorization. Close the old process and resume with the new
policy before another turn. Resume metadata must carry the mode explicitly.

Read-only must restrict native tools and MCP paths, not merely ask the model to
avoid editing. Claude is constrained through its launch tool catalog, safe mode
and host rejection of unexpected tool approvals; Codex uses a read-only sandbox
and disables MCP/plugins that could otherwise bypass it. Keep internal helper
sessions' existing restrictions. Native backend and store persistence are owned
by separate isolated agents; root owns shared trait, approval lifecycle and UI.

The status dropdown uses Full access / Read only; the session-strip plus uses
Codex (OpenAI) / Claude (Anthropic), with no separate chevron button.

## Verification
Test protocol option/response mappings and single-use authorization behavior.
Inspect the visible approval control in a debug editor. Run affected tests and
clippy, build debug for smoke and release-fast for handoff. Document final
results, commit/push relevant files; exclude temporary screenshot/report files.
