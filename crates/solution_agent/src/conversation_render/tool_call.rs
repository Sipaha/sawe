//! Tool-call render cluster for the Solution conversation view.
//!
//! Relocated verbatim from `conversation_render.rs` (Tier-1 god-object split).

use super::*;
use util::markdown::MarkdownCodeBlock;

/// The single most informative string value in a tool call's `raw_input`,
/// VERBATIM — newlines and full length intact. Prefers a well-known argument
/// name (`command`, `file_path`, `path`, `pattern`, `query`, `url`) when
/// present so a Bash call surfaces its command, a Read its file_path, a Grep
/// its pattern; falls back to the first non-empty string value.
///
/// This is what the "full argument" modal shows. [`tool_call_arg_preview`]
/// squeezes the same value onto one line for the header row — the two must
/// agree on WHICH value they are talking about, hence one picker.
pub(crate) fn tool_call_arg_value(raw_input: &serde_json::Value) -> Option<&str> {
    if let Some(value) = raw_input.as_str() {
        return (!value.is_empty()).then_some(value);
    }
    const PREFERRED_KEYS: &[&str] = &[
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "old_string",
    ];
    let obj = raw_input.as_object()?;
    PREFERRED_KEYS
        .iter()
        .find_map(|k| {
            obj.get(*k)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            obj.values()
                .find_map(|v| v.as_str().filter(|s| !s.is_empty()))
        })
}

/// ACP permits approval details in content without rawInput. Keep both:
/// an approval may also carry a diff or context absent from the shell command.
pub(crate) fn approval_details(
    raw_input: Option<&serde_json::Value>,
    content_md: &[String],
) -> Option<String> {
    let mut sections = Vec::new();
    if let Some(input) = raw_input.filter(|input| !input.is_null()) {
        let input = match input.as_str() {
            Some(text) => text.to_owned(),
            None => serde_json::to_string_pretty(input).ok()?,
        };
        if !input.is_empty() {
            sections.push(input);
        }
    }
    sections.extend(content_md.iter().filter(|text| !text.is_empty()).cloned());
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

/// One-line preview of [`tool_call_arg_value`] for the tool header's sub-row.
/// Truncates to ~240 chars (ellipsis suffix on overflow) so even a multi-line
/// bash invocation stays glanceable; the row is clickable and the modal behind
/// it carries the untruncated text.
pub(crate) fn tool_call_arg_preview(raw_input: &serde_json::Value) -> Option<String> {
    // On its own sub-row under the tool header (`render_tool_call`),
    // `.truncate()` on the Label clips to whatever width the container
    // has. The cap below bounds what this ALLOCATES per frame — `raw_input`
    // can carry a multi-megabyte string — rather than the layout, so leave it
    // generous enough that wide windows still show more.
    const MAX_LEN: usize = 240;
    let picked = tool_call_arg_value(raw_input)?;
    // Single-line: replace embedded newlines with `↵` so a multi-line
    // shell pipeline collapses without dropping content silently. Truncated
    // while mapping rather than after it, so a megabyte of `raw_input` never
    // becomes a megabyte of `String` on the way to a 240-char label.
    let mut preview: String = picked
        .chars()
        .take(MAX_LEN)
        .map(|c| if c == '\n' { '↵' } else { c })
        .collect();
    if picked.chars().nth(MAX_LEN).is_some() {
        preview.push('…');
    }
    Some(preview)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_tool_call(
    entry_idx: usize,
    tool_call_id: &str,
    label_text: &str,
    status: &ToolStatus,
    content_md: &[String],
    raw_input: Option<&serde_json::Value>,
    status_started_at: Option<i64>,
    markdown_for: &HashMap<(usize, usize), Entity<Markdown>>,
    style: &MarkdownStyle,
    thread: gpui::WeakEntity<AcpThread>,
    workspace: &gpui::WeakEntity<workspace::Workspace>,
    cx: &App,
) -> AnyElement {
    let status_text = tool_status_text(status);
    let status_color = match status {
        ToolStatus::Failed => Color::Error,
        ToolStatus::Rejected | ToolStatus::Canceled => Color::Warning,
        ToolStatus::Completed => Color::Success,
        _ => Color::Muted,
    };

    // Elapsed-time badge: shown only while the tool is actively running
    // so the user can tell a 30-second hang apart from a freshly-started
    // call. Terminal statuses skip the badge — we keep the timestamp on
    // the entity (see acp_thread::ToolCall::status_started_at) but
    // rendering "ran for Xs" on done/failed/canceled calls is a
    // deliberate follow-up, not part of the live-counter surface.
    let elapsed_label = if matches!(status, ToolStatus::InProgress) {
        status_started_at.map(|started_ms| {
            let elapsed_secs =
                ((chrono::Utc::now().timestamp_millis() - started_ms) / 1000).max(0) as u64;
            crate::status_row::format_elapsed(elapsed_secs)
        })
    } else {
        None
    };

    // Pull a one-line preview of the most informative input arg —
    // command for Bash, file_path for Read/Edit, pattern for Grep, etc.
    // Without this the user can't tell which file a Read targeted,
    // which pattern a Grep searched for, or which command a Bash
    // actually ran — only the output is shown, which is often
    // ambiguous (a green `cargo check` and a green `cargo build` look
    // identical post-hoc).
    let arg_preview = raw_input.and_then(tool_call_arg_preview);
    // The same value untruncated, for the modal the preview row opens. A long
    // heredoc or a 300-char pipeline is unreadable on one clipped line, and
    // nothing else in the UI shows what a tool actually ran.
    // Bounded, because this allocates on EVERY render of every visible tool
    // call whether or not the modal is ever opened, and `raw_input` can carry a
    // multi-megabyte string. The cap is far above anything a person reads in a
    // modal and far below a per-frame memcpy that matters.
    const MAX_MODAL_LEN: usize = 64 * 1024;
    let approval_full = matches!(status, ToolStatus::WaitingForConfirmation)
        .then(|| approval_details(raw_input, content_md))
        .flatten()
        .map(SharedString::from);
    let arg_full = approval_full.clone().or_else(|| {
        raw_input.and_then(tool_call_arg_value).map(|full| {
            match full.char_indices().nth(MAX_MODAL_LEN) {
                Some((cut, _)) => SharedString::from(format!(
                    "{}\n\n[…truncated by the editor at {MAX_MODAL_LEN} characters]",
                    &full[..cut]
                )),
                None => SharedString::new(full),
            }
        })
    });
    let arg_modal_title =
        SharedString::from(crate::session_entry::single_line_tool_label(label_text));

    let mut container = v_flex()
        .gap_0p5()
        .my_1()
        .pl_2()
        .border_l_2()
        .border_color(cx.theme().colors().border_variant)
        .child(
            h_flex()
                .gap_1p5()
                .items_center()
                .child(
                    // Only the title itself is the click target — not the row.
                    // The row is full width, so making IT the button lit up the
                    // whole line on hover and swallowed clicks aimed at nothing
                    // in particular; the preview row below is worse still,
                    // since it carries the command people reach for with the
                    // mouse to read and select. `w_flex` is deliberately
                    // absent: this group sizes to its content, so the hit area
                    // ends where `Tool: Bash` ends.
                    //
                    // `MarkdownElement` does not stop mouse propagation, so a
                    // click on the rendered label reaches this group's handler.
                    h_flex()
                        .id(("tool-header", entry_idx))
                        .flex_none()
                        .gap_1p5()
                        .items_center()
                        .when_some(arg_full, |this, full| {
                            this.cursor_pointer()
                                .tooltip(ui::Tooltip::text("Show the full argument"))
                                .on_click(move |_, window, cx| {
                                    // The shared preview window, not a
                                    // workspace modal: the one argument worth
                                    // opening is the one too big for the row,
                                    // and a modal cannot be moved, cannot be
                                    // resized, and hides the conversation the
                                    // command belongs to.
                                    crate::preview_window::open_preview(
                                        crate::preview_window::PreviewContent::Text {
                                            title: arg_modal_title.clone(),
                                            body: full.clone(),
                                        },
                                        window,
                                        cx,
                                    );
                                })
                        })
                        .child(
                            Icon::new(IconName::ToolHammer)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(render_span(
                            (entry_idx, 0),
                            label_text,
                            markdown_for,
                            style,
                            workspace,
                        )),
                )
                .child(
                    Label::new(status_text)
                        .size(LabelSize::XSmall)
                        .color(status_color),
                )
                .when_some(elapsed_label, |this, label| {
                    this.child(
                        Label::new(SharedString::from(label))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                }),
        )
        // Preview lives on its own row under the header, and is deliberately
        // NOT clickable — see the header above.
        .when_some(arg_preview, |this, preview| {
            this.child(
                div().pl_4().child(
                    Label::new(SharedString::from(preview))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                ),
            )
        })
        .when_some(approval_full, |this, full| {
            this.child(
                h_flex().child(
                    Button::new(("approval-details", entry_idx), "View full request")
                        .style(ButtonStyle::Outlined)
                        .on_click(move |_, window, cx| {
                            crate::preview_window::open_preview(
                                crate::preview_window::PreviewContent::Text {
                                    title: "Approval details".into(),
                                    body: full.clone(),
                                },
                                window,
                                cx,
                            );
                        }),
                ),
            )
        });

    let mut span_idx = 1;
    for summary in content_md {
        if !summary.is_empty() {
            container = container.child(div().child(render_span(
                (entry_idx, span_idx),
                summary,
                markdown_for,
                style,
                workspace,
            )));
            span_idx += 1;
        }
    }

    // Authorization affordance: when the agent is blocked waiting for the
    // user to allow/deny this tool call, render its options as buttons.
    // Clicking one calls `AcpThread::authorize_tool_call`, which fulfills
    // the `respond_tx` oneshot the connection is awaiting and unblocks the
    // turn. The buttons disappear on the next render once the status moves
    // off `WaitingForConfirmation`.
    //
    // The owned `SessionEntry` only carries the `WaitingForConfirmation`
    // MARKER — not the live `PermissionOptions` or the respond channel
    // (those are not serializable and never enter `SessionEntry`). An
    // in-flight call only exists while the live thread does, so for such an
    // entry we look the live `ToolCall` up by id in the thread the view
    // still holds, and render the buttons against ITS options + ITS
    // `acp::ToolCallId` (so `authorize_tool_call` fulfils the right
    // oneshot). Phase 4/5 adds a side-map for the mobile wire.
    let live_authorization = if matches!(status, ToolStatus::WaitingForConfirmation) {
        thread.upgrade().and_then(|thread| {
            thread.read(cx).entries().iter().find_map(|entry| {
                let AgentThreadEntry::ToolCall(call) = entry else {
                    return None;
                };
                if call.id.0.as_ref() != tool_call_id {
                    return None;
                }
                match &call.status {
                    ToolCallStatus::WaitingForConfirmation { options, .. } => {
                        Some((call.id.clone(), permission_buttons(options)))
                    }
                    _ => None,
                }
            })
        })
    } else {
        None
    };
    if let Some((live_tool_call_id, buttons)) = live_authorization {
        if !buttons.is_empty() {
            let tool_call_id = live_tool_call_id;
            let mut row = h_flex().gap_2().mt_2().flex_wrap();
            for (button_idx, button) in buttons.into_iter().enumerate() {
                let style = if button.is_allow() {
                    ButtonStyle::Tinted(ui::TintColor::Accent)
                } else {
                    ButtonStyle::Outlined
                };
                let label_color = Color::Default;
                let thread = thread.clone();
                let tool_call_id = tool_call_id.clone();
                // Composite id: a named-integer per entry, with the
                // button index nested as a child. Collision-proof
                // regardless of how many buttons a tool exposes (the old
                // `entry_idx * 1000 + button_idx` collided at ≥1000
                // buttons).
                let button_id = ElementId::NamedChild(
                    std::sync::Arc::new(ElementId::named_usize("tool-auth", entry_idx)),
                    button_idx.to_string().into(),
                );
                row = row.child(
                    Button::new(button_id, button.label.clone())
                        .style(style)
                        .size(ButtonSize::Large)
                        .label_size(LabelSize::Default)
                        .color(label_color)
                        .on_click(move |_, window, cx| {
                            let outcome = button.outcome();
                            let tool_call_id = tool_call_id.clone();
                            thread
                                .update(cx, move |thread, cx| {
                                    thread.authorize_tool_call(tool_call_id, outcome, cx);
                                })
                                .log_err();
                            // Drop the answered controls on the next frame,
                            // even before the provider sends another update.
                            window.refresh();
                        }),
                );
            }
            container = container.child(row);
        }
    }

    container.into_any_element()
}

/// Produces the markdown source for one item of a tool call's `content`.
/// Shared by `entry_text_spans` (the find-bar / markdown-cache pre-pass)
/// and `render_tool_call` so they always agree — historically they
/// diverged and the cache won, sticking the placeholder text on screen
/// even after the real output arrived in `raw_output`. Special-cases
/// `Terminal` blocks: when the inner terminal has no bytes (claude-acp
/// often skips meta.terminal_output for short/synchronous commands)
/// falls back to the call's `raw_output` field, which is where the
/// captured stdout typically ends up in those cases.
pub(crate) fn tool_call_content_summary(
    call: &ToolCall,
    content: &ToolCallContent,
    cx: &App,
) -> String {
    let raw = match content {
        // Tool output via `ContentBlock` is plain text the agent emitted
        // (grep matches, file reads, ls listings — anything not Diff and
        // not Terminal). claude-acp ships those as `ContentBlock::Text`
        // with single `\n`s between rows, which CommonMark renders as
        // soft breaks — i.e. all the rows get joined into one paragraph
        // and the user loses the line structure. Wrap in a fence longer
        // than any backtick run in the output so the markdown widget
        // paints it monospaced + line-preserving.
        ToolCallContent::ContentBlock(block) => fence_plain_text(&content_block_text(block, cx)),
        ToolCallContent::Diff(diff) => diff_summary_markdown(diff, cx),
        ToolCallContent::Terminal(terminal) => {
            let primary = terminal_output_markdown(terminal, cx);
            if primary.contains("(no output yet)") {
                raw_output_fallback_markdown(call.raw_output.as_ref()).unwrap_or(primary)
            } else {
                primary
            }
        }
    };
    truncate_tool_summary(&raw)
}

/// A top-level CommonMark fence. Shorter/different markers inside a block
/// are content; backtick info strings cannot contain backticks.
#[derive(Clone, Copy)]
struct CodeFence {
    marker: char,
    width: usize,
}

impl CodeFence {
    fn parse(line: &str) -> Option<(Self, &str)> {
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() > 3 {
            return None;
        }
        let marker = trimmed.chars().next()?;
        if !matches!(marker, '`' | '~') {
            return None;
        }
        let width = trimmed.chars().take_while(|ch| *ch == marker).count();
        (width >= 3).then(|| (Self { marker, width }, &trimmed[width..]))
    }

    fn opening(line: &str) -> Option<Self> {
        let (fence, suffix) = Self::parse(line)?;
        (fence.marker != '`' || !suffix.contains('`')).then_some(fence)
    }

    fn closes(self, line: &str) -> bool {
        Self::parse(line).is_some_and(|(fence, suffix)| {
            fence.marker == self.marker
                && fence.width >= self.width
                && suffix.trim_matches([' ', '\t']).is_empty()
        })
    }
}

fn is_single_fenced_block(text: &str) -> bool {
    let is_blank = |line: &str| line.trim_matches([' ', '\t']).is_empty();
    let mut lines = text.lines().skip_while(|line| is_blank(line));
    let Some(fence) = lines.next().and_then(CodeFence::opening) else {
        return false;
    };
    for line in lines.by_ref() {
        if fence.closes(line) {
            return lines.all(is_blank);
        }
    }
    false
}

/// Wrap plain-text tool output in a collision-free fence so CommonMark
/// preserves newlines instead of joining them as soft breaks. No-op for
/// empty strings and one complete pre-fenced code block. A leading fence
/// followed by prose is raw output too: leaving it alone leaks that prose
/// into Markdown headings, links, and other formatting.
fn fence_plain_text(text: &str) -> String {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return text.to_string();
    }
    if is_single_fenced_block(text) {
        return text.to_string();
    }
    MarkdownCodeBlock {
        tag: "",
        text: trimmed,
    }
    .to_string()
}

/// Trims tool-call output for the inline chat view — long Read / Bash /
/// Diff results would otherwise push the rest of the conversation off the
/// screen on every turn. Caps at `MAX_LINES` and appends a `… (+N more
/// lines)` hint matching the claude-code CLI convention. The full content
/// is still available via the original tool / file in the editor; this is
/// just the chat-side preview.
pub(crate) fn truncate_tool_summary(text: &str) -> String {
    const MAX_LINES: usize = 15;
    let mut lines = text.lines();
    let head: Vec<&str> = lines.by_ref().take(MAX_LINES).collect();
    let remaining = lines.count();
    if remaining == 0 {
        return text.to_string();
    }
    // Nested/shorter fences are literal code, not toggles. Only a matching
    // marker at least as long as the opener, with no info string, closes it.
    let mut open_fence: Option<CodeFence> = None;
    for line in &head {
        match open_fence {
            Some(fence) if fence.closes(line) => open_fence = None,
            None => open_fence = CodeFence::opening(line),
            _ => {}
        }
    }
    let mut out = head.join("\n");
    if let Some(fence) = open_fence {
        out.push('\n');
        out.extend(std::iter::repeat_n(fence.marker, fence.width));
    }
    out.push_str(&format!("\n\n_… (+{remaining} more lines)_"));
    out
}

/// Try to coerce a tool call's `raw_output` JSON into something printable
/// in the chat. Strings get returned as-is, objects/arrays land as a JSON
/// code block. Returns None when there's nothing usable (Null / empty
/// string / empty object) so the caller can fall through to its own
/// placeholder.
pub(crate) fn raw_output_fallback_markdown(raw: Option<&serde_json::Value>) -> Option<String> {
    let raw = raw?;
    match raw {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => {
            let trimmed = s.trim_end();
            if trimmed.is_empty() {
                return None;
            }
            Some(
                MarkdownCodeBlock {
                    tag: "",
                    text: trimmed,
                }
                .to_string(),
            )
        }
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        other => {
            let pretty = serde_json::to_string_pretty(other).ok()?;
            if pretty.trim().is_empty() || pretty.trim() == "{}" || pretty.trim() == "[]" {
                return None;
            }
            Some(
                MarkdownCodeBlock {
                    tag: "json",
                    text: &pretty,
                }
                .to_string(),
            )
        }
    }
}

/// Format a `Diff` tool-call content as a `diff`-fenced markdown block —
/// the markdown widget syntax-highlights `+` lines green and `-` lines
/// red, matching the inline-diff style claude-code shows in the CLI.
/// Includes a one-line "Edited <path>" header + `Δ +X / -Y` summary so
/// the cost of the change is visible without expanding. The full diff is
/// still passed through `truncate_tool_summary`, capping the body at the
/// same line limit as other tool output.
pub(crate) fn diff_summary_markdown(diff: &Entity<acp_thread::Diff>, cx: &App) -> String {
    let diff = diff.read(cx);
    let path = diff.file_path(cx).unwrap_or_else(|| "file".to_string());
    let old_text = diff.base_text().to_string();
    let new_text = diff.buffer().read(cx).text();
    let body = language::unified_diff(&old_text, &new_text);
    if body.is_empty() {
        return format!("**Edited** `{path}`");
    }
    let added = body.lines().filter(|l| l.starts_with('+')).count();
    let removed = body.lines().filter(|l| l.starts_with('-')).count();
    format!(
        "**Edited** `{path}` · +{added} / −{removed}\n{}",
        MarkdownCodeBlock {
            tag: "diff",
            text: &body
        }
    )
}

/// Render `Terminal` tool-call content as fenced code in markdown so the
/// existing markdown widget paints it monospaced (matches how command
/// labels are already rendered above the output). For an empty / still-
/// starting terminal returns a hint placeholder so the user sees the
/// command body has not produced bytes yet, instead of a blank gap.
/// Truncates to keep the markdown parser snappy on long outputs — tighter
/// than the agent-side byte limit on purpose; the user reads "the gist",
/// not the full stream, in this inline view.
pub(crate) fn terminal_output_markdown(
    terminal: &Entity<acp_thread::Terminal>,
    cx: &App,
) -> String {
    const MAX_BYTES: usize = 8 * 1024;
    let term = terminal.read(cx);
    let mut content = if let Some(output) = term.output() {
        output.content.clone()
    } else {
        term.inner().read(cx).get_content()
    };
    let was_truncated = content.len() > MAX_BYTES;
    if was_truncated {
        let mut cut = MAX_BYTES;
        while cut > 0 && !content.is_char_boundary(cut) {
            cut -= 1;
        }
        content.truncate(cut);
    }
    let trimmed = content.trim_end();
    if trimmed.is_empty() {
        return "_(no output yet)_".to_string();
    }
    let mut out = MarkdownCodeBlock {
        tag: "",
        text: trimmed,
    }
    .to_string();
    if was_truncated {
        out.push_str("\n_(output truncated)_");
    }
    out
}

pub(crate) fn render_plan(
    entry_idx: usize,
    items: &[crate::session_entry::PlanItem],
    markdown_for: &HashMap<(usize, usize), Entity<Markdown>>,
    style: &MarkdownStyle,
    workspace: &gpui::WeakEntity<workspace::Workspace>,
    cx: &App,
) -> AnyElement {
    let mut container = v_flex()
        .gap_0p5()
        .my_1()
        .pl_2()
        .border_l_2()
        .border_color(cx.theme().colors().border_variant)
        .child(
            h_flex()
                .gap_1p5()
                .items_center()
                .child(
                    Icon::new(IconName::ListTree)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(render_span(
                    (entry_idx, 0),
                    "Plan",
                    markdown_for,
                    style,
                    workspace,
                )),
        );
    for (i, _item) in items.iter().enumerate() {
        let span_idx = 1 + i;
        // Bullet prefix is now part of the span text (see
        // entry_text_spans), so the rendered markdown already includes
        // it — list items render as a list line.
        container = container.child(render_span(
            (entry_idx, span_idx),
            "",
            markdown_for,
            style,
            workspace,
        ));
    }
    container.into_any_element()
}

pub(crate) fn content_block_text(block: &ContentBlock, cx: &App) -> String {
    block.to_markdown(cx).to_string()
}

#[cfg(test)]
mod fence_tests {
    use super::*;
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    fn assert_single_code_preview(markdown: &str, expected_code: &str, truncated: bool) {
        let mut code = String::new();
        let mut outside = String::new();
        let mut in_code = false;
        let mut blocks = 0;
        for event in Parser::new(markdown) {
            match event {
                Event::Start(Tag::CodeBlock(_)) => {
                    blocks += 1;
                    in_code = true;
                }
                Event::End(TagEnd::CodeBlock) => in_code = false,
                Event::Start(Tag::Heading { .. }) => panic!("output became a heading"),
                Event::Text(text) if in_code => code.push_str(&text),
                Event::Text(text) => outside.push_str(&text),
                _ => {}
            }
        }
        assert_eq!(blocks, 1, "{markdown}");
        assert_eq!(code, expected_code, "{markdown}");
        if truncated {
            assert!(
                outside.starts_with("… (+"),
                "hint must be outside code: {markdown}"
            );
        } else {
            assert!(outside.is_empty(), "output escaped code: {markdown}");
        }
    }

    #[test]
    fn nested_fences_in_shell_output_preserve_makefile_comments() {
        // A shell command can print another session's already-fenced transcript
        // followed by Makefile comments. Four fixed backticks were insufficient.
        for width in [3, 4, 5, 12] {
            let inner = "`".repeat(width);
            let output = format!(
                "session entries:\n{inner}\ninner output\n{inner}\n# Builds a temp file\n## Keeps the old inode\n"
            );
            assert_single_code_preview(&fence_plain_text(&output), &output, false);
            let raw = serde_json::Value::String(output.clone());
            assert_single_code_preview(
                &raw_output_fallback_markdown(Some(&raw)).unwrap(),
                &output,
                false,
            );
        }
    }

    #[test]
    fn leading_fence_does_not_allow_trailing_prose_to_escape() {
        for output in [
            "```sh\necho hello\n```\n# Builds a temp file\n",
            "\n  ~~~sh\necho hello\n  ~~~~~\n# Builds a temp file\n",
            "```sh\necho hello\n```\n```\nsecond block\n```\n",
            "```sh`invalid\n# Builds a temp file\n```\n",
            "    ```sh\n# Builds a temp file\n    ```\n",
            "```sh\n# An unclosed block\n",
        ] {
            assert_single_code_preview(&fence_plain_text(output), output, false);
        }
    }

    #[test]
    fn complete_prefenced_block_is_preserved() {
        for source in [
            "```sh\necho hello\n```",
            "\n \t\n   ```sh\necho hello\n   ````` \t\n\n",
            "~~~sh\necho hello\n~~~~~",
            "````sh\n```\n# Still code\n`````",
        ] {
            assert_eq!(fence_plain_text(source), source);
        }
    }

    #[test]
    fn truncation_closes_outer_fence_and_keeps_hint_outside_code() {
        for width in [3, 4, 5, 12] {
            let inner = "`".repeat(width);
            let output = format!(
                "session entries:\n{inner}\n# Literal heading\n{}",
                "line\n".repeat(30)
            );
            let fenced = fence_plain_text(&output);
            let expected = output.lines().take(14).collect::<Vec<_>>().join("\n") + "\n";
            assert_single_code_preview(&truncate_tool_summary(&fenced), &expected, true);
        }
    }

    #[test]
    fn truncation_ignores_short_fences_and_closing_fences_with_info_strings() {
        for marker in ['`', '~'] {
            let fence = marker.to_string().repeat(6);
            let text = format!(
                "{fence}text\n{}\n{fence}still code\n{}",
                marker.to_string().repeat(3),
                "# Literal heading\n".repeat(30)
            );
            let expected = text.lines().skip(1).take(14).collect::<Vec<_>>().join("\n") + "\n";
            assert_single_code_preview(&truncate_tool_summary(&text), &expected, true);
        }
    }
}
