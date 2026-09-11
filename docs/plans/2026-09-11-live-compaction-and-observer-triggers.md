# Cooperative live compaction and proactive observer checks

Status: complete

## Goal
Allow context compaction during active work, and make an enabled observer sustain autonomous progress with periodic/context-driven checks.

## Approved behavior
The user approved context thresholds: 80% for windows up to 128k, 75% up to 256k, 65% up to 512k, 50% above 512k. Hourly checks apply only while a session has active work. Existing idle checks remain. Explain why a check fired to the judge.

## Compaction
A live request asks the worker to finish its current step at a safe boundary, write the standard handoff and call the existing compaction completion tool. Never force-kill a running tool or erase context at request time. Use existing native steering/queue semantics; when immediate steering is unsupported, queue safely. Deduplicate pending requests. Preserve clear-context protections, explicit approval waits, cancellation and newer user intent.

## Observer
Keep disabled/held/stopped/waiting-user/backoff/typing protections. Hourly/context checks may inspect running work without stopping it. A review taken during running work must not cause duplicate continue/wait/ask actions or discard newer human instructions. Permit a cooperative compact verdict from the active review while keeping idle-review race guards. Deduplicate threshold triggers and rearm after context rotation/recovery. No recursion on judge/auditor sessions.

## Prompt
State the operator's intent explicitly: proceed autonomously within authorized scope; ask the human only for indispensable input; record blockers and continue independent TODOs. Enabling the observer does not create missing permissions. Preserve user language and earlier constraints.

## Work division
Live-compaction agent owns compact.rs, relevant model/queue/lifecycle/status-row integration and tests. Scheduling agent owns supervisor state and engine scheduling/apply guards and tests. Root owns judge/system prompt prose, integration, UI verification and documentation. Coordinate the compact-pending accessor across scopes.

## Verification
Deterministic tests for active requests, deduplication, approval gates, safe completion, timer boundaries, thresholds, resets and stale active reviews. Run affected suites, prompt contracts, debug clippy/build and headless rendered UI smoke. Build release-fast, update docs, commit/push without screenshots.

## Active follow-up delivery
The user additionally requested verification of both native runtimes. Codex uses `turn/steer` with the current `expectedTurnId`; reserve stable queue bundles until a receipt, retry only definite rejection, and surface ambiguous delivery without automatic repetition. A local Claude Code 2.1.258 experiment sent a stream-json user message during a running Bash command: the new instruction was consumed before the single final result. Evaluate a receipt-aware native stream path against the existing PostToolUse/Stop hooks, preserving turn ownership, targeted teammate delivery and queue ordering.

## Clarified autonomy boundary
Routine ordering between approved tasks is an agent decision unless the user explicitly reserves it. Consequential architecture/product choices that are unresolved and undelegated remain human decisions. A visible question alone is not a blocker; preserve real blockers and continue independent TODOs. Added three synthetic regression cases and checked both installed providers.

## Final verification
The affected debug suites passed 1,053 tests (856 solution_agent, 55 solution_git, 39 console_panel, 92 claude_native, 11 codex_native); the existing non-assertion timing probe remains ignored. Prompt contracts and 21 Python tests passed. Scoped debug script/clippy passed with warnings denied. Both debug and release-fast binaries were rebuilt after the final UI correction.

Real Codex active-input smoke passed in an isolated temporary Solution: one final response reflected the follow-up sent during a running command, with no duplicate or pending queue. Rendered UI at 102.4k/128k (80%) enabled Compact and disabled Clear. Claude native-stream cancellation evidence is documented separately; its existing hook remains. Temporary worktrees were removed; screenshots and model reports were excluded from Git.
