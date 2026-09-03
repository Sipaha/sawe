# Plan — group consecutive blame lines from one commit

Status: in progress (2026-09-03)
Tracks: `TODO.md` **C10**, `FORK.md` **#135** ("Not done" paragraph)

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
