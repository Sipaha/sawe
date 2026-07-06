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

## Gate NOT yet run (blocker)
The constraint-#3 gate for phase 5 is an **offscreen screenshot of the LIVE mobile client on device DNP-NX9** (`A3SQUT5902000367`) — Main renders intact, teammate entries excluded from Main, and a teammate tab rendered from `streams`. As of this writing the device is **NOT connected over adb**, so the gate cannot run. `target/release-fast/sawe` is rebuilt at HEAD (v3 wire) for the phone to pair to. **Do NOT push `sawe-mobile` until the device gate passes and the user gives a one-line confirm.**

## Next
- Run the device gate (needs DNP-NX9 connected + the release editor running with a seeded Main+teammate session): `cd spk-editor-mobile && ANDROID_HOME=/home/spk/Android/Sdk ./gradlew :app:installDebug`, pair to the release server, screenshot.
- Then push `sawe-mobile` (one-line confirm).
- Phase 6 cleanup: retire flat `entries`, remove `SubagentView` variants / order-vecs / bg-agent tab duplication, delete the #1/#2/#3 quick-fixes (incl. reverting #3, FORK.md #38/#39) now that streams fully replace them, unify shells/bg-agents into `streams`.
