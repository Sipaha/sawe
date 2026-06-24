# R-5c: Solutions + agent-sessions list UI

**Status:** complete (sibling-repo commit `7fa4615`)
**Repo:** `spk-editor-mobile/`
**Depends on:** R-5a (`:core` `RemoteClient`), R-5b (QR pairing reaches a connected state).
**Goal:** From the post-pairing connected state, drill into solutions → drill into one solution → see its agent sessions → drill into one. Pure read paths; chat send/receive lives in R-5d.

## Why this phase exists

R-5b leaves the Connected state as a placeholder "we are paired" screen. The user's actual ask is "see my open solutions and watch agent progress from the phone". This phase paints the navigation tree from solution to session, on the way to R-5d's chat surface.

## Scope

### Navigation graph

Use **AndroidX Navigation Compose** (`androidx.navigation:navigation-compose:2.8.x`).

Routes:

- `pairing` (entry — handled by R-5b's `QrPairingScreen`)
- `solutions` (post-pairing landing)
- `solutions/{solutionId}` (solution detail — shows sessions list)
- `solutions/{solutionId}/sessions/{sessionId}` (session detail — R-5d wires the chat surface here; in R-5c it's a stub "Session X — chat coming soon")

After successful `RemoteClient.connect()`, navigate to `solutions`.

### Screens

**`SolutionsListScreen`**
- Calls `client.call("remote.solutions.list")` on entry; deserialises into `List<SolutionSummary>`.
- Pull-to-refresh (`PullToRefreshBox`).
- Lazy column: each row shows solution name + member count + status indicator (idle/agent-running). Tapping a row navigates to `solutions/{id}`.
- Empty state: "No solutions open in SPK Editor. Open one on your computer to see it here."
- Error state: snackbar "Couldn't load solutions: {message}", with retry button.

**`SolutionDetailScreen`**
- Calls `remote.solutions.get` for the solution name + members, `remote.solution_agent.list_sessions` for sessions, both in parallel.
- Header: solution name + project members count.
- Lazy column of sessions: title, "running" / "idle" / "awaiting input" / "errored" status pill, last-modified-time relative ("2m ago"). Tap → session detail.
- FAB: "New session" → R-5d will wire `remote.solution_agent.create_session`. In R-5c this is a stub button that snackbars "Coming in R-5d".

**`SessionDetailScreen` (stub for R-5c, real surface in R-5d)**
- Top bar with session title + a back arrow.
- Body: `Text("Chat UI coming in R-5d.")`.
- R-5d replaces this entire screen.

### Live-update wiring

Subscribe to `remote.editor.subscribe { kinds: ["agent_session_state_changed"] }` on entering `SolutionDetailScreen`. Update session status pills as notifications arrive. Unsubscribe on screen exit.

The notification flow:
1. Compose effect spins up a flow collector on `client.notifications`.
2. Filter by event kind; update the in-memory session state.
3. Compose recomposes the relevant rows.

### Data layer

Reuse `:core`'s `RemoteClient` directly from `MainViewModel`. No repository layer yet; the API surface is thin enough that adding repositories would be premature abstraction.

Define small DTOs in `:core` (since they round-trip JSON-RPC bodies and might be reused by `:cli`):

```kotlin
@Serializable data class SolutionSummary(val id: String, val name: String, val memberCount: Int, val status: String /* idle | running | etc */)
@Serializable data class SessionSummary(val id: String, val title: String, val state: String, val lastActiveAt: String /* RFC3339 */)
```

Map from the actual `remote.solutions.list` / `remote.solution_agent.list_sessions` response shapes (mirror the server-side schema exactly; refer to `crates/editor_mcp/tests/` for sample frames if shape is unclear).

## Out of scope

- Creating / renaming / deleting solutions (read-only on this phase).
- Multiple paired servers (R-6).
- Offline cache. Always re-fetch on screen enter.
- Search / filter UI on the lists.

## Architectural decisions

1. **Navigation Compose, not Voyager or Decompose.** Standard library, well-documented, type-safe-ish argument passing via Bundle.
2. **No Room / no Repository pattern yet.** All state lives in the ViewModel; `:core` is the data source. Add caching only if a real pain point shows up.
3. **DTO definitions live in `:core`**, not `:app`. Anything that round-trips the wire belongs alongside the client. `:app` only adds UI bindings.
4. **Subscriptions are screen-scoped**, not app-scoped. Avoids accumulating subscriptions on backgrounded screens.

## Verification

```bash
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor-mobile
JAVA_HOME=$HOME/.jdks/temurin-21.0.10 ./gradlew :app:assembleDebug :core:test --rerun-tasks 2>&1 | tee /tmp/r5c.txt
grep -E "BUILD SUCCESSFUL|FAILURE:" /tmp/r5c.txt
```

Manual smoke against a live spk-editor with at least one solution open + one running agent session:
- Pair, land on solutions list, see the solution.
- Tap → see sessions list with at least one row.
- Trigger an `agent_session_state_changed` event on the server side (e.g. start an agent turn) → row's pill updates without manual refresh.

## Acceptance

- [x] `:core:test` and `:app:assembleDebug` both BUILD SUCCESSFUL.
- [x] `:core` gains DTOs + their round-trip tests (one test per DTO, asserts JSON shape matches a recorded server response sample).
- [x] Manual smoke: pair → solutions list populates → drill into one → sessions list populates.
- [x] Live-update: starting an agent turn on the server side flips the pill from idle → running on the phone without a manual refresh.
- [x] Back navigation from sessions → solutions → pairing screen works (no crash).

## When done

Sub-agent reports commit SHA, Navigation Compose version chosen, sample JSON frames the DTOs were validated against, and any place where the server-side schema was ambiguous (so the supervisor can clarify on the spk-editor side for R-5d).

---

## Post-merge log (2026-05-16)

**Sibling-repo commit:** `7fa4615 app: navigate solutions → sessions; live-subscribe state changes (R-5c)` on top of `6e444e5`.

**Verified by supervisor:**
- `:core:test --rerun-tasks` → 41 tests, 0 failed (R-5a 30 + new `RemoteDtosTest` 11).
- `:app:assembleDebug --rerun-tasks` → BUILD SUCCESSFUL, APK 11.18 MB (+0.28 MB vs R-5b's 10.9 MB — Navigation Compose 2.8.4 transitive footprint).

**DTO + classifier (in `:core`):**
- `SolutionSummary`, `ListSolutionsResult`, `SessionSummary`, `ListSessionsResult` — all `@Serializable`, using `@SerialName` for snake_case fields per the server side. Optional fields (`last_opened_at`, `main_window_id`) marked nullable with `= null` default so missing keys deserialise cleanly.
- `DisplayState { Idle, Running, AwaitingInput, Errored, Unknown }` + `parseDisplayState(raw)` — `startsWith`-based classifier to handle the gnarly Rust Debug `state` strings (e.g. `"Running { started_at: Instant { tv_sec: 0, tv_nsec: 0 }, notified: false }"`).
- 11 new round-trip tests with locked fixtures including the long `Running { ... }` debug string.

**Deviations sub-agent took:**
- Skipped `PullToRefreshBox` (Material 3 1.3 API is fiddly) — replaced with a `TopAppBar` Refresh `IconButton`. Defer to R-5d or polish.
- No "any session Running" dot indicator on `SolutionRow` — deferred as R-5c-followup.
- Used `JsonArray(listOf(JsonPrimitive(...)))` instead of the DSL `buildJsonArray { add(...) }` because the latter required an `@OptIn` import dance.

**Gotcha — Kotlin smart-cast across modules:**

`JsonRpcResponse.error` is declared in `:core`, read from `:app` via `MainViewModel`. Kotlin's smart-cast doesn't fire across module boundaries (the read site can't prove the property's getter is stable), so this fails:

```kotlin
if (resp.error != null) error(resp.error.message ?: "rpc error")
//                            ^^^ "Smart cast to 'JsonRpcError' is impossible..."
```

Fix: lift to a local val before the null check:

```kotlin
val err = resp.error
if (err != null) error(err.message ?: "rpc error")
```

Bit the sub-agent twice in `MainViewModel`. Same pattern will bite any future consumer wrapping the `:core` JSON-RPC types in `:app` — record as a gotcha for R-5d.

**Other quirks:**
- Stale incremental APK reads as 14.6 MB; only the `--rerun-tasks` clean reports the real 11.18 MB. Always re-run from scratch when validating size.
