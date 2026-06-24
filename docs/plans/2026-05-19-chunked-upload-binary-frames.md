# Chunked upload via WebSocket binary frames + start-on-pick + resume

**Status:** complete
**Estimated:** server ~90 min (HEAVY), mobile ~120 min (HEAVY). Two sequential sub-agent dispatches.
**Goal:** Replace the inline base64 image flow with a proper chunked upload protocol. Upload starts the moment the user picks a file (not on Send). Chunks travel as WebSocket **binary** frames — no base64, no JSON encoding overhead. Survives WS disconnects + force-kills via `upload_status` resume. On Send, the message references uploaded blobs via `spk-upload://<id>` ResourceLinks; the server resolves these into `acp::ContentBlock::Image` (or Resource) just before forwarding to ACP, so the agent sees the same Image block as today.

## Context

Today the mobile attach flow base64-inlines image bytes into a JSON-RPC `send_message_blocks` text frame. 5 MB image → 6.7 MB on the wire, blew through the 64 KiB WS cap (post-mortem of bug #15). My first fix was to bump the cap to 32 MiB, but that misses the actual problem: shipping 6+ MB through base64 over mobile network is bad UX (slow, fails on flaky LTE, wastes metered data, no progress feedback, no resume). User caught this and asked for the right design.

Right design = chunked binary upload, started in the background when the file is picked, with per-chunk acks and post-disconnect resume. Standard file-transfer protocol shape, mapped onto WebSocket's binary frame type.

## Scope

### A. Server side — `solution_agent` upload manager + new MCP tools

**A.1 New module `crates/solution_agent/src/upload.rs`:**

```rust
pub type UploadId = u64;  // server-generated, monotonic counter

pub struct UploadState {
    pub id: UploadId,
    pub session_id: SolutionSessionId,
    pub mime: String,
    pub display_name: String,
    pub expected_size: u64,
    pub tmp_path: PathBuf,       // <runtime_dir>/uploads/<id>.bin
    pub received_bytes: u64,
    pub created_at: Instant,
    pub sha256: Option<String>,  // optional client-supplied integrity check
}

pub struct UploadManager {
    state: HashMap<UploadId, UploadState>,
    next_id: u64,
}

impl UploadManager {
    pub fn init(&mut self, ...) -> Result<UploadId>;
    pub fn write_chunk(&mut self, id: UploadId, offset: u64, data: &[u8]) -> Result<u64>;
    pub fn status(&self, id: UploadId) -> Option<u64>;
    pub fn finish(&mut self, id: UploadId, expected_sha256: Option<&str>) -> Result<UploadHandle>;
    pub fn abort(&mut self, id: UploadId) -> Result<()>;
    pub fn gc(&mut self, now: Instant, ttl: Duration) -> usize;  // returns # purged
    pub fn resolve(&self, id: UploadId) -> Option<&UploadState>;  // for send_message_blocks
}
```

- `tmp_path` lives under `<editor_mcp::runtime_dir()>/uploads/`. Created lazily on first `init`. Path scheme: `<runtime>/uploads/<UploadId>.bin`.
- `write_chunk` opens the tmp file with `O_RDWR | O_CREAT`, seeks to offset, writes, increments `received_bytes` only when offset matches `received_bytes` (refuses out-of-order writes — simpler than gap-tracking, mobile is sequential by design).
- `finish` checks `received_bytes == expected_size`, optional sha256 verify, returns a `UploadHandle { id, tmp_path, mime, display_name }` that `send_message_blocks` consumes to inline into the ACP message. The tmp file is NOT deleted at finish — it's deleted at GC OR when the resolved ResourceLink is consumed by `send_message_blocks`.
- GC: prune entries where `now - created_at > 1h`; delete tmp files. Run on a periodic timer (every 5 min).
- Stored as a `GlobalUploadManager(Entity<UploadManager>)` GPUI global, mirroring SolutionAgentStore's pattern.

**A.2 New MCP tools in `crates/solution_agent/src/mcp.rs`:**

```rust
// solution_agent.upload_init({ session_id, mime, display_name, total_size, sha256? })
//   -> { upload_id: u64 }
// solution_agent.upload_status({ upload_id })
//   -> { received_bytes: u64, total_size: u64 }
// solution_agent.upload_finish({ upload_id, sha256? })
//   -> { handle: "spk-upload://<id>" }
// solution_agent.upload_abort({ upload_id })
//   -> {}
```

All four tools follow the existing pattern (Params + Result structs, register in `register()`). `upload_init` validates session_id exists.

**A.3 Listener changes in `crates/remote_control/src/listener.rs`:**

Currently the post-auth dispatch loop reads `Message::Text` only and routes to JSON-RPC. Add a parallel branch for `Message::Binary`:

```rust
loop {
    match ws.next().await {
        Some(Ok(Message::Text(text))) => { /* existing JSON-RPC dispatch */ }
        Some(Ok(Message::Binary(bytes))) => {
            // Header: [u8; 16] = u64::to_be_bytes(upload_id) ++ u64::to_be_bytes(offset)
            if bytes.len() < 16 {
                /* drop with log */ continue;
            }
            let upload_id = u64::from_be_bytes(bytes[0..8].try_into().unwrap());
            let offset = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
            let payload = &bytes[16..];
            // Look up upload via GlobalUploadManager, write_chunk, fire
            // upload_chunk_acked notification with the new received_bytes.
        }
        // ... existing Close / Ping / Pong handling
    }
}
```

Notification: `upload_chunk_acked` fired after successful write, payload `{ upload_id, received_bytes }`. Goes through the existing notification fan-out (allow-listed by name prefix or by inclusion in the existing `should_forward_event` list — see below).

**A.4 Allow-list extension in `crates/remote_control/src/allow_list.rs`:**

Add `translate()` entries for `remote.solution_agent.upload_{init,status,finish,abort}`. Extend the `allow_list_round_trip` test.

For notifications: `should_forward_event` currently allows `agent_session_*`. Add `upload_*` to the allow-list so `upload_chunk_acked` reaches the WS client.

**A.5 WS cap dial-back:**

With binary chunks of 256 KiB max + 16-byte header, the largest single WS frame is ~262 KiB. JSON-RPC control messages are all tiny. Dial back `max_frame_size`/`max_message_size` from the current 32 MiB to **1 MiB** — generous headroom for chunk + a margin for any out-of-band oversized message, but doesn't leak 32 MB of pre-auth memory budget per connection.

**A.6 `send_message_blocks` ResourceLink resolution:**

Today `SendMessageBlocksTool` forwards `Vec<acp::ContentBlock>` verbatim to `store.send_message_blocks`. Pre-process: walk the blocks; for each `acp::ContentBlock::ResourceLink { uri }` where `uri` matches `spk-upload://<id>`:
1. Look up upload via `UploadManager::resolve(id)`.
2. If found AND mime starts with `image/`: read tmp file → construct `acp::ImageContent { data: base64(bytes), mime_type: mime, ... }` → replace the ResourceLink block with `acp::ContentBlock::Image(image_content)`.
3. If found AND mime is text-like: read tmp file → wrap as `acp::TextContent` with the same fenced-code format the mobile encoder used to produce.
4. If found AND mime is binary non-text: error "unsupported binary attachment" (matches existing client-side gate).
5. If NOT found (upload expired / never finished): error "unknown_upload_id: <id>".
6. After successful resolution, delete the tmp file (one-shot consumption).

The base64 encoding happens HERE (server-side, no network cost) — only because ACP's `ImageContent::data` is itself a base64 string. The wire from mobile→server is binary; the wire from server→agent (claude-acp adapter) inherits whatever ACP demands.

**A.7 Tests:**

- `upload::tests`: init → write a 200KB chunk → status returns 200K → write another 56K → finish → handle has correct size.
- `upload::tests`: out-of-order chunk rejected (gap detection).
- `upload::tests`: GC purges entries older than TTL.
- `upload::tests`: abort cleans up tmp file.
- `mcp::tests`: round-trip the 4 new MCP tools via ListSessionsTool-style fixtures.

### B. Mobile side — `:core` + `:app` upload manager + binary send + UI

**B.1 `core/.../UploadProtocol.kt` (pure-fn helpers):**

```kotlin
const val UPLOAD_CHUNK_HEADER_BYTES = 16
const val UPLOAD_CHUNK_PAYLOAD_BYTES = 256 * 1024  // 256 KiB

fun buildUploadChunkFrame(uploadId: Long, offset: Long, payload: ByteArray): ByteArray {
    // 8-byte BE upload_id + 8-byte BE offset + payload
}

@Serializable data class UploadInitParams(val sessionId: String, val mime: String, val displayName: String, val totalSize: Long, val sha256: String? = null)
@Serializable data class UploadInitResult(val uploadId: Long)
@Serializable data class UploadStatusParams(val uploadId: Long)
@Serializable data class UploadStatusResult(val receivedBytes: Long, val totalSize: Long)
@Serializable data class UploadFinishParams(val uploadId: Long, val sha256: String? = null)
@Serializable data class UploadFinishResult(val handle: String)  // "spk-upload://<id>"
@Serializable data class UploadAbortParams(val uploadId: Long)
@Serializable data class UploadChunkAckedPayload(val uploadId: Long, val receivedBytes: Long)
```

Unit tests in `:core` for `buildUploadChunkFrame` (header byte order, payload concatenation, size math).

**B.2 `RemoteClient` / `RemoteTransport` binary send path:**

Currently `OkHttpRemoteTransport.send(text: String)` sends text frames. Add `sendBinary(data: ByteString)`. OkHttp WebSocket has `webSocket.send(bytes: ByteString)` directly. `FakeRemoteTransport` mirrors with a capture list for tests.

**B.3 `app/.../UploadManager.kt`:**

Singleton (constructed once in MainViewModel.init like other repos). API:

```kotlin
class UploadManager(
    private val scope: CoroutineScope,
    private val context: ConnectionContext,
) {
    /**
     * Per-upload public state. `Done` carries the spk-upload://<id>
     * handle to embed in the eventual ResourceLink block.
     */
    sealed class State {
        data class Queued(val total: Long) : State()
        data class Uploading(val sent: Long, val total: Long) : State()
        data class Paused(val sent: Long, val total: Long, val reason: String) : State()
        data class Done(val handle: String) : State()
        data class Failed(val reason: String) : State()
    }

    /** Add a picked attachment; returns the local upload key. */
    fun start(localKey: String, uri: Uri, sessionId: String, mime: String,
              displayName: String, totalSize: Long): StateFlow<State>
    
    /** Pause/resume control (auto-invoked on WS reconnect transitions). */
    fun pauseAll()
    fun resumeAll()
    
    /** Cancel + abort server-side. */
    fun cancel(localKey: String)
    
    /** Called by SessionDetailStore after Send to release server tmp file. */
    fun forget(localKey: String)
}
```

Per-upload coroutine loop:
1. Call `remote.solution_agent.upload_init` → get `uploadId`.
2. Open InputStream from URI, read 256 KiB chunks.
3. For each chunk: build binary frame via `buildUploadChunkFrame(uploadId, offset, chunk)` → `transport.sendBinary(...)`.
4. Wait for matching `upload_chunk_acked` notification (`uploadId, receivedBytes >= offset + chunk.size`). Timeout 30s; on timeout, transition to Paused.
5. On Disconnected from `ConnectionContext`: pause loop, persist state.
6. On Reconnected: call `remote.solution_agent.upload_status({uploadId})` → resume from `receivedBytes`.
7. When `receivedBytes == totalSize`: call `remote.solution_agent.upload_finish` → State.Done(handle).

**B.4 Persistence for force-kill recovery:**

New repo `InFlightUploadsRepository` (per-server, EncryptedSharedPreferences key `inflight-uploads-v1-${serverId}`). Stores:

```kotlin
@Serializable data class PersistedUpload(
    val localKey: String,
    val uploadId: Long,
    val uriString: String,
    val sessionId: String,
    val mime: String,
    val displayName: String,
    val totalSize: Long,
    val lastConfirmedOffset: Long,
)
```

On UploadManager init: load persisted list → for each, call `upload_status` to get server-side offset → resume from there. If `upload_status` returns "unknown_upload_id" (server GC'd it): mark Failed("upload expired, please re-attach").

**B.5 SessionDetailScreen compose-row integration:**

- `PickedAttachment` data class extended with `localKey: String` and `uploadState: StateFlow<UploadManager.State>`.
- On pick (in PhotoPicker / OpenDocument callback): immediately call `uploadManager.start(...)` → store the StateFlow on the PickedAttachment.
- `AttachmentPreviewCard`:
  - Image thumbnail same as today.
  - Overlay: `CircularProgressIndicator` (small, top-right corner) when state is `Queued | Uploading | Paused`, with percent label below.
  - Green checkmark icon when state is `Done`.
  - Red error icon + tap-to-retry when state is `Failed`.
  - Dismiss × always available; calls `uploadManager.cancel(localKey)` and removes from list.
- Send button enabled condition: `text.isNotBlank() || (attachments.isNotEmpty() && attachments.all { state == Done })`.

**B.6 Send path rewrite:**

In `SessionDetailStore.sendMessageBlocks` (or wherever the picker callback assembles blocks for Send):
- Before today: `attachments.map { encodeAttachment(...) }` (inline base64).
- After: `attachments.map { ContentBlockDto.ResourceLink(uri = it.uploadState.value.handle) }` — every attachment is `Done` by the Send gate, so the handle is available.
- After successful Send: `uploadManager.forget(localKey)` for each — the server consumes the tmp file when resolving the ResourceLink, but explicit forget gates the local persistence cleanup.

**B.7 Cleanup:**

- DELETE `encodeAttachment` from `UserMessageBlocks.kt` (no longer used for images/files; only the text-file conversion to `ContentBlockDto.Text` might still be useful for small text attachments — keep that fork, route binary/image via upload).
  - Actually simpler: text files ALSO route through upload. Server-side resolution converts the text-file ResourceLink to a `TextContent` with the fenced-code format. Uniform UX, one less code path.
- Update `UserMessageBlocksTest.kt` to drop encoder tests; add ResourceLink-resolution tests on the server side.

**B.8 WS cap revert (server-side, owned by the server sub-agent in phase A):**

Once chunked upload lands and the inline-base64 image flow is gone, the post-#15 32 MiB WS cap is wasteful. Dial back to 1 MiB. Update the comment in `listener.rs` to reference this plan-doc instead of "multi-modal sends".

## Out of scope (V1)

- Multipart HTTP fallback. WebSocket binary frames work; no need for a side-channel server.
- Server-side dedup by sha256 (same blob uploaded twice → reuse). Future optim.
- Resume across server restarts. Server's tmp files survive a process restart, but the in-memory `UploadManager` doesn't reload them. V1: server restart kills in-flight uploads; mobile gets "unknown_upload_id" and reports. Future: persist `UploadState` index on disk too.
- Concurrent uploads per session > N. Cap at 4 concurrent (matches PhotoPicker multi-pick).
- Background upload while app is killed. Android requires a foreground service for that — out of scope. Upload continues only while the app process is alive; on kill, it pauses and resumes on next launch.
- Audio attachments.

## Architectural decisions

1. **Binary WebSocket frames, not base64-in-text.** Reason: user explicit ask + obvious efficiency win (no 1.33× wire inflation, no JSON escaping, no client-side encode + server-side decode). Cost: WS dispatch loop now branches on frame type — small fork-touch to `remote_control::listener`. How to apply: future binary protocols (audio streaming, file download) reuse the same Header { id, offset } shape.

2. **Server-generated upload_id (u64 counter), not client UUID.** Reason: fits cleanly in the 16-byte binary header, server already keeps the upload state authoritatively so it's the natural owner. Cost: client can't pre-allocate the upload id (small — `upload_init` round-trip is tiny). How to apply: server uses `AtomicU64` seeded at startup; recycle never (overflow at 1.8e19 uploads is a problem we'd love to have).

3. **256 KiB chunk size, fixed.** Reason: small enough to keep per-chunk latency low and progress smooth; large enough that header overhead is < 0.01% per chunk. Cost: a 5 MB image = 20 chunks = 20 round-trip acks. How to apply: make `UPLOAD_CHUNK_PAYLOAD_BYTES` a `:core` constant so server and mobile agree; bump together if profile shows ack-roundtrip is the bottleneck.

4. **Tmp files on server, not in-memory.** Reason: a large upload + a process restart would lose in-memory bytes anyway; tmp files survive (V1 doesn't restore the in-memory index from them, but the bytes don't churn through RAM). Cost: an extra fsync per chunk. How to apply: use `tokio::fs::File::write_all` per chunk; let the OS coalesce.

5. **`spk-upload://<id>` as a ResourceLink URI scheme, resolved server-side at send.** Reason: keeps ACP's ContentBlock vocabulary unchanged — the agent sees a normal `Image` block, fork-specific transport machinery stays behind the abstraction. Cost: server now mutates the blocks Vec before forwarding; one more place to keep schema-aware. How to apply: a single `resolve_upload_blocks(blocks, &uploads, cx) -> Result<Vec<acp::ContentBlock>>` helper called at the top of `SendMessageBlocksTool::run`.

6. **Per-chunk ack notification (not RPC response).** Reason: binary frames don't carry JSON-RPC ids; using a notification keeps the binary frame's wire shape minimal. Cost: client correlates by `uploadId` rather than per-chunk request. How to apply: server fires `upload_chunk_acked: {uploadId, receivedBytes}` after every successful write; client tracks per-upload `lastAckedOffset` and resumes from there.

## Risks

- **Out-of-order chunks**. V1 design rejects writes where `offset != receivedBytes`. Mobile is sequential by design (single-coroutine per upload), so this shouldn't fire in practice. If it does, client surfaces as Failed; user re-attaches.
- **Binary frame on the WRONG socket**. The `remote_control` listener path is the only one accepting client → server binary right now. If a future surface (e.g. desktop drag-drop over a hypothetical local WS) sends binary, the dispatcher needs to know which protocol it's in. V1: binary always means upload-chunk; if header parse fails, log + drop.
- **Tmp file disk fill**. 4 concurrent uploads × 5 MB max + GC TTL 1h = max 20 MB outstanding per session. Cheap. But if a buggy client floods upload_init: limit `max_concurrent_uploads_per_session = 4` and `max_total_outstanding_bytes_per_connection = 64 MB`. Reject `upload_init` with "too_many_uploads" beyond that.
- **Mobile force-kill mid-upload**. Persisted state allows resume on next launch. But the URI we persisted may be revoked (Android SAF persistable URI permission is opt-in). V1: if URI read fails post-resume, mark Failed("attachment access lost, please re-attach"). User picks again.
- **Server restart loses in-memory UploadManager** but keeps the tmp files. Mobile's persisted upload IDs would all become "unknown_upload_id". V2: persist `UploadState` index on disk too so the server can rebuild after restart. V1: explicit FAQ — server restart cancels in-flight uploads.
- **`send_message_blocks` resolving a non-existent upload_id**. Returns `unknown_upload_id` error, client surfaces in snackbar. User re-attaches.

## Verification

```bash
# Server
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor
cargo build --bin spk-editor
cargo clippy -p solution_agent -p remote_control --all-targets -- -D warnings
cargo test -p solution_agent --no-fail-fast  # 117 + upload tests
cargo test -p remote_control --lib  # 37 + allow-list extension

# Mobile
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor-mobile
./gradlew :core:test  # 222 + UploadProtocol tests + DTO tests
./gradlew :app:compileDebugKotlin
./gradlew :app:assembleDebug
```

Manual smoke (deferred to maintainer):
- Open chat, attach a 4 MB photo. Progress bar fills smoothly. Send → message appears, agent receives normal Image block.
- Same with airplane mode toggled mid-upload: progress halts → reconnect → progress resumes from where it stopped (no restart from 0).
- Force-kill app mid-upload, relaunch: persisted upload picks up, completes.
- Pick 4 max-size images: 4 concurrent uploads + progress per chip. Send disabled until all Done.
- Attach a 50 MB file: rejected at picker (mobile cap) BEFORE any upload starts.

## When done

- [ ] `solution_agent::upload` module + `UploadManager` GPUI global + 4 MCP tools.
- [ ] `remote_control::listener` binary-frame branch + `upload_chunk_acked` notification fan-out.
- [ ] `remote_control::allow_list` extended for the 4 new methods + the new notification kind.
- [ ] `send_message_blocks` resolves `spk-upload://<id>` ResourceLinks server-side.
- [ ] WS cap dialled back from 32 MiB to 1 MiB.
- [ ] Mobile `:core` `UploadProtocol.kt` (DTOs + frame builder + tests).
- [ ] Mobile `:core` `RemoteTransport.sendBinary(ByteString)` seam + Fake impl + test.
- [ ] Mobile `:app` `UploadManager` + persistence (`InFlightUploadsRepository`).
- [ ] Mobile `:app` `PickedAttachment` carries StateFlow<UploadManager.State>; preview shows progress.
- [ ] Mobile `:app` Send gate waits for all attachments `Done`; send uses ResourceLink not inline bytes.
- [ ] Mobile `encodeAttachment` deleted; text attachments also route through upload + server-side fenced-code resolution.

## Implementation notes worth carrying forward

- **Dep direction inverted from the original sketch.** Plan said listener.rs would call `solution_agent::upload::with_manager` directly, requiring `solution_agent.workspace = true` in remote_control's Cargo.toml. That feature-unifies a second rustls CryptoProvider into remote_control's dep set via the transitive `agent_servers` / `claude-acp` graph, breaking the post-auth TLS handshake (caught by `set_enabled_starts_and_stops_listener` test panic: "Could not automatically determine the process-level CryptoProvider from Rustls crate features"). Fix: `remote_control::set_binary_frame_handler(Arc<dyn Fn(&[u8]) -> Result<(), String>>)` indirection; the third-party `crates/zed/src/main.rs` wires `solution_agent::upload::dispatch_binary_frame` into it during init. Neither crate deps the other.
- **Header byte-order sign-extension trap.** Kotlin's `Long ushr ... and 0xFF` SHIFT-then-MASK is required; a naive `Long ushr ... .toByte()` chain sign-extends negative-looking ids to 0xFF for every byte. Test `negative-looking uploadId masks correctly without sign extension` catches it. Verified the binary header decoded with `u64::from_be_bytes` on the Rust side matches the bytes Kotlin emits.
- **UploadManager internal constructor pattern.** Class itself + nested `State` are `public` (MainViewModel exposes them on `startAttachmentUpload` return type), but the constructor is `internal` so only the coordinator can instantiate. Cleaner than `internal class` (which would leak `internal` types through public surface).
- **Queue-replay with stale upload.** No special handling needed: a replayed `send_message_blocks` whose ResourceLink references a GC'd upload bubbles up as a normal tool error → existing snackbar path. Documented in commit `8e5f10d`.
- **Single notification observer in SessionListStore** owns the existing subscribe loop; added `upload_chunk_acked` to its subscribe list + `uploadNotificationRouter` callback fired BEFORE the agent_session branches. Wired from `MainViewModel.init` via `sessionList.uploadNotificationRouter = uploadManager::onChunkAcked`.

## Final commit SHAs

Server:
- `76bbd57912` solution_agent: headless make_headless_project_for_solution helper (companion for #14 but shared infra)
- `65fd88d508` solution_agent: UploadManager + upload_{init,status,finish,abort} MCP tools
- `30fe8eed6a` remote_control: binary-frame chunk dispatch via BinaryFrameHandler trait + WS cap dial-back + allow-list

Mobile:
- `c270abb` core: chunked-upload protocol + binary-frame seam on RemoteClient
- `47b719e` app: UploadManager + InFlightUploadsRepository + notification wiring
- `8e5f10d` app+core: route attachments through chunked upload, drop encodeAttachment
