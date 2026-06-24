# Embedded MCP wire envelope: `tools/call` vs bare method names

**Date:** 2026-05-16 (R-4)
**Status:** stable
**Author:** R-4 subagent

## TL;DR

On the embedded `editor_mcp` Unix socket, the upstream tool catalogue
(`editor.capabilities`, `solutions.list`, `solution_agent.send_message`,
…) is **not** invoked by setting JSON-RPC `method = "editor.capabilities"`.
Tools are nested inside the MCP wrapper request:

```jsonc
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": { "name": "editor.capabilities", "arguments": {} }
}
```

A bare `{"method": "editor.capabilities"}` returns `-32601 "unhandled
method editor.capabilities"`. This is per the MCP spec — `tools/call`
is the dispatch verb, and the tool name is data — but if you grew up
reading the WS-side `remote.*` allow-list, the temptation is to translate
`remote.editor.capabilities` → bare `editor.capabilities` and call it
as the JSON-RPC method. R-4's `UnixMcpProxy::call_tool` wraps the
envelope explicitly; the allow-list module's `translate()` returns the
bare tool name, and the proxy adds the `tools/call` wrapper.

## Frame shapes (canonical)

### Request (`tools/call`)
```jsonc
{ "jsonrpc": "2.0", "id": <i32>, "method": "tools/call",
  "params": { "name": "<tool>", "arguments": <opaque> } }
```

### Response (success)
```jsonc
{ "jsonrpc": "2.0", "id": <i32>,
  "result": { "content": [...], "structuredContent": <opaque>, "isError": false } }
```

Note the JSON key is `structuredContent`, camelCase (not snake). Per
`crates/context_server/src/types.rs::CallToolResponse` serde rename.

### Response (error)
```jsonc
{ "jsonrpc": "2.0", "id": <i32>,
  "error": { "code": -32601, "message": "Tool not found: foo" } }
```

The error shape is the minimal JSON-RPC variant — `data` is omitted.

### Notification (server-pushed)
```jsonc
{ "jsonrpc": "2.0", "method": "editor/notification",
  "params": { "kind": "agent_session_message_appended", "payload": {…} } }
```

No `id` field. `kind` is at `params.kind` (not `params.payload.kind`),
which matters when filtering at the fan-out layer.

## Why this isn't obvious

1. The wire-side `editor_mcp` documentation in `.rules` says "60-tool
   catalog — JSON-RPC 2.0 over the socket — namespaces: `editor.*`,
   …" — which reads as if `editor.capabilities` is the JSON-RPC method.
   It's not. The list catalogues tool names within the `tools/call`
   envelope.
2. The reference client at
   `crates/editor_mcp/tests/solutions_add_member_e2e_test.rs::call_tool`
   shows the right shape but it's tucked inside a 200-line e2e test;
   easy to miss when grepping for "how do I call X".
3. The `editor.subscribe` tool name uses a dot, which would be a legal
   JSON-RPC method name in its own right. There's no syntactic tell
   that this string is data inside an envelope vs the envelope's
   method.

## Implications for R-4 proxy

- `allow_list::translate("remote.X.Y") = Some("X.Y")`. The `X.Y` then
  goes into `tools/call`'s `params.name`. There's no other translation.
- The proxy's per-request id is a freshly minted local i32 (matches
  upstream's `RequestId::Int(i32)`), independent of the WS client's id
  (which can be any JSON value). The response's id is substituted with
  the WS id before forwarding.
- The dispatcher matches on the response's `error` xor `result` keys
  and rewrites into the wire-side `JsonRpcResponse` shape. The
  `result` body (`{content,structuredContent,isError,meta}`) is passed
  through verbatim — clients see the same MCP-shaped payload as
  autonomous agents.

## How to verify

```
$ socat - UNIX-CONNECT:$HOME/.spk/sawe-dev/config/mcp.sock
{"jsonrpc":"2.0","id":1,"method":"editor.capabilities"}
{"jsonrpc":"2.0","id":1,"error":{"message":"unhandled method editor.capabilities","code":-32601}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"editor.capabilities","arguments":{}}}
{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"editor_mcp vX.Y.Z"}],"structuredContent":{"protocol_version":"2024-11-05",…}}}
```

The first frame's `-32601` is the diagnostic that distinguishes the
two protocols.
