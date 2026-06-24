# Finding: Agent-tool worktrees branch from session-start HEAD, not current HEAD

**Date:** 2026-05-15
**Status:** active gotcha — workaround in place; tool-level fix not in our hands

## Symptom

Supervisor's workflow expects `git commit <plan-doc>` → `Agent({isolation:
"worktree"})` → sub-agent reads the freshly-committed plan-doc from the
worktree. Observed 2026-05-15 (clickable-tree phase): sub-agent reported
**"plan/workflow docs are not in this worktree"** — `docs/plans/2026-05-15-
clickable-tree.md`, `docs/workflow/supervisor-mode.md`, `docs/architecture/
decisions/0001-fork-philosophy.md`, etc. were all absent.

Verified post-merge by inspecting the worktree's commit history:

```
$ cd .claude/worktrees/agent-af208bef99d679d50 && git log --oneline -6
c3d8e90048 solutions: surface clickables array on dump_visual_structure result
6e3bf489dc workspace: add clickables module + windows.click_id MCP tool
1db0affc7f gpui: add Window::iter_hitboxes accessor for clickable-tree surface
248593ea7b Merge branch 'remove-auto-shelve': remove auto-shelve crash-recovery feature
aba24467e4 git: remove auto-shelve crash-recovery feature
bddbbe8630 Merge branch 'idea-parity-followups': run-config & git-graph & autosave IDEA-parity tweaks
```

The worktree branched from `248593ea7b` — the session-initial main HEAD
when this Claude session started. None of the in-session commits
(`f64e50c` workflow scaffold, `1606fdf` headless prereqs, `68ac7af`
clickable plan, etc.) were visible to the sub-agent.

## Why

The Claude Agent SDK's `isolation: "worktree"` mode snapshots its base
from session-start, not from current HEAD at dispatch time. Likely a
deliberate safety choice: a sub-agent that hits an unexpected state
shouldn't pull in supervisor-side changes that haven't been validated
yet. But it breaks our "commit plan, then dispatch" ritual.

## Impact

- Sub-agent operated without reading the plan doc; followed the prompt
  alone. Quality was still good (~95% of spec implemented), but the
  acceptance items they couldn't tick (because the doc wasn't there)
  required supervisor follow-up.
- More subtly: every sub-agent reads the **session-initial** `.rules` /
  `CLAUDE.md` / `FORK.md`. Any rules added mid-session don't reach
  sub-agents in that session.

## Workaround

The supervisor's dispatch prompt **must include the full plan doc content
inline** when the plan doc was created mid-session. Either:

1. Paste the plan doc body into the dispatch prompt (under a clear
   `## PLAN DOC (inline because worktree may be stale)` header).
2. Or commit + tag + push, dispatch with explicit instruction to
   `git fetch && git checkout <tag>` — heavyweight, not viable for
   private fork without remote sync.

Option 1 is the practical fix. Add to the sub-agent prompt template.

## Adjacent rule update

`docs/workflow/supervisor-mode.md` § 4 "DISPATCH — sub-agent prompt
template" gets a note: **when the plan-doc was created or last modified
in the current session, paste it inline under `## PLAN DOC` rather than
linking the path. Sub-agent worktrees are pinned to session-start state.**

Same applies to recently-modified `.rules` / `CLAUDE.md` / `FORK.md`
sections.

## Doesn't apply to

- Plan docs that were committed BEFORE the session started — those are
  on the session-initial HEAD and visible to the worktree.
- Workspace state (uncommitted files) — these are NEVER in worktrees by
  design.

## Verification trick

To check whether the dispatched worktree sees your latest commits, look
at the first line of the sub-agent's report — if it says "the plan doc
isn't here", the workaround applies. Or proactively, `cd <worktree-path>
&& git log --oneline -5` right after dispatch.

## Status

This is an Agent-SDK behavioural quirk we work around in our prompts.
Not an SPK Editor bug; no upstream fix to file.
