# Solution Band Utility Section (phase 2b) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the band's utility half hosts **three** alternative contents — terminal, git graph, debug — chosen from a button group in the status bar, with "click the active button to hide the section". The vertical dock button strips are deleted and the project-zone panel toggles relocate to the project toolbar. A `ctrl-shift-a` hotkey toggles the dialog half.

**Spec:** `docs/plans/2026-08-26-solution-band-ai-dialogs-design.md` §3–§4. Phase 2a (`docs/plans/2026-08-26-solution-band-layout.md`) built the band with one utility occupant; the band's own height landed in `docs/plans/2026-08-27-solution-band-height.md`.

**Tech Stack:** Rust, GPUI. Crates touched: `workspace`, `solution_agent`, `console_panel`, `git_graph`, `debugger_ui`, `title_bar`, `zed`, and the keymap assets.

**Architecture:** `Workspace`'s single type-erased `solution_band_utility_item: Option<AnyView>` becomes a **keyed** map `HashMap<UtilityKind, AnyView>` with `UtilityKind` defined in `workspace` (which cannot depend on the occupant crates, and must not — see the cycle note below). `BandState` gains `utility_kind`, persisted per Solution beside `utility_visible`. `SolutionBand::render` looks the selected kind up in the map. A new status-bar item renders the three buttons and owns the show/hide/switch rule. The dock strips and their `PanelButtons` go away once the project-zone toggles have a new home in `ProjectToolbar`.

## Facts established by recon (authoritative — do not re-derive)

1. **The `AnyView` slot exists because of a real crate cycle.** `console_panel` depends on `solution_agent` (`crates/console_panel/Cargo.toml:29,45`); `solution_agent` does **not** depend back. So neither `Workspace` nor `SolutionBand` can hold a typed handle to an occupant. Moving the map into `SolutionBand` would need the same type erasure one crate over and buys nothing — the map stays on `Workspace`.
2. **The slot is written exactly once, at startup**, from `crates/zed/src/zed.rs:820-844` (`add_console_panel_when_ready`, joined at `:849`). Every other caller is a test bootstrap: `console_panel/src/panel.rs:1783`, `:2186`, `run_config_ui/src/run_controller.rs:1178`, `debugger_ui/src/tests.rs:97`.
3. **`ConsolePanel` is already fully de-docked** — no `Panel` impl, no zoom, no position logic. Its root (`crates/console_panel/src/panel.rs:543-584`) carries `v_flex().size_full()`, `.bg(cx.theme().colors().panel_background)` (added by `e3eb066ce7` because neither `half()` nor `Workspace::render` paints one), `.key_context("ConsolePanel")` and `.track_focus(&self.focus_handle)`.
4. **`GitGraphPanel` is nearly de-docked already** (`crates/git_graph/src/git_graph_panel.rs`): `position()` (`:188`) hardcodes `DockPosition::Bottom`, `set_position()` (`:196`) is a documented no-op, no zoom override, and `Render` (`:174`) never branches on position.
5. **`DebugPanel` is the coupled one** (`crates/debugger_ui/src/debugger_panel.rs`): `position()` (`:1547`) reads `DebuggerSettings::get_global(cx).dock`; `set_position()` (`:1555-1573`) calls `state.invert_axies(cx)` (`running.rs:1967`) when the axis changes and writes the setting; `Render` branches on `position(...) == DockPosition::Bottom` at `:1811`; it implements real zoom at `:1617-1624` and has a `hide_button_setting` status button at `:1609`.
6. **The strips:** `render_left_dock_strip` (`crates/workspace/src/workspace.rs:8253`) and `render_right_dock_strip` (`:8271`), called at `:9122` / `:9165`, backed by `left/right/bottom_dock_strip: Entity<PanelButtons>` (`:1392-1394`, built `:1803-1805` via `PanelButtons::new_vertical`). **The bottom strip is nested inside the left strip's div** (`:8265`). Buttons come from `PanelButtons::render` (`crates/workspace/src/dock.rs:1260-1318`) iterating `dock.panel_entries`. Loaded dock panels today: ProjectPanel, OutlinePanel, GitPanel (project zone) + GitGraphPanel, DebugPanel (utility). `terminal_view::TerminalPanel` still implements `Panel` but is **not loaded** in this fork.
7. **Status-bar registration:** `StatusBar::add_left_item` / `add_right_item`, a generic registry. Fork-local example to copy: `crates/run_config_ui/src/toolbar_strip.rs:52-61`; the session tab strip is registered the same way at `crates/zed/src/zed.rs:645-661`.
8. **`ProjectToolbar` is NOT a registry** (`crates/title_bar/src/project_toolbar.rs`): `Render` (`:320-388`) composes fixed children, and pulls one type-erased slot via `workspace.read(cx).run_config_strip()` (`:340`, set through `Workspace::set_run_config_strip`, `workspace.rs:3180`). A new button group goes in through the same slot idiom or as a direct child.
9. **The four defensive "reveal the utility section" call sites** all go through `console_panel::panel::reveal_utility_section` (`panel.rs:69-77`): `panel.rs:999`, `:1005`, `:1175`, `:1186` (`add_terminal_task` / `replace_terminal`, driven by run-configuration output). Each must now name the kind it wants.
10. **Notification click-through never touches the utility section** — `crates/zed/src/notification_focus.rs:79-131` only sets the active dialog session.
11. **`ctrl-shift-a` is already bound** in contexts the hotkey needs: `"Terminal"` → `editor::SelectAll` (`assets/keymaps/default-linux.json:1277`, `default-windows.json:1264`) and, on macOS, `"Editor"` → `editor::SelectToBeginningOfLine` (`default-macos.json:141`). A `"Workspace"`-context binding is shadowed by both.
12. **Nothing remembers the last-selected session.** `session_tab_strip::toggle_selection` (`crates/solution_agent/src/session_tab_strip.rs:91-100`) returns `None` on re-click and that `None` **is** the collapsed state; `tab_order` is a manual display order, never touched by a click; `Solution::last_opened_at` is per-Solution, not per-session. A dialog-toggle hotkey has nothing to re-open on without new state.
13. **Adding one `BandState` field costs 12 sites**: model default; store setter; `BandStateTouched` bit; `overlay`; the now-vs-debounced persist choice; the hydration drain (`store.rs:823-876`); `upsert_band_state`; `select_band_states`; `CREATE TABLE`; `apply_idempotent_add_column_to`; the view-local `local_state` branch; and the two `db/tests.rs` migration tests (`:1330`, `:1362`) to clone.

## Rulings made up front (binding; a reviewer judges against these)

- **The keyed map lives on `Workspace`, and `UtilityKind` is defined in `workspace`.** `workspace` must not gain a dependency on `console_panel` / `git_graph` / `debugger_ui`. *Cost if wrong:* an enum in a crate that does not own any of its variants' implementations — which is exactly what the existing `AnyView` slot already accepts.
- **The old single-slot API is replaced, not kept alongside.** One production writer, four test writers; a deprecated parallel path would rot.
- **`DebugPanel` gets the `GitGraphPanel` treatment: `position()` hardcodes `Bottom` and `set_position()` becomes a no-op.** The bottom dock is not rendered in this fork, so `DebuggerSettings.dock` steers nothing a user can see, while `position()` still drives `Render`'s layout branch and `invert_axies`. Hardcoding it makes the band-hosted orientation deterministic instead of settings-dependent. *Cost if wrong:* a user who had set `debugger.dock` to `left`/`right` loses an axis preference that no longer has a surface to apply to; recovering it means re-plumbing a real position input. Leave the setting itself in place (removing it is a settings migration this plan does not want).
- **The opaque background moves onto the band's `half()`** so all three occupants inherit it, and `ConsolePanel`'s own `.bg(...)` line is removed in the same commit. This changes the contract from "each occupant must be opaque" to "the band is opaque". *Cost if wrong:* an occupant that deliberately wants to show through can no longer do so without a change to `half()`.
- **`ctrl-shift-a` is bound in the `"Workspace"` context only. The `"Terminal"` and macOS `"Editor"` bindings are NOT overridden.** On Linux (this fork's primary platform) that leaves the hotkey working everywhere except while the terminal has focus, where `editor::SelectAll` is the older, more useful behaviour. On macOS it additionally does not fire while an editor — including the dialog's compose box — has focus. Document the macOS limitation rather than breaking `SelectToBeginningOfLine` for every editor. *Cost if wrong:* a macOS user must click out of the compose box to use the hotkey. The spec itself marks this binding "subject to maintainer's veto at implementation time".
- **The dialog toggle's re-open target is in-memory, not a new persisted column.** The store keeps `last_dialog_session: HashMap<SolutionId, SolutionSessionId>`, written whenever a non-`None` active dialog is set; the hotkey re-opens on that, or, failing it, on the first session in `tab_order`. *Cost if wrong:* after a restart with a collapsed band, the first `ctrl-shift-a` opens the first tab rather than the one from the previous run. That is one column and one migration cheaper than persisting it, and the persisted `active_dialog_session` already restores the non-collapsed case correctly.
- **Task order is load-bearing: the project-zone toggles get their new home BEFORE the strips are deleted.** Deleting first would ship a commit in which the project panel cannot be toggled at all.

## Global Constraints

- **GPUI double-lease:** reading the `Workspace` entity under a live `&mut Workspace` borrow panics at runtime, compiles clean, and wrongly-shaped unit tests miss it. `SolutionBand` resolves its Solution off `Entity<Project>` for exactly this reason (module doc, `crates/solution_agent/src/solution_band.rs`). Anything reachable from a `register_action` handler or from inside `workspace.update_in` must not read the Workspace entity.
- **A `cx.notify()` raised during a draw is discarded, not deferred** (`Window::invalidate_view` returns false when `draw_phase != None`). Never derive-and-persist from `render`.
- **`on_drop` never fires under a `deferred()` + `block_mouse_except_scroll()` drag handle** (FORK.md #84).
- **The band's three-part layout invariant must survive** (FORK.md, added 2026-08-27): `min_h_0()` on the workspace column (`workspace.rs:9028`), `flex_none()` on the status bar (`status_bar.rs:117`), and a shrinkable band. Removing any one of them pushes the status bar off-screen on a short window. If a task touches `Workspace::render`'s column or the status bar's root, it must not disturb these.
- **Debug builds only** for agent verification: `cargo build`, `cargo test`, never `--release`. `./script/clippy` runs scoped, at the final task: `./script/clippy -p workspace -p solution_agent -p console_panel -p git_graph -p debugger_ui -p title_bar -p solutions_ui -p editor_mcp`. The **unscoped** run is RED on pre-existing debt in `denoise` and `git_ui` that this plan must not touch.
- **Never pipe cargo output through `tail` without `set -o pipefail`.**
- **Harness `<new-diagnostics>` blocks are frequently stale mid-edit snapshots.** Confirm with `set -o pipefail; cargo check -p <crate> --all-targets`.
- **`mcp__sawe__*` tools drive the maintainer's LIVE running editor.** Never verify with them. `cargo build --bin sawe` first (`script/run-mcp` only compiles a *missing* binary), then `script/run-mcp --debug --headless`, and drive that socket. `windows.resize` is available in debug builds for short-window checks.
- **Any user-visible UI change requires a screenshot** before it counts as done. `workspace.screenshot` renders the retained scene and does not run a draw — drive a real event first.
- Commit messages: imperative, crate-prefixed, **no `Co-Authored-By`**, never `git commit --amend`. **Implementers do not push.**
- Rust style: no `unwrap()` outside tests; comments explain *why*; no organizational comments; never `let _ =` on a fallible call.

---

### Task 1: Key the utility slot by content kind

**Files:** `crates/workspace/src/workspace.rs`, `crates/solution_agent/src/solution_band.rs`, `crates/zed/src/zed.rs`, plus the four test bootstraps in fact 2.

**Interfaces produced:**
```rust
// crates/workspace/src/workspace.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UtilityKind { Terminal, GitGraph, Debug }
impl UtilityKind {
    pub fn as_str(self) -> &'static str;          // "terminal" | "git_graph" | "debug"
    pub fn from_str(s: &str) -> Option<Self>;     // for the persisted column
}
Workspace::set_solution_band_utility_item(kind: UtilityKind, item: AnyView, window, cx)
Workspace::solution_band_utility_item(kind: UtilityKind) -> Option<AnyView>
```

- [x] Replace the `Option<AnyView>` field with `HashMap<UtilityKind, AnyView>`; keep the setter's existing `cx.notify()` behaviour.
- [x] `SolutionBand::utility_panel` takes the kind to show. For this task it always asks for `UtilityKind::Terminal`, so behaviour is unchanged end to end — the switch arrives in Task 2.
- [x] Update `zed.rs`'s `add_console_panel_when_ready` to pass `UtilityKind::Terminal`, and the four test bootstraps likewise.
- [x] Tests: the workspace round-trip test for the slot (`workspace.rs:11375-11402` is the phase-2a precedent) becomes per-kind — two kinds set, each reads back independently, an unset kind reads `None`.
- [x] Gate: `set -o pipefail; cargo check -p workspace -p solution_agent -p console_panel -p zed --all-targets`; `cargo test -p workspace -p solution_agent`.

### Task 2: Persist which content the utility section shows

**Files:** `crates/solution_agent/src/{model.rs,store.rs,db.rs,db/band.rs,db/tests.rs,solution_band.rs}`

Add `utility_kind: UtilityKind` to `BandState`, defaulting to `Terminal`. Follow the 12 sites in fact 13 exactly — the `height` field's own commits (`8c00d8845c`) are the template, and the persist choice is **immediate** (`persist_band_state_now`), like `utility_visible`, not debounced: switching content is a discrete click, not a drag.

- [x] Column: `utility_kind TEXT NOT NULL DEFAULT 'terminal'`, added to `CREATE TABLE` **and** via `apply_idempotent_add_column_to` — the table already exists on every install. An unparseable value loads as `Terminal` with a `log::warn!`, mirroring how `active_dialog_session` degrades.
- [x] `SolutionAgentStore::set_band_utility_kind(solution_id, kind, cx)` + the `BandStateTouched` bit + the `overlay` arm.
- [x] `SolutionBand::render` shows `band_state(cx).utility_kind`, and the view-local `local_state` branch handles it for non-Solution windows.
- [x] Tests: clone both `db/tests.rs` migration tests for the new column (fresh-table default; pre-migration table backfills to `'terminal'` with the other columns intact); a store round-trip; a pre-hydration touched-mask test.
- [x] Gate: `cargo test -p solution_agent`.

### Task 3: Paint the band's half, not the occupant

**Files:** `crates/solution_agent/src/solution_band.rs`, `crates/console_panel/src/panel.rs`

- [x] Move `.bg(cx.theme().colors().panel_background)` from `ConsolePanel`'s root onto `SolutionBand::half()`, so every occupant inherits it (ruling above). Keep `ConsolePanel`'s `size_full`, `key_context` and `track_focus` — those are per-occupant identity, not band layout.
- [x] Verify by screenshot that the terminal half looks unchanged, and that a band whose occupant is missing (a kind with no registered view) paints an opaque half rather than a transparent slab.
- [x] Gate: `cargo test -p solution_agent -p console_panel` + a screenshot.

### Task 4: Host the git graph in the utility section

**Files:** `crates/git_graph/src/git_graph_panel.rs`, `crates/zed/src/zed.rs`

- [x] Drop `GitGraphPanel`'s `Panel` impl (its dock methods are already stubs — fact 4) and everything that exists only to satisfy it, keeping `Render` + `Focusable`. `ConsolePanel` is the worked example of a de-docked occupant.
- [x] Load it into the keyed slot under `UtilityKind::GitGraph` instead of `add_panel_when_ready` (`zed.rs:849`).
- [x] Anything that referenced it as a dock panel — its `toggle_action`, any `add_panel` call, the panel-button path — either moves to Task 6's button group or goes away. Name in the report what you deleted and what still references it.
- [x] It must be able to open as a pane item still if it could before (`zed.rs:791-793` claims it can) — verify that claim and say which way it went.
- [x] Gate: `cargo test -p git_graph -p workspace -p zed` + a screenshot of the graph inside the band.

### Task 5: Host the debugger in the utility section

**Files:** `crates/debugger_ui/src/debugger_panel.rs`, `crates/zed/src/zed.rs`, `crates/debugger_ui/src/tests.rs`

The coupled one — budget accordingly, and read fact 5 before starting.

- [x] Per the ruling: `position()` hardcodes `DockPosition::Bottom` and `set_position()` becomes a documented no-op. Keep the `Panel` impl itself if other machinery still needs it; say in the report which parts are now vestigial.
- [x] Ensure the persisted `dock_axis` on a running session agrees with the forced Bottom orientation on first render — `invert_axies` (`running.rs:1967`) is the existing mechanism; a session restored with a vertical axis must not paint sideways inside the band.
- [x] Load it into the keyed slot under `UtilityKind::Debug`.
- [x] Zoom (`:1617-1624`): decide and document what zoom means for a band occupant. If it cannot work there, make it a no-op explicitly rather than leaving a control that does nothing visible.
- [x] Gate: `cargo test -p debugger_ui -p workspace` + a screenshot of a debug session inside the band.

### Task 6: The utility button group in the status bar

**Files:** a new status-bar item (crate choice is the implementer's — justify it against the cycle in fact 1), `crates/zed/src/zed.rs`, `crates/console_panel/src/panel.rs`

- [x] Three buttons (Terminal / Git graph / Debug), registered with `StatusBar::add_left_item` following `run_config_ui/src/toolbar_strip.rs:52-61`. Icons: reuse each panel's existing `icon()` value where one exists so the affordance the user learned survives the strip's deletion.
- [x] Behaviour, per spec §3: clicking an inactive button switches the content **and** shows the section; clicking the **active** button hides the section. `utility_visible == false` renders all three unselected.
- [x] Re-point the four defensive reveal sites (fact 9) at `UtilityKind::Terminal` explicitly — `reveal_utility_section` grows a kind parameter. Run-configuration output must still land in the terminal even when the user last left the section on Debug.
- [x] `console_panel::ToggleFocus` (`ctrl-\``) keeps its tri-state but now also selects `Terminal` when it reveals — otherwise the hotkey shows the section with the debugger in it.
- [x] Tests: the show/switch/hide rule as a unit test over the store's state, including "hide leaves `utility_kind` untouched so re-showing returns to the same content".
- [x] Gate: `cargo test` over the touched crates + screenshots of all three contents and of the hidden state.

### Task 7: Relocate the project-zone toggles, then delete the strips

**Files:** `crates/title_bar/src/project_toolbar.rs`, `crates/workspace/src/workspace.rs`, `crates/workspace/src/dock.rs`

Order inside the task matters (ruling above): add the new home first, delete second, in two commits.

- [x] Add toggles for ProjectPanel, OutlinePanel and GitPanel to `ProjectToolbar` (fact 8 — it is not a registry; either extend `Render` directly or add an `AnyView` slot mirroring `run_config_strip`). Each button dispatches the same `toggle_action()` the strip button dispatched, so behaviour is identical.
- [x] Then delete `render_left_dock_strip` / `render_right_dock_strip`, their call sites (`workspace.rs:9122`, `:9165`), the three `Entity<PanelButtons>` fields and their construction, and whatever in `PanelButtons` is only reachable from `new_vertical`. Do not delete `PanelButtons` wholesale without checking for other callers.
- [x] Do not disturb the layout invariant (Global Constraints) while editing `Workspace::render`.
- [x] Tests: whatever exists for the strips must be updated rather than deleted silently — say in the report what coverage was lost.
- [x] Gate: `cargo test -p workspace -p title_bar` + before/after screenshots showing no vertical strip and working toggles.

### Task 8: `ctrl-shift-a` toggles the dialog

**Files:** `crates/solution_agent/src/{store.rs,session_tab_strip.rs}`, wherever the action is defined and registered, `assets/keymaps/default-{linux,macos,windows}.json`

- [x] `last_dialog_session: HashMap<SolutionId, SolutionSessionId>` on the store, written whenever a non-`None` active dialog is set (ruling above). Not persisted.
- [x] The action: collapsed → re-open on `last_dialog_session`, else the first session in `tab_order`, else do nothing (no sessions). Expanded → collapse (`set_active_dialog_session(None)`).
- [x] Bind in the `"Workspace"` context of all three default keymaps. Do **not** override `"Terminal"` or macOS `"Editor"` (ruling + fact 11); document the macOS limitation where the binding lives.
- [x] Follow `console_panel::ToggleFocus`'s full path (fact 7 in the other recon: `actions!` → `workspace.register_action` in `zed.rs`'s `observe_new` closure → handler resolving the band).
- [x] Tests: toggle from expanded, toggle from collapsed with a remembered session, toggle from collapsed with none remembered but tabs present, and toggle with no sessions at all.
- [x] Gate: `cargo test -p solution_agent` + a screenshot of the collapsed and re-expanded band.

### Task 9: Verify live, then document

- [x] Drive a real editor (`cargo build --bin sawe`, then `script/run-mcp --debug --headless`): switch between all three contents from the status bar, hide by re-clicking the active button, confirm run-configuration output still lands in the terminal from a non-terminal selection, confirm `ctrl-\`` and `ctrl-shift-a`, and confirm the selection survives a restart. Screenshot each state and read the PNGs back.
- [x] Check a short window with `windows.resize` — the status bar must stay visible with the new button group in it.
- [x] Gates: `cargo test` over every touched crate; `cargo fmt --all --check`; the scoped `./script/clippy` from Global Constraints.
- [x] Docs: `FORK.md` entries for the keyed slot, the de-docked occupants, the deleted strips and the hotkey; update the `.rules` MCP catalog only if a tool changed; mark this plan's checkboxes; add the `docs/INDEX.md` row.
