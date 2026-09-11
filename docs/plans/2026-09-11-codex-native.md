# Native Codex support in Solutions

Status: complete

## Goal
Choose Codex when creating a Solution chat and use the existing conversation UI with the official Codex app-server runtime.

## Context
Solutions registers only Claude Native. AgentServer and AgentConnection are reusable, but model/effort handling and live message injection are coupled to Claude. Codex CLI 0.153.4 is installed. Its generated protocol schema is the implementation reference, alongside https://learn.chatgpt.com/docs/app-server.

## Scope
- Add codex_native: stdio JSON-RPC initialization, thread start/resume, turn start/interrupt, event translation, approvals, process lifecycle and MCP configuration.
- Register Codex in Solutions; supply solution instructions and preserve existing session persistence.
- Integrate model discovery and supported reasoning settings without exposing Claude-only effort values to Codex.
- Support CLI authentication with actionable setup errors; never copy account credentials into editor settings.
- Fix encountered integration bugs and UI inconsistencies; keep unrelated existing local reports out of commits.

## Architecture
Sawe owns the UI and local transcript projection; Codex owns execution and resumable thread state. A native adapter translates app-server events into AcpThread updates without an external ACP wrapper. Every approval request must receive a user decision or a fail-closed response. Keep process failure from stranding Running sessions.

## Work division
Backend agent works in an isolated worktree on codex_native and workspace registration only. Supervisor implements solution_agent integration on main. Merge sequentially, then run verification against the combined tree.

## Verification
Protocol fixtures cover deltas, tools, failed/cancelled turns, permissions and transport closure. Check model/effort selection and session registration. Build debug Sawe with no warnings, test affected crates, inspect a real headless editor screenshot, and smoke-test installed Codex initialization/models/thread lifecycle without modifying user projects. Build release-fast for user hand-off.

## Completion
Document configuration and limitations, record architecture decisions, update docs/INDEX.md, commit and push to origin/main. Do not include screenshots or temporary reports.

## Desktop feature candidates requested by the maintainer

Prioritize review from the Git panel with line comments, followed by account rate limits and reset times beside the model selector. Next: fork from a selected turn, a task-owned worktree with lifecycle management, and goals/budgets integrated with the existing supervisor. Scheduled project checks need a durable scheduler with overlap prevention and meaningful-change notifications.

App-server exposes review/start, thread/fork, turn/steer, thread/goal and account/rateLimits operations. Task worktree orchestration and the scheduler belong to Sawe. Do not restore per-turn Git checkpoints casually: FORK.md decision 23 disabled them for CPU/IO and object churn. Existing queue, supervisor, Git tools and RunConfig should be extended rather than duplicated.

References: https://learn.chatgpt.com/docs/app-server ; https://learn.chatgpt.com/docs/code-review ; https://learn.chatgpt.com/docs/environments/git-worktrees ; https://learn.chatgpt.com/docs/long-running-work ; https://learn.chatgpt.com/docs/automations . These are candidates, not claims that the features ship in this change.

## Implementation notes

Native session registration, CLI authentication, streamed text/reasoning/tools, approvals, images, resume, model discovery, per-model effort options and stdio/HTTP MCP configuration are connected to the existing Solution UI. The adjacent new-chat menu preserves the existing direct Claude `+` action. Model metadata now has a runtime-neutral persisted representation, with unchanged JSON fields.

Lifecycle review fixed child-thread event contamination, abandoned processes on session close or transport EOF, stale cancellation timers, uncertain turn-start failures and retained tool-output buffers. Selecting another model clears an unsupported effort override. The existing parent-transcript completion test now injects its path resolver and uses a temporary directory instead of creating files under the real Claude home.

Live checks with the installed CLI confirmed a streamed reply, context after agent restart, command approval and output, interruption followed by another successful turn, and the new-chat menu/model controls in a rendered headless editor. Screenshots and probe data are temporary artifacts outside the repository.

Restarting a never-used Codex chat exposed its `no rollout found for thread id` response. The existing missing-session recovery now recognizes that specific response and creates a new thread with the saved model/effort. Failed cold wakes also transition to Errored instead of leaving an endless Running indicator; a turn-identity guard protects any newer session activity.

## Merged test results

915 tests passed: GPUI list 24, Codex native 7, console panel 39, Solution agent 835, agent prompt templates 9, multiline summary streaming 1. One existing Solution-agent test remains ignored. Workspace formatting, diff whitespace and scoped debug `script/clippy` checks passed without warnings.

Debug and release-fast builds completed successfully. Final headless UI checks passed for context recovery controls, visible cold-session errors and wrapped-history scrolling. Temporary screenshots/probes were excluded from Git.
