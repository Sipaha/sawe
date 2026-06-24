# Session handoff — 2026-05-26

**Status:** session paused for context reset; resume on branch `hook-inject`.

Active arc: **ConsolePanel** — unified bottom-dock panel hosting both terminal
and AI-chat tabs, plus a layout change so Right Dock spans the full
workspace-area height. Phase A (layout shift) shipped end-to-end. Phase B
(panel merge) is at 7 / 15 tasks — `ConsolePanel` skeleton, two providers,
settings, icon, and the tab-strip render are all in. Remaining: `+` popover,
tab context menus, persistence, wire-up + deletion of old `TerminalPanel` /
`SolutionSessionsNavigator`, cleanup of obsolete settings + actions + keymap
entries, docs, e2e test, final screenshot.

Spec + plan live on disk (gitignored):

- Spec: `docs/superpowers/specs/2026-05-25-console-panel-design.md`
- Plan: `docs/superpowers/plans/2026-05-25-console-panel.md`

## What shipped on 2026-05-26

| Phase | Commit | Summary |
|---|---|---|
| A1 | `8c456278b5` | Delete `BottomDockLayout` enum + `workspace.bottom_dock_layout` setting. 4-arm match in `Workspace::render` collapses to the body of the old `Contained` arm. 7 files, ~217 net deletions. |
| A2 | `f9cc92c2d2` | Restructure `Workspace::render`: Right Dock moves into the outer flex-row (sibling of the centre-bottom column), so Right runs the full workspace-area height; Bottom Dock stays nested with Centre and stops at Right's left edge. |
| A3 | `b9b96e98ae` | Bug found via MCP `dump_visual_structure`: `render_dock`'s container had no `h_full()` for horizontal docks, so Left/Right collapsed to height 0 after A2's restructure. Fix: add `h_full()` to the container when `position.axis() == Axis::Horizontal`. 7 insertions. |
| A4 | `091d689d80` | Snapshot test `right_dock_spans_full_workspace_height` in `workspace.rs` `tests` mod. Asserts right.top == dock-row.top, right.bottom == dock-row.bottom, bottom.right == right.left. Added `debug_selector` annotations on `render_dock` containers + the dock-row `h_flex`. |
| B1 | `d766535be4` + `ed2daae399` | Scaffold `crates/console_panel/` crate. 20 deps in `[dependencies]`, `*/test-support` only in `[dev-dependencies]` per memory rule. Empty `pub fn init`. Cargo.lock followup commit. |
| B2 | `b3c47b2a91` | `IconName::Console` enum variant + `assets/icons/console.svg` (speech-bubble outline containing `>` prompt + underline cursor). Asset-exists + strum-snake-case smoke tests pass. |
| B3 | `0f5be9aa47` | `ConsolePanelSettings` (in `crates/console_panel/src/console_panel_settings.rs`) + `ConsolePanelSettingsContent` (in `crates/settings_content/src/console_panel.rs`). Fields: `default_position: DockPosition`, `default_width: Pixels`, `default_height: Pixels`, `button_visible: bool`. Defaults: Bottom / 360 / 240 / true. `vscode_import.rs` + `assets/settings/default.json` updated. |
| B4 | `8db67cbfea` + `d16bc82a2d` | `TerminalProvider` — stateless facade. API: `new_tab(cwd, window, cx) -> Task<Result<Entity<TerminalView>>>`. Spawns via `Project::create_terminal_shell(cwd, cx)` then `TerminalView::new(terminal, ws.weak_handle(), ws.database_id(), project.downgrade(), window, cx)`. Async dispatched through `Window::spawn`. Cargo.lock followup. |
| B5 | `5f54dd618e` | `ChatProvider` + `ChatProviderEvent::SessionCreatedExternally(SolutionSessionId)`. API: `new_tab(solution_id, agent_id: AgentServerId, cwd, window, cx)` and `new_tab_from_existing(session_id, window, cx)`. Subscribes to `SolutionAgentStoreEvent::SessionCreated { id, parent_session_id }`. Uses `SolutionSessionView::new(session_id, session, workspace, navigator, window, cx)` with `navigator = WeakEntity::new_invalid()` — view gracefully no-ops the navigator status row, ConsolePanel provides its own chrome. |
| B6 | `173951e0a8` | `ConsolePanel` skeleton + `impl Panel`. Fields: `workspace`, `tabs: Vec<ConsoleTab>`, `active_index`, `dock_position`, `width`, `height`, `terminal_provider`, `chat_provider`, `focus_handle`, `_subscriptions`. `ConsoleTab::{Terminal { view }, Chat { view, session_id }}`. Actions `gpui::actions!(console_panel, [ToggleFocus, NewTerminal, NewChat])`. Trait requires both `persistent_name()` and `panel_key()` — both return `"ConsolePanel"`. `activation_priority = 2`. Icon `Some(IconName::Console)`, tooltip `"Toggle Console"`. |
| B7 | `924c54ad20` | Tab-strip render. Terminal-tab icon = `IconName::Terminal`, title via `TerminalView::tab_content_text(0, cx)` (requires `use workspace::Item`). Chat-tab icon = `IconName::Sparkle` (NOT `ZedAssistant`), title looked up via `SolutionAgentStore::global(cx).read_with(\|s,_\| s.session(*session_id))` → `.title.clone()`, fallback `session_id.to_string()`. `activate_tab` / `close_tab` with correct active-index bookkeeping (decrement if removed-before, clamp to last if removed-active, None if list empties). Active tab embedded as `.child(view.clone())` because `Entity<T: Render>: IntoElement`. |

13 commits. Phase A complete; Phase B at 7 / 15.

## Outstanding pool (Phase B, in execution order)

Each task references the plan's section. Plan is at `docs/superpowers/plans/2026-05-25-console-panel.md`.

| Item | Track | Notes |
|---|---|---|
| **B8** `+` popover | LIGHT-MEDIUM | Three-item `PopoverMenu` (New Terminal / New AI Chat / Spawn Task…). `New AI Chat` disabled when no active solution. Wire workspace-level `register_action` calls (`NewTerminal`, `NewChat`, `ToggleFocus`) inside `console_panel::init`. Caller of `ChatProvider::new_tab` must pass `solution_agent::claude_adapter::CLAUDE_ACP_AGENT_ID` as `agent_id` — flagged in B5. |
| **B9** tab context menus | LIGHT | Right-click on tab. Terminal: Close / Rename Tab / Reveal CWD in Project Panel. Chat: Close / Rename Session / Restart Agent. **No** Close Others / Close All / Move to Dock / Reset Context — user removed these. |
| **B10** persistence | HEAVY | New `workspace.db` table `console_panel_state(workspace_id, tab_index, kind TEXT CHECK kind IN ('terminal','chat'), item_id TEXT, cwd TEXT, active INTEGER)`. `cwd` is per-row, terminal-only; chat rows store NULL. **OQ1 resolved:** on restart re-spawn a fresh shell at the stored CWD. Save: `view.read(cx).working_directory(cx)` (or terminal-model accessor) on Terminal tabs. Load: `TerminalProvider::new_tab(Some(stored_cwd), …)` for terminal rows; `ChatProvider::new_tab_from_existing(session_id, …)` for chat rows. Persistence triggers at end of `activate_tab` / `close_tab` / `add_*_tab` / `set_position`. SQL queries live in `crates/workspace/src/persistence.rs` (inline `sql!` macros, no separate migrations directory). |
| **B11** wire-up + delete old | HEAVY | In `crates/zed/src/zed.rs::initialize_panels` (≈line 715), drop the `TerminalPanel::load` and the Navigator registration, add `ConsolePanel::load(workspace_handle.downgrade(), store, …)` via the existing `add_panel_when_ready` helper. `git rm crates/terminal_view/src/terminal_panel.rs` + `git rm crates/solution_agent/src/navigator.rs`. Fix every compile-error from external call sites (Spawn Task modal, LSP run-task, dap, hover "Run in terminal" — find via `grep -rn "TerminalPanel\\|SolutionSessionsNavigator" crates/`). |
| **B12** settings + actions + keymap cleanup | LIGHT-MEDIUM | Hard removal (no aliases). `crates/terminal/src/terminal_settings.rs`: drop `pub dock` field. Mirror in `crates/settings_content/src/terminal.rs`. `assets/settings/default.json`: drop `terminal.dock`. `crates/solution_agent/src/actions.rs`: delete only `FocusNavigator`; **OQ3 rule** says keep `NewSession` / `CycleSession` / `FocusActiveSession` / `DuplicateSession` / `CloseSession` and re-route their handlers to operate on `ConsolePanel` chat tabs (move the registration into `crates/console_panel/src/console_panel.rs::init`). Default keymaps: replace `ctrl-\`` → `terminal_panel::Toggle` with `ctrl-\`` → `console_panel::ToggleFocus` in both `assets/keymaps/default-linux.json:660` and `default-macos.json:715`. |
| **B13** docs | LIGHT | `CLAUDE.md`: replace `solution_agent::FocusNavigator` references with `console_panel::ToggleFocus` (search `windows.dispatch_action`). `FORK.md`: add entry to the touched-files table for `crates/workspace/src/workspace.rs` (first local edit to its render method) + a numbered "Key architectural decisions" entry naming the merge. |
| **B14** e2e test via MCP | MEDIUM | `crates/console_panel/tests/integration_test.rs`. Boot via the `editor_mcp::set_runtime_dir_for_test` pattern from `crates/editor_mcp/tests/solutions_add_member_e2e_test.rs`. Dispatch `workspace::ToggleBottomDock` + `console_panel::NewTerminal` + `console_panel::NewChat`; verify the tabs appear (`workspace.dump_visual_structure`) and a screenshot is non-empty (`workspace.screenshot`). |
| **B15** final screenshots into `docs/findings/2026-05-25-console-panel-shipped/` | LIGHT | Run via `script/run-mcp --debug --headless`, save 3 screenshots: layout shows right-full-height; mixed-tab strip; chat-tab active. |

## Architectural decisions worth carrying forward

1. **Two-phase delivery.** Phase A (layout) is independent of Phase B (panel merge) and was committed separately so screenshot regressions land before the panel surgery. After A2, `render_dock` needed an explicit `h_full()` for horizontal docks (A3) — that fix is part of the layout-area, NOT the new panel.
2. **`render_dock` `h_full()` for horizontal axis.** Without it, the outer flex-row gives the dock zero height when no intermediate wrapper sets one. Bottom Dock is `Axis::Vertical` and unaffected. Documented in A3's commit body.
3. **`debug_selector` for layout tests.** `crates/workspace/src/workspace.rs` now has 4 selectors: `left-dock-container`, `right-dock-container`, `bottom-dock-container`, `dock-row`. Test in `tests` mod uses `VisualTestContext::debug_bounds` to read rendered geometry by selector. Pattern is `#[cfg(any(test, feature = "test-support"))]`-gated, zero production cost.
4. **`ConsolePanel` is a thin coordinator.** It owns `tabs: Vec<ConsoleTab>` and `active_index`. The two providers (`TerminalProvider`, `ChatProvider`) are stateless spawn-helpers; they do NOT track open items. This matches the Plan's "option B" decision the user picked during brainstorming.
5. **`Entity<T: Render>: IntoElement`** — embedding terminal / session views as `.child(view.clone())` works because `gpui::prelude` re-exports the conversion. No wrapper needed.
6. **Chat icon is `IconName::Sparkle`**, not `ZedAssistant`. Verified by `navigator.rs` line 991 and the adapter default.
7. **Title lookup for chat tabs goes through the store**, not the view — `SolutionSessionView` has no public title accessor; the store is the source of truth.
8. **`SolutionSessionView::new` takes a navigator weak entity** that ConsolePanel passes as `WeakEntity::new_invalid()`. The view's status row gracefully no-ops on `navigator.upgrade().is_none()`; this was checked by reading `SolutionSessionView::render_status_row`. **If a future agent extends the view to require a real navigator entity, this assumption breaks** — the new panel will need to provide a stand-in or the view will need a panel-agnostic accessor.
9. **`agent_id` is a `ChatProvider::new_tab` parameter.** Caller picks the AI backend. `console_panel::init` will need to know about `solution_agent::claude_adapter::CLAUDE_ACP_AGENT_ID` (or expose its own default-agent setting) in B8.
10. **Clean-start persistence.** No migration of legacy `terminals` / `pane_groups` rows or navigator state. Old tables get `DROP TABLE IF EXISTS` in B10's migration step. User accepted this trade-off during planning.
11. **Hard removal of obsolete actions + settings + keymap entries.** No deprecated aliases. User keymap entries pointing at `terminal_panel::Toggle` or `solution_agent::FocusNavigator` will log "unknown action" warnings and stop firing; user updates their keymap manually.

## Active gotchas

1. **Uncommitted modifications in `crates/solution_agent/src/store.rs` and `store/tests.rs`** from a separate, now-stopped agent (user said «остановил другого агента» mid-session). Adds `is_session_gone_error` helper + tests. **Do NOT include them in any ConsolePanel commit.** Use explicit `git add` paths, never `-A` / `-u`. The current cargo build is green WITH these files in the working tree, so they don't block, but they are not part of this arc — leave them for the user to discipline separately.
2. **`Panel` trait requires BOTH `persistent_name()` AND `panel_key()`.** B6 finding. Plan's pseudocode only listed `persistent_name`. Use the same string for both (e.g., `"ConsolePanel"`) following the `TerminalPanel` convention.
3. **`cx.new(...)` requires `use gpui::AppContext as _`** and `.size_full()` requires `use gpui::Styled as _`. B6 finding.
4. **`TerminalView::tab_content_text(detail: usize, cx)`** is the `workspace::Item` trait method; you must `use workspace::Item` to bring it into scope.
5. **`debug_selector` closures allocate a String per render** for each annotated dock container. Same cost as every other call site in the codebase; not a regression to fix.
6. **Workspace tests have 7 pre-existing failures** unrelated to this arc (pane / multibuffer save-prompt territory). `cargo test -p workspace` reports `209 passed / 7 failed` after Phase A. The +1 vs baseline 208 is the new layout snapshot test.
7. **Cargo.lock update is its own commit.** When you add dev-deps to `crates/console_panel/Cargo.toml`, `cargo build` rewrites `Cargo.lock` — commit it as a one-line follow-up (`Update Cargo.lock for console_panel ...`). Don't try to fold it into the scaffold commit if the lock-changes weren't staged simultaneously.
8. **Screenshots: native `--headless` only.** `script/run-mcp --debug --headless` uses the WgpuHeadlessRenderer. **Do not** try `xvfb-run --display` — NVIDIA Vulkan under Xvfb produces blank PNGs (see `docs/findings/2026-05-headless-screenshot-blank.md` and ADR-0002).
9. **GPUI test bootstrap is heavy for any test that needs `SolutionSessionView::new`** — the constructor builds an `editor::Editor` which transitively needs the language + theme + font stacks. Six `console_panel` unit tests are `#[ignore]`'d for this reason (`#[ignore = "requires editor stack…"]`). Don't take the absence of runtime coverage as a quality signal — it's structurally bounded. B14's MCP e2e covers the real path.
10. **`Project::create_terminal_shell(cwd, cx)`** is the only public terminal-spawn entrypoint that `TerminalProvider` could use (the existing `TerminalPanel` has internal helpers that aren't `pub`). If a Spawn-Task path in B11 needs an env-aware spawn (`TerminalPanel::add_terminal_task`), check whether that helper is pub or if we need to expose a thin convenience in `terminal_view`'s public surface.
11. **Pre-existing `recent_projects` unreachable-pattern warnings** are not new — A1/A2/A3/A4 didn't introduce them.

## Open questions — resolved before pause

- **OQ1 — Terminal restore.** Re-spawn fresh shell at stored CWD on restart. Schema has a `cwd` column.
- **OQ2 — Console SVG.** Concrete glyph shipped in B2 (not a placeholder).
- **OQ3 — Action triage.** Delete only `FocusNavigator`. Keep `NewSession` / `CycleSession` / `FocusActiveSession` / `DuplicateSession` / `CloseSession` and re-route their handlers to `ConsolePanel`.

## Resume recipe for the next session

1. Read this file first.
2. Read `docs/superpowers/specs/2026-05-25-console-panel-design.md` for the design intent.
3. Read `docs/superpowers/plans/2026-05-25-console-panel.md` § Phase B (start at task B8).
4. `git log --oneline -15` — confirm the 13-commit chain matches the table above.
5. `git status` — confirm only the `crates/solution_agent/src/store.rs` + `store/tests.rs` uncommitted items (NOT from this arc).
6. Pick up at **B8 — `+` popover**. Plan steps are inline; verify `ChatProvider::new_tab`'s `agent_id` parameter resolution (likely `solution_agent::claude_adapter::CLAUDE_ACP_AGENT_ID` — confirm by reading where `Navigator` currently gets its default agent).
7. After B8 / B9 (popover + context menus), B10 persistence is the next major checkpoint — table + save/load round-trip + tests.
8. B11 is the most invasive: deletes two files, fixes every external call site referencing the removed types. Expect a build cascade; work through compile errors mechanically.

Use the subagent-driven-development skill, same dispatch pattern as this session (sonnet for nontrivial implementation, haiku for combined spec+quality reviews on mechanical tasks).
