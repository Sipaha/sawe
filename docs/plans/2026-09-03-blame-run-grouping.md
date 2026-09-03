# Plan — group consecutive blame lines from one commit

Status: **complete** (2026-09-03) — commits `c0085392d9`, `e9e3978a7e`, `3e04449a7a`,
`b14ee2f7eb`, `58e722a7cd`, `aff2e0800b`, plus this doc pass.
Tracked: `TODO.md` **C10** (now deleted), `FORK.md` **#135** ("Not done" paragraph, now
amended) — the decision that records what shipped is `FORK.md` **#140**.

Everything below the "What actually shipped" heading at the end of this file is the
record; everything above it is the plan as written before dispatch, left unedited so the
divergences are readable.

## The goal

The blame gutter prints the date and the shortened author on **every** line, so a
file written in a few large commits reads as a wall of repetition. IntelliJ
draws the metadata **once per run** of consecutive lines sharing a commit and
separates runs visually. Do that here, without losing anything a row can do
today.

*Done when* (from `TODO.md` C10): runs are visually grouped, the metadata is
drawn once per run, and **every** row in a run — including continuation rows —
still hovers (tooltip), right-clicks (context menu) and left-clicks to open its
commit.

## The mechanics that constrain the design

Verified by an exploration pass on 2026-09-03; every claim below was read out of
the file named.

- `EditorElement::layout_blame_entries` (`crates/editor/src/element.rs:2138`) is
  the **only** place that holds both the display-space row metadata
  (`buffer_rows: &[RowInfo]`, viewport-sliced at `element.rs:8341`) and the
  flattened blame entries (`GitBlame::blame_for_rows`,
  `crates/editor/src/git/blame.rs:446`). A `BlameRenderer` is handed one entry
  and cannot see its neighbours.
- `blame_for_rows` yields `Option<(BufferId, BlameEntry)>` per **display** row,
  cloning the same `BlameEntry` for every row of a git hunk. A `None` row means
  one of four different things — soft-wrap continuation
  (`RowInfo { buffer_id: None, wrapped_buffer_row: Some(_) }`, `wrap_map.rs:1162`),
  a block row (excerpt header / diff-hunk controls / folded-buffer header,
  `RowInfo::default()`, `block_map.rs:2740`), a row edited since blame ran
  (`blame.rs:591`), or a genuinely unblamed row (`blame.rs:833`). Only `RowInfo`
  tells them apart.
- Folds delete rows from display space, so **two runs of one commit separated in
  the buffer by other commits become adjacent display rows when the lines
  between them are folded**. Sha equality alone is not a run.
- `BlameEntry::range` looks like a run key but is stale after an in-buffer edit
  (`GitBlame::sync` clones the original entry into both halves of a split,
  `blame.rs:584`, `:611`), so adjacency must come from `RowInfo::buffer_row`.
- Every listener on a gutter row — hover background, tooltip, right-click menu,
  left-click `OpenAtCommit` — is attached to the row's `h_flex`
  (`crates/git_ui/src/blame_ui.rs:183-249`), not to its text children. The row
  has **no explicit height**; it takes `line_height` from the style plus its
  children's intrinsic height.
- `render_muted_blame_entry` (`blame_ui.rs:596`) is the existing "reduced row"
  shape and it drops **every** listener. It is the anti-pattern here, not the
  precedent. (It is also currently unreachable — see `TODO.md` C11.)
- The gutter width is priced row-agnostically from
  `max_author_columns.min(renderer.max_author_columns()) + renderer.gutter_fixed_columns(cx)`
  (`crates/editor/src/editor.rs:11567`). Drawing metadata on fewer rows leaves it
  an upper bound, which is correct; reclaiming the freed columns is **not** part
  of this work.
- Inline (end-of-line) blame is a separate pipeline (`layout_inline_blame`,
  `element.rs:1949`) and must stay untouched.
- Six existing painted tests assert `debug_bounds("GIT-BLAME-ENTRY[-LEFT|-RIGHT]")`
  presence/absence (`crates/git_ui/src/solo_diff_view.rs:1712-2005`).
  `debug_bounds` is keyed by name only, so those selectors cannot distinguish
  rows — asserting "metadata on this row, none on that one" needs **new,
  per-row** selectors alongside the existing ones.

## Rulings made before dispatch (controller)

**R1 — the run boundary rule.** A row continues the previous run only when all
hold: same `BufferId`; same `sha`; its `RowInfo::buffer_row` is exactly the
previous blamed row's `buffer_row + 1`; and no **block** row intervened. A
soft-wrap continuation row (`buffer_id: None` **and** `wrapped_buffer_row:
Some(_)`) does **not** break a run — it is the same buffer line still being
drawn. A block row (`buffer_id: None` **and** `wrapped_buffer_row: None`) does
break it, because an excerpt header visually severs the run and the rows either
side may be from different excerpts of the same buffer. *Cost if wrong:* a
mis-grouped run draws metadata once too often or once too rarely at a boundary;
no data is lost and no row loses behaviour.

**R2 — the first blamed row of the viewport is always a run head.** Runs are
computed from the visible slice only, so a run that started above the viewport
has no head in view. Rather than reach outside the slice, treat the topmost
blamed row as a head: scrolling into the middle of a large commit then shows who
wrote it instead of a blank column, and a fully-visible run is unaffected.
*Cost if wrong:* the metadata line appears to stick to the top of the viewport
while scrolling through one long run; changing it later is a one-line change in
the run computation.

**R3 — visual grouping is a separator, not a tint.** Draw a hairline at the top
edge of each run head (theme `border_variant`), spanning the blame column, and
leave the row background alone. A per-run background would fight the existing
per-row `hover` background (`blame_ui.rs:203`), and a sha-derived tint on a
narrow gutter column risks a rainbow. The per-commit colour already on the
author name (#135) stays the "same commit" cue. Task 3 also captures a
screenshot of the tint alternative so the maintainer can overrule with evidence.
*Cost if wrong:* a visual preference, one constant and one element deep.

**R4 — the trait grows one argument, not a new method.** Both gutter renderers
(`BlameRenderer::render_blame_entry`, `:render_blame_entry_with_options`) take
the run position. There are exactly two implementors (`impl … for ()` at
`blame.rs:214` and `GitBlameRenderer` at `blame_ui.rs:49`), so the churn is
bounded, and a parallel "run-aware" method would leave two paths that can
disagree. *Cost if wrong:* a wider trait signature.

**R5 — work lands on `main` in the primary checkout, not a worktree.** This is
the repo's own convention (solo repo, pushing pre-authorised) and the headless
probe plus `target/release-fast` hand-off both key off this checkout. No
implementers run in parallel, so there is no conflict to isolate.

## Global Constraints

Binding on every task; a reviewer should treat a violation as a finding.

1. **Continuation rows keep every behaviour.** Hover background, tooltip,
   right-click context menu and left-click-opens-commit must work on a row that
   draws no metadata. Give the row an explicit height so an empty flex still has
   a hit area. Do **not** reuse the `render_muted_blame_entry` shape.
2. **The gutter width reservation does not change.** `gutter_fixed_columns` and
   `max_author_columns` stay row-agnostic. No test in
   `crates/git_ui/src/blame_ui.rs`'s width battery may change meaning.
3. **Existing debug selectors keep their names** — `GIT-BLAME-ENTRY`,
   `GIT-BLAME-ENTRY-LEFT`, `GIT-BLAME-ENTRY-RIGHT`. Six tests in
   `crates/git_ui/src/solo_diff_view.rs` depend on them. Add new per-row
   selectors alongside; do not rename.
4. **Inline blame and the blame popover are out of scope.** No change to
   `layout_inline_blame`, `render_inline_blame_entry`,
   `render_blame_entry_popover`, or `longest_line_blame_width`.
5. **The run computation is a pure function with unit tests**, living where the
   tests can reach it. "It is exercised by the paint test" is not coverage for
   the boundary rules in R1.
6. **A test that asserts the predicate is not a test of the painted UI**
   (repo-root `.rules`). At least one assertion must read the painted tree via
   `VisualTestContext::debug_bounds`, and it must assert **both** sides —
   metadata present on a head row, absent on a continuation row.
7. **Reports carry a mutation table**, not "I added a test": for each new test,
   the one-line source mutation it would catch. A test that survives inverting
   the code it covers is a finding.
8. **Build discipline.** `CARGO_BUILD_JOBS=4` always (`earlyoom` kills the
   largest-RSS process). Never pipe cargo through `| tail` / `| head` (masks the
   exit code); grep the captured log for `^error` and `^warning`.
   `cargo check --workspace --all-targets` must be warning-free.
   `script/clippy` forces `--release` — do not run it for routine checks.
9. **Rust guidelines** from `CLAUDE.md`: no `unwrap()`, no silent `let _ =` on a
   fallible call, no organisational comments, comments explain *why*.
10. **Never** delete `target/release-fast`, never `rm -rf` anything under
    `~/.spk/sawe/` — `~/.spk/sawe/ss` holds the maintainer's real checkouts,
    including this repository.
11. **A documented negative result is a valid outcome.** If a task's premise is
    wrong, say so with evidence instead of implementing around it.

## Task 1 — compute run positions in the editor and plumb them to the renderer

**Files:** `crates/editor/src/git/blame.rs`, `crates/editor/src/element.rs`,
`crates/git_ui/src/blame_ui.rs` (signature only).

Add a `BlameRunPosition` (name it as you see fit; two states — the row starts a
run, or continues one) and a **pure function** that maps
`(&[RowInfo], &[Option<(BufferId, BlameEntry)>])` to one position per row,
implementing R1 exactly. Put it in `crates/editor/src/git/blame.rs` beside
`blame_for_rows`, which already has a `mod tests` and where `RowInfo` is in
scope.

Call it from `layout_blame_entries` (`element.rs:2138`) after `blamed_rows` is
collected, pass the row's position through the free `render_blame_entry`
(`element.rs:7150`) into **both** gutter methods of `BlameRenderer`
(`blame.rs:137`, `:156`), update the `()` implementation (`blame.rs:214`) and
`GitBlameRenderer` (`blame_ui.rs:63`, `:92`).

**This task changes no pixels.** `GitBlameRenderer` accepts the argument and
ignores it; the visual change is Task 2. That keeps the plumbing reviewable on
its own and the six existing painted tests green.

**Tests (in `crates/editor/src/git/blame.rs`):** unit tests for the boundary
rule, one per rule, each built from a hand-made `Vec<RowInfo>` plus a hand-made
blamed-row vector — no git needed:
- consecutive rows, same sha, same buffer → one head then continuations;
- sha changes → new head;
- `buffer_row` jumps (a fold) with the sha unchanged → new head;
- `buffer_id` changes with the sha unchanged → new head;
- a **block** row between two same-sha rows → new head after it;
- a **soft-wrap** row between two same-sha rows whose buffer rows are still
  consecutive → **no** new head;
- an unblamed (`None`, non-wrap) row between two same-sha rows → new head after
  it, since the rows are no longer adjacent;
- the first blamed row of the slice is always a head (R2), including when the
  slice starts mid-run.

## Task 2 — draw the metadata once per run, keep continuation rows alive

**Files:** `crates/git_ui/src/blame_ui.rs`, tests wherever the painted harness
already lives.

On a continuation row, do not draw the date, the avatar, or the author name;
draw the row container with its listeners intact and an explicit height. On a
head row, nothing changes. Keep the *outer* `div`'s existing `debug_selector`
untouched (Global Constraint 3) and add per-row selectors that let a test say
"this row painted metadata and that one did not" — the metadata subtree needs
its own name, and `debug_bounds` is keyed by name only, so the name must carry
the row index.

**Tests:**
- a painted test that drives a real frame and asserts, via `debug_bounds`,
  metadata present on the head row of a run and **absent** on a continuation row
  of the same run (both sides — Global Constraint 6);
- a painted assertion that a continuation row still has a hit area (non-zero
  bounds of the row container, at the expected height).

The blame-capable painted harness already exists in
`crates/git_ui/src/solo_diff_view.rs` (`blame_both_panes`, `:1689`); reuse or
generalise it rather than building a second one.

## Task 3 — separate consecutive runs visually, and screenshot the alternative

**Files:** `crates/git_ui/src/blame_ui.rs` (and `crates/editor/src/element.rs`
only if the hairline genuinely cannot be drawn from the row element).

Implement R3: a hairline at the top edge of every run head, spanning the blame
column, in a theme colour (`border_variant` or the nearest existing token — pick
by reading `crates/theme`, do not invent a colour). No hairline above the first
row of the viewport (it would read as a frame edge).

Then capture evidence in a **headless probe editor**, not in the maintainer's
live instance:

```
cargo build --bin sawe            # CARGO_BUILD_JOBS=4; run-mcp only builds when missing
script/run-mcp --debug --headless --runtime-dir /tmp/blame-run-probe
python3 /tmp/mcp.py <socket> <tool> '<json>'
```

Open a file with several multi-line commits (this repository is one), turn the
blame gutter on, screenshot it, and save the PNGs. Deliver:
- `after-separator.png` — this task's implementation;
- `after-tint.png` — the same view with a per-run background tint instead
  (a throwaway local edit; **revert it** before committing, it exists only to be
  looked at);
- `before.png` — the same view at `HEAD` before this branch's first commit, for
  the wall-of-repetition comparison.

Report the three absolute paths. The controller looks at them and rules.

## Task 4 — the durable record

- `FORK.md`: a numbered entry for this work under "Key architectural decisions"
  — where the run is computed and why it cannot be the renderer's job, R1's
  boundary rule with the fold/excerpt cases that motivate each clause, R2, R3,
  and what a future reader must not "fix" (do not key runs on `sha` alone, do not
  reclaim the date's gutter columns, do not reduce a continuation row to
  `render_muted_blame_entry`). Amend #135's "Not done" paragraph — it is done.
- `TODO.md`: delete C10 and renumber nothing (later items keep their numbers;
  say in the entry's place nothing at all — the section is a list, not a
  register).
- This plan file: a "What actually shipped" section with every divergence.

Nothing here is a code change; do not touch code in this task.

## What actually shipped

Six commits, in order: `c0085392d9` (this plan), `e9e3978a7e` (Task 1),
`3e04449a7a` (Task 2), `b14ee2f7eb` (Task 3), `58e722a7cd` (Task 3b — not in
this plan), and `aff2e0800b`, a review follow-up that strengthens the spacer
test to assert its continuation rows are *painted* rather than merely missing
their metadata, and takes the accompanying doc-comment correction in
`block_map.rs`. The durable record is `FORK.md` #140; the per-task detail, including
each task's mutation table, is in
`.superpowers/sdd/2026-09-03-blame-run-grouping/task-{1,2,3,3b}-report.md`, and
every ruling is in that directory's `progress.md` — all of which are **local working
notes only**: `.superpowers/` is gitignored, so the reports and the five screenshots
they cite are not in the repository and this file plus `FORK.md` #140 are the durable
record.

### Rulings made after dispatch

- **R6 (Task 1)** — a deletion hunk inside a diff does *not* break a run. Deleted
  rows carry `buffer_id: Some(base_buffer)` and are unblamed-but-not-block, so R1
  joins the lines either side of them. No code was written for this; it is what
  R1 already does. Task 3 was told to screenshot a split diff so the pixel was
  judged on evidence — which is what produced R7.
- **R7 (Task 3), superseded** — Task 3's screenshots showed R6 holding in the
  left pane and failing in the right, where the deleted lines render as a
  *block* row and R1 therefore severs the run and re-prints the label. The
  controller first ruled this was the honest pixel: a block row is a visible
  break, and teaching the rule to distinguish "a block that is a visual gap"
  from "a block that is a header" needed a distinction `RowInfo` does not carry.
- **R7′ (supersedes R7)** — the reviewer confirmed on pixels that the two panes
  of one split diff then render the *same commit* differently, which is worse
  than either behaviour on its own; and the premise was only half true, since
  the block map knows the block's kind even though `RowInfo::default()` throws it
  away. R7′: an alignment/deleted-line spacer must not sever a run, while excerpt
  and folded-buffer headers must keep severing. Dispatched as Task 3b.

### Divergences from the plan

- **Task 3b did not exist in this plan at all.** It added
  `Block::is_alignment_only()` to `crates/editor/src/display_map/block_map.rs` —
  a file no task's file list named — and a viewport wrapper,
  `blame_run_positions_in_viewport(snapshot, start_row, rows, blamed_rows)`,
  which reads the alignment flags off the `DisplaySnapshot` so the classifier
  stays pure and no caller has to expand blocks over their `height()`. The
  rejected alternative was a flag on `RowInfo`: cheaper at the producer, but
  `RowInfo` lives in `multi_buffer`, knows nothing about blocks, and is built by
  exhaustive struct literals in crates other agents were mid-flight in.
- **`blame_run_positions` returns `Vec<Option<BlameRunPosition>>`, not one
  position per row.** The plan said "one position per row"; an unblamed row
  genuinely has no position, and making that `None` is what lets the block and
  soft-wrap tests assert the *middle* row rather than only its successor.
- **It takes a third slice.** Task 1 shipped `(rows, blamed_rows)`; Task 3b added
  `alignment_rows`, positionally aligned like the other two, with its own
  `debug_assert_eq!` on the length.
- **The `debug_assert_eq!` on slice lengths was not planned** — it came from
  Task 1's review as a deferred minor and landed in Task 2.
- **The explicit row height is conditional.** Global Constraint 1 said to give
  the row an explicit height; only *continuation* rows got one. A head row is
  22.5px of intrinsic text height inside a 23px pitch and stating a height for it
  would have moved the date relative to its code line.
- **The hairline is absolutely positioned, not a top border**, for the same
  geometry reason, and it is added *after* the row in the child list so the row's
  hover background cannot paint over it. `crates/editor/src/element.rs` was
  therefore **not** needed for Task 3 — the escape hatch the task allowed went
  unused.
- **`before.png` is a reconstruction, not a build of `c0085392d9`.** Task 3 could
  not afford a cold ~20-minute debug compile in a checkout other agents were
  editing, so it disabled the two visual deltas in the current tree, built,
  photographed and reverted. The report justifies the equivalence hunk by hunk;
  it is stated here because a screenshot labelled "before" that is not a build of
  the before-commit is exactly the claim a later reader would assume.
- **Task 3 delivered a fourth screenshot** beyond the three the plan listed —
  `after-separator-split-diff.png`, the R6 evidence shot — and Task 3b a fifth,
  `after-r7-split-diff.png`, for the same view after the fix.
- **The tint alternative (R3) was photographed and rejected on the evidence**, as
  planned: it reads run structure faster at a glance but competes with the code
  across the whole column, weakens the per-row hover background it sits under,
  and leaves a 0.5px unpainted seam under every labelled row (the head-row height
  again). The hairline, being one out-of-flow pixel, has no seam.
- **Task 3b found the defect in both panes, not one.** Task 3's report described
  the re-cutting as right-pane-only; the left pane has its own spacer, standing
  for the commit's added lines, and was re-cutting there too. One change fixed
  both.
- **Two file citations in the dispatch briefs were wrong** (`wrap_map.rs` and
  `block_map.rs` live under `crates/editor/src/display_map/`, not
  `crates/multi_buffer/src/`). The facts they asserted were correct and were
  re-verified by the implementer.

### Corrections to the record

The whole-feature review caught four false claims in the first version of this
doc pass. They are listed here rather than silently edited, because one of them
also lives in a commit message this repo does not rewrite:

- **"a seventh painted test" — wrong, and it retracted a correct statement.** The
  first version of this section claimed the plan undercounted the pre-existing
  painted blame tests at six when there were seven. There are **six**: at
  `c0085392d9` all fifteen `debug_bounds("GIT-BLAME-ENTRY…")` assertions sit
  between `:1741` and `:1999`, across
  `test_the_left_pane_paints_blame_for_a_working_tree_diff`,
  `test_a_re_split_left_pane_still_paints_blame`,
  `test_both_panes_paint_blame_for_a_commit_diff`,
  `test_an_added_file_blames_only_the_right_pane`,
  `test_a_deleted_file_blames_only_the_left_pane` and
  `test_a_binary_file_blames_neither_pane`.
  `test_the_blame_base_follows_the_source` (`:2192`) asserts no such selector at
  all. The plan's Global Constraint 3 was right as written. **The commit message
  of `96e9a1e5e0` still carries the wrong claim** — this paragraph is the
  correction of record.
- **`FORK.md` #140 said a `BlameEntry::range` key "collapses six of the eight"**
  run-position tests. Two errors: there are **ten** such tests
  (`crates/editor/src/git/blame.rs:1797-1958`, two of them added by Task 3b), and
  swapping the identity test for range equality fails **five** of them — the sha
  change, the buffer-row jump, the buffer change, the jump across a spacer, and
  the break across an unblamed row. The header-block test survives, because it is
  pinned by the separate block-row clause rather than by the adjacency test.
  Corrected in #140 and in the test helper's own comment.
- **"`BlockRows::next` collapses *every* block row to `RowInfo::default()`" is
  false**, in #140, in the touched-files row, and in `Block::is_alignment_only`'s
  own doc comment. `block_map.rs:2756-2763` forwards the real row info for the
  first output row of a non-`FoldedBuffer` `is_replacement()` block — i.e. a
  `Block::Custom(BlockPlacement::Replace)`, which `DisplayMap::fold` produces for
  a block crease. Gutter consequence, now recorded in #140: a collapsed block
  crease's first row is a blamed text row, so when it continues the run above it
  it draws a blank gutter row where it used to draw the date.
- **The commit list was stale by one** — `aff2e0800b` landed after the doc pass
  and is now in both the Status line and the list above.

### Deferred, with the reasons

Carried out of the reviews and deliberately not fixed here: the duplicated test
fixture helpers `set_blame_at_revisions` / `set_blame_runs_at_revisions`; one
combined `assert!` in `solo_diff_view.rs` that loses the diagnostic
distinguishing its two halves; the fact that a filtered-out author's run head
draws no hairline (unreachable while `TODO.md` C11 stands); and
`alignment_rows.get(ix).copied().unwrap_or(false)`, which means a short flag
slice reverts to pre-fix behaviour in a release build — documented, with a single
production caller that builds the slice itself. Two cosmetic edges are accepted
rather than deferred and are stated in `FORK.md` #140 so there is one copy of
each: the hairline's one-row flicker at the viewport's top edge, and the absence
of any painted coverage for the plain single-editor gutter (every painted
assertion goes through the split-diff `-LEFT` / `-RIGHT` suffix).

### Task 4 (this pass)

`FORK.md` gained decision **#140** (and #135's "Not done" paragraph was amended
to point at it) plus a touched-files row for `block_map.rs`; `TODO.md` **C10**
was deleted with nothing in its place and no renumbering.
