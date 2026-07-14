# Shell-tab command/output/tooltip/resize Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In the AI session view's background-shell tabs, show the full command (un-truncated) as a readable block plus the output-file path, add a full-command tooltip on the pill, and keep the compose region resizable even when the input is locked.

**Architecture:** Four focused changes in `solution_agent`: (1) lift the 120-char command capture cap behind a testable pure helper; (2) reformat the shell stream's content entry so the command is its own fenced block and the metadata line carries the output path; (3) feed the pill's existing tooltip the full command; (4) hoist the compose resize handle into a shared helper rendered in both the enabled and disabled compose arms.

**Tech Stack:** Rust, GPUI, the `solution_agent` crate.

## Global Constraints

- Crate: `solution_agent`. No wire-schema or DB-schema change (the `command` column already stores whatever we capture; we only lift its length cap).
- `COMMAND_CAP = 4096` chars — a safety guard against pathological megabyte inputs, NOT a layout cap.
- `BackgroundShell::stream_entry` MUST stay `cx`-free (pure data → `SessionEntry`).
- The shell input stays DISABLED on shell tabs; only the region's resizability is restored.
- No `Co-Authored-By` trailer in commits. Imperative commit subjects. Direct-to-`main` is fine (solo repo); pushing is pre-authorized.
- Debug builds only for agent verification: `cargo build --bin sawe` (no `--release`); tests `cargo test -p solution_agent --lib …` (no `--release`). Use `set -o pipefail` if you pipe cargo output.

---

### Task 1: Lift the 120-char command capture cap

**Files:**
- Modify: `crates/solution_agent/src/background_shell.rs` (add helper + const near `stream_label`, ~line 124)
- Modify: `crates/solution_agent/src/store/teammate_reconciler.rs:763-776` (call the helper)
- Test: `crates/solution_agent/src/background_shell.rs` (unit tests module, ~line 630)

**Interfaces:**
- Produces: `pub const COMMAND_CAP: usize` and `pub fn command_label_from_raw_input(raw_input: &serde_json::Value) -> SharedString` in `background_shell.rs`.

- [ ] **Step 1: Write the failing tests** — add to the `#[cfg(test)] mod tests` in `background_shell.rs` (after `stream_label_keeps_short_command`, ~line 645):

```rust
#[test]
fn command_label_prefers_command_then_description() {
    let v = serde_json::json!({"command": "ls -la", "description": "listing"});
    assert_eq!(command_label_from_raw_input(&v).as_ref(), "ls -la");
    let v = serde_json::json!({"description": "listing"});
    assert_eq!(command_label_from_raw_input(&v).as_ref(), "listing");
    let v = serde_json::json!({});
    assert_eq!(command_label_from_raw_input(&v).as_ref(), "");
}

#[test]
fn command_label_keeps_commands_longer_than_120_chars() {
    // Old behaviour truncated at 120; a 205-char command must now survive whole.
    let long = format!("echo {}", "x".repeat(200));
    let v = serde_json::json!({ "command": long });
    let out = command_label_from_raw_input(&v);
    assert_eq!(out.chars().count(), 205);
    assert!(!out.ends_with('…'));
}

#[test]
fn command_label_caps_pathological_command() {
    let huge = "a".repeat(COMMAND_CAP + 500);
    let v = serde_json::json!({ "command": huge });
    let out = command_label_from_raw_input(&v);
    assert_eq!(out.chars().count(), COMMAND_CAP + 1); // COMMAND_CAP chars + ellipsis
    assert!(out.ends_with('…'));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p solution_agent --lib background_shell::tests::command_label 2>&1 | tail -20`
Expected: FAIL — `cannot find function command_label_from_raw_input` / `cannot find value COMMAND_CAP`.

- [ ] **Step 3: Add the const + helper** — in `background_shell.rs`, immediately after the `stream_label` method's closing brace (~line 124, inside `impl BackgroundShell`'s enclosing module scope but as free items; place them just below the `impl BackgroundShell { … }` block that ends near line 179, or as associated items — put them as FREE functions right after the `impl BackgroundShell` block so they're `cx`-free and importable):

```rust
/// Cap on the command string captured for a background shell. Generous: the
/// pill label truncates to 24 chars for the strip and the tab content renders
/// the command as its own fenced block, so display never depends on this being
/// short — it only guards against a pathological multi-megabyte
/// `raw_input.command`.
pub const COMMAND_CAP: usize = 4096;

/// Extract a background shell's launch command from a Bash tool call's
/// `raw_input`: prefer `command`, fall back to `description`. Capped at
/// [`COMMAND_CAP`] chars (ellipsis suffix on overflow). Empty `SharedString`
/// when neither key holds a non-empty string.
pub fn command_label_from_raw_input(raw_input: &serde_json::Value) -> SharedString {
    let picked = raw_input
        .get("command")
        .or_else(|| raw_input.get("description"))
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    if picked.chars().count() > COMMAND_CAP {
        let truncated: String = picked.chars().take(COMMAND_CAP).collect();
        SharedString::from(format!("{truncated}…"))
    } else {
        SharedString::from(picked.to_string())
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p solution_agent --lib background_shell::tests::command_label 2>&1 | tail -20`
Expected: PASS (3 tests).

- [ ] **Step 5: Wire the reconciler to the helper** — in `teammate_reconciler.rs`, replace the `command_label` computation at lines 766-776:

```rust
                    // Command label: prefer `raw_input.command`, fall back to
                    // `raw_input.description`; capped generously so a long
                    // pipeline is stored whole (only pathological inputs clip).
                    let command_label: SharedString = snapshot
                        .raw_input
                        .as_ref()
                        .map(crate::background_shell::command_label_from_raw_input)
                        .unwrap_or_default();
```

- [ ] **Step 6: Build to verify the wiring compiles**

Run: `set -o pipefail; cargo build -p solution_agent --lib 2>&1 | grep -E "^error|warning: unused" | head; echo "exit ${PIPESTATUS[0]}"`
Expected: no `error` lines; exit 0.

- [ ] **Step 7: Commit**

```bash
git add crates/solution_agent/src/background_shell.rs crates/solution_agent/src/store/teammate_reconciler.rs
git commit -m "solution_agent: Capture background-shell command in full (lift 120-char cap)"
```

---

### Task 2: Render the full command as a fenced block + output path in metadata

**Files:**
- Modify: `crates/solution_agent/src/background_shell.rs:139-178` (`stream_entry`)
- Test: `crates/solution_agent/src/background_shell.rs:648-664` (update the existing snapshot test)

**Interfaces:**
- Consumes: `BackgroundShell.command`, `.output_path`, `.state`, `.latest`, `.id` (unchanged fields).
- Produces: the `stream_entry` `text` now = `"{metadata_line}\n\n```\n{command}\n```\n\n{body}"` where `metadata_line = "{state} · {observed} · id: {short_id} · out: {output_path}"`.

- [ ] **Step 1: Update the failing test** — replace the body of `stream_entry_with_snapshot_fences_output_and_derives_seq_from_mtime` (lines 648-664) so it asserts the new format:

```rust
    #[test]
    fn stream_entry_with_snapshot_fences_output_and_derives_seq_from_mtime() {
        let shell = running_shell("echo hi", Some("hello\nworld\n"));
        let entry = shell.stream_entry(chrono::Utc::now());
        assert!(entry.subagent_id.is_none());
        assert_eq!(entry.mod_seq, 1_720_000_000_000);
        assert_eq!(entry.created_ms, 1_720_000_000_000);
        let SessionEntryKind::AssistantMessage { chunks } = &entry.kind else {
            panic!("expected AssistantMessage");
        };
        let AssistantChunk::Message(text) = &chunks[0] else {
            panic!("expected a plain Message chunk");
        };
        // Output tail still fenced.
        assert!(text.contains("```\nhello\nworld\n\n```"), "output: {text}");
        // Command is now its OWN fenced block, not an inline `code` span.
        assert!(text.contains("```\necho hi\n```"), "command block: {text}");
        assert!(!text.contains("`echo hi`"), "command must not be inline: {text}");
        // Metadata line carries id + output path.
        assert!(
            text.contains("id: bvb4ful1z · out: /tmp/claude/tasks/bvb4ful1z.output"),
            "metadata: {text}"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p solution_agent --lib background_shell::tests::stream_entry_with_snapshot 2>&1 | tail -20`
Expected: FAIL — the old `stream_entry` still emits `` `echo hi` `` inline and no `out:` path.

- [ ] **Step 3: Rewrite `stream_entry`'s header/body assembly** — in `background_shell.rs`, replace lines 153-164 (`let header = …` through `let text = …`):

```rust
        let header = format!(
            "{} · {} · id: {} · out: {}",
            state_label,
            observed,
            self.id.short(),
            self.output_path.display(),
        );
        let command_block = format!("```\n{}\n```", self.command);
        let body = match &self.latest {
            Some(snapshot) => format!("```\n{}\n```", snapshot.output_tail),
            None => "_No output captured yet._".to_string(),
        };
        let text = format!("{header}\n\n{command_block}\n\n{body}");
```

- [ ] **Step 4: Run the full `stream_entry` + `stream_label` test set**

Run: `cargo test -p solution_agent --lib background_shell::tests::stream 2>&1 | tail -20`
Expected: PASS — `stream_entry_with_snapshot_…`, `stream_entry_exited_state_label_carries_exit_code`, `stream_entry_without_snapshot_…`, and both `stream_label_…` tests green (the exited/no-snapshot tests still find their `exited (137)` / `_No output captured yet._` / `running (stale)` substrings on the metadata line and body).

- [ ] **Step 5: Commit**

```bash
git add crates/solution_agent/src/background_shell.rs
git commit -m "solution_agent: Show shell command as its own block + output path in the tab"
```

---

### Task 3: Full-command tooltip on the shell pill

**Files:**
- Modify: `crates/solution_agent/src/session_view/task_subagent_strip.rs:79-86` (shell_streams builder), `:154-171` (shell loop), `:293-342` (`shell_pill`)

**Interfaces:**
- Consumes: `session_ref.background_shells: HashMap<BackgroundShellId, BackgroundShell>` (pub, `model.rs:539`), `stream.label`.
- Produces: `shell_pill` gains a `full_command: SharedString` param used as its tooltip source.

- [ ] **Step 1: Thread the full command through the strip builder** — change `shell_streams`' element type (line 79) and its `filter_map` (82-85):

```rust
    let shell_streams: Vec<(BackgroundShellId, SharedString, SharedString)> = session_ref
        .streams
        .iter()
        .filter_map(|(id, stream)| match id {
            crate::stream::StreamId::Shell(bsid) => {
                // Full command for the hover tooltip; fall back to the short
                // strip label when the shell row is somehow absent.
                let full_command = session_ref
                    .background_shells
                    .get(bsid)
                    .map(|sh| sh.command.clone())
                    .unwrap_or_else(|| stream.label.clone());
                Some((bsid.clone(), stream.label.clone(), full_command))
            }
            _ => None,
        })
        .collect();
```

- [ ] **Step 2: Pass the command into `shell_pill`** — update the shell loop (lines 154-171) destructure + call:

```rust
    for (id, label, full_command) in shell_streams {
        let is_active = matches!(&selected, StreamId::Shell(s) if s == &id);
        let id_for_listener = id.clone();
        let pill_id = SharedString::from(format!("task-subagent-strip-shell-{}", id));
        row = row.child(shell_pill(
            pill_id,
            label,
            full_command,
            is_active,
            cx,
            move |this, _, _, cx| {
                let next = StreamId::Shell(id_for_listener.clone());
                if this.selected_stream != next {
                    this.selected_stream = next;
                    cx.notify();
                }
            },
        ));
    }
```

- [ ] **Step 3: Use the command as the pill tooltip** — update `shell_pill` (line 293) signature + tooltip source. Add the param after `label` (line 295) and replace the `tooltip_text` line (314):

```rust
fn shell_pill<F>(
    id: SharedString,
    label: SharedString,
    full_command: SharedString,
    is_active: bool,
    cx: &mut Context<SolutionSessionView>,
    on_click: F,
) -> AnyElement
```

and replace line 314:

```rust
    // Hover shows the FULL command (the visible label is truncated to fit the
    // narrow strip). Empty command falls back to the label so the tooltip is
    // never blank.
    let tooltip_text = if full_command.is_empty() {
        label.clone()
    } else {
        full_command
    };
```

- [ ] **Step 4: Build**

Run: `set -o pipefail; cargo build -p solution_agent --lib 2>&1 | grep -E "^error" | head; echo "exit ${PIPESTATUS[0]}"`
Expected: no `error` lines; exit 0. (Visual verification of the tooltip happens in Task 5.)

- [ ] **Step 5: Commit**

```bash
git add crates/solution_agent/src/session_view/task_subagent_strip.rs
git commit -m "solution_agent: Show the full shell command in the pill tooltip"
```

---

### Task 4: Keep the resize handle when the compose input is locked

**Files:**
- Modify: `crates/solution_agent/src/session_view.rs:1726-1796` (disabled arm + extract handle), and the enabled arm's first child (~1769-1796)

**Interfaces:**
- Produces: `fn render_compose_resize_handle(&self, cx: &mut Context<Self>) -> gpui::Div` (or `impl IntoElement`) on `SolutionSessionView`, reused by both compose arms.

- [ ] **Step 1: Extract the resize handle into a helper** — add this method to the `impl SolutionSessionView` block that contains `render` (place it near the other private render helpers). It is the verbatim handle element from lines 1770-1795:

```rust
    /// The 3px drag strip that resizes the compose region. Shared by the
    /// enabled and disabled compose arms so a locked shell tab is still
    /// resizable (the input stays disabled; only `compose_height` changes).
    fn render_compose_resize_handle(&self, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .id("solution-session-compose-resize")
            .flex_none()
            .h(px(3.0))
            .w_full()
            .cursor_row_resize()
            .bg(cx.theme().colors().border)
            .hover(|s| s.bg(cx.theme().colors().border_focused))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    this.resize_start_y = e.position.y;
                    this.resize_start_height = this.compose_height;
                    cx.stop_propagation();
                }),
            )
            .on_drag(DraggedComposeHandle, |handle, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| handle.clone())
            })
    }
```

- [ ] **Step 2: Use the helper in the enabled arm** — replace the inline handle `div()…` that is the first child of `compose_row` (lines 1769-1796) with:

```rust
                let compose_row = div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .h(self.compose_height + px(3.0))
                    .child(self.render_compose_resize_handle(cx));
```

- [ ] **Step 3: Restructure the disabled arm to include the handle** — replace the disabled-arm `h_flex()…into_any_element()` block (lines 1735-1755) with a `flex_col` carrying the handle then the label row:

```rust
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    // Same total height as the enabled arm (handle 3px +
                    // compose_height) so switching Main↔shell doesn't jump.
                    .h(self.compose_height + px(3.0))
                    .child(self.render_compose_resize_handle(cx))
                    .child(
                        h_flex()
                            .id("compose-row-disabled")
                            .w_full()
                            .flex_none()
                            .h(self.compose_height)
                            .px_3()
                            .items_center()
                            .bg(cx.theme().colors().panel_background)
                            .border_t_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                Label::new("View only · switch to Main to send")
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            ),
                    )
                    .into_any_element()
```

- [ ] **Step 4: Build**

Run: `set -o pipefail; cargo build --bin sawe 2>&1 | grep -E "^error" | head; echo "exit ${PIPESTATUS[0]}"`
Expected: no `error` lines; exit 0. (This builds the whole binary for Task 5's MCP run.)

- [ ] **Step 5: Commit**

```bash
git add crates/solution_agent/src/session_view.rs
git commit -m "solution_agent: Keep the compose resize handle on locked shell tabs"
```

---

### Task 5: End-to-end verification in a live editor (dev headless MCP)

**Files:** none (verification only; fix-forward into the relevant task's commit if something fails).

- [ ] **Step 1: Run the full crate test suite**

Run: `cargo test -p solution_agent --lib 2>&1 | tail -15`
Expected: all tests pass (watch for any other `stream_entry`/strip snapshot test the earlier tasks might touch).

- [ ] **Step 2: Launch the dev editor headless**

```bash
rm -f ~/.spk/sawe-dev/state/mcp.lock ~/.spk/sawe-dev/state/mcp.sock
cd /home/spk/.spk/sawe/ss/Sawe1/sawe
nohup script/run-mcp --debug --headless >/tmp/devmcp.out 2>&1 &
sleep 22; tail -4 /tmp/devmcp.out
```
Expected: `mcp socket ready: …/sawe-dev/state/mcp.sock`.

- [ ] **Step 3: Paint a shell stream and screenshot the tab content**

Set up a scratch solution with a member and paint a shell via the debug seed tool (`solution_agent.seed_cold_session`, which has a `live_shell: Option<String>` param — `mcp/debug.rs:59` — that registers one Running background shell whose `command` is the passed string). Using `/tmp/mcpcall.py <global-dev-sock> <tool> '<json>'`:

```
solutions.create           {"name":"ShellTabRepro"}                 → sid
solutions.add_empty_member {"solution_id":sid,"name":"m1"}
solutions.open             {"solution_id":sid}                      → window_id
solution_agent.seed_cold_session {"solution_id":sid,"title":"shelltest","entries":[],
  "live_shell":"cd /home/spk/x && ./configure --with-alpha --with-beta --with-gamma --with-delta --prefix=/opt/thing && make -j8 && ./run --flag"}
```

(the `live_shell` string is deliberately >120 chars). Then on the session view:
- `windows.dispatch_action console_panel::ToggleFocus`, select the shell pill.
- `workspace.screenshot {solution_id:sid, format:"png"}` via the per-solution socket (`…/state/solutions/<sid>/mcp.sock`), decode the base64 image block, Read the PNG.

Expected in the PNG: the command appears **in full** as a fenced block (not truncated at 120, not glued to metadata), and a metadata line reads `… · id: <short> · out: /tmp/sawe-seed-shell.output`. (Note: the seed path sets `command` directly, so it does NOT exercise Task 1's cap-lift — that is covered by Task 1's unit tests; this step verifies Task 2's rendering.) Delete the scratch solution (`solutions.delete`) in Step 6.

- [ ] **Step 4: Verify the pill tooltip**

`windows.hover_id` the shell pill, then `workspace.screenshot`.
Expected: tooltip shows the FULL command (not the truncated `<id>·cd …` label).

- [ ] **Step 5: Verify the resize handle on the locked shell tab**

With the shell pill selected, `workspace.screenshot` and confirm the 3px handle strip sits above the "View only · switch to Main to send" row. Then drive a drag (`windows.click_at`/drag on the handle coords, or dispatch a mouse-down+move) and screenshot again.
Expected: the compose region height changes; the input stays disabled (still shows "View only …", no cursor). Read both PNGs with the Read tool to confirm.

- [ ] **Step 6: Tear down**

```bash
kill "$(pgrep -f 'target/debug/sawe --headless' | grep -v crash | head -1)" 2>/dev/null
rm -f ~/.spk/sawe-dev/state/mcp.lock ~/.spk/sawe-dev/state/mcp.sock
```

- [ ] **Step 7 (if the user will hand-test): build release-fast**

Only when handing back for hands-on use: `cargo build --bin sawe --profile release-fast` (~5 min, background). The user's running editor is `target/release-fast/sawe` on `~/.spk/sawe/state/mcp.sock`.

---

## Notes for the implementer

- `background_shells` is `pub` on `SolutionSession` (`model.rs:539`), reachable via `session.read(cx)` in the strip.
- Do NOT touch the output-tail display (already correct) or the pill label format (`CMD_CAP = 24` is intentional for the narrow strip).
- FORK.md / `.rules`: no new crate, no rebrand identifier, no wire/DB schema change — nothing to record there. If any `.rules`-worthy trap surfaces, propose it in the PR/handoff, don't edit `.rules` inline.
