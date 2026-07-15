# Plan — IDEA-tight git graph: lanes, no hash column, hash search

**Date:** 2026-07-15 · **Status:** shipped

## Feedback (user, comparing to IDEA's Git Log; reference screenshot #6)
- The commit-graph lanes are too wide — pack them tighter ("4 lanes in a row",
  "minimal indent"), so the graph is compact and the description sits close.
- Nicer graph rendering.
- Don't show the commit hash column in the graph panel — it's noise while
  scanning; the hash should appear only in the detail panel on click.
- BUT search-by-hash must still work.

("дерево" in the feedback = the commit GRAPH, not the changed-files tree from
the prior task — clarified by reference image #6 which is pure graph lanes.)

## Findings
`crates/git_graph/` is a dedicated fork crate; `GitGraph` (`git_graph.rs`) draws
the columned view. Geometry is simple top-of-file constants. Lines are already
bezier (`curve_to`) — no straight→curved work needed. Layout is column-level
(fixed graph-column width then a flex text table), same as IDEA. Search feeds
`filters::QueryFilter` → `--grep` (messages only); hash never matched → hung
empty. `select_commit_by_sha` + `commit_oid_to_index` already exist for jumping.

## Changes (all `git_graph.rs`)
1. `LANE_WIDTH` 16→10, `LEFT_PADDING` 12→8, `COMMIT_CIRCLE_RADIUS` 3.5→3.0.
2. Drop the SHA table column: `render_table_rows` cell, "Commit" header,
   `Table::new(4)`→`(3)`, `RedistributableColumnsState::new(4,…)`→`(3,…)`.
3. `update_query_filter`: `is_hash_like` (hex, ≥7) → `find_loaded_commit_by_prefix`
   → `select_commit_by_sha` (jump + highlight, no grep). Non-hash → grep as before.

## Verify (done)
`cargo build -p git_graph` green; `test_is_hash_like` passes. MCP live on a
branchy merge history: graph compact with tight colored lanes + smooth curves,
no hash column (Graph|Description|Date|Author), typing `9509ee5` jumps to +
highlights that commit with the detail panel showing `# 9509ee5`. FORK.md #56.
