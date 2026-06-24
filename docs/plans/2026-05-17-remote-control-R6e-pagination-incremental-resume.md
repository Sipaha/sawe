# R-6e: Pagination on `get_session` + incremental resume via `lastSeenEntryIndex`

**Status:** complete (server commit `b756392b52`, client commit `c7fbddc`)
**Repos:** spk-editor (server) → `spk-editor-mobile` (client). Two sub-agent dispatches.
**Depends on:** R-5e (`get_session` shape) + R-6d (`lastSeenEntryIndex` persisted on the client side).
**Goal:** Stop sending entire session histories on every reconnect / screen entry. A 200-entry chat with embedded images can be 50+ MB on the wire; on a flaky LTE link, that's the difference between "instant" and "30-second waits + frequent timeouts". Add cursor-style pagination to `get_session` + wire the client to fetch only the delta on reconnect.

## Why this phase exists

R-5d/R-5f's session detail screen renders fine for short sessions but blows up on long ones:
- Every screen entry → full `get_session` (entire history fetched).
- Every reconnect → same.
- The client only ever shows entries in a single `LazyColumn`; there's no scroll-to-load-older affordance.

R-6d's `lastSeenEntryIndex` is now persisted, but unused for incremental fetch — the resume path still calls `get_session()` full.

## Scope

### A. Server: extend `solution_agent.get_session` params

```rust
pub struct GetSessionParams {
    pub session_id: String,
    pub include_full_content: bool,
    pub include_images: bool,
    /// NEW: return entries with index < before_index. None = include up
    /// to the latest entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_index: Option<usize>,
    /// NEW: return entries with index > after_index. None = no lower
    /// bound. Both before_index + after_index together select a slice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_index: Option<usize>,
    /// NEW: max number of entries to return. None = unbounded
    /// (backwards-compatible default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
}
```

Selection logic in the tool body:
1. Filter `acp_thread.entries()` by index, applying `after_index` (exclusive lower bound) and `before_index` (exclusive upper bound).
2. If `count` is `Some(n)`, take only the LAST n entries (so the default screen-entry query `count=50, before_index=None` returns the 50 newest, not the 50 oldest).
3. Map to `EntrySummary` as before.

Add `total_count: usize` to `GetSessionResult` so the client knows how many entries exist server-side:

```rust
pub struct GetSessionResult {
    pub id: String,
    // ... existing fields ...
    pub entries: Vec<EntrySummary>,
    /// NEW: total number of entries in the session, regardless of
    /// pagination filter. Lets the client decide whether to render
    /// a "Load older" affordance.
    pub total_count: usize,
}
```

Each entry in the response keeps its **absolute** index (so the client can build a sparse map). This means `EntrySummary` may need an `index: Option<usize>` field for paginated responses (default None when the response is non-paginated — backwards-compat). Or always set it. Choose "always set" since the field is `usize` (4-8 bytes) and disambiguates rendering.

```rust
pub struct EntrySummary {
    pub role: String,
    pub preview: String,
    /// NEW: absolute index in the session, regardless of pagination.
    pub index: usize,
    pub markdown: Option<String>,
    pub images: Option<Vec<EntryImage>>,
    pub tool_call: Option<ToolCallSummary>,
    pub plan: Option<PlanSummary>,
}
```

The `get_session_entry { index }` tool from R-5e already returns the indexed entry — preserve its behavior unchanged.

### B. Server: pagination on `list_sessions` (defensive)

Less critical (sessions per solution are typically ≤ 20) but worth doing in the same phase since the wire shape extension is mechanical:

```rust
pub struct ListSessionsParams {
    pub solution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
}
```

Order sessions by `last_activity_at` descending so `count=20` returns the 20 most-recent. Add `total_count` to result.

### C. Server: tests

Add to `mcp::tests`:
- `get_session` with `count=2, before_index=None` on a 5-entry session → returns the 2 newest, `total_count=5`.
- `get_session` with `before_index=3` → returns entries indices 0-2, `total_count=5`.
- `get_session` with `after_index=2` → returns entries indices 3-4.
- `get_session` with `after_index=2, count=1` → returns only index 3.
- `get_session` with no pagination params → backwards-compat full response.
- Each `EntrySummary.index` matches its actual position.

`list_sessions` pagination test (one happy path).

### D. Client: paginated initial load

In `MainViewModel.openSession(sessionId)`:
- Initial fetch: `get_session { session_id, include_full_content=true, include_images=true, count=50 }`. Server returns the 50 newest + `total_count`.
- Track loaded range: `var loadedRange: IntRange? = null` per session. Set to `(total_count - entries.size) until total_count` after the initial load.
- Track `total_count` so UI knows there's more.

### E. Client: "Load older" affordance in `SessionDetailScreen`

In the `LazyColumn`, at the top (i.e., when scrolling up past the oldest loaded entry):
- If `loadedRange.first > 0`, show a sticky-header-style "Load older" button (or auto-trigger when the LazyListState's `firstVisibleItemIndex` ≤ 2).
- Tap / auto-fire → `viewModel.loadOlder(sessionId, beforeIndex=loadedRange.first, count=50)`. Append to the start of the entries list, update `loadedRange`.
- Surface a small `CircularProgressIndicator` while loading.

### F. Client: incremental resume on reconnect

In `MainViewModel`, the existing reconnect collector ((R-6a)) currently re-fetches `get_session()` full on every `Disconnected → Connected` transition. Replace with:

```kotlin
private fun resumeSession(sessionId: String) {
    val lastSeen = lastSeenRepository.get(sessionId)
    if (lastSeen == null) {
        // First open or cleared marker → fall back to initial paginated load.
        openSession(sessionId)
        return
    }
    // Fetch only entries with index > lastSeen.
    viewModelScope.launch {
        val result = client.call(
            "remote.solution_agent.get_session",
            buildJsonObject {
                put("session_id", sessionId)
                put("include_full_content", true)
                put("include_images", true)
                put("after_index", lastSeen)
            },
        )
        // Append to the in-memory list. Update lastSeenRepository on each new entry.
    }
}
```

If `result.entries` is empty AND `total_count > currentEntries.size` (i.e. server thinks we missed something), fall back to a full re-fetch as a safety net.

### G. Client: tests

Add to `:core`:
- `GetSessionParams` round-trip with all new fields populated + None defaults.
- `EntrySummary` round-trip with `index` field.
- `total_count` in `GetSessionResult` round-trip.

Add to `:app`-side ViewModel-level: skip (no `:app` test infra; document for next phase).

### H. Out of scope

- Compression (R-6f).
- Cross-session-list pagination beyond the simple `list_sessions` extension.
- `:app` instrumented test framework.
- Migrating older clients (R-6e is additive — old clients sending no pagination params still get the full-session legacy path).

## Acceptance (server side)

```bash
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor
set -o pipefail
cargo build --bin spk-editor 2>&1 | tee /tmp/r6e_build.txt
grep -E "^error|could not compile" /tmp/r6e_build.txt
cargo clippy -p solution_agent --all-targets -- -D warnings 2>&1 | tee /tmp/r6e_clippy.txt
cargo test -p solution_agent --no-fail-fast 2>&1 | tee /tmp/r6e_test.txt
grep "test result:" /tmp/r6e_test.txt
cargo test -p remote_control proxy_e2e 2>&1 | grep "test result:"
```

- [x] `cargo build` passes.
- [x] `cargo clippy -p solution_agent -- -D warnings` clean.
- [x] `solution_agent` tests grow by 6-8 (current baseline 91).
- [x] `remote_control proxy_e2e` still passes.
- [x] `EntrySummary.index` is always populated; backwards-compat callers (no pagination params) see the full response in original order.
- [x] FORK.md `solution_agent` row mentions the pagination extension (one line).

## Acceptance (client side, after server lands)

```bash
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor-mobile
ANDROID_HOME=$HOME/Android/Sdk JAVA_HOME=$HOME/.jdks/temurin-21.0.10 ./gradlew :core:test :app:assembleRelease --rerun-tasks 2>&1 | tee /tmp/r6e_client.txt | tail -10
grep -E "BUILD SUCCESSFUL|FAILURE:" /tmp/r6e_client.txt
```

- [x] `:core:test` BUILD SUCCESSFUL — ~85-88 tests.
- [x] `:app:assembleRelease` BUILD SUCCESSFUL — APK ≤ 2.5 MB.
- [x] Initial fetch is paginated (count=50, no `before_index`).
- [x] Load-older affordance fires when scrolling near top of loaded entries.
- [x] Resume on reconnect uses `after_index=lastSeen` instead of full refetch.

## When done

Server sub-agent reports: commit SHA on spk-editor main, test count delta, any place where the existing `acp_thread` API forced a less-clean filter (e.g. if entries don't carry indices naturally).

Client sub-agent reports: sibling commit SHA, test count delta, new APK size, UX trade-offs (when to auto-load-older vs require a tap), any place where the server's wire shape didn't match the spec above.
