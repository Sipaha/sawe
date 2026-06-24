# Mobile: per-session chat-history disk cache with diff-only fetch on open

**Status:** complete
**Estimated:** 1 sub-agent session, ~75–120 min (HEAVY)
**Goal:** Opening a chat dialog on mobile no longer re-fetches the full transcript every time. The phone keeps a per-session encrypted-on-disk cache of `EntrySummary` entries up to `lastSeenIndex`; on open it pulls cached entries instantly + asks the server only for the diff via `after_index=cache.lastIndex`.

## Context

Today `SessionDetailStore.openSession(sessionId)` always calls `solution_agent.get_session(session_id, count=50)` and pulls the most-recent 50 entries from scratch every time the dialog is opened. On a long-running session this is wasted bandwidth + latency. The R-6e pagination is already wired (`before_index` / `after_index` / `count` on the server, paginated initial pull + `loadOlder` on the phone), but no persistent cache: every fresh open starts from zero.

Existing patterns in `:app/src/.../data/`:
- `EncryptedQueueStore.kt` — JSON-blob-in-EncryptedSharedPreferences per server.
- `DraftRepository.kt` — debounced 500ms per-session writes.
- `LastSeenRepository.kt` — per-session `lastSeenEntryIndex` cursor (R-6e). **Already exists** and is exactly the "where am I in the transcript" pointer the cache layer needs to coordinate with.
- `ListCacheRepository.kt` — `List<SessionSummary>` + `List<SolutionSummary>` per server, JSON blob in EncryptedSharedPreferences.

User explicit asks (this session):
1. Load only the diff on open.
2. Invalidate cache on context reset (`restart_agent` rotates the session_id; old transcript no longer reachable).
3. Invalidate cache on session delete (task #3 — when the user closes a session, drop its cache).

User did NOT explicitly mention `start_compact`, but compact also rotates the session_id (new continuation session). Treat compact's old session_id same as reset's: evict from cache. The OLD session_id remains in the DB as a closed transcript — currently not visible in the mobile sessions list, but if the maintainer later exposes "archived sessions" the cache can be rebuilt on demand.

## Scope

### A. New repository: `SessionHistoryRepository`
**File:** `app/src/main/kotlin/ru/sipaha/spkremote/app/data/SessionHistoryRepository.kt`

Storage: encrypted JSON blob per server, keyed by session_id. Mirror `ListCacheRepository`'s shape — one `EncryptedSharedPreferences` instance per server context, key = session_id, value = serialised `CachedSessionHistory` DTO.

```kotlin
@Serializable
data class CachedSessionHistory(
    val sessionId: String,
    val solutionId: String,
    val agentId: String,
    val entries: List<EntrySummary>,
    /** Highest entry_index present in [entries]; null when [entries] is empty. */
    val lastIndex: Long?,
    /** Server-reported total at the time of last write; used to detect gaps. */
    val totalCountAtLastWrite: Long,
    /** Persistence schema version; bump on incompatible changes. */
    val schemaVersion: Int = 1,
)
```

API (rough):
- `load(sessionId: String): CachedSessionHistory?`
- `save(history: CachedSessionHistory)` — debounced 500ms like DraftRepository
- `appendEntries(sessionId: String, newEntries: List<EntrySummary>, newTotalCount: Long)` — merge + save
- `evict(sessionId: String)` — single-session purge
- `evictAll()` — server switch / forget-server hook
- `prune(keepSessionIds: Set<String>)` — GC sweep on next `list_sessions` refresh

**Image-bytes caveat:** `EntrySummary` may carry base64-inlined images. The existing `get_session_entry(entry_index, include_images=true)` lazy-load pattern is already wired (R-5e); the cache should store **only** entries fetched with `include_images=false` and never inline images on disk. The mobile UI already handles "preview-only entry, on-demand image fetch" — no change there.

### B. Intercept `SessionDetailStore.openSession`
**File:** `app/src/main/kotlin/ru/sipaha/spkremote/app/vm/SessionDetailStore.kt`

Current pattern (paraphrasing — read the file to confirm):
```kotlin
fun openSession(sessionId: String) {
    scope.launch {
        val result = client.call("remote.solution_agent.get_session", ...)
        // → updates _session.value with the fetched entries
    }
}
```

New pattern:
```kotlin
fun openSession(sessionId: String) {
    scope.launch {
        val cached = historyRepo.load(sessionId)
        if (cached != null) {
            // Show cached entries instantly so the user sees their last
            // transcript with zero round-trip latency.
            _session.value = SessionDetailUi(cached.entries, …)
        }
        // Then fetch the diff: anything after cached.lastIndex.
        val afterIndex = cached?.lastIndex
        val result = client.call("remote.solution_agent.get_session", buildJsonObject {
            put("session_id", sessionId)
            afterIndex?.let { put("after_index", it) }
            // Don't pass `count` — server returns up to the configured default.
        })
        // Merge result entries into the existing state; if the server's
        // total_count differs from cached.totalCountAtLastWrite by more
        // than (result.entries.size), there's a gap (the cache is older
        // than R-6e's gap-detect window) — fall back to full openSession
        // with no after_index, replacing the cache wholesale.
        historyRepo.appendEntries(sessionId, result.entries, result.totalCount)
    }
}
```

Live-update path: when `agent_session_message_appended` arrives, the existing notification handler already calls `get_session_entry(entry_index)` to fetch the full entry. After that call lands the entry into `_session`, ALSO append it into the cache via `historyRepo.appendEntries(sessionId, listOf(entry), newTotalCount=entry.index+1)`.

### C. Eviction hooks

Hook into each of the four mutation sites:

1. **`close_session` (task #3 — mobile delete-session UI):** add `historyRepo.evict(sessionId)` right after the server `remote.solution_agent.close_session` RPC succeeds, in `SessionListStore.closeSession(sessionId)`.

2. **`restart_agent` (task #5 — Reset context):** add `historyRepo.evict(oldSessionId)` after the RPC returns the NEW session_id, in whatever VM action wires the "Reset context" overflow item. Task #5 is on the same Phase B as compact, so the agent for Phase B will own this hook too.

3. **`start_compact` (task #5 — Compact context):** same shape — `historyRepo.evict(oldSessionId)` after server confirms `queued=true` and a new session_id is created. Note: the server-side `start_compact` orchestration enqueues a message in the OLD session that triggers the agent to call `compact_session` (which mints the new session_id internally). The phone may need to listen for the resulting `agent_session_created` event with `parent_session_id == oldSessionId` to know when to evict — wire defensively, evict-then-refresh is cheap.

4. **GC sweep on `list_sessions` refresh:** in `SessionListStore.refreshSessions(solutionId)`, after a successful refresh, call `historyRepo.prune(keepSessionIds = result.sessions.map { it.id }.toSet())` so cache entries for sessions deleted on another client (or via the desktop) get cleaned up automatically.

### D. Server-switch + forget-server hook

`ConnectionManager` already has `forgetServer(...)` / `switchToServer(...)` paths that wipe per-server queue/draft/last-seen/nav-state blobs. Add `historyRepo.evictAll()` (or per-server `historyRepo.clearForServer(serverId)`) into the same teardown flow so a server reset doesn't leave stale history cached for the previous pairing.

### E. Tests

- Unit tests in `:core` for the merge/gap-detect logic (extract the merge fn into `:core/src/main/kotlin/.../SessionHistoryMerge.kt` as a pure function — mirrors the pattern of `SessionEntryMerge.kt` the audit just landed). Tests cover: empty cache + first fetch; non-empty cache + diff-only fetch; gap detected → fall-back full fetch; live-append from notification.
- `:app` integration test for `SessionHistoryRepository` itself: round-trip save/load, evict, prune. Existing in-process EncryptedSharedPreferences pattern from `ListCacheRepository` tests can be copied if such tests exist.

## Out of scope

- Caching image bytes on disk. Lazy-fetch via `get_session_entry(include_images=true)` is fine; cache is text-only for now. Filing as follow-up if user pain warrants.
- Caching `total_tokens` / `max_tokens` per-session for cold start of the meter (task #6). The meter falls back to server values on open; one extra RPC at open-time is acceptable.
- Cache size cap / LRU eviction. R-6d's queue store doesn't have one either; if a user accumulates 200 sessions it might warrant a cap, but punt for now.
- Cross-session attachment dedup (multiple tool calls referencing the same image hash). Marginal win, complex code.

## Architectural decisions

1. **Encrypted SharedPreferences over SQLite/Room.** Mirrors the existing in-repo pattern (`ListCacheRepository`, `EncryptedQueueStore`). Room would force a schema-migration story and a new heavy dep. EncryptedSharedPreferences struggles with multi-MB blobs; if transcripts cross ~1 MB we may need to migrate, but that's a known re-route with a clear trigger. **Reason:** consistency with existing repos + zero new deps. **How to apply:** if a future profile shows >5s writes on big sessions, evaluate SQLite/Room.
2. **Cache key = `(server_id, session_id)`.** Server_id is the user's `PairedServer` identity (alpha-server, beta-server, …). On `switchToServer` the cache loads from the new server's namespace; on `forgetServer` the whole namespace dies. **Reason:** R-6c multi-server is supported; cache must respect it. **How to apply:** SharedPreferences file name is `history-cache-v1-${serverId}.xml` (matches existing `queued-messages-v1-${serverId}` pattern).
3. **Pure-function merge in `:core`, repository wiring in `:app`.** The merge logic (existing entries + new entries + gap detection) is testable JVM logic; the encryption + Android lifecycle is `:app`. **Reason:** mirrors `SessionEntryMerge.kt` which the audit pass extracted for the same reason. **How to apply:** new fn `SessionHistoryMerge.merge(cached: CachedSessionHistory?, fetched: GetSessionResult, lastIndexHint: Long?): MergeOutcome` with `MergeOutcome` = `Appended(entries, newTotalCount)` | `GapDetected(reason)` | `FullReplace(entries, newTotalCount)`.
4. **Evict-then-refresh on session-id rotation, not move-to-archive.** Compact/restart-agent both create a NEW session_id; the OLD id is closed but its blob is preserved server-side. User-facing: "Reset" means "forget this conversation" semantically. **Reason:** matches what the user said explicitly ("при сбросе контекста и кэш надо чистить"). **How to apply:** evict the OLD id from cache; the NEW id starts with no cache and gets populated on first open.

## Risks

- **`agent_session_created` event for the compact continuation may arrive before the `start_compact` RPC response** (depends on event ordering). If the eviction hook relies on the RPC response carrying the new session_id, but the event lands first, the cache might briefly serve stale entries for the old id between event and RPC return. Mitigation: evict on EITHER signal (idempotent) — first-to-fire wins.
- **Disk write debounce + force-kill race.** DraftRepository handles this by .commit() (synchronous) on visibility-loss; the cache writer should mirror — async writes for steady-state, synchronous on `ON_STOP`/`ON_DESTROY`.
- **Cache + server divergence** (server-side prune of old sessions, or desktop deleted a session while phone was offline): the GC sweep on next `list_sessions` refresh covers it. Until that fires, opening a deleted session shows the cached transcript then errors on the diff fetch — same UX as today for deleted sessions, just with an extra render of the cached entries first. Acceptable.
- **Image-bytes leak via base64 in cached preview text.** EntrySummary preview can include short base64 thumbnails for images. If those slip in, the cache size grows fast. **Mitigation:** at write time, scan the entries for content fields and strip any base64 chunks larger than e.g. 4 KB; rely on lazy-fetch for those. Add a unit test.

## Verification

```bash
cd /home/spk/.spk/sawe/solutions/spk-solutions/sawe-mobile
./gradlew :core:test  # SessionHistoryMerge tests
./gradlew :app:compileDebugKotlin
./gradlew :app:testDebugUnitTest  # if any :app unit tests exist
./gradlew :app:assembleDebug  # full R8 run
```

Manual smoke:
- Open session A → first open shows spinner briefly then full transcript. Close session.
- Open session A again → transcript appears INSTANTLY from cache. Server is then queried for the diff (in DevTools: a single `get_session` RPC with `after_index=<lastSeenIndex>`). 
- Send a message in session A → cache grows on the live `agent_session_message_appended` path.
- Trigger "Reset context" on session A → cache for A is evicted; opening the new (post-reset) session shows empty transcript + the post-reset welcome message only.
- Delete session B from the sessions list → next open of any session shouldn't show B in any list; opening B explicitly via cached nav would 404 cleanly.

## When done

- [x] `SessionHistoryRepository` class + `CachedSessionHistory` DTO in `:app`.
- [x] `SessionHistoryMerge` pure-fn in `:core` with unit tests (6 new tests).
- [x] `SessionDetailStore.openSession` intercepts the cache + uses `after_index` for the diff fetch.
- [x] Live notification handler persists new entries via `fetchAndReplaceEntry` (re-saves whole snapshot rather than appendEntries — notifications can be replacements not appends; 500ms repo debounce coalesces back-to-back writes).
- [x] Eviction hooks: closeSession (via SessionListStore after RPC success) / restart_agent (after RPC returns new id) / start_compact (two-phase: pendingCompactSourceIds set + onChildSessionCreated router consumes) / GC sweep on refreshSessions / removeServer evictAll.
- [x] Base64-blob strip on write — chunks > 4KB stripped via `stripImages` companion fn; mobile already lazy-fetches via get_session_entry(include_images=true).
- [x] `:core:test` 179 → 185 (+6 merge tests). `:app:compileDebugKotlin` clean. `:app:assembleDebug` SUCCESSFUL.

## Implementation notes worth carrying forward

- **Per-server prefixed keys, not per-server file**. Plan said `history-cache-v1-${serverId}.xml`, but the audit-shaped codebase uses one prefs file with per-server prefixed keys (`spk_history_cache` + `history-v1:<serverId>:<sessionId>`). Mirroring the actual `EncryptedQueueStore`/`LastSeenRepository`/`DraftRepository`/`ListCacheRepository` precedent was the right call.
- **`switchToServer` doesn't wipe.** Plan's claim that ConnectionManager wipes per-server blobs on `switchToServer` was wrong — those blobs survive switches BECAUSE they're per-server-keyed. History cache follows the same: survives switches, dies only on `removeServer` / explicit forget.
- **Int not Long for index types.** EntrySummary.index is Int (sentinel -1), GetSessionResult.totalCount is Int (sentinel -1). Used Int/Int? throughout for type-consistency with wire DTOs.
- **`appendEntries` is a no-op when no prior cache exists.** Live-update splices to an uncached session don't seed a half-cache; partial transcript without the head defeats gap-detection on the next open. First full fetch populates the cache properly.
- **`:app` test seam absent**. `:app` test target is pure JVM (no Robolectric), so SessionHistoryRepository (needs Android Context + EncryptedSharedPreferences) is uncovered. The pure logic lives in `:core/SessionHistoryMerge` and is fully covered by the 6 merge tests.
- **`onChildSessionCreated` is the right hook for compact's two-phase eviction.** Existing router decodes SessionCreatedPayload.parentSessionId; just consult the pending set in addition to the openSid check.

## Final commit SHAs
- `969b804` feat: SessionHistoryRepository + mergeSessionHistory scaffolding
- `f7dbefe` feat: wire SessionHistoryRepository into chat-open + eviction sites
