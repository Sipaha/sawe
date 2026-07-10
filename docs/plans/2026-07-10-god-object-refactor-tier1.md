# God-object refactor — Tier 1 (mechanical file splits)

**Status:** in progress
**Date:** 2026-07-10
**Owner:** supervisor session

## Goal

Five fork-local files have grown into god-objects. A read-only survey
(five parallel agents) classified them. Tier 1 is the **mechanical,
low-risk** subset: files that are *long-but-flat* (independent handlers /
1:1 wrappers / pure render helpers), where the fix is a pure relocation
into a submodule tree — **no behavior change, no logic edits**.

Tier 2 (session_view satellites, store.rs sub-objects) and Tier 3
(SupervisorEngine, `SolutionSession` struct decomposition) are **out of
scope** for this plan.

## Hard invariants (apply to every split)

1. **Pure relocation only.** Move code verbatim. No renames, no logic
   tweaks, no "while I'm here" cleanups. Comments move with their code.
2. **No `mod.rs`.** Use the `foo.rs` (module root) + `foo/bar.rs`
   (submodule) pattern. The lib-root `mod foo;` line stays unchanged;
   the *root file* `foo.rs` gains `mod bar; mod baz;` and re-exports.
3. **Preserve the module's public surface.** Any item used from outside
   the module (another file references `crate::foo::item`) must remain
   reachable at `crate::foo::item` — add `pub(crate) use bar::*;` (or
   explicit re-exports) in the root file. Grep for `foo::` before/after;
   the set of externally-referenced paths must be identical.
4. **One split = one commit.** Each file's split is its own commit that
   **must compile and pass that crate's tests** before moving on:
   - `cargo build -p <crate>` (debug — agent-only verification, never
     `--release`; see CLAUDE.md build conventions).
   - `cargo test -p <crate>` (debug). All previously-green tests stay green.
   - Use `set -o pipefail` or don't pipe — `cargo … | tail` masks failures.
5. **Move inline `#[cfg(test)] mod tests` into a sibling `foo/tests.rs`.**
   Declare it `#[cfg(test)] mod tests;` in the root. This is the biggest
   line-count win and is near-zero-risk. Keep test code verbatim; only fix
   `use super::*` → `use crate::foo::*` / `use super::super::…` as needed.
6. Commit messages: imperative, no `Co-Authored-By`, no conventional-commit
   prefix in the subject. E.g. `solution_agent: Split mcp.rs into per-namespace submodules`.

## Work item A — `crates/solutions/src/mcp.rs` (5,766 → submodules)

39 tools across 6 namespaces, fully independent handlers. Registration is
one flat `register(cx)` at `mcp.rs:14-129`.

**Extra invariants specific to this file:**
- **`const NAME` strings are load-bearing** — external routing in
  `crates/editor_mcp/src/lifecycle.rs` (`GLOBAL_TOOLS`/`SHARED_TOOLS`,
  `is_global_tool`, startup `assert!` at ~`lifecycle.rs:496`) keys off the
  exact string. Move every `const NAME` byte-identical.
- **Preserve the tolerant `Inner`-shim hand-written `Deserialize` idiom**
  (36 sites) verbatim per struct — it is what lets `editor_mcp`
  force-inject `solution_id` on per-solution sockets. Do NOT try to
  centralize it.

**Layout** (`crates/solutions/src/mcp/`), each submodule exposes
`pub(crate) fn register_<ns>(cx: &mut App)`; the root `register` fans out:
- `mcp/solutions_lifecycle.rs` — list, get, create, rename, delete, open,
  close, find_for_path (+ `SolutionSummary`, `SolutionDetail`,
  `MemberDetail`, `WindowDetail`, `FindForPathMatch`; helpers `build_summary`,
  `build_detail`, `build_window_detail`, `find_window_id_for_solution`).
- `mcp/member_mgmt.rs` — add_member, add_empty_member, remove_member,
  reorder_members, set_active_member.
- `mcp/catalog.rs` — list, add_project, remove_project, edit_project,
  clear_cache, refresh_cache (+ `CatalogProjectInfo`, `build_catalog_info`).
- `mcp/project_files.rs` — the 12 `project.*` tools (+ `FileEntry`,
  `EditSpec/EditRange/EditPoint`, `AfterEditMeta`, `SearchMatch`,
  `LocationRef`, `PathValidationError`; helpers `project_for_solution`,
  `resolve_project_path`, `validate_path_in_solution`, `collect_files`,
  `cursor_for`, `location*_to_ref`).
- `mcp/workspace_state.rs` — `workspace.*` (5) + `windows.dump_visual_structure`
  + `diagnostics.get` (+ `BufferInfo`, `VisualNode`, `DiagnosticPathSummary`,
  `DiagnosticItem`; helpers `collect_buffers`, `find_window_for_solution`,
  `render_window_to_image`, the ~9 visual-tree builders,
  `collect_diagnostic_*`, `severity_to_string`).
- `mcp.rs` (root) — keeps `register(cx)` + shared imports; `mod` + fan-out.
- Tests → split alongside subjects under `mcp/…/tests.rs` (or a single
  `mcp/tests.rs` if they're not cleanly partitionable — judgement call).

## Work item B — `crates/solution_agent/src/mcp.rs` (8,483 → submodules)

31 tools + a **shared read-only DTO layer**; 40% is tests. Registration is
`register(cx)` at `mcp.rs:19-112`.

**Order matters: extract `mcp/dto.rs` FIRST**, then the tool groups depend
on it.
- `mcp/dto.rs` — all `*Dto` types + `SessionSummary`, `EntrySummary`,
  `ToolCallSummary`, `ToolCallAuthOption`, `PlanSummary`,
  `QueuedBundleSummary`, `StreamDto`, and conversion helpers:
  `session_summary`, `summarize_entry`, `tool_call_summary`, `entry_role`,
  `tool_status_dto/label`, `session_entry_to_markdown`,
  `count_images_in_entry`, `extract_images_for_entry`,
  `live_auth_options_for_session`, `permission_kind_str`,
  `build_pending_bundle_summaries`, `apply_user_anchored_filter`,
  `default_true`. Make these `pub(crate)`.
- `mcp/read.rs` — list_sessions, list_agents, get_session,
  get_session_children, get_session_entry, get_session_changes,
  read_session_history.
- `mcp/lifecycle.rs` — create_session (+ `project_for_solution`),
  delete_session, rename_session, restart_agent, reconnect_agent, force_idle.
- `mcp/messaging.rs` — send_message, send_message_blocks, cancel_turn,
  push_system_note.
- `mcp/authorization.rs` — authorize_tool_call (+ `resolve_authorization_outcome`).
- `mcp/context.rs` — reset_context, compact_session, start_compact
  (+ `validate_handoff_files`).
- `mcp/uploads.rs` — upload_init/status/finish/abort.
- `mcp/supervisor.rs` — supervisor_verdict, supervisor_audit_verdict,
  set_supervisor_enabled, set_supervisor_prompt, get_supervisor_state.
- `mcp/debug.rs` — `#[cfg(debug_assertions)]` `seed_cold_session`
  (+ `SeedColdSessionEntry/Params/Result`). **Preserve the cfg gate** on
  both the module and the `add_tool` call in `register`.
- `mcp.rs` (root) — `register(cx)` fan-out + shared trait/imports.
- Tests (`5098-8483`) → `mcp/tests.rs` (or per-submodule tests files).

## Work item C — `crates/solution_agent/src/db.rs` (2,896 → submodules)

35 methods = thin 1:1 wrappers over free fns keyed by table. Split by
table-group behind the shared `SolutionAgentDb { executor, Arc<Mutex<Connection>> }`.

**Specific invariant:** the two savepoint cascade fns (`purge_session_fn`
~`db.rs:1095`, `delete_by_solution` ~`db.rs:1464`) hard-code the full table
list and must be kept **with the schema/core** (not scattered), so they stay
in sync with the DDL.

**Layout** (`crates/solution_agent/src/db/`):
- `db.rs` (core) — `SolutionAgentDb` struct, `GlobalSolutionAgentDb`/`Global`,
  `connect`, `open` + schema DDL, migration helpers
  (`apply_idempotent_add_column{,_to}`, `column_exists` — keep `pub(crate)`,
  a test references it), the DTO structs, and the two cascade fns.
- `db/sessions.rs` — metadata, blob, tab_order, closed_at, epoch, change_seq
  (`solution_sessions`).
- `db/entries.rs` — transcript entries (`solution_session_entries`).
- `db/background.rs` — background_agent + background_shell rows (+ their DTOs).
- `db/attachments.rs` — `solution_session_attachment`.
- `db/supervisor.rs` — supervisor_state (self-contained, inline SQL already).
- The wrapper methods stay as `impl SolutionAgentDb` blocks in each submodule
  (Rust allows split inherent impls in-crate); the `&Connection`-taking free
  fns move next to their wrappers as `pub(crate)`.
- Tests (`1706-2896`) → `db/tests.rs`, split by concern if easy.

## Work item D — `crates/solution_agent/src/conversation_render.rs` (2,246 → submodules)

Pure render helpers, one-directional dependency. **Used cross-module**
(`session_view/render_queue.rs`, `event_sources.rs`, `session_entry.rs`,
`mcp.rs` all call `crate::conversation_render::*`) → the root **MUST
re-export** every moved item so those paths keep resolving. Grep
`conversation_render::` across the crate first; that set is the contract.

**Layout** (`crates/solution_agent/src/conversation_render/`):
- `conversation_render.rs` (root) — find/highlight primitives (`FindMatch`,
  `matches_for_span`, `render_span`, `find_all`), permission-button model,
  `render_entry` dispatch, small pure helpers, assistant-message render; plus
  `pub(crate) use` re-exports of everything moved to submodules.
- `conversation_render/tool_call.rs` — the tool-call cluster (`~1319-1786`):
  `tool_call_arg_preview`, `render_tool_call`, `tool_call_content_summary`,
  `fence_plain_text`, `truncate_tool_summary`, `raw_output_fallback_markdown`,
  `diff_summary_markdown`, `terminal_output_markdown`, `render_plan`,
  `content_block_text`.
- `conversation_render/user_message.rs` — user-message cluster (`~576-1031`)
  incl. the compaction-prompt chip + `CompactPromptPopover`.
- `conversation_render/image.rs` — image cluster (`~1032-1168`): the three
  `LazyLock<Regex>`, `clean_assistant_message_text`, `decode_image_local`,
  `open_image_preview`, `ImagePreviewWindowView`.
- Tests (`1787-end`) → `conversation_render/tests.rs`.

## Sequencing / dispatch

- **Two parallel worktree sub-agents** (independent crates → no merge conflict;
  files touched are disjoint):
  - Agent SA: items B → C → D (all in `solution_agent`, sequential in one
    worktree so the cold build amortizes across three commits).
  - Agent SOL: item A (`solutions`).
- Supervisor merges both branches back to `main`, runs a **final full
  workspace build + affected-crate tests** to confirm no cross-crate
  breakage, then pushes.

## Acceptance criteria

- [ ] All four files reduced to thin module roots; submodule trees created.
- [ ] `cargo build -p solutions` and `cargo build -p solution_agent` clean (debug).
- [ ] `cargo test -p solutions` and `cargo test -p solution_agent` all green
      (same count as before, minus none).
- [ ] No `const NAME` string changed; no behavior change; `git diff` is
      relocation-only (verify with a rename-aware diff spot-check).
- [ ] Externally-referenced `crate::<mod>::*` paths all still resolve
      (the build proves this).
- [ ] `docs/INDEX.md` plans table updated; this doc flipped to `complete`.

## Commit log

(to be filled as work lands)
