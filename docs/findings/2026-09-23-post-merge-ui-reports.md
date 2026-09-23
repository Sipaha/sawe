# 2026-09-23 — first UI reports on the merged build

The maintainer restarted onto the merged binary (`05fb87ac68`, containing the
2026-09-22 upstream integration) and reported two things within the hour.

## 1. "The first item in the new-session selector is highlighted without hover" — merge regression, fixed

Cause: upstream #60397 makes `ContextMenu` select its first (or checked) row when
it gains focus, and `DropdownMenu` do the same in `on_open`, for screen readers.
The selection is what paints a row highlighted, so every click-opened menu lit a
row. Fixed by gating both on `window.last_input_was_keyboard()`; recorded as
FORK.md #202 with the tests that pin it.

Evidence, in order: a unit test mounting the menu as a root view reproduced
`Some(first)` on a mouse open; the `PopoverMenu` + `ContextMenu::build` +
`custom_entry` shape of the `+` picker reproduced it once the test window was
activated; and the real merged release binary, driven headless, showed it by
state — click-open, then Down lands on the **second** row. With the fix, Down lands
on the first.

## 2. "The left padding seems gone on some messages" — not a merge change

Assistant prose and the queued-message bubble sit where they sat before the merge,
to the pixel. Measured with one seeded conversation (user message, two shell tool
calls, two assistant messages, a queued follow-up with a quote) on the pre-merge
debug binary and on the merged one, reading the first dark pixel of each text row:

| Row | pre-merge x | merged x |
|---|---|---|
| assistant prose | 13 | 13 |
| tool-call row icon | 20 | 20 |
| tool-call command subline | 35 | 35 |
| queued bubble, quoted line | 36 | 36 |
| queued bubble, plain text / hint | 16 | 16 |

Prose sitting ~7px left of tool-call content is deliberate:
`conversation_render::render_assistant_message` gives it `px_1` and explains that
the offset is the role cue, after an earlier attempt to align it read as one
undifferentiated column. Changing that is a design decision, not a fix.

## 3. Monospaced-UI follow-ups: dialog text size, tab gap, tab height, logo level — fixed (FORK.md #206)

- **Dialog text too big.** `agent_ui_font_size` had never reached the transcript: markdown text
  inherits its size from the enclosing element, and `SolutionSessionView` set none, so prose sat
  at the UI's 16px whatever the setting said (a probe with `agent_ui_font_size: 30` grew only the
  composer). The view root now sets `text_size(agent_ui_font_size)`; default is 14.
- **Probe trap found on the way:** a probe editor left running keeps `mcp.lock`/`mcp.sock`, the
  next launch exits, and the driver silently screenshots the *old* build. `.v/round2.sh` now
  deletes the socket before launch and refuses to continue unless `mcp.lock` holds the PID it
  just started.
- **Tab height / logo.** Measure from the maintainer's own screenshot before theorising — it
  showed a 35px bar (client decorations) where the probe has 36px, which is why the probe looked
  fine: the logo-vs-text offset is a half-pixel rounding difference between SVG and glyph
  snapping, visible only at the odd bar height.

## Reproducing a pre-merge build without rebuilding one

A long-running process keeps its executable readable at `/proc/<pid>/exe` after
the file on disk is replaced — the two stale pre-merge `target/debug/sawe
--headless` probes were copied that way. A debug build reads `assets/` from the
checkout at run time, so an old binary panics in `register_setting` against the
new `assets/settings/default.json`. Run it in a private mount namespace with the
old assets bound over the path, which leaves the checkout untouched:

    git archive <old-commit> assets | tar -x -C <dir>
    unshare --user --map-root-user --mount sh -c \
      "mount --bind <dir>/assets <checkout>/assets && exec env ZED_ALLOW_ROOT=true SAWE_HOME=<short-dir> <old-binary> --headless"

`ZED_ALLOW_ROOT` is needed because the namespace maps the user to uid 0. A
**merged** debug build instead finds its checkout by walking up from its own path,
so run it from `target/debug/`, not from a copy.
