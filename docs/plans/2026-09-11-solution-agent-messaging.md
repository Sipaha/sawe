# Agent communication through the Solution socket

Status: complete

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

## Verification evidence
The merged regression suite covers sender-header replies A→B→A, invalid
participants, scope override on a real Unix socket, held recipients, provenance,
and identity before native creation. Prompt inventory and 21 Python checks pass.

A real debug editor, isolated under `/tmp/sawe-codex-smoke`, exposed the new tool
only on its Solution socket. Native Codex session `m38ufs3s` read sender
`10q6c7gi` from the incoming header and called the peer tool with its own ID,
delivering `PEER_REPLY_OK` back to `10q6c7gi`. The reply arrived with sender
`m38ufs3s` in its text; both turns completed without a reply loop. Live checking
caught missing per-tool approval configuration and a stale bridge test fixture;
the verified built-in bridge is `--nc <socket>`. Peer send is registered at Write
tier. No user files or working-session content were used in model requests.

A repeated regression run also exposed an existing Claude process-reaping test
race: GPUI virtual time exhausted its retries before the OS reaper ran. Its wait
now uses a real timer, matching the real subprocess under test.

Final regression run: 1062 Rust tests passed (92 Claude native, 11 Codex native,
39 console, 865 Solution agent, 55 Solution Git); one pre-existing timing probe
remains ignored. The real peer reply test completed with the Write-tier bridge.

Scoped all-target/all-feature debug clippy passed with warnings denied. Debug
build and native socket smoke passed; the final release-fast build completed in
6m03s. The temporary editor was stopped and implementation worktrees removed.
Screenshots and temporary reports were excluded from version control.
