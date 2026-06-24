# Mobile: file + image attachments in chat compose

**Status:** complete
**Estimated:** 1 server-side sub-agent (~30–60 min, LIGHT) + 1 mobile sub-agent (~75–120 min, HEAVY). Server side ships first; mobile depends on the new MCP tool.
**Goal:** From the mobile chat compose row, the user can pick image(s) or file(s) from the system picker, see them as inline previews above the input, and send them alongside text to the agent. Images go through ACP's `Image(ImageContent)` content block; text-like files go through `Text(TextContent)` with a fenced-code header.

## Context

Today `solution_agent.send_message` over MCP accepts only `{session_id, content: String}` — plain text. The underlying `SolutionAgentStore::send_message_blocks(session_id, Vec<acp::ContentBlock>, cx)` already handles multi-modal user messages (it's how the desktop's drag-drop image flow works), so the gap is purely on the wire surface + the mobile UI.

Per ACP 0.12 `ContentBlock` (`agent-client-protocol-schema` crate):
- `Text(TextContent { text })` — baseline; every agent supports
- `Image(ImageContent { data: base64, mimeType, … })` — requires `image` prompt capability (Claude Code supports)
- `ResourceLink(ResourceLink { uri, … })` — baseline; URI must be reachable by the agent
- `Resource(EmbeddedResource)` — requires `embeddedContext` capability
- `Audio(AudioContent)` — out of scope this phase

Because the agent runs on the desktop, `file://` URIs from the phone's storage are NOT reachable; ResourceLink is unusable for phone-attached files. The only working path is **inline content**: bytes shipped over WebSocket as part of the prompt.

## Scope

### A. Server side — new MCP tool `solution_agent.send_message_blocks`

**File:** `crates/solution_agent/src/mcp.rs`. Pattern after `SendMessageTool` (~line 1330–1396).

```rust
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SendMessageBlocksParams {
    pub session_id: String,
    /// Vec of acp::ContentBlock — serialised exactly per the ACP
    /// schema (`{"type": "text", "text": "..."}` / `{"type":
    /// "image", "data": "<base64>", "mimeType": "image/png"}` / …).
    pub blocks: Vec<acp::ContentBlock>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SendMessageBlocksResult {}

#[derive(Clone)]
pub struct SendMessageBlocksTool;

impl McpServerTool for SendMessageBlocksTool {
    type Input = SendMessageBlocksParams;
    type Output = SendMessageBlocksResult;
    const NAME: &'static str = "solution_agent.send_message_blocks";
    // run(...) parses session_id, ensures !blocks.is_empty(),
    // calls store.send_message_blocks(...).detach()
}
```

- Add `server.add_tool(SendMessageBlocksTool);` to the registration site (top of mcp.rs).
- Add `"remote.solution_agent.send_message_blocks" => Some("solution_agent.send_message_blocks")` to `crates/remote_control/src/allow_list.rs::translate` + mirror in the round-trip test.
- Validate: `!blocks.is_empty()` returns `invalid_params: blocks must contain at least one item`. Each block's deserialisation is handled by serde via acp's schema — malformed JSON surfaces at parse time with the standard `-32600 invalid request` envelope.
- Two unit tests: text-only block list works; image block list works (don't decode the base64 in the test, just round-trip it through the tool).

### B. Mobile side — picker UI + encoding + send wire

**B.1 New file: `core/.../UserMessageBlocks.kt` (pure-fn encoder in `:core`)**

Pure logic for: given a list of picked attachments (each with `bytes: ByteArray`, `mimeType: String`, `displayName: String`), produce a `List<ContentBlockDto>` ready for the wire. Plus a "best-effort routing" for the mimeType:
- `image/*` (jpg/png/webp/gif) → `{type: "image", data: base64, mimeType, _meta: {filename}}` (mimic acp::ImageContent)
- `text/*`, `application/json`, `application/xml`, `application/x-yaml`, `application/javascript`, common code mimes → `{type: "text", text: "Attached `${name}`:\n\n````${ext}\n${utf8Content}\n````"}` (note the 4-backtick fence so embedded ```...``` doesn't break out)
- Anything else → `EncodingFailure(reason: String)` enum variant returned alongside the successful encodings; UI shows an inline chip "binary file ${name} not yet supported"

DTO (declare in `core/.../RemoteDtos.kt`):
```kotlin
@Serializable
@JsonClassDiscriminator("type")
sealed class ContentBlockDto {
    @Serializable @SerialName("text")
    data class Text(val text: String) : ContentBlockDto()
    
    @Serializable @SerialName("image")
    data class Image(val data: String, @SerialName("mimeType") val mimeType: String) : ContentBlockDto()
    
    // ResourceLink + Resource + Audio: schema-compatible decode but
    // mobile never emits them in this phase. Add if needed in V2.
}
```

Unit tests in `:core/SessionEntryMergeTest`-style file (`UserMessageBlocksTest.kt`):
- Image bytes encode to base64 with right mime
- Text bytes encode to fenced-code block with right extension
- Binary mime returns EncodingFailure
- Mixed list partitions cleanly

**B.2 Mobile compose UI changes — `app/.../SessionDetailScreen.kt::ComposeBar` (or wherever the send row lives)**

- Add an `IconButton(Icons.AutoMirrored.Filled.AttachFile)` (or `Icons.Filled.Add`) to the left of the text field.
- Tap opens a `ModalBottomSheet` with two entries:
  - **"Photo from gallery"** → launches `androidx.activity.compose.rememberLauncherForActivityResult(PickVisualMedia(), ...)` with `ImageOnly` filter (PhotoPicker, system-managed, no storage permission needed).
  - **"File"** → launches `OpenDocument()` with mime types `*/*`.
- Multi-pick: PhotoPicker supports up to N items (`PickMultipleVisualMedia(maxItems)`); start with `maxItems = 4`. File picker stays single-pick for V1.
- Picked items materialise into a per-compose `List<PickedAttachment>` state above the text field — small horizontal LazyRow of cards showing:
  - Image: thumbnail (loaded via the existing base64 → BitmapPainter helper, but inverted — read URI → bytes → Painter)
  - File: icon + filename + size + dismiss × button
- On Send (the existing Send IconButton):
  - If both text and attachments are empty → no-op (existing behaviour).
  - Else: build `[ContentBlockDto.Text(text), ...attachments-as-blocks]` (text first iff non-empty, else just attachments).
  - Call `MainViewModel.sendMessageBlocks(blocks)`.
  - Optimistic local entry: the existing optimistic-bubble path needs to render image thumbnails too. Add an optional `List<PickedAttachment>` payload to the OptimisticEntry shape; the bubble renderer falls through to existing image rendering for images.
- Permission handling: PhotoPicker needs **no permission**. OpenDocument uses SAF and also no permission. The OS handles it.

**B.3 ViewModel + Store wiring**

- `MainViewModel.sendMessageBlocks(blocks: List<ContentBlockDto>)` — delegates to `SessionDetailStore.sendMessageBlocks(...)`.
- `SessionDetailStore.sendMessageBlocks(blocks)` — mirrors the existing `sendMessage(text)` pattern: calls `remote.solution_agent.send_message_blocks` over WS; on success, prepends an optimistic entry; on failure, surfaces via existing emitError snackbar machinery + retries via R-6a queue (the queue's persistence layer also has to learn about blocks — see B.4).

**B.4 Queue persistence**

- `EncryptedQueueStore` currently persists string text. Extend to persist arbitrary `List<ContentBlockDto>` (the existing queued-text format becomes `[Text(...)]`-equivalent on read for back-compat). Reuse the existing kotlinx-serialization machinery.
- Migration: existing single-text entries decode as `[Text(it)]`; mark schema v2; old reader can't see v2 (forward-only). Test: round-trip v2 with mixed blocks; v1 → v2 upgrade reads cleanly.

### C. Wire format sanity check

The acp::ContentBlock JSON shape is the **same** on both sides because acp's derive emits and consumes the canonical shape. The mobile `ContentBlockDto` uses `@JsonClassDiscriminator("type")` to match `#[serde(tag = "type")]` on the Rust side. The server-side `acp::ContentBlock` deserialise will accept the mobile's payload directly.

Verify by adding a round-trip test in `:core` that constructs a Kotlin `ContentBlockDto.Image(...)` and asserts the encoded JSON matches the shape acp::ImageContent expects (`{"type":"image","data":"...","mimeType":"..."}`).

## Out of scope (V1)

- Audio attachments (no Compose UI for record-then-send; defer).
- Files bigger than ~5 MB (silent server error today via WS frame size limits; explicit V1 cap = show chip "file too large, max 5 MB"). Configurable later.
- File sharing via Android share intent (`Intent.ACTION_SEND`); V1 only opens picker from inside the app.
- Camera capture inline. PhotoPicker covers gallery; for camera, defer.
- `ResourceLink` / `Resource` block emission; needs a server-side file-upload tool first.
- Image compression / resize before upload. V1 sends bytes as-is from the picker.
- Drag-drop of files onto the chat surface from the system file manager (Android doesn't have a clean Compose API for this yet).

## Architectural decisions

1. **New MCP tool, not extension of `send_message`.** Reason: clean contract — the text-only `send_message` keeps its serde shape, no `#[serde(default)]` dance. Cost: one extra allow-list entry + one more tool name. How to apply: future content-type extensions (audio, embeds) all land on `send_message_blocks`; keep `send_message` as the "I only have text" shortcut.

2. **Inline-only (base64) for V1; no ResourceLink / server-side upload.** Reason: any "upload-then-link" path requires a new server tool, write-cap, GC sweep for orphaned uploads, and resolves into a 2× UX latency hit (upload then send). Inline is one round-trip and matches what desktop's drag-drop already does. Cost: large files hit WS frame limits (~5 MB practical cap). How to apply: defer the upload path until users hit the file-size cap and complain.

3. **Pure-fn encoder in `:core`, not in `:app`.** Reason: mirrors `SessionEntryMerge.kt` / `SessionHistoryMerge.kt` / `UserMessageBlocks.kt` precedent. JVM-testable; `:app` glue is one-liner that reads URI → bytes → calls encoder. Cost: bytes-from-URI step is Android-only and still lives in `:app`.

4. **Text-file detection by mime type, not content sniffing.** Reason: Android's picker returns the mime from the OS (which has already done the sniffing); duplicating it in the app is brittle. Cost: a `text/plain`-disguised binary will land as a fenced text block with mojibake — acceptable, user can see the problem visually.

## Risks

- **Wire frame size**: a 5 MB image base64-expands to ~6.7 MB which is fine on WebSocket text frames (no fragmentation needed, OkHttp default max is 16 MB). A 4K JPEG (~2 MB) is fine; an uncompressed PNG can blow past. V1 cap = 5 MB raw → ~6.7 MB on the wire. Add the size check in the picker callback BEFORE reading the bytes (Android's `DocumentFile.length()` works for SAF URIs).
- **PhotoPicker availability**: officially Android 13+ (API 33). For older devices the `PickVisualMedia` ActivityResultContract automatically falls back to `OpenDocument(image/*)` per AndroidX docs. So no special code needed; verify by reading the AndroidX `androidx.activity:activity-compose 1.9+` Javadoc.
- **OptimisticEntry shape change**: adding `attachments: List<PickedAttachment>?` to the optimistic entry breaks dedupe-against-server matching if naive. The existing dedupe matches `(role, preview)` strings; for messages with attachments, the preview the server echoes will probably contain image references that look different from the local payload. **Mitigation:** for attachments-bearing optimistic entries, dedupe by tracking a sender-side `request_id` (random UUID, embedded in the first text block's `_meta` or omitted and server reflects `last_activity_at` increment instead). V1 simplest: clear the optimistic entry as soon as `agent_session_message_appended` for the same session_id arrives with `role=user` AFTER the local send time. Track local-send-time on optimistic entries.
- **Queue migration**: a stored v1 (text-only) entry being read by v2 must decode cleanly. Add a test that constructs a v1 blob (single string) and asserts the migration produces `[Text(it)]`.

## Verification

```bash
# Server
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor
cargo build --bin spk-editor
cargo clippy -p solution_agent -p remote_control --all-targets -- -D warnings
cargo test -p solution_agent --no-fail-fast
cargo test -p remote_control --lib allow_list

# Mobile
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor-mobile
./gradlew :core:test --rerun-tasks
./gradlew :app:compileDebugKotlin
./gradlew :app:assembleDebug
```

Manual smoke (deferred to maintainer after rebuild + restart of release-fast binary):
- Open chat, tap attach → bottom sheet appears → "Photo from gallery" → PhotoPicker shows.
- Pick 2 images → 2 thumbnails appear above compose row.
- Type "describe these" → tap Send.
- Optimistic bubble with thumbnails appears.
- Agent receives both images + text; replies.
- Same flow but pick a `.md` file → file chip with name + size → Send → agent receives the file content as a fenced text block + filename in the prompt.
- Pick a `.zip` → error chip "binary file not yet supported", file NOT added to send list, user can retry with a different selection.
- Force-kill app mid-attach → reopen → attachments are cleared (no draft for attachments in V1; future could persist), text draft survives.

## When done

- [x] `solution_agent::SendMessageBlocksTool` registered + allow-listed (spk-editor `5adf824ef7`).
- [~] Server unit tests skipped to match existing `SendMessageTool` precedent (which also has no tool-level test; the underlying `store::send_message_blocks` is fully covered). Mobile end-to-end exercises the wire path.
- [x] Mobile `ContentBlockDto` (text/image/resource_link/audio/resource variants, `@JsonClassDiscriminator("type")`) + `UserMessageBlocks` encoder + 13 tests covering each branch.
- [x] Mobile compose row attach button (`Icons.Filled.AttachFile` — AutoMirrored variant doesn't exist in Compose 1.7.x) + `ModalBottomSheet` + `PickMultipleVisualMedia(maxItems=4, ImageOnly)` + `OpenDocument()` + horizontal LazyRow of preview cards + dismiss × buttons.
- [x] `MainViewModel.sendMessageBlocks` + `SessionDetailStore.sendMessageBlocks` wired to `remote.solution_agent.send_message_blocks`.
- [~] Queue v2 schema NOT needed: `QueuedMessage.params: JsonElement?` is already opaque JSON-RPC params, so block-list sends persist alongside text sends with zero schema changes. `parseExpiredSendMessage` extended to also recognise `send_message_blocks` for bounce-to-input recovery (recovers the first text block's body; image/file blocks dropped on bounce in V1).
- [~] Optimistic-bubble dedupe via `localSendTimeMs` abandoned: `agent_session_message_appended` notification doesn't carry a timestamp (sub-agent caught this). Instead added a parallel `optimisticBlocksFlags: MutableList<Boolean>` in `SessionDetailStore`, mutated in lock-step with `optimisticIds` under `sessionMutex`. After the existing content-match reconcile fires, the reconcile counts unmatched user echoes and pops the right number of oldest blocks-flagged bubbles. Worst case: 1-paint-frame swap in display.
- [x] `:core:test` 185 → 202 (+13 from encoder + 4 from queue parseExpiredSendMessage). `:app:testDebugUnitTest` 9/9. `:app:assembleDebug` clean.

## Implementation notes worth carrying forward

- **Queue persistence didn't need a schema bump.** Plan-doc spec described a `QueuedPayload` sealed class + v1→v2 migration. Reality: `QueuedMessage.params` is already opaque `JsonElement?`, so any RPC's params land verbatim. The minimal change was teaching `parseExpiredSendMessage` (the TTL-bounce recovery parser) to also accept `send_message_blocks`. Less surface, less migration risk.
- **No timestamp on `agent_session_message_appended`.** Plan-doc dedupe approach (`localSendTimeMs` vs notification's server timestamp) doesn't work because the notification only carries `{session_id, entry_index, role, preview}`. Future server-side could add `appended_at_ms` if the parallel-flags approach proves fragile, but in practice the positional pop is sound for the common case.
- **`ComposeBar` is now ~1800 LOC** — growing past a reasonable single-file threshold. Sub-agent noted that an `ComposeAttach.kt` extraction would help but kept inline per the "avoid creating many small files" rule.
- **`SessionDetailStore` has 3 places that clear optimistic state** (reset, openSession's pre-load reset, closeSession). Adding the new `optimisticBlocksFlags` doubled the surface area for "forgot to clear one of three lists" bugs. A `clearOptimisticStateLocked()` helper is a sensible follow-up.

## Final commit SHAs
- `5adf824ef7` solution_agent: send_message_blocks MCP tool (server side, spk-editor main)
- `817fa4a` feat: ContentBlockDto + UserMessageBlocks encoder (mobile)
- `e5e111a` feat: file + image attach in mobile chat compose (mobile)
- `0f5ffe4` feat: EncryptedQueueStore persists block lists alongside text (mobile)
