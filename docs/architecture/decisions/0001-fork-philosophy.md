# ADR-0001: Fork philosophy — no scheduled upstream merge, two-zone refactor rules

**Status:** accepted
**Date:** 2026-05-15
**Deciders:** Pavel Simonov (@Sipaha)
**Related:** [`FORK.md`](../../../FORK.md), [`CLAUDE.md`](../../../CLAUDE.md) § "Fork philosophy"

---

## Context

SPK Editor is a personal hard fork of [Zed](https://github.com/zed-industries/zed)
maintained by @Sipaha. The fork has shipped meaningful local additions
(`solutions`, `solution_agent`, `solutions_ui`, `editor_mcp`, `solution_git`,
`run_config*`) and disabled meaningful upstream subsystems (`collab`,
`auto_update`, `telemetry`, `zeta`, native cloud LLM, Sentry, the upstream
agent panel) — see [`FORK.md`](../../../FORK.md) for the full table.

A standing decision is needed on how the fork relates to upstream over time:

- Do we merge `upstream/main` on a cadence, or never?
- Do we cherry-pick specific upstream commits, or maintain full independence?
- Are upstream files off-limits to refactor (to keep merge cost low), or
  fair game (treating each file as locally-owned the moment we touch it)?
- How do we communicate the answer to sub-agents who don't have the long
  context of "why this rule"?

Without a fixed answer, every session re-litigates these. Worse, sub-agents
default to one interpretation or another and produce drift — either a
sub-agent treats every file as locally-owned and renames identifiers across
the codebase (raising cherry-pick cost we may want), or a sub-agent
treats every upstream-shaped file as untouchable and refuses to fix a bug
in-tree.

---

## Options considered

### Option A — Track upstream via scheduled `git merge upstream/main`

A cron-style "merge upstream every N weeks". The familiar model for forks
that intend to converge with upstream eventually.

- **Pro:** stays close to upstream features and bug fixes for free.
- **Con:** the fork has *deliberately disabled* upstream subsystems
  (`collab`, `zeta`, `CloudLanguageModelProvider`, upstream `AgentPanel`).
  Upstream evolution of those subsystems → repeated conflict resolution on
  every merge, with the conflicts always re-introducing the disabled code.
- **Con:** fork-owned crates (`solution_agent`, `solutions_ui`, etc.) wire
  into upstream files at multiple integration points. Upstream restructuring
  of those files conflicts predictably; resolving by hand on a cadence is
  expensive and error-prone.
- **Con:** no functional benefit — the fork doesn't depend on upstream
  receiving its features (we run local-only, no shared service infrastructure).

### Option B — Full divergence, never look at upstream again

Treat upstream as a snapshot. Never merge, never cherry-pick.

- **Pro:** zero merge cost.
- **Con:** upstream still ships genuinely useful primitives — LSP improvements,
  language support, editor features, GPU backend fixes. Cutting them off is
  pure loss.

### Option C — No scheduled merge, selective cherry-pick on demand, two-zone refactor rules

No `git merge upstream/main` on a cadence. The fork ships features and bug
fixes directly. We **may occasionally cherry-pick** specific upstream
commits/crates that bring genuinely new core capabilities (LSP, language
support, editor primitives, GPU backend, etc.) — and the only reason for
"be careful with upstream-shaped code" is **that** cherry-pick lane.

The codebase is split into two zones with different rules:

- **Fork-owned zones** (`solution_agent`, `solutions`, `solutions_ui`,
  `editor_mcp`, `solution_git`, `run_config*`, `git_conflict_ui`, plus any
  upstream files we've already modified — listed in
  [`FORK.md`](../../../FORK.md) § "Notable upstream file modifications"):
  **refactor freely**. We're not getting upstream patches into these files
  anyway; diff-minimality buys nothing.
- **Untouched upstream crates** (`editor`, `language`, `lsp`,
  `multi_buffer`, `project`, `terminal`, `dap`, `vim`, `theme`, bulk of
  `gpui`, etc.): **stay upstream-shaped**. Don't refactor / rename /
  cleanup for style. Bug fixes in-tree are allowed (rule is "don't
  refactor for style", not "never touch"); additive patches preferred.
  An ungated non-additive refactor of one of these files is allowed only
  when it pays for itself meaningfully — and the supervisor signs off
  (sub-agents surface in REPORT, don't take the call).

---

## Decision

**Option C** — no scheduled upstream merge, selective cherry-pick on demand,
two-zone refactor rules.

Load-bearing reasons:

1. The fork's disabled subsystems would re-conflict on every scheduled merge
   to no benefit (we'd re-disable the same code monthly forever).
2. Cherry-picking specific upstream commits when a useful core capability
   lands is cheap and predictable — we pay merge cost only when we explicitly
   want upstream code, not on every cadence.
3. The two-zone split gives sub-agents a clear rule they can apply without
   re-deriving the why each session: *if the file is in FORK.md's "Notable
   upstream file modifications" table, refactor freely; else stay
   upstream-shaped*.
4. The "escape valve" (supervisor-authorised non-additive refactor of an
   untouched file when it pays) preserves the right to fix code that's
   genuinely better restructured locally, without making it the default.

This decision is **load-bearing** for:
- The structure of [`FORK.md`](../../../FORK.md) (touched-files table is the
  pivot for the two-zone rule).
- The HARD RULES block in [`supervisor-mode.md`](../../workflow/supervisor-mode.md)
  sub-agent prompt template.
- The dispatch model for refactor proposals (sub-agents surface, supervisor
  decides).

---

## Consequences

### Positive
- Zero ongoing merge cost. The fork ships at its own pace.
- Sub-agents apply the two-zone rule from a single source of truth
  (FORK.md's touched-files table) without needing per-file judgement.
- Cherry-picks remain possible when upstream ships something genuinely
  worth importing.
- Rebrand identifiers (display name, CLI binary, bundle IDs) are stable —
  no surprise upstream renames to reconcile.

### Negative
- The fork drifts further from upstream over time. Cherry-picking an
  upstream change against a heavily-diverged file may itself require
  manual work.
- We bear the cost of every upstream bug-fix that doesn't migrate to us
  (we have to notice + port).
- The two-zone rule is one more thing to teach each new session (mitigated
  by FORK.md + this ADR + the HARD RULES block in supervisor-mode.md).

### Reversibility
- **Switching to Option A (scheduled merge)** would be expensive: re-enabling
  disabled subsystems would touch every fork crate that knows about their
  types/globals. Estimate: 1–2 weeks of cleanup before the first merge
  even attempts.
- **Switching to Option B (full divergence)** is free; we just stop
  cherry-picking. No code changes.
- Switching the per-zone rule (e.g. allowing untouched-file refactors
  freely) would require updating FORK.md, this ADR, and the HARD RULES
  block in `supervisor-mode.md` — small change, but propagation matters
  because sub-agents read all three.

---

## How to apply

For the **supervisor**:
- When dispatching a sub-agent that will touch an upstream file:
  - Check FORK.md "Notable upstream file modifications" first.
  - If listed → sub-agent may refactor freely; include the file in the
    SCOPE.
  - If not listed → sub-agent gets the "stay upstream-shaped" rule in the
    HARD RULES block. If a refactor would genuinely pay, request it
    explicitly and add the row to FORK.md in the same dispatch.
- A sub-agent REPORT proposing a non-additive refactor of an untouched
  file is **not** a green light to land it — supervisor evaluates and
  either accepts (adds FORK.md row, re-dispatches with the broader scope)
  or rejects (asks for the additive form).
- When you do cherry-pick from upstream, treat it as a HEAVY-track phase:
  plan doc, scoped sub-agent dispatch, full verification matrix.

For **sub-agents** (this is mirrored into the HARD RULES block in
[`supervisor-mode.md`](../../workflow/supervisor-mode.md)):
- Untouched upstream files (not in FORK.md's table): bug fixes fine,
  additive patches preferred, no style refactors.
- First-touch of an upstream file → add a row to FORK.md "Notable
  upstream file modifications" in the same commit.
- Refactor proposal on an untouched file → surface in REPORT, don't take
  the call yourself.

Files that encode this decision:
- [`FORK.md`](../../../FORK.md) — touched-files table is the
  refactor-zone pivot.
- [`CLAUDE.md`](../../../CLAUDE.md) § "Fork philosophy" — supervisor's
  always-in-context summary.
- [`docs/workflow/supervisor-mode.md`](../../workflow/supervisor-mode.md)
  § "HARD RULES" — sub-agent-facing version.
