# F: Sub-agent indication UI

**Status:** complete — F-server `104881302c`, F-desktop `cd8a6aebb5`, F-phone (sibling) `1af444b`
**Repos:** spk-editor (server + desktop UI) → `spk-editor-mobile` (phone UI).
**Depends on:** R-5e (`get_session` enriched shape), R-5g (`create_session`), R-6e (pagination + index).
**Goal:** Surface "sub-agents" — independent AI sessions spawned from a parent session — in both the desktop session view and the phone client. Inspired by Claude Code's running-agents bar: a horizontal strip of bubbles above the status row, click a bubble to drill into that session's chat. Auto-hides when no sub-agents exist.

User-confirmed scope:
- a) **visible when running** — panel appears only when current session has child sub-agents.
- b) **inspectable** — clicking switches the active session to the sub-agent. Tokens consumed are displayed on the bubble.
- Design authority delegated to me — "потом откорректируем".

## Sub-agent model

We model sub-agents as **first-class spk-editor sessions** with a `parent_session_id` reference, NOT as Task-tool tool_use frames inside the parent's ACP thread. Rationale:

1. **Visibility (b) requires real transcripts.** Claude Code's internal Task tool dispatches do NOT stream their sub-agent transcripts through ACP — they're internal to Claude Code's runtime. We can't show "what they're doing" if we only see the final `tool_result`.
2. **Real sub-sessions have their own ACP threads.** That gives us full transcripts (the same `get_session(child_id)` API works), full token tracking, full state transitions, full reconnect / streaming behavior — all the infrastructure R-5/R-6 already built.
3. **The trade-off:** to surface a sub-agent in this UI, the parent agent has to **explicitly call `solution_agent.create_session({parent_session_id})`** rather than relying on Claude Code's built-in Task tool. We can encourage this via system-prompt instruction or a slash command in a future polish phase; this phase ships the infrastructure.

If the user later wants Claude Code's Task tool dispatches to ALSO show up — that's a separate phase that synthesises fake child sessions by parsing ACP tool_use frames. Defer.

## Scope

### Phase F-server (spk-editor)

**1. `crates/solution_agent/src/model.rs`** — `SolutionSession` gains:

```rust
/// Optional parent. None for top-level sessions. Set at creation
/// time via solution_agent.create_session({parent_session_id}).
pub parent_session_id: Option<SolutionSessionId>,
```

Plus persistence: add `parent_session_id` column to the cold-persistence schema (sqlite migration — additive nullable column). Backwards-compat: existing rows have NULL → top-level.

**2. `crates/solution_agent/src/mcp.rs::CreateSessionParams`**:

```rust
pub struct CreateSessionParams {
    pub solution_id: String,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_message: Option<String>,
    /// NEW: parent session reference. When set, the new session is
    /// shown as a sub-agent under the parent in the running-agents UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
}
```

Store the parent on creation. If `parent_session_id` references a non-existent session, error with `unknown_parent_session: <id>`.

**3. `SessionSummary` enrichment** (used by `list_sessions` + the new `get_session_children`):

```rust
pub struct SessionSummary {
    // ... existing fields (id, solution_id, agent_id, title, state, created_at, last_activity_at) ...
    /// Cumulative tokens reported by the agent (sum of input + output).
    /// None when no usage info available (cold session, or agent
    /// doesn't report).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Parent session reference. None for top-level sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
}
```

`total_tokens` comes from `SolutionSession::cached_total_tokens` (or read live from `AcpThread`'s `TokenUsage` if available).

**4. New MCP tool `solution_agent.get_session_children`**:

```rust
pub struct GetSessionChildrenParams {
    pub session_id: String,
}

pub struct GetSessionChildrenResult {
    pub children: Vec<SessionSummary>,  // ordered by created_at ASC
}
```

Lists immediate children. Empty list when none.

Also: extend `ListSessionsParams` with an optional `parent_session_id: Option<String>` filter (analogous to the existing `solution_id` filter — same crate, same pattern).

**5. Allow-list addition** in `crates/remote_control/src/allow_list.rs`:

```rust
"remote.solution_agent.get_session_children" => Some("solution_agent.get_session_children"),
```

**6. Events**: emit `agent_session_created` with parent info when applicable, so subscribers can react to "a new sub-agent appeared under session X". Existing event payload is `{ session_id }`; extend to:

```json
{ "session_id": "<id>", "parent_session_id": "<id>|null" }
```

Additive; old clients ignore the new field.

**7. Tests:**
- `create_session { parent_session_id }` → child has parent set; `get_session(child).structured_content` includes `parent_session_id`.
- `get_session_children(parent_id)` returns the child + total_tokens populated when usage data exists.
- `create_session { parent_session_id: <unknown> }` returns `unknown_parent_session` error.
- `list_sessions { parent_session_id }` filters correctly.
- `agent_session_created` notification carries `parent_session_id`.

Target: `solution_agent` tests grow by 5-6.

**8. FORK.md** one-line: "F: parent_session_id on sessions + get_session_children tool + token field on SessionSummary."

### Phase F-desktop (spk-editor, `crates/solution_agent/src/session_view.rs`)

Render a **sub-agents strip** as a new fixed-height container directly above the existing status row in `SolutionSessionView`:

```
┌──────────────────────────────────────────────┐
│ Message list                                  │
│ ...                                           │
├──────────────────────────────────────────────┤
│ ◉ main · 13m 25s                              │  ← Sub-agents strip
│ ○ explore-helpers · running · 138.3k tokens   │     (only visible when
│ ○ refactor-mod · idle  · 12.5k tokens         │      children exist)
├──────────────────────────────────────────────┤
│ Status row (existing)                         │
├──────────────────────────────────────────────┤
│ Compose row (existing)                        │
└──────────────────────────────────────────────┘
```

Per-row visual:
- Filled dot `◉` for the **currently focused** session; hollow `○` for others.
- Compact label: title (truncated ~40 chars) + state pill ("running" / "idle" / "awaiting input" / "errored", color-coded) + tokens (if available, abbreviated "138.3k").
- Hover background for click affordance.
- Click → switch the view's focused session to that one (existing session-switching plumbing exists in `solution_agent::store`).

Hide the entire strip when:
- The current session has no `parent_session_id` AND no children of its own.
- Children list is empty AND current session is top-level → strip absent.

Show the strip when:
- Current session has children (show them), OR
- Current session is a child (show siblings + parent in the strip — same view, navigate up/sideways).

The first row is always "main" (= top-most ancestor of current session). Subsequent rows are descendants of main, rendered DFS-ordered with one indent per nesting level. Indent visual: padding-left ~24px per level.

Data flow: subscribe to `SolutionAgentStoreEvent::SessionCreated` / `SessionStateChanged` / `SessionMessageAppended` (for token updates) → refresh the children list via the same crate's store API (no MCP round-trip, this is desktop in-process).

**9. Tests for the desktop UI:** add at least one rendering test that asserts the strip appears when children exist + is absent otherwise. Use the existing `solution_agent` test infra.

### Phase F-phone (`spk-editor-mobile`)

After F-server lands, the wire shape is stable. Phone UI:

- In `SessionDetailScreen.kt`, add a **horizontal LazyRow of agent chips** ABOVE the existing compose row + below the chat messages.
- Each chip: Material 3 `AssistChip` with leading dot icon (filled/hollow), label = `<title> · <tokens>` (truncated), trailing state indicator.
- Tap chip → `navController.navigate("solutions/{solutionId}/sessions/{childSessionId}")` — reuses existing route.
- Hide LazyRow when the children list is empty AND current session is top-level.
- On `solutions/{solutionId}/sessions/{id}` entry: ViewModel calls `remote.solution_agent.get_session_children(id)` once + subscribes to `agent_session_created` events filtered by `parent_session_id == id` to refresh.

Phone doesn't render the "main → children" tree structure (too cramped on a phone). Just the immediate children of the current session, plus a small "Parent: <title>" chip at the start when current session is a sub-agent (taps it → navigate up).

**10. `:core` DTO additions** mirroring F-server:

```kotlin
data class SessionSummary(
    // existing fields
    @SerialName("total_tokens") val totalTokens: Long? = null,
    @SerialName("parent_session_id") val parentSessionId: String? = null,
)

data class GetSessionChildrenResult(val children: List<SessionSummary>)
```

DTO round-trip tests for the new fields.

### Out of scope

- Synthesising fake sub-sessions from Claude Code's Task tool dispatches (deferred — separate phase).
- "Interrupt sub-agent" button (use existing `cancel_turn` on the child session).
- Session graph visualisation (tree view, "show full tree" modal). Strip is enough for v1.
- Cross-solution sub-agents (a sub-agent always belongs to the same solution as its parent).

## Acceptance — F-server

```bash
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor
set -o pipefail
cargo build --bin spk-editor 2>&1 | tee /tmp/F_build.txt
grep -E "^error|could not compile" /tmp/F_build.txt
cargo clippy -p solution_agent --all-targets -- -D warnings 2>&1 | tee /tmp/F_clippy.txt
cargo test -p solution_agent --no-fail-fast 2>&1 | tee /tmp/F_test.txt
grep "test result:" /tmp/F_test.txt
cargo test -p remote_control proxy_e2e 2>&1 | grep "test result:"
```

- [x] cargo build passes.
- [x] clippy clean.
- [x] solution_agent tests grow by 5-6 (99 → ~105).
- [x] proxy_e2e still passes.
- [x] Allow-list extended; FORK.md row updated.

## Acceptance — F-phone (after F-server merges)

```bash
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor-mobile
ANDROID_HOME=$HOME/Android/Sdk JAVA_HOME=$HOME/.jdks/temurin-21.0.10 ./gradlew :core:test :app:assembleRelease --rerun-tasks 2>&1 | tee /tmp/F_phone.txt | tail -10
grep -E "BUILD SUCCESSFUL|FAILURE:" /tmp/F_phone.txt
```

- [x] :core:test BUILD SUCCESSFUL — ~89-91 tests.
- [x] :app:assembleRelease BUILD SUCCESSFUL.
- [x] Release APK ≤ 2.4 MB.
- [x] Sub-agents chip row renders when children exist + collapses when empty.
- [x] Tap chip navigates to child session via existing route.

## When done

Server sub-agent reports: commit SHA, test counts, schema migration changes if any, whether token tracking integration was straightforward.

Phone sub-agent reports: commit SHA, test counts, APK size, any Compose `AssistChip` quirk, whether tree-style rendering tempted you to over-engineer.
