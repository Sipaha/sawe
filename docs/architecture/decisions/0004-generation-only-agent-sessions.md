# ADR-0004: Enforce generation-only sessions in the runtime

Status: accepted
Date: 2026-09-11

## Context

Commit messages, explanations, rebase plans and conflict/cherry-pick suggestions receive source context from the editor. The shared ephemeral runner previously gave them the same role, project instructions and tools as an implementation agent. A prompt asking for read-only behavior does not remove those capabilities.

## Decision

Generation sessions receive a narrow replacement system prompt and no built-in tools, MCP servers, project hooks, plugins or project instructions. Native Claude keeps installed CLI authentication while using its safe mode and explicit empty tool/MCP configuration. Any unexpected tool authorization is denied. These restrictions survive native process respawn.

`AgentConnection::supports_generation_only` defaults to false. The store requires explicit runtime support before relying on `_meta.generationOnly`; otherwise an unknown ACP metadata extension could be silently ignored. Supervisor and interactive sessions retain their existing roles and permissions.

## Consequences

Callers must supply enough evidence for the requested text. They cannot ask the model to discover repository history or inspect files implicitly. Missing context must lead to a constrained answer or uncertainty allowed by the output contract. Cherry-pick evidence is collected by bounded read-only host Git operations.

A future generation provider must implement and test the same restrictions before advertising the capability. No-tools operation is a runtime contract, not a claim that prompts alone prevent instruction injection. CLI/runtime upgrades require capability regression checks.

## Verification

Tests cover session metadata, CLI arguments for initial and resumed sessions, and denial of unexpected tools while retaining interactive policy. Synthetic live probes check model behavior separately from deterministic runtime enforcement.
