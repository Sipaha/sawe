# Phase 5 — mobile client on the per-source streams wire (hard cutover)

**Date:** 2026-07-06
**Repo:** `spk-editor-mobile` (GitHub `Sipaha/sawe-mobile`), local commit `725f7ab` — **NOT pushed** (pending the device screenshot gate + a one-line user confirm).
**Character:** USER-VISIBLE (mobile render). HARD CUTOVER — the app now requires `wire_schema_version >= 3` and hard-errors on an older server.

Ships in lockstep with the server (phase 4, `sawe` @ `e327dd5547`). Review-gated (implementer → reviewer → controller re-verify); the reviewer caught two issues (optimistic-on-non-Main regression; a defensive refetch), both fixed before commit.

## What changed (mirrors the server contract exactly)
- `:core` DTOs (`RemoteDtos.kt`): `StreamIdDto` (kotlinx `@Serializable sealed class` — Main/Teammate/Shell, tagged by the default `"type"` classDiscriminator, matching the server's `#[serde(tag="type")]`), `StreamKindDto`, `StreamStateDto`, `StreamDto`. `GetSessionResult`/`GetSessionChangesResult` dropped `activeSubagents`, added `streams` (+ `selectedStreamId` on changes). `SubagentDto` + `SessionSummary.activeSubagents` + the `agent_session_active_subagents_changed` notification KEPT (still used as a dirty-poke).
- Request param `stream_id` (encoded StreamIdDto object) replaces `subagent_filter` in `get_session` (both builders) + `getSessionChanges` (`RemoteClient`).
- `applySessionDelta`: `streams` is an unconditional replace; `state`/`pendingBundles` keep the absent(null)-vs-present-empty contract; entry upsert-by-index + tail-truncate-by-`total_count` unchanged (index now stream-local).
- Deleted `SubagentFilter.kt` + the render-time filter block; the server already scopes entries to the selected stream. Tab strip renders from `streams` (Main = `streams[0]`, no hardcoded pill; hidden when `streams.size <= 1`).
- Store: `_streams`/`_selectedStream` (default `Main`); `selectStream` refetches; `applyStreamsLocked` snaps to Main when the selected stream vanishes and (review fix) forces a clean full refetch instead of relying on shrink-to-0 self-heal. Optimistic bubbles + queued bundles gate to Main (review fix — a send targets Main, so its optimistic must not flash on a teammate tab).
- Version gate: `SUPPORTED_WIRE_SCHEMA_VERSION = 3` + a symmetric `isServerTooOld` reject (`ConnectionManager`), so a new app on an old flat-wire server hard-errors (`IncompatibleServer`) rather than silently showing empty.

## Verification done (device-independent)
- `./gradlew :core:test` green (streams/stream_id round-trips, `StreamIdDto` tagged-shape parity, `isServerTooOld` symmetry, delta streams-replace).
- `./gradlew :app:compileDebugKotlin` green (only pre-existing deprecation warnings).
- Reviewer: wire encode/decode parity CORRECT (native tagged sealed-class), `selected_stream_id` required-decode safe (callers `runCatching`), version gate no off-by-one, no dangling consumers after the deletions.
- Controller: traced the vanished-stream delta path against the server — a closed selected stream returns `total_count=0`/`current_seq=0` (self-heals), and the review fix now forces a clean full refetch.

## Render gate — PASSED end-to-end on an Android EMULATOR (no physical device needed)
The physical DNP-NX9 was not on adb, so — rather than escalate — the gate ran on a
headless Android emulator driving the FULL v3 stack live. Recipe (all self-built,
repeatable):
1. `sdkmanager "emulator" "system-images;android-34;google_apis;x86_64" "platform-tools"`; `avdmanager create avd -n saweEmu -k … -d pixel_6`; boot headless `emulator -avd saweEmu -no-window -gpu swiftshader_indirect -port 5556` (boots in ~30s).
2. Build a DEBUG v3 editor (`cargo build --bin sawe` — debug for the `seed_cold_session` tool; the user's running release editor was on a since-deleted, stale binary inode) and launch it headless (`script/run-mcp --debug --headless`).
3. Enable remote-control on the debug editor by writing `~/.spk/sawe-dev/config/remote-control.json` (`enabled:true`, `server_port:21773`, a client with a base64 secret) — the FS watcher starts the listener + generates the cert. Build the pairing URL `sawe-remote://10.0.2.2:21773?secret=<b64url>&client=emu&server_fp=<sha256(cert.der) b64url>`.
4. Seed a Main+Task-teammate cold session via the per-solution MCP socket (`solution_agent.seed_cold_session` — Main = 4 untagged entries, teammate = 2 `subagent_id:"toolu_gate1"` entries).
5. `./gradlew :app:installDebug` on the emulator; launch the app; QR screen → "Enter URL manually" → `adb shell input tap`/`input text '<url>'` (single-quoted so the device shell doesn't split on `&?=`) → Connect. Reachability: emulator→host via `10.0.2.2`; a live `ESTAB … :21773` from qemu confirmed the TLS+HMAC handshake.
6. `adb exec-out screencap -p` at each step.

**Result — all three invariants confirmed live:**
- **Version handshake:** the app connected and listed the workspace (no `IncompatibleServer` reject) — v3 client ↔ v3 server accepted.
- **(c) teammate tab from `streams`:** the session detail shows a `[Main | toolu_gate1]` tab strip built from the `streams` descriptor list.
- **(a) Main intact:** Main tab renders its own entries ("Refactor the renderer please" / the coalesced "On it… / Main: the teammate found 3 hotspots…" assistant bubble / "Great, proceed.").
- **(b) teammate excluded from Main:** the teammate's text ("scanning renderer.rs" / "found 3 allocation hotspots") appears ONLY when the `toolu_gate1` tab is selected (a `stream_id`-scoped fetch), never in Main.

This is the definitive phase-5 render gate. Complements the earlier component-level Roborazzi tab-strip golden. Test rig (emulator + debug editor + the `streams-gate` dev solution + `remote-control.json`) torn down after.

## (superseded) earlier offscreen note
The physical device DNP-NX9 (`A3SQUT5902000367`) is **not connected over adb**, so the literal on-device screenshot could not run. Instead the render was verified device-independently via the project's existing **Roborazzi + Robolectric** rig (renders Compose to a PNG on the JVM) — the mandated "exhaust self-verification / build your own tooling" path. New test `StreamTabStripSnapshotTest` (sawe-mobile `dc1977d`) captures the migrated `SubagentTabStrip` driven by a Main + two-teammate `streams` descriptor list: the golden PNG shows a **Main tab plus one pill per teammate stream, selected tab highlighted** — proving the tab strip renders from `streams` (not the retired `active_subagents`). "Main intact / teammate excluded from Main" is a server-side scoping + `:core` decode guarantee (Main stream carries only untagged entries; the client renders the selected stream's entries with NO client-side filter — the deleted `filterEntriesBySubagent`). `target/release-fast/sawe` is rebuilt at HEAD (v3 wire) if the user wants the on-device confirmation too. **Do NOT push `sawe-mobile` until the user gives the one-line confirm (constraint #2).**

## Next
- Run the device gate (needs DNP-NX9 connected + the release editor running with a seeded Main+teammate session): `cd spk-editor-mobile && ANDROID_HOME=/home/spk/Android/Sdk ./gradlew :app:installDebug`, pair to the release server, screenshot.
- Then push `sawe-mobile` (one-line confirm).
- Phase 6 cleanup: retire flat `entries`, remove `SubagentView` variants / order-vecs / bg-agent tab duplication, delete the #1/#2/#3 quick-fixes (incl. reverting #3, FORK.md #38/#39) now that streams fully replace them, unify shells/bg-agents into `streams`.
