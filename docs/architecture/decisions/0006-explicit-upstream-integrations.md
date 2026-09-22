# ADR-0006: Explicit upstream integrations preserve Sawe product contracts

**Status:** accepted
**Date:** 2026-09-22
**Decider:** Pavel Simonov
**Supersedes:** [ADR-0001](0001-fork-philosophy.md)

## Context

After auditing Git-panel bugs against current Zed, the maintainer explicitly
requested «а давай вольем свежую ремоут версию к нам». The older no-merge
wording would contradict this request. The fork owns a substantial product
layer whose behavior cannot be replaced by choosing upstream wholesale.

## Decision

Upstream integration is user-directed, with a pinned revision and an isolated
working branch. This request authorizes merging Zed main at
`b54cc1d0acc8fe3f7581721ee1195516e7581f9d`; it does not create an automatic
update cadence. Every crate remains available for product-driven fixes.

Preserve Sawe identity, profile and Solution paths, native agent workflows,
MCP/remote control and intentional disabled subsystem boundaries. Resolve
conflicts semantically, retain both Git histories, and run build, regression
and native UI checks before advancing the shipping branch. Do not rewrite
history or silently re-enable upstream hosted services.

## Consequences

The maintainer can request useful upstream features and fixes without an
additional permission round trip. Integration still bears the compatibility
cost of the fork's APIs and UI architecture; a textual merge alone is not a
verified update. Existing user data and the running editor remain isolated
from probes. See the [integration plan](../../plans/2026-09-22-upstream-main-integration.md)
for this merge's concrete decisions and results.
