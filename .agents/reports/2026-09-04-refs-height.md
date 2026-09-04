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
