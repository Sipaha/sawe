# Nested tool output must remain literal text

A shell printed another session's transcript, including four-backtick code
fences. `conversation_render/tool_call.rs` enclosed the whole output in a
fixed four-backtick fence. The inner fence closed the outer block, and
Makefile comments beginning with `#` became large Markdown headings.

Use `util::markdown::MarkdownCodeBlock` to choose a fence wider than any
backtick run in plain output, raw JSON, terminal output and diff summaries.
The pre-fenced shortcut is valid only when the entire nonblank source is one
explicitly closed code block. A block followed by prose is still raw output.

The 15-line preview truncator now tracks the opening marker and width:
shorter or differently marked fences are literal content, and closing fences
cannot carry info strings. If truncation cuts an open block, insert its actual
closing fence before the more-lines hint.

Five parser-based regressions cover nesting, initial code followed by headings,
valid preformatted blocks, malformed fences and truncation. Verification logs
and screenshots live in the Solution's `.agents/tmp/tool-fence-*` artifacts.

Verified: `cargo test -p solution_agent --lib` passed 997 tests (one existing
ignored test); Clippy with warnings denied passed in the debug profile. Debug
and release-fast editor builds passed. A headless editor screenshot of nested
transcript output shows Makefile comments as literal monospaced text, with the
more-lines hint outside the code block (`.agents/tmp/tf/fixed-output.png`).
