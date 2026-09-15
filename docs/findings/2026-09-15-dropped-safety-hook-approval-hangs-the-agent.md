# A dropped safety-hook approval hangs the agent with no UI

**Date:** 2026-09-15 · **Status:** two root causes found and fixed; verified end to end on an isolated probe editor

## Symptom

A Bash tool call sits at `running` for minutes and never returns. There is no
prompt, no error, nothing to click. The only way out is restarting the agent.
Reported as *"беда с подтверждениями — я не вижу никаких запросов, а агент
подвисает"*.

The `The user doesn't want to proceed with this tool use` text that follows is
**the operator's own Stop**, not a refusal by the editor.

## What it looks like while stuck

- the shell for the command **does not exist** — nothing in `/proc` matches it;
- the `claude` process is `S (sleeping)`, `wchan = ep_poll`, ~8 s of CPU over
  11 minutes, with only its MCP servers as children;
- `~/.claude/projects/<cwd>/<session>.jsonl` ends on a `tool_use` with no
  matching `tool_result`, and stops growing.

So the call stalls **before** the shell is spawned.

## Root cause

1. The command's `rm` target is a shell variable claude cannot prove non-empty —
   it is assigned inside the same command, e.g. in a loop:
   `for n in a b; do D=…/$n; rm -f "$D"/*.jar; done`.
2. Claude's safety check fires: *"Dangerous rm operation detected: `"$D"/*.jar`
   … points at the filesystem root (or a top-level directory) when the variable
   is unset or empty … **This requires explicit approval and cannot be
   auto-allowed by permission rules.**"* `--permission-mode bypassPermissions`
   and `--allow-dangerously-skip-permissions` deliberately do **not** cover this
   class.
3. Claude sends `control_request` / `can_use_tool` and blocks on the reply. That
   frame carries **no `tool_use_id`** — the call has not been admitted yet, so
   no id exists. It brings `display_name`, `description` and `decision_reason`
   instead.
4. `ControlRequestKind::CanUseTool` declared `tool_use_id: String` as
   **required**, so serde rejected the whole frame. `#[serde(other)]` does not
   catch this: it only covers an unknown *tag*, not a variant that fails to
   deserialize.
5. `process.rs`'s reader logged `claude stdout parse error` at `warn` and
   **dropped the line**. Nothing ever replied.

Claude waits forever. The editor has no idea a question was asked.

## Reproducer (deterministic)

Any single Bash call of this shape, in a full-access session:

```sh
for n in a b; do D=/tmp/probe/$n; rm -f "$D"/*.jar; done
```

Bisection that isolated it — each probe its own tool call:

| Probe | Shape | Result |
|---|---|---|
| `rm -f $HOME/.m2/.../__none__/*.jar` | `rm` + glob, huge tree, literal path | 1.2 s |
| `cp -a "$D"/*.jar …` in a loop | `cp` + variable + glob | 9 s |
| `D=/tmp/probe/a; rm -f "$D"/*.jar` | `rm` + variable, **no loop** | 9 s |
| `for n in a b; do rm -f /tmp/probe/$n/*.jar; done` | `rm` + glob, literal path | 12 s |
| `for n in a b; do D=/tmp/probe/$n; rm -f "$D"/*.jar; done` | `rm` + loop-assigned variable | **hangs** |
| same, unquoted `$D` | | **hangs** |

Neither the tree size nor `~/.m2` nor quoting matters; `cp` is not affected. A
variable claude can resolve statically (assigned once, outside the loop) is not
affected either.

## What was NOT the cause

Each of these was checked and cleared, so don't re-check them:

- **The claude version.** `2.1.258` was pinned by an earlier workaround
  (`~/.spk/sawe/ss/chunkedupload/.agents/claude-code-hang-brief.md`) on the
  theory that 2.1.259 caused it. All agents were running the pinned build and
  still hung. That brief's conclusion is superseded.
- **The `PreToolUse` hook** (`guard_process_probes.py`): 28 ms, exit 0, on the
  exact stalled command.
- **Permission mode / deny rules.** Sessions run with bypass; this class is
  exempt from it by design, which is why bypass looked like "no gate at all".
- **A shared resource** (the editor, the MCP socket). Across 4874 Bash calls in
  four days there were 7 unexplained stalls, and stalls in different projects
  never overlap in time.
- **The command actually running.** A call whose text merely *contained* the
  stalled command inside single quotes — never executing it — stalled the same
  way. The trigger is the text.

## Fix

Three parts, all in `crates/claude_native`:

1. **`protocol.rs` — parse the frame.** Every field of `CanUseTool` is now
   optional (`tool_use_id: Option<String>`, `decision_reason: serde_json::Value`
   so a richer shape tomorrow cannot resurrect the same hang).
2. **`connection.rs` — never leave a control request unanswered.** The
   `ControlRequestKind::Other` arm now *declines* instead of logging and
   dropping. Declining is recoverable; silence is not.
3. **`tool_authorization.rs` — decide who answers.** A request with no
   `decision_reason` is the ordinary Agent Teams gate and is answered by policy
   (unchanged). One *with* a reason is a safety hook, and the operator's rule
   applies: **full rights inside the Solution, a confirmation outside it.** The
   module resolves the flagged target statically — literal `NAME=value`
   assignments, `$HOME`/`~`, stopping at the first unresolvable part and keeping
   the longest literal prefix — and auto-allows only when every target provably
   lands inside a work directory. Anything unprovable goes to the operator.

An `Ask` is raised on the live `AcpThread` as an ordinary tool-call
authorization, attached to the tool call claude already streamed, so the
existing Allow/Reject affordance renders it. Claude's own words go on the call's
`content` (desktop) and on `raw_input.reason`, which `mcp::dto` lifts into the
new `ToolCallSummary.authorization_reason` for the phone.

## A second bug, found only by verifying

The first end-to-end run raised the prompt correctly, the operator allowed it —
and the command still did not run. Two more things were wrong with the reply:

1. **The envelope was flat.** `InputMessage::ControlResponse` writes
   `{"type":"control_response","request_id":…,"response":<body>}`, and
   `permission_response` put `{"behavior":…}` straight into `<body>`. Claude
   requires the body to be the nested success envelope —
   `{"subtype":"success","request_id":…,"response":{"behavior":…}}` — the same
   shape it uses when *it* answers us, and the same shape `build_hook_response`
   has always hand-built. A flat body is **silently ignored**: claude keeps
   waiting, indistinguishable from the dropped-frame stall.
2. **An allow must carry `updatedInput`** — the input claude actually runs.

A/B against a live claude, answering the same `can_use_tool` both ways:

| Reply shape | Outcome |
|---|---|
| flat | ignored — claude kept waiting until the harness killed it |
| nested | accepted — `tool_result` 0.2 s later |

This means the *ordinary* teammate gate never worked either: every
auto-approval we have ever sent was dropped on the floor. It was invisible
because Agent Teams teammates are rare next to main-agent calls, and its
symptom is the same silent stall.

Deny carries a `message` instead of `updatedInput`.

## Mobile

The prompt already rode the wire (`ToolCallSummary.options` +
`solution_agent.authorize_tool_call`), but without the *question*: the reason
survived only inside the ~500-char-truncated `args_preview`. Added
`ToolCallSummary.authorization_reason`, gated by the `tool_auth_reason`
`wire_features` token — **not** a `wire_schema_version` bump, which is an
equality check and would brick every installed phone.

## How to apply

- A control request from a runtime is a **question that blocks the other side**.
  Parse it permissively and answer it always, including the paths you did not
  anticipate. A required field in a wire enum is a decision to hang on any
  message shape you have not seen yet.
- `#[serde(other)]` is not a safety net for missing fields — only for unknown
  tags.
- When a runtime says an approval "cannot be auto-allowed", do not auto-allow it
  because a broader policy flag is set. Route it to the human, or prove the
  specific thing it is worried about is not true here.
- **A write-side wire shape needs a test against the real peer.** The
  deserializer for `control_response` had already been fixed once, with a
  comment explaining the nesting — and the serializer next to it stayed flat for
  as long, because nothing ever asserted that claude ACTED on a reply. Unit
  tests that check our own JSON prove nothing about what the other end accepts.

## Verification

Against a probe editor (`script/run-mcp --debug --headless --runtime-dir …`),
driving a real session over MCP:

| Case | Expected | Result |
|---|---|---|
| target outside the Solution | asks, then runs on approval | asked in 3 s with options + `authorization_reason`; answered through `solution_agent.authorize_tool_call` (the phone's own path); command completed 3 s later |
| target inside the Solution | full rights, never asks | completed in 6 s, no prompt |
