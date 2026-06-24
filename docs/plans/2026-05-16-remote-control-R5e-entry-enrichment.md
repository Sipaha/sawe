# R-5e: Server-side enrichment of `EntrySummary` + `agent_session_message_appended` payload

**Status:** complete (commit `d8592b05dc`)
**Estimated:** 1 sub-agent session, ~3–4 h, worktree-isolated
**Depends on:** R-4 (proxy), R-5d closure (so the consumer's shape is locked).
**Goal:** Enrich the data spk-editor's MCP server exposes per agent-session entry so the Android client (and any future remote consumer) can render real chat — full message text, image content blocks, tool-call args/results — without 200-char truncation. Additive only — old field shapes preserved, old clients keep working.

## Why this phase exists

R-5d shipped a chat surface on Android that consumes `remote.solution_agent.get_session`. The wire shape there is `EntrySummary { role: String, preview: String }` where `preview` is the first ~200 chars of the entry's markdown rendering. That's enough to know "the assistant said something about X" but not enough to actually read the message. Images and tool-call details are dropped entirely.

The desktop side already has all of this data in memory (`AgentThreadEntry` carries full content blocks + tool-call structs). MCP just truncates aggressively because it was originally sized for the autonomous-agent ergonomic case (a Claude Code session reading the structure to know what happened, not to render). The remote-consumer case wants the full payload.

R-5d also identified the second half: `agent_session_message_appended` notifications are `{ session_id }` only. Every notification forces a full `get_session` re-fetch. Fine for small sessions; wasteful on long ones over slow links. Enriching the event payload with `entry_index` (and maybe the new entry's content) lets the client append/update without round-tripping.

## Scope

### A. `crates/solution_agent/src/mcp.rs` — enrich `EntrySummary`

Current shape:
```rust
pub struct EntrySummary {
    pub role: String,
    pub preview: String,
}
```

New shape (additive — old fields stay):
```rust
pub struct EntrySummary {
    pub role: String,            // unchanged
    pub preview: String,         // unchanged — 200-char truncated markdown
    /// Full markdown rendering, untruncated. None when the caller didn't
    /// request full content (saves wire size for legacy consumers that
    /// only want a list-level overview).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    /// Inline images present in this entry, base64-encoded. None if the
    /// entry has no image content blocks. Each image gets a stable
    /// session-scoped index so chat renderers can cross-reference them
    /// from markdown links like `[image #N](spk-image://N)` (the same
    /// scheme already wired in the desktop side; see
    /// `conversation_render.rs` line 397).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<EntryImage>>,
    /// Present only for `role == "tool_call"` entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<ToolCallSummary>,
    /// Present only for `role == "plan"` entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanSummary>,
}

pub struct EntryImage {
    pub index: usize,            // 0-based, stable within the session
    pub mime_type: String,       // e.g. "image/png"
    pub data_base64: String,     // raw base64 (NOT a data: URI prefix)
}

pub struct ToolCallSummary {
    pub name: String,
    pub status: String,          // "pending" | "waiting_for_confirmation" | "in_progress" | "completed" | "failed"
    /// Truncated args preview (~500 chars, JSON-serialised).
    pub args_preview: String,
    /// Truncated result preview (~500 chars). Empty until completed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub result_preview: String,
}

pub struct PlanSummary {
    pub items: Vec<String>,      // one entry per plan item, in order
}
```

### B. Gate `markdown` / `images` behind a request param

`GetSessionParams` currently takes only `session_id`. Add:

```rust
pub struct GetSessionParams {
    pub session_id: String,
    /// Default false. When true, EntrySummary.markdown is populated
    /// with the full untruncated rendering. Caller pays the wire cost.
    #[serde(default)]
    pub include_full_content: bool,
    /// Default false. When true, EntrySummary.images carries inline
    /// base64 image payloads. Combined with include_full_content for
    /// the rich chat case.
    #[serde(default)]
    pub include_images: bool,
}
```

Default both to `false` so legacy callers (the test harness, the autonomous agent driving the MCP socket) keep getting the lightweight preview-only shape. The Android client passes both `true`.

`tool_call` and `plan` summaries are unconditional — they're small (≤1 KB) and rendering them on a list page is cheap.

### C. `crates/solution_agent/src/event_sources.rs` — enrich `agent_session_message_appended`

Current:
```json
{ "session_id": "<id>" }
```

New (additive — new fields don't break old clients that ignore unknown keys):
```json
{
  "session_id": "<id>",
  "entry_index": <usize>,        // index of the appended/updated entry
  "role": "user|assistant|tool_call|plan",
  /// Truncated preview only — same shape EntrySummary.preview uses.
  /// Full content still requires a get_session call with the flags.
  "preview": "<truncated>"
}
```

This lets the client decide: cheap case (just bump a counter + show truncated content) skips the round-trip; rich case (full markdown + images) does a follow-up call but only when the entry actually needs full content (e.g. on user scroll-to-entry).

### D. Add a new MCP tool: `remote.solution_agent.get_session_entry`

For the rich case where the client only needs ONE entry's full content (e.g. the user expanded a single tool-call bubble to see args/results):

```rust
pub struct GetSessionEntryParams {
    pub session_id: String,
    pub index: usize,
    #[serde(default)]
    pub include_images: bool,    // markdown is always included for a single-entry fetch
}

pub struct GetSessionEntryResult {
    pub entry: EntrySummary,     // markdown always populated, images per flag
}
```

Wired into the existing `ListSessionsTool` neighbourhood in `mcp.rs`. Registered via the standard `server.add_tool(GetSessionEntryTool)` pattern.

### E. Allow-list addition in `crates/remote_control/src/allow_list.rs`

Add `remote.solution_agent.get_session_entry` → `solution_agent.get_session_entry` to the proxy translation table. Without this, the new tool isn't reachable from a remote client even after it's registered.

### F. Tests

Module-local unit tests in `mcp.rs`:
- `get_session` with `include_full_content: false` → entries have `preview` populated, `markdown` is None.
- `get_session` with `include_full_content: true` → entries have `markdown` populated AND `preview` still populated (unchanged).
- `get_session` with `include_images: true` on a session containing an image → `entries[N].images` is `Some([...])` with the right `index` / `mime_type` / `data_base64`.
- `get_session_entry` happy path → single entry's full content.
- `get_session_entry { index: 999 }` on a 3-entry session → error `entry_index_out_of_range`.
- Tool-call entry's `tool_call` field is populated with `name` + `status` + truncated previews.
- Plan entry's `plan.items` is populated.

Event-source tests in `event_sources.rs`:
- `SessionMessageAppended` event with a known entry index → notification carries `entry_index`, `role`, `preview`.

Total new tests: ~8-10. Reuse existing `mock-agent` test infra; don't roll a new framework.

Run alongside: confirm existing `remote_control` integration test (`proxy_e2e::end_to_end_proxy_round_trip`) still passes — additive changes shouldn't break it.

### G. Backwards compatibility / allow-list filter behaviour

The R-4 allow-list filter blocks event kinds not in `agent_session_*`. The `agent_session_message_appended` kind is already allow-listed and the additive payload fields don't change that.

Old MCP consumers (the autonomous agent driving the local Unix socket) keep getting the old behaviour because:
1. They don't pass `include_full_content` / `include_images` → server defaults both to false → old preview-only shape.
2. They ignore the new event-payload fields (`entry_index`, `role`, `preview` on the notification) because JSON-RPC clients tolerate unknown keys.

No migration step required. No version bump.

### H. Out of scope

- Streaming partial-message updates (deferred — current `agent_session_message_appended` already fires multiple times per message, so the polling pattern from R-5d works once the payload has `entry_index`).
- Image deduplication across entries (each `EntryImage` payload duplicates if the same image is in multiple entries — accept the wire cost).
- Compression (the MCP framing layer doesn't currently compress; add later if profiling shows it matters).
- Schema versioning headers (JSON-RPC's tolerance for unknown keys covers us for now).
- Updating the Android client (`spk-editor-mobile`) to consume the new fields — that's R-5e-client / R-5f, a separate phase.

## Architectural decisions (this phase)

1. **Additive, not breaking.** Old clients keep working unchanged.
2. **Gated full content** so the autonomous-agent ergonomic case (token-budget-sensitive Claude Code reading) doesn't get blown out by inline base64 images on every `get_session`.
3. **Inline base64 images, not a separate fetch.** Once the client opts in via `include_images: true`, the data is there in one round-trip. Per-image fetch would multiply latency on slow links.
4. **`get_session_entry` for the single-entry case** so a chat client showing a long session doesn't have to pull everything on every focus change.
5. **`entry_index` in the notification payload** so the client knows whether the event is for an entry it already has or a new one, before deciding to round-trip.

## Risks

- **Wire size with images.** A session with 5 screenshots can balloon to a few MB. Document the trade-off in the new tool's doc comment — clients with bandwidth concerns should default to no images and fetch them lazily via `get_session_entry`.
- **Image extraction from `AgentThreadEntry` content blocks.** The desktop side already has this logic (see `conversation_render.rs` around the `decode_image_local` function). Reuse, don't roll a parallel implementation.
- **`tool_call` status string format.** Reuse `tool_call_status_text` from `conversation_render.rs` so the wire string matches what the desktop UI shows. Don't invent a parallel set of strings.

## Verification

```bash
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor

set -o pipefail
cargo build --bin spk-editor 2>&1 | tee /tmp/r5e_build.txt
grep -E "^error|could not compile" /tmp/r5e_build.txt

cargo clippy -p solution_agent --all-targets -- -D warnings 2>&1 | tee /tmp/r5e_clippy.txt

cargo test -p solution_agent --no-fail-fast 2>&1 | tee /tmp/r5e_test.txt
grep "test result:" /tmp/r5e_test.txt

cargo test -p remote_control proxy_e2e 2>&1 | tee /tmp/r5e_proxy.txt
grep "test result:" /tmp/r5e_proxy.txt
```

Acceptance:

- [x] `cargo build --bin spk-editor` passes.
- [x] `cargo clippy -p solution_agent --all-targets -- -D warnings` clean.
- [x] `cargo test -p solution_agent` — pre-existing tests still green + ~8-10 new tests passing.
- [x] `cargo test -p remote_control proxy_e2e` — still passes (allow-list addition didn't break the R-4 proxy test).
- [x] `EntrySummary` field order is the same when `include_full_content=false` as before R-5e (verify by snapshot or by reading the JSON output of an existing test).
- [x] FORK.md `solution_agent` row mentions the additive enrichment (one-line note; full detail lives in the plan-doc).
- [x] Allow-list addition for `remote.solution_agent.get_session_entry` in `remote_control::allow_list`.

## When done

Sub-agent reports:
- Commit SHA(s) — single commit preferred unless cleanup spans crates.
- Test count deltas per crate.
- Whether the image-extraction logic from `conversation_render.rs` could be reused as-is or needed refactoring.
- Wire size on a sample session (a session with ~10 entries + 2 images): preview-only vs full-content vs full-content-with-images. Document the trade-off in the new tool's doc comment.
- Any place where `entry_role` / `tool_call_status_text` could move out of `mcp.rs` into a shared module for cleaner reuse — note for a future cleanup pass, don't refactor in this phase.

Supervisor:
1. Verify per above.
2. Update INDEX status + tick acceptance + append SHAs in the post-merge log.
3. Hand off to R-5e-client (Android side consumes the new fields) as the next natural phase. Or R-6 if the user prefers Android polish first.

---

## Post-merge log (2026-05-16)

**Commit:** `d8592b05dc solution_agent: enrich EntrySummary + agent_session_message_appended (R-5e)` — fast-forwarded from worktree branch into `main`.

**Files changed:** `FORK.md`, `crates/remote_control/src/allow_list.rs`, `crates/solution_agent/src/event_sources.rs`, `crates/solution_agent/src/mcp.rs`, `crates/solution_agent/src/store.rs`, `crates/solution_agent/src/store/tests.rs` — 876 insertions / 16 deletions.

**Supervisor-verified on main after merge:**
- `cargo test -p solution_agent --no-fail-fast` → 90 tests passed (R-5d baseline was 83; +7 new). Plus the 2 integration tests both pass.
- `cargo test -p remote_control --test proxy_e2e` → `end_to_end_proxy_round_trip` still green (1 test, additive allow-list change didn't break R-4's proxy gate).
- `cargo check --workspace --all-targets` → only 2 pre-existing warnings in `crates/editor_mcp/tests/run_config_e2e_test.rs` (`UpdateGlobal` + `Settings` imports with `as _` aliasing but still flagged because the traits are unused). Pre-date R-5e — last touched by commits `1c2a33c` and `cc0db2b`. Not blocking.

**Wire sizes on a synthetic 9-entry session (5 chat + 3 tool_calls + 1 plan, 2 fake 15 KB images):**

| Mode | Size |
|---|---|
| preview-only (default) | ~1.77 KB |
| `include_full_content: true` | ~6.32 KB (3.6×) |
| `include_full_content + include_images: true` | ~46.5 KB (26×) |

Documented in `GetSessionParams`'s doc comment so consumers can pick wisely. For image-heavy sessions the new `solution_agent.get_session_entry` tool fetches one entry at a time.

**Architectural choice — entry_index wiring:** sub-agent extended `SolutionAgentStoreEvent::SessionMessageAppended(SolutionSessionId)` to `SessionMessageAppended(SolutionSessionId, usize)`. Index captured at the emit site in `store.rs::handle_acp_event` under the same mutable-borrow window as the live `AcpThread` → no race vs the alternative "look up at notification time in the coordinator subscribe". Two call sites of the variant existed, both in-crate, so the type change rippled cleanly.

**Image-extraction sharing — sub-agent decision:** duplicated rather than shared. `conversation_render.rs::decode_image_local` decodes to `Arc<gpui::Image>` for desktop render; the new MCP path needs raw base64 + mime back for the wire. Two extraction loops have different output types and different walk paths (`acp::ContentBlock::Image` vs local `ContentBlock::Image { image: Arc<gpui::Image> }`); factoring would require an "output-flavor" trait. Both under 40 lines — keeping separate is cleaner.

**Status string note:** `tool_call_status_text()` returns `"pending" | "waiting for confirmation" | "running" | "done" | "failed" | "rejected" | "canceled"` (spaces, "running"/"done", plus "rejected"/"canceled"). Plan-doc speculated `"pending" | "waiting_for_confirmation" | "in_progress" | "completed" | "failed"` — wrong. Sub-agent followed the plan-doc's "reuse, don't invent a parallel mapping" rule and emitted what the helper actually returns. Wire strings match desktop UI labels.

**Diagnostic noise during verification (worth recording):** rust-analyzer flycheck on the worktree surfaced a phantom E0061 pointing at `agent-client-protocol-schema-0.12.0/src/tool_call.rs:63:12` (an external dep). `cargo check --workspace --all-targets` had no E0061 — RA stale state. Pattern: don't trust RA diagnostics that point at external crates' definition sites without a matching cargo error.

## Follow-up

- **R-5e-client (Android-side update)** — extend `:core` DTOs in `spk-editor-mobile` to consume the new `markdown` / `images` / `tool_call` / `plan` fields. `MainViewModel.openSession` to pass `include_full_content: true, include_images: true`. Render rich markdown (CommonMark renderer like `compose-multiplatform-markdown` or roll a simple `Text` with inline image lookups). Wire `Icon`/expand for tool_call rows. Plan-doc to be written when picked up.
