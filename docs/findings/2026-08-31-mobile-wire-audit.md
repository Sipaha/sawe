# The mobile client is still in sync with the desktop wire — 2026-08-31

The previous handoff listed "bring the mobile client up to date" as outstanding
work of unknown size: its sync agent had died before doing anything, so nothing
was known. A field-by-field audit found **no breakages**. This note records the
result so nobody re-derives it, and the four latent items it turned up.

## Sync point

`spk-editor-mobile` is clean at `c58d88a` on `main` ("wire schema to v6"). That
commit lands ~19 minutes after the desktop's `1679c533d0` *"editor_mcp: Bump the
wire schema to v6 for numeric identity ids"*. That is the sync point.

`SUPPORTED_WIRE_SCHEMA_VERSION = 6` (`core/.../RemoteDtos.kt:72`), gated at
`app/.../vm/ConnectionManager.kt:390-417`, against the desktop's
`wire_schema_version: 6` (`crates/editor_mcp/src/tools/capabilities.rs:115`).
The gate hard-fails both too-new and too-old, so a drift here is loud, not silent.

## Why 157 desktop commits produced no delta

Since the sync point, 157 commits touch `solution_agent` / `editor_mcp` /
`remote_control` / `workspace/src/mcp`. The diff restricted to the wire-shape
files is tiny:

- `crates/solution_agent/src/mcp/dto.rs`: **no field added, removed or renamed.**
  Only `SessionSummary::member_id` went from populated to hard-coded `None`
  (`dto.rs:277`) — and it is `skip_serializing_if`, so it is now simply never on
  the wire.
- `crates/solution_agent/src/mcp/read.rs`: one new **optional** param
  (`GetSessionEntryParams::stream_id`) and a new `session_unreadable:` error prefix.
- `crates/solution_agent/src/event_sources.rs`: `agent_session_message_appended`
  gained `stream_id`, re-based `entry_index` onto the stream-local space, and
  gained a minimal `{session_id}`-only fallback payload.
- `crates/remote_control/src/allow_list.rs`: rustfmt only.
- `crates/editor_mcp/src/tools/capabilities.rs`: unchanged.

## The three suspects, all closed

**`entry_index` became stream-local.** Real change, inert client-side: the only
consumer is `lastSeen.recordIfNewer(...)` (`SessionDetailStore.kt:809`), and
`LastSeenIndex` is **write-only in shipped code** — `getCached` and `readFromDisk`
have no callers anywhere in `app/src/main`. No unread badge, no scroll anchor, no
RPC parameter reads it.

**Coalesced indexing.** The two read RPCs the client actually calls
(`get_session`, `get_session_changes`) both use the stream-local coalesced space,
and the client consumes it as such: `loadOlder` feeds `entries.first().index` back
as `before_index` (`SessionDetailStore.kt:1297-1304`). `push_coalesced` advances
the merged head's `mod_seq` (`crates/solution_agent/src/stream.rs:136`), so a
coalesced update cannot slip past the client's `mod_seq > since_seq` cursor.

**New error codes.** `session_unreadable` travels as an opaque string; the client
parses error codes in exactly two places (`no_active_workspace_for_solution`,
`unknown_upload_id`) and both still exist on the desktop.
`transcript_unavailable` **is not a wire code at all** — it is an internal
`SolutionSession` flag (`crates/solution_agent/src/model.rs:469`) that makes the
read RPCs raise `session_unreadable:`.

## What makes the "no breakages" claim structural rather than a spot check

- All 40 `remote.*` methods the client calls translate through
  `crates/remote_control/src/allow_list.rs:19-77` and are in `GLOBAL_TOOLS` — and
  the desktop has a test (`allow_list.rs:247-255`) that **fails the build** if a
  forwarded tool ever falls off the global socket. The proxy dials the global
  socket (`proxy.rs:70`), so the two-tier split cannot silently strand the client.
- Every param key the client sends is a declared field on the corresponding
  `deny_unknown_fields` params struct, checked tool by tool.
- Every result/notification field the client declares **non-nullable with no
  default** is unconditionally serialized by the desktop (no `skip_serializing_if`).
- The one payload that can now omit a field the client requires — the
  `agent_session_message_appended` fallback — decodes inside
  `runCatching{}.getOrNull()` with a branch that still triggers a delta poll
  (`SessionListStore.kt:511-519` → `SessionDetailStore.kt:814`).
- All three tagged enums have tolerant serializers with an `Unknown` arm, so a new
  variant degrades to a rendered plaque rather than a failed list decode.
- The DEFLATE preset dictionary matches byte-for-byte, Adler-32 `639723996`
  (`crates/remote_control/src/wire_dict.rs:19-46` vs `WireDictionary.kt:36-70`).

## Method note, worth keeping

The audit was ordered to **read the client, not the plan docs**, because of a
prior incident: a review once rated a finding Critical on the strength of three
committed plan docs claiming the client feeds an event index into
`get_session_entry` — and that method has **zero call sites**. That held again
here: `getSessionEntry` is still dead code. A plan doc's checked-off box is not
evidence about current client behaviour.

Static analysis only — no `./gradlew test`, no live phone↔editor session.
