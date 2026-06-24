# Background Shells on Mobile — implementation plan

**Status:** COMPLETE (2026-05-29). Server: `spk-editor` `cdfd800e0f` (get_session_background_shells
tool + agent_session_background_shells_changed notification + allow-list; 330 solution_agent tests,
clippy clean). Client: `spk-editor-mobile` `2ae83135` (BackgroundShellDto + dispatch + BackgroundShellStrip
pills + stdout drill-in sheet; `:core:test` green incl. 7 new BackgroundShellTest; `:app:assembleDebug`
OK, debug APK ~22.4 MB). Additive wire, no schema bump. Needs: editor release-fast rebuild to serve the
new wire for end-to-end mobile testing.
**Track:** HEAVY, two repos (server in `spk-editor`, client in `spk-editor-mobile`).
**Goal:** Surface the desktop V3 "Background Shells Strip" on the Android client — a pill
strip of the session's background shells (Bash run_in_background) with state + a drill-in
showing the stdout tail. Follows the "F" (sub-agent indication) arc precedent: server wire
exposure → mobile client.

## Context
V3 shipped background shells as a DESKTOP-only GPUI feature in `solution_agent`
(`session_view`/`task_subagent_strip`). The data (`SolutionSession.background_shells`) is
NOT exposed over the remote wire (grep of `remote_control`/`mcp.rs` is empty). The mobile
client consumes sessions via `remote.*` proxy + `solution_agent.*` tools + `agent_session_*`
notifications. This arc adds the wire layer + the client UI, mirroring F-server/F-phone and
R-5e. Scope is SHELLS ONLY — background *agents* on mobile is a separate follow-up.

## Architectural decisions
1. **Additive wire — no schema bump.** New tool + new notification only. Old clients ignore
   the unknown notification; the new tool is opt-in; mobile DTOs default-tolerate missing
   fields. Do NOT bump `SUPPORTED_WIRE_SCHEMA_VERSION` (still 2).
2. **Notification carries the lite list** (id/command/state/mtime_ms, NO output_tail), like
   `agent_session_active_subagents_changed`. The client updates its pills straight from the
   notification — no refetch for the strip.
3. **Drill-in output via the tool with `include_output=true`** (mirrors
   `include_full_content`/`include_images`). Keeps notifications + strip-load small; the
   64 KiB tail only crosses the wire when the user opens a shell.
4. **mtime → epoch ms**, no unwrap in prod: `mtime.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).ok()`.
5. **`state` as the existing `to_state_text()` string** (`running`/`exited:N`/`exited`/`killed`)
   — the client parses it for the pill color/label. Reuse, don't invent a new enum on the wire.

## Scope

### Phase 1 — SERVER (`spk-editor`, crate `solution_agent` + `remote_control`)

A. **`crates/solution_agent/src/mcp.rs`** — mirror `GetSessionChildrenTool`:
   - `BackgroundShellDto { id, command, state, mtime_ms: Option<i64>, output_tail: Option<String> }`
     (`#[serde(skip_serializing_if=…)]` on the optionals, like `SessionSummary`/`EntrySummary`).
   - `GetSessionBackgroundShellsParams { session_id, #[serde(default)] include_output: bool }`.
   - `GetSessionBackgroundShellsResult { background_shells: Vec<BackgroundShellDto> }`.
   - `GetSessionBackgroundShellsTool`, `NAME = "solution_agent.get_session_background_shells"`,
     handler validates session exists, returns shells in `background_shell_order`,
     populates `output_tail` only when `include_output` (from `latest.output_tail`).
   - A shared `fn background_shell_dto(shell, include_output) -> BackgroundShellDto` reused by
     both the tool and the notification builder (notification passes `include_output=false`).
   - Register it in `register(cx)`.
B. **`crates/solution_agent/src/event_sources.rs`** — replace the `SessionBackgroundShellsChanged(_) => {}`
   no-op (line ~141) with `emit_notification(cx, "agent_session_background_shells_changed",
   build_background_shells_changed_payload(id, cx))`. Add the builder (mirror
   `build_active_subagents_changed_payload`): `{ session_id, background_shells: [lite DTOs] }`.
   Add the kind to the module docstring list.
C. **`crates/remote_control/src/allow_list.rs`** — add
   `"remote.solution_agent.get_session_background_shells" => Some("solution_agent.get_session_background_shells")`.
   (The notification auto-forwards — `should_forward_event` already passes any `agent_session_` prefix.)
D. Tests: an mcp-level test that a session with a registered shell returns it via the tool
   (lite + `include_output=true`); a test that `include_output=false` omits `output_tail`.
   Mirror existing `get_session_children` / `get_session_entry` tests. Verify
   `cargo build --bin spk-editor` + `cargo test -p solution_agent --lib` + clippy
   `-p solution_agent -p remote_control --no-deps`.

### Phase 2 — CLIENT (`spk-editor-mobile`)

E. **`core/.../RemoteDtos.kt`** — `BackgroundShellDto(id, command, state, @SerialName("mtime_ms") mtimeMs: Long?=null, @SerialName("output_tail") outputTail: String?=null)`;
   `GetSessionBackgroundShellsResult(@SerialName("background_shells") backgroundShells: List<BackgroundShellDto> = emptyList())`;
   `SessionBackgroundShellsChangedPayload(@SerialName("session_id") sessionId, @SerialName("background_shells") backgroundShells: List<BackgroundShellDto> = emptyList())`.
   `@Serializable`, all optionals defaulted (forward-compat). `:core` decode tests
   (mirror `RemoteDtosTest`): decodes full + tolerates missing optionals + unknown `state` string.
F. **`core/.../RemoteClient.kt` / store** — a `loadBackgroundShells(sessionId, includeOutput)` call
   on `remote.solution_agent.get_session_background_shells` → `decodeResultOrThrow(GetSessionBackgroundShellsResult.serializer())`.
   Mirror the `get_session_children` call site.
G. **`app/.../vm/SessionListStore.kt`** — add `"agent_session_background_shells_changed"` to
   `notificationKinds` + a dispatch arm decoding `SessionBackgroundShellsChangedPayload` →
   `router.onBackgroundShellsChanged(payload)`. Add `onBackgroundShellsChanged` to the
   `DetailNotificationRouter` interface; `SessionDetailStore` implements it (update a
   `_backgroundShells` StateFlow). On `openSession`, fetch the lite list once.
H. **`app/.../ui/solutions/SessionDetailScreen.kt`** — a `BackgroundShellStrip` Composable
   (mirror `SubagentTabStrip`): horizontally-scrollable pills, one per shell, label
   `command` truncated, color by parsed `state` (running=primary, exited:0=tertiary/green,
   exited:N=error, killed=error-ish). Tap → drill-in (bottom sheet or expandable) that calls
   `loadBackgroundShells(includeOutput=true)` and shows the matching shell's `output_tail`
   in a monospace block. Render below/near the existing `SubagentTabStrip`.
I. Tests: `:core` DTO tests (E). Verify `./gradlew :core:test` green; `./gradlew :app:assembleDebug`
   compiles (Android SDK present per prior arcs). Note APK size delta.

## Out of scope
- Background *agents* (managed agents) on mobile — separate follow-up, same pattern.
- Live output streaming (the tail is fetched on drill-in open / refetched on notification;
  no incremental push — matches the desktop's snapshot model).
- A "kill shell" / × action from mobile (read-only surface for V1).

## Verification
- Server: `cargo build --bin spk-editor`; `cargo test -p solution_agent --lib`;
  `cargo clippy -p solution_agent -p remote_control --no-deps --all-targets -- -D warnings`.
- Client: `./gradlew :core:test`; `./gradlew :app:assembleDebug`.
- Supervisor end-to-end: optional — would need the mobile app against a live editor; defer to
  user hands-on (document a recipe in the handoff).

## When done
- Server tool + notification + allow-list shipped + tested on `spk-editor` main.
- Mobile DTO + dispatch + strip + drill-in shipped + `:core` tests green on `spk-editor-mobile` main.
- FORK.md untouched (no upstream-Zed files). INDEX + handoff updated.
