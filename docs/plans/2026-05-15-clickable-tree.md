# Clickable tree + click-by-id (phase 1: hitbox-based MVP)

**Status:** complete (2026-05-15). Phase 1 sub-agent shipped the
structural API (`Window::iter_hitboxes`, `Clickable` struct,
`clickables: Vec<Clickable>` on dump result, `windows.click_id` tool,
e2e test). Supervisor follow-on **phase 1b** (`3954269122`) populated
`kind` + `label` from inspector source-loc data — 29% of clickables
now labelled with `file:line` (covers all title-bar / tabs / panels /
buttons). Sub-agent commits `1db0aff`, `6e3bf48`, `c3d8e90` merged as
`07c3cee`. Remaining 71% anonymous (text spans / gutter / layout
chrome built without `inspector_id`) — out of scope for phase 1;
addressing them needs deeper GPUI instrumentation, defer to phase 2.
**Estimated:** 1 sub-agent session, ~2 h, worktree-isolated
**Goal:** Agents drive the editor's UI via stable element IDs and synthetic
click-by-id, not pixel coordinates. Most LIGHT-track UI verifications stop
needing a screenshot at all.

## Context

Today the supervisor's MCP surface has three UI introspection / driving
primitives:

- `workspace.dump_visual_structure` — a logical tree (Workspace / TitleBar /
  Dock / Pane / Tab) hand-walked from known view types. Does NOT recurse
  into rendered pane content, and does NOT enumerate buttons / dropdowns /
  menu items. `Modal(...)` entries show with empty `children: []`.
- `windows.click_at(window_id, x, y)` — pixel coordinates. Geometry-
  dependent (every layout change invalidates agent test fixtures), and
  agents can't discover which (x, y) corresponds to which feature.
- `windows.dispatch_action(window_id, action_name, args)` — works only for
  things bound to a named `Action`. Most buttons / icon buttons in the fork's
  fork-owned crates dispatch named actions, but contextual menu items,
  IconButtons inside modals, picker rows, and ad-hoc clickables don't all
  have one.

User-set goal 2026-05-15: "сделать чтобы в ответе от функции, которая
возвращает структуру панелей и попапов возвращались так же все элементы
управления (кнопки, дропдауны. В общем все кликабельное) вместе с их id и
функцию. чтобы имитировать клик ПКМ или ЛКМ на них передавая id. В ответ
на функцию клика возвращаем структуру попапа, который появился при клике
(если появился). Т.е. идея в том, чтобы агенту не надо было в большинстве
случаев с скриншотами работать."

## Options considered

### Option A — Per-component metadata registration

Every clickable primitive (`Button`, `IconButton`, `ContextMenuItem`,
`Tab`, etc.) emits `cx.register_clickable(id, ClickableMetadata { label,
kind, action })` on `paint`. Frame-reset registry collected by the MCP
tool.

- **Pros:** Rich metadata. Labels are author-supplied (no heuristics).
- **Cons:** Touches every UI primitive. Many primitives live in untouched-
  upstream crates (`ui`, `editor`, `terminal`, `git_ui` shared with
  upstream surfaces). Either we touch them (FORK.md table grows), or
  coverage stays partial. Slow rollout.

### Option B — Generic hitbox walking (MVP)

Walk `Window.rendered_frame.hitboxes` post-paint. Each `Hitbox` has
`{id: HitboxId, bounds: Bounds<Pixels>}`. Cross-reference with the
existing `dump_visual_structure` tree by bounds-overlap to attach
semantic kind/label heuristically. Click-by-id = look up bounds, simulate
mouse event at centre.

- **Pros:** No per-component changes. Auto-covers every clickable. Single
  contained change.
- **Cons:** Labels weak (only from existing hand-walked tree's overlapping
  nodes). No `action_name` attached. HitboxId is per-frame; need stable
  id derived from element-path or content.

### Option C — Hybrid (B as MVP, A as future incremental opt-in)

Phase 1 = B verbatim. Phase 2 (separate plan, later) = primitive crates
in fork-owned zones (`ui` is fork-owned for many components, but lots
are upstream-shaped) opt in via `register_clickable`. Untouched-upstream
primitives stay anonymous-hitbox until we have a reason to first-touch
them.

- **Pros:** Ships value in one phase. Iterative path to richer metadata
  without re-architecting.
- **Cons:** Phase 1 alone may feel under-labelled — labels come from the
  hand-walked tree's overlap, so deep menu items still have no label.
  Acceptable for the demo case (Picker, ProjectPanel) but not for "every
  context menu item is named".

## Decision

**Phase 1 = Option B (generic hitbox walking).** This plan covers phase 1
only. Phase 2 (per-component metadata) lands as a separate plan when the
phase-1 labels prove insufficient.

Phase 1 unblocks the queued LIGHT-track UI work (Picker / ProjectPanel /
queued-message UI) — agents can locate and click elements by ID, and verify
"clicking X opens modal Y with N items" without screenshots.

## Scope

### A. `gpui` — expose hitbox enumeration via `Window`

`crates/gpui/src/window.rs`:
- Add `Window::iter_hitboxes(&self) -> impl Iterator<Item = &Hitbox>` (or
  `hitboxes()`) that returns the hitboxes from
  `self.rendered_frame.hit_test_nodes` / `self.rendered_frame.hitboxes`
  (whichever is the right field — sub-agent reads the source).
- `Hitbox` already has `id` and `bounds`. If `id` isn't already publicly
  accessible, surface it via a getter.

### B. `solutions::mcp` — enrich `dump_visual_structure` with clickables

`crates/solutions/src/mcp.rs`:
- New struct alongside `VisualNode`:
  ```rust
  pub struct Clickable {
      pub id: String,              // stable hash of (hitbox path, label_hint)
      pub bounds: [i32; 4],        // [x, y, w, h] logical pixels, window-relative
      pub kind: Option<String>,    // "Tab", "Button", "MenuItem", ... — best-effort
      pub label: Option<String>,   // best-effort, from overlapping tree node
      pub focused: bool,           // whether the underlying focusable is focused
  }
  ```
- Add `clickables: Vec<Clickable>` to `DumpVisualStructureResult` (additive
  — existing clients ignoring the field still work).
- After building the `VisualNode` tree, walk `window.iter_hitboxes()` and
  for each hitbox:
  1. Convert pixel bounds to `[i32; 4]`.
  2. Cross-reference with the `VisualNode` tree: find the deepest node
     whose bounds enclose the hitbox centre. Carry that node's `kind` /
     `label` onto the `Clickable`.
  3. Stable ID: hash of `(window_id, hitbox content-path)` if such a path
     exists; else hash of `(kind, label, bounds-rounded-to-grid)` —
     resilient to small layout shifts.
- Recurse the tree-builder into modals / context menus / dropdowns
  (current builder stops at `Modal(...)` with empty children). The
  modal's content view is reachable via `cx.windows()`-side state.

### C. `windows.click_id` MCP tool

New tool registered alongside the existing `windows.click_at` /
`windows.dispatch_action`.

`crates/workspace/src/mcp/windows.rs`:
```rust
pub struct ClickIdParams {
    pub window_id: String,
    pub id: String,
    #[serde(default = "default_button")]
    pub button: String,            // "left" | "right" | "middle"
}

pub struct ClickIdResult {
    pub clicked: bool,
    pub bounds: [i32; 4],          // bounds that were clicked
}
```
- Look up the clickable by ID via the same hitbox-enumeration code (or
  a shared cache that `dump_visual_structure` populated).
- Compute centre of bounds, dispatch a synthetic `PlatformInput::MouseDown`
  + `MouseUp` via the platform window's `deliver_input` (HeadlessWindow)
  or the X11/Wayland send-event path.
- Return the bounds that were clicked (so the agent can verify it hit
  what it expected).

**Idempotency note:** the click is a one-shot. There's no "click and wait
for popup" combinator in phase 1 — the agent calls `windows.click_id` →
sleeps ~200 ms → calls `dump_visual_structure` to see the resulting state.
Building the wait-and-diff combinator into the tool is a phase-2 nice-to-
have, deferred.

### D. Tests

`crates/solutions/tests/clickable_tree_e2e_test.rs` (new) — must call
`editor_mcp::set_runtime_dir_for_test(tempdir)` BEFORE `start_server` (per
`.rules` § "Integration tests"). End-to-end:
1. Open AlphaSol headless.
2. `dump_visual_structure` returns ≥ N clickables (N = something like 5 —
   the tabs + project-panel rows + status-bar dots should all be there).
3. Pick the AlphaSol tab clickable by label/kind; `click_id`; verify the
   tab is now active (`focused: true`).
4. Open a context menu via right-click on a project-panel row; verify the
   menu's items appear as new clickables in the next dump.

### E. Documentation

- `FORK.md`: add row for the new file `crates/solutions/src/mcp.rs` (already
  listed) → no new row needed; if `windows.click_id` lives in `workspace`
  add row for `crates/workspace/src/mcp/windows.rs` (already listed).
- `docs/INDEX.md`: no new doc — this is implementation only. ADR not needed
  (phase 1 is small enough that the design rationale lives in this plan
  doc; ADR-0003 would land if phase 2 ships per-component registration).
- Bump MCP tool count in `CLAUDE.md` / `.rules` § "Autonomous testing via
  embedded MCP" (`59 tools` → `60 tools`).
- Tick acceptance items in this plan.

## Out of scope

- **Per-component label registration** (Option A). Phase 2.
- **Action-name attachment** to clickables. Phase 2 (requires
  per-component opt-in).
- **`click_id_and_wait` combinator** (click + wait + diff dump). Phase 2 if
  the call-pair pattern proves clunky.
- **Hover events.** No MCP primitive for hover today, and the click-to-open
  pattern covers most cases. Punt.
- **Drag/drop.** Punt.
- **Touch events.** Punt.
- **Recursing into editor buffer content** (text, syntax-highlighted
  glyphs, etc.) as "clickables". Out of scope — editor content lives in
  the multi-buffer subsystem and is reached via `project.*` MCP tools.

## Architectural decisions

1. **Hitbox-first, metadata-later.** A generic hitbox walk (B) ships
   value in one phase without touching upstream-shaped UI primitives. The
   trade-off is weak labels at the leaves; the cure is incremental
   opt-in in fork-owned crates later (A on top of B).
2. **Stable ID via content-path hash, not raw HitboxId.** `HitboxId` is a
   per-frame slot id; hashing the rendered element-path (or a fallback of
   `(kind, label, bounds-rounded)`) gives an ID that survives a notify+
   rerender cycle.
3. **No "wait for popup" combinator in phase 1.** Sleep+poll is good
   enough for now; building a proper "click and observe new modal" needs
   subscribing to gpui's window-event broadcaster, which is a separate
   feature surface. Agents tolerate the sleep — they already sleep after
   `solutions.open`.
4. **Single-shot click, no auto-retry.** If the click lands on a busy /
   suppressed clickable, the agent observes via the next dump and decides.
   Auto-retry hides agent bugs.

## Risks

- **Hitbox enumeration may not cover modals / context menus.** If gpui's
  modal layer paints into a separate `rendered_frame` (overlay), our walk
  misses them. Mitigation: sub-agent verifies the test in § D actually
  enumerates menu items; if missing, surface in REPORT — supervisor
  decides on a small follow-up (most likely walk gpui's modal-stack too).
- **Stable-ID collisions.** Two buttons with the same label inside the
  same parent → identical hashes. Mitigation: include the bounds (rounded
  to a coarse grid) in the hash. Genuine collisions become "two
  clickables with the same id but different bounds" which the dump
  exposes — agents can disambiguate by bounds.
- **Phase 1 labels may be too sparse to be useful.** If most clickables
  come back with `kind: None, label: None`, agents are still stuck
  guessing. Mitigation: the test in § D measures coverage; if <30% of
  clickables get labels, sub-agent surfaces in REPORT and supervisor
  decides whether to bring phase 2 forward.

## Verification

```bash
cd <worktree>
cargo build --bin sawe 2>&1 | tee /tmp/build.txt
grep -E "^error|could not compile" /tmp/build.txt   # must be empty
cargo clippy -p gpui -p solutions -p workspace -p editor_mcp --all-targets -- -D warnings
cargo test -p gpui -p solutions -p workspace -p editor_mcp --no-fail-fast
# End-to-end via the headless platform (now reliable post ADR-0002):
script/run-mcp --debug --headless &
until [ -S "$HOME/.spk/sawe-dev/config/mcp.sock" ]; do sleep 0.5; done
# Drive a Python client:
# - solutions.open alphasol
# - workspace.dump_visual_structure → assert clickables.len() >= 5
# - find the "README.md" Tab clickable; windows.click_id; verify
#   the dump now reports that tab focused
# - right-click a project-panel row (button: "right") via click_id; sleep
#   200 ms; new dump shows the context menu's items as clickables
pkill -f target/debug/sawe
```

Supervisor § H smoke-test runs the Python flow above; sub-agent does NOT
inline it (per supervisor-mode.md HARD RULES).

## When done

- [ ] `cargo build --bin sawe` clean.
- [ ] `cargo clippy` on touched crates clean.
- [ ] `cargo test` on touched crates green.
- [ ] `Window::iter_hitboxes()` (or equivalent accessor) exists in `gpui`.
- [ ] `dump_visual_structure` returns a `clickables: [...]` field with at
  least the title-bar tabs + project-panel rows of an open Solution.
- [ ] Modals and context menus appear in the tree with non-empty children.
- [ ] `windows.click_id` MCP tool dispatches a click by ID; result
  includes the bounds clicked.
- [ ] Right-click variant works (`button: "right"`).
- [ ] E2E test in `crates/solutions/tests/clickable_tree_e2e_test.rs`
  passes.
- [ ] MCP tool count bumped in `.rules` § "Autonomous testing via embedded
  MCP" (one new tool).
- [ ] Plan doc ticked + final SHA appended at the bottom.
