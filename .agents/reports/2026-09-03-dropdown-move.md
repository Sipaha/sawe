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
