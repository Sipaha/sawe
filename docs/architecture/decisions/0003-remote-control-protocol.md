# ADR-0003: Remote Control transport — WebSocket over TLS, fingerprint-pinned, secret-authenticated

**Status:** accepted
**Date:** 2026-05-15
**Deciders:** Pavel Simonov (@Sipaha)
**Related:** [`docs/plans/2026-05-15-remote-control.md`](../../plans/2026-05-15-remote-control.md) (arc scoping), [`docs/plans/2026-05-15-remote-control-R1.md`](../../plans/2026-05-15-remote-control-R1.md) (R-1 settings+UI), [`docs/plans/2026-05-15-remote-control-R1-5.md`](../../plans/2026-05-15-remote-control-R1-5.md) (R-1.5 QR), [ADR-0001](0001-fork-philosophy.md)

---

## Context

R-1 + R-1.5 shipped the settings model, status-bar widget, modal UI, and QR
popover for the Remote Control feature. R-2 ("Server protocol + listener")
is the next phase. Before dispatching it the supervisor needs a load-bearing
decision on:

1. **Wire transport** — what bytes go over the socket.
2. **Encryption + server auth** — how the Android client knows it's talking
   to the right workstation and not an attacker on the LAN.
3. **Client auth** — how the workstation knows this WebSocket frame is from
   a paired client and not a probe.
4. **Application protocol** — what semantic API rides on top.
5. **NAT traversal** — how the user gets the workstation reachable from
   their phone when it's not on the same Wi-Fi.

Existing inputs that constrain the decision:

- `RemoteControlSettings::clients[]` already has `name` + `secret_base64`
  (32-byte symmetric secret, base64). R-1.5 surfaces this via QR code as
  `sawe-remote://<addr>:<port>?secret=<b64>&client=<name>`. So **the
  pre-shared symmetric secret is already provisioned end-to-end**; any
  protocol choice must consume it directly (no PKI, no separate cert
  enrollment dance).
- The local `editor_mcp` socket exposes 60 JSON-RPC tools that the agent
  drives every UI test through. R-4 wants to expose a curated subset of
  these to remote clients. Reusing the existing tool catalogue means
  Android clients hit the same surface autonomous agents use — one place
  to fix bugs, one place to add features.
- Workspace already depends on `rustls = 0.23` (declared workspace-wide)
  and pulls `tokio-tungstenite` transitively through other crates.
  Adding **no** new heavy crates is preferable.
- `sawe-mobile` will be Kotlin + Jetpack Compose + OkHttp
  (R-5 plan). OkHttp ships first-class WebSocket + TLS + cert-pinning
  support out of the box. Anything Noise-based would require porting a
  Noise implementation to Kotlin or pulling in `noise-java` (dormant
  upstream, would need vendoring).

---

## Options considered

### Option A — WebSocket over TLS, server-cert fingerprint-pinned, client HMAC challenge (THIS)

- **Wire:** `wss://<addr>:<port>` via `tokio-tungstenite` + `rustls`.
  Server boots, generates a self-signed TLS cert via `rcgen`, persists it
  alongside `remote-control.json` so cert identity is stable across
  restarts. The cert's SHA-256 fingerprint is included in the QR alongside
  the secret: `sawe-remote://<addr>:<port>?secret=<b64>&client=<name>&server_fp=<b64>`.
- **Server auth:** Android client pins by fingerprint (rejects any cert
  whose SHA-256 differs from the one in the QR). No CA, no Let's Encrypt,
  no DNS dependency.
- **Client auth:** post-TLS, server sends a 16-byte random `challenge`
  frame as the first WebSocket message. Client replies with
  `HMAC-SHA256(secret, "sawe-remote-v1\0" || challenge)`. Server verifies
  against every authorized client's secret (constant-time compare); the
  first match identifies which `AuthorizedClient` this is. Mismatch closes
  the connection with policy code 1008.
- **App protocol:** JSON-RPC 2.0 newline-delimited inside each
  WebSocket text frame. Method names use a `remote.*` namespace that
  delegates to the existing `editor_mcp::call_tool` with a remote-allowed
  gating layer (see "How to apply"). Events stream as JSON-RPC
  `remote/notification` frames just like the embedded MCP path.
- **NAT:** out of scope at the transport layer. Manual port-forward in
  R-6 docs first; UPnP via `igd-next` as a later opt-in.

#### Pros
- **Zero new heavy crates.** `tokio-tungstenite` + `rustls` + `rcgen` are
  already in the dependency graph (rcgen via rustls-pemfile transitively;
  if not, it's a 50KB additive dep). No `snow` / Noise port to Kotlin
  needed.
- **Android-trivial.** OkHttp 4.x has `CertificatePinner` for fingerprint
  pinning out of the box. WebSocket + JSON is the most Kotlin-natural
  combination available.
- **Symmetric with the local MCP socket.** Same JSON-RPC body, same
  newline-delimited framing, same tool method names (`remote.*` instead
  of bare). Code reuse on the editor side is high: the `remote.*` handler
  is a thin auth + delegate-to-`call_tool` wrapper. Bugs in tool
  implementations fix both surfaces.
- **TLS is well-vetted.** rustls is FIPS-curve audited; ChaCha20-Poly1305
  + X25519 ECDHE is the default cipher in TLS 1.3. The threat model
  (single LAN attacker, no nation-state) is many sigma below what TLS 1.3
  defends against.
- **Cert is stable across restarts.** Persisted alongside settings, so the
  QR a user generated yesterday still works tomorrow. No "reprovision your
  phone every reboot" UX trap.
- **Proxy-friendly.** Works through Cloudflare Tunnel / Tailscale Funnel /
  any HTTPS-aware tunnel a user might already have for other reasons.
  Future R-7 "relay server for road warriors" doesn't need a protocol
  change.

#### Cons
- **Self-signed cert is a UX line item.** The QR has to carry the
  fingerprint; user can't paste `wss://1.2.3.4:7777` into a generic
  WebSocket client and expect it to work. Acceptable because the QR
  flow is the only sanctioned client-bootstrap path.
- **WebSocket framing overhead.** ~2-14 bytes per frame on top of TLS
  framing. Irrelevant at LAN bandwidths; matters only over slow mobile.
- **rcgen as a hard dep.** Probably already in the tree via tonic /
  rustls-platform-verifier; if not, ~50KB additive. Worst case we
  generate the cert in-tree from raw OpenSSL bindings, but that's
  reinventing rcgen for no reason.

### Option B — Raw TCP + Noise_NNpsk0 (snow) + JSON-RPC

- Raw TCP socket on `<addr>:<port>`. After connect, run a Noise_NNpsk0
  handshake using `snow` with the per-client `secret_base64` as the PSK.
  Post-handshake, every JSON-RPC frame is encrypted in a Noise transport
  message (length-prefixed).
- **Pros:** No TLS cert to manage. No fingerprint in QR. The PSK
  authenticates both sides in one handshake (mutual auth for free). One
  fewer crypto layer to reason about (Noise is the entire stack).
- **Cons:**
  - **Kotlin-side Noise port is a real cost.** `noise-java` (Rhys
    Weatherley, 2018) is the only existing port; it's been dormant for
    7 years and only implements a subset of patterns. We'd either vendor
    it + audit, or hand-roll Noise_NNpsk0 in Kotlin (~250 LOC of
    ChaCha20-Poly1305 + BLAKE2s + X25519 + state machine, all of which
    have to be exactly right or the channel is silently broken). TLS is
    already done for us in OkHttp.
  - **No proxy/CDN compatibility.** Raw TCP doesn't ride HTTP, so
    Cloudflare Tunnel / Tailscale Funnel won't work. R-7 relay would
    need its own protocol bridge.
  - **`snow` is a workspace addition.** ~50KB. Not bad alone but
    asymmetric with the Kotlin port cost.
  - **Application-level streaming is reinvented.** WebSocket's
    text/binary frames map 1:1 to JSON-RPC frames. Over raw TCP we'd
    add a length prefix and a tiny demuxer. Trivial code, but trivial
    code we don't have to write at all in Option A.

### Option C — HTTP/2 + gRPC (tonic)

- Define the API as a `.proto` file. `tonic` server-side, `grpc-kotlin`
  client-side.
- **Pros:** Strong typing. Bidi streaming first-class. Tooling is
  industrial.
- **Cons:**
  - **Protoc in the build chain.** tonic-build pulls a protobuf compiler
    download or system dep. The build is harder to set up on a fresh
    Linux box.
  - **Two protocol surfaces to maintain.** The editor already speaks
    JSON-RPC for the local MCP socket. Adding a parallel gRPC surface
    duplicates every tool definition into a `.proto` schema. R-4's
    promise of "remote gets the same tool catalogue as agents" becomes
    "remote gets a hand-translated parallel catalogue we maintain
    forever".
  - **JSON-RPC's lossy boundary works in our favour for evolvability.**
    A tool that adds a new optional field doesn't break old clients.
    With protobuf this is also true, but the schema has to be edited
    in lockstep on both sides.

### Option D — Stay on the existing `editor_mcp` Unix socket via SSH tunnel

- Don't add a TCP listener at all. Instruct the user to `ssh -L 7777:$HOME/.spk/sawe/state/mcp.sock` from their phone (Termux + ssh on Android).
- **Pros:** Zero new code. Reuses the existing 60-tool surface as-is.
- **Cons:**
  - **Termux SSH is not a remote-control UX.** The Android product needs
    one-tap pairing from a QR. Asking the user to install Termux, set up
    a private key, configure ssh-agent, and tunnel a Unix socket is a
    different product (developer ssh-into-laptop, not phone-as-pager).
  - **No client identity model.** The local MCP socket has no auth: any
    process with FS access to `~/.spk/sawe/state/` can drive the
    editor. Punching that through ssh exposes the same trust model to
    every device on the other side of the tunnel; one phone with the
    private key = full editor control with no per-client revoke.

---

## Decision

We picked **Option A — WebSocket over TLS, server-cert fingerprint-pinned, per-message HMAC challenge for client auth**.

Load-bearing reasons:

1. **Reuses the dependencies we already have.** `rustls` + `tokio-tungstenite`
   are in the graph today. No `snow` / no Noise-port-to-Kotlin cost. The
   ADR doesn't gate R-2 on any new crate audit.
2. **OkHttp on Android does the hardest parts for free.** WebSocket,
   TLS, cert-fingerprint pinning are all first-class. R-5's client
   crypto layer is `OkHttpClient.Builder().certificatePinner(...)` —
   one statement.
3. **Reuses the existing JSON-RPC tool catalogue verbatim.** `remote.*`
   handlers are auth-gating thin wrappers around `editor_mcp::call_tool`;
   no second source of truth for tool semantics. Fixes and new tools
   land once.
4. **Self-signed + fingerprint pinning is the right trust model for
   LAN/personal use.** No CA, no DNS, no Let's Encrypt — but full TLS
   1.3 transport security and per-server identity. Threat model: one
   LAN-local attacker. Defended.
5. **Symmetric secret authenticates the client.** Doesn't replace the
   transport layer (cleaner separation); just gates the connection
   after TLS is up. Per-client secrets stay in
   `AuthorizedClient.secret_base64` — no schema change needed.

The decision is **load-bearing** for:

- R-2 — directly defines the listener stack (`tokio-tungstenite::accept_async`
  on a `TlsAcceptor`, post-handshake auth challenge, per-connection task).
- R-3 — QR payload format gains `&server_fp=<base64-sha256>` field; revoke
  flow is "remove from `clients[]`, kick any open connection with that
  client_id".
- R-4 — `remote.*` namespace dispatch maps 1:1 to `editor_mcp::call_tool`
  with a remote-allow-list filter.
- R-5 — Android client TLS is `OkHttpClient + CertificatePinner`; auth
  step is HMAC-SHA256 over the challenge.

---

## Consequences

### Positive

- **One JSON-RPC tool catalogue for both surfaces.** Local agents and
  Android clients hit the same `editor.capabilities`, `solutions.list`,
  `solution_agent.send_message`, etc. The only difference at the API
  level is the namespace prefix on the call (`remote.solutions.list` vs
  `solutions.list`) and the gating layer that rejects calls outside the
  remote-allowed set.
- **No new Kotlin crypto code.** Android client uses standard library
  + OkHttp. Saves an estimated 1-2 weeks of "is the Noise port actually
  correct" review.
- **Cert persists, secret persists** — pairing is durable. User scans
  the QR once, app remembers it across restarts on both sides.
- **Proxy / tunnel compatibility for free.** A user already running
  Tailscale Funnel or Cloudflare Tunnel just points the QR at the public
  URL. We don't need a "relay server" to ship the feature.

### Negative

- **rcgen is a new dependency** (or, equivalently, a tiny in-tree cert
  generator). Adds ~50 KB to the bundle, runs once at first
  `enabled = true` to mint the cert. Not a hot path.
- **Cert generation is a one-time operation; rotation is a manual
  re-pair.** If the user wants to rotate the cert (e.g. cert leaked
  somehow), the flow is "delete cert file, regenerate, re-issue QR to
  every paired client". Acceptable — this is a personal-server
  feature, not a multi-tenant SaaS.
- **No forward secrecy for the symmetric secret.** If the secret leaks,
  past *and future* sessions with that client are compromised. TLS 1.3
  ECDHE gives forward secrecy at the transport layer, but the per-client
  auth secret is long-lived. Mitigated by per-client (not server-wide)
  scope + revoke-by-removal.
- **Locks us into JSON-RPC for the long haul on this socket.** If the
  protocol ever needs binary streaming (file upload of a screenshot,
  say), we use a WebSocket binary frame with our own header but lose
  the JSON-RPC convenience for that one frame. Manageable.

### Reversibility

- **Switching to gRPC/Option C** is a hard pivot: the Android client's
  client-side has to be rewritten, all QR codes invalidated. Not free.
- **Switching to Noise/Option B** is somewhat easier: the editor-side
  swap is "replace the rustls TlsAcceptor + HMAC challenge with a Noise
  handler", and the Android client's WebSocket layer would become a
  raw TCP socket + Noise lib. Still meaningful work.
- **Adding a relay server** (R-7-ish) is fully compatible — the relay
  just forwards WebSocket-over-TLS frames opaquely.

Bottom line: **WebSocket+TLS+secret is the lowest-cost choice that
satisfies the threat model and ships the soonest**, with the easiest
expansion path if the requirements grow.

---

## How to apply

### Files / identifiers that encode this decision

When R-2 lands, expect these new files in `crates/remote_control/`:

- `crates/remote_control/src/listener.rs` — `tokio::net::TcpListener` +
  `tokio_rustls::TlsAcceptor` + `tokio_tungstenite::accept_async` per
  connection. Owns the accept loop.
- `crates/remote_control/src/cert.rs` — `rcgen::CertificateParams::new`
  for a self-signed cert (DNS-name = configured `server_address`,
  validity = 10 years). Persist `(cert.der, key.der)` next to
  `remote-control.json` (`~/.config/sawe/remote-control.cert.der` +
  `…/remote-control.key.der`). Generate on first `enabled = true` if
  absent; reuse otherwise.
- `crates/remote_control/src/auth.rs` — HMAC-SHA256 challenge logic.
  Server sends 16 random bytes as the first WS frame after TLS. Client
  has 10 s to reply with `hex(HMAC-SHA256(secret, b"sawe-remote-v1\0" || challenge))`.
  Server tries each `AuthorizedClient.secret_base64` in constant time;
  identifies the client on match or closes the connection (WS code 1008)
  on mismatch / timeout.
- `crates/remote_control/src/dispatch.rs` — JSON-RPC reader/writer, plus
  the `remote.*` allow-list and the wrapper that translates
  `remote.solutions.list` → `editor_mcp::call_tool("solutions.list", ...)`
  via an internal dispatch handle.

### QR payload extension (R-3 / R-1.5 follow-up)

```
sawe-remote://<addr>:<port>?secret=<b64-32>&client=<name>&server_fp=<b64-sha256-32>
```

`server_fp` is the **SHA-256 of the cert's DER bytes**, base64-encoded
(URL-safe, no padding). R-1.5's QR encoder is in
`crates/remote_control_ui/src/qr_popover.rs`; add `server_fp` to the
encoded URL there. The reader on Android pins by exact match.

### `remote.*` allow-list (R-4)

Curated subset, not the full 60-tool catalogue. Initial set:

- `editor.capabilities`
- `solutions.{list, get, open}`
- `solution_agent.{list_sessions, get_session, create_session, send_message, cancel_turn}`
- `workspace.{list_buffers}` (read-only ok; no apply_edit / save_buffer over remote yet)
- `editor.subscribe` / `editor.unsubscribe` — but restricted to the
  agent_session_* event family. No `lsp_started` push, no `diagnostic_updated`.

Block-list everything else (file CRUD, project ops, full workspace dumps).
The gating layer is a `HashSet<&'static str>` of allowed method names; a
remote call to anything outside it returns JSON-RPC error -32601
("method not found") so reconnaissance gets the same answer as an actual
typo.

### When to revisit this ADR

- **A relay-server requirement lands** (R-7-style, road warrior NAT
  punch). Doesn't invalidate — relay just forwards WebSocket frames
  opaquely.
- **The secret leaks frequently in practice** → add per-message
  HMAC + nonce-based replay protection on top of the post-handshake
  channel. TLS gives transport integrity, but auditable per-message
  signatures would be a clean defense-in-depth add.
- **An iOS client appears.** TLS + WebSocket + fingerprint pinning all
  trivially port (URLSession + NSURLSessionWebSocketTask + a
  pinning delegate). The decision survives.

### Anti-patterns (don't)

- **Don't auto-trust the cert on the client side.** Without fingerprint
  pinning, the self-signed cert is `OK to a MITM attacker on the LAN`.
  Pinning is the entire point.
- **Don't store the cert+key in `remote-control.json`.** Use sibling
  files. Keeping the private key in a JSON file the user might paste in
  a bug report is a foot-gun.
- **Don't reuse the same secret for HMAC auth AND symmetric-encryption
  application data.** The secret is for client identity. Transport
  encryption is TLS. Don't entangle them.
- **Don't expose `editor_mcp::register_tool` to remote.** Tools are
  registered at editor init, not over the wire. The `remote.*` surface
  is read-only on the tool registry.
- **Don't bind to `0.0.0.0` by default.** The user explicitly toggles
  Remote Control ON. Even then, bind to the configured
  `server_address` (typically the LAN interface), not `0.0.0.0`. A
  small thing but reduces blast radius if the toggle is left on after
  travel.

---

## Notes

- Why HMAC-challenge over "TLS client cert + per-client X.509 cert":
  client X.509 means the user has to manage a CA, generate per-client
  certs, distribute them. The whole point of the symmetric secret is
  to skip that. HMAC over a server-supplied challenge gives the same
  freshness guarantee (a recorded handshake can't be replayed) with a
  single base64 string in the QR.
- Why TLS 1.3 only: rustls defaults to TLS 1.3, and there's no reason
  to support 1.2 for a brand-new protocol with brand-new clients.
  Configured explicitly via `ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])`.
- The "constant-time compare" wording on the client-auth step matters:
  the trial loop over `AuthorizedClient.secret_base64` MUST use a
  constant-time comparator (e.g. `subtle::ConstantTimeEq`) for the HMAC
  output, otherwise a side-channel timing attacker can binary-search
  out a secret. Cheap to get right; expensive to get wrong.
- Tooling note: when R-2 actually ships, an end-to-end smoke test would
  be a Python `websockets` + `cryptography` client that completes the
  handshake and calls `remote.editor.capabilities`. ~50 LOC, runs
  against a `sawe --debug --headless` instance with a paired
  client preloaded. R-2 plan-doc should bake that in as part of CHECKS.
