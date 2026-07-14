# Shell-tab: full command, output path, pill tooltip, and resize-handle-when-locked

Date: 2026-07-15
Status: approved (user), pending implementation plan
Crate: `solution_agent`

## Problem

Two user-reported bugs in the AI session view's **background-shell tabs** (the
`StreamId::Shell` streams shown as pills in the bottom strip, `Main` + one pill
per background shell):

1. **The command is not shown in full.** Two independent causes:
   - At registration the command is capped: `teammate_reconciler.rs:774`
     captures it with `.chars().take(120)` and persists that truncated string to
     SQLite (`teammate_reconciler.rs:810`). A command longer than 120 chars
     loses its tail permanently — it is never stored.
   - The shell tab's content header (`background_shell.rs::stream_entry`, ~line
     153) formats the command and its metadata into a single inline-code span:
     `` `{command}` · {state} · {observed} · {id} ``. For a multi-line command
     the metadata glues onto the last line (`ls -la · running (stale) · … ·
     bwo312mcw` in the report), and the whole thing reads as one cramped inline
     span rather than a readable block.

2. **No resize handle when the input is locked.** For a shell tab
   `compose_disabled_for(view)` returns `true` (`session_view.rs:979-981`), so
   the compose region renders the *disabled arm* of the `if
   self.compose_disabled(cx)` conditional (`session_view.rs:1726`). The disabled
   arm (1735-1755) is a static "View only · switch to Main to send" label row
   with no drag handle. The 3px drag-to-resize handle lives ONLY in the enabled
   arm (`session_view.rs:1770-1795`), so it vanishes on shell tabs. The input
   *should* stay locked, but the region must still be resizable.

### What we do NOT have: an OS PID

The Claude Code background-launch announcement carries only a task id and an
output path:

```
Command running in background with ID: bwo312mcw. Output is being written to: /tmp/.../tasks/bwo312mcw.output. You will be notified when it completes.
```

`parse_bash_bg_launch` (`background_shell.rs:223`) extracts exactly those two
fields; there is no OS PID anywhere in the announcement, the
`<task-notification>` completion block, or the `BackgroundShell` model (grep for
`pid` in `solution_agent` is empty). `bwo312mcw` is Claude Code's internal shell
id (the handle `BashOutput`/`KillShell` use), not a process id. So a "PID" field
is out of scope — instead we surface the **output-file path**, which we do have
(`BackgroundShell.output_path`) and which is genuinely useful (`tail -f`).

### What already works

The shell tab body already shows the captured output: `stream_entry` renders the
snapshot's `output_tail` (last ≤64 KiB, `OUTPUT_TAIL_CAP`) as a fenced block
(`background_shell.rs:160-163`). The report's "No output captured yet" is just
the pre-first-snapshot state (`latest = None`). No change needed to output
display.

## Design

### Fix 1 — store the full command (lift the 120 cap)

`teammate_reconciler.rs:774`: replace `.take(120)` with a generous safety cap of
**4096** chars. Rationale: the pill label already truncates to 24 chars for the
narrow strip (`BackgroundShell::stream_label`, `CMD_CAP = 24`) and the content
header now renders the command as a fenced block, so display never depends on
the stored length being short; the cap exists only to guard against a
pathological multi-megabyte `raw_input.command`. The same lifted value flows to
both the in-memory `command` and the persisted `BackgroundShellRow.command`
(line 810) — no schema change; existing shorter rows are unaffected.

### Fix 2 — readable command + output path in the tab content

Rework `BackgroundShell::stream_entry` (`background_shell.rs:139-178`) so the
`text` becomes:

```
{state_label} · {observed} · id: {short_id} · out: {output_path}

```<newline>{command}<newline>```

{body}
```

- Metadata (state, observed, id, **output path**) on its own header line — no
  longer glued to the command's last line.
- The command rendered as its **own fenced code block**, full and multi-line.
- `{body}` unchanged: the fenced `output_tail`, or `_No output captured yet._`.

`output_path` is rendered via `self.output_path.display()`. Keep it on the
metadata line (not a fence) so it stays a single glanceable line.

This method is `cx`-free and already unit-tested; update/extend its tests.

### Fix 3 — full-command tooltip on the shell pill

In `shell_pill<F>()` (`task_subagent_strip.rs:293-342`), attach a tooltip to the
pill showing the shell's **full** command (`shell.command`, now un-truncated).
The pill label stays the short `stream_label` form. Use the standard GPUI
`.tooltip(Tooltip::text(full_command))` pattern (match how other pills/buttons in
this crate set tooltips). Threading: the `shell_streams` builder
(`task_subagent_strip.rs:79-86`) already reads `session_ref`, so extend its tuple
from `(BackgroundShellId, label)` to `(BackgroundShellId, label, full_command)`
by looking up `session_ref.background_shells.get(bsid).map(|sh|
sh.command.clone())` (fall back to the label when the shell row is somehow
absent). Pass the third element into `shell_pill` as the tooltip source. The
visible pill label stays the short `stream_label` form.

### Fix 4 — resize handle in both compose arms

Extract the 3px drag handle (`session_view.rs:1770-1795`) into a private helper
`fn render_compose_resize_handle(&self, cx) -> impl IntoElement` (same `id =
"solution-session-compose-resize"`, `h(px(3.0))`, `cursor_row_resize`,
`on_mouse_down` capturing `resize_start_y`/`resize_start_height`, and
`on_drag(DraggedComposeHandle, …)`). Then:

- Enabled arm: first child of `compose_row` becomes
  `self.render_compose_resize_handle(cx)` (behaviour identical to today).
- Disabled arm (`session_view.rs:1735-1755`): restructure from a single `h_flex`
  into a `flex_col` whose first child is `self.render_compose_resize_handle(cx)`
  and whose second child is the existing "View only · switch to Main to send"
  label row at height `self.compose_height`. Total height stays `compose_height +
  px(3.0)` (unchanged, so no vertical jump on pill switch).

The outer `on_drag_move` handler (`session_view.rs:1320-1336`) is registered on
the outer render regardless of arm and mutates `compose_height`, so dragging
works in the disabled arm with no further wiring. The input stays disabled; only
the region height changes. `compose_height` is shared state, so a height set on a
shell tab persists when flipping back to Main.

## Isolation / boundaries

- `stream_entry` stays a `cx`-free pure data normalizer (input: `&self`, `now`;
  output: `SessionEntry`) — testable without GPUI.
- `render_compose_resize_handle` is a pure element factory depending only on
  `self.compose_height` / theme / the drag marker — one purpose, reused by both
  arms so they can't diverge again.
- Tooltip threading adds one owned `SharedString` argument to `shell_pill`; no
  new coupling.

## Testing

- **Unit (`background_shell.rs`):**
  - `stream_entry` header format: metadata line contains state/observed/`id:`/`out:
    <path>`; command appears as its own fenced block; multi-line command is not
    glued to metadata. Extend the existing `stream_entry` tests.
  - 120→4096 cap: registration keeps a >120-char command intact (add a
    reconciler-level or helper test asserting the stored `command` length).
- **Unit (`task_subagent_strip.rs`):** shell pill carries the full command as its
  tooltip source (assert on the value passed in, since tooltip rendering is
  interaction-driven).
- **MCP screenshot verification (dev headless):**
  - Seed a shell stream (the `seed_cold_session` / `live_shell` debug path exists
    — `store.rs:2841`) with a long multi-line command; screenshot the shell tab
    content → command shown in full as a block + `out:` path in the metadata.
  - Screenshot a shell tab's compose region → resize handle present; drive a drag
    via `windows` MCP and confirm height changes while input stays disabled.
  - Hover the shell pill (`windows.hover_id`) → screenshot shows the full-command
    tooltip.

## Out of scope

- OS PID (unavailable — see above).
- Output-tail display (already implemented).
- Pill label format / `CMD_CAP` (intentionally short for the narrow strip).
