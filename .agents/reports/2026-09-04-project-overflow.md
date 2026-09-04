# Project tab strip — overflow menu and early truncation

Three maintainer-reported problems with the project tab strip in the Solution
band's project-toolbar row, and what shipped for each.

## 1. The measured cause of the early truncation

**It was a fixed count, not a width budget at all.**

`crates/solutions_ui/src/project_tab_strip.rs` had:

```rust
/// How many project tabs render inline before the rest spill into the
/// `more` popover. A simple fixed cap — the strip lives in the title bar
/// where horizontal space is tight, and pixel-measured overflow isn't
/// worth the complexity here.
const MAX_VISIBLE_TABS: usize = 6;
```

and split `members.split_at(MAX_VISIBLE_TABS)`. Nothing in the strip ever
looked at a width. Compounding it, `crates/title_bar/src/project_toolbar.rs`
mounted the strip as a **content-sized** child followed by a separate
`div().flex_1()` spacer, so even if the strip had wanted to measure its
budget, its own box was only as wide as the tabs already in it.

### Numbers, measured off the running editor

A probe Solution with 12 members (`script/run-mcp --debug --headless
--runtime-dir /tmp/overflow-probe`), ink-run analysis of the toolbar row at
y=48 in `workspace.screenshot` output.

At **window width 1920** (`.agents/reports/2026-09-04-overflow-before-1920.png`):

| thing | x range | width |
|---|---|---|
| first tab (`ecos-base`) starts | 35 | |
| six tabs occupy | 35 – 780 | **745 px**, mean 124.2 px/tab |
| `…` overflow button | 784 – 813 | 33 px (incl. its `px_1`) |
| rule + `+` cell | 813 – 844 | 31 px |
| **empty run** | **844 – 1634** | **≈ 790 px** |
| trailing git / run-config / dock cluster starts | 1634 | |

So **41 % of a 1920px row was blank** while six of twelve projects sat hidden
behind the `…`. At the measured mean tab width of 124 px, **all six hidden
projects would have fitted with ~50 px to spare.**

At **window width 1000** (`2026-09-04-overflow-before-1000.png`) the ink runs
are byte-for-byte identical out to x=833 — same six tabs — which is the proof
that the count was independent of width. Worse, at that width the trailing git
widget, run-config strip and right dock toggle were pushed **entirely off the
right edge** of the window: a second, pre-existing bug in the same row.

Candidates from the brief, resolved:

* **fixed tab count rather than a width budget** — *confirmed, this was it.*
* `min_w`/`max_w` over-reserving — *ruled out.* `min_w(80)`/`max_w(200)` never
  bind for these names; the painted widths (111–154 px) sit strictly between.
* container not actually `flex_1` — *confirmed as a contributing cause.* The
  strip could not have measured a budget even if it had tried.
* the ~10 % status-bar/tab rescale (`e12527465a`, `0c569d6c95`, `2de70b530b`)
  — *ruled out.* Those changed heights and font metrics; the cap is a literal
  `6` and predates them.
* unconditional room reserved for `…` + trailing controls — *ruled out.* The
  `…` is already gated on `!overflow.is_empty()`; it just never got the chance
  to be empty.

### The fix

* `project_toolbar.rs`: the strip is now `div().flex_1().min_w_0().h_full()`
  and the bare `flex_1` spacer is gone (kept only for the no-solution case).
  The trailing git cluster, run-config strip and right dock toggles are
  `flex_none()` so the row's slack is the one quantity a resize moves — which
  also fixes the ≤1050px case where they used to be shoved off-screen.
* `project_tab_strip.rs`: `MAX_VISIBLE_TABS` is deleted. A `canvas` covering
  the strip's own box records `measured_bounds`; each tab's natural width is
  derived from its shaped label (`window.text_system().shape_line`) through
  `project_tab::tab_width_for_label`; `fit_count` takes tabs greedily out of
  `measured_width − pending ghost tabs − `+` cell − rule − 4px margin`,
  subtracting the `…` only when something actually spills.

The measure→decide feedback loop is the documented `cx.defer(notify)` pattern
(`docs/findings/2026-08-17-gpui-draw-phase-invalidation.md`), guarded on the
bounds actually changing. It **cannot oscillate**, because the measured
quantity does not depend on the decision it drives: the strip is `flex_1`, so
its width is "whatever the row has left" regardless of how many tabs are in
it. Verified live — measurement settles to one value per window width:

| window | strip box | left cluster + trailing cluster |
|---|---|---|
| 1920 | 1580 px | 340 px |
| 1400 | 1060 px | 340 px |
| 1000 | 660 px | 340 px |
| 760 | 420 px | 340 px |

## 2. What shipped, per problem

### (3) «Список вкладок обрезается слишком рано»

Real width budget, as above. At 1920 the probe Solution now paints **all 12**
tabs with no `…` at all (was 6). At 1400 → 8 tabs + `…`. At 1000 → 4 + `…`. At
760 → 2 + `…`. In every case the trailing git/run-config/dock cluster stays on
screen, which it did not before.

### (1) «Нет индикации какой проект сейчас выбран»

Each overflow row now carries the leading `IconName::Check` in `Color::Accent`
that the rest of this fork's `ContextMenu` lists use for "the one you're on"
(`workspace::multi_workspace`, `workspace::dock`, `zed::quick_action_bar`,
`acp_tools`, …). No new colour, no new visual language. The check is kept in
the tree and made `invisible()` when inactive so every label stays aligned —
the same trick `ContextMenu`'s own toggle slot uses.

This is not a hypothetical state: `solutions.set_active_member` (and opening a
file in a hidden member) activates a project without touching the order, which
before this change left **no tab highlighted anywhere** in the window.

### (2) «Нельзя D&D вытащить оттуда проект»

**It works. Drag out of the overflow menu onto the strip is implemented and
verified, live and in a test.**

The brief's structural worry — GPUI menus dismiss on mouse-down outside
themselves, so a drag starting in a `ContextMenu` may be impossible — turns
out not to apply, and it is worth recording why: the drag **starts inside** the
menu, and `active_drag` lives on the `App`, not in the menu's element tree. So
the payload outlives the menu's dismissal and lands on whichever visible tab
(or the trailing end-drop zone) it is released over, through the `on_drop`
handlers the tabs already had. Nothing about `ui::ContextMenu` had to change.

Mechanically the only requirement is that the row be a `custom_entry` rather
than a `toggleable_entry`, because `toggleable_entry` gives no way to attach
`on_drag`. `custom_entry` still wraps the row in `ContextMenu`'s own
`ListItem` (inset, hover highlight, click routing), so it reads as an ordinary
menu entry — the check icon and label are hand-built to match what
`toggleable_entry` would have rendered.

Live proof (`2026-09-04-drag-result.png`): dragging `ecos-process` from the
menu onto the second visible tab moved it from position 11 to position 2 in
`Solution::members`, persisted through `SolutionStore::reorder_members` — the
existing ordering path, no second ordering invented.

Clicking a row additionally **promotes that project to the front** of the
member order and activates it. That was the maintainer's fallback ruling, and
it is worth keeping even though the drag works: it is the coarse version of the
same gesture for when the user just wants the project on the strip and does not
care where, and the head of the order is the one slot that is inside the budget
at any strip width.

Gesture model unchanged otherwise: single click on a tab still activates, tabs
still carry no close cross, the failed-add ghost tab keeps its right-click
escape hatch (`405c9600a3`) and is now budgeted for explicitly so it can never
be pushed out by member tabs.

## 3. Mutation table

Each mutation applied to the shipped tree, `cargo test -p solutions_ui
project_tab_strip` run, then reverted (`diff` against a pre-mutation copy
confirmed the revert).

| # | Mutation | Expected to fail | Actually failed | Verdict |
|---|---|---|---|---|
| M1 | Drop `.on_drag(...)` from `overflow_menu_row` | drag test | `a_project_can_be_dragged_out_of_the_overflow_menu_onto_the_strip` | caught |
| M2 | Pass `false` instead of `active_member == Some(id)` to every row | active-marking test | `the_overflow_menu_marks_the_active_project_and_only_that_one` | caught |
| M3 | Replace `fit_count(...)` with `6.min(widths.len())` (the old cap) | width tests | 6 of 11 tests, incl. both width tests | caught |
| M4 | Menu click only `set_active_member`, no `promote_to_front` | promote test | `picking_a_hidden_project_from_the_menu_puts_it_on_the_strip` | caught |
| M5 | Tab styling uses `px(20.)` while the budget still uses `TAB_PADDING_X` | prediction test | `tabs_lay_out_at_their_predicted_width` + the wide-strip test | caught |

M5 is the one that matters for durability: it is the drift the whole
`tab_width_for_label` / shared-constants arrangement exists to prevent, and it
is caught by comparing the predicted width against the width a real frame
painted.

## 4. Tests

11 tests in `crates/solutions_ui/src/project_tab_strip.rs`, all asserting
against `VisualTestContext::debug_bounds` after driving a real frame (the strip
needs **two** draws — one to measure its box, one to render the split that
measurement decided; `redraw()` in the test module does this).

* `a_wide_strip_paints_more_than_the_old_fixed_cap_and_still_fits` — count
  strictly greater than the old cap of 6; no tab's right edge past the strip's
  own painted box; `ICON-Ellipsis` painted **iff** something spilled; and, if
  anything spilled, the leftover space was genuinely smaller than the next
  tab's predicted width (`assert_no_room_was_wasted`). No magic width appears
  in any assertion — the geometry is read from the frame.
* `a_narrow_strip_paints_fewer_tabs_and_surfaces_the_overflow_button` — the
  other side, plus the split is a prefix in member order.
* `tabs_lay_out_at_their_predicted_width` — the analytic model vs. painted
  geometry, ±1 px.
* `the_overflow_menu_marks_the_active_project_and_only_that_one` — both sides,
  via a selector whose *name* encodes the state
  (`PROJECT-OVERFLOW-{ACTIVE,INACTIVE}-{id}`). Necessary because
  `debug_bounds` is recorded *before* `Style::visibility` is consulted
  (`gpui/src/elements/div.rs:2181`), so an `invisible()` check still has
  bounds and a state-blind selector would pass for both.
* `picking_a_hidden_project_from_the_menu_puts_it_on_the_strip` — asserts the
  tab is painted afterwards, not that the store's order changed.
* `a_project_can_be_dragged_out_of_the_overflow_menu_onto_the_strip` — the
  real gesture, ending in the dropped project occupying the drop target's slot.
* 5 pure-function tests for `fit_count` boundaries and `promote_to_front`.

Verification runs:

* `CARGO_BUILD_JOBS=4 cargo build --bin sawe` — exit 0, zero `^error`/`^warning`.
* `CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets` — exit 0, **0
  errors, 0 warnings**.
* `CARGO_BUILD_JOBS=4 cargo test -p solutions_ui -p solutions -p workspace -p
  title_bar` — exit 0; 234 + 61 + 9 + 255 passed, 0 failed.

## 5. Screenshots

All under `.agents/reports/`.

| file | what it shows |
|---|---|
| `2026-09-04-overflow-before-1920.png` | **Before.** 1920px window, 12-member Solution: exactly 6 tabs, `…` at x≈797, then ~790 px of dead space before the git cluster at x=1634. This is the maintainer's screenshot, reproduced. |
| `2026-09-04-overflow-before-1000.png` | **Before.** Same 6 tabs at a 1000px window — identical ink runs, proving the count ignored width — and the trailing git/run-config/dock cluster pushed off the right edge entirely. |
| `2026-09-04-menu-before.png` | **Before.** The `…` menu: six plain rows, no mark of any kind on the active project. |
| `2026-09-04-after-1920.png` | **After.** Same window, same Solution: all 12 tabs painted, no `…` (nothing spills), `ecos-bpmn` highlighted as active, git cluster intact at the right. |
| `2026-09-04-after-1000.png` | **After.** 1000px: 4 tabs + `…` + `+`, and the git branch widget / run-config strip / dock toggle all still on screen. |
| `2026-09-04-menu-after.png` | **After.** The `…` menu with `ecos-bpmn` (the active, hidden project) carrying a blue `Check` and the other seven rows unmarked but label-aligned. |
| `2026-09-04-drag-result.png` | **After.** The strip immediately after dragging `ecos-process` out of the overflow menu onto the second tab: it is now the second visible tab, and `solutions.get` confirms the persisted order changed to match. |

## 6. Sentence for `FORK.md` (not applied)

> The project tab strip sizes its visible tab list from a measured width
> budget rather than a fixed count: the strip is the project toolbar's
> `flex_1` child, a `canvas` reports the box it was actually given, and tabs
> are taken greedily using widths derived from their shaped labels — the
> feedback loop is safe only because a `flex_1` box's width does not depend on
> its content, which is also why the trailing toolbar widgets are `flex_none`.
> Rows of the trailing `…` menu mark the active project with the standard
> `ContextMenu` check and can be dragged out onto the strip; that drag works,
> despite menus dismissing on outside mouse-down, because it starts inside the
> menu and `active_drag` lives on the `App` rather than in the menu's element
> tree.
