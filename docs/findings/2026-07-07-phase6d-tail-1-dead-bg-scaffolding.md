# Phase 6d-tail-1 — remove the dead background-agent scaffolding (desktop-only)

**Date:** 2026-07-07. **Character:** pure dead-code sweep of the scaffolding left unreachable after 6d-B
folded async agents onto their demux `Teammate` stream. **No advertised wire surface changed shape** — so
this is editor-only, no mobile lockstep, no wire bump, push pre-authorized. It net-removes ~1000 lines and
SIMPLIFIES the render path (deletes the last JSONL drill-in branch).

## What was removed (all confirmed unreachable post-6d-B)
- **`SubagentView::Background` variant** + every match arm (store.rs `queue_target`/`parent_stream_id`;
  session_view.rs `next_selection_after_change`/`compose_disabled_for`/render; status_row.rs meter arm).
  `SubagentView` is now exactly `{Main, Task, Shell}` — isomorphic to `StreamId` (the literal
  `SubagentView`→`StreamId` rename is deferred to 6d-tail-2; it's cosmetic).
- **`build_background_entries_for_render`** + the fields `background_entries_for_render` /
  `background_entries_fingerprint` + the whole JSONL drill-in RENDER branch. The `render()` source-switch
  (`is_background`/`is_drill_in`) collapses: the view now ALWAYS sources from `main_stream_entries_for_render`
  (which already serves Main/Teammate/Shell via `parent_stream_id`). `list_state` reset-on-view-switch,
  `collect_entry_texts`, the row `.get(idx)`, and every count site now reference the single vec.
- **`next_selection_after_background_change`** (dead snap-from-Background). `on_background_agents_changed`
  simplified to just `cx.notify()` (KEPT — still repaints the strip so a bg-agent's live `activity_label`
  change refreshes its teammate pill).
- **Dead methods** `SubagentView::is_parent_thread_view` + `matches_parent_entry` (no non-test caller).
- **Dead fns** `remove_background_agent` + `remove_background_shell` (zero callers post-6d-A/6d-B).
- **The unadvertised bg-agent/shell WIRE notifications:** the `emit_notification` forwards for
  `agent_session_background_{shells,agents}_changed` in `event_sources.rs` (not in `SUPPORTED_EVENT_KINDS`,
  mobile unsubscribed in 6d-B, desktop reacts to the in-process GPUI store event not the wire push) + their
  payload builders + the now-orphaned mcp.rs DTO builders (`BackgroundShellDto`/`background_shell_dto`/
  `build_background_shells_vec`/`BackgroundAgentDto`/`background_agent_dto`/`build_background_agents_vec`).
  The in-process `SessionBackground{Shells,Agents}Changed` store events + their desktop GPUI subscriptions
  STAY (the `dirty_target_session` convergence mapping still lists both, so `agent_session_dirty` is intact).

## Product note (consequence of decision 23, surfaced here)
Because 6d-B made an async `Agent` render as its `SubagentView::Task(parent_tool_use_id)` teammate pill, and
`queue_target(Task) == Main`, a LIVE async agent's tab is now **view-only** (you can no longer type a
follow-up routed to that agent — that affordance was tied to the removed `Background` pill +
`QueueTarget::Subagent`). This was already true + shipped in 6d-B (the `Background` selection became
unreachable then); 6d-tail-1 just deletes the now-dead code. `QueueTarget::Subagent` + `is_messageable` were
KEPT (still constructed at `store/queue.rs` + gate the supervisor's `has_live_background_work`), so
re-introducing "message a live async agent from its teammate tab" later is a small change, not a rebuild.

## KEPT (wire-backing or still-live) — NOT touched
`active_subagents`/`active_subagent_order`/`SubagentTab`/`build_active_subagents_vec`/`SubagentDto`/
`build_active_subagents_changed_payload` + **`SessionSummary.active_subagents` (STILL ON THE WIRE)** + the
strip's inline-Task label lookup; `background_agents`/`background_agent_order` (feed the strip's async-agent
label + the `close_stream(Teammate)` done-signal); `db.delete_background_shell` (live fs-watch reap caller);
`SubagentView::{Main,Task,Shell}` + `parent_stream_id`/`queue_target`/`QueueTarget::Subagent`/`is_messageable`.

## Remaining after 6d-tail-1
- **6d-tail-2 (needs a mini wire cutover + mobile lockstep):** remove `active_subagents` from the wire
  (`SessionSummary.active_subagents` — the one true wire dependency), then delete
  `active_subagents`/`active_subagent_order`/`SubagentTab`/`build_active_subagents_vec`/`started_at`, and
  RE-HOME the desktop strip's inline-Task friendly-label source onto the teammate `Stream.label` (enrich it
  at `rebuild_streams`/registration). This is NOT desktop-only — SessionSummary is mobile-facing — so it
  ships with a `sawe-mobile` update (drop `SessionSummaryDto.active_subagents`) + a wire bump. Optionally the
  cosmetic `SubagentView`→`StreamId` rename.
- **6e:** final docs (supersede FORK.md #38/#39, spec Phasing §6 fully ✅), `.rules` refresh, whole-branch
  migration review.

## Verification
`cargo build --bin sawe` clean; `cargo test -p solution_agent --lib` **530 passed** (was 543; −13 dead tests:
4 store + 5 session_view + 4 event_sources). `cargo clippy` byte-identical to base HEAD (no new warnings, no
`dead_code` left behind). Subagent reviewer: no blockers, 3 stale doc-comments (event_sources module doc,
status_row meter comments, `is_messageable` doc) — all FIXED in the same commit. **Desktop render gate PASSED**
(offscreen `seed_cold_session` live_teammates+live_shell): Main / teammate / shell tabs each render their own
body through the collapsed uniform `main_stream_entries_for_render` path — no blank screen, teammate scoped
out of Main, shell view-only.
