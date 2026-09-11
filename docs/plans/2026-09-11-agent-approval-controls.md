# Clear, single-use agent approval controls

Status: complete

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

## Automated results
The affected crate suites pass 1156 tests (acp_thread 85, claude_native 94,
codex_native 16, console_panel 39, solution_agent 867, solution_git 55).
Three tests are ignored: the two upstream checkpoint tests require capture
intentionally disabled by FORK.md #23, and one pre-existing ignored test.
The second checkpoint test's known failure was recorded on 2026-07-06; it now
has the same explicit ignore as its companion rather than a misleading failure.
The single-use authorization regression and provider-menu paint/click test pass.
Scoped debug clippy passes with warnings denied. Prompt inventory/contracts and
21 prompt-check unit tests pass.

## Editor and provider verification
A real Codex session read a neighboring synthetic session through
`solution_agent.get_session`: the MCP tool completed without elicitation,
returned its state and latest assistant message, and reported 828400 context
capacity. After selecting Read only in the status menu, the same native chat
resumed and reported its one attempted temporary-file write failed with
`Read-only file system`; the marker was absent. Read only survived an editor
restart. No working-session content was sent in these checks.

A synthetic app-server requested approval in an isolated debug editor. The
render showed Allow once / Allow for this session / Deny at the larger size.
Clicking the session option returned native `acceptForSession`; all buttons
immediately disappeared while the tool remained running and the provider sent
no completion event (intentionally delayed 120 seconds). Screenshots also
confirmed the plus menu's two provider labels and the permission dropdown.
The synthetic provider is test scaffolding only, not part of the product.

Debug and release-fast builds completed. Screenshots, model transcripts and
smoke harnesses remain under /tmp; unrelated user report files were excluded.
