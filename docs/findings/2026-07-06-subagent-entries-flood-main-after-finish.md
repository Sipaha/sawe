# Sub-agent transcript floods the Main tab once the Task/Agent finishes

**Date:** 2026-07-06
**Status:** fixed
**Repos:** editor (`crates/solution_agent`) + mobile (`spk-editor-mobile`)

## Symptom (user, live)

> "в основную вкладку чата все еще попадают сообщения от саб агентов"

The Main chat tab shows a sub-agent's messages. Persisted across restart
(hence "все еще" — still, after the restart that carried the previous fix).

## Root cause — a "cold-restart bypass" that fired in the normal steady state

Every sub-agent (`Task`/`Agent` tool) entry is correctly stamped with its
parent `toolu_xxx` id: `subagent_id` on `AssistantMessage`/`ToolCall`, lifted
from `_meta.claudeCode.parentToolUseId` (`claude_native::translate::stamp_subagent_meta`
→ `acp_thread::subagent_id_from_meta` → `SessionEntry.subagent_id`). The DB
confirms it — for forge session `3xo8kh9i`, 83 of 219 entries carry a
`subagent_id`. So the data was never the problem; the **view filter** was.

Both surfaces gated the Main-only filter on "are there subagents active *right
now*":

- desktop `SolutionSessionView::should_render_entry` — `if active_subagents.is_empty() { return true }` (render everything).
- mobile `ChatList` — `if (!hasActiveSubagents) server` (skip the filter).

The intent was a cold-restart guard: after a restart `active_subagents` is
empty and the strip is hidden, so filtering by a now-dead `toolu_xxx` would make
those rows *vanish*. The author preferred "show them in Main" over "lose them".

But `active_subagents` is emptied the instant a Task/Agent reaches a **terminal
status** (`store.rs` ~6832 removes it), not just on restart. So the real
lifecycle is:

1. Sub-agent running → `active_subagents` non-empty → Main correctly hides its
   entries (they live in the sub-agent's strip tab). ✅
2. Sub-agent finishes → `active_subagents` empties → strip hides → **bypass
   fires → the entire sub-agent transcript floods back into Main**. ❌

Step 2 is the normal end of every sub-agent, and it survives restart. That is
exactly the leak the tab split exists to prevent.

## Fix

Main **never** shows sub-agent-tagged entries, active or not — drop the bypass
on both surfaces and always apply `matches_parent_entry` / `filterEntriesBySubagent`.

- `crates/solution_agent/src/session_view.rs` — `should_render_entry` is now
  just `self.selected_subagent.matches_parent_entry(entry.subagent_id.as_ref())`
  (also dropped the now-unused `cx` param + its one caller's arg).
- `spk-editor-mobile/.../SessionDetailScreen.kt::ChatList` — the three
  `filtered*` remembers no longer branch on `hasActiveSubagents` (param removed;
  queued-bundle visibility now keys purely on `selectedSubagent == null`).

**Nothing essential is lost.** The sub-agent's `Agent` tool *call* and its
result summary are Main-thread entries (`subagent_id == None`) — the main agent
receives the sub-agent's report as the Task `tool_result` and renders/acts on it
in Main. Only the verbose *interior* steps (the sub-agent's own reads/bashes/
narration) are hidden once it completes. When the strip empties the store snaps
`selected_subagent` back to `Main`, so Main-only is the correct steady state.

**Trade-off:** a finished sub-agent's interior transcript is no longer viewable
(its tab disappears with the strip). If reviewing completed sub-agent work
becomes desirable, the follow-up is *persistent* sub-agent tabs (mark completed,
keep in the strip) rather than reinstating the Main flood.

## Verification

- Editor `solution_agent` suite: 522 passed. `matches_parent_entry` unit tests
  (`store.rs` ~9048) already lock "Main hides tagged entries".
- Mobile `:app:compileDebugKotlin` + `:core:test` green.
- Data: DB dump of `3xo8kh9i` shows sub-agent entries are correctly tagged, so
  the filter now has the information it needs to exclude them.
- Live eyeball (desktop restart / mobile) pending on the user's instances — the
  standing live-test loop.
