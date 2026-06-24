# R-6f: WebSocket permessage-deflate compression

**Status:** **cancelled (upstream gap)** — see
[`../findings/2026-05-17-remote-control-r6f-upstream-gap.md`](../findings/2026-05-17-remote-control-r6f-upstream-gap.md).
The pinned `tokio-tungstenite 0.28` and its dependency `tungstenite 0.28`
(and the latest 0.29 release) do **not** implement `permessage-deflate` —
no Cargo feature, no `WebSocketConfig` knob, no extensions API. The
plan-doc's "option 3: punt" applies. Listener stays uncompressed; R-2 /
R-4 wire protocol is unaffected. Re-open when upstream lands the
extension (long-open issue on `snapview/tungstenite-rs`) or when we
migrate the WS stack.

**Repos:** sawe (server) → `sawe-mobile` (client verify).
**Depends on:** R-6e shipped (so the wire payloads that benefit are already minimised, and compression layers on top).
**Goal:** Enable RFC 7692 `permessage-deflate` on the WebSocket so the JSON-RPC text frames (markdown, tool-call previews, list responses) compress on the wire. Image content blocks are pre-compressed and won't benefit much; text payloads typically shrink 5-10×.

## Why this phase exists (and why it's lower priority than R-6e)

R-6e already did the big wins: diff streaming, pagination, no full session re-fetch on reconnect. With those, a typical chat interaction is a few KB per turn. Compression takes that to ~hundreds of bytes — material on a slow LTE link but not load-bearing.

Doing it now because the spec asked for it explicitly and the wire change is small.

## Scope

### A. Server — enable `deflate` feature on tokio-tungstenite

`Cargo.toml` workspace dep:

```toml
tokio-tungstenite = { version = "0.28", default-features = false, features = ["rustls-tls-webpki-roots", "handshake", "connect", "deflate"] }
```

The `deflate` feature enables the `permessage-deflate` extension. If it's not available on `0.28`, fall back to whatever the version actually exposes (the upstream feature name has been stable for a couple of releases).

### B. Server — accept compressed handshake

`crates/remote_control/src/listener.rs::handle_conn` currently calls `tokio_tungstenite::accept_async(tls_stream)`. Switch to `accept_async_with_config` and pass a `WebSocketConfig` with deflate enabled:

```rust
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

let config = WebSocketConfig {
    // Defaults for everything else; enable compression.
    // The exact field name varies by tokio-tungstenite version —
    // check the docs for `WebSocketConfig`. As of 0.20+ it's
    // typically a `compression: Option<...>` or a builder on
    // the extension. If the field is private / not exposed,
    // accept_async with the `deflate` cargo feature alone may
    // auto-enable it. Verify with a quick handshake test.
    ..Default::default()
};
let mut ws = tokio_tungstenite::accept_async_with_config(tls_stream, Some(config)).await?;
```

### C. Server — handshake compatibility

A client that doesn't advertise `Sec-WebSocket-Extensions: permessage-deflate` must still complete the handshake (uncompressed). This is the protocol-level default behaviour — confirm with a test.

### D. Server — tests

`crates/remote_control/tests/listener_e2e.rs` (or `proxy_e2e.rs`) — extend or add:

1. `permessage_deflate_negotiated_when_client_offers` — connect a Tungstenite client with the `deflate` feature → handshake response includes `Sec-WebSocket-Extensions: permessage-deflate`.
2. `permessage_deflate_skipped_when_client_does_not_offer` — connect a client without the deflate extension → handshake succeeds without extension in the response → server still functions normally.

Both tests pin the cert via the existing R-2 fingerprint mechanism.

### E. Client — verify OkHttp auto-negotiates

OkHttp 4.x WebSocket client does NOT advertise `permessage-deflate` by default (as of OkHttp 4.12 — verify against the docs). Two options:

1. Leave as-is. The client doesn't negotiate compression; the server falls back to uncompressed. No change.
2. Add a request header `Sec-WebSocket-Extensions: permessage-deflate; client_no_context_takeover; server_no_context_takeover` to the WebSocket upgrade request. OkHttp doesn't process the extension itself — but the server will see the offer and respond accordingly. **The wire will still NOT be compressed unless OkHttp implements the extension** (which it doesn't out of the box). So option 2 adds a header that does nothing.

The honest read: without an OkHttp extension or replacing OkHttp's WS implementation with something that supports deflate (e.g. nv-websocket-client, scarlet, or a hand-rolled tungstenite-rs-equivalent for JVM), the Android client won't get compression even if the server supports it.

**Pragmatic outcome for R-6f**: enable server-side support so any future / non-Android client gets compression for free (and so a tungstenite-based load test confirms the win on synthetic traffic), but document the OkHttp limitation in the report. The Android client itself doesn't benefit until we either:
- Switch to a different WS library on Android (heavy lift — R-6f follow-up),
- OR (preferred) accept that on a flaky-but-not-bandwidth-limited link, R-6e's diff streaming already covers the main use case.

### F. Out of scope

- Replacing OkHttp's WS client with a compression-capable alternative.
- Tuning deflate compression parameters (default window/level is fine).
- Brotli or other algorithms.

## Acceptance (server)

```bash
cd /home/spk/.spk/sawe/solutions/spk-solutions/sawe
set -o pipefail
cargo build --bin sawe 2>&1 | tee /tmp/r6f_build.txt
grep -E "^error|could not compile" /tmp/r6f_build.txt
cargo clippy -p remote_control --all-targets -- -D warnings 2>&1 | tee /tmp/r6f_clippy.txt
cargo test -p remote_control --no-fail-fast 2>&1 | tee /tmp/r6f_test.txt
grep "test result:" /tmp/r6f_test.txt
```

- [ ] `cargo build` passes.
- [ ] `cargo clippy -p remote_control` clean.
- [ ] `remote_control` tests grow by ~2 (compression handshake cases).
- [ ] `proxy_e2e::end_to_end_proxy_round_trip` (R-4 gate) still passes — backwards-compat handshake works.

## When done

Server sub-agent reports: commit SHA, tokio-tungstenite feature actually used (whether `deflate` was the correct flag), whether `WebSocketConfig` had a compression field or if it was implicit via the cargo feature, observed bytes-on-the-wire on a synthetic compressed-vs-uncompressed handshake.

Then a tiny client-side verification (no commit needed): confirm OkHttp does NOT advertise `permessage-deflate` on the upgrade request → so the existing Android client gets uncompressed frames as before (backwards-compat with the server change).
