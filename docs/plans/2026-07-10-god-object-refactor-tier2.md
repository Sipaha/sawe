# God-object refactor — Tier 2 (structural seams)

**Status:** complete (T2-A + T2-B shipped; remaining store sub-objects deferred — see finding below)
**Date:** 2026-07-10
**Owner:** supervisor session
**Predecessor:** [`2026-07-10-god-object-refactor-tier1.md`](2026-07-10-god-object-refactor-tier1.md) (complete)

## Goal

Tier 1 was pure relocation of *long-but-flat* files. Tier 2 is **real
decoupling** of the two genuine coordinators the survey flagged:
`session_view.rs` (view god-object) and `store.rs` (state coordinator).
Unlike Tier 1 these change ownership/interfaces, so each item is verified
against the FULL test suite before the next.

Out of scope (Tier 3, deferred): `store.rs` SupervisorEngine (needs
create/close + send exposed as traits first — wide interface);
`SolutionSession` struct decomposition (ripples through 10k-line `store.rs`).

## Shared invariants

1. **Behavior-preserving.** No logic change. All existing tests stay green,
   same counts. Verify with `cargo build -p solution_agent` +
   `cargo test -p solution_agent` (debug, `set -o pipefail`), and a
   whole-binary `cargo build --bin sawe` at integration.
2. **No `mod.rs`;** `foo.rs` root + `foo/bar.rs` submodules.
3. **Preserve the module's external call surface.** Callers outside the file
   (esp. cross-file within the crate) must not need to change. For `store.rs`
   sub-object extractions this means: **Store keeps its existing public
   method signatures as thin delegating wrappers** (`pub fn select_model(&mut
   self, …) { self.model_catalog.select_model(…) }`) — only the *fields and
   logic* move into the new focused type.
4. One item = one commit; imperative subject, no `Co-Authored-By`, no
   conventional-commit prefix.

## Item T2-A — `session_view.rs` satellite extraction (MECHANICAL, low risk)

`crates/solution_agent/src/session_view.rs` (~3044 lines). Follow the
**existing proven pattern** already used by `session_view/{subagent_strip,
render_queue, recall, task_subagent_strip}.rs` and sibling `status_row.rs`:
move a method cluster into `session_view/<cluster>.rs` as either a
`impl SolutionSessionView` block or a free `fn render_*(view: &mut
SolutionSessionView, …)`. This is the *partial-class* idiom — it splits source
text, `self`/field ownership stays on `SolutionSessionView`. Bump the needed
methods to `pub(crate)` so satellites can call back in (the main file already
exposes ~14 such).

Extract these clean clusters (per the survey), each its own submodule:
- `session_view/find.rs` — `open_find`, `close_find`, `next_match`,
  `scroll_to_selected_match`, `previous_match`, `recompute_matches`,
  `render_find_bar` (find-in-session; `FindMatch`/`find_all` already live in
  `conversation_render`).
- `session_view/compose.rs` — `submit_compose_action`, `submit_compose_now`,
  `submit_compose_and_interrupt`, `enqueue_text_pending_send_and_resume`,
  `flush_pending_send_if_ready`, `validate_slash_command`,
  `restore_recalled_bundle`.
- `session_view/expanded.rs` — `open_expanded_compose`, `close_expanded_compose`.
- `session_view/paste.rs` — `paste_without_formatting`, `paste_intercept`,
  `handle_external_paths_drop`.
- `session_view/lifecycle.rs` — the constructor `new` (~271 lines) +
  `sync_thread_subscription` + the subscription wiring it installs.
- Move the inline `#[cfg(test)] mod tests`… note it is ALREADY a submodule
  (`session_view/tests.rs`) — leave it.

Leave `Render::render` (~750 lines) and the remaining orchestration in the
root `session_view.rs`. (Its internal decomposition — the list-processor
closure, compose-row builder — is optional and NOT required here; readability
only, defer unless trivial.)

**Do NOT** split `SolutionSession` the struct (model.rs) — out of scope.

Commit: `solution_agent: Extract session_view clusters into satellite submodules`

## Item T2-B — `store.rs` `ModelCatalog` extraction (REAL decoupling, cleanest seam)

Extract the model/effort catalog (survey cluster C6) into a focused type
`ModelCatalog` in a new `crates/solution_agent/src/model_catalog.rs`.

**Fields to move off `SolutionAgentStore`:** `agent_models`
(`HashMap<AgentServerId, Vec<ModelInfo>>`), `agent_models_probing`
(`HashSet<AgentServerId>`), and `adapters` (`Arc<AdapterRegistry>`) — **BUT
FIRST** `grep` `self.adapters` / `\.adapters` / `agent_models` across the
crate. If `adapters` (or either map) is read outside cluster C6, do NOT move
that field — instead have `ModelCatalog` borrow it (pass `&adapters` into the
methods, or keep `adapters` on Store and give ModelCatalog only the two maps).
Report what you found. Correctness over aggressiveness.

**Logic to move (survey C6 methods):** `session_models`, `selected_model`,
`new_chat_model_options`, `probe_models_for_agent`, `ensure_agent_models`,
`select_model`, `selected_effort`, `select_effort`, `refresh_models`,
`refresh_models_cold`, plus the `EFFORT_LEVELS` const.

**Interface:** `SolutionAgentStore` gains a field `model_catalog: ModelCatalog`
(constructed in `new_in_app`). It **keeps every existing public method as a
thin delegating wrapper** so no caller (session_view, mcp/*, etc.) changes.
Where a method needs per-session data (a session's `agent_id`, the sessions
map), pass it in as an argument — `ModelCatalog` must NOT hold a back-reference
to `Store` or to `sessions`. If a method fundamentally needs `&mut Store`
(e.g. it also mutates a session), keep that method on `Store` and only move the
pure catalog part.

Any C6 unit tests move next to the type or stay green via the delegates.

Commit: `solution_agent: Extract model/effort catalog into ModelCatalog`

## Remaining store sub-objects — DEFERRED (finding from T2-B)

**T2-B changed the calculus.** The coupling audit found the survey's "C6 is
the cleanest seam / easiest win" claim was optimistic: the model/effort
*fields* were C6-local, but every C6 *method* is intrinsically Store-coupled
(reads `sessions`, mutates session entities, calls `persist_session_row`,
emits `SolutionAgentStoreEvent`, spawns on `Context<Store>`). So the methods
stayed on Store and `store.rs` shrank by only **9 lines** — the win was
encapsulating state ownership + the probe-dedup invariant in a focused type,
not reducing the coordinator.

The lesson generalizes: **`store.rs` is a genuine coordinator, not flat
bloat.** Field-ownership extraction of the remaining sub-objects would repeat
the ModelCatalog outcome — small line reduction, real churn against the most
delicate file in the crate (watchdog/reconnect hardening #5–#9), for marginal
gain. The remaining items are therefore **deferred, not cancelled**:

- **PoolManager** — `pool` methods (`pool_release_session` etc.) read
  `sessions`/emit events → same coupling; low yield.
- **ArchiveGc** — the pure halves (`stale_archive_dirs`,
  `push_and_evict_transcripts`) are *already* extracted + unit-tested; the
  `reap_stale_*` orchestration is Store-coupled. Little left to gain.
- **TeammateWatchers** (C10) — the strongest remaining candidate (a cohesive
  ~3-map watcher subsystem), but the survey flags it reaches back into
  `sessions` + the event hub. Worth doing ONLY if a future change to the
  teammate-watching subsystem makes the extraction pay for itself. Revisit
  then, not speculatively.

Real further reduction of `store.rs` needs the Tier-3 work (trait seams to move
*orchestration*, not just fields) — explicitly out of scope.

## Sequencing / dispatch

- T2-A (`session_view.rs`) and T2-B (`store.rs` + new `model_catalog.rs`) touch
  **disjoint files** → run in parallel: T2-A in a worktree, T2-B in the main
  workspace. Supervisor cherry-picks the worktree branch back, runs the final
  whole-binary build + `solution_agent` tests, then pushes.

## Acceptance criteria

- [x] T2-A: 5 clusters moved to `session_view/{find,compose,expanded,paste,lifecycle}.rs`;
      root 3044 → 1864; `Render::render` stays in root. 563 tests green.
- [x] T2-B: `ModelCatalog` owns the model/effort maps + probe-dedup invariant;
      external call surface preserved (methods stayed on Store per the `&mut Store`
      escape clause — see finding); `adapters` correctly kept on Store (used
      externally). 563 tests green.
- [x] Whole-binary `cargo build --bin sawe` clean (EXIT 0) with both landed.
- [x] `docs/INDEX.md` updated; this doc flipped to `complete`.
- [~] Remaining store sub-objects deferred with rationale (not a blocker).

## Commit log

- `ceb233f8e7` — T2-B: `solution_agent: Extract model/effort catalog into ModelCatalog`
- `1dafbc0bcf` — T2-A: `solution_agent: Extract session_view clusters into satellite submodules` (cherry-picked from worktree `0bf8a03f9a`)
