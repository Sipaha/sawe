# Kimi integration review and approval fixes

Status: in progress.

## Scope

Review the seven main commits dated 2026-10-01 in Asia/Novosibirsk, from
`139a1e62b1` through `6f3bae7dbc`. Fix unreadable approval commands, weak
button affordances, and permission drift in Kimi Solution sessions.

## Findings to verify

- Permission RPCs are detached before success is known.
- Context replacement through clear/compact does not restore ACP controls.
- Approval content can carry the command without a `rawInput` argument.
- Installed CLI help and ACP mode descriptions disagree; inspect actual behavior.

## Implementation

Keep the existing ACP adapter and shared conversation renderer. Await controls
before making new or restored sessions available for prompts. Preserve stored
policy on failure. Make approval details explicitly inspectable and use visible
button backgrounds and borders. Preserve the Solution boundary instructions.

## Acceptance

- Long and multiline approval commands can be inspected and copied.
- Allow and reject controls look interactive without hovering.
- New, resumed, cleared and compacted Kimi sessions retain the chosen policy.
- Failed mode changes cannot silently claim a different effective policy.

## Verification

Run focused regression tests, affected-crate checks, a debug editor build and
headless UI screenshots. Build release-fast for the user's editor after fixes.
Probe the installed CLI protocol where behavior cannot be inferred reliably.

## Review record

Record confirmed findings and limitations in a dated finding linked from
`docs/INDEX.md`; update FORK.md's Kimi contract with the resulting behavior.
