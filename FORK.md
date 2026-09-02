# Fork-Local Additions

This file is an index of everything **Sawe** adds on top of upstream [Zed](https://github.com/zed-industries/zed). It's the canonical place to look for "what's different here" before diving into code or merging upstream.

For fork **philosophy** (rebrand identifiers, what's disabled, build conventions, embedded MCP usage) see `.rules` / `CLAUDE.md` at the repo root.

## Re-fork onto upstream v1.7.2 (2026-06-24)

This repo's `main` was **re-forked onto upstream Zed `v1.7.2`** (commit
`aa8ac4b04e261f19c2465f68e9ce2fa9721ae1a2`) and rebranded `spk-editor` → **`sawe`**.
The predecessor fork (`SPK Editor`, content base Zed v0.235) had a git history disjoint
from upstream after March 2021, so `git merge upstream/main` was infeasible. The re-fork
restores a **real recent merge-base**, so future `git merge upstream/main` is a normal merge.

Method: based `main` on the `v1.7.2` tag, copied the 18 net-new fork crates, applied the
fork delta (`git diff v0.235-base..predecessor-tip`) as a 3-way patch, then resolved
conflicts by the rule **prefer upstream v1.7.2 unless the hunk is a documented fork feature**
(most of the old delta was upstream commits the predecessor had already absorbed, now native
in v1.7.2). A layered compile-loop fixed v0.235→v1.7.2 API drift; Phase B rebranded the
product identity to `sawe` (internal `zed` cargo crate + `zed_actions` + license kept).

Consequences for future upstream merges:
- **`git2` was re-added** (`git2` workspace dep, used by `git/src/operations` for the
  `git_conflict_ui` resolver). Upstream v1.7.2 went shell-only (no libgit2); this fork still
  needs git2. Keep ours on future merges.
- The screenshot/headless stack (`gpui_wgpu::render_to_image`/`render_scene_into`,
  `WgpuRenderer::new_offscreen`, the Option-`surface` refactor) and `ListState::measure_last`
  were re-grafted additively onto v1.7.2's evolved renderer/list internals.
- Resolved-functionality stubs to revisit: `solution_agent` managed-agent timeouts pinned as
  consts (not re-plumbed into `AgentSettings`); the `AcpModelSelector` status-bar model selector
  was dropped against v1.7.2's refactored model plumbing (re-implement when needed); two
  `project_panel` tests still reference the removed in-`git_ui` `git_graph` module (affects
  `cargo test` only, not the `--bin` build).

## Fork-only crates

| Crate | Purpose | Notes |
|---|---|---|
| `crates/editor_mcp` | Embedded JSON-RPC MCP server (`~/.spk/sawe/state/mcp.sock`) so an external agent can drive a live editor for E2E tests + autonomous work. Owns `SingleInstanceLock`, server bind, broadcast. | 50 builtin tools across `editor.*` / `windows.*` / `workspace.*` / `project.*` / `diagnostics.*` namespaces. Tools registered from each domain crate's `init`. |
| `crates/solutions` | Multi-project workspace abstraction. A **Solution** groups N catalog projects (each a remote git URL) into one editor window with all members mounted as worktrees. Persisted to SQLite via `SolutionsDb` (one-time migration from legacy `solutions.json`); warm clone cache at `~/.cache/sawe/catalog/<sha256>/`. | Adds 11 `solutions.*` + 6 `catalog.*` MCP tools. Emits `solution_changed` events. |
| `crates/solutions_ui` | UI for Solutions: title-bar tab strip, picker, modals, welcome integration, plus the per-panel `ActiveProjectSelector` element hosted by `project_panel` and `git_panel`. | Touches upstream `title_bar`, `welcome`, `app_menus`, `project_panel`, `git_ui` for integration points. |
| `crates/solution_agent` | N parallel Claude Code-style AI sessions scoped to a Solution, multiplexed onto a shared `claude` subprocess per `(solution_id, agent_id)` pair. The active dialog renders in the Solution band (`solution_band`) and every session with a `tab_order` gets a tab in the status-bar `session_tab_strip` (`candidates_for` drops `is_ephemeral` / `is_supervisor_ephemeral` sessions and anything whose `tab_order` is `None`); the pane-item and side-dock-navigator hosting this crate started with are both gone (decisions 7, 22, 93). SQLite persistence at `~/.spk/sawe/data/solution_agent/solution_agent.db`. | Adds 10 `solution_agent.*` MCP tools (R-5e: `get_session_entry` joined the catalog; `get_session` gained additive `include_full_content` / `include_images` flags + per-entry `markdown` / `images` / `tool_call` / `plan` fields. R-6e: cursor pagination on `get_session` (`before_index` / `after_index` / `count` — count is LAST-N) + `list_sessions` (`before_last_activity_at_ms` / `count`, DESC by `last_activity_at`); both return `total_count` and every `EntrySummary` now carries an absolute `index`. Additive — old callers stay on the legacy full-response path). F: `parent_session_id` on sessions + `get_session_children` tool + `total_tokens` / `parent_session_id` on `SessionSummary`; `create_session` gained an optional `parent_session_id` param (validated same-solution); `agent_session_created` payload now carries `parent_session_id`. Emits `agent_session_*` event kinds; `agent_session_message_appended` payload carries `stream_id` + a **stream-local** `entry_index` + `role` + `preview` so remote consumers can render the new bubble without re-fetching the whole transcript (the degraded form is `session_id` alone; see #108 for why the index is stream-local). Auth via subscription (`claude` CLI's own `~/.claude/`); no `ANTHROPIC_API_KEY`. |
| `crates/git_conflict_ui` | Standalone 3-way merge conflict resolver. Independent crate because of the size of the resolver view and isolation of the UI surface, not for merge-friendliness. | Skeleton — full implementation lands in S-CFL (`docs/superpowers/plans/git-panel-plan.md`). Will own `editor.git.{list_conflicts,resolve_conflict,mark_resolved,continue_merge,abort_merge}` MCP tools. |
| `crates/solution_git` | Solution-aware git operations: aggregated log, status dashboard, solution-wide commit/push, cross-member cherry-pick, branch protection. Built on top of `solution`, `git_ui`, and existing `git` crates. Per P-9 (inversion of control), depends *downward* on `git_ui` and registers trait providers via `git_ui::providers::*` at `init()`. | Skeleton — full implementation lands across milestone M4 of the git-panel plan (S-SOL-LOG / S-SOL-DSH / S-SOL-CMT / S-SOL-PSH / S-SOL-CHP / S-SOL-PRT). Will own the `solution.git.*` MCP namespace. |
| `crates/run_config` | Headless Run Configurations model: `RunConfiguration` + a `RunConfigProvider` registry (extensible config *types*), persistence to `.sawe/run-configurations.json` (per worktree, watched/hot-reloaded) + a global `~/.spk/sawe/config/run-configurations.json`, built-in `shell` / `debug` / `task-ref` providers, and the `run_config.*` MCP tool namespace. Translates a config into a `task::SpawnInTerminal` / `task::DebugScenario` at launch time — sits on top of the `task`/`dap` engines, doesn't replace them. | Adds 6 `run_config.*` MCP tools (`list`, `create`, `delete`, `select`, `run`, `stop`). Emits `RunConfigStoreEvent`. `run` / `stop` / `select` are no-ops until a `RunController` window is live. |
| `crates/run_config_ui` | UI for Run Configurations: the compact run-config widget rendered right-aligned in the title bar (config dropdown + Run / Debug / Stop — IDEA-style), the Edit Configurations modal (schema-driven provider forms, before-launch = Save All, executors, project/global storage), the status-bar run indicator, and the per-`Workspace` `RunController` (launches terminal tasks via `ConsolePanel::spawn_task` — falls back to `Workspace::spawn_in_terminal` when no terminal panel — and debug runs via `start_debug_session`; tracks active runs by terminal handle / `dap` session id so Stop actually kills them; publishes the running set + a command sink for MCP). | Touches upstream `workspace`, `zed/main.rs`, `zed/app_menus.rs`, `settings_content`, keymaps. |
| `crates/remote_control` | Headless model + JSON persistence + (R-2) network listener + (R-4) `remote.*` proxy to the embedded `editor_mcp` Unix socket. State model: `RemoteControlSettings { server_address, server_port, enabled, clients }` persisted to `~/.spk/sawe/config/remote-control.json` (live-watched + atomic writes; FS-watcher echo squelched via `self_write_echoes` to prevent round-trip self-clobber). Generates 32-byte client secrets via `OsRng` + base64. R-2 adds the listener (`set_enabled(true)` → load-or-generate self-signed TLS cert via `rcgen` → bind `0.0.0.0:server_port` → TLS 1.3 → WebSocket upgrade → 16-byte HMAC-SHA256 challenge). R-4 swaps the R-2 `MinimalDispatcher` stub for `ProxyDispatcher`: each WS connection lazily opens a `UnixMcpProxy` to the in-process `editor_mcp::socket_path()`, translates `remote.X.Y` → upstream `tools/call { name: "X.Y", arguments }` per the allow-list in `allow_list::translate`, and fans `editor/notification` frames out as `remote/notification` (block-list filter: only `agent_session_*` kinds reach the WS). Per ADR-0003 + R-4 plan-doc. | Emits `RemoteControlStoreEvent::Changed`. Tokio runtime via `gpui_tokio::Tokio::spawn_result`. Watch-channel live client-list propagation. `cert_fingerprint()` / `bound_addr()` accessors expose listener state for the R-3 QR generator. Per-WS proxy lifecycle: connection-scoped `UnixMcpProxy` holds a per-id `oneshot` map for response demux and a bounded mpsc (256, drop-newest on overflow) for notifications; reader task aborts on `Drop` → upstream socket closes → embedded server cleans up subscriptions. `MinimalDispatcher` retained for unit/integration tests that don't want a live MCP socket. |
| `crates/remote_control_ui` | UI for Remote Control: right-aligned status-bar entry (`RemoteControlStatusItem` — colored dot + "Remote Control") and the workspace modal (`RemoteControlModal`: address row + Detect button (HTTP GET to `https://ifconfig.me`), port row, enable/disable toggle, authorized-client list with secret-prefix + "Show QR" stub toast, inline add-client name input). | "Show QR" emits a `TODO R-1.5: QR rendering` toast for now; the address row's Detect uses `cx.http_client()` and writes the trimmed body straight into the address input + store. |
| `crates/console_panel` | Hosts the terminal tab strip for the Solution band's utility section (phase 2a task 6) — NOT a dock panel: `ConsolePanel` keeps `Render`/`Focusable` but has no `Panel` impl, `dock_position`, or `ConsolePanelSettings` any more (decision 91). Owns `ConsolePanel`, the `TerminalProvider` spawn helper, and the `console_panel::{NewTerminal, ToggleFocus}` actions (`NewChat`/`ShowSession` route straight to `SolutionAgentStore`, not through the panel). Persists tab list to the `console_panel_state` table in `workspace.db`. Decision 22 (dock-panel origin), decision 91 (moved into the band). | `console_panel::init` registers the workspace actions; `ConsolePanel::load` is called from `zed::initialize_panels`, which installs the resulting entity via `Workspace::set_solution_band_utility_item` (an `AnyView` slot) instead of `Workspace::add_panel`. Any crate needing the concrete `Entity<ConsolePanel>` (run-configuration output, the inline assistant) calls `console_panel::console_panel_for_workspace(workspace)`, which downcasts that slot — see decision 91. `ShowSession { session_id }` (data action, dispatch via `windows.dispatch_action` args) selects an already-resident session as the Solution band's active dialog via `SolutionAgentStore::set_active_dialog_session` — it does NOT spawn or hydrate, and returns early on either of two conditions: the id is not in the store, or the session is one the tab strip would not draw a tab for (`SolutionSession::can_be_active_dialog` — ephemeral helpers, supervisor judges/auditors, and anything with `tab_order: None` such as a sub-agent). The second guard is why an MCP-driven `ShowSession` on a sub-agent id silently no-ops: the band resolves its dialog through a different map than the strip builds tabs from, and the selection is persisted, so an unguarded select would leave the band reopening after a restart on a dialog with no tab to leave it by. It is otherwise the deterministic "bring session N into view" seam for MCP-driven UI verification. Note: chat tabs are Solution band dialogs, not `ConsolePanel` tabs, and are never filtered by the active member (decision #89). Only terminal tabs are member-scoped (`TabScope::Member`, keyed by the terminal's cwd) and get hidden by a project switch. |
| `tooling/test_target_guard` | Guards one workspace invariant: a package must not set `[lib] test = false` while carrying test code under `src/`. That combination compiles and passes and is **never run** — it left 49 tests dead in `project`, `worktree` and `fs` for months. The check runs from a **build script** so it fires on `cargo check --workspace --all-targets`, and is also exposed as an integration test under `tests/` so `[lib] test = false` cannot silence the guard against itself. | Leaf package — nothing depends on it, so `cargo build --bin sawe` is unaffected. ~0.15s per run over 259 manifests. See decision #109. |

## Disabled upstream subsystems

See `.rules` § "What's disabled" for the table. Brief: `auto_update`, `telemetry`, `collab` / `collab_ui`, sign-in, native cloud LLM (`CloudLanguageModelProvider`), `zeta` edit prediction, Sentry uploads, 41 CI workflows, **`agent_ui::AgentPanel` dock panel + Welcome `render_agent_card`** (the fork's AI is `solution_agent`; upstream's panel is a parallel unconfigured surface). Code stays in tree, init/dispatch/UI sites are commented out (`if false { … }` is fine) — re-enabling stays a one-line change and we haven't audited what other crates implicitly depend on these subsystems' types or globals.

## Notable upstream file modifications

This fork no longer constrains itself to additive-only modifications of upstream files. When refactoring or restructuring upstream code yields a meaningfully better result, it is done — the fork accepts the merge-conflict cost as the price of clean local code. The table below is informational, not normative: it records significant divergences from upstream, but is not a contract that prevents further changes.

**Working principles for upstream modifications:**

1. **Locality over indirection.** Extensions live where the thing they extend lives. Don't create wrapper crates solely to keep upstream files untouched.
2. **Refactor when it pays.** If splitting an upstream file into submodules, renaming types, or restructuring layout meaningfully improves the local code, do it. Document significant divergences in the table below.
3. **Identifiers stay.** Crate names, module paths, and public type names follow upstream unless there's a strong reason — they're cheap to preserve and reduce friction in cross-references.
4. **Prefer file-level rewrites over scattered patches.** If five separate hunks across a file each conflict with upstream, a single full-file rewrite is often easier to maintain than five conflicting patches.

| File | Change | Owning fork crate |
|---|---|---|
| `crates/zed/src/main.rs` | `editor_mcp::init`, `solutions::init`, `solutions_ui::init`, `solution_agent::init`, `run_config::init`, `run_config_ui::init` calls inserted in startup flow. Various subsystem inits commented out. Adds `--headless` CLI flag forwarded to `gpui_platform::current_platform(headless)` (ADR-0002 native headless platform). | mixed |
| `crates/zed/src/zed.rs` | `initialize_agent_panel` call commented out in `futures::join!` (fn kept under `#[allow(dead_code)]` for one-line re-enable). | `solution_agent` |
| `crates/zed/Cargo.toml` | Workspace deps on all fork crates (`editor_mcp`, `solutions`, `solutions_ui`, `solution_agent`, `solution_git`, `git_conflict_ui`, `run_config`, `run_config_ui`). | mixed |
| `crates/zed/src/zed/app_menus.rs` | Run / Debug / Stop / "Edit Configurations…" items (with separator) prepended to the existing "Run" menu (S-RUN). Solutions / sessions items added by earlier work. | `run_config_ui` / `solutions_ui` |
| `crates/title_bar/src/title_bar.rs` | Embeds `solutions_ui::SolutionTabStrip` after the hamburger; project-info segment (solution name + worktree + branch) removed; uses fork-local `fork_title_bar_content_height()` for the content row. **(2026-06-23, decision #27)** The branch widget + run-config widget no longer render here — they moved to the new `ProjectToolbar` row; `title_bar` still registers the `git::BranchesPopupOpen` shortcut but now downcasts `project_toolbar_item()` → `ProjectToolbar`, and constructs the `ProjectToolbar` alongside the `TitleBar` in `init`. `render_restricted_mode` call site disabled (function kept under `#[allow(dead_code)]`) — see decision 13. | `solutions_ui` / `solutions` |
| `crates/title_bar/src/project_toolbar.rs` | **New (decision #27).** Full-width `ProjectToolbar` row entity below the title bar: `ProjectTabStrip` left (aligned to the hamburger inset), relocated branch widget (`render_branch_widget` → anchored `PopoverMenu<BranchesPopup>`, repo = active member's) + run-config strip right. Holds `branch_popover_handle`; owns `toggle_branch_popover`. **(phase 2b task 7)** Also hosts the project-zone dock toggles: one `workspace::dock::PanelButtons` per dock (left / bottom / right) at the leading edge, which replaced the vertical edge strips `Workspace` used to flank itself with. Built from the `&Workspace` parameter, never by upgrading the weak handle — `new` runs under a live `&mut Workspace` borrow. | `solutions_ui` / `git_ui` / `run_config_ui` / `workspace` |
| `crates/recent_projects/src/recent_projects.rs` | Added an **unconditional** `_ => unreachable!()` catchall to the `RemoteConnectionOptions` match, marked `#[allow(unreachable_patterns)]`. Upstream matches only the concrete variants plus a `Mock` arm gated on this crate's `test-support`, while the variant itself is gated on **`remote`'s** `test-support` — two different crates' features, which cargo drives apart, so upstream's set is non-exhaustive in the mixed configuration our test runs hit. The catchall is *dead* in the shipping build (the concrete arms are already exhaustive there) and load-bearing only when `remote/test-support` is on without this crate's own; a crate cannot `cfg` on a dependency's feature, so `#[allow]` is the only form that is warning-free in every configuration. It was previously gated `#[cfg(not(any(test, feature = "test-support")))]`, which is exactly the configuration where it is dead — that produced five warnings in `cargo build --bin sawe` that `cargo check --workspace --all-targets` structurally could not see (`docs/findings/2026-08-31-cfg-universes-and-the-warning-gate.md`). | upstream-fix |
| `crates/recent_projects/src/remote_connections.rs` | Same `RemoteConnectionOptions` catchall as the row above, at two sites. | upstream-fix |
| `crates/recent_projects/src/remote_servers.rs` | Same `RemoteConnectionOptions` catchall as `recent_projects.rs` above. | upstream-fix |
| `crates/remote_connection/src/remote_connection.rs` | Same `RemoteConnectionOptions` catchall as `crates/recent_projects/src/recent_projects.rs` above. | upstream-fix |
| `crates/remote_connection/Cargo.toml` | Adds a `[dev-dependencies]` section the crate never had: a **self** dev-dependency plus one on `project`, both `features = ["test-support"]`, present purely to force features on rather than for any code use. Without them `cargo check/clippy/test -p remote_connection --all-targets` does not compile at all — the lib-test unit is built with `--cfg test`, which switches the `Mock` arm on while nothing switches `remote/test-support` on (E0599), and enabling `remote/test-support` alone then breaks `project`'s own `Mock` match (E0004). So the crate's per-crate gates could never run, which reads identically to their passing. Idiom copied from `crates/project/Cargo.toml`. `[package.metadata.cargo-machete] ignored = ["project"]` records that the `project` dev-dep is deliberately unused in code. | upstream-fix |
| `crates/clock/Cargo.toml` | Adds a `[dev-dependencies]` section the crate never had, holding only a **self** dev-dependency with `features = ["test-support"]`. `FakeSystemClock` and its `parking_lot::Mutex` field are gated `#[cfg(any(test, feature = "test-support"))]`, but `parking_lot` is an *optional* dependency that only `test-support` pulls in — so the lib-test unit switched the code on while nothing switched the dependency on (E0433) and `cargo check/clippy/test -p clock --all-targets` could not compile at all. Same idiom and same mechanism as `crates/remote_connection/Cargo.toml` above; see `docs/findings/2026-08-31-cfg-universes-and-the-warning-gate.md`. | upstream-fix |
| `crates/call/Cargo.toml` | Adds a **self** dev-dependency (`features = ["test-support"]`) to the existing `[dev-dependencies]`. `call_impl/diagnostics.rs` mirrors `livekit_client`'s `any(test, feature = "test-support")` switch between the real `RtcStats` and the mock one, keyed off `call`'s *own* feature; the other dev-dependencies force `livekit_client` to the mock while nothing turned `call/test-support` on, so the two switches disagreed (E0599). Note this one failed on the **lib**, not the lib-test unit — `cargo check -p call` alone is fine, `--all-targets` is not. Same family as `crates/remote_connection/Cargo.toml` above; see `docs/findings/2026-08-31-cfg-universes-and-the-warning-gate.md`. | upstream-fix |
| `crates/language_extension/Cargo.toml` | Adds a `[dev-dependencies]` section the crate never had: `util = { features = ["test-support"] }`. Its `#[cfg(test)]` module imports `util::test::marked_text_ranges`, which `util` gates behind `any(test, feature = "test-support")` (E0432). The crate has no `test-support` feature of its own, so this is the *dependency* half of the idiom rather than a self dev-dependency. Same family as `crates/remote_connection/Cargo.toml` above; see `docs/findings/2026-08-31-cfg-universes-and-the-warning-gate.md`. | upstream-fix |
| `crates/acp_thread/src/connection.rs` | Adds `AgentConnection::new_session_with_meta` extension point (default impl drops the meta + falls back to `new_session`) so adapters can act on protocol-level `_meta` keys (e.g. `claude-agent-acp` reads `_meta.systemPrompt` to seed the session prompt). | `solution_agent` |
| `crates/acp_thread/src/acp_thread.rs` | (1) `ToolCall::status_started_at: Option<chrono::DateTime<Utc>>` — stamped on the first transition into `InProgress` (both `from_acp` and `update_fields`), preserved across the transition to a terminal status. Lets fork-owned renderers (`solution_agent::conversation_render`, the MCP wire) display a live "ran for Xs" badge without inventing a parallel start-time table. (2) `SPK_CLIENT_SEND_ID_META_KEY` constant + `client_send_id_from_user_message(&UserMessage) -> Option<i64>` helper — read-only scan over a UserMessage's `chunks` for a client-stamped `_meta.spk_client_send_id`. Lets the mobile client round-trip-match its in-flight optimistic bubble to the server-echoed entry by id instead of fragile content-equality on truncated previews. Zero changes to `AcpThread::send` or `UserMessage` struct — the id rides on the existing `acp::ContentBlock._meta` field. | `solution_agent` |
| `crates/agent_servers/src/acp.rs` | (1) `mcp_servers_for_project` prepends a fork-local `acp::McpServer::Stdio` entry pointing at `<current_exe> --nc <editor_mcp.socket_path>` so spawned ACP subagents see the editor's embedded MCP tools (helper: `sawe_mcp_bridge_server`) — see decision 14. (2) `AcpConnection::new_session_with_meta` impl splices `extra_meta` into `NewSessionRequest::meta`. (3) **(decision #51)** `solution_scope_for_project(project, cx) -> Option<SolutionScope>` — resolves the `(SolutionId, root)` that owns a project's worktrees, so the claude adapter can write/point at that Solution's `claude-settings.json`. | `editor_mcp` / `solution_agent` |
| `crates/agent_servers/Cargo.toml` | New dep on `editor_mcp` for the socket path. | `editor_mcp` / `solution_agent` |
| `crates/agent_servers/src/agent_servers.rs` | Re-exports `acp::solution_scope_for_project` alongside `mcp_servers_for_project` (decision #51). | `solution_agent` |
| `crates/git_ui/src/git_ui.rs`, `crates/git_ui/src/project_diff.rs`, `crates/git_ui/src/branch_picker.rs` | `ProjectDiff` / branch picker follow the **active solution member's** repo (`active_solution` + `SolutionStore::active_member`, subscribed to `ActiveMemberChanged`). Since decision #50 the lookup is `store.active_member(solution.id) -> MemberId` + `solution.member(id)`, not a catalog-slug scan. | `solutions_ui` / `solutions` |
| `crates/context_server/src/listener.rs` | (1) `broadcast_notification` for fork event push. (2) Per-solution sockets (decision 17): tool handlers became `Rc` (shareable across sockets); `RegisteredTool.wants_solution_id` computed from input schema in `add_tool`; `McpServer.bound_solution_id` + `set_bound_solution`; `split_off_tools` / `export_tools` / `install_tools` to partition the catalog; `handle_call_tool` injects the bound `solution_id` — as a JSON **number** since decision #50, and it **overrides** any id the caller supplied (a per-solution socket can never be talked into acting on another Solution). | `editor_mcp` |
| `crates/gpui/src/elements/list.rs` | `ListState::measure_last(N)` chunked tail prefetch (plus `MEASURE_LAST_DEFAULT_BATCH` / `LOOKAHEAD` / `EAGER_THRESHOLD` knobs) so virtualized lists can pre-warm their most-recent items on the first layout pass without paying the full-list measurement cost. Used by `solution_agent`'s conversation list to keep scroll-up off long resumed conversations from triggering a height-discovery cascade. | `solution_agent` |
| `crates/gpui/src/window.rs` | `Window::render_to_image` ungated (was `#[cfg(any(test, feature = "test-support"))]`) so `workspace.screenshot` works in normal builds. Adds `Window::iter_hitboxes()` — a public accessor over the most-recently rendered frame's hitboxes, used by `workspace::mcp::clickables` to surface clickable regions to the autonomous-testing MCP surface. | `solutions` (screenshot tool) / `workspace` (clickable tree) |
| `crates/gpui/src/platform.rs` | `PlatformWindow::render_to_image` default + the `use image::RgbaImage` import ungated (were `#[cfg(test|test-support)]`). Non-implementing backends still return the "not implemented for this platform" error. | `solutions` (screenshot tool) |
| `crates/gpui_wgpu/src/wgpu_renderer.rs` | Extracted the per-frame primitive-encoding loop from `WgpuRenderer::draw` into `render_scene_into(scene, target_view)` (no behaviour change for `draw`); added `WgpuRenderer::render_to_image` — offscreen render-to-texture matching the swapchain size/format, `copy_texture_to_buffer` + readback, BGRA→RGBA fixup → `RgbaImage`. New `image` dep in `gpui_wgpu/Cargo.toml`. | `solutions` (screenshot tool) |
| `crates/gpui_linux/src/linux/x11/window.rs`, `crates/gpui_linux/src/linux/wayland/window.rs` | `PlatformWindow::render_to_image` override → `renderer.render_to_image(scene)`. | `solutions` (screenshot tool) |
| `crates/gpui_wgpu/src/wgpu_context.rs` | Adds `WgpuContext::instance_offscreen()` + `WgpuContext::new_offscreen()` — surfaceless adapter/device selection with integrated-GPU bias for the native headless platform (ADR-0002). | `gpui_wgpu` (headless platform) |
| `crates/gpui_linux/src/linux/headless.rs`, `crates/gpui_linux/src/linux/headless/client.rs` | `HeadlessClient::open_window` now returns a real `gpui::HeadlessWindow` backed by `gpui_wgpu::WgpuHeadlessRenderer`; `displays()` / `primary_display()` / `active_window()` / `window_stack()` populated against a synthetic 1920×1080 `HeadlessDisplay`. New sibling file `display.rs` for the display impl. (ADR-0002.) | `gpui_linux` (headless platform) |
| `crates/gpui_platform/src/gpui_platform.rs` | `current_headless_renderer()` ungated (was `#[cfg(feature = "test-support")]`); adds Linux/FreeBSD arm returning `WgpuHeadlessRenderer`. macOS arm still gated on `test-support` (existing constraint). (ADR-0002.) | `gpui_platform` (headless platform) |
| `crates/gpui_platform/Cargo.toml` | Adds `gpui_wgpu` dep for the Linux/FreeBSD target (needed by the new headless-renderer arm of `current_headless_renderer`). | `gpui_platform` (headless platform) |
| `crates/util/src/paths.rs` | `home_dir()` honours an `SAWE_HOME` env var before the `test-support`→`/home/zed` hard-code, so a `test-support` build can run interactively against the real home. `script/run-mcp` sets it. Unit tests don't set the var. | build / agent testing |
| `crates/workspace/src/workspace.rs` | `Workspace::swap_worktrees_to(target_paths)` delta worktree reconciliation used by the in-place Solution switch (decision 16). Drops worktrees not in the set, adds missing ones, preserves overlapping `WorktreeId`s so LSP / panels / caches don't churn.; adds `run_config_strip` / `run_config_controller` slots (+ `set_run_config_strip` / `set_run_config_controller` / `run_config_strip()` / `run_config_controller()` getters) for the Run Configurations widget (set by `run_config_ui`; the `run_config_strip` view is read + rendered in the `ProjectToolbar` row). **(2026-06-23, decision #27)** Adds a `project_toolbar_item: Option<AnyView>` slot (+ `set_project_toolbar_item` / `project_toolbar_item()`, mirror of `titlebar_item`), rendered between the title bar and the body in `Workspace::render`. Gates the constructor's `on_focus_lost` refocus on `owns_window_chrome()` so retained background workspaces don't grab shared-window focus (decision #42). | `solutions_ui` / `solutions` |
| `crates/workspace/src/dock.rs` | **(phase 2b task 7)** `PanelButtons` has a single horizontal layout again: the `vertical` flag / `new_vertical` constructor that backed this fork's vertical edge strips are gone, along with the per-dock divider and right-dock button reversal (both were status-bar-era affordances). Its context menu now drops *below* its button (`Anchor::TopLeft` / `Anchor::BottomLeft`) because the buttons live in the project toolbar near the top of the window, not in the status bar at the bottom. | `title_bar` (`ProjectToolbar`) |
| `crates/workspace/src/persistence.rs` | `console_panel_state` table + `console_panel_tabs(workspace_id)` / `save_console_panel_tabs(...)` queries (decision 22). | `console_panel` |
| `crates/workspace/src/pane.rs` | **(decision #41)** Tab-bar New/Split/Zoom button visibility no longer gates purely on keyboard focus: `should_show_tab_bar_buttons` also shows them on `Workspace::active_pane` so they don't vanish/flicker when focus is in a dock panel. | `solutions` / dock-focus UX |
| `crates/welcome/src/welcome.rs` | Recent Solutions section + buttons. | `solutions_ui` |
| `crates/project_panel/src/project_panel.rs` | **(2026-06-23, decision #27)** No longer hosts a project dropdown; filters `state.visible_entries` to worktrees under the **solution-wide active member's** `local_path` (resolved via a private `active_solution` helper + `SolutionStore::active_member`) after each `update_visible_entries`, and subscribes to `ActiveMemberChanged`; resets `max_width_item_index` and recomputes `last_worktree_root_id` post-filter. | `solutions_ui` / `solutions` |
| `crates/git_ui/src/git_panel.rs` | **(2026-06-23, decision #27)** No longer hosts a project dropdown; `refresh_active_repository_for_selector` overrides `active_repository` with the active member's matching repo; subscribes to `ActiveMemberChanged`. (Per-member dropdown-badge change-count map retired with the selector.) **(decision #75)** That override now resolves through `solutions::active_member_repository`, and `set_active_repository` is the single seam every piece of per-repository panel state hangs off — the History tab's rows and subscriptions used to be cleared there, and since decision #100 the Commit tab is closed there for the same reason. **(decision #100)** The tab set is now `Changes \| Commit`; History is deleted. | `solutions_ui` / `solutions` |
| `crates/git_ui/src/git_panel/commit_tab.rs` | **New (decision #100).** The git panel's Commit tab: `CommitSelection` / `CommitSelectionSource`, the `CommitTabState` the panel hangs off an `Option`, the two guarded background loads (`Repository::show` / `load_commit_diff`), and the changed-files tree / message split / markdown style / client-side +/− fold **relocated verbatim** from the git graph's deleted commit-details sidebar. A private `mod`: `git_panel.rs` re-exports only `CommitSelection` and `CommitSelectionSource`, which is all `git_graph` needs to name. | `git_ui` / `git_graph` |
| `crates/git/Cargo.toml` | `test-support` feature now also activates `db/test-support` — the `db::static_connection!` macro's expansion references `db::open_test_db`, which only exists under that feature; without it, crates that enable `git/test-support` but not `db/test-support` fail to compile. Pre-existing latent workspace bug, fixed in-tree. **(2026-08-31)** It now also activates `gpui/test-support`, for the same reason one level along: `CommitDataReader::for_test`, gated `#[cfg(any(test, feature = "test-support"))]`, calls `BackgroundExecutor::simulate_random_delay`, which exists only under that feature — so `git_hosting_providers`, whose dev-deps enable `git/test-support`, could not compile its own test targets (E0599). Fixed in `git`'s feature list rather than in the consumer, so the arm and the method it needs stay in lockstep for every consumer. See `docs/findings/2026-08-31-cfg-universes-and-the-warning-gate.md`. | build / upstream-fix |
| `crates/git/src/repository.rs` | Adds `branches_containing` / `tags_containing` / `tags_pointing_at` / `load_commit_against_parent` methods on `GitRepository` (default no-op impls + real impls in `RealGitRepository`) for the S-DET commit-view metadata surface. Adds the module-level `parse_ref_name_lines` parser shared by all three ref queries (named `parse_contains_output` until `--points-at` joined them). Also: `load_commit_template` special-cases `ErrorKind::NotFound` on the `git config --get` spawn — treats "cwd disappeared" (e.g. an open repo whose underlying directory was removed mid-session via `git worktree remove`) as "no template available" with a debug log, instead of propagating ENOENT through to `detach_and_log_err`. | `git_ui` (S-DET) / general robustness |
| `crates/git/src/blame.rs` | `Blame::for_path_at_revision` + a `BlameTarget` enum, so `git blame` can annotate a commit-ish instead of only working-tree content piped on stdin (decision #58). **(decision #135)** Also `display_author` + `UNKNOWN_AUTHOR`, the shortened author name the blame gutter draws — it lives here because both the renderer (`git_ui`) and the gutter's width reservation (`editor`) have to agree on it. | `editor` (diff-pane blame, blame gutter width) / `git_ui` (blame gutter) |
| `crates/fs/src/fake_git_repo.rs` + `crates/fs/src/fs.rs` | `FakeGitRepositoryState::blames_at_revision` + `FakeFs::set_blame_at_revision_for_repo`, the test double for the revision-aware blame call. | `editor` (diff-pane blame) |
| `crates/fs/src/fs_watcher.rs` | `unwatch` swallows `notify::ErrorKind::WatchNotFound` and `PathNotFound` as benign races (the watched path was already removed from underneath us — `git worktree remove`, `rm -rf`, tempdir teardown — and the kernel-side watch was invalidated before our bookkeeping caught up). Logs at debug instead of propagating, so one removed directory tree doesn't flood the log with one ERROR per nested subdir's unwatch attempt. | general robustness |
| `crates/project/src/git_store.rs` | Adds `Repository::branches_containing` / `tags_containing` / `tags_pointing_at` / `load_commit_diff_against_parent` job-dispatch helpers. Adds `Repository::refresh_branches` + the shared `rescan_branches` helper (extracted from the tail of `Repository::push`) so a push that bypasses `Repository::push` can still republish ahead/behind and drop the cached graph log. | `git_ui` (S-DET) |
| `crates/settings_content/src/settings_content.rs` | Adds `CommitViewSettingsContent` (avatars, lazy threshold, mention parsing) + nested field on `GitPanelSettingsContent`. Also adds `SolutionAgentSettingsContent { ephemeral }` (S-AI-MSG ephemeral-pool sizing). Adds `RunConfigSettingsContent { toolbar }` + nested `run_config` field on `SettingsContent` (S-RUN). | `git_ui` (S-DET) / `solution_agent` (S-AI-MSG) / `run_config` (S-RUN) |
| `crates/settings/src/vscode_import.rs` | Add `solution_agent: None` field initializer to keep VS Code import in lockstep with the new `SettingsContent.solution_agent` field. Adds `run_config: None` for the same reason (S-RUN). | `solution_agent` (S-AI-MSG) / `run_config` (S-RUN) |
| `crates/task/src/task_template.rs` | Adds `before_commit: bool` field on `TaskTemplate` (default `false`). Read by `git_ui::pre_commit` to surface a task as a before-commit check row in the commit panel. | `git_ui` (S-PCH-HK) |
| `crates/project/src/task_inventory.rs` | Adds `Inventory::before_commit_templates(worktree)` accessor (mirrors `templates_with_hooks` shape) so the git panel can enumerate pre-commit-flagged tasks without touching `templates_from_settings`. Also adds `Inventory::task_templates_from_settings(worktree)` — a synchronous, context-free listing of settings-derived task templates used by the `task-ref` run-config provider (language runnables excluded; those need the async `list_tasks`). | `git_ui` (S-PCH-HK), `run_config` (S-RUN) |
| `crates/workspace/src/welcome.rs` | `render_agent_card` gated off via `false &&` — fork uses `solution_agent`, not upstream agent panel. | `solution_agent` |
| `crates/workspace/src/active_file_name.rs` | `ActiveFileName::new` now takes the `Workspace` (holds a `WeakEntity<Project>`); the status-bar label prefixes the worktree-relative path with the worktree's root name so it's unambiguous across a Solution's worktrees. (`status_bar.show_active_file` also flipped to `true` in `default.json`.) | rebrand / solutions |
| `crates/git_ui/src/conflict_view.rs` | The merge-conflict status-bar indicator and the in-editor conflict block both dispatched agent actions whose only handler early-returns without an `AgentPanel`, which this fork never registers — two silent no-ops, one of which also dismissed itself as if it had worked. Both now open the fork's own conflict resolver (`git_conflict_ui::OpenConflictResolver`) and are relabelled accordingly; the indicator no longer self-dismisses, and neither is gated on `AgentSettings::enabled`, since neither involves the agent any more. | `git_conflict_ui` |
| `crates/git_ui/src/commit_view.rs` | S-DET commit-view surface (header / parents / refs / contains / affected-files / footer decomposed into `commit_view::*` submodules). **(decision #136, 2026-09-02)** The `single_file: Option<RepoPath>` mode that used to live here is **deleted** — `open_file_diff`, `preview_holds_single_file_diff` and `open_internal` with it — and single-file commit diffs are served by `SoloDiffView`. What remains is the whole-commit view and the `base..head` compare-range view (#87). `open`'s `file_filter` parameter is *not* that mode and stays: it narrows which files the whole-commit diff shows while keeping the metadata chrome. | `git_ui` (S-DET) / `git_graph` |
| `crates/git_ui/src/commit_blob.rs` | **New (decision #136).** The historic-blob loader extracted out of `CommitView`: `GitBlob` (a `DiskState::Historic` synthetic file), `build_buffer` / `build_buffer_diff`, and `load_commit_file_blob`, which turns one `CommitFile` into a `LoadedBlob { buffer, diff, status, excerpt_ranges, path_key, is_binary }`. Two callers: `CommitView`'s per-file loop and `SoloDiffView::open_commit_file`. Takes `&mut AsyncWindowContext` deliberately — narrowing to `AsyncApp` would trade five recoverable `?` short-circuits for a `.upgrade().expect(..)` panic. | `git_ui` |
| `crates/git_ui/src/file_diff_view.rs` | **(decision #137)** Its multibuffer is a `MultiBuffer::singleton`, which is why `needs_expand_collapse_option` used to suppress the diff style controls here and nowhere else among the `SplittableEditor` consumers — so decoupling those controls from the collapse predicate **gave this view prev/next hunk + Unified/Split, which it never had**. A deliberate, reviewed widening: its sibling `text_diff_view` builds a headered `MultiBuffer::new` and so already had them, and a carve-out to exclude this one view is the exact class of gate that caused two regressions. | `git_ui` |
| `crates/search/src/buffer_search.rs` | The bar hosts the split-diff style controls (prev/next hunk, Unified/Split) for every `SplittableEditor` consumer that has no toolbar of its own — see decision #137 for `paints_diff_style_controls`, `keeps_primary_left` and `has_files_to_collapse`, and for why the first two must not be welded to the "Collapse All Files" predicate. Also carries the `debug_selector` paint tests over both the dismissed and the shown element tree. | `editor` / `git_ui` |
| `crates/paths/src/paths.rs` | `.zed` → `.sawe` rename for per-worktree config dir. Adds `run_configurations_file()` (global `~/.spk/sawe/config/run-configurations.json`) and `local_run_configurations_file_relative_path()` (`.sawe/run-configurations.json`) for S-RUN. Adds `remote_control_settings_file()` (`~/.spk/sawe/config/remote-control.json`) for R-1. Adds `remote_control_cert_file()` / `remote_control_key_file()` siblings for the R-2 self-signed TLS cert + key (persisted across restarts so fingerprint pinning stays stable). | rebrand / `run_config` (S-RUN) / `remote_control` (R-1 / R-2) |
| `crates/gpui_tokio/src/gpui_tokio.rs` | Adds `Tokio::try_handle(cx) -> Option<tokio::runtime::Handle>` — the non-panicking analogue of `Tokio::handle`, used by `remote_control::store::start_listener_async` to short-circuit when the runtime isn't installed (rather than panic deep in the bootstrap path). | `remote_control` (R-2) |
| `assets/keymaps/default-*.json` | Default shortcuts for Solutions / sessions. Adds `alt-shift-f10` → `run_config::Run`, `alt-shift-f9` → `run_config::Debug`, `alt-shift-f2` → `run_config::Stop` (Workspace context; IntelliJ-style — `alt-shift` variants chosen because `shift-f10`/`shift-f9`/`ctrl-f2` are already bound in Editor context). | `solutions_ui` / `run_config_ui` |
| `assets/settings/default.json` | Default `solutions.root`; default `icon_theme: "Material Icon Theme"` + auto-install of the matching extension (colored project tree, IDEA-like, vs upstream's monochrome `Zed (Default)`); default `toolbar.{breadcrumbs,quick_actions,selections_menu}: false` (IDEA-style — no toolbar row under the tab bar; whole row disappears when all items hidden, Ctrl+F search bars unaffected; re-enable per-user in settings or per-editor via `editor::ToggleBreadcrumb`); default `project_panel.{sticky_scroll,auto_fold_dirs}: false` (the pinned ancestor rows cover the tree while scrolling, and the folded `a/b/c` chains hide real directory levels — both are opt-in here, doc-comment defaults in `settings_content/src/workspace.rs` updated to match). | `solutions` / rebrand |
| `crates/zed/Cargo.toml` `[[bin]]` | Binary name overridden to `sawe` (cargo crate `zed` unchanged). | rebrand |
| `.cargo/config.toml` | `[target.x86_64-unknown-linux-gnu]` block forcing `-fuse-ld=mold`. See decision 15. | build |
| `crates/terminal_view/src/terminal_panel.rs` | Dropped the now-unused `TerminalDockPosition` import (a local edit had removed its only use, leaving a dead import that failed `clippy -D warnings`). | upstream-fix |
| `crates/ui/src/components/scrollbar.rs`, `crates/gpui/src/elements/div.rs` | **(decision #82)** `track_anchor` / `tracks_scroll_handle` + `nested_in_scroll_container`; gpui gains `Interactivity::tracked_scroll_handle()` and `PartialEq` for `ScrollHandle`. | `ui` / `gpui` |
| `crates/git_ui/src/rollback_modal.rs` | **New.** IDEA's Rollback Changes dialog: checkbox tree over the affected files (reusing the git panel's `TreeViewState::build_tree_entries`), "N modified" summary, "Delete local copies of added files", Rollback / Close. Only checked files are rolled back. | `git_ui` |
| `crates/workspace/src/mcp/windows.rs` | `windows.click_at` gained a `clicks` parameter — two separate calls are not a double click, so a handler branching on `click_count()` was untestable. Also `windows.resize` (content size in logical pixels): the headless window is fixed at 1920x1080, which hid the Solution band's status-bar overflow from every agent-driven check. It calls `Window::bounds_changed` after `Window::resize` because the headless platform window mutates its bounds without firing the resize callback. | `workspace` |
| `crates/workspace/src/status_bar.rs` | `flex_none` on the 30px row — it silently absorbed the workspace column's overflow (default `flex-shrink: 1`) and an over-tall Solution band ate it. | `workspace` |
| `crates/editor/src/split_connectors.rs` | Connector ribbons for the side-by-side diff. **(decision #62)** `ribbon_edges` gives a collapsed insertion edge the insertion rule's real 2px extent so the ribbon and the rule join flush. | `editor` |
| `crates/editor/src/split.rs` | **(decision #79)** The left pane mirrors the right pane's `show_headers()` instead of guessing from `is_singleton()`. | `editor` |
| `crates/git_ui/src/solo_diff_view.rs`, `crates/git_ui/src/project_diff.rs`, `crates/git_ui/src/commit_view.rs` | **(decision #78)** Diff toolbars lost every staging/commit button and gained the `N difference(s)` count (`difference_count_label` + `HunkCountCache`). | `git_ui` |
| `crates/solutions/src/member_repository.rs` | **New (decision #75).** The single member→repository resolver: `active_member_context`, `repositories_under` (outermost first), `active_member_repositories`, `active_member_repository`, `set_active_member_repository`. Holds the per-`(SolutionId, MemberId)` explicit pick as a `Global`. Replaces six duplicated `find(… starts_with …)` copies in `git_ui`, `git_graph` and `title_bar`. | `solutions` |
| `crates/solutions/src/store.rs`, `crates/solutions/src/db.rs` | **(decision #27)** `active_member` model replaces `panel_member_selections`/`PanelKind`; `active_member` SQLite table + append-only migration; `set_active_member` / `ensure_active_member` / `active_member_worktree`; `ActiveMemberChanged` event. `event_sources.rs` emits MCP kind `solution_active_member_changed`. | `solutions_ui` / `solutions` |
| `crates/solutions_ui/src/project_tab_strip.rs`, `project_tab.rs`, `add_project_picker.rs` (moved), `solutions_ui.rs` | **(decision #27)** New `ProjectTabStrip` + `ProjectTab` (mirror of solution strip); `AddProjectPicker` promoted to a standalone module; `active_project_selector.rs` + `member_picker.rs` DELETED; shared `dot_color_for_str` extracted in `solution_tab.rs`. | `solutions_ui` |
| `crates/run_config_ui/src/toolbar_strip.rs`, `run_controller.rs`, `Cargo.toml` | **(decision #27)** Run-config strip filters configs to the active member's worktree (`filter_configs_for_active_worktree`) + subscribes to `ActiveMemberChanged`; `RunController::revalidate_selection_against` reselects when the active project changes so Run/Debug follow it. New `solutions` dep. | `run_config_ui` / `solutions` |
| `crates/git_ui/src/branch_picker/popup.rs` | **(decision #27 follow-up)** Branch-popup action header: "Update Project"/"Push" act on the active project's repo; new "Update All Projects" row (solution-wide update, ≥2 members). Comment touch for single-repo commit. | `git_ui` / `solutions` |
| `crates/git_ui/src/repository_selector.rs` | **(decision #27 follow-up)** Repo switcher list scoped to the active member's `local_path` (fallback: all repos for non-solution projects). | `git_ui` / `solutions` |
| `crates/git_ui/src/providers.rs`, `crates/solution_git/src/solution_git.rs` | **(decision #27 follow-up)** `SolutionPanelProvider` (solution-wide commit) deleted: removed the trait re-export/registry slot/setter+getter and the `solution_git` registration block. Deleted files: `git_ui/src/providers/solution_panel.rs`, `solution_git/src/commit.rs` (+ its `solution.git.commit_all` MCP tool). PUSH/UPDATE providers + dashboard kept. | `git_ui` / `solution_git` |
| `crates/solution_agent/src/store/supervisor_engine.rs` | **New (god-object refactor Tier 3).** Partial-class split of `SolutionAgentStore`: the ~36 observer/supervisor methods (judge/auditor spawn + verdict application `apply_verdict{,_authenticated}` / `apply_audit_verdict{,_authenticated}` / `on_judge_failed` / `apply_usage_limit_stop`, the `tick_supervisor` + `tick_stuck_sessions` watchdogs carrying the #5–#9 hardening, supervision state/nudge glue) plus the `JudgeHandle` / `VerdictAuth` types relocated VERBATIM out of `store.rs` (10161→7998 lines) into a `mod supervisor_engine` child (mirrors `store/queue.rs`). Same idiom as queue: `impl SolutionAgentStore` blocks, `self`/`Context<Self>`/fields unchanged — source text split, not state ownership. | `solution_agent` |
| `crates/search/src/find_in_path.rs` | **New.** IDEA-style Find-in-Path modal (see decision #53) — bespoke `ModalView`: header input + option toggles + scope tabs (In Solution / In Project / Directory) + file mask fields, grouped streaming results list, live read-only preview editor, Replace/Replace All. Opens seeded from the active editor (`query_from_active_editor`, same `seed_search_query_from_cursor` semantics as `ProjectSearchView`); the seed is applied after the modal exists so it drives the search through the normal `Edited` subscription. | `search` |
| `crates/search/src/find_in_path_tests.rs` | **New.** Unit tests for `find_in_path.rs`. | `search` |
| `crates/search/src/search.rs` | Registers `find_in_path::init` alongside the existing `project_search` registration. | `search` |
| `crates/search/Cargo.toml` | Adds `schemars` (modal action/settings schema) + `solutions` (Solution/member scope resolution for the scope tabs) deps. | `search` |
| `crates/editor/src/display_map.rs` | Adds `HighlightKey::FindInPathPreview` variant used to highlight the active match in the Find-in-Path preview editor. | `search` |
| `crates/file_finder/src/file_finder.rs` | IDEA parity: `ToggleFileFinder` seeds the picker query from the active editor's **selection** (`FileFinder::query_from_selection`), so selecting a path in a buffer and hitting `ctrl-shift-n` searches for it. Single-line selections only; suppressed entirely when `seed_search_query_from_cursor` is `never`. | `file_finder` |
| `crates/editor/src/input.rs` | **(decision #83)** `Editor::newline` no longer extends its edit back to column 0 to clear the auto-indent whitespace of the line it leaves, so a blank line inside a block keeps the block's indent. | `editor` |
| `crates/language/src/buffer.rs`, `crates/multi_buffer/src/multi_buffer.rs`, `crates/editor/src/selection.rs` | **(decision #83)** `Buffer::set_caret_positions` / `MultiBuffer::set_caret_positions` (local-only bookkeeping — no collab op, no notify) plumbed from `Editor::selections_did_change`; `Buffer::remove_trailing_whitespace` skips rows that hold a cursor. | `editor` / `project` |
| `crates/editor/src/split_editor_view.rs` | **(decision #84)** `SplitEditorState` collapsed to a single `left_ratio` written through `set_ratio`, which stores `LastSplitRatio` as the divider moves; the container's dead `on_drop` commit was removed. | `editor` |
| `crates/git_graph/src/git_graph.rs` | **(decision #85)** Multi-row commit selection: `selected_entry_idxs` + `selection_anchor_idx`, the pure `fold_row_click` / `is_first_parent_chain` helpers, modifier-aware `on_row_click`, and `deploy_multi_commit_context_menu`. | `git_graph` |
| `crates/git_ui/src/commit_context_menu.rs` | **(decision #85)** `MultiCommitContext` + `build_multi_commit_context_menu` (Compare Versions / Squash Commits… / Copy Hashes), and `NameInputModal::with_initial_text` so the squash prompt starts from the surviving commit's subject. | `git_ui` / `git_graph` |
| `crates/git/src/operations/squash.rs`, `crates/git/src/operations/fixup.rs` | **(decision #86)** Commits newer than the folded range are `pick`ed instead of being rejected as a contiguity violation. | `git` |
| `crates/git/src/repository.rs`, `crates/project/src/git_store.rs` | **(decision #87)** `load_commit_range(base, head)` on the `GitRepository` trait + the `Repository` entity; `load_commit`'s cat-file blob loader extracted into a shared helper the three loaders now call. | `git` / `project` / `git_ui` |
| `crates/git_ui/src/commit_view.rs` | **(decision #87)** `CommitView::open_range` + the `compare_range` field: a bare two-commit diff tab titled `base..head`. | `git_ui` |
| `crates/solution_agent/src/status_row.rs`, `session_view.rs`, `session_view/lifecycle.rs` | **(decision #88)** `ratchet_used_tokens` + `status_peak_thread`: the meter's high-watermark is scoped to the `AcpThread` that produced it. | `solution_agent` |
| `crates/solution_agent/src/db/sessions.rs`, `crates/solution_agent/src/store.rs` | **(decision #88)** `SolutionAgentDb::clear_total_tokens` (the COALESCE upsert cannot clear a column), called from `rotate_context` / `reset_context`. | `solution_agent` |
| `crates/multi_buffer/src/multi_buffer.rs` | `MultiBuffer::language_settings` / `MultiBufferSnapshot::language_settings` resolve through `representative_buffer_id()` — the excerpted buffers' settings are used only when they all agree on the language, otherwise the language-agnostic defaults apply. Upstream took the **first excerpt's** buffer, which made one Markdown file in a diff soft wrap every other file in it (see decision below). | `editor` / `git_ui` |
| `crates/editor/src/config.rs`, `crates/editor/src/split.rs` | `Editor::is_soft_wrap_enabled(&App) -> bool` and `SplittableEditor::is_soft_wrap_enabled(&App) -> bool` — public read-only accessors so a toolbar outside the `editor` crate can drive a soft-wrap toggle's `toggle_state` without widening `soft_wrap_mode` (`pub(super)`) or the private `soft_wrap_mode_override` field. | `git_ui` |
| `crates/git_ui/src/solo_diff_view.rs` (toolbar) | `SoloDiffStyleToolbar` gained a `TextWrap`/`TextUnwrap` soft-wrap toggle next to Unified/Split. It **dispatches the `ToggleSoftWrap` action** rather than calling `Editor::set_soft_wrap_mode`, so `SplittableEditor`'s `capture_action` handler copies the override to both panes and their `Block::Spacer` row balancing stays aligned. Shared icon/label helpers `soft_wrap_icon` / `soft_wrap_tooltip` live in `git_ui.rs`. | `git_ui` |
| `crates/image_viewer/src/image_viewer.rs` | **First local change.** Fit-to-window is a persistent mode (`ZoomState { level, pan_offset, fit_to_window }`) re-derived from the current container bounds on every layout instead of a one-shot latch on first layout, so resizing the pane re-fits; the zoom readout doubles as the actual-size control and is `disabled` once the image is at its true size (see decision #64). | `image_viewer` |
| `crates/theme/src/styles/colors.rs`, `crates/theme/src/default_colors.rs`, `crates/theme/src/fallback_themes.rs`, `crates/theme_settings/src/schema.rs`, `crates/settings_content/src/theme.rs`, `crates/ui/src/styles/color.rs` | **First local change.** Adds a dedicated `version_control_untracked` theme colour (`version_control.untracked` in theme JSON, `Color::VersionControlUntracked` in `ui`) so the Changes panel can tint unversioned files without reusing `version_control_added` — green already means "added to the index" in that same panel. Defaults to `tomato().{light,dark}().step_11()` (IDEA-style brick red). Optional in theme JSON like its `version_control.*` siblings, so bundled themes that omit it fall back to the default. | `git_ui` (Changes panel) |
| `crates/debugger_ui/src/debugger_panel.rs`, `crates/debugger_ui/src/debugger_ui.rs`, `crates/debugger_ui/src/attach_modal.rs`, `crates/debugger_ui/src/new_process_modal.rs`, `crates/debugger_ui/src/session/running.rs`, `crates/debugger_ui/src/persistence.rs`, `crates/debugger_ui/src/tests*.rs` | **First local change (phase 2b task 5, decision #91).** `DebugPanel` de-docked into the Solution band's utility section: `Panel` impl and `EventEmitter<PanelEvent>` removed (`Render` + `Focusable` kept), `Panel::position` collapsed to a `BAND_DOCK_POSITION: DockPosition = Bottom` constant with the side-dock layout branches deleted, panel-level zoom reduced to an explicit no-op, and the "Close Panel" button repointed from `workspace::ToggleBottomDock` to hiding the band's utility section. Every `Workspace::panel::<DebugPanel>` / `focus_panel` / `open_panel` call site moved to the new `debug_panel_for_workspace` / `reveal_debug_panel` / `handle_toggle_focus` helpers. `RunningState::invert_axies` deleted (its only caller was `Panel::set_position`; the restore path is `RunningState::new`'s `should_invert` into `deserialize_pane_layout`). Test bootstrap installs a real `SolutionBand` — without one nothing paints the panel. | `debugger_ui` |
| `crates/collab/tests/integration/remote_editing_collaboration_tests.rs` | **First local change (phase 2b task 5).** The two SSH debugger tests install `DebugPanel` into `Workspace::solution_band_utility_item` under `UtilityKind::Debug` and look it up with `debug_panel_for_workspace`, instead of `add_panel` + `Workspace::panel::<DebugPanel>` — the panel has no `Panel` impl any more. Behaviour of the tests is unchanged; collab itself stays disabled in this fork. | `debugger_ui` |
| `crates/settings_ui/src/page_data.rs` | **First local change (phase 2b task 5).** Dropped the "Debugger Panel" section (`debugger.dock` dock-position picker) from the Panels page and the "Debugger Button" switch (`debugger.button`) from the Status Bar section. Both settings are inert in this fork — the debugger is hosted in the band, not a dock, and the dock strips the button gated are gone — so rendering working-looking controls for them was exactly the dead-control trap. The settings fields themselves are kept in the schema (deleting them is a migration), annotated as inert in `assets/settings/default.json` and `docs/src/debugger.md`. | `settings_ui` |
| `crates/install_cli/src/install_cli_binary.rs`, `crates/install_cli/src/install_cli.rs`, `crates/install_cli/Cargo.toml` | **First local change (decision #115).** `File → Install CLI` targets `/usr/local/bin/sawe`, never removes an entry it cannot prove it created, and no longer registers a URL scheme. `register_zed_scheme.rs` (the `cli::RegisterZedScheme` palette action + `cx.register_url_scheme(ZED_URL_SCHEME)`) is **deleted**, along with the crate's `client` dependency, which existed only for that constant. | `install_cli` |

| `crates/zed/src/zed/open_listener.rs`, `crates/zed/src/zed/open_url_modal.rs`, `crates/zed/src/zed/windows_only_instance.rs`, `crates/zed/src/main.rs`, `crates/cli/src/main.rs`, `crates/client/src/client.rs`, `crates/client/src/zed_urls.rs`, `crates/agent_skills/agent_skills.rs`, `crates/settings_ui/src/settings_ui.rs`, `crates/settings/src/settings_store.rs`, `crates/json_schema_store/src/json_schema_store.rs`, `crates/terminal/src/alacritty/hyperlinks.rs`, `crates/zed_actions/src/lib.rs`, `crates/project/src/lsp_store/json_language_server_ext.rs`, `assets/settings/default.json`, `tooling/xtask/src/tasks/workflows/run_tests.rs`, `docs/src/**` | **Decision #116.** `sawe://` is the URL scheme this fork parses and produces; `zed://` has no arm, no normalisation and no producer. Every routing arm, the settings **Copy Link**, the skill share link, the builtin-JSON-schema URI prefix (`sawe://schemas/`, which the JSON language server round-trips), the terminal hyperlink regex, the Open-URL modal placeholder and short-circuit, and both CLI/editor url classifiers moved together. | `zed` |
| `crates/gpui_linux/src/linux/platform.rs` | **First local change (decision #117).** `KEYRING_LABEL` is `sawe-github-account`, not `zed-github-account`. The Secret Service keyring is an OS-wide namespace, so this was the one rebrand leftover that actually collided with a real Zed install rather than merely reading wrong. | rebrand |
| `script/uninstall.sh` | Repointed at `paths::base_dir()` (`~/.spk/sawe`), which is the only directory the binary creates; it previously removed `~/.config/sawe`, `~/.local/share/sawe` and `~/Library/Application Support/Sawe`, none of which exist, so neither its per-channel cleanup nor its "keep your preferences?" prompt did anything. Never removes the root recursively — `ss/` under it is the Solutions root and holds the user's project checkouts. | rebrand |
| `.github/workflows/*.yml` (21 generated files) | **Decision #118.** One line under the `cargo xtask workflows` rebuild instruction saying not to follow it: regenerating deletes `retag_release.yml`, strips 25 `if: false # sawe: not applicable` guards and restores 5 upstream triggers this fork narrowed to `workflow_dispatch:`. | build |

Locked rebrand identifiers (display name, bundle ids, URL scheme, config dirs, etc.) — see `.rules` § "Locked rebrand identifiers". Changing any requires explicit approval — they're cross-referenced in spec docs.

## Key architectural decisions

These are decisions where the "obvious" approach was rejected for a non-obvious reason. Knowing the *why* helps you avoid undoing them.

### 1. `editor_mcp` is a sibling crate, not part of `agent`/`workspace`/`zed`

Why: `editor_mcp` would create a dep cycle if it lived inside `workspace` (workspace would depend on it for tool registration; it depends on workspace for the `Workspace` type). Sibling crate + `register_tool` from each domain crate's `init` breaks the cycle.

How to apply: when adding new MCP tools, register them from the crate that owns the underlying state — never from inside `editor_mcp` itself.

### 2. Solutions, not "Workspaces"

Why: upstream Zed already overloads `Workspace` (single-window) and `MultiWorkspace` (sidebar that switches between projects). The Catalog/Solution layer sits **above** all of that and adding another meaning for "workspace" would have been confusing. `Solution` was deliberately picked as a fresh term.

How to apply: never refer to a Solution as a "workspace" in user-facing strings or commit messages.

### 3. Subprocess pool keyed by `(SolutionId, AgentServerId)`, not per-session

Why: `AgentServer::connect()` sets cwd at subprocess spawn time. Multi-session-per-cwd is the normal ACP pattern (sessions multiplex over one subprocess via `acp::SessionId`). Spawning per-session would burn quota + memory; per-pair gives the right granularity for solution-scoped work.

How to apply: when adding a new agent (Codex, Gemini), it gets its own pool entry per Solution. Closing the last session in a `(solution, agent)` pair arms a 60-second debounced shutdown — see `crates/solution_agent/src/pool.rs::SHUTDOWN_DEBOUNCE`.

### 4. Solution sessions live PAST window close

Why: long-running agent tasks (e.g. "refactor across all members") shouldn't die because the user closes the window mid-task. The pool stays alive while the Solution exists in `SolutionStore`; closing the *Solution* is what kills its subprocesses. Notification on completion can re-open the window via `solutions.open`.

How to apply: don't tie session lifecycle to workspace events. The wire-up is in `crates/solution_agent/src/store.rs::on_solution_event`.

### 5. cwd = `solution.root` (always), no per-session member override

Why: the whole point of Solutions is cross-project work. Forcing per-member cwd would make cross-project tasks awkward. Per-member CLAUDE.md / settings still get loaded by the agent reading them on demand inside member subdirs. Trade-off: `git status` etc. need an explicit `cd member` first — agents handle this fine.

How to apply: `make_production_project_for_solution` (in `crates/solution_agent/src/pool.rs`) is currently a stub — Plan B (`create_session` takes `project: Entity<Project>` from the open workspace) is in use. If you wire production-side synthesis, keep cwd = solution.root.

**2026-06 → 2026-08 interlude:** for a stretch this decision was reversed — sessions gained a per-session `member_id` binding and were created with the active member's `local_path` as cwd, so switching the active project could filter which chat tabs were even visible. That contradicted decision #27 ("open files / terminals / AI dialogs stay agnostic" of the active member) and hid dialogs on a project switch — a regression tracked and reverted by the `2026-08-26-solution-scoped-sessions` plan (spec: `docs/plans/2026-08-26-solution-band-ai-dialogs-design.md`). "cwd = solution.root always, no member override" is TRUE again as of that plan. See decision #89 for the removal details.

### 6. MCP event kinds use `agent_session_*` prefix (not bare `session_*`)

Why: defensive namespacing. Other future subsystems might emit `session_*` events; the prefix makes the source unambiguous on the wire.

How to apply: any new event from `solution_agent` must follow the same prefix. Tests/consumers reference `agent_session_created`, `agent_session_state_changed`, etc.

### 7. Sessions live INSIDE the bottom-dock ConsolePanel, not as workspace pane Items (reversed; navigator retired)

Why: an earlier draft tried "sessions as pane items + side-panel navigator". In practice the navigator just duplicated the editor's tab strip (same uuid in two places) and the chat ended up competing with code for the main editor area without users actually wanting that split — the "session A next to code on the right" use case is rare while "where is my chat?" was constant. The flagship-AI-editor pattern (Cursor / Cody / Copilot Chat / upstream Zed AgentPanel) puts chat in a dedicated docked panel with its own internal tab strip, and that is what users expect.

A later iteration (2026-05-26) merged the standalone `SolutionSessionsNavigator` dock and the upstream `TerminalPanel` into a single bottom-docked **`ConsolePanel`** that hosts both terminal and AI-chat tabs in one tab strip with heterogeneous icons (Terminal / Sparkle). The navigator dock is gone; `render_status_row` lives as a free function at `solution_agent::status_row::render` and is called directly from `SolutionSessionView::render`.

**Superseded again by decision 93 (phase 2a, 2026-08-26): chat left `ConsolePanel` for the Solution band.** `ChatProvider` is deleted; `ConsoleTab` has a single `Terminal` variant. The surviving half of this decision is the negative one, and it still holds: do NOT re-introduce `Item`/`add_item_to_active_pane` hosting for sessions. For where chat lives now, read decision 93; terminal tabs are still spawned via `console_panel::TerminalProvider::new_tab(cwd, …)` (calls `Project::create_terminal_shell`).

### 8. AI auth via CLI subscription, NOT API keys

Why: respects the user's Claude subscription policy. The subprocess inherits `~/.claude/` via `$HOME` and authenticates itself; the editor never sees a token. `ANTHROPIC_API_KEY=""` is explicitly empty in the spawn env (set in `crates/agent_servers/src/custom.rs::CLAUDE_AGENT_ID` branch).

How to apply: never inject Anthropic credentials into a subprocess env. If a user wants BYOK, they configure that through Zed's normal language model providers — those are kept but not promoted in UI.

### 9. File drops on a session view insert plain `@path` text, not `MentionSet` entries

Why: upstream `agent_ui::MessageEditor` integrates with a heavy `MentionSet` machinery (mention rendering, project-path resolution, capability negotiation). Pulling that into `solution_agent` would couple us to `agent_ui` internals. v1 keeps the compose box a vanilla `editor::Editor` and the drop handler emits text like `@member-name/src/lib.rs`. The agent reads the path on its own via the `Read` tool — no editor-side resolution needed.

How to apply: if rich mentions or capability-aware path expansion become user requirements, integrate `agent_ui::message_editor::insert_mention_for_project_path` and bring `MentionSet` along — don't half-build a parallel mention layer in `solution_agent`. Plain text paste (`Ctrl+V` for clipboard text) works via `editor::Editor`'s native action; no patch needed.

### 10. Welcome page is the launcher; `restore_on_startup = "none"` by default

Why: the editor is built around Solutions. Restoring "the last workspace" pins the user to whatever they happened to close last (often a one-off `/tmp` or a single member subfolder), hiding the rest of their solutions. The fork's startup story is "open the editor → see all your solutions → pick one (or create one)". Welcome is always shown; the Solutions section in `solutions_ui::welcome` lists every solution (opened-recent first, never-opened in store order) and always shows a `Create new solution` button.

How to apply: the default lives in `assets/settings/default.json` (`"restore_on_startup": "none"`). Users can override it in their own settings if they want upstream behavior. The Welcome section renderer in `crates/solutions_ui/src/welcome.rs::render_section` is the single place that defines what the launcher shows — keep it as the only fork-local Welcome section unless there's a strong reason for more.

### 11. Single-instance handoff with no args focuses the existing window (best-effort on Linux)

Why: when the user runs `sawe` a second time without path args while another instance is alive, the new process should NOT silently exit while the existing window stays buried. The handoff endpoint (`workspace::mcp::handle_cli_args`) now picks the first existing window and dispatches `Window::activate_window` (X11 `_NET_ACTIVE_WINDOW` ClientMessage).

How to apply: this is best-effort on Linux. Most window managers implement focus-stealing prevention — the WM will only honor an `_NET_ACTIVE_WINDOW` request from a process with a recent user-interaction timestamp. The new `sawe` invocation has no such timestamp, so the WM may downgrade the request to a taskbar-flash or ignore it entirely (mutter / KWin do this aggressively; lighter WMs like i3 / sway honor it). `App::activate(...)` is a documented no-op on the upstream Linux backend (`activate is not implemented on Linux, ignoring the call`). User-facing options: disable focus-stealing prevention in the WM, OR launch with an explicit path which goes through `open_paths` and forces a new window.

### 12. Image paste: clipboard `gpui::Image` → base64 → `acp::ContentBlock::Image`

Why: Claude (and other ACP agents that declare the `image` prompt capability) accepts image content blocks alongside text. We want native paste UX without dragging in `MentionSet`. The compose box registers a `capture_action(Paste)` handler that runs **before** the editor's default text-paste, inspects the clipboard, and:
- if the first entry is text → returns without consuming (action falls through to the editor's text paste)
- if the first entry is an image → encodes via `base64::engine::general_purpose::STANDARD`, stashes a `PendingImage` on the view, drops a `[image #N]` placeholder into the buffer, and calls `cx.stop_propagation()`

On submit, `pending_images` are converted to `acp::ContentBlock::Image(ImageContent::new(base64, mime))` and combined with the text block via `SolutionAgentStore::send_message_blocks(...)` (the new structured-content API alongside the legacy text-only `send_message`).

How to apply: this is a deliberate parallel implementation of upstream's `paste_images_as_context`, NOT a reuse. The upstream version requires `MentionSet`, image-upload state, capability checks — all coupled to `agent_ui`. Our path stays self-contained inside `solution_agent`. If the agent doesn't support images (capability missing), the call still goes out — the agent rejects with an error that surfaces to the user as a normal `Errored` state. Adding capability negotiation pre-flight is a follow-up.

### 14. Editor's embedded MCP socket is bridged into spawned ACP subagents via `<exe> --nc <socket>`

Why: `editor_mcp` exposes 58+ tools (`solution_agent.*`, `solutions.*`, `editor.*`, `windows.*`, `workspace.*`, `project.*`, `diagnostics.*`) over a Unix socket at `~/.spk/sawe/state/mcp.sock`. Upstream's `agent_servers::acp::mcp_servers_for_project` only feeds claude-acp / codex-acp / gemini the MCP servers configured in user settings — so the embedded server is invisible to those subagents. ACP's `McpServer` enum supports `Stdio` and `Http` transports, but not Unix sockets. The fork already ships an `--nc <socket>` mode in the editor binary (`crates/nc/src/nc.rs`) that proxies stdin/stdout to a Unix socket — same pattern upstream uses for the `--askpass` SSH flow. So the bridge is: a fork-local entry in `mcp_servers_for_project` that runs `<current_exe> --nc <socket_path>` as the stdio command. Spawned subagents speak JSON-RPC stdio to that subprocess, which forwards to the editor socket.

How to apply: the entry is named `sawe`, gated on the socket file existing (so headless test runs that never started the server skip it cleanly) and on `current_exe()` resolving. Implementation in `crates/agent_servers/src/acp.rs::sawe_mcp_bridge_server`. **Security note:** the cross-solution leak this note originally flagged is now closed by **decision 17** (per-solution sockets) — a subagent spawned for a Solution is bridged to that Solution's own socket, which serves only the solution-scoped tool subset with `solution_id` force-injected.

**Reliability caveat (empirical, 2026-06-27):** the `sawe` MCP server does NOT reliably surface as `mcp__sawe__*` tools inside a spawned **claude-acp** subagent. claude's MCP-server registration silently drops it — across all `~/.claude/projects` history, no claude session ever called a `mcp__sawe__*` tool; the only MCP tools claude sees are the user's own globally-configured servers (runner/playwright/citeck/outwall). The bridge transport itself is fine (driving `<bin> --nc <socket>` by hand returns the full `tools/list`, 132 tools incl. `solution_agent.*`), but you cannot count on a claude subagent having those tools in its toolset. **When a subagent must call back into the editor, give it the `<bin> --nc <socket>` Bash recipe in its prompt** (a bare `tools/call` works with no `initialize` handshake; pipe `( printf '%s\n' "$req"; sleep 2 ) | timeout 12 <bin> --nc <socket>`) and forbid `ToolSearch`/grepping raw `~/.claude` transcripts — do NOT assume `mcp__sawe__*` exists. This is exactly how the Supervisor judge reaches the socket (commit `891069f8cf`); see also the per-turn briefing in `crates/solution_agent/resources/supervisor_judge_instructions.md`.


### 110. A restore may only rewrite a transcript it actually read

What: `SolutionSession` restore computes a `migrating` flag, and `migrating` authorizes a **destructive rewrite** — `persist_all_rows` flushes zero rows and bumps the epoch, and for a row-native session `trim_from_idx = 0` deletes every row. It was derived from "the entry set came back empty", which is where **every** way of failing to read one lands.

Why: four separate swallowed read failures each collapsed onto that same empty set and turned a healthy session into a wiped-looking one. A failed blob *decode*, a failed blob *load*, and a failed *entry-row* load all yield "no rows" — and the row one is the worst in the set, because a born-row-native session has `epoch` 0, so nothing gates the flush and `delete_entries_from_idx(id, 0)` removes the transcript. Measured 3 rows → 0, unrecoverable, from **one transient sqlite read error**. A failed *epoch* load does the mirror image: it collapses to `0`, which `is_wiped_row_native` reads as "legacy, consult the blob", so a genuinely `/clear`ed session gets its erased conversation repainted, written back as rows, and its epoch rewound 5 → 1 — permanently, because rows now exist and every future read takes the rows branch.

How to apply:
- **The rule, and it is grep-checkable:** every awaited `SolutionAgentDb::load_{entries,blob,epoch,cold_head}` on a transcript-restore path must either `?` or set `transcript_unavailable`. **There is no third arm.** `load_change_seq` is the one documented exception — a client cursor, not an input to `migrating` or to a flush. That is fifteen production call sites today — nine in `store/hydration.rs`, six in `mcp/read.rs`, found by `grep -rn '\.load_entries(\|\.load_blob(\|\.load_epoch(\|\.load_cold_head(' crates/solution_agent/src`. Re-check them in one grep rather than reasoning about which failure points which way. Two things keep the grep total: `load_entries_blocking` is a fifth loader outside those four names (no production caller today, which is exactly why it is the one that slips past later), and "or set `transcript_unavailable`" is sufficient **only because the flag lives on the session** — re-implemented as a local `bool` the rule reads satisfied while the close-flush hole returns.
- **"Any `unwrap_or` / `.ok()` on a value feeding this decision is the bug" is the right shortlist generator and the wrong rule.** Both fixed sites still match it literally, and a fifth swallow written `match … { Err(_) => 0 }` matches nothing. Use it to find candidates, then apply the rule above.
- **Stopping the write is not the same as stopping the repaint**, and a one-line fix that conflates them ships a half-fix. Joining the epoch read to the flag stops `migrating`, but with the epoch collapsed to `0` the `wiped_row_native` check still reads false and the retained blob is still decoded — so the flag has to gate the blob **load** as well. A mutation that reverts only that second gate still repaints.
- **The flag lives on the session, not on the load**, because a full flush is the same destructive rewrite through a different door: after a failed read the tab sits live-and-empty, and *closing* it runs `persist_all_rows` → trim from 0. A test that evicts the session from `store.sessions` by hand instead of closing it passes with the guard reverted — that substitution is exactly what hid this once already.
- **The residual is closed by two opposite mechanisms, because there are two kinds of writer and they need opposite answers.** A **send** retries then refuses: `send_message_blocks_targeted` — the one funnel every send passes through — routes a flagged session to `retry_transcript_load`, which re-reads **all three** inputs, `?`-ing every one, and clears the flag only inside the same update that repopulated the entity. A **system note** is simply dropped with a warning. The asymmetry is the point: suppressing a send silently loses the USER'S TURN, which is why suppression is rejected above; a system note is editor-generated breadcrumb text about the editor's own recovery, so dropping it loses nothing.
- **The complete question is "what reaches `persist_main_stream`", not "what reaches `AcpThread::push_entry`", and neither is "what can the agent produce".** Each narrower framing missed a door in turn. `push_system_note` was the door that needed no typing: a note is a full transcript append, and `respawn_agent` fires one **unconditionally** after every successful respawn — the tab-strip Restart agent button, the stuck-session watchdog, and the `restart_agent` / `reconnect_agent` MCP tools. Measured on a shipped commit with no send anywhere: **3 rows → 1 and a wipe marker rewound 5 → 0**. `respawn_agent`'s own `persist_all_rows` was already declined by the flush guard, and then the breadcrumb wrote anyway. `store::acp_event`'s `EntriesRemoved` and `EntryUpdated` arms also reach `persist_main_stream` without `push_entry`; both are unreachable on a flagged session today, but that is a property of the code, not of the rule.
- **`persist_main_stream` therefore carries a TRIPWIRE, not a guard** — `log::error!` + `debug_assert!` when it is reached with the flag set. On a correct build it is unreachable, so a line that fires is by construction a bug report, and a third door announces itself instead of deleting rows quietly. A mutation removing it necessarily survives; that is acceptable, because the row-count and epoch assertions hold the guarantee either way and the tripwire only accelerates diagnosis.
- **Three things are load-bearing and non-obvious in the retry.** It must **re-anchor `live_base`** to `entries.len()` — it was pinned at 0 when the restore had nothing to put there, and without the re-anchor `acp_event`'s `global_entry_index` arithmetic silently *drops* the user's message rather than misplacing it. It must **refuse rather than replace** when `entries` is non-empty. And the refusal must go through **`mutate_state`**: `notifier::decide_notification` has exactly one production caller, inside it, so a direct `s.state = Errored(...)` sets a state nobody is notified about — the sibling `Err` arm gets away with the direct assignment only because `run_turn` had already emitted `AcpThreadEvent::Error`. Cost of that: `mutate_state` re-broadcasts only on a *discriminant* change, so a second refusal on an already-`Errored` tab does not update the status-row text.
- **A `/clear` clears the flag** rather than merely being exempt from the guard: after `persist_context_wipe` the empty transcript is the user's own instruction, so the flag has nothing left to protect, and leaving it set made a wiped-then-reused session unflushable on close for the rest of its life.
- **Surfacing a refusal is two decisions, deliberately scoped differently — do not re-bundle them.** The **toast is broad** (every send failure raises one), because it is the only surface a cold tab has: `status_row` renders `is_cold` ("Sleeping") ahead of `Errored`. The **draft restore is narrow**, keyed on whether the send *consumed* the message rather than on "was this a refusal" — `AcpThread::send_inner` pushes the `UserMessage` entry before `connection.prompt`, so an ordinary turn failure already has the message in the transcript and restoring it would duplicate it and make Enter a re-send. Stating the rule as consumed-versus-not-consumed is what makes the next failure arm land on the right side by default. **Both halves need their own test:** a mutation that narrowed the toast to match the restore survived the entire suite until one was added — for any pair described as "deliberately broad / deliberately narrow", the first mutation to try is the one that flips the *other* half.
- **The refusal's advice must match the failure.** A blob that will not *decode* fails identically forever, so its refusal must not say "close and reopen the tab". Without that distinction a corrupt tab silently eats every message typed into it for good.
- **The refusal covers both regimes, and getting there required disproving the convergence claim this entry used to make.** Every read RPC prefers the in-memory store, so for a while a corrupt session was refused when the Solution was closed and served as an ordinary *empty* transcript once it was open — non-destructive, but a client could not tell "cleared" from "unreadable". The obvious fix was wrong: `build_get_session_result` is a convergence point for `get_session` **only**. `get_session_changes` and `get_session_entry` have their own build helpers and `read_session_history` shares none, so a guard placed at the convergence point would have closed **one tool of four**. The guard is applied in the in-memory branch of all four instead. The machine-readable prefix is identical across regimes; the prose differs on purpose, because a cold refusal is always a permanent decode failure while a hot one may be a transient row, epoch or blob read — the flag cannot name its cause.
- **The desktop is still silent, and the divergence now points the wrong way.** `transcript_unavailable` has no reader in `session_view`, so such a tab renders as an ordinary empty conversation; the only signal is on *send*. Before the hot-path refusal, desktop and MCP were consistently silent. Now the phone user — who can do nothing about it — gets the error, while the desktop user, who is the only one who can act, is told nothing. A passive indicator is a product decision, but that asymmetry is the argument for taking it: note that `status_row` renders `is_cold` ("Sleeping") ahead of `Errored`, so naively reusing `Errored` would be invisible on exactly the tab that needs it most.

### 106. Per-solution MCP sockets physically scope subagents to one Solution

Why: with decision 14, every spawned subagent bridged to the single editor-global socket and saw all ~155 tools, passing `solution_id` by convention — nothing stopped a subagent for Solution A from operating on Solution B by passing B's id. The fork's unit of work is the Solution, so scoping should be physical, not advisory.

How it works: at `start_server`, the global catalog is split (`editor_mcp::lifecycle`): the global socket (`<state>/mcp.sock`) keeps only `GLOBAL_TOOLS` (editor.\*, solution lifecycle/discovery, catalog.\*, windows.list) plus `SHARED_TOOLS` (`solutions.{get,add_member,add_empty_member,remove_member}` — needed both globally, e.g. to add a member before a Solution can open, and per-solution); everything else becomes a template. On `solutions.open` a per-solution `McpServer` binds a socket at `<state>/solutions/<id>/mcp.sock`, gets the scoped template installed (shared `Rc` handlers — no re-running registrations) and is `set_bound_solution(id)`; on close it's dropped and the dir removed (wired from `solutions::event_sources` mark_open/mark_closed). A solution-bound `McpServer` force-injects its id into any tool whose input schema declares a `solution_id` property (`wants_solution_id`, computed in `context_server::listener::add_tool`), overwriting whatever the caller passed — so a scoped subagent cannot reach another Solution. The `--nc` bridge resolves the project's Solution via `editor_mcp::solution_socket_for_path` and points the subagent at that socket (falling back to the global socket when the project isn't under an open Solution). `solutions.list` reports each open Solution's `mcp_socket` path so the operator can connect to a scoped socket directly.

How to apply: the global/shared/scoped split is the `GLOBAL_TOOLS` / `SHARED_TOOLS` allow-lists in `crates/editor_mcp/src/lifecycle.rs` — a NEW tool defaults to solution-scoped (fail-safe: worst case it's missing from the global socket, a visible gap, not an unscoped leak). To make a new tool global, add it to `GLOBAL_TOOLS`; to make it both, add to both lists. Injection only fires for tools whose input has a `solution_id` field. **Windows are orthogonal to Solutions** — one window hosts many Solutions, so a window has no single owning Solution and window ops cannot be solution-scoped; the entire `windows.*` surface is therefore in `GLOBAL_TOOLS` (cross-solution / operator-level), kept off per-solution sockets. The split/export/bound machinery is `McpServer::{split_off_tools,export_tools,install_tools,set_bound_solution}` in `crates/context_server/src/listener.rs` (handlers became `Rc` so one implementation serves several sockets).

### 13. Catalog membership IS the trust signal — Restricted Mode badge hidden

Why: upstream Zed's worktree-trust UX prompts before starting a language server in any unfamiliar directory and surfaces a "Restricted Mode" badge in the title bar. The fork's mental model is different: a project is in a Solution because the user explicitly added its remote URL to the catalog AND chose to clone it. That decision IS the trust signal — re-prompting at LSP-start time and parking a yellow badge in the title bar is noise, not safety.

How to apply: `crates/solutions/src/auto_trust.rs` observes new workspaces and trusts every `solution.root` whose path covers any worktree of the project (uses `PathTrust::AbsPath`, so all current and future member subdirs inherit trust via the path-hierarchy in `crates/project/src/trusted_worktrees.rs`). The title-bar render call in `crates/title_bar/src/title_bar.rs` is commented out; the function itself is kept under `#[allow(dead_code)]` for upstream-merge friendliness. Trust still works as upstream for ad-hoc opens (File → Open Folder of a non-Solution path) — they go through the original prompt path.

### 16. Solution switch is in-place — same `Workspace`, swap worktrees, replay tabs

Why: switching the active Solution used to allocate a fresh `Workspace`
via `OpenMode::Add` + `MultiWorkspace::activate`, which retained the
old workspace but visibly tore down all panels in the active one and
re-created them from defaults — losing dock widths, scroll positions
in `ProjectPanel`/`OutlinePanel`, expanded items, panel-specific UI
state — every single switch. The retained-workspace mechanism kept
the *previous* Solution's state alive in memory but didn't help the
in-flight switch UX, which is what the user actually feels several
times an hour. Recreate-on-switch was a holdover from upstream's
`git_ui::worktree_service` flow; for Solutions the cost was paid an
order of magnitude more often.

How to apply: use `solutions_ui::switch_active_solution_in_place`
(orchestrator) when the user wants to swap solution scope without
window churn. The orchestrator (1) snapshots the current Solution's
open editor tabs into `SolutionStore::tab_snapshots`, (2) bumps
`touch_last_opened` (which fires `SolutionStoreEvent::ActiveSolutionChanged(target)`),
(3) reconciles worktrees inside the existing `Project` via
`Workspace::swap_worktrees_to`, and (4) replays the target Solution's
saved tab snapshot. Upstream panels react to `WorktreeAdded`/`Removed`
automatically; fork panels (`SolutionTabStrip`,
`SolutionSessionsNavigator`) listen to `ActiveSolutionChanged` —
*don't* assume your panel will be re-`new`'d on switch. The
`OpenIntent::SameWindow` path in `solutions_ui::open::open_solution`
goes through this orchestrator; `OpenIntent::NewWindow` and
already-open-in-other-window focus paths still use the
`MultiWorkspace::activate` machinery (they're inherently per-window).

Tab restoration is best-effort: snapshot-save failures don't abort the
switch (the user wants to *get to* the new Solution; one lost tab
list is recoverable). Snapshots are runtime-only — losing them across
an editor restart is acceptable, persistence would mean keeping the
map in sync with potentially-stale paths.

Dock (panel) layout is **per-Solution** — each Solution keeps its own
dock layout, and switching Solutions does NOT carry panel state across.
Each Solution is its own retained `Workspace` with its own three `Dock`
entities (the tab-strip click / keyboard cycle activate a different
workspace, not the in-place worktree-swap), and each `Workspace` already
holds + persists its own live dock state (its workspace-DB row + the KVP
panel-size store). `MultiWorkspace::activate` therefore does **nothing**
to the docks — the arriving Solution's workspace simply renders whatever
layout the user last left it with. `solutions_ui::switch` (the MCP-only
`solutions.switch` in-place path) likewise leaves docks untouched.

Why per-Solution (2026-07-10): a Solution's *member projects differ*, and
the bottom dock's git-graph panel, the left dock's project tree, and the
git panel are all **project-bound**. Unifying their open/active/size
state across Solutions leaked one Solution's panel layout onto another
whose projects it doesn't describe. The general rule the user set:
**anything bound to a project must not be shared across Solutions.**

This **reverts** the earlier "single shared layout" decision
(`fef6e1e34c`, 2026-06-29), which captured the leaving workspace's layout
via `Workspace::capture_dock_layout` and replayed it onto the arriving
one via `Workspace::apply_dock_layout` inside `activate`. Both methods and
the `workspace::dock::DockLayout` / `DockSideLayout` types were **removed**
— nothing else used them. (Note this is the *second* reversal on this
axis: `b30df54c67` (2026-05-21) first made it per-Solution via a
`SolutionDockSnapshot`; `fef6e1e34c` unified it; this restores per-Solution
without reintroducing the snapshot machinery — the retained per-`Workspace`
`Dock` entities carry the state for free.)

Within a single Solution, per-*member* dock state (open/active only; sizes
stay shared) is a separate mechanism — see decision #28
(`solutions_ui::member_layout`), which is orthogonal to this and unaffected.

### 15. mold mandatory for x86_64-linux builds

Why: system `ld` is the wall-clock bottleneck of `release-fast` incremental rebuilds (multi-GB peak RAM, several seconds per re-link on Zed's link graph). mold is ~5-10× faster and uses a fraction of the RAM. The existing aarch64 entry pins `lld` out of *necessity* (libwebrtc.a fails to link otherwise); the x86_64 entry pins `mold` for *perf* but elevated to required because silent fallback to `ld` is a worse failure mode than a one-line apt install. Mirrors the same "you must install a fast linker before first build" contract.

How to apply: contributors install `mold` (`apt install mold` on Debian/Ubuntu, `brew install mold` on macOS-with-Linux-cross, prebuilt binaries on the [mold releases page](https://github.com/rui314/mold/releases) elsewhere). The pinned block lives in `.cargo/config.toml` — never delete it during an upstream merge (Zed upstream may add their own `[target.x86_64-unknown-linux-gnu]` entry for some unrelated rustflag; merge by combining flags, don't drop ours). To verify mold is active on a build: `cargo build --profile release-fast -v 2>&1 | grep -m1 fuse-ld` should show `-fuse-ld=mold`.

### 17. Per-panel project selectors are independent — no global "active project" — **SUPERSEDED by #27**

> **Superseded (2026-06-23) by decision #27.** This fork now HAS a single solution-wide active project (`SolutionStore::active_member`); the per-panel `ActiveProjectSelector` dropdowns and the `panel_member_selections` table were removed. The text below is kept for historical context — do NOT follow its "no global field" guidance anymore.

The Phase 3 `ActiveProjectSelector` element lives in two places (`project_panel`, `git_panel`) and each instance keeps its own selection in the `panel_member_selections` SQL table keyed by `(solution_id, panel_kind)`. There is no global "the user's active project" concept on `SolutionStore`; cross-panel sync is intentionally absent.

Why: a global active-project field cascades into search-scope, terminal cwd, new-file location, find-in-files default, and several other behaviours — an unbounded set of consequences that have to be designed before the first feature ships. Per-panel scoping keeps Phase 3's footprint tight: each panel filters its own content (project_panel filters worktrees; git_panel drives `active_repository`), and `set_panel_member_selection` emits `PanelMemberSelectionChanged` so multi-window same-solution stays in sync without a global field.

How to apply: if a future feature needs cross-panel "current project" awareness, do **not** add a global field to `SolutionStore`. Pick one of: (a) "follow the focused panel's selection" heuristic, (b) "last-touched panel" heuristic, (c) per-feature opt-in argument that asks the relevant panel for its selection. The two cycle actions `SwitchToNext/PrevProjectInPanel { panel_kind }` already work this way — they're scoped to a single panel, not "the active project."

Initial-selection rule, also intentional: on first load of a (solution, panel) pair, default to the first member in `solution_members.position` order **and persist the default immediately** via `set_panel_member_selection`. Subsequent loads (and other windows on the same solution) read the persisted value. This makes "what does the user see in this panel?" a deterministic single-row lookup, not a derive-from-N-signals computation.

### 18. `SolutionSession::set_acp_thread` is the only legal way to swap the thread

`SolutionSession.acp_thread` swaps on compact rotation, `/clear`, cold→live promotion, and `restart_agent` reuse. Each callsite goes through `SolutionSession::set_acp_thread(thread, cx)` (`crates/solution_agent/src/model.rs`), which atomically reassigns the field, emits `SolutionSessionEvent::ThreadReplaced`, and calls `cx.notify()`. `SolutionSessionView` listens for `ThreadReplaced` via `cx.subscribe(&session, ...)` (field `_session_event_subscription` in `crates/solution_agent/src/session_view.rs`) and re-attaches `_thread_subscription` to the new `AcpThread`.

Why: GPUI auto-notify gets dropped silently when a nested `session_entity.update(cx, |s, _| { s.acp_thread = ... })` runs inside an outer `this.update(cx, |store, cx| ...)` on the store — the deduplication in `App::push_effect` (`crates/gpui/src/app.rs`) collapses pending notifications across the outer flush. Without an explicit notify on the *session* entity, `cx.observe(&session)` callbacks (notably `SolutionSessionView::sync_thread_subscription`) never fire, leaving `_thread_subscription` bound to the dropped thread. Result: the conversation list stops growing while the agent keeps streaming events into the new thread — visible to the user as "messages stopped appearing even though the agent is clearly working." Push-channel via `ThreadReplaced` makes the contract explicit and synchronous: every swap fires exactly one event, every subscriber re-attaches, no auto-notify dependence.

How to apply: never assign to `s.acp_thread` directly outside `set_acp_thread` — it's enforced at compile time. The field is **private**; reads go through `s.acp_thread()` (returns `Option<&Entity<AcpThread>>`), writes only through the setter. Direct struct-literal construction is also blocked (private field), so all `SolutionSession` instances are built via `SolutionSession::new_idle(id, solution_id, agent_id, acp_session_id)` followed by `s.<other_pub_field> = ...` for any defaults that need overriding, then `s.set_acp_thread(thread, cx)` as the *last* mutation if a live thread is being attached so observers wake up to a fully populated session struct. Tests for any new thread-swap path must include the same `cx.subscribe(&session, ThreadReplaced)` + `cx.observe(&session)` probe pair as `model::tests::set_acp_thread_emits_thread_replaced_and_notifies`.

### 19. SQLite `Domain` for cx-less state stores; `OnceLock` cache + `gpui::block_on` for sync API

Why: state stores called from background tasks (`OpRunner` from S-BAK; pre-commit check pipeline; favorites toggles fired from a list-row click; shelf saves) don't have `cx: &App` available. The fork's prevailing persistence convention is SQLite via `db::sqlez::Domain` (`GitGraphsDb`, `SolutionsDb`, `WorkspaceDb`, `solution_agent::db`). `query!` macros generate async functions; `static_connection!` provides the `Domain::open_test_db` helper for tests.

How to apply: per state store, declare a `mod persistence` (or `<name>_db.rs`) inside the owning crate. Define a `Domain` impl + `MIGRATIONS` array. Cache the connection in a module-local `OnceLock<Domain>` populated by `<module>::init(cx)` at app startup right after `cx.set_global(app_db)`. Public sync methods use `gpui::block_on(domain.async_method())` for writes; the connection's executor pool guarantees no deadlock against the calling thread.

Tests use a per-thread `Mutex<HashMap<ThreadId, Domain>>` registry (each parallel test gets its own UUID-named in-memory DB) to sidestep `SQLITE_LOCKED_SHAREDCACHE` under `cargo test`'s parallel runner. The pattern is duplicated across four modules today (`undo_registry`, `branch_picker::favorites`, `shelf`, `pre_commit`); consolidating into a `db::test_registry<T>` helper is a low-priority follow-up — the duplication is `#[cfg(test|test-support)]`-only.

Stores that should NOT use this pattern: caches living in `paths::temp_dir()` (`commit_explanations/`, `ai_cherry_pick_cache/`) — direct file IO is fine for write-once / read-once / age-out shapes. Per-worktree filesystem markers (`.sawe-readonly.json` from S-SAR) similarly stay as files because their detection runs at worktree-load time before any DB connection is available.

### 20. Cross-crate dynamic action dispatch via `cx.build_action(name, params)`

Why: `git_ui` is the central git-UI crate; `git_graph` and `solution_git` depend on it (downward). When `git_ui::commit_context_menu` needs to fire an action owned by `git_graph` (`ShowAffectedPathsInLog`) or `solution_git` (`CrossCherryPick`), a direct `Box::new(action)` would invert the dep graph. Each downward crate `pub` declares the action and registers a workspace handler at `init`; `git_ui` discovers it dynamically via `cx.build_action("crate::ActionName", Some(params_json))` which silently no-ops when the action isn't registered.

How to apply: when an upward crate needs to fire a downward-crate action, use the dynamic dispatch path. The action must be JSON-deserializable and take its full payload through the `params` argument. Document the upward call site with the action's owner crate so future contributors can find the registration point. Don't add the upward dep just to call the action statically — the silent no-op behavior is the right semantic for "feature available only when its owning crate is initialized."

Don't use this pattern for tightly-coupled action sequences where a missing handler is a bug. Examples in tree: `commit_context_menu::build_commit_context_menu` for ShowAffectedPathsInLog and CrossCherryPick; the menu entries are gated on whether the relevant crate state is available (e.g. CrossCherryPick entry is hidden when no `member_id` is set on the CommitContext).

### 21. Run Configurations are a UX + model layer on top of `task` / `dap`, not a new execution engine

**Why:** the fork already has a full static-task engine (`task` + `.sawe/tasks.json` + language runnables) and a DAP layer (`dap` + `.sawe/debug.json`); rebuilding execution would duplicate both. So `run_config` is purely a *model + persistence + provider registry*, and a `RunConfiguration` is translated into a `task::SpawnInTerminal` (Run) or `task::DebugScenario` (Debug) at launch time, which `run_config_ui::RunController` then hands to `Workspace::spawn_in_terminal` / `Workspace::start_debug_session`. Run output lands in the existing terminal panel, debug output in the existing debugger panel — there's no separate "Run console" panel. The picker + Run/Debug/Stop widget lives in the title bar, right-aligned (IDEA-style) — `run_config_ui::install` builds the view and parks it in `Workspace::run_config_strip`; `title_bar::TitleBar::render` reads that slot and renders it in the right-side controls cluster (no separate full-width strip row).

**How to apply:** new configuration *types* are `RunConfigProvider` impls registered via `run_config::register_provider(cx, …)` in some crate's `init` (mirrors `editor_mcp::register_tool`); a provider's `resolve()` returns a `RunRequest` (`Terminal(SpawnInTerminal)` or `Debug(DebugScenario)`) — never spawn processes directly from a provider. **Config identity:** every persisted config carries a stable, name-independent `RunConfigId` (a random uuid) materialized in `run-configurations.json` as the first key, `"id"`. New configs (modal `+` / duplicate / promote-ephemeral, MCP `run_config.create`) get a fresh `RunConfigId::new_random()`; renaming a config keeps its id, and two configs with the same display name are fine (distinct ids — no `-2` slug workaround anymore). Legacy entries without an `"id"` key get a deterministic-from-name id on load (`file_format::legacy_id` = `"<type>:<slugified-name>"`), which is then written into the file on the next save. Ephemeral discovered configs use `RunConfigId::discovered(type, key)` = `"<type>:discovered:<task-label>"`, regenerated each load and never persisted. `RunConfigId::from_raw(s)` wraps an id string verbatim (parsing `"id"` keys, accepting id strings over the MCP surface). There is no `RunConfigId::new(type, key)` anymore. The crate split mirrors `solutions` / `solutions_ui`: `run_config` is headless (deps `task` / `project` / `fs` / `paths` / `editor_mcp`, no `workspace` / `editor`); everything needing `Workspace` / `Window` lives in `run_config_ui`. MCP tools (`run_config.*`) reach the per-`Workspace` `RunController` (in `run_config_ui`) through the `RunConfigStore` command-sink indirection (`set_command_sink` / `dispatch_command`) to avoid a `run_config → run_config_ui` dependency cycle; the running-config set is similarly published *up* into `RunConfigStore::set_running`. Stop for a terminal task spawns it through the Solution band's console panel (`ConsolePanel::spawn_task` → killable `WeakEntity<Terminal>`, resolved via `console_panel::console_panel_for_workspace`) and calls `Terminal::kill_active_task()`; if there's no console panel (headless test harness) the `RunController` falls back to `Workspace::spawn_in_terminal` and Stop just drops the tracking entry. Stop pressed *during the launch window* (before `spawn_task` hands back the terminal handle) is honoured too: each `run()` gets a monotonic launch token, Stop records it in `terminal_launches_pending_kill` and keeps the completion poller alive (moved to `_detached_tasks` rather than dropped — dropping it would cancel the only thing that'll ever see the handle), and the poller — once the handle resolves — kills the terminal and exits instead of tracking it. The token is keyed per launch (not per config), so a stale poller from a since-stopped-and-rerun launch can't kill the newer launch's terminal. Since 2026-08-27 the *spawn itself* also happens inside that poller rather than inline (`ConsolePanel::spawn_task` reads its own `WeakEntity<Workspace>`, so it cannot run while `run`'s caller holds the workspace borrow — see FORK.md #93), which adds an earlier sub-case: a Stop landing before the poller's first tick drains the token up front and returns without spawning anything, so a run cancelled that fast never starts a process at all. Debug runs are tracked per `dap::client::SessionId`: `Workspace::start_debug_session` hands back nothing, so right before each launch the controller snapshots the set of existing `SessionId`s and pushes `(RunConfigId, snapshot)` onto `pending_debug_launches`; on `DapStoreEvent::DebugClientStarted(id)` it hands `id` to the first pending entry whose snapshot doesn't already contain it — that launch must be the one that created it (see the `claim_started_session` free fn, unit-tested). Matching is by session novelty, never by label, so two configs with the same display name are no longer ambiguous. Stop calls `DapStore::shutdown_session(id)`, and the entry clears on `DebugClientShutdown` for that id. A debug launch that never starts a session (adapter died during launch → no `DebugClientShutdown` will ever come either) is cleared by a per-launch `DEBUG_LAUNCH_TIMEOUT` (20s — generous, since adapters can be slow to come up) timer — see the `debug_launch_timed_out` free fn, unit-tested; a run that did get a session id, or was already stopped, makes the timer a no-op. When this controller's workspace window closes, an `on_release` handler calls `RunConfigStore::clear_running_source` to drop its slice of the running set (entity ids can be reused after release, so this also closes the collision window). Known limitations: if the user manually starts an unrelated debug session in the exact tick a config's debug launch is in flight, that session can be mis-attributed to the launch (a much narrower race than the old name-collision bug, and unavoidable without an id handed back from `start_debug_session`); `run` / `stop` / `select` MCP tools are no-ops when no workspace window with a `RunController` is open.

### 23. Per-turn git checkpoint capture disabled in `AcpThread::send`

Why: upstream Zed writes a `commit-tree` to the project's `.git/` on every user message so the user can later one-click revert the agent's edits via the AgentPanel UI. This fork hides `agent_ui::AgentPanel` (decision 22 + the "what's disabled" table in CLAUDE.md) and `solution_agent::session_view` doesn't expose a restore-checkpoint affordance, so every `git_store.checkpoint(cx)` call wastes CPU/IO, accumulates dangling commit objects in member repos, and noisily `log::error!`'s ENOENT whenever any project repository is in a state `git add --all` can't traverse cleanly (e.g. a stale `repositories` entry pointing at a removed worktree).

How to apply: `AcpThread::send` no longer captures `git_store.checkpoint(cx)`. `UserMessage.checkpoint` always stays `None`, which makes `update_last_checkpoint` early-return at its existing `let Some(checkpoint) = … else { return Task::ready(Ok(())); }` and makes `restore_checkpoint(id)` a no-op for any user message. The `restore_checkpoint` / `update_last_checkpoint` methods themselves stay in code (per "disable, don't delete") so re-enabling is a one-line change — flip the body of `AcpThread::send` back and they work again. `test_checkpoints` is `#[ignore]`'d for the same reason.

### 22. ConsolePanel merges TerminalPanel + SolutionSessionsNavigator into one bottom-dock

Why: two separate bottom-dock panels (terminal + AI chat) competed for the same screen real estate; switching between them required two keybinds and visual context-switches. IDEA-style "Tool Window: Console" puts terminal and AI in one tool window with heterogeneous tabs, distinguished by icon, and the same `+` popover spawns either kind. Side effect: deleting `SolutionSessionsNavigator` forced `render_status_row` (model selector, token meter, "Thinking…" timer, compact, history) to lift out into a free function, eliminating Navigator's HashMaps in favour of per-view scalar caches.

How to apply (**as amended by decisions 91 and 93 — the chat half of this entry is history, the terminal half is live**): `ConsolePanel::tabs: Vec<ConsoleTab>` now has the single `Terminal { view, origin_cwd }` variant and one stateless `TerminalProvider` spawn helper — `ChatProvider`, the `Chat` variant and `ConsolePanelSettings` were all deleted when chat moved to the Solution band. The `+` popover's *New Terminal* entry calls `add_terminal_tab(active_member_path)` directly (so the tab is scoped to the active member); its *New AI Chat* and *Spawn Task…* entries dispatch actions, while *Reopen Closed Chat…* calls `panel.open_reopen_session_modal` directly. Right-click on a terminal tab opens a `deferred(anchored(...))` overlay menu — Close / Rename Tab / Reveal CWD in Project Panel. Persistence: a `console_panel_state(workspace_id, tab_index, kind, item_id, cwd, active)` table in `workspace.db`; save on every mutation; restore on `ConsolePanel::load`. `kind = "chat"` rows are no longer written, are skipped on restore, and are purged once by a migration; the `kind` CHECK constraint still admits the value because narrowing it would mean rebuilding the table. The `terminal.dock` setting stays removed.

**Important call-site rule** (learned via double-lease panic): inside a `workspace.register_action(|workspace: &mut Workspace, …, cx|)` handler, do NOT call `self.workspace.upgrade()?.read(cx)` — the Workspace entity is already mutably borrowed. Read whatever Workspace-derived state you need (e.g. `active_solution_id_for_workspace(workspace, cx)` — public helper) before delegating to `panel.update(cx, |panel, cx| panel.add_*_tab(...))`.

**Known gaps after the refactor** (TODO B10+): History popover (clock icon), history-card empty-state, `subagent_strip::switch_to_session` "click bubble → open tab" router, and `solution_agent::actions::FocusNavigator` all became no-ops with `TODO(B10)` markers. They are now the *band's* to take over, not `ConsolePanel`'s — the panel no longer knows what a chat is.

### 24. IDEA branches popup is an anchored `PopoverMenu<BranchesPopup>` from a title-bar widget, not a centered modal

Why: the S-BRP branches popup (`git_ui::branch_picker::BranchesPopup`) shipped keyboard-only as a centered `toggle_modal`. IDEA's branch UI is a toolbar **widget** (`⎇ <branch> ⌄`) that drops an anchored popover. `BranchesPopup` already implements `ModalView + EventEmitter<DismissEvent> + Focusable` ⇒ it is a `ManagedView`, so it can be hosted directly as the `M` in `ui::PopoverMenu<M>` — no rewrite needed, just a different shell.

How to apply:
- **(Updated 2026-06-23, decision #27)** The branch widget moved OUT of `TitleBar` into the new `ProjectToolbar` row entity (`crates/title_bar/src/project_toolbar.rs`). `ProjectToolbar` holds `branch_popover_handle: PopoverMenuHandle<BranchesPopup>` and renders `render_branch_widget` (right region of the project-toolbar row, left of the run-config widget). The widget reads the **active member's** repository (decision #27; falls back to `project.active_repository`); **detached HEAD shows `CommitDetails::short_sha()`** (don't hide the widget when `branch` is `None` but a repo exists). The `.menu()` closure builds `BranchesPopup::new(...)` against the same resolved repo.
- Keyboard `git::BranchesPopupOpen` is registered in **`title_bar`** (which legally depends on `git_ui`; the reverse would be a dependency cycle), downcasts `workspace.project_toolbar_item()` to `ProjectToolbar`, and toggles the handle. `git_ui` no longer registers the action.
- **Double-lease trap:** the action handler holds a `&mut Workspace` lease; toggling the popover synchronously runs the `.menu()` closure, which `read`s the same `Workspace` → double-lease panic. Defer with **`window.defer(cx, …)`** (callback gets `&mut Window, &mut App`, no `Workspace` lease) — `cx.defer_in(window, …)` does NOT work, it re-leases `Workspace`.
- **Popup height hugs its content (updated 2026-07-02, `df60866dd0`).** The root has NO fixed height — it sizes to content so a short / filtered result set doesn't reserve a tall empty slab (IDEA-style). What bounds the popup when a repo has many rows is the inner scroll list's own `.max_h(rems(26.))` + `overflow_y_scroll`; a 192-tag repo scrolls inside that cap. (This replaced an earlier fixed `.h(rems(36.))` that existed only because a `flex_1` scroll list collapsed to zero height in a content-sized container. Putting `max_h` on the list itself needs no height basis, so the fixed height — and the empty slab — are gone.)
- Layout: the tab bar was replaced by a single list — search → action header (`render_action_header`) → collapsible section nodes in order `recent / favorites / local / remote / tags / backups` (`SECTION_ORDER`, `collapsed_sections: HashSet<&'static str>`, `PopupRow::Section`).
- S-DST (rebase/merge) and S-PSH (force push) context-menu entries now wire to existing infra (`handlers::{rebase,merge}::run` under `OpRunner` w/ auto backup-ref + `git_conflict_ui::OpenConflictResolver` routing; `git::ForcePush` → push dialog in force mode).

### 25. Agent Teams teammate tool calls are AUTO-APPROVED, not gated

Why: the fork spawns the main `claude` agent with `--permission-mode bypassPermissions` (+ `--allow-dangerously-skip-permissions`, `command.rs`), so it never sends a `can_use_tool` control request. Enabling Agent Teams (`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`, decision in `command.rs`) means the agent delegates to teammates — and `claude` deliberately does NOT let an auto-spawned sub-agent inherit `bypassPermissions` (an autonomous sub-agent with blanket bypass is a safety hole). So every teammate tool call would pop an Allow/Reject prompt, the session would sit in `AwaitingInput` until each is answered, and answering from a paired mobile could leave the turn looking hung. Since the user already opted the whole workspace into bypass for the main agent, we extend the same trust to its teammates.

How to apply:
- `claude_native::connection::spawn_tool_authorization` answers `can_use_tool` with `behavior:"allow"` **synchronously** and returns `None` — it no longer drives `AcpThread::request_tool_call_authorization`. The tool call stays visible in the teammate's transcript (claude streams the `tool_use` block before the request), so nothing is hidden; only the per-call gate is dropped.
- This is a **deliberate, security-reviewed choice** (an automated review flags it HIGH "permission bypass" — that finding is acknowledged and accepted: it matches the existing main-agent bypass posture, it is NOT a new exposure). Do not "fix" it back to a gated prompt without the maintainer's say-so.
- The gating machinery (`request_tool_call_authorization`, the Allow/Reject UI, the `ToolAuthorizationRequested/Received` store events) stays in tree. To re-enable per-call prompts, re-point `spawn_tool_authorization` at it; for defense-in-depth, gate auto-approval behind a `tool_name`/`input` classifier (allow read-only, prompt for `Bash`/`Write`/network) — explicitly declined for now since it would re-introduce prompts for exactly the `Bash` case this removes.

### 26. Mid-turn follow-ups route to the active tab (`QueueTarget`)

Why: with Agent Teams, a user can watch a specific teammate's tab and type a follow-up meant for that teammate, not the parent. `SolutionSession::pending_messages` holds `PendingBundle { target: QueueTarget, blocks }` (in-memory only — not persisted, so no serde compat). `QueueTarget::Subagent(agent_id)` equals the teammate's `BackgroundAgentId` == its `claude` hook `agent_id`.

**Migration status (SUPERSEDED for teammate targeting, 2026-07-07):** the per-source-streams fold made all tabs view-only except `Main`. `SubagentView` no longer has a `Background` variant, `queue_target()` returns `QueueTarget::Main` for EVERY variant (`Main`/`Task`/`Shell`), and `compose_disabled_for` gates only on `Shell` — so a follow-up is never routed to a specific teammate anymore (an async agent's tab is view-only, see #40 / phase 6d-tail-1). The `QueueTarget::Subagent`/`take_pending_for_delivery`/`is_messageable` machinery is KEPT (still constructed for the supervisor's `has_live_background_work` gate + a possible future "message a live teammate" feature) but is no longer reachable from a tab's compose row.

How to apply (historical shape, pre-6d-tail-1):
- Enqueue stamped the target from `SubagentView::queue_target()` (only `Background` → `Subagent`; `Main`/`Task`/`Shell` → `Main`). `take_pending_for_delivery` drains only bundles whose `target.matches_hook(agent_id)` matches the firing hook (main hook = no `agent_id`); other-target bundles stay queued.
- A `Subagent` bundle whose teammate finishes without draining it is DROPPED at turn end (idle-flush) — never re-routed to the parent. The ghost + up-arrow recall are filtered to the selected tab's target (now always `Main`).

### 27. Solution-wide active project (`active_member`) + project tab strip — replaces per-panel selectors (reverses #17)

Why: per-panel `ActiveProjectSelector` dropdowns (decision #17) let the file tree and the git panel point at different members, there was no single "the project I'm working on," and the dropdowns cluttered every panel header. Product decision: one active project per solution, chosen from a horizontal project-tab strip that mirrors the solution-tab strip one level down. Switching a project tab switches every project-specific surface at once; open files / terminals / AI dialogs stay agnostic.

**Split as of decision #89:** the "AI dialogs stay agnostic" half of that claim was contradicted for a stretch (commits `2ffeb840e1` / `9c3e0e9d1e` gave chat sessions a `member_id` binding and filtered the visible tab strip by the active member) and is enforced again now — see decision #89. **Terminals remain member-scoped** and are NOT covered by that reversal: a terminal tab's cwd still ties it to the member it was opened under, and `console_panel`'s active-tab bookkeeping (`ConsolePanel::active_by_member`) still remembers a per-member *selection* — but for terminal tabs only, since phase 2a: `ConsoleTabKey` has the single `Terminal(EntityId)` variant and chat tabs have no entry in that map at all (decisions #89, #93). Do not go looking for per-member chat bookkeeping here, and do not restore it.

What changed:
- Model: `SolutionStore::panel_member_selections: HashMap<(SolutionId, PanelKind), CatalogId>` → `active_member: HashMap<SolutionId, CatalogId>`. SQLite `panel_member_selections` table → `active_member` (append-only migration carries old data: tree-priority then git-fallback via a quote-free `MAX(panel_kind)` subquery — `'tree' > 'git'`). `PanelKind` deleted. Event `PanelMemberSelectionChanged` → `ActiveMemberChanged { solution, catalog }` (MCP kind `solution_active_member_changed`). API: `active_member` / `set_active_member` / `ensure_active_member` (seeds first member) / `active_member_worktree` (resolver to the member's `WorktreeId` by `local_path` prefix).
- UI: new `solutions_ui::ProjectTabStrip` (+ `ProjectTab`), mirror of `SolutionTabStrip` — click-select, drag-reorder via `reorder_members`, `+` opens the extracted standalone `AddProjectPicker`, fixed-cap overflow popover. `ActiveProjectSelector` + `MemberPicker` deleted; `AddProjectPicker` promoted to its own module.
- Layout: a new full-width `ProjectToolbar` row (`crates/title_bar/src/project_toolbar.rs`) sits below the title bar, hosted by `Workspace` via `project_toolbar_item: Option<AnyView>` (mirror of `titlebar_item`, rendered between the title bar and the body). It holds `ProjectTabStrip` on the left (left-aligned to the solution-strip/hamburger inset) and the relocated branch widget + run-config strip on the right. The `title_bar` crate owns it because `workspace` cannot depend on `solutions_ui`/`git_ui`/`run_config_ui` (cycle).
- Followers: `project_panel` + `git_panel` read `active_member` and subscribe to `ActiveMemberChanged` (the worktree/repo `local_path` filters are unchanged — only the selection source moved). The branch widget shows the active member's repo; the run-config strip filters configs to the active member's worktree (`filter_configs_for_active_worktree`, Global always shown) and revalidates the controller's *selection* on switch (`RunController::revalidate_selection_against`) so Run/Debug never act on another project's config.

How to apply: anything that should be project-scoped reads `SolutionStore::active_member` (or `active_member_worktree`) and observes `ActiveMemberChanged`; do NOT reintroduce per-panel selection. Keep open files / terminals / AI session views agnostic. The two `SwitchToNext/PrevProjectInPanel { panel_kind }` cycle actions now drive the solution-wide active member (the `panel_kind` payload is retained for keymap stability but ignored).

**Follow-up (2026-06-23): git operations follow the active project.** Extending #27 so git surfaces are project-scoped, not solution-wide:
- **Solution-wide commit REMOVED.** The git panel's "Solution-wide" commit toggle, its `solution_wide_commit`/`solution_wide_add_trailer` state, the `SolutionPanelProvider` trait (`git_ui/providers/solution_panel.rs`), its orchestrator (`solution_git/src/commit.rs`), and the `solution.git.commit_all` MCP tool were all deleted. Commit is always single-repo (the active member's repo). Why: no safe scenario for solution-wide commit was found. The PUSH (`SolutionPushProvider`) and UPDATE (`SolutionUpdateProvider`) providers and the S-SOL-DSH dashboard (Fetch/Pull/Push All) are KEPT.
- **Branch popup (`git_ui/branch_picker/popup.rs`) actions act on the active project**: "Update Project" = fetch+pull the active member's repo (was: all members); "Push" = single-repo (was: solution-wide). A new **"Update All Projects"** row keeps the solution-wide fetch+pull (`SolutionUpdateProvider`), shown only when ≥2 members.
- **RepositorySelector (`git_ui/repository_selector.rs`) scoped to the active project** — its repo list is filtered to repos under the active member's `local_path` (fallback: all repos for non-solution projects).
- Note: the S-SOL-DSH status dashboard is still only reachable via the command palette (`OpenStatusDashboard`) — no keybinding/menu/button. Surfacing it is a possible future task.

### 28. Per-Solution-member workspace layout is session-only, keyed off the `ActiveMemberChanged` event

The member-level analog of #16's solution-level tab replay. `crates/solutions_ui/src/member_layout.rs` remembers each active member's open editor tabs + dock open/active state and swaps them on member switch; dock **sizes** stay shared across members.

Why: all members of a Solution share ONE `Workspace`/`Project` (#27), so the center pane and dock open/active state are global to the window — switching the active member left them untouched. Persisting per-member layout to disk would create a second owner of center-pane state fighting Zed's own workspace serializer (which writes the whole center pane per `workspace_id`), so it is deliberately **in-memory / session-only** (lost on restart; on restart Zed restores the last-active member's layout). Mirrors `console_panel`'s existing in-memory `active_by_member`.

How it works: a per-`Workspace` handler registered via `observe_new(Workspace)` in `solutions_ui::init`; state (`current` member key, per-member `layouts`, in-flight apply `Task`) lives in an `Rc<RefCell<MemberLayoutState>>` captured by a `SolutionStore` subscription owned by the Workspace. On `SolutionStoreEvent::ActiveMemberChanged { solution, catalog }`, `apply_active_member_change` snapshots the outgoing member (`Workspace::open_item_abs_paths` + active path + `capture_dock_state`) and applies the incoming member's snapshot (close all → reopen paths → activate; `set_dock_structure`). No worktree↔member resolution — the event carries the key.

How to apply: dock size stays shared for free — use ONLY `capture_dock_state`/`set_dock_structure` (open/active/zoom), NEVER the panel-size KVP (`set_panel_size_state`/`capture_dock_layout`). First visit to a member (no stored snapshot) leaves the current layout intact (no blank editor). v1 snapshots a flat open-file list (no center-pane splits). Durable persistence across restart is a deliberate non-goal; layer it on the same seam later without changing the swap logic.

**Anti-jank (the swap must not thrash the project tree).** Naively closing then reopening tabs one-by-one made the swap visibly ugly: each `close_item_by_id`/`open_abs_path` was individually `.await`ed (so the removals/opens spread across frames — tabs vanishing and appearing one at a time), and every one re-pointed the active entry, so the project panel's `auto_reveal_entries` scrolled the tree once PER tab ("the tree jumps on every tab"). Two fixes: (1) close the whole outgoing set with a single `Pane::close_items(.., &|_| true)` (one task, one repaint) instead of a per-item await loop; (2) a transient per-window flag `Workspace::{active_entry_reveal_suppressed,set_active_entry_reveal_suppressed}` that the project panel checks in its `Event::ActiveEntryChanged` handler — the swap sets it for the whole close/reopen batch and clears it just before re-activating the final file, so the tree reveals exactly ONCE, at the end. `apply_active_member_change` also clears the flag on entry (belt-and-suspenders against a cancelled apply task leaving it stuck). Opens stay sequential (tab ORDER matters); the reveal suppression, not batched opens, is what removes the jank.

### 29. Editor/supervisor system notes render as readable message bubbles (supersedes the bug-sweep Observer breadcrumb)

`SessionEntryKind::System { level, text_md }` (watchdog / usage-limit / supervisor "Observer" notes — editor-injected, **never part of the agent's context**) renders in `crates/solution_agent/src/conversation_render.rs` as a proper message bubble: a "plaque" badge (level icon + tag) over a markdown body, with the level color tinting a subtle background + a left border. `Info`/`Error`/`Observer` stay visually distinct via that color. The notes stay in the dialog like any other entry.

Why: the body previously went through a plain `Label::new(text_md)`, so a supervisor summary's markdown (bold, links, lists) was dumped raw and cramped — unreadable (the maintainer's screenshot showed a supervisor live-run summary as an illegible one-line grey breadcrumb). This replaces the partial wrapping-row mitigation from the 2026-06-30 bug-sweep (#2 in `docs/findings/2026-06-30-solution-agent-bug-sweep.md`, now marked SUPERSEDED).

How it works / how to apply: the body is rendered with `render_span((entry_idx, 0), text_md, markdown_for, style)` — the SAME markdown path user/assistant messages use. A `Markdown` entity already exists for a System entry because `entry_text_spans` emits its `text_md` as span 0 (so no new markdown plumbing is needed). Any new editor-injected note kind should reuse this arm rather than a bespoke `Label`. Distinguish note classes by `SystemEntryLevel` (icon + color), not by inventing a second renderer.

**Supervisor Observer nudge = ONE marked message, not a note + a bubble.** A supervisor "Continue"/nudge is delivered to the agent AS a user message (so the agent acts on it). It used to ALSO push a separate `Наблюдатель направил агента: {gist}` Observer breadcrumb note — two conversation elements for one action. That breadcrumb is removed (`store::send_supervisor_nudge`). Instead the nudge's content block is stamped with the `spk_observer_nudge` `_meta` marker (`acp_thread::meta_with_observer_nudge` / `SPK_OBSERVER_NUDGE_META_KEY`, mirroring `spk_client_send_id`; rides on `_meta`, invisible to the agent's text and round-trips into `SessionEntry::UserMessage.chunks` verbatim). `render_user_message` detects it via `acp_thread::is_observer_nudge_blocks(chunks)` and renders the message as an Observer comment (eye plaque + `Наблюдатель` tag, accent tint + left border) instead of the plain blue user bubble. Net: the single marked message carries both the full instruction and the observer attribution. How to apply: any editor-injected "acts-as-a-user-message-but-isn't-the-human" send should use the same `_meta`-marker + renderer-detect pattern rather than a companion breadcrumb note.

**The agent-INVISIBLE observer note and the agent-VISIBLE observer nudge must be visually distinguishable** (both are "the observer speaking," but one the agent acts on and one only the operator ever sees — the maintainer reported them looking identical). The two plaques now diverge: the agent-invisible `SystemEntryLevel::Observer` note uses a **crossed-eye** icon (`IconName::EyeOff`), the tag **`Наблюдатель · только вам`**, and a **dashed** left border (`border_dashed`, "not part of the agent's conversation stream"); the agent-visible nudge keeps the plain `Eye`, tag **`Наблюдатель · агенту`**, and a solid border. How to apply: audience (agent-visible vs operator-only) is a first-class distinction in the render — don't collapse the two observer bubbles to the same chrome just because both are accent-tinted eye plaques. The debug screenshot gate for this lives in `seed_cold_session` (`mcp.rs`): `role:"observer"`/`"system"` seeds the invisible note, and the new `role:"nudge"` seeds a `UserMessage` carrying `meta_with_observer_nudge()` so both plaques can be shot side by side without a live judge cycle.

### 30. The git-graph panel scopes to the active Solution member — and its initial resolve must be deferred out of panel construction

`crates/git_graph/src/git_graph_panel.rs` (`GitGraphPanel`) tracks the **active Solution member's** repository, not the raw `Project::active_repository`. In a multi-member Solution all members share one `Project`, so `active_repository` follows whichever repo the last-focused editor's file belongs to — not the member selected in the tab strip. The panel resolves the repo the same way the title bar / git panel / branch picker do (`active_member_repository`, falling back to `active_repository` outside a Solution) and subscribes to `SolutionStoreEvent::ActiveMemberChanged` so a member-tab switch re-points the graph. (Bug it fixed: with `ecos-data` selected, the graph showed `ecos-unilever`'s develop2/hotfix history because a unilever file was open.)

**Crash follow-up (commit `ba24832848` fixes `75cef0a026`):** `resolve_active_repo_id` reads the `Workspace` entity (`self.workspace.upgrade()?.read(cx)`) to find the active member's repo. But `GitGraphPanel::new` runs INSIDE the `workspace.update_in` that constructs the panel, so the `Workspace` is already mutably leased — reading it there hits GPUI's `double_lease_panic` and **crashes on solution open / panel load** (it compiles; the panic is runtime-only). The pre-fix code read the `git_store` (a different entity), so it never tripped this. Fix: defer the initial `refresh_active_repo` via `cx.defer_in(window, …)` so it runs on the next effect cycle, after the construction lease releases. Subscription-callback refreshes already run outside any `Workspace` update, so they need no defer.

How to apply: never read the parent `Workspace` entity during a panel's `new()` when `new` is invoked from `workspace.update_in` — defer it. See memory `gpui-panel-new-workspace-double-lease`.

**Focus follow-up (commits `114e2b3e79`, `a90ade3341`, `5ac3b50703`, `63c3182eba`):** re-pointing the panel does not mutate the graph — it **drops the old `Entity<GitGraph>` and builds a new one**, and that has a focus consequence the paragraphs above do not cover. The dropped entity's focus handle leaves the dispatch tree, so the window's focus points at a dead id, `render`'s `is_focused` guard is false and its redirect into the graph cannot fire, and `Workspace`'s focus-lost listener then yanks focus out to the centre pane. The user's next arrow key scrolls a buffer instead of the commit list, and because `contains_focused` is false the tri-state hotkey spends a press re-focusing instead of hiding. `set_active_repo` therefore captures `contains_focused` **before** replacing the graph and re-focuses afterwards — onto the panel's OWN handle, not the new graph's, so `render` stays the single place that decides what inside the panel holds focus (with no repository there is nothing to redirect into, and focus correctly rests on the tracked container). Identical shape and reason to `console_panel::ConsolePanel::close_tab` (`2819c6b1bd`).

The hazard predates the redirect, but it only became the *default* path once the graph got a way in from the keyboard: **`ctrl-alt-\`` = `git_graph::ToggleFocus`** (`assets/keymaps/default-{linux,macos,windows}.json`; the graph is de-docked into the Solution band and has no dock button, so this chord is its only hotkey — `ctrl-shift-g` was unavailable, it is `git_panel::ToggleFocus`). With a hotkey, "focus lives inside the graph" is the normal state, so the ejection became the normal outcome.

How to apply: any panel that swaps out a child view it may be focused into has this bug. Capture `contains_focused` before the swap and re-home focus after it; a unit test that only asserts the new child exists will not catch it.

### 31. Supervisor decisions are re-checked at SEND time, not only at judge START — and known-stale verdicts interrupt the judge instead of running it to completion

Every condition that gates the observer was historically evaluated ONLY at judge-**start** (`should_fire` / `tick_supervisor`). But an ephemeral judge turn runs for seconds→minutes; the world moves between fire and the verdict's delivery. So each gate is now **double-checked** — at start *and* at send — and the moment a condition makes an in-flight verdict useless, the judge is **interrupted** rather than left to finish and have its verdict dropped at the end.

- **Send-time gate (`store::apply_verdict`):** in addition to the bug-#1 `judge_superseded` staleness marker, the verdict is dropped when the live state says supervision isn't actively running: `!enabled` (Disabled), `Held` (manual Stop), or `Stopped` (usage wall / provider death). `Watching`/`Judging`/`WaitingUser` pass through (so the direct-apply paths — e.g. a `Done` verdict — still act). This is the backstop for a verdict that already left the judge before it could be torn down. **It also re-checks SESSION state**: `should_fire` only lets a judge fire while the session is idle/errored, but the agent can resume on its own during the judge turn (a `Bash(run_in_background)` continuation lands as an orphan result → `Running`; a tool-auth prompt → `AwaitingInput`). A verdict delivered against a session that is no longer idle/errored is dropped — otherwise the nudge queues a spurious extra turn behind the live one (reported: "supervisor reacted while the agent was still alive and the message got queued"). **The drop also re-arms `Judging → Watching`** (`finish_judge` doesn't touch `status`): leaving it pinned `Judging` with no live judge would let the judge-stuck watchdog mistake it for a crash and charge a bogus backoff, compounding to a false `Stopped(ProviderError)`. Guarded by `continue_verdict_dropped_when_agent_already_running`. The hold-on-typing `pending_nudge` flush is likewise gated on `Watching` (drop a parked nudge once the session is `Held`/`WaitingUser`/`Stopped` — `pending_nudge_dropped_when_paused_before_flush`).
- **Interrupt on known-stale (synchronous teardown of the running judge):** turning supervision OFF (`set_supervision_enabled(false)`) now calls `finish_judge`+`finish_auditor` (previously it did NOT — a running observer kept going and nudged after you disabled it); changing the instruction (`set_supervisor_prompt`) tears down a judge that reviewed under the old prompt and re-arms to `Watching`; a user reply (`supersede_judge_on_user_reply`) and a manual Stop (`hold_supervisor`) already did. All also set `judge_superseded` so a racing verdict is dropped by the send gate.
- **Hold-on-typing (`store::send_supervisor_nudge` + `tick_supervisor` flush):** the start-time typing guard (`should_fire`'s silence clock) only stops a NEW judge from firing while you type; it can't cover a judge that fired while you were idle and finished after you started composing. Rather than barge into your half-written message, the verdict is accepted (continue-counter bumps) but its nudge is **parked** in the transient `SupervisorState.pending_nudge`. `tick_supervisor` flushes it once you've gone quiet for `IDLE_THRESHOLD_SECS` (the "changed my mind, stopped writing" case); a genuine user SEND discards it in the `from_user` funnel (the held observer nudge is stale once you answered yourself).

How to apply: any autonomous supervisor action that reaches into the supervised session must re-validate the live `SupervisorState` at the point of the side-effect (not trust the start-of-run snapshot), and any state change that invalidates an in-flight judge should `finish_judge` + set `judge_superseded` rather than relying solely on the send-time drop. UI: the status-row eye pulses (opacity animation, `pulsating_between`) while `status == Judging` so "observer working now" is visible at a glance.

### 32. The supervisor judges only genuinely-new state — it never polls

The judge is an expensive one-shot decision (a whole ephemeral `claude` turn), not a poller. Re-invoking it on a session whose state hasn't changed produces the same reasoning burned over and over — one real session accumulated 89 identical `wait` verdicts (and 82 `continue`s) polling a parked agent every ≤5 min. Three mechanisms keep the judge from firing when there is nothing new:

- **No fire while a background command/agent is running (`tick_supervisor`).** A session sitting idle over a live `background_shell` (`ShellRuntimeState::Running`) or a still-running managed `background_agent` (`is_messageable()`) is legitimately idle — the agent is waiting on work it launched, and hung background work is already watched by the background-shell watcher + the `Running`-stuck watchdog. The tick skips such sessions entirely (same shape as the typing-defer), re-engaging once the work finishes and the agent goes genuinely idle. This removes the bulk of `wait` verdicts at the root. **Completion resets the silence clock (`mark_background_shell_state` + `refresh_background_agent_snapshot`):** while the work ran, `last_activity_at` stayed frozen at launch, so on completion the accrued silence is already past `IDLE_THRESHOLD_SECS` and the judge would fire the INSTANT `has_live_background_work` flips false — racing (and usually losing to) the agent resuming ON ITS OWN to read the result (a `Bash(run_in_background)` orphan continuation). A terminal transition bumps `last_activity_at` — for a background SHELL (`Exited`/`Killed`) and, symmetrically, for a managed background AGENT reaching a terminal `stop_reason` — giving the agent a fresh full idle window to self-resume before the supervisor judges. Guarded by `background_shell_completion_resets_silence_clock` + `background_agent_terminal_transition_resets_silence_clock`; the send-time session-idle re-check in `apply_verdict` (§31) is the backstop for the residual race.
- **`wait` is one-shot (`apply_verdict` Wait arm + `tick_supervisor` wait handler + `SupervisorState.wait_until_ms`).** When the judge decides "the agent is waiting on X," it commits a single realistic timeout (`wait_seconds`, clamp raised 5 min → **30 min**) parked in the transient `wait_until_ms`. The mechanism honors the FULL duration — it does NOT re-spawn a judge in between — and when the deadline elapses the mechanism itself wakes the agent (a deterministic "check the result and continue" nudge, only if it's idle; if the agent already resumed, the wait is just dropped). `wait_until_ms` is cleared on a fresh fire, a user message (`from_user` funnel), and enable/disable; the handler is gated on `Watching` so a stale deadline can't act on a `Held`/`WaitingUser` session.
- **Waiting on the operator is `done`, not `wait` (`supervisor_judge_instructions.md`).** `wait` is reserved for the agent's OWN async task that finishes on its own clock. If the agent is idle pending the operator (asked you to compact, handed off, awaiting go-ahead) or another party — anything with no timer of its own — the judge returns `done` (park in `Held`; the operator's next message re-arms) or `ask`. The instruction states the invariant explicitly: **if nothing changed since the last verdict, the verdict must not change.**

How to apply: before spawning a judge, ask "is there genuinely new state to judge?" — if the agent is parked on tracked async work, or a committed one-shot wait hasn't elapsed, or supervision is waiting on the human, don't fire. Poll-shaped verdicts (`wait` re-issued on unchanged state) are the anti-pattern.

### 33. Supervisor in-flight state is reconciled on restart — no phantom judge, no instant fire on inherited idle

The supervisor persists its **row** (`enabled`, `status`, `last_fired_at`, …) but its in-flight state lives only in transient maps that don't survive a restart (`judge_sessions`, `last_user_input_ms`, `pending_nudge`, `wait_until_ms`). Reopening the editor therefore used to produce two wrong behaviours, both reported live: (a) the observer eye **fired a judge the instant the editor opened**, before the user typed anything, and (b) a session that had been mid-`Judging` at shutdown restored as **"reviewing" that hangs forever**. Two reconciliations at load fix them:

- **Phantom `Judging` → `Watching` on load (`db::load_supervisor_states`).** A judge exists only in the transient `judge_sessions` map, so a row persisted mid-`Judging` restores with no judge actually running: `supersede_judge_on_user_reply` (gated on `judge_sessions`) no-ops on the user's next message, and the judge-stuck watchdog only fires if the persisted `last_fired_at` is already stale — so the status row can sit at "reviewing" indefinitely. Load coerces `Judging → Watching` and drops the stale `last_fired_at` so the session resumes clean.
- **Inherited idle is left alone until a manual kick (`SupervisorState.watch_started_ms`, transient).** Product rule (operator's explicit ask): after closing and reopening the editor, **nothing auto-resumes** — every session that was parked before the restart waits for a manual kick. The idle-nudge measures silence from `last_activity_at`, which after a restart is stale (hours old), so `should_fire` would see "silent for hours" and fire on the first tick. The restart/load path (`set_persistence`) stamps `watch_started_ms = now`; `tick_supervisor` then fires only when the session has produced genuinely-new activity THIS process (`last_activity_at > watch_started_ms` — a manual kick starts a turn, which bumps it past the baseline). So a pre-restart idle session is watched but never auto-nudged; once the operator kicks it and its turn completes, the normal idle-nudge cycle re-engages. A **fresh in-session enable** leaves `watch_started_ms = None` (always eligible) — its idle arose under our watch, so immediate-idle semantics are unchanged.

How to apply: any supervisor field that is transient (lives in a runtime map, not the DB row) must be reconciled against the persisted `status` on load — a persisted status that implies live in-flight state (`Judging`) is a phantom after restart and must be coerced. The autonomous supervisor must never act on state it did not observe change: a wall-clock idle gate must require activity *after* this process began watching, not merely a stale persisted timestamp — otherwise reopening the editor retroactively "resumes" everything. Guarded by `judging_status_coerced_to_watching_on_load` (db) and `restart_leaves_inherited_idle_until_fresh_activity` (store).

### 34. The hang watchdog treats a usage-limit wall as a wall, not a hang

`tick_stuck_sessions` reconnects a session stuck `Running` with no streaming / tool activity for `STUCK_TURN_SECS` (the "hung subprocess" heuristic). But a turn that hits claude's usage/session/weekly limit prints the wall as its last assistant message and then **stalls without ending** — indistinguishable, by silence alone, from a hang. The watchdog would reconnect and send the "your process hung, carry on" continuation, which starts a fresh turn that re-hits the same wall → stall → reconnect → a quota-burning loop (reported live: repeated `You've hit your session limit` + a spurious *"твой процесс завис… продолжай"* nudge).

Fix: before recovering a wedged session, scan its latest assistant message; if it matches `supervisor::is_usage_limit_error`, route it to `apply_usage_limit_stop` (stop the turn as `Errored(<wall>)` so tick_stuck — `Running`-only — can't re-fire; push a `system` note; schedule an auto-resume at the parsed reset time if the observer is enabled, else `Stopped(Quota)`) instead of `reconnect_agent`. Genuine hangs (no limit text) take the unchanged reconnect path. `apply_usage_limit_stop` is extracted from `on_judge_failed`'s `JudgeFailure::Quota` arm — the judge-failure path and the agent's-own-turn path now share one wall handler.

How to apply: any "the session went silent / errored" recovery heuristic (reconnect, retry, nudge-continue) must first check whether the silence is a provider *wall* (`is_usage_limit_error` on the surfaced message) — a wall is recovered by waiting for the reset, never by retrying, which just re-hits it. Guarded by `stuck_usage_limit_wall_stops_without_reconnect`.

### 35. Every turn-end path must flush the end-of-turn tail into `session.entries`

The final buffered assistant text of a turn is flushed into the entry markdown at run-turn completion, followed by an explicit `AcpThreadEvent::EntryUpdated(last)` — that emit is what makes the store re-convert the entry and **bump its `mod_seq`** (the reveal task's per-tick updates stop before the final tail). The mobile client's whole delta-sync (`get_session` / `get_session_changes` filter `mod_seq > since_seq`) reads `SolutionSession.entries`; if the tail never lands there with a bumped seq, NO client catch-up (reopen delta, full reload, safety-net poll, reconnect) can recover it — the reply is permanently truncated.

`claude_native` synthesizes a terminal event **out of band** for an *orphan result* (claude emits a terminating `result` with no `prompt()` in flight — e.g. a `Bash(run_in_background=true)` continuation resuming on its own and emitting a second result). That path emitted `Stopped`/`Error` directly on the thread WITHOUT the tail flush, so the follow-up message's tail was lost from `session.entries` while `Idle` still propagated — the "mobile shows a stale intermediate step + Idle" bug. Fix: `AcpThread::flush_end_of_turn_tail` (new `pub` method = `flush_streaming_text` + `EntryUpdated(last)`, called by the mainline arms AND the orphan path). Secondary: the store's `Error`/`LoadError` arm now flushes pending entry-append throttles synchronously (`flush_pending_entry_appends`, shared with the `Stopped` arm) so a turn that errors while already `Errored` doesn't strand the final append on the 500 ms debounce.

How to apply: any code that ends a turn out-of-band (synthesizes `Stopped`/`Error`, force-idles, reconnects mid-turn) MUST call `flush_end_of_turn_tail` in the same synchronous step before the terminal emit — the reveal task won't have flushed the last bytes. Guarded by `flush_end_of_turn_tail_signals_last_entry` (acp_thread) + `errored_flushes_pending_entry_update_debounce_immediately` (store).

### 36. Desktop "agent finished" notifications: fire only when truly quiescent, and click-to-focus the originating session

Two refinements to the freedesktop desktop notifications (`solution_agent::notifier` + the new `crates/zed/src/notification_focus.rs`):

- **"Truly done" gate (`decide_notification`).** The `Completed` toast must only fire when nothing more will happen without the user. Beyond the existing `has_pending_messages` suppression, two signals were added: `has_live_background_work` (idle OVER a running `background_shell` / messageable `background_agent` — it resumes on its own) and `supervisor_will_continue` (the Observer is enabled + `Watching`/`Judging` — it will auto-nudge, and fires its OWN `notify_supervisor_done` / `escalate_to_user` when work actually concludes). `AwaitingInput`/`Errored` still fire regardless (parked-needing-you / broken).
- **Click-to-focus (`zed::notification_focus`).** `notifier::dispatch` now stamps each notification with `default_action("open")` (id already `dev.sawe.session-{sid}`). A single long-lived listener spawned in `notification_focus::init` (from `main.rs`, Linux/FreeBSD only) subscribes to the portal's `ActionInvoked` signal via one `ashpd::NotificationProxy` (the signal is on the shared portal object, so one proxy sees clicks for every notification we sent). On a click it parses the id → `SolutionSessionId` and runs a main-thread focus sequence: raise the window (`Window::activate_window`), activate the session's Solution (`MultiWorkspace::activate` — resolve the owning `Workspace` by worktree→`SolutionStore::solution_for_path`), then select the session as the Solution band's active dialog (`SolutionAgentStore::set_active_dialog_session`). **Amended 2026-08-26 (decision 93):** the dock-reveal step this used to describe no longer exists. Chat left `ConsolePanel` for the band, so there is no `focus_panel::<ConsolePanel>` call and no `ConsolePanel::show_session` — both were deleted. The selection is a store mutation the band reads directly, which is also why it is called inline here rather than dispatched as `console_panel::ShowSession`: a background notification click has no reliably-focused view to dispatch an action from.

How to apply: the click-routing lives in the `zed` crate because it spans `solution_agent`+`solutions`+`workspace` (a cycle if placed lower). The window root is `MultiWorkspace` — enumerate via `cx.windows().filter_map(|w| w.downcast::<MultiWorkspace>())`. Guarded by `notifier` unit tests (quiescence gate) + `notification_focus::tests` (id parsing); the actual portal click→focus is manual-verify only (no headless portal).

### 37. The supervisor reads the agent's ANSWER, never anchors on its own nudges, and a manual `/clear`/`/compact` wipes its memory

Three linked fixes to what the observer (judge) sees, all rooted in the same DTO/filter surface (`solution_agent::mcp` `get_session` + `apply_user_anchored_filter`, instructions in `resources/supervisor_judge_instructions.md`). Full story: `docs/findings/2026-07-05-observer-cant-see-agent-answer.md`.

- **The judge now sees the agent's answer.** `apply_user_anchored_filter` gained a **trail**: after each real-user anchor it keeps up to `USER_ANCHORED_TRAIL_ASSISTANT` (5) assistant *text* turns (the reply), skipping tool calls, stopping at the next user-role entry so adjacent messages don't overlap. Before, only the entries *before* a user message (lead) + the resting turn were kept, so an answer the agent gave then worked past was invisible next wake-up → the observer re-nudged the same already-answered directive.
- **A supervisor nudge is no longer mistaken for a user goal.** A nudge is delivered as a `role==user` entry carrying only the `spk_observer_nudge` `_meta` marker (`store::deliver_nudge_now`). New `EntrySummary.observer_nudge: bool` surfaces it (via `acp_thread::is_observer_nudge_blocks`); the filter anchors on `role==User && !observer_nudge`, and the judge instructions tell the auditor its own past nudges / `system_level:observer` notes are its own voice, not fresh requests. The **same DTO field fixes mobile's Observer plaque** (`spk-editor-mobile` keyed on `role==System`, but a nudge is `role==User` → it rendered as a plain user bubble; now `role==User && observerNudge` renders the eye plaque). Desktop was already correct (`conversation_render` reads the marker off retained chunks).
- **Manual `/clear` and `/compact` reset the observer to a clean slate.** New `supervisor::wipe_supervisor_memory` (diary + verdicts + user_intent) + in-memory reasoning-cursor reset, called from `store::reset_context` and the USER path of `start_compact_for_session` (gated by `CompactInitiator::{User,Observer}`). The observer's OWN `compact` verdict deliberately does NOT wipe — `user_intent.md` must survive the transcript loss there. This is the operator's escape hatch for a looping observer.

How to apply: the observer/nudge distinction is INVISIBLE at `role` level (a nudge is user-role by design so the agent acts on it) — always gate on `observer_nudge`/`is_observer_nudge_blocks`, never on role alone. When adding a new compaction/clear entry point, thread `CompactInitiator` through it so the user-vs-observer wipe decision stays explicit.

### 38. Async `Agent` teammates: register the background pill from tool_result CONTENT, and never leak their tagged output into Main

claude's `Agent` tool is an **async teammate** — the tool call returns a spawn-ack immediately, then the teammate streams for minutes, its output tagged in the parent thread with `subagent_id = <the Agent call's toolu id>`. Two linked bugs made an actively-streaming teammate show *no strip tab* while its messages flooded Main. Full story: `docs/findings/2026-07-06-subagent-entries-flood-main-after-finish.md`.

- **A — the background pill never registered.** `apply_subagent_lifecycle` parses the `agentId:`/`output_file:` announcement from the terminal `Agent` call's **`raw_output`** — but for an async launch `raw_output` is null and the announcement rides in the tool_result BODY (the tool call's **content**). Parse failed → no `BackgroundAgent` → no strip pill (and the Task pill was already removed on completion). Fixed: `background_agent::managed_agent_announcement(raw_output, content)` tries `raw_output` first then falls back to content; the lifecycle snapshot now captures the terminal task-like call's content text.
- **B — Main showed tagged entries when the strip was empty.** Sub-agent entries are `subagent_id`-tagged and the Main view filters them out (`SubagentView::matches_parent_entry` / `filterEntriesBySubagent`), but both surfaces *bypassed* the filter when `active_subagents` was empty (desktop `should_render_entry`, mobile `ChatList`) — a "cold-restart guard". A background agent leaves `active_subagents` empty, so the bypass fired the whole time it streamed → tagged output flooded Main. Removed on both surfaces: Main is always main-thread-only.

How to apply (historical — see the superseded note below for the current model): the two fixes were load-bearing together. Still-true invariants: don't reintroduce an "empty active set ⇒ show everything" shortcut; an async `Agent`'s useful metadata is in its CONTENT, not `raw_output`; the `Agent` spawn call itself is a Main-thread entry (`subagent_id == None`) so "spawned a teammate" shows in Main, while the teammate's interior now lives in its demux `Teammate` stream tab (not a JSONL Background tab).

**Migration status (FULLY SUPERSEDED, migration complete 2026-07-07):** fix **B** is gone — Main is a demux stream that never contains `subagent_id`-tagged entries, so there is no filter to bypass (`should_render_entry` deleted phase 2c, `filterEntriesBySubagent` deleted phase 5, no `active_subagents.is_empty()` guard remains). The **separate `Background`/JSONL tab described above no longer exists**: phase 6d-B folded async `Agent` teammates onto their demux `StreamId::Teammate(parent_tool_use_id)` stream (they render as a normal teammate pill, view-only), and phase 6d-tail-1 deleted the `SubagentView::Background` variant + `build_background_entries_for_render`. Fix **A** (`managed_agent_announcement` content-fallback) STILL STAYS — but ONLY as the completion-signal/label path: it registers the `BackgroundAgent` whose `parent_tool_use_id` (a) auto-closes the teammate stream on the real `stop_reason` and (b) is captured as the stream's friendly label at registration (`teammate_labels`, phase 6d-tail-2). The JSONL is no longer a live render source (spec Decision 3).

### 39. Assistant-message coalescing skips *other* sources' interleaved entries (torn-message fix)

A claude async `Agent` teammate streams into the PARENT thread concurrently with the parent, so the parent's own text deltas arrive interleaved with the teammate's `subagent_id`-tagged chunks. `AcpThread`'s coalescing keyed on `entries.last()`, so after an interleaved subagent chunk the parent's next delta started a fresh `AssistantMessage` — tearing one message into many bubbles (split even mid-word), on Main and mobile. Fixed with a backward scan (`AcpThread::coalesce_target_index`, reinstated 2026-07-13 as `open_assistant_message_index` — see the migration note below) that skips *other* sources' entries and appends to the most recent same-source message, stopping at the source's OWN tool call or any structural entry (user msg / plan / compaction / system note). Both the streaming-buffer and non-buffer paths use it and emit `EntryUpdated(target_idx)` for the real (possibly non-last) entry. Full story: `docs/findings/2026-07-06-torn-message-interleaved-subagent-chunks.md`.

How to apply: coalescing target ≠ `entries.last()` once concurrent subagents exist. If you add a new `AgentThreadEntry` variant, classify it in `coalesce_target_index` (source-bearing boundary vs structural boundary vs skippable) or streaming will mis-group. The sub-agent tab looks clean because it sources from the teammate's own JSONL, not the interleaved parent thread — a "renders fine on the sub tab but torn on Main" report is this bug.

**Migration status (phase 6b, 2026-07-06, `306ca1af5f`): reverted — then RE-INSTATED (2026-07-13, `ad45c60d95`). The model is TWO-LAYER; do not delete either half.**

Phase 6b deleted `coalesce_target_index` on the theory that the demux made it redundant: the store re-groups each `AcpThread` event into per-source `session.streams` and coalesces within a stream (`stream::push_coalesced`), so a flat tear "cannot be seen". That is **half true, and the half it misses is the user-visible half.** `push_coalesced` merges the two torn ENTRIES, but it merges them as `chunks.extend(...)` — the fragments stay separate CHUNKS, and each chunk renders as its own markdown block. A sentence split mid-word therefore still renders with a paragraph break through the middle of the word, and any markdown span crossing the split (`**bold**`) never closes. The demux reunites the entry; it cannot reunite the text. Only `AcpThread` can, because that is the layer where the delta is still being appended into the SAME `Markdown` entity.

So the backward scan is back as `AcpThread::open_assistant_message_index`, and both layers are load-bearing:

- **`AcpThread` (text):** a chunk continues the open message of ITS OWN stream, stepping over entries owned by a different one; a same-stream tool call / user message / system note still closes it. Guards the text and the markdown spans. Test: `test_interleaved_subagent_chunk_does_not_tear_parent_message`.
- **`stream.rs` demux (entries):** still coalesces adjacent same-source assistant entries — the backstop for every tear `AcpThread` does not or cannot prevent (cold-load, rewind, hydration). Test: `demux_reunites_a_parent_message_split_by_an_interleaved_teammate`.

Load-bearing corollary: the coalescing target is NOT `entries.last()`, so **every** `EntryUpdated` emit along the streaming path must carry the index of the entry actually appended to. `StreamingTextBuffer.target_entry` records it (the reveal task, `flush_end_of_turn_tail` and `flush_streaming_text_and_signal` all read it). Emitting `entries.len() - 1` re-syncs a teammate's entry and leaves the parent's stale in the persisted / MCP mirror. Findings: `docs/findings/2026-07-06-phase6b-retire-flat-entries.md`.

### 40. Desktop renders the demux'd per-source stream, not flat `entries` + a filter (per-source-streams migration, phase 2c)

The in-flight per-source-streams migration (spec `docs/superpowers/specs/2026-07-06-per-source-streams-design.md`) makes desktop + mobile run on the SAME per-stream model, replacing the flat, global-indexed `SolutionSession.entries` with `SolutionSession.streams: IndexMap<StreamId, Stream>` (a maintained demux mirror). Phase 2c flipped the DESKTOP render (`session_view.rs`): the non-drill-in path reads `session.streams[selected].entries` (Main→`StreamId::Main`, Task(toolu)→`Teammate(toolu)`) via the frame-local `main_stream_entries_for_render`, instead of iterating `session.entries` + a `should_render_entry` filter (deleted). `list_state` became the **render authority** — sized to the selected stream's count at render top (reset+tail on tab switch via `prev_render_view`; grow/shrink by tail-splice otherwise), and `on_thread_event`'s global-index splices/remeasures were removed (visible rows self-remeasure every layout pass, so streaming still grows). `markdown_cache` needs no re-key (self-heals via `ensure_markdown` text-validation + the render-top `idx < count` retain). Full story: `docs/findings/2026-07-06-phase2c-desktop-render-flip.md`.

How to apply: every writer of `session.entries` MUST call `rebuild_streams()` after (phase 2c found the four cold-load/hydration paths didn't → restored sessions would render blank). Route any new view→stream selection through `SubagentView::parent_stream_id()`, the single mapping the render/rewind/find paths share. The shipped quick-fixes #38/#39 stay live through the migration; phase 6 removes them once `streams` fully owns rendering on both clients. Verification infra added the same session: a debug-only `solution_agent.seed_cold_session` MCP tool (paints arbitrary multi-stream states without a live subprocess; `#[cfg(debug_assertions)]`, not in release) and a `windows.scroll_at` primitive.

**Phase 3 (auto-close):** a `SolutionSession.closed_streams` overlay that `rebuild_streams` `shift_remove`s, fed by `close_stream(id, reason)`. Auto-close on a teammate's tool-call TERMINAL status is gated to inline `Task` ONLY (`!tool_name_is_agent`): an async `Agent`'s spawn tool-call goes terminal AT SPAWN-ACK while the teammate keeps streaming `subagent_id`-tagged entries for minutes, so closing there would suppress its still-live demux stream (decision #5: the demux is the teammate's source of truth). Async `Agent` close + hydration-orphan close are deferred to phase 4. Non-obvious and load-bearing — see `docs/findings/2026-07-06-phase3-teammate-stream-autoclose.md`.

**Phases 4+5 (wire + mobile, HARD CUTOVER — SHIPPED in lockstep):** the mobile wire (`solution_agent.get_session`/`get_session_changes` in `mcp.rs`) now serves `session.streams` instead of flat `entries` + `active_subagents`: descriptors-for-ALL-streams (`StreamDto`, tagged `StreamIdDto`) + entries-for-the-SELECTED-stream (`stream_id` param replacing `subagent_filter`; STREAM-LOCAL index; per-stream `seq` delta keyed on coalesce-aware `entry.mod_seq`). `current_seq` = the selected stream's watermark, not the global `change_seq`. `wire_schema_version` 2→3 (`crates/editor_mcp/src/tools/capabilities.rs`); `sawe-mobile` mirrors the DTOs, deletes `filterEntriesBySubagent`, drives tabs from `streams`, and adds an `isServerTooOld` reject — shipped together (`sawe-mobile` `origin/main` `dc1977d`). Phase 4a also closed the two phase-3 deferrals: async-`Agent` stream close via `BackgroundAgent.parent_tool_use_id` on the real `stop_reason`, and hydration-orphan close via a reopenable `hydration_orphan_streams` + `hydration_watermark` overlay (distinct from the permanent `closed_streams` Done-close). Non-obvious gotchas (both review-caught): the per-stream delta keys on a COALESCE-AWARE `mod_seq` (`push_coalesced` bumps a merged entry's `mod_seq`), and a rewind that SPLITS a coalesced group re-stamps the surviving boundary entry (`store.rs` `EntriesRemoved`) or the delta silently misses the shrink. Render gate passed end-to-end on a headless Android emulator (recipe in `docs/findings/2026-07-06-phase5-mobile-streams.md`). Findings: `2026-07-06-phase4a-server-model.md`, `-phase4b-server-wire.md`, `-phase5-mobile-streams.md`.

**Phase 6 (cleanup)** — sequenced + de-risked in `docs/superpowers/plans/2026-07-06-per-source-streams-phase6-cleanup.md` (gitignored). **6a ✅** (`4285108bd5`): #38 flood-bypass (fix B) confirmed structurally gone; fix A retained. **6b ✅** (`306ca1af5f`): persistence authority moved to `streams[Main]` (Main-local index, seq-watermark incremental persist; per-session persist SERIALIZED against GPUI's non-FIFO detached-write race; legacy global-index rows realign on cold-load), #3 reverted (see #39 above), decision-#11 rewind re-stamp re-homed to Main. Flat `entries` KEPT as the 1:1 `AcpThread` ingest mirror + demux input (full removal deferred). Findings: `docs/findings/2026-07-06-phase6b-retire-flat-entries.md`. **6c ✅** (`7aeeee7470`): the DESKTOP tab strip's teammate (`tabs`) loop + `next_selection_after_change` now read `session.streams` instead of `active_subagent_order` (snap-to-Main only on stream removal). STAGED teammates-only — `SubagentView` variants and the `active_subagents*`/`background_*_order` fields are KEPT (still wire-backing `SessionSummary` + needed for bg-agent/shell drill-in until 6d; `StreamId` has no `Background` variant and async-`Agent` teammates are double-represented until 6d). The `∈ active_subagents` filter on the streams-derived `tabs` is the behavior-preserving bridge (excludes async-`Agent` teammate streams that render as `bg_agents` pills, so no double-pill). Fixed WHILE HERE: the decision-#16 latent-prod bug — `hydrate_streams_main_only` recorded orphans from the STALE `self.streams` (cold-load assigns `entries` with no rebuild first) → zero orphans → decision-#9 zombie teammate tabs after restart; now derives orphans from `demux(&self.entries)`. Plus a review-caught regression: the `→Idle` strip GC (`store.rs:8805`) cleared `active_subagents` without closing the teammate stream, stranding a viewer on a frozen tab under the new streams-only snap → GC now `close_stream`s each cleared teammate. Findings: `docs/findings/2026-07-06-phase6c-desktop-strip-streams.md`. **6d-A ✅** (`ed335daa49`): background SHELLS fold into `session.streams` as auto-closing `StreamId::Shell` tabs (only while `Running` — the dismissible terminal-× UX dropped per the user); cx-free `BackgroundShell::stream_entry`/`stream_label`, injected in `rebuild_streams` from `background_shells` (rebuild called at all 5 mutation sites), rendered from streams (deleted the map-based shell strip loop + `build_background_shell_entries_for_render` + `build_shell_drill_in_entries`); `SubagentView::Shell.parent_stream_id()`→`Some(StreamId::Shell)` so the shell body renders through the unified stream path. WIRE-INVISIBLE: `build_streams_vec` filters `StreamKind::Shell` + `get_session`/`_changes` coerce a Shell `stream_id` to Main, so the v3 wire is byte-identical (mobile untouched). Findings: `docs/findings/2026-07-07-phase6d-A-shells-into-streams.md`. **6d-B ✅** (cross-repo HARD CUTOVER, `wire_schema_version` 3→4): background AGENTS fold onto their demux `Teammate` stream — dropped the `∈ active_subagents` bridge filter (all live teammate streams render; async `Agent` shows as a `Task` pill labelled from its `background_agents` JSONL `activity_label`, looked up by `parent_tool_use_id`), deleted the `Background` pill machinery (`background_pill`/`BackgroundAgentDisplayState`/classifier); removed the 6d-A Shell wire filter + Shell→Main coercion (shells now ride the wire as `kind: shell`, selectable/pageable); removed the two `get_session_background_{shells,agents}` tools + their `GLOBAL/SHARED_TOOLS`/`allow_list` entries (catalog 88→86; DTO builders KEPT — `event_sources` still emits the payloads). `sawe-mobile` in lockstep: deleted the parallel `BackgroundShell`/`BackgroundAgent` strips/sheets/StateFlows/RPCs/notifications + 6 now-unreferenced DTOs, `SUPPORTED_WIRE_SCHEMA_VERSION` 3→4 (v4 client rejects a v3 server), added a Shell Roborazzi golden. **Both gates PASSED** (desktop offscreen strip: async-agent teammate stream not in `active_subagents` now pills; Android emulator v4↔v4 render gate: shell pill from `streams` confirmed on-device). Scope-fenced to 6d-tail (all now dead-but-compiling): `SubagentView::Background` variant + its arms, `build_background_entries_for_render`, the bg-agent `event_sources` payload emission, `wire_dict`/`WireDictionary` dead RPC-name literals (byte-pinned across repos). Findings: `docs/findings/2026-07-07-phase6d-B-agents-and-wire-v4.md`. **6d-tail-1 ✅** (desktop-only dead-code sweep, no wire-shape change, ~1000 lines removed): dropped `SubagentView::Background` + all its arms (enum now `{Main,Task,Shell}`, isomorphic to `StreamId`) + `build_background_entries_for_render` + the JSONL drill-in RENDER branch (render now ALWAYS sources `main_stream_entries_for_render`) + `next_selection_after_background_change` + the dead methods `is_parent_thread_view`/`matches_parent_entry` + the dead fns `remove_background_agent`/`remove_background_shell` + the unadvertised bg-agent/shell WIRE notification emit + its payload builders + the orphaned mcp.rs bg DTO builders. KEPT (wire-backing/live): `active_subagents*`+`SessionSummary.active_subagents`, `background_agents`/`background_agent_order`, `db.delete_background_shell`, `QueueTarget::Subagent`/`is_messageable`, the in-process store events. Product consequence (from decision 23, shipped in 6d-B): a live async agent's tab is now view-only. 530 lib tests; desktop render gate passed (Main/teammate/shell each render via the collapsed uniform path). Findings: `docs/findings/2026-07-07-phase6d-tail-1-dead-bg-scaffolding.md`. **6d-tail-2 ✅** (cross-repo HARD CUTOVER, `wire_schema_version` 4→5): **`Stream.label` is now the single source of truth for a teammate's display label** on BOTH clients. Introduced a lean `teammate_labels: HashMap<toolu,label>` (stable label captured at registration for inline Task AND async Agent, reclaimed in `close_stream`), which `rebuild_streams` enriches onto each `StreamId::Teammate` stream's `label`; the desktop strip + the wire (`StreamDto.label`) just read it. Deleted `active_subagents`/`active_subagent_order`/`SubagentTab`/`SubagentDto`/`build_active_subagents_vec`/`started_at` + the `SessionSummary.active_subagents` wire field; `agent_session_active_subagents_changed` slimmed to a bare `{session_id}` dirty-poke. Reworked the →Idle GC to source stranded ids from `streams` EXCLUDING async-agent parents (`background_agents.parent_tool_use_id`) — label-safe, regression-tested (`idle_transition_gc_excludes_live_async_agent_teammate`). `sawe-mobile`: dropped the dead `SessionSummaryDto.activeSubagents` + `SubagentDto`, slimmed the poke payload, `SUPPORTED_WIRE_SCHEMA_VERSION` 4→5 — and teammate pills get FRIENDLY labels for free (they already render `StreamDto.label`). **Both v5 gates PASSED** (desktop offscreen: pill shows `task-<toolu>` not raw id; Android emulator v5↔v5: mobile pill shows the friendly label from `streams`, session list decodes with no `active_subagents`, handshake accepted). 531 lib tests. Findings: `docs/findings/2026-07-07-phase6d-tail-2-labels-on-streams-wire-v5.md`. **6e ✅** (`c815bfa4f6`): FORK.md #26/#38/#39 marked superseded (async tabs view-only, `queue_target`=`Main` all variants, no `Background`/JSONL tab); stale "Background view" compose comments → Shell; `SUPPORTED_EVENT_KINDS` +queue_changed/active_subagents_changed; whole-branch migration review CLEAN (no functional findings — streams model coherent, wire-parity solid, persistence sound, zero orphaned code). **Cosmetic `SubagentView`→`StreamId` rename DONE:** the redundant `SubagentView` enum is deleted; the view's selection is `selected_stream: StreamId` directly (`Task`→`Teammate`), `parent_stream_id()`/`queue_target()` methods removed (callers index `streams.get(&selected_stream)` / inline `QueueTarget::Main`). 529 lib tests; reviewer-clean; desktop render gate passed (Main/teammate/shell select + render). **THE PER-SOURCE-STREAMS MIGRATION (phases 1→6e) IS COMPLETE + SHIPPED — `streams: IndexMap<StreamId,Stream>` is the maintained render+wire truth on both clients, `Stream.label` the single label source, wire v5, all quick-fixes removed.**

### 41. Pane tab-bar buttons show on the active center pane, not just the keyboard-focused one

What: the pane New (`+`) / Split / Zoom tab-bar buttons render whenever the pane is the workspace's **active center pane**, in addition to the upstream "pane holds keyboard focus" condition. Predicate extracted as `should_show_tab_bar_buttons(pane, window, cx)` in `crates/workspace/src/pane.rs` (used by `default_render_tab_bar_buttons`).

Why: upstream gated purely on `pane.has_focus()`. In this fork's multi-pane Solution windows the user does most work in a **dock panel** (console / agent), so the center pane loses keyboard focus and the buttons vanished entirely; they also flickered on Solution switch (`MultiWorkspace::activate` → `focus_active_workspace` momentarily focuses the center pane, then focus returns to the dock). A temporary `SPK_TABBAR_DEBUG` instrumentation block existed to chase this — now removed, its purpose served.

How to apply: `should_show_tab_bar_buttons` returns true if the pane has focus OR its context menu is focused OR `Workspace::active_pane().entity_id() == pane`. Keying on `active_pane` is the fix's crux: focusing a **dock** panel does not call `set_active_pane` (only center-pane focus does), so the active pane is stable when focus moves to a dock and is restored intact across an in-place Solution switch (decision #16) — hence no flicker. In a split exactly one pane is active, so only one button set shows, matching upstream's single-set behavior. Don't regress this back to a focus-only gate.

### 42. `Workspace::on_focus_lost` refocus is gated on `owns_window_chrome()` (background workspaces must not grab shared-window focus)

What: the focus-guard registered in `Workspace::new` (`cx.on_focus_lost` → refocus own handle, `crates/workspace/src/workspace.rs`) now returns early unless `self.owns_window_chrome()` is true.

Why: in a `MultiWorkspace` window the retained (background) workspaces stay alive (`MultiWorkspace::retained_workspaces`) and keep this handler live. Focus is a shared-window resource, so an ungated background workspace re-grabbing focus clobbers the active one — the same "background must not act on the shared window" class that `owns_window_chrome` already guards for the title / edited-indicator writers.

How to apply: any per-workspace focus / window-global side effect in a `MultiWorkspace` (focus grabs, window-title / edited-indicator writes, other shared-window chrome) must be gated on `owns_window_chrome()`. **Honesty note:** this was hypothesized as the mechanism behind an observed rapid tab-bar flicker on Solution switch (a cross-workspace `on_focus_lost` ping-pong), but that oscillation was **NOT reproduced** — the gpui test harness never fires `on_focus_lost` on a workspace switch, so there is no behavioral regression test. It ships as a principled hardening, not a verified flicker fix; the visible flicker itself is fixed independently by decision #41. Invariant guard: `multi_workspace_tests::test_retained_workspace_does_not_own_shared_window`.

### 43. Async background `Agent` subagents are NOT restored on session cold-load (they don't survive an editor restart)

What: `solution_agent::store::reconcile_background_agents_for` — called only from the cold-load/hydration paths — no longer re-registers persisted `background_agents`; it drops ALL persisted rows and registers none. It went from a "restore" pass to a "purge stale rows" pass.

Why: an editor restart restarts the `claude` subprocess, and the async `Agent` subagents (tracked by tailing their JSONL output files) do not survive it. Re-registering them on cold-load resurrected their teammate stream **pills** in the console chat view — pills for agents that were gone and would never be reaped, so they accumulated across restarts. Crucially, a DB-restored async agent carried `parent_tool_use_id: None` (the toolu isn't persisted), which broke the async classification in `reconcile_finished_teammate_streams` — a live-vs-dead call that could not be made safely (an earlier "close stale unregistered Agent pills" attempt was reverted because it would permanently close the pill of a still-live resumed agent).

How to apply: on cold-load, teammate streams are already collapsed to `hydration_orphan_streams` (rendered Main-only, no pill) by `hydrate_streams_main_only`; with nothing re-registered there is no JSONL watcher, so no new tagged entry ever reopens the orphan — it stays collapsed (no pill), and nothing is *closed* (the collapse is reversible, so there's zero risk of killing a live agent). The tradeoff — accepted by the maintainer — is that a background agent that DID survive a restart is no longer tracked after it. Live (same-run) spawns still register + track normally; only the cold-load restore changed. Guards: `store::tests::{cold_load_drops_all_background_agents, reconciliation_drops_alive_row_too}`.

### 44. Self-resume re-arms a supervisor-parked session — and `done`-park is distinguished from a manual stop

What: when a session the supervisor **parked** resumes ON ITS OWN — the agent's self-scheduled monitor / `ScheduleWakeup` fired a fresh turn, or a background task the editor doesn't track came back — supervision re-arms to `Watching` instead of leaving a stale paused status hanging while the agent works. Two park states qualify: `WaitingUser` (an `ask` escalation) and `Held` **when it was set by a `done` verdict**. The hook is in `store::handle_acp_event`'s `NewEntry` branch: a genuinely-new agent entry (guarded by the existing `is_system_note` check, so the observer's OWN `ask`-question / summary notes don't count) calls `rearm_supervisor_on_self_activity`.

Why: the observer parks a completed-looking session with `done` (→ `Held`, "On hold" clock in the status row) or escalates with `ask` (→ `WaitingUser`, "Waiting for you"). Both rest on the premise "nothing moves until the human acts." But the maintainer's agents routinely park to await a **self-clocked** task — a CI/merge-gate `verify`, a build, a monitor armed to re-check a result — often running in a DIFFERENT solution member/project than the one being judged. When that task finishes and the agent continues on its own, the premise is false, yet the supervisor status stayed stuck at "On hold"/"Waiting for you" while the agent visibly worked (reported live).

The subtlety: `apply_verdict`'s `Done` arm deliberately parks in **`Held`** — the SAME status a manual user Stop (`hold_supervisor`) uses — so "done" and "I stopped it" both re-arm on the user's next message. That overload means a naive "re-arm any `Held` on self-activity" would ALSO drag a manually-stopped agent back to work, violating the maintainer's hard rule ("don't close/resume what I stopped"). So a **transient** (not persisted) `SupervisorState.held_by_done` flag distinguishes the two `Held` sources: `apply_verdict::Done` sets it `true`, `hold_supervisor` sets it `false`, and `rearm_supervisor_on_self_activity` re-arms `Held` only when it's `true`. The flag is read in exactly one place, always under a `status == Held` guard, and both (only) runtime `Held`-setters set it explicitly in the same synchronous block — so a stale value while `Watching` is inert. Transient is deliberate: a cold-loaded `Held` row defaults `held_by_done = false` → treated as a manual stop (won't auto-resume across a restart), consistent with #43 (background agents aren't restored on cold-load anyway). The human-message re-arm (`reset_supervisor_continue_counter`) is untouched and still resumes both `Held` sources on the user's next message.

Related prompt change (`supervisor_judge_instructions.md`): the judge instructions were over-generalizing "if the supervisor woke you, no async task can be running, so the agent is idle" — true ONLY for background work registered IN the judged session (which suppresses the wake via `has_live_background_work`, see #32), FALSE for a verify/CI/monitor the agent armed in another project. The `wait` guidance now states the deciding test is "WHO moves it next" (a self-clocked task with its own clock → `wait`, even cross-project; a human-with-no-timer → `done`/`ask`), plus a hung-`wait` rule (on re-consult, if the agent's own last entry is stale past its stated ETA, `continue` to wake-and-check rather than `wait` again — the reliable staleness signal, backed by the 30-min `wait` ceiling). The judge also now writes operator-facing text (`ask` question + `reasoning`) in the **user's language**.

How to apply: any status the supervisor treats as "parked until the human acts" must also consider whether the AGENT itself can end the park (a self-clocked task it's awaiting) and re-arm on that. When one status value is overloaded across two intents that need different behavior, disambiguate with an explicit flag set at every entry point and read only under the status guard — don't branch on the shared status alone. Guard: `store::tests::self_resume_rearms_parked_supervisor`.

### 45. The stuck-session reconnect must re-surface an unanswered user message, not "carry on"

What: `SolutionAgentStore::maybe_send_reconnect_continuation` now picks its continuation prompt by the transcript tail. When the hang happened on an UNANSWERED human message (`tail_is_unanswered_user_message` — scanning `session.entries` from the end past `System` notes, the first real entry is a non-observer-nudge `UserMessage`), it sends `RECONNECT_UNANSWERED_USER_PROMPT` ("you hung before answering the user's last message — re-read it and do it now, don't treat it as already handled") instead of the generic `RECONNECT_CONTINUATION_PROMPT` ("carry on where you left off"). `reconnect_agent` captures the flag from `session.read(cx).entries` before the cold-ize.

Why: a user message was silently dropped. Proven from a live transcript: the user sent a message; the `claude` subprocess hung processing it for `STUCK_TURN_SECS` (5 min); `tick_stuck_sessions` fired `reconnect_agent`, which respawned the subprocess and injected the generic "carry on" continuation. The fresh subprocess, told to continue prior work, treated the replayed user message as already-handled history and never acted on it — the message was visible in the conversation but never answered. The generic continuation is correct for a MID-WORK wedge (the agent was doing its own thing and should resume), but actively wrong when the tail is a bare human message the agent never started answering: "carry on where you left off" points at the wrong place.

How to apply: any recovery path that re-drives a respawned agent with a synthetic prompt must condition that prompt on WHERE the interruption happened. A generic "continue" is safe only when there was in-progress agent work to continue; if the interruption sits on an unhandled human request, the recovery prompt must name that request or the fresh agent will mistake it for context. Observer nudges are excluded from the "unanswered human" detection deliberately — a nudge is the supervisor's own voice and self-heals (the supervisor re-fires on the next idle tick), unlike a human message which has no automatic retry. Guards: `store::tests::{tail_unanswered_user_detection, reconnect_on_unanswered_user_message_points_at_it}`. Coverage boundary (unchanged): the mock backend can't drive a real `reconnect_agent` resume, so the flag CAPTURE at the call site is exercised only via the pure helper + the direct `maybe_send_reconnect_continuation` test, not end-to-end.

### 46. Supervisor verdict tools are authenticated by a per-briefing nonce, and applying a verdict is the idempotency key

What: `supervisor_verdict` / `supervisor_audit_verdict` are solution-scoped MCP tools reachable by anything on the per-solution socket. Each judge/auditor spawn now mints a single-use nonce (`supervisor::new_verdict_nonce`, 32 chars / ~165 bits, same rejection-sampled alphabet as `SolutionSessionId`), stores it on the in-flight `JudgeHandle`, and bakes it into the briefing via the `{VERDICT_NONCE}` placeholder. The MCP handlers route through `SolutionAgentStore::apply_verdict_authenticated` / `apply_audit_verdict_authenticated`, which look up the in-flight `judge_sessions` / `auditor_sessions` entry for the target session and constant-time-compare its nonce (`verdict_nonce_matches`, with an empty-`expected` guard) before touching state. Three outcomes (`VerdictAuth`): `Applied` (nonce matched → the raw `apply_verdict`/`apply_audit_verdict` runs), `Unauthorized` (handle present, wrong nonce → MCP error, no mutation), `NoInFlight` (no handle → idempotent no-op returned as MCP success). Raw `apply_verdict`/`apply_audit_verdict` stay `pub(crate)` and un-authenticated for internal/test callers.

Why: unauthenticated, any socket client (including the worker itself) could forge a verdict for any session; and the `--nc` bridge exits on stdin EOF while the judge instructions say to retry on an empty reply, so a slow-but-successful call was re-sent and re-ran `apply_verdict` from scratch (a second nudge, a second continue-counter bump). Adding the tools to `GLOBAL_TOOLS` was NOT the fix — that would remove them from the per-solution socket the judge actually uses (global/scoped sets are disjoint, see #3-era socket split). The nonce is the only credential that reaches the ephemeral `claude` subprocess, because the briefing plaintext is its only side channel.

How to apply: the nonce doubles as the idempotency key precisely because applying a verdict ALWAYS reaps the handle first — `apply_verdict` calls `finish_judge` before the send-time-gate `drop_verdict` early-return, and `apply_audit_verdict` calls `finish_auditor` before the gated escalate — so any re-submit (bridge-EOF retry, or a stray late verdict from a torn-down judge) finds no handle and lands on `NoInFlight` rather than double-acting. Corollary: a late verdict racing out of a judge whose handle was already reaped (disable / prompt-change / user-reply supersede / watchdog timeout) is now a boundary no-op and is no longer logged with `dropped:true` (#2) — acceptable because nothing was acted on and `verdict_stats` already excluded dropped rows; only the forensic record of an already-undelivered race is lost. When adding any new socket-reachable tool that mutates supervised state, gate it the same way (in-flight handle + nonce), and keep the credential on the handle it authenticates rather than in a parallel map (single source of truth, reaped atomically). The two instruction `.md` resources tell the judge/auditor to echo `{VERDICT_NONCE}` and to read `recorded` / `unauthorized` / `no active … ignored` correctly (the last means DONE, do not re-send). Guards: `store::tests::{verdict_nonce_authenticates_and_dedups, audit_verdict_nonce_authenticates_and_dedups}` + the `briefing_substitutes_paths_and_custom_prompt` nonce assertion.

### 47. The stuck-tool watchdog gates on tool liveness, and display-only output recency IS the session silence clock

What: `tick_stuck_sessions` reconnects a `Running` session silent for `STUCK_TURN_SECS` (5min); to avoid killing a session merely blocked on a slow foreground command it holds off while a tool is `InProgress` until `TOOL_STUCK_SECS` (20min). That 20-min wall-clock alone still killed a legitimately long build/deploy. Now an in-progress tool past `TOOL_STUCK_SECS` is reconnected only when it also shows NO liveness: `shows_liveness = pty_running || silent_secs < TOOL_OUTPUT_SILENCE_SECS` (15min), where `pty_running = tc.terminals().any(|t| t.read(cx).is_process_running(cx))` (`acp_thread::Terminal::is_process_running` → the inner `terminal::Terminal`'s `task().status == Running`). The pure `turn_is_wedged(Option<(tool_secs, shows_liveness)>)` keeps the decision unit-testable.

Why: claude-acp streams a foreground command's output into a DISPLAY-ONLY terminal (no client-side OS process), and each `terminal_output` chunk rides a `ToolCallUpdate` — which `AcpThread::update_tool_call` emits `EntryUpdated` for unconditionally, and the store's `EntryUpdated` handler bumps `last_activity_at` from (the #5-era hardening). So for the display-only path `silent_secs` ALREADY measures "time since the command last printed" — no separate per-terminal output timestamp is needed (an earlier draft added a `last_output_at` field + `note_output` wiring; it was pure redundancy with `silent_secs` and, with its window == `STUCK_TURN_SECS`, dead code — removed). The one path `silent_secs` does NOT cover is a real client-side PTY (an ACP agent that uses `terminal/create`): its output flows through alacritty, not a `ToolCallUpdate`, so it never bumps `last_activity_at` — hence the direct `is_process_running` check.

How to apply: `TOOL_OUTPUT_SILENCE_SECS` MUST stay well above `STUCK_TURN_SECS`, or the display-only liveness branch can never fire (the candidate gate already required `silent_secs >= STUCK_TURN_SECS`, so a window equal to it is unreachable). Consequence/trade (accepted, this finding is Risky): a build that streams a lot then genuinely hangs is detected up to `TOOL_OUTPUT_SILENCE_SECS` later than the old flat 20-min wall; and a real-PTY process that is alive-but-deadlocked (running, zero output) is now never auto-reconnected — correct, since reconnecting would just kill in-flight work and re-run the command (the duplicate-build hazard #7 is about), and a live process is better surfaced to the human than silently re-run. When adding liveness logic, don't reach for a new signal that duplicates `last_activity_at` — check first whether the event you care about already bumps it. Guards: `store::tests::turn_wedged_decision_gates_on_tool_liveness` (pure decision, both sides of the `>=` bound) + `acp_thread::…::display_only_terminal_reports_no_running_process` (display-only never falsely reports a running process). Coverage boundary (unchanged): `MockConnection` can't drive a real `reconnect_agent`, so the full watchdog-fires-reconnect path stays untested end-to-end (see #45).

### 48. Background-shell staleness-reap is gated on PARENT-subprocess liveness, not just output silence

What: `tick_background_shells` reaps a `run_in_background` shell that has gone silent (its `.output` mtime, or `registered_at` if it never wrote) past `MANAGED_AGENT_STALE_TIMEOUT_SECS + MANAGED_AGENT_DEAD_LINGER_SECS` (420s). That flat timeout killed a legitimately long *silent* shell (a `sleep`, a quiet build) at 7min, dropping it from `background_shells` — which is exactly what the inline `has_live_background_work` predicate (supervisor idle-nudge gate + desktop-notification gate) reads, so the supervisor lost its "don't nudge while background work runs" suppression while the command was still executing. Now the Running-stale threshold depends on whether the owning session still has a live `acp_thread()`: alive → keep the shell up to `BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS` (60min) hard cap; gone → the ordinary 420s. Terminal (`Exited`/`Killed`) shells reap immediately regardless.

Why: output-silence is not death — the two can't be told apart from the output stream alone. The discriminator is the PARENT subprocess: a background shell runs inside the session's `claude` subprocess and its completion is announced by a `<task-notification>` line appended to that subprocess's JSONL, which `scan_parent_jsonls_for_completions` tails every tick (before the reaper) and turns into an immediate `Exited` reap. So while the parent subprocess is alive a completing shell is ALWAYS caught by that scan — a silent Running shell is therefore presumed still-running, not dead. The only way a shell leaks as a Running pill "forever" (the case the staleness arm was written to guard) is a subprocess that died WITHOUT emitting a notification — crash / restart / killed harness — and in every one of those the parent is gone too (no `acp_thread`), which is exactly the branch that keeps the 420s reap. The 60min cap still ages out a genuine runaway (a shell that never completes and never prints while its parent stays up).

How to apply: `acp_thread().is_some()` is the parent-liveness proxy — `set_acp_thread(None)` is set only by the reconnect cold-ize / cold restore, not by going Idle, so an Idle-waiting-on-background-work session correctly keeps its shells. Accepted trade (this finding is LOW): a shell whose subprocess died but whose `acp_thread` hasn't been cleared yet (unnoticed crash, or the ≤60s reconnect window) over-suppresses up to the cap instead of 7min — bounded, never a leak, and it degrades to exactly the pre-fix behavior. Don't reach for a wall-clock-only staleness signal when a liveness signal (here, the parent that would deliver the completion) exists. The `background_agents` twin had the structurally identical exposure and WAS left as-is, out of #9's scope — that gap was closed on 2026-07-13 (`da3dc1a9c9`): `tick_background_agents` AND the async arm of `reconcile_finished_teammate_streams` now take the same live-parent cap, so a detached agent grinding through a long silent tool call no longer has its pill reaped (and no longer lies to `has_live_background_work`). Guard: `silent_async_agent_with_live_parent_survives_below_cap`. Guards: `store::tests::{stale_running_shell_with_live_parent_survives_below_cap, stale_running_shell_reaped_when_parent_gone}` + the two repurposed cap-branch tests.

### 49. `store.rs` sub-state is split by partial-class SOURCE relocation, NOT by dependency-inversion into owned sub-objects

The god-object refactor (2026-07-10) reduced `SolutionAgentStore` (`store.rs`). The flat tool-catalog files and the view split cleanly, but `store.rs` itself is a **genuine coordinator, not flat bloat**: its methods spawn on `Context<Store>`, read `self.sessions`, and `cx.emit(SolutionAgentStoreEvent)`. Two mechanisms were tried:

- **Field-ownership extraction into a sub-object** (`ModelCatalog`, `TeammateWatchers`): moves only the *fields*; every method stays on Store because it's `&mut Store`-coupled. Net shrink was −9 / −21 lines — **marginal**. Kept for the encapsulated invariants (probe-dedup; forward-cursor/arm-once) but NOT worth chasing for the remaining sub-objects (PoolManager/ArchiveGc left undone).
- **Trait-seam dependency-inversion** (a `SupervisorEngine` owning the supervisor maps + calling back into Store via a `SupervisorHost` trait): **REJECTED**. Three GPUI walls make it impossible without turning the engine into its own `Entity`: (1) `store.engine.method(host = &mut store)` is an E0499 double-borrow, and `mem::take`-ing the engine out breaks methods that re-enter Store and re-read the taken-out maps; (2) the judge/auditor async continuations capture `WeakEntity<Store>` (`this.update(cx, |this| this.on_judge_failed(…))`) minutes later — a plain struct engine can't be that target; (3) only Store can `cx.emit` (the whole event-source/UI layer subscribes to Store). Making the engine an Entity reintroduces event routing + a `store ⇄ engine` cycle for zero glue reduction.
- **Partial-class SOURCE relocation — ACCEPTED and used.** The ~36-method supervisor/judge/auditor cluster (incl. the #5/#6/#7/#9 hardening in decisions #44–#48) was moved VERBATIM into `store/supervisor_engine.rs` as `impl SolutionAgentStore` blocks — the exact idiom as `store/queue.rs`. `self`/`Context<Self>`/fields unchanged: source text splits, state ownership does not. `store.rs` 10161 → 7998 (−2163). All 563 tests pass by construction (call paths stay `SolutionAgentStore::method`).

How to apply: to get orchestration-heavy code out of `store.rs`, **relocate the method bodies into a child `mod` of `store` (partial-class)** — do NOT try to make a sub-object *own* the orchestration. A GPUI `Entity`'s methods that spawn/emit/read-siblings cannot be lifted onto a plain sub-struct without an unresolvable borrow + async-target problem. True dependency inversion here would have to be driven by a *feature* need (e.g. multiple supervisor strategies), not god-object hygiene, and would cost making the sub-object an Entity. Full analysis: `docs/plans/2026-07-10-god-object-refactor-tier3.md`.

### 50. Solution / member / catalog ids are surrogate counters, not slugs

*Why:* the id used to be a slug of the display name **and** doubled as a path
component (`root = <settings.root>/<slug>`, `local_path = root/<catalog_id>`,
per-solution MCP socket at `<runtime>/solutions/<id>/mcp.sock`), so "rename"
literally meant "change the primary key". That is why rename silently degraded
to a label-only change and the on-disk folder drifted away from the name
forever. `SolutionId` / `MemberId` / `CatalogId` are now `pub struct X(pub i64)`
(`Copy`) backed by SQLite `INTEGER PRIMARY KEY` rowids; `name` and
`local_path` / `root` are ordinary mutable columns.

*How to apply:* address solutions and members by `SolutionId(i64)` /
`MemberId(i64)` everywhere — MCP tool params and wire DTOs included (they carry
raw `i64`, not strings). A member owns its own `name`; `origin_catalog_id` is
provenance only, so a tab/chip label is `member.name` and never a catalog
lookup with a slug fallback. Never `INSERT OR REPLACE` into a
table that is an FK parent with `ON DELETE CASCADE` (see
`docs/findings/2026-07-13-rename-solution-cascade-data-loss.md`). Note the one
resolution loss this brings: `Solution::last_opened_at` is now epoch **millis**
(`Option<i64>`), so two touches inside the same millisecond tie under a stable
sort — order-sensitive tests must advance the real wall clock, not a GPUI
virtual timer.

### 51. The editor owns a claude settings layer (`--settings <file>`), and the editor binary is its own worktree hook

*Why:* two claude defaults are keyed to the **git repo root** — i.e. the
*member*, not the Solution: agent worktrees land in `<member>/.claude/worktrees/`
(a member rename then breaks their absolute `gitdir` pointers, and the trees get
swept along by the member's `mv`), and auto memory is bucketed per repo. Both
belong to the Solution. `--setting-sources` can't express this (it accepts only
`user|project|local` — there is no way to name a directory of our own), so the
editor writes its own settings JSON and passes it as `--settings <file>`, which
sits at the *command-line* precedence tier: above local/project/user, and
**additive** to them (`--setting-sources user,project,local` stays). The file is
`<runtime>/solutions/<id>/claude-settings.json`, beside that Solution's MCP
socket. It carries a `WorktreeCreate`/`WorktreeRemove` hook pair pointing
worktrees at `<solution_root>/.agents/worktrees/<member>/<name>` and
`autoMemoryDirectory` → `<solution_root>/.agents/memory`. The hook command is
the **running editor binary itself** (`sawe --worktree-hook create|remove
--worktree-base <dir>`, an early-return in `main()` before GPUI init), mirroring
the `--nc` MCP bridge: no `jq` dependency, no shipped shell script that can
drift from the JSON we generate, and a dev build hooks the dev build.

*How to apply:* `--settings` overrides same-named keys **wholesale** ("keys you
omit keep their file-based values" — but `hooks` is one key), so
`claude_native::claude_settings` reads the `hooks` object out of the three
enabled sources (user / project / local) and re-emits them alongside ours;
never emit a bare `{"hooks": {ours}}` or you silently disable the user's hooks.
Resolve the exe with `std::env::current_exe()`, never a bare `sawe` on `$PATH`.
The remove hook refuses any path that doesn't canonicalize inside its base
(symlinks included), so legacy `<member>/.claude/worktrees/*` trees are left to
the rename reconcile's `git worktree repair`. `SAWE_CLAUDE_SETTINGS_DISABLED=1`
turns the whole layer off. Verified live: the subagent worktree landed at
`<solution_root>/.agents/worktrees/proj/agent-…` and auto memory in
`<solution_root>/.agents/memory/` — the settings doc's workspace-trust gate on
`autoMemoryDirectory` applies to project/local settings only, not to ours.

### 52. Renaming a Solution / member moves its folder in two halves: a hot `rename(2)` + symlink, and a cold reconcile at the next startup

*Why:* the on-disk folder is referenced from three places that cannot all be
fixed at the same instant. (a) **Live processes** — a `claude` subprocess, a
shell, an LSP server — hold the directory as an *inode* via their cwd, so a
same-filesystem `rename(2)` does not break them; they keep working in the moved
directory without noticing. That is what makes a hot move legal at all. A
cross-device move would have to copy + delete and would break them, so
`rename(2)` failing with `EXDEV` is a **hard error**, never a copy fallback.
(b) Those same processes also hold the old path as a *string* (transcript-bucket
names, `gitdir` pointers, absolute paths already in an agent's context). Nothing
can rewrite a string inside a running process, so the hot half drops a **compat
symlink** old → new; every stale string keeps resolving until the process dies.
(c) **Databases** — `workspaces`, `editors`, `terminals`, `breakpoints`,
`bookmarks`, `trusted_worktrees`, `toolchains`, `console_panel_state` in the app
DB, plus `solution_sessions` / background-agent JSONL paths in the agent DB —
are owned by subsystems (`WorkspaceDb`, `EditorDb`, `TerminalDb`) that hold
their rows in memory and write them back on shutdown, so rewriting them under a
live window would just be clobbered. So the DB rewrite is deferred: the hot half
queues a `pending_path_migrations` row, and `path_migrations::drain_and_apply`
runs in `SolutionStore::init_with_db` — **before any window opens** — rewriting
every path-bearing row, moving/merging the claude transcript bucket, repairing
git worktrees, removing the compat symlink, and deleting the row. It is
idempotent and crash-safe: a crash mid-drain leaves the row, and the next start
re-runs it.

The load-bearing subtlety inside the cold half: **`workspaces.paths` is the
workspace's identity key**, so it is rewritten **in place** (`UPDATE workspaces
SET paths = …, paths_order = … WHERE workspace_id = ?`). Delete-and-reinsert
would mint a new `workspace_id` and silently orphan every pane, tab, dock and
editor row that references the old one — the window would come back empty after
a rename, which is exactly the class of bug that started this work.

*How to apply:* never "fix up" a rename by re-inserting rows keyed on a path —
find the row by its integer key and `UPDATE` the path column (`toolchains` is
the exception: its key *is* the path, so stale rows are deleted, not rewritten).
The worktree self-heals (`ScanState::RootUpdated` → `update_abs_path_and_refresh`),
so a rename must NOT remove and recreate it. Folder names are derived from the
display name with **no transliteration** (Unicode is fine on every supported
filesystem) and a collision is a hard error, not a silent suffix. When you add a
new table that stores an absolute path, add it to `rewrite_app_db` /
`rewrite_agent_db` in `crates/solutions/src/path_migrations.rs` — a table that
is not listed there is silently orphaned by the next rename (see
`docs/findings/2026-07-13-rename-with-folder-move-shipped.md`). A table keyed by
a **hash of the repo path** rather than the path itself (`shelf_entries`,
`branch_favorites`, `branch_recent`, `pre_commit_configs`) cannot be rewritten
by prefix — it must be *re-keyed* instead: `remap_repo_hashed_tables` recomputes
`git::repo_hash(old) → git::repo_hash(new)` for every repo the move relocated.
`repo_hash` lives in one place (`crates/git/src/git.rs`) precisely so the reconcile
and the production writers hash identically; a second textual copy of that
`DefaultHasher` one-liner would compile and silently stop matching.

### 53. IDEA-style Find-in-Path modal is a bespoke `ModalView` in `crates/search`, not a re-shell of `ProjectSearchView`

What: `crates/search/src/find_in_path.rs` adds an IntelliJ-style "Find in Path" centered overlay modal (bound `ctrl-shift-f` / replace `ctrl-shift-r`) — input row + option toggles (case/word/regex) + scope tabs (**In Solution** / **In Project** / **Directory**) + file-mask fields, a grouped streaming results list with keyboard nav, and a live read-only preview editor pane showing the selected match with its surrounding context. It is its own `ModalView` entity, not the existing pane-tab `ProjectSearchView` reused inside a modal shell.

Why: `ProjectSearchView` is a full pane *tab* — results replace the editor's tab content, there's no split preview, and it can't be summoned as a transient centered overlay without a structural rewrite of a widely-used, stable component. IDEA-fidelity (results tree on one side, a live preview of the selected match on the other, no tab churn) needs a different shell. Crate placement stays `search` (not a new crate) because the modal reuses `search`'s crate-private input-row helpers, `SearchOptions`, and the `Project::search` streaming backend + replace machinery wholesale — only the presentation layer is new.

**Scope tabs map to include-pattern shaping on ONE `Project::search` call, not a fan-out.** A Sawe Solution is a single `project::Project` with each catalog member mounted as its own worktree (decision #2/#27), so there is no per-member `Project` to query separately: *In Solution* = empty include pattern (searches every worktree), *In Project* = `<active-member-root-name>/**` (derived from `SolutionStore::active_member`), *Directory* = `<typed-dir>/**`. An empty or unresolvable Directory path returns zero results rather than silently falling back to "search everything" (a `Regression: Fix empty/unresolved Directory scope silently matching everything` fix landed mid-branch — see commit `059122131d`) — a typo'd directory scope must fail closed, not widen.

**Replace / Replace All write straight to disk (`project.save_buffer`) instead of leaving dirty tabs.** The maintainer's editing model is IDEA-like: autosave is always on and **Local History** (a separate, not-yet-built feature) is the undo net, not per-buffer dirty state. Each affected file gets a transient `Editor::for_buffer` for the replace op; since `BufferStore` holds buffers weakly, an explicit save is required or the edit is lost the moment the transient editor drops.

**Coexistence, not replacement:** `ProjectSearchView` and its pane-tab UX stay fully intact — reachable from the modal via an "Open in Find Window" button (dispatches `workspace::DeploySearch`) and via `shift-find` (uses the deprecated keybinding pattern search already had). Users who want the old dedicated tab keep it.

How to apply: any future search-shaped modal (e.g. "Find Usages" if it ever needs a similar split view) should follow the same shell pattern — bespoke `ModalView` reusing the domain crate's backend, not shoehorning `ProjectSearchView`. Don't add a second Scope-tab-like concept without checking whether it can reduce to include-pattern shaping on the existing single-`Project` search the same way. Spec: `docs/superpowers/specs/2026-07-15-find-in-path-modal-design.md`.

### 54. Git-panel diff is IDEA-style single-file-at-a-time via the preview-tab slot, not the stacked accordion by default

What: selecting a changed file in the git panel now opens **only that file's** diff (`SoloDiffView`), not the all-files stacked multibuffer (`ProjectDiff`, "the accordion"). Single-click opens it as a **preview tab** (italic, occupies the pane's one preview slot, replaced by the next preview via `Pane::replace_preview_item_id`) and keeps focus in the panel; double-click / `Enter` (`menu::Confirm`) **pins** it (permanent tab, `unpreview_item_if_preview`, focus into the diff); ↑/↓ arrow-nav (`git_panel::{Previous,Next}Entry` → `move_diff_to_entry`) makes the preview **follow the selection** — but only when the pane's current preview item already is a `SoloDiffView` (never opens a diff from nothing). Threaded through a new `SoloDiffOpen { Preview, Permanent }` param on `SoloDiffView::open_or_focus`. Files: `crates/git_ui/src/{solo_diff_view.rs,git_panel.rs}`.

**Amended 2026-09-02 — the gesture description in the paragraph above is superseded by #136; read that first.** The half about *placement* still holds: one shared diff per pane, living in the preview slot, arrow-nav following the selection only when the slot already holds one. What changed is the mapping. **Nothing in the git panel pins any more** — not double click and not `Enter` — because pinning promotes the item out of the preview slot and the next single click then summons a second tab. Double click **summons** (opens if not open, never pins, focus stays in the panel); single click and arrow steps **retarget only** and do nothing when no shared diff is open; `Enter` summons *and* focuses, and still does not pin. `SoloDiffOpen { Preview, Permanent }` no longer exists — it is `DiffOpen { Summon { focus }, Retarget }`, which names the gesture rather than the destination. Everything below in this entry about `allow_preview` / `replace_preview_item_id` is still accurate and is still cited from `solo_diff_view.rs`; only the "pin" sentence in its "How to apply" is not.

Why: the maintainer wanted the IntelliJ Changes-view feel — pick a file, see just that file's diff, navigate the list with the diff tracking your selection — instead of scrolling one giant accordion buffer. The single-file view (`SoloDiffView`, a fork-local item) already existed but was bound to the *secondary* gesture; the whole change is making single-file the default and wiring it to Zed's existing preview-tab machinery (all the needed `Pane` methods — `replace_preview_item_id`, `unpreview_item_if_preview`, `add_item`, `preview_item_id` — are already `pub`, and `PreviewTabsSettings.enabled` gates it, default true). No `workspace`/`pane` or keymap edits were needed — the change is entirely in `git_ui`.

**The accordion is kept, demoted to a secondary gesture:** `ProjectDiff` opens via `alt-enter` / cmd-click (`menu::SecondaryConfirm` → `open_accordion_diff`), `ctrl-shift-d` (`git::Diff`), and the overflow/context-menu **"Open All Changes"** entries. `preserve_preview` is left at the default `false`, which *would* let a real text edit inside the diff promote the preview to permanent — **but as of #136 it cannot fire**: `SoloDiffView` never emits an `ItemEvent`, so `Pane::handle_item_edit` is never reached. That is load-bearing for the gesture model, not an oversight; see #136 and `TODO.md` C7.

How to apply: to add a custom `workspace::Item` view as a replaceable preview, don't look for an `allow_preview` param on `add_item` — there isn't one; call `pane.replace_preview_item_id(item.item_id(), window, cx)` (gated on `PreviewTabsSettings::get_global(cx).enabled`) to close+reuse the preview slot, pass its returned index as `add_item`'s `destination_index`, and open with `focus_item = false` to leave keyboard focus with the driver (the list). ~~A "pin" gesture skips the replace and calls `unpreview_item_if_preview`.~~ **Superseded 2026-09-02 (#136): the git panel has no pin gesture. Do not add one back** — the amendment above says why. Guard: `git_panel::tests::test_open_diff` (Confirm → one `SoloDiffView`, no `ProjectDiff`; SecondaryConfirm → `ProjectDiff` scrolled to the file). Plan: `docs/superpowers/plans/2026-07-15-idea-git-diff-preview.md`.

### 55. A commit's changed files render as an IDEA-style collapsible directory tree, with its own local tree builder (not the git-panel one)

What: the commit-detail "affected files" list (`crates/git_ui/src/commit_view/affected_files.rs`, shown in `CommitView`) changed from a flat list of full repo paths to a collapsible **directory tree**: folder rows (open/closed icon + name + muted "N file(s)" count) that toggle on click, file leaves with a status icon, single-child directory chains compacted onto one row (`src/main/java/ru/citeck/ecos/apps/domain`), and a root file shown at top level. Collapse state lives in `CommitAffectedFiles.collapsed_dirs: HashSet<String>` (default absent = expanded, matching IDEA); toggled via a `cx.listener` on `CommitView`. The existing fuzzy filter + lazy "Load more" window is preserved — the tree is built from the already-filtered/windowed `Vec<&CommitFile>` slice.

Why: the maintainer finds IDEA's git changed-files tree much nicer than a flat truncated-path list. The git panel already has a working-changes tree (`GitPanelViewMode::Tree`, `build_tree_entries`/`flatten_tree`), but that one is private to `git_panel.rs` and coupled to `GitStatusEntry` + `Section` + staging checkboxes — extracting/generalizing it risked destabilizing the heavily-used working-changes list for no real gain here. A read-only commit-file tree is a genuinely separate, smaller concern, so `affected_files.rs` grows its own ~90-line builder (`Node { dirs: BTreeMap, files }` → compact single-child chains → `flatten` to `TreeRow::{Dir,File}` honoring `collapsed_dirs`). Per-folder counts come from subtree file aggregation; indent uses a local `INDENT = 16.0` mirroring `git_panel::TREE_INDENT`.

Deliberately out of scope (there was no data / the ask didn't need it): per-file +/- counts (`git::repository::CommitFile` carries only `path`/`old_text`/`new_text`/`is_binary` — no numstat; adding them needs a diff-stat load path), click-a-file-to-scroll-the-commit-diff, and log-row branch/tag chips. The two tree builders (working-changes vs commit-detail) are intentionally NOT unified yet; if a third changed-files tree ever appears, that's the trigger to extract a shared `RepoPath`-keyed tree module. **Amendment (decision #100): that trigger has now fired and the extraction is deliberately deferred.** The git panel's Commit tab is the third tree — `git_panel/commit_tab.rs`'s `build_changed_file_rows` — and it was relocated wholesale from the git graph's sidebar rather than merged into either existing builder, because phase 3 had to leave `main` with a working commit-details surface at every intermediate commit. The three still differ in what a leaf *is* (a `GitStatusEntry` with staging semantics, a filtered/windowed `&CommitFile`, a `ChangedFileEntry`) and in what a row *does* (stage, scroll, open a per-commit diff), so the extraction is a real design job, not a move. Do it as its own change, not as a rider.

**Second amendment (recon 2026-08-31): the trigger is withdrawn, permanently — the third tree never arrived.** `commit_tab.rs`'s `build_changed_file_rows` is not a tree builder at all: no recursion, no `depth`, no compaction pass. It is a two-level `BTreeMap<full_dir_path, Vec<file>>` grouper, pinned by its own test (`test_build_changed_file_rows_groups_by_directory`), and its rendered structure differs from the other two *on purpose* — `docs/plans` and `docs/findings` are sibling flat headers rather than a nested `docs`, root files get a header named after the repository, and file rows use one fixed `COMMIT_TREE_INDENT` measured against the Changes tab's content edge instead of a per-level indent. Sharing a tree builder with it would be a UX change, not a refactor. It belongs with `branch_picker/tree.rs`'s `BranchTree::build`, the crate's other flat prefix grouper. If the maintainer ever wants the Commit tab to paint a real nested tree, that is a UX decision on its own merits — and only *then* does it become a consumer of the option below.

**Correcting this entry's original reasoning while we are here:** the `GitStatusEntry` / `Section` / staging coupling cited above as the reason not to generalise the git-panel builder is **render-side, not build-side**. `Section` appears in `build_tree_entries` only to namespace a `TreeKey`; the builder touches `GitStatusEntry` only for `.repo_path` and for cloning into a `Vec<_>`. Everything the entry names — the tri-state staging checkbox, `stage_status_for_directory`, folder icons, chevrons, indent guides — lives in `changes_list.rs`'s `render_directory_entry`. The proof is that `rollback_modal.rs` is an unaccounted-for **fourth** consumer that already reuses `TreeViewState::build_tree_entries` verbatim, passing a synthetic `Section`, with zero generics.

**Third amendment (2026-09-02): the per-file +/− carve-out above is withdrawn — see decision #127.** "No numstat, so it needs a diff-stat load path" stopped being true once the Commit tab computed its header total from `line_diff` over the commit's own `old_text`/`new_text`: the per-file figures fall out of that same fold and were simply being discarded.

**The remaining option, labelled and NOT scheduled:** the two *real* trees (`git_panel.rs`'s ~130 lines and `commit_view/affected_files.rs`'s ~72) are near-identical — same compaction predicate, same one-level-per-compacted-chain depth rule, same dirs-before-files ordering, same deepest-path collapse key. If they are ever unified, the route is **leaf adaptation** the way `rollback_modal` did it — adapt the leaf, keep one builder — never a generic `PathTree<L>`, which needs six knobs plus a lifetime across two call sites to save ~50 net lines. The template for the leaf conversion is `ChangedFileEntry::from_commit_file` in `commit_tab.rs`, which already derives a `FileStatus` from a `CommitFile`'s `old_text`/`new_text`; what does not exist anywhere yet is a `CommitFile` → `GitStatusEntry` conversion, and writing one is the actual work. Three costs keep it unscheduled: `build_tree_entries` is `&mut self` with side effects while `affected_files` rebuilds per frame, so reuse forces a caching refactor; the row-emission contract flips (`TreeViewState` keeps hidden rows and namespaces keys by `Section`); and `affected_files` has **zero** tests today, while the git-panel builder is exercised end-to-end by the nine `#[gpui::test]`s in `git_panel/changes_list.rs` plus nine more in `rollback_modal.rs` that build real trees through the same function — so the tests have to be written first or it is an unverified refactor of the surface being rewritten.

How to apply: when you need a directory tree over `RepoPath`-bearing leaves, this local builder is the lightweight template; only reach for extracting the git-panel one if the leaf really needs staging/section semantics. Plan: `docs/superpowers/plans/2026-07-15-idea-commit-changed-files-tree.md`.

### 56. Git-graph is tuned IDEA-tight, drops its hash column, and makes the search box find commits by hash

What: three coupled changes to the dedicated `crates/git_graph` panel (the columned Graph | Description | Date | Author log with colored lane lines), all in `git_graph.rs`:
1. **Tighter lanes.** `LANE_WIDTH` 16→10, `LEFT_PADDING` 12→8, `COMMIT_CIRCLE_RADIUS` 3.5→3.0 — the graph column (`graph_column_width` = `LANE_WIDTH * lanes + LEFT_PADDING*2`, min `MIN_GRAPH_LANES`=4) is now compact, so the description text sits close to the graph instead of being shoved right. Lane *assignment* is pixel-agnostic; only these x-mapping constants changed. Lines were already bezier-curved (`builder.curve_to` for merge/checkout) — untouched. **Superseded by #70** — the maintainer later compared against IDEA again and asked for the opposite: `LANE_WIDTH` is back to 16, the fixed graph column is gone, and the per-row indent this entry ruled out is exactly what shipped.
2. **No hash column.** The 4th table column (short SHA) is gone: `render_table_rows` drops the `short_sha` cell, header drops "Commit", `Table::new(4)`→`(3)`, and `RedistributableColumnsState::new(4, …)`→`(3, …)` with the freed width redistributed. The full/short SHA still lives in the commit **detail panel** on select (`# <short>`), so the hash is one click away, not column noise while scanning.
3. **Search-by-hash.** Removing the column would strand hash lookups (the search box only ever built `--grep=<text>`, which matches commit *messages* — a SHA matched nothing and the list hung empty). `update_query_filter` now detects a hex query ≥7 chars (`is_hash_like`) and, instead of grepping, resolves it against loaded commits by SHA prefix (`find_loaded_commit_by_prefix`) and `select_commit_by_sha` — jumping to + highlighting the matching commit (IDEA "find by hash"), list unfiltered. Non-hash text still greps messages.

Why: the maintainer compared our graph to IDEA's and wanted it visually tighter (reference screenshot: ~4 lanes packed into a narrow column with the message right beside them), the per-row hash column removed as scanning noise, but — explicitly — search-by-hash kept working. It didn't work before (pre-existing: `--grep` on a SHA), so "keep it working" meant "make it work."

Commit nodes are drawn as a fully-rounded `paint_quad` (`gpui::fill(bounds).corner_radii(Corners::all(radius))`), not a hand-built two-arc `PathBuilder` fill — the arc path rasterized blocky/"square" at the ~3px node radius; a quad with corner-radius = half its side is a reliable circle across GPU backends.

Limits (accepted): hash search resolves only among **loaded** commits — a SHA for a commit below the fetched window won't jump (no `git rev-parse` round-trip). The `>= 7` hex threshold is deliberate so 4-char hex words (`face`, `dead`) still search messages. Guard: `git_graph::tests::test_is_hash_like`. Plan: `docs/superpowers/plans/2026-07-15-idea-git-graph-polish.md`.

(This entry originally also recorded that per-row indentation was deliberately **not** done, on the reading that IDEA uses a fixed graph column. That reading was wrong — IDEA measures each row's own print elements — and #70 reverses it. Kept here rather than deleted because the *other* two changes in this entry still stand.)

### 57. Split-diff center strip is IDEA-style: the left pane's gutter is right-aligned so both number columns meet at the divider

> **Partly superseded by #72.** Right-aligning the left gutter was necessary but not sufficient: the two panes still *composed* their gutters differently, so the number columns did not actually meet. See #72 for the composition forcing and the mirroring that fixed it.

Why: the maintainer wants the split diff to read like IDEA's — a narrow central strip of `[left numbers][divider][right numbers]`, resizable by dragging the divider, with no debug affordances inside a review surface. Upstream's layout put the left pane's gutter at its left edge, leaving a bloated center (left scrollbar + right gutter with multibuffer padding) and breakpoint dots in a read-only diff.

How it works: `EditorElement` already carries `split_side: Option<SplitSide>`; when it is `Some(SplitSide::Left)` the gutter hitbox is placed at the pane's **right** edge (`element.rs::gutter_bounds(.., right_aligned)`) and the text hitbox starts at the pane origin. Everything that keys off `gutter_hitbox`/`text_hitbox` origins follows automatically; the handful of sites that *assumed* gutter-left (line-highlight x-offset, spacer-block origin, block content masks — see `text_area_mask_bounds`) now check where the gutter actually sits (`gutter_hitbox.left() <= hitbox.left()`). The lhs vertical scrollbar was already hidden (`split.rs`), so nothing collides at the divider. Breakpoints/bookmarks (and the hover "add breakpoint" dot) are disabled centrally in `SplittableEditor::new` — every diff view is a SplittableEditor consumer, so per-view opt-outs were removed. Diff toolbars are IDEA-ordered: change-nav ↑↓ first (in `SoloDiffStyleToolbar` for the git-panel solo diff; in `BufferSearchBar`'s PrimaryLeft multibuffer row for commit/project diffs), then viewer mode (unified/split), then the git/stat controls on the right; the diff views' hardcoded `breadcrumb_location = PrimaryLeft` now defers to the editor (hidden by default with `toolbar.breadcrumbs: false`). The split divider's grab zone is the whole 36px connector strip (see #58), double-click resets to 50/50.

Limits (accepted): IDEA's "ignore whitespace" and "highlight words" dropdowns are NOT implemented — whitespace-insensitive diffing needs `DiffOptions`/hunk-engine support (word-level highlighting already exists via `word_diff_enabled`, on by default). How to apply: any new drawing keyed off the gutter must not assume it sits at the pane's left — compare `gutter_hitbox.left()` to `hitbox.left()` like the existing sites.

### 58. A multibuffer's language settings come from its buffers only when they agree — soft wrap is not resolved per excerpt

Why: upstream resolved a multibuffer's `LanguageSettings` from the **first excerpt's** buffer and applied them to every row. In a diff that means whichever file sorts first decides, so a `README.md` (Markdown defaults to `"soft_wrap": "editor_width"`) made every TypeScript, Rust and JSON file in the same changeset soft wrap, while the identical diff without the Markdown file did not.

The obvious fix — resolve soft wrap *per excerpt* — was rejected as out of proportion to the bug. Wrap width is a single value for the whole display map: `WrapMap` holds one `Option<Pixels>`, `WrapSnapshot::update` wraps every row against that one width on a background thread, and `set_wrap_width` invalidates by comparing it. Per-excerpt wrapping means threading a row→buffer→width lookup (precomputed on the foreground, since resolving settings needs `&App`) through `update`, `interpolate`, the incremental `flush_edits` path and `check_invariants`, in the most performance-sensitive part of the editor, for every editor in the app. That is a display-map project, not a diff-readability fix.

How it works instead: `MultiBufferSnapshot::representative_buffer_id()` returns the first excerpt's buffer **only if every excerpted buffer resolves to the same language**; otherwise both `language_settings` functions fall back to `AllLanguageSettings::defaults`. A singleton or single-language multibuffer is unaffected (a Markdown-only diff still wraps); a mixed one gets the user's language-agnostic settings instead of an arbitrary file's.

Accepted tradeoff: in a **mixed** changeset the Markdown file no longer wraps either — the setting is off for everyone rather than on for everyone. That is why the toolbar toggle (`ToggleSoftWrap` button in `SoloDiffStyleToolbar` and `ProjectDiffToolbar`) ships with it: the whole-diff switch is now something the user can flip, and it is honest about being whole-diff. How to apply: anything that needs genuinely per-file behaviour in a multibuffer must use `language_settings_at(point)`, not `language_settings()`.
### 59. Diff-pane git blame is driven by a `BlameBaseSource` side-channel, and the backend gained a revision-aware blame

Why: the left pane of a split diff is built from `BufferDiff::base_text_buffer()` — a detached in-memory `language::Buffer` with no `File`, never registered in the project's buffer store. `GitStore::repository_and_path_for_buffer_id` therefore returns `None` for it, so `Editor::toggle_git_blame` silently produced nothing on that side. On top of that, `Repository::blame_buffer` shells out to `git blame --contents -` and always annotates against HEAD's history; the diff base is not necessarily HEAD (merge-base, a reviewed commit, a stash), so even with a repository in hand HEAD's answer can be the wrong one.

How it works, two halves:

- **Backend.** `GitRepository::blame_at_revision(path, revision)` runs `git blame --incremental <rev> -- <path>` with no piped content, so the result's line ranges describe the file *at that revision*. `GitStore::blame_path_at_revision` / `Project::blame_path_at_revision` expose it without needing an open buffer. Remote projects return `Ok(None)`: the `BlameBuffer` proto message is keyed by buffer id and has no revision field, so the honest answer is "no annotation" rather than HEAD's.
- **Association.** `GitBlame` consults `HashMap<BufferId, BlameBaseSource>` — `{repository, repo_path, revision}` — before falling back to the buffer's own repository. `Editor::set_blame_base_sources` owns the map (it survives blame being toggled off and on); `SplittableEditor::sync_lhs_for_paths` fills it from the *right-hand* buffer's repository once a consumer has declared the base revision via `set_lhs_blame_base`.

Rejected alternative: synthesising a `File` on the base-text buffer so the ordinary lookup would find it. That buffer is deliberately fileless — a `File` makes unrelated code (save, project-path resolution, worktree/status lookups, `PathKey::for_buffer`) treat it as a real project file. A side-channel that only the blame pipeline reads cannot leak that way, and it carries the revision, which a `File` could not.

How to apply: consumers **opt in**. `SplittableEditor` also backs `file_diff_view` / `text_diff_view` (two unrelated files) and `agent_diff` (a pre-agent snapshot), where the left text is not any revision of the right path — those get no left-pane blame. `project_diff` on `DiffBase::Head` and `solo_diff_view` opt in with `HEAD`; `DiffBase::Merge` deliberately does not, because `git blame` takes a single commit-ish and has no syntax for a merge base. `commit_view` is also unwired: *both* of its panes are detached buffers, so it needs base sources on the right too. Ordering trap: `sync_lhs_blame_sources` prunes entries whose buffer is no longer excerpted, so it must run **after** `update_path_excerpts`, never before.

Reachability: the gutter right-click menu gained "Annotate with Git Blame" / "Close Git Blame Annotations". Its breakpoint and bookmark sections are now gated on those affordances being enabled — diff panes disable both (#57), so the menu there previously offered nothing but dead entries. When no section qualifies, `gutter_context_menu` returns `None` and no menu is deployed.
### 60. Split-diff connector ribbons pair hunks by zipping the two panes' hunk lists, and each ribbon is exactly one GPUI path

Why: IDEA fills the strip between the two line-number columns with polygons tying each old block to the new block it became. Sawe had only a 1px divider there. `crates/editor/src/split_connectors.rs` widens it to a 36px strip (`CONNECTOR_STRIP_WIDTH`) and paints the ribbons into it with a `gpui::canvas`; the strip also hosts the resize handle and a `border_variant` line on each edge.

How it works: the two panes are separate `Editor`/`MultiBuffer` entities, so correspondence has to be established explicitly. `connector_rows` zips `lhs_snapshot.buffer_snapshot().diff_hunks()` with the rhs equivalent — the panes are already kept hunk-for-hunk in lockstep (`SplittableEditor::check_invariants` asserts the zip pairwise on `diff_base_byte_range`), so ordinal pairing is exact and O(hunks); pairing stops if a mid-edit frame catches the lists out of step. A pure insertion/deletion has an **empty** row range on one pane, and its anchor there resolves to the row *after* the `Block::Spacer` that keeps the two row counts equal — so the collapsed end is snapped to the other side's start row, which is where the gap actually begins. That is what gives a ribbon its flat top and sloped bottom.

**Trap:** each ribbon must be a *single* path. GPUI rasterizes every path of a batch into one shared screen-sized texture, then composites each path by drawing a quad over that path's **bounding box** and sampling the texture (`vs_path`/`fs_path` in `crates/gpui_wgpu/src/shaders.wgsl`). Two paths whose bounding boxes overlap therefore composite the shared coverage twice — a fill plus a separate outline stroke rendered as a ribbon with two different tints. Separately, the resize handle needs `deferred()` (the treatment `workspace::dock` gives its own handle): inside the strip it otherwise never wins the hit test and drag-to-resize silently stops working. It must **not** use `occlude()` for that, though — `occlude()` is `HitboxBehavior::BlockMouse`, which also makes `should_handle_scroll()` false, and since the strip is a flex sibling of the panes rather than an overlay on them, nothing behind it scrolls either. The strip therefore carries its own `on_scroll_wheel` that drives the rhs editor (the shared scroll anchor carries the lhs), and the handle uses `block_mouse_except_scroll()` so the strip can see the event at all. Without both halves, widening the divider from 1px to 36px carves a dead band out of the middle of the diff.

Limits (accepted): ribbons have no outline, because a border would need a second overlapping path. Endpoints are clamped to a few screen heights around the strip so a hunk spanning many screens never hands the tessellator absurd coordinates. Guard: `split::tests::test_connector_rows_pair_hunks_across_panes`.

### 61. Gutter indicators are placed by column, IntelliJ-style: breakpoints over the line number, runnables and bookmarks beside the fold chevron

Why: upstream reserves one shared three-character column left of the line numbers for runnables, breakpoints and bookmarks, and shows only the runnable when a row wants more than one. All three settings default to `true`, so every install pays for that column on every row of every singleton buffer, and a row can only ever show one of the three. IntelliJ instead puts the breakpoint dot *on* the line number (hiding it) and the run arrow next to the fold chevron, which costs no dedicated column and lets a row carry two indicators at once.

How it works: `Gutter::prepaint_button` (`element.rs`) is the single place that positions every gutter indicator, so the choice is one `GutterIndicatorColumn` argument threaded through `Gutter::layout_item{,_skipping_folds}` rather than four call-site hacks. `LineNumber` centres the element in `GutterDimensions::line_number_area()` (`left_padding .. width - right_padding`) and `Icon` in the new `indicator_column_width`, the leading slice of `right_padding` that precedes the fold column; each falls back to the old left-hand position when its column has no width, which is what keeps gutters with line numbers turned off unchanged. `prepaint_crease_toggles` starts at `fold_area_start()` instead of `width - right_padding` so the chevron cannot land on the indicator.

Two consequences worth knowing. **Line numbers.** A row that shows a breakpoint or the hover preview must not also paint its number, so `layout_line_numbers` takes a precomputed row set; building it forces the bookmark/breakpoint/hover-preview bookkeeping to be hoisted above the line-number layout, and it reuses `Gutter::renders_item_skipping_folds` so a row whose indicator is suppressed (folded, deleted, expand affordance) keeps its number. **Hover arming.** `mouse_moved` now requires the pointer to be inside the line-number span, since arming from anywhere in the row would drop the preview dot far from the cursor; that needed `GutterDimensions` on the `PositionMap`.

Sizing: the indicator column is three characters, matching the column it replaces, so the same buttons fit unchanged. It is reserved only for singleton buffers with line numbers — multibuffers keep their indicators on the left, in the four characters their expand-excerpt buttons pay for regardless, and fold toggles only ever render in singletons so there is nothing to sit beside elsewhere. When it *is* present it also absorbs the fold column's fourth character, which exists only to keep the numbers off the chevron.

Also here: a one-pixel `border_variant` rule on the gutter↔text boundary (`paint_background`), because `editor_gutter_background` equals the editor background in the default theme and diff hunk tinting floods across the seam. It is painted after the active-line and highlighted-row fills, which span the gutter and would otherwise cover it, and it goes on the gutter's **left** edge in a split diff's left pane, whose gutter is right-aligned against the divider (#57).

How to apply: a new gutter indicator picks a `GutterIndicatorColumn`; it does not compute an x. If it needs a column of its own, add the reservation to `EditorSnapshot::gutter_dimensions` in the same commit that places it — the fallback silently stacks new indicators on top of whatever else sits on the left.

### 62. Split-pane alignment reconciles one frame late, through a deferred companion notify — gpui discards invalidations raised during a draw

Why: the two panes of a split diff derive their `Block::Spacer` alignment from **each other's** wrap rows, and they lay out one after another inside a single frame. The pane that lays out first computes its spacers against the companion's *old* wrap width; the companion's later layout repairs the first pane's block map (`DisplayMap::snapshot` syncs both), but that pane has already laid out. So state converged and the painted frame did not, and dragging the split divider left the panes showing the same unchanged line on different rows until an unrelated redraw — a scroll — came along. Soft wrap is what makes it visible: without it, a width change cannot change a row count.

**The trap, and it is a general one:** the fix is *not* a `cx.notify()` at the point the wrap width changes. `Window::invalidate_view` (`crates/gpui/src/window.rs`) returns `false` and pushes **no** `Effect::Notify` when `draw_phase != DrawPhase::None` — an invalidation raised during layout or paint is dropped outright, not deferred. `Window::refresh()` is gated identically. A wrap width is only ever discovered from `EditorElement::prepaint`, so an inline notify there is delivered only by luck (whatever else happens to dirty the window that frame). `DisplayMap::set_wrap_width` / `set_font` therefore notify the companion through **`cx.defer`**, which lands in the effect cycle after the frame. Guard: `split::tests::test_split_panes_repaint_after_wrap_width_change`, which drives frames through the real `Window::draw` — `VisualTestContext::draw` never runs `record_entities_accessed`, so the panes are not live window entities there and the draw-phase gate does not apply at all, meaning a test on that path **cannot** tell an inline notify from a deferred one.

Cost: exactly one extra frame per real wrap-width or font change (frame N+1 sees both widths unchanged, so nothing notifies). Same-frame reconciliation was rejected: the dependency between the panes is mutual, gpui lays out top-down in one pass, and a pane's wrap width is not knowable until its own gutter/minimap/scrollbar geometry resolves inside its element — hoisting that into a parent would duplicate per-pane layout math to remove a sub-frame transient.

How to apply: anything in this codebase that discovers state during layout/paint and needs another view to react must `cx.defer` the notify. And when verifying such a fix, remember `workspace.screenshot` renders the *retained* scene — see `docs/findings/2026-08-17-gpui-draw-phase-invalidation.md`.

### 63. The before-pane insertion marker is derived from empty-range diff hunks, not from a flag on the spacer

Why: lines that exist only on the right leave the left pane with a neutral grey hatched gap, which says a gap is there but not that it is an insertion or where the block went in. IDEA draws a thin coloured rule at the insertion point.

The obvious route — tag `Block::Spacer` as "stands in for an insertion" — was rejected. Spacers come out of `compute_spacers`, which works on wrap-row deltas and an excerpt patch, not on diff hunks, so deciding insertion-ness there means re-deriving diff semantics inside the alignment machinery and threading a flag through every construction site, the `Debug` impl and `render_spacer_block` — which also does not know its `SplitSide`, so the flag alone would green-line the right pane's deletion spacers too. And a spacer is not evidence of an insertion in the first place: the same unchanged text wrapping onto more rows on the other side produces one as well, and that must stay grey.

How it works instead: `split_connectors::insertion_marker_rows` reads the pane's own snapshot. A diff hunk whose `row_range` is **empty** means "this pane has no lines here, the companion does" — exactly a pure insertion, and unambiguous. The hunk's display row is then snapped *up* over the spacer that stands in for it (`spacer.row + height == anchor.row`) so the rule lands on the gap's top edge instead of the following text row, falling back to the anchor row when alignment produced no gap. `EditorElement` paints it as a 2px `version_control_added` quad, gated on `split_side == Some(SplitSide::Left)`, right after `paint_spacer_blocks` so it sits on top of the hatch. It spans the whole pane including the gutter, matching the right pane's added hunk, which colours both its gutter strip and its line background.

A replacement — balanced or not — keeps its old lines in the deleted colour and is never marked. Guards: four tests in `split.rs`, including a fixture with three spacers (one insertion, two soft-wrap) that must yield exactly one marker.

### 64. Commit-detail state is keyed by sha, and the loaded diff carries the sha it was loaded for

Why: a commit arriving from outside the editor (a `claude` subprocess committing while the editor runs) pushes every row of the log down by one. `GitGraph::invalidate_state` cleared the commit vector but kept `selected_entry_idx` and the loaded diff, so the sidebar header re-read row N — now a different commit — while the file list and diffstat still described the previous one. Clicking a file then asked `CommitView` for a path the displayed commit never touched; `open_internal`'s `files.retain(|f| f.path == filter_path)` emptied the list, the population loop never ran, and the tab opened blank with the right title. It never self-healed, because `select_entry` returned early on an unchanged index.

How it works: the selection is re-anchored by **sha** (`pending_select_sha`) across an invalidation, not by row index, and `SelectedCommitDiff { sha, loaded }` tags the diff with the commit it was loaded for. `commit_detail_for_render` hands the renderer a commit and *only* the diff belonging to it, so a late-resolving load whose selection has moved on is dropped and the worst case degrades to an empty file list rather than a wrong one — a structural guarantee, not a promise that every future call site remembers to invalidate. `select_entry` re-loads when the held diff no longer matches the commit at that row.

Accepted tradeoff: on an external commit the sidebar now closes for the duration of the log refetch and is re-established by sha afterwards, instead of showing a populated-but-wrong panel. A visible blink, deliberately preferred to wrong data.

How to apply: index-based identity in the commit graph is only valid within one `graph_data` generation. Anything that outlives a refetch — a selection, a cached diff, an in-flight task's result — must be keyed by sha, and every consumer of a view index goes through `view_to_data_idx` (the header did not, and was off by one whenever the local-changes row was present).

### 65. Restart Agent respawns the subprocess and resumes the SAME session — it never mints a new one

Why: Restart used to close the session and create a blank one, which is what the "+" button already does. The maintainer's words: *"для новой сессии я могу просто через + создать. Рестарт должен делать рестарт существующей с сохранением истории."* A restart that quietly throws away the conversation is the trap that prompted the change.

How it works: `SolutionAgentStore::respawn_agent(session_id, reason)` is the single implementation, reached from both `restart_agent` (`RespawnReason::UserRestart`) and the stuck-session watchdog's `reconnect_agent` (`RespawnReason::Watchdog`) — the mechanics are identical, only the wording differs. It reuses the existing `resume_session` machinery (full replay via `load_session`, falling back to `--resume <acp_session_id>`) after detaching the live `acp_thread`, so `resume_session`'s "already hot" early return does not short-circuit it. The `SolutionSessionId`, the tab, the title, the entries and the pending message queue all survive; the user's own queued messages are explicitly preserved, because dropping them without asking is a bug this fork already fixed once.

`RespawnReason` exists because the two paths must not lie to anyone. It carries `transient_status`, `failure_status`, `recovered_note` **and** `continuation_prompt` — the last one matters most: the continuation is read by the *model*, and telling it "your process hung" after a restart the user asked for sends it hunting for a fault that never happened. On resume failure the session is left as it was with the error surfaced (variant A); it never silently degrades into a blank session.

How to apply: any new respawn entry point takes a `RespawnReason` and adds an arm to every one of its four strings — a wrong arm is invisible in tests unless both variants are driven, which is why `restart_and_watchdog_respawn_report_differently` exercises `restart_agent` *and* `reconnect_agent`. And the MCP tool docs are a contract agents read: `ResetContextParams`' description contrasts itself with `restart_agent`, so it has to be updated in the same commit as any change to what restart does.

### 66. `CLAUDE_CODE_HARBOR_KITE=1` is forced for every spawned `claude`, because cross-session messaging is otherwise a per-process coin toss

Why: upstream gates the *entire* cross-session messaging feature behind the remote flag `tengu_harbor_kite` (default `false`), re-asked on **every process start** — and `/clear`, `/compact` and Restart Agent each spawn a new process, because `--session-id` is a launch argument. So a session's ability to be reached by its peers was decided afresh on every respawn, and its absence is invisible: no socket is bound under `/run/user/<uid>/cc-socks/<pid>.sock`, the session silently vanishes from peers' `ListAgents`, and inbound messages are refused with an internal cause literally named `"kill-switch"`.

How it works: `claude_native::command` sets the env var next to the existing `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS`. It is the override upstream provides beside the flag lookup, not a reach into internals. Worst case if a future CLI renames it: the override stops applying and we are back to today's coin toss — no worse.

Diagnostic note worth keeping: a session that claims "I'm headless so I don't register" is **wrong** — a headless `stream-json` session does register. Check `/run/user/<uid>/cc-socks/<pid>.sock` and the process's `environ` instead of accepting that explanation. (Those socket files are never cleaned up; a majority belonging to dead pids is normal and harmless.)

### 67. The file finder is member-scoped by default, and the *repeat-keystroke* scope toggle is a keymap fact, not an action payload

Why: a Sawe window hosts a whole Solution and mounts every member project as a worktree of one `project::Project`, so an unrestricted fuzzy search spans all of them — usually noise. IDEA's Search Everywhere solves this with a scope selector that starts at `Project Files`, and `ctrl-shift-n` is bound to the finder here precisely for IDEA parity, so the finder now opens scoped to the Solution's *active member*.

The non-obvious part is how the repeat keystroke is differentiated. `ctrl-shift-n`, `ctrl-p` and `ctrl-e` are all bound to the same `file_finder::Toggle` action, and a repeat of that action while the finder is open calls `Picker::cycle_selection` — that is the load-bearing hold-`ctrl`-and-tap quick switcher (`FileFinder::handle_modifiers_changed` confirms on modifier release). Making the repeat toggle the scope for *all* of them would destroy it. The obvious fix — a discriminating field on `ToggleFileFinder` — is wrong too: the action lives in `workspace`, which cannot depend on `file_finder`, and the payload would then have to be threaded through every open path. Instead a separate `file_finder::ToggleScope` action is bound to `ctrl-shift-n` **inside** the `FileFinder` key context (`assets/keymaps/default-linux.json`), where the more specific context shadows the workspace-level binding. `ctrl-p` / `ctrl-e` keep cycling untouched, and nothing about the action payloads changes.

How to apply: the scope itself is delegate state (`FileFinderDelegate::scope` + `active_member_worktree`, resolved once at open through `SolutionStore::active_member_worktree`), reset to `ActiveProject` on every fresh open — it is a per-search refinement, not a preference, and a sticky "Everywhere" would silently change what the binding does days later. It is inert (and its header control unrendered) when the window has no multi-member Solution, in which case `ToggleScope` falls back to `cycle_selection` so the binding stays a strict superset of the old behaviour. Path labels are deliberately *not* re-derived per scope (`should_hide_root_in_entry_path` still counts all visible worktrees), so flipping the scope filters the list without reshuffling every row's text.

### 68. A rejected push stays *in* the push dialog, with git's verbatim stderr and the remediations beside it

Why: `git::Push` opens `git_ui::push_dialog::PushDialog` (decision #24's S-PSH surface), not the git panel's `GitPanel::push`. The panel path toasts failures through `show_error_toast`; the dialog path did not — `confirm_push` matched `Err(err) => { log::warn!(…); cx.notify(); }`, so a non-fast-forward rejection cleared the `pushing` flag, flipped the button from "Pushing…" back to "Push", and left no trace in the UI at all. `run_push_cli` already carried git's stderr in the `anyhow` error (`git push failed: {stderr}`); the loss was purely at the UI boundary.

Routing it to a toast would have been the smaller change and is the wrong one: the remediations for a rejection (pull, re-lease) need the dialog's already-resolved branch / remote / remote-branch / force-mode state, and the user's next action after remediating is to press Push again — which is in the dialog. So the dialog keeps itself open and renders a `PushFailure { kind, detail }` block between body and footer.

How to apply:
- `git::push_rejection::PushRejection::classify(stderr)` is the shared classifier (`crates/git/src/push_rejection.rs`), usable from the panel path too. It matches `stale info` **before** the generic non-fast-forward patterns — a rejected `--force-with-lease` also prints `! [rejected]`, and retrying the same lease fails until a fetch.
- The classification only chooses **which buttons to offer** (`is_diverged()` gates them; hook-declined and auth failures deliberately offer none, since pulling is a dead end there). It never replaces or paraphrases the message — the verbatim `{err:#}` chain is always rendered in a scrollable buffer-font block with a Copy button. Keep that invariant: git's wording is the only thing that reliably explains a server-side refusal.
- Remediation pulls go through `Repository::pull` (the typed git-store API the panel uses), not a `run_git` shell-out, so askpass, the job queue and the store's own refresh behave identically. That is the one place the dialog does *not* bypass the git store — `run_push_cli` still does, for the reason recorded on `PushDialog::repository`.
- "Force push with lease" re-enters `confirm_push` with `ForceMode::WithLease` rather than calling `run_force_with_lease` directly, so the S-SOL-PRT branch-protection check at the press boundary still runs. Only `--force-with-lease` is ever offered as a remediation — see #74, which removed the bare `--force` from the tree entirely.
- Keymap: `ctrl-shift-k` is `git::Push` in the `Workspace` block of `assets/keymaps/default-linux.json` (IDEA parity). It displaced `editor::DeleteLine`, which moved to IDEA's `ctrl-y`; `editor::Redo` keeps `ctrl-shift-z` and the `redo` key. macOS/Windows defaults were left alone.

### 69. The commit menu's branch submenu is a transcription of IDEA's, and it lists remote-tracking refs

Why: S-CTM's "Branches at This Commit" section originally filtered remote-tracking refs out — the comment in `git_ui::commit_context_menu` argued that acting on them from a commit row would be surprising — and gave local branches a three-entry submenu (Checkout / Merge / Delete). Both were wrong against the reference: the tip of `origin/master` on a repo where nothing is merged locally carries *only* remote decorations, so the whole section vanished exactly when the row's ref chips most needed explaining; and IDEA's own `Branch '<name>'` submenu is ~14 rows deep. The submenu is now a row-for-row transcription of IDEA's (same entries, order, separators and `'name'` interpolation), and remote refs are listed beside local ones with `IconName::Screen`, the icon the branch picker already uses for remote entries.

How to apply:
- **The current branch is the only ref filtered out.** Every other decoration gets a submenu, local or remote. (`<remote>/HEAD` is skipped as a *classification* matter — it is a symbolic ref for the remote's default branch, not a branch.)
- **Classification is a repository-ref lookup, never a slash count.** `%D` spells local branches bare and remote-tracking refs as `<remote>/<branch>`, and a GitFlow local branch (`feature/FOO`) is slash-bearing too. Known local branches win first; then the **longest** configured remote name that prefixes the token (`git remote add team/fork …` is legal, so first-slash splitting can name a remote that doesn't exist). Remote names come from `Repository::get_remotes`, which is async, so `GitGraph` prefetches them once at view init into `remote_names`, mirroring `local_user_email`. A token that can't be split against a known remote is still listed, but every row that needs a remote name is withheld — guessing means pushing, pulling or deleting on the wrong one.
- **The entry list is planned, then rendered.** `plan_branch_submenu` is a pure function returning `Vec<SubmenuRow>` (label + action + optional "unavailable" reason); `build_branch_ref_submenu` only attaches handlers. That is what makes the IDEA layout — including which rows are disabled and why — assertable in a unit test instead of only visible in a screenshot. Add rows to the plan, not to the renderer.
- **Rows this fork can't back are disabled with the reason on their info aside, never dropped.** `ContextMenu` supports `disabled(true) + documentation_aside` (it renders an info icon), and IDEA itself greys rows out — its protected-branch `Delete` is why the reference screenshot shows a disabled row. Currently unavailable: *Compare with '\<head\>'* (needs a commit-vs-commit diff; `ProjectDiff` only diffs the working tree against one base ref) and *Update* on a non-current branch (needs `git fetch <remote> <branch>:<branch>`; the fetch API takes no refspec).
- **Checkout of a remote ref asks before stranding local commits.** `change_branch` on `origin/master` creates or re-points local `master` with `--track` and checks *that* out, so when `master` is ahead of the upstream, a silent checkout hides commits from someone who thinks they just synced. `checkout_divergence` reads the snapshot's `Branch::upstream` tracking status (no extra git call) and, when the local branch tracks this exact ref and is ahead, raises IDEA's three-button modal: **Rebase onto Remote** (default) → checkout + `handlers::rebase`; **Drop Local Commits** → `protection::enforce(…, "reset_hard", …)` then checkout + `reset --hard`; Cancel. Answer order is load-bearing — the destructive option must never be what Enter does.
- **Two deletes, and the destructive one reuses `Repository::push`.** Local `Delete` is `git branch -d` with the existing force escape hatch. Remote `Delete` is the real server-side delete, spelled as a push with an **empty source refspec** (`git push <remote> :<branch>`) rather than a new `--delete` API: `Repository::push` is the only path that already carries the askpass delegate, the collab proxying and the post-push branch rescan, and a `git push --delete` spawned directly would block forever the first time a credential helper wanted input. Both are gated by `protection::enforce(work_dir, branch, "delete_branch", …)` at *menu-build* time, so a protected branch shows the row disabled with the policy's own wording instead of failing after the click. A successful server-side delete then drops the now-dangling local tracking ref (`git branch -dr`), because otherwise the ref chip stays painted on the row until someone runs a pruning fetch and the delete reads as having failed.

### 70. The git graph is painted as a hitbox-free overlay over the Description column, and every row is indented by its own lane extent

Why: #56 fixed the graph column at `clamp(max_lanes, 4, 12)` lanes and argued that per-row indentation was unnecessary because IDEA uses a fixed column. Both halves were wrong. The clamp silently **clipped** lanes 13+ — a busy history lost edges with no indication — and IDEA does not use a fixed column at all: `GraphCommitCellRenderer` measures each row's own print elements, so a linear stretch pulls the subject text left and a merge storm pushes it right. The maintainer's reference screenshots show exactly that, including refs sitting far left on the rows where the DAG narrows.

How to apply:
- **Width is capped against the viewport, not against a lane count.** `graph_column_width_for(lanes, available)` grows with `max_lanes` and is capped at `MAX_GRAPH_WIDTH_FRACTION` (0.4) of the **Description column's measured width** — not the whole log, so the graph can never spill over Date/Author even when the divider is dragged. `available == 0` (first frame, unmeasured) means uncapped natural width; the measurement is one frame stale by design, because feeding it back into the container width would require notifying from the draw phase, which gpui discards (#62).
- **Per-row indent lives on the row, not on the column.** `GraphData::max_column_at_row` is filled incrementally in `add_commits` where `max_lanes` is updated, and must count the commit's own lane, the lanes still live after the row, *and* any lane that terminates on the row. `graph_row_extent` floors it at `MIN_GRAPH_LANES` so a linear stretch does not slam the subject against the left edge or jitter on every 1↔2 lane change. The floor lives inside `graph_row_extent`, not at the call sites, so no caller can forget it.
- **The canvas is an absolutely-positioned overlay with no hitbox, painted last.** It has no `id`, no listeners and no tooltip, so `should_insert_hitbox` is false and it registers nothing: hover, click, double-click and the context menu all belong to the table row underneath, which is how the graph region gained a context menu it never had as a separate column. If you ever add a listener or an `id` to that element, every row interaction under the graph dies silently — the row still paints, it just stops responding.
- **There is exactly one painter for the row background.** The canvas no longer draws hover/selection quads; the (now full-width) table row does. Two painters meant a seam at the column boundary and a double-blend of the translucent hover colour.
- Horizontal scrolling is still absent, so a history wider than the 40% cap is still clipped — the cap only makes that far rarer than 12 lanes did.

### 71. Closing a Solution is a COLD close, and desktop and wire share one seam

Why: the desktop title-bar tab strip closed a Solution by looping `SolutionAgentStore::close_session` over every live session. That is the **permanent per-tab archive**: it stamps `closed_at` and emits `SessionClosed`, which cascades `ChatProvider` → `ConsolePanel::close_chat_tab_by_session_id` → `ConsolePanel::persist`, and `persist` both rewrites `console_panel_state` without the chat rows and NULLs `tab_order` for every session of the solution. All three restore predicates (`select_open_tabs`, `select_open_session_ids`, and the panel's own row set) then fail, so reopening the Solution showed an empty AI tab strip. The MCP path had always done the right thing; only the desktop path diverged, and nothing made them share code.

How to apply:
- **`workspace_events::close_solution_runtime` is the single seam.** Both `workspace.close_solution` (the wire tool) and `solutions_ui::close_solution` (the tab strip) call it. It cancels in-flight agent turns, then calls `SolutionStore::mark_closed`, whose `SolutionStoreEvent::Closed` `SolutionAgentStore` maps to `cold_close_solution` — memory evicted, pooled `claude` subprocess released, `closed_at` and `tab_order` untouched. Never reintroduce a `close_session` loop on a solution-close path: that primitive is for the user closing *one tab*.
- **Order is load-bearing.** It must run *before* the workspace teardown, because `cold_close_solution` reads `by_solution` and flushes each transcript off the live entities. Running it after the panes are gone makes it a no-op on an already-emptied map — which is precisely how the original bug hid: cold-hydrated sessions are inserted into `store.sessions` but deliberately **not** into `by_solution` (`hydration.rs`), so `sessions_for()` returns only the sessions the user actually interacted with this run, and only those got archived. A session you never touched survived; the ones you worked in vanished.
- **A partial restore must not commit its own loss.** `ConsolePanel::restore_from_db` used to call `persist()` unconditionally at the end. Since `persist` is a destructive DELETE+INSERT of the whole row set, one bad boot — a session not yet hydrated, a workspace whose active solution had not resolved, a window closed mid-restore — permanently erased the rows it had just failed to read. The reconciliation now runs only when every persisted row came back; otherwise the DB is left exactly as it was and the next good boot reconciles.

### 72. The split diff's two panes are composed identically and mirrored about the divider

Why: #57 records that the left pane's gutter is right-aligned so both number columns meet at the divider, and claims that made the panes agree. It did not. `EditorSnapshot::gutter_dimensions` chose its composition from `buffer_snapshot().is_singleton()`, and a split diff's **left** pane is always a synthesized multi-buffer of base text (`SplittableEditor::split` builds it with `MultiBuffer::without_headers`) while the **right** pane is whatever the consumer handed over — a genuine `MultiBuffer::singleton` for the git panel's solo file diff. Same function, two different answers: 4ch vs 2ch left padding, no indicator column vs 3ch, 1ch vs 3ch fold column. Measured 71px against 98px. `Editor::disable_runnables()` leaked separately: it set `enable_runnables = false` but `gutter_dimensions` sizes the indicator column from `show_runnables`, so a read-only diff reserved three characters for a run arrow it can never draw.

How to apply:
- **`Editor::split_side` (owned by `SplittableEditor`) forces the multi-buffer composition for any split pane**, so the two sides can no longer diverge on what the buffer happens to be. Do not reintroduce an `is_singleton` branch in gutter composition — that predicate describes the *buffer*, and in a split diff the buffer is an implementation detail of which side you are on.
- **Width parity alone would have made it worse.** The left pane's gutter is right-aligned (#57) but its *contents* were not mirrored, so `left_padding` (git strip, blame, expand-excerpt buttons) sat next to the code on the left and next to the divider on the right. Equalising the widths without mirroring would have grown that visible gap from 27px to 44px. `GutterDimensions::mirrored` is set for the right pane only, and the x-positioning sites go through `line_number_area()` / `content_area_start()` / `indicator_column_area()` / `fold_area_start()` instead of measuring from the gutter's raw left edge.
- **`split_side` is stored once, and composition and alignment read the same copy.** It briefly lived on both `Editor` (composition) and `EditorElement` (physical alignment), agreeing only by convention — a render path that built an `EditorElement` for a split pane without calling `set_split_side()` got correct dimensions and wrong alignment, silently. No second copy was ever needed: `EditorSnapshot` already carries the field, and it is the same field `gutter_dimensions` composes from, so every alignment site reads it off the snapshot it already holds (`Gutter` borrows one, `PositionMap` owns one). `EditorElement::new` deliberately takes no side and there is no setter to forget. Guard: `split::tests::test_split_pane_element_derives_its_side_from_the_editor`, which renders through the bare `EditorElement::new` path.
- **Right-click had a dead band.** `mouse_right_down` excluded the whole trailing `right_padding` of the gutter and then returned *unconditionally*, so a click there produced no gutter menu **and** never fell through to the text menu — 54px of nothing between the numbers and the code on a singleton pane. It now excludes only `indicator_column_area()`, whose icons carry their own handlers. This also fixed ordinary singleton editors, where the same band was eating right-clicks.
- Fold toggles are suppressed in split panes on purpose: the left pane cannot fold, so keeping them on the right would re-create the asymmetry.

### 73. The git panel has exactly one source of truth for which rows are visible

Why: collapsible `Tracked` / `Untracked` headers could not be layered onto the old model. `TreeViewState::logical_indices` only existed in tree mode, so flat mode had no notion of "which rows are currently on screen" at all, and the selection walkers indexed a mix of the two. Two latent bugs fell out of the same confusion once it was untangled: `select_first` indexed `logical_indices` with an `entries` index, and `select_last` could land on a row that was not visible.

How to apply:
- **`visible_indices` is panel-level and maintained in BOTH view modes**, built in `update_visible_entries`. Every selection walker (`select_next` / `select_previous` / `select_first` / `select_last`) navigates it, never `entries` directly. If you add a new way to hide a row, it belongs here and nowhere else.
- **Hiding a row is responsible for relocating the selection.** `toggle_section` / `toggle_directory` call `clamp_selection_to_visible`, which moves a now-hidden selection to the nearest *preceding* visible row — i.e. the header or directory that swallowed it. Leaving that to the caller worked only because every existing call site happened to pre-point the selection at the row being toggled; a caller that collapsed a section while the selection sat on a descendant would have left `visible_position` returning `None` forever, and since `select_next` / `select_previous` early-return on `None`, arrow-key navigation would freeze with no panic and no failing test.
- **Every row kind is exactly one `list_item_height` tall, and uses a plain chevron `Icon`, never `ui::Disclosure`.** `uniform_list` sizes every row from the first one, so a taller header or directory row is clipped rather than laid out.

### 74. A bare `git push --force` cannot be constructed anywhere in this fork

Why: `--force` overwrites the remote whatever it holds; `--force-with-lease` refuses when the remote moved since your last fetch, which is exactly the accident the bare flag performs silently. The fork offered both, in two places, with the bare flag one checkbox away.

How to apply:
- **Every force posture emits `--force-with-lease`.** `git_ui`'s push dialog, its MCP tool, and `solution_git`'s Solution-wide push panel and its `solution.git.push_all` tool all route through the leased runner. `ForceMode::Force` survives *only* as a legacy wire input so a caller that still spells `force_mode: "force"` can be recognised and told its request was upgraded — never so it can be executed. Do not reintroduce a `--force` arm in an argument builder; the guard test is `solution_git`'s `no_force_mode_can_build_a_bare_force_argv`, which walks every posture × pinned/unpinned sha.
- **A force push is refused outright, not merely confirmed, unless it is both meaningful and legible.** The dialog resolves the commits a force push would destroy (`git log <branch>..<remote>/<remote_branch>`, **with** merges — `preview.behind` is `--no-merges` filtered, which is right for "what am I sending" and wrong for "what am I destroying"). Empty set → refused as pointless, because forcing can then only differ from an ordinary push if our reading of the state is wrong. Unresolvable set → refused as dangerous; a dialog that cannot read what it would overwrite must never render an empty list, which the user would read as "nothing will be lost".
- **When it is offered, it shows what it destroys and waits.** The commit rows carry subject, date and author, and the confirm control is absent — not disabled — in either refusal state, so no countdown ever runs toward an action that cannot happen. In the offered state a 5-second armed countdown blocks a reflex click; Cancel and Escape stay live throughout. The state transition is monotonic: a late remote read must not restart a countdown mid-wait or reopen an already-refused push.
- **A protection reason must never name a gesture this fork lacks.** `branch_protection`'s `RequiresConfirmation` strings used to end with "confirm by typing the branch name" — a flow that was only ever a TODO comment. The fork answers that tier with a plain confirmation naming the branch, the remote and the exact command.

Known gap: `crates/agent/src/tool_permissions.rs` still classifies a literal `git push --force` *shell* command as confirm-rather-than-deny, so an agent can type one into a terminal. That is the Bash permission layer, not the push code.

### 75. One deterministic member→repository resolution, and the header owns the choice

Why: a Solution mounts N members as worktrees of ONE `Project`, so `Project::active_repository` follows the last-focused buffer rather than the member selected in the tab strip. Six surfaces each carried their own copy of the workaround, and every copy was the same line: `repositories().values().find(|repo| repo.work_directory_abs_path.starts_with(&member.local_path))`. That is wrong twice. `repositories()` is a `HashMap`, so iteration order is arbitrary; and `starts_with` matches a repository nested *inside* the member's worktree as readily as the member's own — a vendored plugin with its own `.git` is a first-class `Repository`, because the worktree scanner rejects only a `.git` inside a `.git`. Two matches, arbitrary winner, six independent draws: the maintainer had the title bar showing a detached-HEAD sha from a vendored plugin, the Changes panel footer naming that plugin, and the log showing a third member entirely — all at once, all "correct" by their own lookup.

How to apply:
- **`solutions::member_repository` is the only resolver.** `active_member_repositories` returns the member's repositories **outermost first** (fewest path components, then lexicographic), so the default pick is deterministic and is the member's own repository. `active_member_repository` returns the user's explicit per-member pick when it is still present, else that default, else `None` — and `None` means "not a Solution", which is the caller's cue to fall back to `Project::active_repository`. Do not re-derive any of this at a call site; add the call site to the module.
- **The explicit pick is keyed by `(SolutionId, MemberId)` → work-directory path, not by `RepositoryId`.** Ids are minted fresh on every rescan; the path is stable.
- **Choosing also sets the project-wide active repository.** `set_active_member_repository` writes the pick *and* calls `set_as_active_repository`, so surfaces that only know `GitStore` follow along and everyone re-renders off the existing `ActiveRepositoryChanged` event rather than a new one. The reverse is deliberately not true: `set_active_repo_for_path` on buffer focus still moves the global, but it cannot move a member that has an explicit pick — which is what stops the header drifting as you click through files.
- **The repository selector lives in the header, not in a panel.** `ProjectToolbar::render_repository_selector` renders to the LEFT of the branch widget and **only** when the active member owns ≥2 repositories; with one there is nothing to choose and nothing renders — no placeholder, no disabled button. Its appearance is itself the signal that a nested repository got registered. The git panel's footer repo switcher was removed: two switchers that could disagree is the bug this decision closes. `repository_selector.rs`'s `SelectRepo` modal stays.
- **A detached HEAD is a legitimate state, not a fallback.** The title bar's short-sha branch label and the git log both handle it; they were only ever showing the *wrong repository's* detached HEAD, and a log with no branch reads from `LogSource::Sha(head)`. The general lesson outlived the surface that taught it: **every piece of panel state that is about one repository must be dropped at `GitPanel::set_active_repository`, which is the single seam.** The git panel's History tab (deleted in decision #100) cleared its rows and subscriptions there; the Commit tab is closed there now, for exactly the same reason.

### 76. `LogSource::Sha` had never worked

Why: `LogSource::get_args` built the argument with `str::from_utf8(oid.as_bytes())`, and `Oid::as_bytes` is the raw 20/32-byte digest, not text. For virtually every sha that is not valid UTF-8, so the arm returned an error and `git log` was never reached; on the rare sha that did decode, git received binary. `git_graph`'s file-history and sha-pinned views ran through it.

How to apply: `get_args` returns `Vec<Cow<'_, str>>` so the sha arm can own its hex string. Guard test: `test_log_source_sha_is_passed_as_hex` in `crates/git/src/repository.rs`. If you add a `LogSource` variant whose argument is not already text, own it the same way — do not reach for `as_bytes`.

### 77. A markdown style's font size must be set on the container too

Why: `MarkdownStyle::base_text_style` looks like it controls the rendered text size. It does not, on its own. Markdown emits its text as runs carrying `HighlightStyle`, which has **no font size** — the glyphs are laid out at whatever size the containing div inherits, i.e. the window's UI size. The git graph's commit detail panel asked for `TextSize::Small` and silently rendered at 16px next to 12px `Label`s for as long as the panel has existed; changing the requested size had literally no effect, which is what makes this worth writing down.

How to apply: set the size on **both** — `base_text_style` (used for measurement and for run attributes) and `container_style.text.font_size` (what the glyphs actually inherit). `MarkdownStyle::with_preview_overrides` in `crates/markdown` does exactly this and is the model. If a markdown block ignores your size, this is why; do not reach for a wrapper div's `text_size`, set the container style on the `MarkdownStyle`.

### 78. The diff toolbars navigate and count; they do not stage

Why: the maintainer's reference is IDEA, whose diff window carries a difference count and no VCS action buttons at all. Sawe had `Stage` / `Unstage` / `Toggle Staged` / `Restore` / `Stage File` / `Unstage File` / `Commit` in both the solo-file and project diff toolbars, duplicating the git panel, which is where staging and committing already live.

How to apply: `SoloDiffGitToolbar`, `ProjectDiffToolbar`, `BranchDiffToolbar` and `CommitViewToolbar` show the file status, the `+N −M` diff stat, and `N difference(s)`. The count is hunks — the number of stops the hunk arrows next to it have, which is why it is a hunk count and not a files-changed count even in the multi-file views. It is memoised behind an O(1) sum-tree key (`HunkCountCache`), because `diff_hunks()` is O(excerpts + hunks) with anchor resolution per hunk and the toolbar renders every frame. The **actions** and their keybindings all survive — only the buttons went. Do not re-add a staging button here.

### 79. The left diff pane mirrors the right pane's headers, not its singleton-ness

Why: `SplittableEditor::split` chose the left pane's multibuffer from `rhs.is_singleton()`. But `MultiBuffer::without_headers` is used for non-singleton multibuffers too — `CommitView`'s compare-range mode and `SoloDiffView`'s commit source (both since #136; the example this originally named, `commit_view`'s *single-file* mode, is deleted), `acp_thread::diff`, the inline assistant. For those the left pane got headers **on** and emitted a `Block::BufferHeader` of `FILE_HEADER_HEIGHT = 2` rows above its first excerpt while the right pane emitted none, so the whole left pane sat two rows lower and equivalent lines did not line up. `check_invariants` catches it, but it is `#[cfg(test)]`.

How to apply: the left pane mirrors `rhs.show_headers()`. That subsumes the singleton case, which is headerless by construction. Guard test: `split::tests::test_headerless_right_pane_does_not_offset_the_left_pane`.

### 80. Gutter indicators are placed by column, and the breakpoint's column is the code edge

Why: the breakpoint dot was centred in `line_number_area()`, which put it in the middle of the gutter with the right half empty — the numbers themselves are right-aligned on that area's end. JetBrains puts it against the code.

How to apply: `GutterDimensions::indicator_code_edge()` is the rightmost x an indicator may end at without entering a column that paints something of its own (`fold_area_start()` upright, `content_area_start()` mirrored), and `GutterIndicatorColumn::CodeEdge` right-aligns on it. Hover arming must be widened to the same span or pointing at the dot arms nothing. Note the mirrored (right split pane) branch is unit-tested only: split panes turn breakpoints off. Related: in a mirrored gutter the git hunk strip is inset by one deletion-marker reach (`hunk_strip_inset`) so the strip and the wider deletion pill clear the line-number column — anything else deriving an x from `git_gutter_width` must go through `git_column_width`, or it will paint under the strip.

### 81. The commit detail panel's two regions are sized in pixels, never percentages

Why: the panel is a changed-files tree above a commit message, and the message used to be `max_h(relative(0.5))` over content. A percentage max-size only applies when the container's own height is definite, and inside this flex chain it is not — the cap silently became "no cap". With a long message that had two consequences at once: the message claimed more height than the panel had and slid out of view, and the sibling `uniform_list` measured itself against a container that overflowed its parent and rendered **no rows at all**, so the file list looked empty.

How to apply: `DetailSplitState` holds the message region's height in **pixels** (default 180), the tree takes the rest with `flex_1`, and the divider drag writes pixels straight from the drag bounds. Do not reintroduce a percentage here, and do not size the message from its content — a definite height sizes both regions in one pass and neither can surprise the other. Double-click restores the default.

Related trap in the same panel: a bare `div()` is a ROW flex, so a `min_w_0` child takes its width from its own content and can shrink to a single character — which is how the author line came to wrap vertically, one letter per line, after a resize. Use `flex_1().min_w_0()` for text that must fill a row. Conversely, do **not** add `w_full()` to the scrolling message region or its children: the region is a scroll container, and a percentage width inside it does not resolve either, which reproduces the same one-character wrap.

### 82. A scrollbar attached to the element it scrolls must anchor to the viewport

Why: `WithScrollbar` attaches the scrollbar as a *child* of the decorated element. When that element is itself the scroller, `Div::prepaint` prepaints all children — absolutely positioned ones included — inside `with_element_offset(scroll_offset)`, so the track, thumb and hitbox all slide up by the scroll offset. Measured on the commit-message region: at offset 700px the track's top landed at −596px against a 100px viewport. The visible thumb height then shrinks linearly as you scroll, and after the panel is resized there is no thumb to grab at all. The scroll *extent* was never wrong.

How to apply: `ScrollbarState::track_anchor` uses `scroll_handle().viewport()` — recorded before the children are prepainted, hence free of the translation — whenever the decorated element drives the very handle being drawn. 1 of 49 call sites is in that shape; `uniform_list` and `list` own their own viewport and are unaffected. If you add a scrollbar to a div you also called `.track_scroll()` on, this is the path you are on.

### 83. A blank line keeps the indent you just typed, and saving spares the line the cursor is on

Why: pressing Enter inside an indented block auto-indented the new line, but upstream then extended the *next* Enter's edit back to column 0, wiping the whitespace off the line it was leaving (VS Code's `editor.trimAutoWhitespace`). The line was left genuinely empty, so arrowing back up onto it put the cursor in column 1 — in the middle of a Kotlin `companion object`, the caret jumps to the far left and you have to re-indent by hand. IDEA does the opposite: it keeps the indentation, and its "always keep trailing spaces on caret line" save option (on by default) keeps it from being trimmed out from under you on the next save.

How to apply: `Editor::newline` (`crates/editor/src/input.rs`) no longer rewrites the line it leaves. The whitespace it leaves behind is still cleaned up on save — except on rows holding a cursor: `Editor::selections_did_change` reports the cursor anchors down through `MultiBuffer::set_caret_positions` to `Buffer::set_caret_positions`, and `Buffer::remove_trailing_whitespace` resolves them to rows and skips those ranges. The report is deliberately **not** gated on focus, because autosave (`on_focus_change`) runs right after focus is lost, which is exactly when the rows still need to be known. Two consequences worth knowing: an explicit *Format Document* spares the caret line too (same code path — `test_multiple_formatters` parks its caret on a clean line for that reason), and the protection is per-`Buffer`, so if the same file is open in two editors the last one to move its cursor owns the protected rows.

### 84. The split-diff divider stores its position as it moves, because `on_drop` never fires under the handle

Why: the divider's position is remembered process-wide (`LastSplitRatio`) so that stepping to the next file in a commit keeps it, and the ratio used to be committed to that global from an `on_drop` listener on the container. That listener never ran. The drag handle is `deferred` and sets `block_mouse_except_scroll`, so it is the topmost hitbox and truncates `MouseHitTest::hover_hitbox_count`; gpui's drop listener requires `hitbox.is_hovered()` on the element that owns it, and the container is behind the handle. Since the handle tracks the cursor for the whole drag, the mouse-up always landed on it — the ratio was never stored, and every file after the first opened with the divider back where it had been.

How to apply: `SplitEditorState` has one `left_ratio` field (the split visible/committed pair is gone) and every mutation goes through `set_ratio`, which writes the global. Do not reintroduce a commit-on-release step here, and be suspicious of any `on_drop`/`on_click` handler on an ancestor of an element that blocks the mouse — that hitbox rule applies to hover styles, tooltips and clicks as much as it does to drops.

### 85. The commit graph selects sets of commits, and the menu it deploys knows the difference

Why: IDEA's log lets you Ctrl/Shift-pick several commits and act on the set — compare two, squash a range. `GraphView` only had `selected_entry_idx`, and every commit action was written against a single sha.

How to apply: `selected_entry_idx` stays the **active** row and keeps driving the detail panel; `selected_entry_idxs` (a set) and `selection_anchor_idx` sit beside it, with the invariant that the set is empty exactly while the active row is `None` and otherwise always contains it. Every existing `select_entry` caller (keyboard, restore-from-db, programmatic) collapses the set to one row, so nothing had to change at those call sites. The click algebra is a pure function (`fold_row_click`) so it is unit-testable without a window — the synthetic "local changes" row can never join a multi-selection, and Ctrl-toggling the last row off keeps it selected. A right-click *inside* a multi-selection deploys `git_ui::commit_context_menu::build_multi_commit_context_menu` instead of the single-commit menu; anywhere else behaves as before. Entries that do not apply (Compare Versions without exactly 2, Squash without a contiguous range) are shown **disabled**, not hidden, so the menu keeps one shape.

`gpui`'s `ClickEvent::modifiers()` reports the modifiers at mouse-**up**, so releasing Ctrl before the button does not toggle. That is how the rest of the app behaves; do not "fix" it locally.

### 86. `SquashOp`/`FixupOp` rejected any range that did not end at HEAD

Why: both walk `git rev-list <oldest>^..HEAD` and classify each commit. Once every selected commit had been folded, the walk kept comparing the *remaining* commits — the ones newer than the range, which are simply untouched history — against the selection and bailed with "targets are not contiguous: commit X sits between selected commits". So squash only ever worked when the selection reached the branch tip; "Squash with Previous" on anything but HEAD failed the same way. Found by driving the new multi-select squash on the dev instance, not by any test — the existing tests only exercised tip-most squashes.

How to apply: after the last target is folded, `pick` the rest. The contiguity error is only correct *while* targets are still outstanding. `crates/git/tests/rebase_e2e.rs` covers both ops with a four-commit repo whose middle two are folded.

### 87. Diffing two arbitrary revisions is `load_commit_range`, and `CommitView` renders it bare

Why: "Compare Versions" needs a commit-vs-commit diff, and everything in tree diffed a commit against its parent (`load_commit`) or the working tree against a base ref (`ProjectDiff` branch mode). Neither can express `git diff A B`.

How to apply: `GitRepository::load_commit_range(base, head)` (local backend only — an arbitrary revision pair has no meaningful degraded answer over the collab wire, so the remote arm errors) reuses `load_commit`'s name-status parser and cat-file blob loader, which were extracted into one shared helper rather than copied a third time. `CommitView` gained `compare_range: Option<(base, head)>`; when set it takes the same bare shape as single-file mode (no metadata panel, no message excerpt), titles the tab `base..head`, and dedups open tabs on the range rather than on a sha.

### 88. The context meter's high-watermark belongs to one thread, and the persisted count dies with the conversation

Why: the meter ratchets `used_tokens` up freely and only ratchets down when a reading collapses to ≤ 10 % of the peak (`smooth_used_tokens`) — a guard against the SDK's per-call usage wobble. That heuristic cannot tell "the context was just compacted" from "this call reported less", so any value carried across a rotation pins the meter forever: observed live as **797.4k / 1.0M** against a real post-compaction context of 485k, where 797 438 was exactly the last per-message reading (`input + cache_creation + cache_read`) of the *pre*-compaction Claude session. The `SessionContextReset` handler did clear the peak, but nothing stopped a stale reading from re-establishing it, and every honest reading afterwards fell in the swallowed 10–100 % band.

Second half of the same bug: `insert_or_update_metadata` COALESCEs `total_tokens`, so `rotate_context` clearing `cached_total_tokens` in memory could not clear the DB column — the pre-compaction number stayed on disk for any later read to resurrect.

How to apply: the peak is stored with the `EntityId` of the `AcpThread` it was measured against (`status_peak_thread`), and `ratchet_used_tokens` drops it the moment the meter observes a different thread — compaction, `/clear` and restart all mint a fresh thread, so a stale peak is unrepresentable rather than merely unlikely. Keep the `SessionContextReset` handler too: it clears immediately instead of at the next render. `rotate_context` / `reset_context` additionally call `db.clear_total_tokens` — a plain `UPDATE … = NULL`, because the COALESCE upsert structurally cannot express "forget this".

### 89. AI sessions are Solution-scoped (member binding removed)

Why: a 2026-06 change (commits `2ffeb840e1` / `9c3e0e9d1e`) gave `solution_sessions` a `member_id` column, stamped every new session with the active member's id, created sessions with the member's `local_path` as cwd instead of the solution root, and filtered the visible chat-tab strip by it. This directly contradicted decision #27 ("open files / terminals / AI dialogs stay agnostic" of the active member) and produced a real regression: switching the active project could hide a chat tab entirely, not just change its selection state. A session belongs to the Solution — the cross-project unit of work — not to whichever member happened to be active when the user clicked "new chat". The `2026-08-26-solution-scoped-sessions` plan (spec: `docs/plans/2026-08-26-solution-band-ai-dialogs-design.md`) reverted this: chat tabs are never filtered by `active_member` (`console_panel::tab_in_scope` treats every `ConsoleTab::Chat` as `TabScope::Unscoped`), `create_session` and its `_with_cwd`/`_with_parent` variants no longer take or stamp a member, and `cwd: None` means "solution root" (decision #5).

How to apply: never reintroduce a `member_id` read on the session-creation or session-listing path, and never filter a chat-tab surface by `active_member` — that job is exclusively a terminal-tab concern now (`TabScope::Member`, decision #27's split). Two things are **deliberately dead-but-present**, don't "clean them up":
- The `solution_sessions.member_id` SQLite column still exists — the startup migration (`migrate_identity`) needs a column to `UPDATE … SET member_id = NULL` on upgrade from a pre-revert build, and dropping columns in SQLite is its own hazard. Nothing reads it after migration; see the doc comment above `apply_idempotent_add_column(&connection, "member_id INTEGER")` in `crates/solution_agent/src/db.rs`.
- `mcp::dto::SessionSummary::member_id: Option<i64>` is a **schema-only** artifact: it derives `JsonSchema` and is `Serialize`-only, `#[serde(default, skip_serializing_if = "Option::is_none")]` and hardcoded `None`, so it is **never actually emitted on the wire** — the JSON is byte-identical to what deleting the field would produce. It's kept only so a client codegen'd from the published schema (which still advertises the property) doesn't have to churn its generated type when the field disappears. `GetSessionResult` (the `get_session` response shape) never had this field.
- `crates/solution_agent/src/store/teardown.rs::gc_orphan_members` is the one place that still infers a session's owning member from its `cwd` (`at_root` / `under_member` matching), rather than treating a session as purely Solution-scoped. This is intentional, not a leftover: sessions created during the 2026-06→2026-08 interlude have a member-folder cwd, and that cwd can never be rewritten to `solution.root` post hoc — `claude-acp` buckets transcripts by encoded cwd (`~/.claude/projects/<encoded-cwd>/`), so moving it would orphan the on-disk transcript and break resume. The consequence: removing a member from a Solution still hard-purges (`purge_session_hard` — deletes the DB rows and `.agents/<sid>`) any legacy session whose cwd sits inside that member's folder, even though the user now has every reason to think of it as a Solution-level dialog. Narrowing or removing this predicate is out of scope for this decision — it would require either rewriting a survivor's cwd (forbidden above) or leaving orphaned-member sessions permanently un-collectible, both bigger calls than a doc-only pass. If this needs to change, it's a fresh decision, not a bug fix. **Amended 2026-08-27 — the purge reaches only LIVE sessions.** `gc_orphan_members` now skips any orphan whose `acp_thread` is `None`, logging it (`target: "solution_agent::gc"`, with the session's cwd and the solution's current member list) instead of purging. Why: that loop iterates `by_solution`, which until this date only ever held sessions opened in the current process, so "removing a member hard-purges its legacy sessions" meant *the ones you had actually been using*. Making cold-hydrated sessions visible to the tab strip (they must be, or a restart shows an empty strip) also made every restored transcript on disk visible to this GC — and it purges every orphan it can see on ANY `SolutionStoreEvent::Changed`, not just ones under the member just removed. On the maintainer's own database that turned a single member removal into the irreversible loss of ~18 transcripts (six DB tables plus `remove_dir_all(<root>/.agents/<sid>)`, no confirmation, no undo). The liveness gate restores substantially the pre-existing blast radius while keeping the strip fix. **Correction:** commit `6c29b8b63f`'s body said it restores that radius *exactly*; it does not, in either direction, and the claim should not be relied on. It is **narrower** than before on one path — `respawn_agent` (agent restart *and* watchdog reconnect) cold-izes a live session via `set_acp_thread(None, cx)` and only re-attaches after an async `resume_session`, so a `Changed` landing inside that window used to purge and now logs. It is **wider** than before on another — post-commit a cold session sits in `by_solution`, so `/clear` on a legacy orphan makes it purgeable on the next `Changed` where pre-commit it was not (materially harmless: `/clear` already clears the entries and re-persists, so the user's own action destroyed the transcript first). What the gate does guarantee is the load-bearing half: no editor-initiated event can delete a transcript the user has not touched this run. It also closes the no-user-action half of a race: a rename landing mid-hydration leaves a cold entity holding the pre-rename cwd (`rewrite_session_cwds_for_move` runs on `PathsMoved`, finds nothing in `by_solution` yet, and fixes only the DB), which this predicate would have read as an orphan. It does NOT make a stale cwd unpurgeable: `reset_context` (`/clear`) warms a cold session without assigning `s.cwd`, and `resume_session` tries the persisted cwd FIRST by design and then writes it back — and a member rename leaves a compat symlink at the old path, so that stale candidate succeeds. Either route yields a live session holding a pre-rename cwd, i.e. purgeable. That is the pre-existing hazard in `docs/findings/2026-07-14-rename-purges-open-sessions.md`, unchanged by this commit in either direction. Consequence to accept: legacy orphans now accumulate rather than self-collect. That is deliberate — the log makes the backlog a decision the maintainer makes, and "close the chat" remains the explicit, reversible way to retire one.

That carry-forward gap is **closed as of phase 2a (decision 93)**: `ConsolePanel::active_by_member` now keys terminal tabs only, and the active dialog is per-Solution state on `SolutionAgentStore`, so switching the active project no longer moves which chat is selected. Verified live: switching members swaps the band's terminal while the dialog half and the status-bar session tab strip are untouched.

### 90. The client does not infer branch protection from branch names

Why: `branch_protection` shipped with `default_protected: ["main", "master", "release/*"]`, so out of the box the editor treated any branch with one of those names as protected and answered `merge` / `rebase` / `cherry_pick` / `revert` / `squash` / `fixup` / `drop` / `move_commit` / `edit_commit_message` with `RequiresConfirmation`. Whether a branch may be written is the **server's** decision — GitHub/GitLab branch protection, hooks, CI gates — and the server enforces it at push time regardless of what this client believes. Guessing locally from a name produced only false positives (`master` is an ordinary working branch in plenty of repos, including ones this fork is used on daily) while adding no safety the remote did not already provide. Worse, it was a *dead end*: the `RequiresConfirmation` tier requires the UI to prompt and re-invoke with `confirmed: true`, and only force-push (`push_dialog::force_confirm`) and reset ever wired that up — so on a "protected" branch a merge surfaced `branch protection: requires confirmation: 'master' is protected — confirm 'merge'` as a failure toast with no way to proceed.

How to apply: the shipped default is now an empty pattern list (`BranchProtectionSettings::default`), so no branch is protected unless the user opts in via `solutions.branch_protection.default_protected` or a per-member override. The policy machinery itself is kept and still fully tested — it is genuinely useful for someone who wants local nagging — but it is off by default and must stay off. Do not re-add name-based defaults. If you touch the `RequiresConfirmation` tier, note that nine ops still have a `run_with_confirmation` variant with **no caller**: the confirm-and-re-invoke UI was only ever built for force-push and reset, so opting in today still hits the dead end described above for the other nine.

### 91. The Solution band's utility section reaches `ConsolePanel` through a type-erased `Workspace` slot, not a typed field

Why: phase 2a task 6 moved the terminal (`ConsolePanel`) out of the bottom dock and into the Solution band's utility section, beside the AI dialog. The obvious design — give `SolutionBand` (crate `solution_agent`) a typed `Option<Entity<ConsolePanel>>` field — is a crate-graph cycle: `console_panel` already depends on `solution_agent` (for `SolutionAgentStore`, `ReopenSessionModal`), so `solution_agent` cannot depend back on `console_panel`. Meanwhile `run_config_ui` and `agent_ui` (inline assistant) both need the concrete `Entity<ConsolePanel>` too — for `spawn_task`/`set_assistant_enabled`/`active_terminal_view` — but neither depends on `solution_agent` at all, so a typed handle stashed on `SolutionBand` wouldn't reach them either.

How to apply: `Workspace` gains a second `AnyView` slot, `solution_band_utility_item` (`set_solution_band_utility_item` / `solution_band_utility_item()`), a sibling of `solution_band_item` set from the same `zed.rs` call site that used to call `workspace.add_panel(console_panel, ..)`. `SolutionBand::render` reads it fresh every frame (no cached copy) and renders it beside the dialog when `utility_visible` is set; task 7's draggable divider replaces the current fixed even split. Any crate that needs the *typed* entity — not just to paint it — calls `console_panel::console_panel_for_workspace(workspace)`, which downcasts the same slot; this replaces every former `workspace.panel::<ConsolePanel>(cx)` / `workspace.toggle_panel_focus::<ConsolePanel>(..)` call site (`run_config_ui::RunController::spawn`, `agent_ui::inline_assistant`'s terminal-target resolution + assistant-enabled sync, `console_panel`'s own `NewTerminal`/`ToggleFocus` handlers, `debugger_ui`'s test harness). `console_panel::ToggleFocus` (`ctrl-\``) now resolves both the panel (for its `FocusHandle`) and the band (via `Workspace::solution_band_item` downcast to the concrete `SolutionBand` — legal because `console_panel` already depends on `solution_agent`) and calls `SolutionBand::toggle_utility_focus`, which mirrors `Workspace::toggle_panel_focus`'s tri-state (hidden → show + focus; shown + focused → hide; shown + unfocused → refocus) using a `FocusHandle` the caller supplies, since the band itself only ever holds the `AnyView`. `ConsolePanel`'s `Panel` impl, `dock_position` field, and `ConsolePanelSettings` (`default_position` / `default_width` / `default_height` / `button_visible`) were removed outright rather than left inert — with no dock chrome anywhere, none of the four had a reachable caller left, and the generic per-dock "Dock Left/Right/Bottom" right-click menu (`workspace::dock::PanelButtons`) only ever renders entries for panels actually present in `dock.panel_entries`, so it needed no separate fix.

Phase 2b generalised the slot to a `HashMap<UtilityKind, AnyView>` (terminal / git graph / debugger) and moved the other two occupants in the same way — `GitGraphPanel` (task 4) and `DebugPanel` (task 5) each lost their `Panel` impl, and `zed.rs` installs all three through one `add_utility_item_when_ready<T: Render>(kind, ..)`. The debugger was the coupled one: `Workspace::panel::<DebugPanel>` had ten production callers, so it gained `debugger_ui::debugger_panel::debug_panel_for_workspace` (the same downcast as `console_panel_for_workspace`) plus `reveal_debug_panel`, which is how "open my dock and activate me" translates — `SolutionBand::set_utility_kind(Debug)` + `set_utility_visible(true)`, the band's first non-terminal writer. Its `Panel::position` (which read `DebuggerSettings::dock`) collapsed into a `BAND_DOCK_POSITION: DockPosition = Bottom` constant, since the band's utility half is a wide, short, bottom-dock-shaped region; the setting stays in the schema but steers nothing (`test_new_sessions_ignore_the_debugger_dock_setting`). Panel-level zoom became an explicit swallow-the-action no-op (there is no dock to zoom and the band owns its own geometry), and the panel's "Close Panel" button, which used to dispatch `workspace::ToggleBottomDock`, now hides the utility section.

Testing a `FocusHandle` tri-state without a live `ConsolePanel` view is a trap worth knowing about: `FocusHandle::contains_focused` consults the most recently **rendered** frame's dispatch tree, and `Entity::update_in` resolves the window an entity was *created* in (`with_window(entity_id, ..)`), not whichever `VisualTestContext` happens to be calling it. `solution_band::tests::toggle_utility_focus_shows_focuses_then_hides` therefore builds `SolutionBand` inside a dedicated probe window (a bare view that does nothing but `.track_focus()` the same handle) rather than `Workspace`'s window — sharing `Workspace`'s window made the assertion flaky, because `Workspace::new` registers its own `cx.on_focus_lost` handler (decision noted on `crates/workspace/src/workspace.rs` in the touched-files table) that refocuses itself the instant the probe's tracked focus is momentarily absent from a redrawn frame.

### 92. The Solution band's geometry is one per-Solution row in the agent DB, and the band resolves its Solution off the *Project*

Why: phase 2a task 7 made the divider between the band's dialog and utility halves draggable, and the position had to survive a restart per Solution — the split-diff precedent (`editor::split_editor_view`, decision 84) stores its ratio in a process-wide `Global`, which would make every Solution share one divider. Three constraints shaped where the state ended up. (1) A plain-folder window that resolves to no Solution is a supported case here (`console_panel::panel::workspace_has_project` has an explicit non-Solution branch) and its terminal still opens with `ctrl-\``, so a store keyed solely on `SolutionId` would silently turn that hotkey into a no-op. (2) `on_drag_move` fires per mouse move, and unlike the precedent's free `Global` write, ours is SQLite. (3) `SolutionBand::set_utility_visible` and `toggle_utility_focus` are both invoked from inside a live `&mut Workspace` borrow (`console_panel::handle_toggle_focus` under `workspace.register_action`; `panel::reveal_utility_section` inside `workspace.update_in`), so the "which Solution am I?" lookup they need must not read the `Workspace` entity — the double-lease panic that has bitten this fork repeatedly.

How to apply: `BandState { divider_ratio, utility_visible, active_dialog_session }` (`solution_agent::model`) is owned by `SolutionAgentStore::band_state: HashMap<SolutionId, BandState>` and mirrored to a `solution_band_state` row keyed on an INTEGER `solution_id` (`crates/solution_agent/src/db/band.rs`), hydrated once in `set_persistence` and dropped by `delete_by_solution`. There is deliberately **no** `dialog_collapsed` flag: `active_dialog_session == None` *is* the collapsed state, which is exactly what `session_tab_strip::toggle_selection` already produces when the active tab is re-clicked; a second boolean could only ever disagree with it. `utility_visible` and `active_dialog_session` are written immediately *once hydration has landed*; `divider_ratio` is written behind a cancel-on-replace debounce whose `Task` lives in `band_state_writes` keyed by Solution (an immediate write drops the pending debounced one first, since that older snapshot would otherwise land after it and undo it). `SolutionBand` holds a window-local `BandState` used only when `solution_id` resolves `None`, and never persisted — there is no key to persist it under. `purge_solution_fully` calls `forget_band_state` **before** its `purge_session_hard` loop, not after: `clear_active_dialog_for_session` would otherwise re-persist a row for the dying Solution, and that detached write races `delete_by_solution` with no ordering guarantee between two background DB writes. Because `set_persistence` is gated behind two sequential DB awaits, a `ctrl-\`` inside that window used to occupy the map key with a defaults-seeded entry and make the merge skip that Solution for the whole process; so no band row is written until hydration has merged, each setter records which field it changed in `band_state_touched` until then, and the merge overlays only those fields onto the persisted row before flushing them. The guarantee that buys is narrow and worth stating exactly: **a persisted field is lost only if the user themself set that field this run** — the per-setter no-op guards cannot provide it, since before hydration the in-memory value is the default, which is precisely what a saved row differs from. Residual: if `load_band_states` itself errors, hydration still marks itself done and flushes, which can write a defaults-seeded row over a saved one.

For constraint (3), `SolutionBand::new` takes `Entity<Project>` as a plain constructor parameter alongside the `WeakEntity<Workspace>` (the `ProjectToolbar::new` pattern), and `solution_id` walks *that* entity's worktrees. Reading `Project` under a `Workspace` borrow is proven safe — `console_panel::panel::active_solution_id_for_workspace` already does it from `handle_new_chat`. Only `utility_panel` still reads the `Workspace` entity, and it is therefore called from `render` and nowhere else. The drag callbacks capture the `Option<SolutionId>` the enclosing render pass already resolved rather than re-resolving it, so a frame's writes can never target a different Solution than that frame's reads. The divider itself keeps the precedent's mechanics verbatim, including committing the ratio continuously in `on_drag_move` — `on_drop` never fires under `deferred()` + `block_mouse_except_scroll()` (decision 84); do not "fix" that back.

### 93. The Solution band is a full-width `Workspace` slot between the project zone and the status bar — not a dock

Why: the AI dialog is this fork's primary surface, and it was sharing one bottom-dock slot with the terminal
(decision 22). A dock shows exactly one panel per position, so "chat and terminal at the same time" was
unreachable by construction — the user paid a keybind and a context switch every time they wanted to read the
agent's answer while a build ran. Widening the dock does not help; the constraint is one *visible* panel, not
width. A dedicated full-width region that the workspace lays out itself is the only shape that shows both, and
it also lets the two halves be sized against each other rather than against the editor.

The band is therefore a `Workspace` slot in the same family as `project_toolbar_item`: `solution_band_item`
(the band view) plus `solution_band_utility_item` (the terminal, decision 91), both `AnyView`. Only the first is
rendered by `Workspace::render`, after the dock row and before the status bar; the utility slot is a plain
field that `SolutionBand::render` pulls back out (`utility_panel`) and paints inside its own right half — so
**that** is where phase 2b hosts GitGraph and Debug, not `Workspace::render`. Selecting which dialog it shows is
per-Solution store state (`SolutionAgentStore::active_dialog_session`), driven from the status-bar
`solution_agent::session_tab_strip`, and its geometry persists per Solution (decision 92).

How to apply:

- **It is a slot, not a dock.** Do not give it a `Panel` impl, a `DockPosition`, or a settings-backed position.
  Content is installed from `zed.rs` via `set_solution_band_item` / `set_solution_band_utility_item` and
  resolved back out by downcast (`console_panel::console_panel_for_workspace`).
- **The bottom dock is deliberately left empty, not deleted.** GitGraph and Debug still live in it and the
  vertical dock button strips still render (phase 2b relocates them). Keeping the dock means no upstream
  `Dock`/`Panel` surgery and a one-line path back if the band is ever reverted.
- **Nothing rendered in the band may read the `Workspace` entity.** Everything the band draws runs under a
  `Workspace` lease, so `workspace.read(cx)` / `workspace.update(cx, …)` from band content (or from anything a
  `workspace.register_action` handler reaches) is a double-lease panic — see decision 22's call-site rule and
  decision 92's `Entity<Project>` resolution. Take `&Workspace` / `Entity<Project>` as a constructor parameter
  instead. This fork has been bitten by it repeatedly. `run_config_ui::run_controller::RunController::run` was
  a live instance until 2026-08-27 (it aborted the process on every `run_config::Run` of a Terminal config) and
  is now the worked example of the fix: `run` is an associated function taking the caller's `&mut Workspace` +
  `Context<Workspace>`, and anything further down that reads the workspace on its own (here
  `ConsolePanel::spawn_task`, which holds a `WeakEntity<Workspace>`) is started from inside an async task
  instead of synchronously — see `docs/findings/2026-08-26-run-controller-terminal-double-lease-crash.md`.
  Threading `&mut Workspace` alone is **not** sufficient when a callee re-derives the workspace itself.

Correction to an earlier claim (this file asserted the opposite until 2026-08-26): the bottom dock does **not**
span the window. `BottomDockLayout` still exists in `crates/settings_content/src/workspace.rs` with `#[default]
Contained`, `assets/settings/default.json` sets no override, and the `workspace` crate never reads the setting
at all — `Workspace::render` nests the bottom dock inside the centre column between the left and right docks
unconditionally, so there is no `Full` arm to flip a default into. The commits that once implemented it are on
unreachable refs, not in `main`'s history. `settings_ui` still renders the dropdown, so the setting is
user-visible but inert; wiring it up (or removing it) is its own task.

**Phase-2b carry-forward** (known, deliberately not done here):

- GitGraph and Debug still live in the bottom dock; the vertical dock button strips still render. The band's
  utility section hosts only the terminal.
- **The band has no height of its own.** It is content-driven, so the dialog half is whatever the status row
  plus the compose box demand — measured live at ~128px, which leaves the transcript region roughly 30px and
  therefore blank even for a session that has messages. The spec only ever specified the *vertical* divider
  *inside* the band (decision 92); a band height and a horizontal drag handle between the project zone and the
  band need their own task, and until then the dialog half is a compose box, not a conversation.
- Spec §3's "clicking the *active* utility-section button hides the section" and the `ctrl-shift-a`
  dialog-toggle hotkey are both deferred: no section button exists yet (they arrive with 2b's "relocate the
  dock buttons by geometry" work), and the hotkey is the one collapse path that would need a "remember the last
  session" field, which the no-`dialog_collapsed` ruling deliberately removed.

**Traps found while building this and recorded nowhere else:**

- **`ui::ContextMenu` will not nest a `right_click_menu` inside an open popover.** `ContextMenu::build`/`new`
  unconditionally wires `cx.on_blur(focus_handle) -> cancel() -> DismissEvent`, and `right_click_menu`'s
  two-frames-deferred focus grab for its inner menu blurs the OUTER popover — dismissing it and tearing down
  its whole deferred child tree, inner menu included, before that inner menu renders. The one internal escape
  hatch (`ignore_blur_until`) is private with no public setter short of replacing the blur subscription
  wholesale. Proven live twice; this is why the session tab strip's overflow rows are `submenu`s rather than
  nested right-click menus.
- **The ACP `StopReason` wildcard is an observability hole.** `StopReason` is genuinely `#[non_exhaustive]`
  (agent-client-protocol-schema 0.13.6), so an exhaustive match is impossible from `solution_agent` and the
  wildcard is unavoidable. The wall-lifted check defaults the `_` arm to "not proven", which is the
  conservative choice given the store-wide blast radius — but a dependency bump that adds a variant will
  surface only as "parked supervisors wake later than expected", with nothing in the logs pointing at why.
  Whoever next bumps the ACP schema should add a `debug_assert!` or a log on that arm first.
- **`cold_close_solution` evicts sessions without emitting `SessionClosed`**, so a view cache keyed on that
  event does not evict through that path. Benign only because the band lives inside the same `Workspace` entity
  as the closing window — i.e. it holds under one-window-per-Solution. Break that invariant and this silently
  leaks editor-bearing views.

### 94. The Solution band owns a persisted per-Solution height in absolute pixels; the window-relative cap is a render-time function that is never written back

Why: phase 2a shipped the band as a *content-driven* row (decision 93). With an empty session it measured
~128px, of which the transcript region was ~30px — the primary surface of the whole redesign painted a status
row and a compose box and no conversation at all. The band therefore needs a height of its own, and three
sub-decisions were not obvious.

**Absolute logical pixels, not a fraction of the window.** The band's content has an intrinsic pixel floor
(status row + compose box; `MIN_BAND_HEIGHT = 140`), which a fraction cannot express — on a short window a
"25% band" is unusable and on a tall one it is absurd. Every dock in this editor already persists an absolute
size, so a fraction would also make the band the odd one out. The cost is the one every dock already pays: move
the window from a laptop panel to an external monitor and the band keeps its pixel height rather than its
proportion. `DEFAULT_BAND_HEIGHT = 320` (also what double-clicking the top edge restores), `MAX_BAND_HEIGHT =
4096` guards only a corrupt or hand-edited row.

**The window-relative ceiling (`MAX_BAND_HEIGHT_FRACTION = 0.8`, floored by `viewport - BAND_RESERVED_HEIGHT`)
is applied at render, as a pure function of (stored height, live viewport height), and never persisted.**
The fraction alone is not enough, because it is a fraction of the *whole window* while the band only competes
for what is left after the chrome: title bar 30 + project toolbar 30 + status bar 30 + two 1px workspace
borders. The band is `flex_none`; the project zone is `flex_1` with basis 0 (shrinks to 0 first) and the status
bar is a plain 30px row with the default `flex-shrink: 1` — so an over-tall band zeroes the editor and then eats
the status bar. That happens for every window shorter than ~460px (`0.8H + 30 > H - 62`), which is reachable:
`window_min_size` is 240. Hence `BAND_RESERVED_HEIGHT = 150` (~92px of chrome + ~58px so the project zone is
still an editor rather than a hairline); re-derive it if any chrome height changes. Below ~290px of window the
reserve and `MIN_BAND_HEIGHT` cannot both hold and the floor deliberately wins. The tempting shape — notice during layout that the
band no longer fits, clamp it, and save the clamped value — cannot work here: `Window::invalidate_view` returns
`false` and pushes no `Effect::Notify` while `draw_phase != DrawPhase::None`, so a `cx.notify()` raised from
`request_layout` / `prepaint` / `paint` is silently *discarded*, not deferred (`docs/findings/2026-08-17-gpui-
draw-phase-invalidation.md`). Hopping out with `cx.defer` to make the write stick would then re-derive from the
new bounds on the next frame and spin. It is also wrong on its own terms: a window temporarily shrunk (tiling
WM, projector, split screen) would permanently destroy the user's saved geometry. So `model::effective_band_height`
takes the stored value and the viewport and returns what to paint; `model::clamp_band_height` is the only thing
that touches what gets stored. Verified live at 1920x1080: stored 990 painted 864 (= 0.8 x 1080) with the status
bar intact, and the row still read 990 afterwards.

**The cap alone does NOT keep the status bar on screen — the layout invariant does.** No pure function of
(stored, viewport) can, because it cannot know the project zone's *content* minimum. The workspace column in
`Workspace::render` (the `flex_col` holding `#workspace`, the band and the status bar) has visible overflow, so
taffy floors it at its own min-content: the project zone's docks, tab bars and toolbars (~120px measured) on top
of the band's fixed height. On a short window that floor exceeds the window, the column overflows *downward*, and
the last row in it — the status bar — leaves the screen entirely. Three things hold the invariant together, and
removing any one of them re-breaks it: `min_h_0` on that column (kills the floor; the deficit lands on the
project zone, which clips, since `#workspace` is `overflow_hidden`), `flex_none` on the status bar (it is a fixed
30px row and must never be the thing that yields — it used to shrink silently, a few pixels at a time), and a
*shrinkable* band with `min_h_0` (the last-resort yielder, for windows so short that even the chrome does not
fit). Measured before/after at 1280x384 with the `windows.resize` MCP tool: before, band 307 (= 0.8 x 384),
project zone 47, status bar **0 visible pixels**; after, band 234, project zone 59, status bar a full 30 — and
30 at every height from 200 to 1080.

**The top-edge handle commits from `on_drag_move`, not `on_drop`.** The handle is `deferred()` +
`block_mouse_except_scroll()`, which truncates the hover stack so no ancestor's `on_drop` ever fires — the same
trap decision 84 records for the split-diff divider and decision 92 for the band's own vertical divider. Do not
"fix" this back to `on_drop`. The height is measured as `event.bounds.bottom() - cursor.y`: the band's *bottom*
is the anchored edge (the status bar sits directly under it), so measuring down from the top would chase the
value being changed. Writes ride the same 400ms cancel-on-replace debounce and the same per-Solution
`band_state_writes` slot as `divider_ratio` (decision 92) — one SQLite round-trip per mouse move is not
acceptable, and the two are never dragged concurrently.

The handle deliberately paints **no line of its own**. Live verification confirmed the boundary is already
unambiguous: the project zone's own 1px `border` row sits directly above the band across the full window width,
with a tonal step from the pane background to the band background beneath it. Adding a second rule there would
have been a double border.

### 95. The band's utility content is picked from a status-bar button group, which never moves focus and never falls back to a kind that loaded

Phase 2b gave the band's utility section three possible occupants (terminal, git graph, debugger) keyed by
`workspace::UtilityKind`, but de-docking the git graph and the debugger deleted their dock buttons and gave the
git graph *no* way in at all — it had no keybinding either at the time (it has one now, `ctrl-alt-\``;
see decision 30). `solution_agent::utility_buttons::UtilityButtons`
is the replacement affordance: three `IconButton`s in the status bar's left group.

**It drives `SolutionBand`, not `SolutionAgentStore`.** The store only knows Solutions; a plain-folder window
resolves to none and falls back to `SolutionBand::local_state`, so buttons wired to the store would be inert in
exactly that window (`ctrl-\`` works there today). Going through the band also keeps the click handler off the
`Workspace` entity — the band resolves its Solution off `Entity<Project>` precisely so its mutators are safe
under a live `&mut Workspace` borrow, which a status item's click handler must be assumed to run under
(decision 92). It lives in `solution_agent` because that is the only crate that owns the band and depends on
none of its occupants (`workspace` must not depend on them; all three already depend on `solution_agent`), so
the tooltips' keybindings are resolved by *action name* via `cx.build_action` — pinned by a test in each
occupant's own crate, since a rename would otherwise only cost a silently missing hotkey hint.

**Buttons and hotkeys agree on "the active content" but not on what a click does.** Active content is
`utility_kind` while `utility_visible`, for both. `ctrl-\`` / `ctrl-shift-d` stay tri-state (show+focus /
focus / hide) and are the only focus path; a button is a two-state content switch that never moves focus.
The single cell where they diverge is *visible && kind == mine && unfocused*: the hotkey focuses, the button
hides. That is deliberate — a mouse click on a status bar should not steal focus out of the editor, and a
button that did nothing visible when clicked would read as broken.

**Order in the status bar is buttons-then-tab-strip, inverting the band's own left-to-right layout.** The left
group is `min_w_0().overflow_x_hidden()`, so on a narrow window whatever sits last is clipped. The session tab
strip is variable-width and already absorbs a squeeze through its overflow popover; these three fixed icons are
the only route to the git graph. Clipping has to land on the surface that can handle it.

**A selected kind whose occupant failed to load renders a placeholder — it never falls back to a kind that
did.** A fallback would silently rewrite the user's persisted `utility_kind` and desynchronise the button group
from it, turning a load failure into a permanent, invisible preference change. The placeholder is gated on
`Workspace::solution_band_utility_unavailable(kind)`, a per-kind flag set only when the load task *resolves
with an error* — not on the slot simply being empty. `zed.rs` loads the three concurrently (`futures::join!`),
so absence is ambiguous between "still loading" and "gave up", and the weaker gate would tell the user a kind
had failed during the interval in which a sibling had merely resolved first.

**Icon note:** the Terminal button uses `IconName::Terminal`, NOT the `IconName::Console` that
`ConsolePanel::icon()` returned before it was de-docked. `console.svg` was the icon of the merged
Terminal + AI-chat panel; the chat half moved to the band's dialog side in phase 2a, so the occupant is
terminal-only now and the button is labelled "Terminal". `GitGraph` and `Debug` do reuse their old dock icons,
so the affordance those two users learned survives verbatim.


### 96. The band's utility slot is a `HashMap<UtilityKind, AnyView>` on `Workspace`, and `UtilityKind` is defined in `workspace`

Phase 2a gave the band a single `solution_band_utility_item: Option<AnyView>` holding the terminal.
Phase 2b needed three interchangeable occupants, so the slot became keyed:
`HashMap<UtilityKind, AnyView>` plus a `HashSet<UtilityKind>` of kinds whose load *resolved with an error*
(decision 95 explains why absence is not the same as failure).

**Why the map lives on `Workspace` rather than on `SolutionBand`, which is the thing that renders it.**
The band lives in `solution_agent`, which cannot depend on `console_panel` / `git_graph` / `debugger_ui` —
the edge already runs the other way, since all three depend on `solution_agent` for `SolutionAgentStore`.
So the band can never be handed a typed occupant. `Workspace` can hold them only because they are erased to
`AnyView`, and `zed.rs` — the one crate that depends on all of them — is the only place that can do the
erasing. The map is therefore on `Workspace` for the same reason `project_toolbar_item` is: it is the shared
surface both the producer and the consumer can already see. `SolutionBand::render` looks up the selected
kind fresh every frame rather than caching a handle, so an occupant that installs late is picked up with no
invalidation plumbing at all.

**`UtilityKind` is defined in `workspace`, not in a new shared crate and not in `solution_agent`.** It is
the map's key, so `workspace` must name it; and `workspace` must not depend on the occupant crates, so it
cannot be re-exported from any of them. Putting it in `solution_agent` would make `workspace` depend on
`solution_agent`, inverting the existing edge. It is a three-variant enum with a string form for the DB
(`as_str` / `from_str`) and an `ALL` iteration order that is deliberately part of the type, because that
order is the order the status-bar buttons paint in and re-spelling it per call site is how the two drift.

Anything needing a *typed* occupant downcasts the `AnyView` at its own key —
`console_panel::console_panel_for_workspace` (run-config output, the inline assistant) and
`debugger_ui::debugger_panel::debug_panel_for_workspace`. That downcast is the replacement for every former
`Workspace::panel::<T>(cx)` call, which is what a de-docked occupant loses.

### 97. The git graph and the debugger are band occupants with no `Panel` impl at all, which is what makes the debugger's orientation deterministic

`GitGraphPanel` (task 4) and `DebugPanel` (task 5) were dock panels; both now install into the keyed slot
from `zed.rs` and neither implements `Panel` any more. The `Panel` impls were **removed, not left inert** —
the fork keeps disabled *subsystems* in tree, but a `Panel` impl on a type no dock ever holds is not a
disabled subsystem, it is a trait impl with no caller whose methods lie about where the type lives.

**The debugger is the case worth reading.** Its `Panel::position` read `DebuggerSettings::dock`, and
`RunningState` branched on that position to lay its sub-panes out horizontally or vertically. The band's
utility half is one shape — wide and short — so the position is now a `BAND_DOCK_POSITION: DockPosition =
Bottom` constant and the side-dock layout branches are deleted. Keeping `Panel::position` would have left a
user-settable `debugger.dock: "left"` silently re-orienting a panel that is not in a dock: the orientation
is deterministic *because* the impl went, not merely as a side effect. The setting stays in the schema
(deleting it is a migration) but steers nothing, pinned by
`test_new_sessions_ignore_the_debugger_dock_setting`, and `settings_ui` no longer renders a control for it —
a working-looking dropdown over an inert setting is the dead-control trap.

**Ruling (2026-08-31): `debugger.dock` and `debugger.button` stay in the schema, inert, permanently — do not
re-open this.** Two plans have now declined to remove them and a third session re-derived the question from
scratch, so here is the settled reasoning. Both are genuinely dead: `dock` has exactly one reader,
`dap::send_telemetry`, and telemetry is disabled fork-wide, so it steers nothing even there; `button` has no
reader anywhere in the tree. Deleting them is a settings migration — a `migrator` entry plus a schema change
plus removing the telemetry field — for zero user-visible benefit, because an accepted-and-ignored key behaves
identically to an absent one. What was actually wrong was not their presence but that they **lied**: the JSON
schema's hover text still described a status-bar button and a dock position, so the settings editor advertised
working controls. `assets/settings/default.json` already annotated both as inert; the Rust doc comments on
`DebuggerSettings` and `DebuggerSettingsContent` now do too, which is what the schema actually renders. The
general form: **an inert setting is fine, a lying one is not** — when a fork disables the thing a setting
steered, annotate the doc comment that feeds the schema, don't just annotate the defaults file.

Two more consequences fall out of having no dock: panel-level zoom became an explicit swallow-the-action
no-op (there is no dock to zoom, and the band owns its own geometry), and the panel's "Close Panel" button,
which dispatched `workspace::ToggleBottomDock`, now hides the band's utility section instead. `RunningState`'s
test bootstrap has to install a real `SolutionBand` — with no band, nothing paints the panel at all.

### 98. Deleting the vertical dock strips also deleted a 40px correction term from the dock-resize math

The two 40px vertical strips that flanked the workspace hosted the ProjectPanel / OutlinePanel / GitPanel
toggles. Once those toggles moved into the project toolbar (task 7) the strips had nothing left to host, so
task 8 deleted them — **in that order, which is load-bearing**: deleting the strips first would have left the
project-zone panels with no affordance at all.

The subtle part is `DOCK_STRIP_WIDTH`. It was not only the strips' width: `dock_resize_target_size`
subtracted it when converting a pointer position into a dock size, precisely because the strips sat between
each side dock and the window edge. With the strips gone every dock is flush with its edge, so the
subtraction had to go with the constant — leaving it would have made every horizontal dock drag lag the
cursor by exactly 40px. `test_dock_resize_handle_tracks_cursor` now asserts the handle lands on the cursor
with no inset term, which is the assertion that catches a reintroduced offset.

`PanelButtons` shed everything only the strips used: the `vertical` flag, `new_vertical`, the 24px icon
size, the per-dock divider and the right-dock button reversal. The divider and the reversal existed to
separate dock buttons from the rest of the status bar and to mirror the right dock against the window edge;
all three docks' buttons now sit together at the toolbar's leading edge, where the toolbar draws the single
separator — and draws it only when there is a button group to separate, since all three toggles can be
hidden at once and are all absent while the panels load.

### 99. `ctrl-shift-a` toggles the band's dialog half, and is bound only in the `Workspace` context

The band's utility half had `ctrl-\`` from phase 2a; its dialog half had no hotkey. `console_panel::ToggleDialog`
is it, bound on all three platforms **only in the `"Workspace"` keymap context**. That scoping is the whole
design: `ctrl-shift-a` is already `editor::SelectAll` in the `"Terminal"` context, and on macOS
`editor::SelectToBeginningOfLine` in the broad `"Editor"` context. Binding it more widely would shadow both.
The consequence is deliberate and worth knowing before "fixing" it: the hotkey does **not** fire while the
terminal is focused. Collapsing the dialog from inside the terminal is not worth silently breaking select-all
in a shell.

Reopening a collapsed band needs a session to reopen *onto*, and that memory is a per-Solution
`last_dialog_session` map that is deliberately **not persisted**: the persisted `active_dialog_session`
already restores the non-collapsed case across a restart, so a second column for the same bit buys nothing.
The fallback chain is remembered session -> first entry in `tab_order` -> do nothing (a Solution with no
sessions has nothing to show).

**The trap the remembered id creates.** A remembered session that is later closed or purged leaves a dangling
id, and reopening onto it renders nothing while persisting the dead id into `solution_band_state` — after
which the next press "collapses" a dialog that was never showing, and the press after repeats the dead
reopen. Two independent guards, because either alone leaves a hole: `clear_active_dialog_for_session` scrubs
`last_dialog_session` *unconditionally* rather than only when the `band_state` scan matches (a Solution
already collapsed when its session disappears is otherwise never touched), and the toggle re-validates
whatever it reads with `session_can_be_active_dialog` — the same predicate `session_tab_strip` uses to decide
whether to draw a tab — before trusting it. The second guard is what holds against a teardown path that never
routes through the first.

### 100. The git panel's tabs are `Changes | Commit`; the graph pushes selections down by typed call and the panel signals closes back up by event

What: phase 3 of the Solution-band work (spec `docs/plans/2026-08-26-solution-band-ai-dialogs-design.md` §5, plan
`docs/plans/2026-08-30-git-panel-commit-tab.md`). The git panel's tab bar is now exactly **Changes | Commit**. The
History tab is deleted outright, and the git graph's inline right-hand commit-details sidebar is deleted with it.
Selecting a commit in the graph opens a closable **Commit** tab carrying the full commit message, a
`short hash · author · date` row, whole-commit +/− totals and a changed-files tree; double-clicking a file there opens
that file's diff **for that commit** in the centre pane (at the time `CommitView::open_file_diff`, reused verbatim — its doc comment
already named the graph's changed-files list as its caller). **The gesture and the tab identity changed on 2026-09-01 —
see #125**: double click now summons one shared diff into the pane's preview slot and single click retargets it, and
that call's only caller was the Commit tab (the doc comment's graph attribution was stale and was corrected). **And the callee changed
on 2026-09-02 — see #136**: `CommitView`'s single-file mode is deleted, and the Commit tab now opens
`SoloDiffView::open_commit_file`, the same view type the Changes tab opens. A multi-row graph selection renders a bare "N commits
selected" and loads nothing. The sidebar's building blocks — the changed-files tree, the commit-message split, the
markdown style, the client-side +/− fold — were **relocated, not rebuilt**, into `crates/git_ui/src/git_panel/commit_tab.rs`.

Why the sidebar had to go: phase 2b (#96, #97) moved the git graph into the Solution band's *compact utility half*. The
sidebar's `min_w(px(300.))` was the only min-width in the whole view, and deleting it is what reclaims the horizontal
width the band actually needed. Note that this is horizontal, not vertical: the spec described a bottom strip, but the
sidebar was a right-hand column (the bottom strip was the message region *inside* it).

**Why the two directions of communication differ — the dependency edge decides it.** `git_graph` depends on `git_ui`
and never the reverse (`crates/git_graph/Cargo.toml` has `git_ui.workspace = true`; `crates/git_ui/Cargo.toml` has no
`git_graph` entry). Down is therefore the only direction the shared code could move, and the two halves of the
conversation are asymmetric by necessity:

- **graph → panel is a direct typed call.** The graph may name `git_ui` types and already looks the panel up
  (`workspace.panel::<GitPanel>(cx)` off its `WeakEntity<Workspace>`), so it simply calls
  `GitPanel::show_commit_selection` / `close_commit_tab`. No string-named-action IoC trick, no new provider registry —
  neither is needed here and both are harder to test. The call is made through **`cx.defer_in`**, and that is not
  decoration: `GitGraph::select_entry` is reachable from `invalidate_state` and from the deserialize path, where a
  synchronous `workspace.update` is a double-lease panic.
- **panel → graph is a GPUI event.** `git_ui` may not name `git_graph` at all, so `GitPanel` extends its existing
  `pub enum Event` with `CommitTabClosed(Vec<Oid>)` and `GitGraph` subscribes. The subscription is installed in
  `GitGraph::new`, not once at panel construction, because `GitGraphPanel` re-creates its inner `Entity<GitGraph>` on
  every repository switch. **The payload is load-bearing:** the event reaches *every* `GitGraph` in the window, so a
  graph clears its selection only when the closing tab's shas equal its own `selected_commit_shas()`. That also subsumes
  the feedback-loop guard — with the synthetic local-changes row selected the graph's shas are `[]` while the event
  carries a real commit, so the mismatch already stops the bounce.

**`CommitSelectionSource` exists because a background re-anchor is not a gesture.** The graph re-anchors its selection
by sha after every refetch, and refetches are triggered by repository events nobody asked for — a `git fetch` landing in
a terminal, a branch checked out elsewhere. Those re-anchors reach `show_commit_selection` through the *same call* a
click does, so before the split a fetch would swap the panel body out from under a user who had gone back to Changes to
stage files and type a commit message. `UserGesture` re-activates the Commit tab (which is also what makes "select a
commit, switch to Changes, click that row again" work); `Background` refreshes an already-open tab in place, never
touches `active_tab`, and does nothing at all when the tab is closed. The only `Background` callers are the two
re-anchors in `on_repository_event` and the deserialize path's `select_commit_by_sha`. A re-anchor that *fails* — the
sha is gone after a `git commit --amend` — closes the tab instead, and only while it describes exactly that one sha.

**What was dropped, and where it still lives.** Ref chips and the "In N branches" line are gone from the commit-details
surface, along with `CommitBranches` / `format_branches_containing`; both remain in the full `CommitView`
(`commit_view/refs_bar.rs`, `commit_view/contains_panel.rs`), one click away — the same place the full sha already
lives. From the sidebar's context menu, Copy SHA and Copy Web URL survive on the graph's row menu and selection-copy of
the message survives as `ctrl-c` → `markdown::Copy`, but **"Copy Author Email" has no equivalent anywhere and "Copy
Message" degrades to "Copy Subject"**. Accepted deliberately: a message-block context menu on the Commit tab is new
scope beyond spec §5. Per-file +/− counts stay out of scope for the reason #55 already recorded — `CommitFile` carries
no numstat. The identity line also stopped being markdown: it is plain `Label`s now, so it is no longer selectable text
and `markdown::Copy` no longer reaches it; the email and time-of-day moved into the row's tooltip.

**The palette guard, and the class of hole a guard on the panel cannot close.** Narrowing `dispatch_context` makes the
Commit tab inert to the *keymap*, but the panel's `.on_action` registrations stay live, so palette-dispatching
`git::ToggleStaged` / `git::RestoreFile` / `menu::Confirm` would still act on the hidden Changes selection. The fix is
applied **at registration, not per handler**: one `let shows_changes_list = self.active_tab == GitPanelTab::Changes;`
and two `.when(shows_changes_list, …)` blocks cover 20 actions across 13 handlers (staging, restore, gitignore,
expand/collapse, all eight selection movers, `open_diff`, `open_accordion_diff`, `jump_to_source`). Unregistering rather
than guarding is safe only because nothing between the panel element and the window root handles any of them — the
other `git::ToggleStaged` / `RestoreFile` handlers live on `editor` and `project_panel` elements, which are not
ancestors. Repository-wide actions (`stage_all`, `commit`, `amend`, `stash_*`) are deliberately **not** guarded: they
target the repository, not the invisible selection.

**The trap: a guard that lives on the panel's own registrations is bypassed by any cross-crate reader of the panel's
selection.** `git::FileHistory` is registered by `git_graph` on the **Workspace** element and resolves its target
through `GitPanel::selected_file_history_target`, so it sailed straight past the guard — and a conditional registration
would additionally have leaked whether a hidden file was selected. It is fixed on the *panel* side, by
`selected_file_history_target` returning `None` off the Changes tab. `git_panel::FocusChanges` is the same class from
the other end: a direct call, so it now switches to Changes before focusing rather than moving a selection nobody can
see. The one deliberate exception is `GitPanel::select_entry_by_path`, which `project_diff` calls on every
`EditorEvent::SelectionsChanged { local: true }`: it stays unguarded because it is a *sync*, not a command — it changes
no repository state, opens nothing, and guarding it would only make the Changes list stale the moment the Commit tab
closes. It says so in a comment, so the next reader does not file it as the same bug twice.

**The vertical budget is solved by flex, not by arithmetic — and that is a GPUI constraint, not a preference.** At the
shipped dock height the tab body is ~282px. Pinning the message block at its 200px cap (`flex_shrink_0`) plus a
three-line identity row left 19px for a 28px header and **zero** for the tree. The obvious fix — read the available
height and take a fraction of it — is not available: that height is only knowable during layout, where a `cx.notify()`
is *discarded* (`Window::invalidate_view` returns false while `draw_phase != None`) and a re-derive-and-notify loop
spins. So the layout states three floors and lets flex do the arithmetic: the message block drops `flex_shrink_0` and
gains `min_h(COMMIT_MESSAGE_MIN_HEIGHT)`, the tree swaps `min_h_0()` for `min_h(COMMIT_FILE_TREE_MIN_HEIGHT)`, and the
tree's explicit floor freezes it during the shrink pass so the message gives back exactly the shortfall (measured at
1080: message 200 → 157, identity 63 → 25, tree 0 → 72; a one-line commit instead leaves the message at its 44px
content height and the tree *grows* to 185 — the cap is not a floor). **An explicit `min_h` is load-bearing:** a flex
item's automatic minimum size is its content, so without one neither child can shrink at all and the overflow returns.

How to apply:

- **When two crates in a dependency chain must talk both ways, do not reach for an IoC registry by reflex.** Call
  downward directly and event upward. Reserve the string-named-action / provider-registry tricks for the cases where
  the *downward* direction is the one that is blocked.
- **A typed call from inside one view's `update` into another view must be deferred** (`cx.defer_in`) whenever the
  calling method is reachable from an invalidation or deserialize path. Compiles clean, panics at runtime, and a
  `VisualTestContext`-shaped unit test will not catch it.
- **An event that fans out to N subscribers needs a payload identifying who it was about.** "Clear yourself" is only
  correct when there is exactly one of you.
- **Split a push API by *source* the moment a background refresh and a user gesture share it.** The symptom is always
  the same: the UI moves under a user who did not touch it.
- **Ephemeral by design.** The Commit tab is never persisted and never restored; the panel keeps booting on Changes.
  The graph's own `selected_sha` *is* persisted and could re-drive it later if the maintainer asks, which is why no
  `SerializedGitPanel` migration was worth writing.
- The Commit tab lives as an `impl GitPanel` block in a sibling module (`commit_tab.rs` beside `changes_list.rs`), not
  as its own `Entity` — the grain the Changes tab already set. It hangs off `GitPanel` as one `Option<CommitTabState>`,
  so `Some` *is* the tab's presence in the tab bar and "open" cannot drift from "has something to show".
- The row renderers take their host behaviour as `Rc<dyn Fn>` closures (`ChangedFileRowHandlers`). **Those may only be
  installed into event callbacks**: the erasure means a `GitPanel` double-lease is not a compile error, and unlike the
  graph the Commit tab renders under a live `Context<GitPanel>` lease. Build the bundle from `cx.weak_entity()` at the
  top of `render`, never inside a nested `update`, and never invoke one during layout/prepaint/paint.
- **An always-active tab needs an affordance of its own.** With the Commit tab closed the lone "Changes" tab painted as
  a bare centred label above the toolbar and read as a section header, because `render_tab_bar` styled only the
  *inactive* branch. The active tab now carries a 2px `border_focused` underline — the same idiom as the solution
  band's project tabs (`crates/solutions_ui/src/project_tab.rs`).

### 101. Entry-persist-chain disposal is stated by each teardown caller, and a drained chain outlives its session under its own key

What: every AI session has at most one `entries_persist_chain` entry (`crates/solution_agent/src/store.rs`), a
`PersistChain` holding the **outermost** link of that session's serialized entry-row write chain. Each link moves the
previous `Task` into its own future, so the map entry transitively owns the whole chain and dropping it cancels *all* of
it, not just the last hop. The chain exists because GPUI detached tasks have no FIFO guarantee — that is the phase-6b
keystone bug, two `upsert…` + `delete_entries_from(main_len)` pairs racing over the same rows. `evict_session_runtime_maps`
(`store/teardown.rs`) used to drop that entry on **every** teardown, with a comment asserting the cancellation as a fact
of teardown. It now takes an explicit `ChainDisposition::{Drain, Abandon}`: `close_session` and `cold_close_solution`
drain, `purge_session_hard` and `purge_solution_fully` abandon (loudly — the old drop logged nothing at all). Plan:
`docs/plans/2026-08-30-entries-persist-chain-teardown.md`.

Why draining is not optional. Closing a chat tab or a Solution window discarded in-flight transcript writes
**permanently and silently**: `persist_all_rows` / `persist_main_stream` advance `persisted_main_seq` synchronously
*before* they spawn, and every persist filters `mod_seq > watermark`, so "a later persist catches up" is false — there is
no later persist that re-picks those rows. There is also no persist debounce (`persist_main_stream` runs on every ingest
event; the 500ms/2s `entry_update_throttles` govern only the MCP emit), so the loss is bounded only by how far the event
stream had outrun sqlite.

**Why the disposition is stated per caller and never inferred inside the evictor.** `purge_session_hard` evicts the
runtime maps *before* it issues `db.purge_session`, and the two are unordered background work over the same connection.
Drain a hard purge and a queued link runs after the cascade DELETE and re-inserts entry rows for a session that no longer
has a `solution_sessions` row. Nothing enumerates entry rows without a session row, so those orphans are invisible to
every UI — and `delete_by_solution` sweeps via `session_id IN (SELECT id FROM solution_sessions WHERE solution_id = ?)`,
so the solution-level purge cannot reach them either. There is no orphan reaper anywhere in the crate. The old
unconditional drop is what had been preventing this, which is exactly why a generic "just detach it" fix ships the bug.

**Why a drained chain stays under its KEY rather than being handed off with `.detach()` — the non-obvious part, and a
Critical that shipped and was reverted.** The first cut (`76be3e00fa`) drained by detaching, reasoning that the key had
to free synchronously because a *deferred* eviction would race a close→reopen that re-keyed the same
`SolutionSessionId`. True, but one-directional: freeing the key lets the reopened session build a **second chain with
nothing ordering it against the first**, so the close flush's trailing `delete_entries_from(old_main_len)` can land after
the new chain's tail upsert and delete the user's new message — the phase-6b bug reintroduced at the close/reopen seam.
Close a tab, reopen it, type one message, and that message was absent from disk and gone at the next cold load. Measured
A/B on one fixture: a 3-entry transcript closed, reopened and appended to yields **4 rows on the parent commit and 3 on
`76be3e00fa`**; at 200 entries, 201 vs 200. The window was wide because `persist_all_rows` awaited one background round
trip *per row* — it is a single batched write now (last bullet below), which narrows the window without touching the
ordering argument. The fix (`f851e02f97`) is retention: `Drain` removes nothing, so the reopen's first persist finds the
flush already under the key and takes it as its `prev` for free, and the deferred-eviction race never arises because
nothing is deferred. This is viable only because a chain link **captures no entity** — rows, length, epoch and
change_seq are snapshotted synchronously before the spawn and `db` is an `Arc` — so a link that outlives its session can
neither touch a dropped entity nor read stale in-memory state.

**The map is bounded by a spent-chain sweep, not a generation counter.** Each link flips an `Arc<AtomicBool>`
(`Release`) as its last act, with no `.await` after it; `retire_finished_persist_chains` `retain`s on the paired
`Acquire` load. Removing a chain that has already run can neither cancel work nor reorder anything, so it is the one
removal that is unconditionally safe. The safety argument rests on the map always holding the **outermost** link:
`finished == true` there implies every predecessor completed. The chain runs on a background thread (decision 103), so
the guarantee is carried by the `Release`/`Acquire` pairing rather than by same-thread reads: seeing `true` means seeing
every write the chain made, and seeing a stale `false` only postpones reclaiming a spent key.

**`close_session` needed `cold_close_solution`'s `is_live` gate in the same change, because the two bugs point in
opposite directions.** Bug 1 is loss of writes that should happen; bug 2 is execution of a rewrite that should not.
`persist_all_rows` is a *full* Main rewrite whose trailing trim past `main_len` deletes the teammate-tagged rows of a
pre-2026-07-06 ("legacy") row layout. While the flush was being cancelled, `close_session` calling it unconditionally
was unobservable; repairing the cancellation makes it live, so closing a restored, never-resumed tab would rewrite a
session the user never touched. Gated on `acp_thread().is_some()`, mirroring `cold_close_solution`.

**The legacy realign is intended, not something the gate avoids.** `hydrate_streams_main_only` deliberately arms the same
trim past `main_len` at cold-load time, and `legacy_teammate_tagged_rows_realign_to_main_local_on_cold_load`
(`store/tests/hydration.rs`) asserts the teammate rows *are* deleted. Repairing the flush extends an accepted truncation
to sessions that were merely restored; the gate protects **untouched sessions**, not the row layout.

How to apply:

- **When one function is shared by a keep-the-rows caller and a delete-the-rows caller, make the difference a parameter,
  not a heuristic.** "Is the session live" and friends are the wrong axis here — a soft-closed session is not live either,
  and it must still drain.
- **Cancelling a `Task` you hold is not the same as the work not happening.** Only the outermost link's handle is in the
  map; the inner ones sit inside their successors' futures and are released only as those successors run, so an abandon
  walks inward one runnable at a time and deeper chains keep writing (measured: 2 links leak 1 row, 8 leak 5). If a
  cancellation must be *complete*, sequence the delete after it — do not pick a different disposition.
- **A serialization chain keyed by an id is only serialized while the key survives.** Anything that frees the key while
  work is queued under it — `.detach()`, a deferred evict, a "clean up on close" sweep — permits a second, unordered
  chain the moment that id comes back. Reclaim such a key only when the work under it has provably finished.
- **A completion flag on a chain is cheaper and stronger than a generation counter**, provided the map holds the
  outermost link. Set it as the future's last act with no await after it, read it on the executor's own thread, and the
  only removal it authorizes is a no-op one.
- If a persist path advances a watermark before it spawns, treat the spawned work as **unrecoverable if dropped**. Look
  for the watermark before assuming a retry will cover a cancelled write.
- **A DB helper awaited in a loop is a torn read waiting to happen.** A flush goes out as one
  `SolutionAgentDb::upsert_entries_and_trim` — every row *plus* the trailing trim past `main_len`, under a single
  connection acquisition and savepoint — rather than N `upsert_entry` awaits followed by a separate
  `delete_entries_from`. Each of those awaits was an `executor.spawn` that released the connection, and every reader
  takes that same lock, so a concurrent `load_entries` could observe any prefix of the flush; cold load derives
  `persisted_main_seq` from the short read and the session's next persist trims the rest away permanently. Measured
  before/after on the same fixture: a 200-row flush cost **407 executor turns and exposed 38 distinct partial row sets**,
  now **7 turns and two states** — the same 7 a 4-row flush costs. The trim rides inside the same write on purpose: the
  gap between the last upsert and a separate trim holds a *fresh head followed by a stale tail*, and cold load accepts
  that splice as authoritative under **either** branch of `hydrate_streams_main_only`'s `entries.len() == main_len`
  check. A stale tail is normally untagged (the write that left it was itself a Main-local flush), so `demux` routes
  every row into Main, the counts match, the layout is not read as legacy at all, and the watermark is seeded from the
  spliced transcript with no realign armed; and if `push_coalesced` does merge across the seam so the counts differ,
  "legacy detected" only means the watermark is seeded to 0 and the full rewrite that arms writes the splice back
  permanently. **This narrows the close→reopen window,
  it does not close it** — a reopen ordered entirely before the flush still hydrates the stale rows and still trims the
  tail afterwards; closing that needs hydration ordered behind the chain, which `hydrate_all_for_solution` cannot do
  while it is `&self` and the chain's completion signal is an `AtomicBool` rather than a cloneable handle.

### 102. The visual dump's rows are contributed by the crates that own them, not synthesized by `solutions`

What: `solutions::mcp::visual_structure` (which backs `workspace.dump_visual_structure` /
`windows.dump_visual_structure`) now emits a `ProjectToolbar` row and a `SolutionBand` row, spliced into the workspace
column in painted order. Neither node is built there. `StructureSlot::{ProjectToolbar, SolutionBand}` plus
`register_structure_provider(cx, slot, provider)` is a pull-based global registry; `title_bar::init` and
`solution_agent::init` each register a closure that downcasts the relevant `Workspace` `AnyView` slot
(`project_toolbar_item` / `solution_band_item`) back to its concrete type and calls a `structure_node` method living
next to that type's `render`. `VisualNode` gained an `attributes: BTreeMap<String, serde_json::Value>` bag (omitted
when empty, so pre-existing nodes serialize unchanged) for facts that do not fit `kind` / `label` / `visible` /
`focused` — band height, effective height, divider ratio.

Why not build the nodes in `solutions`. It cannot: `solution_agent` depends on `solutions` (so the edge cannot run back),
`title_bar` reaches `solutions_ui` / `git_ui` / `run_config_ui`, and both rows reach `Workspace` only as an `AnyView`.
Inverting the direction — the owner registers into the dump — is the only arrangement in which the node builder can see
the state its own `render` sees. The alternative that was actually tried in this file's history and abandoned is worse:
`build_title_bar_node` and `build_status_bar_node` both used to hand-synthesize children from whatever state
`solutions` could reach. Only `build_title_bar_node` actually drifted: `TitleBar::render` replaced the project-info
chain wholesale with the solution tab strip, and the dump went on reporting `SolutionSegment` / `ProjectName` /
`Branch` until `1cc929c9f3` cut them. `build_status_bar_node`'s synthesis was cut in `3861a2adfa`, the same commit
that deleted the status-bar item it described, so it never had the chance to diverge — and it is not a bare presence
node either, it still reports `visible` from `Workspace::status_bar_visible`. Both carry a comment saying a wrong
child is worse than no child. The registry is how a row gets real children without re-acquiring that failure mode.

How to apply. Adding a row to the dump = add a `StructureSlot` variant, splice it at the right index in
`build_workspace_node` (child order is the window's vertical order and agents read it that way), and register a
provider from the owning crate's `init`. Build the node's `visible` flags from the *same helper* `render` gates the
child on rather than a re-derivation — `ProjectToolbar::{repository_selector_state, branch_summary, unpushed_commits}`
were extracted out of the three `render_*` methods for exactly this, so the probe and the paint cannot disagree. Where
the content is a type-erased slot the owning crate also cannot see into (the band's utility occupant, the run-config
strip), say so in the payload with `occupant_introspectable: false` instead of guessing; the band's utility node
additionally separates `visible` (what is painted) from `requested_visible` (the persisted `utility_visible` toggle the
status-bar buttons and `ctrl-`` flip) and reports `occupant: registered | hidden | pending | unavailable`, mirroring
`SolutionBand::render`'s own four-way `match` — collapsing those two booleans into one would make a still-loading
occupant indistinguishable from a failed one, which is the same distinction decision #95 exists to preserve.

Providers run under the dump's live `&Workspace` borrow, so each `structure_node` takes that reference as a parameter
instead of upgrading its own weak handle — the same double-lease discipline `ProjectToolbar::new` and
`SolutionBand::{set_utility_visible, activate_utility_kind}` already follow.

## Where specs and plans live

`docs/superpowers/{specs,plans}/` is in `.gitignore` — these are personal working notes, not committed. Each major fork feature has a design spec + step-by-step implementation plan there. They're append-only history; the canonical state of the code lives in code + this file + `.rules`.

If you're picking up a feature mid-stream and the specs are missing locally, recover from git history:

```sh
git log --oneline --all --diff-filter=A -- 'docs/superpowers/specs/*'
```

(Will show empty if no specs were ever committed — that's the steady state for this fork.)

## Memory of subagent dispatches

Some sessions used `superpowers:subagent-driven-development` to land features task-by-task. Those agents make local-pragmatic deviations from plans. Notable plan-vs-code deviations worth knowing:

- `solution_agent::SolutionSession.acp_thread` is `Option<Entity<AcpThread>>`, not `Entity<AcpThread>` — reflects the real lazy-construction lifecycle.
- `solution_agent::SolutionAgentStore::create_session` takes `project: Entity<Project>` (Plan B). Synthetic single-worktree project per session was rejected as too coupled to `Arc<Client>` / `UserStore` / etc.
- `solution_agent` registers `AgentServer` instances via `store.register_agent_server(id, Rc<dyn AgentServer>)`, not via global `AgentServerStore::get_external_agent`. The wire-up call lives in `solution_agent::init`.
- MockAgentServer in `solution_agent::test_support` uses `unsafe impl Send` because the trait requires Send but holds non-Send test state behind a Mutex. Test-only.
- `solutions_ui::ActiveProjectSelector` (Phase 3) hosts its two popovers via `ui::PopoverMenu<MemberPicker>` / `PopoverMenu<AddProjectPicker>` rather than the manual `anchored()` + `deferred()` + stored-trigger-bounds pattern from `solution_picker_dropdown.rs`. PopoverMenu encapsulates bounds tracking, dismiss subscription, and z-order; nothing the manual pattern provides was needed.
- `ActiveProjectSelector::new` defers its initial `rebuild()` via `cx.spawn(...).detach()` instead of running it synchronously. Reason: panels (`ProjectPanel::new`, `GitPanel::new`) are constructed inside `workspace.update_in(cx, ...)`, which holds a mutable borrow of the `Workspace` entity. The selector's `rebuild()` reads the workspace via `active_solution_in_workspace`, which would panic with "cannot read X while it is already being updated." The defer pushes the first rebuild to the next event-loop turn, after the construction `update_in` has finished. Side-effect: the trigger renders once with the empty-state label ("No project") before the deferred rebuild populates real members; acceptable.
- **Branches popup action header + row polish (updated 2026-07-02, `df60866dd0` / `d9cc9a2462`).** The popup is **branch-only**: Update/Push moved to dedicated project-toolbar buttons and Commit goes through the git panel (decision #27 git-followup), so the action header is just **New Branch** + **Checkout Tag or Revision…**. New Branch dispatches `zed_actions::git::Branch` (the branch-*create* picker). **Checkout Tag or Revision…** opens a dedicated `CheckoutRevisionModal` — a single-line tag/branch/SHA input that runs `Repository::checkout_revision` (the same op the popup's tag rows use), opened via `workspace.toggle_modal` after the popover dismisses. It previously shared New Branch's `git::Branch` dispatch, but that picker only *creates* branches (typing a tag name just proposed `Create Branch: <text>`) and could not check out a tag/revision by name — the reported bug this fixed. Other popup polish (piece 6): no header row (the search field is first and focused on open — `Focusable` returns the query editor's handle, not the container); empty `favorites`/`backups` sections are hidden unconditionally and, during a search, any zero-match section is hidden; tag & backup rows are indented one level so they nest under their section header (branch/group rows already were).

## Updating this file

Add to FORK.md when:
- A new fork-local crate is added.
- A new upstream file gets its first local modification.
- A non-obvious architectural decision is made — record the *why* before it gets lost.

Don't add:
- Per-crate module layout / data flow / type catalogs — those go stale fast and the agent can read the code. Rules are "traps to avoid", not "maps to follow".
- Long-term TODOs — use issues for those.
- Status updates — the git log is canonical.

### 103. The entry-persist chain runs on the BACKGROUND executor so that app quit can drain it

What: `persist_all_rows` / `persist_main_stream` spawn their chain links with `cx.background_spawn` rather than
`cx.spawn`, and `SolutionAgentStore` registers an `on_app_quit` observer
(`flush_persist_chains_on_quit`) that takes the whole `entries_persist_chain` map and awaits every link that has not
finished. Before this there was no quit hook in the crate at all: the store global died with the process and every
queued entry-row write was cancelled — silently, and permanently, because the persist helpers advance
`persisted_main_seq` synchronously before they spawn and every persist filters `mod_seq > watermark`, so no later
persist re-picks those rows. Quitting the editor mid-turn truncated the tail of the conversation.

**GPUI's quit contract, which is what forces the design.** `App::shutdown` invokes each quit observer synchronously
(entities and globals still alive, windows not yet cleared), collects the futures they return, clears the windows,
`flush_effects`, sets `quitting = true` — and then BLOCKS the main thread on those futures for at most
`gpui::SHUTDOWN_TIMEOUT` (200ms) before the process exits. The block goes through
`LocalExecutor::block_with_timeout`, which passes the FOREGROUND session id to the scheduler, so for the whole quit
window that session counts as blocked and **no foreground runnable makes any progress**: in production
`LinuxDispatcher::dispatch_on_main_thread` only enqueues onto a channel the parked main thread has stopped draining,
and `TestScheduler::step` models the identical rule by excluding runnables whose session is in `blocked_sessions`.
Background runnables keep running on their own threads throughout. So a quit-time drain is possible *only* for work that
is not on the foreground executor; awaiting a foreground `Task` from a quit observer parks until the timeout, logs
`timed out waiting on app_will_quit`, and loses the write anyway.

**Why moving the chain is legitimate rather than a workaround.** A chain link never needed the main thread. It captures
no entity — rows, length, epoch and change_seq are snapshotted synchronously before the spawn and `db` is an
`Arc<SolutionAgentDb>` whose every operation is itself a background task — and its ordering comes from `prev.await`,
not from executor FIFO (that is the whole point of the chain; see decision 101). `Task<T>` is `Send` when `T` is, so a
link can own its predecessor across threads. The one property that changes is that `PersistChain::finished` is now read
from a different thread than the one that sets it, which the existing `Release`/`Acquire` pairing already covers.

**Why the quit hook may take the entire map.** Disposition is already resolved by the time it runs:
`ChainDisposition::Abandon` REMOVES a purged session's chain before the purge issues its cascade DELETE, so nothing left
under a key can resurrect rows for a deleted session. Emptying the map cannot cancel anything either — the tasks are
moved into the returned future, which owns them until they finish.

**Why the map is taken on the future's FIRST POLL rather than when the observer runs.** `App::shutdown` collects the
quit futures BEFORE it clears the windows and calls `flush_effects()` — and that flush is itself a persist site.
Releasing the `MultiWorkspace` fires the `cx.observe_release` in `solutions::event_sources`, which reaches
`SolutionStore::mark_closed` → `SolutionStoreEvent::Closed` → `SolutionAgentStore::cold_close_solution` →
`persist_all_rows`. A hook that snapshots the chain map while the observer runs is therefore always exactly that one
flush short, and in the common case (nothing else in flight) its future is Ready on the first poll, so the process exits
without the write getting a single background turn. `entries_persist_chain` is consequently a `PersistChains` newtype
over an `Rc<RefCell<HashMap<…>>>`: the observer clones the handle, and the `async move` body — which does not run until
`block_with_timeout` first polls it, after `flush_effects` — is what takes the map. The future cannot reach the store
any other way: `shutdown` holds `&mut App` across the whole timed block, so an `AsyncApp::update` inside a quit future
panics on the re-borrow. That same borrow is what makes the deferred take sound — no store method can run while the main
thread is inside `shutdown`, and the chain links never touch the map, they only own their predecessor. Pinned by
`app_quit_flushes_a_chain_issued_during_shutdown`, whose fixture releases a window root view (the `MultiWorkspace`
stand-in) from `shutdown`'s own `windows.clear()`; snapshotting at observer time leaves the rows `["stale","stale",
"stale"]`.

**What it deliberately does not do.** It drains; it does not re-derive a fresh flush from the live sessions. The chain
is already the complete record of what is unwritten (there is no persist debounce — every ingest event issues a
persist), so re-deriving would buy nothing and would rewrite sessions the user never resumed, which is exactly what
`close_session`'s liveness gate exists to prevent.

How to apply:

- **Establish the quit contract from the code before designing anything that has to survive quit.** "Register an
  `on_app_quit` and await your work" is only half true: the answer depends entirely on which executor the work is on.
  `session::app_will_quit` awaits `background_spawn` work and is sound. Of the 14 `on_app_quit` registrations outside
  `gpui` itself, `MultiWorkspace::app_will_quit` is the only *unconditionally* foreground-bound one — it awaits `_serialize_task` plus `pending_removal_tasks`, and `serialize` is a
  `cx.spawn` whose body does `this.read_with` and then `cx.update`, so if any of them is still pending the hook can only
  burn the 200ms. **It is nonetheless defended, by an explicit foreground pre-flush on the main quit route:** `zed::quit`
  collects `Workspace::flush_serialization`, `MultiWorkspace::take_pending_removal_tasks` and
  `MultiWorkspace::flush_serialization`, `join_all`s them on the foreground, and only then calls `cx.quit()` — so both
  vectors are already empty by the time the observer runs. The other `cx.quit()` routes fire only once no
  `MultiWorkspace` window remains, where the weak handle is dead and the hook is a no-op. **That pre-flush is a
  legitimate second pattern** and was an available alternative design for the persist chain; it was not taken because it
  does not cover a platform-initiated quit (an OS logout that bypasses `zed::quit`), which is exactly the residual
  exposure `MultiWorkspace` still carries. What is at risk there is the `multi_workspace_state` KV blob — window chrome:
  active workspace, project groups, sidebar — not the pane layout in the `workspaces` table and not the session binding,
  which is a `background_spawn`; and with a dozen-plus eager, undebounced `serialize` call sites the pending delta is
  seconds old at worst. Recorded, not fixed here. (`LspStore::shutdown_server` was the second instance and is
  **fixed**: its `LanguageServerState::Starting` arm used to unconditionally `await` a foreground `cx.spawn`ed
  `startup`, so a quit while any language server was still starting burned the whole timeout and shut *that* server down
  not at all — `join_all` cannot resolve until every member does, so the hook never completed and consumed the
  entire budget every other quit observer shares. (The `Running` arms' futures are background-driven and were
  still polled concurrently inside those 200ms; what was lost was the starting server's shutdown, and everyone
  else's time.) It now splits that arm on `Task::is_ready()`. **A
  still-pending startup is dropped, not awaited**, because when the only options are "await foreground work that cannot
  run" and "drop it", dropping is strictly better: both produce zero shutdown effect and the same orphaned child
  (`kill_on_drop` does not help — `Drop for LanguageServer` hands the kill to a *detached* background task that never
  gets scheduled before the process exits), but only one of them costs the entire 200ms budget that every other quit
  observer shares. **An already-finished startup is still awaited and still shut down**, because a finished task
  resolves from its stored output with no scheduler involvement, so the blocked session cannot stall it, and
  `LanguageServer::shutdown()` is background-driven; skipping it would have orphaned a fully initialized server. Mind
  that `Task::is_ready()` is `CLOSED | COMPLETED`, not "completed successfully" — awaiting a closed-without-output task
  panics — but no route to that state survives a normal `App::shutdown`, so the guard is sound. The `Running` arm is
  background work and was always fine. Pinned by
  `crates/project/tests/integration/lsp_store.rs::{test_quit_does_not_await_a_starting_language_server,
  test_quit_shuts_down_a_language_server_whose_startup_finished}`, which block on the hook through
  `cx.foreground_executor().block_with_timeout` — the same call `App::shutdown` makes — with the tick budget pinned via
  `set_block_on_ticks`, because the `0..=1000` floor of zero polls documented below lets the block return without
  polling the quit future at all, which it reports as *not completed*. The *non-quit* stop path
  (`LspStore::shutdown_language_server`) legitimately still awaits `startup` against a 5s timer, because there the
  foreground executor is alive. `MultiWorkspace::app_will_quit` is now the only remaining unconditionally
  foreground-bound hook.)
- **Foreground is not the default for durability work.** If a future captures no entity and its ordering is explicit,
  put it on the background executor; that is the difference between surviving a quit and not.
- **A `TestAppContext` test can prove this**, because `TestScheduler` models the blocked-session rule. Issue the chain,
  do NOT `run_until_parked`, call `cx.quit()`, then read the rows with `load_entries_blocking`. Pin the tick budget with
  `cx.executor().set_block_on_ticks(usize::MAX..=usize::MAX)` first — a timed block otherwise draws a random budget and
  the drain is randomly cut short. The budget every `TestAppContext` actually draws from is `0..=1000`, hard-coded in
  `TestDispatcher::new`: **the floor is zero polls**, so an unpinned test can quit without the future being polled at
  all. (`TestSchedulerConfig::default()`'s `1..=1000` is reachable only from the scheduler crate's own tests — do not
  quote it as gpui's budget.)

### 104. The git graph's Date and Author columns are sized from their content, and the width they are sized against is measured by the view itself

Why: the log's three columns were fixed fractions of the table — `0.74 / 0.13 / 0.13` — and no single fraction can serve the two widths the same view is used at. The Date cell's text is a constant ~130px (`format_timestamp` renders `[day] [month repr:short] [year] [hour]:[minute]`, always a two-digit day, a three-letter month, a four-digit year and `HH:MM`), which is ~20% of the Solution band's compact half but only ~7% of a full-window pane item. So 0.13 truncated *every* row to `30 Aug 2026 12…` in the band while, at 1920, handing that same 132px string a 250px column — ~118px of dead whitespace, with Author wasting another ~113px beside it. Retuning the fraction just moves the failure between the two widths. There is no horizontal scroll and no table min-width, so nothing else absorbs the error.

How to apply:
- **Date and Author are sized to their content and Description absorbs the remainder** (`default_column_fractions`), which is what IDEA's log does. Date is measured exactly, from a `DATE_COLUMN_SAMPLE` whose shape `test_date_column_sample_matches_the_formatter` pins to the formatter's real output — change `timestamp_format` and that test tells you to change the sample. Author has no fixed width, so its width is a *default* measured from a sample name rather than from the loaded commits: deriving it from the widest author on screen would resize the column as rows stream in from `fetch_commit_data`, and a column that jumps while you scroll is worse than one that is slightly too wide.
- **The widths must go through `RedistributableColumnsState`, not just through the rendered `ColumnWidthConfig`.** The header, the rows, the resize handles' painted positions *and* `on_drag_move`'s arithmetic all read that one entity. Overriding only what is rendered leaves the dividers painted where the drag math does not believe they are, and the first grab jumps the column sideways. The state exposes no width mutator, so a new state entity is installed instead — carrying `set_cached_container_width` across, or `graph_column_width` reads zero and the graph snaps to its uncapped natural width for a frame.
- **A user drag wins, and it is re-checked rather than latched.** "`preview_widths` still equals `initial_widths`" is the test for an untouched table; while it differs the derivation keeps out of the way. Double-clicking a divider can hand the table back, but only when that restores the preview to the initial widths *exactly*: `reset_column_to_initial_width` restores the one column and redistributes the difference onto its neighbours, so once several dividers have been tuned there is no guarantee any sequence of double-clicks gets back, and there is no explicit "reset to auto" affordance. Treat automatic sizing as something a drag ends for that view's lifetime unless the user happens to land back on it.
- **The derived widths are cached against every input, not just the table width** (`ColumnWidthInputs`). The two measurements scale with `rem_size` and the UI font, and neither of those moves the table's width — so a width-only key stays satisfied across a font-size or theme change and the columns keep the *previous* font's sizing. That reproduces the exact truncation this entry is about (`14 May 202…`, `Firstname L…`), and nothing short of a resize repairs it, which makes it indistinguishable from the bug being unfixed. Keying on the derivation function's own arguments cannot go stale by construction; keying on what the measurement happens to read today has to be revisited whenever it reads something new.
- **The header's `ElementId` seed must be an entity that outlives the columns.** `render_table_header`'s `entity_id` parameter only seeds the header cells' element ids, so it takes `table_interaction_state` (as `data_table`'s own call site does) rather than the column-widths entity, which is *replaced* on every re-derivation and would discard each cell's retained hover/active state on every resize. The entity swap is otherwise safe — nothing observes or subscribes to `RedistributableColumnsState` — with one narrow exception: between a divider's MouseDown and its first drag-move the preview still equals the initial widths, so a swap there is not gated out and the in-flight `DraggedColumn`'s `state_id` stops matching what `bind_redistributable_columns` guards on, killing that drag until the divider is re-grabbed.
- **The table's width is measured by the view's own `on_children_prepainted`, not read off `cached_container_width`.** Both land during the draw, but nothing notifies when the cached value does, so after a window or band resize the table kept the *previous* width's shares until some unrelated event happened to redraw it — visible as a full-width layout squeezed into a half-width band. `GitGraph::observe_table_width` defers a notify out of the draw phase (gpui discards invalidation raised inside one, #62) and guards it on the width having actually changed, or it re-arms every frame and spins. Note that `Div::on_children_prepainted` holds a *single* listener, so the observer sits on a wrapper div outside the one `bind_redistributable_columns` claims.

### 105. `solution_agent`'s read RPCs serve a closed session from a detached cold entity, and never by hydrating the store

What: `solution_agent.get_session` and `solution_agent.get_session_changes` used to resolve only through `store.sessions`, so after a window close both returned `session_not_found` until something else re-hydrated the store — even though every row was still in the database. They now fall back to a shared `load_cold_session`, which rebuilds the session through `store::build_cold_session` (the *same* constructor `hydrate_all_for_solution` uses), reads the answer off it, and drops it.

Why: the obvious alternative — call `hydrate_all_for_solution` the way `list_sessions` does — is wrong twice over. It needs a solution id the caller does not have, and it deliberately refuses rows with `closed_at` set, so it can never serve a session the *user* closed, which is the whole case. Mirroring `read_session_history`'s ad-hoc rows→blob→title fallback was rejected too: that path recovers only entries and a title, while `GetSessionResult` carries ten-plus metadata fields it never loads. Sharing hydration's own constructor is the reuse that cannot drift.

**Both RPCs had to move together, and that is the load-bearing part.** Fixing only `get_session` created a worse failure than the one it fixed: a client would render a transcript served from the cold path, then hard-error on its first delta poll with the `(epoch, current_seq)` cursor that same call had just handed it. Any *third* read RPC that grows a cold path must go through `load_cold_session` for the same reason — so they all agree on whether a closed session exists and on the cursor they issue. `get_session_entry` is the known outstanding one.

How to apply:
- **A cold read is a pure read.** The entity is constructed, read and dropped; nothing is inserted into `store.sessions` or `by_solution`. That is what lets these RPCs serve a user-closed tab where `list_sessions` cannot, and it is asserted by test. Do not "optimise" it into a hydration.
- **Serve absent fields as absences, never as guesses.** A cold session has no subprocess, so `state` is `Idle`, `max_tokens` is `None`, `pending_bundles` is empty, teammate streams collapse to Main, and an in-flight tool call's authorization options are empty. Three assertions pin the ones a future refactor could start filling with persisted-but-wrong values.
- **Pin the cursor, not just self-consistency.** `epoch` and `mod_seq` are the wire contract. Tests that only compare a full load's cursor to a delta's own agree with each other under a renumbering or an epoch-policy change and stay green while every cached client cursor is forced through a spurious reset. Pin the literal values, and pin a *partial* delta (`since_seq` mid-transcript) — a max-preserving interior renumbering survives everything else.
- **"No rows + `epoch > 0`" is load-bearing: it means the session was row-native and got wiped.** `build_cold_session` consults `acp_thread_blob` whenever a session has no entry rows, and `/clear` (`reset_context`) and `/compact` (`rotate_context`) both delete every row through `persist_all_rows` while nothing used to clear the blob — `save_blob` had no production caller. So a legacy session that was migrated to rows and later wiped had zero rows *permanently*, and **four** separate reconstruction paths replayed the pre-wipe transcript: `build_cold_session` (desktop restore and the cold `get_session`), `resume_session`'s fresh-entity branch — the reopen-from-History path, and the most user-visible of them — and `read_session_history`. The user wiped a conversation, reopened the tab, and it was back; the epoch also moved backwards. Fixed on both sides: the wipe now drops the blob **inside the same savepoint** as the row deletion, and all four readers go through one `is_wiped_row_native(rows_empty, epoch)` predicate, which also repairs sessions an earlier build already broke, on read, with no migration.
- **That predicate is an inference, not a stored fact, so its inputs have to stay honest.** The `epoch` column is nullable with no `DEFAULT` and `insert_or_update_metadata` never names it, so a never-persisted session reads `NULL` → `0` → "legacy, read the blob". Three writers can move it (`persist_all_rows`, `persist_context_wipe`, `persist_main_stream`), and the guarantee they must preserve is that **the epoch never outruns the rows it describes**. Two things enforce that, both added because the guard made them load-bearing rather than merely tidy: a failed entry write no longer lets `save_epoch` run, and — since the persist chain is `background_spawn`ed (#103) and cannot touch an entity — a failure rolls the in-memory watermark back through an `Arc<AtomicBool>` that the next *foreground* capture reads, with the epoch save additionally gated on that flag so a chained successor cannot save an epoch over a table its predecessor left short. **If this ever needs to be exact rather than inferred, add a `row_native` column; do not invent a cleverer epoch test.** The flag must only ever be cleared on the foreground — clearing it from a background path silently disables the rollback, and a mutation that `swap`ped instead of loading it passed every epoch assertion while doing exactly that.
- **All three read RPCs and the append event speak one index space: stream-local and coalesced.** `get_session` / `get_session_changes` always did; `get_session_entry` indexed the flat `session.entries` mirror instead, and `agent_session_message_appended` announced a flat index too — so an index one surface handed out could address a different entry in another, and `get_session_entry`'s `spk-image://N` cursor replayed over the wrong list. Both moved onto the stream space, `get_session_entry` gaining the `stream_id` parameter the other two already had. **Do not "fix" this in the other direction**: two RPCs and a documented wire contract are built on the stream-local space. `read_session_history` remains flat and that is correct — it stamps no `EntrySummary.index`, its `limit`/`offset` are page controls, and it cross-references nothing, so it is internally consistent. **And that is also why it keeps its own archive decode instead of calling `load_cold_session`, a question that has now been asked twice.** Every *decision* in its ladder is already shared (`load_cold_head`, `is_wiped_row_native`, `entries_from_rows`, the `session_unreadable` constant and formatter); what is local is a `serde_json::from_slice` that slices `entry_summaries` **flat**, where `build_cold_session` prefers `entries_v2` and **coalesces** adjacent assistant entries. A two-line legacy blob is `total_count: 1` to `get_session` and `total_entries: 2` here, pinned on both sides. Unifying would move `total_entries`, change the unit its page controls page over, need a mode parameter on the one function three RPCs share precisely so they cannot disagree, and route archive sweeps through the cold cache #107 sized for delta paging. The test to apply to a fourth space, before changing anything: **does any index it emits get handed back to a different RPC?** If not, flat is fine and must be left alone. A parity test for this only proves something on a transcript where the spaces actually diverge — it needs both a coalesced assistant pair and teammate-tagged entries *before* the index under test, or the flat and stream prefixes tie and the test passes with the bug live.

- **`read_session_history` and `get_session` now agree on every combination of rows / blob / epoch / metadata row**, including the case that used to differ for a silly reason: the archive path used `load_blob` as its existence check, so "no blob" meant "no session". With `load_cold_head` establishing existence first, both no-transcript shapes collapse into one arm and `session_not_found` is raised for a genuinely absent session only. That divergence is closed too: an **undecodable** blob is now `session_unreadable` on every read RPC, and the archive path carries the same code.
- **`total_count` is the stream's entry count, not the number of persisted rows** — adjacent legacy assistant lines coalesce into one entry. Pinned, not "fixed", because the desktop reads it the same way.
- **Known cost, not yet paid down:** every cold call does four or five sequential round-trips on the single shared sqlite connection mutex and decodes the whole transcript on the foreground thread, and `CHANGED_ENTRIES_PAGE = 10` makes a client `B` entries behind page in `ceil(B/10)` back-to-back polls — each one a full re-read. On the maintainer's real database (29k rows, largest session 1,520 rows / 5.3 MB) a client 500 behind costs ~50 full re-reads. Folding `epoch`/`change_seq` into `select_metadata_by_id` removes two of the round-trips for free; a cold-specific page size attacks the cliff directly; a cache is the last resort and must not key on `change_seq`, which degenerates to `None` on exactly the legacy sessions whose decode is most expensive.

### 107. Cold MCP reads of a closed session are validated against a cheap database head, not invalidated by hooks

What: `solution_agent`'s three read RPCs (`get_session`, `get_session_changes`, `get_session_entry`) rebuild a session that is no longer in `store.sessions` from its rows (#105), and they now **retain** that reconstruction, so a client's `has_more` paging burst costs one transcript read instead of `ceil(behind / CHANGED_ENTRIES_PAGE)`. Measured against the maintainer's real 206 MB database — 29,064 entry rows over 108 sessions, largest 1,520 rows / 5.3 MB — a client 500 entries behind was doing ~50 full re-reads and decodes, each on the foreground thread, with live sessions' persist flushes queued behind the same sqlite connection mutex.

Why not hooks: **"a closed session has no writer" is false.** `close_session` and `cold_close_solution` tear down with `ChainDisposition::Drain` *specifically* so the queued persist chain keeps writing rows after the entity is gone (#101), and `purge_session` / `delete_for_solution` write for sessions that were never hydrated at all. Hook-based invalidation would have to enumerate those writers and stay correct as they change. Keying the cache on `(session_id, change_seq)` — the obvious alternative — is worse than useless: `persist_all_rows` issues the row upsert, `save_epoch` and `save_change_seq` as three separate lock acquisitions and `update_change_seq` is a `max`, so it is a genuine no-op while rows move. That key ships a stale transcript.

How to apply:
- **Every call re-reads `SolutionAgentDb::load_cold_head`** — one query, one mutex acquisition, no `payload` bytes (the entry aggregates come from `idx_session_entry_modseq`; measured <0.1ms against 77ms for the transcript) — and reuses the retained copy only on `==`. Read the head **before** the transcript: that is what makes a torn read fail closed, and it is also what makes purge safety structural, since the head reads `solution_sessions`, which a purge deletes.
- **The check is total because `build_cold_session` is a pure function of `(meta, rows, blob, epoch, change_seq, tab_order)`** — it touches `cx` only for `cx.new`, with no global or settings read. The head compares four of those verbatim and **fingerprints the rows** by `(COUNT, MAX(mod_seq))`; a row's `idx`, `created_ms`, `subagent_id` and `payload` are never compared. Adding a field to `SolutionSessionMetadata` joins the check automatically through its `PartialEq`. Adding a **writer** that can change a transcript without moving `COUNT`, `MAX(mod_seq)` or a metadata column breaks this — say so rather than widening the TTL.
- **Both halves of the row fingerprint are load-bearing, and a mutation proved it.** Dropping `MAX(mod_seq)` and keeping `COUNT` passed the entire suite: an entry **edited in place** keeps its `idx`, so the count does not move and only the bumped `mod_seq` distinguishes old from new — which is `upsert_entry`'s `ON CONFLICT DO UPDATE` on every tool-call transition. `cold_cache_rebuilds_when_an_entry_is_edited_in_place` is the guard.
- **`blob` is fingerprinted too, and the story of why is the lesson.** It originally had no fingerprint, on the argument that `update_blob` had no production caller. `persist_context_wipe` (#105) gave it one within the same session, and the replacement argument — that both wipe call sites also `bump_epoch()` and `clear_total_tokens()`, so two independently-compared fields move — then acquired a hole of its own when a failed chained predecessor made the epoch save decline. A blob-only, zero-token session wiped in that window moved nothing the head compared. So the head now carries `LENGTH(acp_thread_blob)`: four inputs compared verbatim, two fingerprinted, and no dependence on another module's incidental behaviour. **The measurement inverted the obvious assumption** — over 200 × 2 MB blobs, `sum(length(blob))` costs 0.00s, `sum(blob IS NOT NULL)` costs 0.10s and a real read costs 0.49s, because sqlite answers `length()` from the record header. The more informative form was also the cheaper one; there was no trade-off to make. The residual is a same-size payload rewrite of either input, which only a hash would catch — but that is a stated property of a fingerprint, not a dependency that can rot when an unrelated path changes.
- **The retention bound is bytes and the TTL is swept, not lazy.** Four sessions / 16 MiB of *source* payload bytes (proportional to, not equal to, decoded heap), summed from `EntryRow::payload` lengths already in hand. The sweep is one coalesced timer, so an entry stored just after a tick lives up to `2 × TTL`. Before the sweep existed the TTL was evaluated only on the next cold read, which meant an idle editor never gave the memory back — the opposite of what the code's own comment claimed.
- **A cold read is still a pure read.** The reconstruction is shared by `&` at every call site and nothing may `update()` it; a future cold path that needs to mutate must clone.

### 108. The Solution-agent stream mirror shares the transcript behind `Arc` instead of copying it

What: `SolutionSession::entries` and `Stream::entries` hold `Arc<SessionEntry>`. `stream::demux` shares those handles instead of deep-copying every entry, and `Arc::make_mut` forks only where `push_coalesced` actually merges — once per maximal run of adjacent same-source assistant entries.

Why: `rebuild_streams` re-demuxed the **whole** transcript, and `SessionEntry::clone` deep-copies markdown strings, `Vec<AssistantChunk>`, tool-call `content_md`, `raw_input`/`raw_output` as recursive `serde_json::Value` trees, and a user message's `Vec<acp::ContentBlock>` **including retained base64 image payloads**. The store's `EntryUpdated` arm calls `rebuild_streams` unthrottled — the 500ms/2s throttle beside it governs only the MCP notification emit — while `acp_thread`'s reveal timer fires `EntryUpdated` every `TASK_UPDATE_MS = 16ms` for as long as text is revealing. **Measured on a synthetic transcript matched to the maintainer's largest real session (1,520 entries / 5.45 MB): 2.676ms per rebuild, 16% of a 16.6ms frame, at a verified 62.5 events/s — about 167ms of foreground CPU per second of streaming, all of it thrown away.** After: 0.137ms, deep clones per rebuild 1,520 → 122. A second full copy per *render frame* (`SessionView::main_stream_entries_for_render`) died to the same type change.

How to apply:
- **The alternative to reject is incremental demux, and the reason is not performance.** `rebuild_streams`'s output is not `demux`'s output: it `shift_remove`s closed and hydration-orphaned streams, then `insert`s the shell and agent folds *after* the demux loop, and both folds derive their synthetic entry from `Utc::now()`. So an incremental path has three independent ways to diverge silently — a lossy projection it cannot resume from, an `IndexMap` order the wire depends on (#105), and a frozen elapsed-time pill — to buy 0.8% of a frame down to 0.06%. It would not remove the fork-per-run residual either, so its ceiling is lower than it looks. Cache merged heads instead, if it ever matters.
- **A representation change made the equivalence a compiler obligation instead of an argument.** The demux algorithm is untouched, so "the mirror is byte-identical to a full demux" holds by construction. That is the property to preserve: this mirror feeds the one index space `get_session`, `get_session_changes`, `get_session_entry` and `agent_session_message_appended` all share.
- **`Arc::make_mut` on the flat side is safe; `Arc::get_mut` is the new trap.** A flat-side `make_mut` behaves exactly as the old deep-clone mirror did: it forks when the mirror shares the entry and mutates in place only when the mirror does not hold it (a coalesced head, or a fragment merged into one), so no aliased mirror copy is ever corrupted. `Arc::get_mut` returns `None` when the mirror shares the entry, which **silently drops the write** — a failure mode that did not exist when these were plain `SessionEntry`. Nothing uses it today; do not introduce it.
- **A mirror-side mutation that used to be local now needs saying so.** The `EntriesRemoved` survivor re-stamp mutates through the mirror's handle and must not reach the flat entry; sharing makes the write-through the *easy* mistake, and a mutation proving exactly that passed all 743 tests before a test pinned the flat stamps. Any third mirror-side mutation added later is unguarded by construction — pin it the same way.

### 109. One workspace invariant is enforced by a build script, because nothing else in this fork runs often enough

What: `tooling/test_target_guard` fails the build when a package sets `[lib] test = false` and still has test code under `src/`. Cargo builds no lib test target for such a package, so those `#[cfg(test)]` modules compile, pass locally if you invoke them by hand, and are silently skipped by `cargo test -p <crate>`, by a workspace run, and by CI.

Why: this is not hypothetical. Three upstream Zed commits moved `project`, `worktree`, `fs` and three others' tests into `tests/integration/` and then set the flag to suppress the now-redundant lib target — a sound trade whose invariant then decayed when ten `src/` files regrew test modules. **49 test functions were dead for months** and were found only because an unrelated task tripped over one. `collab`, `opencode` and `vercel` still set the flag and are correct only because they happen to have no in-`src` tests today.

How to apply:
- **The mechanism is a build script because of what actually runs in this fork.** CI is doubly disabled (`workflow_dispatch:` plus a `repository_owner == 'zed-industries'` gate) — a fact about this fork, not a general argument against hygiene scripts — every existing `script/check-*` is CI-only, `script/clippy` forces a release-profile workspace compile that agents are told not to run, and `cargo test --workspace` is run approximately never — while `cargo check --workspace --all-targets` runs continuously under rust-analyzer's flycheck and appears in every task's verification block. A build script fires on `check`, `build`, `test` and `clippy` alike, so the guard reports within seconds of the offending edit. **That half of the argument only works with the other half:** nothing depends on this package, so a rerun recompiles the guard alone. The mechanism is not portable to a crate that has dependents, where every rerun would drag a subtree with it. **The tempting rationale to avoid:** "a test in another crate isn't in `cargo test -p opencode`'s path" is true, but so is the build script — it is not in that package's graph either. Flycheck frequency is the real argument.
- **`cargo::rerun-if-changed` on a directory is a recursive mtime scan**, not a shallow check of the directory's own mtime, so watching each flag-setting crate's `src/` catches an edit to a file already inside it. That was measured, not assumed — it is the property the whole design rests on, and a build script that cached a success would look green forever. An errored build script is not fingerprinted, so a violation re-errors every invocation and clears the moment it is fixed, with no `cargo clean`.
- **Refuse `test = false` on `[[bin]]` and `[[test]]`, never on `[[bench]]` or `[[example]]`.** Bench and example sources live outside `src/`, and this repo has twelve `harness = false` criterion benches for which `[[bench]] test = false` is the *documented* remedy — refusing it would break the workspace check for a safe change.
- **Parse the manifest as TOML and scan Rust as tokens, not text.** A substring match for `test = false` also matches `doctest = false` (that exact bug corrupted a `Cargo.toml` during this work), and a substring match for `cfg(test)` misses `cfg(any(test, …))` — the form 158 files here use, including a real historical offender (`project/src/debugger.rs`) that a literal scan would not have flagged. `doctest = false` is deliberately not guarded: ~110 packages set it on purpose, and losing doc examples is a documentation loss rather than a suite that silently stops running.

### 111. The agent transcript database runs in WAL at `synchronous=NORMAL`, and copying it now means checkpoint-or-copy-two

What: `crates/solution_agent/src/db.rs`'s `open_connection` issues `journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout=500`, `foreign_keys=ON` (`CONNECTION_PRAGMAS`) before its DDL, matching `db::CONNECTION_INITIALIZE_QUERY` / `db::DB_INITIALIZE_QUERY`. Until this, `SolutionAgentDb` was the **only fork-owned** database still on sqlite's bare defaults — rollback journal, `synchronous=FULL`, `busy_timeout=0`. It is *not* the only bare `Connection::open_file` in the tree: `crates/agent`'s `threads.db` (`agent/src/db.rs:403`), `copilot_chat` and `edit_prediction_cli` open theirs with no pragmas either. Those are upstream crates holding upstream data, none of them on a write path anything like this one, and none of them were in scope here — do not read this entry as a claim that they are fine.

Why: `store::persist_main_stream` is called **once per `AcpThreadEvent::EntryUpdated`, unconditionally and with no coalescing anywhere in the chain** — the 500 ms/2 s throttle beside it governs only the MCP `SessionMessageAppended` emit, and each `PersistChain` link captures its own row plan and issues its own three transactions (upsert+trim savepoint, `save_epoch`, `save_change_seq` — the split #105 defends). `acp_thread`'s reveal timer fires `EntryUpdated` every `TASK_UPDATE_MS = 16 ms` while text reveals, so a streaming turn issues ~187 durable commits per second. **Measured on a temp database seeded to production scale (108 sessions x 269 rows = 29,052 rows, 3.6 KiB payloads, 114.8 MiB): 48.5 ms per event, i.e. 3,032 ms of database time per 1,000 ms of streaming — a 3x overrun on a chain that is serialized per session and holds the shared connection mutex the whole time.** After: **0.135 ms per event (359x), 8.4 ms/s of streaming, write amplification 56.0 -> 12.9 KiB per 3.6 KiB row, full flush of the largest real session 74.4 -> 8.1 ms.** This is the write-side twin of the read-side stall #107 records.

How to apply:
- **This database has a WAL. Copying it means `PRAGMA wal_checkpoint(TRUNCATE)` first, or copying `<name>.db` *and* `<name>.db-wal`.** Copy the `.db` alone and you get a silently *older* database, missing every commit since the last checkpoint, with no error anywhere — the worst shape a data bug can take. Do NOT copy `-shm`: it is a rebuildable index into the WAL that sqlite recreates on open, and one snapshotted at a different instant than the `-wal` is worse than absent. This is not hypothetical hygiene; four tracked sites already did it. `crates/solutions/tests/identity_migration_rehearsal.rs::copy_aside` now copies the `-wal`. `script/migrate-from-spk-editor.sh` reads its donors with `?immutable=1` — a URI parameter that makes sqlite **ignore a `-wal` outright** — and now refuses to run when one exists. That same script's post-boot `rm -f …-wal` is now a checkpoint, because SIGTERM-ing the booted editor can leave committed transactions, including the freshly created schema, in a WAL that `rm` would discard. And `docs/superpowers/plans/2026-07-13-rename-1-identity.md` tells the operator to `cp` the file — annotated in place rather than rewritten, because a shipped plan's history is not ours to edit; note that `docs/superpowers/` is only *partly* gitignored scratch, so grep it when auditing this class of instruction.
- **`journal_mode` is in the file header; `synchronous`, `busy_timeout` and `foreign_keys` are per-connection.** A second connection on this file picks WAL up for free and inherits none of the rest. `solutions::path_migrations::apply_one` opens exactly such a connection at startup, so it sets all three of its own (`open_agent_db`, pinned by `path_migrations::tests::the_migration_connection_sets_its_own_pragmas`): at `busy_timeout=0` an overlap with the store's writer is an instant `code 5 "database is locked"` rather than a 209 ms wait, and its failure path is "log it and retry next editor start". `foreign_keys` is the one of the three that changes nothing measurable — **no table in `solution_agent.db` declares a `REFERENCES` clause**, so the pragma is inert on this file either way, and the FK-bearing schemas are in `solutions::db`, a different database reached through `ReconcileContext::app_db`. It is issued regardless, because the alternative is not "off" but "whatever `SQLITE_DEFAULT_FOREIGN_KEYS` was compiled to" — a libsqlite3-sys build flag, currently 1 — and the whole point of the explicit pragma on the store's connection is to stop depending on it. Two connections on one file silently disagreeing about constraint enforcement is not a state worth saving a statement for. It duplicates the values rather than importing them because the crate edge runs the other way — `solution_agent` depends on `solutions`, which takes `solution_agent` as a dev-dependency only.
- **`NORMAL`, not `FULL`, and the reason is not that durability is cheap.** WAL+`FULL` measured 17.0 ms/event (2.9x) — still 1,062 ms of database time per second of streaming, i.e. it does not fix the overrun, only survives it. What `NORMAL` gives up is bounded and re-derivable: a process crash, panic, OOM-kill or `SIGKILL` loses **nothing** (committed frames are in the page cache and the next opener replays them); only a power loss or kernel panic can drop the transcript tail since the last checkpoint, the file is never corrupted, that same event destroys the in-memory session anyway, and the tail is rewritten by the next full flush and independently held in the `claude` subprocess's own JSONL. `synchronous` is per-connection and not in the header, so reverting the trade changes one word — but in **two** places, `db.rs`'s `CONNECTION_PRAGMAS` and `path_migrations::open_agent_db`, which is the cost of the deliberate duplication described above. Change both or the second connection silently keeps the old setting.
- **Do not adopt `sqlez::ThreadSafeConnection` to get these.** Its per-URI write queue and thread-local read connections duplicate machinery this crate already has — one `Arc<Mutex<Connection>>` plus `PersistChain`'s per-session ordering — while forcing every `db/*.rs` method onto the `write(|connection| …)` callback shape and reopening the migration question against a 200 MB real database. The entire measured win is four pragmas.
- **The pragmas are unobservable from almost every test in the crate**, because `SolutionAgentDb::open` swaps in an in-memory connection under test cfgs and `PRAGMA journal_mode` answers `memory` there no matter what was asked. `db::tests::connection_pragmas_are_in_effect_on_a_file_database` reads them back through `open_at_path` on a tempdir — deleting `CONNECTION_PRAGMAS` leaves the other 763 tests green. That `memory` answer is also why `open_connection` must NOT assert the result: `PRAGMA journal_mode` reports the mode that took effect instead of erroring, and hardening it into a check would fail every in-memory open.

### 112. A tool's socket placement is part of its contract, and the caller that dials the wrong socket has to say so out loud

What: `GLOBAL_TOOLS` in `crates/editor_mcp/src/lifecycle.rs` is fail-*safe* for leaks but fail-*silent* for reachability. `start_server` splits every tool that is not on that list off the editor-global socket onto the per-solution sockets, so a **brand-new tool defaults to solution-scoped** — and a client that dials the global socket gets `-32601 Tool not found` rather than anything that names the real problem. This has now shipped broken four times: the Remote Control surface (`6ce92bf3f4` — every allow-listed `remote.workspace.*` / `remote.solution_agent.*` call came back "Tool not found" on the phone), `solutions.set_active_member` (`0bbe686a00` — uncallable from the socket the operator actually drives), the three supervisor tools including `get_supervisor_state` (`d1d8dcc689` — the mobile supervisor sheet would not open), and `editor.handle_cli_args` (`cef86369c5` — `sawe <path>` silently opened nothing for anyone with a running editor).

Why: the omission is invisible from inside the tool. Registration, schema, unit tests and a per-solution `tools/list` all look right; only the *caller's* socket choice makes it wrong, and every one of these callers lives in a different crate from the tool. The CLI hand-off case also shows the second half of the failure mode: `handoff::interpret_reply` threw the `-32601` away and reported "missing result.structuredContent" instead, and `main.rs` sent that string to the log file while the terminal printed only `sawe is already running`. A reachability bug that reports itself as a malformed reply, in a log nobody reads, survives for as long as nobody happens to look.

How to apply:
- **Before registering an MCP tool, ask which socket its real caller dials.** If any non-agent client reaches it — the CLI hand-off (`editor_mcp::handoff`), the mobile proxy (`remote_control::proxy::connect`, which dials `editor_mcp::socket_path()`) — it must be in `GLOBAL_TOOLS`. Pin it with a test that reads the caller's own constant rather than a re-typed literal: `lifecycle::handoff_tool_is_global` asserts `GLOBAL_TOOLS` contains `handoff::HANDOFF_TOOL_NAME`, `handoff::constant_matches_the_registered_tool_name` asserts that constant equals `HandleCliArgsTool::NAME`, and `remote_control::allow_list`'s `allow_list_round_trip` asserts that every allow-listed `solution_agent.*` / `workspace.*` upstream satisfies `editor_mcp::is_global_tool` (the two namespaces that shipped broken; the guard is prefix-limited, so an allow-listed tool in a *third* namespace is still unpinned). Three separate strings that must agree, each pinned to the next.
- **Then ask what per-solution `solution_id` injection does to the tool's params.** A tool whose `solution_id` is a *target* rather than a *scope* (`solutions.switch`) is actively corrupted by injection — the bound id overwrites the argument, so it can only ever "switch" to the Solution it is already on. A tool with **no** `solution_id` property at all (`editor.handle_cli_args`; every one of the six `run_config.*` params structs in `crates/run_config/src/mcp.rs`) gets no injection, so solution-scoping it buys exactly zero isolation while removing it from the global socket. Neither kind belongs in `SHARED_TOOLS`.
- **A client of this socket must report *why* a call failed, on stderr, not only in the log.** `interpret_reply` now has one branch per failure shape the server can produce, because they are three different shapes and only the last one was handled: a JSON-RPC `error` member (`-32601` and friends), a *successful* response carrying `result.isError: true` with the message in `result.content[..].text` and no `structuredContent` (`context_server::listener`'s `Err` arm — note `isError` is also serialized as `false` on success, so test for `true`, not for presence), and a well-formed `structuredContent` with `handled: false`. `main.rs` carries the reason to the exit that gives up (`failed_single_instance_check`) rather than printing it where the failure happens, so a probe failure that is followed by successfully becoming canonical — stale lock, dev channel, `ZED_STATELESS` — stays quiet, and only the exit that opens nothing is loud.

### 113. Two independent single-instance gates decide `sawe <path>`, and the one the hand-off reads must be held for the whole process life

What: this fork answers "is an editor already running?" twice, in two different places, against two different files, and the answers can disagree.

- `data_dir()/zed-<channel>.sock` — a `UnixDatagram` bound by `crate::zed::listen_for_cli_connections`, **before** `app.run` (`crates/zed/src/main.rs`). Losing this bind is what prints `sawe is already running` and returns without opening anything.
- `state/mcp.lock` — an `flock` taken in `editor_mcp::start_server`, which is the **last statement of the entire `app.run` closure**. `handoff::probe_lock` reads only this one: a free lock means `BecameCanonical`, i.e. "nobody is running, carry on".

The window between them is the whole of startup, and a failure inside it is not a race. The lock guard used to be moved into `start_server`'s spawned task and published only at that task's end (inside `ActiveServer`), so any `?` in that task — `McpServer::new` failing to make its tempdir or bind inside it, or the well-known-socket `symlink` failing — dropped the guard and released the flock while the process went on living and went on owning `zed-<channel>.sock`. From that moment until the editor was restarted, **every** `sawe <path>` probed a free lock, took `BecameCanonical`, then lost the socket gate to that very process and exited having opened nothing.

Why it stayed invisible: `detach_and_log_err` put the only record in `logs/sawe.log`, and the giving-up exit printed `sawe is already running` — a sentence that is true, reassuring, and describes a completely different situation from the one the user is in.

How to apply:
- **The flock's lifetime is the process's, not the startup task's.** It is parked in its own `InstanceLock` global (`crates/editor_mcp/src/lifecycle.rs`) the instant it is acquired, never in a structure that is only published on success. `tests/startup_failure_lock_e2e_test.rs` plants a *directory* at the socket path to fire the real symlink `?` and asserts the lock is still held. Keeping the lock across a failure cannot strand it: `flock` is released by the kernel on process exit, so a crashed or `SIGKILL`ed editor still frees it.
- **The price is paid by the second invocation, and it is deliberate.** A permanently MCP-less editor now holds the lock, so `sawe <path>` reaches `LockBusyButUnreachable` — five connect attempts one second apart (`handoff::RETRY_COUNT`), i.e. a measured 4s worst case, with one line on stderr after the first failure — and then exit 1, instead of an instant, wrong "already running". That branch cannot tell "still starting" from "failed to start" and must not pretend otherwise; it names both and says to restart if it persists.
- **A lock that was never acquired cannot be held longer.** `SingleInstanceLock::acquire` returning `LockError::Io` (unwritable state dir, full disk, `mcp.lock` shadowed by a directory) reaches the same end state — live editor, no MCP server — and no guard lifetime fixes it. That arm reports itself the same way; only `LockError::Busy`, which is a genuinely second instance, stays quiet.
- **The paths are still lost when the MCP server is down**, in every one of these routes: the hand-off is the *only* channel this fork has for carrying CLI arguments into a running instance, and it is the thing that failed. The remaining option would be to forward `file://` datagrams to `zed-<channel>.sock` — which is exactly what `crates/cli` does, and would be a second hand-off mechanism to own. Not done; the exits say what they dropped instead.

### 114. The CLI hand-off carries paths; everything else it was given is named, not dropped

What: `editor.handle_cli_args` is the only channel a second `sawe` process has into a running instance, and its contract is a list of paths handed to `workspace::open_paths`. Everything else on the command line — `sawe://`, `zed://`, `zed-cli://`, `ssh://`, `--diff` pairs — is something the hand-off cannot carry, and it used to be thrown away in silence on the branch that *worked*: `sawe /tmp/a sawe://x` against a healthy instance opened `/tmp/a`, lost `sawe://x` and exited 0 with an empty terminal. That is worse than the give-up exits, because nothing failed anywhere and the user has no reason to look.

Why the obvious fix is not available: URL routing in this fork is `OpenRequest::parse` -> `handle_open_request`, both in `crates/zed`, which depends on `workspace` — and it is `crates/workspace/src/mcp/handle_cli_args.rs` that serves the tool. Teaching the tool to route a URL means inverting that dependency edge, not adding a field; and `HandleCliArgsResult` could not express `OpenRequestKind`'s dozen outcomes even if it could reach them.

How to apply:
- **`file://` is a path, not a URL, and is carried.** `crates/zed/src/main.rs::file_url_as_path` uses the same decoding rule as `OpenRequest::parse_file_path` — strip the scheme, percent-decode, take the rest verbatim, authority component and all (`file://host/p` is the relative `host/p` in both). Do not "improve" one without the other: parity is the entire argument, because the same argv must not open different files depending on whether another editor happened to be running. It parts company in exactly one place, deliberately — an escape that does not decode is *reported* here where `parse_file_path` drops it and logs.
- **This is about direct invocation of the editor binary**, a dev build or the bundle's `libexec/sawe-bin`, not about the desktop entry. `Exec=` there is `bin/sawe`, which is `crates/cli`; the CLI puts only `zed-cli://<server>` in the editor's argv and sends the user's URLs over that IPC channel. A `%U` launch therefore never reaches `split_handoff_args` at all.
- **Do not add more schemes to `file_url_as_path`.** Each one copies a line of `OpenRequest::parse` into a second place that has to be kept in sync. When URLs need to reach a running instance for real, route them to `data_dir()/zed-<channel>.sock` — the `UnixDatagram` `listen_for_cli_connections` already binds and already parses URLs from (see #113's last bullet) — instead of widening the MCP tool.
- **Exit stays 0 whenever the hand-off succeeded**, even in the `sawe sawe://x` case where nothing the user asked for was opened. A non-zero exit from this binary means "the running editor could not be reached, your work was not done"; folding "I do not route that scheme" into the same code would make the exit status depend on whether another editor happened to be running, which is exactly the coupling the parity rule above exists to prevent.
- **The two give-up exits disagree with each other, and that asymmetry is inherited, not a rule.** `LockBusyButUnreachable` (`main.rs`) gives up after `handoff::RETRY_COUNT` one-second connect attempts — a 4s worst case — prints `unreachable_instance_report`, and exits **1**. `failed_single_instance_check` gives up having lost the *other* gate (#113), prints `sawe is already running` on stdout plus `dropped_args_report` on stderr, and `return`s from `main`, i.e. exits **0** — and it is reachable with a `handoff_failure` in hand, including after the full 30s `handoff::READ_TIMEOUT` against a wedged instance, which is the longest wait and the largest loss in the whole family. So the worse outcome exits 0 while the lesser one exits 1. Both codes are pre-existing and were deliberately left alone by two rounds of this work; what those rounds actually established is the property worth relying on — **every give-up exit now names or counts what it dropped, none of them double-reports, and none of them exits non-zero having lost nothing.** Do not "fix" either code on the strength of the other: an exit status is part of this binary's contract with whatever script invoked it, so aligning them is a deliberate interface change with its own argument to make, not a consistency cleanup.
- **The report names its arguments where the give-up exits only count them.** A give-up exit loses everything, so a count is enough; here only a subset is lost and the user has to be told which. `--diff` is in the same boat as a URL and is accounted for in both places — named in the hand-off's unforwarded list, and included in `dropped_arg_count` (`paths_or_urls.len() + diff.len()`), which used to size itself off `paths_or_urls` alone and so said nothing at all about a dropped `--diff` pair.
- **`zed-cli://<server>` is not one of the user's arguments and must never be listed as one.** It is the ipc handshake `crates/cli` puts in this process's argv before booting it, and the user's real request is on the far end of it. Naming that url in "N argument(s) were NOT opened" told the reader two untrue things — that they had typed it, and that one argument was the extent of the loss. It gets its own line saying nothing the `sawe` command asked for was opened — and nothing more, because **what happens to that command next differs per CLI mode and this process cannot see which it is in**. Without `--foreground`, `App::launch` -> `boot_background` fork/execs the editor after `fork::close_fd` has closed its stderr, so the line is not shown at all, and the CLI then blocks in `sender.join()` (`cli/src/main.rs:794`) on a receiver still sitting in `server.accept()` — it hangs. With `--foreground` it is the opposite on both counts: `run_foreground` uses `Command::status()`, which inherits stderr, so this line *is* what the user reads, and the `join` is never reached, so the command exits normally. The `join` is unconditional on `--wait`; `!args.foreground` is the real condition. A sentence about waiting therefore reads false on the one CLI path where it is read at all. Answering the handshake for real (connect back, send `CliResponse::Stderr` + `Exit`, or relay the `CliRequest::Open` through the MCP hand-off) is the unbuilt repair. Exit still stays 0 by the rule above: a canonical instance answers the handshake and a handing-off one cannot, so a non-zero exit here would be decided by whether another editor happened to be running — and it would be unobservable anyway, since `boot_background` detaches and `run_foreground`'s `ExitStatus` is discarded at its call site. The clause naming *the `sawe` command that started this process* as the connection's opener is deliberate and stays: the third mode — direct invocation of the editor binary — has no `sawe` command, but it also has no user request to describe, because a live `zed-cli://<server>` only ever comes from `crates/cli`. Weighed and left alone; the reasoning is in `handoff_loss_report`'s doc comment so it does not get reopened.
- **A `:line:column` suffix is split off by the *sender*, not carried.** `handle_cli_args` hands its strings straight to `workspace::open_paths`, so the suffix used to travel as part of the filename: `sawe probe.rs:3:2` against a running instance opened a window rooted at the nonexistent `…/probe.rs:3:2` (measured — `windows.list` reported `kind: "folder"` and that literal string) and never opened the file, while a canonical instance opened `…/probe.rs` at line 3. `split_off_position` applies `derive_paths_with_position`'s own rule — strip only when the literal string is not already a real file, and skip that check on Windows where `name:stream` is an NTFS alternate data stream — so the right file opens and the lost position is reported — reported as *dropped*, not as "opened at the start of the file", because when the stripped path does not exist no file is opened at all: the running instance grows a second window of `kind: "folder"` rooted at that nonexistent path instead (measured the same way — `windows.list` after handing off `…/absent.rs:9:1` reports `root_paths: ["…/absent.rs"]`, title `absent.rs`). The position itself stays lost on purpose: the navigation is `recent_projects::navigate_to_positions`, which downcasts the opened item to an `Editor` and therefore sits above `workspace` in the crate graph, so carrying it means moving the tool's registration, not adding a field.
### 115. `Install CLI` claims one name in a shared directory, and refuses to touch anything else in it

What: the maintainer's rule is that a user may have both Zed and Sawe installed and the two must not intersect in any way. `/usr/local/bin` is the one directory where this fork writes into a namespace it shares with every other program on the machine, and upstream's installer treated that directory as its own.

Upstream `install_script` symlinked **`/usr/local/bin/zed`**, and before creating the link it ran an unconditional `remove_file(link_path)` — with an `osascript … with administrator privileges` escalation running `ln -sf` if the unprivileged unlink or symlink failed. On a machine that also has a real Zed, `File → Install CLI` therefore deleted Zed's CLI, put ours at its path under Zed's name, and would ask for the admin password in order to do it.

How to apply:

- **The target is `/usr/local/bin/sawe`** (CLAUDE.md §3 locks the CLI binary name), and the *only* entry this fork will ever replace is a symlink that already points at **this build's own** `cli` executable — one it can prove it created, where replacing is a no-op anyway. Everything else is refused by name: a symlink pointing elsewhere is reported together with the path it points to, a regular file or an entry that cannot be read is reported as not ours. Nothing is unlinked, ever. The escalation is `ln -s`, never `ln -sf`, so even the privileged path cannot clobber an entry that appeared between the check and the link.
- **The refusal is deliberately stricter than "does it look like a Sawe path?"**, and it costs something: if the app moves (bundle relocated, a different channel installed over it), the existing symlink points at the old `cli` and is no longer provably ours, so `Install CLI` refuses to repair itself and instead tells the user exactly what is there to remove. That is the price of the only test that cannot be wrong in the destructive direction — every looser rule is a heuristic, and a wrong heuristic here deletes somebody else's binary with admin rights.
- **`register_zed_scheme` is deleted, not repointed at `sawe`.** It registered `ZED_URL_SCHEME = "zed"` from two places: a `cli: register zed scheme` palette action, and the last line of `Install CLI`, unconditionally. Both are gone. It was *not* turned into a `register_sawe_scheme`, because nothing would call it and no platform needs it: `sawe://` is already declared at install time on all three targets (`osx_url_schemes = ["sawe"]` on all four channels, `x-scheme-handler/sawe` in `sawe.desktop.in`, `HKCU\Software\Classes\sawe` in `sawe.iss`), and `register_url_scheme` is implemented **only on macOS** — `gpui_linux` and `gpui_windows` both return `Err("register_url_scheme unimplemented")`. The imperative call adds exactly one thing on top of the declarative registration, `NSWorkspace.setDefaultApplication`, which only matters when two *installed* apps claim the same scheme; once `zed://` is disowned that can only be two Sawe channels fighting over `sawe://`, a case this fork has never needed. If it ever does, the function was twelve lines and this entry says where it went.
- **A correction to the recon that produced this work:** the claim that the `Install CLI` *menu item* routinely showed Linux users an "Error registering zed:// scheme" was wrong twice over. The call site was `register_zed_scheme(cx).await.log_err()` — the error went to the log, never to a dialog — so the menu item could not produce that message on **any** platform; and on Linux/FreeBSD `install_cli_binary` early-returns behind `cfg!(any(target_os = "linux", target_os = "freebsd"))` with an informational prompt, so it never reached `install_script` or the scheme registration at all. The palette action was the only route to the dialog, on every platform. What the menu item actually did was worse than the reported symptom and silent: on macOS it reached the unconditional `remove_file` under administrator privileges *and* `setDefaultApplication`, reporting neither. Do not cite the Linux dialog as the motivation.

### 116. `sawe://` is the scheme this fork parses, and `zed://` gets no arm and no producer

What: `OpenRequest::parse`'s routing table, and everything that mints a link for it,
are spelled `sawe://`. `zed://` reaches no arm; it is an ordinary unrecognised URL.

Why: `zed://` is a different product's external contract. The ruling is that a user
may have both editors installed and they must not intersect in any way — so this fork
neither claims the scheme with the OS (decision #115, phase 1) nor answers it in
process. Commit `915bd2b73f`, earlier the same day, had gone the other way: it added a
single `normalize_fork_url_scheme` at the top of `parse` rewriting `sawe://` into
`zed://` so the upstream-spelled arms would match, on the invented premise that
`zed://` was a preserved upstream identifier like `.zed_server`. It is not; `.rules`
§3 names `.zed_server` / `.zed_wsl_server` as the *only* preserved ones and locks
`sawe://` as the fork's scheme. That normalisation is deleted, not inverted: an alias
in either direction is still two spellings for one route.

How to apply:

- **Generation and parsing move in one commit, always.** Deleting the parse arms alone
  breaks the schema round trip at the halfway point — `SCHEMA_URI_PREFIX` is handed to
  `vscode-json-language-server`, which hands it back as a `vscode/content` request, so
  `json_schema_store`, `settings_store`'s `LSP_SETTINGS_SCHEMA_URL_PREFIX`,
  `assets/settings/default.json`'s `$schema` and `main.rs`'s re-synthesised
  `format!("sawe://schemas/{}")` are one unit. The failure mode this decision exists to
  prevent is a producer left behind spelling a URL nobody parses; the guard against it
  is `no_arm_claims_the_disowned_scheme` in `open_listener.rs`, which asserts per
  spelling that a `zed://` url sets no `kind`, no `open_paths`, **and** no
  `join_channel` / `open_channel_notes` — the last two because `client::parse_zed_link`
  reaches `OpenRequest` through fields the first two assertions do not cover, and an
  earlier version of the test passed with that arm restored.

- **No special branch and no message for an incoming `zed://`.** The question that
  dissolves the idea is how one would arrive: after phase 1 the only routes were the
  user typing it, or our own "Open URL" modal, **whose placeholder suggested
  `zed://…`** — we were the one proposing it. The placeholder now reads `sawe://...`
  and the case has no producer. Measured on a live instance, `zed://settings` and
  `vscode://settings` are now treated identically at both entry points: through the
  url handler both log one `ERROR … unhandled url: <url>`, and on the command line both
  fall out of `is_url_scheme`, are carried as paths and fail canonicalisation with
  `failed to canonicalize root path "$PWD/<url>"`. "Ordinary unrecognised URL" is
  therefore a statement about parity, not about a nice message; if the argv path
  deserves a better one, that is a change for *all* unknown schemes, not for this one.

- **`client::parse_zed_link` keeps its `<server_url>/channel/…` arm and loses only the
  `zed://` one**, along with `ZED_URL_SCHEME`, its sole consumer. It was not renamed
  and not repointed at `sawe://channel/…`: the links it parses are minted by the collab
  web app on `ClientSettings::server_url`, a service this fork does not talk to
  (`collab` is disabled, so both `ZedLink` variants dead-end at `join_channel` /
  `open_channel_notes`), so a fork-branded spelling would advertise a route to nothing.
  Deleting the function outright would go past "disable, don't delete" and touch
  `editor`, `command_palette` and `main.rs` for a subsystem that is only switched off.

- **`zed-cli://` and `zed-dock-action://` are deliberately untouched here.** They are
  distinct prefixes, both ends ours, and they belong to the on-disk / internal-name
  pass, not to the URL-scheme contract — which is where they were renamed, see #117.
  So is `acp_thread`'s `MentionUri`, which is `zed:///…` (empty authority): a
  self-consistent ACP resource-link namespace that is persisted in thread history and
  never reaches `cx.open_url`, and which therefore still awaits a migration or a
  tolerant reader rather than a rename in place.

### 117. A name the guarded crate does not spell is a name it cannot guard

What: `crates/paths` has asserted since the rebrand that the directories it hands out
name this fork and not the product it was forked from. `data_dir()` passed that
assertion the whole time it contained `zed-<channel>.sock` — because only the parent
was spelled in `paths`; the file name was appended by callers in `crates/cli`
(`InstalledApp::launch`) and `crates/zed` (`listen_for_cli_connections`). Same shape
for `zed-crash-handler-<pid>` under `temp_dir()`.

Why: widening the assertion cannot fix this. It has no way to enumerate strings that
other crates `join` onto its return values, and a lint that tried would be a
grep over the workspace rather than a test. The fix has to remove the split, not
police it, so the **construction moves into the guarded crate**:
`paths::cli_ipc_socket_in(data_dir, release_channel)` and
`paths::crash_handler_socket(pid)` compose the full names, both call sites go through
them, and `rebrand_tests::caller_composed_names_are_ours` asserts on the result. The
socket takes its data dir as an argument rather than calling `data_dir()` because
`--user-data-dir` moves it and the editor the CLI wakes must be looked for under the
same root.

How to apply: any new runtime path whose *file name* this fork owns gets its
constructor in `crates/paths` and a line in the rebrand assertions — not a `format!`
at the call site. If you find yourself writing `paths::something_dir().join(format!(…))`
in another crate, that is the bug this entry exists for.

**The one instance that was knowingly outstanding has been discharged, so this rule
has no exception.** `crates/remote_server/src/server.rs` composed
`paths::temp_dir().join(format!("zed-remote-server-crash-handler-{pid}"))` and its
`…-proxy-…` sibling at the call site, deferred to phase 4 because the name family it
belongs to carried a compatibility consequence. Phase 4 (#119) routed both through
`paths::remote_server_crash_handler_socket` / `remote_server_proxy_crash_handler_socket`,
and both are asserted by `caller_composed_names_are_ours`. The remote-server *binary*
name went the same way in the same commit — `paths::remote_server_binary_name` /
`remote_server_binary_prefix`, composed at three transports before — which is the
stronger case, since unlike the crash sockets it lands in a directory shared with
another product.

Renamed at the same time and for the same reason, both ends in one commit: the
`zed-cli://` ipc handshake url and its Windows sibling `zed-dock-action://`, and the
Linux keyring label `zed-github-account`. Only the last is a real collision rather
than hygiene — the Secret Service keyring is a namespace shared with every
application on the machine, so a real Zed and this fork were reading and overwriting
one credential entry; it costs one re-authentication. None of the others can be known
from outside this repo, so all are renames with no compatibility window. `.zed_server`
/ `.zed_wsl_server` and the `zed-remote-server-…` binary name are **not** in this set:
they are visible to remote hosts, so they carry a real migration and were a separate
task — #119.

### 118. The GitHub workflows are hand-maintained; `cargo xtask workflows` destroys the fork's CI policy

What: 21 files in `.github/workflows` still start with
`# Generated from xtask::workflows::…` / ``# Rebuild with `cargo xtask workflows`.``.
Following that instruction is destructive in this fork, and an implementer found it
only by reading the regeneration diff before committing it. Verified against
`tooling/xtask/src/tasks/workflows.rs`:

- `remove_generated_workflows` deletes *every* file in `.github/workflows` whose
  content starts with that preamble, then `run_workflows` re-emits 20. The 21st,
  `retag_release.yml`, is deleted and never comes back: `retag_release.rs` still
  exists in the generator's source directory but is neither declared as a `mod` nor
  listed in `run_workflows`. (The 20 hand-written workflows without the preamble are
  untouched.)
- 25 of this fork's 41 `if: false # sawe: not applicable` job guards live in 10 of
  those generated files and are stripped. The other 16 are in hand-written files and
  survive.
- 5 workflows whose triggers this fork narrowed to `workflow_dispatch:` get their
  upstream triggers back — `run_tests` (`push` + `pull_request: '**'`),
  `compliance_check` (weekly `schedule`), `deploy_collab` (`push` tag
  `collab-production`), `publish_extension_cli` (`push` tag `extension-cli`) and
  `extension_auto_bump` (`push`).

So a single regeneration silently re-enables CI that CLAUDE.md's "What's disabled"
table says is off, in a repo where those jobs would run against upstream's
infrastructure.

Why a comment rather than a fix: making the generator emit the fork's policy means
porting 41 guards and 5 trigger narrowings into `tooling/xtask` as fork-local Rust,
which buys nothing — nothing in this fork *adds* workflows, so the generator's only
remaining job would be to reproduce a state we already have on disk. Deleting the
generator instead would go past "disable, don't delete". The cheapest correct move is
to make the file say so where the misleading instruction is read.

How to apply: the line `# sawe: do NOT run that -- this file is hand-maintained
here. See FORK.md #118.` sits directly under the rebuild instruction in all 21 files.
If you are editing CI, edit the YAML. If a future change genuinely needs the
generator, the fork's policy has to move into `tooling/xtask` first, and the numbers
above are the checklist for what must survive.

### 119. The remote-server directory and binary name are this fork's own — the one disown change that breaks compatibility with another installation

What: a Sawe client uploads its remote server to **`~/.sawe_server/sawe-remote-server-<channel>-<version>`** on the remote host, not to `~/.zed_server/zed-remote-server-<channel>-<version>`. `paths::remote_server_dir_relative()` returns `.sawe_server`; the file name is composed by `paths::remote_server_binary_name()` (and `remote_server_binary_prefix()`, which the server's reaper strips). `remote_wsl_server_dir_relative()` and the `cleanup_old_binaries_wsl()` that was its only caller are gone.

Why: this is the last shared namespace in the disown series and the only one with a real consequence. Every other rename (#115 `/usr/local/bin`, #116 `zed://`, #117 the on-disk runtime names) was either a name only we could see or a claim we simply stopped making. Here both products wrote **the same directory** on a machine neither owns, with **byte-identical file names** inside it, so:

- a Sawe client would silently adopt a binary a real Zed had uploaded, and vice versa, whenever the channel and version strings happened to match;
- worse, `cleanup_old_binaries()` runs *on the host* and deletes every `zed-remote-server-<channel>-*` whose version is older than its own and which no running process holds — so a Sawe server reaped a real Zed's binaries, and a real Zed's server reaped ours;
- worst, `cleanup_old_binaries_wsl()` did an unconditional `remove_dir_all` of the whole `.zed_wsl_server` directory.

The accepted cost is one re-upload per remote host, which the existing code performs by itself: the client's entire version negotiation is `<dst_path> version` returning zero, so a name that is not there simply reads as "no server binary" and the normal download/upload path runs. Nothing else had to change for that, because nothing else depends on the name.

How to apply:

- **The name is the version check.** The client never asks the server what it is; it runs the *expected path* with the `version` argument and looks only at the exit status (`ssh.rs`, `wsl.rs`, `docker.rs`). Neither the directory nor the binary name appears in any proto message, in the JSON log framing, or in `ProxyLaunchError`'s exit-code channel (90). The only magic string in the whole handshake is the Windows-only `ZED_SSH_CONNECTION_ESTABLISHED`, which the client echoes through `ssh` and reads back from its own stdout — both ends the same process, and it carries no path. So a rename cannot desynchronise a version check; it can only *fail* one, into the branch that re-uploads.
- **Nothing is cleaned up on the remote host, deliberately.** The old `~/.zed_server` is left where it is. Its contents cannot be attributed: this fork's own earlier builds wrote binaries there under exactly the file names a real Zed writes, so "delete the stale ones" and "delete somebody else's editor" are the same operation. This is the phase-3 ruling from #115 applied to a directory instead of a symlink — nothing in this fork removes a path it cannot prove it created. The dead bytes are one binary per channel per version, and the user can remove the directory by hand.
- **The one consequence of that: `cleanup_old_binaries_wsl` was deleted rather than renamed.** Renaming its target to `.sawe_wsl_server` would leave a `remove_dir_all` aimed at a directory no code in this fork writes — `wsl.rs` has used the *shared* `remote_server_dir_relative()` since upstream's own "remove this once 223 goes stable" marker — so it could never fire again. Keeping an inert `remove_dir_all` is worse than not having one. What it used to delete is now covered by the rule above: we do not touch `.zed_wsl_server`.
- **`crates/auto_update`'s `"zed-remote-server"` is not ours and stays.** It is the *asset name in Zed Industries' release feed*, sent as a query parameter to `cloud.zed.dev` by `get_release_asset`; renaming it would ask their API for an asset that does not exist. Same category as #116 keeping `client::parse_zed_link`'s `<server_url>/channel/…` arm. It is also unreachable here, since `auto_update::init` is commented out, so both delegate methods that call it fail with "auto-update not initialized" — meaning this fork's live remote-server acquisition is the `ZED_BUILD_REMOTE_SERVER` build-from-source path, or a binary already present on the host.
- **The release-*artifact* names in `.github/workflows` and `tooling/xtask/.../vars.rs` are a different name family and were left alone.** They are `zed-remote-server-linux-x86_64.gz` and friends, alongside `Zed-aarch64.dmg` and `zed-linux-x86_64.tar.gz` in the same `EXPECTED_ASSETS` list, while `script/bundle-{linux,mac,freebsd}` have emitted `sawe-*` since the phase-B rebrand — so that manifest is already wholly out of sync, for every artifact family, not just this one. Renaming one row of it fixes nothing, and #118 forbids regenerating the YAML, so `vars.rs` and the 21 files must move together as one deliberate task. Be precise about the reach of that, because it is narrower than it looks: in `release.yml`, 9 of the 20 jobs carry `github.repository_owner == 'zed-industries' || 'zed-extensions'` in their own `if:` and the remaining 11 inherit it through `needs:` (in `release_nightly.yml`, 5 of 12 directly and the rest by inheritance), so nothing in either file can run here, but **`run_bundling.yml` is not owner-gated** — it fires on a `run-bundling` pull-request label. It is nonetheless already broken on its own terms rather than broken by this change: it uploads `target/release/zed-linux-aarch64.tar.gz` and `target/zed-remote-server-linux-aarch64.gz`, both with `if-no-files-found: error`, against a `bundle-linux` that has emitted `sawe-*` names for both (`script/bundle-linux:200,207`) since the phase-B rebrand. That is the out-of-sync manifest above, in action. `script/bundle-windows.ps1` and `script/upload-nightly.ps1` *were* fixed: they are scripts a maintainer runs by hand, and they were simply the Windows arm the phase-B rename missed.

**Verified live, against a real remote target.** Two `#[ignore]`d tests do it, and each
asserts *both* halves of the claim — that our binary lands under our own directory and
file name, **and that a neighbouring editor's upload is byte-for-byte untouched**, a
decoy planted at `~/.zed_server/zed-remote-server-dev-build` whose digest is taken
before connecting and again after. The second half is the one that matters: a client
that merely puts its own binary in the right place can still clobber somebody else's on
the way there, which is exactly what this code used to do. It is therefore asserted
*first*, as soon as the connect returns: sequenced behind the path assertion it would be
unreachable on exactly the regression it exists to catch.

- `transport::docker::tests::docker_upload_uses_our_names_and_spares_the_neighbour`
- `transport::ssh::live_tests::ssh_upload_uses_our_names_and_spares_the_neighbour`

Both were run against a real container (`ubuntu:24.04`; the SSH one against an `sshd`
inside it, reached on a published loopback port with a throwaway key and
`UserKnownHostsFile=/dev/null`, so a container is a sufficient target). Both pass and
the decoy is unchanged. Reverting `paths.rs` to the old names failed the path assertion
in both, and — with that assertion neutralised so the later one was reached — failed the
decoy assertion too, the digest moving to that of the 600 MB server binary: **the old
code overwrote the neighbour's file in place.** Needing that edit to reach the decoy
comparison is why the assertions are ordered decoy-first now. Their doc comments carry
the exact reproduction commands, including the `.sawe-live-test-target` marker a target
must already carry before either test will delete anything on it, and the teardown.

Still code-verified only, and stated rather than glossed: the **WSL** arm (not
exercisable off Windows), every **Windows** arm (the `.exe` branch of
`remote_server_binary_name`, `extract_server_binary_windows`, `bundle-windows.ps1`,
`upload-nightly.ps1`), and the host-side reaper **`cleanup_old_binaries()`**, which only
runs inside a booted headless project — the live tests stop once the binary is in
place. The reaper's prefix is pinned to the client's file name by a unit test, but
"a Sawe server no longer deletes a Zed binary" is an argument from that pinning, not an
observation. Two things about the tests themselves also postdate the run above and are
code-verified only: the decoy-first ordering, and `-F /dev/null` in the SSH test's
client arguments, which stops `ssh`, `scp` and `sftp` reading `~/.ssh/config` — without
it a `Host *` stanza carrying `ControlMaster auto` and a `ControlPath` under `~/.ssh`
would have had them *create* a socket there, so "the operator's own `~/.ssh` is
untouched" was a property of this machine's config rather than of the test. There is
deliberately no `-o ControlPath=none` beside it: those arguments are handed to the
transport, which appends its own `-o ControlPath=<temp socket>` after them, and `ssh`
keeps the **first** value it obtains for an option, so ours would win and disable the
transport's own multiplexing.

### 120. The right dock's toggles paint at the trailing edge of the project toolbar, and only the leading groups gate the divider

What: `ProjectToolbar` builds one `workspace::dock::PanelButtons` per dock (left, bottom,
right). All three used to render together at the row's leading edge. The **right** dock's
group now renders as the row's **last** child, after the run-config strip, flush against
`pr_1p5`; left and bottom stay leading. `has_dock_buttons` became
`has_leading_dock_buttons` and no longer counts the right group, or the divider that
separates the dock cluster from the project tabs would draw with nothing to its left on a
workspace whose only buttoned panel is in the right dock.

Why: the panels those buttons open — git panel, outline panel — open on the **right**, and
the maintainer asked for the control to sit on the side the thing appears. The property
that makes this a small change rather than a filter is that each `PanelButtons` is bound to
a `Dock` **entity**, not to a list of panels: `Panel::position` is re-read from settings
every frame, and `Dock::add_panel`'s `SettingsStore` observer physically moves the panel
between docks. So dragging a panel from the right dock to the left makes its button move
groups with no extra code. **Do not replace this with a per-panel predicate.**

How to apply: the array stays three entries even though no panel in this fork accepts
`DockPosition::Bottom` — `structure_node` and its order test hard-code the three-element
shape, and the array is what makes the dynamic property above work. `structure_node`'s
contract is that its children are emitted **in painted order**; reordering `render` without
reordering it lies to every agent reading `workspace.dump_visual_structure`, and
`the_toolbar_structure_node_lists_its_row_in_painted_order` is the only thing that catches
it. Note also `crates/workspace/src/dock.rs`'s `PanelButtons` comment about upstream's
right-dock button *reversal*: this fork dropped it because the buttons had moved to the
leading edge, and that premise is now gone — reinstating the reversal is an open question,
not a settled one.

### 121. The Solution band's utility buttons live in the status bar's right group

What: `solution_agent::utility_buttons::UtilityButtons` — the terminal / git-graph /
debugger switches for the Solution band's utility half — moved from `add_left_item` to
`add_right_item`, registered **before** `remote_control_status` so they paint outboard of
it, hard against the window edge.

Why: the band's utility half is the last child of a `w_full` row, i.e. genuinely flush to
the window's right edge, and the maintainer's rule is that the control belongs on the side
the panel appears — *"it's still a panel pinned to the right edge, so the buttons that open
it should be pinned to the right edge too."*

How to apply: **the right group paints in reverse registration order** (`render_right_tools`
iterates `.rev()`), so "register first" means "paint rightmost" — the opposite of the left
group. `UtilityButtons` is one composite entity rendering three buttons, so the group-level
reversal cannot scramble terminal/graph/debug among themselves. The move also *improves* on
the rationale it replaced: the old comment argued the buttons had to lead the left group so
that group's `overflow_x_hidden` would clip something else, because these icons are the only
mouse path to the git graph; in the `flex_shrink_0` right group they cannot be clipped at
all. The cost is ~70px off the left group's budget, whose first casualties are the activity
indicator, the conflict indicator and the active file name — readouts, not destinations.
`workspace.dump_visual_structure` emits **no** status-bar children by design, so this is
invisible to structural verification; `test_utility_buttons_sit_at_the_outer_end_of_the_right_status_bar_group`
is what pins it.

### 122. The Commit tab paints files above the message, and its message is the Changes tab's commit typography

What: the git panel's Commit tab now paints **changed-files header → file tree → message →
identity → branches**, mirroring the Changes tab, where the file list is on top and the
commit message at the bottom. The message renders with `git_commit_text_style` — the buffer
font at `git_commit_buffer_font_size` on `buffer_line_height` — the same typography the
Changes tab's commit editor uses, instead of the UI font it inherited before.

Why: the maintainer asked for both, and they turned out not to conflict. This fork ships
`"git_commit_buffer_font_size": 12`, so aligning to the Changes tab *is* the smaller font;
upstream's 15px default is never reached.

How to apply, and the bug that makes it work at all: `detail_text_style` set the size only
on `MarkdownStyle::base_text_style` and left `container_style` empty, while the comment
beside it insisted the container needed it too. **The comment was right.** Markdown lowers
every span through `TextStyle::to_run`, and a `TextRun` carries family, weight, colour and
decorations but **no font size and no line height**; `TextLayout::layout` reads both off the
*ambient* style, which only `container_style` refines. So the old code painted at the window
UI size and merely coincided with `TextSize::Default` because both are 14px, and any size
change here would have been a silent no-op. Both metrics are now set on the container.

Two things this does **not** change, deliberately. Reordering siblings in a `v_flex` does not
touch the height arithmetic — flexbox clamps by `min_h`/`max_h`, not DOM order — so the
message keeps its shrinkable `min_h`/`max_h` + `overflow_y_scroll` and is **not** pinned the
way the Changes tab pins its editor — true only while the user has never dragged the divider,
which #128 later added: a dragged height replaces `max_h` with `.h()`, and the floor and the
tree's floor are then what bound it; that tab can afford a hard height because its file list
has no floor and is allowed to collapse to zero, and the Commit tab's tree has a documented
72px floor. And `COMMIT_TAB_SECTIONS` is a real constant the renderer loops over, not
documentation — `test_commit_tab_paints_files_above_message` is load-bearing only because of
that.

### 123. The Commit tab shows containing branches again, with a clickable expander

What: an IDEA-shaped line under the identity row — `In 1 branch: main` /
`In 7 branches: a, b, c, d, e` plus a **`Show all`** button, and `Show less` back. This
reverses half of the ruling at `docs/plans/2026-08-30-git-panel-commit-tab.md` that dropped
it; the **ref-chips half of that ruling stands**, since decoration is not containment.

Why the reversal: the deleted `format_branches_containing` printed `and N more` as plain
**text**, so on a busy repo it announced that information existed and refused to show it.
That, not the line itself, is what made it droppable. The tail is a button now.

How to apply: `BRANCHES_CONTAINING_DEBOUNCE` (150 ms) is not optional and is the reason the
original had one — the Commit tab is driven by graph selection *including arrow-key
movement*, so an undebounced query queues one `git branch --contains` per row **ahead of the
diff the surface shows first**. The task awaits the timer before touching the repository, and
re-assigning the task handle cancels a pending one. Staleness uses the same
`commit_tab_sha()` re-check as the tab's other two loads. A remote/collab repository returns
`Ok(vec![])` — `Repository::branches_containing` has no proto path — which renders as
**nothing**, never `In 0 branches:`; an unreachable commit and a collab repo are
indistinguishable here and the function says so. Expanded, the text wraps inside a 64px
`overflow_y_scroll` block with the toggle *outside* it, so `Show less` cannot scroll out of
reach and a commit on 300 branches cannot eat the file tree.

### 124. A server-side branch delete pushes a fully-qualified refspec, and fetch prunes

What: deleting a branch on the remote pushes `refs/heads/<branch>` rather than `<branch>`;
`RealGitRepository::fetch` passes `--prune`; `Repository::fetch` now runs `rescan_branches`
after a successful local fetch; and the delete's **failure** path refreshes the branch list
before reporting, where it previously refreshed nothing at all.

Why: "git has no idempotent delete-push" is **false**, and believing it produces a much worse
fix. The failure is client-side *name resolution*, not a server refusal — a short destination
makes git resolve the name against the remote's advertised refs and abort, a qualified one
needs no resolution:

```
git push origin :absent                     → exit 1  "unable to delete 'absent': remote ref does not exist"
git push origin --delete absent             → exit 1
git push origin :refs/heads/absent          → exit 0  "remote: warning: deleting a non-existent ref"
```

Verified on git 2.43.0 over both the local-path shortcut and `file://`, and the qualified form
still deletes a ref that exists. So the fix is one `format!`, with no locale dependence, no
`ls-remote` probe and no credential prompt before the confirmation dialog. Tags were never
affected — both tag paths already spell `refs/tags/<name>`.

How to apply: an error-text classifier (`is_remote_ref_already_absent_error`) remains as a
**fallback** for a git that still refuses the qualified form; it follows the existing
`BRANCH_DELETE_FORCE_DELETE_PROMPTS` shape, matching a lowercased haystack built from both
`format_git_error_toast_message(error)` and `error.to_string()` so collab-wrapped `RpcError`
payloads match too, and it degrades to today's modal under a translated git.
**Never prune on an arbitrary failure** — a network or auth refusal says nothing about
whether the branch still exists. Separately, `LocalBranchInfo.upstream_gone` (from
`UpstreamTracking::is_gone()`) now disables the "Delete on remote" row for a `[gone]` upstream
rather than offering an action that cannot work. Note `--prune` on fetch is a behaviour change
for every Fetch and every toolbar "Update Project", and its one real cost is a user who keeps
a stale `origin/x` as a deliberate marker.

### 125. The Commit tab's file diffs share the pane's preview slot: double click summons, single click retargets

What: double-clicking a file in the Commit tab opens its diff into the pane's **preview
slot** and leaves it a preview; single-clicking another file **retargets that same tab**, but
only if the slot already holds a single-file diff — it never summons a diff from
nothing. Opening uses `focus_item = false`, so clicking down a file list keeps focus in the
git panel. This reverses the ruling that made single click selection-only.

Why: the ruling's own justification was anti-tab-spray, and it did not hold — the dedupe key
was `(sha, file)`, so double-clicking N files still produced N tabs. The machinery needed
already existed in this crate (`SoloDiffView`, FORK.md #54) and the Changes tab already
behaves this way; the Commit tab was simply never wired to it.

How to apply: **double click must not pin.** Pinning would promote the item out of the preview
slot, after which single clicks would stop retargeting and would open a *second* preview —
exactly the complaint.

**Amended 2026-09-02 (see #136) — there is no inversion left, and the guard is no longer a
type check.** This entry used to warn that the rule *inverted* the Changes tab's mapping,
where double click meant `SoloDiffOpen::Permanent`, and that a later reader would "fix" it
back. The unification made the Changes tab obey this rule too, so both tabs now behave
identically and there is nothing left to invert. What survives — and is now the rule for
**both** tabs — is that neither double click **nor `menu::Confirm` / Enter** may pin; the
reasoning in the paragraph above is exactly why, and it is unchanged. The guard also changed
shape: the two surfaces no longer have separate item types to type-check each other with (one
`SoloDiffView` serves both), so it is now the single question "does the active pane's preview
slot hold a `SoloDiffView`" (`SoloDiffView::preview_holds_a_diff`), and reuse is by *source
identity* rather than by path. `CommitView::open_internal`, its `single_file` field and
`open_file_diff` no longer exist.

Also fixed here — in code this refactor has since deleted: the
dedupe in `CommitView::open_internal` found the *index* by `(sha, single_file)` but the item
to remove by **sha alone** and then `.unwrap()`ed it, so re-opening a file could close a full
commit view open to its left; both halves now come from one `find_map`, and an already-open
tab is **activated** rather than destroyed and rebuilt, which used to re-run the async load
and drop scroll position. Known edge, not fought: Zed's own promotion gestures pin the item
behind your back, after which single clicks stop retargeting until it is summoned again.
Two of the three are reachable — double-click on the *tab*, and `TogglePreviewTab`.
**Promotion-on-edit is not**, and that is worth knowing now that this item is editable for
the working-tree source: it runs `ItemEvent::Edit` → `Pane::handle_item_edit` →
`unpreview_item_if_preview`, and `SoloDiffView` declares `EventEmitter<EditorEvent>` and
`to_item_events` but never subscribes to its editor and never emits, so no `ItemEvent` ever
reaches the pane. That is load-bearing for the gesture model and is the same fact that makes
`is_dirty` wrong (`TODO.md` C7) — anything that starts emitting item events has to solve both
at once.

### 126. The Solution tab's AI badge counts what the chat strip can draw

What: `SolutionAgentStore::visible_session_count` now also requires
`SolutionSession::can_be_active_dialog()`, the predicate the chat strip, MCP
`list_sessions`, `workspace.snapshot` and the lifecycle deltas all already use. It was the
only user-visible surface that did not.

Why: it counted everything hydration indexes under the Solution — `closed_at IS NULL`,
tabbed or not. On the maintainer's database that made one Solution's badge read **30** when
the strip showed **2** and `list_sessions` returned 2: 18 of the extras were the known
legacy orphans (cwd under members the Solution no longer has) and 10 more had lost their
`tab_order` without ever getting a `closed_at`. 65 such rows exist across the database and
the badge was the only place any of them appeared — they have no tab, and the
"Reopen Closed Chat" picker is `closed_at IS NOT NULL`. Sub-agents were **not** a factor:
`parent_session_id IS NOT NULL` matches zero rows database-wide, because teammate tabs live
in `solution_session_background_agent` and are not sessions.

How to apply: the doc comment this replaced argued the old behaviour was intentional, so
read it before re-widening. Its fear was real but aimed at a different filter — it was
guarding against excluding `is_cold()`, which would blank the badge for restored-but-tabbed
chats. That property survives, because hydration stamps `tab_order` back from
`list_open_tabs`. **Never layer `is_cold()` on top.** Keep the `live_supervisor_session_ids`
filter as well: `can_be_active_dialog` subsumes it for production judges and auditors (which
are ephemeral *and* untabbed), but the handle maps are the authoritative in-flight record and
cannot be missed by a future create path that forgets a flag. This changes only what is
counted — **no session rows are deleted**, and the standing instruction not to clean up the
legacy orphans is untouched.

### 127. The Commit tab's per-file +/− figures are derived client-side, in the pass that computes the total

What: `compute_diff_stats` (`crates/git_ui/src/git_panel/commit_tab.rs`) now returns a
`CommitDiffStats { total, per_file: HashMap<RepoPath, DiffLineCount> }` instead of a bare
`(usize, usize)`, and the changed-files rows render their own `ui::DiffStat` next to the file
name — right-aligned and `flex_shrink_0`, so a narrow dock truncates the *path* and never the
numbers, the way a Changes tab row behaves. Binary files get no figures at all and are
skipped from the total as well, which keeps `total == sum(per_file)` true by construction.
Both halves — rows and header total — are gated on `git_panel.diff_stats`, the setting whose
own documentation is "the addition/deletion change count next to each file in the Git panel"
and which the Changes tab already applies to its header total.

Why: **this withdraws the "per-file counts are out of scope" carve-out in #55 and in
`docs/plans/2026-08-30-git-panel-commit-tab.md`.** Both rested on "`CommitFile` carries no
numstat, so this needs a new diff-stat load path", and that premise died the moment the
header's total was computed here: the total is `line_diff(old_text, new_text)` folded over
every file of the `CommitDiff`, so the per-file figures were already being produced and then
thrown away. No new git invocation, no new proto message, no new load path — only a map that
is kept instead of discarded.

How to apply: the counts must stay on the **load** side. `line_diff` runs once per file of
the commit, and the Commit tab re-renders on every panel notify, so a row that derives its
own figures would run the whole commit's diff on the render path; `ChangedFileEntry` takes
its `stat` as a parameter for exactly that reason and `changed_file_entries` is the single
join between the map and the rows. Directory header rows deliberately show **no** subtotal:
they already carry a muted "N files" count, the whole-commit total sits in the header
directly above the tree, and a third number would crowd a row whose path is already
`truncate_start`-ed at dock width.

### 128. The Commit tab's message is resizable, in pixels, committed on every drag move

What: a drag handle between the changed-files tree and the commit-message block.
`GitPanel::commit_message_height: Option<Pixels>` — `None` is the automatic layout
(`min_h` + `COMMIT_MESSAGE_MAX_HEIGHT`), `Some(h)` applies `.h(h)` and **drops the cap
entirely**, leaving the message's own floor and the tree's `min_h` to bound it through the
flex pass. Double-click resets to `None`. Persisted per workspace as a `#[serde(default)]`
field on `SerializedGitPanel`. This reverses the second half of the "no resize split inside
the Commit tab" ruling; the deleted `SplitState` pair did **not** come back — one field.

Why pixels and not a fraction: FORK.md #81's failure mode, and the Commit tab is exposed to
it — a fraction inside this flex chain never resolves, the cap silently evaporates, and the
sibling `uniform_list` then measures against an overflowing container and renders **zero
rows**. The tree *is* a `uniform_list`.

Why the cap has to go once dragged: `COMMIT_MESSAGE_MAX_HEIGHT` is layout *policy*
("however tall the panel, the message never exceeds 200px, surplus goes to the tree"), not a
safety rail. A drag is the user overriding that policy, so keeping the cap would freeze the
divider at 200px — broken exactly where they are pulling.

How to apply — the three traps, in the order they bite:

**The height is measured absolutely, from the block's painted bottom edge to the cursor, and
must NOT be capped at what the last frame granted.** That cap looks obviously right and was
written, reviewed and removed: `bounds` is the hitbox from the **last paint**, and both X11
and Wayland dispatch a whole batch of motion events back to back with no draw between them,
so two moves in one frame read the same stale bounds — the first raises the height, the
second sees a phantom shortfall and puts it back. An even number of motions per frame means
an upward drag does not move at all while downward stays smooth, and a temporarily short
panel can permanently overwrite a taller stored height. The cap was also unnecessary: because
the value is absolute rather than a delta, reversal is already immediate — the moment the
cursor descends past the painted divider the value drops and the divider follows on that very
event. **Neither a live drag nor an MCP `drag_at` can reproduce this** (both yield a frame per
step), and the unit tests feed `bounds` by hand, so nothing but reading catches it.

**Commit in `on_drag_move`, never `on_drop`** — #84 and #92 both record it, and the deleted
predecessor shipped exactly that bug: a `deferred` + `block_mouse_except_scroll` handle is the
topmost hitbox for the whole drag, so the container's `on_drop` never fires.

**The height belongs to `GitPanel`, not `CommitTabState`** — that struct is rebuilt on every
selection push, so a height stored there resets each time the user arrow-keys to another
commit. That is the bug `LastSplitRatio` exists to prevent.

Also: a drag never produces a click (crossing gpui's drag threshold takes the pending
mouse-down), so the double-click reset cannot be triggered by a fast second drag.

### 129. Both Commit-tab row kinds are pinned to the Changes tab's row height, because `ButtonLike` and `Label` disagree

What: the Commit tab's changed-files rows and directory headers render at the Changes tab's
sizes — file names and directory paths at `LabelSize::Default`, the "N file(s)" count staying
`Small`, exactly as `changes_list.rs` splits it — and **both** row kinds carry
`.height(changes_list::list_item_height())`, a `rems(1.75)` value now shared by the two tabs
rather than duplicated.

Why the height had to come with the font: `ButtonLike` pins its own height at
`ButtonSize::Default` = 22px, while a `LabelSize::Default` line box is `round(14 × 1.618)` ≈
23px. A bare size swap overflows the button by a pixel and leaves the rows visibly denser
than the Changes tab's — same font, wrong rhythm, which is not "aligned typography". And
`uniform_list` measures **item 0** and applies that height to every row, so two row kinds at
two different heights clip rather than merely misalign. Both kinds get the same value or
neither does.

Note the deliberate asymmetry with `COMMIT_FILE_TREE_MIN_HEIGHT`: the row height is `Rems`
and scales with `ui_font_size`, the tree's floor is a pixel constant and does not. The floor
promises "a guaranteed share rather than zero pixels", never a row count, so it stays 72px —
2.6 rows at the default rem instead of the 3.3 it was. Do not re-derive it from the row
height.

### 130. A search popover with no `menu::Confirm` handler is not a focus bug

What: the Solution and Project tab strips' `+` popovers were hand-rolled — a single-line `Editor` plus a `div` of clickable rows, with no selection cursor and, between them, exactly one registered action (`menu::Cancel`). Enter did nothing and the arrow keys did nothing. Both are now `Picker::list(...).modal(false)`, with the action rows (`Create new solution…`, `Add project from git…`) as **in-list entries pinned at index 0**, so Enter targets the first *match* and the action rows are still reachable by arrow key.

Why it was never a focus problem, which is where an hour goes if you assume otherwise: a single-line editor's key context is `mode == single_line`, so the `Editor` binding `enter → editor::Newline` (which requires `mode == full`) does not match, and the global `enter → menu::Confirm` is the only candidate; it dispatches from the focused editor up through the popover container. The proof by existing behaviour was already on screen — **`escape` worked**, via `Editor::cancel`'s `cx.propagate()` into the container's `menu::Cancel`. Same for the arrows: `Editor::move_up`/`move_down` propagate immediately in single-line mode. The keystrokes always arrived; there was nothing to receive them.

How to apply: **not `render_header`** for the action rows, which is what this decision originally specified and an implementer correctly refused — `Picker` renders the header inside `.when(match_count() > 0, …)`, so it disappears exactly when the filter matches nothing and `Create new solution…` is the only thing left to do. **Not `uniform_list`** either: the rows are not homogeneous (a solution row carries a hover trash button, the action row a smaller icon), and `uniform_list` measures item 0 and applies that height to all. `Picker::list` is the variant for that and still gives scroll-into-view, which is the whole reason to prefer `Picker` over hand-rolling — the fork's one hand-rolled popover keyboard layer, `BranchesPopup`, admits in a comment that it has none.

Consequence to know: `tab` is globally bound to `menu::SelectNext`, so it now moves the selection in these popovers. Nothing there wanted it.

### 131. The git graph's filter popovers get a cursor, not a `Picker`

What: the branch / user / path filter popovers in the git-graph toolbar gained arrow-key navigation, `Enter` to toggle the cursored checkbox, `ctrl-enter` to apply, `escape` to dismiss, scroll-into-view, and — a second bug — **focus on their search field when they open**, which they never had, so the user had to click into the field before typing.

Why not `Picker`: these are **multi-select with checkboxes and an explicit Apply footer**, and `PickerDelegate::confirm(secondary)` has no vocabulary for "toggle this row, stay open, apply the set later". The decisive constraint on `Enter`'s meaning is that `space` is unavailable — it is text input in the focused search field — so if `Enter` applied-and-closed there would be **no** keyboard route to a checkbox at all and the popover would be mouse-only. Hence toggle-and-stay-open.

How to apply: the cursor is a **second, orthogonal** notion to the existing checked `BTreeSet` — do not overload that field. The cursor resets to the first actionable row on every rebuild rather than being clamped, because after a query change index 3 of the old ranking has no relationship to index 3 of the new one. `Focusable` must return the *query editor's* handle: `PopoverMenu::show_menu` focuses whatever `focus_handle(cx)` returns, so returning the container handle is precisely the bug, and returning the query's handle also means the two-frame deferral inside `show_menu` cannot race the constructor's own focus call.

### 132. Resolving a conflict is a staging gesture, and the panel now says so

What: a family of fixes to the merge-conflict flow, prompted by the maintainer resolving a conflict's text and asking what to do next. The workflow already worked — ticking a conflicted row runs `git update-index --add --remove`, which resolves the unmerged index entry, after which `Commit` enables and produces a proper two-parent merge commit — but nothing said so. Now: an in-progress-operation banner above the commit box naming the op, the unresolved count and the gesture, with a confirming `Abort`; the checkbox and context menu say **Mark Resolved / Mark Unresolved** instead of Stage / Unstage for a conflicted path; the row menu offers **Resolve Conflicts…**; and a conflicted row opens the resolver rather than a single-file diff.

Why the last one: `SoloDiffView`'s index side is `git show :<path>`, which **fails on an unmerged path** (`is in the index, but not at stage 0`), so the diff had no index base and the toolbar read `0 differences` over the one file blocking everything. The tempting deep fix — falling back to `git show :2:<path>` — is **wrong**: it would make `BufferDiff`'s index base a lie for staging purposes, and hunk-level stage/unstage against an unmerged path could then silently write a wrong index. Gate on the **live** `status.is_conflicted()`, not the sticky `had_conflict_on_last_merge_head_change`, so a row that has been marked resolved diffs normally again.

Also fixed, and the reason none of this was reachable before: the fork's own 3-way resolver had exactly one in-product entry point (a merge started from the branch picker's context menu), and its `Continue` was broken twice over — see #133.

### 133. `git merge --continue` needs `GIT_EDITOR`, and "unrelated changes" is staged-minus-incoming

What: the conflict resolver's `Continue` now spawns git with `GIT_EDITOR=true` and `GIT_SEQUENCE_EDITOR=true`, surfaces its errors instead of `.log_err()`-ing them, and blocks only on **staged paths that the incoming change did not touch**.

Why the editor override: `git merge --continue` opens `$EDITOR`. Spawned with piped stdio it fails with `Standard input is not a terminal` / `error: There was a problem with the editor 'editor'`, exit 1, and no commit — and the failure went to the log and nowhere else. **`--no-edit` does not work**: `fatal: --continue expects no arguments`.

Why the guard had to be rewritten rather than narrowed: it counted *any* non-conflict porcelain record as unrelated, **including untracked files**, so a merge with a stray `.md` in the tree could not be continued at all, silently — `log::warn!` plus a bare `notify`. Narrowing it to untracked-only was the obvious fix and is wrong; narrowing it to *staged-only* is also wrong, and this is the non-obvious part: **a merge auto-stages every path the incoming side changed**, so "any staged path" flags the merge's own work and produces the same dead end by a different route. The rule is staged minus `git diff --name-only HEAD...MERGE_HEAD` (or `<PICK>^ <PICK>` for rebase/cherry-pick/revert). The tempting cheaper spelling `git diff --cached --name-only MERGE_HEAD` misclassifies a cleanly auto-merged file as the user's work.

How to apply: the guard **fails open** — if `git status` or `git diff` cannot run, `Continue` proceeds. Blocking on a failed courtesy check is exactly the dead end being fixed. Feedback is a toast rather than a disabled button, because the condition is only knowable by spawning git: a cached predicate would leave the button wrongly disabled with a lying tooltip the moment the user unstages the offending file.

### 134. The Commit tab's tag row is `--points-at`, not `--contains`

What: `GitRepository::tags_pointing_at` (`git tag --points-at <sha>`), a sibling of the existing `tags_containing` rather than a replacement — the latter still serves `CommitView`'s "Contains" panel, where containment is the correct semantic.

Why: the two answer different questions and the difference is enormous, not cosmetic. `--contains` is a reachability query, so on any repo that tags releases, a commit from a year ago is contained in *every subsequent tag* — dozens of names. The maintainer wanted the commit's own tags, and there can be more than one (a monorepo release commit carries one tag per published package; a repo with moving aliases stacks `v1.4`, `v1.4.0`, `stable`, `latest`). The discriminating test is a commit that is an **ancestor** of a tagged release and carries no tag itself: `--points-at` returns empty, `--contains` returns every later tag.

How to apply: the branches half of the same row stays `--contains` — `In N branches:` is genuinely a containment question, and IDEA shows it that way too. Both queries share one 150 ms-debounced task, one `futures::join!` and one staleness guard, so they cannot disagree about which commit they describe; the debounce is not optional, because the tab is driven by graph selection including arrow-key movement.

### 135. The blame gutter is a date and a name, and its width reservation is an upper bound the renderer states itself

What: the per-line gutter entry (`crates/git_ui/src/blame_ui.rs::render_blame_entry_with_options`) went from `b0fc119 <avatar> Alexandr Taushkanov … 1 year, 10 months ago` — around 45 monospace columns, right-aligned against the line numbers — to `21 Mar 2019 Taushkanov`, left-aligned, typically 20-24. The font is unchanged: this fork's gutter stays monospace, and only the content shrank.

- **The SHA is gone.** It cost seven columns plus a gap on every line and answered a question the row already answers three other ways: the hover tooltip shows it, the right-click menu copies it, and a left click opens the commit. The per-commit colour it used to be tinted with moved onto the author name, so the "these lines are one commit" cue survives the removal.
- **The author is `git::blame::display_author`**, shared by the renderer and the width calculation. It only ever *selects a slice* of what git reported — family name before a comma, else the last whitespace token, else the local part of an address — so the 34% of this repository's 1931 authors who are a single handle, and every bot name, pass through untouched. Measured, not assumed: 60% are `First Last`, and the shortened names come out at a median of 6 columns and a 99th percentile of 15.
- **The date is `git_ui::format_compact_date`**, the Commit tab's `[day] [month repr:short] [year]` lifted to the crate root so the two cannot drift. Absolute beats relative twice over: `21 Mar 2019` is half the width of `1 year, 10 months ago` and strictly more precise.
- **The avatar defaults off** (`git.blame.show_avatar`, `assets/settings/default.json`). It is another column in the narrowest place in the window and, without a remote that serves avatars, it is a generic person icon carrying nothing the name does not.

Why the width reservation had to change with it: `EditorSnapshot::gutter_dimensions` reserves `columns * ch_advance` pixels for the blame column, and the blame element is prepainted into that width with nothing clipping it — so an under-estimate does not truncate, it **paints over the line numbers**. The old estimate budgeted `"60 minutes ago"`, seven columns short of the `"1 year, 10 months ago"` that `time_format` actually produces past 12 months, and counted the author in **bytes** (`String::len`), which is roughly double for Cyrillic and short for CJK. It also budgeted nothing at all for the avatar. That is the collision in the screenshot, and it is a measurement bug, not a preference.

How to apply: the renderer owns the row's layout, so it is the only thing that can price it — `BlameRenderer::gutter_fixed_columns(cx)` reports the date, the gaps and the avatar, `max_author_columns()` caps the name, and `GitBlame::max_author_display_columns` measures the *shortened* name in **display columns** (`unicode-width`), never bytes and never `char`s. If you add an element to the row, add its columns there in the same commit. `truncate_to_columns` (not `util::truncate_and_trailoff`, which counts `char`s) keeps the drawn name inside the cap, and `.overflow_x_hidden()` on the row is the backstop that turns any future mis-measurement into a clipped name instead of text over the line numbers.

Not done: IntelliJ also tints runs of lines from one commit and prints the metadata once per run. `layout_blame_entries` does have the neighbouring rows in hand, so it is reachable — but only by adding a run-position argument to both `BlameRenderer::render_blame_entry*` methods, painting the run background in `element.rs`, and keeping the continuation rows interactive so hovering one still shows its commit. That is a redesign of the trait, not a width change, and it is deliberately left as a follow-up.

### 136. One `SoloDiffView` with a `DiffSource` serves both git-panel tabs, and the open gesture names what the user did

The maintainer's ruling that started it, verbatim: *«Вообще мне видится, что это должен быть
один компонент с флагом "editable". Я за рефакторинг.»*

What: `SoloDiffView` (`crates/git_ui/src/solo_diff_view.rs`) renders one file's diff from
either of two sources, and `CommitView`'s single-file mode is **deleted** — `single_file`,
`open_file_diff`, `preview_holds_single_file_diff` and `open_internal` with it. The Commit tab
calls `SoloDiffView::open_commit_file`, the Changes tab calls `SoloDiffView::open_or_focus`,
and `CommitView` is left serving the whole-commit view and the `base..head` compare-range view
(#87). The historic-blob loader they share moved out to `crates/git_ui/src/commit_blob.rs`.

Why a `DiffSource` enum and not the bare `editable` bool the ruling proposed: a bool answers
one question, and the view has to derive **four** things from the same fact — the multibuffer
shape (`MultiBuffer::singleton` over a live project buffer vs
`MultiBuffer::without_headers(Capability::ReadOnly)` over detached blobs), the capability, the
blame base, and the tab's identity (used both for dedupe in `resolve_gesture` and for the git
panel's open-diff mark, `OpenDiff::from_active_item`). Those four must not be able to
disagree, and a bool has nowhere to hang the sha that two of them need. Everything the modes
differ about is a method on the source — `is_editable`, `matches`, `blame_base`, `tab_icon`,
`tab_title` — and the two places that still branch on the variant do it in one spot each
(`SoloDiffView::new` for the multibuffer, `configure_editor_for_source` for the editor), never
scattered through the view. `SoloDiffView::new` also carries a
`debug_assert!` that the `DiffSource` and the `LoadedDiff` handed to it are the same variant,
because a mismatched pair builds a view that is read-only in one respect and editable in
another.

**There are exactly two essential differences, and blame is not one of them.** Editability and
hunk staging both follow from the single fact that the right-hand side is a live project
buffer in one case and a detached historic blob in the other; `can_save` and `save` read
`DiffSource::is_editable()`, and `disable_diff_hunk_controls` is applied by
`configure_editor_for_source` matching the variant. Blame is a **parameter**:
`blame_base()` is `Some("HEAD")` for the working tree (the left pane holds the file at HEAD)
and `None` for a commit. The `None` is mechanical, not conceptual —
`SplittableEditor::sync_lhs_blame_sources` (`crates/editor/src/split.rs`) resolves the
`(repository, repo_path)` to blame through `repository_and_path_for_buffer_id` on the
**right-hand** buffer id, and a detached blob is not in the project's buffer store, so every
source it builds is dropped. Wiring commit-mode blame needs an explicit repository override on
`SplittableEditor`: a separate change inside `editor`, with its own tests. #59 records the
same gap from the other end — its "how to apply" lists `commit_view` among the consumers that
deliberately do **not** opt in, because *both* of its panes are detached buffers — and its
ordering trap applies here too: `sync_lhs_blame_sources` prunes entries whose base buffer is
no longer excerpted, so it must run **after** the excerpts are installed, never before.

The gesture model — the maintainer's, now the rule for **both** tabs (amends #125):

| gesture | behaviour |
|---|---|
| double click (either tab) | **summon** the shared diff into the active pane's preview slot; never pin; focus stays in the panel (`DiffOpen::Summon { focus: false }`) |
| — with `preview_tabs.enabled: false` | there is no shared slot to click into, so `add_to_pane` falls back to a **permanent, focused** tab for every summon. Both halves of the row above are off under that setting, and deliberately so. |
| single click (either tab) | **retarget** the shared diff if one is open; do nothing at all if it is not (`DiffOpen::Retarget`) |
| arrow-key step (Changes) | same as single click — retarget only |
| `menu::Confirm` / Enter (Changes) | summon **and** focus (`Summon { focus: true }`); still never pins |
| `menu::SecondaryConfirm` (cmd-click / alt-enter) | unchanged — the stacked `ProjectDiff` accordion |

`DiffOpen` names the gesture rather than the destination (`SoloDiffOpen { Preview, Permanent }`
is gone) because the placement is never the caller's decision once one tab is shared: a summon
always takes the preview slot and a retarget always reuses whatever is in it. That is the
property that keeps the two tabs from drifting apart again — and it is a behaviour change for
the **Changes** tab, which used to pin on double click and summon a preview from nothing on
every single click and every arrow step.

How to apply — three things a future reader must not "fix":

**Neither double click nor Enter may pin.** Nothing in the open path calls
`unpreview_item_if_preview` any more; that call *was* pinning. Pinning promotes the item out
of the preview slot, `preview_holds_a_diff` then answers false, and the next single click
summons a *second* tab — the exact complaint #125 was written to fix. Pinning stays reachable
through the editor's own double-click-on-the-*tab* gesture and `TogglePreviewTab`.

**`resolve_gesture` declines a `Retarget` *before* it searches for an existing view, not
after.** Search-first looks obviously better (it would surface a diff parked in another pane)
and is wrong: the pre-unification Changes tab gated the whole call in `move_diff_to_entry`, so
a declined arrow step never reached a workspace-wide activate. Searching first lets an
arrow-key step flip a pane onto a pinned diff and never flip back. The guard is the *only*
mode-specific step: a `Retarget` that passes it reaches the same workspace-wide reuse as a
`Summon` and will activate a match in any pane. A declined retarget never searches.

**Conflict routing follows the summon gesture, not the pin gesture.** A conflicted file opens
the merge resolver on `Summon` and does nothing on `Retarget` — the old rule was `Permanent`
opens / `Preview` does nothing, expressed against the gesture that survived. Stepping past a
conflict with the arrow keys must not spawn a three-pane merge view.

Not carried over deliberately: `ExplainCommit` is bound unconditionally on `CommitView`'s root
element (`commit_view.rs`), so it was dispatchable in single-file mode and spawned an AI task
whose output nothing rendered. Deleting the mode closed that **for the single-file case only**
— do not re-add the binding to `SoloDiffView`. The hole itself survives for the compare-range
view, which returns before `render_metadata_panel` and so has nowhere to draw the answer
either; that is a separate one-line fix in `CommitView`.

### 137. Who paints the diff style controls is stated by the consumer, and the search bar's toolbar location is one predicate

What: `SplittableEditor` gained `set_style_controls_painted_by_consumer(bool)` /
`style_controls_painted_by_consumer()` (`crates/editor/src/split.rs`), and
`BufferSearchBar::paints_diff_style_controls` (`crates/search/src/buffer_search.rs`) is the
single gate on the four diff style buttons — previous hunk, next hunk, Unified, Split — that
the bar draws in `PrimaryLeft`. `SoloDiffView` is the one consumer that sets the flag, because
it paints the same quartet itself in `SoloDiffStyleToolbar`, which lands in the same
`PrimaryLeft` slot. Alongside it: `keeps_primary_left()` = "the leading group has anything in
it" is what all **three** location emitters ask, and `has_files_to_collapse()` is what
"Collapse All Files" asks.

Why the flag lives on the consumer: `search` sits below `git_ui` / `editor`'s diff views and
cannot name them, and "who paints this" is a fact about the *consumer*, not about the
multibuffer's shape. The commit-source `SoloDiffView` is the case that exposed it — its
multibuffer is not a singleton, so the bar drew a second identical quartet beside the view's
own. The working-tree source escaped only because its multibuffer happens to be a singleton,
which is a coincidence rather than a reason.

Why this is **not** welded to "Collapse All Files": it was, and that shipped two regressions in
one task. The collapse predicate and the style controls ask different questions about
different buttons. `MultiBuffer::new` sets `show_headers: true` in its constructor while
`without_headers` leaves it `false`, so a headered multibuffer answers the collapse predicate
`true` even with **zero** buffers — which is why only `CommitView`'s compare-range mode
(`compact = compare_range.is_some()`, the one headerless multi-file case) lost its controls
while its blobs were still loading asynchronously. Narrow the claim to that: it is not "any
multi-file commit view". Conversely, gating the style controls on "the multibuffer spans more
than one buffer" would have silently stripped hunk navigation and Unified/Split from one-file
`ProjectDiff`, one-file whole-commit `CommitView`, one-file compare-range and
`text_diff_view` — four everyday surfaces.

One more surface changed and nobody set out to change it: **the LSP Logs toolbar loses a
button that never worked.** `LspLogView` does not override `Item::buffer_kind`, so it inherits
`ItemBufferKind::None`, skipped the old early return and got the leading group — a lone
collapse chevron that sat in `PrimaryLeft` even while dismissed. Its editor is
`Editor::multi_line` → `MultiBuffer::singleton`, so `has_files_to_collapse` now answers
`false` and the bar drops to `Secondary` on `ctrl-f` / `Hidden` on Escape. The button was
inert either way: `Editor::fold_all` takes the singleton branch and folded *syntax creases* in
a plain-text log, and `has_any_buffer_folded` hard-returns `false` for a singleton, so the icon
could never flip to "Expand All Files". Read as an improvement, not a regression.

Deliberate scope widening, recorded because it is user-visible: `file_diff_view` now shows the
quartet, which it never had. It is the only `SplittableEditor` consumer affected, its sibling
`text_diff_view` already had them, and a `buffer_kind` carve-out to exclude it would reinstate
exactly the class of gate that caused both regressions.

How to apply — **the failure mode here is one decision expressed in more than one place where
the copies can disagree**, and all three regressions in this work had that shape: the
`split_buttons` element vs. the toolbar location; the location emitted independently by
`set_active_pane_item`, `show()` and `dismiss()` with nothing recomputing it afterwards; and
the painted row vs. the predicate that was supposed to describe it. So: the row's visibility
condition is `split_buttons.is_some()` — the thing actually being rendered — not a
separately-computed boolean; and every emitter calls `keeps_primary_left`, never
`needs_expand_collapse_option` directly. If you add a fourth emitter, it calls the same
function.

And **assert on the painted element tree, not on the predicate** — each of the three shipped
green because a test asserted the predicate and not its consequence. The paint tests here do
that with `VisualTestContext::debug_bounds`; the idiom, its precedents and its traps (including
the one hole these tests still have) are in
`docs/findings/2026-09-02-paint-tests-with-debug-bounds.md`.
