# Model-neutral prompt audit and error-state compaction

Status: implementation

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
