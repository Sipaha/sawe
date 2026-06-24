# R-6f: `permessage-deflate` not supported by upstream `tokio-tungstenite` / `tungstenite`

**Date:** 2026-05-17
**Phase:** R-6f (WebSocket compression on remote-control listener)
**Outcome:** **cancelled — upstream library does not implement the
extension at any released version.** No code change shipped. R-6f is
parked until upstream lands compression (or we migrate the WS stack).

---

## What we tried

R-6f's plan-doc (`docs/plans/2026-05-17-remote-control-R6f-ws-compression.md`)
called for enabling `permessage-deflate` (RFC 7692) on
`crates/remote_control/src/listener.rs` by:

1. Adding a `deflate` Cargo feature to the workspace `tokio-tungstenite`
   dep.
2. Switching `accept_async` to `accept_async_with_config` and toggling a
   compression field on `WebSocketConfig`.
3. Two listener_e2e tests covering negotiate / skip + handshake compat.

The plan flagged that the exact feature / field name should be verified
against the pinned version (`tokio-tungstenite = "0.28"` →
`tungstenite = "0.28"`).

## What we found

Verified directly from the cached crate sources at
`~/.cargo/registry/src/index.crates.io-*/`:

**`tokio-tungstenite-0.28.0/Cargo.toml`** — features list:

```
__rustls-tls, connect, default, handshake, native-tls, native-tls-vendored,
rustls-tls-native-roots, rustls-tls-webpki-roots, stream, url
```

No `deflate`, no `compression`, no `permessage-deflate`. Same story on
the underlying **`tungstenite-0.28.0/Cargo.toml`**.

**`tungstenite-0.28.0/src/protocol/mod.rs` → `pub struct WebSocketConfig`**
fields:

```
read_buffer_size, write_buffer_size, max_write_buffer_size,
max_message_size, max_frame_size, accept_unmasked_frames
```

No compression-related field. No `Extensions` toggle. No builder method
for it on the `impl WebSocketConfig` block.

A `grep -rn 'deflate\|permessage\|compression'` over the entire
`tungstenite-0.28.0/src/` tree returns four hits — all in
`handshake/headers.rs` test fixtures parsing a `Sec-WebSocket-Extensions:
permessage-deflate` *header string*. The library can lex the extension
name but does not implement it.

### Latest released versions

I also verified against `cargo info tokio-tungstenite@0.29.0` and
`cargo info tungstenite@0.29.0` (the current latest, as of 2026-05-17):
the feature sets are unchanged. The 0.29 release notes don't mention
compression. The snapview/tungstenite-rs GitHub repo has had open issues
and proposed PRs for permessage-deflate since ~2019; none have merged.

This is a long-standing gap in the upstream ecosystem, not something a
version bump fixes today.

## Decision

**Punt R-6f cleanly.** The plan-doc explicitly anticipated this:

> If NO permessage-deflate support exists in `0.28`, you have three
> options: (1) bump to a version that does; (2) use tungstenite
> directly; (3) **punt R-6f: document that the upstream library doesn't
> support compression at this pin and surface it cleanly in the report
> — don't fake-implement.**

Options 1 and 2 are not actually available — 0.29 doesn't have it, and
the underlying `tungstenite-rs` crate is what we'd be falling through
to. There is no Rust-ecosystem WS server library that implements
permessage-deflate today that we could swap to without significant
rework (`fastwebsockets` has partial support but is async-runtime-coupled
in a way that doesn't fit our `tokio-rustls` stack, and the migration
cost dwarfs R-6f's benefit).

The plan-doc's payoff calculus already covered this: with R-6e shipped
(diff-streaming + pagination), a typical chat turn is a few KB. The
~5-10× compression deflate could deliver only matters on a slow LTE
link. R-6e's win is bigger and is already deployed. Compression is
"nice to have", not load-bearing.

## What we *did* ship

Nothing functional. This commit:

- Adds this finding.
- Flips the R-6f plan-doc status to `cancelled (upstream gap)` with a
  one-paragraph deferral note pointing here.
- Updates `docs/INDEX.md` plans table row.

`Cargo.toml`, `crates/remote_control/`, and `FORK.md` are untouched.
`cargo build --bin spk-editor` and `cargo test -p remote_control` were
not re-run — no code changed.

## When this unblocks

Re-open R-6f when **any** of:

1. `tokio-tungstenite` or `tungstenite` merges a permessage-deflate
   implementation (watch
   <https://github.com/snapview/tungstenite-rs/issues> for the long-open
   issue).
2. A different mature Rust WS server library with compression support
   becomes viable for our stack — i.e. supports `tokio-rustls` + custom
   `TlsAcceptor` + low overhead, and the migration cost is recoverable.
3. We observe real-world bandwidth pain on the Android client that
   diff-streaming + pagination + R8-shrunk frames doesn't already
   cover. (R-6e wire sizes are: preview 1.77 KB, full 6.32 KB,
   full+images 46.5 KB per turn on a 9-entry synthetic chat. Below the
   "compression actually helps" threshold for typical sessions.)

Until then, the WS frames stay uncompressed. The R-2 / R-4 protocol is
unaffected — clients negotiate as before.

## OkHttp side-note (for completeness)

Even if upstream tungstenite had landed compression, OkHttp 4.x (the WS
client on Android) does not advertise `Sec-WebSocket-Extensions:
permessage-deflate` on the upgrade request, so the server would
negotiate uncompressed regardless. The Android client would need to
either:

- Hand-emit the extension header (which doesn't actually enable client-side
  deflate framing — OkHttp's WS layer doesn't implement the codec), or
- Replace the OkHttp WS engine with a deflate-capable lib
  (nv-websocket-client, scarlet, java-websocket, etc.) — heavy lift.

The Android side is therefore also blocked on a lib swap, not just a
config change.

## Files referenced

- Plan-doc: `docs/plans/2026-05-17-remote-control-R6f-ws-compression.md`
- Listener: `crates/remote_control/src/listener.rs` (untouched)
- Workspace dep pin: `Cargo.toml:778`
- Upstream source verified: `~/.cargo/registry/src/index.crates.io-*/tungstenite-0.28.0/src/protocol/mod.rs`
