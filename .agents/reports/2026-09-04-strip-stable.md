# The project strip stops moving on its own

**Date:** 2026-09-04
**Crate:** `solutions_ui` (`crates/solutions_ui/src/project_tab_strip.rs` — the only file changed)
**Supersedes the behaviour of:** `cc05f6ef6d` (partially) and `f3ef02f0f2` (entirely)

## The rule

> «Я хочу на панели видеть в первую очередь самые важные проекты, а в `…` ожидаю что будут
> незначительные. Самопроизвольные скачки тут только все портят.»

The painted set is a function of **stored order** and **available width**, and of nothing else.
Which project is active is not an input. Activating a project that lives in the `…` leaves the
strip exactly as it was.

## What was removed

### 1. `cc05f6ef6d` — promote-on-click

The `…` menu's click handler called `reorder_members` with the picked member moved to the head of
the order, so navigating permanently rewrote the arrangement the user had built. Already removed by
`f3ef02f0f2`; this change keeps it out and now asserts against it as the **first** assertion in
`picking_a_project_from_the_overflow_menu_activates_it_and_moves_nothing`, so a reintroduction
fails on its own name rather than downstream on a consequence.

### 2. `f3ef02f0f2` — the active-member width reservation

`visible_indices(widths, budget, more_button, active: Option<usize>)` paid for the active member's
width before walking the prefix, then sorted its index back into place. The active project was
therefore always on the strip — at the cost of one tab at the fold dropping into the `…` the moment
an overflow project was selected. Same class of defect as the promotion: a navigation gesture
rearranging the layout.

**Removed by deleting the parameter, not by branching around it.** The function is back to
`fit_count(widths, budget, more_button) -> usize`, a plain greedy prefix. The rule is now carried by
the signature: there is no active member to consult, so no code path can reintroduce the jump
without also reintroducing the parameter. Correspondingly, `render` no longer computes an
`active_index`, and the visible/overflow split is a `members.split_at(count)` rather than an index
set — the painted tabs are always a prefix, never an arbitrary subsequence.

The unmeasured first frame lost its active-member injection too (it was there so the first frame
after a switch never flashed an unhighlighted strip; that job now belongs to the `…` marker).

### What was NOT removed

- **The width budget from `cc05f6ef6d`** — the `canvas` measurement, `measured_bounds`, the
  `flex_1`/`flex_none` toolbar arrangement, `tab_width_for_label`, `MORE_BUTTON_WIDTH`,
  `PLUS_DIVIDER_WIDTH`, `BUDGET_SAFETY_MARGIN`, the "never hide every project" floor. Untouched.
- **The drag out of the `…` menu.** Still the one deliberate way to change order. `project_tab.rs`
  is byte-for-byte unchanged.
- **The menu's check mark.** It was reachable-in-principle-only under the reservation (the active
  member was never in the overflow list at steady state). It is fully live again.

## How the `…` marker works

Because the active project can now be invisible, the feedback moved onto the `…` button itself.

```rust
let active_in_overflow = active_member
    .is_some_and(|active| overflow.iter().any(|(member_id, _)| *member_id == active));
```

When true, the `IconButton`:

- takes `icon_color(Color::Accent)` instead of `Color::Muted` — the same `Color::Accent` the menu's
  own `IconName::Check` row marker already uses;
- gains `indicator(Indicator::dot().color(Color::Accent))` with
  `indicator_border_color(Some(cx.theme().colors().title_bar_background))` — the exact idiom
  `workspace::status_bar` uses to mark the sidebar toggle when there is something behind it. The
  border colour is the strip's own background (`title_bar_background`, which
  `title_bar::project_toolbar` paints the row with), not `IconWithIndicator`'s
  `elevated_surface_background` default;
- swaps its tooltip to "More projects — the active project is in here".

No new colour was invented. The dot is absolutely positioned inside `IconWithIndicator`, so it
costs **zero layout width** and `MORE_BUTTON_WIDTH = px(33.0)` stays honest — verified by
`tabs_lay_out_at_their_predicted_width` and by the live pixel diff below (the `…` cell occupies the
same x-range marked and unmarked).

### Testability of the marker

`IconButton` registers its own `ICON-{IconName:?}` debug selector, which is **state-blind** — it has
identical bounds marked and unmarked. So the button's cell is wrapped in a div carrying a
state-named selector, the same trick `overflow_menu_row_selector` already uses and the one
`docs/findings/2026-09-02-paint-tests-with-debug-bounds.md` prescribes for state-dependent icons:

```rust
pub(crate) fn overflow_more_selector(active_in_overflow: bool) -> &'static str {
    if active_in_overflow { "PROJECT-OVERFLOW-MORE-ACTIVE" } else { "PROJECT-OVERFLOW-MORE-INACTIVE" }
}
```

**Known limit, stated plainly:** this pins *which state the strip decided to paint*, not the accent
hue or the dot's pixels. `Indicator` has no debug selector to hang an assertion on, and giving it
one would mean hand-rolling `IconWithIndicator` instead of using the fork's component. The colour
and the dot are covered by the screenshots below, not by a test.

## Tests

All painted, via `VisualTestContext::debug_bounds` after a real frame (two draws — one to measure
the strip's box, one to render the split it decided). 15 tests in `project_tab_strip`, all green.

### Rewritten from `f3ef02f0f2` (none deleted outright without saying where the cover went)

| `f3ef02f0f2` test | Fate |
|---|---|
| `the_active_member_is_reserved_a_slot_past_the_fold` (unit) | → `the_split_is_the_leading_run_that_the_width_pays_for`. The reservation half is gone; the budget arithmetic it was built on survives and is **strengthened** with the 332px/333px boundary pair (one pixel decides tab three). |
| `an_active_member_wider_than_the_whole_budget_is_still_painted` (unit) | → `a_leading_tab_wider_than_the_whole_budget_is_still_painted`. The over-wide case still matters; the member it protects is now the **first**, not the active one. |
| `an_active_member_that_already_fits_changes_nothing` (unit) | **Not rewritten.** It tested an edge case of the `active` parameter, and the parameter is gone; "which member is active does not move a tab" is now a property of the *signature*, so a unit test of it would be a tautology. Cover moved to the paint test `activating_a_project_in_the_overflow_changes_nothing_on_the_strip`. A comment in `fit_tests` says exactly this so a future reader does not think it was silently dropped. |
| `an_out_of_range_active_index_is_ignored` (unit) | **Not rewritten**, same reason — it guarded an `active` index that no longer exists. It was the only cover for the out-of-range guard, and that guard is gone with its input; nothing else regressed. |
| `the_active_project_is_painted_in_place_even_when_it_sits_past_the_fold` (paint) | → **`activating_a_project_in_the_overflow_changes_nothing_on_the_strip`**, which asserts the opposite rule. |
| `picking_a_project_from_the_overflow_menu_activates_it_without_reordering` (paint) | → `…_activates_it_and_moves_nothing`. Keeps the stored-order assertion, adds the painted-frame equality, and reorders the assertions so a reintroduced promotion dies on the order assertion. |
| `an_overflow_row_paints_the_active_mark` / `…_unmarked_when_it_is_not_active` (paint) | **Kept as-is**, doc comment corrected: they used to be justified as "this state is unreachable through the menu"; it is reachable now, and the end-to-end path is additionally covered by the paint test above. They stay as the cheap both-sides cover for the row itself. |
| `a_project_can_be_dragged_out_of_the_overflow_menu_onto_the_strip` (paint) | **Kept**, plus one new assertion: the repainted strip must be a prefix of the **new** stored order (`assert_keeps_relative_order`, which would otherwise have become dead code). |

### New

- `the_overflow_button_is_unmarked_while_the_active_project_is_on_the_strip` — the other side of the
  marker.
- `activating_a_project_in_the_overflow_changes_nothing_on_the_strip` asserts, in one comparison,
  that the painted tabs are **identical `Vec<(MemberId, Bounds<Pixels>)>`** before and after: same
  members, same left-to-right order, same origin and size to the pixel. Plus: stored order
  unchanged, the activated project still off the strip, **no** tab claims to be active, the `…` is
  marked, and the active project's menu row carries the check.

## Mutation table

Every mutation was applied to a working tree, compiled, run, and **reverted** (`diff -q` against a
pre-mutation copy confirmed a clean tree afterwards; `git diff --stat` shows one changed file).

| # | Mutation | Applied | Run | Reverted | Result |
|---|---|---|---|---|---|
| M1 | `cc05f6ef6d`'s promotion restored — the menu's click handler also calls `reorder_members` with the picked member moved to the head | yes | yes | yes | **1 killed**: `picking_a_project_from_the_overflow_menu_activates_it_and_moves_nothing`, on `"…must not reorder the members — dragging is the gesture that reorders"` |
| M2 | `f3ef02f0f2`'s reservation restored — pay for the active member up front, drop one tab at the fold, pull the active member onto the strip in place | yes | yes | yes | **2 killed**: `activating_a_project_in_the_overflow_changes_nothing_on_the_strip` and `picking_…_and_moves_nothing`, both on the painted-geometry equality, with the diff naming the displaced fold tab (`MemberId(5)` at x=432 replaced by `MemberId(13)`) |
| M3 | `active_in_overflow = true` (marker always on) | yes | yes | yes | **2 killed**: `the_overflow_button_is_unmarked_while_the_active_project_is_on_the_strip`, `activating_…_changes_nothing_on_the_strip` |
| M4 | `active_in_overflow = false` (marker never on) | yes | yes | yes | **2 killed**: `activating_…_changes_nothing_on_the_strip`, `picking_…_and_moves_nothing` |
| M5 | `ProjectTab`'s `on_drop` gutted (no `reorder_members`) | yes | yes | yes | **1 killed**: `a_project_can_be_dragged_out_of_the_overflow_menu_onto_the_strip` |

M1's kill point moved as a result of running it: the first pass killed on the `…`-marker assertion
instead of the order assertion, which is a true but indirect signal. The assertions in that test
were reordered so the on-point one fires first, and M1 was re-derived against the reordered test.

## Verification

| Gate | Result |
|---|---|
| `CARGO_BUILD_JOBS=4 cargo build --bin sawe` | exit 0, zero `^error`, zero `^warning` |
| `CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets` | exit 0, zero `^error`, zero `^warning` |
| `CARGO_BUILD_JOBS=4 cargo test -p solutions_ui -p solutions -p title_bar` | 234 + 65 + 9 passed, 0 failed (2 ignored, pre-existing) |
| Live, `script/run-mcp --debug --headless --runtime-dir /tmp/strip-stable-probe` | see below |

Binary freshness confirmed before the live run: `strings target/debug/sawe | grep -c "the active
project is in here"` → 1.

## Live check

Isolated probe (`--runtime-dir /tmp/strip-stable-probe`, never the maintainer's socket), a
12-member Solution, window narrowed to 1000×700 with `windows.resize` so four tabs fit and eight
spill.

1. **Before** — `2026-09-04-strip-stable-before.png`. Strip: `ecos-base` (active, highlighted),
   `ecos-records`, `citeck-community`, `ecos-webapp`, `…` (muted grey), `+`.
2. Opened the `…` (hover, then click — a bare click is swallowed), clicked `ecos-bpmn`, the
   **last** member in the stored order.
3. **After** — `2026-09-04-strip-stable-after.png`.

Pixel diff of the two full frames (threshold 8 on summed channel delta), differing x-runs within
the toolbar row `y ∈ [36, 63)`:

```
total differing px: 2628   bbox x 47..575  y 36..60
toolbar-row differing x-runs: [(47, 151), (563, 575)]
```

Only two regions changed:

- **x 47–151** — the `ecos-base` tab losing its active highlight (background, bottom border, label
  colour). Correct: it is no longer the active project.
- **x 563–575** — the `…` button gaining its marker.

`ecos-records` (~160–270), `citeck-community` (~280–410), `ecos-webapp` (~430–550) and the `+`
(~600) are **pixel-identical**. No tab moved, none was added, none was dropped.

Zoomed crops of the strip: `2026-09-04-strip-stable-more-unmarked.png` (before) vs
`2026-09-04-strip-stable-more-marked.png` (after) — the `…` glyph goes accent-blue and picks up the
accent dot at its lower right.

Stored order read back from `solutions.get` after the click, unchanged:

```
ecos-base, ecos-records, citeck-community, ecos-webapp, ecos-model, ecos-uiserv,
ecos-apps, ecos-integrations, ecos-notifications, ecos-process, ecos-history, ecos-bpmn
```

4. **Menu** — `2026-09-04-strip-stable-menu-check.png`: reopening the `…` shows all eight overflow
   projects in stored order with the check on `ecos-bpmn`. The check is reachable again.
5. **Drag** — `2026-09-04-strip-stable-drag.png`: dragged `ecos-uiserv` out of the menu onto the
   `ecos-records` tab. Stored order became
   `ecos-base, ecos-uiserv, ecos-records, citeck-community, ecos-webapp, …` and the strip repainted
   `ecos-base, ecos-uiserv, ecos-records, citeck-community` — `ecos-webapp` fell into the `…`
   because a genuinely reordered member took its slot. Deliberate reorder, deliberate consequence.

Two gotchas for whoever drives this next, both cost a cycle here:

- **The `…` trigger's own tooltip overlays the menu's first row.** Immediately after clicking the
  `…`, a hover on row 1 lands on "More projects", not on the row — a `drag_at` from there silently
  does nothing. Park the cursor elsewhere inside the popover first to dismiss the tooltip, *then*
  hover the row, *then* drag.
- **The open popover's contents are captured at open time.** The post-drag screenshot still lists
  the pre-drag overflow set. A screenshot artefact of the retained scene, not a strip bug.

## Replacement wording for `FORK.md` #146

`FORK.md` was **not edited** (per instruction). Note that #146 was never updated for `f3ef02f0f2`
— it still names `fit_count`, which is accurate again, and its last-but-one paragraph still
describes the promote-on-click that has been gone since `f3ef02f0f2`. Proposed replacement for the
whole entry:

---

### 146. The project tab strip fits tabs to a measured width; nothing but order and width decides what it paints

What: `ProjectTabStrip` decides how many tabs to paint from the width it was actually given, not
from a count. `MAX_VISIBLE_TABS = 6` is deleted; a `canvas` over the strip's own box records
`measured_bounds`, each tab's natural width comes from its shaped label through
`project_tab::tab_width_for_label`, and `fit_count` takes tabs greedily out of the measured width
minus the ghost tabs, the `+` cell, the rule and the `…` button — subtracting the `…` only when
something actually spills, and never returning zero (a strip narrower than one tab shows one and
scrolls).

The old cap was not a bad budget, it was **no budget**: nothing in the strip ever looked at a
width, so the count was identical at 1920px and at 1000px. Measured in an isolated probe with a
12-member Solution, six tabs occupied ~745px of a 1920px row and ~790px — 41% of the row — sat
blank while six projects hid behind the `…`. Compounding it, `ProjectToolbar` mounted the strip as
a **content-sized** child next to a separate `flex_1` spacer, so the strip could not have measured
its budget even if it had wanted to.

**The strip is the user's layout, and it does not move on its own.** The painted set is a pure
function of the stored member order and the available width; which member is *active* is not an
input. Two attempts to make a selection visible got this wrong and are both reverted:
`cc05f6ef6d` promoted a clicked overflow project to the head of the member order, and `f3ef02f0f2`
replaced that with a width **reservation** for the active member, which spilled a tab at the fold
instead. Both let a navigation gesture rearrange the arrangement the user built. The rule is now
enforced by the **signature**: `fit_count(widths, budget, more_button) -> usize` has no active
member to consult, so the jump is unrepresentable rather than merely absent. Activating a project
that lives in the `…` therefore changes nothing about the strip.

The feedback lives on the `…` **button** instead, which is where it belongs once the active project
can be invisible: `icon_color(Color::Accent)` plus `Indicator::dot().color(Color::Accent)` bordered
with the row's own `title_bar_background` — the same idiom `workspace::status_bar` uses for the
sidebar toggle, and the same accent as the menu's `IconName::Check`. The dot is absolutely
positioned inside `IconWithIndicator`, so it costs no layout width and `MORE_BUTTON_WIDTH` stays
honest. The menu's per-row check (kept in the tree and `invisible()` when inactive so labels stay
aligned) is fully reachable now that the active member is no longer pulled out of the overflow.

Two structural facts make the measurement safe, and both are the reason to keep it this shape:

- **The strip is the toolbar's `flex_1().min_w_0()` child and the trailing widgets are
  `flex_none()`.** The measure→decide loop (the `cx.defer(notify)` pattern from
  `docs/findings/2026-08-17-gpui-draw-phase-invalidation.md`, guarded on the bounds having changed)
  cannot oscillate *because* a `flex_1` box's width does not depend on its content — the measured
  quantity is independent of the decision it drives. Make the strip content-sized again and that
  guarantee is gone. Making the trailing cluster `flex_none` also fixed a second, pre-existing bug:
  below ~1050px the git widget, run-config strip and dock toggles used to be pushed off the right
  edge entirely.
- **A drag can start inside a `ContextMenu` and survive the menu's dismissal**, because
  `active_drag` lives on the `App`, not in the menu's element tree. That is what makes "drag a
  project out of the `…` menu onto the strip" work at all, and it needed no change to
  `ui::ContextMenu`. The only mechanical requirement is that the row be a `custom_entry` rather than
  a `toggleable_entry`, since the latter offers nowhere to hang `on_drag`; the row hand-builds what
  `toggleable_entry` would have drawn. The drag is the **only** gesture that rewrites the order.

How to apply: assert this against painted geometry, never against a width literal or a predicate —
the tests compare `fit_count`'s prediction with what a real frame painted, and the stability rule is
asserted as an equality of the whole `Vec<(MemberId, Bounds<Pixels>)>` before and after an
activation, which is what catches a reservation creeping back in. State that is invisible to
geometry (a tab's active highlight, the `…`'s marker) must be carried in a **state-named**
`debug_selector`, because `IconButton`'s own `ICON-{Icon}` selector and an `invisible()` check both
have identical bounds in either state. Note the strip needs **two** draws in a test: one to measure
its box, one to render the split that measurement decided.

---

## Concerns

- The `…` marker's **colour** is not asserted by a test, only its state (see "Testability of the
  marker"). A refactor that swapped `Color::Accent` for something else would pass the suite. The
  screenshots are the cover.
- `Indicator`'s default border is `elevated_surface_background`; this call site overrides it with
  `title_bar_background` because that is what `title_bar::project_toolbar` paints the row with. If
  the toolbar's background ever changes, this override has to follow — there is no compile-time
  link between the two.
- Unrelated and pre-existing: the open `…` popover renders the overflow list captured at open time,
  so it goes stale if the order changes underneath it. Harmless in practice (the menu is dismissed
  by any interaction that would change the order), but noted.
