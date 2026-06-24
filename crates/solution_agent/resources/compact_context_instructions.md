# Compact this session and prepare a clean handoff

The user has triggered the **Compact Context** action because this session
is approaching its context budget. Your job now is to capture every load-
bearing piece of state from the current conversation into durable files,
then ask the editor to start a fresh session that will pick up exactly
where this one left off — minus the ballast.

The editor has injected the variables you need below; do not invent
paths, do not write files anywhere else.

## Variables

- `SESSION_ID` = `{{session_id}}`
- `COMPACT_DIR` = `{{compact_dir}}`
  (= `<solution_root>/.agents/<SESSION_ID>/c<NN>/`, where `c<NN>` is
  the 1-based index of the context being closed; the next context in
  this session lives in `c<NN+1>` after rotation. Path ends with a
  separator — `{{compact_dir}}continue.md` is the literal path of the
  file you will write.)
- `SOLUTION_ID` = `{{solution_id}}`
- `AGENT_ID` = `{{agent_id}}`
- `STARTED_AT` = `{{started_at_iso}}`
- `TOKENS_USED` = `{{tokens_used}}`
- `TOKENS_MAX` = `{{tokens_max}}`
- The directory `COMPACT_DIR` already exists and is writable.

## Step 1 — Decide the scope

Before writing anything, classify the conversation:

- **A. Clear next task.** You and the user have an agreed-upon plan or an
  in-flight feature with obvious next steps. Capture the plan; the
  continuation prompt should resume that plan.
- **B. Multiple possible next steps, none picked.** The conversation
  branched and you are unsure which direction to take. **Stop and ask
  the user** which direction to compact toward — do NOT call the MCP
  tool until they answer. Once they answer, treat their reply as case
  A and proceed.
- **C. No clear forward task** (exploration, debugging, post-mortem
  with no commitments). Skip the "next task" assumptions; just dump
  what was *learned* so the next session can pick up cold without
  re-deriving it.

## Step 2 — Write the handoff files into `COMPACT_DIR`

Create exactly these files. Use plain, dense prose — no banners, no
emojis, no "I will now …" preamble. Each file stands alone.

### `state.md`
What is the current state of the world?
- What was the user trying to accomplish in this session.
- What got *done* (concretely: files edited, commits, PRs, tools run,
  conclusions reached).
- What is *in flight* (e.g. "branch X has uncommitted changes to Y").
- Any environment / config the next session must know about that it
  cannot rederive (auth tokens already exchanged, mocked services,
  scratch directories created, running PIDs that hold sockets/DB
  locks, etc.).
- **The natural language the user has been communicating in** during
  this session. The next context starts cold and cannot otherwise know
  which language to use, so state it explicitly (e.g. "User writes in
  Russian — address the user in Russian; internal reasoning and generic
  status phrases may stay English."). This is load-bearing: the resumed
  agent must keep talking *to the user* in their own language.

### `decisions.md`
Architectural / design / approach decisions made during the session.
For each: the decision itself, the reasoning *why*, and one line on
"what this rules out". Future-you needs the *why* to handle edge cases
the recap doesn't anticipate.

### `next.md` *(only for cases A and B; omit for case C)*
The plan going forward. A numbered list of concrete next actions, each
with a single-line "done when" criterion. Do not pad — if there are
two real steps, write two.

### `continue.md`
**This is the user-message that will be fed verbatim into the new
session.** Write it as if you are a teammate who has read all of the
above files and is briefing a fresh agent. It must:
- State the goal in one paragraph.
- Reference `state.md`, `decisions.md`, `next.md` by their full
  absolute paths (they live in `COMPACT_DIR` — i.e. under
  `<solution_root>/.agents/<SESSION_ID>/c<NN>/`). The new session
  has a cold context; those files are its only memory of this one.
- **Tell the new agent which language to address the user in.** Name the
  language the user has been communicating in this session (also recorded
  in `state.md`) and instruct the new agent: every reply *addressed to the
  user* — the terse acknowledgement above, answers, questions, summaries —
  must be in that language, starting with the very first one. Its own
  internal reasoning / thinking and throwaway progress interjections (e.g.
  "exploring the current state") are NOT replies to the user in this sense
  and may stay in English — the user does not read those for language.
- **Demand a terse, action-first style.** Instruct the new agent that,
  on reading this brief, it must reply with a single short
  acknowledgement in the user's language (the equivalent of "got it —
  on it") and then just do the work. No progress narration, no status
  recaps, no "I will now…" preamble, no restating the plan back — emit
  user-facing prose only when it hits a real blocker or genuinely needs
  the user's input. The user does not want tokens burned on commentary
  they did not ask for.
- End with the *first concrete instruction* (the new agent's first
  step), not with "let me know if you have questions". Be directive.
- For **case C**, the first instruction is "Read the files above and
  ask the user what they want to tackle now."

### `session-state.json`
Machine-readable technical metadata. Write exactly:

```json
{
  "session_id": "{{session_id}}",
  "solution_id": "{{solution_id}}",
  "agent_id": "{{agent_id}}",
  "started_at": "{{started_at_iso}}",
  "compacted_at": "<UTC ISO-8601 of the moment you wrote this file>",
  "tokens_used": {{tokens_used}},
  "tokens_max": {{tokens_max}},
  "scope": "<one of: planned | branching | exploratory>"
}
```

`scope` corresponds to the case you picked in Step 1 (A → planned,
B → branching, C → exploratory).

## Step 3 — Trigger the session rotation

After all files are on disk, invoke the editor's
`solution_agent.compact_session` MCP tool with these arguments:

```json
{
  "session_id": "{{session_id}}",
  "prompt_file": "{{compact_dir}}continue.md"
}
```

### How to invoke (pick the path your runtime supports — try in order)

1. **Native MCP-tool binding.** If the sawe MCP server's tools
   are auto-bound in your runtime as callable functions (Claude Code
   bridges them as `mcp__<server>__<tool>` or similar), invoke
   directly. The exact callable name varies by harness; try
   `mcp__sawe__solution_agent_compact_session` or
   `mcp__sawe__solution_agent_compact_session` first.

2. **Direct JSON-RPC over the editor's Unix socket.** Always works as
   long as the editor process is alive. The socket lives at
   `$HOME/.spk/sawe-dev/config/mcp.sock` for a `--debug` build
   or `$HOME/.spk/sawe/config/mcp.sock` for release; the path
   is symlinked into the runtime tmpdir but the symlink under
   `$HOME` is stable. Newline-delimited JSON-RPC 2.0. Example using
   Bash + Python (substitute `{{session_id}}` and the prompt path):

   ```bash
   python3 - <<'PY'
   import socket, json, os
   sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
   sock.connect(os.path.expanduser("~/.spk/sawe/config/mcp.sock"))
   f = sock.makefile("rwb", buffering=0)
   req = {
       "jsonrpc": "2.0", "id": 1,
       "method": "tools/call",
       "params": {
           "name": "solution_agent.compact_session",
           "arguments": {
               "session_id": "{{session_id}}",
               "prompt_file": "{{compact_dir}}continue.md",
           },
       },
   }
   f.write((json.dumps(req) + "\n").encode())
   while True:
       line = f.readline()
       if not line: break
       msg = json.loads(line)
       if msg.get("id") == 1:
           print(json.dumps(msg, indent=2))
           break
   PY
   ```

### What success looks like

The tool response carries a `structuredContent` block of the form
`{"new_session_id": "...", "prompt_bytes": <N>}` and a `content`
text line like `"rotated <sid> into context c<NN> (<N> bytes)"`.
Once you see that, the rotation has happened: your conversation
history is gone from the current ACP thread, `continue.md` is the
first user message in the new context, and the next reply you'd
otherwise compose would already be running against the fresh
context. Do NOT send any further messages — the rotation is the
end of this context's contribution.

### If the tool errors

- `unknown session ...` — the editor has lost track of this session.
  Don't retry; surface the error to the user.
- `prompt_file is empty` / `prompt_file is N bytes, max is M` /
  `prompt_file contains only whitespace` / `not a regular file` —
  your `continue.md` write didn't land the content you think. Re-read
  the file, fix, retry.
- `invalid_params: prompt_file must live under <root>/.agents/` —
  you wrote the file outside `COMPACT_DIR`. Move it under `COMPACT_DIR`
  and retry.
- `<COMPACT_DIR>/<file>.md missing or empty` — `validate_handoff_files`
  refused the rotation because one of `state.md` / `decisions.md` /
  `next.md` (case A / B) / `continue.md` / `session-state.json` is
  missing or zero-bytes. Re-check Step 2 + retry.

### If you cannot invoke the MCP tool at all

(transport unavailable, socket missing, runtime gives no way to
shell out, etc.) — STOP. Do NOT mark the compact "done". Tell the
user: "Handoff files are written at `{{compact_dir}}` but I cannot
invoke `solution_agent.compact_session` from this runtime. To
complete the rotation, please start a fresh session manually and
feed the contents of `{{compact_dir}}continue.md` as the first
user message." A failed compact must be observable, not silent.

## Hard rules

- Never write files outside `COMPACT_DIR`.
- Never call any other MCP tool to "clean up" the session yourself —
  rotation is owned by `solution_agent.compact_session`.
- If a previous compact attempt left files in a sibling directory
  (`c01`, `c02`, etc.), ignore them; they belong to a different
  rotation.
- If you cannot write a file (permission error, disk full), tell the
  user, stop, and do **not** call the MCP tool. A failed compact must
  be observable, not silent.
- Do not paste the contents of `state.md` / `decisions.md` / `next.md`
  back into your final reply — they're for the NEXT session to read
  off disk, not for the user to scroll past in this one.
