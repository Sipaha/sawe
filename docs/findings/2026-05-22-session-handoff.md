# Session handoff — 2026-05-22 — native Claude stream-json connection (Foundation)

## Context for resume
Building a **native Rust replacement** for the `@agentclientprotocol/claude-agent-acp`
node wrapper: a new crate `claude_native` that spawns the `claude` binary in
stream-json mode directly and implements `acp_thread::AgentConnection`. Motivated
by a real bug — sessions stuck in `Running` forever (the `session/prompt` await
has no timeout; the wrapper can drop the terminal response on a dead MCP call /
mishandled `{"type":"ping"}` / killed process; Stop is equally powerless). Native
ownership makes turn-end deterministic and enables a two-stage Stop + watchdog.

**Three sub-projects, sequential:** 1. Foundation (this) → ship & verify →
2. subagent output rendering → 3. mid-turn message injection.

- **Spec:** `docs/superpowers/specs/2026-05-22-claude-native-connection-foundation-design.md`
- **Plan:** `docs/superpowers/plans/2026-05-22-claude-native-connection-foundation.md`
  (NOTE: both under `docs/superpowers`, which is **gitignored** — local only.)
- **Cross-context memory:** `project_claude_native_connection.md` (auto-loaded).

## Branch + commit chain
Branch **`claude-native-foundation`** (off `main`). 8 commits:
```
<lock>     Cargo.lock + docs: native connection foundation handoff
a02d3b390b claude_native: translate stream-json to acp SessionUpdate
823647c0cb claude_native: claude argv + mcp-config builder
8d629bbc43 claude_native: stream-json input/control serialization
af73359d68 claude_native: stream-json output message parsing
cda38627b6 solution_agent: select native claude backend by setting
e50fb88fd4 solution_agent: add claude_backend setting (default acp)
41b2eeb041 claude_native: crate scaffold
```

## What shipped (Phases 0-3, all the PURE layers — 24 tests green)
- **Phase 0:** crate `crates/claude_native/` (`[lib] path = src/claude_native.rs`);
  setting `solution_agent.claude_backend = "acp"|"native"` (default `acp`) in
  `settings_content` + `agent_settings.rs` (`ClaudeBackend` enum); backend-select
  branch in `solution_agent.rs::init` — native registers a **stub**
  `ClaudeNativeAgentServer` whose `connect()` returns
  `Err("native claude backend not yet implemented")`.
- **Phase 1:** `protocol.rs` — `OutputMessage` parse (`#[serde(other)]`→`Unknown`
  for unknown `type`, incl `{"type":"ping"}`); `InputMessage` serialize
  (`user_text`, `user_blocks` text+image, `interrupt`, `permission_response`).
- **Phase 2:** `command.rs` — `ClaudeCommandSpec::to_std_command` argv (reproduced
  verbatim from a live `ps`) incl `--mcp-config` + `--append-system-prompt` +
  `--session-id`/`--resume`; `mcp_config_json` maps `acp::McpServer::Stdio`.
- **Phase 3:** `translate.rs` — `translate(&OutputMessage) -> Vec<acp::SessionUpdate>`
  (text/thinking deltas, assistant tool_use→ToolCall, user tool_result→
  ToolCallUpdate, subagents collapsed via `parent_tool_use_id`); `classify_result`
  → `TurnEnd::{Stop(StopReason),Error}`; `usage_update` with sticky window.

## Outstanding pool (Phases 4-8 — the INTEGRATION half, none started)
Execute via subagent-driven-development (or inline) per the plan:
- **Phase 4 — `process.rs` `ClaudeProcess`:** spawn via `util::process::Child::spawn`
  + `cx.background_spawn` reader/writer/stderr tasks (pattern: `agent_servers/src/
  acp.rs:734-815`, `futures::io::BufReader::lines`); control-request router
  (`request_id → oneshot`); EOF/exit detection. Integration tests need a **mock
  `claude`** script emitting canned NDJSON.
- **Phase 5 — `connection.rs`:** real `ClaudeNativeConnection: AgentConnection` +
  `ClaudeNativeAgentServer::connect`; `new_session`/`resume_session` (spawn → await
  `system{subtype:init}` for `session_id` → build `AcpThread::new(...)` → pump
  `incoming`→`translate`→`thread.handle_session_update`); `prompt()` resolves on
  `result` (**the hang fix**); process-death→`Errored`.
- **Phase 6 — permission bridge:** `control_request{can_use_tool}` →
  `request_tool_call_authorization` (mirror `acp.rs:3292 handle_request_permission`)
  → reply `control_response{behavior}`.
- **Phase 7 — Stop + watchdog + token-limit:** two-stage `cancel` (interrupt →
  30s → kill+resume, force-resolve prompt `Cancelled`); 15-min silence watchdog →
  one-shot headless `claude -p` analyzer (verdict hung/working; do nothing if it
  fails to launch); sticky context window (also harden `status_row.rs:469-473`).
- **Phase 8 — `close_session` kills the process; live bring-up verification.**

## Open decisions / confirmed choices
- System prompt via **`--append-system-prompt`** CLI flag (binary supports it) —
  decided NOT to replicate the SDK's `initialize` control_request. Phase 4 must
  verify on the live binary that user messages are accepted **without** any
  `initialize` handshake; add a minimal one only if strictly required.
- One `claude` process per session; killed only on tab/solution/editor close
  (no idle reaping). Translate to `acp::SessionUpdate` so `AcpThread`/UI unchanged.
- Coexist with the `acp` backend behind the setting; flip default + delete the old
  path only after the native backend is verified.

## Active gotchas
- **`claude --help`** confirms flags: `--append-system-prompt`, `--system-prompt`,
  `--mcp-config`, `--permission-mode`, `--input-format stream-json`, etc. Live binary:
  `/home/spk/.npm/_npx/b555b4fead8494dc/node_modules/@anthropic-ai/claude-agent-sdk-linux-x64/claude`.
- **Control protocol** (from SDK `browser-sdk.js`): stdin `{"type":"control_request",
  "request_id","request":{"subtype":"interrupt"|...}}`, paired `control_response`.
  Interrupt = what `query.interrupt()` sends.
- **acp schema = `agent-client-protocol-schema-0.12.0`** — exact constructors noted
  in `project_claude_native_connection.md`. `UsageUpdate` arm in
  `handle_session_update` is gated by `cx.has_flag::<AcpBetaFeatureFlag>()`.
- **Subagent dispatch was 529-Overloaded all session** (≥6 failures) — Phases 0-3
  were done inline. Retry subagent-driven-development for Phases 4-8 when infra is back.
- Bash tool **cwd resets** between calls — `cd <repo>` each cargo invocation; use
  `set -o pipefail` and don't pipe cargo through `tail`/`head` (masks exit code).
- Build only `-p claude_native` / `-p solution_agent` to stay fast; if a file lock
  hangs cargo for minutes, `pkill -f "cargo check --workspace"`.
- `docs/superpowers/**` is gitignored (spec + plan live there, not committed);
  `docs/findings/**` IS tracked (this file).

## Verification status
`cargo test -p claude_native` → 24 passed. `cargo build -p solution_agent` → clean.
No live/end-to-end run yet (native `connect()` is still a stub — Phase 5 lights it up).
The editor default backend is still `acp`; nothing user-visible changed.
