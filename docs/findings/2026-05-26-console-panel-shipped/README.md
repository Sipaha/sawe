# ConsolePanel shipped — verification screenshots

Live-tested 2026-05-26 against the `hook-inject` branch (debug build, native `--headless` mode).

| File | What it shows |
|---|---|
| `00-empty-panel.png` | ConsolePanel after a fresh open — empty tab strip with only the `+` popover button. |
| `01-terminal-and-chat-tabs.png` | Both tab kinds in one strip: terminal "mini — bash" (`Terminal` icon) at the active worktree's cwd, AI chat "AlphaSol" (`Sparkle` icon) with the live status row (state badge, token meter, `claude-acp` adapter, cwd `ROOT`) and the compose box. |
| `02-restored-after-restart.png` | Same two tabs after a full editor restart — verifies the `console_panel_state` table round-trip and the chat-session hydration path (`SolutionAgentStore::hydrate_all_for_solution`). The chat status badge says `Sleeping` because the session resumed from disk. |

Driving the verification:

```sh
script/run-mcp --debug --headless --skip-onboarding &
# solutions.open alphasol
# workspace.dispatch_action console_panel::NewTerminal
# workspace.dispatch_action console_panel::NewChat
# workspace.screenshot { solution_id: "alphasol", format: "png" }
```

(Use `python3` against `~/.spk/spk-editor-dev/config/mcp.sock` via a 10-line JSON-RPC client — see `crates/editor_mcp/tests/solutions_add_member_e2e_test.rs::call_tool` for the shape.)
