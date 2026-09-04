# Overflow-menu click: activate, don't reorder

`cc05f6ef6d` made a click on a row of the project strip's `…` menu **promote**
that project to the head of the member order (`solutions.reorder_members`) and
then activate it. The maintainer questioned it — «а почему у нас при выборе
проекта из выпадающего списка … он перемещается в панель проектов на первое
место?» — and they are right: selecting a project is navigation, and navigation
must not permanently rewrite the tab order the user arranged. Reordering is what
the drag is for.

## What changed

Two files, `crates/solutions_ui/src/project_tab_strip.rs` and
`crates/solutions_ui/src/project_tab.rs`.

1. **The click handler is now one call.** The `custom_entry` callback for an
   overflow row does `store.set_active_member(solution_id, member_id, cx)` and
   nothing else. `promote_to_front` — the helper that existed only to serve that
   click — is deleted, along with the `order` capture the menu needed.

2. **The active member is folded into the width budget.** `fit_count(widths,
   budget, more_button) -> usize` is replaced by

   ```rust
   fn visible_indices(
       widths: &[Pixels],
       budget: Pixels,
       more_button: Pixels,
       active: Option<usize>,
   ) -> Vec<usize>
   ```

   which returns the *set of member indices* to paint instead of a prefix
   length. See "How the active member is folded in" below.

3. **The strip's split is no longer `members.split_at(n)`.** `visible` is the
   members at those indices, `overflow` is the complement — both in stored
   order, so the painted tabs are always a subsequence of the member order and
   the menu lists exactly the rest.

4. **A tab's active state is now paint-assertable.** `project_tab.rs` gained
   `project_tab_state_selector(member_id, is_active)` →
   `PROJECT-TAB-{ACTIVE,INACTIVE}-{id}`, hung on the colour dot the tab already
   paints (no new element, no layout change). Active and inactive tabs differ
   only in background, border colour and label colour — none of which are
   geometry — so without the state in the *name*, `debug_bounds` cannot tell
   them apart and a test that "asserts the tab is painted" would pass for the
   unhighlighted one too. Same reasoning as the existing
   `overflow_menu_row_selector`.

Nothing about the drag path changed: `on_drag` on the row, `on_drop` on the
tabs, `move_to_end` on the trailing zone, all still call `reorder_members`.

## How the active member is folded into the budget

`visible_indices` is the old greedy prefix with one addition: **the active
member's width is reserved out of the budget before the prefix is walked**, and
its index is added to the chosen set; the result is then sorted, so it is
painted in its *stored* position among whichever leading tabs still fit.

```
total <= budget                     -> every index (no `…`, nothing reserved)
otherwise budget -= more_button     -> the `…` is paid for first, as before
if active: used += widths[active]; chosen += active
for each index in order, skipping active:
    if used + width > budget { break }      // break, not continue:
    used += width; chosen += index          // the tabs stay the LEADING ones
if chosen is empty: chosen += 0             // never collapse to a bare `…`
chosen.sort()
```

Three properties this was chosen for:

* **No index is special-cased.** The active member pays for its tab out of the
  same pixels as every other tab. When it already fits in the prefix — the
  common case — the output is byte-for-byte the unreserved split
  (`fit_tests::an_active_member_that_already_fits_changes_nothing`).
* **The painted set is a subsequence, never a permutation.** Sorting the chosen
  indices is what makes "in place" true; dropping the sort is mutation M3 below
  and it kills four tests.
* **The cost is one tab at the fold**, which drops into the `…`. That is the
  expected trade the task named: "dropping a different tab into the overflow to
  make room is expected; shuffling the order is not."

The pre-measurement branch (`measured_bounds == None`, the very first frame)
does the same thing coarsely: the leading `UNMEASURED_VISIBLE_TABS`, plus the
active index if it is not already in there, sorted.

Consequence worth recording: **the check mark in the `…` menu is now
unreachable at steady state.** The reservation guarantees the active member is
on the strip, so it is never in the overflow list. The check is kept — it is
still live on the pre-measurement frame and it names what the menu is showing —
but it is no longer the thing that answers "which project am I on". It is
covered by two direct-render tests rather than by pretending the menu can show
it.

## Anything else that reorders on selection?

No. `reorder_members` has exactly four call sites in the UI, and after this
change every one of them is a drag or an explicit API call:

| Call site | Gesture |
|---|---|
| `project_tab.rs` `on_drop` | tab-to-tab drag |
| `project_tab_strip.rs` end-drop zone | drag past the last tab |
| `solutions/src/mcp/member_mgmt.rs` | the `solutions.reorder_members` MCP tool |
| ~~`project_tab_strip.rs` menu click~~ | **removed by this change** |

The other selection surfaces (`solution_picker_dropdown`, `switch`,
`add_member_picker`, the tab's own `on_click`) only call `set_active_member` /
open a window. Nothing else had the same one-line shape.

## Tests

Rewritten (not deleted) from `cc05f6ef6d`:

* `picking_a_hidden_project_from_the_menu_puts_it_on_the_strip` →
  **`picking_a_project_from_the_overflow_menu_activates_it_without_reordering`**.
  Clicks a real row in a real menu, then asserts on the painted frame: the
  picked project paints `PROJECT-TAB-ACTIVE-*` and not `PROJECT-TAB-INACTIVE-*`;
  the project we came from paints `INACTIVE` and not `ACTIVE`; the stored member
  order read back from `SolutionStore` is `assert_eq!`-identical to what it was;
  the head of the strip did not move; the painted tabs are a subsequence of the
  stored order.
* `the_overflow_menu_marks_the_active_project_and_only_that_one` →
  **`the_active_project_is_painted_in_place_even_when_it_sits_past_the_fold`**.
  Activates the **last** of twelve members (the furthest possible from the fold)
  via `set_active_member`, then asserts it paints, paints *active*, paints at
  the **right end** of the strip (not promoted to the head), the leading tab is
  unchanged, the relative order holds, the stored order is unchanged, and it is
  no longer listed in the `…` menu while a genuinely hidden project still is.
  That last pair is what the deleted "marks the active project" assertions
  become: the new rule makes "the active project is in the menu" the thing that
  must *not* happen.
* The menu-mark assertions themselves were the only coverage of
  `overflow_menu_row`'s check. Rather than lose it, two new tests render the row
  directly, both sides: **`an_overflow_row_paints_the_active_mark`** and
  **`an_overflow_row_paints_unmarked_when_it_is_not_active`**.
* **`a_project_can_be_dragged_out_of_the_overflow_menu_onto_the_strip`** keeps
  its painted-position assertions and gains the stored-order half: `assert_ne!`
  against the order before, and `assert_eq!` against the exact expected order
  (dragged member takes the drop target's slot). Drag reorders; click does not —
  the two tests now state that contrast against the same store.

`fit_tests` rewritten for `visible_indices`: the four old boundary cases
(everything fits / the `…` is paid first / narrower than one tab / no members)
plus `the_active_member_is_reserved_a_slot_past_the_fold`,
`an_active_member_that_already_fits_changes_nothing`,
`an_active_member_wider_than_the_whole_budget_is_still_painted`,
`an_out_of_range_active_index_is_ignored`.

Unchanged and still green: the three width-budget paint tests from
`cc05f6ef6d` (`a_wide_strip_…`, `a_narrow_strip_…`,
`tabs_lay_out_at_their_predicted_width`).

### Mutation table

Every mutation was applied to the working tree, run, and reverted (`cp` of the
pristine file back over it; the final `git diff --stat` and a clean
`cargo test -p solutions_ui -p solutions -p title_bar` confirm nothing survived).

| # | Mutation | Applied | Run | Result | Reverted |
|---|---|---|---|---|---|
| M1 | Restore promote-on-click: the menu callback rebuilds the order with the picked member first and calls `reorder_members` before `set_active_member` | yes | `cargo test -p solutions_ui project_tab_strip` | **1 failed** — `picking_a_project_from_the_overflow_menu_activates_it_without_reordering`, on the exact assertion "selecting a project from the overflow menu must not reorder the members" | yes |
| M2 | Drop the reservation: `let active: Option<usize> = None;` inside `visible_indices` | yes | same | **4 failed** — `the_active_member_is_reserved_a_slot_past_the_fold`, `an_active_member_wider_than_the_whole_budget_is_still_painted`, `the_active_project_is_painted_in_place_even_when_it_sits_past_the_fold`, `picking_a_project_…_without_reordering` | yes |
| M3 | Turn the reservation into a promotion: delete `chosen.sort_unstable()` so the active member paints first | yes | same | **4 failed** — `an_active_member_that_already_fits_changes_nothing`, `the_active_member_is_reserved_a_slot_past_the_fold`, `the_active_project_is_painted_in_place_…`, `picking_a_project_…_without_reordering` | yes |
| M4 | Make the drag stop reordering: gut the tab's `on_drop` body | yes | same | **1 failed** — `a_project_can_be_dragged_out_of_the_overflow_menu_onto_the_strip` | yes |

M1 is the shipped regression; M3 is the tempting wrong fix (pin the active tab
to the front of the *paint* instead of reserving it in place) — worth noting
that it is caught, since it looks correct in a screenshot of a single click.

## Verification

* `CARGO_BUILD_JOBS=4 cargo build --bin sawe` — clean (`Finished dev profile`,
  no `^error`).
* `CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets` — exit 0, **zero**
  `^error`, **zero** `^warning`.
* `CARGO_BUILD_JOBS=4 cargo test -p solutions_ui -p solutions -p title_bar` —
  234 + 66 + 9 passed, 0 failed.

### Live check

`script/run-mcp --debug --headless --runtime-dir /tmp/overflow-click-probe`, a
fresh Solution with twelve empty members, window narrowed to 1100x800 with
`windows.resize`. Every screenshot was taken after driving a real event
(`workspace.screenshot` renders the retained scene).

| Screenshot | What it shows |
|---|---|
| `2026-09-04-overflow-click-before.png` | Strip: `ecos-base`(active) `ecos-records` `citeck-community` `ecos-webapp` `ecos-model` `…` |
| `2026-09-04-overflow-click-menu.png` | The `…` menu: the seven spilled projects, `ecos-uiserv` … `ecos-bpmn` |
| `2026-09-04-overflow-click-after.png` | After clicking **`ecos-bpmn`** — the *last* member of twelve. It is painted, highlighted, and **at the right end** of the tabs. The four leading neighbours (`ecos-base`, `ecos-records`, `citeck-community`, `ecos-webapp`) have not moved; `ecos-model`, the tab at the fold, dropped into the `…`. `solutions.get` before and after the click returns the **identical twelve-member order** (asserted in the probe: `ORDER UNCHANGED: True`). |
| `2026-09-04-overflow-click-drag.png` | After **dragging** `ecos-history` out of the menu onto the second tab: the stored order really changes to `ecos-base, ecos-history, ecos-records, citeck-community, …`, and `ecos-bpmn` is still the painted, active, right-most tab. |

Reopening the menu after the click also confirms the other half: `ecos-bpmn` is
gone from the list and `ecos-model` has taken its place at the top.

MCP gotcha worth remembering: driving a `ContextMenu` row live needs a
`windows.hover_at` on the row *before* `windows.click_at`/`windows.drag_at` — a
bare click at the row's coordinates dismissed the menu without invoking the
entry (twice), and a bare `drag_at` from an un-hovered row produced no reorder.

## FORK.md

**Do not edit `FORK.md` in this change.** Entry **#146** currently documents the
promote-on-click behaviour. It should now read something like:

> **#146 — The project strip's `…` menu selects; it does not reorder.** Clicking
> an overflow row calls `set_active_member` only. The picked project is still
> guaranteed to appear on the strip because the width budget
> (`solutions_ui::project_tab_strip::visible_indices`) *reserves* the active
> member's width before filling the strip greedily from the front, so the active
> tab is painted in its stored position — one tab at the fold drops into the `…`
> instead. Promoting the picked project to the head of the member order (the
> original shape) was wrong: selecting is navigation and must not persist a new
> tab arrangement. Dragging a row out of the menu onto the strip remains the
> deliberate reordering gesture.
