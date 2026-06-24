# Session handoff — 2026-05-29 (V3 Background Shells Strip shipped)

**READ FIRST on session resume.** Supersedes [`2026-05-29-session-handoff.md`](2026-05-29-session-handoff.md).

## One-line status

V3 **Background Shells Strip** shipped end-to-end to `main` (all 14 plan tasks
+ V3-polish-1), 324 `solution_agent` tests green, `solution_agent` clippy-clean
(`--no-deps`), release-fast binary rebuilt for hands-on testing. Live `Exited(code)`
badges now work (gotcha #1 RESOLVED — see below).

## Continued this session (after V3 shipped)

- **Code review** (`/review-changes`, Sonnet+Opus) of the whole session → fixes in
  `sawe` `7f86c01ea8` (committed + **pushed** to origin): unique per-pill `{id}-close`
  ElementIds (was a constant → × click misrouting with ≥2 terminal pills); `read_complete_lines_from`
  skips an over-cap line instead of wedging the completion scan forever (+ regression test);
  doc/test fixes. Deferred (follow-up): extract the background-shell machinery out of the
  `store.rs` god-object into `store/background_shell.rs` (recorded in the review dismissed archive).
  New review-surfaced memories: [[incremental-tail-offset-wedge]], [[gpui-duplicate-elementid-in-lists]].
- **Background Shells on Mobile arc** (F-arc precedent, two repos — NOT yet pushed):
  - SERVER `sawe` `cdfd800e0f`: `get_session_background_shells` tool + `BackgroundShellDto`
    + `agent_session_background_shells_changed` notification (replaced the event_sources no-op) +
    remote_control allow-list. Additive, no wire-schema bump. 330 `solution_agent` tests.
  - CLIENT `sawe-mobile` `2ae83135`: `BackgroundShellDto`/payload DTOs (+7 `:core` tests),
    notification dispatch + `DetailNotificationRouter.onBackgroundShellsChanged`, `SessionDetailStore`
    `backgroundShells` StateFlow, `BackgroundShellStrip` pills + `include_output` stdout drill-in sheet.
    `:core:test` green; `:app:assembleDebug` OK (debug APK ~22.4 MB).
  - Plan: `docs/superpowers/plans/2026-05-29-background-shells-on-mobile.md` (COMPLETE).
  - **To test end-to-end:** rebuild the editor release-fast (to serve the new wire) + install the
    Android debug APK; open a session with a background shell → pill strip + tap → stdout sheet.
  - Out of scope (follow-ups): a kill action from mobile.
- **Background Agents on Mobile arc** (mirror of the shells arc — two repos):
  - SERVER `sawe` `f0d1c197b7`: `get_session_background_agents` tool + `BackgroundAgentDto`
    (label/mtime_ms/stop_reason — no command/output_tail, no include flag) +
    `agent_session_background_agents_changed` notification (replaced its no-op) + allow-list. 334 tests.
  - CLIENT `sawe-mobile` `93969b7`: DTO + dispatch + `onBackgroundAgentsChanged` +
    `backgroundAgents` StateFlow + `BackgroundAgentStrip` (running/done pills) + minimal drill-in sheet
    (no network fetch — DTO has everything). `:core:test` green (+6); `:app:assembleDebug` ~23.6 MB.
  - Plan: `docs/superpowers/plans/2026-05-29-background-agents-on-mobile.md` (COMPLETE).
  - Follow-up: full JSONL-transcript drill-in for agents (the rich desktop view); a kill action from mobile.
  - Mobile now has THREE strips: active subagents (F-arc), background shells, background agents — each
    a separate row. Merging them into one row is a deferred UX follow-up.

## The big pivot this session

The committed V3 plan was built on a **false premise** (its Constraint #1: "background
shell stdout lives only in claude's subprocess memory, observable only via `BashOutput`
tool_calls"). Ground-truth research disproved it — see
[`2026-05-29-background-shell-real-shapes.md`](2026-05-29-background-shell-real-shapes.md).
The real mechanism:

- **Launch**: a `Bash` tool_call with `raw_input.run_in_background == true`; its
  `raw_output` says `Command running in background with ID: <tok>. Output is being
  written to: /tmp/claude-<uid>/<encoded-cwd>/<session>/tasks/<tok>.output. …`
  (id is a random token like `bvb4ful1z`, **not** `bash_1`).
- **Live output**: that `.output` file is plain text on disk, tailable like a
  managed-agent JSONL.
- **Completion**: a `<task-notification>` user message with `<task-id>`, `<status>`,
  and `(exit code N)` in `<summary>`.

So the feature was rewritten to **mirror Managed Agents almost verbatim** (parse launch
→ tail the `.output` file → mark terminal), which is simpler and gives a *live* stdout
tail. Plan revised + re-committed as `7752146ae2` before any implementation.

## Commit chain (oldest → newest), base `01271195e7`

| SHA | Subject |
|-----|---------|
| `7752146ae2` | plan: revise background-shells-strip to file-backed architecture |
| `9565e87b2e` | T1 background_shell module skeleton + SolutionSession fields |
| `120838c16e` | T2 parse Bash(bg) launch announcement |
| `0f13ad0797` | (drive-by fix) two test `AgentSettings` literals missing `managed_agent_*` fields |
| `33e921b4dc` | T3 parse `<task-notification>` completion block |
| `5d6fff55e9` | T4 parse KillShell input |
| `ae8ea240f8` | T5 plain-text `.output` tail helper |
| `176a53cb47` | T6 `solution_session_background_shell` table + CRUD |
| `4e12e31519` | T7+T9 register + live-tail shells on Bash(bg) |
| `d88079b2dc` | T8 mark terminal on task-notification / KillShell |
| `218cace3b0` | T10 reap expired shells on healthcheck tick |
| `2a5ebe6c0a` | T11 `SubagentView::Shell` variant + selection fallback |
| `f93077f2a3` | T12 render shell pills in subagent strip |
| `7da463dc6e` | T13 shell drill-in live-stdout view |
| `ea5989ba0e` | T14 disable compose in shell view + drop stale rows on hydrate |
| `244256d885` | clear clippy debt surfaced by `--no-deps` (identity map, redundant clones, last→rfind) |
| `9562fdc580` | **V3-polish-1:** flip shells to `Exited(code)` live via parent-JSONL `<task-notification>` scan on the 1Hz tick |

## How it works now (architecture)

`crates/solution_agent/src/background_shell.rs` (new) owns: `BackgroundShellId`,
`ShellRuntimeState{Running,Exited(Option<i32>),Killed}`, `BackgroundShell`,
`BackgroundShellSnapshot`, `parse_bash_bg_launch`, `parse_task_notification`,
`parse_kill_shell_input`, `tail_output` (full trailing-window plain-text tail),
`to_state_text` (SQLite mapping).

`store.rs`: registration branch in `apply_subagent_lifecycle` (placed **before** the
`is_task_like` early-return — `Bash` isn't task-like); `SessionBackgroundShellsChanged`
event; `background_shell_watchers`; `ensure_background_shell_watcher` (watches the
`tasks/` dir, refreshes on `*.output` events); `refresh_background_shell_snapshot`
(reads the full trailing window via `tail_output(path, 0)`); `tick_background_shells`
(reaps on terminal-state OR staleness, wired into the existing 1Hz loop);
`remove_background_shell`; KillShell→Killed + task-notification→Exited observers.

`session_view.rs` + `task_subagent_strip.rs`: `SubagentView::Shell(id)`, shell pills
(terminal icon, `id.short() · command`, state-colored border, × on terminal/stale),
drill-in body (command/state/time header + fenced stdout tail), compose disabled in
shell view, stale-id snap-to-Main. `db.rs`: `solution_session_background_shell` table;
rows dropped on hydrate (no phantom pills after restart).

## Active gotchas (carry forward)

1. **~~`Exited(code)` badges are DORMANT~~ — RESOLVED (`9562fdc580`, V3-polish-1).**
   Background: `claude_native::translate_user` (translate.rs:~197) emits `SessionUpdate`s
   only for `tool_result` user blocks, so a `<task-notification>` text user message never
   becomes a `UserMessage` entry — the original `observe_task_notification` NewEntry scan
   was dormant. **Rejected the obvious fix** (surfacing it as a `UserMessage` entry) because
   a `NewEntry` flips the session `Idle→Running` and would leave it STUCK on Running if the
   bg command completes while idle and claude doesn't auto-resume (the bug the orphan-result
   path guards against), plus it'd render raw XML in the transcript. **Shipped instead:**
   the store tails the parent session JSONL on the existing 1Hz tick
   (`scan_parent_jsonl_for_completions`), raw-line-matches `<task-notification>`, parses,
   and `mark_background_shell_state(Exited(code))` — no entries, no events, no
   `claude_native`/`acp_thread` change, no transcript pollution. Forward-only offset
   (lazy-init to EOF, cleared when a session has no shells). Completed shells now flip to
   `Exited(N)` within ~1s. `KillShell→Killed` was already live. (The old NewEntry
   `observe_task_notification` scan remains wired but dormant — harmless; it'd activate if
   translate ever surfaces these as entries.)
2. **clippy on `solution_agent` needs `--no-deps`.** `cargo clippy -p solution_agent`
   never lints solution_agent's own code because it fails first on pre-existing
   `-D warnings` errors in dep crates `claude_native` (`int_plus_one`) and `solutions`
   (`disallowed_methods`, `redundant_clone`). Use `cargo clippy -p solution_agent
   --no-deps --all-targets -- -D warnings`. **Those dep-crate clippy errors are still
   unfixed** — a good LIGHT follow-up so `./script/clippy` is clean workspace-wide.
3. **No live visual smoke yet (agent-verification ceiling).** Populating a real shell
   pill needs a real claude subprocess running `Bash(run_in_background=true)` — no MCP
   tool injects a synthetic tool_call. Unit tests cover the render logic; the live
   screenshot is deferred to the user (recipe below), same ceiling as V1's Task 14.
4. **`background_shells` is NOT cleared on context reset / thread swap** (mirrors
   `background_agents` by design). Stale shells from a dead subprocess are reaped by the
   tick's staleness path; their `/tmp` `.output` files are gone so the tail just yields
   no new bytes.

## Manual smoke recipe (for the user / next live session)

1. Launch the release-fast editor (`target/release-fast/sawe`, sha `244256d885`).
2. In a session, ask claude: *"Run `for i in $(seq 1 30); do echo tick $i; sleep 2;
   done` in the background, then leave it."*
3. EXPECT: a terminal-icon pill appears in the subagent strip labelled
   `<id> · for i in $(seq 1 30)…`. Click it → drill-in shows the live `tick N` stdout
   (refreshing as the file grows). After ~60s the command ends; within ~1s the pill flips
   to **Exited (0)** (green border, × appears), and the staleness reap later drops it.
4. Ask claude to *"kill that background shell"* → KillShell → pill flips **Killed**
   (live), × appears.

## Outstanding pool (next phase candidates, § 7 NEXT)

- ~~**V3-polish-1:** make `Exited(code)` live~~ — DONE (`9562fdc580`, see gotcha #1).
- **V3-polish-2 (LIGHT, next pick):** fix the pre-existing clippy debt in `claude_native`
  (`int_plus_one`) + `solutions` (`disallowed_methods`, `redundant_clone`) so
  `./script/clippy` is workspace-clean and `cargo clippy -p solution_agent` works without
  `--no-deps` (gotcha #2).
- Refresh button on the shell drill-in (deferred — the file tail is already live, so
  lower value than for managed agents).
- Carried from prior handoff: queue persistence, crash reporting, F/G arcs.

## Resume recipe

`продолжай` / `resume` → glob `docs/findings/*-session-handoff.md`, read THIS file
(lex-latest), read `docs/workflow/supervisor-mode.md` §7, `git log --oneline -20`,
pick from the pool above. V3-polish-2 (workspace clippy debt) is the natural next pick;
or pivot to the carried-over arcs (queue persistence, crash reporting, F/G).
