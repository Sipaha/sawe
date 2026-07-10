# God-object refactor — Tier 4 (audit-driven full decomposition)

**Status:** in progress
**Date:** 2026-07-10
**Owner:** supervisor session
**Predecessors:** tier1/2/3 (complete). Source: an architecture audit
(feature-dev:code-architect) that ranked meaningful *logical* decompositions
across the post-Tier-3 big files — whole responsibilities, not line-nibbles.

## Governing constraints (unchanged from Tier 3 / FORK.md #49)

- **Partial-class relocation, not ownership, for anything on `SolutionAgentStore`
  / `SolutionStore`.** Methods that spawn on `Context<Store>`, read
  `self.sessions`, or `cx.emit(...)` stay on the Store; only their *source text*
  moves into a `store/<name>.rs` child module (`impl SolutionAgentStore { … }`),
  exactly like `store/queue.rs` + `store/supervisor_engine.rs`. Trait-seam
  ownership is a dead end (FORK.md #49). Items that are genuinely GPUI-decoupled
  or independent tool/modal structs CAN be true sub-modules (noted per item).
- **Verbatim. No logic/timing/guard edits.** Preserve hardening #35 (mod_seq
  tail-flush), #40 (rebuild_streams after entries writes), #43–#48 (watchdog /
  teammate / shell-reap). No `mod.rs`. Preserve `const NAME` strings + the
  tolerant `Inner`-shim `Deserialize` for any MCP-tool move.
- **`cargo build -p <crate>` + `cargo test -p <crate>` green (debug, pipefail)
  after EVERY commit.** Baselines: `solution_agent` 563, `solutions` 146.
  Whole-binary `cargo build --bin sawe` at each stage's integration.
- One item = one (or a few sub-)commit(s); imperative subject, no `Co-Authored-By`.

## Stage A — low-risk logical splits (parallel by crate)

**A1 (#1) — `store/tests.rs` (13,584 lines) → subject-matched test tree.** The
single biggest file in the repo. Split into `store/tests/{supervisor,
teammate_reconciler,hydration,teardown,model_catalog,misc}.rs` (grep-then-classify
the ~199 test fns by subject, mirroring the source clusters; a `misc.rs` bucket
for anything that doesn't partition cleanly). Thin `store/tests.rs` root declares
the submodules. Shared test helpers (`native_mock_binary`, `subscribe_subagents_changed`,
`make_bash_bg_tool_call`, `arm_resume_gate`, `insert_test_background_shell`) →
`store/tests/common.rs` as `pub(crate)` (or `pub(super)`), used via `use super::common::*`.
Pure test-code relocation, bodies verbatim. TRUE sub-modules. Risk LOW.

**A6 (#6) — `supervisor.rs` (1,323 lines) → `supervisor/{state,persistence,briefing}.rs`.**
This file is 100% GPUI-decoupled (no `cx`/entities) → TRUE sub-modules with a
real interface. state = `SupervisorState`/`SupervisorStatus`/`VerdictAction`/
`should_fire`/`classify_judge_error`/`parse_usage_limit_reset_ms`/`ContinueGuard`;
persistence = the diary/verdict-log/intent disk I/O (`supervisor_dir`, `*_path`,
`wipe_supervisor_memory`, `cap_log_tail`, `append_session_log`, `append_verdict`,
`read_verdicts`, `verdict_stats`); briefing = `JudgeBriefingContext`,
`build_judge_briefing`, `new_verdict_nonce`, `verdict_nonce_matches`. Thin
`supervisor.rs` root re-exports (`pub(crate) use`). Risk LOW.

**A10 (#10) — `model.rs` inline tests (835 lines) → `model/tests.rs`.** Mechanical.
Risk negligible. (Do NOT split the `SolutionSession` struct — audit rejected it.)

**A7 (#7) — `solutions/src/mcp/{project_files,workspace_state}.rs` deeper split.**
`project_files.rs` (1,826) → `mcp/project_files/{fs_ops,buffer_ops,code_nav}.rs`
(filesystem CRUD / buffer lifecycle / LSP nav; shared helpers
`project_for_solution`/`resolve_project_path`/`validate_path_in_solution` stay
reachable). `workspace_state.rs` (1,278) → `mcp/{workspace_state,visual_structure,
diagnostics}.rs` (the `diagnostics.*` tools are a different namespace bundled in
by Tier-1 accident — clean seam). TRUE ownership (independent tool structs).
Preserve `const NAME` + `Inner`-shim. Risk LOW.

**A8 (#8) — `solutions_ui/src/modals.rs` (1,055 lines) → per-modal files.** 8
independent `ModalView` structs → `modals/{new_solution,add_catalog_project,
delete_solution,edit_catalog_project,delete_catalog_project,rename_solution,
new_project_in_solution}.rs` + shared `EditCatalogPrefill`. TRUE ownership. Risk LOW.

**A9 (#9) — `solutions/src/store.rs` (1,602 lines) → catalog/lifecycle/members.**
Partial-class (still `&mut SolutionStore` + `cx.emit`), but no watchdog density.
`store/{catalog,lifecycle,members}.rs`. Risk LOW-MEDIUM.

### Stage A dispatch (parallel tracks, minimize concurrent cold builds)
- Main workspace (`solution_agent`, warm, sequential): A1, then A6, then A10.
- Worktree (`solutions`, sequential): A7, then A9.
- Worktree (`solutions_ui`): A8.
Supervisor cherry-picks worktree branches back, runs whole-binary build +
`solution_agent`/`solutions` tests, pushes. Then Stage B.

## Stage B — `store.rs` cluster relocations (sequential, all edit store.rs)

Same partial-class idiom as `store/supervisor_engine.rs`; verbatim; tests green
between commits; **watchdog-dense — treat like Tier 3.** Order by ascending risk:

- **B2 (#2) — Background-teammate reconciler → `store/teammate_reconciler.rs`**
  (~1,380 lines; `store.rs:5187–6568` + the cx-free helpers `claude_project_dir_for`/
  `background_agent_dir_for`/`parent_session_jsonl_for`/`push_and_evict_transcripts`/
  `read_complete_lines_from`/`scan_lines_for_completions`). Carries #43/#47/#48.
  MEDIUM. Sub-commit: watcher/snapshot methods, then tick/reconcile methods.
- **B3 (#3) — Hydration / cold→live resume → `store/hydration.rs`** (~1,700–1,900;
  `cold_entries_from_persisted`, `entries_from_rows`, `PersistedSession`, title/preview
  helpers, `resume_session` [541 lines, its own commit], `restore_open_tabs`,
  `hydrate_all_for_solution`, `hydrate_open_tabs_lazy`, `load_cold_blob_into_session`,
  reap/reopen). Carries #40/#43. MEDIUM.
- **B5 (#5) — Teardown / archive-GC → `store/teardown.rs`** (~550;
  `evict_session_runtime_maps`, `teardown_session_runtime`, `finalize_session_teardown`,
  `close_session`, `spawn_remove_archive_dir`, `purge_session_hard`,
  `purge_solution_fully`, `gc_orphan_members`, `cold_close_solution`,
  `gc_orphan_solutions`, `stale_archive_dirs`). LOW-MEDIUM.
- **B4 (#4) — `handle_acp_event` (786 lines) → `store/acp_event.rs`** (`store.rs:6570–7356`).
  Move the WHOLE function verbatim; do NOT decompose its match arms (nexus of
  #35 + #44). HIGH — do LAST, alone, full 563-gate immediately before + after.

Expected: `store.rs` 7,998 → ~3,500.

## Rejected (do NOT do — from the audit)
PoolManager/ArchiveGc field-extraction (marginal, #49); any trait-seam (#49);
entry-persistence cluster (430 lines on the #35/#40 path — risk/reward inverted);
`mutate_state`/`mark_*` (too central, too small); further `session_view.rs` /
`status_row.rs::render` decomposition (UI construction, already triaged in Tier 2);
`mcp/{dto,read,lifecycle}.rs`, `upload.rs`, `compact.rs`, `event_sources.rs`,
`store/queue.rs`, `message_generator.rs` (already right-shaped).

## Commit log
(filled as work lands)
