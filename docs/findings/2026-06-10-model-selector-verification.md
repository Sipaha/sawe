# Status-row model selector — verification status (2026-06-10)

Branch: `status-bar-model-selector` (13 commits, `4fb8153242`..`67eb33ba7e`).
Spec: `docs/superpowers/specs/2026-06-10-status-bar-model-selector-design.md`.
Plan: `docs/superpowers/plans/2026-06-10-status-bar-model-selector.md`.

## What shipped

A per-session model selector on the AI session status row:
- **List from Claude, persisted per session.** Captured from the `initialize`
  control-response (`parse_available_models`, after the first turn), cached on
  `SolutionSession.cached_models`, persisted in `PersistedSession`
  (`#[serde(default)]`), restored on cold load. Cold/sleeping sessions show the
  list offline.
- **Dropdown** replaces the read-only model label (`status_row.rs`), gated to
  `!is_subagent_tab && !cached_models.is_empty()`; check on the current model;
  a "Refresh models" entry.
- **Switching by state:** live → `set_model` control request
  (`ClaudeNativeConnection::select_model`); cold/not-started → `desired_model`
  persisted, applied as `--model <value>` at the next spawn. Delivered to BOTH
  the new-session path (meta `modelId`) AND the resume/load wake path (native
  `desired_models` map, since native `resume_session` carries no meta).
- **Refresh** for cold sessions → throwaway `claude` probe
  (`ClaudeNativeAgentServer::probe_models`), no wake.

## Verified ✅

- **Compiles**: `cargo build --bin spk-editor` clean (debug + release-fast).
- **Unit tests green** (per task): `claude_native` 69 lib + 14 integration;
  `solution_agent` 358 + 2. Covers: `set_model` wire shape, `--model` arg
  on/off, `parse_available_models` (+ malformed), `PersistedSession` round-trip
  + old-blob serde-default migration, and the **cold-path store test**
  (`select_model_on_cold_session_records_desired`: cold `select_model` →
  `desired_model` persisted, `selected_model` returns it).
- **Spec-reviewed** (subagent, end-to-end trace): the cold-pick→wake chain and
  the live-pick chain were verified INTACT at the code level with line cites.
- **Editor launches headless** (`script/run-mcp --debug --headless`), MCP works
  (`tools/list`, `solutions.open`, `solution_agent.create_session` all OK).

## BLOCKED in this environment ❌ (not a code defect)

The turn-dependent live checks could NOT run: in the headless dev instance the
`claude` subprocess spawns and connects its MCP bridge but **wedges before
producing any turn** (sleeping, 0% CPU, zero `agent_session_*`/`message_start`
ever logged). The model-selector code does not touch the prompt/stdin/turn
path, so this is an environment/auth stall, not the feature. Consequence: no
live model capture → the dropdown can't render → **no `.rules` screenshot**, and
the two Phase-0 assumptions below remain live-unconfirmed.

> Future sessions: don't burn time driving live `claude` turns in the headless
> dev instance — they stall here. Verify UI hands-on in the real release-fast
> instance (`~/.spk/spk-editor/config/mcp.sock`).

## Follow-up work (2026-06-10, same branch)

- **`set_model` CONFIRMED working** by hands-on test (the agent correctly
  reports its new model after a pick) — Phase-0 assumption #1 resolved, no
  fallback needed.
- **Status-row fixes** (`7682e0804e`): the trigger now shows the *picked*
  model immediately (was reading the stale observed `active_model`); selection
  is blocked while the agent is mid-turn/resuming.
- **New-session model picker** (`9aa57f70a1` store + `417d69cc66` UI): the "+"
  popover has a flattened "Model" section (list derived from the latest
  session, + "Refresh models" probe); the chosen model is threaded into
  `create_session_with_cwd` → `--model`, so a new session starts on it. Default
  = the latest session's model; persisted on the new session's `desired_model`.

## Pending live confirmation (with fallback)

1. **`/context` flushes the `initialize` response with `models`** (the cold
   Refresh probe's first stdin — `PROBE_FLUSH_MESSAGE` in
   `connection.rs::probe_models`). If `models` is empty after `/context`, change
   it to a tiny real prompt. Verify: Refresh a cold session, confirm the list
   populates.

## Hand-off

`release-fast` binary built for hands-on testing (`target/release-fast/spk-editor`).
To verify: open a Solution, run one agent turn, confirm the model dropdown
appears on the status row with the current model checked; switch models and run
another turn (label should follow); restart the editor and confirm a restored
(Sleeping) session shows the persisted list and "Refresh models" works; pick a
model while cold, then send the first message and confirm the turn runs on it.
