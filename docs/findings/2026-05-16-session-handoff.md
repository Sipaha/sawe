# Session handoff — 2026-05-16

**Status:** session paused for context reset; resume from current `main`.

This supersedes [`2026-05-15-session-handoff.md`](2026-05-15-session-handoff.md).
The previous session was interrupted before writing its own handoff; this
one captures the cumulative state after Remote Control phases R-2/R-3/R-4
shipped and after auditing the rest of the 2026-05-15 pool.

## What shipped between 2026-05-15 handoff and now

| Phase | Commit chain | Plan / artefact |
|---|---|---|
| Remote Control R-2 (TLS + WS + HMAC listener) | `7020b69` (ADR-0003) → `759365d` (plan) → `5519f82` (deps) → `a38f34e` → `977bd29` → `731a97c` → `8bc441e` (finalize) | [`plans/2026-05-15-remote-control-R2.md`](../plans/2026-05-15-remote-control-R2.md), [`adr/0003-remote-control-protocol.md`](../architecture/decisions/0003-remote-control-protocol.md), [`findings/2026-05-remote-control-watcher-echo.md`](2026-05-remote-control-watcher-echo.md) |
| Remote Control R-3 (server fingerprint in pairing QR) | `85dec79` | (No separate plan-doc — small enough to ride on the R-1.5 / R-4 work.) |
| Remote Control R-4 (`remote.*` proxy to embedded MCP socket) | `25024fe` (plan) → `5d8e013` → `ff8f00e` (tick acceptance + envelope quirk finding) | [`plans/2026-05-16-remote-control-R4.md`](../plans/2026-05-16-remote-control-R4.md), [`findings/2026-05-remote-control-r4-mcp-envelope.md`](2026-05-remote-control-r4-mcp-envelope.md) |
| Workflow doc: resume-from-paused-session bootstrap | `7540f6b` | [`workflow/supervisor-mode.md`](../workflow/supervisor-mode.md) |
| Workflow doc: `pgrep -f` self-match anti-pattern | `0ee8e7d` | [`workflow/supervisor-mode.md`](../workflow/supervisor-mode.md) anti-patterns section |
| INDEX hygiene: R-4 row + fresh handoff link | (this commit) | — |

`cargo test -p remote_control` is 40-green (37 unit + 2 listener_e2e + 1 proxy_e2e). `cargo check -p remote_control --all-targets` clean.

## Phase E (queued message → claude) — **already shipped before this session**

The previous 2026-05-15 handoff listed E as "outstanding". Audit on resume
found it had actually shipped earlier (commits `7b0b4c5` /clear+queue UX,
`e1ebb2e` queue audit logs, then later cold-resume polish in
`06731b9` / `f80f183`). The complete surface:

- `SolutionSession::pending_messages: VecDeque<Vec<acp::ContentBlock>>`
  in [`crates/solution_agent/src/model.rs`](../../crates/solution_agent/src/model.rs).
- Queue bubble + collapse strip + "send now" Bolt button in
  [`crates/solution_agent/src/session_view/render_queue.rs`](../../crates/solution_agent/src/session_view/render_queue.rs).
- Up-arrow recall in
  [`crates/solution_agent/src/session_view/recall.rs`](../../crates/solution_agent/src/session_view/recall.rs).
- Drain-on-`Stopped(Cancelled)` in `store::handle_acp_event`.
- Notifier suppresses `Completed` when the queue is non-empty
  ([`notifier.rs::decide_notification`](../../crates/solution_agent/src/notifier.rs)).
- MCP tool `solution_agent.send_message` returns immediately with `"queued"`.
- Cold-resume optimistic ghost bubble (`render_resuming_section`) so the
  3-4 s ACP handshake doesn't look like a stuck Send.

Don't reopen this phase. If a sub-issue surfaces (a queue UX miss), file
a new dated plan-doc — don't resurrect "E".

## Findings created in this session pair

- [`2026-05-remote-control-watcher-echo.md`](2026-05-remote-control-watcher-echo.md) — FS-watcher self-write echo loop in `remote_control::store`; resolution: `self_write_echoes` counter squelches the next inbound event after each `RemoteControlSettingsBackend::write`.
- [`2026-05-remote-control-r4-mcp-envelope.md`](2026-05-remote-control-r4-mcp-envelope.md) — Bare `{"method":"editor.capabilities"}` returns `-32601`; the embedded `editor_mcp` server actually wants `tools/call { name: "editor.capabilities", arguments }`. R-4's `proxy::call` had to wrap calls in this envelope before the proxy round-trip would succeed.

## Pool — outstanding tasks at session end

| Item | Track | Where | Notes |
|---|---|---|---|
| **R-5a** Android client bootstrap | HEAVY | `sawe-mobile` (sibling of `sawe`) | **Shipped 2026-05-16**: sibling repo commit `77eb966`; two-module Gradle (`:core` JVM + `:app` Compose stub), 30 green `:core` unit tests, `:app:assembleDebug` confirmed to fail only on missing `ANDROID_HOME`. See [`plans/2026-05-16-remote-control-R5a-android-bootstrap.md`](../plans/2026-05-16-remote-control-R5a-android-bootstrap.md). |
| **R-5a-followup** :cli + live integration test | LIGHT | (same sibling repo) | **Shipped 2026-05-16**: sibling-repo commit `4e478f1`. `:cli` JVM smoke client + six-step `LiveEditorIntegrationTest` (still tag-gated as `integration`, default `:core:test` keeps 30 PASSED baseline). Supervisor-verified including the `-DincludeTags=integration` discovery path. |
| **R-5a-fixup** `:core` api configuration | LIGHT | (same sibling repo) | **Shipped 2026-05-16**: sibling-repo commit `d83ab47`. Promoted `okhttp` / `kotlinx-coroutines-core` / `kotlinx-serialization-json` in `:core/build.gradle.kts` from `implementation` to `api` after Android SDK install surfaced "Unresolved reference 'serialization'" and "Cannot access class 'okhttp3.OkHttpClient.Builder'" in `:app:compileDebugKotlin`. Dropped redundant declarations from `:cli`. `:app:assembleDebug` → 9.5 MB APK, `:core:test --rerun-tasks` 30 PASSED. The R-5a deferred follow-up "OkHttpClient.Builder leak on :core public surface" is now resolved. |
| **R-5b** QR scanner | HEAVY | (same sibling repo) | **Shipped 2026-05-16**: sibling commit `6e444e5`. zxing-android-embedded:4.3.0 + real camera-permission flow + manual-entry fallback. APK 10.9 MB. Gotcha logged: zxing needs an explicit `androidx.appcompat` dep declaration. |
| **R-5c** Solutions/sessions list UI | HEAVY | (same sibling repo) | **Shipped 2026-05-16**: sibling commit `7fa4615`. Navigation Compose graph (pairing → solutions → solution detail → session-detail stub) + DTOs in `:core` + live `agent_session_state_changed` re-fetch. `:core` tests 30 → 41. APK 11.18 MB. |
| **R-5d** Chat with send / cancel / live entries | HEAVY | (same sibling repo) | **Shipped 2026-05-16** — closes the R-5 arc. Sibling commit `6ef0cd7`. Chat surface with bubbles by role, optimistic Send, Cancel button, id-only event → `get_session` refetch. `:core` 45 tests, APK 11.22 MB. |
| **R-5e** Server-side enrichment for remote chat | HEAVY | sawe (`crates/solution_agent/src/mcp.rs` + `event_sources.rs`) | **Shipped 2026-05-16**: commit `d8592b05dc`. EntrySummary gained optional markdown/images/tool_call/plan fields, gated by include_full_content + include_images; agent_session_message_appended notification now carries entry_index + role + preview; new remote.solution_agent.get_session_entry tool; allow-list extended. solution_agent tests 83 → 90. The Android client (R-5e-client / R-5f) still needs to be updated to consume the new fields. |
| **R-5f** Android consumes R-5e enriched data + diff streaming | HEAVY | (sibling) | **Shipped 2026-05-17**: sibling commit `ee804aa`. `:core` DTOs extended; rich markdown render via `multiplatform-markdown-renderer-m3:0.27.0` with inline `spk-image://N` resolved via custom `ImageTransformer`; per-entry diff fetch via `get_session_entry(entry_index)` keyed by `agent_session_message_appended` payload. `:core` tests 45 → 54, APK 11.21 → 11.58 MB. |
| **R-5g** create-session from phone | HEAVY | sawe (server) + sibling repo (client) | **Shipped 2026-05-17**: server commit `3fb5ee51ac` (new `solution_agent.list_agents` tool + allow-list extension, solution_agent tests 90 → 91); client commit `41531a1` in sibling (Material 3 AlertDialog with RadioButton agent picker + optional initial message + auto-open-and-retry on `no_active_workspace_for_solution`). `:core` tests 54 → 57, APK 11.58 → 11.59 MB. |
| **R-6a** Network resilience | HEAVY | (sibling) | **Shipped 2026-05-17**: sibling commit `c69e7e3`. Auto-reconnect with 1-2-4-8-16-30s backoff, OkHttp `pingInterval(30s)`, in-memory outbound queue (`queueCall` with 5-min TTL + FIFO reflush) used by `MainViewModel.sendMessage`, subscription auto-restore, `ConnectionState` StateFlow + Compose banner, `lastSeenEntryIndex` resume on Disconnected→Connected. New `RemoteTransport` seam (`OkHttpRemoteTransport` + `FakeRemoteTransport` for tests). `:core` tests 57 → 72. APK 11.63 MB. Outbound queue is in-memory only (force-kill drops queued messages — documented). |
| **R-6b** Production polish | HEAVY | (sibling) | Pairing URL persistence (encrypted SharedPreferences) so app restart doesn't lose pairing, Settings screen (forget server / re-pair / view fingerprint), signed-release Gradle config + README docs, app launcher icon placeholder, INTERNET permission audit. Next phase. |
| **R-6c (deferred)** FCM push + multi-server | HEAVY | (sibling) | Push notifications when agent finishes a turn (needs Firebase project setup by maintainer); multiple paired servers (one phone, multiple workstations). Defer until single-server flow is in real-world use. |
| **F** Sub-agent indication UI | HEAVY | `spk-cockpit` (different project) | **Audit 2026-05-16: spk-cockpit has no sub-agent / AI surface.** It's a personal productivity tray app (todos, timers, calendar, Markdown notes). The user's 2026-05-15 description ("show running sub-agents with progress / tokens / interrupt") doesn't match anything in the current cockpit codebase. Needs clarification before action — possibly was a different project the user had in mind, or a green-field feature to be designed from scratch. |
| **G** `spk-image://` URL in queued message | LIGHT | `spk-cockpit` (different project) | **Audit 2026-05-16: no `spk-image://` references anywhere in the cockpit source tree (web/src/, internal/, cmd/).** cockpit doesn't have queued-message UI either. Same situation as F — the handoff description does not match the codebase. Needs clarification. |

**Pre-flight audit (2026-05-16):** F and G as written in the 2026-05-15 handoff cannot be acted on. spk-cockpit's actual surfaces (kanban todos, time tracking, CalDAV calendar, Markdown notes, tray menu) have nothing to do with sub-agent indication or the `spk-image://` URL scheme. Either the 2026-05-15 supervisor misfiled them, or they describe planned-but-not-yet-existing functionality, or the user has a different project in mind. The next supervisor session must surface this for the user before attempting either task.

**Within this `sawe` repo: pool is empty.** All in-tree HEAVY phases
from the 2026-05-15 plan arc are either shipped or in a separate repo.
The natural next thing inside this cwd is whatever the user names next.

## Open architectural decisions

- **R-5 repo location** — confirmed separate repo (not a sibling crate), per [`plans/2026-05-15-remote-control.md`](../plans/2026-05-15-remote-control.md). Needs creation before R-5 work can be dispatched.
- **`MinimalDispatcher` retention** — kept as `#[cfg(test)]`-friendly fallback for unit/integration tests that don't want a live MCP socket. Don't delete it during cleanup passes.

## Active gotchas the next session should know

1. **Agent SDK worktree branches from session-start HEAD.** Inline plan-doc content + tell sub-agent to rebase. See [`findings/2026-05-agent-worktree-staleness.md`](2026-05-agent-worktree-staleness.md).
2. **`script/run-mcp --headless` is the default** for agent-driven runs (post ADR-0002, no Xvfb needed).
3. **MCP `windows.click_id`** by stable ID is preferred over `windows.click_at`.
4. **`workspace.screenshot` works in headless** (offscreen wgpu).
5. **`editor_mcp` over the socket needs `tools/call` envelope** — bare-method calls get -32601. See R-4 envelope finding.
6. **FS-watcher self-write echo** — any settings-file writer that also watches the file must squelch its own writes. R-2 uses `self_write_echoes` counter; mirror the pattern for any new live-watched config.
7. **`pgrep -f` in watch-loops self-matches the running bash.** Use marker grep or `pgrep | grep -v $$`. See `supervisor-mode.md` anti-patterns.
8. **MCP tool catalog count is 60** (unchanged this session — R-4 added the `remote.*` namespace but those tools are proxied through, not registered with the local registry).

## Stale handoff notice

`2026-05-15-session-handoff.md` lists E + R-2/R-3/R-4 as outstanding. They
are not — E shipped earlier (audited above), R-2/R-3/R-4 shipped between
the two handoffs. Treat 2026-05-15 as the snapshot at that moment and
this file as authoritative for current state.

## Resume recipe for the next session

1. Read this file first.
2. Read `docs/INDEX.md`.
3. Read `docs/workflow/supervisor-mode.md`.
4. `git log --oneline -25` to confirm the chain.
5. The in-tree pool is empty — wait for the user to name the next task
   or pick something workable from `spk-cockpit` if cwd has shifted.
