# Session handoff — 2026-06-26 (mobile-delta-sync: Phase 4 complete)

**READ FIRST on resume.** Project: fix the mobile chat dialog (flicker, scroll
jumps, redundant requests) by rebuilding the sync/persistence stack. Spec:
`docs/superpowers/specs/2026-06-26-mobile-delta-sync-design.md`. Live ledger:
`.superpowers/sdd/progress.md` (bottom). Prior per-context memory:
`.agents/survgpy5/c04/{state,decisions,next}.md`.

Work is on `main` (user's explicit choice — no feature branch), **NOT pushed**,
commit-per-task, **no `Co-Authored-By`**. Executed via subagent-driven-development
(TDD implementer + per-task review + opus whole-phase review).

## Phase numbering (spec reordered in execution)
my Phase 3 = seq-stamping (done last session) · **my Phase 4 = transcript-as-DB-rows
(THIS session, COMPLETE)** · Phase 5 = RPC `get_session_changes` (NEXT) · Phase 6 =
Kotlin client.

## Commit chain this session (all reviewed, on `main`, NOT pushed)
```
f831cb5975 serve closed-session history and resumed model from rows   (whole-phase-review fixes)
073f665d7c remove the dead transcript blob write path                  (T5b)
2a47da75f7 serve MCP session reads from the unified entry model        (T5a)
c4e1e19ed2 backfill model and effort columns on transcript migration   (T4 review fix)
5ec3d64e9f load transcript from rows with lazy blob migration          (T4)
a28882f5c2 test transcript row clearing on context reset               (T3 review fix)
7ebaba0d2a persist transcript as incremental entry rows                (T3)
3364134fa8 persist session model and effort as metadata columns        (T3a, NEW task)
854a9f720b add per-entry DB row primitives                             (T2)
d88778b77f add solution_session_entries table, epoch column, codec     (T1, prior session)
```
Phase base `f5f620a3b8` .. head **`f831cb5975`**. 397 lib + 2 e2e tests green; clippy 5 pre-existing.

## What Phase 4 shipped (transcript: JSON blob → per-entry DB rows)
- `solution_session_entries` table (session_id, idx, mod_seq, created_ms, subagent_id,
  payload=serialized SessionEntryKind) + `epoch` column; row primitives in `db.rs`.
- Incremental row writes on each `handle_acp_event` mutation (NewEntry/EntryUpdated/
  EntriesRemoved) + `persist_all_rows` at close/cold_close/reset/rotate. Throttle now
  governs ONLY the MCP emit (blob flushes removed). Stale-row trap closed at reset/rotate.
- Row-based cold load + **lazy blob→rows migration** at all 4 cold-load sites (resume_session,
  restore_open_tabs, hydrate_all_for_solution, load_cold_blob_into_session); reuses
  `cold_entries_from_persisted`+`rebuild_entries` (handles v2 AND legacy v1). Rows branch
  READS persisted epoch (no bump); migrate branch writes rows + backfills model/effort columns.
- MCP `get_session`/`get_session_entry`/`read_session_history` repointed to serve from
  `session.entries` (was `cold_persisted_v2`+live thread). Dead blob WRITE machinery deleted
  (`serializable_snapshot`, `persist_session_blob`, `cold_persisted_v2` field). Blob READ path
  KEPT (legacy migration source — `acp_thread_blob` column not dropped).

## Key decisions made THIS session (architect-level, beyond spec)
1. **model/effort/cached_models → metadata columns** (new Task 3a). The blob also carried
   `desired_model`/`desired_effort`/`available_models`; deleting the blob would drop them.
   User approved moving them to `solution_sessions` columns (COALESCE non-clobber; cached_models
   JSON, NULL-when-empty). Cold restore reads column-first with blob fallback.
2. **Task 5 split into 5a (MCP read repoint, regression fix) + 5b (blob-write deletion).**
   Phase 2 only repointed the DESKTOP render to `session.entries`; the MCP layer was left on
   `cold_persisted_v2`+live, so Task 4 emptying `cold_persisted_v2` for row-native sessions
   silently broke `get_session` (empty transcript for cold/resumed sessions — the mobile path).
   5a fixed it AND is Phase-5 groundwork.
3. **Blob NOT nulled in migration** (kept as read-path safety + migration source). The blob
   column stays read-only; a future task may drop it once confident no legacy blobs remain.
4. **Image fidelity tradeoff (5a):** USER images preserved (raw `UserMessage.chunks`);
   ASSISTANT/TOOL images degrade to `spk-image://N` markdown links (no base64) — SessionEntry
   flattens those to markdown. Acceptable (claude doesn't emit images); documented. Enrich
   `SessionEntry::{AssistantChunk,ToolCall}` to retain raw image blocks only if real fidelity needed.

## Offscreen verification (done — on REAL legacy data)
`script/run-mcp --debug --headless`, dev solution `btest` (3 legacy blob-persisted sessions).
get_session on cold `0i3d559y` → 5 correct entries (the 5a fix on real data). Screenshot
(`/tmp/p4-coldrestore.png`): console renders restored session (markdown, code block, status row
w/ restored tokens/model/effort). DB post-migration: rows 5/5/4 (mod_seq 1..n), epoch=1, blob
PRESERVED, model/effort NULL (legacy never set). Live blob→rows migration confirmed end-to-end.

## Active gotchas / carried Minors (triaged acceptable by opus whole-phase review)
- Screenshot tool in this `sawe` build is **`windows.screenshot {window_id}`** (NOT
  `workspace.screenshot {solution_id}` as CLAUDE.md says); 63 MCP tools in this build.
- epoch NOT bumped on `EntriesRemoved`/rewind (rewind is dead-for-claude per decisions GOTCHA).
  Reconcile if Phase 5/6 relies on epoch for shrink detection (else client uses mod_seq gap).
- Sub-ms upsert/delete race in `persist_all_rows` (overlapping callers don't append — narrow).
- `cold_persistence::to_persisted` is production-dead, kept only for a self-test (annotated).
- T4 migrate-site comments at resume_session/load_cold_blob mildly overstate model/effort flush.
- Phase-2 index forward-risk RECONCILED: EntryUpdated uses global_idx = live_base+local; NewEntry
  persists [first_new..]. (Still relevant for Phase 5 delta building on these indices.)

## NEXT (resume here) — Phase 5: RPC `get_session_changes`
Spec § "Server sequence + delta" + `.agents/survgpy5/c04/next.md` item 2. Add the delta RPC:
params (session_id, since_seq, known_epoch, subagent_filter); diff each section vs since_seq;
`changed_entries` = entries.filter(mod_seq > since_seq && passes filter); `removed_indices`;
`reset` on epoch mismatch. Add section watermarks (queue_seq/subagents_seq/state_seq). Reset/epoch
push event (`agent_session_context_reset` already in capabilities). The MCP layer NOW serves from
`session.entries` (5a) — build the delta on that. Then Phase 6 (Kotlin `SessionDetailStore`:
cache-first single-writer delta applier). Recommend resuming in a FRESH context (this session ran
the full Phase 4 + reviews + verification — deep context).
