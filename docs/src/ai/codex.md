# Codex in Sawe

Sawe runs the installed Codex CLI through its native app-server interface.
Codex chats appear alongside Claude chats in the Solution conversation area.

## Start a chat

1. Install the Codex CLI and make `codex` available on your executable path.
2. Run `codex login` in a terminal and finish authentication.
3. Open a Solution in Sawe.
4. Click the arrow next to the chat `+`, then choose **New Codex chat**.
   You can also run **Console Panel: New Codex Chat** in the command palette.

The `+` button continues to create a Claude chat. Each new chat starts at
its Solution root and receives the list of member projects. Codex uses
`AGENTS.md` for project instructions.

## Models and reasoning

Use the model selector above the message input. Sawe reads available models
from your installed Codex runtime. The reasoning selector offers the levels
advertised for the selected model; choices apply to the next turn.

## Tools and permissions

Text, reasoning summaries, commands, file changes and tool results appear
in the conversation. Codex starts with workspace-write sandboxing and
on-request approvals. Command and file-change approval requests appear in
the existing permission UI.

Messages sent while Codex is busy enter Sawe's queue. **Send now** interrupts
the current turn and sends the queued message. **Stop** cancels the current
turn. Closing and reopening a chat resumes its Codex thread and keeps the
locally saved conversation.

## Troubleshooting and current limits

Startup errors appear as editor notifications. If Codex is missing, check
`codex --version` in a terminal. If authentication is missing, run
`codex login`, then create or reopen the chat.

This integration uses Codex CLI authentication and runtime files; it does
not require copying an API key into Sawe settings. It supports text and
image input. Additional permission-profile requests and MCP elicitation
forms are declined rather than automatically approved. Codex-internal
subagent streams are not merged into the parent transcript.

Desktop scheduling, task worktree management and other Desktop UI features
are separate from the runtime integration.

See [Codex app-server documentation](https://learn.chatgpt.com/docs/app-server)
for the underlying protocol.
