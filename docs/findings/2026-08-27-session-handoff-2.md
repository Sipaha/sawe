# Session handoff — 2026-08-27 (second session of the day)

Supersedes `findings/2026-08-27-session-handoff.md` for everything after
`c14a18328c`. That file remains the record for phase 2a and the Run-crash
fix; this one covers the band's own height (complete) and phase 2b
(in progress, 6 of 9 tasks).

Plans: `docs/plans/2026-08-27-solution-band-height.md` (done),
`docs/plans/2026-08-27-solution-band-utility-section.md` (tasks 7–9 open).
Spec: `docs/plans/2026-08-26-solution-band-ai-dialogs-design.md` §3–§4.

---

## Where the work stands

**The band now has a height of its own, and its utility half hosts three
contents.** Both were the top two items in the previous handoff's pool.

- **Height (plan complete, pushed).** Persisted per Solution
  (`solution_band_state.band_height`), dragged from the band's top edge,
  clamped at render against the live viewport. Verified live: 320px of real
  transcript, dragged to 550/670, floored at 140, capped, double-click
  reset, and 480px restored across a real process restart.
- **Phase 2b (tasks 1–6 of 9).** `Workspace`'s single type-erased utility
  slot became a `HashMap<UtilityKind, AnyView>`; which kind a Solution shows
  is persisted; the git graph and the debugger are both de-docked and hosted
  in the band; a three-button group in the status bar switches between them
  and hides the section.

**Tasks 1-6 are complete, reviewed and pushed.** Task 6's fix round
(`57e30ff613`) closed all seven of its review items and its scoped re-review
found no new breakage. Nothing is in flight; the working tree is clean.
Resume at **Task 7**.

---

## Commit chain since `c14a18328c`

**Band height** — `e24481962c` (plan), `8c00d8845c` (model/DB/store),
`30b3770ef5` (top-edge drag), `351b9f3535` (FORK.md #94 + docs).

**Final-review fix wave for the height work** — `243598719b`
(`BAND_RESERVED_HEIGHT`), `ca49178668` (migration test over the real
pre-migration schema), `2bb656d96a` (the `windows.resize` MCP tool),
`9c6eb1c8c0` (**the layout invariant**, below), `9676ca0527` (fmt),
`ff4704511b` (FORK.md), `e2396c2cab` (gate `windows.resize` debug-only).

**Phase 2b** — `eb6374f73c` (plan), `134a51bc05` (keyed slot),
`c180820740` (persist the kind), `850712d29d` (background onto the band's
`half()`), `009aefddd3` (git graph de-docked), `628ab25064` (debugger
de-docked), `8561f3b461` (its fix round), `d66a2fb9da` (the button group),
`57e30ff613` (its fix round).

---

## The three things a future session must not re-derive

### 1. The band's three-part layout invariant

Recorded in FORK.md; repeated here because removing any one of the three
silently pushes the status bar off the bottom of a short window:

- `min_h_0()` on the workspace column (`crates/workspace/src/workspace.rs`),
- `flex_none()` on the status bar (`crates/workspace/src/status_bar.rs`),
- the band itself shrinkable as the last-resort yielder.

The column's `overflow` is visible, so without `min_h_0` taffy floors it at
its own min-content (project-zone chrome ~120px **plus** the band's fixed
height); that floor is resolved in the grow pass, so the column overflows
downward and the last row — the status bar — leaves the screen. Measured
before the fix at 1280×384: band 307 / project 47 / **status bar 0 visible
pixels**. After: band 234 / project 59 / status bar a full 30px.

`effective_band_height`'s `BAND_RESERVED_HEIGHT = 150` is a *coarse* guard
(chrome ~92px + a project zone worth having) — it is **not** what keeps the
status bar on screen. The invariant is.

### 2. `windows.resize` is debug-only, deliberately

`gpui::Window::resize` fans out through `Workspace`'s bounds observer into
`save_window_bounds`, so an agent resizing a window to reproduce a
short-window bug would write that geometry into the real workspace DB. The
tool is `#[cfg(debug_assertions)]`-gated end to end — struct, registration,
and its `editor_mcp::is_global_tool` branch — mirroring
`solution_agent.seed_cold_session`. Width/height are clamped to 1..=8192
(a 20000px surface fails wgpu validation on a later screenshot instead of
bailing cleanly), and `resized` is a read-back comparison, not a constant.

It is what found the layout bug at all: at 1920×1080 the margin is 216px
against ~92px of chrome, so the bug was invisible by construction.

### 3. The orphan-purge backlog is still the maintainer's call

Unchanged from the previous handoff and **not touched this session**:
`gc_orphan_members` is gated on the session having a live `acp_thread`, and
~18 cold orphans in the maintainer's DB are logged at
`target: "solution_agent::gc"` on every solution open instead of being
hard-purged. Retiring one reversibly = close the chat. Do not clean them up
on the maintainer's behalf.

---

## Open pool, in priority order

1. **Finish phase 2b — tasks 7, 8, 9** of
   `docs/plans/2026-08-27-solution-band-utility-section.md`:
   - **Task 7** relocates the project-zone toggles (ProjectPanel,
     OutlinePanel, GitPanel) into `ProjectToolbar` and *then* deletes
     `render_left_dock_strip` / `render_right_dock_strip` and their
     `PanelButtons`. Order inside the task is load-bearing — deleting first
     ships a commit where the project panel cannot be toggled at all.
     `ProjectToolbar` is **not** a registry: it composes fixed children and
     pulls one `AnyView` slot (`run_config_strip`) — copy that idiom.
   - **Task 8** is the `ctrl-shift-a` dialog toggle. Two facts settled by
     recon: the binding **collides** (`"Terminal"` → `editor::SelectAll` on
     Linux/Windows; macOS `"Editor"` → `editor::SelectToBeginningOfLine`),
     and **nothing in the codebase remembers the last-selected session** —
     `tab_order` is a manual display order, `Solution::last_opened_at` is
     per-Solution. Rulings already made: bind in `"Workspace"` context only,
     do not override the more specific contexts, document the macOS
     limitation; and keep the re-open target in memory
     (`last_dialog_session` on the store, falling back to the first tab)
     rather than adding a persisted column.
   - **Task 9** is live verification + docs, and must also fix the
     pre-existing `cargo test -p zed test_action_namespaces` failure as its
     own commit (its expected list lacks `"find_in_path"`, added
     2026-08-19 by `6217672762`) — confirmed failing at `c14a18328c`, so it
     is not this work's regression, but it blocks that task's gate.
2. **`entries_persist_chain` is cancelled wholesale on every teardown** —
   unchanged from the previous handoff, and still the item whose naive fix
   is dangerous (letting the flush run is what arms
   `delete_entries_from(main_len)` on legacy row layouts). Settle the
   row-layout question first.
3. **Phase 3 — the git panel** (`Changes | Commit`, History removed). Spec
   §5 exists; no plan yet. Note phase 2b makes the graph viable in the
   compact utility section, which was its prerequisite.
4. **Smaller, tracked here so they are not lost:**
   - **The git graph now has no keybinding at all** — its dock toggle action
     died with its `Panel` impl in `009aefddd3`. The status-bar button is
     its only path. Deliberate for now.
   - `DebuggerSettings.dock` / `.button` are **inert** (the only reader of
     `dock` is disabled telemetry): their settings-UI controls were removed
     and the keys annotated, but the fields remain, so a user's existing
     value is silently ignored.
   - `crates/debugger_ui/src/session/running.rs::handle_run_in_terminal` is
     a second, independent embedded-terminal mechanism inside the debug
     session's own sub-pane. If the band shows Terminal when an adapter
     fires `runInTerminal`, that tab lands invisibly in the unshown Debug
     pane. Pre-existing; the right fix is `reveal_debug_panel`, ~3 lines,
     but it changes the debugger's start behaviour.
   - FORK.md's touched-files rows added for `debugger_ui` / `settings_ui`
     carry the wrong crate attribution (copy-paste from the row above).
   - `workspace::PaneGroup::invert_axies` is now dead but still `pub`.
   - The band's `local_state` fallback and everything else from the previous
     handoff's deferred list still stands.

---

## Rulings made this session (reversible; each with what it costs if wrong)

**Band height**

- **Absolute pixels, not a window fraction.** Matches every dock in the
  editor. *Cost:* moving between a laptop and an external monitor keeps the
  pixel height, not the proportion.
- **The viewport cap is computed at render, purely, and never written back.**
  GPUI discards a `cx.notify()` raised during a draw, so a
  derive-and-persist loop either no-ops or spins. *Cost:* none found; a
  temporarily-shrunk window cannot corrupt the saved geometry.
- **No top border added.** The project zone's own `border_b_1` already draws
  the boundary — verified by pixel-sampling at five x positions. *Cost:* if
  anyone reorders that column or drops that border, the band's only visual
  edge goes with it; `cursor_row_resize` is the backstop.
- **Build `windows.resize` rather than hand the verification gap to the
  operator**, then gate it debug-only rather than surfacing the
  bounds-persistence trap it introduced. *Cost:* a primitive that exists
  only in debug builds — which is where every agent-driven run happens.

**Phase 2b**

- **The keyed map lives on `Workspace` and `UtilityKind` is defined there**,
  because `workspace` must not depend on `console_panel` / `git_graph` /
  `debugger_ui`. Moving the map into `SolutionBand` needs the same type
  erasure one crate over and buys nothing.
- **The opaque background moved onto the band's `half()`**, so occupants no
  longer each carry one. *Cost:* an occupant that wants to show through
  can't without changing `half()`.
- **The debugger's whole `Panel` impl was deleted**, not just neutered — the
  implementer proved the bottom dock IS still rendered (it simply has no
  panels), so keeping the impl would have been worse than ceremony. Its
  `position()` is gone with it, which is what makes the band-hosted
  orientation deterministic. *Cost:* `debugger.dock` no longer steers
  anything (see the inert-settings item above).
- **Buttons drive `SolutionBand`, not `SolutionAgentStore`**, so a
  plain-folder window's `local_state` path works and the click handler never
  touches the `Workspace` entity.
- **A selected kind with no registered occupant shows an explicit
  placeholder; it never falls back to another kind** — a fallback would
  silently rewrite the user's persisted choice and desync the buttons. The
  placeholder is gated on the install task *resolving with an error*, per
  kind, so it cannot lie during async startup.
- **Buttons never move focus; the hotkeys remain the focus path.** They
  differ in exactly one cell — visible, this kind selected, occupant
  unfocused: the hotkey focuses, the button hides.
- **`IconName::Terminal`, not the old `IconName::Console`** — Console was
  the merged Terminal+Chat panel's icon and the chat half left in phase 2a.
- **The utility buttons sit ahead of the session tab strip** in the status
  bar's left items, which are `min_w_0 + overflow_x_hidden`: the tab strip
  has an overflow popover and can absorb a squeeze, the three-icon group
  can't, and it is the only way to reach the git graph. Verified at 420px.
- **The Critical from Task 5's review was fixed in Task 5, not deferred to
  Task 6** — after the debugger became the first non-Terminal writer,
  `ctrl-\`` would have reopened the band on the debugger with focus on an
  unrendered `ConsolePanel`, i.e. `main` would have shipped broken between
  the two tasks.

---

## Active gotchas (additions to the previous handoff's list)

- **The machine ran out of disk mid-session** (54 MB free; `target/` at
  686 GB), which silently broke a subagent's `cargo check`. Freed 407 GB by
  deleting `target/*/incremental`, `target/release` and `target/doc` — all
  rebuildable; `target/release-fast` (the maintainer's running binary) was
  left intact. `target/debug` is still ~217 GB. Watch for ENOSPC in agent
  reports: it looks like an unexplained compile failure.
- **A screenshot renders the retained scene and does not run a draw.** Two
  separate implementers this session captured the *previous* frame after
  dispatching an action, even though the persisted state had already
  flipped. Drive a real event, then re-capture.
- The harness's `<new-diagnostics>` blocks were stale mid-edit snapshots
  **again**, several times, including one claiming `debugger_panel.rs` did
  not compile while `cargo check` returned 0.
- `cargo test -p zed test_action_namespaces` fails on `main` and has since
  before this session. Do not attribute it to current work.

---

## Process note

Nine agents across the two plans came back having **disproved part of their
own brief**, and were right every time — including the two that mattered
most: `BAND_RESERVED_HEIGHT` alone did **not** fix the status bar (the
workspace column's automatic minimum size did), and `invert_axies` was
**not** the debugger's restore-axis mechanism. Keep giving implementers
explicit permission to return a documented negative result.
