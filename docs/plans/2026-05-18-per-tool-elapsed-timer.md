# Per-tool elapsed timer for InProgress tool calls

**Status:** complete
**Estimated:** 1 sub-agent session, ~45–75 min (HEAVY)
**Goal:** Each tool call that's currently running shows a live "Xs" elapsed badge next to its status, so the user can see at a glance how long a tool has been hanging vs. just finished.

## Context

The chat surface already shows `(running)` text next to in-flight tool calls (`conversation_render.rs::render_tool_call`, line 759 area; status text from `tool_call_status_text`). What's missing: a duration showing how long it's been running. The session-level "Thinking… Ns" counter on the status row demonstrates the equivalent pattern (`status_row.rs:237 ensure_thinking_tick` — 1s background tick + cx.notify while the session is `Running`).

User pain: when a tool sits for a long time (slow network, hung subprocess, long file read), the user can't tell if it's frozen or just slow. A live elapsed timer disambiguates.

Mobile is task #9 and is blocked on the parallel-session mobile-WIP situation. This plan covers ONLY the server + desktop sides; mobile picks up the new wire field later.

## Scope

### A. `acp_thread`: add `status_started_at: Option<chrono::DateTime<Utc>>` on `ToolCall`
- **File:** `crates/acp_thread/src/acp_thread.rs` (untouched-upstream crate per FORK.md — patch must stay additive, no renames/refactor; this counts as first-touch → add a row to `FORK.md` § "Notable upstream file modifications").
- **Field:** `pub status_started_at: Option<chrono::DateTime<Utc>>` on `ToolCall` (lines 247–260). `chrono::Utc` because it serialises to the wire cleanly (Instant doesn't).
- **Init sites:**
  - `ToolCall::from_acp` (line 263): if `status` arg is `InProgress`, set `status_started_at = Some(Utc::now())`; else `None`. Most adapter-created tool calls are `Pending` at construction, so usually None.
  - `ToolCall::update_fields` (line 320): when the incoming `status` flips to `InProgress`, set `status_started_at = Some(Utc::now())`. When the status flips OUT of `InProgress` to a terminal state, **keep** the timestamp (used for "ran for Xs" historical display).
- **One new test** in the existing `acp_thread::tests` module (or `#[cfg(test)] mod` in the same file): construct a ToolCall, drive a status update via `update_fields` to `InProgress`, assert `status_started_at` is Some and within a small delta of `Utc::now()`.

### B. `solution_agent` MCP: expose `tool_status_started_at_ms` in tool-call summary payload
- **File:** `crates/solution_agent/src/mcp.rs` around line 562 (tool-call summary builder) and line 921 (similar second site). The current payload carries status text via `tool_call_status_text`; add a new field next to it.
- **Field:** `pub tool_status_started_at_ms: Option<i64>` on the relevant payload struct. Populate from `call.status_started_at.map(|t| t.timestamp_millis())`.
- **Mobile consumes via this**; the DTO addition is back-compat (optional).
- **One unit test** in mcp.rs (~line 2420 area) asserting the ms field surfaces when the underlying ToolCall has a started_at.

### C. `solution_agent` cold persistence: don't try to serialise it
- **File:** `crates/solution_agent/src/cold_persistence.rs`. The cold blob only stores `TerminalToolCallStatus` (Pending/InProgress are never cold). So `status_started_at` is irrelevant to cold persistence; no change needed. **Verify by reading** the file — if there IS a path that serialises an InProgress ToolCall (shouldn't be one), drop the field on the way to disk and reconstruct as None on hydrate.

### D. `solution_agent` desktop UI: render the elapsed badge
- **File:** `crates/solution_agent/src/conversation_render.rs::render_tool_call` (~line 752).
- Where the status `Label::new(status_text)` is appended (~line 791), add — when `call.status` is `ToolCallStatus::InProgress` AND `call.status_started_at.is_some()` — a sibling `Label` carrying e.g. `"4s"` or `"1m 12s"` (whichever your existing duration formatter renders nicely; check status_row.rs:340 for the format pattern — `let elapsed = started_at.elapsed().as_secs();` then probably `format!("{elapsed}s")`).
- For terminal statuses (`Completed`/`Failed`/`Canceled`/`Rejected`) with a `status_started_at`: optional follow-up to show "ran 4s" — keep it OUT OF SCOPE for this phase to avoid scope creep. Only InProgress shows the badge.

### E. `solution_agent` session view: 1s tick while any visible tool call is InProgress
- The existing `status_row.rs::ensure_thinking_tick` ticks the navigator while session is `Running` — but tool calls render in `session_view`, not in the status row. So they don't get notified by the existing tick.
- **File:** `crates/solution_agent/src/session_view.rs` (or `session_view/` directory — find the entity rendering the conversation).
- Add an `ensure_tool_tick(&mut self, cx: &mut Context<Self>)` mirroring the status_row pattern:
  - Field `tool_tick: Option<Task<()>>` on the view.
  - On each render, if any entry in the displayed thread is a `ToolCall` with status `InProgress`, call `ensure_tool_tick`.
  - The spawned task: loop `cx.background_executor().timer(Duration::from_secs(1)).await` → on update, check if any visible tool is still InProgress; if yes, `cx.notify()` and continue; if no, drop the task and clear the slot.
- Rationale: piggybacking on the navigator's tick would force a re-architect (session view doesn't have a handle to the navigator). One extra tick per session view that has InProgress tools is cheap.

## Out of scope

- Mobile UI (task #9 in pool — will consume the new `tool_status_started_at_ms` field once the parallel session frees the mobile repo).
- "Ran for Xs" historical display on terminal statuses (deferred; would be a 2-line addition once the field is in).
- Per-tool wall-clock display ("started at 14:32:18") — only elapsed is in scope here.

## Architectural decisions

1. **`Option<DateTime<Utc>>` not `Option<Instant>`** in `acp_thread::ToolCall`. Reason: the field has to make it through MCP wire serialisation; Instant is monotonic-clock-only and unprintable. Cost: one extra `.timestamp_millis()` call at render time per tool. Acceptable.
2. **Field on `acp_thread::ToolCall`, not a side-table in `solution_agent`.** Reason: single source of truth, no lifecycle/eviction bugs. Cost: one new field on an untouched-upstream struct → first-touch on `acp_thread/src/acp_thread.rs` (additive, allowed per ADR-0001 + FORK.md rules; add the row).
3. **Keep `status_started_at` after status flips to terminal.** Reason: enables the deferred "ran for Xs" display later without re-touching this code. Cost: 8 bytes per terminal tool call in memory. Negligible.
4. **Per-session-view 1s tick, not piggybacked on the navigator tick.** Reason: session view has no clean handle to the navigator without re-plumbing; the navigator's tick is scoped to status-row repaint and doesn't propagate down. Cost: one extra `Task` per visible session view with running tools (typically ≤1 at a time). Acceptable.

## Risks

- **Test flakiness on `Utc::now()` assertion** — the equality test should use a delta tolerance (`assert!((now - field).num_milliseconds().abs() < 1000)`), not an exact match.
- **Tick loop leak if session view drops while task runs** — the cleanup branch (clear `self.tool_tick = None` from inside the task on no-more-running) handles the normal case; for view-drop, the task's `this.update(cx, ...)` returns Err which breaks the loop. Standard gpui idiom; existing `ensure_thinking_tick` works the same way.
- **Re-render storm** if a session has many InProgress tools simultaneously: still just one tick per second per session view, one cx.notify call, one repaint. Not a concern at sane tool concurrency (≤10 simultaneous tools).
- **First-touch on `acp_thread/src/acp_thread.rs`** raises future cherry-pick cost from upstream Zed on that file. Trade: small ongoing cost vs. having the elapsed feature. User has explicitly asked for it.

## Verification

```bash
cargo build --bin spk-editor
cargo clippy -p acp_thread --all-targets -- -D warnings
cargo clippy -p solution_agent --all-targets -- -D warnings
cargo test -p acp_thread --no-fail-fast
cargo test -p solution_agent --no-fail-fast
```

Manual visual smoke-test (supervisor does this after merge):
- Open editor (headless or display), open Citeck Launcher solution, send a prompt that triggers a slow tool (long Read of a big file, or a Bash sleep). Confirm "Xs" badge appears next to the running tool's status text and ticks every second. Confirm it stops at the moment the status flips to Completed/Failed (badge disappears or freezes per design).

## When done

- [x] `acp_thread::ToolCall.status_started_at` field present + populated in `from_acp` + `update_fields`. Monotonic-stamp: only the genuine `Pending → InProgress` transition stamps (repeated InProgress flips ignored), so the elapsed counter doesn't reset on adapter status churn.
- [x] `FORK.md` updated — new row for `crates/acp_thread/src/acp_thread.rs`.
- [x] `solution_agent::mcp` tool-call summary payload carries `tool_status_started_at_ms`.
- [x] `solution_agent::conversation_render::render_tool_call` shows the elapsed badge for InProgress (via `status_row::format_elapsed` promoted to `pub(crate)` so both surfaces share the `Xs / 1m02s / 1h05m` format).
- [x] `solution_agent::session_view` ticks once per second while any visible tool is InProgress.
- [x] 2+ new tests (acp_thread `test_status_started_at_set_when_tool_call_enters_in_progress`; mcp `tool_call_entry_surfaces_status_started_at_when_in_progress` + existing Pending test updated to assert None).
- [x] cargo build / clippy / test all green for `acp_thread` (50 passing) and `solution_agent` (117 unit + 2 e2e = 119 passing; combined 169).

## Final commit SHAs
- `198c04de94` acp_thread: track when tool-call status becomes InProgress (+FORK.md row)
- `ed689dba9f` solution_agent: surface tool_status_started_at_ms on MCP wire
- `53c5bc193d` solution_agent: render elapsed "Xs" badge next to InProgress tool calls

## Implementation notes worth carrying forward

- The plan said "two tool-call summary builder sites at mcp.rs:562 and :921" — actually a single shared helper `tool_call_summary` is invoked from `build_entry_summary_for_index` and covers both surfaces. Plan's mental model was stale; one edit was sufficient.
- `cold_persistence` needed a one-line addition: hydrate sets `status_started_at: None` because cold blobs only carry terminal statuses (no on-disk schema change).
- Visual smoke-test (live editor + slow tool call) deferred to the maintainer's next editor restart — running release-fast process is stale relative to these commits.
