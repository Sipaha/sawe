# Picker dropdown + Project panel header tweaks

**Status:** complete (2026-05-15). All 7 acceptance items verified via
supervisor's headless MCP smoke-test. Commits `5b230b1` (picker) +
`d9dcd0b` (active_project_selector) merged as `88a3fc7`. Root cause for
the invisible-text bug: `Editor::single_line` uses
`EditorMode::SingleLine` which hard-codes a transparent background; the
picker was rendering it directly on the popover's
`elevated_surface_background` so `editor_foreground` text had near-zero
contrast. Fix: wrap the search input in an `h_flex` that paints
`editor_background` (matches upstream `picker::PickerDelegate`).
Screenshot: [`2026-05-15-picker-and-panel-ui-tweaks-screenshot.png`](2026-05-15-picker-and-panel-ui-tweaks-screenshot.png).
**Estimated:** 1 sub-agent session, ~1 h, worktree-isolated
**Goal:** Apply two batches of UI polish from the user's 2026-05-15
playtest — both LIGHT-track tweaks in fork-owned crates, verifiable
end-to-end via the headless platform + clickable tree.

## Context (user playtest 2026-05-15)

**Picker dropdown** (`solutions_ui::solution_picker_dropdown`):

> Растянуть строки на всю ширину дропдауна, строку поиска поменьше
> сделать и лупу справа нарисовать чтобы было понятно что это поиск.
> Когда вводишь что-то при открытии попапа, фильтр срабатывает, но
> визуально текст поиска нигде не появляется.

**Project panel header** (`project_panel` + active project selector):

> Сделай имя проекта чуть больше + убери слева шеврон. По нему кажется
> буд-то можно свернуть проект или развернуть. Лучше справа нарисовать
> [V] как обычно у дропдаунов бывает.

Both files are fork-touched (FORK.md "Notable upstream file modifications"
already lists `crates/project_panel/src/project_panel.rs`) — refactor
freely per ADR-0001.

## Scope

### A. Picker dropdown — `crates/solutions_ui/src/solution_picker_dropdown.rs`

1. **Stretch rows to full popup width.** The current rendered rows are
   width-constrained by their content (label-sized) — let the parent
   popup width drive each row's width so the hover background fills
   edge-to-edge.
2. **Shrink the search input.** Currently uses default `Editor` height;
   make it visually compact — `text_sm`, reduce vertical padding to
   match a 28-px row.
3. **Magnifier icon on the right side** of the search input. Use
   `IconName::MagnifyingGlass` from `ui`. Placement: inside the input's
   right padding, dimmed (`Color::Muted`).
4. **Search text visibility bug.** When the user types, the filter
   filters but the typed text doesn't appear in the input. Investigate
   — probably one of:
   - `text_color` set to a value matching the background;
   - the `Editor` isn't being painted because of a layout-zero-height
     bug;
   - the focus handle isn't grabbing keystrokes so the keys go to the
     filter logic but never into the Editor buffer.
   Fix the cause; verify by typing into the popup and reading the
   `Editor`'s buffer text back via project / buffer MCP if needed.

### B. Project panel header — `crates/solutions_ui/src/active_project_selector.rs`

(This is the `ActiveProjectSelector` element hosted at the top of the
`project_panel` — see FORK.md.)

1. **Bigger project-name label.** Currently default `text_xs` (or
   similar small size). Bump to `text_sm` minimum, ideally
   `text_base` so it reads as the panel header.
2. **Remove the left-side chevron.** It implies collapse/expand — but
   the only function of clicking the header is to **switch active
   project** (open a dropdown listing the solution's members), not
   collapse the panel. Misleading affordance.
3. **Add a right-side `[V]` chevron** (`IconName::ChevronDown`,
   `Color::Muted`, `IconSize::Small`) — the standard dropdown-trigger
   shape. Placement: right-aligned in the header row.

The clickable result of the header (open the project-switcher dropdown)
stays unchanged — only the visual indicator moves from left to right,
and changes from "expand" semantics to "dropdown" semantics.

### C. Tests

- Unit-test the `ActiveProjectSelector` element where it has a
  testable surface — confirm the chevron is right-aligned in the
  rendered structure (use existing `dump_visual_structure` test
  patterns).
- For the picker: the search-text-visibility fix is the only
  behavioural change worth a regression test. Add a unit-level test
  if the fix is text-property-related; otherwise the supervisor's
  end-to-end smoke-test catches it.
- Do NOT inline an end-to-end MCP test — supervisor handles § H.

### D. Documentation

- Tick acceptance items in this plan doc as you go.
- No new ADR (these are visual tweaks, not architectural).
- No FORK.md changes (both files already listed).

## Out of scope

- Picker keyboard-navigation overhaul. Just fix the typed-text bug;
  full keymap work is a future LIGHT-task if needed.
- Project panel: don't touch the worktree-tree below the header, just
  the header itself.
- Other places that use `IconName::ChevronRight` for "expand" — those
  meanings (e.g. tree folders) are correct in context. Only the
  active-project-selector misuses it.

## Verification

```bash
cd <worktree>
cargo build --bin spk-editor 2>&1 | tee /tmp/build.txt
grep -E "^error|could not compile" /tmp/build.txt   # must be empty
cargo clippy -p solutions_ui -p project_panel --all-targets -- -D warnings
cargo test -p solutions_ui -p project_panel --no-fail-fast
```

Supervisor § H smoke-test post-merge:
- `script/run-mcp --debug --headless &`
- Open AlphaSol → trigger the picker → assert dump_visual_structure's
  clickables include the search input (typed text round-trips) and the
  magnifier icon clickable.
- Inspect ActiveProjectSelector dump — left-chevron gone, right-chevron
  present, name label has bigger size.
- `workspace.screenshot` → visual confirms.

## When done

- [ ] cargo build / clippy / test clean.
- [ ] Picker rows stretch to full popup width (verify in screenshot).
- [ ] Picker search input is smaller + has right-side magnifier.
- [ ] Typed text is visible in the picker search input.
- [ ] ActiveProjectSelector header: bigger name, no left chevron, right
      chevron `[V]`.
- [ ] Click on the header still opens the project-switcher dropdown.
- [ ] Plan doc ticked + final SHA appended.
