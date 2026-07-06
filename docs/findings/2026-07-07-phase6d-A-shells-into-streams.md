# Phase 6d-A — fold background shells into `session.streams`

**Date:** 2026-07-07 · **Commit:** `ed335daa49` · **Crate:** `solution_agent`.
**Status:** ✅ shipped to `sawe` `origin/main`. `sawe-mobile` UNCHANGED (`dc1977d`) — 6d-A is
desktop + in-process-model only and WIRE-INVISIBLE.

## What shipped

Background shells render as ordinary `session.streams` tabs (kind `Shell`) instead of the
separate `background_shell_order`-driven strip + drill-in. `background_shells` stays the DATA
source (registration/watcher/snapshot untouched); only the RENDER moved to streams.

- `BackgroundShell::stream_entry(now)` / `stream_label()` (`background_shell.rs`) — cx-free
  normalizer: the fenced-output `SessionEntry` (plain `AssistantChunk::Message`, no `Markdown`
  entity) + the `<short>·<command>` pill label. Moved out of the view layer so
  `rebuild_streams` (which has no `cx`) can stamp them. `created_ms`/`mod_seq` derive from the
  snapshot mtime-ms so the per-stream `seq` advances when output changes (the 6d-B delta cursor).
- `rebuild_streams` (`model.rs`) folds each **Running** shell into `streams` as a
  `StreamId::Shell` stream, inserted after Main+teammates. Terminal (`Exited`/`Killed`) shells
  are **skipped** — a shell stream exists only while `Running`, which IS the product's
  **auto-close** (the dismissible terminal-× UX is dropped — user confirmed they never relied
  on it and never even saw shell tabs). Derived here from the map (single maintainer); every
  shell mutation site (registration / `refresh_background_shell_snapshot` /
  `mark_background_shell_state` / `tick_background_shells` reap / `remove_background_shell`) now
  calls `rebuild_streams`.
- Desktop strip renders shell pills from `session.streams` (`StreamId::Shell` filter); deleted
  the `bg_shells` map loop + `ShellDisplayState` / `classify_background_shell_display` /
  `background_shell_pill`.
- `SubagentView::Shell(id).parent_stream_id()` → `Some(StreamId::Shell(id))`, so a selected
  shell body renders through the SAME `selected_parent_stream_entries` path as Main/teammates;
  deleted `build_background_shell_entries_for_render` + its two cache fields +
  `build_shell_drill_in_entries` + the `is_shell`/`is_shell_inner` render branches. Shell stays
  **view-only** (`compose_disabled_for`).
- `next_selection_after_change` handles `Shell` (snap to Main on stream removal);
  `next_selection_after_shells_change` deleted.
- Wire kept **v3-byte-identical**: `build_streams_vec` filters `StreamKind::Shell`, and
  `get_session`/`get_session_changes` coerce a `Shell` `stream_id` request to Main. The separate
  `get_session_background_shells` tool + DTO + `event_sources` payload are UNTOUCHED (mobile
  still uses them until 6d-B).
- `seed_cold_session` gained a debug-only `live_shell: Option<String>` param for the gate.

## Verification

- `cargo test -p solution_agent --lib` → **553 passed** (net -5 vs 6c's 558: deleted the shell
  classifier + `next_selection_after_shells_change` tests, added shell-fold/auto-close/
  survives-rebuild/stream_entry/stream_label/selection tests).
- `cargo build --bin sawe` clean; `cargo clippy -p solution_agent --all-targets` no findings in
  touched files.
- **Offscreen screenshot gate PASSED** (headless, `streams-gate` dev sol, `live_shell` seed):
  (1) Main selected → strip `Main` + shell pill "seedshell·cargo build…" (terminal icon, accent,
  no ×); (2) shell tab selected → body renders the fenced output via the unified stream path,
  status "Running", compose "View only · switch to Main to send". `/tmp/6dA-shot-{main,shell}.png`.
- release-fast rebuilt at `ed335daa49` for the user's hands-on test.

## Review (no blockers)

Reviewer confirmed: injection Running-only + skips-terminal (auto-close), all 5 mutation sites
rebuild, render seam correct, wire byte-identical, auto-close-while-selected snaps to Main
cleanly. Fixed 2 flagged stale doc comments (`selected_parent_stream_entries` +
strip module header). Deferred (noted): `remove_background_shell` is now uncalled (× removed) —
delete it in 6d-tail with the other field cleanup (avoid DB-plumbing scope creep here);
`is_parent_thread_view` now test-only (inert). Nit accepted: `rebuild_streams` reformats a
Running shell's "observed X ago" header on every Main tick — local repaint cost only, fine for
6d-A scope.

## Next: 6d-B (the big cross-repo cutover) — see handoff-4

Fold background AGENTS onto their demux `Teammate` stream (drop the JSONL `Background` pill +
the `∈ active_subagents` bridge; async agents show as teammate streams; JSONL demoted to
completion-signal + archival per spec Decision 3 — USER APPROVED the content reduction), REMOVE
the `StreamKind::Shell` wire filter + the separate `get_session_background_{shells,agents}` tools,
bump `wire_schema_version` 3→4, and update `sawe-mobile` in lockstep (delete `BackgroundShellStrip`
+ `BackgroundAgentStrip` + their RPCs/notifications/StateFlows, fold into the `streams`-driven
`SubagentTabStrip`, bump `SUPPORTED_WIRE_SCHEMA_VERSION` 3→4, add a Shell Roborazzi fixture) +
re-run the emulator render gate. **Mobile push (and the coordinated v4 server push) need a
one-line user confirm.** Then 6d-tail: `SubagentView`→`StreamId` collapse + remove
`active_subagents*`/`background_*_order`/`remove_background_shell`.
