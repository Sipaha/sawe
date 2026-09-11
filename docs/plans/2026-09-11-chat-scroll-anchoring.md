# Stable chat scrolling while measuring wrapped history

Status: complete

## Goal
Keep the same visible conversation content anchored while scrolling upward when newly measured, wrapped messages are taller than estimated.

## Reproduction described by the user
Rows a,b are above the viewport; c,d below. Scrolling upward into b reveals that b spans three row-heights. The added height should extend above the current visual position rather than pushing the viewport to the beginning of b and skipping a large section.

## Scope
Trace Solution conversation virtualization, GPUI list measurements, scroll anchors and resize/streaming updates. Fix the cause without forcing all history to render eagerly or increasing per-frame work with transcript length.

## Architecture
Preserve the reader's visual anchor while browsing history; maintain follow-tail behavior only when actually pinned to the bottom. Distinguish user scroll from height corrections and preserve intended wheel movement.

## Verification
Add a deterministic regression with variable-height items expanding above/at the viewport during upward scrolling, plus bottom-follow behavior. Run targeted tests, debug build, and a headless UI repro with wrapped long messages. Final release-fast after all active tasks merge.

## Delivery
Record the algorithm and any known limits in FORK.md and a finding, update INDEX, commit/push alongside Codex, prompt audit and context recovery work. Screenshots stay outside Git.

## Implemented

GPUI preserves the prior visible row while measuring backward through newly crossed rows. Signed pixel accumulation retains net movement across direction reversals. Deterministic tests cover stale 100px→300px heights, multiple events before repaint, 1000 cold rows, bounded measurement and return to tail. Details: `docs/findings/2026-09-11-chat-scroll-anchoring.md`.

## Merged test results

915 tests passed: GPUI list 24, Codex native 7, console panel 39, Solution agent 835, agent prompt templates 9, multiline summary streaming 1. One existing Solution-agent test remains ignored. Workspace formatting, diff whitespace and scoped debug `script/clippy` checks passed without warnings.

Debug and release-fast builds completed successfully. Final headless UI checks passed for context recovery controls, visible cold-session errors and wrapped-history scrolling. Temporary screenshots/probes were excluded from Git.
