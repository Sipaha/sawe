# Session handoff — 2026-07-06 (post phase 6b)

**READ FIRST on resume.** Supersedes `2026-07-06-session-handoff.md`.

## Where we are
Per-source-streams migration. Phases 1–5 shipped (hard cutover, `wire_schema_version` 3).
**Phase 6b (the keystone) is now DONE + SHIPPED + verified.**

Commit chain since the last handoff (all on `sawe` `origin/main`, pushed):
- `306ca1af5f` — **phase 6b code**: persist authority → `streams[Main]`, revert #3.
- `fdeb268f06` — **phase 6b docs**: FORK.md #39 reverted, findings, INDEX row.

`sawe-mobile` unchanged (`origin/main` `dc1977d`) — 6b is server-only.

## What 6b shipped
- Persistence authority moved from flat `session.entries` to `streams[Main].entries`
  (Main-LOCAL index, subagent_id None) via a seq-watermark incremental persist
  (`persisted_main_seq`). `persist_all_rows` + the 3 ingest arms rewired; flat-index
  `persist_upsert_range`/`_entry`/`_delete_from` removed.
- Quick-fix #3 REVERTED (`AcpThread::coalesce_target_index` deleted → naive
  `entries.last()`); the un-tear guarantee now lives in `stream::push_coalesced` /
  the demux reunite test.
- Decision-#11 rewind re-stamp re-homed onto the Main stream.
- Flat `entries` KEPT as the 1:1 `AcpThread` ingest mirror + demux input (full removal
  deferred — it's not needed for #3-revert safety; the remaining flat readers stay
  1:1-aligned with `AcpThread`). See decision #15.

## Two review-caught bugs, both fixed in 6b (decision #16)
1. **Append-race**: unconditional `delete_entries_from(main_len)` + GPUI's NON-FIFO
   detached DB writes → a later append's upsert can land before an earlier link's
   stale delete → lost row. Fixed by SERIALIZING per-session persist writes
   (`entries_persist_chain`: capture plan synchronously, `prev.await` before DB ops).
2. **Legacy cold-load realign**: pre-6b teammate-tagged rows at GLOBAL indices don't
   match Main-local; the skip-opt would overwrite a Main slot + strand a phantom
   tagged row. Fixed: `hydrate_streams_main_only` seeds `persisted_main_seq = 0` when
   `entries.len() != streams[Main].len()` → first persist rewrites the whole Main
   stream Main-local + trims.

## Verification (all green)
- `cargo test -p solution_agent --lib` → **556 passed** (+2: torn-persist + legacy-realign).
- `cargo test -p acp_thread --lib` → 80 passed, 1 pre-existing unrelated failure.
- clippy clean on touched files. Live torn-message render gate re-passed offscreen
  (interleaved seed → one coalesced Main bubble, teammate on its own stream;
  `get_session` Main total_count=2, teammate a separate stream).
- **release-fast** rebuilt at HEAD (`target/release-fast/sawe`) for the user's hands-on test.

## Outstanding pool (phase 6 remaining) — per `docs/superpowers/plans/2026-07-06-per-source-streams-phase6-cleanup.md`
- **6c (NEXT)** — unify desktop strip onto `session.streams`; drop `SubagentView`
  variants + `active_subagent_order`/`background_agent_order`/`background_shell_order`
  parallel vecs; simplify `next_selection_after_change` (snap-to-Main on stream_removed).
  Render-coupled (`session_view`). May stage teammates-only then finish shells in 6d.
  Gate: desktop strip offscreen screenshots (Main + teammate + after-close).
- **6d** — fold shells/bg-agents into `streams` = CROSS-REPO wire bump 3→4 + `sawe-mobile`
  lockstep + emulator render gate (harness recipe in `-phase5-mobile-streams.md`).
  Mobile push needs a one-line user confirm. Delicate — best fresh context.
- **6e** — final docs (supersede FORK.md #38/#39), spec §6 ✅, whole-branch review.

## Open architectural question flagged in 6b (verify in 6c/6d)
`hydrate_streams_main_only` records `hydration_orphan_streams` from the PRE-rebuild
`streams` snapshot, but real cold-load sites assign `s.entries=…` directly → the
snapshot is Main-only-empty → NO orphans recorded on the production path (model tests
hide this via `set_entries`-first). If truly empty on real cold-loads, decision-#9
zombie-teammate suppression may not fire in production. 6b's realign was made
independent of the orphan set. **Investigate + fix if confirmed.**

## Active gotchas
- Detached DB writes are NOT FIFO (memory `solution-agent-detached-db-writes-race`) —
  any new `delete_from`-style persist must serialize or be order-independent.
- `script/run-mcp` won't recompile a stale binary — `cargo build --bin sawe` after any
  crate edit before an MCP screenshot run.
- acp_thread's `test_checkpoint_shows_when_file_changes_during_pending_message` is a
  PRE-EXISTING unrelated failure (documented in `ff50156359`) — not a regression.
