---
title: Native AI Sessions
description: Work with Claude and Codex sessions scoped to a Sawe Solution.
---

# Native AI Sessions

Sawe runs Claude Code and Codex through their installed native runtimes. Create a
session in the Solution band and select its provider. Each session has a separate
conversation, while its project context includes the Solution's member projects.
Provider authentication comes from the installed CLI rather than a Zed account.

The session controls expose runtime models and approval settings. Keep approval
choices appropriate for the repositories and commands you want that session to
use. Stop cancels ongoing work; session state and history remain visible in the
Solution band.

You can keep multiple sessions in a Solution and switch their tabs without
changing the active member project. The adjacent utility area hosts terminals,
Git history, or the debugger.

## Conversation previews {#conversation-previews}

Click a local file link in a conversation to preview it inside the current
window. PNG and other supported images open visually; Markdown renders as a
formatted document, and other text opens as selectable source. Links within a
Markdown preview open in the same preview, relative to that document's folder.

Other binary files show file information. Use **Open in File Manager** to reveal
one, or **Open in Default Application** to open it with the application associated
with that file type by your operating system. Press Escape or click the close
button to return to the conversation.

The upstream Zed Agent Panel, cloud-agent entry, collaboration UI, and sign-in
surface are disabled. Other pages describing those upstream interfaces are
reference material and do not describe the Sawe session controls.

See [Windows & Projects](../windows-and-projects.md) for Solution organization and
[Git](../git.md) for working-tree changes and commit review.
