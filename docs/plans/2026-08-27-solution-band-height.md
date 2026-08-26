# Solution Band Height (phase 2b, task 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the Solution band a height of its own — persisted per Solution, draggable from the band's top edge, clamped against the window — so the dialog half shows a real conversation instead of a status row plus a compose box, and a long transcript scrolls inside the band instead of growing it against the project zone.

**Why now:** phase 2a shipped the band as a *content-driven* row. With an empty session it measures ~128px, of which the transcript region is ~30px — the primary surface of the whole redesign currently has no conversation view. This is the first thing a user notices, and it is a prerequisite for phase 2b's utility-section work (GitGraph / Debug need a band tall enough to be usable).

**Architecture:** `BandState` gains a fourth field, `height` (logical pixels), persisted in the same per-Solution `solution_band_state` row through the same debounced write path Task 7 built for `divider_ratio`. `SolutionBand::render` sets that height on its root and paints a horizontal resize handle on its top edge, mirroring `Dock`'s `RESIZE_HANDLE_SIZE` handle and the band's own vertical divider. The window-relative clamp is applied at render time as a **pure** function of (stored height, viewport height) — never written back — because a `cx.notify()` raised during a draw is silently discarded.

**Tech Stack:** Rust, GPUI. Crates touched: `solution_agent` only (model, db, store, view). No `workspace` / `console_panel` / `zed` changes are expected.

**Spec:** `docs/plans/2026-08-26-solution-band-ai-dialogs-design.md` §2–§4. The spec specifies only the *vertical* divider inside the band (built in phase 2a Task 7); it says nothing about a band height or a horizontal handle. This plan therefore extends the spec rather than implementing it, and the design decisions below are this plan's own — recorded in Global Constraints so a reviewer judges against them, not against a silent spec.

**Prior art to read before touching anything:**
- `crates/solution_agent/src/solution_band.rs` — the band view, its vertical divider, its `local_state` fallback, and the module doc explaining why it resolves its Solution off `Entity<Project>` and never off the Workspace entity.
- `crates/solution_agent/src/store.rs` `:350-400` (`band_state`, `band_state_writes`, `band_state_touched`, `BandStateTouched::overlay`) and `:1436-1600` (`band_state`, `set_band_utility_visible`, `set_band_divider_ratio`, `persist_band_state_now`, `persist_band_state_debounced`).
- `crates/solution_agent/src/db/band.rs` — `save_band_state` / `load_band_states`.
- `crates/workspace/src/dock.rs` `:1131-1190` — the dock's resize handle, the closest precedent for a horizontal drag edge.

## Global Constraints

These are binding. A reviewer judges the diff against them.

- **Height is stored in absolute logical pixels, not as a fraction of the window.** The docks persist absolute panel sizes and the band's content has an intrinsic pixel minimum (status row + compose box). *Cost if wrong:* a user who moves the window between a laptop screen and an external monitor sees the band keep its pixel height rather than its proportion — the same behaviour every dock in this editor already has.
- **The window-relative clamp is applied at render, as a pure function, and never written back to the store.** `Window::invalidate_view` drops a notify raised during `request_layout`/`prepaint`/`paint`, so a "re-derive from current bounds and persist" loop either does nothing or spins every frame (FORK.md; `docs/findings/2026-08-17-gpui-draw-phase-invalidation.md`). The stored height survives a temporary window shrink untouched.
- **The DB write reuses the existing debounce** (`persist_band_state_debounced`, `BAND_STATE_WRITE_DEBOUNCE` = 400ms, cancel-on-replace). A drag emits `on_drag_move` continuously; one SQLite round-trip per mouse move is not acceptable. Height and ratio share one row and one pending-write slot per Solution — that is deliberate; a drag of one is never concurrent with a drag of the other.
- **The pre-hydration touched-mask discipline applies to the new field exactly as it does to the other three.** `BandStateTouched` gains a `height` bit, `overlay` handles it, and `set_band_height` sets it when `!band_states_hydrated`. Skipping this re-opens the data-loss class `95a4d5523a` closed: a band mutation racing the DB open would discard that Solution's persisted geometry.
- **`on_drop` never fires under a drag handle wrapped in `deferred()` + `block_mouse_except_scroll()`** (FORK.md #84). Commit the height from `on_drag_move`, like the band's divider and the split-diff divider. Do not "improve" this to `on_drop`.
- **GPUI double-lease trap.** Anything reachable from a `workspace.register_action` handler or from inside a `workspace.update_in` must not read the Workspace entity. `SolutionBand::solution_id` walks `Entity<Project>` for exactly this reason; the new height setters must go through the same path and must not call `self.workspace.upgrade()?.read(cx)`.
- **A plain-folder window that resolves to no Solution keeps working.** Height falls back to `SolutionBand::local_state`, unpersisted, same as `divider_ratio` and `utility_visible` do today.
- **Debug builds only** for agent verification: `cargo build`, `cargo test`, never `--release`. `./script/clippy` runs once, at the final task, scoped: `./script/clippy -p solution_agent -p console_panel -p solutions_ui -p solutions -p workspace`. The **unscoped** `./script/clippy` is RED on pre-existing debt in `denoise` and `git_ui` that this plan must not touch.
- **Never pipe cargo output through `tail` without `set -o pipefail`** — the pipe reports `tail`'s status and a failed build reads as a success.
- **Harness `<new-diagnostics>` blocks are frequently stale mid-edit snapshots** (wrong ten times in the previous session). Confirm with `set -o pipefail; cargo check -p solution_agent --all-targets` before believing one.
- **`mcp__sawe__*` tools drive the maintainer's LIVE running editor**, not a test instance. Never verify with them. Build (`cargo build --bin sawe`) and launch your own `script/run-mcp --debug --headless`, then drive its socket — `script/run-mcp` only compiles the binary when it is *missing*, so an un-built binary silently tests old code.
- **Any user-visible UI change requires a screenshot** from `script/run-mcp --debug --headless` + `workspace.screenshot` before it counts as done. That call renders the *retained* scene and does not run a draw — drive a real event first (a keystroke, a click, a second `windows.hover_at` a pixel off the first).
- Commit messages: imperative, crate-prefixed, **no `Co-Authored-By`**, never `git commit --amend`. **Implementers do not push** — the controller pushes after review.
- Rust style: no `unwrap()` in non-test code; comments explain *why*, not *what*; no organizational/summary comments; never `let _ =` on a fallible call.

## Values fixed by this plan (use verbatim)

```rust
/// Shortest the Solution band may be dragged. Below this the compose box
/// and the status row stop fitting together, which is the state phase 2a
/// shipped by accident.
pub const MIN_BAND_HEIGHT: f32 = 140.0;
/// What a band with no persisted row opens at, and what double-clicking
/// the band's top edge restores.
pub const DEFAULT_BAND_HEIGHT: f32 = 320.0;
/// Hard ceiling applied to a stored value, independent of any window. Only
/// guards a corrupt or hand-edited row; the real ceiling is the viewport
/// fraction below, applied at render.
pub const MAX_BAND_HEIGHT: f32 = 4096.0;
/// Most of the window the band may occupy, leaving the project zone
/// something to be. Applied at render against the live viewport.
pub const MAX_BAND_HEIGHT_FRACTION: f32 = 0.8;
/// Half-height of the top-edge grab area, in logical pixels either side of
/// the band's top edge. Matches the divider's `DIVIDER_HIT_SLOP` and the
/// dock's 6px `RESIZE_HANDLE_SIZE`.
const BAND_EDGE_HIT_SLOP: f32 = 3.0;
```

DB column: `band_height REAL NOT NULL DEFAULT 320` on `solution_band_state`.

---

### Task 1: Persist a band height

**Files:**
- Modify: `crates/solution_agent/src/model.rs` (constants + `clamp_band_height` + `effective_band_height` + `BandState`, near the existing divider-ratio block at `~:1176-1230`)
- Modify: `crates/solution_agent/src/db.rs` (schema: the `solution_band_state` `CREATE TABLE` at `~:342` plus an idempotent `ADD COLUMN` for existing installs)
- Modify: `crates/solution_agent/src/db/band.rs` (`upsert_band_state`, `select_band_states`)
- Modify: `crates/solution_agent/src/store.rs` (`BandStateTouched`, `overlay`, new `set_band_height`)
- Test: `crates/solution_agent/src/model.rs` test module (or the existing band tests in `solution_band.rs`, whichever the surrounding code uses) and `crates/solution_agent/src/solution_band.rs`'s `mod tests` for the store/DB round-trip

**Interfaces:**
- Produces: `BandState.height: f32`; `model::{MIN_BAND_HEIGHT, DEFAULT_BAND_HEIGHT, MAX_BAND_HEIGHT, MAX_BAND_HEIGHT_FRACTION, clamp_band_height, effective_band_height}`; `SolutionAgentStore::set_band_height(&mut self, solution_id: SolutionId, height: f32, cx: &mut Context<Self>)`.
- Consumed by: Task 2 (`SolutionBand`).

- [ ] **Step 1: Read the precedent.** `divider_ratio` end to end — `model.rs` (`clamp_divider_ratio`, `BandState`, `Default`), `db.rs` (the table), `db/band.rs` (upsert + select), `store.rs` (`set_band_divider_ratio`, `BandStateTouched`, `persist_band_state_debounced`). Your field is the same shape at every one of those sites. Deviating from that shape needs a reason in the report.

- [ ] **Step 2: `model.rs`.** Add the four constants above with their doc comments verbatim. Add:

```rust
/// Clamp a stored band height into the range a row may hold. Mirrors
/// `clamp_divider_ratio`, including the NaN fold: `f32::clamp` propagates
/// NaN, and a NaN height lays out as garbage.
pub fn clamp_band_height(height: f32) -> f32 {
    if height.is_nan() {
        return DEFAULT_BAND_HEIGHT;
    }
    height.clamp(MIN_BAND_HEIGHT, MAX_BAND_HEIGHT)
}

/// The height the band actually paints at: the stored height, capped so the
/// band can never eat the whole window. Deliberately a pure function of the
/// stored value and the live viewport — the cap is NOT written back to the
/// store, because a `cx.notify()` raised during a draw is discarded (see the
/// plan's Global Constraints) and because a temporarily-shrunk window must
/// not permanently shrink the user's saved geometry.
pub fn effective_band_height(stored: f32, viewport_height: f32) -> f32 {
    let ceiling = (viewport_height * MAX_BAND_HEIGHT_FRACTION).max(MIN_BAND_HEIGHT);
    clamp_band_height(stored).min(ceiling)
}
```

Add `height: f32` to `BandState` and `height: DEFAULT_BAND_HEIGHT` to its `Default`. Extend the `BandState` doc comment with one sentence on what the height is and that its window-relative cap lives at render.

- [ ] **Step 3: The schema.** Add `band_height REAL NOT NULL DEFAULT 320` to the `CREATE TABLE IF NOT EXISTS solution_band_state` statement, **and** call the existing `apply_idempotent_add_column_to(&connection, "solution_band_state", "band_height REAL NOT NULL DEFAULT 320")` right after it — the table already exists on every install that has run phase 2a, so `CREATE TABLE IF NOT EXISTS` alone would never add the column. Comment *why* both are needed. Follow whatever ordering the surrounding migration code uses for its other `apply_idempotent_add_column*` calls.

- [ ] **Step 4: `db/band.rs`.** Widen the bound tuple in `upsert_band_state` and the selected tuple in `select_band_states` to carry `band_height`, passing it through `clamp_band_height` on the way in *and* on the way out (`select_band_states` already clamps `divider_ratio` on read for the hand-edited-row case).

- [ ] **Step 5: `store.rs`.** Add `height: bool` to `BandStateTouched` and its arm to `overlay`. Add:

```rust
/// Set the Solution band's height. The in-memory value lands synchronously
/// so the dragged edge tracks the cursor on the next frame; the row is
/// written behind the same debounce the divider ratio uses (see
/// `band_state_writes`) — they share one pending-write slot per Solution
/// because a drag of one is never concurrent with a drag of the other.
pub fn set_band_height(&mut self, solution_id: SolutionId, height: f32, cx: &mut Context<Self>)
```

Body mirrors `set_band_divider_ratio` exactly: clamp via `clamp_band_height`, no-op check read through `band_state(solution_id)` (not `entry().or_default()` — see the comment there), write through `entry().or_default()`, set the touched bit when `!self.band_states_hydrated`, `persist_band_state_debounced`, emit `BandStateChanged`, `cx.notify()`.

- [ ] **Step 6: Tests.** Write these before or alongside the code; all must fail against the unmodified tree for the right reason.

1. `clamp_band_height` folds NaN to `DEFAULT_BAND_HEIGHT` and clamps both ends (mirror `divider_ratio_is_clamped_to_a_usable_range`).
2. `effective_band_height` caps against a small viewport (`effective_band_height(600.0, 400.0) == 320.0`), returns the stored value under a large one, and never returns less than `MIN_BAND_HEIGHT` even when the viewport is absurdly short (`effective_band_height(300.0, 10.0) == MIN_BAND_HEIGHT`).
3. Round-trip: `set_band_height` → advance the clock past the debounce → `load_band_states` returns the new height, and `divider_ratio` / `utility_visible` / `active_dialog_session` in the same row are unchanged. Use the existing `store_with_persistence` / `persisted` helpers in `solution_band.rs`'s test module.
4. Debounce: mirror `a_replaced_divider_drag_cancels_the_write_it_supersedes` for height — two `set_band_height` calls 300ms apart write nothing at the 600ms mark and the final value at the 800ms mark.
5. Pre-hydration touched-mask: with a persisted row holding a non-default height *and* a non-default ratio, call `set_band_height` before `set_persistence`'s load lands, then let it land — the height is the live value and the ratio is still the persisted one. (The existing band-hydration tests show the shape.)
6. A DB written by phase 2a — a row with no `band_height` column value, i.e. one inserted before the migration — loads as `DEFAULT_BAND_HEIGHT`. Construct it by inserting through a raw connection or by asserting the column default; state in the report which you did and why.

- [ ] **Step 7: Verify.** `set -o pipefail; cargo check -p solution_agent --all-targets` then `cargo test -p solution_agent`. Both must be clean. Commit as `solution_agent: Persist a per-Solution band height`.

---

### Task 2: Drag the band's top edge

**Files:**
- Modify: `crates/solution_agent/src/solution_band.rs` (module doc, `DraggedBandEdge`, `set_band_height` plumbing, `render`, `render_top_edge_handle`, tests)

**Interfaces:**
- Consumes: everything Task 1 produced.
- Produces: a band that paints at `effective_band_height(...)` and a top-edge drag handle.

- [ ] **Step 1: Read.** `SolutionBand::render`, `render_divider`, `on_divider_drag_move`, `set_divider_ratio` — your additions are their horizontal twins. Then `crates/workspace/src/dock.rs:1131-1190` for the dock's handle, which is what this should feel like to the user.

- [ ] **Step 2: The setter.** Add `fn set_band_height(&mut self, solution_id: Option<SolutionId>, height: f32, cx: &mut Context<Self>)` alongside `set_divider_ratio`, with the same `Some` → store / `None` → `local_state` split and the same `clamp_band_height` on the local branch.

- [ ] **Step 3: The drag.** Add `struct DraggedBandEdge;` beside `DraggedBandDivider` and a second `on_drag_move::<DraggedBandEdge>` listener on the band root:

```rust
fn on_edge_drag_move(
    &mut self,
    solution_id: Option<SolutionId>,
    event: &DragMoveEvent<DraggedBandEdge>,
    cx: &mut Context<Self>,
) {
    // `event.bounds` is the band root's hitbox from the last paint. Its
    // BOTTOM edge is what's anchored (the status bar sits directly under
    // it), so the dragged height is measured from there up to the cursor —
    // measuring from the top edge would chase the value being changed.
    let height = event.bounds.bottom() - event.event.position.y;
    self.set_band_height(solution_id, f32::from(height), cx);
}
```

Both `on_drag_move` listeners coexist on the same element: each is filtered by its own drag payload type in the capture phase, so only one fires per drag.

- [ ] **Step 4: The handle.** Add `render_top_edge_handle`, modelled on `render_divider`'s deferred grab area but horizontal:

- painted only when the band has content (i.e. not on the collapsed early-return path);
- the band root needs `.relative()` for the absolute handle to anchor to it;
- `deferred(div().id("solution-band-edge-handle").absolute().top(px(-BAND_EDGE_HIT_SLOP)).left_0().right_0().h(px(BAND_EDGE_HIT_SLOP * 2.)).cursor_row_resize().block_mouse_except_scroll().on_click(…).on_drag(DraggedBandEdge, …))`;
- double-click (`event.click_count() >= 2`) resets to `DEFAULT_BAND_HEIGHT` and `cx.stop_propagation()`, mirroring the divider's double-click reset;
- unlike the divider, this handle paints **no** visible line of its own — the band already has the workspace's row boundary above it. If verification shows the edge is invisible and unguessable, say so in the report rather than inventing a border here.

- [ ] **Step 5: The height.** In `render`, after the collapsed early-return, set the root's height to `effective_band_height(state.height, f32::from(window.viewport_size().height))` and mark it `flex_none()` so the workspace's column flex cannot shrink it. `window` is already a parameter of `render`. Do not clamp-and-store; do not `cx.notify()` from here.

- [ ] **Step 6: Tests.** In `solution_band.rs`'s `mod tests`:

1. The band's height for a Solution window comes from the store: set a height through the store, and `SolutionBand`'s view of it (add a `#[cfg(test)]` accessor mirroring the existing `active_view` helper) reports it.
2. A non-Solution window's height round-trips through `local_state` and writes **nothing** to the DB (mirror the existing plain-folder/non-Solution regression test for `utility_visible`).
3. A double-lease guard for the new setter, in the shape of the existing `ctrl-\`` guard test — the new path must be safe to call while a `&mut Workspace` borrow is live.
4. `effective_band_height` is what `render` would use: assert the clamped value directly rather than trying to measure a drawn element.

- [ ] **Step 7: Verify.** `set -o pipefail; cargo check -p solution_agent --all-targets`, `cargo test -p solution_agent`. Commit as `solution_agent: Let the Solution band's top edge be dragged`.

---

### Task 3: Verify it live, then document it

**Files:**
- Modify: `FORK.md` (one numbered decision entry after #93)
- Modify: `docs/plans/2026-08-27-solution-band-height.md` (this file — mark the tasks done)
- Modify: `docs/INDEX.md` if the plans table needs the new row

- [ ] **Step 1: Build and launch your own editor.** `cargo build --bin sawe` first (mandatory — `script/run-mcp` only compiles a *missing* binary), then `script/run-mcp --debug --headless`. Drive its socket directly with newline-delimited JSON-RPC; `crates/editor_mcp/tests/solutions_add_member_e2e_test.rs::call_tool` has a 10-line client that filters the interleaved `editor/notification` frames. Solution-scoped tools (`workspace.*`, `project.*`) live on the **per-solution** socket, whose path comes back in `solutions.get`'s `mcp_socket`.

- [ ] **Step 2: Paint a real conversation.** `solution_agent.seed_cold_session` is a `#[cfg(debug_assertions)]` verification-only tool that paints an arbitrary render state without a live subprocess — use it to put enough entries in a session that the transcript is genuinely scrollable, select that session in the band, and screenshot. **The screenshot must show a conversation occupying most of the band**, not a status row and a compose box. Read the PNG back yourself.

- [ ] **Step 3: Exercise the drag.** `windows.drag_at {from_x, from_y, to_x, to_y}` on the band's top edge (`workspace.dump_visual_structure` / a screenshot gives you the coordinate). Screenshot before and after; the band must be visibly taller/shorter and the project zone correspondingly smaller/larger. Note that `windows.drag_at` rests the cursor on the start point first because GPUI only arms a drag from a MouseDown on an already-hovered hitbox.

- [ ] **Step 4: Prove persistence across a restart.** Drag to a distinctly non-default height, wait past the 400ms debounce, quit the editor, relaunch it, reopen the same Solution, screenshot. The band comes back at the dragged height. If it does not, that is a real defect — report it, do not paper over it.

- [ ] **Step 5: Prove the clamp.** With a stored height larger than 80% of the window, confirm the band paints capped and the status bar is still visible. A very small window is the cheap way to produce this.

- [ ] **Step 6: Gates.** `cargo test -p solution_agent -p solutions -p workspace -p console_panel -p solutions_ui`, `cargo fmt --all --check`, and `./script/clippy -p solution_agent -p console_panel -p solutions_ui -p solutions -p workspace`. All three green. (`cargo fmt --all --check` catches slips the per-crate gates do not — it was RED at the end of phase 2a while every crate check was green.)

- [ ] **Step 7: Document.** Add one `FORK.md` decision entry after #93 covering: the band owns a persisted per-Solution height; it is absolute pixels, not a window fraction; the window-relative cap is computed at render and never written back, and *why* (the draw-phase notify trap); and the handle commits from `on_drag_move`, not `on_drop` (FORK.md #84). Mark this plan's tasks complete. Do not touch `.rules`.

- [ ] **Step 8: Commit** as `solution_agent,FORK.md: Record the band-height decision` (docs) — verification artifacts are not committed.
