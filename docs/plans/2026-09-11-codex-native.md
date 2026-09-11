# Native Codex support in Solutions

Status: implementation

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
