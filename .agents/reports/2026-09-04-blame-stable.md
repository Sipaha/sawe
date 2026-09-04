# The blame gutter stops moving: run heads are found in display space, not in the viewport

Date: 2026-09-04 · Crates: `editor` (`src/git/blame.rs`, `src/element.rs`), `git_ui`
(`src/blame_ui.rs`, `src/solo_diff_view.rs`) · Branch: `main`

Two self-moving elements are gone from the blame gutter, both consequences of one thing —
the run classification could only see the rows it was about to draw:

1. **The viewport-sticky head (R2).** A run that started above the visible rows had its date
   and author reprinted on whatever row the scroll left on top, and the label slid down the
   gutter as the user scrolled. Now the label stays on the line the run really starts on: the
   visible tail of a scrolled-through run is blank, the way IntelliJ leaves it.
2. **The `ix > 0` hairline exemption.** The separator was suppressed on viewport row 0, so a
   genuine boundary's hairline vanished when that row reached the top edge and came back one
   row later — a flicker while scrolling. Now the hairline is drawn wherever a run really
   begins. The exemption's real motivation is kept, in display coordinates instead of viewport
   ones: the very first row of the display draws no hairline, because there is nothing above it
   to be separated from.

## The seam

`blame_run_positions` keeps the whole rule and gains two things: it is **seeded** with what sits
above its slice, and it **reports** what it leaves for the rows below.

```rust
pub enum BlameRunPredecessor { DisplayStart, Severed, Blamed { buffer_id, sha, buffer_row } }

pub fn blame_run_positions(rows, blamed_rows, alignment_rows, predecessor)
    -> (Vec<Option<BlameRunPosition>>, BlameRunPredecessor)
```

Because the seed and the report are the same type, classifying the rows above a slice and
classifying the slice **compose**: `GitBlame::run_predecessor_above` runs the rows above the
viewport through *the same function* and hands the result to
`GitBlame::run_positions_in_viewport`. There is no second reading of "what severs a run" — which
block row cuts, which spacer does not, which wrap does not, what identity survives to the next
row are all decided in exactly one place, and a future clause added to the rule is automatically
in force above the viewport too.

The scan is adaptive rather than fixed-depth. One row above settles the question in the ordinary
case; it does not when that row is a soft-wrap continuation or an alignment spacer, which stand
for nothing in this buffer. So the scan starts at one row and **doubles until the classification
of the scanned window comes back as anything other than `DisplayStart`** — i.e. until the rows
above answer the question — capped at `MAX_RUN_PREDECESSOR_LOOKBACK = 1024` display rows, beyond
which it reads as `DisplayStart` (a labelled head with no hairline, which is exactly what the
top row drew before it could see above itself at all). The stop condition is the classifier's own
output, not a second predicate.

`BlameRunPosition` gained a third variant, `DocumentHead`: a head with nothing above it. That is
what lets `git_ui` draw the hairline from the position alone (`Head` → boundary, `DocumentHead` →
none) instead of re-deriving "am I on the top row" from `ix`, which is precisely the
viewport-relative reasoning being removed. `run_separator` is now `matches!(position, Head)`.

`nothing_above` — the flag behind `DocumentHead` — is **not** cleared by a soft-wrap row or an
alignment spacer. A split diff whose first line is padded on one side puts a spacer above that
pane's first blamed row; clearing on it would make one pane draw a hairline across its top edge
while the other did not. Same reasoning as the spacer exemption in the run rule itself.

### Ruled out

- **Prepending the context rows to the slice and dropping the prefix from the result.** Same
  single-source-of-truth property, and it was the first shape I wrote. It clones the whole
  viewport's `blamed_rows` (~60 `BlameEntry`, each with several `String`s) on every frame of a
  scroll to build the extended vector. The seed carries the same information in three words.
- **A `preceding: Option<…>` parameter computed by `EditorElement::layout_blame_entries`.** It
  puts "look one row up, and keep looking while the rows are wraps or spacers" — a clause of the
  boundary rule — in the caller, which is what the task asked to avoid.
- **Answering it from the `SumTree` in buffer-row space** (does `buffer_row - 1` carry the same
  sha?). It skips the display-space question entirely: a fold, an excerpt header or a diff-hunk
  block between the two lines has to sever the run, and none of them is visible from buffer rows.
  It would have been a second, weaker copy of the rule.
- **A fixed lookback depth** (say 8 or 64 rows). Cheap, and wrong in exactly the case that made
  the label slide: scrolling into the middle of a long soft-wrapped line, or past a tall
  alignment spacer. The doubling scan costs one row in the common case and is exact.
- **Suppressing the hairline on nothing at all** (every `Head` including the display's first
  row). One variant less, but it paints a 1px rule across the top edge of the gutter when the
  file is scrolled to the top, where it separates nothing and reads as a frame — the original
  exemption's actual motivation, which the task asked to preserve. See *Open question* below.

## Tests whose meaning changed

All in `crates/editor/src/git/blame.rs` unless noted.

| Test | Was | Is |
|---|---|---|
| `blame_run_positions_group_consecutive_rows_of_one_commit` | first row `Head` | first row `DocumentHead` — the fixture starts the display |
| `blame_run_positions_break_when_the_sha_changes` | first row `Head` | `DocumentHead` |
| `blame_run_positions_break_when_buffer_rows_jump` | first row `Head` | `DocumentHead` |
| `blame_run_positions_break_when_the_buffer_changes` | first row `Head` | `DocumentHead` |
| `blame_run_positions_break_across_a_header_block_row` | first row `Head` | `DocumentHead` (the row *below* the block stays `Head`) |
| `blame_run_positions_survive_an_alignment_spacer_row` | first row `Head` | `DocumentHead` |
| `blame_run_positions_still_break_when_buffer_rows_jump_across_a_spacer` | first row `Head` | `DocumentHead` |
| `blame_run_positions_survive_a_soft_wrapped_row` | first row `Head` | `DocumentHead` |
| `blame_run_positions_break_across_an_unblamed_row` | first row `Head` | `DocumentHead` |
| `blame_run_positions_head_the_first_row_of_a_mid_run_slice` | **encoded R2**: rows 7,8 of one commit, first row asserted `Head` | **deleted as a rule, rewritten as its opposite** — split into `…_continue_a_run_the_rows_above_started` (rows 7,8 below a blamed row 6 → both `Continuation`) and `…_head_a_slice_that_starts_at_a_run` (rows 7,8 of a *different* commit → `Head`, `Continuation`) |
| `solo_diff_view.rs::test_a_run_boundary_draws_a_hairline_above_the_head_row` | row 0 has no hairline *because it is the top of the viewport* | row 0 has no hairline *because it is the first row of the file*; doc comment and assertion message rewritten |

Test helpers: `assert_run_positions` / `assert_run_positions_with_alignment` now state that their
fixture starts the display; the shared body moved into `run_positions(spec, predecessor)`, and
`assert_run_positions_below(context, spec, expected)` was added — it is the composition the
gutter performs (classify the rows above, then the visible ones), with the display-row scan
standing in as a literal fixture.

### New tests

- `blame_run_positions_continue_a_run_the_rows_above_started` — the core new rule.
- `blame_run_positions_head_a_slice_that_starts_at_a_run` — the other side of it.
- `blame_run_positions_head_below_a_block_row_above_the_slice` — a block row above still severs.
- `blame_run_positions_continue_across_a_soft_wrap_above_the_slice` — a wrap above does not.
- `blame_run_positions_continue_across_an_alignment_spacer_above_the_slice` — nor does a spacer.
- `blame_run_positions_head_below_an_unblamed_row_above_the_slice` — an unblamed line does.
- `blame_run_positions_open_the_display_at_its_first_row` — buffer row 0: `DocumentHead`.
- `blame_run_positions_open_the_display_across_a_leading_spacer` — a spacer at the display's
  first row keeps the pane's first blamed row a `DocumentHead`, so two split panes agree.
- `blame_run_positions_report_what_the_rows_below_look_at` — the trailing predecessor
  (`Blamed` / `Severed` / `DisplayStart`), which is what the widening loop keys on.
- `test_run_positions_reach_past_wrapped_rows_above_the_viewport` (`#[gpui::test]`) — the only
  test that drives a real `DisplaySnapshot`: a soft-wrapped second line, viewport starting at
  the third buffer line, asserting the visible rows are continuations. Added **because a
  mutation survived without it** (M5 below).
- `solo_diff_view.rs::test_a_run_scrolled_past_its_head_leaves_its_rows_blank` (painted,
  `debug_bounds` after a real frame, both sides): scrolled to display row 1 the top row draws
  neither metadata nor separator while the second run's head one row down draws both; scrolled
  to row 2 that same head is the top row and still draws both. The scroll is asserted to have
  landed (`scroll_to_row`), or a clamped scroll would satisfy the assertions for the wrong
  reason.

## Mutation table

Each mutation was applied to the working tree, the named suite run, then reverted.

| # | Mutation | Suite | Result |
|---|---|---|---|
| M1 | `blame_run_positions` ignores the seed (`previous = None` always) | `cargo test -p editor --lib git::blame` | **killed** — 3 failed: `…continue_a_run_the_rows_above_started`, `…continue_across_a_soft_wrap_above_the_slice`, `…continue_across_an_alignment_spacer_above_the_slice` |
| M2 | `run_positions_in_viewport` never looks above the viewport (`predecessor = DisplayStart`) | `cargo test -p git_ui --lib solo_diff_view` | **killed** — `test_a_run_scrolled_past_its_head_leaves_its_rows_blank` |
| M3 | `git_ui`: `opens_a_boundary = !is_continuation` (hairline on every head, incl. the display's first row) | `cargo test -p git_ui --lib solo_diff_view` | **killed** — `test_a_run_boundary_draws_a_hairline_above_the_head_row` |
| M4 | `nothing_above` also cleared by wrap / spacer rows | `cargo test -p editor --lib git::blame` | **killed** — `…open_the_display_across_a_leading_spacer`, `…report_what_the_rows_below_look_at` |
| M5 | the widening loop removed (`run_predecessor_above` looks up exactly one row) | `cargo test -p git_ui --lib solo_diff_view` | **SURVIVED** (33 passed) — no test drove a `DisplaySnapshot` whose rows above the viewport were wraps. Fixed by adding `test_run_positions_reach_past_wrapped_rows_above_the_viewport`; re-run with that test present: **killed** |

## Verification

- `CARGO_BUILD_JOBS=4 cargo build --bin sawe` — exit 0, zero `^error` / `^warning` lines.
- `CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets` — exit 0, zero `^error` and zero
  `^warning` lines. (First pass had one: an unused `buffer_id` in the new gpui test, since
  removed. `cargo build --bin sawe` cannot see that universe — the warning only exists in
  `lib test`.)
- `CARGO_BUILD_JOBS=4 cargo test -p editor -p git_ui` — exit 0; `editor` 819 passed / 0 failed /
  1 ignored, `git_ui` 405 passed / 0 failed.
- Live: `script/run-mcp --debug --headless --runtime-dir /tmp/blame-stable-probe`, driven over
  the MCP socket. This repository was not usable as the probe fixture — `solutions.add_member`
  only takes a `catalog_id` and *clones*, and `catalog.add_project` takes `name` + `remote_url`,
  so pointing a Solution at an existing local checkout is not an operation the tool surface
  offers. Instead `solutions.add_empty_member` made a `git init`ed member and the probe wrote a
  200-line file into it with two commits: commit A (Lovelace, 02 Jan 2024) wrote all 200 lines,
  commit B (Hopper, 07 Jun 2024) rewrote lines 5–7. That leaves runs 1–4 / 5–7 / **8–200**, a
  193-line run to scroll inside — a sharper fixture than any file in this repo.

### Screenshots

| File | What it shows |
|---|---|
| `2026-09-04-blame-stable-file-top.png` | Scrolled to the top: labels on lines 1, 5 and 8; hairlines above 5 and 8; **no hairline above line 1** — the display's first row separates nothing. |
| `2026-09-04-blame-stable-midrun-a.png` | Top row = line 26, inside the 8–200 run: the gutter is **blank**. No label has slid up to the top row, no hairline at the top edge. |
| `2026-09-04-blame-stable-midrun-b.png` | Top row = line 38, same run: **identical** blank gutter. The label did not move between the two offsets — the two frames differ only in the code. |
| `2026-09-04-blame-stable-boundary-at-top.png` | Scrolled so line 8 — a real boundary — is the top row: it draws its date and author **and** its hairline on the top edge. |

Pixel check on the last two, at the first content row (y=97, x=300 inside the blame column,
x=600 outside it):

| Frame | x=300 | x=600 | reading |
|---|---|---|---|
| boundary at top (line 8) | `(223,223,224)` | `(250,250,250)` | hairline painted, spanning the blame column only |
| mid-run (line 26) | `(250,250,250)` | `(250,250,250)` | nothing painted |
| file top (line 1) | `(239,239,239)` | `(239,239,239)` | active-line highlight across the full width, not a hairline |

## Replacement wording for `FORK.md` #140

`FORK.md` was **not** edited. Two paragraphs of entry #140 now describe behaviour that no longer
exists; below is the wording to put in their place.

**Replaces the paragraph beginning "The first blamed row of the viewport is always a head."**

> **A run's head is a display-row fact, not a viewport one.** The classification is seeded with
> what sits above the rows being drawn: `BlameRunPredecessor { DisplayStart, Severed, Blamed {…} }`
> is both the parameter and the return value of `blame_run_positions`, so classifying the rows
> above a slice and classifying the slice compose, and `GitBlame::run_predecessor_above` obtains
> the seed by running the rows above the viewport through *the same function* rather than by a
> second reading of the rule. A run that begins above the visible rows therefore keeps its label
> up there and leaves its visible tail blank, the way IntelliJ does — scrolling no longer drags
> the date and author down the gutter. The scan above the viewport doubles its reach (1, 2, 4, …
> up to `MAX_RUN_PREDECESSOR_LOOKBACK` = 1024 display rows) and stops as soon as the scanned
> window classifies to anything but `DisplayStart`: one row settles it unless the rows above are
> soft-wrap continuations or alignment spacers, which stand for nothing in this buffer and
> settle nothing. Do not replace this with a fixed depth, and do not answer it from the
> `SumTree` in buffer-row space — a fold, an excerpt header or a hunk block between two lines
> has to sever the run, and none of them is visible from buffer rows.

**Replaces the sentence "`ix > 0` exempts the topmost visible row, where a rule reads as a frame
around the editor rather than as a break."**

> The hairline is drawn for `BlameRunPosition::Head` and not for `DocumentHead` — the third
> variant, meaning "opens a run with nothing above it at all", which only the display's first
> row can be. That is the whole of the rule the renderer applies: it never reasons about `ix`,
> because a viewport-relative exemption is what used to make a real boundary's hairline vanish
> as it reached the top edge and reappear one row later. `DocumentHead` survives a leading
> alignment spacer for the same reason spacers do not sever a run: otherwise the padded pane of
> a split diff would draw a hairline across its top edge and its companion would not.

**Replaces the final bullet, "Two known cosmetic edges, both consequences of a viewport-local
computation."** The first of the two is gone; the second is not:

> - **One known cosmetic edge.** The one row `BlockRows::next` does *not* collapse — the first
>   output row of a collapsed block crease — is a blamed text row, so when it continues the run
>   above it it draws a blank gutter row where it used to draw the date.

The bullet "Both the run computation and the pixels are testable without git" still holds, with
one addition: `test_run_positions_reach_past_wrapped_rows_above_the_viewport` is a `#[gpui::test]`
that builds a real `DisplayMap` with a wrap width, because the *scan* above the viewport (as
opposed to the rule it feeds) cannot be reached from hand-made `RowInfo` vectors — a mutation
proved it (M5 above).

## Open question / concerns

- **The display's first row draws no hairline** — I ruled that a run head with nothing above it
  is not a boundary, which keeps `test_a_run_boundary_draws_a_hairline_above_the_head_row`'s
  assertion (and the original exemption's stated motivation) intact and costs one enum variant.
  The alternative — a hairline on *every* head, one variant less — paints a rule across the top
  edge of the gutter when a file is scrolled to the top. `2026-09-04-blame-stable-file-top.png`
  shows the chosen behaviour; if the maintainer wants the line there anyway, it is a one-line
  change (`matches!(position, Head | DocumentHead)`) plus the two tests that assert its absence.
- **`MAX_RUN_PREDECESSOR_LOOKBACK` is a bound on work, not on correctness**, and nothing tests
  the give-up path: it needs 1024 consecutive display rows above the viewport that are all wraps
  or spacers. If it ever fires, the top row renders as it did before this change.
- The probe fixture is synthetic (see above); the tool surface has no way to point a Solution at
  an existing local checkout. Worth a `solutions.add_local_member` some day — an agent that
  wants to drive a real repository's history currently has to clone it.
