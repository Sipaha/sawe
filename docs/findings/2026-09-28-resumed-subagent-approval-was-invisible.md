# A resumed sub-agent's approval request was invisible — "Awaiting input" with no button

**Date:** 2026-09-28 · **Status:** fixed

## Symptom

In the Mattermost session `b865aj5p` the status row said **Awaiting input** for 26 minutes. No
approval button existed anywhere, and no sub-agent tab was shown.

## Mechanism

1. The main agent woke a **finished** async sub-agent (`a46a185…`, "Threads Task 6 UI panel")
   with `SendMessage`. claude answers that with
   `{"success":true,"message":"Resuming agent …","resumedAgentId":"<id>",…}`.
2. When the sub-agent first finished, its teammate stream had been **closed for good**, either by
   the `SubagentStop` hook or by its `stop_reason`. Its record stayed in `background_agents` with a
   stale snapshot, or had already been dropped. Nothing noticed the resume. Its new entries were
   still tagged with the original spawn `toolu`, and `rebuild_streams` dropped them together with
   the closed stream.
3. The resumed run ran `rm -rf "$(cat …/appdir.txt)"`. The safety hook cannot resolve a `$(…)`
   target, so it asked the operator. The question was attached to that sub-agent's tool call,
   which lived only in the suppressed stream. The session went `AwaitingInput`, and there was no
   surface on which to answer.

## Fix

- **A pending approval is never suppressed.** `SolutionSession::rebuild_streams` keeps any closed
  or hydration-orphaned teammate stream in the mirror while it holds a tool call in
  `WaitingForConfirmation`. After the question is answered, the Done-close applies again.
- **A resumed sub-agent is tracked again.** A terminal `SendMessage` whose result carries
  `resumedAgentId` triggers `reregister_resumed_agent`:
  - it reads the spawn `toolu` from `subagents/agent-<id>.meta.json` (`toolUseId`);
  - it reopens that teammate stream;
  - it re-inserts the agent with its transcript offset at the **current end**. The earlier lines
    end in the first run's `stop_reason`, which would retire the agent again on the next tick.
- **Two gotchas while building it:**
  - `apply_subagent_lifecycle` returns early for anything that is not `Task`/`Agent`, so the
    branch has to sit above that return, as the `Bash`/`TaskStop` branches already do.
  - "Tracked and live" is not "visible": the `SubagentStop` hook closes the tab while the record
    lingers. The skip condition therefore checks that the stream is actually open.

## The question itself need not have been asked

For `rm -rf "$(cat file)"` claude 2.1.282 gives `decision_reason = "Dangerous rm operation on
statically-unresolvable target: command substitution output"`. This was captured from a live
`can_use_tool` with `--permission-prompt-tool stdio`. The reason names no path, so the
"inside the Solution → allow" policy (`claude_native::tool_authorization`) had nothing to
resolve, and asked.

It now resolves the substitution itself, conservatively (`substitutions_land_inside`):
- Only `$(cat <file>)` and `$(< <file>)` are understood, with a literal or literally-assigned
  file path.
- The file is read. Its content must be one absolute path with no whitespace, glob, `$`,
  backtick or `..`, lying **strictly below** a work directory (never the Solution root).
- Any other substitution anywhere in the command, backticks, or an unreadable file still goes
  to the operator.

The file can change between the check and the run. It is a file inside the Solution, where the
agent already has full rights, so this grants nothing wider than the operator's standing rule.

## Verification

- Unit tests:
  - `tool_authorization::tests::a_substituted_rm_target_read_from_a_file_inside_is_allowed`
  - `model::tests::a_closed_teammate_asking_for_approval_stays_visible`
  - `store::tests::teammate_reconciler::resumed_agent_files_read_the_spawn_toolu_and_start_at_the_end`
  - `store::tests::teammate_reconciler::parses_the_agent_a_send_message_resumed`
- Live probe (debug build, headless, real claude):
  - a sub-agent finished and its tab closed;
  - the main agent `SendMessage`d it;
  - the log showed `resumed agent … re-registered` and the teammate tab came back;
  - it closed again when the resumed run ended.
