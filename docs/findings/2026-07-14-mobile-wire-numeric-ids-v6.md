# Mobile client crashed on numeric ids — wire schema bumped to v6

**Date:** 2026-07-14
**Status:** fixed (editor + mobile, lock-step)
**Repos:** editor (`crates/editor_mcp`) + `spk-editor-mobile` (`Sipaha/sawe-mobile`)

## Symptom (user, live)

The mobile client failed to decode the editor's wire messages:

```
String literal for value of key 'id' should be quoted at path: $.id
```

kotlinx.serialization rejecting a bare JSON number where its DTO declared a
`String`.

## Root cause

The identity migration (2026-07-13, FORK.md #50) made Solution / member /
catalog ids **surrogate counters** (`i64`). Every wire field carrying one became
a JSON **number**: `SolutionSummary.id`, `SessionSummary.solution_id` /
`member_id`, `WorkspaceSolution.id`, the `workspace.*` payload `solution_id`s,
`catalog_id` everywhere, and the `solution_id` **parameter** of the
`workspace.open_solution` / `close_solution` tools. Session ids and agent ids
stayed strings.

That shape **shipped without bumping `wire_schema_version`** (stayed 5), and the
mobile DTOs still declared these ids as `String`. So a v5 mobile talking to a v5
editor got a number where it wanted a quoted string → decode crash, with no
version gate to catch the mismatch. (On this codebase a "member id" *is* its
`catalog_id` — there is no separate `member_id` field on the mobile wire.)

## Fix

Bumped the wire schema to **v6** on both sides so the break is explicit and an
un-migrated client gates on `IncompatibleServerScreen` instead of crash-decoding.

- **Editor** (`1679c533d0`): `crates/editor_mcp/src/tools/capabilities.rs`
  `wire_schema_version` 5 → 6 with a v6 history entry; `server_e2e_test.rs`
  assertion updated to `>= 6`.
- **Mobile** (`Sipaha/sawe-mobile` `c58d88a`, 26 files): every Solution / catalog
  id field, VM mirror, client-method parameter, map key, and test fixture flipped
  `String` → `Long` (`RemoteDtos.kt`, `RemoteClient.kt`, the `app` VMs/UI/repos,
  the `cli` `open-solution`/`close-solution` subcommands). Session / agent /
  operation / tool-call ids stay `String`. `SUPPORTED_WIRE_SCHEMA_VERSION`
  5 → 6; `CachedSessionHistory.CACHE_SCHEMA_VERSION` 2 → 3 to drop
  string-keyed cache blobs. Notable seams: the shared
  `WorkspaceClientImpl.lifecycleCall(value: String)` was split into a `Long`
  (solution) and `String` (session) overload so `solution_id` serialises as a
  bare number while `session_id` stays quoted; nav routes keep `NavType.StringType`
  with a `.toLong()` boundary; catalog-id display fallbacks gained `.toString()`.

## Verification

- Editor: `editor_mcp::server_e2e_test` green; `editor.capabilities` now
  advertises `wire_schema_version: 6`.
- Mobile (independently re-run with `--rerun-tasks`): `:core:test` 332/0,
  `:app:testDebugUnitTest` 41/0, `:app:compileDebugKotlin` + `:cli:compileKotlin`
  BUILD SUCCESSFUL. New regression test
  `RemoteDtosTest::workspace_solution_decodes_numeric_ids` decodes a
  `WorkspaceSolution` (nested `SessionSummary`) with bare-number `id` /
  `solution_id` and a string session `id` — the exact crash shape.

## Operational note

Editor v6 and mobile v6 must ship together: a v6 mobile gates out a v5 editor
and vice-versa. The editor `release-fast` binary is rebuilt at v6; the mobile
commit is **not pushed yet** (awaiting the maintainer). The old
`docs/superpowers/specs/2026-05-27-unified-open-workspace-design.md` still shows
`solution_id: String` — it is a dated design artifact, superseded by this note.
