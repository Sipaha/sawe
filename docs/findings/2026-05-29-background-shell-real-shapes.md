# Finding — Claude Code background-shell lifecycle is file-backed, not tool-call-backed

**Date:** 2026-05-29. **Context:** research gate before V3 (Background Shells Strip).

The V3 plan was drafted on the assumption (its "Hard Architectural Constraint #1")
that `Bash(run_in_background=true)` output lives only in claude code's subprocess
memory, observable solely through subsequent `BashOutput` tool_calls. **That is
false.** Verified against real session transcripts under
`~/.claude/projects/<encoded-cwd>/<session>.jsonl` and the live `/tmp/claude-<uid>/`
task dirs.

## The real mechanism (three signals)

1. **Launch** — a `Bash` tool_use with `input.run_in_background == true`
   (`input` also has `command` + `description`). Its `tool_result`
   (`is_error: false`) text is:

   ```
   Command running in background with ID: bvb4ful1z. Output is being written to: /tmp/claude-1000/<encoded-cwd>/<session-uuid>/tasks/bvb4ful1z.output. You will be notified when it completes. To check interim output, use Read on that file path.
   ```

   - Shell id is a **random base36-ish token** (`bvb4ful1z`), **NOT** `bash_1`.
     The plan's `bash_[0-9]+` regex and `strip_prefix("bash_")` are wrong.
   - The **full `.output` path is in the announcement** — structurally identical
     to the Managed-Agent `output_file:` marker.

2. **Live output** — `/tmp/claude-<uid>/<encoded-cwd>/<session>/tasks/<id>.output`
   is a **real on-disk file** holding live stdout/stderr, tailable exactly like a
   managed agent's JSONL. (Managed-agent `.output` entries in the same dir are
   *symlinks* to the subagent JSONL; background-shell ones are plain files.)

3. **Completion** — a `user`-role message whose content is a `<task-notification>`
   block:

   ```xml
   <task-notification>
   <task-id>bvb4ful1z</task-id>
   <tool-use-id>toolu_01AqJufkNFAd7Aef3ojZ8d5J</tool-use-id>
   <output-file>/tmp/.../tasks/bvb4ful1z.output</output-file>
   <status>completed</status>
   <summary>Background command "Sleep for 60 seconds in background" completed (exit code 0)</summary>
   </task-notification>
   ```

   Exit code is in the `<summary>` `(exit code N)` suffix; `<status>` is
   `completed` (other values presumed `failed`/`killed` — unconfirmed).

## Consequence for V3 design

- `BashOutput` / `KillShell` tool_calls are **effectively never emitted** by claude
  in practice (0 found across every alphasol transcript) — claude just `Read`s the
  `.output` path. So a "parse BashOutput result text" strategy would almost never
  fire. **The file + the `<task-notification>` are the source of truth.**
- The feature should **mirror Managed Agents almost verbatim**: parse the launch
  announcement → tail the `.output` file (live stdout) → mark terminal on a matching
  `<task-notification>`. This is simpler AND gives a live tail instead of a stale
  last-observed snapshot.
- The plan's Constraint #3 ("clear background_shells on subprocess swap") is moot:
  research confirms `background_agents`/`active_subagents` are NOT cleared on
  `reset_context`/`rotate_context`/`set_acp_thread` — they persist by design and are
  reaped by the healthcheck tick. Mirror that; rely on stale-timeout +
  `<task-notification>` for reaping.

See the revised plan: `docs/superpowers/plans/2026-05-29-background-shells-strip.md`.
