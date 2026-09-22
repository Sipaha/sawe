# Integrate current upstream Zed into Sawe

**Status:** in progress
**Requested by:** Pavel, 2026-09-22: «а давай вольем свежую ремоут версию к нам».

## Goal
Merge fresh upstream Zed into Sawe, preserving the fork's product identity,
Solution workflows, native AI providers, remote control, and Git-panel fixes.

## Authorization and scope
This explicit user instruction authorizes a one-time upstream merge despite
the older no-merge guidance in `.rules` and ADR-0001. It does not authorize
re-enabling disabled services, renaming Sawe, resetting user data, or adding an
automatic merge cadence. Do not ask the user to repeat this authorization.

- Starting Sawe main: `1535e330a5`.
- Pinned upstream main: `b54cc1d0acc8fe3f7581721ee1195516e7581f9d` (2026-09-22).
- Merge base: `c1b45aaa5f31401fa5368a8c9636f9d8db979517`.
- Divergence at start: 1133 fork-side commits, 1676 upstream-side commits.
- Upstream requires Rust 1.98.1.

## Integration strategy
Use a dedicated integration branch/worktree inside this Solution. Resolve
conflicts semantically rather than selecting one side globally. Separate agent
worktrees own disjoint conflict groups; the supervisor integrates their patches
and owns root manifests, the lockfile, shared integration points and final checks.
Retain both histories in a real merge commit and fast-forward main only after
verification. Never rewrite existing history or force-push.

## Product invariants
- Sawe branding, CLI/bundle IDs, `.sawe` project settings, and the `~/.spk/sawe`
  profile and Solution storage layout stay intact.
- Solutions, solution_agent (Claude/Codex), embedded MCP, remote control,
  project toolbar, Solution band and run configurations remain functional.
- Upstream collab UI, sign-in, auto-update, telemetry, cloud-only model services,
  Zeta and Sentry uploads remain disabled at their existing integration seams.
- Keep the Changes/Commit UI and regression coverage from `45f43cd492`; adopt
  compatible upstream fixes without losing Solution-specific repository routing.
- Preserve license and upstream attribution requirements.

## Verification
1. Confirm no unresolved merge markers or unmerged index entries remain.
2. Use Rust 1.98.1; keep any new toolchain installation inside the Solution.
3. Run formatting, a normal debug editor build, and the workspace/all-targets
   check where feasible; diagnose failures rather than suppressing them.
4. Run focused tests for conflict-heavy crates and fork-owned integration crates,
   including Git-panel regressions and native watcher tests. Test temporary
   projects must be outside member Git checkouts but inside the Solution.
5. Launch an isolated headless debug editor; verify Solution/member switching,
   Git panels, agent controls and representative editor UI with screenshots.
6. Once source and debug checks settle, build release-fast for the next user launch.
7. Update documentation, commit, fast-forward main, and push origin/main.

## Documentation
Record conflict decisions, compatibility adaptations, actual check results and
remaining blockers here. Amend ADR-0001 and `.rules` only to reflect the user's
explicit authorization and the new pinned baseline, without inventing a cadence.

## Progress
Initial repository clean; upstream fetched and pinned. Integration pending.
