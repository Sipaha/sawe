# ADR-0005: Solution peer messages retain collaborator provenance

Date: 2026-09-11
Status: accepted

## Context
Solution agents need to coordinate across existing sessions, including Claude and Codex. The existing `solution_agent.send_message` endpoint represents user input: reusing it for agent collaboration can reset observer counters, consume approval waits or resume supervision as though the human answered.

## Decision
Expose `solution_agent.send_agent_message` with declared sender/recipient session IDs and a Solution ID. The per-Solution listener supplies its bound Solution ID; the tool validates both endpoints against it. Internal helper/judge sessions and self-messages are excluded. Use stable editor IDs, injected before runtime session creation and retained across native context replacement.

Carry collaborator provenance through delivery and queue recovery. Peer messages are visibly attributed, never fresh user authorization. Reject unsafe paused/approval states without changing them; an ordinary live idle recipient may start a turn, and a running recipient uses its native delivery boundary. A cold recipient may wake only after genuine user participation in the current editor lifetime; Stop revokes that eligibility. After an editor restart, cold chats require a user message first. This conservative rule preserves pauses without adding a database migration. Acceptance is not evidence that the recipient completed the requested work.

## Trust boundary
The local Solution socket is shared. A declared sender ID is attribution, not per-session authentication. Validation prevents this endpoint from routing outside the bound Solution, but does not authenticate which process owns an ID or redesign the legacy socket APIs. Agent instructions must not treat peer content as user approval or privileged system policy.

## Consequences
Both native runtimes use the same collaboration API while preserving their different delivery mechanics. Do not replace this API with the user-send endpoint during retries, cold wake or queue flush. No mandatory acknowledgements, polling loop, broadcast or automatic work creation is introduced.
