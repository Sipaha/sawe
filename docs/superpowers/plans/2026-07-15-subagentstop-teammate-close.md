# Close teammate tabs on SubagentStop — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a completed async-`Agent` teammate tab vanish promptly by wiring Claude Code's `SubagentStop` hook into the existing teammate-close path.

**Architecture:** One-file change in `crates/claude_native/src/connection.rs`: register the `SubagentStop` hook, treat its callback as end-of-turn (so the existing store pull closure calls `close_teammate_on_stop` with the sub-agent's `agent_id`), and give it the correct response event name without blocking. `solution_agent` is unchanged.

**Tech Stack:** Rust, Claude SDK control-protocol hooks, GPUI.

## Global Constraints

- Fix lives ONLY in `crates/claude_native/src/connection.rs`. `solution_agent` (`close_teammate_on_stop`, `rebuild_streams`, reaper) is NOT modified — the reaper stays as the lost-hook backstop.
- `decision: "block"` on a hook response stays gated to the main `Stop` callback ONLY (`callback_id == HOOK_CALLBACK_STOP`). A finished sub-agent must NOT be forced to keep generating.
- `SubagentStop`'s response `hookEventName` must be `"SubagentStop"`; main `Stop` stays `"Stop"`; everything else `"PostToolUse"`.
- No `Co-Authored-By` trailer. Imperative commit subjects. Direct-to-`main` is fine (solo repo); pushing pre-authorized.
- Debug builds for verification: `cargo build --bin sawe` / `cargo test -p claude_native --lib` (no `--release`). `set -o pipefail` when piping cargo. `claude_native` is smaller than `solution_agent` but a build can still take minutes — use `timeout: 600000`.

---

### Task 1: Register `SubagentStop` and route it as end-of-turn

**Files:**
- Modify: `crates/claude_native/src/connection.rs` — const (~`:63`), `build_default_hooks` (`:375-393`), the `HookCallback` handler `is_end_of_turn` (`:1530`), `build_hook_response` (`:410-448`)
- Test: `crates/claude_native/src/connection.rs` (`#[cfg(test)] mod tests`, ~`:2282`)

**Interfaces:**
- Produces: `const HOOK_CALLBACK_SUBAGENT_STOP: &str = "sub_stop"`. `build_default_hooks` now contains a `"SubagentStop"` key. `build_hook_response` maps the SubagentStop callback to `hookEventName "SubagentStop"` with no `decision`.

- [ ] **Step 1: Write the failing tests** — add to `mod tests` (after `build_default_hooks_registers_post_tool_use_and_stop`, ~`:2290`, and after `hook_response_stop_blocks_with_reason`, ~`:2338`):

```rust
    #[test]
    fn build_default_hooks_registers_subagent_stop() {
        let hooks = build_default_hooks();
        let sub = hooks.get("SubagentStop").expect("SubagentStop registered");
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].hook_callback_ids, vec![HOOK_CALLBACK_SUBAGENT_STOP.to_string()]);
    }

    #[test]
    fn hook_response_subagent_stop_sets_event_name_and_does_not_block() {
        // A SubagentStop response must name the event correctly but must NOT
        // carry decision:block — a finished sub-agent is not forced to continue.
        let response = build_hook_response("hk3", HOOK_CALLBACK_SUBAGENT_STOP, Some("FOLLOWUP".to_string()));
        let inner = &response["response"];
        assert_eq!(inner["hookSpecificOutput"]["hookEventName"], "SubagentStop");
        assert!(inner.get("decision").is_none(), "sub-agent stop must not block");
        assert!(inner.get("reason").is_none());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p claude_native --lib subagent_stop 2>&1 | tail -20`
Expected: FAIL — `cannot find value HOOK_CALLBACK_SUBAGENT_STOP`; and (once the const exists) `hookEventName` would be `"PostToolUse"` for the SubagentStop callback.

- [ ] **Step 3: Add the callback const** — after `HOOK_CALLBACK_STOP` (`connection.rs:63`):

```rust
/// Stable id for the `SubagentStop` hook callback. Claude Code fires
/// `SubagentStop` (not `Stop`) when a sub-agent finishes; its input carries the
/// sub-agent's `agent_id`, so routing it as end-of-turn drives the same
/// `close_teammate_on_stop` path that closes the teammate's tab.
const HOOK_CALLBACK_SUBAGENT_STOP: &str = "sub_stop";
```

- [ ] **Step 4: Register the hook** — in `build_default_hooks`, insert after the `"Stop"` entry, before `hooks` is returned:

```rust
    hooks.insert(
        "SubagentStop".to_string(),
        vec![HookConfig {
            matcher: None,
            hook_callback_ids: vec![HOOK_CALLBACK_SUBAGENT_STOP.to_string()],
            timeout: 30_000,
        }],
    );
```

- [ ] **Step 5: Treat the SubagentStop callback as end-of-turn** — in the `HookCallback` handler, replace the `is_end_of_turn` line (`connection.rs:1530`):

```rust
                    let is_end_of_turn = matches!(
                        callback_id.as_str(),
                        HOOK_CALLBACK_STOP | HOOK_CALLBACK_SUBAGENT_STOP
                    );
```

(No other handler change: `agent_id` is already read from `input["agent_id"]` and forwarded to the store pull, which closes the teammate; the degenerate-nudge branch stays guarded by `agent_id.is_none()`.)

- [ ] **Step 6: Map the response event name; keep block main-Stop-only** — in `build_hook_response`, replace the `is_stop`/`event_name` lines and the block guard so `is_stop` governs ONLY the block, and a separate 3-way governs the event name:

```rust
    let is_stop = callback_id == HOOK_CALLBACK_STOP;
    let event_name = match callback_id {
        HOOK_CALLBACK_STOP => "Stop",
        HOOK_CALLBACK_SUBAGENT_STOP => "SubagentStop",
        _ => "PostToolUse",
    };
```

Leave the rest of the function unchanged: `response` uses `event_name`, and the existing `if is_stop { … decision:block … reason … }` block now fires only for the main `Stop` (SubagentStop has `is_stop == false`).

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p claude_native --lib 2>&1 | tail -15`
Expected: PASS — the two new tests plus the existing `build_default_hooks_registers_post_tool_use_and_stop`, `hook_response_post_tool_use_carries_additional_context`, `hook_response_stop_blocks_with_reason` (unchanged behavior for Stop / PostToolUse).

- [ ] **Step 8: Build the binary**

Run: `set -o pipefail; cargo build --bin sawe 2>&1 | grep -E "^error" | head; echo "exit ${PIPESTATUS[0]}"`
Expected: no `error` lines; exit 0. (Builds the debug binary for Task 2's live run.)

- [ ] **Step 9: Commit**

```bash
git add crates/claude_native/src/connection.rs
git commit -m "claude_native: Close teammate tabs on SubagentStop hook"
```

---

### Task 2: Live verification — teammate tab closes on completion (dev headless)

**Files:** none (verification only; if it fails, fix forward into Task 1's commit).

- [ ] **Step 1: Launch the dev editor headless**

```bash
rm -f ~/.spk/sawe-dev/state/mcp.lock ~/.spk/sawe-dev/state/mcp.sock
cd /home/spk/.spk/sawe/ss/Sawe1/sawe
nohup script/run-mcp --debug --headless >/tmp/devmcp.out 2>&1 &
sleep 22; tail -4 /tmp/devmcp.out
```
Expected: `mcp socket ready: …/sawe-dev/state/mcp.sock`. (Confirm `editor.capabilities` `binary_built_at` is fresh, i.e. includes Task 1.)

- [ ] **Step 2: Create a scratch solution + session and dispatch one async `Agent`**

Using `/tmp/mcpcall.py <global-dev-sock> <tool> '<json>'`:
```
solutions.create           {"name":"SubStopFix"}                    → sid
solutions.add_empty_member {"solution_id":sid,"name":"m1"}
solutions.open             {"solution_id":sid}                      → window_id
solution_agent.create_session {"solution_id":sid,"agent_id":"claude-acp","title":"t"}  → session_id
solution_agent.send_message   {"session_id":session_id,"content":"Dispatch exactly ONE background sub-agent (use the Agent tool) to run the shell command: echo SUBAGENT_FIX_OK — and report its output. Do only that, then stop."}
```

- [ ] **Step 3: Confirm the teammate pill appears, then vanishes on completion**

While the sub-agent runs: reveal the console (`windows.dispatch_action console_panel::ToggleFocus`), `workspace.screenshot {solution_id:sid}` via the per-solution socket → expect a teammate pill in the strip. After the sub-agent finishes (poll the dev log for the `SubagentStop` hook callback and a teammate close, or wait ~15 s): screenshot again → the teammate pill is **gone** (only `Main` remains), promptly (not after 1 h). Read both PNGs to confirm.

Cross-check the dev log (`~/.spk/sawe-dev/logs/sawe.log`): a `hook pull (agent_id=Some(...), end_of_turn=true)` now appears for the sub-agent (previously only `end_of_turn=false`), and NO 3600 s reaper wait was needed.

- [ ] **Step 4: Cover the detached-background case**

Repeat with a prompt that dispatches a background agent doing a slightly longer task (e.g. `sleep 5 && echo LATE_OK`) and let the parent turn go idle before the agent finishes. Confirm the teammate pill still vanishes shortly after the agent completes (SubagentStop delivered post-idle). If it does NOT close in this case, STOP and report — the fix may need the reaper or a post-idle drain; do not silently accept.

- [ ] **Step 5: Tear down**

```bash
python3 /tmp/mcpcall.py ~/.spk/sawe-dev/state/mcp.sock solutions.delete '{"solution_id":sid}'
kill "$(pgrep -f 'target/debug/sawe --headless' | grep -v crash | head -1)" 2>/dev/null
rm -f ~/.spk/sawe-dev/state/mcp.lock ~/.spk/sawe-dev/state/mcp.sock
rm -rf ~/.spk/sawe-dev/ss/substopfix
```

- [ ] **Step 6 (hand-off): build release-fast**

When handing back for the user to hands-on confirm on their real editor: `cargo build --bin sawe --profile release-fast` (background), then they RESTART into `target/release-fast/sawe`.

---

## Notes for the implementer

- Do NOT touch `solution_agent`. The close is entirely driven by `is_end_of_turn && agent_id` reaching the existing store pull (`store.rs:3138-3140`).
- `FORK.md`: `crates/claude_native` is already a listed fork-touched file — no new row needed. No rebrand identifier, no wire/DB change.
- If Task 2 Step 4 (detached case) fails, that's a real finding, not a plan defect — surface it; the fix scope may widen (documented as a risk in the spec).
