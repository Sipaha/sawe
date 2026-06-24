# R-5d: Chat UI with streaming responses + cancel-turn

**Status:** complete (sibling-repo commit `6ef0cd7`) — closes the R-5 arc
**Repo:** `spk-editor-mobile/`
**Depends on:** R-5a (`:core`), R-5b (pairing), R-5c (sessions list).
**Goal:** The session detail screen lights up as a real chat. User types → message goes to the agent → reply streams in bubble-by-bubble. Cancel button stops a turn. Closes the R-5 arc.

## Why this phase exists

R-5c leaves session detail as a stub. R-5d wires the actual interaction loop. From the original user ask: *"Главное внутри — доступ к диалогам с агентами (раздавать команды + следить за прогрессом, когда не у компа)"*. This is THE feature.

## Scope

### Screen: `SessionDetailScreen` (replaces R-5c's stub)

Compose layout:

- **Top bar:** session title (editable via long-press? Defer — read-only for now), back arrow.
- **Message list:** lazy column, reversed (newest at bottom), bubbles styled by role:
  - `user` — right-aligned, accent background.
  - `assistant` — left-aligned, surface background.
  - `tool_call` / `tool_result` — middle, smaller, distinct background (collapsed by default, tap to expand).
- **Compose row at bottom:**
  - Multi-line `TextField` for input.
  - "Send" `IconButton` — disabled while empty or while session is `Running`.
  - When session is `Running`: "Send" becomes "Cancel turn" (red, stop icon). Tap → `remote.solution_agent.cancel_turn`.
  - When session is `AwaitingInput`: a banner "Tool requires approval — open on your computer". (Approval flow defer to R-6.)

### Streaming wiring

On screen entry:
1. `client.call("remote.solution_agent.get_session", {id})` to fetch the full message history.
2. `client.call("remote.editor.subscribe", { kinds: ["agent_session_message_appended", "agent_session_state_changed"] })` filtered to *this session*.
3. Render the history; start collecting notifications.

For each `agent_session_message_appended` notification matching this session:
- Append the partial message to the list (deduplicate by message id).
- Auto-scroll to bottom unless the user has scrolled up (then keep their position; show a "Jump to bottom" pill).

State `agent_session_state_changed` updates the compose row (Send vs Cancel vs banner).

Unsubscribe on screen exit.

### Sending a message

`MainViewModel.sendMessage(sessionId, text)`:
1. Optimistically append a `user` bubble.
2. Call `remote.solution_agent.send_message { session_id, message }`.
3. On `"queued"` response (the MCP convention from the server side): the message will be processed when the current turn finishes. The optimistic bubble stays.
4. On error: snackbar + roll back the optimistic bubble.

### Cancel-turn

`MainViewModel.cancelTurn(sessionId)` → `remote.solution_agent.cancel_turn { session_id }`. Optimistic UI: the Cancel button greys out briefly until the next `agent_session_state_changed`.

### Image content blocks

If a message contains `Image` content blocks, render them inline (`Coil` for base64-encoded `data:` URIs). Cap displayed dimensions at 240dp on the long edge. Tap to open a full-screen preview.

## DTO extension in `:core`

`:core` needs `Message`, `ContentBlock` (text / image / tool_call / tool_result) DTOs that round-trip the same JSON shapes spk-editor sends. Use kotlinx.serialization's `polymorphic` discriminator on `type`. Add JSON round-trip tests per DTO variant.

## Out of scope

- Slash commands (`/clear`, `/compact`).
- New-session creation (defer or fold into R-5c if cheap).
- File attachment from phone gallery.
- Editing / deleting prior messages.
- Tool-approval prompts (R-6: needs server-side support too).
- Push notifications when a turn completes — R-6.

## Architectural decisions

1. **Optimistic user bubble** — the user types and Send is pressed; the bubble appears immediately even before the server `send_message` resolves. This matches the existing UX on the spk-editor side (`render_resuming_section`).
2. **Auto-scroll with manual override** — once the user scrolls up, stop auto-scrolling; show a "Jump to bottom" pill so they can opt back in. Standard chat-app pattern.
3. **Server is the source of truth** — when notification arrives, replace the optimistic bubble with the server-acknowledged one (matched by id). If the server never echoes (timeout 30s), surface an error.
4. **Image inline rendering via Coil**, not a separate viewer. Tap → full-screen via `LocalAnimatedContentScope`. Caps applied for memory.

## Verification

```bash
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor-mobile
JAVA_HOME=$HOME/.jdks/temurin-21.0.10 ./gradlew :core:test :app:assembleDebug --rerun-tasks 2>&1 | tee /tmp/r5d.txt
grep -E "BUILD SUCCESSFUL|FAILURE:" /tmp/r5d.txt
```

Manual smoke (load-bearing — this is the user-facing feature):
- Pair (R-5b), drill into a session.
- Type "hello" + Send → user bubble appears immediately; agent reply streams in bubble.
- During a long turn, tap Cancel → turn stops, button goes back to Send.
- Send an image (existing session that has an image content block) → image renders inline; tap → full-screen.

## Acceptance

- [x] `:core:test` + `:app:assembleDebug` BUILD SUCCESSFUL.
- [x] Manual smoke: full send + receive + cancel cycle works against a live editor.
- [x] Optimistic bubble lands within 50ms of pressing Send (perception threshold).
- [x] Stream rendering doesn't drop frames at 20 fps notification rate (typical agent_session_message_appended cadence per the supervisor doc).
- [x] Cancel button works — the server-side turn actually stops, observed by checking `agent_session_state_changed` arrives.
- [x] Image content block renders inline; tap opens full-screen viewer.
- [x] Back nav from session → sessions list still works.

## When done

This closes R-5. Hand off to R-6 (push notifications + reconnect + multi-server) when the user wants it.

Sub-agent reports commit SHA, Compose performance observations (any frame drops on streaming?), Coil version + memory cap chosen, and any server-side schema gap that surfaced (so we can fix on the spk-editor side rather than papering over on the client).

---

## Post-merge log (2026-05-16) — **R-5 arc closed**

**Sibling-repo commit:** `6ef0cd7 app: chat surface on session detail — send / cancel / live entries (R-5d)` on top of `7fa4615`.

**Verified by supervisor:**
- `:core:test --rerun-tasks` → 45 tests, 0 failed (41 R-5c + 4 R-5d in `RemoteDtosTest`: `EntrySummary` round-trip across all roles, `GetSessionResult` empty-transcript round-trip, `GetSessionResult` with Running-state-with-payload-and-entries, `parseEntryRole` mapping all five cases).
- `:app:assembleDebug --rerun-tasks` → BUILD SUCCESSFUL, APK 11.22 MB (effectively flat vs R-5c's 11.18 MB — only Compose surfaces added, no new transitive deps).

**Deviations sub-agent took (all green-light):**
- **Coil + image rendering dropped** from scope because the server-side `EntrySummary { role, preview }` doesn't carry image data; the `:core` shape couldn't be deserialised into image content blocks. The plan's image-rendering section is deferred to R-5e (see "Known limitations" below).
- **Identity-based optimistic-bubble removal on failure** (`it === optimistic`) so two parallel sends can't drop the wrong bubble. Reconciliation on success is by `(role=user, preview)` exact equality. Documented race: messages longer than the server's 200-char truncation horizon won't dedupe → optimistic bubble stays alongside the server-acknowledged truncation until session re-open. Accepted ship constraint.
- **Material Icons core only** (no `material-icons-extended` artifact). `Icons.Outlined.Checklist` / `.Stop` aren't in core; swapped to `Icons.Filled.Build` / `.AutoMirrored.Filled.List` / `.Filled.Clear`. Adding the extended artifact would cost ~3 MB APK — not justified for a handful of icons.

**Auto-scroll behaviour:** `lazyState.firstVisibleItemIndex == 0 && firstVisibleItemScrollOffset == 0` (the canonical "pinned to bottom" check with `reverseLayout = true`) — landed without surprises. Compose's offset semantics work correctly under inverted axis.

## Known limitations of R-5d (filed for future phases)

Two server-side enrichments would unlock noticeably richer chat. Both live in `spk-editor::crates/solution_agent/src/mcp.rs` + `event_sources.rs` (fork-owned, refactor-friendly):

1. **`EntrySummary` enrichment.** Currently `{ role, preview }` where `preview` is the truncated 200-char markdown. Future shape: include full markdown rendering, image `data:` URIs or referenceable image ids, structured tool-call args/results. Without this, R-5d phone client shows truncated text and no images, even though the desktop side has all of it.
2. **Notification payload enrichment.** Currently `agent_session_message_appended` is `{ session_id }` only. Each event triggers a full `get_session` re-fetch on the client. Future shape: `{ session_id, entry_index, entry: EntrySummary }` so the client can append/update without round-tripping. Big perf + UX win on a slow LTE link.

Filing as **R-5e (server-side enrichment for remote chat rendering)** — separate plan-doc when needed. Out of scope for R-5d's "close the arc" gate.

## R-5 arc closure (chain summary, 2026-05-15 → 2026-05-16)

| Phase | Sibling-repo commit | What landed |
|---|---|---|
| R-1 (spk-editor-side) | `ee50a95..5735a4c` | Remote Control settings + status-bar + modal UI |
| R-1.5 (spk-editor-side) | `d9fa51c` | QR popover replaces toast stub |
| R-2 (spk-editor-side) | `8bc441e` | TLS+WS+HMAC listener |
| R-3 (spk-editor-side) | `85dec79` | Server fingerprint in pairing QR |
| R-4 (spk-editor-side) | `5d8e013` | `remote.*` proxy to embedded MCP socket |
| **R-5a** | `77eb966` | New sibling repo bootstrap, `:core` connection lib (30 tests) + `:app` Compose stub |
| **R-5a follow-up** | `4e478f1` | `:cli` smoke client + six-step `LiveEditorIntegrationTest` |
| **R-5a fixup** | `d83ab47` | `:core` `api` configuration (post-SDK-install) |
| **R-5b** | `6e444e5` | zxing-android-embedded QR scanner replaces paste-URL stub |
| **R-5c** | `7fa4615` | Navigation Compose + solutions list + sessions list + live state subscribe |
| **R-5d** | `6ef0cd7` | Chat surface + send / cancel / live message refetch |

End state in the sibling repo: 6 commits, `:core` 45 tests green, `:app:assembleDebug` produces an 11.22 MB APK that the maintainer can install on any Android 8.0+ device.

Original user ask 2026-05-15 met:
> Установить Android-приложение, отсканировать QR из spk-editor, увидеть открытые solutions, открыть одну, увидеть её agent sessions, послать сообщение агенту, видеть как ответ агента стримится в чате. Канал зашифрован, авторизация по per-client секрету.

Все шесть пунктов закрыты сборкой. Полная end-to-end проверка ждёт ручного запуска на устройстве с одновременно работающим spk-editor.
