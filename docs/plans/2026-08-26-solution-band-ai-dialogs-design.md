# Solution-band AI dialogs — design spec

Date: 2026-08-26. Status: approved by maintainer in brainstorming session
(browser mockups in `.superpowers/brainstorm/3033871-1787713963/content/`,
final layout = `final.html` + `commit-details.html`).

## Motivation

AI dialogs are today filtered by the active member project (`TabScope` in
`console_panel`), which contradicts the original design intent (FORK.md #27:
"AI dialogs stay agnostic") and the maintainer's mental model: sessions belong
to the Solution, not to a member. Separately, dialogs share one bottom-dock
panel with terminals, so the primary AI surface competes with the console for
the same slot. This redesign makes the scoping visible in the window chrome
itself: **solution-scope wraps project-scope** ("sandwich").

## 1. Scoping model

- **AI sessions become purely solution-scoped.**
  - `SolutionSession.member_id` is no longer read or written. Existing DB rows
    get `member_id = NULL` via migration; the column stays in place (harmless,
    avoids a destructive schema change).
  - Session cwd is always the solution root (`solution.root`). No per-member
    cwd at creation. Agents `cd` wherever they need themselves.
  - The chat-tab `TabScope` filtering dies. The per-session project label in
    the session status row dies with it.
- **Terminals, GitGraph, Debug stay project-scoped** — filtered by the active
  member exactly as today (terminal `origin_cwd` scoping unchanged).
- Visual encoding: solution-scope surfaces are the window frame (title bar
  with solution tabs, the new bottom AI band, the status bar with session
  tabs); everything between the project toolbar and the band is
  project-scope and swaps on member switch.

## 2. Window layout (top → bottom)

1. **Title bar** — solution tabs (unchanged).
2. **Project toolbar** — member project tabs; right side: branch widget,
   Run strip, then the project-zone panel toggle buttons: 🗀 project tree,
   ⎇± git panel, ☰ outline. (These buttons move here from the removed
   vertical dock bars.)
3. **Project zone** — left dock (tree) | center editors | right dock (git
   panel). **No bottom dock inside the project zone anymore.**
4. **Solution band** (new region, full window width, between project zone
   and status bar):
   - Left: **AI dialog** — the active session's `SolutionSessionView`
     (solution-scope).
   - Draggable vertical divider; position persisted.
   - Right: **utility section** (project-scope) hosting exactly one of:
     Terminal console / Git graph / Debug — chosen by the status-bar buttons.
5. **Status bar**:
   - Left: **AI session tabs** — one tab per session of the active solution,
     with agent-state indicator (● running / ○ idle etc.) and a `+` button.
     Overflow beyond the visible max spills into a popover (same pattern as
     solution tabs). Context menu keeps today's actions (rename, close,
     restart agent, …).
   - Right: utility-section buttons ⌨ / ⎇-graph / 🐛, then the surviving
     status items (Remote Control indicator, editor items like line:col and
     language when an editor is focused).
   - **Removed from the status bar:** "SolutionName · N projects" and
     "AI: N" indicators (useless per maintainer).
6. **Vertical dock button bars are removed entirely** — all panel toggles now
   live in the two horizontal rows ("by geometry" placement: each button in
   the row nearest the panel it controls). The window gains the reclaimed
   horizontal space.

## 3. Solution band behaviour

- **Divider** between dialog and utility section is drag-resizable;
  ratio persisted per solution.
- **Collapse dialog:** clicking the *active* session tab (or the dialog
  hotkey) collapses the dialog; the utility section takes the full band
  width. Clicking any session tab re-opens the dialog on that session.
- **Hide utility section:** clicking the *active* section button (⌨/⎇/🐛)
  hides the section; the dialog takes the full band width. Clicking an
  inactive button switches the section content.
- **Both hidden:** the band disappears; only the status bar remains and the
  project zone gets the full window height.
- **Member switch** changes only the utility section content (that project's
  terminals / graph / debug), never the dialog or the session tabs.
- Hotkeys: `ctrl-\`` keeps toggling the terminal (now the utility section
  with Terminal selected). New default binding for the dialog toggle:
  `ctrl-shift-a` (subject to maintainer's veto at implementation time).
- Run-configuration output continues to land in the Terminal section
  (`RevealTarget::Dock` semantics re-pointed at the band's terminal host).

## 4. Fate of `ConsolePanel`

The merged Terminal+Chat panel splits:

- **Chat tabs leave.** The session views are re-hosted by the solution band;
  the rendered widget stays `solution_agent::SolutionSessionView` (status row,
  input, context meter unchanged except the project label removal). The tab
  strip moves to the status bar.
- **Console becomes terminal-only** and becomes the band's Terminal section
  (member-scoped tab strip of terminals + task outputs, as today).
- GitGraph and Debug panels are re-hosted as the other two utility-section
  contents. The bottom dock as a workspace concept is no longer rendered in
  this fork's layout (implementation may keep dock machinery under the hood
  or replace it — planner's choice, but no user-visible bottom dock).
- Actions: `console_panel::NewChat` → creates a solution-root session and
  opens the dialog; `console_panel::ToggleFocus` → toggles the Terminal
  section; `console_panel::ShowSession` → selects a session tab + expands
  the dialog (notification click-through keeps working, FORK.md #36).
- **Persistence migration:** `console_panel_state` chat rows migrate to the
  new band/session-tab state (active session, tab order); terminal rows stay
  with the terminal section. Band geometry (divider ratio, collapsed flags)
  is persisted per solution.

## 5. Git panel: Commit tab, History removed

- **History tab is removed entirely** (no value for the maintainer; the git
  graph covers commit browsing).
- Git panel tabs become: **Changes | Commit**.
  - **Commit** is a closable tab that appears and activates when a commit is
    selected in the git graph; ✕ or deselecting in the graph removes it and
    returns to Changes.
  - Content: commit message (full), short hash · author · date, file list
    with add/mod/del coloring and +/− totals. Clicking a file opens that
    file's diff for the commit in the center pane.
  - Multi-select in the graph (range actions from the multi-select feature):
    the Commit tab shows a "N commits selected" summary (combined file list
    is out of scope for this iteration; range actions stay in the graph's
    context menu).
- **Git graph loses its inline commit-details subpanel** (the bottom strip
  with description + changed files) — the graph becomes a clean list (graph ·
  description · date · author), which is what makes it viable in the compact
  utility section.

## 6. Out of scope

- Combined diff/file-list for multi-commit selections in the Commit tab.
- Any change to solution tabs, project tabs, editors, left/right dock
  contents beyond button relocation.
- Per-member terminal scoping changes (stays as-is).
- Reintroducing sessions as workspace pane Items (FORK.md #7 still stands —
  the band is a dedicated region, not the center pane).

## 7. Verification

- Debug build + `script/run-mcp --debug --headless`; `workspace.screenshot`
  of every band state: both visible / dialog collapsed / section hidden /
  both hidden; member switch with dialog open (dialog must not change);
  commit selected → Commit tab; History gone.
- e2e/unit: session creation stamps no member_id and cwd = solution root;
  DB migration nulls member_id; chat tabs unfiltered by active member;
  terminal tabs still filtered; `ShowSession` works from a notification;
  persistence round-trip of band geometry + session tab order.
- FORK.md updates: new decision entry for the sandwich layout; fix stale
  #5/#27 text; update the crates-table row for `console_panel`; touched-files
  rows for newly modified upstream files (status bar, dock, git panel).

## Decisions log (from the brainstorm)

1. Member binding removed entirely (not just the filtering) — sessions start
   at solution root and "walk wherever they need".
2. Layout = variant E ("sandwich", dialog + utility section share one band)
   refined with: divider resize, per-half hide, buttons moved to horizontal
   rows.
3. Button placement = "by geometry" (🗀 ⎇± ☰ in the project toolbar,
   ⌨ ⎇-graph 🐛 in the status bar) over "by scope purity".
4. Utility section stays project-scoped; only AI dialogs are solution-scoped.
5. Status bar cleaned: "Solution · N projects" and "AI: N" removed.
6. Commit details render in the git panel as a closable Commit tab
   (variant A); History tab deleted, leaving Changes | Commit.
