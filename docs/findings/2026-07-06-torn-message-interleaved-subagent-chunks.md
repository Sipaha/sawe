# Parent messages torn into fragments by interleaved subagent chunks

**Date:** 2026-07-06
**Status:** fixed
**Repo:** editor (`crates/acp_thread`)

## Symptom (user, live, mobile + desktop Main)

A main-agent message renders as many separate bubbles — split even **mid-word**
("…собирают точные сигнатуры (ag" | "ent-plane, CLI+daemon-wiring…"). The user
noticed it correlates with sub-agent activity, and that the **sub-agent tab
renders the same messages cleanly** — because that tab sources from the agent's
own JSONL, not the interleaved parent thread.

## Root cause

A claude async `Agent` teammate streams into the **parent thread** concurrently
with the parent agent, each chunk tagged with the teammate's `subagent_id`
(`_meta.claudeCode.parentToolUseId`). So the parent's own text deltas arrive
**interleaved** with the teammate's:

```
parent-delta-1 · subagent-chunk · parent-delta-2
```

`AcpThread::push_assistant_content_block_with_subagent_id` and
`streaming_markdown_target` coalesced onto `self.entries.last()` only. After the
interleaved subagent chunk, `last()` is the subagent entry — `subagent_id`
mismatch — so `parent-delta-2` started a **fresh** parent `AssistantMessage`.
Every interleaved chunk tore the parent message at that point. Filtering the
subagent entries out of Main (earlier fix) hid the noise but left the parent
**fragments** as separate entries.

## Fix

New `AcpThread::coalesce_target_index(subagent_id, indented)` scans back from the
end and **skips entries produced by other sources** (concurrent interleave),
returning the most recent same-source `AssistantMessage` to append to. It stops
(→ new entry) at:

- this source's **own** tool call (a genuine message boundary), and
- any structural entry (`UserMessage` / `CompletedPlan` / `ContextCompaction` /
  `SystemNote`),

and coalesces only when the candidate's `indented` flag matches. Both the
streaming-buffer path and the non-buffer append path now target that index (and
emit `EntryUpdated(target_idx)` so the store re-syncs the *correct* older entry —
`store.rs`'s EntryUpdated handler already reconverts entry `*idx` with a bumped
`mod_seq`, so the coalesced bubble propagates to mobile unchanged).

Normal single-stream agents are unaffected: with no interleaving the backward
scan returns `entries.last()`, exactly as before.

## Verification

- `acp_thread` suite: 82 passed (+2 new — `..._coalesces_across_interleaved_
  subagent_chunks`, `..._source_own_tool_call_is_a_message_boundary`; the one
  pre-existing `test_checkpoint_shows_when_file_changes_during_pending_message`
  failure is unrelated and reproduces without this change).
- `solution_agent` suite: 525 passed.
- Mobile needs no change — it renders `session.entries`, which the store now
  feeds coalesced.

## Relation to the other 2026-07-06 findings

Same underlying reality — a concurrent async `Agent` teammate streaming into the
parent thread — as `2026-07-06-subagent-entries-flood-main-after-finish.md`
(filter + background-pill registration). This one fixes the *rendering* of the
parent's own messages; that one fixes *where the teammate's* messages go.
