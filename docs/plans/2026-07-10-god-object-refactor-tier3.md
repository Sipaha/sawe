# God-object refactor — Tier 3 (store.rs supervisor relocation)

**Status:** in progress
**Date:** 2026-07-10
**Owner:** supervisor session
**Predecessors:** [`tier1`](2026-07-10-god-object-refactor-tier1.md) (complete),
[`tier2`](2026-07-10-god-object-refactor-tier2.md) (complete)

## Architectural decision (from the C2 design/feasibility pass)

**Trait-seam / dependency-inversion `SupervisorEngine` — REJECTED (NO-GO).**
Three GPUI walls make it impossible without turning the engine into its own
`Entity` (a bigger, riskier change):
1. **Double-borrow.** With the supervisor maps owned by an `engine` field of
   Store, `store.engine.apply_verdict(host = &mut store, …)` needs `&mut
   store.engine` and `&mut store` simultaneously → E0499. `mem::take`-ing the
   engine out breaks the methods that re-enter Store and re-read the taken-out
   maps (`apply_verdict → finish_judge → evict_session_runtime_maps` reads
   `supervisor_states`; the Compact arm re-acquires `SolutionAgentStore::global`).
2. **Async continuations capture `WeakEntity<Store>`, not the engine.**
   `spawn_ephemeral_supervisor_session` (`store.rs:~1496`) resolves minutes
   later via `this.update(cx, |this, cx| this.on_judge_failed(…))`. A plain
   struct engine can't be the `this.update` target; making it one means it must
   be a GPUI `Entity` with its own `EventEmitter` — reintroduces event routing
   + a `store ⇄ engine` cycle for zero glue reduction.
3. **Only Store can `cx.emit(SolutionAgentStoreEvent)`** (×19 in C2) — the whole
   UI/event-source layer subscribes to Store.
Field-only move would shrink `store.rs` ~70 lines out of 2150 (~3%) — the same
marginal ModelCatalog/TeammateWatchers trap, with worse churn. Do NOT attempt
this. (If a future *feature* needs multiple supervisor strategies or supervisor
logic reused outside Store, revisit then — driven by the feature, not hygiene.)

**Partial-class source relocation — ACCEPTED (GO).** Move the ~2000-line C2
method *bodies* into a new child module `crates/solution_agent/src/store/
supervisor_engine.rs` as `impl SolutionAgentStore { … }` blocks — the exact
idiom already in this module (`store.rs:31 mod queue;` → `store/queue.rs`, a
908-line `impl` block). `self` stays `&mut SolutionAgentStore`, `cx` stays
`Context<Self>`, fields stay on the struct. This splits *source text*, not
*state ownership* — so behavior is unchanged and the 46 C2 guard tests (which
call through `Store::method`) pass by construction. Net: `store.rs` **10161 →
~8200 (−19%)**.

Note: the pure supervisor decision logic is ALREADY extracted into
`crates/solution_agent/src/supervisor.rs` (1323 lines: `SupervisorState`,
`should_fire`, verdict/nonce/diary primitives). What moves here is the Store
*glue* around those pure fns — there is no further "pure core" to peel off.

## Hard invariants

1. **Verbatim relocation. No logic/timing/guard edits.** The #5–#9 watchdog
   hardening lives entirely in the moving methods and MUST survive byte-for-byte:
   - #5 usage-wall Error-vs-Stopped + judge liveness: `tick_supervisor`
     (judge-liveness gate + phantom-`Judging` un-wedge), `on_judge_failed`,
     `apply_usage_limit_stop`, `session_wall_message` anchor scan.
   - #7 stuck-tool liveness gate: `tick_stuck_sessions` (`active_tool` liveness
     via `pty_running || silent_secs < TOOL_OUTPUT_SILENCE_SECS`).
   - #9 parent-liveness shell reap: `tick_supervisor` `has_live_background_work`.
   - #6 verdict nonce auth + idempotency: `apply_verdict_authenticated`,
     `JudgeHandle::nonce`, `VerdictAuth`.
   - Compact double-lease defer: `apply_verdict` Compact arm's `cx.defer` +
     `SolutionAgentStore::global(cx)` re-acquire — move verbatim, do NOT inline.
2. **`cargo build -p solution_agent` + `cargo test -p solution_agent` green
   between EVERY commit** (`set -o pipefail`). Baseline **563** tests — stays 563.
3. **No `mod.rs`;** child module is `store/supervisor_engine.rs`, declared
   `mod supervisor_engine;` in `store.rs` next to `mod queue;`.
4. Visibility bumps only as the compiler demands (child sees ancestor privates,
   so most `self.foo()` calls just work; bump to `pub(crate)`/`pub(super)` any
   moved method still called from `store.rs` or another submodule).
5. Touch only `store.rs`, new `store/supervisor_engine.rs`, and — only if a test
   references a now-moved private item directly — `store/tests.rs`.

## Build sequence (each a commit; split by file-mechanics, not logic)

The subsystem is too interlinked to split methods across commits by behavior;
split by contiguous cut/paste. Full C2 method list is in the blueprint — the
groups:

1. **Scaffold + leaf helpers** — add the module; move `append_supervisor_diary_note`,
   `persist_supervisor_state`, `solution_root_for{,_app}`, `session_wall_message`,
   `judge_wall_message`, `ephemeral_session_tokens`, `JudgeHandle`, `VerdictAuth`.
2. **Spawn/finish + verdict core** — `spawn_judge`, `spawn_auditor`,
   `spawn_ephemeral_supervisor_session`, `finish_judge`, `finish_auditor`,
   `apply_verdict{,_authenticated}`, `apply_audit_verdict{,_authenticated}`,
   `on_judge_failed`, `apply_usage_limit_stop`.
3. **State-transition + nudge** — `set_supervision_enabled`, `set_supervisor_prompt`,
   `reload_supervisor_state_for`, `supervisor_state`, `wipe_supervisor_memory`,
   `reset_supervisor_continue_counter`, `rearm_supervisor_on_self_activity`,
   `hold_supervisor`, `supersede_judge_on_user_reply`, `note_user_input`,
   `clear_supervisor_question`, `escalate_to_user`, `notify_supervisor_done`,
   `send_supervisor_nudge`, `deliver_nudge_now`, `prune_raw_transcripts`.
4. **Watchdogs LAST (highest hardening risk, moved alone for clean bisect)** —
   `tick_supervisor`, `tick_stuck_sessions`.
5. **Whole-binary gate** — `cargo build --bin sawe` EXIT 0; add `FORK.md`
   touched-files row for `store/supervisor_engine.rs`; flip this doc to complete.

Commit subjects: `solution_agent: Relocate supervisor <group> into store/supervisor_engine.rs`.

## Acceptance criteria

- [ ] C2 (~40 methods + `JudgeHandle`/`VerdictAuth`) relocated to
      `store/supervisor_engine.rs`; `store.rs` ~10161 → ~8200.
- [ ] Behavior byte-for-byte unchanged; #5–#9 semantics preserved.
- [ ] `cargo test -p solution_agent` = 563 green after each commit.
- [ ] Whole-binary `cargo build --bin sawe` clean.
- [ ] `FORK.md` + `docs/INDEX.md` updated; this doc flipped to `complete`.

## Commit log

(to be filled as work lands)
