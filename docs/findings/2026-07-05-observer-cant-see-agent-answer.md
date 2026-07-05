# The supervisor never saw the agent's answer — anchored on its own nudges, blind to the reply

_2026-07-05. Triggered by a live observation: in the `citeck-forge` MDM
session the agent explained the same directive (#2, "why offline-forge didn't
catch these bugs") ~3 times, and the observer kept re-flagging it as
undelivered across four consecutive wake-ups._

## Symptom

The supervisor (observer) fixated on one user directive, treating it as the sole
thing gating `done` for four straight wake-ups, and at the 7th wake-up bounced an
answer the agent had ALREADY given as "not a proper standalone answer" — pushing
the agent to re-post substantially the same explanation. From the user's side it
read as the agent (and the observer) looping on one question.

## Root cause — two compounding defects in what the observer reads

The judge reads the supervised conversation via `solution_agent.get_session`
with `user_anchored_lead: 3` (`supervisor_judge_instructions.md`). The filter is
`apply_user_anchored_filter` in `mcp.rs`.

1. **The observer never saw the agent's ANSWER.** The old filter kept, for each
   user message, only the message itself and the `lead` entries *before* it (the
   context that prompted it) — plus the final resting turn. Entries *after* a
   user message (the agent's reply) were kept only if they happened to fall in
   the lead window of the *next* user message, or were the resting turn. So an
   answer the agent gave and then kept working past became invisible on the next
   wake-up → the observer re-nudged "you still haven't answered #2" → the agent
   answered again → repeat.

2. **The observer anchored on its OWN past nudges.** A supervisor nudge is
   delivered into the thread AS a user-role message (so the agent acts on it),
   marked only by the `spk_observer_nudge` `_meta` marker
   (`store::deliver_nudge_now`). That marker was **never surfaced in the
   `EntrySummary` DTO**, and the filter anchored on `role == User` — so the
   observer's own steering ("deliver directive #2") was re-read as a fresh user
   goal, reinforcing the fixation. The same DTO gap silently broke the **mobile**
   Observer-plaque render (it keys on `role==System && systemLevel==Observer`,
   but a nudge arrives `role==User` + dropped marker → plain user bubble). The
   desktop render was unaffected — it reads `is_observer_nudge_blocks(chunks)`
   straight off the retained chunks (`conversation_render::render_user_message`),
   and the marker survives cold persistence.

## Fix

- **`EntrySummary.observer_nudge: bool`** (`mcp.rs`), detected via
  `acp_thread::is_observer_nudge_blocks` off the user entry's chunks. One field,
  three consumers: judge visibility, judge anchoring, mobile plaque.
- **`apply_user_anchored_filter`** now (a) anchors on `role==User &&
  !observer_nudge` only — never the observer's own nudges; (b) adds a **trail**:
  after each real-user anchor it keeps up to `USER_ANCHORED_TRAIL_ASSISTANT` (5)
  assistant text turns — the agent's answer — skipping tool calls, stopping at
  the next user-role entry so adjacent messages never overlap.
- **Judge instructions** updated: the slice now includes the agent's answer;
  `observer_nudge:true` / `system_level:observer` entries are the judge's own
  past voice — never anchor on them, judge "delivered?" against the agent's
  trail, not against a restated ask.
- **Mobile** (`spk-editor-mobile`): `EntrySummary.observerNudge` decoded;
  `role==User && observerNudge` renders the Observer plaque
  (`CenteredAnnotatedBubble`, eye icon) instead of a user bubble.
- **Manual `/clear` and `/compact` wipe the observer's memory** — new
  `supervisor::wipe_supervisor_memory` (diary + verdicts + user_intent) +
  in-memory reasoning-cursor reset, called from `store::reset_context` and the
  USER path of `start_compact_for_session` (gated by `CompactInitiator::User`;
  the observer's own `compact` verdict keeps its memory, since `user_intent.md`
  must survive the transcript loss there). Lets the operator reset a looping
  observer to a clean slate.

## Tests

`mcp.rs`: trail keeps the answer past tool calls + caps at 5; observer nudge is
not an anchor; trail stops at the next user (no overlap); existing lead/since_ms
tests updated for the trail. `supervisor.rs`: `wipe_supervisor_memory` removes
the three files, leaves unrelated files, idempotent. `spk-editor-mobile`:
`observer_nudge` decode (present→true, absent→false). Editor suite green, bin
builds; mobile `:core:test` + `:app:compileDebugKotlin` green.

## Note

The desktop editor plaque was already correct; the "it doesn't mark on desktop
either" impression was almost certainly old nudges predating FORK.md #29 (the
single-marked-message design) — those carry no marker and render as plain
bubbles. New nudges mark on both surfaces.
