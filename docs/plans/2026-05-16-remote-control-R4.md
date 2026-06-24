# R-4: `remote.*` tool surface — proxy to embedded MCP socket

**Status:** ready to dispatch
**Estimated:** 1 sub-agent session, ~2–4 h, worktree-isolated
**Goal:** Replace `MinimalDispatcher` (R-2 stub that handles only
`remote.editor.{capabilities,ping}`) with a real dispatcher that
forwards a curated allow-list of `remote.*` calls to the embedded
`editor_mcp` Unix-socket server. Includes notification fan-out so an
Android client can drive an agent session live (send a message, watch
streaming output, cancel a turn) end-to-end.

## Context

R-2 shipped the TLS+WS+HMAC listener with a `MinimalDispatcher` proving
the wire works. ADR-0003 mandates that the remote API surface reuses
the existing 60-tool `editor_mcp` catalogue — no parallel
implementation, one source of truth for tool semantics.

Two architectural options were considered:

- **A. In-process direct call** — add a public `McpServer::call_tool`
  to `context_server::listener` and invoke it from `remote_control`.
  Pro: no Unix-socket round-trip. Con: touches an *untouched* upstream
  crate (per ADR-0001 we prefer additive patches there but want to
  avoid them when an alternative is clean); also needs the tier-guard
  layer to be reapplied.
- **B. Proxy over the embedded MCP Unix socket** (THIS) — the
  `remote_control` listener opens (per-connection) a Unix-socket
  client to `editor_mcp::socket_path()`, forwards filtered JSON-RPC
  requests, fans out `editor/notification` frames back as
  `remote/notification` to the WS client.

We pick **B** because (i) it leaves `context_server` untouched —
upstream-shaped behaviour preserved per ADR-0001, (ii) every code path
the Android client exercises is the EXACT path autonomous agents
already test daily via the same Unix socket — fewer "works locally,
breaks remotely" surprises, (iii) lifecycle is naturally
per-connection (Unix-socket session aligned 1:1 with WebSocket
session). The cost is one extra ~50 µs hop per call — negligible at
LAN latencies.

## Scope

### A. New module `crates/remote_control/src/proxy.rs`

- `pub struct UnixMcpProxy { stream: tokio::net::UnixStream }`.
- `pub async fn connect() -> Result<UnixMcpProxy>` — resolves
  `editor_mcp::socket_path()`, connects, returns the wrapper. Sets
  a 5 s connect timeout so a broken socket fails fast instead of
  hanging the WS connection.
- `pub async fn call(&mut self, method: &str, params: Option<Value>, id: i64) -> Result<JsonRpcResponse>`:
  writes a newline-delimited JSON-RPC 2.0 request, reads frames until
  one matches the request `id` (intermixed `editor/notification`
  frames go to a separate channel).
- `pub fn notifications(&self) -> tokio::sync::mpsc::UnboundedReceiver<Value>`:
  a side channel that receives every `editor/notification` frame so the
  WS task can forward them. (Initial reader split: a background task
  consumes from the socket, demuxes by `id` to a per-id oneshot map for
  responses and into the mpsc for notifications.)

### B. New module `crates/remote_control/src/allow_list.rs`

- `pub fn translate(method: &str) -> Option<&'static str>`:
  - `remote.editor.capabilities` → `editor.capabilities`
  - `remote.editor.subscribe` → `editor.subscribe`
  - `remote.editor.unsubscribe` → `editor.unsubscribe`
  - `remote.editor.list_subscriptions` → `editor.list_subscriptions`
  - `remote.solutions.list` → `solutions.list`
  - `remote.solutions.get` → `solutions.get`
  - `remote.solutions.open` → `solutions.open`
  - `remote.solution_agent.list_sessions` → `solution_agent.list_sessions`
  - `remote.solution_agent.get_session` → `solution_agent.get_session`
  - `remote.solution_agent.create_session` → `solution_agent.create_session`
  - `remote.solution_agent.send_message` → `solution_agent.send_message`
  - `remote.solution_agent.cancel_turn` → `solution_agent.cancel_turn`
  - everything else → `None` (caller returns JSON-RPC -32601).
- Subscription event filter: only forward `agent_session_*` events.
  Drop `solution_changed`, `buffer_*`, `diagnostic_*`, `lsp_*`,
  `window_focused`, `operation_*` — those leak local-only state.
  Filter applies at the notifications fan-out path in `dispatch.rs`.
- Tests: every allow-listed method round-trips through `translate`;
  unknown / banned methods return None.

### C. Replace `MinimalDispatcher` with `ProxyDispatcher` in `dispatch.rs`

- `pub struct ProxyDispatcher`. Implements `RemoteDispatcher`.
- `dispatch(client_name, req) -> BoxFuture<JsonRpcResponse>`:
  1. `allow_list::translate(&req.method)` → `None` → return -32601.
  2. Lazily open a `UnixMcpProxy` for this connection (held in
     per-connection state — see Wire-up).
  3. Call `proxy.call(translated_method, req.params, req.id).await`.
  4. Map the local response back: status code, structured content,
     error fields. `id` MUST be echoed exactly so the WS client can
     match responses.
- Subscription notifications: when a `remote.editor.subscribe` succeeds
  and the kinds include allowed `agent_session_*` types, the per-WS
  connection task's notification pump starts forwarding from
  `proxy.notifications()` (filtered by `allow_list::should_forward_event`)
  to the WS client as `{"jsonrpc":"2.0","method":"remote/notification","params":{...}}`.

### D. Wire-up in `crates/remote_control/src/listener.rs`

The per-connection handle path (post-welcome, in the request loop):
- Construct one `UnixMcpProxy` lazily on the first request.
- On WS close → drop the proxy → its `UnixStream` closes → embedded
  server cleans up subscriptions per existing
  `editor_mcp::lifecycle.rs` connection-drop semantics.
- The notification pump runs as a `tokio::select!` alongside the WS
  read loop in `handle_conn`. Frame format on the wire:
  `{"jsonrpc":"2.0","method":"remote/notification","params":<original>}`.

### E. Plumb the dispatcher swap from `store.rs`

The R-2 store builds `ListenerConfig { dispatcher: Arc::new(MinimalDispatcher) }`.
Swap to `Arc::new(ProxyDispatcher::default())`. The `MinimalDispatcher`
stays in the tree as a `#[cfg(test)]`-only fallback for the unit tests
that don't want a live MCP socket.

### F. Integration test `crates/remote_control/tests/proxy_e2e.rs`

End-to-end through both surfaces — assembles the listener AND the
embedded MCP server in-process:

1. `editor_mcp::set_runtime_dir_for_test(tempdir)` so the local
   MCP socket doesn't collide with a running editor.
2. `editor_mcp::start_server_for_test(cx)`.
3. `RemoteControlStore::new_with_fs(FakeFs)` → `add_client("Test")` →
   `set_address(Some("127.0.0.1"))` → `set_port(0)` → `set_enabled(true)`.
4. Build a pinning WS client (mirror R-2's `listener_e2e.rs`).
5. Handshake.
6. Call `remote.editor.capabilities` → assert the response carries
   the embedded server's actual capabilities response (not the R-2
   stub).
7. Call `remote.solutions.list` → assert it returns an empty list
   (test app starts with no solutions).
8. Call `remote.lsp.start` → assert -32601 (not in allow-list).
9. Close. Assert proxy's UnixStream dropped (test reachability of
   `editor_mcp` is unaffected — connection drop doesn't leak).

This is the load-bearing acceptance gate.

## Out of scope

- The Android client (R-5).
- Per-client rate limiting (follow-up).
- Per-client allow-list overrides (future feature: read-only clients).
- Kicking existing WS connections on `remove_client` (deferred from R-2).
- Authorization of write tools (`remote.solution_agent.send_message`
  trusts whoever passed the HMAC challenge). Tier-guard layering is
  out of scope here — the allow-list is the gate.
- Direct in-process dispatch (Option A). If the proxy hop becomes a
  bottleneck (≥10 ms p99 round-trip on benchmarks), revisit.

## Architectural decisions (this phase)

1. **Per-WS-connection UnixMcpProxy, lazily opened.** Lightest-weight
   pairing — no shared connection pool, no race on a global resource.
   The local socket can handle hundreds of concurrent connections
   trivially.
2. **Notification fan-out via tokio::select! in handle_conn.** No new
   thread / task per WS connection beyond what R-2 already spawns.
3. **JSON-RPC `id` preservation.** Server doesn't generate fresh ids —
   the client's id is passed through to the local socket and the
   response echoes whatever the local server returned (which is the
   client's id, by JSON-RPC spec).
4. **Subscription event filter is a *block*-list at the
   notifications layer**, not a wrap of `editor.subscribe`'s
   parameters. The local socket happily fires every kind the client
   asks for; we drop the disallowed ones before they hit the WS
   write. This keeps the local protocol untouched and the filter
   easy to test.

## Risks

- **Reader split correctness.** The notifications/responses demux
  must not lose frames. Use a single background task that owns the
  read half and demuxes by `id` presence. Test specifically for
  interleaved notification-during-response-wait.
- **Backpressure.** A slow WS client can wedge a fast notification
  source (`agent_session_message_appended` streams 10-20 frames/s
  during a turn). Bound the notification mpsc at 256 entries; on
  overflow, drop oldest and log. (Better than killing the connection
  on a momentary client stall.)
- **`editor_mcp::socket_path()` race.** The embedded server symlinks
  `~/.spk/spk-editor-dev/config/mcp.sock` to a tempdir-backed socket;
  the symlink lifetime is the editor's lifetime. Resolve the path at
  proxy-open time, not at listener startup, so an editor restart
  (which rebuilds the symlink target) doesn't break new proxies.

## Verification

```bash
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor

set -o pipefail
cargo build --bin spk-editor 2>&1 | tee /tmp/r4_build.txt
grep -E "^error|could not compile" /tmp/r4_build.txt

cargo clippy -p remote_control --all-targets -- -D warnings 2>&1 | tee /tmp/r4_clippy.txt

cargo test -p remote_control --no-fail-fast 2>&1 | tee /tmp/r4_test.txt
grep "test result:" /tmp/r4_test.txt | awk '{ tot+=$4; failed+=$6 } END { print "TOTAL:", tot, "failed:", failed }'

# Integration test: confirm it ran end-to-end
grep "proxy_e2e" /tmp/r4_test.txt
```

Acceptance:

- [x] `cargo build --bin spk-editor` passes.
- [x] `cargo clippy -p remote_control --all-targets -- -D warnings` passes.
- [x] `cargo test -p remote_control` ≥ 31 tests (R-2 baseline) + new
      proxy unit tests + the `proxy_e2e` integration test, all green.
      (40 tests: 37 unit + 2 listener_e2e + 1 proxy_e2e.)
- [x] `proxy_e2e::end_to_end_proxy_round_trip` reaches the assertion
      that the embedded server's `editor.capabilities` was returned
      via the WS surface unchanged. (Asserts `protocol_version =
      "2024-11-05"` — the real `CapabilitiesTool` output, not the
      R-2 stub's `protocol_version: 1`.)
- [x] `remote.lsp.start` returns -32601 (asserted in the same
      `proxy_e2e` test — single integration test covers both
      acceptance items).
- [x] FORK.md remote_control row mentions the proxy (extended in
      place; no new touched-upstream entries — `context_server`
      stayed untouched per the architectural choice that motivated
      the proxy).

## When done

Sub-agent reports:
- Test counts (deltas vs R-2 baseline).
- The commit SHA(s).
- Confirmation that `context_server::listener` was NOT modified.
- The notification demux design (per-id oneshot map vs broadcast,
  bounded mpsc capacity, drop policy).
- Any subscriptions / fan-out surprises (`agent_session_*` frame
  shape, missing kinds, etc.).
- Follow-ups: per-client rate-limit, in-process direct-call switch
  if profiling shows the hop matters.

Supervisor:
1. Post-merge MCP smoke-test:
   - Toggle Remote Control ON via JSON (per the watcher path
     proven in R-2).
   - Connect a Python `websockets` client with the cert pinned.
   - Drive `remote.editor.capabilities`, `remote.solutions.list`.
   - Subscribe to `agent_session_*`, create a session, send a
     trivial message, watch a `remote/notification` arrive.
2. Mark this plan-doc `complete`, append SHAs, update
   `docs/INDEX.md`.
3. Hand off to R-5 (Android client) as the natural next phase.
