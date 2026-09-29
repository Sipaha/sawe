# The user's Compact on a working session now climbs the observer's ladder

Date: 2026-09-29. Status: shipped. Decision: FORK.md #219, which amends #183.

## What was wrong

"Compact context" on a session whose turn was running sent the whole compaction prompt at once. On
claude it was steered into the running turn; on other agents it was queued for the turn's end. The
agent therefore stopped mid-step to write its handoff. The observer had not worked this way since
#183: it asks twice, 15 minutes apart, and only then sends the prompt.

## What changed

- There is one ladder per session, `SolutionSession::handoff_ladder`. The observer's ladder state
  (`compact_requests`, `last_compact_request_ms`, `last_force_ms`, `compact_request_note`) moved
  there from `SupervisorState`. The escalation logic (`store/compaction_ladder.rs`: `request_handoff`,
  `climb_compaction_ladder`, `tick_compaction_ladders`, `run_ladder_compaction`) is shared. The two
  kinds of asker differ only in:
  - the ask's wording;
  - how the ask is delivered: the observer sends a nudge, and the user's ask goes as the user's own
    message;
  - where a refused force is reported: the observer's diary, or a system note in the chat.
- The desktop button and the phone's `start_compact` go through
  `compact::request_compact_for_session`:
  - an idle session is compacted at once;
  - a working session gets the first ask;
  - a second request while the ladder runs sends the prompt now. While a user ladder is armed, the
    menu entry reads "Compact context now".
- The agent's own `start_compact(initiator: "agent")` bypasses the ladder. It consumes the ladder
  and inherits its `claims_user`. So an agent that honours the user's request resets the observer as
  a user compaction would, and one that honours the observer's request does not.

## Behaviour changes to the observer, deliberate

- The user's Stop drops the ladder. Before, the next tick saw the stopped session as idle and
  compacted it, which acted on a request the user had just cut off.
- A pending permission prompt makes the ladder wait. Before, it forced the compaction and got
  refused.
- Queuing the prompt consumes the ladder, instead of leaving it armed until the transcript rotates.
- A user's request takes over an observer's ladder and starts it from the first rung, because the
  user has not asked yet. An observer verdict leaves a user's ladder alone.

## The observer stands down while a handoff is under way

At the maintainer's request, no judge fires from the first ask until the rotation, whoever started
the handoff. The check is `SolutionSession::is_handing_off`: the ladder is armed, the compaction
prompt is out, or the rotation is parked. The old `compact_pending` check in `tick_supervisor` was
widened to this. A judge that is already running is superseded when the user arms a ladder or when
any non-observer compaction starts, and its late verdict is dropped as stale. A context-threshold
review blocked this way is not consumed: `ObserverSchedule::observe` only latches when a trigger
fires.

Consequence to know about: if an observer's ladder keeps being refused (for example, no headroom
is left), the observer stays silent for the rest of that context. The refusal is recorded in its
diary.

## Verification

- `store/tests/supervisor.rs::no_judge_fires_while_the_context_is_being_handed_off` and
  `user_handoff.rs::the_users_request_supersedes_an_in_flight_judge` cover the observer standing down.
- `store/tests/user_handoff.rs` has 8 ladder tests:
  - ask, ask again, force;
  - a turn ending without a handoff;
  - a second request forcing at once;
  - the agent's own handoff inheriting the user's claim;
  - Stop;
  - an idle session;
  - the user taking over an observer's ladder;
  - an observer verdict leaving a user's ladder alone.
- The observer's ladder tests in `store/tests/supervisor.rs` pass unchanged, apart from where the
  state is read.
- The full `solution_agent` suite (974 tests) passes, and `./script/clippy -p solution_agent` is
  clean.
- Live headless probe with real claude (Opus 5.5, 25% of 1M) on 2026-09-29:
  - Compact was pressed during a 200-second foreground command, and the user's ask was queued
    ("Delivered when Bash finishes").
  - While the ask was open, the menu read "Compact context now".
  - The ask was steered in when the command finished.
  - The agent itself called `start_compact` 5 s later.
  - The context rotated in place into `c02`.
