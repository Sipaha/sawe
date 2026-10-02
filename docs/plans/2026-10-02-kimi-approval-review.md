# Kimi integration review and approval fixes

Status: complete.

## Scope

Review the seven main commits dated 2026-10-01 in Asia/Novosibirsk, from
`139a1e62b1` through `6f3bae7dbc`. Fix unreadable approval commands, weak
button affordances, and permission drift in Kimi Solution sessions.

## Findings confirmed

- Permission RPCs are detached before success is known.
- Context replacement through clear/compact does not restore ACP controls.
- Approval content can carry the command without a `rawInput` argument.
- Installed CLI help and ACP mode descriptions disagree; actual engine policy
  and a real command probe confirm that Full access requires `auto`.

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

## Results

- `cargo test -p agent_servers -p solution_agent --lib`: 38 and 992 passed,
  respectively; 0 failures, 1 existing ignored test in `solution_agent`.
- Updated an obsolete agent-server test: Sawe forces ACP beta on, so its
  assumption that a settings override disabled it was false before this work.
  The test still verifies first-class terminal authentication routing.
- Installed Kimi 2.1.1 ACP smoke: initialize, new session, `set_mode(auto)`,
  then a real Bash `chmod +x ./approval-probe.sh` in an isolated home/workdir.
  Result: `end_turn`, executable bit set, zero permission requests.
- Headless UI fixture reproduces content-only multiline approval requests:
  full-request preview opens, the final `END-OF-COMMAND` marker is visible,
  and the preview exposes Copy. Final dark-theme screenshots verified:
  `final-approval.png` and `final-full-request.png`. Both Approve once and
  Reject returned the correct ACP option. After `/clear`, the replacement
  session received `auto` before its first prompt and retained Solution context.
- Logs and screenshots: solution `.agents/tmp/kimi-*.log` and
  `.agents/tmp/ka/`. No copied CLI credentials are retained after the probe.
- Debug and release-fast editor builds passed. Release-fast emitted only the
  existing build-script informational commit-hash warning, no Rust warnings.
- `script/clippy -p agent_servers -p solution_agent` passed with warnings denied;
  the `CARGO` wrapper removed `--release` to honor agent-only debug verification,
  preserving the script's all-targets/all-features checks.
- Solution boundaries are model instructions, not an OS sandbox. Kimi's `auto`
  mode can resolve questions automatically; Full access deliberately uses that
  provider policy. Read-only remains Kimi's planning policy.
