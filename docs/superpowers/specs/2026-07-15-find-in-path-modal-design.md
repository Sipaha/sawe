# Find in Path modal — design spec

**Date:** 2026-07-15
**Status:** Implemented on branch `find-in-path` (pending merge to main)
**Author:** supervisor session `gf2kf7wn` (c02)
**Feature:** IntelliJ-IDEA-style "Find in Path" centered modal, replacing the pane-tab
project search on `ctrl-shift-f` (and `ctrl-shift-r` for replace).

## 1. Goal & motivation

The user wants `ctrl-shift-f` to open a floating, centered **Find in Path** modal that
behaves as close to IntelliJ IDEA as practical, instead of the current
`pane::DeploySearch` which opens `ProjectSearchView` as a full pane **tab** (query
toolbar + multibuffer results editor). The current UI occupies the whole active pane
("appears in the center"); the user wants an overlay dialog with a grouped results tree
and a live code preview.

Approved scope (user decisions):
- **Full IDEA fidelity** ("максимально близко к IDEA"), large-effort accepted.
- **Scope tabs:** `In Solution` / `In Project` / `Directory`, plus file mask
  include/exclude. (Sawe has no IDEA "Module"/named-"Scope" concept — dropped.)
- **Replace included** in the same modal (replace field + Replace / Replace All);
  `ctrl-shift-r` opens the modal in replace mode. IDEA parity.
- The old `ProjectSearchView` pane tab **stays** and is reachable via an
  **"Open in Find Window"** button in the modal (hands the current query/options to
  `workspace::DeploySearch`).

## 2. Architecture decision

**Approach A — bespoke composite modal reusing the existing search/replace backend.**

Rejected alternatives:
- **B — modal shell hosting the existing multibuffer results editor** instead of a
  grouped tree + separate preview. Cheaper/lower-risk but not IDEA's tree+preview shape.
  Retained only as an optional phase-1 checkpoint if we want something running earlier.
- **C — re-shell `ProjectSearchView` into a modal.** Minimal effort but delivers neither
  the grouped tree nor the preview split; does not meet the approved scope.

The feature lives **inside `crates/search`** as a new module
`crates/search/src/find_in_path.rs`, registered from `search::init`. Rationale
(explorer risk #1): the reusable input-row helpers (`render_text_input`,
`input_base_styles`, `render_action_button`, `crates/search/src/search_bar.rs:40-139`)
and `SearchOptions` (`crates/search/src/search.rs`) are crate-private to `search`, and
the replace machinery already lives here. A new module avoids pub-ifying/duplicating
helpers and avoids a new crate.

### Backend reuse (confirmed signatures)

- Query: `project::search::SearchQuery::{text,regex,escaped_regex}` +
  `.with_replacement(String)` / `.replacement() -> Option<&str>`
  (`crates/project/src/search.rs:96-211,356-366,451-452`). Include/exclude are
  `util::paths::PathMatcher::new(&queries, path_style)`; glob comma-split respecting
  `{...}` via `split_glob_patterns` (`crates/search/src/project_search.rs:73-97`).
- Execute: `Project::search(query, cx) -> SearchResults<SearchResult>`
  (`crates/project/src/project.rs:4690-4696`), streaming via
  `SearchResults { rx: async_channel::Receiver<SearchResult>, _task_handle }`
  (`crates/project/src/project_search.rs:68-71`).
- `SearchResult::Buffer { buffer: Entity<Buffer>, ranges: Vec<Range<text::Anchor>> }`,
  plus progress signals `WaitingForScan` / `Searching` / `LimitReached`
  (`crates/project/src/search.rs:21-30`). Limits `MAX_SEARCH_RESULT_FILES=5_000`,
  `MAX_SEARCH_RESULT_RANGES=10_000` (`crates/project/src/project_search.rs:154-155`).
- Streaming-consumption reference to mirror (background batching + foreground fold):
  `ProjectSearch::search` `crates/search/src/project_search.rs:423-540` (esp.
  `rx.ready_chunks(1024)`, `cx.background_executor().spawn`, `yield_now()` at :517).
- Replace reference: `ProjectSearchView::{replace_next,replace_all}`
  (`crates/search/src/project_search.rs:843-912`), `editor.replace_all(...)`.
- Preview primitives: `Editor::for_buffer`/`for_multibuffer`, `set_read_only(true)`
  (`read_only` field `crates/editor/src/editor.rs:1039`, getter :3058),
  `highlight_background` (:8804) / `highlight_rows` (:8602) / `highlight_text` (:9110),
  `request_autoscroll(Autoscroll::center(), cx)`.

### Modal infra reuse

- `Workspace::toggle_modal(window, cx, build)` + `impl ModalView`
  (`crates/workspace/src/modal_layer.rs:13-47,121-...`). Override
  `fade_out_background() -> true` (dim backdrop), keep `dismiss_on_overlay_click()`
  default. `ModalLayer` pins the modal `top_20()` horizontally centered with intrinsic
  size (`:259-263`) — this feature introduces an **explicit fixed size** on the modal
  shell (no current modal does; see risk R2).
- Canonical `Picker`-in-`ModalView` pattern to model action registration and toggle
  after: `FileFinder::register` / `FileFinder::open`
  (`crates/file_finder/src/file_finder.rs:104-204`), including the
  `workspace.active_modal::<Self>(cx)` "already-open → refocus" short-circuit.

## 3. Layout & sizing

Floating overlay via `toggle_modal`, `fade_out_background() = true`. Explicit fixed size
~**85% viewport width / 80% height** (resize handle deferred to a follow-up, not v1).
Vertical stack:

```
┌───────────────────────────────────────────────┐
│ [search field.............] [Aa][W][.*]         │  query row
│ [replace field............] [Replace][Replace All]  (only in replace mode)
│ [In Solution | In Project | Directory] [dir ▾]  │  scope tabs (+ dir picker)
│ [File mask: *.rs ]   [exclude: target/** ]      │  masks
├──────────────────────┬────────────────────────┤
│ results list          │  preview editor         │
│  grouped by file (40%)│  read-only (60%)        │
│  ▸ file.rs (3)         │  matched lines, syntax  │
│    12  let x = …       │  highlight, context,    │
│    40  fn foo() …      │  selected range hl'ed   │
├──────────────────────┴────────────────────────┤
│ "128 matches in 34 files"    [Open in Find Window]│  status bar
└───────────────────────────────────────────────┘
```

## 4. Components

- **`FindInPath`** (view, `impl ModalView`) — owns everything; holds a `FocusHandle`;
  routes focus (query editor focused on open; Tab cycles query → replace → results
  list; clicking a result does not steal input focus). Own key-context `"FindInPath"`.
- **Header widgets:**
  - `query_editor`, `replace_editor` — single-line `Editor`s.
  - Option toggles (`Aa` case, `W` whole-word, `.*` regex) via reused `SearchOptions`
    bitflags + `search_bar.rs` render helpers.
  - Scope tabs → `enum Scope { Solution, Project, Directory(PathBuf) }`; `Directory`
    reveals a path picker/field.
  - Mask fields → single-line editors; parsed like
    `ProjectSearchView::parse_path_matches` (`crates/search/src/project_search.rs:1541-1549`).
- **Results model** — `MatchList`:
  `Vec<FileGroup { path, buffer: Entity<Buffer>, matches: Vec<MatchRow { range: Range<Anchor>, line, snippet }> }>`,
  rendered with `uniform_list`. A flattened `Vec<Row>` (`Header | Match`) backs
  keyboard navigation and selection index. **Not** a `MultiBuffer`.
- **Preview** — `preview_editor: Option<Entity<Editor>>`. On selected-`MatchRow` change:
  if the file changed, build `Editor::for_buffer(buffer, Some(project), window, cx)` +
  `set_read_only(true)`; scroll to the range (`request_autoscroll(center)`) and
  highlight it (`highlight_background`). Same-file line change → rescroll/rehighlight
  only, no rebuild.

## 5. Data flow

1. `ctrl-shift-f` → `find_in_path::Toggle` (and `ctrl-shift-r` → `ToggleReplace`)
   registered in `search::init` → `toggle_modal` (or refocus if already open).
2. On any change to query / options / scope / masks: **debounce ~150 ms** →
   build `SearchQuery` (map `Scope` to the search domain: `In Solution` = whole solution
   / all worktrees; `In Project` = restrict `files_to_include` to the active member
   project's paths; `Directory` = restrict to the chosen dir path) →
   `Project::search(query)`.
3. Consume the stream mirroring `project_search.rs:462-486`: `rx.ready_chunks(1024)` on
   the background executor, incrementally build `MatchList`, `yield_now()` for
   responsiveness; fold `WaitingForScan` / `Searching` / `LimitReached` into the status
   line. Cancel the previous search by dropping its `Task`/`_task_handle` on each new
   query. Must stay responsive up to `MAX_SEARCH_RESULT_FILES=5_000`.

**Open question for the plan (scope↔project model):** confirm how a Sawe *Solution*
with N member projects maps onto `project::Project` / worktrees — one `Project` holding
all members as separate worktrees, or one `Project` per member. This determines the
`In Solution` vs `In Project` implementation: if members are worktrees of a single
`Project`, `In Solution` searches all worktrees and `In Project` restricts
`files_to_include` to the active member's worktree root(s); if members are separate
`Project`s, `In Solution` must fan the search across each member's `Project::search` and
merge streams. Resolve by reading the Solution→Project wiring (`crates/solutions`,
`workspace::MultiWorkspace`) as the first plan step.

## 6. Replace

- Replace field + `Replace` (selected match) / `Replace All` buttons.
  `ctrl-shift-r` opens the modal with the replace field revealed.
- `Replace All`: `query.with_replacement(text)` then reuse the existing replace path
  (`ProjectSearchView::replace_all` idiom / `editor.replace_all(...)`,
  `crates/search/src/project_search.rs:879-912`).
- Single `Replace` on the selected match = `ReplaceNext` idiom.
- After a replace, re-run the search to refresh the list.

## 7. Keymap & coexistence

- Rebind at the **`Workspace`** context in `default-linux.json`, `default-windows.json`,
  and `default-macos.json` (`cmd-shift-f` / `cmd-shift-r`):
  `ctrl-shift-f` → `find_in_path::Toggle`, `ctrl-shift-r` → `find_in_path::ToggleReplace`.
  Current sites: `default-linux.json:680`, `default-windows.json:651`,
  `default-macos.json:686`.
- Internal binds (Esc close, Up/Down navigate, Enter open, Tab cycle) under the modal's
  own `"FindInPath"` key-context — do **not** reuse `ProjectSearchBar` / `Pane` /
  `ProjectSearchView` contexts (`default-linux.json:441-537`).
- **Do not touch:** in-editor buffer search `search::FocusSearch` / `ctrl-f`
  (`default-linux.json:453`), terminal `buffer_search::Deploy` (:1259), and the
  jetbrains-keymap `project_panel::NewSearchInDirectory` override
  (`linux/jetbrains.json:164`).
- **Coexistence:** `ProjectSearchView` pane tab stays. The modal's
  **"Open in Find Window"** button dispatches `workspace::DeploySearch` populated with
  the current query + options (fields on the action:
  `crates/workspace/src/pane.rs:190-210`), then dismisses the modal.

## 8. Testing & verification

- **Unit (GPUI):** `SearchQuery` construction from each `Scope` + masks; `MatchList`
  flatten ↔ navigation index; mask parsing; debounce/cancel of the search task (use GPUI
  executor timers, not `smol::Timer`).
- **e2e via embedded MCP:** `windows.send_keystroke ctrl-shift-f` → `send_text` →
  `workspace.screenshot` for the modal, grouped list, preview pane, and replace mode;
  `dump_visual_structure` + `clickables` to assert scope tabs / option toggles /
  buttons. Screenshot of the running editor is part of "done" (UI change). Add any
  missing MCP input primitive in-session rather than pushing manual repro to the user.
- Debug builds for agent verification; `release-fast` rebuild for the user's hands-on
  pass (assets/keymap changes are embedded → require a rebuild).

## 9. Risks (from architecture map)

- **R1 crate boundary** — resolved: build inside `crates/search` to reuse crate-private
  helpers.
- **R2 composite modal + fixed sizing** — no precedent for input + list + persistent
  preview in one `ModalView`, and `ModalLayer` sizes to content (`top_20()`). Introduce
  an explicit modal size; watch focus routing across nested children and key-context
  composition.
- **R3 embedded live-preview `Editor` in a modal** — zero precedent (outline scrolls the
  *real* editor behind the overlay; project_search's results editor lives in a pane
  tab). Build a fresh `Editor::for_buffer` per selected file; size it inside the
  fixed-height shell.
- **R4 streaming into a grouped list model** — the proven streaming pipeline only ever
  folds into a `MultiBuffer`; the grouped `MatchList` + per-line snippets +
  progress-state status line is net-new. Mirror the background-batching pattern; stay
  responsive at the 5_000-file / 10_000-range limits.

## 10. Phasing (implementation-plan checkpoints)

1. Modal shell + header (query + options) + streaming grouped results list — `In Solution`
   scope only, no preview / replace / scope tabs.
2. Preview pane (read-only editor, scroll + highlight on selection).
3. Scope tabs `In Project` / `Directory` + include/exclude masks.
4. Replace (field + Replace / Replace All) + `ctrl-shift-r`.
5. "Open in Find Window" + keymap swap + polish + tests + screenshots.

## 11. Out of scope (v1)

- Resize handle / remembered modal size & position.
- Search history dropdown in the query field (IDEA has one) — candidate follow-up.
- Named/custom search scopes and "Module" scope (no Sawe equivalent).
- Preserve-case replace option.
