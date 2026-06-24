# Session handoff — 2026-05-17

**Status:** session paused for context reset; resume from current `main`.

Supersedes [`2026-05-16-session-handoff.md`](2026-05-16-session-handoff.md). The
R-5 + R-6 arcs are closed end-to-end: the Android client is prod-shippable —
release APK 2.12 MB after R8, persists pairing across restarts, survives
flaky LTE/Wi-Fi, renders rich chat with images + tool-calls.

## What shipped on 2026-05-17

| Phase | Commit (and where) | Summary |
|---|---|---|
| R-5f | sibling `ee804aa` | Android consumes R-5e enriched data (markdown + images + tool_call + plan) via `multiplatform-markdown-renderer-m3:0.27.0`; diff streaming via `get_session_entry(entry_index)`. `:core` 45 → 54 tests, APK 11.21 → 11.58 MB. |
| R-5g server | spk-editor `3fb5ee51ac` | New `solution_agent.list_agents` MCP tool + allow-list extension. `solution_agent` 90 → 91 tests. |
| R-5g client | sibling `41531a1` | FAB stub replaced with Material 3 AlertDialog (RadioButton agent picker + optional initial message + auto-open-and-retry on `no_active_workspace_for_solution`). `:core` 54 → 57 tests, APK 11.58 → 11.59 MB. |
| R-6a | sibling `c69e7e3` | WS reconnect with 1-2-4-8-16-30s backoff, OkHttp `pingInterval(30s)`, `queueCall` 5-min TTL FIFO for `send_message`, subscription auto-restore, `ConnectionState` StateFlow + Compose banner, `lastSeenEntryIndex` per session. New `RemoteTransport` seam (`OkHttpRemoteTransport` + `FakeRemoteTransport`). `:core` 57 → 72 tests, APK 11.59 → 11.63 MB. |
| R-6b | sibling `c517f03` | Pairing persistence (`EncryptedSharedPreferences`) + Settings screen + adaptive launcher icon + INTERNET permission + signed-release Gradle config + ProGuard rules + expanded README. `:core` stable at 72 tests. Debug APK 11.63 → 12.0 MB; **release APK 2.12 MB after R8**. |
| Welcome delete fix | spk-editor `8c7d87c931` | Replaced silent `DeleteSolution` action dispatch (no Workspace in WelcomeWindow's focus tree → action dropped) with in-row "Delete?" [Yes][Cancel] confirmation in the same WelcomeEditMode state machine as Rename. |
| Remote Control Detect HTML bug | spk-editor `4cba330338` | `ifconfig.me` → `ifconfig.me/ip` so the public IP comes back as plain text instead of an HTML page. |
| Remote Control QR popover crash | spk-editor `900e894942` | `workspace.toggle_modal` from inside the modal's own listener double-borrowed; now deferred via `window.defer(...)`. |
| Welcome window delete | spk-editor `8c7d87c931` | (Same as the line above — duplicated bullet here is intentional, the welcome delete bug is the third of the three small bug fixes from the start of this session.) |

## Cumulative state of the Android client (sibling `spk-editor-mobile`)

Six R-5 phases + two R-6 phases shipped over 2026-05-16 → 2026-05-17. Sibling-repo chain:

```
c517f03 R-6b production polish (pairing persistence + settings + signed-release)
c69e7e3 R-6a WS reconnect + outbound queue + ping/pong
41531a1 R-5g New session dialog (agent picker + auto-open retry)
ee804aa R-5f rich rendering + diff streaming
6ef0cd7 R-5d chat surface (send / cancel / live entries)
7fa4615 R-5c solutions/sessions nav + live state subscribe
6e444e5 R-5b zxing QR scanner
d83ab47 R-5a fixup (:core api configuration after SDK install)
4e478f1 R-5a follow-up (:cli + LiveEditorIntegrationTest)
77eb966 R-5a bootstrap
```

End-state: `:core` 72 tests green, debug APK 12.0 MB, release APK 2.12 MB.
Build commands and pairing instructions live in the sibling repo's README.

**Sibling repo remote** (added 2026-05-18): `git@github.com:Sipaha/spk-editor-mobile.git`. Push uses the same `github.com-sipaha` SSH host-alias as the spk-editor remote (config in `~/.ssh/config`). The local directory was renamed from `spk-editor-android-client` to `spk-editor-mobile` on 2026-05-18 to match the GitHub repo name; older plan-docs (R-5a..R-5e historical record) still use the original name in their commit-message quotes but the project is now uniformly `spk-editor-mobile`.
No CI yet.

## Additional 2026-05-17 phases (after the 1st handoff cut)

| Phase | Commit | Summary |
|---|---|---|
| R-6d disk persistence | sibling `ff9ca8c` | `QueueStore` interface in `:core` + `EncryptedQueueStore` in `:app` (FIFO, .commit() synchronous so "Send → force-kill" path is durable); TTL **5 min → 24 h**; `DraftRepository` per session (debounced 500ms saves; read-and-clear `bouncedFor` channel); `LastSeenRepository` per session; `NavStateRepository` with route-template resolver; single shared `AppMasterKey` singleton. `:core` tests 72 → 82. |
| R-6e server pagination | spk-editor `b756392b52` | Cursor params on `get_session` (`before_index` / `after_index` / `count`) + `list_sessions` (`before_last_activity_at_ms` / `count`); always-populated `EntrySummary.index`; `total_count` on both results. Additive, back-compat. `solution_agent` tests 91 → 99. |
| R-6e client pagination | sibling `c7fbddc` | Paginated initial pull (`count=50`), `loadOlder` via `before_index=oldestLoaded` with LazyColumn auto-trigger, `resumeSession` on `Disconnected→Connected` via `after_index=lastSeenRepository.get()` with gap-detect safety net falling back to full `openSession`. `:core` tests 82 → 87. APK 2.22 → 2.24 MB. |
| R-6f WS compression | spk-editor `2b22c9557c` | **Cancelled (upstream gap)** — `tokio-tungstenite 0.28` and `0.29` parse `permessage-deflate` headers but don't implement the codec. Listener stays uncompressed. Sub-agent refused to fake-ship; committed docs-only deferral + finding (`docs/findings/2026-05-17-remote-control-r6f-upstream-gap.md`). R-6e's diff streaming + pagination remains the load-bearing bandwidth win. Re-open when upstream lands the extension or when we migrate the WS stack. |

## Open follow-ups

| Item | Track | Notes |
|---|---|---|
| **R-6c** FCM push + multi-server | HEAVY (sibling) | Push notifications when an agent finishes a turn (needs Firebase project setup by the maintainer); support multiple paired workstations from one phone app. Defer until single-server v1 has real-world use feedback. |
| **Crash reporting** | LIGHT-MEDIUM (sibling) | No Crashlytics / Sentry yet by design. Add a local-log fallback if shipping reveals visibility gaps. |
| **F** Sub-agent indication UI | HEAVY (spk-editor + sibling) | **Shipped 2026-05-17/18**: F-server `104881302c`, F-desktop `cd8a6aebb5`, F-phone (sibling) `1af444b`. Sub-agents modelled as first-class sessions with `parent_session_id`; new `get_session_children` MCP tool; `SessionSummary` carries `total_tokens` + `parent_session_id`; desktop bubble strip in session_view above status row (DFS, indent per nesting, click→open_session); phone AssistChip LazyRow above compose. Trade-off: Claude Code's internal Task tool dispatches still won't surface — parent agents must explicitly call `create_session({parent_session_id})` to be visible (filed as a potential future synthetic-from-tool_use phase). solution_agent tests 99 → 112, `:core` 87 → 91. |
| **G** `spk-image://` in queued message | LIGHT (spk-editor) | Audit 2026-05-16 showed `spk-image://` IS wired in 3 places. Bug from 2026-05-15 likely already fixed; needs user repro to confirm there's still a remaining failure mode. |
| **R-6f re-open** | HEAVY (deferred) | When `tokio-tungstenite` ships permessage-deflate (long-open upstream issue) OR when we migrate the WS stack to a compression-capable alternative (`fastwebsockets`, OkHttp + custom extension, etc.). |
| **`list_sessions` UI pagination** | LIGHT (sibling) | R-6e wired the wire-side `total_count` for list_sessions; UI infinite-scroll on solutions/sessions list deferred until needed. |

## Architectural decisions worth carrying forward

1. **Two-module Kotlin/Gradle (`:core` JVM + `:app` Android) + `:cli`** — `:core` builds + tests without Android SDK, reusable for non-Android consumers. Settled R-5a.
2. **R-5e additive server enrichment** — `EntrySummary` gained optional fields gated behind `include_full_content` + `include_images` so token-budget-sensitive autonomous-agent callers (the desktop Claude session driving the local MCP socket) keep getting the lightweight preview-only shape.
3. **Diff streaming via `entry_index` in notifications** — `agent_session_message_appended` carries `{ session_id, entry_index, role, preview }`. Client decides append vs update by index, fetches full content via `get_session_entry` only when needed. Bandwidth proportional to new content per turn instead of full history per token batch.
4. **`RemoteTransport` seam in `:core`** — `OkHttpRemoteTransport` for prod + `FakeRemoteTransport` for tests. Lets us drive virtual-time reconnect/queue scenarios without a real server.
5. **5-min outbound queue TTL** — long enough to survive elevator / metro signal drops, short enough that a stale "Send" from yesterday doesn't fire after the user pockets the phone for hours.
6. **`androidx.security:security-crypto:1.1.0-alpha06`** is the right choice despite the alpha tag — stable 1.0 still drags a deprecated Tink and breaks under AGP 8+.
7. **Locked rebrand identifiers** still apply on the sibling repo: package root `ru.sipaha.spkremote`, Apache 2.0 (changed 2026-05-18 from GPL-3.0-or-later — the sibling is a clean implementation and doesn't inherit upstream Zed's license obligations), `© 2026 Pavel Simonov`.

## Active gotchas

1. **`pgrep -f <pattern>` in watch-loops self-matches the running bash.** Use marker grep or `pgrep | grep -v $$`. (`supervisor-mode.md` anti-patterns.)
2. **rust-analyzer flycheck on a worktree** can surface phantom E0061 diagnostics pointing at external crates' definition sites. Run `cargo check --workspace --all-targets` to disambiguate — if cargo doesn't report it, ignore RA.
3. **MCP tool catalog count** on the server side: ~64 now (R-5e's `get_session_entry`, R-5g's `list_agents`, + earlier R-5e additions). Bump on each new namespace/tool.
4. **Worktree-staleness for sub-agent dispatches**: the worktree branches from session-start HEAD. Tell sub-agents to `git rebase origin/main` if their base looks stale; or paste plan-doc inline; or skip worktree isolation entirely for sibling-repo work (no worktree in the first place).
5. **zxing-android-embedded** needs an explicit `androidx.appcompat` dep declaration — its AAR registers `CaptureActivity` without bringing the AppCompat style classpath.
6. **Kotlin smart-cast across modules** doesn't fire on nullable `JsonRpcResponse.error` from `:core` read in `:app`. Lift to a local `val` before the null-check.
7. **Server-side substring-match for `no_active_workspace_for_solution`** — the R-5g auto-open retry detects this error via free-form message substring. When the server moves to JSON-RPC `code`-based errors, switch to code-matching.

## Stale handoff notice

`2026-05-15-session-handoff.md` is superseded by `2026-05-16-session-handoff.md`,
which itself is now superseded by this file. Treat the older two as historical
snapshots; this is authoritative for current state.

## Resume recipe for the next session

1. Read this file first.
2. Read `docs/INDEX.md`.
3. Read `docs/workflow/supervisor-mode.md`.
4. `git log --oneline -25` for the spk-editor side.
5. `cd ../spk-editor-mobile && git log --oneline -10` for the sibling.
6. The R-5/R-6 arc closes here. Pool of named items: R-6c (push + multi-server), outbound-queue disk persistence, crash reporting, F (sub-agent indication UI), G (`spk-image://` repro). User direction expected before picking up F/G; the others are LIGHT-MEDIUM polish work.

If the user gives a direction in their first message, that overrides the pool ordering.
