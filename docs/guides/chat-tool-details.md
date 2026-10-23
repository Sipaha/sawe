# Tool calls in Solution chat

Consecutive tool calls occupy one compact block in the conversation. The block
shows the number of calls, running/failure status and the latest call, with a
short command and output preview. Its height is capped at 112 pixels, so long
commands and output do not fill the chat.

Click **View** beside the tool count to open **Tool calls** inside the editor.
The rest of the block does not open details, reducing accidental clicks. The
modal contains the full requests and outputs of that group and updates while
calls are running.
Use its Copy button to copy the full details; close it with Escape or the close
button. Opening another preview retargets the same modal. Tool details use the
chat font settings and compact headings; document previews retain their own
Markdown preview font settings.

An assistant reply, user message or other non-tool entry starts a new group.
Calls from different agent streams do not merge. Calls waiting for confirmation
remain visible with their normal approval controls instead of being hidden in
a group.

Find in session temporarily expands tool groups so matches in the saved
conversation text remain visible and highlighted. Closing Find restores compact
groups. Grouping changes
only presentation: stored history, exported/wire entries and agent context retain
all original tool calls and output.

If older history retained only a shortened preview and no full result, the modal
says so explicitly. It cannot recover output the older editor did not save.
