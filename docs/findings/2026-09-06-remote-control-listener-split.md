# Remote-control listener: reader/writer split and the ordering trap — 2026-09-06

Server-side half of the mobile-client audit. The full audit and its fix status live in the mobile
repo at `spk-editor-mobile/docs/findings/2026-09-05-mobile-network-and-bugs-audit.md` (findings
N-50…N-55, N-08, N-38, plus the additive `image_count`). This note records only what a future
reader of `crates/remote_control` needs to know, because one of the changes has a trap that is
easy to reintroduce.

Nothing here bumps `wire_schema_version` (still 6). The one wire change is additive:
`EntrySummary.image_count`.

## What changed

`run_request_loop` was a single `select!` loop that dispatched each JSON-RPC call inline. One slow
RPC or one large frame write therefore blocked pong replies, notification forwarding, the idle
timer and every other request from that phone — which the phone's own watchdog then misread as a
dead socket and force-reconnected. It is now a reader/writer split: the reader never writes, a
single writer task owns the socket, notifications get their own pump, and `MAX_INFLIGHT_REQUESTS`
bounds concurrency.

Alongside it: the notification queue coalesces `agent_session_dirty` per `(kind, session_id)` and
evicts the **oldest** on overflow (its doc had always promised that; the code did the opposite and
dropped the newest, i.e. the healing poke); the idle timer is re-armed only by **inbound** frames;
an oversize frame gets a real `Close(1009)` after a bounded half-close; upload TTL counts from last
activity and a rejected chunk now emits `upload_chunk_rejected` instead of costing the client a
30 s ack timeout.

## The trap: per-connection request order is load-bearing

The first version of the split spawned a task per request. That is wrong, and it fails almost
deterministically rather than rarely: tokio's LIFO slot polls the most recently spawned task first,
and two frames that arrive in one TCP segment are spawned back-to-back, so they reach the editor
**reversed**. A model of the exact shape inverted 500/500 trials.

It matters because the mobile client's offline queue opens each send's gate when the previous frame
is *handed to the transport*, not when its response arrives — deliberately, so a flush of N queued
messages puts N sends in flight at once. Two messages typed offline in order could therefore land
in the transcript reversed. Nothing detects or heals that.

The fix keeps the upstream write on the reader, in wire order: `UnixMcpProxy::call_tool` is split
into `begin_call` (mint id → insert pending → `write_all`, microseconds on a local Unix socket) and
`PendingCall::finish` (pure oneshot wait). Only the reply-wait is spawned. Every benefit of the
split survives.

**If you touch this loop, keep that invariant and keep its test.** The test must drive the *real*
proxy path: a stub dispatcher that merely sleeps inside `dispatch` never touches the shared write
mutex and cannot see this class of bug — that is exactly why the first version looked tested.

## The other trap: write timeouts on a weak link

A per-frame write timeout looks like a liveness check and is not one. `SinkExt::send` does not
resolve until the whole frame is accepted by the kernel, and outbound frames are unbounded
(tungstenite's size limits are read-side only). A flat 30 s cut a 25 MB session response on any
link slower than ~7 Mbit/s, and the client then retried it forever — the precise weak-network
scenario the audit exists for. The budget now scales with payload size and is documented as a
wedged-socket backstop, not a detector: the independent reader plus the 60 s inbound idle timer do
the detecting.

Related: `CLOSE_GRACE_SECS` must exceed the write budget, or the reader aborts the writer before it
can deliver the close frame and the peer sees an unexplained reset instead of a reason.

## Verification

`cargo test -p remote_control` — 71 lib + 9 `listener_e2e` + 1 `proxy_e2e`, all green, and
`listener_e2e` run repeatedly to rule out flakes (one test deliberately sleeps 7 s).
`cargo test -p solution_agent --lib` — 797 passed. clippy and `fmt --check` clean.

Beyond that, the mobile client was driven against this build on a headless emulator: pair, connect,
kill the editor, observe an honest reconnecting banner, restart the editor, observe automatic
recovery. The same client also connects to a binary built *before* these changes, which is the
backward-compatibility check that matters given no schema bump.

---

## Addendum 2026-09-07 — the negotiated wire additions

A second round added the parts of the mobile audit that needed both sides to change together
(N-05, N-29 server half, N-37, and a narrow slice of N-34). Two things a future reader needs.

**`wire_schema_version` deliberately stayed 6.** The mobile client's compatibility gate is an
*equality* check in both directions, wired to a terminal screen that tears the connection down. A
bump would therefore brick every phone already in the field the moment the desktop updated. Nothing
added here is breaking, so there is nothing to version. Negotiation lives in a capability list
instead: `Capabilities` now carries `wire_features` (**always** serialised, empty vector included, so
a client can distinguish "no features" from "no negotiation"), `server_instance_id` and
`csid_dedupe_window_ms`. If you ever do need a real break, widen `MIN_SUPPORTED_WIRE_SCHEMA_VERSION`
on the client first — the split exists so the next break does not repeat the equality trap.

**Two traps recorded at their structs, because both are silent.** `SendDeliveryDto`'s two literals
are a cross-repo agreement point: adding a third variant is at best "unknown, assume accepted" and
at worst a strict decoder failing a send the server accepted — confirm every fielded build decodes
leniently first, and gate the new value behind a new feature token. And `KnownEntryDto` is
`deny_unknown_fields`, so it is unextendable: a newer client adding a field there does not get it
ignored by an older desktop, serde rejects the element and fails the whole `get_session_changes`
call.

One defect worth remembering, found only by cross-checking the two implementations against each
other rather than against the spec: the `spk_client_send_id` claim was taken before
`resolve_upload_handles` (correct per the design) but **never released when that step failed**. A
replay of such a send was then answered `duplicate`, the client read that as success, and the user's
message vanished with no diagnostic — where before dedupe existed the same path failed loudly. The
claim/release now brackets everything between the claim and the enqueue, so a fallible step added
later cannot skip it.

Verification for this round: `solution_agent --lib` 826, `remote_control` 85, `editor_mcp` 79,
clippy and `fmt --check` clean. Live: the desktop advertises all four tokens at
`wire_schema_version` 6, a freshly paired phone connects and loads its workspace, and with the
feature list forced empty the client still works with zero `invalid_params` on the wire.
