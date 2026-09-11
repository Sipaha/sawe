# Cooperative live compaction and proactive observer checks

Status: implementation

## Goal
Allow context compaction during active work, and make an enabled observer sustain autonomous progress with periodic/context-driven checks.

## Approved behavior
The user approved context thresholds: 80% for windows up to128k,75% up to256k,65% up to512k,50% above512k. Hourly checks apply only while a session has active work. Existing idle checks remain. Explain why a check fired to the judge.

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
