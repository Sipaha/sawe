# Console panel `+` popover: drop the AI-session entries, move "Reopen Closed Chat…" to the session strip

Date: 2026-09-03 · Branch `main` · crates touched: `console_panel`, `solution_agent`

## The defect

The terminal panel's `+` popover still offered AI-session entries ("New AI Chat",
"Reopen Closed Chat…") after AI sessions moved to the status-bar session tab strip.
"New AI Chat" was pure duplication of the strip's own `+`; "Reopen Closed Chat…" was
the *only* entry point to `ReopenSessionModal` / `SolutionAgentStore::list_closed_sessions`,
and `SessionTabStrip::close_tab`'s confirmation prompt pointed at it by name.

## What was done

### R-1 — "New AI Chat" deleted

Removed from `ConsolePanel::render_plus_popover` (`crates/console_panel/src/panel.rs`).
No behaviour lost: the strip's `+` dispatches the same `console_panel::NewChat` action.
`has_active_solution` (only used to grey the two AI entries) went with it, as did the
module-level `use crate::actions::NewChat` (now `#[cfg(test)]`-scoped, since only the
`new_chat_action_matches_the_status_bar_strips_dispatch_string` pin test still names it).

### R-2 — "Reopen Closed Chat…" moved, not deleted

- `ConsolePanel::open_reopen_session_modal` (a method) became the free function
  `solution_agent::reopen_session_modal::open_reopen_session_modal(&WeakEntity<Workspace>,
  SolutionId, &mut Window, &mut App)`, living next to the modal it opens. The only
  mechanical change is `cx.spawn_in(window, …)` → `window.spawn(cx, …)`, because the
  free function takes `&mut App`, not `&mut Context<ConsolePanel>`.
- `console_panel` no longer imports `solution_agent::reopen_session_modal` at all.
- **New home: a `PopoverMenu` on the strip's `+`**, not the tab right-click menu.
  Reason: the strip's `+` is the only affordance that still paints when the Solution
  has **zero** session tabs — and zero tabs is precisely the state a user who just
  closed their last session is in, i.e. the one state the recovery path must survive.
  A tab context menu disappears with the tabs. The cost is one extra click on
  "New AI Session"; the alternative cost is losing the recovery path outright. The
  two-entry popover also mirrors the console panel's own `+` idiom, so it is the
  shape the maintainer already knows.
- `SessionTabStrip::close_tab`'s prompt now reads `… the tab can be brought back via
  "Reopen Closed Chat…" in this strip's "+" menu.` — the prompt is no longer a lie.
- The reopen entry is a no-op when the strip has no workspace handle
  (`workspace_weak` → `None`); it cannot be reached in that state anyway, since the
  whole strip renders empty without an active Solution.

### R-3 — console `+` popover kept

Still a `PopoverMenu`, now "New Terminal" (greyed to "New Terminal (no project)"
without a project) · separator · "Spawn Task… (Alt-Shift-T)".

### R-4 — height unchanged

The strip's `+` trigger is the same `IconButton::new("session-tab-strip-plus",
IconName::Plus).icon_size(IconSize::Small).icon_color(Color::Muted)`; only its
click handler was replaced by a `PopoverMenu` wrapper (tooltip moved to
`trigger_with_tooltip`). The paint test asserts the painted `+` is **22 px** tall —
the same `ButtonSize::Default.rems()` at 16 px rem that the existing tab-pill test
asserts — so the status bar's height is provably unchanged. Confirmed live too: the
running probe reports the `+` hitbox at `[8, 1051, 23, 24]` inside a status bar
occupying y 1046–1080, unchanged from before.

## Mutation table

| # | Mutation | Run | Outcome |
|---|---|---|---|
| 1 | Delete "New AI Chat" entry from `render_plus_popover` | applied | kept |
| 2 | Delete "Reopen Closed Chat…" entry from `render_plus_popover` | applied | kept |
| 3 | Extract `console_panel::panel::build_plus_menu` free fn (testability) | applied | kept |
| 4 | Delete `ConsolePanel::open_reopen_session_modal` + its `solution_agent::reopen_session_modal` import | applied | kept |
| 5 | Move `use crate::actions::NewChat` into `mod tests` | applied | kept |
| 6 | Add `solution_agent::reopen_session_modal::open_reopen_session_modal` free fn | applied | kept |
| 7 | `SessionTabStrip::render_plus_button` → `render_plus_popover` (PopoverMenu) | applied | kept |
| 8 | Add `session_tab_strip::build_plus_menu` free fn (2 entries) | applied | kept |
| 9 | Rewrite `close_tab`'s confirmation prompt to name the new home | applied | kept |
| 10 | Module-doc updates in `session_tab_strip.rs` | applied | kept |
| 11 | Test: `the_plus_menu_offers_terminals_and_tasks_but_no_ai_sessions` (console) | run | passes |
| 12 | Test: `the_plus_menu_reopens_a_closed_chat_even_with_no_tabs` (strip) | run | passes |

Nothing was reverted. No mutation was needed in a crate another agent is editing
(`crates/editor`, `crates/git_ui`); a transient `unused variable: is_alignment_row`
warning from `crates/editor/src/git/blame.rs` appeared in one intermediate build and
was gone by the final `cargo check --workspace --all-targets` — another agent's
in-flight edit, not touched.

## Tests

Both sides are pinned on the **painted tree**, not on a predicate (repo-root `.rules`).

- `console_panel::panel::tests::the_plus_menu_offers_terminals_and_tasks_but_no_ai_sessions` —
  renders exactly the `ContextMenu` the popover builds (`build_plus_menu`) as a window
  root and reads `ui::ContextMenu`'s own `MENU_ITEM-{label}` debug selectors. Asserts
  **both** sides: `MENU_ITEM-New Terminal` and `MENU_ITEM-Spawn Task…` present (so the
  negative assertions are not vacuously true on an empty frame), `MENU_ITEM-New AI Chat`
  and `MENU_ITEM-Reopen Closed Chat…` absent.
- `solution_agent::session_tab_strip::tests::the_plus_menu_reopens_a_closed_chat_even_with_no_tabs` —
  drives the strip's **real `Render`** (not the `TabPaintHarness`): a live
  `MultiWorkspace::test_new` over the solution's own worktree makes
  `active_solution_id` resolve, with zero sessions created. Asserts no
  `SESSION-TAB-*` painted (the precondition that makes the test about the
  zero-tab case), `ICON-Plus` painted at 22 px, then `simulate_click`s the `+`'s
  centre and asserts `MENU_ITEM-Reopen Closed Chat…` and `MENU_ITEM-New AI Session`
  painted. This is the first test in the file to exercise the strip's real
  `Render` — the module previously claimed the scaffolding "cannot build" a live
  `MultiWorkspace`; it can.

Gates:

- `CARGO_BUILD_JOBS=4 cargo build --bin sawe` → exit 0, 0 `^error`, 0 `^warning`.
- `CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets` → exit 0, 0 `^error`, 0 `^warning`.
- `CARGO_BUILD_JOBS=4 cargo test -p console_panel -p solution_agent` → exit 0;
  31 + 771 + 1 + 1 passed, 0 failed.

## Screenshots (live `script/run-mcp --debug --headless --runtime-dir /tmp/dropdown-probe`)

Binary stamp confirmed fresh via `editor.capabilities`: `binary built 2026-09-03 16:34:50 +07:00`.

| Path | What I see |
|---|---|
| `/tmp/shot-base.png` | Baseline. Solution "Probe" with member `proj`. Status bar bottom-left: the session strip's `+` (no session tabs), then the search and diagnostics items. |
| `/tmp/shot-strip-plus.png` | After clicking the strip's `+` at (19, 1063): a two-row popover anchored above the status bar reading **"New AI Session"** and **"Reopen Closed Chat…"** (second row hover-highlighted). There are **no session tabs** in the strip — the exact discoverability case R-2 asks about. |
| `/tmp/shot-reopen-modal.png` | After clicking "Reopen Closed Chat…": the centred **"Reopen Closed Chat"** modal with "No closed chats in this solution." — correct for a fresh probe Solution, and proof the moved flow reaches `SolutionAgentStore::list_closed_sessions` from its new home. |
| `/tmp/shot-console-plus.png` | Console panel open (auto-started `proj — bash` tab), its `+` clicked: the popover reads **"New Terminal"**, a separator, **"Spawn Task…  Alt-Shift-T"**. No AI entries. |

## Sentence for `FORK.md` (not edited here — consolidated docs pass owns it)

> AI-session affordances live on the status-bar session tab strip, not in the console
> panel: `ConsolePanel`'s `+` popover is terminal/task only, and the reopen-a-closed-chat
> picker moved to a `PopoverMenu` on `SessionTabStrip`'s trailing `+`
> (`solution_agent::reopen_session_modal::open_reopen_session_modal`) — chosen over the
> tab right-click menu because the `+` is the only strip affordance that still paints
> when the Solution has zero session tabs, which is exactly the state a user who just
> closed their last chat is in.

## Suggested `.rules` additions

None that meet the three-criteria bar. One observation for whoever writes the docs
pass: `ui::ContextMenu` already registers a `MENU_ITEM-{label}` debug selector for
every entry, so menu contents are directly paint-testable via
`VisualTestContext::debug_bounds` — worth knowing, but it is a map, not a trap.

---

# Fix pass — split the strip's `+` (coordinator ruling on concern (1))

Ruling: creating a session is the frequent action and must stay one click;
reopening is rare and can afford a right click. A second icon next to `+` is
the wrong direction on a strip the maintainer has been *removing* chrome from
(`0c569d6c95` dropped the per-tab close cross). So the two-entry `PopoverMenu`
from the first pass is gone.

## What changed

| # | Mutation | Run | Outcome |
|---|---|---|---|
| 13 | `SessionTabStrip::render_plus_popover` → `render_plus_button`: `PopoverMenu` replaced by `ui::right_click_menu` wrapping the same `IconButton`; the button keeps its `on_click` → `cx.build_action("console_panel::NewChat")` → `window.dispatch_action` | applied | kept |
| 14 | `build_plus_menu` reduced to a single entry, `REOPEN_ENTRY_LABEL` | applied | kept |
| 15 | Consts `REOPEN_ENTRY_LABEL` / `PLUS_TOOLTIP` / `REOPEN_GESTURE`; prompt text extracted to `close_prompt_detail()` and assembled from them | applied | kept |
| 16 | Tooltip → `"New AI Session — right-click for more"` | applied | kept |
| 17 | Module doc + `render_plus_button` doc rewritten for the two-gesture split | applied | kept |
| 18 | Test `the_plus_menu_reopens_a_closed_chat_even_with_no_tabs` → split into the two gesture tests below, sharing `paint_strip_with_no_tabs` | applied | kept |
| 19 | `#[cfg(test)] mod stub { gpui::actions!(console_panel, [NewChat]); }` + `StripHarness` root with `on_action` | applied | kept |
| 20 | `StripHarness.focus_handle` + `track_focus` + focus it in setup | applied | kept (see below) |
| 21 | Temporary `zz_probe` diagnostic test | run | **reverted** (deleted after it isolated the failure) |

**Left click**: `on_click` unchanged from the pre-defect behaviour, so a plain
click is still exactly one action dispatch. **Right click**: `right_click_menu`
fires on `MouseDownEvent { button: Right }` *only when its hitbox is already
hovered*, and only when the click lands on the `+` itself — it does not shadow
the left click (both gestures are asserted independently below).

**Wording agreement.** The tooltip and the close prompt are now built from the
same three consts, and `close_prompt_detail()` is a function precisely so a
test can read the string the user is shown:

> The agent is still working. Closing interrupts the current turn — the tab can
> be brought back with "Reopen Closed Chat…": right-click the session strip's
> "+" button.

## The one non-obvious failure, and what it was

`left_clicking_the_plus_creates_a_session_and_opens_no_menu` first failed with
`left: 0, right: 1`. The `zz_probe` throwaway test isolated it in one run:
`build_action` resolved fine and even a **direct** `window.dispatch_action` was
not counted. Cause: `Window::dispatch_action` routes to the *focused* dispatch
node and bubbles **up** from there; with nothing focused it targets the tree
root, and a root-element `on_action` is not on that path. Focusing the harness
(mutation 20) fixed both the direct dispatch and the click. This mirrors the
real app, where the handler is on `Workspace` — an ancestor of whatever holds
focus — not on an unfocused sibling.

## Covering tests (all in `crates/solution_agent/src/session_tab_strip.rs`)

Shared setup `paint_strip_with_no_tabs` boots a Solution with a live
`MultiWorkspace` (so the strip's **real** `Render` gets past its
`active_solution_id` early return) and **zero** sessions, then asserts no
`SESSION-TAB-*` painted — the precondition that makes both tests about the
zero-tabs case rather than accidentally true.

- `left_clicking_the_plus_creates_a_session_and_opens_no_menu` — hovers, then
  `simulate_click`s the painted `+`. Asserts **both sides**: the
  `console_panel::NewChat` dispatch count is exactly `1`, **and**
  `MENU_ITEM-Reopen Closed Chat…` did **not** paint. Also asserts the `+` is
  22 px tall (`ButtonSize::Default` at the 16 px test rem — the same number the
  existing tab-pill test asserts), which is the status-bar height guard.
- `right_clicking_the_plus_reopens_a_closed_chat_even_with_no_tabs` — rests the
  cursor (mandatory: `RightClickMenu` gates on `hitbox_id.is_hovered`), sends a
  right `MouseDownEvent`, asserts `MENU_ITEM-Reopen Closed Chat…` painted
  **and** that the dispatch count is still `0` (the right click must not also
  create a session — the two gestures share one element).
- `the_close_prompt_and_the_tooltip_point_at_the_same_affordance` — reads
  `close_prompt_detail()` and asserts it quotes `REOPEN_ENTRY_LABEL` verbatim,
  that it and `PLUS_TOOLTIP` name the same gesture, and that it no longer says
  "console panel".
- `console_panel::panel::tests::the_plus_menu_offers_terminals_and_tasks_but_no_ai_sessions`
  — unchanged from the first pass, still pinning the console side.
- The `stub::NewChat` action is what makes "creates a session" *observable*:
  `solution_agent` cannot link `console_panel`, so without it the left click
  just `log::error!`s and the test could only ever have asserted the weaker
  "no menu" half. Paired with console_panel's existing
  `new_chat_action_matches_the_status_bar_strips_dispatch_string`, the two ends
  of the by-name dispatch are both pinned.

## Commands and output

```
$ CARGO_BUILD_JOBS=4 cargo build --bin sawe
build exit=0        # 0 lines matching ^error, 0 matching ^warning

$ CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets
check exit=0        # 0 lines matching ^error, 0 matching ^warning

$ CARGO_BUILD_JOBS=4 cargo test -p console_panel -p solution_agent
test exit=0
test result: ok. 31 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 773 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## Screenshots (re-shot; `--runtime-dir /tmp/dropdown-probe2`, binary stamp `2026-09-03 16:49:06 +07:00`)

| Path | What I see |
|---|---|
| `/tmp/shot-strip-rightclick.png` | Right-click on the strip's `+` with **zero session tabs**: a single-entry context menu anchored at the cursor, just above the status bar, reading **"Reopen Closed Chat…"**. Nothing else changed on screen. |
| `/tmp/shot-strip-leftclick.png` | Plain left click on the same `+`: a session tab **"Probe"** now paints in the strip in its selected style (status dot + accent underline), the Solution band has opened below it showing "(no messages yet)", the status row `Idle · just now · 0 / 1.0M · 0.0% · claude-acp · model · auto` and the "Send a message…" composer, and the Solution tab up top carries a `1` session badge. **No menu opened.** The `+`'s tooltip is visible reading exactly **"New AI Session — right-click for more"**. `solution_agent.list_sessions` confirms `1 session(s)`. |
| `/tmp/shot-console-plus.png` | (unchanged from the first pass) Console panel `+`: "New Terminal" · separator · "Spawn Task… Alt-Shift-T". No AI entries. |

Height check, live: with the tab present the strip reads session tab
`[8, 1051, 99, 24]` and `+` `[111, 1051, 23, 24]` — the same 24 px hitbox as
every other status-bar item (`[1839/1866/1893, 1051, 23, 24]`), status bar
still occupying y 1046–1080. Unchanged.

## `FORK.md` sentence (revised — supersedes the first pass's)

> AI-session affordances live on the status-bar session tab strip, not in the
> console panel: `ConsolePanel`'s `+` popover is terminal/task only, and the
> reopen-a-closed-chat picker moved to the **right-click** menu of
> `SessionTabStrip`'s trailing `+`
> (`solution_agent::reopen_session_modal::open_reopen_session_modal`), whose
> plain left click still creates a session in one click. The `+` rather than
> the tab context menu because it is the only strip affordance that still
> paints when the Solution has zero session tabs — exactly the state a user who
> just closed their last chat is in — and a right click rather than a second
> icon because the strip is deliberately losing chrome, not gaining it.
