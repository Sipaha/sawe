# Background Agents Strip — implementation handoff

**Date:** 2026-05-28
**Plan:** `docs/superpowers/plans/2026-05-27-background-agents-strip.md` (14 tasks)
**Status:** Tasks 1–13 implemented, committed to `main`. Task 14 (manual MCP smoke + screenshots) requires real `claude` subprocess with `Agent`-tool dispatch — agent-verification ceiling reached, requires user hand-test.

## What shipped

19 commits on `main` (base `7db3b8a995`, head `0084dbbf99`), ordered oldest → newest:

| # | SHA | Subject |
|---|-----|---------|
| 1 | `8ae4b046ed` | `solution_agent: background_agent module skeleton + SolutionSession fields` |
| 2 | `26489ce053` | `background_agent: parse_managed_agent_announcement regex` |
| 2′ | `88fb8da592` | `background_agent: cover 15/16-char agentId boundary in parse tests` |
| 3 | `8f070a6260` | `background_agent: parse_jsonl_snapshot + tail_jsonl` |
| 3′ | `c7793138f5` | `background_agent: document tail_jsonl since_offset precondition` |
| 4 | `00c011be9f` | `background_agent: jsonl_to_entries converter (V1)` |
| 4′ | `7021f286c7` | `background_agent: strengthen pairs test + dedupe tool name clone` |
| 5 | `d131c313f9` | `solution_agent: SubagentView enum replaces Option<SharedString> selector` |
| 5′ | `de6b4f8c03` | `solution_agent: refresh selected_subagent doc + cover Background pass-through` |
| 6 | `585b163894` | `solution_agent::db: solution_session_background_agent table + CRUD` |
| 7 | `17e6f4deb5` | `solution_agent: background-agent watcher scaffolding` |
| 8 | `5dbfe8ff31` | `solution_agent: register Managed Agents on Agent-tool_call done` |
| 8′ | `08af128365` | `solution_agent: close Managed Agent registration→watcher race with inline refresh` |
| 9 | `44bf73a79c` | `solution_agent: managed-agent healthcheck tick + V1 timeout constants` |
| 10 | `2a229257cc` | `solution_agent: render background-agent pills in subagent strip` |
| 11 | `2ee515223b` | `solution_agent: switch entry source on Background view` |
| 11′ | `2de3e260f6` | `session_view: walk to char boundary on JSONL 5MB tail cut` |
| 12 | `a0cc5c47f8` | `solution_agent: disable compose row when Background view selected` |
| 13 | `0084dbbf99` | `solution_agent: reconcile background_agents from SQLite on hydrate` |

(Primed-number rows are review-loop follow-ups within the same task.)

The commit `8a1e20e0a6 solutions.create: mark_open the new solution before returning` was authored by Pavel Simonov in parallel — not part of this work.

Test status at handoff:
- `cargo test -p solution_agent --lib`: **248 passed, 0 failed, 0 ignored**.
- `cargo check` clean on `acp_thread`, `workspace_events`, `editor_mcp`, `agent_servers`.
- `cargo build --bin sawe --profile release-fast`: green (binary at `target/release-fast/sawe`, ~6.3 GB, sha `0084dbbf99`).

## V1 deviations from the plan (load-bearing)

1. **Hardcoded timeouts.** Plan called for `managed_agent_stale_timeout_secs` + `managed_agent_dead_linger_secs` in `agent_settings::AgentSettings`. This crate's settings pipeline runs through `settings::SettingsContent` + a JSON schema — adding two keys requires touching both crates plus the default-schema. For V1, both are `pub(crate) const` in `crates/solution_agent/src/store.rs` (120s and 300s). V2 promotion is mechanical: add fields to `AgentSettings` + `SettingsContent`, replace the two const reads (`store.rs::tick_background_agents` + `task_subagent_strip.rs` classifier). No data-shape lock-in.

2. **JSONL is re-read on every render frame.** `session_view::build_background_entries_for_render` does `std::fs::read_to_string(path).unwrap_or_default()` + `jsonl_to_entries(...)` synchronously. 5 MiB cap → ~1 ms. V2 should cache the converted Vec and invalidate on `SessionBackgroundAgentsChanged`.

3. **`tail_jsonl` always reads from offset 0** in `refresh_background_agent_snapshot`. V2 should persist a per-agent `last_offset` so 64 KiB cap holds even on multi-MB files.

4. **`thinking` content blocks are ignored** by `jsonl_to_entries` (V1 lossy). V2 should render them folded.

5. **Tool-use ↔ tool-result pairing** uses presence-only HashSet (`Completed` vs `Pending`). V2 should render result content + error states.

6. **No leading `-` prepended to encoded cwd** in `background_agent_dir_for` — the leading `/` of an absolute path naturally encodes to `-`, so the plan's "then prepend `-`" instruction was redundant. Current path is `~/.claude/projects/<encoded-cwd>/<acp-session-id>/subagents/`.

## V1 limitations to flag to the user

- **Pre-watcher-subscribe gap:** `Agent` dispatch → JSONL writes → `fs.watch` subscribes is a 100 µs-ish window. Closed via an inline `refresh_background_agent_snapshot` call right after `ensure_background_agent_watcher` (commit `08af128365`), but a degenerate slow-disk scenario could still miss the first line. V2 fix: start the watcher BEFORE the agent dispatch and merge initial snapshots.

- **Stale `Background(id)` blank frame on close:** clicking × on the active Background pill drops the agent from the session map. `on_background_agents_changed` snaps `selected_subagent` back to `Main`, but there's a one-frame gap where the empty Vec renders. Cosmetic, not blocking.

- **No project → no watcher:** if `session.project` is `None` (rare, mostly during tests or weird startup paths), the agent registers + persists but never live-tails. Healthcheck tick still removes done/stale agents. Documented at `store.rs::reconcile_background_agents_for` and the registration site in `store.rs:2580-2589`.

## What needs manual verification (Task 14)

The unit/integration tests cover the data + persistence + lifecycle plumbing. What they CAN'T cover:

1. **Real `Agent` tool dispatch** — claude code's actual output shape. The regex (`parse_managed_agent_announcement`) is matched against the documented format; if claude's output drifts the parser silently returns `None` and no pill appears. Test by running an Agent dispatch and watching for the pill.

2. **Pixel correctness of the strip.** Tests verify classifier states + listener wiring; not the rendered appearance. Test with a screenshot.

3. **JSONL → AgentThreadEntry round-trip in the rendered conversation.** The converter has 3 unit tests but doesn't exercise GPUI rendering on real claude JSONL.

### Recipe for manual smoke

```bash
# 1. Confirm the release-fast binary is the new one:
ls -la target/release-fast/sawe
# mtime should be 2026-05-28 18:45 or later.

# 2. Stop any running editor and start a fresh one via the release-fast binary:
~/.spk/sawe/config/  # find the running pid
# kill it, then:
./target/release-fast/sawe

# 3. Open a Solution with at least one project. Create a new claude session.

# 4. Send a prompt that triggers an Agent dispatch, e.g.:
#    "Spawn a research subagent to find every TODO comment in this repo and report back."
#    Claude code's `Tool: Agent` returns immediately with `agentId:` + `output_file:`.

# 5. EXPECT: a new outlined pill appears in the subagent strip, label `<6hex>·<activity>`.
#    Click it → conversation source switches to the JSONL transcript.
#    Compose row becomes "View only · switch to Main to send".
#    Click `Main` pill → back to parent thread; compose unlocks.

# 6. Wait 120s without claude writing — pill should turn red (Dead state).
#    × button appears on the Dead pill; click it → pill disappears, SQLite row dropped.

# 7. Restart the editor while a Managed Agent is mid-stream:
#    The pill should re-appear with whatever was the latest JSONL state at restart.
```

If anything in steps 4–7 misbehaves, the failure points are isolated:
- Pill never appears → `parse_managed_agent_announcement` regex mismatch with current claude output. Add a `log::debug!` of `raw_output` in `store.rs:2580` (the registration branch).
- Click doesn't switch view → check `on_click` in `task_subagent_strip.rs::background_pill`.
- Compose row not greyed → `session_view::compose_disabled_for` predicate.
- Doesn't survive restart → `hydrate_all_for_solution` reconcile call at `store.rs:1741-1745`.

## Architectural decisions worth recording

These are V1 choices that V2 should re-validate but shouldn't re-litigate without reason:

1. **`SubagentView::Background` is a separate axis from `Task`.** Plan considered shoehorning Background into the existing Task pill machinery — rejected because Task pills' label/lifecycle/render-source are all tied to `active_subagents`, which Managed Agents don't enter.

2. **Reading JSONL on every render frame is acceptable for V1.** 5 MiB cap × actually-rendered-when-selected → microseconds. V2 cache adds complexity that's only justified once there's a perf measurement.

3. **No store→view back-pointer for stale-selection snap.** The `next_selection_after_background_change` helper runs on `SessionBackgroundAgentsChanged` events instead. This is the right shape — keeps the store free of view dependencies.

4. **Watcher takes `Arc<dyn fs::Fs>` from `session.project.fs()` instead of a store-global handle.** Future no-project sessions degrade gracefully without crashing. The plan's `self.fs` field doesn't exist and would have been an over-engineering of the store's surface.

5. **`registered_at` (wall-clock) and `latest.mtime` (file mtime) are separately tracked.** On hydrate, `registered_at = now()` (the new process's "added at"), but `latest.mtime` carries the actual file mtime — so the Dead classifier correctly sees real elapsed silence across restarts.

## Where to look in the code

| Concern | Module / function | Notes |
|---|---|---|
| Data types | `background_agent.rs::{BackgroundAgentId, BackgroundAgent, BackgroundAgentSnapshot, Tail}` | Pure data. |
| Parsing | `background_agent.rs::{parse_managed_agent_announcement, parse_jsonl_snapshot, tail_jsonl, jsonl_to_entries}` | Pure logic + 1 FS op (`tail_jsonl`). |
| Persistence | `db.rs::{BackgroundAgentRow, save_background_agent, load_background_agents, delete_background_agent}` | SQLite, upsert semantics. |
| Selection | `store.rs::SubagentView` + `session_view::next_selection_after_change` / `next_selection_after_background_change` | Pure helpers. |
| Lifecycle | `store.rs::{ensure_background_agent_watcher, refresh_background_agent_snapshot, tick_background_agents, reconcile_background_agents_for, remove_background_agent}` | Foreground-thread store methods. |
| Registration | `store.rs::update_subagent_for_entry` (terminal `Agent`-tool branch) | Hooked via `Snapshot.tool_name + raw_output_text`. |
| Rendering | `session_view/task_subagent_strip.rs::{background_pill, classify_background_agent_display, BackgroundAgentDisplayState}` | Pure classifier + GPUI builder. |
| View source | `session_view.rs::build_background_entries_for_render` | Switches between AcpThread entries and JSONL converter. |
| Compose gate | `session_view.rs::{compose_disabled, compose_disabled_for, submit_compose_now, submit_compose_and_interrupt}` | Predicate + 2 submit choke points. |

## Constants worth knowing

- `MANAGED_AGENT_STALE_TIMEOUT = 120s` (`store.rs::~222`).
- `MANAGED_AGENT_DEAD_LINGER = 300s` (`store.rs::~226`).
- `JSONL_LINE_CAP = 64 KiB` (`background_agent.rs::~116`).
- `ARG_BUDGET = 30 chars` for tool-use label truncation (`background_agent.rs::~185`).
- `SOFT_CAP = 5 MiB` for full-JSONL render-time read (`session_view.rs::~953`).
- 1 Hz healthcheck tick interval (`store.rs::new_in_app`).
- 200 ms `fs.watch` debounce (`store.rs::ensure_background_agent_watcher`).

## Open follow-ups (not blocking)

1. Promote the two timeout constants to `agent_settings`. ~1 hour.
2. Cache `jsonl_to_entries` output + invalidate on `SessionBackgroundAgentsChanged`. ~2 hours.
3. Persist per-agent `last_offset` for incremental tail. ~1 hour.
4. Render `thinking` blocks (folded) in JSONL converter. ~2 hours.
5. Tool-result content rendering on paired `ToolCall`s. ~3 hours.
6. Start `fs.watch` BEFORE `Agent` dispatch returns (race-free first snapshot). ~2 hours.

These are V2 polish, not V1 correctness.
