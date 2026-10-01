# Session handoff — Kimi Code support shipped

**Status:** DONE. Kimi Code runs as a Solution-chat provider on `main`
(committed + pushed). The pause snapshot this file started as (2026-10-01
morning, work half-landed) is fully resolved; what remains is optional
follow-up, listed at the bottom.
**Recorded:** 2026-10-01, at completion.

## Commit chain

| Commit | What |
|---|---|
| `446142f83c` | `main` tip before this work |
| `2b4b983c45` | `qwen-code-support` (pushed earlier this session) — another agent's unfinished Qwen Code attempt, parked verbatim per user request; never compiled, reference only |
| this commit | Kimi Code support (see below) |

## What shipped

Kimi Code as a third Solution-chat runtime, mirroring the qwen branch's
shape with kimi-specific facts probed live from `kimi acp` 2.1.1:

- `agent_servers::KimiAgentServer` (`crates/agent_servers/src/kimi.rs`) —
  launcher only; `kimi acp` is a positional subcommand, not a flag. Auth is
  the CLI's own (`kimi login`); no token, no `NO_BROWSER`.
- `solution_agent::kimi_adapter` — brand (Kimi / Moonshot AI / `0x1783FF` /
  official "K Only" mark, `assets/icons/ai_kimi.svg`), `ReadOnly→plan`,
  `FullAccess→yolo` mode mapping, `model` config-option id, `KIMI.md`
  instruction file, unit tests.
- `native_controls` — `available_models`/`set_model` gained `cx`; generic
  config-option model path (deliberately not agent-gated);
  `set_permission_mode`/`uses_acp_permission_modes[_for_agent]` gated on
  `kimi-code`; live mode switch without a process restart (applied BEFORE
  the DB write; a refused mode is an error).
- `store.rs` / `hydration.rs` / `acp_event.rs` — all call sites; resume
  prefers `session/resume` unconditionally (kimi advertises load+resume;
  load would replay over the fork's own transcript, FORK.md #161);
  `sawePermissionMode` meta is native-only; persisted model + mode pushed
  post-attach on create and resume.
- UI: `NewKimiChat` action, provider-menu entry (Codex / Claude / Kimi),
  `AiKimi` icon.
- FORK.md decision #224 + touched-files rows.

## The upstream bug this work flushed out (fixed in-tree)

`AcpConnection::config_state` (upstream #58308) dropped a `session/new`
response's legacy `modes` whenever `configOptions` was present. Kimi sends
both, so `session_modes()` always returned `None` and the permission push
could never land (the store logged "advertises no ACP modes; permission
mode not applied" on every create). The fork now keeps both surfaces —
they are equivalent on kimi (`session/set_mode` ≡ its `mode` config
option). Recorded in FORK.md #224 and the `agent_servers/src/acp.rs`
table row.

## Verification (all against this tree)

- `cargo check -p icons -p agent_servers -p solution_agent -p console_panel
  --all-targets` — clean, no warnings.
- `cargo clippy` on those crates, `--all-targets -- --deny warnings` — clean.
- `cargo test -p solution_agent` — **981 passed, 0 failed** (+2 e2e files).
- `cargo test -p icons -p console_panel` — green.
- `cargo test -p agent_servers` — 34 passed, 1 failed:
  `acp::tests::connection_routes_terminal_auth_without_acp_beta` **fails on
  clean `main` too** (verified via `git stash` round-trip) — pre-existing,
  unrelated to this work.
- Live E2E via `script/run-mcp --debug --headless` + MCP: created a real
  `kimi-code` session, the agent replied on the first prompt; status row
  shows the blue-K brand chip, "Full access" and the live ACP mode text
  "YOLO" (proof the mode push reached the CLI); the `+` menu lists
  Codex / Claude / Kimi with correct chrome. Probe solutions/sessions and
  their on-disk dirs were cleaned up.

## Follow-ups (optional, not blocking)

- Branch `qwen-code-support`: finish and land, or delete — maintainer's
  call. If it lands, expect textual conflicts with this work in
  `native_controls.rs` / `store.rs` / `hydration.rs`; resolution is to keep
  both provider ids side by side (the helpers already match on id). Note
  the qwen branch also contains two never-compiled bugs this session hit
  and fixed (`set_config_option` needs `&mut App`; `AsyncApp::update`
  returns `R`, not `Result`) — rebase rather than merge, and re-verify.
- `permission_mode_for` in `kimi_adapter` has no caller yet (same as on the
  qwen branch) — it exists for the future "adopt the mode the CLI reports"
  path and is covered by tests.
- Kimi's `thinking` config option is deliberately not wired to the fork's
  effort control (claude/codex-specific today).
