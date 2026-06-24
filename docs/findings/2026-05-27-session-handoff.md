# Session handoff — 2026-05-27

## What shipped this session

Five commits to `main` first, then the branch flipped to `feature/unified-workspace-wire` (another session pulled ahead with the `workspace_events` work — our commits are still in its history). Ordered oldest → newest:

| SHA | Subject | One-line why |
|---|---|---|
| `c4b437f715` | `console_panel: extract chat_cwd_options for new-chat root picker` | Pure-fn that maps a `Solution` to `[Solution root, member1, member2, …]` choices for the upcoming submenu. Two unit tests. |
| `762d25debf` | `console_panel: add add_chat_tab_with_cwd plumbing` | Renames `add_chat_tab(solution_id)` → `add_chat_tab_with_cwd(solution_id, cwd)`; the old name becomes a one-line wrapper passing `None`. |
| `3e7e080447` | `console_panel: add cwd submenu under New AI Chat` | First cut wired a GPUI `ContextMenu::submenu`. Later **superseded** by the flat-menu fix (see below) but the commit is kept for diff sanity. |
| `b1e8bfce0d` | `console_panel: remove expect() in chat submenu render` | Followup from code-quality review — `.expect()` in a `render_*` body violates the no-panic guideline. Replaced by `if let Some(...) else menu.action_disabled_when(true, ...)`. |
| `64f3dd4d81` | `console_panel: flatten New AI Chat cwd choices instead of submenu` | **The actual fix.** GPUI `ContextMenu::submenu`'s `on_click` handler doesn't reach via `windows.click_id` / single-tap mouse click — submenu only opens on hover, which means "click on `New AI Chat ▸` did nothing" for real users. Replaced submenu with a header + flat list of leaf entries: `New AI Chat in: / Solution root / member1 / member2 / …`. Each entry is one click. Verified in headless: clicking each pill creates a session with the right cwd. |
| `ecb5d1ff57` | `solution_agent: expose session cwd on list_sessions and get_session` | Adds an `Option<String> cwd` field to `SessionSummary` + `GetSessionResult`. Drove from the Task 4 verification needing SQLite peek to confirm cwd — now MCP exposes it directly. Empty `PathBuf` (legacy DB rows) → `None`. |
| `0ac4d1a4ca` | `acp_thread: skip per-turn git checkpoint in AcpThread::send` | Upstream writes a commit-tree to project repos on every user message so AgentPanel can "revert agent edits." This fork hides AgentPanel and surfaces no restore-checkpoint UI — every call was wasted CPU + `.git/objects/` bloat + a `log::error!` ENOENT whenever a project repo couldn't be traversed. `UserMessage.checkpoint` now always `None`; `update_last_checkpoint` early-returns; `restore_checkpoint` becomes a no-op for unchecked messages. Methods kept (disable-don't-delete). `test_checkpoints` marked `#[ignore]`. FORK.md decision #23 added. |

Verification artifacts:
- `docs/findings/2026-05-27-new-chat-root-picker/{popover-with-submenu-arrow,submenu-open,submenu-frontend-highlighted}.png` — pre-flatten state of the new-chat root picker, captured while debugging the submenu issue.

## What's in-flight (NOT YET STARTED)

**Background agents strip** — spec + plan committed to disk, implementation queued:

- Spec: `docs/superpowers/specs/2026-05-27-background-agents-strip-design.md` (in `.gitignore`'d dir, not committed; lives on disk only).
- Plan: `docs/superpowers/plans/2026-05-27-background-agents-strip.md` (same — on-disk only).

### Why this feature

Claude Code's built-in **Managed Agents** (the `Tool: Agent` async dispatch) is invisible in our UI. When the parent claude dispatches a background sub-agent via the `Agent` tool, the tool call returns immediately with an `agentId:` + `output_file:` (a symlink to `~/.claude/projects/<encoded-cwd>/<session-id>/subagents/agent-<id>.jsonl`). The parent goes Idle. The user has no way to tell whether the background agent is alive, what it's doing, or how many are running. The user kept asking "что claude делает сейчас?" — and the answer was buried in a tmp file with a path they didn't know existed.

### Design summary (full text in the spec)

- New pills in the existing `task_subagent_strip` (after `Main` + Task pills), visually distinct (outlined, 6-hex prefix on the label).
- Pill states: 🟢 running (mtime <120 s, no terminal `stop_reason`); ⚪ done → **hide immediately** (user explicitly asked); 🔴 dead (mtime ≥120 s, no `stop_reason`) with `×` close button + auto-disappear after 5 min linger.
- Click a pill → conversation panel switches to read-only JSONL view of that agent's transcript. Compose row goes greyed-out with `View only · switch to Main to send` and Submit is short-circuited.
- Persisted in a new SQLite table `solution_session_background_agent` so they survive restart.
- Watcher via the existing `fs::Fs::watch` abstraction on the `subagents/` directory (works under FakeFs in tests).
- Two `agent_settings` keys: `managed_agent_stale_timeout_secs` (default 120), `managed_agent_dead_linger_secs` (default 300).

### Implementation plan

14 tasks, decomposed by layer:

1. Module skeleton + types + `SolutionSession.background_agents` fields.
2. Regex parser for the Agent-tool announcement.
3. JSONL tail parser (incl. 64 KiB single-line cap).
4. JSONL → `AgentThreadEntry` converter (V1, lossy).
5. `SubagentView` enum replaces `Option<SharedString>` selector.
6. SQLite table + CRUD helpers.
7. Watcher via `fs::Fs::watch`.
8. Registration hook on `Agent`-tool_call terminal status.
9. Settings keys + 1 Hz healthcheck tick.
10. Strip render — background pills with `×` on dead.
11. Conversation source switch (parent thread vs JSONL).
12. Compose disable when Background selected.
13. Startup reconciliation from SQLite.
14. Manual MCP verification + screenshots.

Each task is TDD-shaped (failing test → impl → green → commit). Self-review confirmed spec coverage 14/14 + type consistency.

### Execution path

User chose **subagent-driven-development** (option 1). Workflow:
- Dispatch fresh subagent per task with full task text inline (no plan-file read).
- Spec-compliance reviewer → code-quality reviewer between tasks.
- Continuous execution; no per-task user check-in.

Already started but **paused before Task 1** — current branch state is unexpected (see below) and the user requested a context reset before diving in.

## Branch state (important)

When this session ended, the local checkout was on `feature/unified-workspace-wire`, NOT `main`:

```
1276811c29 docs(readme): rebrand from Sawe to Sawe
8a2335bf88 test(workspace_events): full lifecycle round-trip e2e
2b941553ab feat(workspace_events): close_solution cancels in-flight agent threads (terminals TBD)
… (~18 commits ahead of where we left off on main)
0ac4d1a4ca  ← our last commit ("acp_thread: skip per-turn git checkpoint")
```

The `feature/unified-workspace-wire` branch picked up our work and added:
- New `workspace_events` crate with a sequenced event protocol (atomic seq counter, snapshot tool, `workspace.{snapshot, list_solutions, open_solution, close_solution, open_session, close_session}`).
- Wire-breaking renames: `SolutionSummary.window_open → open`, `solution_agent` wire `close_session → delete_session`.
- `wire_schema_version` bumped to 2.
- Throttled `session_metrics_changed` + replication lock for snapshot seq atomicity.
- README rebrand to "Sawe" (display name only; identifiers in code presumed unchanged but verify before naming anything in new code).

Also: `Cargo.lock` shows `M` (modified) and is uncommitted. Likely a side-effect of the in-flight branch work — not from our session.

### What this means for the next session

1. **Pick branch first.** Before Task 1, the supervisor must ask the user: stay on `feature/unified-workspace-wire`, switch back to `main`, or branch off `main` for the new feature? The plan tasks reference `main`; they need to be reread/adapted for whichever branch is chosen.

2. **Resolve `Cargo.lock` first.** Either commit the in-flight diff or stash it before starting fresh feature work. Don't blindly bundle it into the first background-agents task commit.

3. **Verify the rebrand state.** Spec mentions `SolutionSession`, `SolutionAgentStore`, `crate::store::SubagentView`, etc. If the in-flight rebrand renamed these (looks unlikely from the commit messages — only README mentions Sawe), confirm before relying on the symbol names in the plan.

4. **Schema migration footprint.** Task 6 adds a new SQLite table `solution_session_background_agent` — this is purely additive (`CREATE TABLE IF NOT EXISTS`) and uses the existing `apply_idempotent_add_column` style, so no risk of clashing with the `workspace_events` branch's changes (which work at the wire layer, not the DB layer).

## Outstanding decisions

None for background-agents — spec is settled and signed off by user. Just execute.

## Active gotchas (do NOT relearn these)

### GPUI `ContextMenu::submenu` doesn't open on click

Spent ~1 h verifying this. `ContextMenu::submenu(label, builder)` registers an entry with `on_click` AND `on_hover` handlers (see `crates/ui/src/components/context_menu.rs:1612` for the click path, `:1565` for hover). In practice **only hover opens the submenu** — a single `windows.click_id` in headless or a single mouse click in a real session does nothing visible. The on_click body is reached (logged) but the submenu render either doesn't fire or fires + immediately dismisses. Did not find a clean fix in 2 h — bypassed by flattening the menu in commit `64f3dd4d81`. If a future task wants a real submenu, expect a real investigation.

For the background-agents work this is a non-issue (we render pills in the existing strip, no submenu).

### `Tool: Agent` is Claude Code built-in, NOT a custom MCP

Tool name `Agent` (and its sibling `SendMessage`) with the `agentId` + `output_file` output format are Anthropic's **Managed Agents** feature, bundled into recent claude code versions. User confirmed they have no custom MCP launcher (`voxelcraft-agent` MCP in `.mcp.json` has only game-driving tools, nothing called "Agent"). All `crates/agent-mcp` paths visible in `~/.claude/projects/.../subagents/` come from claude code itself.

This is why the spec puts the regex parse on `tool.name.eq_ignore_ascii_case("agent")` instead of inventing an MCP-side registration protocol.

### `acp_thread::AcpThread::send` no longer captures git checkpoints

Disabled in commit `0ac4d1a4ca` (FORK.md decision #23). Side effect for the next agent: `UserMessage.checkpoint` is always `None`; `restore_checkpoint(id)` is a no-op; any new code that wants a checkpoint must restore the capture at the `send` call site (3 lines around `acp_thread.rs:2450`).

### Build target = `sawe` (the cargo bin), not `sawe`

Despite the README rebrand on the `feature/unified-workspace-wire` branch, `cargo build --bin sawe` is still the right binary command — the cargo bin name + `target/release-fast/sawe` are intentionally unchanged (see FORK.md "Locked rebrand identifiers" — rename "requires user approval" and hasn't been done).

## What to do on resume

If user says "продолжай работу" / "resume":

1. **Read `docs/INDEX.md`** + this handoff (auto by the resume protocol).
2. **Decide branch** — ask user which branch the background-agents work goes on (see "Branch state" §1).
3. **Resolve `Cargo.lock`** — commit, stash, or revert per user's call.
4. **Start Task 1** from `docs/superpowers/plans/2026-05-27-background-agents-strip.md` via `superpowers:subagent-driven-development`. The plan is self-contained — each task has the full text the implementer needs; the supervisor just dispatches + reviews.

5. After all 14 tasks complete + final review: `superpowers:finishing-a-development-branch`.

## What's NOT outstanding

The "New AI Chat root picker" feature **shipped** this session. Commits `c4b437f715` → `64f3dd4d81` are live. No followups — the flat menu is the final UX and the user signed off.

MCP cwd field (`SessionSummary.cwd`, `GetSessionResult.cwd`) **shipped** in `ecb5d1ff57`. Tested via `solution_agent.list_sessions` + `solution_agent.get_session` in headless. No followups.

Git-checkpoint disable **shipped** in `0ac4d1a4ca` with FORK.md decision and ignored test. No followups.
