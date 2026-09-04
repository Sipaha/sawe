# The Commit tab's ref row: one height at rest, growth only on request

**Status:** shipped on `main`. One commit, one file (`crates/git_ui/src/git_panel/commit_tab.rs`).

## The defect

`1a73d7d001` made the row paint every ref, wrapped, capped at
`COMMIT_CONTAINMENT_EXPANDED_MAX_HEIGHT` (64px) and scrolling past the cap. That fixed a real
bug — a tag folded behind `+N` disappeared from the pane entirely, because `uncharted_tags`
subtracts against the whole decoration list — but it made the row's height a function of the
selected commit's ref count. Walking the log then moved everything the row is stacked with:
measured live below, a 9-ref commit ate **42px** more of the changed-files tree than a 1-ref
commit, purely from the selection. «Самопроизвольные скачки тут только все портят.»

## What shipped

The row now has two states, and only the user moves it between them — the same mechanism, cap,
affordance and wording the two containment rows below it already use:

- **At rest (every selection starts here).** One line. The chips container is a nowrap
  `h_flex` with `overflow_hidden`; every chip is still built with `truncate: true`, so a narrow
  pane shortens each name instead of dropping any. The row's height is therefore one chip line
  whether the commit carries one ref or nine.
- **Expanded, after `Show all`.** The container gains `flex_wrap()`,
  `max_h(COMMIT_CONTAINMENT_EXPANDED_MAX_HEIGHT)` and `overflow_y_scroll()` — literally the
  layout `1a73d7d001` shipped, now behind a click. `Show less` puts it back.

State lives in `CommitTabState::refs_expanded`, beside `branches_expanded` / `tags_expanded`, so
`CommitTabState::new` collapses it for every new selection: an expansion left behind on one
commit cannot set the height of the next one.

Two structural details worth keeping:

- **The toggle is outside the scrolling block**, for the reason
  `render_commit_containment_line` gives: `Show less` must stay reachable without scrolling back
  up to it.
- **The chips container carries `min_h(ButtonSize::Default.rems())`.** The toggle is hidden for
  a single ref (nothing a second line could add — the containment rows hide theirs on the same
  principle), and a button that comes and goes with the ref count would make the row's height
  follow the ref count again by another route. The floor is in rems, not pixels, so it tracks
  the UI font exactly as the button does. It cannot be hidden on *fit* instead: chip widths
  depend on a panel width the user drags, so "does this fit" is knowable only during layout, and
  a notify from there is dropped (`Window::invalidate_view` mid-draw).

**Nothing became unreachable.** The chip *list* is the whole decoration list in both states —
collapsing changes the layout, never the list — so `uncharted_tags`' subtraction against the
whole list stays sound, unchanged, and its doc comment now says exactly that: dropping names
from the *list* is what made a tag vanish, and a cap on the list still has to hand
`uncharted_tags` the painted slice.

## Tests (`cargo test -p git_ui`: 405 passed, 0 failed)

| Test | What it pins |
|---|---|
| `test_the_commit_tab_paints_every_ref_pointing_at_the_commit` *(kept, extended)* | Its meaning is now **the list, not the layout**: at a `compact_refs_threshold` of 2, all three decorations are chips and there is no `+N`; additionally the three sit on **one line** at rest and the row offers `Show all`. That is the property `uncharted_tags` subtracts against — a collapsed line may be too narrow to spell a name out, but no name is dropped from the row. The undecorated-commit half (no row at all) is unchanged. |
| `test_the_commit_tab_ref_row_rests_at_one_height_for_every_commit` *(new)* | The regression itself. One ref vs nine: `COMMIT-TAB-REFS`'s painted **height and bottom edge are equal** (an equality, not a literal — the number is a function of UI font and chip metrics). Then `Show all` makes it taller (growth is the user's), and selecting the next commit brings it back to the resting height (`refs_expanded` reset). |
| `test_the_commit_tab_ref_row_wraps_into_a_capped_scroll_box_when_expanded` *(rewrite of `…wraps_a_long_ref_row_into_a_capped_scroll_box`)* | Twelve refs. At rest: a chip painted for every one of them (loop over 12 static selectors) and all twelve on the first line. After a real click on the painted `Show all`: the third chip wraps to a second line at the row's left edge, the chips block is exactly `COMMIT_CONTAINMENT_EXPANDED_MAX_HEIGHT`, and the twelfth chip is painted **wider** than at rest — the expand is what gives it the width to say its name. `Show less` returns it to the one line. |

All assertions read the painted tree (`VisualTestContext::debug_bounds`) after a real frame, and
the toggle is driven by `cx.simulate_click` on the painted button rather than by flipping
`refs_expanded` behind its back.

### Mutation table

| # | Mutation applied | Result | Reverted |
|---|---|---|---|
| 1 | Drop `min_h(ButtonSize::Default.rems())` from the chips container | **SURVIVED** — see note | yes |
| 2 | Collapsed wraps too (`if true { flex_wrap() … }`) | **FAILED** — `rests_at_one_height` (24px vs 69px) and `wraps_into_a_capped_scroll_box` ("all twelve rest on the one line") | yes |
| 3 | `let expanded = false && state.refs_expanded` (the expand never expands) | **FAILED** — both new tests | yes |
| 4 | `names.iter().take(2)` (a cap on the list again) | **FAILED** — all three tests, incl. the every-ref list guard | yes |
| 5 | `refs_expanded: true` in `CommitTabState::new` (a selection starts expanded) | **FAILED** — both new tests | yes |
| 6 | Drop the `names.is_empty() → None` guard | **FAILED** — "a commit no ref points at gets no row at all" | yes |
| 7 | Never paint the toggle | **FAILED** — all three tests (`click_ref_row_toggle` finds no button; the every-ref test loses its `Show all` assertion) | yes |

**On the surviving mutant (1).** In the test's metrics the chip line and the button both paint
19px, so removing the floor leaves the two rows equal by coincidence rather than by
construction. The floor is what makes the equality hold when they diverge (a different UI font
size, a chip that grows an icon). It is deliberate insurance, documented as such at the call
site; I did not add an assertion that would only restate the constant.

## Live check

`script/run-mcp --debug --headless --runtime-dir /tmp/refs-height-probe`, a throwaway clone of
this repo as the single Solution member, with extra refs created **in the clone** so one commit
carries nine (`HEAD -> main`, two tags, `origin/main`, `origin/HEAD`, four branches), the next
carries one (`only-one`), and the one below carries none.

Measured from the PNGs: the y of the rule at the top of the message block, i.e. **the bottom
edge of the changed-files tree**.

| Selection | Rule y | Ref-chip band |
|---|---|---|
| 9 refs, at rest | **441** | 673–693 (20px, one line) |
| 1 ref, at rest | **438** | 670–690 (20px, one line) |
| no refs | 466 | — (no row at all) |
| 9 refs, after `Show all` | **399** | 631–694 (63px ≈ the 64px cap, third line clipped and scrolling) |

Reading the shots myself:

- `2026-09-04-refs-height-nine-collapsed.png` — nine chips squeezed onto one line
  (`✓…`, `t…`, `t…`, `…`, `r…`, `r…`, `h…`, `fea…`) with `Show all` at the right. Legibility is
  poor at nine refs; the height is not.
- `2026-09-04-refs-height-nine-expanded.png` — the same commit after the click: full names,
  three wrapped lines, the third one cut by the 64px cap (it scrolls), `Show less` at the right,
  outside the scroll box.
- `2026-09-04-refs-height-one-ref.png` — a single `only-one` chip, no toggle, same 20px band.
- `2026-09-04-refs-height-no-refs.png` — identity line straight into `In 6 branches: …`; no ref
  row, no padding spent.

**Does the tree stay put walking the log?** Between the 9-ref and the 1-ref commit it moves
**3px** (441 → 438), and that 3px is not this row: it is the *containment* line below, which
gains a `Show all` button (19px) in place of a plain label when the commit is in more than five
branches. Before this change the same two selections differed by **42px** (441 vs 399 — the
expanded state *is* structurally what `1a73d7d001` painted automatically). A commit with no refs
still drops the row entirely (466), which is deliberate and unchanged: an empty row would spend
its padding for nothing.

Residual, out of scope and not touched here: that 3px belongs to
`render_commit_containment_line` — its own height depends on whether its toggle is present, i.e.
on the selected commit's branch count. Worth the same treatment if the maintainer sees it.

## Replacement wording for FORK.md #139 (not applied — do not edit `FORK.md` from here)

Replace the bullet that currently begins **"The row paints every ref — wrapped, capped and
scrolling — and applies no threshold."** with:

> - **The row paints every ref and applies no threshold — on one line at rest, wrapped only when
>   the user asks.** *(Amended twice. It first read "the row never wraps", folding the overflow
>   into the graph's `+N` chip at `git.log.compact_refs_threshold`; `1a73d7d001` replaced that
>   with an always-wrapped, capped, scrolling row; `acc78db74e` kept the list and put the wrapping
>   behind an explicit expand. Both amendments are stated rather than quietly overwritten,
>   because the reasoning that produced each is the reasoning a future reader will re-derive.)*
>   Two things were wrong with the threshold. A pane whose job is to answer "which branch is this
>   commit on" must not fold the answer into a tooltip — folding is defensible on a log row,
>   which is a list, and not on the detail pane the list points at. And applying it
>   *unconditionally* made the pane disagree with the graph row a few pixels below it: the graph
>   caps only under its own `compact_refs` view toggle, which `ViewOptions::default()` leaves
>   **off**. But the answer `1a73d7d001` gave — bound the *row* instead of the list, always —
>   bought that at the price of a height that follows the selected commit's ref count, and the
>   row is stacked directly against the changed-files tree: walking the log moved the tree by
>   42px between a nine-ref commit and a one-ref one. Self-inflicted jumps in this pane are worse
>   than a truncated chip. So the row rests on **one line** (`overflow_hidden`, chips built with
>   `truncate: true`, so a narrow pane shortens every name rather than hiding any) and expands on
>   **`Show all`** into exactly the previous layout — `flex_wrap()` +
>   `max_h(COMMIT_CONTAINMENT_EXPANDED_MAX_HEIGHT)` (64px) + `overflow_y_scroll()`. That is the
>   same mechanism, cap, affordance and wording the two containment rows below already use, and
>   `refs_expanded` lives on `CommitTabState`, so every new selection comes up collapsed. The
>   toggle sits outside the scrolling block (`Show less` must stay reachable) and is hidden for a
>   single ref; the chips container therefore carries a `min_h(ButtonSize::Default.rems())` floor,
>   so a row *with* a button measures the same as a row *without* one. It cannot be hidden on fit
>   instead: chip widths depend on a user-dragged panel width, so "does this fit" is knowable
>   only during layout, and a notify from there is dropped mid-draw.
>   `commit_refs::overflow_chip` still exists and is now the graph's alone.

And in the `uncharted_tags` bullet, replace the sentence **"`uncharted_tags` subtracts against
the whole decoration list, which is only sound because the row now paints the whole list."**
with:

> `uncharted_tags` subtracts against the **whole** decoration list, which is only sound because
> the row builds a chip for every name on it *in both of its states* — collapsing changes that
> row's layout, never its list, and `Show all` is what gives a squeezed chip room to spell its
> name. While the row applied the threshold it dropped names from the **list**, and a tag past
> the threshold was subtracted here *and* folded into a `+N` chip there, so its name appeared
> nowhere in the pane but a tooltip — the bug `1a73d7d001` fixed. If a cap on the *list* ever
> returns to that row, `uncharted_tags` must be given the painted slice instead of the full list,
> in the same commit.

## Concerns

- Nine refs on one line is nine ellipsis stubs. That is the maintainer's own stated bargain
  ("a narrow pane shortens each name rather than hiding any") and the brief said to keep per-chip
  truncation, so I kept it. The alternative — `truncate: false` collapsed, letting the first two
  or three chips stay readable and hard-clipping the rest at the row's edge — reads better at
  nine refs and worse at one long ref (a hard cut with no ellipsis, and no toggle to escape to,
  since a single ref paints none). Say the word and it is a one-argument change.
- The 64px cap is not a whole number of chip lines (19px + 4px gap), so the expanded block clips
  its last visible line mid-height. That is inherited from the containment rows, which share the
  constant.

---

# Follow-up: the collapsed row fits whole names and counts the rest

**Ruling** (coordinator, on the nine-ref screenshot above): *«A row that fits everything by
making every name unreadable communicates less than one that shows three names and says there
are six more.»* Collapsed must fit as many chips as the width allows, each with its **full
name**; per-chip truncation drops back to being the last-resort backstop it was written as; and
the toggle must say how many are hidden. Expanded behaviour unchanged. Priorities: no ref name
unreachable > constant height > number of chips visible.

**Status:** shipped on `main`, second commit. Files: `crates/git_ui/src/git_panel/commit_tab.rs`,
`crates/git_ui/src/commit_refs.rs`, `crates/git_ui/src/git_panel.rs` (one field).

## What it does now

The collapsed row is a measured fit, built on the pattern this fork already uses for the project
strip (`solutions_ui::project_tab_strip`, `cc05f6ef6d`), because that is the same problem:

1. **Measure.** A `canvas` covering the row reports its box into
   `GitPanel::commit_refs_row_width` and hops the notify out of the draw with a guarded
   `cx.defer` (`Window::invalidate_view` throws away a notify raised mid-draw). Safe to feed back
   into `render` because the measured quantity does not depend on the decision it drives: the row
   is `w_full`, so its width is the panel's however many chips end up inside it. The field lives
   on `GitPanel`, not `CommitTabState`, so walking the log does not re-enter the unmeasured
   state once per commit.
2. **Predict.** `commit_refs::ref_chip_width` — new, next to `ref_chip`, sharing one
   `chip_glyph` decision with it so a chip and its prediction cannot disagree about the check /
   lock glyph. Widths are shaped with the chip's real font (`Label::buffer_font`, `TextSize::Small`)
   and the box metrics are read in **rems**, so the budget does not lie at a non-default UI font
   size. Same discipline as `project_tab::tab_width_for_label`.
3. **Fit.** `ref_chips_that_fit(widths, gap, budget, toggle)` — a pure function, a greedy
   **prefix**, with the toggle's width charged to the budget only when something actually spills,
   `REF_ROW_BUDGET_SAFETY_MARGIN` (4px) of slack, and a floor of one chip. No parameter says
   which ref is interesting, so nothing about the selection can reshuffle which chips are on
   screen.
4. **Paint.** The chips that fit, whole, plus a `Show N more` button; `Show less` on the way
   back. `Chip::truncate` stays on permanently but is inert while the prefix fits (nothing
   overflows, so flexbox never shrinks anything); it fires only where the fit can promise
   nothing — the single ref wider than the whole row, and the one pre-measurement frame.

**Wording:** `Show 7 more` collapsed, `Show less` expanded. `Show less` is verbatim the
containment rows' word, so the pair still reads as one family; the collapsed label carries the
count because, unlike `In 12 branches: …`, a chip row cut at the width states no total anywhere
else, and `Show all` would leave "all of what?" unanswered. `commit_refs::overflow_chip` (`+N`)
stays the graph's: a chip that is secretly clickable is a weaker affordance than a labelled
button, and a count whose names live only in a tooltip is the shape of the original bug.

## The invariant `uncharted_tags` now states

> **A tag may only be subtracted from the tag row by a chip the user can see.**

`uncharted_tags(tags, ref_names)` takes the decorations the ref row **painted this frame** — the
prefix when collapsed, all of them when expanded — never the commit's whole decoration list.
`GitPanel::ref_row_fit` computes that count once per frame, above the section loop, and
`render_commit_tab` hands the same slice to both the ref row and the tag row, so the two cannot
derive it separately. Subtract against the full list instead and a tag past the fold is
suppressed on the tag row *and* unpainted on the chip row — `1a73d7d001`'s bug in a new costume.

Live consequence, visible in the screenshots below: collapsed, `tag: probe-2.41.0` is behind
`Show 7 more`, so the tag row picks it up and paints `probe-2.41.0`; expanded, it is a chip, so
the tag row stands down rather than saying the same thing twice a few pixels lower. The name is
on screen in **both** states; only which row carries it changes.

## Tests

`cargo test -p git_ui` → **408 passed, 0 failed** (was 405; +3 net).

| Test | What it pins |
|---|---|
| `test_the_commit_tab_paints_every_ref_pointing_at_the_commit` *(kept)* | Three short refs at `compact_refs_threshold = 2`: all three painted, no `+N`, all on one line, **`origin/main` painted within 1px of `commit_refs::ref_chip_width`'s prediction** (whole name, and the budget's arithmetic pinned against the paint the way `tabs_lay_out_at_their_predicted_width` does), and **no toggle at all** — nothing folded, so no control offering to show what is not missing. Undecorated commit still gets no row. |
| `test_the_commit_tab_ref_row_rests_at_one_height_for_every_commit` *(kept)* | Unchanged contract: one ref vs nine, equal height and equal bottom edge; expanding grows it; the next selection collapses it again. |
| `test_the_commit_tab_ref_row_counts_the_refs_that_do_not_fit` *(replaces `…wraps_into_a_capped_scroll_box_when_expanded`)* | Twelve refs. The painted chips are a greedy **prefix**; they are all on one line; the first is painted at its full predicted width; the button is looked up **by its label**, so `Show {12−painted} more` is a painted assertion of the count; after the click all twelve are painted, wrapped, and the block is exactly the 64px cap; `Show less` restores exactly the prefix. |
| `test_a_tag_past_the_fold_stays_on_the_tag_row` *(new)* | The coupling, both sides, painted: with the tag behind the fold, `CHIP-tag: 2.41.0` is absent and `COMMIT-TAB-TAGS` is present; after expanding, the chip is present and the tag row is gone. |
| `test_a_single_ref_wider_than_the_row_ellipsizes_inside_it` *(new)* | The backstop: one ref wider than the whole row stays **inside** the row (`chip.right() <= row.right()`) instead of being cut off mid-word, and the row still rests at the same height. |
| `test_ref_chips_that_fit` *(new, pure)* | The fold arithmetic: exact fit; one pixel short; a budget that would hold two chips if the toggle were free (it is not); a row too narrow for even one chip → still one; empty list → zero. |

### Mutation table (second round)

| # | Mutation applied | Result | Reverted |
|---|---|---|---|
| 8 | `uncharted_tags(tags, &state.selection.refs.names)` — subtract against the whole list again | **FAILED** — `test_a_tag_past_the_fold_stays_on_the_tag_row` | yes |
| 9 | `fit.truncate` → `true` (chips always truncatable) | **SURVIVED** — see note; the flag was then deleted | n/a, folded into the fix |
| 10 | `names.iter().take(painted)` → all names (paint every chip on the line) | **FAILED** — the fold test, the height test and the tag-coupling test | yes |
| 11 | `ref_row_fit` ignores the measurement and returns `names.len()` | **FAILED** — same three | yes |
| 12 | `Show {hidden + 1} more` (the toggle miscounts) | **FAILED** — all three tests that click it by label | yes |
| 13 | `let budget = budget - toggle - gap` → `budget` (the fit forgets its own toggle) | **FAILED** — `test_ref_chips_that_fit` and the fold test | yes |
| 14 | Collapsed toggle never rendered (no way back to the folded refs) | **FAILED** — all three | yes |
| 15 | `truncate: false` on the chips (backstop removed) | **FAILED** — `test_a_single_ref_wider_than_the_row_ellipsizes_inside_it` | yes |
| 2′ | Collapsed wraps too (`if true { flex_wrap() … }`) | **SURVIVED** — see note | yes |
| 5′ | `refs_expanded: true` in `CommitTabState::new` | **FAILED** — four tests | yes |
| 6′ | Drop the `names.is_empty() → None` guard | **FAILED** — the every-ref test | yes |

**Survivor 9 was a design signal, not a gap.** With the fit painting only a prefix that fits,
`truncate` is unobservable: nothing overflows, so flexbox never shrinks anything. The
`RefRowFit { painted, truncate }` struct was therefore carrying a flag with no behaviour, and
`truncate: true` is also the *more* forgiving failure mode if a prediction is ever slightly off
(a fraction of a pixel of shrink beats a name clipped mid-word). So the flag was deleted, the
fit now returns a plain `usize`, and the chips are built with `truncate: true` unconditionally —
mutation 15 is what guards that.

**Survivor 2′ is defensive and currently untestable.** `overflow_hidden` + nowrap is the guard
against a *lying* budget: if a chip ever shapes wider than predicted, a wrapping row would grow a
second line and the ref count would be setting the height again. Tests cannot construct that lie
— the canvas re-measures and corrects any width I plant within the same `run_until_parked` —
so the mutation is invisible to them. Left in with the reasoning at the call site.

## Verification commands

```
$ CARGO_BUILD_JOBS=4 cargo build --bin sawe
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1m 01s
    errors: 0 warnings: 0

$ CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 26.77s
    errors+warnings: 0

$ CARGO_BUILD_JOBS=4 cargo test -p git_ui
    test result: ok. 408 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## Live check (second probe)

`script/run-mcp --debug --headless --runtime-dir /tmp/refs-height-probe2`, a throwaway clone
whose HEAD commit carries **nine** decorations (`HEAD -> main`, two tags, four
`origin/…` branches, `origin/main`, `origin/HEAD`), the next carries one, the one below none.

| Selection | Rule y (top of the message block = bottom edge of the changed-files tree) | Ref-chip band |
|---|---|---|
| 9 refs, at rest | 416 | 648–668 (20px, one line) |
| 1 ref, at rest | 441 | 673–693 (20px, one line) |
| no refs | 469 | — (no row) |
| 9 refs, expanded | 399 | 631–~694 (the 64px cap, scrolling) |

**What the collapsed row actually says now** (`2026-09-04-refs-height-fold-nine-collapsed.png`,
read directly): `✓ HEAD -> main` and `tag: probe-2.41.1` — both **whole, legible, at natural
width** — then `Show 7 more` in accent blue. Directly below, the tag row prints the bookmark
glyph and `probe-2.41.0`: that is the tag that fell behind the fold, and the tag row is where it
surfaces. Then `In 1 branch: main`. Compare the previous shot, where the same commit showed
`✓…  t…  t…  …  r…  r…  h…  fea…` and the tag row was silent.

`2026-09-04-refs-height-fold-nine-expanded.png`: after the click — `✓ HEAD -> main`,
`tag: probe-2.41.1`, `tag: probe-2.41.0`, `origin/release/2.42` … wrapped over three lines, the
third clipped by the 64px cap (it scrolls), `Show less` at the right, **and the tag row gone**,
because `probe-2.41.0` is now a chip. `2026-09-04-refs-height-fold-one-ref.png`: a single
`origin/only-one` chip, no toggle, the same 20px band.

**Does the tree stay put walking the log?** The ref row itself is a 20px band for every commit —
that part is exact and is what the height test asserts. The pane below it moved 25px between
the nine-ref commit (rule 416) and the one-ref commit (rule 441), and that is the **tag row**,
not the ref row: the nine-ref commit is tagged, one of its tags is behind the fold, so the tag
row paints a line the untagged commit does not have. That is the ruling's own priority order
working as ordered — reachability first, constant height second — and it is the mechanism that
keeps `probe-2.41.0` on screen at all. A commit with no decorations still drops the ref row
entirely (469), deliberate and unchanged.

## Revised replacement wording for FORK.md #139 (supersedes the block above; still not applied)

Replace the bullet beginning **"The row paints every ref — wrapped, capped and scrolling — and
applies no threshold."** with:

> - **The row keeps every ref reachable and applies no threshold, but only paints what fits: whole
>   names on one line, `Show N more` for the rest.** *(Amended twice. It first read "the row never
>   wraps", folding the overflow into the graph's `+N` chip at `git.log.compact_refs_threshold`;
>   `1a73d7d001` replaced that with an always-wrapped, capped, scrolling row; `43ba41c05a` made the
>   collapsed row a measured fit with a counted expand. Each amendment is stated rather than
>   quietly overwritten, because the reasoning that produced it is the reasoning a future reader
>   will re-derive.)* The threshold was wrong twice over: a pane whose job is to answer "which
>   branch is this commit on" must not fold the answer into a tooltip, and applying it
>   *unconditionally* made the pane disagree with the graph row a few pixels below, which caps only
>   under its own `compact_refs` view toggle (`ViewOptions::default()` leaves it **off**). But
>   bounding the *row* instead — wrap, cap, scroll, always — tied its height to the selected
>   commit's ref count, and the row is stacked against the changed-files tree: walking the log
>   moved the tree 42px between a nine-ref commit and a one-ref one. And simply squeezing every
>   chip onto one line answers even less than `+N` did: nine refs became `t…`, `r…`, `fea…`. So the
>   collapsed row **measures**: a `canvas` reports its width into `GitPanel::commit_refs_row_width`
>   (with the mandatory guarded `cx.defer` notify — a notify raised mid-draw is discarded),
>   `commit_refs::ref_chip_width` predicts each chip from the chip's own metrics in rems, and
>   `ref_chips_that_fit` takes a greedy prefix, charging the toggle to the budget only when
>   something spills. What is left goes behind `Show N more`; `Show less` returns. Expanded is the
>   `1a73d7d001` layout — `flex_wrap()` + `max_h(COMMIT_CONTAINMENT_EXPANDED_MAX_HEIGHT)` +
>   `overflow_y_scroll()` — now reached only by a click. `refs_expanded` lives on `CommitTabState`,
>   so every new selection comes up collapsed; the chip block carries a `min_h(ButtonSize::Default)`
>   floor so a row with a toggle measures the same as one without; and `Chip::truncate` stays on as
>   a backstop that fires only for a single ref wider than the whole row. This mirrors
>   `solutions_ui::project_tab_strip` deliberately — same measurement, same greedy prefix, same
>   safety margin — because it is the same problem, and `commit_refs::overflow_chip` (`+N`) is now
>   the graph's alone.

And in the `uncharted_tags` bullet, replace the sentence **"`uncharted_tags` subtracts against
the whole decoration list, which is only sound because the row now paints the whole list."**
with:

> `uncharted_tags` takes the decorations the ref row **painted this frame** — the fitted prefix
> when collapsed, all of them when expanded — never the commit's whole decoration list. The
> invariant is *a tag may only be subtracted from the tag row by a chip the user can see*:
> `GitPanel::ref_row_fit` computes that count once, above the section loop, and the same slice
> reaches both rows, so the ref row and the tag row cannot derive it separately. Subtracting
> against the full list suppresses the tag row for a tag that is behind `Show N more` — the same
> name-lost-to-a-fold bug `1a73d7d001` fixed when the fold was a `+N` chip's tooltip.
> `test_a_tag_past_the_fold_stays_on_the_tag_row` is the guard, and it asserts both sides: the
> collapsed pane carries the name on the tag row, the expanded pane carries it as a chip and the
> tag row stands down.

## Concerns (updated)

- The tag row's *presence* now depends on where the ref row's fold lands (see the 25px above).
  That is reachability winning over constant height, as ruled — but it means dragging the panel
  narrower can make the tag row appear. Nothing is lost either way; it is a row appearing, never
  a name disappearing.
- Two chips of the nine fit at the shipped panel width. If that reads as too few, the lever is
  the panel width itself or a shorter chip label — not the fold, which is now honest about what
  it hides.
- `ref_chip_width` calls `solutions::branch_protection::check` once per chip per frame, the same
  call `ref_chip` already makes, so a decorated commit now pays it twice per chip per frame. It
  is a policy lookup, not I/O; if it ever shows up in a profile the fix is to hoist the glyph
  decision into the row and pass it to both.
- The 64px cap is still not a whole number of chip lines, so the expanded block clips its last
  visible line mid-height. Inherited from the containment rows, which share the constant.
