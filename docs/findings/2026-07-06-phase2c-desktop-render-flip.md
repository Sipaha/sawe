# Phase 2c — desktop renders the demux'd selected stream

**Date:** 2026-07-06
**Area:** `crates/solution_agent/src/session_view.rs`
**Spec:** `docs/superpowers/specs/2026-07-06-per-source-streams-design.md` (Phasing §2c)

## What changed

The desktop conversation render stopped iterating the flat, global-indexed
`session.entries` + a per-entry `should_render_entry` filter. It now iterates
the **selected stream's** already-demux'd, already-coalesced entries from
`session.streams` (the maintained mirror landed in phase 2b-redo):

- New frame-local field `main_stream_entries_for_render: Vec<SessionEntry>`,
  populated at the top of `Render::render` by
  `build_main_stream_entries_for_render` from
  `session.streams[selected]` (`Main → StreamId::Main`,
  `Task(toolu) → StreamId::Teammate(toolu)`; empty for drill-in views). The
  non-drill-in render path — `collect_entry_texts`, `entries_count`, the item
  processor's entry lookup / `entry_count` / `entry_ms`, and the `list_state`
  sizing — all index THIS vec. `recompute_rewind_table` / `recompute_matches`
  read `session.streams` directly (they can fire from `on_thread_event`, before
  the next render refreshes the field), but index the same per-stream space.
- `should_render_entry` is **deleted** — the stream is single-source by
  construction, so no render-time Main/Task filtering.
- `list_state` becomes the **render authority** for row count:
  - a full `SubagentView` key (`prev_render_view`, generalised from the old
    drill-in-only `prev_render_drill_in`) resets + tail-anchors on ANY tab
    switch (each stream has its own per-stream index space, so counts differ);
  - an **unconditional** reconcile (grow/shrink by TAIL-splice, preserving the
    scroll anchor) fixes same-view count drift (live append, cold hydrate,
    rewind shrink).
- `on_thread_event` no longer touches `list_state` (the `NewEntry`/
  `EntriesRemoved` splices, the `EntryUpdated`/`ToolAuth` remeasures, and the
  `cold_offset` global-index math are gone). It only refreshes the
  index-derived caches (rewind table, find matches) and `cx.notify()`s.

## Why it's correct

- **Blank rows gone.** The old model sized `list_state` to
  `session.entries.len()` (Main + teammate rows) and rendered teammate rows as
  0-height `Empty` under Main. Now `list_state` is sized to the *selected
  stream's* count, so a teammate present adds no phantom slots to Main.
- **Streaming height still grows without an explicit remeasure.**
  `list.rs::layout_items` re-renders + re-measures every VISIBLE item on every
  layout pass (only overdraw-band items reuse cached size). A streaming
  assistant message is always visible/tail-followed, so it re-measures each
  frame; `cx.notify()` (kept) schedules that frame. This is exactly how the
  drill-in (shell) streaming path already worked with no remeasure.
- **`markdown_cache` needs no re-key.** `ensure_markdown` validates by text
  (`cached.source != source → replace`), and the render-top retain
  (`retain(|(idx,_),_| idx < entry_count)`) prunes keys past the current
  stream length. On a Main↔Task switch the numeric indices collide but the
  text-validation rebuilds them (worst case: one rebuild on shifted rows) — the
  same self-heal Main↔Background already relied on. The old EntriesRemoved
  global-index cache retain was dropped (mis-indexed against the new per-stream
  keys and redundant with the render-top retain).

`sync_thread_subscription` still sizes `list_state` on thread swap using the
flat count; that's now belt-and-suspenders — the unconditional render reconcile
corrects it to the selected stream's count on the same frame (list_state slots
are identityless placeholders; the processor owns the slot→entry mapping).

## The three shipped quick-fixes (#1/#2/#3) stay live

Phase 2c does not remove FORK.md #38/#39 — they keep the flat-model render and
the mobile client correct during the migration. Phase 6 removes them once
`streams` fully owns rendering on both clients.

## A prerequisite bug the flip exposed: cold-load didn't rebuild the mirror

Phase 2b-redo synced `streams` after `set_entries` + the 5 store mutation sites,
but the **four cold-load / hydration paths** (`store.rs` ~3584/4389/4683/5013)
assign `session.entries = entries` directly and did NOT call `rebuild_streams`.
Invisible in 2b (nothing read `streams`); a **blank-render regression** in 2c
(the render reads `streams`, which stayed Main-only-empty → a session restored
from the DB would paint nothing). Fixed: all four now `rebuild_streams()` after
the assignment. The invariant is now uniform — every `entries` writer maintains
the mirror.

## Verification

- `cargo test -p solution_agent --lib` — 538 pass, incl. three new tests:
  - `store::…::subagent_view_parent_stream_id_maps_main_and_task` — the
    `SubagentView → StreamId` mapping.
  - `model::…::selected_view_streams_split_main_and_teammate` — end-to-end:
    Main excludes the teammate, Task shows only it, with coalescing.
  - `session_view::…::render_sizes_list_state_to_selected_stream_not_flat_entries`
    — DRAWS the view (`VisualTestContext`): with 5 flat entries (Main+teammate
    interleaved) `list_state.item_count()` == 2 on Main (no phantom slots) and
    == 1 on the teammate tab. Directly asserts the "no blank rows" invariant.
- **Offscreen screenshot gate (release path):** drove the real `--headless`
  editor over MCP with a debug-only seed tool (`solution_agent.seed_cold_session`)
  + a new `windows.scroll_at` primitive. Seeded a Main-with-Task-teammate
  transcript, then screenshotted:
  - Main head (`[MAIN Q1]` + date header), the A4 → "Dispatching a Task
    teammate" → `[MAIN Q5]` → A5 boundary (the three `[TEAMMATE …]` interior
    entries entirely absent, "Dispatching" adjacent to Q5 with no gap), and the
    tail (`[MAIN A8]` flush above the status row — no trailing blank space).
  - **Restored-from-DB session**: persisted the seed, restarted the editor,
    reopened → cold-load path → the `Restore Check` session renders Main intact
    (NOT blank), independently confirming the hydration fix.
- release-fast rebuilt (with the hydration fix) at `target/release-fast/sawe`.
