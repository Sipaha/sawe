# Descriptive permission menu and remembered default

## Goal
Present session permissions as readable labeled choices with explanations,
icons and selection state. Remember the last explicit choice for new chats.

## Scope
Existing sessions retain their own persisted policy. New native sessions use
the last user-selected policy, shared across providers and Solutions. With no
saved preference, retain Full access. Internal generation helpers retain their
restricted capability policy.

## Runtime behavior
Mode changes still require an idle session and reconnect the native runtime.
Only successfully applied explicit choices update the preference, including a
choice equal to the current session value. Merely opening/restoring a chat
must never change the default.

## Ownership
A delegated worktree handles persistence/store integration and regression
coverage. Root owns menu layout, any approved mode additions and final checks.

## Verification
Cover persistence/restart, fresh-session metadata/runtime consistency, existing
session isolation and failed selection behavior. Check the rendered menu in an
isolated debug editor. Run affected tests and clippy; build release-fast.

## Delivery
Document runtime semantics and checks, commit/push main. Exclude report junk.

Status: complete

## Automated results
`cargo test -p solution_agent --lib`: 871 passed, one pre-existing ignored.
Added coverage includes a file-backed database reopen, default/existing-chat
isolation, unsuccessful and same-current selections, persistence attachment
after an early selection, and a gated native startup whose runtime metadata and
session state retain the captured policy despite a concurrent default change.

Scoped debug clippy passes with warnings denied. Debug and release-fast builds
completed. In the isolated debug editor, the two existing modes render with
icons, descriptions and a trailing checkmark; all text fits the menu. Re-selecting
Read only in an existing read-only chat sets the preference. A newly created
Codex chat displays Read only and the synthetic app-server receives
`sandbox: read-only`. After quitting and restarting the editor, another new
chat again launches with `sandbox: read-only`. No model inference or working
session contents were needed; screenshots and harness logs remain in /tmp.
