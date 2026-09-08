# Activating a project tab can hide it: the fold follows the trailing git widgets

**Date:** 2026-09-08 · **Status:** fixed (option 1, the maintainer's pick) — FORK.md #160

## Symptom

Reported by the maintainer: *"когда выбираю последнюю видимую вкладку, то она прячется в `…`"* —
clicking the last visible project tab makes that tab disappear into the overflow menu, leaving no
tab highlighted and the `…` button wearing the accent dot.

## Root cause

`ProjectTabStrip` computes its fold from `measured_bounds` — the width of its own box, which is a
`flex_1 min_w_0` child of the `ProjectToolbar` row, i.e. **the row's leftover space**
(`crates/solutions_ui/src/project_tab_strip.rs`, `visible_count`).

The rest of that row is `flex_none`, and its width is a function of the **active** project:

| widget (`crates/title_bar/src/project_toolbar.rs`) | varies with |
|---|---|
| `render_branch_widget` | the active repo's branch **name** — `Label::new(name)` with no width cap |
| `render_update_button`, `render_push_button` | whether the active member has a repo; push also shows `↑ahead` |
| `render_repository_selector` | the active member's repo count / name (already capped at 140px) |
| run-config strip | configs are filtered by the active member (`run_config_ui::toolbar_strip`) |

So activating a member changes the leftover width, which changes `fit_count`, which moves the fold —
and the tab most likely to fall past it is the one that was last, i.e. the one the user just clicked.
`project_tab_strip.rs`'s header states the intended invariant ("the split is a function of exactly two
things — the stored order and the available width — and of nothing else. In particular the ACTIVE
member has no influence on it"). That invariant holds inside the strip and is broken one level up, in
layout.

## Evidence (live, headless probe)

Solution `TabFold`, five members, window 1100x800, `script/run-mcp --debug --headless
--runtime-dir /tmp/sawe-probe-tabs`, driven over the MCP socket:

* active = `ecos-process` (branch `main`) → **5 tabs visible, no `…`**
  (`/tmp/tabfold-user-before.png`)
* `solutions.set_active_member` → `ecos-webapp-ui` (branch `release/4.10.0-integration-fixes`) →
  **3 tabs + `…`**, the activated tab is inside the `…`, nothing highlighted
  (`/tmp/tabfold-user-after.png`) — the maintainer's screenshot exactly.
* Switching back to a `main` member restores all five tabs. The only thing that changed is the
  branch label: `workspace.dump_visual_structure` reports
  `BranchWidget { label: "main" }` → `BranchWidget { label: "release/4.10.0-integration-fixes" }`.

Note the trigger is not limited to clicking: anything that widens those widgets re-folds the strip.
A local commit makes the push button appear (`↑1`) and can hide a tab with no user gesture at all.

## Candidate fixes

1. **Structural (recommended).** Stop deriving the fold from leftover space. Give the strip a budget
   computed from a stable quantity — the row's own width (or `window.viewport_size()`) minus the
   strip's left edge minus a constant trailing reserve — and let the trailing cluster absorb the
   variation instead: `flex_1 min_w_0 justify_end` with truncating labels, with the strip
   content-sized. The fold then depends only on the window width and the member names. Costs: the
   branch name truncates when many tabs are open; touches the flex semantics of the whole row, which
   is what the current `flex_none` comment was written to protect.
2. **Fixed slot for the whole selection-dependent cluster.** Keep today's leftover-based budget and
   make the trailing cluster's footprint constant (`w(...)` + `overflow_hidden` + truncation), so the
   leftover cannot move. Simplest and preserves the row's existing mental model; costs a permanent
   ~300–400px reservation that repo-less projects and empty run-config strips waste.
3. **Fixed slot for the branch widget only** (`w(...)` + `truncate()`, the idiom the repository
   selector already uses at `max_w(px(140.))`). ~5 lines, kills the dominant unbounded term, but
   leaves the residual triggers above (repo/no-repo members, the push button appearing after a
   commit, run-config differences).

## What shipped

Option 1. `ProjectTabStrip::available_width` = window width − the strip's own left edge −
`TRAILING_RESERVE` (420px, measured off the row it describes); the strip is content-sized in the
toolbar row and the trailing cluster takes the slack (`flex_1 min_w_0 justify_end`) with the branch
label capped at 140px and truncating. FORK.md #160.

The regression test did not need the toolbar after all, and does not depend on which option was
picked: `a_trailing_widget_that_grows_must_not_move_the_fold` hosts the strip beside a trailing block
and grows that block by 240px — the width a long branch name costs the real row. It fails on the old
behaviour (7 tabs painted → 5) and passes on the new one. `STRIP_SELECTOR` stayed `pub(crate)`; what
changed is what it means (the tabs' own extent now, not the room they were given), so the two paint
tests that used it as a boundary re-derive the budget instead.

Verified live at 1100x800 on the same probe: activating the last visible tab (`ecos-model-lib`, on
`release/4.10.0-integration-fixes`) leaves it visible and highlighted, and the branch label truncates
to `release/4.10.0-in…` instead of taking the tab's slot. Measured cost of the reserve at that width:
one tab (five tabs with a short branch before, four now) — the price of a fold that no longer moves.
