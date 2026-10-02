# Kimi integration review

Scope: all seven commits in `main` on October 1, 2026 (Asia/Novosibirsk),
`139a1e62b1` through `6f3bae7dbc`. October 2's MCP schema fix was inspected
as current context. Git records the maintainer as author; this review evaluates
the changes, not independently verified model authorship.

All findings below are addressed by the accompanying changes.

## Confirmed findings

- **P1: missing Solution instructions.** `build_session_meta` writes
  `_meta.systemPrompt`, but Kimi's `newSession` and `resumeSession` ignore it.
  The intended scope, stable session identity and editor instructions were not
  delivered. Carry them in outbound prompt content for Kimi.
- **P1: permission drift on context replacement.** `rotate_context` and
  `reset_context` created a new ACP session without restoring mode or model.
  `Full access` therefore remained visible over a default-mode CLI session.
  Await both controls on every attach path, including clear and compact.
- **P1: permission success reported before acknowledgement.**
  `native_controls::set_permission_mode` detached the RPC and returned `true`;
  the store persisted the new mode even if the server refused it. Apply the
  desired policy during a guarded, awaited attach before accepting prompts.
- **P2: incorrect mode semantics.** Kimi 2.1.1's ACP describes `yolo` as
  approving everything; its actual policy still asks for dangerous commands.
  Its `auto` mode is "Never Ask" and matches Full access. This mode also
  resolves questions automatically. Neither mode enforces a filesystem sandbox.
- **P2: late failure after process recovery.** The recovery guard compared
  provider session IDs, but resume deliberately preserves those IDs. An old
  prompt could overwrite a new thread's state. Guard by thread identity.
- **P2: unreadable approval details.** Kimi's permission request supplies
  `content` without `rawInput`; the existing full-argument viewer required
  `rawInput`. Add an explicit full-request viewer preserving all supplied text
  and arguments, with copy support; give approval controls visible styling.

## Assessment

Reusing ACP and keeping the launcher small are sound choices. Lifecycle
integration and verification were incomplete: six adapter tests mostly assert
constants, labels and constructed prompts, while the reported 981 passing
tests did not establish mode enforcement or instruction delivery. The process
recovery tests were more useful but missed an ordering race. These commits
need behavioral integration tests, not more mapping assertions.

## Evidence and verification

Installed CLI: 2.1.1, SHA-256
`66f47536e40b02bb1d577cdd324768f73d39b8b9472e0ca140e73d8e225cd7de`.
Its embedded source exposes the ACP adapter and policy rules; CLI help confirms
"Ask When Needed" for `--yolo` and "Never Ask" for `--auto`.
Verification results are recorded in the [implementation plan](../plans/2026-10-02-kimi-approval-review.md).
