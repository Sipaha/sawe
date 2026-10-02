# Session handoff — Kimi Code support shipped

> Correction (2026-10-02): ACP mode descriptions were stale, and Kimi ignores
> `_meta.systemPrompt`. Current contracts and fixes are in
> [the Kimi review](2026-10-02-kimi-review.md) and FORK.md #224.


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

## Follow-up fixes (same day, second commit)

Two bugs surfaced the moment the maintainer restarted the editor into the
new build:

1. **Every kimi resume failed** with "Session permissions changed while
   resuming". `hydration.rs`'s change-during-resume guard compares
   `resume_meta["sawePermissionMode"]` against the session's current mode —
   but the meta intentionally carries no such key for an ACP-mode agent, so
   the comparison was `None != Some(...)` → always true. The guard is now
   skipped for `uses_acp_permission_modes_for_agent` ids. Verified by a
   full restart cycle in an isolated home: create → kill editor → relaunch
   → send; the resumed kimi session answered with the word it was asked to
   remember before the restart, via `session/resume` (log-clean).
2. **The Kimi logo rendered nowhere in release builds** (debug was fine).
   `crates/assets`'s `fs_embed!` snapshots `assets/icons/**` at the *compile
   time of the assets crate*; the crate didn't recompile for a pure file
   addition, so the release binary embedded a stale icon set (the one
   `ai_kimi` string in the binary was just the strum name). Fix: `touch
   crates/assets/src/assets.rs` (or `cargo clean -p assets`) before any
   release build that adds asset files. Trap documented here; consider a
   build-script `rerun-if-changed` later.
3. **"YOLO" sat in the model's slot in the status row** (third commit,
   `545f17a323`). `AcpConnection` never overrode `active_model` (the pinned
   schema has no per-session model state), so the model segment showed a
   "model" placeholder while the session-mode segment showed the raw CLI
   mode name ("YOLO") — in the slot the maintainer reads as "where the
   model name goes". `active_model` now reports the `model` config option's
   current value by display name ("K2.8 Preview") for any ACP session that
   publishes one, and the mode segment is hidden for ACP-mode agents (the
   permission control beside it already states the mode).

## Evening round (commits `d6f8cad37e`, `8a9e8857da`)

4. **Mid-turn follow-ups queue and flush on turn end — verified working**
   for kimi (`solution_agent::queue`: follow-ups queued while Running,
   delivered as a new turn on `Stopped`; the log shows
   `flushing N Main block(s)`). True active-turn steering (reading the
   follow-up while the turn still runs) is codex-native only
   (`store/steering.rs` downcasts to `CodexConnection`); kimi has queueing,
   not steering. The maintainer's "подвисшая" session turned out to be the
   next item, not the queue.
5. **A dead agent process used to mean a silently stuck session**
   (`d6f8cad37e`). Live case: kimi session `Running` for minutes with no
   kimi process in the system and zero log lines. `LoadError::Exited` is
   now recoverable: Idle + Error system note ("The agent process exited
   unexpectedly. Your next message will restart it and resume this
   conversation."), dead-thread detach, pool invalidation by pointer
   identity; the next send respawns and `session/resume` restores context.
   Sub-agent did the implementation; 3 new regression tests; isolated E2E
   with SIGKILL answers after ~6s.
6. **Quota limits masqueraded as a bare "Authentication required"**
   (`8a9e8857da`). Kimi's engine log had the full message ("403 You've
   reached your 5-hour usage limit. Your quota will reset when the current
   5-hour window ends…") but the ACP layer reports `authRequired` and
   `AuthRequired`'s Display dropped the captured description.
   Display now appends it; send failures with AuthRequired also pin the
   reason into the transcript as an Error-level system note (link clickable).
   Unverified: whether kimi's wire `message` carries the quota text or a
   generic one — if generic, the note shows the generic text plus the
   description is empty; the durable source of truth is
   `~/.kimi-code/sessions/wd_*/session_*/logs/kimi-code.log`.

## Open follow-ups

- `permission_mode_for` in `kimi_adapter` still has no caller (kept for the
  future adopt-the-reported-mode path).

## Resolved this session (2026-10-02)

- **sawe MCP bridge schema rejected by kimi — FIXED.** Root cause: schemars
  emits draft-07 boolean `true` for `serde_json::Value` fields
  (`workspace.get_effective_settings.settings`,
  `workspace.dispatch_action.args`, `editor.get_operation.result`, …) and
  kimi's zod tool-catalog validation rejects boolean subschemas inside
  `properties`, failing the WHOLE server at `session/new`. Fix:
  `context_server::listener::add_tool` expands every boolean `true` to `{}`
  (`expand_boolean_any_schemas`, FORK.md decision #226); `false`
  (`additionalProperties: false`) is accepted by the same validator and
  untouched. Verified: `tools/list` on both sockets has zero boolean
  schemas; live `kimi acp` E2E shows `mcp.tools_discovered` for `sawe`,
  `toolCount=109`, and a `mcp__sawe__solutions_list` call round-tripping
  through the `sawe --nc` bridge with the real editor response.

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
