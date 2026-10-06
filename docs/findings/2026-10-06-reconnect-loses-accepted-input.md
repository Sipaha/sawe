# Reconnect must carry accepted human input

Observed in session `cghplwlq` on 2026-10-06:

- Sawe queued the new ecos-model hotfix task at 17:30:33 and the Records
  clarification at 17:31:30. Both appeared as user bubbles in the editor.
- The Codex rollout contains neither message before the 17:36:32 watchdog
  reconnect. Its latest human input was the LDAP question from 17:19:22,
  already addressed at 17:21:05.
- Recovery sent only “the latest human message above”, so Codex repeated the
  LDAP answer. The hotfix request reached provider history only when the user
  resent it at 17:38:39.

A steering receipt confirms acceptance, not durable provider history or a
response. The editor's persisted timeline and the provider's resume history
can diverge when the process dies with accepted input still buffered.

`respawn_agent` now captures consecutive human messages at the Main timeline
tail before detaching the thread. Recovery carries their actual content blocks
(including images), in chronological order, with an editor-recovery marker.
Legacy entries without blocks use their markdown. Assistant/tool entries and
non-human messages end the capture; teammate entries and System notes do not
change the Main tail. The prompt asks the agent to reconcile completed work
before acting, rather than asserting that every repeated request was unhandled.

Queued input starts before a generic recovery turn. If steering is still in
flight, recovery waits for settlement and recaptures accepted input recorded
on the replacement thread; it does not interrupt a new turn already started
by the receipt handler or user.

This updates FORK.md decision 45: pointing at a message in provider history is
insufficient. Recovery must include the input the editor actually captured.

Regression tests cover the old answered question followed by a new task and
clarification, image preservation, queued-input priority, and receipt settlement
during reconnect. These test the capture and actual recovery send; they do not
claim to control Codex's internal buffering or force its remote crash timing.

Verification: the full solution_agent suite passes. An isolated native-headless
editor with a synthetic Codex app-server accepted both follow-ups, discarded its
runtime buffer at respawn, and received both messages in the replacement
`turn/start` input. The old question was absent; process IDs confirmed a real
respawn. The recovered conversation was rendered and screenshot-checked.
