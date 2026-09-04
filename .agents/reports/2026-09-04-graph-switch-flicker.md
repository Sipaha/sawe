# The git graph's blank frame on a project switch

Report: «немного глаз мозолит смена git дерева при переключении проектов. Сначала
пропадает старое дерево (пустая панель) и только потом новое содержимое рисуется.»

## 1. What it actually was — measured, not assumed

`GitGraphPanel::set_active_repo` dropped the `Entity<GitGraph>` for the old repository
and installed a freshly-constructed one for the new one *in the same effect cycle*. The
new graph starts its `git log` in its constructor, so every frame painted between the
switch and the arrival of the first commits shows the graph's "Loading" placeholder —
an empty panel. This is **not** a one-frame clear and **not** a slow `git log`: it is a
short, real wait, several painted frames long.

### Fixture

`script/run-mcp --debug --headless --runtime-dir /tmp/graph-flicker-probe`, a Solution
with two members:

| member | repo | commits reachable from all refs | `git log` warm, by hand |
|---|---|---|---|
| `smallrepo` | synthetic | 30 | 13 ms |
| `bigrepo` | full clone of this fork's own history | **79 531** | 241 ms |

Method: temporary `log::info!` with microsecond timestamps in `GitGraph::render`
(commit count + `is_loading`), in `GitGraphPanel::set_active_repo`, and on every
`RepositoryEvent::GraphEvent`. All instrumentation was reverted before committing; the
build in the tree carries none of it.

### Before (cold log cache, switch to the 79 531-commit repository)

```
      0.0ms  solutions.set_active_member returns
      1.3ms  panel: set_active_repo Some(repo 2) -> Some(repo 1)
      3.8ms  graph: fetch_initial_graph_data repo=1        <- git log starts
     15.4ms  graph: render repo=1 count=0 loading=true     <- BLANK FRAME 1
     65.2ms  graph: render repo=1 count=0 loading=true     <- BLANK FRAME 2
    111.6ms  graph: render repo=1 count=0 loading=true     <- BLANK FRAME 3
    149.1ms  CountUpdated(1000)                            <- first commits
    159.1ms  graph: render repo=1 count=2000               <- first content
```

**~144 ms of blank, three painted frames** in the headless build; on a 60 Hz display
that is ~9 frames. The 30-commit repository blanks for **~36 ms, one painted frame**
(`+19.9ms` blank render, `+44.5ms` `CountUpdated(30)`, `+55.7ms` first content).

Which entity clears: the panel, not the graph. The graph never "clears its rows" — it
is thrown away and a rowless one takes its place. And it is a real wait on `git log`
(process spawn + first 1000-commit chunk), not a scheduling artefact.

**A warm cache does not blank at all.** `Repository::graph_data` memoises per
`(source, order, args, paths)`, so a switch *back* to a project visited earlier in the
session paints its rows on the first frame. The cache is evicted on
`HeadChanged` / `BranchListChanged` / `TagListChanged` and after a push rescan, so the
blank returns after any of those — which is why it reads as "every time" in daily use
rather than "only the first time".

## 2. The fix, and why this shape

`crates/git_graph/src/git_graph_panel.rs`: the panel now builds the incoming
`GitGraph` immediately (so its `git log` starts at the same instant it used to) but
**keeps painting the previous project's graph until the incoming one can paint
something**. `PendingGraph` holds the off-screen graph, a subscription to a new
`GraphViewEvent::LoadSettled`, and a hold-expiry task.

Chosen over the alternatives because the wait is 40–150 ms: a placeholder that "is not
empty" would still be a flash of *something else*, and dimming plus a spinner for
150 ms is more visual noise than the bug. This is also what the git panel's Changes
list already does (`update_visible_entries` swaps atomically after a debounce), so the
two neighbours now behave alike.

Three guards, because "cache the old rows" is exactly how this becomes a worse bug:

- **A switch that fails does not keep the old history.** `LoadSettled` is emitted for
  `CountUpdated`, `FullyLoaded` *and* `LoadingError`, so an erroring log replaces the
  previous project's commits with its own error state.
- **The hold is bounded** — `STALE_GRAPH_HOLD = 400 ms`, ~2.7× the measured cold-cache
  worst case above. Past it the panel shows the incoming graph's honest "Loading"
  state rather than another project's history indefinitely (hung `git log`, repository
  removed mid-switch).
- **Nothing is delayed that used to be instant.** A warm log
  (`GitGraph::load_settled`, answered from the repository's cache rather than from the
  view's own rows, which a never-rendered view does not have yet) installs
  synchronously, as does "no repository at all", as does the first graph in a panel
  with nothing on screen to protect.

Switching back to the project still on screen mid-hold cancels the wait instead of
promoting a graph the user has navigated away from.

### After (same cold-cache switch, same fixture)

```
      3.5ms  panel: holding previous graph, pending repo=2
      8.7ms  graph: render repo=1 count=30    <- previous project's rows
     56.2ms  graph: render repo=1 count=30
    119.4ms  graph: render repo=1 count=30
    167.5ms  graph: render repo=1 count=30
    205.6ms  panel: install_graph Some(repo 1) -> Some(repo 2)   <- swap
    240.6ms  graph: render repo=2 count=14000 <- new project's rows, never empty
```

**Zero frames with an empty panel.** The swap lands well inside the 400 ms cap.

### Files

| file | change |
|---|---|
| `crates/git_graph/src/git_graph_panel.rs` | `PendingGraph` + `set_active_repo` / `promote_pending` / `install_graph`; `STALE_GRAPH_HOLD`; five tests |
| `crates/git_graph/src/git_graph.rs` | `GraphViewEvent::LoadSettled` + emit; `GitGraph::load_settled`; `GRAPH_PLACEHOLDER_SELECTOR` on the empty/loading placeholder |
| `crates/project/src/git_store.rs` | `InitialGitGraphData::is_loading()` |
| `crates/fs/src/{fs.rs,fake_git_repo.rs}` | `FakeFs::block_graph_load` / `release_graph_load` — a `git log` a test can hold in flight |
| `TODO.md` | § C12 |

## 3. The neighbours

- **Changes list (git panel dock)** — *no blank, nothing to fix.*
  `ActiveMemberChanged` → `schedule_update` → after `UPDATE_DEBOUNCE`,
  `update_visible_entries` (`crates/git_ui/src/git_panel/changes_list.rs:100`) clears
  and refills `entries` inside one synchronous call, from the git store's in-memory
  status snapshot. No intermediate paint; the old list stays until the new one is
  complete.
- **Uncommitted Changes (`ProjectDiff`)** — *same shape, different owner, left alone,
  recorded as `TODO.md` § C12.* Its subscription calls `BranchDiff::set_repo`, which
  drops `tree_diff` and emits `FileListChanged`; `refresh` then removes every excerpt
  the incoming repo does not share. Whether that is visible depends on how much of the
  incoming repository's status is already in the snapshot — **I read this from the code
  and did not measure it.** Not the same fix: the graph can swap whole view entities,
  the diff view owns one multibuffer whose excerpts *are* the content.
- **File history (`LogSource::Path`)** — unaffected. It is a pane item pinned to the
  repository it was opened for and has no `ActiveMemberChanged` subscription.
- **Commit view** — observed live: an explicitly opened commit tab keeps showing the
  commit from the project you opened it in after a switch (screenshot below). That is
  pane-item pinning, not a blank, and it is pre-existing; not touched.

## 4. Tests

Five in `git_graph_panel::tests`, all paint assertions via
`VisualTestContext::debug_bounds` after a real frame — not predicate assertions on
`panel.active_repo_id`, which the *old* code would have passed (it re-pointed the panel
correctly; it just painted nothing while doing it).

The harness gap that made the honest test possible: `FakeFs::block_graph_load` parks
the fake `git log` on a channel, so `run_until_parked` draws the frame **without**
resolving the load. Without it every load resolves inside the same `run_until_parked`
that starts it and the intermediate state is unobservable.

| test | asserts |
|---|---|
| `switching_projects_paints_the_old_graph_until_the_new_log_lands` | during the held load the old repo's row is painted, the new repo's second row is not, no placeholder; after release the new row is painted |
| `a_failed_switch_drops_the_previous_projects_rows` | an erroring log replaces the old rows with the placeholder |
| `a_log_that_never_resolves_gives_the_panel_back` | past `STALE_GRAPH_HOLD` the old rows are gone, the placeholder is up |
| `a_warm_log_is_shown_without_waiting` | a cached log installs with no `pending` |
| `switching_back_mid_hold_cancels_the_pending_graph` | the abandoned load does not swap itself in later |

### Mutation table

Every mutation was applied to the real tree, run, and reverted. None survived.

| # | mutation | result |
|---|---|---|
| M1 | `set_active_repo` always installs immediately (`if true \|\| …`) — i.e. the old behaviour | **died**: 4 tests fail; the flicker test fails on *"the previous project's rows must still be painted while the incoming log is in flight"* |
| M2 | `GitGraphEvent::LoadingError` returns before `cx.emit(LoadSettled)` | **died**: `a_failed_switch_drops_the_previous_projects_rows` — *"a switch that errors must not leave the previous project's commits on screen looking current"* |
| M3 | hold-expiry task awaits the timer but never promotes | **died**: `a_log_that_never_resolves_gives_the_panel_back` — *"past the cap the previous project's rows must be gone"* |
| M4 | switching back mid-hold leaves `pending` armed | **died**: `switching_back_mid_hold_cancels_the_pending_graph` — *"the wait is dropped"* |

## 5. Verification

| gate | result |
|---|---|
| `CARGO_BUILD_JOBS=4 cargo build --bin sawe` | exit 0, zero `^error` / `^warning` |
| `CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets` | exit 0, zero `^error` / `^warning` |
| `cargo test -p git_graph -p git_ui` | 89 + 403 passed, 0 failed |
| `cargo test -p fs -p project --lib` | 3 + 41 passed, 0 failed |
| `script/clippy -p fs -p project` | exit 0 |
| `script/clippy -p git_graph` | blocked by a **pre-existing** failure in `git_conflict_ui` (`std::process::Command::output` is a disallowed method, `66dcd04c37`); confirmed identical on a stashed tree |
| `cargo fmt` | clean |
| live, isolated instance | `script/run-mcp --debug --headless --runtime-dir /tmp/graph-flicker-probe`, two-member Solution, cold and warm switches both ways |

### Screenshots

- `.agents/reports/2026-09-04-graph-switch-before.png` — the graph on `smallrepo`
  before the switch.
- `.agents/reports/2026-09-04-graph-switch-after.png` — after switching to `bigrepo`:
  the tab strip is on `bigrepo`, the graph shows this fork's history, nothing stale.

**What the screenshots do and do not prove.** They show the switch ends correctly and
that no stale content survives it. They **cannot** show the intermediate frame:
`workspace.screenshot` renders the retained scene without running a draw, and each
capture costs ~950 ms in a debug build, during which the editor's main thread cannot
paint at all — a capture loop starves exactly the frames under test (this is visible in
the first probe run, where the app painted nothing for 2 s while screenshots ran). The
intermediate-state evidence is therefore the timestamped log above and the paint tests,
not a PNG.

## 6. Sentence for `FORK.md` (not applied)

> **Project switches hold the previous git graph until the new log lands.**
> `GitGraphPanel` builds the incoming `GitGraph` immediately but keeps painting the
> outgoing one until that graph's `git log` produces rows, finishes empty, or fails
> (`GraphViewEvent::LoadSettled` / `GitGraph::load_settled`), capped at
> `STALE_GRAPH_HOLD` (400 ms) so a hung log falls back to an honest "Loading" state
> rather than leaving another project's history on screen. Swapping the entity
> immediately, as it used to, flashed an empty panel for as long as the log took —
> measured at ~144 ms and three painted frames on a 79 531-commit repository with a
> cold cache, ~36 ms and one frame on a 30-commit one.
