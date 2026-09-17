//! Tool-call render cluster for the Solution conversation view.
//!
//! Relocated verbatim from `conversation_render.rs` (Tier-1 god-object split).

use super::*;

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
    let arg_full = raw_input.and_then(tool_call_arg_value).map(|full| {
        match full.char_indices().nth(MAX_MODAL_LEN) {
            Some((cut, _)) => SharedString::from(format!(
                "{}\n\n[…truncated by the editor at {MAX_MODAL_LEN} characters]",
                &full[..cut]
            )),
            None => SharedString::new(full),
        }
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
                        .child(render_span((entry_idx, 0), label_text, markdown_for, style)),
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
        });

    let mut span_idx = 1;
    for summary in content_md {
        if !summary.is_empty() {
            container = container.child(div().child(render_span(
                (entry_idx, span_idx),
                summary,
                markdown_for,
                style,
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
                    ButtonStyle::Filled
                } else {
                    ButtonStyle::Subtle
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
        // and the user loses the line structure. Wrap in a 4-backtick
        // fence (same trick `terminal_output_markdown` and
        // `raw_output_fallback_markdown` use) so the markdown widget
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

/// Wrap plain-text tool output in a 4-backtick fence so CommonMark
/// preserves newlines instead of joining them as soft breaks. No-op for
/// empty strings and for text that already opens with a code fence (the
/// agent occasionally returns pre-fenced markdown for table-like tools).
fn fence_plain_text(text: &str) -> String {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return text.to_string();
    }
    let already_fenced = trimmed
        .lines()
        .next()
        .map(|line| line.trim_start().starts_with("```"))
        .unwrap_or(false);
    if already_fenced {
        return text.to_string();
    }
    format!("````\n{trimmed}\n````")
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
    // Preserve the closing fence if the truncated output started one,
    // otherwise the markdown widget would parse the rest of the message
    // as a runaway code block.
    let opens_fence = head
        .iter()
        .filter(|line| line.starts_with("```") || line.starts_with("````"))
        .count()
        % 2
        == 1;
    let mut out = head.join("\n");
    if opens_fence {
        // Match whichever fence width opened (prefer 4 to be safe).
        let fence = if head
            .iter()
            .any(|line| line.trim_start().starts_with("````"))
        {
            "````"
        } else {
            "```"
        };
        out.push('\n');
        out.push_str(fence);
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
            // 4-backtick fence so embedded triple-backticks in the
            // captured stdout don't break the markdown widget. Same
            // trick `terminal_output_markdown` uses.
            Some(format!("````\n{trimmed}\n````"))
        }
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        other => {
            let pretty = serde_json::to_string_pretty(other).ok()?;
            if pretty.trim().is_empty() || pretty.trim() == "{}" || pretty.trim() == "[]" {
                return None;
            }
            Some(format!("```json\n{pretty}\n```"))
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
    format!("**Edited** `{path}` · +{added} / −{removed}\n```diff\n{body}\n```")
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
    // 4-backtick fence so an embedded ```…``` in the captured output (e.g.
    // an agent that ran `cat README.md`) does not close our fence early.
    let mut out = String::with_capacity(trimmed.len() + 16);
    out.push_str("````\n");
    out.push_str(trimmed);
    if !trimmed.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("````");
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
                .child(render_span((entry_idx, 0), "Plan", markdown_for, style)),
        );
    for (i, _item) in items.iter().enumerate() {
        let span_idx = 1 + i;
        // Bullet prefix is now part of the span text (see
        // entry_text_spans), so the rendered markdown already includes
        // it — list items render as a list line.
        container = container.child(render_span((entry_idx, span_idx), "", markdown_for, style));
    }
    container.into_any_element()
}

pub(crate) fn content_block_text(block: &ContentBlock, cx: &App) -> String {
    block.to_markdown(cx).to_string()
}
