---
title: Getting Started with Sawe
description: Get started with Sawe. Essential commands, environment setup, and navigation basics.
---

# Getting Started

Sawe is a code editor based on Zed, with multi-project Solutions and native Claude and Codex sessions.

This guide covers the essential commands, environment setup, and navigation basics.

## Quick Start

### Welcome Page

Sawe opens a separate launcher window with Solution controls and recent
Solutions. Opening a Solution opens its workspace. The launcher remains separate
from editor panes and docks. Use {#action zed::ShowWelcome} to reopen it.

### 1. Open a Project

Open a folder from the command line:

```sh
sawe ~/projects/my-app
```

Or use `Cmd+O` (macOS) / `Ctrl+O` (Linux/Windows) to open a folder from within Sawe.

Solution tabs in the title bar switch between open Solutions. To open in a new window instead, use `sawe -n ~/projects/my-app` or press `Cmd+Enter` when selecting from Open Recent. See [Windows & Projects](./windows-and-projects.md) for more details.

### 2. Learn the Essential Commands

| Action          | macOS         | Linux/Windows  |
| --------------- | ------------- | -------------- |
| Command palette | `Cmd+Shift+P` | `Ctrl+Shift+P` |
| Go to file      | `Cmd+P`       | `Ctrl+P`       |
| Go to symbol    | `Cmd+Shift+O` | `Ctrl+Shift+O` |
| Find in project | `Cmd+Shift+F` | `Ctrl+Shift+F` |
| Toggle terminal | `` Ctrl+` ``  | `` Ctrl+` ``   |
| Open settings   | `Cmd+,`       | `Ctrl+,`       |

The [command palette](./command-palette.md) (`Cmd+Shift+P`) is your gateway to every action in Sawe. If you forget a shortcut, search for it there.

### Panel Layout

Sawe uses a project toolbar above the editor and a Solution band below it. The band hosts native Claude or Codex sessions beside terminal, Git history, or debugger utilities. The upstream Agent Panel and Threads Sidebar are not enabled.

### 3. Configure Your Editor

Open the Settings Editor with `Cmd+,` (macOS) or `Ctrl+,` (Linux/Windows). Search for any setting and change it directly.

Common first changes:

- **Theme**: Press `Cmd+K Cmd+T` (macOS) or `Ctrl+K Ctrl+T` (Linux/Windows) to open the theme selector
- **Font**: Search for [`buffer_font_family`](./reference/all-settings.md#buffer-font-family) in Settings
- **Format on save**: Search for `format_on_save` and set to `on`

### 4. Set Up Your Language

Sawe includes built-in support for many languages. For others, install the extension:

1. Open Extensions with `Cmd+Shift+X` (macOS) or `Ctrl+Shift+X` (Linux/Windows)
2. Search for your language
3. Click Install

See [Languages](./languages.md) for language-specific setup instructions.

### 5. Try AI Features

Create a native Claude or Codex session in the Solution band. Sessions share the
Solution's member-project context and keep separate conversations. Authentication
uses the installed provider CLI. See [Native AI Sessions](./ai/native-sessions.md).

The inherited upstream Agent Panel, sign-in, and cloud-hosted agent UI are disabled.
Inline assistance and API-key providers remain available where configured.

## Coming from Another Editor?

We have dedicated guides for switching from other editors:

- [VS Code](./migrate/vs-code.md) — Import settings, map keybindings, find equivalent features
- [IntelliJ IDEA](./migrate/intellij.md) — Adapt to Zed's approach to navigation and refactoring
- [PyCharm](./migrate/pycharm.md) — Set up Python development in Sawe
- [WebStorm](./migrate/webstorm.md) — Configure JavaScript/TypeScript workflows
- [RustRover](./migrate/rustrover.md) — Rust development in Sawe

You can also enable familiar keybindings:

- **Vim**: Enable `vim_mode` in settings. See [Vim Mode](./vim.md).
- **Helix**: Enable `helix_mode` in settings. See [Helix Mode](./helix.md).

## Join the Community

Zed is open source. Join us on GitHub or in Discord to contribute code, report bugs, or suggest features.

- [Discord](https://discord.com/invite/zedindustries)
- [GitHub Discussions](https://github.com/zed-industries/zed/discussions)
- [Zed Reddit](https://www.reddit.com/r/ZedEditor)
