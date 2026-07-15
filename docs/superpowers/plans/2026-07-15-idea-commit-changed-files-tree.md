# Plan — IDEA-style directory tree for a commit's changed files

**Date:** 2026-07-15 · **Status:** in progress

## Problem
The commit detail panel (`CommitView`, git log / history) lists a commit's
changed files as a FLAT list of full repo paths (`commit_view/affected_files.rs`
`render_row`). IntelliJ IDEA shows them as a **collapsible directory tree** with
compacted single-child chains (`main/java/ru/citeck/…` on one row) and per-folder
file counts ("3 files"). The user finds IDEA's git tree much nicer to work with.

## Scope (confirmed via the two reference screenshots)
Only the commit-detail **changed-files** area. Turn the flat list into a
collapsible tree:
- directory rows: folder icon (open/closed) + compacted name + muted "N file(s)"
  count; click toggles collapse.
- file rows: existing status icon + filename, indented under their folder.
- default fully expanded; per-dir collapse state kept in `CommitAffectedFiles`.
- keep the existing fuzzy filter + lazy "Load more" window (build the tree from
  the already-windowed/filtered slice).

Out of scope (noted for later): per-file +/- counts (not in `CommitFile`; needs
numstat), click-a-file-to-scroll-the-diff, the log-row branch chips / branches
sidebar.

## Approach
Self-contained tree in `affected_files.rs` (NOT extracting the git_panel
working-changes tree — that one is coupled to `GitStatusEntry` + staging +
`Section`; a read-only commit-file tree is a genuinely separate, smaller
concern). Build a `Node { dirs: BTreeMap, files: Vec<&CommitFile> }` from the
visible slice, compact single-child chains, flatten to `TreeRow::{Dir,File}`
honoring a `collapsed_dirs: HashSet<String>` keyed by full dir path, render with
`INDENT = 16px` per depth (matching `git_panel::TREE_INDENT`). Counts via subtree
file aggregation. Toggle via `cx.listener` on `CommitView` mutating
`collapsed_dirs` + `cx.notify()`.

## Verify
`cargo build -p git_ui`; MCP live: open a commit with nested changed paths in the
History tab → `CommitView`, screenshot the tree, collapse/expand a folder,
screenshot. FORK.md decision entry.
