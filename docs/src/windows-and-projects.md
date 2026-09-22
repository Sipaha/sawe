---
title: Windows & Projects
description: "Work with Solution members, preserve layouts, and open projects in separate windows."
---

# Windows & Projects

Sawe groups related repositories into **Solutions**. Each Solution has member
projects and its own native AI sessions. Solution tabs appear in the title bar;
use the `+` button to open another Solution or create one.

## Working with Multiple Projects

Use the project toolbar below the title bar to switch the active member. The
project tree, Git operations, run configurations, and default file-finder scope
follow the active member. Editor tabs and pane layouts are restored for each
member when you switch back.

The Solution band below the editor hosts the active Claude or Codex session
beside terminal, Git history, or debugger utilities. Switching Solution tabs
preserves each Solution's dock state. The upstream Agent Panel and Threads
Sidebar are disabled in Sawe.

Solution checkouts live under `~/.spk/sawe/ss` on every platform. This directory
contains project files, not disposable application state.

## How Projects Open

Open an ordinary folder with **File > Open** or the CLI:

```sh
sawe ~/projects/my-app
```

Use the Solution controls to add catalog projects or create an empty member
project within a Solution. Adding a folder as an editor root does not register it
as a Solution member.

## Opening in a New Window

Sometimes you want a completely separate window. Here's how:

### From Open Recent

When using File > Open Recent ({#kb projects::OpenRecent}):

- **Enter** or **click** opens in the current window (threads sidebar)
- **Cmd+Enter** or **Cmd+click** (macOS) / **Ctrl+Enter** or **Ctrl+click** (Linux/Windows) opens in a new window

### From the CLI

Use the `-n` flag to force a new window:

```sh
sawe -n ~/projects/other-project
```

Other CLI options for controlling window behavior:

| Flag            | Behavior                                           |
| --------------- | -------------------------------------------------- |
| `-n`, `--new`   | Always open in a new window                        |
| `-a`, `--add`   | Add to the current window                          |
| `-r`, `--reuse` | Replace the current project in the existing window |

See [CLI Reference](./reference/cli.md) for full details.

### Via Settings

You can change the default CLI behavior with the `cli_default_open_behavior` setting:

```json [settings]
{
  "cli_default_open_behavior": "new_window"
}
```

Options:

- `existing_window` (default): Open folders in the current window
- `new_window`: Open folders in a new window

This setting affects CLI and double-click behavior, not the File > Open menu.

## Adding Folders to a Project

If you want to add a folder to your current project (without registering a Solution member), you have several options:

- **File menu**: File > Add Folder to Project
- **[Project panel](./project-panel.md)**: Right-click in the project panel and choose "Add Folders to Project"
- **Open Recent**: Select a recent project and click the "Add Folder to this Project" button

This adds the folder as an additional root in your current project's file tree, similar to VS Code's multi-root workspaces.

## See Also

- [Native AI Sessions](./ai/native-sessions.md): Claude and Codex in a Solution
- [Getting Started](./getting-started.md): Essential commands and setup
- [VS Code Migration](./migrate/vs-code.md): How Zed's project model differs from VS Code
