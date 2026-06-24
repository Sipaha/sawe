# Background Shells Strip — V3 Implementation Plan (REVISED 2026-05-29)

**STATUS: COMPLETE (2026-05-29).** All 14 tasks shipped to `main`, `7752146ae2..244256d885`.
317 `solution_agent` tests green; `solution_agent` clippy-clean (`--no-deps`). Final
handoff + per-task SHA table: [`docs/findings/2026-05-29-v3-shells-session-handoff.md`](../../findings/2026-05-29-v3-shells-session-handoff.md).
Known V1 limitation: live `Exited(code)` badges are dormant until `claude_native::
translate_user` surfaces text user messages — completed shells reap by output-file
staleness; KillShell→Killed is live. See the handoff's gotcha #1.

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.
>
> **REVISION NOTE (2026-05-29):** The original draft was built on a false premise —
> that background-shell stdout lives only in claude's subprocess memory and must be
> reconstructed from `BashOutput` tool_calls. Ground-truth research
> (`docs/findings/2026-05-29-background-shell-real-shapes.md`) disproved this: the
> output is an on-disk file whose path is in the launch announcement, and completion
> arrives as a `<task-notification>`. This plan is rewritten to the real,
> **file-backed** mechanism, which mirrors Managed Agents almost verbatim and is both
> simpler and better (live tail, not stale snapshot).

**Goal:** Surface Claude Code's background shells (`Bash(run_in_background=true)`) as a
third pill group in `task_subagent_strip`, alongside Task subagents (existing) and
Managed Agents (V1+V2). Drill-in shows a **live tail** of the shell's stdout.

**Architecture:** **File-backed, mirroring Managed Agents.** Parse the `Bash(bg)`
launch tool_result for `(shell_id, output_path)`; tail the on-disk `.output` file for
live stdout; mark terminal when a matching `<task-notification>` (or rare `KillShell`)
is observed on the parent thread, else reap by stale-timeout. Reuse the managed-agent
machinery: newtype id, file-tail, fingerprint render cache, SQLite table, healthcheck
tick, `SubagentView` variant, pill render, drill-in render.

**Tech Stack:** Rust, GPUI, SQLite (`solution_agent::db`), `regex`, `serde_json`.

**Why this exists:** Claude Code's TUI shows running shells under "↓ to manage". A
`Bash(run_in_background=true)` currently appears only as a generic tool_call entry; the
user has no glanceable "N shells running" indicator and no drill-in for live stdout.

All work in commits on `main` (single-developer fork; no Co-Authored-By, no amend).

---

## Ground-truth shapes (verified — see finding doc)

**Launch tool_use input:** `{"command": "...", "description": "...", "run_in_background": true}`

**Launch tool_result text (`is_error:false`):**
```
Command running in background with ID: bvb4ful1z. Output is being written to: /tmp/claude-1000/<encoded-cwd>/<session-uuid>/tasks/bvb4ful1z.output. You will be notified when it completes. To check interim output, use Read on that file path.
```
- shell id = random base36-ish token (`bvb4ful1z`), **no `bash_` prefix**.
- `.output` path is in the announcement (mirrors managed-agent `output_file:`).

**Live output:** `/tmp/claude-<uid>/<encoded-cwd>/<session>/tasks/<id>.output` — real
file, plain text, tailable.

**Completion (`user`-role message content):**
```xml
<task-notification>
<task-id>bvb4ful1z</task-id>
<tool-use-id>toolu_…</tool-use-id>
<output-file>/tmp/.../tasks/bvb4ful1z.output</output-file>
<status>completed</status>
<summary>Background command "…" completed (exit code 0)</summary>
</task-notification>
```
- exit code from `<summary>` `(exit code N)`; `<status>` ∈ {`completed`, …}.

`BashOutput`/`KillShell` tool_calls are effectively never emitted in practice — do not
rely on them as the primary signal. (Keep a minimal `KillShell` observer for the rare
explicit-kill case.)

---

## Reuse map (mirror these exactly — from `background_agent.rs` / `store.rs` / `db.rs`)

| Background shell needs | Mirror this Managed-Agent surface |
|---|---|
| `BackgroundShellId(SharedString)` newtype + `short()` | `BackgroundAgentId` (`background_agent.rs:72`) |
| launch parser `(id, output_path)` | `parse_managed_agent_announcement` (`background_agent.rs:59`) |
| file tail | `tail_jsonl` (`background_agent.rs:231`) — but plain-text variant (see Task 5) |
| `SolutionSession.background_shells` + `_order` | `background_agents` + `background_agent_order` (`model.rs:396`) |
| registration on tool_call terminal | `handle_entry_mutation` Agent branch (`store.rs:2582-2670`) |
| change event | `SessionBackgroundAgentsChanged` (`store.rs:153`) |
| healthcheck reap | `tick_background_agents` (`store.rs:2804`) |
| `remove_*` method | `remove_background_agent` (`store.rs:2991`) |
| `SubagentView::Shell(id)` | `SubagentView::Background(id)` (`store.rs:172`) |
| render fingerprint cache | `background_entries_for_render` + fingerprint (`session_view.rs:323`) |
| selection fallback | `next_selection_after_background_change` (`session_view.rs:899`) |
| pill builder + classifier | `background_pill` + `classify_background_agent_display` (`task_subagent_strip.rs:278/251`) |
| SQLite table + CRUD | `solution_session_background_agent` (`db.rs:132`) |
| compose disable | `compose_disabled_for` (`session_view.rs:2184`) |

**No upstream-Zed files modified.** All work in `crates/solution_agent` + the two
existing `managed_agent_*` settings keys in `crates/agent_settings` (reused, no new keys).

---

## Architectural decisions (this revision)

1. **File-tail, not tool-call-parsing.** The `.output` file is the source of truth for
   stdout; the launch tool_result is the source of truth for `(id, path)`; the
   `<task-notification>` is the source of truth for terminal state. (Finding doc.)
2. **No special clear-on-swap.** Research confirms `background_agents` is NOT cleared on
   `reset_context`/`rotate_context`/`set_acp_thread`; it persists and is reaped by the
   tick. Mirror exactly — drop the original plan's Constraint #3.
3. **Reuse `managed_agent_*` timeout settings.** No new settings keys in V1.
4. **Keep KillShell observer minimal.** Rare in practice; the `<task-notification>` +
   stale-timeout cover the common cases.

## Risks

- **`<task-notification>` may not surface as an `AcpThread` entry** the editor observes
  (it's a synthetic `user` message). Task 8 must verify how/whether it reaches
  `handle_entry_mutation`; if it does not, fall back to stale-timeout-only terminal
  detection (still correct, just no exit code). **Resolve in Task 8 before building the
  exit-code path.**
- **`.output` path under `/tmp` may be cleaned** between launch and read. Tail must
  treat a missing file as "no snapshot yet", not an error.

## Verification (whole feature)

- `cargo build --bin sawe` clean.
- `cargo clippy -p solution_agent --all-targets -- -D warnings` clean.
- `cargo test -p solution_agent --lib` — all green (254 baseline + new).
- MCP smoke-test (supervisor, post-merge) + screenshot of the strip with a shell pill.

---

## Task 1: Module skeleton + types + SolutionSession fields

**Files:** create `crates/solution_agent/src/background_shell.rs`; modify
`crates/solution_agent/src/solution_agent.rs` (add `pub mod` + re-exports);
`crates/solution_agent/src/model.rs` (fields + `new_idle` default-init).

Types (mirror `background_agent.rs`):
```rust
pub struct BackgroundShellId(SharedString);   // Clone+Debug+PartialEq+Eq+Hash
//   ::new / ::as_str / ::short() (first 9 chars — ids are short) / Display

pub enum ShellRuntimeState { Running, Exited(Option<i32>), Killed }   // Clone+Debug+PartialEq+Eq

pub struct BackgroundShell {
    pub id: BackgroundShellId,
    pub command: SharedString,          // from launch input.command (truncate ~120)
    pub output_path: PathBuf,           // the /tmp/.../tasks/<id>.output path
    pub registered_at: DateTime<Utc>,
    pub latest: Option<BackgroundShellSnapshot>,
    pub last_offset: u64,
    pub state: ShellRuntimeState,
}
pub struct BackgroundShellSnapshot {    // Clone+Debug
    pub mtime: SystemTime,
    pub output_tail: SharedString,      // trailing stdout chunk, capped
}
```

- [ ] Module skeleton + types, `pub mod background_shell;` + re-exports.
- [ ] `SolutionSession.background_shells: HashMap<BackgroundShellId, BackgroundShell>`
      + `background_shell_order: Vec<BackgroundShellId>`; default-init in `new_idle`.
      Do **not** add clearing logic (decision #2).
- [ ] Unit test `BackgroundShellId::short`; ensure `BackgroundShell: Clone`.
- [ ] Commit `solution_agent: background_shell module skeleton + SolutionSession fields`.

## Task 2: Launch announcement parser

**Files:** `background_shell.rs`. Mirror `parse_managed_agent_announcement`.

```rust
// regexes (OnceLock<Regex>):
//   shell id:    r"\bID:\s+(\w+)\b"
//   output path: r"written to:\s+(\S+\.output)\b"
pub fn parse_bash_bg_launch(raw_output: &str) -> Option<(BackgroundShellId, PathBuf)>;
```
The command label comes from the tool_call's `raw_input.command` (or `.description`
fallback) at the call-site, NOT from `raw_output`.

- [ ] 5+ TDD tests (happy path with the real announcement string; missing id; missing
      path; non-`.output` path; garbage). Tests fail to compile first, then green.
- [ ] Commit `background_shell: parse Bash(bg) launch announcement`.

## Task 3: `<task-notification>` completion parser

**Files:** `background_shell.rs`.

```rust
pub struct TaskNotification {
    pub id: BackgroundShellId,
    pub status: ShellRuntimeState,   // map <status>completed</status> + (exit code N)
}
pub fn parse_task_notification(text: &str) -> Option<TaskNotification>;
```
Extract `<task-id>…</task-id>`, `<status>…</status>`, and `(exit code N)` from
`<summary>`. `completed` → `Exited(Some(N))` (N from summary, default 0 if absent);
unknown status defensively → `Exited(None)`.

- [ ] TDD tests: completed exit 0; completed exit 137; missing exit-code suffix;
      not-a-task-notification → `None`.
- [ ] Commit `background_shell: parse <task-notification> completion block`.

## Task 4: `KillShell` input parser (minimal)

**Files:** `background_shell.rs`.
```rust
pub fn parse_kill_shell_input(raw_input: &Value) -> Option<BackgroundShellId>;
// reads "shell_id" or "bash_id"; any non-empty string id accepted (no bash_ prefix req)
```
- [ ] 2 tests (happy / missing key). Commit `background_shell: parse KillShell input`.

## Task 5: Plain-text output tail helper

**Files:** `background_shell.rs`. `tail_jsonl` returns the last *line*; shells need the
trailing *chunk* of stdout for the drill-in.
```rust
pub struct OutputTail { pub text: String, pub new_offset: u64, pub mtime: SystemTime }
// Read from min(since_offset, EOF-cap) to EOF; return up to JSONL_LINE_CAP trailing
// bytes as text. Reset since_offset to 0 when it exceeds len (truncation/rotation).
// Missing file → Err(NotFound); caller treats as "no snapshot yet".
pub fn tail_output(path: &Path, since_offset: u64) -> std::io::Result<OutputTail>;
```
- [ ] TDD tests: short file fully read; >64 KiB file returns trailing cap; offset
      resumes mid-file; truncated file resets to 0.
- [ ] Commit `background_shell: plain-text .output tail helper`.

## Task 6: SQLite schema + persistence

**Files:** `db.rs`. Mirror `solution_session_background_agent`.
Table `solution_session_background_shell(solution_session_id, shell_id, command,
output_path, registered_at_ms, last_tail, last_mtime_ms, state_text)`, PK
`(solution_session_id, shell_id)`, index `idx_bg_shell_by_session`. `state_text` ∈
{`running`, `exited:N`, `killed`}.

- [ ] `BackgroundShellRow` + CREATE TABLE + index + 3 free fns + 3 methods
      (`save/load/delete_background_shell`) + `delete_background_shells_for_session`.
- [ ] 2 `#[gpui::test]` round-trip tests (insert/load, insert/delete). Use `cx.executor()`.
- [ ] Commit `solution_agent::db: solution_session_background_shell table + CRUD`.

## Task 7: Registration hook on Bash(bg) tool_call terminal

**Files:** `store.rs`. Mirror the Agent-dispatch branch in `handle_entry_mutation`
(`store.rs:2582-2670`).

- [ ] Extend the local `Snapshot` struct with `raw_input: Option<Value>` (needed for the
      command label + `run_in_background` gate; reuse existing `tool_name` +
      `raw_output_text`).
- [ ] Add the registration branch: `is_terminal && tool_name == "Bash" &&
      raw_input.run_in_background == true`. Parse via `parse_bash_bg_launch`; command
      from `raw_input.command`. Insert into `background_shells` + `_order`; emit a new
      `SessionBackgroundShellsChanged(id)` event; persist if `persistence.is_some()`;
      start the snapshot watcher (Task 9) + inline first refresh.
- [ ] Test mirroring `agent_terminal_with_parseable_raw_output_registers_background_agent`:
      push a `Bash` ToolCall with `run_in_background:true` + the real announcement in
      `raw_output`; assert `background_shells.len() == 1` and `output_path` parsed.
- [ ] Commit `solution_agent: register background shells on Bash(bg) tool_call done`.

## Task 8: Completion / kill observer

**Files:** `store.rs`. **FIRST: verify how a `<task-notification>` reaches the editor**
(is it a `UserMessage` entry in `AcpThread.entries`? a tool_result? neither?). Drive
`run-mcp --debug` + a real bg command if needed, or inspect `AcpThread` ingestion. If it
surfaces as an observable entry, parse it; if not, fall back to stale-timeout-only
terminal detection and note the gap in the commit + a finding.

- [ ] Observe parent-thread entries; on a `<task-notification>` whose id matches a
      tracked shell → set `state = parse_task_notification(...).status`, emit change.
- [ ] KillShell branch: terminal `KillShell` tool_call → `state = Killed`.
- [ ] Tests: notification → Exited(code); KillShell → Killed; unknown id → no-op.
- [ ] Commit `solution_agent: ingest task-notification / KillShell into background shells`.

## Task 9: Snapshot refresh + fs watcher

**Files:** `store.rs`. Mirror `refresh_background_agent_snapshot` +
`ensure_background_agent_watcher`. Tail `output_path` via `tail_output`; set
`latest = Some(BackgroundShellSnapshot{ mtime, output_tail })`; carry `last_offset`.
Watcher on the session's project fs (sourced as
`session.read(cx).project.as_ref()?.read(cx).fs().clone()` — gotcha: store has no `fs`
field). Missing file → leave `latest = None`, no error.

- [ ] Refresh fn + watcher start/stop (drop watcher when `background_shells.is_empty()`).
- [ ] Test: write a temp `.output`, refresh, assert snapshot tail matches.
- [ ] Commit `solution_agent: tail background-shell .output for live snapshot`.

## Task 10: Healthcheck tick reap

**Files:** `store.rs`. Mirror `tick_background_agents`. Reuse `managed_agent_*` timeouts
via `AgentSettings::try_get(cx).unwrap_or((120,300))`. Drop shells whose state is
`Exited`/`Killed` AND whose `latest.mtime` is older than `stale + linger`. Wire into the
**existing** 1Hz tick loop (call both methods per tick — no second timer).

- [ ] `tick_background_shells` + wire-in.
- [ ] Tests: exited shell reaped after linger; killed reaped; fresh-running preserved.
- [ ] Commit `solution_agent: reap terminal background shells on healthcheck tick`.

## Task 11: `SubagentView::Shell` variant + selection fallback

**Files:** `store.rs`, `session_view.rs`, `session_view/tests.rs`. Add
`Shell(BackgroundShellId)` to `SubagentView`; `is_parent_thread_view()` → false;
`matches_parent_entry` → false. Walk every `match SubagentView` compile error (sites
listed in research: `store.rs` ~3725/3732/3740; `session_view.rs`
857/858/908/960/967/2185/2239/2367/2611). Add `next_selection_after_shells_change`
(snap to Main when selected shell absent) + a pass-through `Shell(_)` arm in
`next_selection_after_change`.

- [ ] Variant + arms + selection fallback.
- [ ] Tests mirroring `next_selection_after_background_change_*`.
- [ ] Commit `solution_agent: SubagentView::Shell variant + carry-over fallback`.

## Task 12: Strip rendering — shell pills

**Files:** `task_subagent_strip.rs`, `store.rs`. Append a third pill group after
Background-Agent pills. Mirror `background_pill` + `classify_background_agent_display`.

- [ ] `classify_background_shell_display(state, mtime, now, stale) -> ShellDisplayState`
      (`Running`/`Exited`/`Killed`/`Stale`).
- [ ] `background_shell_pill(...)`: terminal icon (`IconName::Terminal` or nearest) +
      `id.short() · <command-trunc>` label + × on terminal states. Border: `Accent`
      running, `Success` exited(0), `Error` exited(≠0), `Conflict` killed.
- [ ] `remove_background_shell` on the store (symmetric to `remove_background_agent`).
- [ ] 5 classifier tests + pill assembly compile-smoke.
- [ ] Commit `solution_agent: render background-shell pills in subagent strip`.

## Task 13: Drill-in view — live stdout tail

**Files:** `session_view.rs`. When `SubagentView::Shell(id)` selected, render the
shell's `latest.output_tail` as a fenced monospace code block + a header (command,
state, relative observed-at, id). Mirror the `background_entries_for_render` field +
fingerprint cache `(BackgroundShellId, SystemTime, u64)`.

- [ ] `background_shell_entries_for_render` field + builder (one `AssistantMessage`
      with a Markdown fenced block). Stale-id snap-to-Main.
- [ ] 2 `#[gpui::test]`: live shell sources from snapshot; stale-id snaps to Main.
- [ ] Commit `solution_agent: render background-shell drill-in view`.

## Task 14: Compose disable + startup reconciliation + final smoke

**Files:** `session_view.rs`, `store.rs`, `db.rs`; then verification.

- [ ] Extend `compose_disabled_for` to match `Shell(_)`.
- [ ] Startup: `delete_background_shells_for_session` per hydrated session
      (`/tmp` `.output` paths are stale across restarts; don't restore phantom pills).
      Test that hydrate clears persisted shells.
- [ ] Commit `solution_agent: disable compose in shell view + drop stale shells on hydrate`.
- [ ] Supervisor: `cargo build --bin sawe --profile release-fast`; MCP
      screenshot of the strip with a shell pill; manual recipe (ask claude to run a
      background `sleep` + touch a sentinel) in the session handoff.

---

## Deferred (allowed without re-planning)
- No live `BashOutput`-injection "refresh" button (the file tail is already live).
- Reuse `managed_agent_*` timeouts (no shell-specific settings keys in V1).
- One shared strip row (split only if user testing shows overflow).

If the implementer hits NEEDS_CONTEXT or BLOCKED on architecture, halt and surface the
question; don't guess.
