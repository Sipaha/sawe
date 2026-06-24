# Session handoff — 2026-05-15

**Status:** session paused for context reset; resume from current `main`.

This finding captures what landed across the 2026-05-15 supervisor
session and what's still in the pool. Read this first on resume — it's
the index into all the other plans / ADRs / findings created today.

## What shipped (cumulative — every commit on `main` from this session)

| Phase | Status | Commit chain | Plan / artefact |
|---|---|---|---|
| Workflow scaffolding | shipped | `f64e50c` | [`docs/INDEX.md`](../INDEX.md) + [`workflow/supervisor-mode.md`](../workflow/supervisor-mode.md) + [`workflow/doc-discipline.md`](../workflow/doc-discipline.md) + [`workflow/adr-template.md`](../workflow/adr-template.md) + [`architecture/decisions/0001-fork-philosophy.md`](../architecture/decisions/0001-fork-philosophy.md) |
| Delete-solution silent drop fix | shipped | `ff4abc8` | (LIGHT) `solutions_ui::delete_solution_with_cleanup` |
| `run-mcp` stale state + Xvfb windows.list fix | shipped | `1606fdf` | (LIGHT) |
| Native headless platform (no Xvfb) | shipped | `4a11fb8` → `bab1559` | [`plans/2026-05-15-headless-platform-real.md`](../plans/2026-05-15-headless-platform-real.md) + ADR-0002. Resolves [`findings/2026-05-headless-screenshot-blank.md`](2026-05-headless-screenshot-blank.md). Supervisor hotfix `56d7fee`: `atlas.before_frame()` in offscreen + calloop refresh timer. |
| Clickable tree + click-by-id (phase 1) | shipped | `1db0aff` → `07c3cee` + supervisor `3954269` (phase 1b labels) | [`plans/2026-05-15-clickable-tree.md`](../plans/2026-05-15-clickable-tree.md) |
| Picker + ProjectPanel UI tweaks | shipped | `5b230b1` → `88a3fc7` → `d23a124` | [`plans/2026-05-15-picker-and-panel-ui-tweaks.md`](../plans/2026-05-15-picker-and-panel-ui-tweaks.md) |
| Deterministic 1920×1080 in headless | shipped | `54a2ba1` | (LIGHT, supervisor inline) |
| Remote Control R-1 (UI scaffolding) | shipped | `ee50a95` + `5faad00` + `6c97a48` → `5735a4c` | [`plans/2026-05-15-remote-control-R1.md`](../plans/2026-05-15-remote-control-R1.md) + arc scoping [`plans/2026-05-15-remote-control.md`](../plans/2026-05-15-remote-control.md) |
| Remote Control R-1.5 (QR popover) | shipped | `d9fa51c` | [`plans/2026-05-15-remote-control-R1-5.md`](../plans/2026-05-15-remote-control-R1-5.md) |

20+ commits on `main`, all clean. Working tree clean at session end.

## Findings created this session

- [`2026-05-headless-screenshot-blank.md`](2026-05-headless-screenshot-blank.md) — **resolved** by ADR-0002.
- [`2026-05-agent-worktree-staleness.md`](2026-05-agent-worktree-staleness.md) — **active** gotcha; workarounds in `supervisor-mode.md` § 3.

## ADRs created

- [`0001-fork-philosophy.md`](../architecture/decisions/0001-fork-philosophy.md) — no scheduled upstream merge + two-zone refactor rule.
- [`0002-native-headless-platform.md`](../architecture/decisions/0002-native-headless-platform.md) — native GPUI headless, no Xvfb.

## Workflow rules established (in `supervisor-mode.md`)

- **Two-track model** (LIGHT bugfix / HEAVY phase + plan-doc + dispatch).
- **No priority polls between phases** (`§ 7 NEXT`): when the user has named a task pool, supervisor picks order on own judgement and starts the next phase in the same turn. Recorded in auto-memory (`feedback_supervisor_no_priority_questions.md`).
- **Worktree-staleness trap** (§ 3): paste plan-doc inline in dispatch prompt for any plan committed in the same session; instruct sub-agents to `git rebase origin/main` if their base looks stale. Sub-agent for R-1.5 rediscovered the rebase workaround unprompted — solidified in the doc.
- **Untouched-upstream refactor rule** (ADR-0001 + HARD RULES block): bug fixes fine, additive patches preferred, no style refactors. Listed-in-FORK.md files refactor freely.

## Pool — outstanding tasks at session end

| Item | Track | Where | Notes |
|---|---|---|---|
| **R-2** Server protocol + listener | HEAVY (large) | `crates/remote_control` (new MCP-equivalent namespace `remote.*`) | Needs ADR-0003 first — pick WebSocket+Noise vs raw TCP+TLS, decide NAT/uPnP, decide MCP-subset vs custom protocol. Open design questions in [`plans/2026-05-15-remote-control.md`](../plans/2026-05-15-remote-control.md) § "Open design questions". |
| **R-3..R-6** | HEAVY × 4 | (Same arc) | Phase R-3 = client provisioning UX (already mostly UI from R-1/R-1.5 — needs server-side auth from R-2). R-4 = `remote.*` MCP tools. R-5 = Android client (`spk-editor-mobile`, separate repo). R-6 = polish. |
| **E** Queued message → claude | HEAVY | `crates/solution_agent` | User-requested 2026-05-15: send a queued message while the agent is mid-turn; auto-dispatch on turn completion. Backend (store queue) + UI (input stays editable + "queued" indicator). |
| **F** Sub-agent indication UI | HEAVY | `spk-cockpit` (different project) | User-requested 2026-05-15. Show running sub-agents with progress / tokens / interrupt. Cross-project, colder context. |
| **G** `spk-image://` URL in queued message | LIGHT | `spk-cockpit` (different project) | User-reported 2026-05-15: images don't open in queued messages — the custom scheme isn't registered. |

**Picking order recommendation (per § 7 NEXT heuristic + supervisor judgement):**
1. **E (queued message)** — user-facing UX win, contained to `solution_agent`, no architectural decisions blocking.
2. **R-2** — requires ADR-0003 first (~30 min plan + ADR), then sub-agent dispatch. Big phase.
3. **G** then **F** — cross-project work, save for when supervisor has a fresh session.

## Open architectural decisions

- **ADR-0003 (Remote Control protocol)** — not yet written. Required before R-2 dispatch. Three competing choices: WebSocket+Noise XX (via `snow` crate), raw TLS+TCP+JSON-RPC, or HTTP/2+gRPC. Lean WebSocket — proxy-friendly + JS-friendly for Android side. Decide before R-2.
- **QR rendering renderer choice** — R-1.5 chose `div`-per-module grid because `gpui::svg()` only loads from file paths, not inline strings. If higher fidelity (rasterised QR via a real SVG element) becomes needed, that's a small `gpui` patch — non-urgent.

## Active gotchas the next session should know

1. **Agent SDK worktree branches from session-start HEAD.** Inline plan-doc content + tell sub-agent to rebase. See finding above.
2. **`script/run-mcp --headless` is the default** for agent-driven runs (post ADR-0002, no Xvfb needed).
3. **MCP `windows.click_id`** by stable ID is preferred over `windows.click_at` (pixel coords drift across reflows). 29% of clickables have `file:line` labels; the rest are anonymous hitboxes.
4. **`workspace.screenshot` works in headless** (offscreen wgpu) — ≥60 KB PNG = rendered content; ≤35 KB = blank clear (regression signal).
5. **`docs/superpowers/` is gitignored** for personal drafts; `docs/plans/` is the committed home.
6. **MCP tool catalog count is 60** post-clickable-tree (windows.click_id added). Bump on each new namespace/tool.

## Resume recipe for the next session

1. Read this file first.
2. Read `docs/INDEX.md`.
3. Read `docs/workflow/supervisor-mode.md`.
4. `git log --oneline -25` to see this session's commits.
5. Pick from the pool above per § 7 NEXT.

If the user starts with a direction in their first message, that
overrides the pool ordering. Otherwise default to **E (queued message)**.
