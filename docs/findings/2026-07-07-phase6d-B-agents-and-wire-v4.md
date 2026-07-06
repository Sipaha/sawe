# Phase 6d-B — background agents onto streams + wire v3→v4 (cross-repo hard cutover)

**Date:** 2026-07-07. **Character:** the big cross-repo cutover — fold background AGENTS onto their
demux `Teammate` stream, put shells on the wire, drop the two separate bg-fetch tools, bump
`wire_schema_version` 3→4, and ship `sawe-mobile` in lockstep. Supersedes the 6d-A wire-invisible gate.

## What shipped

### Server (`sawe`, crate `solution_agent` + `editor_mcp` + `remote_control`)
- **bg-agent fold (desktop strip).** Removed the `∈ active_subagents` bridge filter in
  `session_view/task_subagent_strip.rs` so ALL live `Teammate` streams render as pills — an async
  `Agent` teammate already keeps a live `StreamId::Teammate(parent_tool_use_id)` demux stream (its
  entries are tagged into the parent thread), so it now shows as a normal `Task` pill instead of a
  separate `Background` pill. Pill label resolves: inline Task → its `active_subagents` friendly
  label; async Agent (dropped from `active_subagents` at spawn-ack) → its `background_agents` entry's
  JSONL snapshot `activity_label` (looked up by `parent_tool_use_id`); fallback → raw teammate id.
  Deleted the whole background-AGENT pill machinery (`bg_agents` snapshot loop, `background_pill`,
  `BackgroundAgentDisplayState`, `classify_background_agent_display`, classifier tests).
- **wire (v4).** `build_streams_vec` no longer filters `StreamKind::Shell`; `GetSessionTool` +
  `GetSessionChangesTool` no longer coerce a `Shell(...)` stream_id → Main — shells now ride the wire
  as `kind: shell` and are selectable/pageable like any stream. Bg-agents ride the wire as
  `kind: teammate` (no new StreamKind — `StreamKindDto` decodes Main/Teammate/Shell only, and the
  mobile enum THROWS on an unknown kind, so a 4th kind was deliberately NOT added).
  `wire_schema_version` 3→4 (`crates/editor_mcp/src/tools/capabilities.rs`).
- **removed the two separate bg tools:** `GetSessionBackgroundShellsTool` /
  `GetSessionBackgroundAgentsTool` (+ their Params/Result structs + registration + 5 unit tests) in
  `mcp.rs`; their `GLOBAL_TOOLS`/`SHARED_TOOLS` entries in `lifecycle.rs`; their
  `remote_control/allow_list.rs` arms + table tuples. Tool catalog 88→86.
  **KEPT** (still used by `event_sources.rs`, which still emits `agent_session_background_
  {shells,agents}_changed`): `BackgroundShellDto`, `BackgroundAgentDto`, `background_shell_dto`,
  `build_background_shells_vec`, `background_agent_dto`, `build_background_agents_vec`. Event cleanup
  is 6d-tail. `server_e2e_test.rs` version assert bumped to `>= 4`.

### SCOPE FENCE respected (deferred to 6d-tail)
`SubagentView::Background` variant + its render/selection arms, `build_background_entries_for_render`,
`on_background_agents_changed`, `next_selection_after_background_change`, the `background_agents` map /
`background_agent_order`, and `refresh_background_agent_snapshot`→`close_stream(Teammate(parent))` on
real stop_reason are ALL unchanged. After removing the Background PILL, the variant is
unreachable-but-compiles; the full `SubagentView`→`StreamId` collapse + its removal is 6d-tail. This
kept the cross-repo wire cutover's server diff tight and reviewable. `wire_dict.rs` /
`WireDictionary.kt` (byte-pinned Adler-32 across the two repos) left untouched — the dead RPC-name
literals are harmless compression-dictionary entries; pruning them is a coordinated future edit.

### Mobile (`spk-editor-mobile`)
Deleted the parallel legacy subsystem (the wire-decode + delta-merge were already generic, so a
shell/async-agent stream flows through with no decode change): `BackgroundShellStrip`/
`BackgroundAgentStrip` + their sheets/pills/helpers + `selectedShellId`/`selectedAgentId` wiring
(`SessionDetailScreen.kt`); `_backgroundShells`/`_backgroundAgents` StateFlows + resets + the 2 seed
RPCs + the 2 notification handlers + `loadBackgroundShellOutput` (`SessionDetailStore.kt`); the 2
subscription kinds + dispatch arms + `DetailNotificationRouter` methods + `loadBackgroundShells`/
`loadBackgroundAgents` (`SessionListStore.kt`); the delegating accessors in `MainViewModel.kt`; all 6
now-unreferenced DTO types (`BackgroundShellDto`, `BackgroundAgentDto`, the 2 Result + 2 Payload
types) in `RemoteDtos.kt`. `SUPPORTED_WIRE_SCHEMA_VERSION` 3→4 (hard cutover: a v4 client rejects a v3
server via `isServerTooOld`). Retired `BackgroundShellTest.kt`/`BackgroundAgentTest.kt`; added a
`StreamKindDto.SHELL` decode round-trip + a Shell-kind fixture + golden PNG
(`StreamTabStrip_main_teammate_shell.png`) to the Roborazzi snapshot test.
`SubagentTabStrip` already iterates every stream with no kind filter, so Shell/Teammate render with
zero UI change. The notification dispatch `when` has no `else` → an unknown kind (a server still
emitting the retired events) is silently ignored, no crash.

## Verification (BOTH gates passed; controller re-verified the diffs + the pixels)
- **Server:** `cargo test -p solution_agent --lib` **543 passed** (was 553; −10 = 5 removed tool tests
  + 5 removed classifier tests); `remote_control` 60 + integration green; `editor_mcp` server_e2e v4
  assert green; clippy warning count identical before/after (all pre-existing, none in touched files).
- **Mobile:** `:core:test` 332 green; `:app:compileDebugKotlin` green; `:app:testDebugUnitTest` 37
  green incl. both Roborazzi snapshots in Compare mode.
- **Desktop offscreen screenshot gate** (`script/run-mcp --debug --headless` + `seed_cold_session`):
  - Shot A (`live_teammates` + `live_shell`): strip = `Main | toolu_alpha | toolu_beta |
    ⧉ seedshell·cargo test --release` — inline-Task teammate pills + shell pill intact, no regression
    from removing the bg-agent machinery.
  - Shot B (`live_teammates:false` + subagent entries): strip = `Main | toolu_async1` — a teammate
    stream NOT in `active_subagents` now renders its pill (raw-id fallback label). Pre-6d-B the
    `∈ active_subagents` filter hid it; this is the async-agent fold render path proven live.
- **Android emulator render gate — v4 ↔ v4, PASSED end-to-end** (headless AVD `saweEmu`, debug v4
  editor over TLS+HMAC remote-control, live `ESTAB :21773`). `editor.capabilities` → `wire_schema_
  version: 4`; mobile `SUPPORTED_WIRE_SCHEMA_VERSION = 4`. Proofs (real device screencaps, controller-
  inspected): (1) v4↔v4 handshake accepted — app listed the workspace, NO version reject;
  (2) **headline:** session detail tab strip = `Main | toolu_gate1 | seedshell·cargo test --release`
  built from the v4 `streams` descriptor list — the SHELL pill is present (shells are NEW on the v4
  wire; a v3 client never rendered them); (3) shell stream body renders via a `stream_id`-scoped fetch
  (fenced `cargo test --release` output); (4) Main intact / teammate text scoped to its own tab.

## Review
Subagent implementer → subagent reviewer → controller re-verification, both repos. Both reviewers
returned no blocker/major/minor findings; controller re-read both diffs and personally inspected all
gate screenshots. Async-agent label lookup (`Option<&SharedString>` comparison), no double-pill, and
Shell wire round-trip all confirmed correct.

## Gotchas for future sessions
- The debug editor's `remote-control.json` schema is `{"enabled":true,"server_port":N,
  "clients":[{"name":..,"secret_base64":<STANDARD base64>,"created_at":<RFC3339>}]}` (see
  `crates/remote_control/src/model.rs::AuthorizedClient`); the pairing URL uses url-safe-base64-no-pad
  for both `secret` and `server_fp`. The AVD retains app data across runs — a stale pinned server cert
  fingerprint needs "Forget this server" before re-pairing with a fresh cert.
- After the coordinated push, this is the LAST wire bump before 6d-tail (desktop-only, no wire).

## Coordinated push
Editor push is pre-authorized, but v4 is a HARD cutover (a v4 server breaks a v3 mobile client), so
`sawe` + `sawe-mobile` push TOGETHER only after the user's one-line confirm.
