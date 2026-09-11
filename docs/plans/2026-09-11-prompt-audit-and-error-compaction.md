# Model-neutral prompt audit and error-state compaction

Status: complete

## Goal
Allow context compaction after an agent error; audit all repository-owned model instructions, translate non-English defaults into English, and recommend improvements that work across models.

## Context
Native Codex integration is implemented and verified, with final release builds in progress. The maintainer additionally reported disabled compaction for Errored sessions and requested a repository-wide prompt audit.

## Scope
Audit runtime instruction resources and generated messages, inline/terminal/commit assistant prompts, agent templates, editing/evaluation prompts, and repository agent workflow templates. Translate model-facing default instructions, preserving machine-readable keys, tool names, formatting placeholders and user-provided content.

## Work division
Isolated worktrees: one agent fixes error-state compaction and tests; one audits and translates Solution-agent runtime prompts; one audits and translates prompts outside solution_agent. The supervisor inventories remaining surfaces, reviews neutrality and behavioral changes, merges and verifies.

## Decisions and limits
Universal means clear model-neutral instructions and explicit output contracts. Provider adapters may still require provider-specific protocol tokens or tool schemas. Do not enable disabled subsystems, alter historical transcripts/test data merely for language, or rewrite user-authored prompts stored outside the repository. Existing instructions about project boundaries and approvals retain their intent.

## Verification
Exercise the actual error-state compaction path with a mock backend and the rendered control where practical. Run affected tests, formatting and clippy; build debug and inspect the real UI; build release-fast after merged sources settle. Audit report records coverage, changes, justified protocol exceptions and prioritized recommendations.

## Delivery
Update FORK.md and docs/INDEX.md, commit and push all relevant changes. Exclude screenshots and temporary reports from Git.

## Implemented

English/capability-based runtime defaults, model-neutral assistant and maintenance templates, precise user-language preservation, safe single-pass path substitution, and documented protocol exceptions. Context controls accept Errored; cold `/clear` is intercepted locally; asynchronous clearing rechecks that the source conversation did not change. Multiline summary chunks are preserved and prompt override management no longer recursively deletes user directories. Full inventory and proposals: `docs/findings/2026-09-11-model-neutral-prompt-audit.md`.

## Merged test results

915 tests passed: GPUI list 24, Codex native 7, console panel 39, Solution agent 835, agent prompt templates 9, multiline summary streaming 1. One existing Solution-agent test remains ignored. Workspace formatting, diff whitespace and scoped debug `script/clippy` checks passed without warnings.

The final UI check confirmed both context actions are enabled for a cold Errored session. Its badge now preserves the error instead of overriding it with Sleeping.

Debug and release-fast builds completed successfully. Final headless UI checks passed for context recovery controls, visible cold-session errors and wrapped-history scrolling. Temporary screenshots/probes were excluded from Git.
