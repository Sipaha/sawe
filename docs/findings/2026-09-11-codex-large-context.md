# Codex context window requested by Sawe

Date: 2026-09-11

Sawe sends `model_context_window: 872000` in the session configuration shared
by `thread/start` and `thread/resume`. This is an editor-scoped request, not a
write to the user's Codex config. It takes effect when a native session opens
or resumes, including context replacement. Already-running native processes
need to reconnect before acquiring the new setting.

The installed Codex runtime applies each model's own maximum and effective
window reserve. A bounded synthetic native app-server check created an
ephemeral Astra thread with that override and obtained `modelContextWindow:
828400`. Switching the same thread to GPT-5.5 produced 258400 after its response.
The initial model-switch update still carried the previous capacity, so clients
must keep accepting subsequent native usage updates. Both synthetic turns
completed; neither read files nor used tools. No working-session content was
sent. This verifies the configuration and clamping behavior, not successful
inference with an actual 800k-token prompt.

The editor displays the reported value and its observer evaluates that same
capacity. No model-name table, catalog-cache parsing, or invented UI limit is
needed. Native automatic compaction remains enabled as a fallback; this change
does not override its threshold separately.

Reference: [Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference),
`model_context_window`.

## Hub migration regression (2026-10-27)

The native adapter's session-only override was omitted when execution moved to
SPK AI Hub. Codex then reported its default effective capacity (475000), which
Sawe correctly displayed. The fix belongs to the Hub-owned native session
configuration shared by start and resume, including read-only sessions.
Sawe continues to consume ACP `UsageUpdate.size` unchanged; live effective
capacity supersedes an older persisted value, and a model switch may lower it.
A status-row regression covers 475000 → 828400 → 258400 and a later missing
reading. No account or global CLI settings are changed.

Verification: all 1083 active `solution_agent` library tests passed (one ignored),
including the seven status-row tests. Strict crate Clippy found existing
`redundant_clone` in `conversation_render/tool_group.rs:268` and `manual_flatten`
in `preview_window.rs:425`; the same crate gate passed with only those two lints
allowed. Those unrelated files were left unchanged.
