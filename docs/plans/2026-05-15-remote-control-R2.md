# R-2: Remote Control server — listener + TLS + auth handshake

**Status:** complete (2026-05-15). Sub-agent landed `5519f825b3`,
`a38f34ea54`, `977bd292bb`. Supervisor follow-up `731a97cacb` reconciled
the FS-watcher path with the listener bootstrap (toggling
`remote-control.json:enabled` externally now starts/stops the listener,
matching the modal-button path). 31 unit/integration tests passing,
build + clippy clean. MCP smoke-test confirmed end-to-end: edit JSON
`enabled:true` → `remote-control.cert.der` + `remote-control.key.der`
generated, `ss -tln` shows port LISTEN, log emits "listener bound on
0.0.0.0:17777". Edit `enabled:false` → port closes within ~3 s.
**Estimated:** 1 sub-agent session, ~3–5 h, worktree-isolated
**Goal:** Wire a real network listener behind the existing
`RemoteControlStore::enabled` toggle. On ON, generate (or load) a
self-signed TLS cert, bind a TCP listener on `(server_address,
server_port)`, accept WebSocket-upgrade connections over TLS, run
the HMAC-SHA256 challenge handshake from ADR-0003, and serve a
single minimal JSON-RPC method (`remote.editor.capabilities`)
proving the wire works end-to-end. The full `remote.*` tool
catalogue is R-4's scope.

## Context

R-1 + R-1.5 shipped the settings model, status-bar widget, modal,
and QR popover. ADR-0003 picked the transport: WebSocket over TLS
1.3, self-signed cert with fingerprint pinning, per-client
HMAC-SHA256 challenge auth. This phase implements the server side
of that picture; clients (Android) come in R-5.

Existing pieces this builds on:

- `crates/remote_control/src/store.rs` — `RemoteControlStore`
  with `set_enabled`, `add_client` (32-byte secret via `OsRng`).
- `crates/remote_control/src/model.rs` —
  `RemoteControlSettings`, `AuthorizedClient { name,
  secret_base64, created_at }`.
- `paths::remote_control_settings_file()` →
  `~/.config/sawe/remote-control.json`. Cert files land
  alongside as `remote-control.cert.der` / `remote-control.key.der`.
- `editor_mcp` JSON-RPC newline-delimited framing, tool registry,
  notification fan-out (the surface this remote layer eventually
  exposes a subset of).

## Scope

### A. New module `crates/remote_control/src/cert.rs`

- `pub struct ServerCert { pub cert_der: Vec<u8>, pub key_der: Vec<u8>, pub fingerprint_sha256: [u8; 32] }`.
- `pub fn load_or_generate(fs: &dyn fs::Fs) -> impl Future<Output = Result<ServerCert>>`:
  1. Try reading `paths::remote_control_cert_file()` +
     `remote_control_key_file()` (both new helpers in
     `crates/paths/src/paths.rs`, alongside the existing
     `remote_control_settings_file`).
  2. If both exist + parse → return.
  3. Otherwise generate via `rcgen::generate_simple_self_signed`
     with SANs `[localhost, 127.0.0.1, ::1]` plus the configured
     `server_address` (if Some). 10-year validity. Persist atomically
     via `fs.atomic_write` for each file.
- `fingerprint_sha256` is the SHA-256 of `cert_der`. Hex-encoded
  uppercase is what R-3 will eventually embed in the QR as
  `server_fp=...`.
- Unit tests: round-trip generate → load returns identical bytes;
  generated cert decodes via `rustls_pemfile`-equivalent path
  (parses via `rustls::pki_types::CertificateDer::from`).

### B. New module `crates/remote_control/src/auth.rs`

- `pub fn make_challenge() -> [u8; 16]` — 16 OS-random bytes.
- `pub fn expected_response(secret_base64: &str, challenge: &[u8; 16]) -> Result<[u8; 32]>`:
  decode base64 secret, compute `HMAC-SHA256(secret, b"spk-remote-v1\0" || challenge)`.
- `pub fn identify_client<'a>(challenge: &[u8; 16], response: &[u8; 32], clients: &'a [AuthorizedClient]) -> Option<&'a AuthorizedClient>`:
  loop with **constant-time** compare via `subtle::ConstantTimeEq`;
  the first match wins. Return `None` only after iterating every
  client (don't short-circuit on length mismatch; use
  `ct_compare_fixed_len` on the 32-byte HMAC output).
- Unit tests:
  - Round-trip: `expected_response` on a fixed secret + challenge
    matches a hard-coded golden vector.
  - `identify_client` returns `Some(&client)` on a matching
    response; `None` when the response is one byte off.
  - Constant-time path: a property test that compares two
    `identify_client` calls — one with a near-match (last byte
    differs) and one with a far-match (first byte differs) —
    asserts both return `None`. (Not a timing-side-channel test,
    just verification that the comparator doesn't early-return on
    length mismatch.)

### C. New module `crates/remote_control/src/listener.rs`

- `pub struct ListenerHandle { shutdown_tx: oneshot::Sender<()>, _task: JoinHandle<()> }`.
  Dropping the handle aborts the accept loop and tears down every
  per-connection task.
- `pub async fn start_listener(cfg: ListenerConfig) -> Result<ListenerHandle>`:
  - `ListenerConfig { bind_addr: SocketAddr, cert: ServerCert, clients: Vec<AuthorizedClient>, dispatcher: Arc<dyn RemoteDispatcher> }`.
  - `TcpListener::bind(bind_addr).await?` → log resolved
    `local_addr` (the user picked port 7777 by default).
  - Build a `tokio_rustls::TlsAcceptor` with `ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])`
    + the self-signed cert/key.
  - Loop `accept().await` → spawn `handle_conn(stream, peer, acceptor.clone(), clients.clone(), dispatcher.clone(), shutdown_rx.clone())`.
  - `tokio::select!` against `shutdown_rx` to break the accept loop;
    the spawned per-connection tasks are owned by their JoinHandle
    inside `handle_conn` and abort when the channel closes via
    a per-conn `select!`.
- `async fn handle_conn(...)`:
  1. `tls_acceptor.accept(stream).await?`.
  2. `tokio_tungstenite::accept_async(tls_stream).await?`.
  3. Send a JSON message `{ "type": "challenge", "challenge": "<hex>", "v": 1 }` as the first WS text frame.
  4. Read a WS text frame within 10 s
     (`tokio::time::timeout(Duration::from_secs(10), ws.next())`):
     expect `{ "type": "response", "response": "<hex-64-chars>" }`.
  5. Decode hex, run `auth::identify_client`. On `None` → send WS
     close frame with code 1008 (Policy Violation) + reason
     `"unauthorized"` → return.
  6. On `Some(client)` → send `{ "type": "welcome", "client": "<name>" }` and enter the request loop.
  7. Request loop: each WS text frame is a JSON-RPC 2.0 request.
     Hand off to `dispatcher.dispatch(client_name, request).await`;
     write the response back as a single WS text frame. Errors
     mapped to JSON-RPC error responses (`-32700` parse error,
     `-32601` method not found, `-32603` internal error).
  8. On WS close / EOF / 60 s read idle → drop the per-conn task.

### D. New module `crates/remote_control/src/dispatch.rs`

- `pub trait RemoteDispatcher: Send + Sync { fn dispatch(&self, client_name: &str, req: JsonRpcRequest) -> BoxFuture<'static, JsonRpcResponse>; }`.
- Implementation `MinimalDispatcher` (R-2 stub):
  - Allow-list: `["remote.editor.capabilities", "remote.editor.ping"]`.
  - `remote.editor.capabilities` returns `{ "protocol_version": 1, "server_software": "sawe", "tool_namespaces": ["remote.editor"], "capabilities": ["json-rpc-2.0", "hmac-sha256-challenge"] }`.
  - `remote.editor.ping` returns `{ "pong": true, "now": "<iso8601>" }`.
  - Anything else → `-32601` method-not-found.
- R-4 will replace `MinimalDispatcher` with a real
  `EditorMcpProxyDispatcher` that delegates to `editor_mcp::call_tool`
  with the gating layer.
- Unit tests: each allow-list method round-trips a request →
  response; unknown method gets `-32601`; non-JSON payload gets
  `-32700`.

### E. Wire-up in `crates/remote_control/src/store.rs`

- Add a `listener: Option<ListenerHandle>` field on
  `RemoteControlStore`.
- `set_enabled(true)`:
  - If listener already present → no-op.
  - Else: spawn a `cx.background_spawn` task that:
    1. Validates `server_address` is `Some` and parseable
       (else `set_enabled(false)` + emit a `Changed` event with
       an error log, the modal's existing "enable disabled when
       address missing" guard makes this defensive).
    2. Calls `cert::load_or_generate(fs).await?`.
    3. Builds `ListenerConfig` from current settings.
    4. Calls `listener::start_listener(cfg).await?`.
    5. Hops back to the foreground via `cx.update` and stores the
       returned `ListenerHandle` on the store.
  - On any error in steps 2–4: log via
    `log::warn!(target: "remote_control", "listener start failed: {err:#}")`,
    flip `settings.enabled` back to `false`, emit `Changed`.
- `set_enabled(false)`:
  - Drop the `ListenerHandle` (taking the `Option` out of the
    field). Drop triggers shutdown_tx send → accept loop exits.
- Mutating `clients[]` while the listener is alive: rebuild the
  listener's client list. Simplest path → send the updated list
  through a `tokio::sync::watch` channel that the per-conn auth
  task reads `Arc::clone` of on each connect. Listener task holds
  the receiver; store holds the sender.
  - On `add_client` / `remove_client` / `update_settings` → if
    `listener` is `Some`, send the new client list through the watch.
- Tests:
  - `set_enabled_starts_and_stops_listener` — using a
    `FakeFs`-backed store, set_enabled(true) → assert
    `listener.is_some()`; set_enabled(false) → assert
    `listener.is_none()` after `run_until_parked`.

### F. Cargo.toml additions

Workspace-level (`Cargo.toml`) — declare or confirm versions:

- `rcgen = { version = "0.13", default-features = false, features = ["aws_lc_rs", "pem"] }` (already may be transitively present; pin explicitly).
- `tokio-rustls = "0.26"` (compatible with `rustls = 0.23.26` already in workspace).
- `tokio-tungstenite = { version = "0.24", default-features = false, features = ["rustls-tls-webpki-roots"] }` — confirm if already in workspace via transitive deps; pin explicit if not.
- `hmac = "0.12"`, `sha2 = "0.10"`, `subtle = "2.6"`, `hex = "0.4"` — small crypto crates.

`crates/remote_control/Cargo.toml`:

```toml
[dependencies]
# ...existing...
async-channel.workspace = true
hex.workspace = true
hmac.workspace = true
rcgen.workspace = true
rustls.workspace = true
sha2.workspace = true
subtle.workspace = true
tokio = { workspace = true, features = ["net", "time", "sync", "macros", "rt-multi-thread"] }
tokio-rustls.workspace = true
tokio-tungstenite.workspace = true
```

Pin the exact features list against what the crate actually
uses — let `cargo build` tell the agent. The agent should
NOT pre-emptively touch other crates' dependency graphs.

### G. Integration test `crates/remote_control/tests/listener_e2e.rs`

End-to-end via an in-process client:

1. `RemoteControlStore::new_with_fs(FakeFs)` in a `gpui::TestApp`.
2. `add_client("Test")` → grab `secret_base64`.
3. `set_address(Some("127.0.0.1"))` + `set_port(0)` (auto-picked
   port — let the OS choose; expose the resolved port back through
   a `bound_addr() -> Option<SocketAddr>` accessor on the listener
   handle for test purposes).
4. `set_enabled(true)` → wait until `bound_addr().is_some()`.
5. Build a `tokio-tungstenite` client (test deps): connect via
   `tokio_rustls::TlsConnector` with a custom cert verifier that
   pins the server cert's SHA-256 (read from the store).
6. Drive the handshake:
   - Read `{"type":"challenge"…}`.
   - Compute the HMAC.
   - Send `{"type":"response","response":"…"}`.
   - Expect `{"type":"welcome","client":"Test"}`.
7. Send a `remote.editor.ping` request; assert pong response.
8. Send a `remote.unknown.method` request; assert `-32601`.
9. Close the WS. `set_enabled(false)`. Reconnect attempt fails
   (ECONNREFUSED).

This test is the load-bearing acceptance gate — if it passes, R-2
is done.

## Out of scope

- The full `remote.*` tool catalogue dispatching into
  `editor_mcp::call_tool` (that's R-4).
- The QR `server_fp` payload extension on the popover side
  (that's R-3 / a small follow-up to R-1.5; can be a 5-min add-on
  if time permits in this sub-agent's session, but not required).
- UPnP / NAT-traversal.
- Rate limiting / per-client connection caps.
- iOS or web clients.
- Push events / subscriptions (subscription wiring lives with R-4
  alongside the real tool dispatcher).
- macOS / Windows cert-storage permission UX (the cert files are
  user-readable on Linux; we may want `chmod 600` on the key file
  but that's a follow-up).

## Architectural decisions (in this phase)

1. **Listener task tree.** A single accept loop (one task) spawns
   one task per connection. Both are owned via a `tokio::JoinHandle`
   stored inside `ListenerHandle`. On `Drop`, send `()` through the
   shutdown oneshot; per-conn tasks listen for the broadcast and
   `select!` on it alongside the WS read loop. Don't abort handles
   directly — graceful close lets us send the proper WS close
   frame.
2. **Synchronization across clients[] mutations.** Pass the
   client list to per-conn auth tasks via a
   `tokio::sync::watch::Receiver<Vec<AuthorizedClient>>`. Watch is
   designed for "always read the latest snapshot"; cheaper than
   broadcasting individual mutations.
3. **Cert SAN list = [`localhost`, `127.0.0.1`, `::1`,
   `<server_address>`].** Including the user-typed address lets
   the future Android client's hostname-match also pass (defense
   in depth alongside fingerprint pin). If `server_address` is a
   bare IP, rcgen emits an IP SAN; if it's a hostname, a DNS SAN.
4. **TLS 1.3 only.** Configure rustls with
   `ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])`.
   No 1.2 fallback — we're talking to brand-new clients we
   control. ChaCha20-Poly1305 + AES-GCM ciphers default; don't
   restrict further.
5. **No accept timeout, but a per-connection idle timeout.**
   60 s read-idle → drop the conn. Prevents an attacker from
   pinning a slot open after auth succeeds.

## Risks

- **`tokio-tungstenite` version skew** with the rustls 0.23 already
  in the workspace. If the version of `tokio-tungstenite` pulled
  transitively binds an older rustls, the agent will see a build
  error mentioning two versions of rustls. Fix: pin the workspace
  to `tokio-tungstenite = 0.24` (uses rustls 0.23). If 0.24 isn't
  available yet, fall back to bundling the WebSocket layer inside
  the crate (~200 LOC, since we only need accept + text frame
  read/write + close).
- **`rcgen` feature flag confusion.** It has both `pem` and `der`
  output APIs; we use DER. Make sure `default-features = false` +
  the right backend (`aws_lc_rs` or `ring`) is enabled. Don't
  accidentally pull in `pem` / `webpki-roots` unless used.
- **gpui background tokio runtime.** `cx.background_spawn` polls
  on gpui's own executor, not tokio's. `tokio::net` types need a
  tokio runtime. Solution: in `set_enabled(true)`, spawn the
  listener bootstrap on a dedicated tokio runtime via
  `tokio::task::Builder` against a process-wide
  `tokio::runtime::Runtime` lazily initialised. If the existing
  codebase already has a tokio runtime (it does for HTTP /
  `http_client`), reuse that. Don't roll a second runtime.
- **`tokio-rustls::TlsAcceptor` move semantics.** It's `Arc`-cloneable
  internally; clone it before the per-connection spawn.

## Verification

```bash
cd /home/spk/.spk/sawe/solutions/spk-solutions/sawe

# Build
cargo build --bin sawe 2>&1 | tee /tmp/r2_build.txt
grep -E "^error|could not compile" /tmp/r2_build.txt   # must be empty

# Clippy on the touched crate
cargo clippy -p remote_control --all-targets -- -D warnings 2>&1 | tee /tmp/r2_clippy.txt

# Unit + integration tests
cargo test -p remote_control --no-fail-fast 2>&1 | tee /tmp/r2_test.txt
grep "test result:" /tmp/r2_test.txt | awk '{ tot+=$4; failed+=$6 } END { print "TOTAL:", tot, "failed:", failed }'

# Integration test in particular: assert it ran (not just compiled)
grep "listener_e2e" /tmp/r2_test.txt
```

Acceptance:

- [x] R-2 plan-doc committed and dispatched (sub-agent reads the
      inlined plan via `## PLAN DOC` in the dispatch prompt; the
      committed copy is for `git log` / future sessions).
- [x] `cargo build --bin sawe` passes from a clean target.
- [x] `cargo clippy -p remote_control --all-targets -- -D warnings`
      passes.
- [x] `cargo test -p remote_control` passes including the new
      `listener_e2e` integration test (29 unit + 2 integration =
      31 tests).
- [ ] Toggling Remote Control ON in the modal (with a non-empty
      address) generates a cert at
      `~/.config/sawe/remote-control.cert.der` (the
      supervisor's post-merge MCP smoke-test will verify visually).
- [x] Toggling OFF cleanly tears down the listener (covered by
      `set_enabled_starts_and_stops_listener` unit test — drops the
      `ListenerHandle`, which closes the accept loop and the
      `Drop` impl aborts the task; reconnect attempt fails with
      ECONNREFUSED in the e2e test).
- [x] FORK.md "Touched upstream files" gains a row for
      `crates/paths/src/paths.rs` (extended description), plus a
      new row for `crates/gpui_tokio/src/gpui_tokio.rs`
      (`try_handle` accessor added).

## When done

Sub-agent reports back with:

- Cargo build / clippy / test counts.
- The integration test's PASS line.
- The commit SHA.
- A 1-paragraph summary of how the watch-channel-based client-list
  reload turned out (was a tokio::sync::watch sufficient, or did
  the listener need a different shape?).
- Any new follow-ups discovered (e.g. R-3 QR `server_fp` field is
  obvious — flag it but don't implement here).

Supervisor then:

1. MCP smoke-test: launch `script/run-mcp --debug --headless`,
   toggle Remote Control modal ON, verify cert file appears and
   `editor.subscribe { kinds: ["remote_control_listener_started"] }` (if
   sub-agent added the event) fires; close.
2. Mark this plan-doc `complete`, append final SHA, update
   `docs/INDEX.md` plans table.
3. Pick the next pool item (likely R-3 `server_fp` add-on first,
   then R-4 real tool dispatcher).
