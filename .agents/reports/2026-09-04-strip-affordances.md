# Session strip: a visible reopen button, and a rule closing the AI group off

Two maintainer requests against the status-bar AI session strip
(`crates/solution_agent/src/session_tab_strip.rs`):

1. «мы еще потеряли кнопку для восстановления закрытых сессий. Думаю её надо
   рядом с + сделать» — the reopen-a-closed-session affordance must be a
   **visible button next to `+`**.
2. «плюс разделитель вертикальный добавить между вкладками и кнопками
   относящимися к ИИ диалогам и остальными кнопками слева» — a **vertical
   separator** between the AI-dialog group and the status bar's other
   left-hand buttons.

## One correction to the brief, before anything else

The dispatch brief said the rule "must sit between the AI group and the items
to its **left**". There are none. `crates/zed/src/zed.rs::initialize_workspace`
registers `SessionTabStrip` **first** in the left group, and
`StatusBar::render_left_tools` paints left items in registration order, so the
strip is the **leftmost** status-bar item and search / LSP / diagnostics / file
name / merge conflicts / activity indicator all sit to its **right**.

Both phrasings — the maintainer's «между … и остальными кнопками слева» and the
brief's — name the same single boundary: where the AI group ends and the rest of
the left group begins. That boundary is the group's **trailing** edge, so the
rule is painted there. Verified in the screenshots: the rule falls between the
history button and the search magnifier.

## What changed

`crates/solution_agent/src/session_tab_strip.rs`

| | |
|---|---|
| **New** `render_reopen_button` | `IconButton` with `IconName::HistoryRerun`, `IconSize::Small`, `Color::Muted`, tooltip `REOPEN_TOOLTIP`. Click → `open_reopen_session_modal(weak_workspace, solution_id, …)` — the same entry point the deleted `+` menu entry called, so the picker itself is untouched. Painted immediately right of the `+`. |
| **Removed** the `+`'s right-click menu | `build_plus_menu` deleted; `render_plus_button` is now a bare `IconButton` with no `right_click_menu` wrapper and no `solution_id` / `weak_workspace` parameters. Two paths to one action is what let the tooltip, the menu entry and the close prompt drift apart in the first place. Left click on `+` still creates a session in one click. |
| **New** `render_group_divider` | `div().h(ButtonSize::Default.rems()).px_1()` hosting `ui::Divider::vertical().color(DividerColor::Border)`. Painted as a **sibling** of the scrolling tab group, not its last child. |
| `PLUS_TOOLTIP` | `"New AI Session — right-click for more"` → `"New AI Session"`. |
| `REOPEN_ENTRY_LABEL` → `REOPEN_TOOLTIP` | Same string (`"Reopen Closed Chat…"`), now the button's tooltip; still the single source the close prompt quotes. |
| `REOPEN_GESTURE` | Deleted — there is no gesture left to name. |
| `close_prompt_detail()` | `…the tab can be brought back with the session strip's "Reopen Closed Chat…" button, next to "+".` Still assembled from `REOPEN_TOOLTIP`, so it cannot drift. |
| `GROUP_DIVIDER_SELECTOR` | New const, shared by the `debug_selector` and the paint tests. |
| Module doc | Rewritten for the new affordance layout + the rule. |

`crates/console_panel/src/panel.rs` — one stale doc-comment pointer
(`session_tab_strip::build_plus_menu` → `render_reopen_button`). No code change.

### Why `IconName::HistoryRerun`

It is the fork's existing "go back through history" glyph — a clock with a
counter-clockwise arrow (`assets/icons/history_rerun.svg`). Already used by
`file_finder` for history matches, `git_ui::undo_modal`'s list header, the
debugger's back-in-history button, `tasks_ui`'s recent list. A second
`+`-family icon would have read as "new", which is the neighbouring button's
job.

### Why the rule is a sibling of the scroll region, not a child of it

The strip's tab row is `overflow_x_scroll()`. A divider placed as its last
child slides out of view once enough tabs are open — a boundary marker that
scrolls away is worse than none. So `render` now returns
`h_flex().child(group).child(render_group_divider())`, where `group` is the old
scrolling row.

`Divider::vertical()` is `w_px().h_full()`, and `h_full` is inert everywhere in
this strip (no ancestor has a definite height — the same reason the tab pills
set `.h(ButtonSize::Default.rems())` explicitly). The wrapper carries that
height, so the rule spans exactly the row the tabs and buttons occupy and
cannot change the bar's height. The wrapper also carries the `debug_selector`:
`Divider` is a `RenderOnce` with no `InteractiveElement`.

### Height budget

Nothing new is taller than `ButtonSize::Default.rems()`. `STATUS_BAR_HEIGHT`
(33px), `STATUS_BAR_UI_SCALE` (1.1) and `BAND_RESERVED_HEIGHT` are untouched;
`workspace::status_bar::tests::the_status_bar_paints_at_its_height_with_a_scaled_subtree`
and
`solution_agent::solution_band::tests::the_band_reserve_is_derived_from_the_live_status_bar_height`
pass unmodified. Three of the new/updated paint assertions independently pin
`px(22.)` (`ButtonSize::Default` at the test window's 16px rem) for the `+`, the
reopen button and the rule.

## Tests

Updated (not deleted):

| Old | New | Change |
|---|---|---|
| `left_clicking_the_plus_creates_a_session_and_opens_no_menu` | `left_clicking_the_plus_creates_a_session_and_opens_no_picker` | The "opens no menu" side became "opens no *picker*" — asserted through `Workspace::active_modal_kind`, since the two neighbouring buttons are what could now be crossed. |
| `right_clicking_the_plus_reopens_a_closed_chat_even_with_no_tabs` | split in two: `the_reopen_button_opens_the_picker_even_with_no_tabs` + `right_clicking_the_plus_opens_nothing` | The zero-tabs case it existed to cover is kept verbatim and re-pointed at the button; the removed gesture gets its own both-sides test. |
| `the_close_prompt_and_the_tooltip_point_at_the_same_affordance` | same name | Still pins prompt-vs-tooltip agreement; now also pins that neither string still promises `right-click`, and that the two buttons have distinct tooltips. |

New: `the_group_divider_paints_beside_the_ai_group` (rule paints, is to the
**right** of the `+`, is 22px tall) and
`the_group_divider_is_absent_when_the_strip_has_no_ai_group` (rule absent on the
`active_solution_id` early-return branch — with `ICON-Plus`/`ICON-HistoryRerun`
asserted absent so the test is meaningful).

Every paint assertion reads `VisualTestContext::debug_bounds` after a real
frame. The one thing `debug_bounds` cannot answer — "did a modal open" — is read
off `Workspace::active_modal_kind`, after a real synthetic click at the button's
**painted** bounds.

`paint_strip_with_no_tabs` now also returns the `Entity<Workspace>` the strip
hosts its modals on (the MultiWorkspace's active workspace, a different window
from the harness that paints the strip).

## Mutation table

Every mutation was applied to the shipped source, the suite run, then reverted.

| # | Mutation | Expected to die | Result |
|---|---|---|---|
| 1 | Drop `.child(self.render_reopen_button(…))` from `render` | `the_reopen_button_opens_the_picker_even_with_no_tabs` | **died** (only that one) |
| 2 | Drop `.child(render_group_divider())` | `the_group_divider_paints_beside_the_ai_group` | **died** (only that one) |
| 3 | Also paint the rule on the `active_solution_id` early-return branch | `the_group_divider_is_absent_when_the_strip_has_no_ai_group` | **died** (only that one) |
| 4 | Gut the reopen button's `on_click` body | `the_reopen_button_opens_the_picker_even_with_no_tabs` | **died** (only that one) |
| 5 | Re-add a `right_click_menu` on `+` with a `REOPEN_TOOLTIP` entry | `right_clicking_the_plus_opens_nothing` | **died** (only that one) |
| 6 | Swap the rule to the group's **leading** edge | `the_group_divider_paints_beside_the_ai_group` (the `origin.x` claim) | **died** |
| 7 | `ButtonSize::Default.rems()` → `ButtonSize::Large.rems()` on the rule wrapper | `the_group_divider_paints_beside_the_ai_group` (the 22px claim) | **died** |

Known survivor, stated honestly: moving the rule *inside* the scrolling `group`
(as its last child) leaves every test green — it still paints, still to the
right of the `+`, still 22px. The reason it is a sibling is that it must not
scroll away with the tabs, and `debug_bounds` on an unscrolled strip cannot see
that. Catching it would need a strip driven past its horizontal overflow.

Temporary instrumentation (not a mutation, and reverted before the final build
and before the committed screenshots): three `log::info!("PROBE: …")` lines in
`open_reopen_session_modal`, used to prove the click reached the handler after a
first live attempt appeared to do nothing. The cause was mine, not the code's —
I clicked x=269 while the button's painted bounds are `[270,1051,23,24]`, i.e.
one pixel outside its left edge, while `hover_at` at the same point still
resolved to it. Lesson for the next agent: take button coordinates from
`workspace.dump_visual_structure`'s `clickables`, not from a zoomed screenshot.

## Gates

| Gate | Result |
|---|---|
| `CARGO_BUILD_JOBS=4 cargo build --bin sawe` | clean, exit 0, no `^error` / `^warning` |
| `CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets` | exit 0, **0 errors, 0 warnings** |
| `CARGO_BUILD_JOBS=4 cargo test -p solution_agent -p workspace -p console_panel` | exit 0 — 33 + 777 + 1 + 1 + 255 passed, 0 failed |
| `script/clippy -p solution_agent -p console_panel -p workspace` | exit 0, 0 warnings |
| `session_tab_strip` unit tests | 18 passed |

## Live check

`script/run-mcp --debug --headless --runtime-dir /tmp/strip-probe` (isolated —
the maintainer's editor was never touched). Solution `StripProbe` with one empty
member; two sessions seeded via `solution_agent.seed_cold_session`; every
screenshot taken after driving a real `windows.hover_at`, since
`workspace.screenshot` renders the retained scene.

| Path | What it shows |
|---|---|
| `.agents/reports/2026-09-04-strip-bar-zoom.png` | 4× crop of the bar's left end with two sessions: `● refactor blame`, `● status bar strip`, `+`, the history glyph, **the rule**, then the search magnifier and the LSP check. |
| `.agents/reports/2026-09-04-strip-two-sessions.png` | Same state, full 1920×1080 window. |
| `.agents/reports/2026-09-04-strip-zero-sessions.png` | Full window with **zero** session tabs — the state a user who just closed their last chat is in. `+`, the history button and the rule all still paint; the recovery path is visible without hovering anything. |
| `.agents/reports/2026-09-04-strip-reopen-picker.png` | After a real session was created, closed through the tab's right-click → Close, and the history button clicked: the centred **"Reopen Closed Chat"** modal listing that chat (`Strip…`, "just now"). End-to-end on the shipping binary. |

Also confirmed live, not committed as images: hovering the history button shows
the tooltip **"Reopen Closed Chat…"** verbatim; right-clicking `+` opens
nothing; left-clicking `+` took the solution from 2 to 3 sessions.

### Does the rule read as structure or as noise at this size?

**Structure.** At 1× it is a 1px `border`-coloured line spanning the 22px button
row inside a 33px bar, with ~5px of clear space on each side (the wrapper's
`px_1` plus the strip's and the status bar's own `gap_1`). It is quiet enough
that it never competes with the icons, and long enough that the eye reads
"group ends here" rather than "stray pixel". Zoomed 4× it is unambiguously a
rule. The one thing I would watch: with the AI group at its widest (5 tabs) the
left group is `min_w_0().overflow_x_hidden()`, so the rule is inside the region
that clips — it will disappear before the search button does. That is correct
behaviour (a boundary is worth less than a destination) but worth knowing.

## Sentence for `FORK.md` (not applied)

> **Session strip affordances.** The status-bar AI session strip carries its
> reopen-a-closed-chat flow as a visible `IconName::HistoryRerun` button beside
> the `+` rather than as a right-click gesture on it (10120c6a27 tried the
> gesture; it was not discoverable), and closes the AI-dialog group off from the
> rest of the status bar's left items with a `Divider::vertical()` on the
> group's *trailing* edge — the strip is registered first in
> `initialize_workspace`, so "the other buttons on the left" are to its right.
> The rule is a sibling of the strip's `overflow_x_scroll` row, not a child, so
> it cannot scroll out of view; its wrapper supplies the
> `ButtonSize::Default.rems()` height that `Divider`'s own `h_full` cannot get
> from this strip's indefinite-height ancestors.

`FORK.md` also currently says the tooltip / prompt / menu entry are tied
together by `REOPEN_ENTRY_LABEL` / `PLUS_TOOLTIP` / `REOPEN_GESTURE`
(line ~3160). Two of those three names no longer exist; whoever edits `FORK.md`
should re-point that to `REOPEN_TOOLTIP` / `PLUS_TOOLTIP`.
