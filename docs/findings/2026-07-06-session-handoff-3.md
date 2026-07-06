# Session handoff — post phase 6c (2026-07-06)

**READ FIRST on resume.** This is the pause snapshot after shipping **phase 6c** of the
per-source-streams migration. Supersedes `2026-07-06-session-handoff-2.md`.

## Commit chain since last handoff (all on `sawe` `origin/main`, pushed)

- `7aeeee7470` — **phase 6c code**: desktop strip teammate loop + selection onto
  `session.streams`; decision-#16 cold-load orphan fix; review-caught →Idle-GC
  stream-close fix. 2 regression tests.
- `7144dc94d2` — `solution_agent.seed_cold_session` gains an opt-in `live_teammates`
  flag (debug-only) so the strip screenshot gate can paint a live teammate pill.

`sawe-mobile` UNCHANGED (`origin/main` `dc1977d`) — 6c is desktop-only.

## What shipped in 6c (detail: `findings/2026-07-06-phase6c-desktop-strip-streams.md`)

Desktop tab strip + selection now read the maintained `session.streams` mirror:
- **Strip `tabs` loop** (`session_view/task_subagent_strip.rs`) iterates `session.streams`
  filtered to `StreamId::Teammate(id)` present in `active_subagents` (label from the map).
  Ordering SSOT is now `streams`; the desktop no longer reads `active_subagent_order`.
- **`next_selection_after_change`** (`session_view.rs`) takes `&session.streams`, snaps a
  removed teammate to Main only.
- **`hydrate_streams_main_only`** (`model.rs`) derives orphans from `demux(&self.entries)`.

### Two bugs fixed
- **decision-#16 (latent prod):** orphans were read from the STALE `self.streams` on
  cold-load (the 4 sites assign `entries` with no rebuild first) → zero orphans →
  decision-#9 zombie teammate tabs after restart. Now `demux(&self.entries)`. Test:
  `model::tests::hydrate_records_orphans_from_directly_assigned_entries`.
- **review-caught (regression the diff introduced):** the `→Idle` strip GC (`store.rs:8805`)
  cleared `active_subagents` but never `close_stream`'d; with the streams-only snap a
  viewer pinned to that Task stranded on a frozen, pill-less tab. GC now closes each
  cleared teammate stream. Test:
  `store::tests::idle_transition_gc_closes_stranded_teammate_stream`.

## Verification (all green)
- `cargo test -p solution_agent --lib` → **558 passed**.
- `cargo build --bin sawe` clean; clippy no findings in touched files (`script/clippy`'s
  deny-gate is blocked by a PRE-EXISTING unrelated lint in `crates/git/src/backup.rs:114`
  — not ours).
- Offscreen strip screenshot gate PASSED: Main+teammate pill, teammate-selected body,
  after-close collapse. (`/tmp/6c-shot-{main2,teammate,closed}.png`.)
- release-fast rebuilt at `7aeeee7470` (seed extension is `#[cfg(debug_assertions)]`,
  inert in release).

## LOCKED scope decision (6c = teammates-only, staged)

The full `SubagentView`→`StreamId` collapse and removal of `active_subagents*` /
`background_*_order` are DEFERRED to 6d because: `StreamId` has no `Background` variant;
async-`Agent` teammates are double-represented (live `StreamId::Teammate` stream +
separate `bg_agents` pill); and `active_subagents`/`active_subagent_order` are still on
the wire (`SessionSummary.active_subagents` via `build_active_subagents_vec` + the
`agent_session_active_subagents_changed` notification) → removing them is a wire-format
change that belongs to 6d's `wire_schema_version` bump. The `∈ active_subagents` filter on
the streams-derived `tabs` is the behavior-preserving bridge (excludes async teammates so
no double-pill; friendly labels; drift-Vec no longer read for ordering).

## Outstanding pool (the ONLY remaining migration work)

- **6d — fold shells/bg-agents into `streams` (CROSS-REPO, delicate, best fresh context).**
  Add `StreamKind::Shell` (+ bg-agent) descriptors to the wire `streams` list. WIRE bump
  `wire_schema_version` 3→4 + matching `sawe-mobile` update (drop separate shell/bg-agent
  strips) + emulator render gate (reuse the harness in
  `findings/2026-07-06-phase5-mobile-streams.md`). **Mobile push needs a one-line user
  confirm.** THEN the full `SubagentView`→`StreamId` collapse, `active_subagents*` /
  `background_*_order` field removal, and double-representation removal become clean —
  do them AS PART OF 6d (or a 6d-tail) once shells/bg-agents are streams. Smaller repeat
  of phases 4b+5.
- **6e — final docs + whole-branch review.** Supersede FORK.md #38/#39 (quick-fixes
  deleted by then), mark spec Phasing §6 fully ✅, refresh `.rules`, whole-branch
  migration review (constraint #6).

## Open architectural notes / gotchas

- The decision-#16 open follow-up from 6b is now CLOSED (fixed in 6c) — orphans populate
  correctly on the real cold-load path.
- `seed_cold_session { live_teammates: true }` paints a LIVE teammate strip for gates;
  default false = the finished/cold-load state (strip hidden). Reuse for 6d shell/bg-agent
  strip screenshots once those are streams.
- Wire bump lives at `crates/editor_mcp/src/tools/capabilities.rs`
  (`wire_schema_version`); mobile `SUPPORTED_WIRE_SCHEMA_VERSION` + `isServerTooOld` gate
  must move in lockstep (both directions already handled per phase 4b/5).

## Environment / cannot-rederive facts

- Editor push to `origin main` PRE-AUTHORIZED. Mobile push needs a one-line user confirm.
- `docs/superpowers/{specs,plans}/*` are GITIGNORED (on-disk only; spec §6 6c + the plan
  6c section + the 6c implementer brief updated on disk, not committed — expected).
  `docs/findings/*`, `FORK.md`, `docs/INDEX.md` ARE tracked + committed.
- Screenshot-gate recipe: `script/run-mcp --debug --headless` (SAWE_HOME=~/.spk/sawe-dev),
  global socket `~/.spk/sawe-dev/config/mcp.sock`, per-solution socket
  `~/.spk/sawe-dev/config/solutions/<sid>/mcp.sock` (has `seed_cold_session` +
  `workspace.screenshot`; `dump_visual_structure` is per-solution too, NOT global).
  MCP client helper: `/tmp/6b-mcpcli.py` (sock, method, params-json, [outpng]; filters
  notification/id frames). Dev solution `streams-gate` reused (window opened via
  `solutions.open`, session tab clicked via `windows.click_at` at the console-panel tab
  row). `script/run-mcp` only recompiles if the binary is MISSING → `cargo build --bin
  sawe` after any crate edit before a screenshot run. NO dev editor running now (torn down).
- Mobile: NEVER touch git-tracked `spk-editor-mobile/.superpowers/sdd/{progress.md,
  task-R-brief.md}`.

## In flight

Nothing uncommitted in tracked files. `sawe` tree clean at `7144dc94d2` (pushed).
Phase 6c DONE. Remaining: 6d → 6e.
