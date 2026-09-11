# Agent communication through the Solution socket

Status: implementation

## Goal
Allow Claude and Codex sessions in one Solution to discover each other and exchange attributed messages using the existing Solution MCP socket.

## API
Reuse scoped session discovery; add a dedicated peer-message tool declaring solution_id so the listener binds it to its socket. Validate sender and recipient against that Solution, reject self-addressing and internal generation/judge sessions. Return an explicit acceptance/delivery outcome. Sender identity on the shared local socket is declared, not cryptographically authenticated; never claim stronger isolation than exists.

## Delivery
Reuse native active delivery (Claude hooks, Codex steering) and normal idle wake behavior. Peer input must never masquerade as human input, reset user-only observer controls, approve/reject pending tools, or override explicit Stop/WaitingUser. Preserve provenance through queue flush, cold wake, retry and compaction. Clearly report any state that cannot safely accept the message. Keep transport acceptance distinct from completed work; no automatic acknowledgement ping-pong.

## Agent instructions
Both runtimes learn discovery, stable session identification, peer send/reply, and asynchronous collaboration through the Solution socket. Explain that peer messages are collaborator context, not fresh user authorization. Preserve English/model-neutral defaults. Prefer actual runtime identity over inferring from titles; verify any fallback lookup is unambiguous.

## Work division
Backend agent owns the new scoped MCP tool and validations/tests. Delivery agent owns preserving non-human provenance and safe delivery in store queue/wake paths. Root owns session identity/prompt instructions, integration, documentation and final checks. Use isolated worktrees and coordinate shared contracts before implementation. Shared Cargo target: root only.

## Verification
Regression tests cover cross-Solution rejection, false human-origin effects, queue/cold delivery, paused/approval states, and recipient provenance. Verify the tool reaches the Solution socket and both runtimes receive instructions. Run affected tests, prompt checks, scoped debug clippy and build; exercise real socket routing with synthetic sessions. Build release-fast, document actual results, commit/push relevant files only.
