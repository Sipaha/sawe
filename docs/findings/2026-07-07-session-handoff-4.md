# Session handoff — post phase 6d-A (2026-07-07)

**READ FIRST on resume.** Pause snapshot after shipping **6d-A** (shells→streams). Supersedes
`2026-07-06-session-handoff-3.md`.

## Commit chain since last handoff (all on `sawe` `origin/main`, pushed)
- `7aeeee7470` / `7144dc94d2` / `0b5bf1bc58` — phase 6c (see handoff-3).
- `ed335daa49` — **6d-A**: background SHELLS fold into `session.streams` as auto-closing
  `StreamId::Shell` tabs. Desktop + model only, WIRE-INVISIBLE (v3 wire byte-identical).

`sawe-mobile` UNCHANGED (`origin/main` `dc1977d`).

## 6d-A recap (detail: `findings/2026-07-07-phase6d-A-shells-into-streams.md`)
Shells render as `session.streams` tabs (kind `Shell`), only while `Running` (auto-close on
exit — the dismissible terminal-× UX was dropped; the user confirmed they never used/saw it).
cx-free `BackgroundShell::stream_entry`/`stream_label`, injected in `rebuild_streams` from
`background_shells` (rebuild now called at all 5 shell-mutation sites). Shell body renders through
the unified `parent_stream_id`→`selected_parent_stream_entries` path (view-only). Wire held v3
via a `StreamKind::Shell` filter in `build_streams_vec` + Shell-`stream_id`→Main coercion. 553
lib tests; screenshot gate PASSED; release-fast rebuilt.

## Locked design decisions for the rest of 6d (USER-APPROVED this session)
- **Shells auto-close on completion** (like teammates) — DONE in 6d-A.
- **Background AGENTS collapse onto their demux `Teammate` stream** per spec Decision 3: the
  JSONL is demoted to a completion-signal + on-disk archival fallback, NOT a live tab. The user
  ACCEPTED the resulting content reduction (the live async-agent tab shows only the outbound
  parent-thread demux content, not the JSONL's inbound spawn-prompt; a finished agent's tab
  auto-closes and is not re-shown after restart). This is 6d-B.

## NEXT: 6d-B — the big cross-repo cutover (best FRESH context; needs mobile-push confirm)

Fold background AGENTS into streams + flip the wire to v4 + mobile lockstep. Server + mobile +
emulator gate, then a COORDINATED push (editor push is pre-authorized but the v4 server breaks a
v3 mobile client, so ship both together — **`sawe-mobile` push needs a one-line user confirm**).

### Server (`sawe`, crate `solution_agent` + editor_mcp + remote_control)
1. **bg-agent fold:** an async `Agent` teammate ALREADY has a live `StreamId::Teammate(parent_tool_use_id)`
   demux stream in `session.streams` (its entries are tagged into the parent thread — CONFIRMED:
   `acp_thread.rs:224` `subagent_id_from_meta` + `claude_native/translate.rs` `stamp_subagent_meta`
   tag async-Agent chunks identically to inline Tasks). So the fold = stop rendering the separate
   `Background(BackgroundAgentId)` pill and let the async agent show as its `Teammate` pill.
   Concretely: drop the `∈ active_subagents` bridge filter in `task_subagent_strip.rs` (so async
   teammate streams show directly), delete `SubagentView::Background` rendering + `build_background_entries_for_render` +
   the `bg_agents` strip loop. KEEP `refresh_background_agent_snapshot`→`close_stream(Teammate(parent))`
   on real `stop_reason` (store.rs ~7257 — the async agent's only done-signal) and KEEP the
   `background_agents` map as a completion-signal/archival source (NOT a render source).
2. **wire:** remove the `StreamKind::Shell` filter in `build_streams_vec` + the Shell-`stream_id`
   coercion (6d-A's temporary gate); remove `GetSessionBackgroundShellsTool` / `GetSessionBackgroundAgentsTool`
   (`mcp.rs`) + their `GLOBAL_TOOLS`/`SHARED_TOOLS` entries (`lifecycle.rs:218-219,278-279`) +
   `remote_control/allow_list.rs:49-54,190-197`. `wire_schema_version` 3→4 (`capabilities.rs:94`).
   Bg-agents ride the wire as `kind: teammate` (StreamKindDto has no bg-agent variant — do NOT
   add one; mobile decodes Main/Teammate/Shell only). Update the loose `>= 2` assert in
   `editor_mcp/tests/server_e2e_test.rs:114`.
3. Consider whether `SessionSummary.active_subagents` + `agent_session_active_subagents_changed`
   + the shell/agent `event_sources` payloads can go (mobile decides — keep the dirty-poke if
   mobile still needs it). This bleeds into 6d-tail.

### Mobile (`spk-editor-mobile`, GitHub `Sipaha/sawe-mobile`) — NOT minimal
Full inventory in this session's scratch; key points (all `file:line` in the mobile repo):
- Wire-decode + delta-merge are ALREADY generic — a Shell-kind stream flows through with zero
  code change (`RemoteDtos.kt` `StreamIdDto` already has `Shell`; `SessionEntryMerge.kt:182`
  replaces `streams` wholesale; `ChatList` renders server-scoped entries with no client filter).
- **The work = delete the parallel legacy subsystem:** `SessionDetailScreen.kt` `BackgroundShellStrip`
  (`:4309-4377`) + `BackgroundAgentStrip` (`:4451-4503`) + their sheets + `selectedShellId`/`selectedAgentId`
  wiring (`:243-246,606-624,711-739`); `SessionDetailStore.kt` `_backgroundShells`/`_backgroundAgents`
  StateFlows + their triplicated reset (`reset`/`openSession`/`closeSession`) + the 2 seed RPCs
  (`:702-720`) + the 2 notification handlers (`:929-955`); `SessionListStore.kt` `DetailNotificationRouter`
  hooks + the `agent_session_background_{shells,agents}_changed` kind registrations/dispatch
  (`:324-336,566-588`) + `loadBackgroundShells`/`loadBackgroundAgents` (`:635-676`).
- Bump `SUPPORTED_WIRE_SCHEMA_VERSION` 3→4 (`RemoteDtos.kt:61`) + KDoc + the two hardcoded-`3`
  tests (`RemoteDtosTest.kt:18-28`).
- Retire `BackgroundShellTest.kt` / `BackgroundAgentTest.kt`; add a Shell-kind fixture + golden PNG
  to `StreamTabStripSnapshotTest.kt` (`app/src/test/.../solutions/`).
- **Emulator gate:** reuse the recipe in `docs/findings/2026-07-06-phase5-mobile-streams.md` §"Render
  gate" (headless AVD, debug v4 editor, `remote-control.json` inject, `seed_cold_session`
  [now with `live_shell`], `adb input` pairing). Verify a shell/teammate tab from `streams` on v4↔v4.
- HARD RULE: never touch `spk-editor-mobile/.superpowers/sdd/{progress.md,task-R-brief.md}`.

### Then 6d-tail (server, desktop-only)
`SubagentView`→`StreamId` collapse (drop `Background` variant); remove `active_subagents`/
`active_subagent_order` (+ `build_active_subagents_vec`) if the wire no longer needs them;
`background_agent_order`; the now-dead `remove_background_shell` (store.rs, × removed in 6d-A) +
its `db.delete_background_shell` plumbing; the inert `is_parent_thread_view`. Then 6e (docs +
whole-branch review).

## Environment / cannot-rederive
- Editor push pre-authorized; mobile push needs one-line confirm; v4 is a HARD cutover so push
  both together after confirm.
- Screenshot gate: `script/run-mcp --debug --headless`, global sock `~/.spk/sawe-dev/config/mcp.sock`,
  per-sol `~/.spk/sawe-dev/config/solutions/streams-gate/mcp.sock` (has `seed_cold_session` +
  `workspace.screenshot`). `seed_cold_session` now takes `live_teammates: bool` AND
  `live_shell: Option<String>`. Helper `/tmp/6b-mcpcli.py`. Open the sol via `solutions.open`,
  click a session tab via `windows.click_at` at the console-panel tab row, click a strip pill
  similarly. NO dev editor running now (torn down).
- `script/run-mcp` only recompiles if the binary is MISSING → `cargo build --bin sawe` after any
  edit before a screenshot. `cargo clippy -p solution_agent --all-targets` (NOT `script/clippy` —
  its deny-gate hits a pre-existing lint in `crates/git/src/backup.rs:114`).
- Pre-existing (not ours) warnings: `store.rs` unused `AgentSettings` import + a `drop(&ref)`;
  git_ui `panel_button`/`build_commit_message_prompt` dead-code.

## In flight
Nothing uncommitted (tracked). `sawe` clean at `ed335daa49` (pushed). 6d-A DONE.
Remaining: 6d-B (cross-repo) → 6d-tail → 6e.
