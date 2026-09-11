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
