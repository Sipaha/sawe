# Active async teammate writes into Main and no sub-agent tab appears

**Date:** 2026-07-06
**Status:** fixed
**Repos:** editor (`crates/solution_agent`) + mobile (`spk-editor-mobile`)

## Symptom (user, live)

> "в основную вкладку чата все еще попадают сообщения от саб агентов … агент то
> активный в моменте и активно пишет. И его сообщения попадают в основную вкладку,
> а табы внизу не появляются"

A sub-agent is actively streaming *right now*, its messages land in the Main
tab, and **no tab appears in the strip** for it.

## Two linked root causes

The forge session runs a supervisor that spawns `general-purpose` executors via
claude's **`Agent` tool = async teammate**: the tool call returns immediately
with a spawn-ack, and the teammate then streams for minutes, its output tagged
in the parent thread with `subagent_id = <the Agent call's toolu_id>`.

### A. The background-agent pill never registers (no tab)

On the `Agent` tool call going terminal, the store registers a `BackgroundAgent`
(→ strip pill + JSONL tail) by parsing an `agentId:` / `output_file:`
announcement — **from `raw_output`**. But for an async launch `raw_output` is
`null`; the announcement rides in the tool_result BODY, which the native adapter
surfaces as the tool call's **content**:

```
raw_output: null
content[0]: "Async agent launched successfully. …
             agentId: a874596024f50661f …
             output_file: /tmp/claude-…/tasks/a874596024f50661f.output …"
```

So `parse_managed_agent_announcement(raw_output="")` returned `None` → no
background agent → **no strip pill**. The Task pill was already removed when the
`Agent` call completed. Net: no tab at all for an actively-streaming teammate.

**Fix:** `background_agent::managed_agent_announcement(raw_output, content)` tries
`raw_output` first (forward-compat) then falls back to the tool call's content —
the current claude path. `apply_subagent_lifecycle` now captures the terminal
task-like call's content text and feeds both. (`store.rs`, `background_agent.rs`,
3 new unit tests.)

### B. Main showed sub-agent-tagged entries whenever the strip was empty

Sub-agent entries are correctly stamped with `subagent_id` and the Main view
filters them out — but both surfaces **bypassed** that filter when
`active_subagents` was empty (desktop `should_render_entry`, mobile `ChatList`),
a "cold-restart guard" against dead-`toolu` rows vanishing. An async teammate
leaves `active_subagents` empty (its Task pill was removed on the early
completion; it lives as a *background* agent, which doesn't populate
`active_subagents`), so the bypass fired the whole time it streamed → its tagged
output flooded Main.

**Fix:** dropped the bypass on both surfaces — Main is always main-thread-only.
(`session_view.rs`; `SessionDetailScreen.kt::ChatList`.)

## Why both are needed

- Fix B alone would *hide* the teammate's tagged parent-thread output from Main
  — but with no background pill (bug A) its work would then be invisible
  everywhere. Regression.
- Fix A alone registers the pill, but `active_subagents` stays empty for a
  background agent, so bug B's bypass would still flood Main.

Together: the teammate gets a **Background strip tab** (its JSONL transcript),
Main stays clean (only the main-agent thread + the `Agent` spawn call, which is
`subagent_id == None`), and the verbose interior lives in the tab.

## Scope note

The fix registers on *new* `Agent` terminal events. Async agents spawned before
the fix (no persisted background-agent row) won't retroactively get a tab after
restart — verify with a freshly-spawned teammate.

## Verification

- Editor `solution_agent` suite: **525** passed (was 522; +3 announcement tests).
- Mobile `:app:compileDebugKotlin` + `:core:test` green.
- Data: forge session `3xo8kh9i` — the three `Agent` calls carry the
  `agentId`/`output_file` announcement in `content[0]` with `raw_output: null`,
  confirming the parse-source bug; their sub-agent output (idx 189→373) is
  correctly `subagent_id`-tagged, confirming the filter has what it needs.
- Live eyeball pending on the user's instances (standing live-test loop).
