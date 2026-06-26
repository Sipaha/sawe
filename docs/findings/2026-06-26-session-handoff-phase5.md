# Session handoff — 2026-06-26 (mobile-delta-sync: Phase 5 complete)

**READ FIRST on resume.** Project: fix the mobile chat dialog (flicker, scroll
jumps, redundant requests) by rebuilding the sync/persistence stack. Spec:
`docs/superpowers/specs/2026-06-26-mobile-delta-sync-design.md`. Plan:
`docs/superpowers/plans/2026-06-26-phase5-delta-rpc.md`. Live ledger:
`.superpowers/sdd/progress.md` (bottom). Prior handoff (Phase 4):
`docs/findings/2026-06-26-session-handoff.md`.

Commit-per-task, **no `Co-Authored-By`**, executed via subagent-driven-development (TDD
implementer + per-task opus review + whole-phase opus review).

## Branch / push status (RESOLVED end of session)
The session originally ran on branch `task-5a-mcp-session-reads` (the prior "on main" was
imprecise; local `main` had lagged at `c4e1e19ed2`). Per user, `main` was **fast-forwarded** to the
work head `d6ec340db4` and **PUSHED to `origin/main`** (GitHub `Sipaha/sawe`,
`583521c9a8..d6ec340db4`). `main` is now the home branch and synced with origin; the stray
`task-5a-mcp-session-reads` pointer remains at the same commit (offer to delete). **Resume on `main`.**

## Phase numbering (spec reordered in execution)
my Phase 3 = seq-stamping · Phase 4 = transcript-as-DB-rows · **Phase 5 = delta RPC
`get_session_changes` (THIS session, COMPLETE)** · Phase 6 = Kotlin mobile client (NEXT).

## Commit chain this session (all reviewed; merged to `main` + pushed to `origin/main`)
```
9e4cb7deaa correct the change_seq durability comments        (whole-phase review fixes)
a1091efdaa expose the delta cursor and RPC                    (5.3: GetSessionResult epoch/current_seq + GLOBAL_TOOLS/allow_list wiring + reset-event)
1edd047209 add the get_session_changes delta RPC             (5.2)
37566235c5 guard persisted change_seq against write reorder   (5.1b review fix: max()-guarded UPDATE)
a55349c1c5 persist change_seq for a restart-monotonic cursor  (5.1b)
6f9a5932e5 add per-section delta watermarks                   (5.1)
```
Phase base `f831cb5975` .. head **`9e4cb7deaa`**. 412 lib + 2 e2e + remote_control 60/2/1 +
editor_mcp lib 17 green; clippy 5 baseline (pre-existing editor_mcp full-suite failure
`run_config_create_list_delete` confirmed unrelated via stash+rerun on base).

## What Phase 5 shipped (the delta protocol, server side)
- **Per-section watermarks** `queue_seq`/`subagents_seq`/`state_seq` on `SolutionSession`, each
  set to a fresh `bump_change_seq()` at its section's mutation via centralized store helpers
  (`mark_queue_changed`/`mark_subagents_changed`/`mark_state_changed`). `state_seq` moves ONLY on
  genuine `SessionState` discriminant changes (model/effort/queue-only sites keep a bare
  `SessionStateChanged` emit).
- **`change_seq` is now PERSISTED** (mirrors the `epoch` column; `max(COALESCE,?1)`-guarded
  UPDATE), flushed on every advance, restored on cold load as the watermark-seed anchor (legacy
  null → `max(mod_seq)`). This fixed a latent protocol bug (see decision 1 below).
- **`get_session_changes` RPC**: `changed_entries` (`mod_seq > since_seq` + subagent filter) via the
  shared `summarize_entry` with byte-identical image-cursor parity to `get_session`; each section
  (state/pending_bundles/active_subagents) `Some` iff its watermark `> since_seq`; `reset:true` on
  epoch mismatch; `removed_indices` empty under the tail-truncate model (shrink rides `total_count`).
  PURE-READ — never bumps/persists.
- **Cursor seed + reachability**: `GetSessionResult` gained `epoch` + `current_seq`; the tool is
  wired into `editor_mcp` `GLOBAL_TOOLS`/`SHARED_TOOLS` + `remote_control::allow_list` so the mobile
  proxy can reach it. The existing `agent_session_context_reset` push is the reset trigger — **no
  new push event added**.

## Key architectural decisions (scrutinized by whole-phase review, all cleared)
1. **Restart-monotonic cursor (the load-bearing one).** Section bumps push `change_seq` ABOVE
   `max(mod_seq)`, and the RPC hands the client `current_seq = change_seq`. Reseating `change_seq`
   from `max(mod_seq)` on restart (the pre-Phase-5 behavior) would drop it BELOW an issued cursor →
   new entries get `mod_seq < since_seq` → **silently lost from deltas**. Fix: persist `change_seq`,
   restore it, seed the 3 watermarks above it (`seed_change_seq(anchor) = anchor+3`, deterministic
   so re-derived identically each restart). Entries are NOT reloaded (honors the Phase-4 no-epoch-
   bump-on-restart decision); ephemeral sections self-heal because the seed lands the watermarks
   strictly above any issued cursor.
2. **`removed_indices` empty.** Transcript only appends / in-place-updates / tail-truncates; the
   client clamps its list to `total_count`. Kept in the wire schema for forward-compat.
3. **`wire_dict.rs` deliberately NOT touched.** It is a DEFLATE preset dictionary Adler-32-pinned
   cross-language to the Kotlin `WireDictionary.kt`; any byte change breaks the pin + desyncs repos.
   Dictionary membership is compression-ratio-only, NOT reachability. Adding the method needs a
   coordinated v2 bump — out of scope. `get_session_changes` already back-references the shared
   `remote.solution_agent` prefix.

## Offscreen verification (real running editor, real DB data — PASS)
`script/run-mcp --debug --headless`, dev solution `btest`, legacy-migrated session `0i3d559y`.
Via `/tmp/p5verify.py`: tool registered (64 tools). `get_session`→ epoch=1, current_seq=8 (=max
mod_seq 5 + 3 seed), 5 entries. delta `since_seq=8` (cursor) → reset=F, changed=0, all sections
OMITTED (no redundant resend). delta `since_seq=0` → changed=5, indices 0-4, MATCHES get_session.
delta `since_seq=6` → changed=0. delta `known_epoch=100` → reset=T, changed=0, sections absent,
real epoch returned. Dev instance killed at end.

## Carried Minors (triaged acceptable)
- `resume_session` re-entity path sets `state=Idle` without an explicit `state_seq` bump — self-
  resolves via the `restore_change_seq` seed (bumps all 3 watermarks). Note, not a fix.
- 5.2 cosmetic: state-anchor (`*_started_at_ms`) computed before the `state_seq > since_seq` gate —
  fold into the `.then(||..)` closure someday. Trivial.

## NEXT (resume here) — Phase 6: Kotlin mobile client
Spec § "Client" + `.agents/survgpy5/c05/next.md`. Repo: `spk-editor-mobile`
(`/home/spk/.spk/sawe/ss/spk-solutions/spk-editor-mobile`). `SessionDetailStore.kt`: cache-first
open (render from disk cache storing `(epoch, last_seq)`, then one `get_session_changes`); the delta
applier becomes the SINGLE writer of entries/queue/subagents/state; push events become debounced poll
triggers; remove `resumeSession(after_index)`/`resyncLatestEntryContent`/`healIncompletePlaceholders`/
per-notification `fetchAndReplaceEntry`; fallback poll only while Running. Pure delta-applier testable
in `:core`. Done when: queued bubble stable across ticks, scroll holds, `/clear` reloads, an in-place
edit of an old desktop entry propagates — user verifies on device. **Resume in a FRESH context**
(this session ran the full Phase 5 + reviews + verification — deep context).
