# Apply the prompt audit recommendations

Status: implementation

## Goal
Apply the uncontroversial recommendations from the model-neutral prompt audit while preserving existing product decisions and user authorization.

## Scope
1. Give generation-only ephemeral tasks a narrow role and enforced capabilities rather than the full implementation-agent briefing.
2. Ground cherry-pick suggestions in bounded source diffs and target context, with cache validity and token estimates reflecting that evidence.
3. Version the prompt inventory and add executable placeholder/output contract checks plus provider-neutral behavioral evaluation cases and an opt-in live runner.
4. Shorten supervisor instructions only where existing host enforcement or duplicate wording makes removal behavior-preserving.

## Boundaries
Do not change supervisor stop/resume policy, silently grant permissions, enable disabled products, add Desktop features, or replace provider transports. Keep real model checks synthetic, bounded and explicit; do not claim static fixtures prove model behavior.

## Work division
Isolated worktrees for generation task capabilities, cherry-pick evidence, and prompt contracts/evaluations. Root reviews supervisor instructions, integrations and final verification.

## Verification
Run affected behavioral and parser tests, contract checker self-tests, scoped debug script/clippy and debug build. Use a small synthetic live evaluation for available installed providers where authentication permits. Build release-fast for handoff. Keep screenshots/logs outside Git.

## Completion
Review every audit recommendation, record applied changes and any genuinely disputed remainder, update FORK and INDEX, commit and push main without temporary reports.
