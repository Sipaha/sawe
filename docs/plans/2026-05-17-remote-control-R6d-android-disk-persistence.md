# R-6d: Disk persistence for Android — outbound queue, drafts, lastSeenEntryIndex, nav state, bounce-to-input

**Status:** complete (sibling-repo commit `ff9ca8c`)
**Repo:** `spk-editor-mobile` (sibling)
**Depends on:** R-6a (in-memory queue) + R-6b (EncryptedSharedPreferences + Settings screen).
**Goal:** Everything that a user-visible action produced must survive a force-kill of the app. The R-6a in-memory queue + R-6b's pairing-only persistence aren't enough — typed-but-unsent messages, queued sends, last-seen indices, and the active nav route should all live on disk.

## Why this phase exists

User feedback on 2026-05-17:
- "5-min outbound queue TTL is too short" — the realistic scenario is "I tap Send → enter the metro for 40 minutes → return". TTL should be ~24 hours.
- "Failed messages should pop back into the input field, even after a restart, so the user can edit + retry."
- "Audit everything that should persist — make nothing slip on restart."

## Scope

### A. Outbound queue → disk + 24h TTL

R-6a's `RemoteClient.queueCall` holds requests in memory. Move to a `QueuedMessage` table on disk (EncryptedSharedPreferences with a JSON list, or DataStore — sub-agent's choice; EncryptedSharedPreferences is the smaller dep step since it's already on the classpath from R-6b).

Schema for one entry:
```kotlin
@Serializable
data class QueuedMessage(
    val id: String,                  // UUID for dedup
    val method: String,              // currently always "remote.solution_agent.send_message"
    val params: JsonElement,         // session_id + content
    val enqueuedAtMs: Long,          // wall-clock time
    val attemptCount: Int = 0,
)
```

Store all queued messages in a single encrypted blob (`queued_messages_v1`). Rewriting the whole blob on every queue change is fine — typical queue size is 0-3 entries.

**TTL**: 24 hours from `enqueuedAtMs`. Configurable per call but defaults to `24 * 3600 * 1000`. On every reconnect (or on app cold start), drain expired entries from the queue → emit a `QueueExpired(message)` event to the consumer.

**Order**: FIFO by `enqueuedAtMs`.

On `Connected` transition: replay the queue head-to-tail, removing each entry after successful RPC response. If the RPC errors with a transient failure, increment `attemptCount` and leave at head; on a terminal failure (auth reject, malformed params) move to expired.

### B. Bounce-to-input on TTL expiry / terminal failure

Where the user-visible recovery happens. `MainViewModel` collects `QueueExpired` events:

1. If `method == "remote.solution_agent.send_message"`, extract `params.session_id` + `params.content`.
2. Look up the session: if it exists locally, persist the content as a pending draft for that session via `DraftRepository.save(sessionId, content)`. If `SessionDetailScreen` is open for that session, prefill the OutlinedTextField; otherwise the next time the user opens that session detail, the draft loads automatically + a snackbar "Couldn't send earlier — added back to your message for retry."
3. If session no longer exists locally (deleted on desktop in the meantime), fall back to a notification / toast with the message body so the user doesn't silently lose typed content.

### C. Compose-field drafts → disk per session

Type a message, app goes to background, come back later — should still be there.

`DraftRepository`:
```kotlin
class DraftRepository(context: Context) {
    fun save(sessionId: String, text: String)
    fun load(sessionId: String): String
    fun clear(sessionId: String)
    fun all(): Map<String, String>       // for debug / admin
}
```

In `SessionDetailScreen`:
- On entry: `var input by remember(sessionId) { mutableStateOf(draftRepository.load(sessionId)) }`.
- On `input` change, debounced 500ms: `draftRepository.save(sessionId, input)`.
- On successful `sendMessage(input)`: `draftRepository.clear(sessionId)` + clear the field.
- On bounce-back: `draftRepository` is the source of truth; the screen re-`load`s.

Storage: regular `SharedPreferences` is fine here (drafts aren't sensitive — same data the server already has the moment Send is tapped). Use the same `MasterKey` mechanism as the queue for consistency, but encryption isn't load-bearing. Sub-agent's choice.

### D. `lastSeenEntryIndex` per session → disk

R-6a's `MainViewModel.lastSeenEntryIndex` is in-memory only. After restart it's zeroed and the next reconnect refetches everything (waste of bandwidth + visible "skipping back to top" UX).

Add `LastSeenRepository`:
```kotlin
fun get(sessionId: String): Int?
fun set(sessionId: String, index: Int)
fun clear(sessionId: String)
```

Wire from MainViewModel:
- Every `agent_session_message_appended` → `lastSeenRepository.set(sessionId, entryIndex)`.
- On cold start with valid pairing → don't lose the marker; reconnect will use it (R-6e will wire the incremental resume path; for R-6d just store + read the marker, fall back to full `get_session` for now).

### E. Active nav route → disk

After R-6b's cold-start auto-resume, MainActivity goes to `solutions` route. If the user was in the middle of a chat session and force-killed → they want to come back to that session, not the solutions list.

Add `NavStateRepository`:
```kotlin
fun saveRoute(route: String)  // e.g. "solutions/abc/sessions/xyz"
fun loadRoute(): String?
fun clear()
```

In `AppNavGraph`:
- Listen to `navController.currentBackStackEntryFlow` → save the current route on every change.
- On cold start, after the pairing-valid check resolves: if `loadRoute()` returns a non-null route AND it's deeper than `pairing`/`solutions`, navigate there as the start destination. Validate the route still resolves (solution/session may have been deleted) — fall back gracefully to `solutions` on lookup failure.

### F. Audit pass on remaining `remember { mutableStateOf(...) }`

The sub-agent should grep through `app/src/main/kotlin/.../ui/` for `remember {` and per-state evaluate:
- Pure UI ephemera (expand/collapse, hover, dialog open) → stays.
- User-typed text that isn't already drafted (e.g. agent picker selection in NewSessionDialog if it would survive better in viewmodel) → judgement call; document choices in commit message.

The four primary persistence categories above are the load-bearing requirement; the audit is a sweep to catch what would also be lost on restart.

### G. Repositories — wire-up

Add to `:app/src/main/kotlin/ru/sipaha/spkremote/app/data/`:
- `QueuedMessageRepository.kt`
- `DraftRepository.kt`
- `LastSeenRepository.kt`
- `NavStateRepository.kt`

All take `Context` constructor arg. All wired into a single `AppContainer` singleton (or use Android-Compose's `LocalContext.current` directly — sub-agent's call; singleton is cleaner). Inject into `MainViewModel` via the existing `AndroidViewModel(application)` mechanism.

### H. Update `:core::RemoteClient`

`queueCall` currently lives in `:core` with an in-memory queue. Two ways to handle the disk move:

**Option 1 (simpler — recommended):** Keep the `:core` API but inject a `QueueStore` interface that `:core` reads/writes from. `:app` supplies a disk-backed impl (`QueuedMessageRepository`) via `RemoteClient.Builder.queueStore(...)`. `:core` tests keep using an in-memory impl. Pure-JVM `:cli` keeps in-memory.

**Option 2:** Move queue into `:app`, leaving `:core` agnostic. `RemoteClient` only exposes `call()`; `MainViewModel.sendMessage` calls into a `:app::SendQueueOrchestrator` that wraps `client.call()` with queueing.

Pick Option 1 — keeps the existing surface contract and tests.

### I. 24h TTL with cleanup at boot

`MainActivity.onCreate` (or in the `MainViewModel.init`):
1. Read all queued messages.
2. For each: if `now - enqueuedAtMs > ttlMs` (default 24h), drain → bounce to input/draft per (B) above.
3. Survivors remain queued for later replay.

### J. Out of scope

- Pagination of `get_session` for huge sessions — R-6e.
- WS compression — R-6f.
- Push notifications — R-6c.
- Multi-server support — R-6c.
- Cross-device sync of drafts (no — drafts are local-only).

## Acceptance

```bash
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor-mobile
ANDROID_HOME=$HOME/Android/Sdk JAVA_HOME=$HOME/.jdks/temurin-21.0.10 ./gradlew :core:test :app:assembleDebug :app:assembleRelease --rerun-tasks 2>&1 | tee /tmp/r6d.txt | tail -15
grep -E "BUILD SUCCESSFUL|FAILURE:" /tmp/r6d.txt
ls -la app/build/outputs/apk/release/*.apk
```

- [x] `:core:test` BUILD SUCCESSFUL — ~75-78 tests (R-6a baseline 72 + new `QueueStore` interface tests).
- [x] `:app:assembleDebug` + `:app:assembleRelease` BUILD SUCCESSFUL.
- [x] Release APK ≤ 3.0 MB (R-6b was 2.12 MB; the new repositories should add <500 KB).
- [x] No regressions on R-5b/R-5c/R-5d/R-5f/R-5g/R-6a/R-6b flows.
- [x] `QueueStore` interface in `:core` + disk-backed impl in `:app` (`QueuedMessageRepository`); FIFO order preserved across simulated restart in test.
- [x] `DraftRepository` round-trip works (save → load → match); cleared on successful Send.
- [x] `LastSeenRepository` persists across simulated process death in test.
- [x] `NavStateRepository` saves the current route on every nav change; cold start restores when route resolves.
- [x] Bounce-to-input: TTL-expired message goes to `DraftRepository(sessionId)` and the next time `SessionDetailScreen` opens for that sessionId, the OutlinedTextField shows the bounced text + snackbar fires once.
- [x] TTL changed from 5 min to **24 hours** in `RemoteClient.queueCall` default.

## Commit message

Subject: `app: disk persistence for queue / drafts / lastSeen / nav (R-6d)`

Body: outline the four repositories, the QueueStore interface in :core, the 24h TTL change, the bounce-to-input flow, and any audit-discovered state that wasn't load-bearing.

## Reporting back

≤400 words. Include:
- New sibling-repo commit SHA on top of `c517f03`.
- `:core` test count delta + new test names.
- New release APK size.
- The audit pass result: list any `remember { mutableStateOf(...) }` that you moved to ViewModel / repository, vs left as ephemeral.
- Whether you used Kotlinx Serialization JSON for the queue persistence or another format.
- Any encrypted-storage quirk (e.g. EncryptedSharedPreferences re-keying on first read after a system upgrade).

## Context safeguards

- Don't touch the spk-editor tree.
- One commit on top of `c517f03`. No push.
- Don't import material-icons-extended.
- Use the same EncryptedSharedPreferences `MasterKey` instance via a shared singleton — don't create one MasterKey per repository (instantiating one is the slow part).
