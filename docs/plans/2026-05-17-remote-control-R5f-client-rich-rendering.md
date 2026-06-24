# R-5f: Android client consumes R-5e enriched fields + diff streaming

**Status:** complete (sibling-repo commit `ee804aa`)
**Repo:** `sawe-mobile` (sibling of sawe)
**Depends on:** R-5e (server enrichment shipped on `main` as `d8592b05dc`).
**Goal:** Replace truncated-preview rendering with rich markdown + inline images + structured tool-call rows. Switch the chat surface from "full `get_session` re-fetch on every notification" to diff streaming via `get_session_entry { index }` keyed by the new `entry_index` in `agent_session_message_appended` payloads.

## Scope

### A. `:core` DTO additions

Mirror R-5e's `EntrySummary` extension:

```kotlin
@Serializable
data class EntrySummary(
    val role: String,
    val preview: String,
    val markdown: String? = null,
    val images: List<EntryImage>? = null,
    @SerialName("tool_call") val toolCall: ToolCallSummary? = null,
    val plan: PlanSummary? = null,
)

@Serializable
data class EntryImage(
    val index: Int,
    @SerialName("mime_type") val mimeType: String,
    @SerialName("data_base64") val dataBase64: String,
)

@Serializable
data class ToolCallSummary(
    val name: String,
    val status: String,
    @SerialName("args_preview") val argsPreview: String,
    @SerialName("result_preview") val resultPreview: String = "",
)

@Serializable
data class PlanSummary(val items: List<String>)

@Serializable
data class GetSessionEntryResult(val entry: EntrySummary)
```

Add `RemoteDtosTest` round-trips for each new shape with locked fixtures matching what the server emits (sample `markdown` with `spk-image://0` reference, sample tool_call with all four status strings, sample plan with 3 items).

### B. `:core::RemoteClient` — add `getSessionEntry` shorthand

Optional helper that wraps `client.call("remote.solution_agent.get_session_entry", buildJsonObject { put("session_id", ...); put("index", ...); put("include_images", true) })` + deserialises into `GetSessionEntryResult`. Keeps `MainViewModel` clean.

### C. Notification payload — extend deserialiser

`RemoteClient.notifications` currently surfaces `JsonElement`. R-5e's enriched `agent_session_message_appended` payload is now `{ session_id, entry_index, role, preview }`. Add a typed shape:

```kotlin
@Serializable
data class MessageAppendedPayload(
    @SerialName("session_id") val sessionId: String,
    @SerialName("entry_index") val entryIndex: Int,
    val role: String,
    val preview: String,
)
```

Parse on the consumer side (`MainViewModel`) — don't bake into `:core` if it complicates the SharedFlow typing.

### D. `MainViewModel.openSession` — pass enrichment flags

```kotlin
// in the params builder for get_session:
buildJsonObject {
    put("session_id", sessionId)
    put("include_full_content", true)
    put("include_images", true)
}
```

### E. Diff streaming — replace full-session refetch

Current flow (R-5d): on every `agent_session_message_appended` → `get_session` (full). Wasteful.

New flow:
1. On notification, parse `entry_index` + `role` + `preview`.
2. If `entry_index == current_entries.size` → append a placeholder with the preview (cheap), then trigger an async `get_session_entry(entry_index, include_images=true)` to populate full content. Replace the placeholder when the result arrives.
3. If `entry_index < current_entries.size` → that's an UPDATE to an existing entry (e.g. the assistant's last bubble is streaming). Trigger `get_session_entry(entry_index, include_images=true)` and replace at that index.
4. Periodic / explicit refresh (e.g. on screen entry, on pull-to-refresh) still uses full `get_session`.

This keeps the bandwidth proportional to "new content per turn", not "full session history per token batch".

### F. `SessionDetailScreen` — rich rendering

Currently the chat bubbles render `entry.preview` as plain `Text`. Replace per role:

- **user** + **assistant**: if `entry.markdown` non-null, render via a Compose markdown library. Pick **`com.mikepenz:multiplatform-markdown-renderer-m3:0.27.0`** (Material 3-aware, supports custom image renderer, ~250 KB). If `markdown` null (legacy), fall back to `Text(preview)`. Inline images: intercept `spk-image://N` URLs via the markdown lib's `imageTransformer` hook → look up `entry.images[N]` → render via Coil with a `ByteArray` source (base64-decoded). Tap → full-screen viewer via `Dialog`.

- **tool_call**: render a `Surface` with the tool-call status pill (color-coded) + `name` + collapsed `args_preview`. Tap to expand → show `args_preview` + `result_preview` in a `Surface` with monospace font. Use `AnimatedVisibility` for expand/collapse.

- **plan**: render the `plan.items` as a `Column` of `Text` with a leading dot/numbered prefix. Material 3 `ListItem`s if cleaner.

If `entry.markdown` is null AND it's a `user` / `assistant` role (legacy server or older session), fall back to truncated `preview`.

### G. Out of scope

- WebSocket reconnect / outbound queue — R-6a.
- New-session creation FAB — R-5g.
- Editing / deleting prior messages.
- Pairing persistence across app restart — R-6b.
- Settings screen — R-6b.

## Acceptance

```bash
cd /home/spk/.spk/sawe/solutions/spk-solutions/sawe-mobile
ANDROID_HOME=$HOME/Android/Sdk JAVA_HOME=$HOME/.jdks/temurin-21.0.10 ./gradlew :core:test :app:assembleDebug --rerun-tasks 2>&1 | tail -10
```

- [x] `:core:test` BUILD SUCCESSFUL with ~50-55 tests (R-5d baseline 45 + new EntryImage / ToolCallSummary / PlanSummary round-trips + enriched EntrySummary round-trip).
- [x] `:app:assembleDebug` BUILD SUCCESSFUL, APK ≤ 12.5 MB (R-5d baseline 11.2 + ~0.5 MB markdown lib + ~0.5 MB Coil).
- [x] No regressions on R-5b QR scanner / R-5c solutions/sessions list flows.
- [x] Rich-mode renders markdown via `multiplatform-markdown-renderer-m3` with `spk-image://` URLs routed to inline image rendering.
- [x] Diff-streaming wired: `get_session_entry` is called on each `agent_session_message_appended` notification instead of full `get_session`. Initial full pull on screen entry still uses `get_session(include_full_content=true, include_images=true)`.
- [x] Tool-call rows render with collapsed/expandable state and show status pill matching one of the seven server strings ("pending" / "waiting for confirmation" / "running" / "done" / "failed" / "rejected" / "canceled").

## When done

Sub-agent reports the new sibling-repo commit SHA, `:core` test count delta, new APK size, markdown library version + APK-size impact, and any place where the server JSON shape didn't match the spec.
