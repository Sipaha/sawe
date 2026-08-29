# Persist-chain teardown: drain a soft close, abandon a hard purge

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** closing a chat tab or a Solution window must **flush** the session's in-flight entry writes instead of cancelling them, while a hard purge keeps cancelling them — and neither may truncate a legacy row layout.

**Crate:** `solution_agent` only. No UI, no schema change, no migration.

---

## What recon settled (authoritative — do not re-derive)

A read-only recon pass answered the question that had blocked this item for two sessions. **The legacy row layout is not the blocker it was believed to be**, and the real hazard is somewhere else entirely.

### The chain

1. `entries_persist_chain: HashMap<SolutionSessionId, Task<()>>` (`store.rs:289`, init `:806`) serialises entry-row writes per session so each `upsert…` + `delete_entries_from(main_len)` pair applies in issue order — GPUI detached tasks have no FIFO guarantee (the "phase-6b keystone bug").
2. Two append sites: `persist_all_rows` (`store.rs:3709`, remove `:3738`, insert `:3759`) and `persist_main_stream` (`store.rs:3775`, remove `:3807`, insert `:3828`).
3. **Dropping the map entry cancels the WHOLE chain, not just the last link.** `prev` is moved *into* the new task's future, so dropping the newest `Task` drops its future, which drops `prev`, recursively. A `Task` in a field is cancel-on-drop (`crates/scheduler/src/executor.rs:288-295`).
4. **The flush future captures no entity.** `rows` / `len` / `epoch` / `change_seq` are snapshotted synchronously before the spawn (`store.rs:3724-3735`, `:3789-3801`); `db` is an `Arc<SolutionAgentDb>`; `_this` / `_cx` are unused. **This is what makes `.detach()` viable at all** — a detached link cannot touch a dropped entity or read stale in-memory state.

### The drop site and its two incompatible callers

5. `evict_session_runtime_maps` (`store/teardown.rs:456-470`) removes the chain at `:463` with the comment *"a hard teardown abandons any in-flight entry-row write (the session's rows are being purged anyway)"*. **That comment is the bug in prose:** it is true for `purge_session_hard` and false for `close_session` and `cold_close_solution`, which route through the same function.
6. Exactly two call sites: `cold_close_solution` (`teardown.rs:407`) and `teardown_session_runtime` (`teardown.rs:548`), the latter reached from `close_session` (`teardown.rs:604`, soft — keeps rows, sets `closed_at`) and `purge_session_hard` (`teardown.rs:70`, hard — `db.purge_session` at `:104`).
7. **Nothing flushes before eviction today.** Both `close_session` (`:636`) and `cold_close_solution` (`:393`) *issue* `persist_all_rows`, but the new task is inserted into the map and removed again **inside the same synchronous `Context<Self>` block**, before the foreground executor polls it. The existing comments at `teardown.rs:360-371` and `:610-620` say this was verified empirically by probe.

### The legacy row layout — settled

8. **It is a data shape, not a schema difference.** `solution_session_entries` is created once (`db.rs:253-266`) with `subagent_id TEXT` nullable from the start. There is **no** migration, no version column, no backfill, no `ALTER` touching this table — verified by an exhaustive read of `db.rs:105-360`.
9. **A legacy row set is one written by a pre-2026-07-06 build** (`306ca1af5f`), i.e. rows keyed by *global flat* indices over `session.entries`: teammate-tagged rows interleaved into the same `idx` space, and/or un-coalesced assistant fragments in separate slots.
10. **Detection is by count at cold load**, `model.rs:1018-1046`: `persisted_main_seq = if entries.len() == main_len { streams[Main].seq } else { 0 }` — "flat mirror longer than Main ⇒ legacy". Two in-memory numbers; the DB row count is never consulted.
11. **Legacy rows CAN still exist on disk today** (high confidence). The "migration" is lazy and conditional on a *write*, and every effective persist call site requires a live `acp_thread` — so a session cold-restored and never resumed since 2026-07-06 still has pre-6b rows.
12. **But the realign is already shipped, tested and intended.** `hydrate_streams_main_only` deliberately arms the same `delete_entries_from(main_len)` at cold-load time (`model.rs:1040`), and `legacy_teammate_tagged_rows_realign_to_main_local_on_cold_load` (`store/tests/hydration.rs:1445`, assertion `:1564`) is green and asserts the teammate rows *are* deleted. **Repairing the flush does not invent a truncation; it extends an accepted one to sessions that were merely restored.** The loss it causes is muted: a cold-restored teammate stream is not rendered anyway (`model.rs:1012`, `:813-825`) — the visible cost is a teammate pill reopening without its pre-resume history.
13. So the gate we want is not "protect the legacy layout" but **"don't rewrite a session the user never touched"** — which is exactly what `cold_close_solution`'s `is_live` check already does.

### The real blocker

14. **`purge_session_hard` orders `evict` (`:83`) BEFORE `db.purge_session` (`:104`)**, both unordered background tasks over the same connection. Detach the chain generically and a persist link can execute **after** the purge DELETE and **re-insert rows for a session that no longer exists in `solution_sessions`** — permanent orphan rows, invisible to every UI (nothing enumerates entry rows without a session row) and never GC'd. The current `drop` is what prevents this. Same hazard at `purge_solution_fully` → `db.delete_for_solution` (`:160`). **Any fix that changes `evict` generically ships this bug.**
15. **`close_session` lacks `cold_close_solution`'s `is_live` gate** (`teardown.rs:387-395`) and calls `persist_all_rows` unconditionally at `:636`. Today that is unobservable because the flush is cancelled either way. **The moment the chain is repaired it becomes a live second bug** — and it points the opposite way: bug 1 is *loss of writes that should happen*, bug 2 is *execution of a rewrite that should not*. Concretely: closing a restored, never-resumed chat tab would run `delete_entries_from(main_len)` against a legacy row set.
16. **Deferred eviction is unsound.** "Spawn a task that awaits the chain, then evicts" races a re-open: `hydrate_all_for_solution` can re-key the same `SolutionSessionId` (close→reopen is a normal action, `teardown.rs:345-350`) and insert a *new* chain before the deferred evict runs, which would then drop a live chain. Hand the `Task` off **by value with `.detach()` at the synchronous eviction point** so the map key frees immediately.
17. **Ordering detail, the fiddliest part of the change:** `close_session` calls `persist_all_rows` at `:636` and *then* `teardown_session_runtime` at `:638`, which evicts internally at `:548`. But `teardown_session_runtime` also calls `cancel_turn` (`:498-501`), which can emit further ACP events and therefore issue **another** persist. The chain preserves their order, so it is safe — but the detach must happen **after** `teardown_session_runtime`'s cancel work, or the cancel-induced link is orphaned.
18. **`persisted_main_seq` is advanced synchronously before the spawn** (`store.rs:3731`, `:3800`) and every persist filters `mod_seq > watermark`. So "skip the flush, a later persist catches up" is unsound — there is no later persist that re-picks those rows. **The flush must run or the data is gone.**

### Blast radius and the silence

19. **There is no persist debounce.** `persist_main_stream` runs on every ingest event (`store/acp_event.rs:162`, `:661`, `:848`); the 500ms/2s `entry_update_throttles` govern only the MCP emit, as `acp_event.rs:842-845` says explicitly. The doc comments' phrase "un-debounced tail" is a misnomer.
20. Loss is the whole cancelled chain — typically the tail of the current streaming message plus an entry, but bounded only by how far the event stream has outrun sqlite. It is **permanent** (see 18) and **completely silent**: `teardown_session_runtime` logs dropped queued messages (`teardown.rs:513-524`) but there is no log anywhere near `entries_persist_chain.remove`.

### Out of scope, recorded so it is not lost

21. **App quit loses everything in flight with no flush attempt at all.** There is no `on_app_quit` hook in `solution_agent`; the store global drops with the process. This is a *separate, larger* bug than the one this plan fixes — adding a quit-time drain is new scope (and the chain runs on the **foreground** executor, so a detached link needs the app to keep pumping, which quit does not). Recorded as a new pool item.

---

## Rulings (binding)

- **Disposition is chosen per caller, never inferred inside `evict`.** `evict_session_runtime_maps` takes an explicit `ChainDisposition::{Drain, Abandon}`. *Rules out:* fact 14's orphan-row resurrection, which is what a generic detach ships.
- **`close_session` gets the `is_live` gate in the SAME change as the drain.** Fixing the cancellation without it makes fact 15 fire. Two commits are fine; two *plans* are not.
- **Detach by value at the synchronous eviction point.** Never defer the eviction (fact 16).
- **Do not touch the realign itself.** The legacy truncation is intended behaviour with a green test (fact 12). The gate protects *untouched* sessions, not the layout.

## Global constraints

- **GPUI:** a `cx.notify()` raised during a draw is discarded; reading an entity already under a `&mut` borrow panics at runtime while compiling clean, and `VisualTestContext`-drawn tests do not catch it.
- **Debug builds only** for verification. **Never pipe cargo through `tail` without `set -o pipefail`.**
- The harness's `<new-diagnostics>` blocks in this repo are frequently **stale mid-edit snapshots** — confirm with a real `cargo check`.
- **`mcp__sawe__*` drives the maintainer's LIVE editor.** Never call it.
- **Do not mutation-test by writing into this shared checkout** — use a `git worktree`.
- Commit messages imperative and crate-prefixed, **no `Co-Authored-By`**, never `git commit --amend`. Implementers do not push.
- Rust style: no `unwrap()` outside tests; comments explain *why*; never `let _ =` on a fallible call.

---

### Task 1: Drain on a soft close, abandon on a hard purge

**Files:** `crates/solution_agent/src/store/teardown.rs`, `crates/solution_agent/src/store.rs` (doc only), `crates/solution_agent/src/store/tests/teardown.rs`.

**Interface produced:**
```rust
enum ChainDisposition { Drain, Abandon }
fn evict_session_runtime_maps(&mut self, id: SolutionSessionId, chain: ChainDisposition);
fn teardown_session_runtime(&mut self, …, chain: ChainDisposition) -> …;
```

- [ ] `evict_session_runtime_maps` takes the disposition. `Drain` ⇒ `task.detach()`; `Abandon` ⇒ today's remove-and-drop, plus a `log::warn!` naming the session (there is none today — mirror the `pending_messages` precedent at `teardown.rs:513-524`).
- [ ] `close_session` → `Drain`; `cold_close_solution` (`:407`) → `Drain`; `purge_session_hard` (`:83`) and `purge_solution_fully` → `Abandon`.
- [ ] Gate `close_session`'s `persist_all_rows` (`:636`) on `acp_thread().is_some()`, mirroring `teardown.rs:387-395`.
- [ ] Respect fact 17's ordering: the detach must land after `teardown_session_runtime`'s `cancel_turn` work has issued whatever persist it is going to issue. Verify this by reading the call order, and say in your report which order you ended up with and why.
- [ ] Correct the four stale doc comments that assert the cancellation as permanent fact: `teardown.rs:461-463`, `:356-386`, `:610-632` (this one claims `close_session` "is not more dangerous" than `cold_close_solution` — true only while neither executes), and `store/tests/teardown.rs:511-534`.
- [ ] Test A — **the one that fails today**: live session, chain issued, `close_session`, `run_until_parked`, assert the rows on disk reflect the flush.
- [ ] Test B: legacy-shaped **cold** session (seed with the `store/tests/hydration.rs:1481-1512` recipe — `db.upsert_entry(id, idx, mod_seq, created_ms, Some("T1"), payload)` then `s.entries = …; s.hydrate_streams_main_only();`), `close_session`, assert the teammate rows **survive** — the `close_session` twin of `cold_close_solution_does_not_rewrite_cold_session_rows`.
- [ ] Test C — the anti-resurrection guard for fact 14: `purge_session_hard` with a chain in flight, `run_until_parked`, assert `load_entries` is **empty**.
- [ ] Each new test must **fail** when its half of the fix is reverted. Mutation-test all three in a worktree and report the results.
- [ ] Scaffolding: `SolutionAgentDb::open(cx.executor())` is a thread-named in-memory sqlite under `cfg(test)` (`db.rs:77-81`); `store::test_support::seed_store_with_session`, `store::tests::insert_cold_session`, `store::tests::setup_solution_and_project`, `create_session_with_thread`. `cx.run_until_parked()` after every store update that spawns.
- [ ] Watch the three tests most likely to break: `cold_close_solution_does_not_rewrite_cold_session_rows` (`store/tests/teardown.rs:536` — the designed tripwire; its own doc admits it currently passes for two reasons and cannot distinguish them), `purge_session_hard_removes_entity_disk_tree_and_rows` (`:207`, would fail *flakily* under a generic detach), `purge_solution_fully_clears_sessions_disk_and_rows` (`:285`).
- [ ] Gate: `set -o pipefail; cargo check -p solution_agent --all-targets`; `cargo test -p solution_agent`; `cargo fmt --all -- --check`; `./script/clippy -p solution_agent`.

### Task 2: Docs

- [ ] FORK.md: a numbered decision entry — what the chain is, why disposition is per-caller (fact 14), why `close_session` needed the gate in the same change (fact 15), and that the legacy realign is intended rather than avoided (fact 12).
- [ ] `docs/INDEX.md`: a row for this plan.
- [ ] Record fact 21 (app quit) as an open pool item with the recon detail, so the next session does not re-derive it.
