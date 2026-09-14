# Recovered 2026-09-03/04 UI work — the parts FORK.md does not carry

Twelve session reports lived in `.agents/reports/` until `5f44405591` removed that
directory (see its message for why). Their *decisions* had already been folded into
`FORK.md` by the consolidated docs pass — #139 (Commit-tab ref chips), #140 (blame runs),
#149 (the project strip is the user's layout), the `LOG_COLUMN_COUNT` rule, the folder-name
derivation, the `ghost tab` context menu. This note keeps what FORK.md does **not**: the
measurements, the hypotheses that were ruled out, the traps, and the concerns nobody has
answered yet.

Full text of any report: `git show a71e73602a:.agents/reports/<name>.md`
(the screenshots it references are in the same tree).

## Measurements that cost real instrumentation to get

**The git graph's blank frame on a project switch**
(`2026-09-04-graph-switch-flicker.md`). Measured with microsecond `log::info!` probes in
`GitGraph::render` / `GitGraphPanel::set_active_repo` / `RepositoryEvent::GraphEvent`, all
reverted before committing:

| repo | commits from all refs | warm `git log` | blank after a cold switch |
|---|---|---|---|
| synthetic | 30 | 13 ms | ~36 ms, **1** painted frame |
| this fork's own history | 79 531 | 241 ms | ~144 ms, **3** painted frames (~9 at 60 Hz) |

Three facts worth keeping: the panel clears, not the graph (the old `Entity<GitGraph>` is
dropped and a rowless one installed in the same effect cycle); it is a real wait on `git
log` (process spawn + the first 1000-commit chunk), not a scheduling artefact; and a **warm
`Repository::graph_data` cache does not blank at all** — it is memoised per
`(source, order, args, paths)` and evicted on `HeadChanged` / `BranchListChanged` /
`TagListChanged` and after a push rescan, which is exactly why the flicker reads as "every
time" in daily use rather than "only the first time".

**The project strip truncated at six tabs** (`2026-09-04-project-overflow.md`). Ink-run
analysis of the toolbar row off the running editor, 12 members:

- At 1920 px: six tabs occupied 35–780 px (mean **124.2 px/tab**), then **≈790 px of empty
  row** — 41 % of the width blank while six of twelve projects sat behind the `…`. At the
  measured mean width all six hidden projects would have fitted with ~50 px to spare.
- At 1000 px the ink runs were **byte-for-byte identical out to x=833** — the proof that the
  cap was independent of width.
- Ruled out along the way: `min_w(80)`/`max_w(200)` binding (painted widths were 111–154 px,
  strictly between); the ~10 % status-bar rescale (`e12527465a`, `0c569d6c95`, `2de70b530b` —
  they changed heights and font metrics; the cap was a literal `6` and predates them);
  unconditional room reserved for `…` (it was already gated on `!overflow.is_empty()`).
- Separately observed and **not** part of that fix: at 1000 px the trailing git widget,
  run-config strip and right-dock toggle were pushed entirely off the right edge.

## The trap worth remembering

From `2026-09-03-pencil-crash.md`, where a synthetic *Local Changes* row emitted four cells
into a three-column table and `TableRow::from_vec`'s un-gated `panic!` aborted the editor on
the first frame — and, because `with_local_changes` is serialized on the item, again on every
relaunch until the runtime dir was wiped:

> **A test that only sets view state never reaches the render closure.**
> `setup_graph_with_git_panel`-style fixtures build the view entity without adding it to a
> pane, so `render_*` is never called; a flag-flipping test passes green while the flag
> crashes the editor on the first frame. If the flag changes what gets *painted*, the test
> has to put the view in a pane.

`ui::ContextMenu` registers a `MENU_ITEM-{label}` debug selector for every entry, and
`ui::components::chip` a `CHIP-{label}` one, so menu and chip contents are paint-testable
through `VisualTestContext::debug_bounds` — that is the way to write the test that does.

## Still open

Verified against the tree at `5f44405591`, not merely copied from the reports.

- **No UI can delete a catalog project.** `DeleteCatalogProject` is registered in
  `solutions_ui::modals` (`modals.rs:64`) and has a modal, but **no dispatch site anywhere** —
  `grep` outside its own module finds only the registration. `EditCatalogProject` has exactly
  one, the failed-add ghost tab (`project_tab.rs:461`), so a catalog row added successfully
  and later needing a URL change is reachable only through the `catalog.*` MCP tools. The
  2026-09-04 report flagged this; `clear_failed_add` / `cancel_add_member` got their callers
  (`project_tab.rs:445,468,513`), the catalog-management surface never did.
- **`normalize_remote_url` runs on edit as well as add**, so a pre-existing catalog row whose
  URL would now be refused cannot be re-saved unchanged through the Edit modal. Intentional,
  but it is a behaviour change for any bad row already on disk.
- **`ref_chip_width` calls `solutions::branch_protection::check` once per chip per frame** —
  the same call `ref_chip` already makes, so a decorated commit pays it twice per chip per
  frame. A policy lookup, not I/O; if it ever shows up in a profile, hoist the glyph decision
  into the row and pass it to both.
- **`COMMIT_CONTAINMENT_EXPANDED_MAX_HEIGHT` (64 px) is not a whole number of chip lines**
  (19 px + 4 px gap), so the expanded ref block clips its last visible line mid-height.
  Inherited from the containment rows, which share the constant.
- **The blame gutter draws no hairline on the display's first row.** Ruled that a run head
  with nothing above it is not a boundary. If the line is wanted there anyway it is
  `matches!(position, Head | DocumentHead)` plus the two tests that assert its absence.
- **`MAX_RUN_PREDECESSOR_LOOKBACK` is a bound on work, not on correctness**, and nothing
  tests the give-up path: it needs 1024 consecutive display rows above the viewport that are
  all wraps or spacers. If it fires, the top row renders as it did before #140.
- **The project strip's `…` marker colour is asserted by no test** — only its state. A
  refactor swapping `Color::Accent` for something else passes the suite. The screenshots were
  the cover, and they are now only in history.
- **`Indicator`'s border override is a silent coupling.** The strip's marker overrides the
  default `elevated_surface_background` with `title_bar_background`, because that is what
  `title_bar::project_toolbar` paints the row with. If the toolbar's background changes, this
  has to follow — nothing links them at compile time.
- **The open `…` popover renders the overflow list captured at open time**, so it goes stale
  if the order changes underneath it. Pre-existing, harmless in practice (any interaction that
  would reorder also dismisses the menu).
- **No way to point a Solution at an existing local checkout.** Every agent that wants to
  drive a real repository's history has to clone it through the catalog first. A
  `solutions.add_local_member` would pay for itself in probe fixtures alone.

## Contracts that live in tests, not here

Two reports were pinned input→output tables, and the tests are the durable form — named here
only so the next reader knows where to look rather than re-deriving the rules:
`crates/solutions/src/folder_name.rs::tests::unified_rule_table` with the cross-path
invariant `store::lifecycle::tests::create_and_rename_derive_the_same_folder` (create and
rename must produce the same folder, and both must equal `derive(name)`), and the seven
camelCase boundary tests in `crates/solutions/src/slug.rs`. The cases that motivated them —
`UpdateDeps` → `Update-Deps`, `ECOSRecords` → `ECOS-Records`, `ecosV2` → `ecos-V2`,
`Мой Проект` → `Мой-Проект` where the old path produced `repo-{hash}` because nothing ASCII
survived, and `Sawe` / `sawe` no longer collapsing onto one folder — are in those tables.
