# Codex autonomous turns must settle the editor and its queue

The time-tracking session `vji7i3o7` completed a client turn at 14:41:52 on
2026-10-07, then Codex continued its Goal without a client `prompt()`.
That autonomous turn completed at 14:42:23. Sawe remained Running; at 14:43:01
the user's consent queued and steering rejected it because the original turn
was no longer active. The rejection left the queue waiting for a Stop that
could not arrive.

The native adapter formerly finished only the prompt oneshot. With no sender,
an autonomous completion reset its own turn id but emitted no thread event.
Steering and cancellation also incorrectly required a client prompt sender.

Codex now tracks both kinds of turns. Autonomous start emits
`AcpThreadEvent::ExternalTurnStarted` and a display-only system note. A new
generation and store start anchor distinguish it from the old client turn.
Autonomous completion flushes final buffered text and emits Stopped or Error;
errors and disconnects remain visible. Completion for another turn id is
ignored. Steering and Stop use actual runtime activity, including autonomous
turns, rather than the presence of the client sender.

The store ignores an old client's Stop/Error while a newer Codex turn is active.
The receipt path recovers a stale Running state only after an explicit rejection
and only when both Codex and the client thread confirm idle. That starts queued
input as a fresh turn. Accepted and ambiguous deliveries keep their existing
non-resend semantics; genuine active turns are not force-idled.

The live-Goal probe also reproduced a late-receipt race: completion first
idled the session, then the receipt's user timeline entry reactivated Running.
A Codex user entry recorded with both runtime and client thread idle is now
passive bookkeeping. It remains persisted without rearming supervision or
starting a phantom turn. Late client start acknowledgements are generation-
checked so they cannot overwrite a newer autonomous or client turn.

Regression coverage includes client-to-Goal lifecycle, duplicate/stale
completion, autonomous error/cancel, and distinct store turn anchors. Native
headless probes exercise completed Goal followed by user consent, steering into
a live Goal, interruption of a live Goal, and rejection with stale Thinking.

Verified: 20 codex_native tests, 1003 solution_agent tests (one ignored),
and 174 acp_thread tests (two ignored) passed. All four native headless
scenarios passed, including exact turn/steer counts to detect duplicate input.
The rendered cancellation result was screenshot-checked as Idle.
