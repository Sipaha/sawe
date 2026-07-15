# Plan — IDEA-style single-file Git Diff (preview tab)

**Date:** 2026-07-15 · **Status:** in progress

## Problem
Opening a git diff shows ONE tab with all changed files stacked in a single
multibuffer (`ProjectDiff`, the "accordion"). The user wants IntelliJ-IDEA
behaviour: selecting a file shows ONLY that file's diff, in a single reusable
**preview tab** that gets replaced as you pick another file; double-click / Enter
pins it as a permanent tab.

## Key finding
The fork already has `SoloDiffView` (`crates/git_ui/src/solo_diff_view.rs`) — a
single-file diff item that de-dupes per `(repo, repo_path)`. It was only bound to
the *secondary* gesture. Zed's preview-tab infra is all public on `Pane`
(`replace_preview_item_id`, `unpreview_item_if_preview`, `add_item`,
`preview_item_id`) and gated by `PreviewTabsSettings.enabled` (default true). So
the whole feature lives in the `git_ui` crate — no `workspace`/`pane` or keymap
edits.

## Design (option B, confirmed by user)
Gestures in the git panel changes list:
- **Single click** → `SoloDiffView` as a **preview** tab (italic, replaces prior
  preview via `replace_preview_item_id`); focus stays in the git panel.
- **Double-click** → `SoloDiffView` **pinned** (permanent), focus moves to diff.
- **Enter** (`menu::Confirm`) → pinned single-file diff (keyboard "open").
- **↑/↓** (`git_panel::PreviousEntry`/`NextEntry` → `move_diff_to_entry`) → if a
  solo-diff preview is the pane's current preview item, the preview follows the
  selection.
- **alt-enter** (`menu::SecondaryConfirm`) / **ctrl-shift-d** (`git::Diff`) /
  overflow + context menu → the accordion (`ProjectDiff`) stays reachable.

`preserve_preview` left at default `false` so a real text edit in the diff
promotes it to permanent (consistent with Zed file previews / IDEA).

## Touch points
- `solo_diff_view.rs`: add `SoloDiffOpen { Preview, Permanent }`; thread it into
  `open_or_focus`; preview-aware add-to-pane; focus only on `Permanent`.
- `git_panel.rs`: row `on_click` (single/double/secondary); `open_diff`
  (Confirm→solo pinned); rename `open_solo_diff`→`open_accordion_diff`
  (SecondaryConfirm→accordion); new `open_diff_for_selected` / `open_all_diffs`;
  extend `move_diff_to_entry` for preview-follow; context-menu + registration
  labels.

## Verify
Unit build + `./script/clippy` git_ui; MCP live: open a repo with changes, click
a file (preview italic), click another (replaces), double-click (pins), arrow-nav
(preview follows), ctrl-shift-d (accordion). Screenshot each.
