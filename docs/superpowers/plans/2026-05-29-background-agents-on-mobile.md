# Background Agents on Mobile — implementation plan

**Status:** COMPLETE (2026-05-29). Server: `sawe` `f0d1c197b7` (get_session_background_agents
tool + agent_session_background_agents_changed notification + allow-list; 334 solution_agent tests,
clippy clean). Client: `sawe-mobile` `93969b7` (BackgroundAgentDto + dispatch + BackgroundAgentStrip
pills running/done + minimal drill-in sheet; `:core:test` green incl. 6 new BackgroundAgentTest;
`:app:assembleDebug` OK, debug APK ~23.6 MB). Additive, no schema bump. Needs editor release-fast
rebuild to serve the new wire. Follow-up: full JSONL-transcript drill-in for agents.
**Track:** HEAVY, two repos. **This is a near-exact mirror of the just-shipped
`2026-05-29-background-shells-on-mobile.md` arc** — read that plan + its commits
(`sawe cdfd800e0f`, `sawe-mobile 2ae83135`) as the template. This doc only
records the DELTAS for managed **agents**.

**Goal:** surface the desktop managed-agents ("Background Agents") strip on the Android
client — pill strip + a minimal drill-in — consuming new server wire support.

## Why a separate arc (deltas vs shells)
Same gap (agents not on the wire; `SessionBackgroundAgentsChanged(_) => {}` no-op at
`event_sources.rs:137`), same desktop data (`SolutionSession.background_agents` +
`background_agent_order`), but different snapshot shape:
- `BackgroundAgent { id, jsonl_path, registered_at, latest: Option<BackgroundAgentSnapshot>, last_offset }`.
- `BackgroundAgentSnapshot { mtime: SystemTime, activity_label: SharedString, stop_reason: Option<SharedString> }`.
- **No `command`, no `output_tail`.** The pill label is `activity_label` (default
  `"Generating…"` when no snapshot yet). Terminal = `stop_reason.is_some()` ("done") — there's
  no exit code. So **no `include_output` param** (the fields are small — always sent).

## Architectural decisions
1. **Additive wire — no schema bump** (same as shells).
2. **Notification carries the full list** (id/label/mtime_ms/stop_reason) — small, always-sent;
   client updates pills directly, no refetch.
3. **No include_* param** — agent DTO fields are tiny; always include them.
4. mtime → epoch ms via `.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).ok()` (no unwrap).
5. **V1 drill-in is minimal** — label + state + stop_reason + relative time. The full JSONL
   transcript drill-in (desktop renders it via `jsonl_to_entries`) is a FOLLOW-UP (needs a
   tail+convert+paginate wire tool — out of scope here).

## Scope

### Phase 1 — SERVER (`sawe`, `solution_agent` + `remote_control`) — mirror cdfd800e0f
A. `mcp.rs`: `BackgroundAgentDto { id, label, mtime_ms: Option<i64> (skip_if none), stop_reason: Option<String> (skip_if none) }`;
   `GetSessionBackgroundAgentsParams { session_id }` (NO include flag);
   `GetSessionBackgroundAgentsResult { background_agents: Vec<BackgroundAgentDto> }`;
   `GetSessionBackgroundAgentsTool` NAME `solution_agent.get_session_background_agents`, handler
   validates session + returns agents in `background_agent_order`. Shared `pub(crate)` builder
   `background_agent_dto(agent) -> BackgroundAgentDto` (label = `latest.activity_label` or
   `"Generating…"`; mtime from `latest.mtime`; stop_reason from `latest.stop_reason`). Register in `register(cx)`.
   Mirror the `background_shell_dto`/`build_background_shells_vec` placement (in `mcp.rs`, `pub(crate)`).
B. `event_sources.rs`: replace `SessionBackgroundAgentsChanged(_) => {}` (line ~137) with
   `emit_notification(cx, "agent_session_background_agents_changed", build_background_agents_changed_payload(id, cx))`.
   Add the builder (mirror `build_background_shells_changed_payload`). Add kind to the docstring list.
C. `remote_control/src/allow_list.rs`: add
   `"remote.solution_agent.get_session_background_agents" => Some("solution_agent.get_session_background_agents")`.
D. Tests: tool returns ordered agents (with + without a snapshot → label defaults to "Generating…");
   stop_reason surfaces; unknown session errors; notification-payload builder test. Mirror the shell tests.
   Verify `cargo build --bin sawe` + `cargo test -p solution_agent --lib` + clippy
   `-p solution_agent -p remote_control --no-deps`.

### Phase 2 — CLIENT (`sawe-mobile`) — mirror 2ae83135
E. `core/.../RemoteDtos.kt`: `BackgroundAgentDto(id, label, @SerialName("mtime_ms") mtimeMs: Long?=null, @SerialName("stop_reason") stopReason: String?=null)`;
   `GetSessionBackgroundAgentsResult(@SerialName("background_agents") backgroundAgents = emptyList())`;
   `SessionBackgroundAgentsChangedPayload(@SerialName("session_id") sessionId, @SerialName("background_agents") backgroundAgents = emptyList())`.
   `:core` decode tests (mirror `BackgroundShellTest`).
F. store: `loadBackgroundAgents(sessionId)` on `remote.solution_agent.get_session_background_agents`
   (NO include flag) → `decodeResultOrThrow`. Mirror `loadBackgroundShells`.
G. `SessionListStore.kt`: add `"agent_session_background_agents_changed"` to the kinds set + dispatch
   arm → `router.onBackgroundAgentsChanged(payload)`; add to `DetailNotificationRouter`;
   `SessionDetailStore` `_backgroundAgents` StateFlow, fetch on `openSession`, clear on reset/close.
H. `SessionDetailScreen.kt`: `BackgroundAgentStrip` (mirror `BackgroundShellStrip`): pills labelled
   `label` truncated; color: stop_reason==null → primaryContainer (running); stop_reason!=null →
   tertiaryContainer (done). Tap → minimal sheet (label + "done: <stop_reason>" or "running" + relative
   mtime). Render near `BackgroundShellStrip` / `SubagentTabStrip`.
I. Verify `./gradlew :core:test` + `:app:compileDebugKotlin` (or `:app:assembleDebug`).

## Out of scope (follow-ups)
- Full JSONL-transcript drill-in for agents (the rich desktop view).
- Merging the three mobile strips (subagents / shells / agents) into one row — keep separate for now.

## Verification / When done
Same matrix as the shells arc. Server tool+notification+allow-list tested on `sawe`;
mobile DTO+dispatch+strip+drill-in with `:core` tests green on `sawe-mobile`. FORK.md
untouched. Plan→complete, handoff + INDEX updated.
