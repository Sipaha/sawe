# Diff readability arc — gutter diet, IDEA connectors, blame, soft wrap

Status: **in progress**
Owner: supervisor session 2026-08-12
Source: operator request (7 numbered items), decisions taken inline in that session.

---

## Why

The side-by-side diff wastes ~200 px of horizontal space on two line-number
columns, offers no visual cue for *what turned into what*, and inherits soft
wrap from whichever file happens to sort first in the changeset. This arc
fixes the whole cluster.

All geometry numbers below were measured off a live `--headless` debug
instance driving a real three-hunk diff, not estimated.

---

## Decisions (locked)

| # | Decision | Rationale |
|---|---|---|
| D1 | Connector ribbons live in a strip **between the two line-number columns** (layout A). | Verified against a real IDEA screenshot: IDEA's order is `[text][numbers][ribbon ~35px][numbers][text]`. Sawe already sweeps both gutters to the divider (`element.rs:9475`), so this is the smaller change *and* the faithful one. |
| D2 | The ribbon strip is paid for by reclaimed gutter padding, **not** by widening the centre. | Operator: "мусорное место надо убирать". 204 px → ~128 px total, of which 36 px is new ribbon strip. |
| D3 | Breakpoint dot renders **in the line-number cell**, replacing the number; the dedicated breakpoint column is removed. | IDEA model, confirmed from operator screenshot (line 30 shows a dot and no number). |
| D4 | Runnables (▶) and bookmarks move to the **icon column right of the numbers** (where crease toggles already live). | Same IDEA screenshot: ▶ sits right of the numbers, so D3 creates no collision. |
| D5 | Git blame in the diff is implemented for **both** panes. | Operator: "давай все делать". |
| D6 | Soft wrap: fix the root cause (per-excerpt language settings) **and** add a toolbar toggle. | A toggle without the fix would be a whole-diff switch masquerading as a per-file one. |

---

## Measured baseline

Live split diff, default theme, default font:

| region | width | what is actually painted there |
|---|---|---|
| left gutter | 98 px | hunk status strip (6 px), expand-excerpt button, line number (~20 px) |
| divider | 1 px | — |
| right gutter | 104 px | mirror of the left |
| **centre total** | **204 px** | for two ≤2-digit numbers |

Waste identified in `Editor::gutter_dimensions` (`crates/editor/src/editor.rs:11339`):

- `right_padding = ch_width * 3.0` when `!is_singleton && show_line_numbers`
  (`editor.rs:11414`). It exists for fold chevrons, but
  `shows_folds = is_singleton && gutter_settings.folds` (`editor.rs:11410`) is
  **always false in a multibuffer**. ~28 px × 2 sides of pure dead space.
- `left_padding = ch_width * 4.0` when `!is_singleton` (`editor.rs:11397`) —
  holds the expand-excerpt button, so it can shrink but not vanish.
- In singleton editors `left_padding = ch_width * 3.0` is reserved for
  "runnables, breakpoints and bookmarks … shown in the same place"
  (`editor.rs:11400`). D3/D4 free this.

No border is painted between gutter and text. `editor_gutter_background` *is*
filled (`element.rs:4837`) but in the default theme it is ~equal to the editor
background, and hunk tinting floods the gutter along with the text — hence the
operator's "панели сливаются с контентом".

---

## Workstreams

### G — gutter diet + breakpoint relocation + border (items 3, 4, 7)

Files: `crates/editor/src/editor.rs` (`gutter_dimensions`, `GutterDimensions`),
`crates/editor/src/element.rs` (gutter layout/paint, breakpoint & hover-button
layout), `crates/editor/src/editor.rs` gutter context-menu deploy sites.

1. Drop the fold reservation for non-singleton editors: `right_padding` must be
   ~1 ch when folds cannot render. Keep singleton behaviour unchanged.
2. Trim `left_padding` in multibuffers to what the expand-excerpt button
   actually needs.
3. D3: render the breakpoint indicator (both the set state and the dim hover
   preview) in the line-number cell, hiding the number for that row. Hover
   preview arms on hovering the **number cell**, not the whole gutter row.
4. D4: runnables/bookmarks render in the crease/icon column right of numbers.
5. Remove the now-empty breakpoint column from `left_padding` for singletons.
6. Paint a 1 px border on the gutter↔text boundary. In split mode the LHS
   gutter is right-aligned, so its border is on the **left**; the RHS gutter's
   border is on the **right**. Use `theme.colors().border_variant`.

Acceptance: screenshots of (a) normal editor, (b) unified diff, (c) split diff
before/after; centre chrome ≤ ~92 px for two gutters; breakpoint set + hover
both render on the number; `cargo test -p editor` green.

### R — IDEA connector ribbons (item 2)

Files: `crates/editor/src/split_editor_view.rs` (new element + strip in the
`h_flex`), possibly a small `pub(crate)` accessor on `Editor`.

- Widen the 1 px divider (`RESIZE_HANDLE_WIDTH` area, `split_editor_view.rs:110`)
  into a ~36 px connector strip that still hosts the drag handle.
- New `impl Element` sibling to `SplitBufferHeadersElement`
  (`split_editor_view.rs:264`) — that type is the working template for reading
  a side's snapshot and painting by row.
- Hunk correspondence: match LHS↔RHS hunks by `diff_base_byte_range`
  (`translate_lhs_hunks_to_rhs`, `split.rs:153`) or the zip-by-order technique
  at `split.rs:1471`.
- Row→y: reuse the `diff_hunk_bounds` formula (`element.rs:5263`).
- Both sides share one scroll anchor and are row-count balanced with
  `Block::Spacer`, so hunk **starts** already align — the ribbon has a flat top
  and a sloped bottom. No new synchronization is required.
- Paint with `PathBuilder` (template: `HighlightedRange::paint`,
  `element.rs:10416`; colour-layering: `git_graph.rs:3023`), clipped to a
  content mask covering only the strip.
- Colour by `DiffHunkStatus` using `version_control_added` / `_deleted` /
  modified, matching the gutter strip colours (`element.rs:5186`).

Acceptance: screenshot of a diff with a 1→7 line modify, a 1→2 modify and a
1→0 delete, showing three correctly-shaped ribbons; ribbons track scrolling;
drag-to-resize divider still works.

### B — Git blame in the diff, both panes (item 5)

Files: `crates/editor/src/git/blame.rs`, `crates/project/src/git_store.rs`,
`crates/git/src/blame.rs`, `crates/git_ui/src/blame_ui.rs`,
`crates/editor/src/split.rs`, gutter context menu in `crates/editor/src/editor.rs`.

- RHS: excerpts point at real project buffers; `GitBlame` is already
  per-`BufferId` (`blame.rs:391`) and multibuffer-safe. Mostly a matter of
  letting the action through in diff panes.
- LHS is the hard half. `BufferDiff::base_text_buffer` is a detached in-memory
  `language::Buffer` with no `File`, so
  `GitStore::repository_and_path_for_buffer_id` (`git_store.rs:2208`) returns
  `None` and blame silently produces nothing. Additionally
  `Repository::blame_buffer` (`git_store.rs:1504`) always blames HEAD against
  supplied content — there is no revision parameter, but the diff base is not
  necessarily HEAD.
- Required: a revision-aware blame path (`git blame <rev> -- <path>`) plus a way
  to associate an LHS excerpt with `(repo, path, base_rev)`.
- Add an "Annotate with Git Blame" entry to the gutter context menu
  (`Editor::gutter_context_menu`, `editor.rs:4092`) and make that menu
  deployable from a plain right-click on the number gutter — today it only
  deploys from breakpoint/bookmark affordances, which diff panes disable
  (`split.rs:521`).

Acceptance: blame column renders on both panes of a split diff of a file with
real history; left pane attributes to the base revision, not HEAD; unit test
for the revision-aware blame call.

### W — soft wrap (items 6a, 6b)

Files: `crates/multi_buffer/src/multi_buffer.rs`, `crates/editor/src/config.rs`,
`crates/git_ui/src/solo_diff_view.rs`, `crates/git_ui/src/project_diff.rs`,
`crates/editor/src/split.rs`.

**6a — root cause.** `MultiBuffer::language_settings`
(`multi_buffer.rs:2088`) resolves settings from the **first excerpt's buffer**
and applies them to the entire multibuffer; `Editor::soft_wrap_mode`
(`config.rs:250`) reads exactly that. Markdown defaults to
`"soft_wrap": "editor_width"` (`default.json:2418`) while TypeScript has no
override and the global default is `"none"` (`default.json:1605`).

Reproduced on a live instance:

| diff contents | long TS line |
|---|---|
| `wide.ts` alone | does not wrap ✔ |
| `README.md` + `wide.ts` | **wraps** ✘ |
| `wide.ts` opened as an ordinary file | does not wrap ✔ |

Fix: resolve soft wrap (and any other per-excerpt-sensitive setting) per
excerpt rather than once per multibuffer.

**6b — toolbar toggle.** No wrap toggle exists anywhere in the app; only
`ctrl-k z` and the palette. Add an `IconButton` (`IconName::TextWrap` /
`TextUnwrap`, idiom at `markdown.rs:2819`) to `SoloDiffStyleToolbar`
(`solo_diff_view.rs:567`) next to the Unified/Split toggles, and to
`ProjectDiffToolbar` (`project_diff.rs:1716`).

**Trap:** the button must dispatch through `SplittableEditor::toggle_soft_wrap`
(`split.rs:1023`), which copies `soft_wrap_mode_override` to the other side
(`split.rs:1044`). Calling `set_soft_wrap_mode` on one editor directly desyncs
the two panes' wrapped-row counts and breaks `Block::Spacer` row balancing.
Also needs a new public getter — `soft_wrap_mode` is `pub(super)`
(`config.rs:250`) and `soft_wrap_mode_override` is private, so `git_ui` cannot
read the state to drive `toggle_state`.

Dormant landmine to leave alone but not re-arm: `toggle_soft_wrap` has a
`SoftWrap::GitDiff => return` early-out (`config.rs:289`) that is currently
unreachable because `language_settings::SoftWrap` has no `GitDiff` variant.

---

## Shipped

- Item 1 — per-hunk Stage/Restore hover buttons hidden by default:
  `bc8467fca5`. `git.show_stage_restore_buttons` already existed; only the
  default flipped (4 sites). Verified with a control test on a live instance
  (forcing the setting back to `true` restores the buttons).
