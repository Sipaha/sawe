# TODO: MCP tool to inspect / clear the remote_control ban list

## Status

Open. Recorded 2026-05-19 after a live incident where a flaky mobile
network put the maintainer's home IP into the
30s → 5min → 1h → 24h ban-escalation ladder. The editor process was
running an active agent session (could not be restarted to clear the
in-memory ban state) and there was no other way to recover than
either waiting out the ban or switching the phone's network to a
different subnet.

## Why this hurts in practice

`crates/remote_control/src/listener.rs` keeps `bans` as a
process-scoped `AsyncMutex<HashMap<IpAddr, BanRecord>>`. There is no
disk persistence and no admin surface — once a subnet hits failure #3
(`BAN_BACKOFF_SECS = [30, 300, 3_600, 86_400]`, indexed by
`consecutive_failures - 1`), the maintainer's only recovery options
today are:

1. Restart `spk-editor` — drops the bans, BUT also tears down every
   live ACP session, which is costly mid-conversation (and impossible
   without losing in-flight agent work / pending sends).
2. Switch network (mobile data ↔ Wi-Fi) so the new connection comes
   from a different /24 subnet. Works as a one-shot but is friction
   for every flap, and not always available (e.g. travel / metered).
3. Wait it out. At failure #3 that's an hour; #4 is 24h.

A scriptable / agent-driven recovery path is missing. The 2026-05-19
fix in `listener.rs` that stopped counting pre-handshake transport
errors as auth failures (the "TLS/WS upgrade error → ban" path that
was tripped by mobile network glitches) takes most of the pressure
off, but the underlying gap remains: there is no way to inspect or
clear the live ban map without bouncing the process.

## Proposed MCP surface

Two read/write tools under `editor.bans.*` (NOT under `remote.*` — the
WS-side allow-list, by design, never exposes admin operations to a
paired remote client; you must be on the local MCP socket to drive
these):

```jsonc
// editor.bans.list  →  { entries: [{ subnet, banned_until_ms, last_seen_ms, consecutive_failures }] }
// editor.bans.clear { subnet?: string }  →  { removed: <count> }
//   omit `subnet` to clear ALL; pass a /24 (v4) or /64 (v6) prefix to clear one
```

Implementation outline:

1. Add `pub fn ban_state_snapshot(state: &ListenerState) -> Vec<BanEntry>`
   and `pub fn clear_ban(state: &ListenerState, subnet: Option<IpAddr>)`
   in `listener.rs` next to `record_auth_failure`.
2. Plumb a `ListenerHandle` (or just `Arc<ListenerState>`) up through
   `remote_control::init` so `editor_mcp` registrants can reach it.
   Mirror the `BinaryFrameHandler` indirection pattern (`30fe8eed6a`)
   — don't add a `remote_control` dep to `editor_mcp`; instead expose
   `set_ban_admin_handler(Arc<dyn Trait>)` from `remote_control` and
   register the handler in `main.rs`.
3. Register the two tools from `editor_mcp::init` (or a fork-owned
   `remote_control_mcp` module if `editor_mcp` should stay generic).

## Acceptance

- `editor.bans.list` returns the current ban map with `banned_until_ms`
  in absolute wall-clock millis so a script can show "unbans in Xs".
- `editor.bans.clear` with no `subnet` empties the map; with a subnet
  argument removes just that row.
- A unit test in `remote_control` covers: insert ban via
  `record_auth_failure`, snapshot, clear, re-snapshot empty.
- A second test covers the partial-clear path (two bans, clear one,
  the other survives).
- Not exposed in `remote_control::allow_list` (deliberately —
  scripting must originate on the local socket).

## Suggested .rules additions

None yet — the lesson ("don't count pre-handshake transport errors as
auth failures") is already addressed by the inline 2026-05-19 fix in
`listener.rs`. The MCP tool itself is a feature gap, not a trap to
avoid.
