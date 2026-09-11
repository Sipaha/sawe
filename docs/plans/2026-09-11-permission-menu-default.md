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

Status: in progress
