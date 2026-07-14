# Teammate completion via an authoritative Stop-hook signal — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close an async `Agent` teammate's stream (tab) immediately on the authoritative Claude SDK `Stop` hook, instead of inferring completion by tailing its JSONL file — killing the recurring "tab stuck Live for up to an hour" class of bugs.

**Architecture:** The `Stop` hook already reaches the editor: the store's `HookPull` closure (set in `subscribe_to_session`) runs on every hook with `(acp_session_id, agent_id, is_end_of_turn)` and today only routes queued messages via `take_pending_for_delivery`. We extend that one closure: when a subagent's `Stop` fires (`is_end_of_turn`, `agent_id.is_some()`) with nothing left to deliver, the teammate is idle → close its stream. The five pre-existing heuristic detectors are then demoted: the JSONL close-trigger is removed, the stale reaper becomes a short-window backstop for a lost hook / dead process, and the kill path closes immediately. **No `claude_native` change is needed** — the signal already flows into the store closure.

**Tech Stack:** Rust, GPUI, the `solution_agent` crate. Tests are `#[gpui::test]` in `crates/solution_agent/src/store/tests/teammate_reconciler.rs`.

## Global Constraints

- Session ids and agent ids: the hook `agent_id` (hex) equals `background_agent::BackgroundAgentId` byte-for-byte (verified). Teammate stream key = `BackgroundAgent.parent_tool_use_id` (a `toolu_…`).
- `SolutionSession::close_stream(id, reason)` is the single close primitive; it records `closed_streams` + `rebuild_streams()`. `StreamId::Main` is non-closable (no-op).
- After mutating streams/background agents from the store, emit `cx.emit(SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(session_id))` (mirror `refresh_background_agent_snapshot`, teammate_reconciler.rs:329).
- Do **not** touch background-*shell* behavior or the shared `BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS` const (shells reuse it).
- Build: `cargo build -p solution_agent`; test: `cargo test -p solution_agent --lib`. Do NOT pipe through `tail` without `set -o pipefail`.
- Commit after each task. No `Co-Authored-By` trailer.

## File structure

- `crates/solution_agent/src/model.rs` — add `SolutionSession::pending_stop` field + `SolutionSession::take_pending_stop` helper.
- `crates/solution_agent/src/store.rs` — add `SolutionAgentStore::close_teammate_on_stop`; extend the `subscribe_to_session` `HookPull` closure.
- `crates/solution_agent/src/store/teammate_reconciler.rs` — drain `pending_stop` at registration (Task 1); remove the JSONL close-trigger (Task 2); short-window backstop (Task 3).
- `crates/solution_agent/src/model.rs` — kill path immediate close (Task 4).
- Tests: `crates/solution_agent/src/store/tests/teammate_reconciler.rs`.

---

### Task 1: Close a teammate on its authoritative `Stop` hook (keystone — fixes the bug)

**Files:**
- Modify: `crates/solution_agent/src/model.rs` (add `pending_stop` field ~line 528; add `take_pending_stop` method; init in ctor ~line 649)
- Modify: `crates/solution_agent/src/store.rs` (add `close_teammate_on_stop`; extend the closure at `subscribe_to_session` lines 3076-3096)
- Modify: `crates/solution_agent/src/store/teammate_reconciler.rs` (drain `pending_stop` at registration ~line 1015)
- Test: `crates/solution_agent/src/store/tests/teammate_reconciler.rs`

**Interfaces:**
- Produces: `SolutionSession::pending_stop: HashSet<BackgroundAgentId>`; `SolutionSession::take_pending_stop(&mut self, id: &BackgroundAgentId) -> bool`; `SolutionAgentStore::close_teammate_on_stop(&mut self, session_id: SolutionSessionId, agent_id: &str, cx: &mut Context<Self>)`.
- Consumes: `SolutionSession::close_stream`, `background_agents`, `BackgroundAgentId::new`, `SolutionAgentStoreEvent::SessionBackgroundAgentsChanged`.

- [ ] **Step 1: Write the failing test**

Add to `crates/solution_agent/src/store/tests/teammate_reconciler.rs` (mirror `background_agent_terminal_closes_teammate_stream`, lines 1210-1294, for setup):

```rust
#[gpui::test]
async fn subagent_stop_hook_closes_teammate_stream(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
    let parent_toolu = SharedString::from("toolu_X");
    let teammate = crate::stream::StreamId::Teammate(parent_toolu.clone());

    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.set_entries(
                vec![crate::session_entry::SessionEntry {
                    created_ms: 0,
                    mod_seq: 0,
                    subagent_id: Some(parent_toolu.clone()),
                    kind: crate::session_entry::SessionEntryKind::AssistantMessage {
                        chunks: vec![crate::session_entry::AssistantChunk::Message(
                            "streaming".to_string(),
                        )],
                    },
                }],
                cx,
            );
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: std::path::PathBuf::new(),
                    registered_at: chrono::Utc::now(),
                    latest: None,
                    last_offset: 0,
                    parent_tool_use_id: Some(parent_toolu.clone()),
                    latest_seq: 0,
                    killed: false,
                },
            );
            s.background_agent_order.push(bg_id.clone());
            assert!(s.streams.contains_key(&teammate), "teammate live before Stop");
        });
    });

    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.close_teammate_on_stop(session_id, "a30f92a688e431edc", cx)
        });
    });

    cx.update(|cx| {
        let session = SolutionAgentStore::global(cx).read(cx).session(session_id).unwrap();
        session.read_with(cx, |s, _| {
            assert!(!s.streams.contains_key(&teammate), "Stop hook closes the teammate");
            assert!(s.closed_streams.contains_key(&teammate), "close reason recorded");
        });
    });
}

#[gpui::test]
async fn subagent_stop_before_registration_buffers_then_closes(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("b11122233344455566");
    let parent_toolu = SharedString::from("toolu_Y");
    let teammate = crate::stream::StreamId::Teammate(parent_toolu.clone());

    // Stop arrives BEFORE the agent is registered → buffered, nothing closed.
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.close_teammate_on_stop(session_id, "b11122233344455566", cx)
        });
        let session = s.read(cx).session(session_id).unwrap();
        session.read_with(cx, |s, _| {
            assert!(s.pending_stop.contains(&bg_id), "stop buffered until registration");
        });
    });

    // Registration lands → drain the buffered stop → close the stream.
    cx.update(|cx| {
        let session = SolutionAgentStore::global(cx).read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.set_entries(
                vec![crate::session_entry::SessionEntry {
                    created_ms: 0,
                    mod_seq: 0,
                    subagent_id: Some(parent_toolu.clone()),
                    kind: crate::session_entry::SessionEntryKind::AssistantMessage {
                        chunks: vec![crate::session_entry::AssistantChunk::Message("x".into())],
                    },
                }],
                cx,
            );
            assert!(s.streams.contains_key(&teammate), "teammate live pre-drain");
            if s.take_pending_stop(&bg_id) {
                s.close_stream(teammate.clone(), SharedString::new_static("done"));
            }
            assert!(!s.streams.contains_key(&teammate), "drained stop closes the teammate");
        });
    });
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p solution_agent --lib subagent_stop`
Expected: FAIL — `no method named close_teammate_on_stop` / `no field pending_stop` / `no method take_pending_stop`.

- [ ] **Step 3: Add the `pending_stop` field + `take_pending_stop` helper**

In `crates/solution_agent/src/model.rs`, next to `background_agent_order` (lines 519-528):

```rust
    /// Subagent `Stop` hooks that arrived before the agent's `agentId:`
    /// announcement registered it in `background_agents`. Drained at
    /// registration (`take_pending_stop`) to close the teammate stream. Same
    /// `BackgroundAgentId` namespace as `background_agents`.
    pub pending_stop: std::collections::HashSet<background_agent::BackgroundAgentId>,
```

Init in the constructor next to `background_agent_order: Vec::new(),` (line 649):

```rust
            pending_stop: std::collections::HashSet::new(),
```

Add the helper method on `impl SolutionSession` (near `close_stream`, ~line 917):

```rust
    /// Remove and return whether a `Stop` was buffered for this agent (arrived
    /// before its registration). The registration site closes the teammate
    /// stream when this returns `true`.
    pub fn take_pending_stop(
        &mut self,
        id: &crate::background_agent::BackgroundAgentId,
    ) -> bool {
        self.pending_stop.remove(id)
    }
```

- [ ] **Step 4: Add `close_teammate_on_stop` to the store**

In `crates/solution_agent/src/store.rs`, add to `impl SolutionAgentStore` (near `take_pending_for_delivery`):

```rust
    /// A subagent's `Stop` hook fired (`is_end_of_turn`, its `agent_id`) with
    /// nothing left to deliver — it is idle and done. Close its demux teammate
    /// stream immediately. If the `agentId:` announcement has not registered the
    /// agent yet, buffer the stop (`pending_stop`) so `apply_subagent_lifecycle`
    /// closes it on registration. Authoritative: replaces the JSONL-tail guess.
    pub fn close_teammate_on_stop(
        &mut self,
        session_id: SolutionSessionId,
        agent_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let bg_id = crate::background_agent::BackgroundAgentId::new(agent_id.to_string());
        let closed = session.update(cx, |s, _| {
            match s
                .background_agents
                .get(&bg_id)
                .and_then(|ba| ba.parent_tool_use_id.clone())
            {
                Some(parent_toolu) => {
                    s.close_stream(
                        crate::stream::StreamId::Teammate(parent_toolu),
                        gpui::SharedString::new_static("done"),
                    );
                    true
                }
                None => {
                    s.pending_stop.insert(bg_id);
                    false
                }
            }
        });
        if closed {
            cx.emit(SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(
                session_id,
            ));
        }
    }
```

- [ ] **Step 5: Wire it into the `subscribe_to_session` closure**

In `crates/solution_agent/src/store.rs`, replace the closure body at lines 3086-3095:

```rust
                move |acp_sid: &acp::SessionId,
                      agent_id: Option<&str>,
                      is_end_of_turn: bool,
                      cx: &mut AsyncApp| {
                    weak.update(cx, |store, cx| {
                        let session_id = store.session_id_for_acp(acp_sid, cx)?;
                        let delivered = store.take_pending_for_delivery(
                            session_id, agent_id, is_end_of_turn, cx,
                        );
                        // Authoritative teammate completion: a subagent `Stop`
                        // (end-of-turn + its own agent_id) with nothing left to
                        // deliver means that teammate is idle and done — close
                        // its stream now, not on the JSONL/stale backstop.
                        if delivered.is_none() && is_end_of_turn {
                            if let Some(agent_id) = agent_id {
                                store.close_teammate_on_stop(session_id, agent_id, cx);
                            }
                        }
                        delivered
                    })
                    .ok()
                    .flatten()
                },
```

- [ ] **Step 6: Drain `pending_stop` at registration**

In `crates/solution_agent/src/store/teammate_reconciler.rs`, in `apply_subagent_lifecycle`, the registration `session_entity.update` block (lines 999-1015): capture the parent toolu for reuse before it is moved into the struct, and drain any buffered stop after the insert. Change the block to:

```rust
                    session_entity.update(cx, |s, _| {
                        let parent_toolu_for_pending = parent_toolu.clone();
                        s.background_agents.insert(
                            id_for_insert.clone(),
                            crate::background_agent::BackgroundAgent {
                                id: id_for_insert.clone(),
                                jsonl_path: path_for_insert,
                                registered_at: chrono::Utc::now(),
                                latest: None,
                                last_offset: 0,
                                parent_tool_use_id: Some(parent_toolu),
                                latest_seq: 0,
                                killed: false,
                            },
                        );
                        s.background_agent_order.push(id_for_insert.clone());
                        // A `Stop` hook may have arrived before this registration
                        // (the hook races the `agentId:` announcement). Honor it
                        // now: close the teammate stream immediately.
                        if s.take_pending_stop(&id_for_insert) {
                            s.close_stream(
                                crate::stream::StreamId::Teammate(parent_toolu_for_pending),
                                gpui::SharedString::new_static("done"),
                            );
                        }
                    });
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p solution_agent --lib subagent_stop`
Expected: PASS (both `subagent_stop_hook_closes_teammate_stream` and `subagent_stop_before_registration_buffers_then_closes`).

- [ ] **Step 8: Run the full crate suite (no regressions)**

Run: `cargo test -p solution_agent --lib`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/solution_agent/src/model.rs crates/solution_agent/src/store.rs crates/solution_agent/src/store/teammate_reconciler.rs crates/solution_agent/src/store/tests/teammate_reconciler.rs
git commit -m "solution_agent: Close a teammate stream on its authoritative Stop hook"
```

---

### Task 2: Remove the JSONL tail as a close trigger

**Files:**
- Modify: `crates/solution_agent/src/store/teammate_reconciler.rs` (`refresh_background_agent_snapshot`, the `close_teammate` branch, lines 272-326)
- Test: `crates/solution_agent/src/store/tests/teammate_reconciler.rs`

**Interfaces:**
- Consumes: nothing new. Removes the snapshot-driven `close_stream`; keeps the snapshot content update + `rebuild_streams`.

- [ ] **Step 1: Update the existing test to the new contract**

`background_agent_terminal_closes_teammate_stream` (lines 1210-1294) currently asserts a terminal JSONL closes the stream. That is no longer the close authority — the hook is (Task 1). Rename it and invert its final assertion so it documents the new contract (JSONL terminal updates content but does NOT close):

```rust
#[gpui::test]
async fn background_agent_terminal_does_not_close_teammate_stream(cx: &mut TestAppContext) {
    // ... identical setup to the old test (build jsonl with stop_reason:end_turn,
    // tagged entry, non-terminal BackgroundAgent, call refresh_background_agent_snapshot) ...
    cx.update(|cx| {
        let session = SolutionAgentStore::global(cx).read(cx).session(session_id).unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                s.streams.contains_key(&teammate),
                "JSONL terminal no longer closes the stream — the Stop hook does"
            );
        });
    });
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p solution_agent --lib background_agent_terminal_does_not_close`
Expected: FAIL — the stream is still being closed by the snapshot branch.

- [ ] **Step 3: Remove the snapshot close branch**

In `refresh_background_agent_snapshot` (teammate_reconciler.rs:272-326), delete the `close_teammate` local and the `now_terminal && !was_terminal` close block, keeping the `ba.latest = Some(snap)` content update and the unconditional `rebuild_streams()`. The `if let Some((parent_toolu, reason)) = close_teammate { s.close_stream(...) } else if changed { s.rebuild_streams(); }` tail collapses to just `if changed { s.rebuild_streams(); }`. Keep parsing `stop_reason` into `ba.latest` (it still feeds `is_messageable`/supervisor gating), but it no longer drives a close.

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test -p solution_agent --lib background_agent`
Expected: PASS (the renamed test + the non-terminal twin `background_agent_non_terminal_leaves_teammate_stream`).

- [ ] **Step 5: Full suite + commit**

Run: `cargo test -p solution_agent --lib` → PASS.

```bash
git add crates/solution_agent/src/store/teammate_reconciler.rs crates/solution_agent/src/store/tests/teammate_reconciler.rs
git commit -m "solution_agent: Drop the JSONL-tail close trigger; the Stop hook is authoritative"
```

---

### Task 3: Demote the reaper to a short-window backstop

**Files:**
- Modify: `crates/solution_agent/src/store.rs` (add a managed-agent backstop const; do NOT touch `BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS`)
- Modify: `crates/solution_agent/src/store/teammate_reconciler.rs` (`tick_background_agents` 1075-1088; `reconcile_finished_teammate_streams` 1258+)
- Test: `crates/solution_agent/src/store/tests/teammate_reconciler.rs`

**Interfaces:**
- Consumes: existing reapers. Changes their close condition to: parent-dead (immediate) OR stale-mtime past a short window — never the old 3600 s live-parent cap for a managed agent, and no longer keyed on `latest.stop_reason` as the primary close (that becomes redundant with the hook).

- [ ] **Step 1: Write the failing test**

Assert a managed agent whose parent is alive but which went silent past the SHORT backstop window is reaped (not held for an hour). Mirror `done_agent_removed_on_tick` (lines ~1000) for setup, but set `latest.mtime` to `now - (backstop_window + 1s)` with `stop_reason: None` and a live parent, and assert the teammate stream closed on `tick_background_agents`.

```rust
#[gpui::test]
async fn managed_agent_lost_hook_closes_on_short_backstop(cx: &mut TestAppContext) {
    // setup: create_session_with_thread (live parent), tagged entry → live teammate,
    // BackgroundAgent with latest.stop_reason = None and mtime older than the
    // managed-agent backstop window; call tick_background_agents.
    // assert: !streams.contains_key(&teammate)
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p solution_agent --lib managed_agent_lost_hook`
Expected: FAIL — under a live parent the current code waits `BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS` (3600 s), so the stream is still open.

- [ ] **Step 3: Add the managed-agent backstop const**

In `crates/solution_agent/src/store.rs` near lines 66-67:

```rust
/// Lost-hook / dead-process backstop for a managed background agent. The `Stop`
/// hook closes every normal completion immediately; this only catches a dropped
/// hook or a silently-dead subprocess, so it is short (minutes), NOT the shell
/// live-parent hour cap. Non-zero so a lost hook cannot strand a tab forever.
const MANAGED_AGENT_LOST_HOOK_BACKSTOP_SECS: u64 = MANAGED_AGENT_STALE_TIMEOUT_SECS;
```

- [ ] **Step 4: Use it in both reapers**

In `tick_background_agents` (1075-1088) and `reconcile_finished_teammate_streams` (1258+), replace the `live_parent_cap = BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS` gate for **managed agents** with `MANAGED_AGENT_LOST_HOOK_BACKSTOP_SECS` (a single short staleness window regardless of parent liveness), keeping the immediate close when the parent subprocess is gone (`acp_thread().is_none()` / `killed`). Leave the shell reaper (`tick_background_shells`) and its const untouched. Keep the `latest.stop_reason.is_some()` reap as a harmless secondary condition, or drop it — it is now redundant with the hook; dropping it fully removes JSONL from done-detection (preferred).

- [ ] **Step 5: Run tests to verify pass**

Run: `cargo test -p solution_agent --lib managed_agent_lost_hook`
Expected: PASS. Also run `cargo test -p solution_agent --lib background_shell` — shell behavior unchanged.

- [ ] **Step 6: Full suite + commit**

Run: `cargo test -p solution_agent --lib` → PASS.

```bash
git add crates/solution_agent/src/store.rs crates/solution_agent/src/store/teammate_reconciler.rs crates/solution_agent/src/store/tests/teammate_reconciler.rs
git commit -m "solution_agent: Shorten the managed-agent backstop to a lost-hook safety net"
```

---

### Task 4: Close a killed teammate immediately

**Files:**
- Modify: `crates/solution_agent/src/model.rs` (`mark_background_agents_killed`, lines 710-735)
- Test: `crates/solution_agent/src/store/tests/teammate_reconciler.rs`

**Interfaces:**
- Consumes: `close_stream`, `KILLED_REASON`, `parent_tool_use_id`. On kill, close the teammate stream immediately instead of leaving a lingering `Done { killed }` tab for the reaper.

- [ ] **Step 1: Write the failing test**

```rust
#[gpui::test]
async fn killed_agent_closes_teammate_stream_immediately(cx: &mut TestAppContext) {
    // setup: live teammate stream + a live BackgroundAgent with parent_tool_use_id.
    // action: session.update(|s,_| { s.set_acp_thread(None, cx) })  // drops thread → mark killed
    // assert: !streams.contains_key(&teammate)  (closed, not lingering as Done{killed})
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p solution_agent --lib killed_agent_closes`
Expected: FAIL — the stream re-states to `Done { killed }` and lingers in the map until the reaper.

- [ ] **Step 3: Close on kill**

In `mark_background_agents_killed` (model.rs:710-735), after flipping each agent to `killed = true`, also close its teammate stream via `close_stream(StreamId::Teammate(parent_tool_use_id), KILLED_REASON)` for each killed agent that has a `parent_tool_use_id`, then `rebuild_streams()`. (Collect the `(parent_toolu)` list alongside `to_kill` so the close runs after the `get_mut` borrow ends.)

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test -p solution_agent --lib killed_agent_closes`
Expected: PASS. Also re-run any existing `mark_background_agents_killed` / `seed_cold_session` tests that asserted a `Done { killed }` render — update them to expect a closed stream if they break (the design now closes immediately).

- [ ] **Step 5: Full suite + commit**

Run: `cargo test -p solution_agent --lib` → PASS.

```bash
git add crates/solution_agent/src/model.rs crates/solution_agent/src/store/tests/teammate_reconciler.rs
git commit -m "solution_agent: Close a killed teammate's stream immediately on thread drop"
```

---

### Task 5: End-to-end live verification

**Files:** none (verification only).

- [ ] **Step 1: Build the debug binary**

Run: `cargo build --bin sawe` (debug). Expected: clean build.

- [ ] **Step 2: Launch the MCP editor**

Run: `script/run-mcp --debug --headless` — note the socket path it prints.

- [ ] **Step 3: Drive a background agent and confirm immediate close**

Create a solution session, dispatch a background `Agent` (Agent-tool teammate) that finishes with a trailing tool call (e.g. runs a shell command last — the exact shape that stranded the tab before). Via the per-solution MCP socket, poll `solution_agent.get_session` for that session and confirm the teammate stream leaves `streams` **promptly** after the agent's final message (seconds, not the old ~1 h window). Capture the editor log lines: the `hook pull (agent_id=…, end_of_turn=true …)` for that agent id, immediately followed by the stream disappearing.

- [ ] **Step 4: Record the result**

Append the observed timing + log excerpt to a short finding note `docs/findings/2026-07-14-teammate-completion-signal-shipped.md` and add an INDEX row.

---

## Self-review

- **Spec coverage:** Core (hook→close) = Task 1. Remove JSONL done-detector = Task 2 (+ Task 3 drops the reaper's `stop_reason` reap). Collapse reapers to one short backstop = Task 3. Kill immediate = Task 4. `pending_stop` race = Task 1 Steps 3/6. Cold-load out of scope = unchanged (no task, correct). Live verification = Task 5. All spec sections mapped.
- **Placeholder scan:** Tasks 1 (keystone) fully coded. Tasks 2-4 are deletions / constant swaps / a small close-loop — described against the exact current code and line ranges with the concrete change; test bodies give the setup mirror + the exact assertion. No "TBD"/"handle edge cases".
- **Type consistency:** `close_teammate_on_stop(session_id, agent_id: &str, cx)`, `take_pending_stop(&BackgroundAgentId) -> bool`, `pending_stop: HashSet<BackgroundAgentId>`, `close_stream(StreamId::Teammate(SharedString), SharedString)`, `SessionBackgroundAgentsChanged(SolutionSessionId)` — consistent across tasks.
