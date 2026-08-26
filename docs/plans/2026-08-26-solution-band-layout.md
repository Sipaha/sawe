# Solution Band Layout (phase 2a) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give AI dialogs their own full-width region between the project zone and the status bar, with the terminal beside them behind a draggable divider, and move the session tab strip into the status bar.

**Architecture:** Add a `solution_band_item` slot to `Workspace` (copying the existing `project_toolbar_item` idiom), populate it with a new `SolutionBand` view that renders `dialog | divider | utility-section`. Chat tabs leave `ConsolePanel` entirely — their strip becomes a status-bar entity and their content is hosted by the band; `ConsolePanel` becomes terminal-only and is rendered *inside* the band's utility section rather than in the bottom dock. The bottom dock stops receiving panels.

**Tech Stack:** Rust, GPUI. Crates touched: `workspace`, `console_panel`, `solution_agent`, `solutions_ui`, `zed`.

**Spec:** `docs/plans/2026-08-26-solution-band-ai-dialogs-design.md` §2-§4. This plan is **phase 2a of 3**. Phase 2b (move GitGraph + Debug into the utility section, delete the vertical dock button strips, relocate their buttons into the project toolbar) and phase 3 (git panel `Changes | Commit`, History removed) are planned separately.

## Global Constraints

- **Debug builds only** for agent verification: `cargo build`, `cargo test`, never `--release`. `./script/clippy` is release by design — run it once, at the final task. It is currently GREEN (exit 0) across `solution_agent console_panel git solutions`; keep it that way.
- **Never pipe cargo output through `tail` without `set -o pipefail`** — the pipe reports `tail`'s status and a failed build looks like it succeeded.
- **Harness `<new-diagnostics>` blocks are frequently stale mid-edit snapshots.** Verify with `set -o pipefail; cargo check -p <crates> --all-targets` before believing them. If a build blocks on the target-dir lock, `pkill -f "cargo check --workspace"`.
- **GPUI double-lease trap.** Reading the `Workspace` entity from code that runs under a `Workspace` lease panics at runtime, compiles fine, and unit tests miss it. Any method reachable from a `workspace.register_action` handler, or from a panel's `new()` invoked inside `workspace.update_in`, must not call `self.workspace.upgrade()?.read(cx)`. This has now bitten this fork three times (`git_graph` panel ctor; `console_panel::add_chat_tab`, twice). When a value is needed there, pass it in from the caller that already holds `&mut Workspace`.
- **A `cx.notify()` raised during a draw is discarded, not deferred** (`Window::invalidate_view` returns false when `draw_phase != None`). Band/divider code that derives state during layout must hop out with `cx.defer` **and** guard on the value actually having changed, or it spins every frame.
- **Any user-visible UI change requires a screenshot** from `script/run-mcp --debug --headless` + `workspace.screenshot` before it counts as done. `workspace.screenshot` renders the *retained* scene — it does not run a draw, so drive a real event first (a click, keystroke, dock toggle, or a second `windows.hover_at` a pixel off the first).
- Commit messages: imperative, crate-prefixed, **no `Co-Authored-By`**, never `git commit --amend`. **Implementers do not push** — the controller pushes after review.
- Rust style: no `unwrap()` in non-test code; comments explain *why*, not *what*; no organizational/summary comments; never `let _ =` on a fallible call.

## Corrections to the spec, established by recon (authoritative over the spec text)

1. **The bottom dock is NOT full-width on `main`.** The spec's aside and `FORK.md:117` both claim upstream's `BottomDockLayout` was removed so the bottom dock spans the window. It was not: `BottomDockLayout` still exists in `crates/settings_content/src/workspace.rs` with default `Contained`, `assets/settings/default.json` has no override, and `Workspace::render` nests the bottom dock *inside* the centre column between the left/right docks. The two commits that did that work (`2dc7d200f0`, `8c456278b5`) are on unreachable refs, not in `main`'s history. **Task 8 fixes `FORK.md:117`.**
2. **Session-tab overflow should copy `ProjectTabStrip`, not `SolutionTabStrip`.** The spec says "same pattern as solution tabs", but `SolutionTabStrip` has no overflow popover — it uses `overflow_x_scroll`. The fixed-cap-plus-popover pattern the spec describes lives in `crates/solutions_ui/src/project_tab_strip.rs` (`MAX_VISIBLE_TABS`, overflow popover).
3. **`ConsolePanel::active_by_member` has two write sites, not one** — `Render::render` (~:761-776, a proactive per-frame refresh) as well as `on_active_member_changed` (~:1122-1147). Narrowing it to terminals means editing both.
4. **Emptying the bottom dock is safe.** `Dock::visible_entry()` returns `None`, `Dock::render` falls to an empty zero-size div, `Workspace::render_dock` skips its sizing block, and `capture_dock_state`/`set_dock_structure` persist `visible:false` harmlessly. No panel needs to be *deleted* from the codebase to remove it from the dock — just stop calling `workspace.add_panel` for it.

---

### Task 1: Add the `solution_band_item` slot to `Workspace`

**Files:**
- Modify: `crates/workspace/src/workspace.rs` (field near `:1404`, ctor near `:1874`, accessors near `:3131`/`:3306`, render site near `:8959`/`:9133`)
- Test: `crates/workspace/src/workspace.rs` test module

**Interfaces:**
- Produces: `Workspace::set_solution_band_item(&mut self, item: AnyView, _: &mut Window, cx: &mut Context<Self>)` and `Workspace::solution_band_item(&self) -> Option<AnyView>`, rendered as a full-width row **between the workspace-area div and the status bar**.

- [ ] **Step 1: Read the precedent**

Read `crates/workspace/src/workspace.rs` around the `project_toolbar_item` field (`~:1404`), its `set_project_toolbar_item` (`~:3131`), its getter (`~:3306`), and its render site (`~:8959`). Your new slot is the same shape, one row lower in the tree.

- [ ] **Step 2: Write the failing test**

In `crates/workspace/src/workspace.rs`'s test module:

```rust
#[gpui::test]
async fn solution_band_item_renders_between_the_workspace_area_and_the_status_bar(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));

    workspace.update_in(cx, |workspace, _window, _cx| {
        assert!(
            workspace.solution_band_item().is_none(),
            "a fresh workspace has no band"
        );
    });

    let band = cx.new(|_| BandProbe);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_solution_band_item(band.clone().into(), window, cx);
        assert!(workspace.solution_band_item().is_some());
    });
}

struct BandProbe;
impl Render for BandProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().id("band-probe").h(px(120.))
    }
}
```

Match the test module's existing scaffolding for `init_test` / `Workspace::test_new` — copy whichever helper the neighbouring tests use.

- [ ] **Step 3: Run it and watch it fail**

Run: `cargo test -p workspace solution_band_item_renders`
Expected: FAIL to compile — `set_solution_band_item` / `solution_band_item` do not exist.

- [ ] **Step 4: Add the field and accessors**

Field, next to `project_toolbar_item`:

```rust
/// The Solution band — a full-width region between the project zone and the
/// status bar, hosting the active AI dialog beside the utility section.
/// Populated by the crate that owns the band, the same way `title_bar`
/// populates `project_toolbar_item`: `workspace` cannot depend on it.
solution_band_item: Option<AnyView>,
```

Initialise to `None` in `Workspace::new`. Accessors mirroring the toolbar pair:

```rust
pub fn set_solution_band_item(
    &mut self,
    item: AnyView,
    _window: &mut Window,
    cx: &mut Context<Self>,
) {
    self.solution_band_item = Some(item);
    cx.notify();
}

pub fn solution_band_item(&self) -> Option<AnyView> {
    self.solution_band_item.clone()
}
```

- [ ] **Step 5: Render it**

In `Workspace::render`, the band is a sibling of the workspace-area div, immediately before the status bar — so it spans the full window width (unlike the bottom dock, which is nested inside the centre column between the side docks). Add `.children(self.solution_band_item.clone())` after the workspace-area child and before `.when(status_bar_visible, …)`.

- [ ] **Step 6: Run the test**

Run: `cargo test -p workspace solution_band_item_renders`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/workspace/src/workspace.rs
git commit -m "workspace: Add a full-width solution band slot below the project zone"
```

---

### Task 2: A solution-scoped "active dialog session" in the agent store

**Files:**
- Modify: `crates/solution_agent/src/store.rs` (new field + accessors + event), `crates/solution_agent/src/model.rs` if an event enum lives there
- Test: `crates/solution_agent/src/store/tests/misc.rs`

**Interfaces:**
- Produces: `SolutionAgentStore::active_dialog_session(&self, solution_id: SolutionId) -> Option<SolutionSessionId>`, `SolutionAgentStore::set_active_dialog_session(&mut self, solution_id: SolutionId, session_id: Option<SolutionSessionId>, cx: &mut Context<Self>)`, and a new event variant `ActiveDialogSessionChanged { solution_id }` on the store's existing event enum. `None` means "the dialog is collapsed".
- Consumed by: Task 3 (the tab strip sets it), Task 4 (the band reads it).

Rationale: the band and the status-bar strip are two separate views that must agree on which dialog is showing. Storing that in either view forces the other to reach across; the store already owns per-solution session state and already broadcasts changes.

- [ ] **Step 1: Write the failing test**

```rust
#[gpui::test]
async fn active_dialog_session_is_per_solution_and_defaults_to_none(cx: &mut TestAppContext) {
    let (store, solution_id, project) = /* the scaffolding used by
        create_session_roots_cwd_at_solution_root in this file */;
    store.read_with(cx, |s, _| {
        assert_eq!(s.active_dialog_session(solution_id), None);
    });

    let session_id = store
        .update(cx, |s, cx| s.create_session(solution_id, agent_id(), project, cx))
        .await
        .unwrap();
    store.update(cx, |s, cx| {
        s.set_active_dialog_session(solution_id, Some(session_id), cx)
    });
    store.read_with(cx, |s, _| {
        assert_eq!(s.active_dialog_session(solution_id), Some(session_id));
        assert_eq!(
            s.active_dialog_session(SolutionId(9999)),
            None,
            "another solution's dialog selection is independent"
        );
    });
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p solution_agent active_dialog_session_is_per_solution`
Expected: FAIL to compile — the accessors do not exist.

- [ ] **Step 3: Implement**

Add `active_dialog: HashMap<SolutionId, SolutionSessionId>` to `SolutionAgentStore`. `active_dialog_session` reads it; `set_active_dialog_session` inserts (or removes, for `None`), then `cx.emit(...)` the new event and `cx.notify()`. Follow the file's existing event-emission idiom exactly — find how `SessionCreated` is emitted and mirror it.

- [ ] **Step 4: Make session close clear the selection**

Find where `close_session` / `purge_session_hard` remove a session from the store. If the removed session is the solution's active dialog, clear the entry — otherwise the band renders a dangling id. Add an assertion for this to the test:

```rust
    store.update(cx, |s, cx| s.close_session(session_id, cx)).unwrap();
    store.read_with(cx, |s, _| {
        assert_eq!(
            s.active_dialog_session(solution_id),
            None,
            "closing the active dialog's session clears the selection"
        );
    });
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p solution_agent active_dialog_session`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/solution_agent
git commit -m "solution_agent: Track which session the Solution band's dialog shows"
```

---

### Task 3: Session tab strip in the status bar

**Files:**
- Create: `crates/solution_agent/src/session_tab_strip.rs`
- Modify: `crates/solution_agent/src/solution_agent.rs` (module decl + registration), `crates/zed/src/zed.rs` (status-bar registration near `:623-638`)
- Test: `crates/solution_agent/src/session_tab_strip.rs` test module

**Interfaces:**
- Consumes: `SolutionAgentStore::{active_dialog_session, set_active_dialog_session}` (Task 2).
- Produces: `SessionTabStrip` — an `Entity<T: Render>` added to the status bar's **left** group.

- [ ] **Step 1: Read the precedent**

Read `crates/solutions_ui/src/project_tab_strip.rs` in full — it is the closest analogue: a fixed visible cap with the remainder in a trailing overflow popover, plus a `+` button. **Do not copy `SolutionTabStrip`** — despite the spec's wording, that one scrolls rather than spilling into a popover. Also read `crates/workspace/src/status_bar.rs` (`StatusItemView`, `add_left_item`) to decide whether to implement `StatusItemView` (which gets active-pane-item notifications you do not need) or to register as a plain view. Prefer `StatusItemView` with a no-op `set_active_pane_item` if that is what `add_left_item` requires.

- [ ] **Step 2: Write the failing test**

```rust
#[gpui::test]
async fn clicking_a_session_tab_sets_the_active_dialog(cx: &mut TestAppContext) {
    // Build a store with two sessions in one solution, build the strip,
    // simulate activating the second tab, assert the store's
    // active_dialog_session is the second session's id.
}

#[test]
fn tabs_beyond_the_visible_cap_spill_into_the_overflow_list() {
    // Pure split of a session list into (visible, overflow) at MAX_VISIBLE_TABS.
    // Extract that split as a free function so it is testable without a view —
    // mirroring `project_tab_strip`'s approach.
}
```

The second test must exercise a **pure function**, not a rendered view: a `ConsoleTab::Chat`-style entity cannot be built in a unit test (it needs a live `SolutionSessionView` embedding a real `editor::Editor`), and this is exactly the gap that let a Critical through in phase 1. Extract the decision, test the decision.

- [ ] **Step 3: Run and watch it fail**

Run: `cargo test -p solution_agent session_tab`
Expected: FAIL — the module does not exist.

- [ ] **Step 4: Implement the strip**

Render one tab per non-ephemeral session of the active solution, ordered by the session's `tab_order`. Each tab shows the title, an agent-state dot (running / idle / errored — reuse whatever `status_row.rs` already uses so the two surfaces cannot diverge), and a close affordance. A trailing `+` creates a session (reuse the existing action rather than calling the store directly, so the keyboard path and this path cannot diverge — the phase-1 Critical was exactly two creation paths disagreeing). Clicking a tab calls `set_active_dialog_session(Some(id))`; clicking the **already-active** tab calls it with `None` (collapse — spec §3).

Subscribe to the store so the strip repaints on session create/close/title-change/state-change.

**Do not read the `Workspace` entity from anything reachable under a lease** (Global Constraints).

- [ ] **Step 5: Register it in the status bar**

In `crates/zed/src/zed.rs`, add the strip to the **left** group, before the existing left items.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p solution_agent session_tab`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/solution_agent crates/zed/src/zed.rs
git commit -m "solution_agent: Put the session tab strip in the status bar"
```

---

### Task 4: The `SolutionBand` view — dialog only

**Files:**
- Create: `crates/solution_agent/src/solution_band.rs`
- Modify: `crates/solution_agent/src/solution_agent.rs` (module decl), and the crate that installs it into the workspace (mirror how `title_bar` installs `project_toolbar_item` — see Task 1's precedent read)
- Test: `crates/solution_agent/src/solution_band.rs` test module

**Interfaces:**
- Consumes: `Workspace::set_solution_band_item` (Task 1), `SolutionAgentStore::active_dialog_session` (Task 2).
- Produces: `SolutionBand` — renders the active session's `SolutionSessionView` full-width. The utility section arrives in Task 6; the divider in Task 7.

- [ ] **Step 1: Implement the band**

Hold `workspace: WeakEntity<Workspace>` and a cache of `Entity<SolutionSessionView>` keyed by `SolutionSessionId` (constructing one per frame would rebuild a real editor every paint). On render: resolve the active solution, ask the store for `active_dialog_session`, look up or build the view, render it. When there is no active dialog, render nothing — the band contributes zero height and the project zone gets the space.

Subscribe to the store's `ActiveDialogSessionChanged` and notify.

- [ ] **Step 2: Install it**

Build the band and call `workspace.set_solution_band_item(band.into(), window, cx)` from the same place and in the same style the project toolbar is installed. **Beware:** that installation site holds `&mut Workspace`; the band's constructor must not read the `Workspace` entity (Global Constraints).

- [ ] **Step 3: Write a test for the height contract**

```rust
#[gpui::test]
async fn the_band_is_absent_when_no_dialog_is_active(cx: &mut TestAppContext) {
    // Build a workspace + band with active_dialog_session == None.
    // Assert the band's rendered element contributes no visible dialog —
    // assert on the band's own state (`active_view().is_none()`), not on pixels.
}
```

If the view cache makes a direct assertion awkward, expose a `#[cfg(test)] fn active_view(&self) -> Option<SolutionSessionId>` and assert on that.

- [ ] **Step 4: Build and test**

Run: `set -o pipefail; cargo build -p solution_agent && cargo test -p solution_agent solution_band`
Expected: PASS.

- [ ] **Step 5: Screenshot the band**

Build the binary first (`cargo build --bin sawe`), then `script/run-mcp --debug --headless`. Open a Solution, create a session, click its status-bar tab, and `workspace.screenshot {solution_id, format:"png"}`. Read the PNG with the Read tool and confirm the dialog renders in a full-width row above the status bar. Save it under `.superpowers/sdd/<this plan>/` and give the path in your report.

- [ ] **Step 6: Commit**

```bash
git add crates/solution_agent
git commit -m "solution_agent: Render the active dialog in the Solution band"
```

---

### Task 5: Chat tabs leave `ConsolePanel`

**Files:**
- Modify: `crates/console_panel/src/panel.rs` (delete `ConsoleTab::Chat`, narrow `ConsoleTabKey`, `tab_key`, `tab_scope`, `active_by_member` at **both** write sites, the `+` popover's chat entries, `add_chat_tab`/`add_chat_tab_with_cwd`, `new_chat_cwd`), `crates/console_panel/src/console_panel.rs` (the `NewChat` / `ShowSession` action handlers), `crates/console_panel/src/chat_provider.rs` (likely deleted)
- Modify: `crates/workspace/src/persistence.rs` (`console_panel_state` rows of kind `chat`)
- Test: `crates/console_panel/src/panel.rs` test module

**Interfaces:**
- Produces: `ConsolePanel` is terminal-only. `ConsoleTab` has a single variant. `console_panel::NewChat` and `ShowSession` now drive the band + strip (Tasks 2-4) instead of the panel.

- [ ] **Step 1: Re-point the actions first**

`NewChat` creates a session and calls `set_active_dialog_session(Some(id))`. `ShowSession` sets the active dialog to the named session (and must keep working from a notification click — FORK.md #36). Neither may touch `ConsolePanel` any more. Keep them registered on `Workspace` where they are; only the bodies change. **These handlers hold `&mut Workspace`** — pass what you need in; do not re-read the entity.

- [ ] **Step 2: Delete the chat tab kind**

Remove `ConsoleTab::Chat` and every match arm for it. `tab_scope` collapses to the terminal rule. `ConsoleTabKey` loses its `Chat` variant. `active_by_member` now only ever holds terminal keys — update the doc comments at both write sites to say so, since they currently describe a chat/terminal mix.

- [ ] **Step 3: Migrate persistence**

`console_panel_state` rows with `kind = 'chat'` are no longer restorable by the panel. On load, ignore them; add a one-shot cleanup that deletes them so they do not accumulate. The band's own active-session persistence is Task 7 — **do not** silently drop the user's open dialogs in the meantime: if Task 7 has not landed yet, restoring "no active dialog" on boot is acceptable and expected (the sessions themselves are untouched in `solution_sessions`; only which one is *showing* is forgotten).

- [ ] **Step 4: Update the tests**

Existing panel tests that build chat tabs or assert on chat scoping must be rewritten to terminals or deleted if their whole subject is gone. Keep `add_chat_tab_does_not_double_lease_the_workspace` alive in spirit: whatever replaces `add_chat_tab` must have an equivalent guard test, dispatched under a leased `Workspace` via `window_handle.update(...)`.

- [ ] **Step 5: Build and test**

Run: `set -o pipefail; cargo build -p console_panel -p solution_agent && cargo test -p console_panel -p solution_agent`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/console_panel crates/workspace crates/solution_agent
git commit -m "console_panel: Hand chat tabs over to the Solution band"
```

---

### Task 6: The utility section hosts the terminal

**Files:**
- Modify: `crates/solution_agent/src/solution_band.rs` (host a second region), `crates/console_panel/src/panel.rs` (drop the `Panel` impl's dock-facing surface if it is no longer docked), `crates/zed/src/zed.rs` (stop registering `ConsolePanel` into the bottom dock, `~:768-800`)
- Test: band + panel test modules

**Interfaces:**
- Produces: the band renders `dialog | utility-section`, the utility section rendering `ConsolePanel`'s terminal content. The bottom dock receives no `ConsolePanel`.

- [ ] **Step 1: Host the panel's content in the band**

The band gains an `Option<Entity<ConsolePanel>>`. Render it beside the dialog. `ConsolePanel` keeps its `Render`/`Focusable`; what it loses is the `Dock` chrome. Recon note: `ConsolePanel::position()` returns a real mutable `dock_position` field driven by settings and a right-click menu — once it is not docked, that field and its settings key are dead. Remove them, or leave them inert and say which in your report; do not leave a right-click "Dock Left/Right/Bottom" menu that no longer does anything.

- [ ] **Step 2: Stop docking it**

In `crates/zed/src/zed.rs::initialize_panels`, remove the `ConsolePanel` registration. Recon confirmed an empty bottom dock is harmless: `Dock::visible_entry()` returns `None`, `Dock::render` yields an empty zero-size div, and `capture_dock_state` persists `visible:false`.

- [ ] **Step 3: Re-point `ToggleFocus` and run output**

`console_panel::ToggleFocus` must now show/hide the band's utility section rather than `toggle_panel_focus::<ConsolePanel>` (which walks the docks and would find nothing). `ctrl-\`` keeps working. Run-configuration output (`run_config_ui::RunController` → `workspace.panel::<ConsolePanel>` → `spawn_task`) also resolves the panel through the dock — re-point it at the band's panel handle, or it silently stops delivering output. **Verify this path explicitly; it is easy to miss and fails quietly.**

- [ ] **Step 4: Build, test, screenshot**

Run the suites, then take a screenshot showing dialog and terminal side by side in the band, with no bottom dock. Read the PNG yourself.

- [ ] **Step 5: Commit**

```bash
git add crates/solution_agent crates/console_panel crates/zed
git commit -m "console_panel: Move the terminal into the Solution band's utility section"
```

---

### Task 7: Divider, collapse, and persistence

**Files:**
- Modify: `crates/solution_agent/src/solution_band.rs`
- Modify: persistence — a per-solution store for `{ divider_ratio, dialog_collapsed, utility_hidden, active_dialog_session }`
- Test: band test module

**Interfaces:**
- Produces: a drag-resizable divider whose ratio, the two collapse flags, and the active dialog survive a restart, per solution.

- [ ] **Step 1: Read the divider precedent**

Read `crates/editor/src/split_editor_view.rs` — `SplitEditorState` is the right template: a `left_ratio: f32` clamped to `[0.1, 0.9]`, driven by `on_drag` + **`on_drag_move`** on a `deferred()` strip with `.block_mouse_except_scroll()`, plus double-click-to-reset. **Critical trap, documented in FORK.md #84:** `on_drop` never fires under `deferred()` + `block_mouse_except_scroll()`, which is why that code sets the ratio continuously in `on_drag_move` instead. Do not "fix" it back to `on_drop`.

Its persistence is a process-wide `Global` (`LastSplitRatio`); the band needs per-solution persistence instead, so keep the drag mechanics and replace the backing store.

- [ ] **Step 2: Write the failing test**

```rust
#[test]
fn divider_ratio_is_clamped_to_a_usable_range() {
    assert_eq!(clamp_divider_ratio(-1.0), MIN_DIVIDER_RATIO);
    assert_eq!(clamp_divider_ratio(2.0), MAX_DIVIDER_RATIO);
    assert_eq!(clamp_divider_ratio(0.5), 0.5);
}

#[gpui::test]
async fn band_geometry_round_trips_per_solution(cx: &mut TestAppContext) {
    // Persist {ratio: 0.7, dialog_collapsed: false, utility_hidden: true}
    // for solution A and different values for solution B; reload; assert each
    // solution gets its own values back and neither leaks into the other.
}
```

- [ ] **Step 3: Implement the divider and the collapse rules**

Per spec §3: clicking the **active** session tab collapses the dialog (utility takes the full width); clicking the **active** utility button hides the section (dialog takes the full width); both hidden means the band contributes no height. Guard any layout-derived state update behind `cx.defer` **and** a changed-value check (Global Constraints) or the band will notify every frame.

- [ ] **Step 4: Build and test**

Run the suites.

- [ ] **Step 5: Screenshot all four states**

Both visible / dialog collapsed / utility hidden / both hidden. Drive a real event between shots. Read all four PNGs and describe each in your report.

- [ ] **Step 6: Commit**

```bash
git add crates/solution_agent crates/workspace
git commit -m "solution_agent: Make the band divider draggable and persist its geometry"
```

---

### Task 8: Status-bar cleanup, verification sweep, and docs

**Files:**
- Modify: `crates/zed/src/zed.rs` (drop two status-bar registrations), `crates/solutions_ui/src/status_bar.rs` and `crates/solution_agent/src/status_item.rs` (the two indicators)
- Modify: `FORK.md` (stale `:117` claim + new decision entry), `docs/INDEX.md`

- [ ] **Step 1: Remove the two indicators**

Per spec §2.5, drop `solutions_status` ("`<Solution>` · N projects") and the `SolutionAgentStatusItem` ("AI: N") from the status bar. If the item types then have no other consumer, delete them; if something else uses them, leave the type and only drop the registration — say which in your report.

- [ ] **Step 2: Full sweep**

```bash
set -o pipefail
cargo build --bin sawe
cargo test -p workspace -p solution_agent -p console_panel -p solutions_ui -p editor_mcp
./script/clippy -p workspace -p solution_agent -p console_panel -p solutions_ui -p git -p solutions
```

All must pass. Clippy was GREEN before this plan — a new failure is yours.

- [ ] **Step 3: Live verification**

`script/run-mcp --debug --headless`. Prove, with screenshots you read yourself:
1. The sandwich renders: title bar / project toolbar / project zone / band / status bar with session tabs on the left.
2. Switching the active member project changes the terminal content but leaves the dialog and the session tab strip untouched (the phase-1 invariant, now with the band).
3. `ctrl-\`` still toggles the terminal; run-configuration output still lands in it.
4. The two removed status-bar indicators are gone.

- [ ] **Step 4: Fix the stale FORK.md claim and record the decision**

`FORK.md:117` asserts the bottom dock spans the full window width via a removed `BottomDockLayout` — recon proved that is false on `main` (the enum still exists with default `Contained`; the commits that changed it are on unreachable refs). Correct it.

Add a numbered decision entry for the band: **why** (the dialog is the fork's primary surface and was competing with the terminal for one dock slot; docks give one visible panel per position, so a dedicated full-width region is the only way to show both), and **how to apply** (the band is a `Workspace` slot like `project_toolbar_item`, not a dock; the bottom dock is deliberately left empty rather than deleted; new band content must not read the `Workspace` entity under a lease).

Also record the phase-2b carry-forward: GitGraph and Debug still live in the bottom dock and the vertical dock strips still render.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "FORK.md,docs: Record the Solution band layout decision"
```

---

## Self-review notes

- Spec §2 coverage: band region (T1, T4), status-bar session tabs (T3), status-bar cleanup (T8). §2's button relocation and dock-strip removal are **phase 2b**, deliberately not here.
- Spec §3 coverage: divider + collapse + persistence (T7), member-switch behaviour (T8 verification), hotkeys and run output (T6).
- Spec §4 coverage: chat tabs leave (T5), console becomes terminal-only (T5, T6), actions re-pointed (T5, T6), persistence migration (T5, T7). GitGraph/Debug re-hosting is **phase 2b**.
- Type consistency: `active_dialog_session`/`set_active_dialog_session` (T2) are used verbatim in T3, T4, T5.
- Known risk carried into execution: Task 6's run-output re-pointing fails *quietly* if missed — it is called out explicitly there and re-verified in T8.
