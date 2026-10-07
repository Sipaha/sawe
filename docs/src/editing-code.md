---
title: Editing Code in Zed
description: Core code editing features in Zed including multi-cursor, refactoring, code actions, and language server integration.
---

# Editing Code

Zed provides tools to help you write and modify code efficiently. This section covers the core editing features that work alongside your language server.

## What's in This Section

- **[Code Completions](./completions.md)** — Autocomplete from language servers and AI-powered edit predictions
- **[Snippets](./snippets.md)** — Insert reusable code templates with tab stops
- **[Formatting & Linting](./configuring-languages.md#formatting-and-linting)** — Configure automatic code formatting and linter integration
- **[Diagnostics & Quick Fixes](./diagnostics.md)** — View errors, warnings, and apply fixes from your language server
- **[Multibuffers](./multibuffers.md)** — Edit multiple files simultaneously with multiple cursors

## How These Features Work Together

When you're editing code, Zed combines input from multiple sources:

1. **Language servers** provide completions, diagnostics, and quick fixes based on your project's types and structure
2. **[Edit predictions](./ai/edit-prediction.md)** suggest multi-character or multi-line changes as you type
3. **Multibuffers** let you apply changes across files in one operation

For example, you might:

- Rename a function using your language server's rename refactor
- See the results in a multibuffer showing all affected files
- Use multiple cursors to make additional edits across all locations
- Get immediate diagnostic feedback if something breaks

## Compare With Clipboard {#compare-with-clipboard}

Copy text, then right-click inside an open file and choose **Compare With
Clipboard**. The diff tab compares the entire current file, including unsaved
changes, with the clipboard text. Your cursor and selection stay in place.
The command is also available as {#action editor::CompareWithClipboard}.

The menu item is disabled when the clipboard has no text or the editor displays
multiple file excerpts. To compare a selection instead, use
{#action editor::DiffClipboardWithSelection}.

## Related Features

- [AI Features](./ai/overview.md) — Agentic editing, inline code transformations, and AI code completions
- [Configuring Languages](./configuring-languages.md) — Set up language servers for your project
- [Key Bindings](./key-bindings.md) — Customize keyboard shortcuts for editing commands
