# The file-history Pencil toggle aborted the editor

`git_graph`'s synthetic *Local Changes* row emitted **four** cells into a
**three**-column table. `Table` converts each rendered row with
`TableRow::from_vec`, whose length check is an un-gated `panic!`, so the first
frame after the toggle aborted the process — and, because `with_local_changes`
is serialized on the item, again on every relaunch.

## 1. Repro, before the fix

`script/run-mcp --debug --headless --runtime-dir /tmp/pencil-crash-probe`,
against a throwaway 3-commit repo added through `catalog.add_project` +
`solutions.add_member` (a local path is a perfectly good git remote, which is
how the catalog's clone-based member flow accepts one).

1. `project.open_file file.txt` → `windows.dispatch_action git::FileHistory`.
   The graph paints: three commits, columns Description / Date / Author.
   (`/tmp/before-history.png`, verified by reading the PNG.)
2. Identify the Pencil control before touching it: `windows.hover_at
   {x:1840,y:115}` + screenshot, cropped — the pencil glyph in the
   file-history toolbar, third from the right.
3. `windows.click_at {x:1840,y:115}` → the tool call itself returns
   `click at (1840, 115) x1` and then the socket dies. `run-mcp` prints:

```
script/run-mcp: line 211: 546061 Aborted (core dumped) "$binary" --headless
```

and `<runtime-dir>/.spk/sawe-dev/logs/sawe.log` has:

```
ERROR [crashes] thread 'main' panicked at crates/ui/src/components/data_table/table_row.rs:35:13:
Expected alloc::vec::Vec<gpui::element::AnyElement> to be created successfully: Row length 4 does not match expected 3
   8: ui::components::data_table::table_row::TableRow<T>::from_vec::{{closure}}
  11: <alloc::vec::Vec<T> as ...::IntoTableRow<T>>::into_table_row      (table_row.rs:136)
  12: <ui::components::data_table::Table as RenderOnce>::render::…      (data_table.rs:1164)
  22: gpui::elements::uniform_list::uniform_list::{{closure}}
  24: gpui::elements::uniform_list::UniformList::measure_item
```

**Crash-loop confirmed, not assumed.** Relaunching on the same
`--runtime-dir` and calling `solutions.open` — no clicks, no gesture —
aborted again the moment the file-history item was restored with the flag on.
2/2 launches. Only wiping the runtime dir got the editor back.

## 2. What the real cause turned out to be

The diagnosis handed to me was correct on the facts and incomplete on the
shape. There are exactly **three** branches in `render_table_rows`
(`crates/git_graph/src/git_graph.rs`): the synthetic local-changes row, the
"no commit at `data_idx`" fallback, and the real commit row. Two of the three
emitted four cells; only the real commit row emitted three. So the header, the
`Table::new` arity, the two fraction arrays and three row branches all
independently hard-coded a `3` (or, in two places, a `4`) with **no shared
declaration of the column contract anywhere** — which is exactly why the same
mistake occurred twice.

Deleting two stray `div()`s would have left the next branch free to repeat it,
so instead:

- **`const LOG_COLUMN_COUNT: usize = 3`** now backs `Table::new`, the header's
  `TableRow::from_vec`, `UNMEASURED_COLUMN_FRACTIONS`,
  `default_column_fractions`, `new_column_widths_state` and
  `RedistributableColumnsState::new`. Its doc comment states the contract and
  says why breaking it is fatal rather than cosmetic.
- **`fn log_table_row(cells: [AnyElement; LOG_COLUMN_COUNT]) -> Vec<AnyElement>`**
  is now the only way a log row (or the header) is built. Because it takes a
  fixed-size **array**, a branch that emits four cells is a *compile error* at
  the call site instead of a paint-time abort at the user's desk. All four
  producers go through it.

## 3. Paint-level regression test

`crates/git_graph/src/git_graph.rs::tests::test_the_local_changes_row_paints_alongside_the_commit_rows`.

The blind spot was structural, not an oversight: `setup_graph_with_git_panel`
builds the graph with `cx.new_window_entity` and never adds it to a pane, so
the table is never rendered — every existing test that sets
`with_local_changes = true` sets it on state and stops there. The new test
adds the graph to the active pane, drives real frames, and asserts against the
painted tree via `VisualTestContext::debug_bounds`, both sides:

- flag **on** → `GIT-GRAPH-LOCAL-CHANGES-ROW` painted, `GIT-GRAPH-COMMIT-ROW-0`
  painted, and the synthetic row's `origin.y` is above the newest commit's;
- flag **off** → synthetic row gone, commit row still painted (otherwise
  "absent" would also pass on a graph that paints nothing at all).

Two new `debug_selector`s carry it: a constant one on the synthetic row's cell
and `GIT-GRAPH-COMMIT-ROW-{data_idx}` on the commit Description cell.
`debug_bounds` takes a `&'static str`, so the test leaks the one derived
selector it needs (one leak, commented).

### Mutation table — each applied, run, reverted

| # | mutation | result |
|---|---|---|
| 1 | Re-introduce the bug: fourth `empty_cell()` in the local-changes branch (with `log_table_row` widened to `const N` so it compiles) | **FAILED** — `data_table.rs:54` `assertion left == right failed: rendered table row has the wrong arity, left: 4, right: 3` |
| 2 | Drop the `debug_selector` on the local-changes cell | **FAILED** — *"the Pencil toggle paints the synthetic Local Changes row"* |
| 3 | `has_local_changes_row()` ignores `with_local_changes` (row always present) | **FAILED** — *"toggling the Pencil back off removes the synthetic row"* |
| 4 | Don't add the graph to a pane — i.e. reproduce the exact blind spot that let this ship | **FAILED** — *"precondition: the commit rows are painted…"* |
| — | unmutated | **ok. 1 passed** |

Mutation 4 is the load-bearing one: it shows the test's value comes from
painting, not from the assertions' wording. Mutation 1 shows the guard fires
on the original defect.

## 4. The crash-loop, and the `panic!` in `crates/ui`

**Is anything else in the serialized state able to wedge a relaunch?** I read
the full serialize/restore pair (`git_graph.rs` ~4210 / ~4600). Restored
fields: `log_source_{type,value}`, `log_order`, `selected_sha`, the four search
fields, `filters.{branches,authors,paths,date_since,date_until,all_refs}`,
`highlights.{my_commits,new_since_refresh,last_seen_sha}`,
`view_options.{compact_refs,group_by_date}` and
`file_history_options.{follow_renames,with_local_changes,show_inline_diff}`.
Of these:

- `with_local_changes` was the only one with a paint-time abort. Closed.
- `show_inline_diff` is a documented v1 **stub** — it toggles a button and
  nothing renders from it, so it cannot wedge anything today. It is the one to
  re-audit when the inline-diff rendering refactor lands, because it will then
  be a serialized flag that changes what the row builder emits — the same
  shape as this bug.
- Everything else either becomes `git log` arguments (a bad value yields an
  empty log, not a panic) or is read through `.get()` / `.unwrap_or()`.
  `compact_refs`'s slice `&commit.data.ref_names[visible..]` is guarded by the
  `total > compact_threshold` test that produced `visible`.

So the serialized state is clean now. What it is *not* is structurally safe:
nothing stopped the next such bug except the review that caught this one.

**Should the `panic!` be a `debug_assert!` plus a degraded render?** I made a
narrower version of that change, and it is the one thing here that touches a
shared component, so — as asked — here is the why.

I did **not** touch `TableRow::from_vec`. Its panic is the point of the type:
its callers construct rows eagerly, at a site they control, where a panic is a
usable stack trace pointing at the bug.

I did change the **two lazy paint paths** in `crates/ui/src/components/data_table.rs`
(`Table::uniform_list` and `Table::variable_row_height_list`) to route their
per-frame closure output through a new `coerce_rendered_row`, which logs,
`debug_assert_eq!`s, and then pads/truncates to `cols`. The distinction that
justifies it: those closures run *during layout, on every frame*, on data the
table cannot validate when it is built. That is the only place where an arity
bug becomes (a) unreachable by construction-site review, (b) fatal in release,
and (c) — when the state that produced the bad row is serialized —
**unrecoverable without deleting the user's runtime dir**. Turning an editor
that cannot be started into a wrong-looking row plus a `log::error!` is the
right trade at that specific site and nowhere else.

Honest caveat: `debug_assert_eq!` is live in dev and test builds, so a **debug**
editor still aborts (and my mutation-1 run is exactly that abort). That is
deliberate — dev builds should be loud — but it means the release editor is
the one that gets the crash-loop protection. If you'd rather have the
protection everywhere, the change is dropping the `debug_assert_eq!` and
keeping the log; say the word and I'll do it. If you'd rather `ui` stayed
untouched, reverting `coerce_rendered_row` back to `into_table_row` is a
two-line revert and the `git_graph` fix stands on its own.

## 5. Verification in the probe, after the fix

Fresh `--runtime-dir /tmp/pencil-crash-probe2`, same script, same click.

- The editor **survived** the Pencil click.
- Screenshot: `.agents/reports/2026-09-03-pencil-crash-after.png` (read it
  myself). Under the Description/Date/Author header there is now a first row
  reading **"Local Changes"** in accent blue with a small pencil glyph, its
  Date and Author cells empty; below it the three commits, shifted down by
  one, unchanged — `commit 3` still carrying its `✓ HEAD -> main`,
  `origin/main` and `origin/HEAD` chips, all three with `03 Sep 2026 23:28` /
  `Tester`. The Pencil button in the toolbar is drawn in its toggled-on state.
- Then: killed the editor, **relaunched on the same runtime dir**, and called
  `solutions.open` — the previously fatal path. It restored the file-history
  item with `with_local_changes = true` and painted the same row. No abort.
  The crash-loop is closed at the source, not merely survivable.

## 6. Gates

| gate | result |
|---|---|
| `CARGO_BUILD_JOBS=4 cargo build --bin sawe` | exit 0, 0 `^error`, 0 `^warning` |
| `CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets` | exit 0, 0 `^error`, 0 `^warning` |
| `CARGO_BUILD_JOBS=4 cargo test -p git_graph -p git_ui -p ui` | exit 0 — git_graph 84, git_ui 403, ui 48+39, 0 failed |

`rustfmt` was run on the two touched files; it also normalised one signature
left unformatted by the preceding commit. `target/release-fast` was neither
rebuilt nor deleted.

## 7. Sentence for `FORK.md` (not applied)

> The git-graph log table's column count is a single `LOG_COLUMN_COUNT`
> constant and every row — header, commit, the "no commit here" fallback and
> the synthetic *Local Changes* row — is built through
> `log_table_row([_; LOG_COLUMN_COUNT])`, because `Table` converts rendered
> rows with `TableRow::from_vec`'s un-gated `panic!`: a row with the wrong
> number of cells aborts the editor during layout in release too, and when the
> state that produced it is serialized (as `with_local_changes` is) the abort
> repeats on every relaunch until the runtime dir is wiped. The array argument
> makes that a compile error at the call site; `ui`'s two lazy row-rendering
> paths additionally degrade rather than abort, since a closure that runs every
> frame cannot be checked when the table is built.

## Suggested `.rules` additions

None from me as a rule — but the mechanism is worth a line wherever paint tests
are discussed: **a test that only sets view state never reaches the render
closure.** `setup_graph_with_git_panel`-style fixtures build the view entity
without adding it to a pane, so `render_*` is never called; a flag-flipping
test can pass green while the flag crashes the editor on the first frame. If
the flag changes what gets *painted*, the test has to put the view in a pane.
