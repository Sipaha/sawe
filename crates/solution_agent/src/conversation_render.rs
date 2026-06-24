//! Pure rendering helpers and shared types for the Solution session conversation view. Extracted from session_view.rs to keep that file focused on view state + input handling.

use std::collections::HashMap;
use std::ops::Range;

use acp_thread::{
    AcpThread, AgentThreadEntry, AssistantMessage, AssistantMessageChunk, ContentBlock,
    PermissionOptions, PlanEntry, SelectedPermissionOutcome, SelectedPermissionParams, ToolCall,
    ToolCallContent, ToolCallStatus, UserMessage, UserMessageId,
};
use agent_client_protocol::schema as acp;
use base64::Engine;
use chrono::TimeZone as _;
use gpui::{
    Anchor, AnyElement, App, Context, DismissEvent, ElementId, Empty, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement as _, Styled, Window, div, px, relative, rems,
};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use ui::prelude::*;
use ui::{
    Button, ButtonStyle, Color, ContextMenu, CopyButton, Icon, IconName, IconSize, Label,
    LabelSize, PopoverMenu,
};
use util::ResultExt as _;

#[derive(Clone, Debug)]
pub(crate) struct FindMatch {
    pub(crate) entry_idx: usize,
    pub(crate) span_idx: usize,
    pub(crate) range: Range<usize>,
}

/// Pure backward-walk that computes, for each entry index, the id of the
/// next user message *after* it (the rewind target). Caller pre-projects
/// the entries list to `Option<UserMessageId>` per slot — `Some(id)` for a
/// user message that carries an id, `None` for everything else (assistant,
/// tool, plan, or a user message without an id).
///
/// At index `i` the result holds:
///   - `None` if `user_ids[i].is_some()` — rewinding TO a user message
///     means truncating its earlier turn; the message itself is never its
///     own rewind target. Also `None` past the last user message in the
///     conversation (no downstream user message exists).
///   - `Some(id)` of the next downstream user message otherwise.
///
/// O(N) once per thread mutation, replacing the previous O(N²) per-render
/// forward scan that lived inside the conversation render loop.
pub(crate) fn compute_rewind_table(
    user_ids: &[Option<UserMessageId>],
) -> Vec<Option<UserMessageId>> {
    let mut table = vec![None; user_ids.len()];
    let mut current: Option<UserMessageId> = None;
    for idx in (0..user_ids.len()).rev() {
        if let Some(id) = &user_ids[idx] {
            current = Some(id.clone());
            continue;
        }
        table[idx] = current.clone();
    }
    table
}

/// Per-entry text spans used by the find bar.
///
/// MUST iterate the entry in the same order as `render_*` functions emit
/// labels, so `(entry_idx, span_idx)` produced by `recompute_matches` lines
/// up with the label rendered for that span. If you add or reorder labels
/// in a render function, mirror the change here or matches will be applied
/// to the wrong line.
pub(crate) fn entry_text_spans(entry: &AgentThreadEntry, cx: &App) -> Vec<String> {
    match entry {
        AgentThreadEntry::UserMessage(message) => vec![clean_user_message_text(
            &content_block_text(&message.content, cx),
        )],
        AgentThreadEntry::AssistantMessage(message) => {
            let has_message = message
                .chunks
                .iter()
                .any(|c| matches!(c, AssistantMessageChunk::Message { .. }));
            if has_message {
                // Coalesce every visible `Message` chunk into ONE span so a
                // single assistant turn matches/renders as one continuous
                // message rather than N stacked blocks. Distinct chunks are
                // separate text ContentBlocks the model emitted in the same
                // turn (text, then more text, before any tool call) — joining
                // with a blank line preserves paragraph breaks without the
                // inter-widget gap. Thoughts are dropped once a real answer
                // exists (mirrors `render_assistant_message`).
                let mut combined = String::new();
                for chunk in &message.chunks {
                    if let AssistantMessageChunk::Message { block } = chunk {
                        let text = content_block_text(block, cx);
                        if text.is_empty() {
                            continue;
                        }
                        if !combined.is_empty() {
                            combined.push_str("\n\n");
                        }
                        combined.push_str(&text);
                    }
                }
                if combined.is_empty() {
                    Vec::new()
                } else {
                    vec![combined]
                }
            } else {
                // Thought-only (mid-turn reasoning before the answer streams):
                // keep one span per thought, each rendered under its own
                // "thinking…" label.
                let mut spans = Vec::new();
                for chunk in &message.chunks {
                    if let AssistantMessageChunk::Thought { block } = chunk {
                        let text = content_block_text(block, cx);
                        if !text.is_empty() {
                            spans.push(format!("thinking: {text}"));
                        }
                    }
                }
                spans
            }
        }
        AgentThreadEntry::ToolCall(call) => {
            let label_text = call.label.read(cx).source().to_string();
            let status_text = tool_call_status_text(&call.status);
            let mut spans = vec![format!("Tool: {label_text} ({status_text})")];
            for content in &call.content {
                let summary = tool_call_content_summary(call, content, cx);
                if !summary.is_empty() {
                    spans.push(summary);
                }
            }
            spans
        }
        AgentThreadEntry::CompletedPlan(entries) => {
            let mut spans = vec!["Plan".to_string()];
            for entry in entries {
                let source = entry.content.read(cx).source().to_string();
                spans.push(format!("• {source}"));
            }
            spans
        }
        AgentThreadEntry::ContextCompaction(compaction) => match &compaction.summary {
            Some(summary) => vec![format!(
                "Context compaction: {}",
                summary.read(cx).source()
            )],
            None => vec!["Context compaction".to_string()],
        },
    }
}

/// Find every (case-insensitive) occurrence of `query_lower` in `text`.
/// Caller pre-lowercases the query — we lowercase `text` here so matches
/// are case-insensitive without modifying the caller's data. Range returned
/// is in BYTE offsets into the *original* `text` (lowercase preserves
/// length only for ASCII; for non-ASCII fold this is approximate but
/// fine for v1 — Latin/Cyrillic typical case-insensitive use works).
pub(crate) fn find_all(text: &str, query_lower: &str, mut emit: impl FnMut(Range<usize>)) {
    if query_lower.is_empty() {
        return;
    }
    let haystack = text.to_lowercase();
    let mut start = 0;
    while let Some(rel) = haystack[start..].find(query_lower) {
        let abs = start + rel;
        emit(abs..abs + query_lower.len());
        start = abs + query_lower.len();
    }
}

pub(crate) fn tool_call_status_text(status: &ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::WaitingForConfirmation { .. } => "waiting for confirmation",
        ToolCallStatus::InProgress => "running",
        ToolCallStatus::Completed => "done",
        ToolCallStatus::Failed => "failed",
        ToolCallStatus::Rejected => "rejected",
        ToolCallStatus::Canceled => "canceled",
    }
}

/// A single clickable authorization choice flattened out of the
/// `PermissionOptions` the agent attached to a `WaitingForConfirmation`
/// tool call. Carries everything the render layer needs to draw a button
/// and everything the click handler needs to rebuild a
/// `SelectedPermissionOutcome` at click time (the outcome itself isn't
/// `Clone`, so we keep the raw pieces instead).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PermissionButton {
    pub(crate) option_id: acp::PermissionOptionId,
    pub(crate) label: SharedString,
    pub(crate) kind: acp::PermissionOptionKind,
    /// Sub-patterns to attach as `SelectedPermissionParams::Terminal` when
    /// answering — only ever non-empty for `Dropdown*` choices that carry
    /// terminal command patterns.
    pub(crate) patterns: Vec<String>,
}

impl PermissionButton {
    /// True for allow-flavoured kinds — used by the renderer to pick a
    /// filled/accent button vs a subtle one.
    pub(crate) fn is_allow(&self) -> bool {
        matches!(
            self.kind,
            acp::PermissionOptionKind::AllowOnce | acp::PermissionOptionKind::AllowAlways
        )
    }

    /// Rebuild the answer to hand to `AcpThread::authorize_tool_call`.
    pub(crate) fn outcome(&self) -> SelectedPermissionOutcome {
        let params = if self.patterns.is_empty() {
            None
        } else {
            Some(SelectedPermissionParams::Terminal {
                patterns: self.patterns.clone(),
            })
        };
        SelectedPermissionOutcome::new(self.option_id.clone(), self.kind).params(params)
    }
}

/// Pick the button to use when auto-resolving a pending authorization as a
/// rejection (the queue path resolves a `WaitingForConfirmation` tool call
/// before flushing a queued message — see `queue::pending_authorization_reject`).
/// Prefers `RejectOnce`, falling back to any non-allow button. Returns `None`
/// when the options offer ONLY allow-flavoured buttons: picking one would
/// silently AUTO-APPROVE the tool call, so a stuck turn is the safer failure.
pub(crate) fn pick_reject_button(options: &PermissionOptions) -> Option<PermissionButton> {
    let buttons = permission_buttons(options);
    buttons
        .iter()
        .find(|button| button.kind == acp::PermissionOptionKind::RejectOnce)
        .or_else(|| buttons.iter().find(|button| !button.is_allow()))
        .cloned()
}

/// Flatten a `PermissionOptions` into the list of buttons to render, in
/// display order. Pure (no `cx`) so it can be unit-tested and reused by
/// the future wire layer.
///
/// v1 simplification: the `Dropdown`/`DropdownWithPatterns` variants are
/// rendered as a flat pair of buttons per choice (its allow + deny
/// `PermissionOption`), reusing the choice's `sub_patterns` so a terminal
/// "always allow these commands" choice still answers correctly. The
/// pattern-picker UI (per-pattern checkboxes from `DropdownWithPatterns`)
/// is intentionally NOT rendered here — answering a dropdown choice
/// applies all of its `sub_patterns`.
pub(crate) fn permission_buttons(options: &PermissionOptions) -> Vec<PermissionButton> {
    let from_option = |option: &acp::PermissionOption, patterns: Vec<String>| PermissionButton {
        option_id: option.option_id.clone(),
        label: SharedString::from(option.name.clone()),
        kind: option.kind,
        patterns,
    };
    match options {
        PermissionOptions::Flat(options) => options
            .iter()
            .map(|option| from_option(option, Vec::new()))
            .collect(),
        PermissionOptions::Dropdown(choices)
        | PermissionOptions::DropdownWithPatterns { choices, .. } => choices
            .iter()
            .flat_map(|choice| {
                [
                    from_option(&choice.allow, choice.sub_patterns.clone()),
                    from_option(&choice.deny, choice.sub_patterns.clone()),
                ]
            })
            .collect(),
    }
}

/// Filters `matches` down to the ones that fall in span `(entry_idx,
/// span_idx)`, preserving order, and translates the global `selected`
/// index into a span-local index (None if the active match isn't in
/// this span). Used by the search-highlight pre-pass in `Render` to
/// hand per-span ranges to each Markdown entity.
pub(crate) fn matches_for_span(
    matches: &[FindMatch],
    selected: Option<usize>,
    entry_idx: usize,
    span_idx: usize,
) -> (Vec<Range<usize>>, Option<usize>) {
    let mut ranges = Vec::new();
    let mut selected_in_span = None;
    for (i, m) in matches.iter().enumerate() {
        if m.entry_idx == entry_idx && m.span_idx == span_idx {
            if Some(i) == selected {
                selected_in_span = Some(ranges.len());
            }
            ranges.push(m.range.clone());
        }
    }
    (ranges, selected_in_span)
}

/// Render a span as either a Markdown widget (preferred — handles
/// headings, bold, lists, code blocks) or, if the entity is missing, a
/// plain Label fallback. Falls back to `Empty` when the text is empty.
pub(crate) fn render_span(
    key: (usize, usize),
    fallback_text: &str,
    markdown_for: &HashMap<(usize, usize), Entity<Markdown>>,
    style: &MarkdownStyle,
) -> AnyElement {
    if let Some(entity) = markdown_for.get(&key) {
        MarkdownElement::new(entity.clone(), style.clone()).into_any_element()
    } else if fallback_text.is_empty() {
        Empty.into_any_element()
    } else {
        Label::new(fallback_text.to_string())
            .size(LabelSize::Small)
            .into_any_element()
    }
}

pub(crate) fn render_entry(
    entry_idx: usize,
    entry: &AgentThreadEntry,
    created_ms: Option<i64>,
    is_last: bool,
    date_separator: Option<String>,
    markdown_for: &HashMap<(usize, usize), Entity<Markdown>>,
    style: &MarkdownStyle,
    assistant_label: &SharedString,
    rewind_target: Option<UserMessageId>,
    thread: gpui::WeakEntity<AcpThread>,
    cx: &App,
) -> AnyElement {
    let inner: AnyElement = match entry {
        AgentThreadEntry::UserMessage(message) => render_user_message(
            entry_idx,
            message,
            created_ms,
            is_last,
            markdown_for,
            style,
            cx,
        ),
        AgentThreadEntry::AssistantMessage(message) => render_assistant_message(
            entry_idx,
            message,
            created_ms,
            is_last,
            markdown_for,
            style,
            assistant_label,
            cx,
        ),
        AgentThreadEntry::ToolCall(call) => {
            render_tool_call(entry_idx, call, markdown_for, style, thread.clone(), cx)
        }
        AgentThreadEntry::CompletedPlan(entries) => {
            render_plan(entry_idx, entries, markdown_for, style, cx)
        }
        // Context compaction is a lightweight divider marking where the model
        // summarized its own history; render it as a muted single-line label.
        AgentThreadEntry::ContextCompaction(_) => gpui::div()
            .px_2()
            .py_1()
            .child(ui::Label::new("Context compacted").color(ui::Color::Muted))
            .into_any_element(),
    };

    // Always wrap each entry in a right-click menu. Copy / Copy-as-
    // markdown are unconditional (they pin the currently-focused
    // markdown widget so empty selection is just a no-op), and the
    // "Rewind to this point" entry only renders when the agent
    // supports truncation AND there's a downstream user message we
    // can truncate at — otherwise the body-wide menu would have been
    // the only Copy affordance, but that wrapper breaks the list's
    // flex layout so we host the menu per-entry instead.
    let inner_cell = std::cell::RefCell::new(Some(inner));
    let body = ui::right_click_menu(("session-entry-menu", entry_idx))
        .trigger(move |_, _, _| {
            inner_cell
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Empty.into_any_element())
        })
        .menu(move |window, cx| {
            let rewind_target = rewind_target.clone();
            let thread = thread.clone();
            // Pin the currently-focused element (typically the Markdown
            // widget the user just clicked into to drag a selection)
            // so Copy / Copy-as-markdown land on it. Without this the
            // entry-scoped menu would silently swallow the actions.
            let focus = window.focused(cx);
            ContextMenu::build(window, cx, move |mut menu, _, _| {
                if let Some(target_id) = rewind_target {
                    menu = menu
                        .entry("Rewind to this point", None, {
                            let thread = thread.clone();
                            move |_window, cx| {
                                let target_id = target_id.clone();
                                if let Some(thread) = thread.upgrade() {
                                    thread.update(cx, |thread: &mut AcpThread, cx| {
                                        thread.rewind(target_id, cx).detach_and_log_err(cx);
                                    });
                                }
                            }
                        })
                        .separator();
                }
                menu.when_some(focus, |menu, focus| menu.context(focus))
                    .action("Copy", Box::new(markdown::Copy))
                    .action("Copy as markdown", Box::new(markdown::CopyAsMarkdown))
            })
        });

    // The separator (when present) renders ABOVE the bubble as a child of
    // the same list item, keeping the list's idx↔entry mapping 1:1.
    // `w_full` is essential: without it this wrapper hugs its content, which
    // collapses the inner bubble row's `w_full`/right-alignment and shrinks
    // every bubble.
    v_flex()
        .w_full()
        .when_some(date_separator, |this, label| {
            this.child(
                h_flex().w_full().my_1().justify_center().child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
        })
        .child(body)
        .into_any_element()
}

/// Plain-text preview of a queued follow-up, used by the "ghost"
/// bubble we draw while the message is sitting in `pending_messages`.
/// Concatenates text blocks (preserving the `\n\n` separators that
/// `send_message_blocks` injects when merging queued submits) and
/// substitutes `[image #N]` placeholders for image blocks.
///
/// Strips agent-only injected metadata (hint line and per-segment
/// `[HH:MM:SS] ` timestamp prefixes) from the assembled output so the
/// ghost shows only what the user typed.
pub(crate) fn pending_blocks_preview(blocks: &[acp::ContentBlock], _cx: &App) -> String {
    // A desktop-pasted image already carries its OWN `[image #N]` paste-
    // placeholder in the text block. Synthesizing another `[image #N]` per
    // Image block on top of that rendered each attachment TWICE — e.g.
    // "image #3 image #1", and the synthesized second link was dead (it
    // pointed past the end of the decoded-image list). So we only synthesize a
    // placeholder for images BEYOND the count already labelled in the text:
    // the common desktop case has one paste-placeholder per image (so nothing
    // is synthesized and nothing doubles), a mobile/MCP "text + unlabelled
    // attachments" bundle has zero (so every image gets one), and a mismatch
    // (fewer placeholders than images) still labels the surplus. Keeps the
    // `[image #N]` shape the wire + mobile queued-bubble renderer expect.
    let labelled_in_text: usize = blocks
        .iter()
        .map(|block| match block {
            acp::ContentBlock::Text(t) => IMAGE_PLACEHOLDER_RE.find_iter(&t.text).count(),
            _ => 0,
        })
        .sum();
    let mut out = String::new();
    let mut image_ordinal = 0usize;
    for block in blocks {
        match block {
            acp::ContentBlock::Text(t) => {
                out.push_str(&t.text);
            }
            acp::ContentBlock::Image(_) => {
                if image_ordinal >= labelled_in_text {
                    out.push_str(&format!("[image #{}]", image_ordinal + 1));
                }
                image_ordinal += 1;
            }
            _ => {}
        }
    }
    strip_injected_meta(out.trim())
}

/// Remove the agent-only metadata the queue bakes in — the optional leading
/// hint line ([`crate::store::QUEUE_HINT_LINE`]) and the per-segment
/// `[HH:MM:SS] ` timestamps ([`crate::store::queue_timestamp_prefix`]) — so the
/// UI shows only the user's own text. Returns an owned String because segment
/// stripping is not a simple prefix slice.
pub(crate) fn strip_injected_meta(text: &str) -> String {
    let body = text
        .strip_prefix(crate::store::QUEUE_HINT_LINE)
        .map(|rest| rest.trim_start_matches('\n'))
        .unwrap_or(text);
    let mut out = String::with_capacity(body.len());
    for (i, segment) in body.split("\n\n").enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        out.push_str(strip_one_timestamp(segment));
    }
    out
}

/// Drop a single leading `[HH:MM:SS] ` prefix from one segment, if present.
fn strip_one_timestamp(segment: &str) -> &str {
    let Some(rest) = segment.strip_prefix(crate::store::TS_PREFIX_OPEN) else {
        return segment;
    };
    let Some(close) = rest.find(crate::store::TS_PREFIX_CLOSE) else {
        return segment;
    };
    let stamp = &rest[..close];
    let is_hms = stamp.len() == 8
        && stamp.as_bytes()[2] == b':'
        && stamp.as_bytes()[5] == b':'
        && stamp.bytes().enumerate().all(|(i, b)| i == 2 || i == 5 || b.is_ascii_digit());
    if is_hms {
        &rest[close + crate::store::TS_PREFIX_CLOSE.len()..]
    } else {
        segment
    }
}

pub(crate) fn render_user_message(
    entry_idx: usize,
    message: &UserMessage,
    created_ms: Option<i64>,
    is_last: bool,
    markdown_for: &HashMap<(usize, usize), Entity<Markdown>>,
    style: &MarkdownStyle,
    cx: &App,
) -> AnyElement {
    // `clean_user_message_text` strips the literal "`Image`"
    // placeholder our acp_thread merger emits AND rewrites the
    // user-typed `[image #N]` placeholders into markdown links so the
    // Markdown widget paints them as clickable spans. The actual
    // image preview opens through the `on_url_click` hook below.
    let raw_text = content_block_text(&message.content, cx);
    let text = clean_user_message_text(&raw_text);
    let bubble_bg = cx.theme().colors().text_accent.opacity(0.12);
    let group_name = SharedString::from(format!("user-msg-{entry_idx}"));

    let images: Vec<std::sync::Arc<gpui::Image>> = message
        .chunks
        .iter()
        .filter_map(|chunk| match chunk {
            acp::ContentBlock::Image(image_content) => decode_image_local(image_content),
            _ => None,
        })
        .collect();

    let body = if let Some(entity) = markdown_for.get(&(entry_idx, 0)) {
        let images_for_handler = images;
        MarkdownElement::new(entity.clone(), style.clone())
            .on_url_click(move |url, window, cx| {
                // Custom URL scheme `spk-image://<idx>` is rewritten
                // by `clean_user_message_text`. Anything else is a
                // genuine link the user typed; defer to the system
                // browser via `cx.open_url`.
                if let Some(idx_str) = url.strip_prefix("spk-image://")
                    && let Ok(idx) = idx_str.parse::<usize>()
                    && let Some(image) = images_for_handler.get(idx).cloned()
                {
                    open_image_preview(image, window, cx);
                    return;
                }
                cx.open_url(url.as_ref());
            })
            .into_any_element()
    } else if text.is_empty() {
        Empty.into_any_element()
    } else {
        Label::new(text.clone())
            .size(LabelSize::Small)
            .into_any_element()
    };

    v_flex()
        .group(group_name.clone())
        .px_1()
        .mb_3()
        .child(
            // h_flex wrap so the bubble shrinks to content (no full-
            // panel-width slab). max_w(85%) caps long messages.
            h_flex().child(
                div()
                    .relative()
                    .max_w(relative(0.85))
                    .px_2p5()
                    .py_1()
                    .bg(bubble_bg)
                    .rounded_md()
                    .child(body)
                    .child(render_floating_copy_button(
                        SharedString::from(format!("copy-user-{entry_idx}")),
                        text,
                        group_name.clone(),
                    ))
                    .when_some(
                        render_message_time(created_ms, is_last, group_name),
                        |this, time| this.child(time),
                    ),
            ),
        )
        .into_any_element()
}

/// True when a user message's text is the (large, agent-only) compact-
/// context prompt the editor injects on a Compact action. Matched by the
/// template's stable first heading rather than a wire flag so it works
/// without a protocol change; `compact::compaction_template_starts_with_heading`
/// keeps the heading constant in lockstep with the resource file.
pub(crate) fn is_compaction_prompt_text(text: &str) -> bool {
    text.trim_start()
        .starts_with(crate::compact::COMPACT_PROMPT_HEADING)
}

/// `is_compaction_prompt_text` against a rendered `UserMessage`. The
/// injected prompt is sent as a plain turn (no queue timestamp / hint
/// prefix), so the raw content already begins with the heading.
pub(crate) fn is_compaction_prompt_message(message: &UserMessage, cx: &App) -> bool {
    is_compaction_prompt_text(&content_block_text(&message.content, cx))
}

/// Renders the compact-context prompt as a distinct, clickable chip that
/// opens the full prompt text in a popover. We deliberately do NOT expand
/// it inline: the prompt is hundreds of lines, and splicing it into the
/// conversation balloons the scroll height (the user has to scroll forever
/// past it). `markdown`/`style` are the cached render entity for the prompt
/// entry; `raw_text` is the fallback shown if the entity isn't ready. `idx`
/// keeps the element ids unique within the virtualized list.
pub(crate) fn render_compaction_prompt_chip(
    idx: usize,
    markdown: Option<Entity<Markdown>>,
    style: Option<MarkdownStyle>,
    raw_text: String,
    _cx: &App,
) -> AnyElement {
    let trigger = Button::new(("compact-prompt", idx), "Compact-context request")
        .style(ButtonStyle::Outlined)
        .label_size(LabelSize::Small)
        .color(Color::Accent)
        .start_icon(
            Icon::new(IconName::Archive)
                .size(IconSize::Small)
                .color(Color::Accent),
        );

    v_flex()
        .px_1()
        .mb_3()
        .child(
            // h_flex so the chip hugs its content instead of stretching the
            // full panel width.
            h_flex().child(
                PopoverMenu::new(("compact-prompt-menu", idx))
                    .trigger(trigger)
                    .menu(move |_window, cx| {
                        let markdown = markdown.clone();
                        let style = style.clone();
                        let raw_text = raw_text.clone();
                        Some(cx.new(|cx| {
                            CompactPromptPopover::new(markdown, style, raw_text, cx)
                        }))
                    })
                    .anchor(Anchor::TopLeft),
            ),
        )
        .into_any_element()
}

/// Popover body for the compact-context prompt: a bounded, scrollable panel
/// showing the full prompt so it never bloats the conversation. A
/// [`ManagedView`](ui::prelude) — `PopoverMenu` owns its open/close state
/// and dismisses it on click-outside.
pub(crate) struct CompactPromptPopover {
    markdown: Option<Entity<Markdown>>,
    style: Option<MarkdownStyle>,
    raw_text: SharedString,
    focus_handle: FocusHandle,
}

impl CompactPromptPopover {
    fn new(
        markdown: Option<Entity<Markdown>>,
        style: Option<MarkdownStyle>,
        raw_text: String,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            markdown,
            style,
            raw_text: raw_text.into(),
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Focusable for CompactPromptPopover {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for CompactPromptPopover {}

impl Render for CompactPromptPopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match (self.markdown.clone(), self.style.clone()) {
            (Some(entity), Some(style)) => MarkdownElement::new(entity, style).into_any_element(),
            _ => Label::new(self.raw_text.clone())
                .size(LabelSize::Small)
                .into_any_element(),
        };

        v_flex()
            .key_context("CompactPromptPopover")
            .track_focus(&self.focus_handle)
            .elevation_2(cx)
            // Definite height (not just max_h): an anchored popover is
            // content-sized, so the flex_1 scroll child below would collapse
            // to 0 without it.
            .w(rems(34.))
            .h(rems(28.))
            .overflow_hidden()
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .gap_1p5()
                    .items_center()
                    .child(
                        Icon::new(IconName::Archive)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .child(
                        Label::new(SharedString::from("Compact-context request"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(div().h_px().bg(cx.theme().colors().border_variant))
            .child(
                v_flex()
                    .id("compact-prompt-popover-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_3()
                    .py_2()
                    .child(body),
            )
    }
}

/// Cleans a user message's merged-markdown source for display:
///   1. Strips agent-only injected metadata (the optional leading hint
///      line and the per-segment `[HH:MM:SS] ` timestamp prefixes) via
///      `strip_injected_meta` so the user sees only what they typed.
///      Same helper as `pending_blocks_preview` so the queued ghost
///      bubble and the sent message render identically.
///   2. Rewrites EVERY image placeholder in the text into a clickable
///      markdown link of the form `[image #N](spk-image://<idx>)`.
///      Two flavours of placeholder hit this path:
///       - `[image #N]` — injected by the desktop compose-paste handler
///         (label is the session-monotonic
///         `SolutionSessionView::image_count_so_far`), so the user-
///         facing `N` is preserved verbatim.
///       - "`Image`" — emitted by `acp_thread::ContentBlock::append`
///         when an Image chunk follows other content in the same
///         message (the common shape for a mobile-originated user
///         message that bundled text + attachments). These get a
///         synthesised 1-based label off the local ordinal so the
///         desktop bubble surfaces them as `[image #1]`, `[image #2]`
///         identically to a desktop-pasted message.
///      The on-click handler intercepts `spk-image://<idx>` and opens
///      an image-preview window for the matching chunk by ORDINAL
///      position — never `N - 1` — because the `N` from the desktop
///      label is a session counter, not a per-message index.
///   3. Collapses leftover double-blank lines so the bubble doesn't
///      grow an empty paragraph where the placeholder used to live.
pub(crate) fn clean_user_message_text(text: &str) -> String {
    let unmarked = strip_injected_meta(text);
    let mut ordinal: usize = 0;
    let rewrite = |caps: &regex::Captures, ordinal: &mut usize| {
        let label_n = caps
            .get(1)
            .and_then(|m| m.as_str().parse::<usize>().ok())
            .unwrap_or(*ordinal + 1);
        let idx = *ordinal;
        *ordinal += 1;
        format!("[image #{label_n}](spk-image://{idx})")
    };
    // A DESKTOP-composed image message carries BOTH a `[image #N]`
    // paste-placeholder (in the typed text) AND a `\`Image\`` literal that
    // acp_thread's `to_markdown` emits for the SAME image chunk — so matching
    // both would render one attachment as two links (e.g. "image #6" +
    // "image #2"). When explicit `[image #N]` placeholders are present, they
    // already represent every attachment 1:1, so we rewrite ONLY those and
    // drop the redundant `\`Image\`` chunk-literals. A message with no
    // explicit placeholders (mobile-originated text+attachment bundle) carries
    // only `\`Image\`` literals, so we rewrite those instead.
    let with_links = if IMAGE_PLACEHOLDER_RE.is_match(&unmarked) {
        let linked = IMAGE_PLACEHOLDER_RE.replace_all(&unmarked, |caps: &regex::Captures| {
            rewrite(caps, &mut ordinal)
        });
        IMAGE_LITERAL_RE.replace_all(&linked, "").into_owned()
    } else {
        USER_IMAGE_PLACEHOLDER_RE
            .replace_all(&unmarked, |caps: &regex::Captures| {
                rewrite(caps, &mut ordinal)
            })
            .into_owned()
    };
    // Reconstruct with explicit markdown line-break semantics:
    //   * single `\n` between non-empty lines → `  \n` (CommonMark
    //     hard break — two trailing spaces + newline). Without this the
    //     parser folds them into soft breaks and the whole pasted block
    //     renders as one squished paragraph.
    //   * blank lines (≥1 in a row) → `\n\n` (paragraph break). Multiple
    //     consecutive blanks collapse to ONE paragraph break — pasted
    //     code with extra blank lines stays readable instead of
    //     stretching the bubble vertically.
    // Both rules preserve inline markdown the user might have typed
    // (bold, code spans, links). Wrapping the whole message in a code
    // fence would lose that.
    let mut out = String::with_capacity(with_links.len() + 16);
    let mut prev_blank = false;
    let mut first = true;
    for line in with_links.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            if !first && !prev_blank {
                out.push_str("\n\n");
                prev_blank = true;
            }
            // Subsequent blanks within the same run: skip.
        } else {
            if !first && !prev_blank {
                out.push_str("  \n");
            }
            out.push_str(trimmed);
            first = false;
            prev_blank = false;
        }
    }
    out.trim_end().to_string()
}

/// `[image #N]` placeholder pattern injected by the compose paste
/// handler. The capture group is the 1-based image index. Used by
/// the recall path (`session_view::recall`) where we want ONLY the
/// desktop-typed placeholders, not the `\`Image\`` literals emitted
/// by acp_thread's image-chunk merge — those don't carry a recall
/// label and would just confuse the recall surface.
pub(crate) static IMAGE_PLACEHOLDER_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\[image #(\d+)\]").expect("static regex compiles")
    });

/// Combined regex for [clean_user_message_text]: matches either the
/// desktop-paste `[image #N]` placeholder OR the literal `\`Image\``
/// inline-code marker that `acp_thread::ContentBlock::append` emits
/// when merging an image chunk into a multi-block user message
/// (e.g. mobile-originated text + attachment bundle). The capture
/// group is the digits inside `[image #N]` when that variant matched;
/// `None` when the `\`Image\`` branch matched, in which case the
/// caller synthesises a 1-based ordinal from the match position.
pub(crate) static USER_IMAGE_PLACEHOLDER_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\[image #(\d+)\]|`Image`").expect("static regex compiles")
    });

/// The bare `\`Image\`` literal that `acp_thread::ContentBlock::append` emits
/// for an image chunk. Used by [clean_user_message_text] to STRIP these
/// redundant chunk-literals when the message also carries explicit
/// `[image #N]` paste-placeholders (which already represent the same images),
/// so a desktop-composed attachment doesn't render as two links.
pub(crate) static IMAGE_LITERAL_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"`Image`").expect("static regex compiles"));

/// Rewrite the `\`Image\`` literals an assistant `to_markdown` emits for
/// agent-emitted image content blocks (Anthropic `image` blocks routed
/// through `acp_thread::ContentBlock::Image`) into `spk-image://N` markdown
/// links so the same render path the user-attached images use can pop a
/// fullscreen preview. `image_index_base` is the GLOBAL image cursor at
/// the start of this entry — `summarize_entry` advances it once per entry
/// so the indices stay aligned with `EntryImage.index` in the wire
/// `images` array (each `Image` block consumes one slot in cursor order).
/// Mobile already handles the `spk-image://N` scheme for user-attached
/// images (`SessionDetailScreen.kt::onLinkClick`); reusing it for agent
/// images means a single render path covers both sides.
///
/// Pass-through for entries without `\`Image\`` literals (the common
/// shape — most assistant messages are pure text/tool_use, no image
/// chunks). The `## Assistant` header and any `<thinking>` blocks are
/// preserved verbatim; this is purely an image-link rewrite.
pub(crate) fn clean_assistant_message_text(text: &str, image_index_base: usize) -> String {
    if !IMAGE_LITERAL_RE.is_match(text) {
        return text.to_string();
    }
    let mut local: usize = 0;
    IMAGE_LITERAL_RE
        .replace_all(text, |_caps: &regex::Captures| {
            let idx = image_index_base + local;
            local += 1;
            // `[image #N]` label uses the 1-based local ordinal so the
            // visible text in the bubble counts up per-message (`image
            // #1`, `image #2`, …) rather than exposing the global cursor
            // (`spk-image://7`, `spk-image://8`, …). The bracket inner
            // text is purely user-facing; the URL drives the click.
            format!("[image #{}](spk-image://{idx})", local)
        })
        .into_owned()
}

/// Mirrors `acp_thread::ContentBlock::decode_image` (private upstream)
/// so we can re-decode image chunks at render time without exposing a
/// new `pub` surface in the acp_thread crate. Returns None on malformed
/// base64 / unsupported mime — caller falls back to the placeholder.
pub(crate) fn decode_image_local(
    image_content: &acp::ImageContent,
) -> Option<std::sync::Arc<gpui::Image>> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image_content.data.as_bytes())
        .ok()?;
    let format = gpui::ImageFormat::from_mime_type(&image_content.mime_type)?;
    Some(std::sync::Arc::new(gpui::Image::from_bytes(format, bytes)))
}

/// Opens the given image in a centred OS popup window for full-size
/// inspection. Used by the chat thumbnail click handler.
pub(crate) fn open_image_preview(
    image: std::sync::Arc<gpui::Image>,
    window: &mut Window,
    cx: &mut App,
) {
    let display_size = window
        .display(cx)
        .or_else(|| cx.primary_display())
        .map(|d| d.bounds().size)
        .unwrap_or(gpui::Size {
            width: px(800.0),
            height: px(600.0),
        });
    let size = gpui::Size {
        width: display_size.width * 0.6,
        height: display_size.height * 0.7,
    };
    let bounds = gpui::WindowBounds::centered(size, cx);
    if let Err(err) = cx.open_window(
        gpui::WindowOptions {
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Image preview".into()),
                appears_transparent: false,
                traffic_light_position: None,
            }),
            window_bounds: Some(bounds),
            is_resizable: true,
            is_minimizable: true,
            kind: gpui::WindowKind::Normal,
            ..Default::default()
        },
        move |window, cx| {
            window.activate_window();
            cx.new(|_| ImagePreviewWindowView { image })
        },
    ) {
        log::error!("failed to open image preview window: {err:?}");
    }
}

pub(crate) struct ImagePreviewWindowView {
    image: std::sync::Arc<gpui::Image>,
}

impl Render for ImagePreviewWindowView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .flex()
            .items_center()
            .justify_center()
            .child(
                gpui::img(self.image.clone())
                    .object_fit(gpui::ObjectFit::Contain)
                    .size_full(),
            )
    }
}

pub(crate) fn render_assistant_message(
    entry_idx: usize,
    message: &AssistantMessage,
    created_ms: Option<i64>,
    is_last: bool,
    markdown_for: &HashMap<(usize, usize), Entity<Markdown>>,
    style: &MarkdownStyle,
    _assistant_label: &SharedString,
    cx: &App,
) -> AnyElement {
    let group_name = SharedString::from(format!("assistant-msg-{entry_idx}"));
    // No "<Adapter>" header above assistant messages either — the absence of
    // the user bubble tint is the role cue. The status row at the top of the
    // panel still shows which adapter owns the active session, so users who
    // need to know which AI is talking still have that signal.
    // Assistant text starts further LEFT than user-bubble inner text
    // on purpose: the offset is the role cue. Aligning them (a
    // previous attempt did `pl_3p5` to match the bubble's inner pad)
    // made the conversation read as one undifferentiated column.
    // `mb_3` mirrors the user bubble's bottom margin so messages
    // breathe without the chunky `my_0p5` gaps.
    let mut container = v_flex().group(group_name.clone()).relative().px_1().mb_3(); // 12 px — a hair more than the user bubble's mb_3 above; both stay synced.
    // While the agent is mid-turn we may have only `Thought` chunks —
    // show them so the user sees activity. Once any real `Message`
    // chunk arrives the thoughts become noise (Claude was reasoning)
    // and we drop them. Matches Cursor / upstream Zed AgentPanel which
    // collapse reasoning tokens once the answer starts streaming.
    let has_message = message
        .chunks
        .iter()
        .any(|c| matches!(c, AssistantMessageChunk::Message { .. }));
    // Must mirror `entry_text_spans` exactly — the markdown cache is keyed by
    // `(entry_idx, span_idx)` and built from that function's spans, so the
    // span shape here has to line up or find-highlighting and the rendered
    // markdown drift apart.
    if has_message {
        // One coalesced block: a single assistant turn reads as one
        // continuous message instead of N stacked widgets with gaps.
        // `combined` also feeds the footer copy button, so the clipboard
        // matches exactly what's painted (no hidden reasoning leaks in).
        let mut combined = String::new();
        for chunk in &message.chunks {
            if let AssistantMessageChunk::Message { block } = chunk {
                let text = content_block_text(block, cx);
                if text.is_empty() {
                    continue;
                }
                if !combined.is_empty() {
                    combined.push_str("\n\n");
                }
                combined.push_str(&text);
            }
        }
        if !combined.is_empty() {
            container = container.child(render_span((entry_idx, 0), &combined, markdown_for, style));
            container = container.child(render_floating_copy_button(
                SharedString::from(format!("copy-assistant-{entry_idx}")),
                combined,
                group_name.clone(),
            ));
        }
    } else {
        // Thought-only: one "thinking…" block per reasoning chunk.
        let mut span_idx = 0;
        for chunk in &message.chunks {
            if let AssistantMessageChunk::Thought { block } = chunk {
                let text = content_block_text(block, cx);
                if text.is_empty() {
                    continue;
                }
                let element = render_span((entry_idx, span_idx), &text, markdown_for, style);
                container = container.child(
                    div()
                        .child(
                            Label::new("thinking…")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .italic(),
                        )
                        .child(element),
                );
                span_idx += 1;
            }
        }
    }
    if let Some(time) = render_message_time(created_ms, is_last, group_name) {
        container = container.child(time);
    }
    container.into_any_element()
}

/// Bottom-right copy affordance, absolute-positioned so it overlays
/// the parent message bubble's lower-right corner instead of sitting
/// on its own row beneath. The previous footer-row layout reserved
/// ~24 px of vertical space whether or not the user hovered, smearing
/// every message with empty padding the user never asked for.
///
/// Caller must wrap the bubble (or assistant container) in `relative()`
/// so the absolute child anchors correctly.
pub(crate) fn render_floating_copy_button(
    button_id: SharedString,
    source: String,
    group_name: SharedString,
) -> impl IntoElement {
    div().absolute().bottom_0p5().right_0p5().child(
        CopyButton::new(button_id, source)
            .icon_size(IconSize::XSmall)
            .tooltip_label("Copy as markdown")
            .visible_on_hover(group_name),
    )
}

/// `HH:MM` affordance for a message bubble. Anchored absolutely just
/// ABOVE the bubble's top-right corner (`bottom_full`), so it floats in
/// the inter-message gap rather than painting over the bubble's own text
/// — a short single-line message would otherwise have the timestamp land
/// directly on top of the text. Stays clear of the bottom-right copy
/// button (different corner). Always hover-only (same group the copy
/// button uses) — the always-visible "last activity" time now lives in
/// the status row instead, so no bubble needs a permanently-painted
/// timestamp. The `_is_last` param is kept for the caller's plumbing but
/// no longer affects visibility. Returns `None` for entries without a
/// real timestamp (`ms <= 0` is filtered upstream).
fn render_message_time(
    created_ms: Option<i64>,
    _is_last: bool,
    group_name: SharedString,
) -> Option<impl IntoElement> {
    let ms = created_ms.filter(|&ms| ms > 0)?;
    let dt = chrono::Utc.timestamp_millis_opt(ms).single()?;
    Some(
        div()
            .absolute()
            .bottom_full()
            .right_1p5()
            .child(
                Label::new(crate::status_row::format_hm(dt))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .visible_on_hover(group_name),
    )
}

/// Extract a one-line summary of the most informative string value from a
/// tool call's `raw_input` for display next to the tool name. Mirrors the
/// pattern from `background_agent::derive_assistant_label`: prefers a
/// well-known argument name (`command`, `file_path`, `path`, `pattern`,
/// `query`, `url`) when present so a Bash call surfaces its command,
/// a Read surfaces its file_path, etc. Falls back to the first non-empty
/// string value in the input object. Truncates to ~120 chars (single
/// line, ellipsis suffix on overflow) so even a multi-line bash invocation
/// stays glanceable on the tool header.
fn tool_call_arg_preview(raw_input: &serde_json::Value) -> Option<String> {
    const PREFERRED_KEYS: &[&str] = &[
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "old_string",
    ];
    // On its own sub-row under the tool header (`render_tool_call`),
    // `.truncate()` on the Label clips to whatever width the container
    // has. The cap below is a memory guard for pathological inputs
    // (`raw_input` could carry a multi-megabyte string), not a layout
    // constraint — leave it generous so wide windows show more.
    const MAX_LEN: usize = 240;
    let obj = raw_input.as_object()?;
    let picked = PREFERRED_KEYS
        .iter()
        .find_map(|k| {
            obj.get(*k)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            obj.values()
                .find_map(|v| v.as_str().filter(|s| !s.is_empty()))
        })?;
    // Single-line: replace embedded newlines with `↵` so a multi-line
    // shell pipeline collapses without dropping content silently.
    let single_line: String = picked
        .chars()
        .map(|c| if c == '\n' { '↵' } else { c })
        .collect();
    let truncated: String = single_line.chars().take(MAX_LEN).collect();
    let needs_ellipsis = single_line.chars().count() > MAX_LEN;
    Some(if needs_ellipsis {
        format!("{truncated}…")
    } else {
        truncated
    })
}

pub(crate) fn render_tool_call(
    entry_idx: usize,
    call: &ToolCall,
    markdown_for: &HashMap<(usize, usize), Entity<Markdown>>,
    style: &MarkdownStyle,
    thread: gpui::WeakEntity<AcpThread>,
    cx: &App,
) -> AnyElement {
    let label_text = call.label.read(cx).source().to_string();
    let status_text = tool_call_status_text(&call.status);
    let status_color = match call.status {
        ToolCallStatus::Failed => Color::Error,
        ToolCallStatus::Rejected | ToolCallStatus::Canceled => Color::Warning,
        ToolCallStatus::Completed => Color::Success,
        _ => Color::Muted,
    };

    // Elapsed-time badge: shown only while the tool is actively running
    // so the user can tell a 30-second hang apart from a freshly-started
    // call. Terminal statuses skip the badge — we keep the timestamp on
    // the entity (see acp_thread::ToolCall::status_started_at) but
    // rendering "ran for Xs" on done/failed/canceled calls is a
    // deliberate follow-up, not part of the live-counter surface.
    let elapsed_label = if matches!(call.status, ToolCallStatus::InProgress) {
        call.status_started_at.map(|started| {
            let elapsed_secs = (chrono::Utc::now() - started).num_seconds().max(0) as u64;
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
    let arg_preview = call.raw_input.as_ref().and_then(tool_call_arg_preview);

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
                    Icon::new(IconName::ToolHammer)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(render_span(
                    (entry_idx, 0),
                    &label_text,
                    markdown_for,
                    style,
                ))
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
        // Preview lives on its own row under the header. Saves the
        // crammed-into-the-title-line layout where a long shell
        // pipeline truncated mid-word and pushed the status badge off
        // to the right; the preview now wraps naturally and the
        // status stays glanceable.
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
    for content in &call.content {
        let summary = tool_call_content_summary(call, content, cx);
        if !summary.is_empty() {
            container = container.child(div().child(render_span(
                (entry_idx, span_idx),
                &summary,
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
    if let ToolCallStatus::WaitingForConfirmation { options, .. } = &call.status {
        let buttons = permission_buttons(options);
        if !buttons.is_empty() {
            let tool_call_id = call.id.clone();
            let mut row = h_flex().gap_1().mt_0p5().flex_wrap();
            for (button_idx, button) in buttons.into_iter().enumerate() {
                let style = if button.is_allow() {
                    ButtonStyle::Filled
                } else {
                    ButtonStyle::Subtle
                };
                let label_color = if button.is_allow() {
                    Color::Default
                } else {
                    Color::Muted
                };
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
                        .label_size(LabelSize::Small)
                        .color(label_color)
                        .on_click(move |_, _, cx| {
                            let outcome = button.outcome();
                            let tool_call_id = tool_call_id.clone();
                            thread
                                .update(cx, move |thread, cx| {
                                    thread.authorize_tool_call(tool_call_id, outcome, cx);
                                })
                                .log_err();
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
    entries: &[PlanEntry],
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
    for (i, _entry) in entries.iter().enumerate() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn id(label: &str) -> UserMessageId {
        // UserMessageId wraps Arc<str>; serde round-trip is the only public
        // way to mint one with a deterministic value (its `new()` always
        // generates a fresh UUID).
        serde_json::from_value(serde_json::Value::String(label.into()))
            .expect("UserMessageId deserializes from any string")
    }

    #[test]
    fn detects_compaction_prompt_by_heading() {
        // The real heading (and a body after it) is folded.
        let prompt = format!(
            "{}\n\nThe user has triggered the Compact Context action…",
            crate::compact::COMPACT_PROMPT_HEADING
        );
        assert!(is_compaction_prompt_text(&prompt));
        // Leading whitespace is tolerated (template is included verbatim).
        assert!(is_compaction_prompt_text(&format!(
            "\n  {}",
            crate::compact::COMPACT_PROMPT_HEADING
        )));
        // Ordinary user messages are never folded.
        assert!(!is_compaction_prompt_text("# Compact the build, please"));
        assert!(!is_compaction_prompt_text("compact this session"));
        assert!(!is_compaction_prompt_text(""));
    }

    #[test]
    fn tool_call_arg_preview_prefers_command_for_bash() {
        let input = serde_json::json!({
            "description": "Run build",
            "command": "cargo build --release",
            "timeout": 600
        });
        assert_eq!(
            tool_call_arg_preview(&input),
            Some("cargo build --release".to_string()),
        );
    }

    #[test]
    fn tool_call_arg_preview_prefers_file_path_for_read() {
        let input = serde_json::json!({ "file_path": "/etc/hosts", "offset": 0 });
        assert_eq!(
            tool_call_arg_preview(&input),
            Some("/etc/hosts".to_string()),
        );
    }

    #[test]
    fn tool_call_arg_preview_falls_back_to_first_string_value() {
        let input = serde_json::json!({ "unknown_field": "some text", "n": 42 });
        assert_eq!(tool_call_arg_preview(&input), Some("some text".to_string()),);
    }

    #[test]
    fn tool_call_arg_preview_collapses_newlines() {
        let input = serde_json::json!({ "command": "echo a\necho b" });
        assert_eq!(
            tool_call_arg_preview(&input).as_deref(),
            Some("echo a↵echo b"),
        );
    }

    #[test]
    fn tool_call_arg_preview_truncates_with_ellipsis() {
        let long = "x".repeat(400);
        let input = serde_json::json!({ "command": long });
        let preview = tool_call_arg_preview(&input).unwrap();
        assert!(preview.ends_with('…'));
        assert!(preview.chars().count() <= 241);
    }

    #[test]
    fn tool_call_arg_preview_none_for_empty_input() {
        assert!(tool_call_arg_preview(&serde_json::json!({})).is_none());
        assert!(tool_call_arg_preview(&serde_json::json!(null)).is_none());
        assert!(tool_call_arg_preview(&serde_json::json!({ "command": "" })).is_none());
    }

    #[test]
    fn user_message_single_newline_becomes_hard_break() {
        let out = clean_user_message_text("first line\nsecond line");
        assert_eq!(out, "first line  \nsecond line");
    }

    #[test]
    fn user_message_blank_line_becomes_paragraph_break() {
        let out = clean_user_message_text("para 1\n\npara 2");
        assert_eq!(out, "para 1\n\npara 2");
    }

    #[test]
    fn user_message_multiple_blank_lines_collapse_to_one_paragraph_break() {
        let out = clean_user_message_text("para 1\n\n\n\npara 2");
        assert_eq!(out, "para 1\n\npara 2");
    }

    #[test]
    fn user_message_mixed_blocks_preserve_structure() {
        let out = clean_user_message_text("intro\nline 2\n\nnext para\nstill next");
        assert_eq!(out, "intro  \nline 2\n\nnext para  \nstill next");
    }

    #[test]
    fn image_placeholder_links_use_per_message_ordinal_not_label() {
        // Earlier code used `N - 1` as the URL idx, but `image #N` labels
        // are session-monotonic — message #2 might own only "image #5",
        // and `images.get(4)` against that message's chunks is `None`,
        // dumping the user into the OS "Open With…" dialog. Ordinal-
        // counted URLs (`spk-image://0`, `spk-image://1`, …) align with
        // the order images appear in `message.chunks`.
        let out = clean_user_message_text("look at [image #5] and then [image #7]");
        assert_eq!(
            out,
            "look at [image #5](spk-image://0) and then [image #7](spk-image://1)"
        );
    }

    #[test]
    fn image_placeholder_link_starts_at_ordinal_zero_in_each_message() {
        // Even if the only image is labelled `image #99`, the URL is
        // `spk-image://0` because there's exactly one image in this
        // message and it's the first.
        let out = clean_user_message_text("only [image #99]");
        assert_eq!(out, "only [image #99](spk-image://0)");
    }

    #[test]
    fn desktop_image_message_does_not_double_render_placeholder_and_literal() {
        // A desktop-composed image message carries BOTH the `[image #6]`
        // paste-placeholder AND the `\`Image\`` literal that to_markdown emits
        // for the same chunk. Only ONE link should render (the placeholder);
        // the redundant `\`Image\`` literal is stripped — otherwise the single
        // attachment showed up as "image #6" + "image #2".
        let out = clean_user_message_text("Restart the runner now\n\n[image #6]\n\n`Image`");
        assert_eq!(out, "Restart the runner now\n\n[image #6](spk-image://0)");
    }

    #[test]
    fn mobile_image_literal_still_links_when_no_placeholder() {
        // A message with no `[image #N]` placeholder (mobile-originated
        // text+attachment bundle) still rewrites the bare `\`Image\`` literal
        // into a clickable link.
        let out = clean_user_message_text("see this\n\n`Image`");
        assert_eq!(out, "see this\n\n[image #1](spk-image://0)");
    }

    #[test]
    fn empty_entries_produce_empty_table() {
        assert_eq!(
            compute_rewind_table(&[]),
            Vec::<Option<UserMessageId>>::new()
        );
    }

    #[test]
    fn user_message_itself_is_never_its_own_target() {
        // [user(A)] — the user message at idx 0 must not target itself.
        let table = compute_rewind_table(&[Some(id("A"))]);
        assert_eq!(table, vec![None]);
    }

    #[test]
    fn assistant_after_last_user_has_no_target() {
        // [user(A), assistant, tool] — the trailing assistant + tool come
        // after the last user message, so they have nothing to rewind TO.
        let table = compute_rewind_table(&[Some(id("A")), None, None]);
        assert_eq!(table, vec![None, None, None]);
    }

    #[test]
    fn entries_between_two_user_messages_target_the_later_one() {
        // [user(A), assistant, tool, user(B), assistant] — the assistant
        // and tool between A and B both rewind to B; the assistant after
        // B has no downstream user message, so it's None.
        let table = compute_rewind_table(&[Some(id("A")), None, None, Some(id("B")), None]);
        assert_eq!(table, vec![None, Some(id("B")), Some(id("B")), None, None]);
    }

    #[test]
    fn user_message_without_id_inherits_next_users_target() {
        // [user(A), assistant, user(None), assistant, user(B)] — the
        // user-without-id at idx 2 falls through the gating branch and
        // gets the same target as the surrounding assistant entries:
        // the next user with id, which is B.
        let table = compute_rewind_table(&[Some(id("A")), None, None, None, Some(id("B"))]);
        assert_eq!(
            table,
            vec![None, Some(id("B")), Some(id("B")), Some(id("B")), None]
        );
    }

    #[test]
    fn many_users_chain_rewind_targets() {
        // [user(A), assistant, user(B), assistant, user(C)] — entries
        // after A but before B target B; entries after B but before C
        // target C; entries after C have no target.
        let table =
            compute_rewind_table(&[Some(id("A")), None, Some(id("B")), None, Some(id("C"))]);
        assert_eq!(table, vec![None, Some(id("B")), None, Some(id("C")), None]);
    }

    #[test]
    fn strip_injected_meta_removes_leading_timestamp() {
        assert_eq!(super::strip_injected_meta("[10:39:12] actual user text"), "actual user text");
    }

    #[test]
    fn strip_injected_meta_removes_each_segment_timestamp() {
        let s = "[10:39:12] first\n\n[10:39:30] second";
        assert_eq!(super::strip_injected_meta(s), "first\n\nsecond");
    }

    #[test]
    fn strip_injected_meta_removes_leading_hint_line() {
        let s = format!("{}\n\n[10:39:12] text", crate::store::QUEUE_HINT_LINE);
        assert_eq!(super::strip_injected_meta(&s), "text");
    }

    #[test]
    fn strip_injected_meta_passes_through_plain_text() {
        assert_eq!(super::strip_injected_meta("hi there"), "hi there");
    }

    #[test]
    fn strip_injected_meta_passes_through_non_timestamp_bracket() {
        // A leading bracket that is NOT a valid HH:MM:SS must be left intact.
        assert_eq!(super::strip_injected_meta("[not-a-timestamp] text"), "[not-a-timestamp] text");
    }

    fn collect(text: &str, query: &str) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        find_all(text, &query.to_lowercase(), |r| out.push(r));
        out
    }

    #[test]
    fn find_all_basic() {
        assert_eq!(collect("hello world", "hello"), vec![0..5]);
        assert_eq!(
            collect("hello hello hello", "hello"),
            vec![0..5, 6..11, 12..17]
        );
    }

    #[test]
    fn find_all_case_insensitive() {
        assert_eq!(collect("Hello World", "hello"), vec![0..5]);
        assert_eq!(
            collect("HELLO HeLLo hello", "Hello"),
            vec![0..5, 6..11, 12..17]
        );
    }

    #[test]
    fn find_all_no_match() {
        assert_eq!(collect("abc", "xyz"), Vec::<Range<usize>>::new());
    }

    #[test]
    fn find_all_empty_query() {
        assert_eq!(collect("anything", ""), Vec::<Range<usize>>::new());
    }

    #[test]
    fn find_all_overlapping_advances_by_query_len() {
        // Advances past the match — does NOT find overlapping matches. This
        // mirrors common find-bar behavior (Cursor / VS Code) where typing
        // "aa" in "aaaa" highlights two non-overlapping pairs at 0..2 and
        // 2..4 rather than three at 0..2, 1..3, 2..4.
        assert_eq!(collect("aaaa", "aa"), vec![0..2, 2..4]);
    }

    fn opt(id: &'static str, name: &str, kind: acp::PermissionOptionKind) -> acp::PermissionOption {
        acp::PermissionOption::new(id, name.to_string(), kind)
    }

    #[test]
    fn permission_buttons_flat_preserves_order_and_kind() {
        let options = PermissionOptions::Flat(vec![
            opt("allow", "Allow", acp::PermissionOptionKind::AllowOnce),
            opt("reject", "Reject", acp::PermissionOptionKind::RejectOnce),
        ]);
        let buttons = permission_buttons(&options);
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0].label, SharedString::from("Allow"));
        assert!(buttons[0].is_allow());
        assert!(buttons[0].patterns.is_empty());
        assert_eq!(buttons[1].label, SharedString::from("Reject"));
        assert!(!buttons[1].is_allow());
        // The rebuilt outcome carries the option id + kind verbatim.
        let outcome = buttons[1].outcome();
        assert_eq!(outcome.option_id, buttons[1].option_id);
        assert_eq!(outcome.option_kind, acp::PermissionOptionKind::RejectOnce);
        assert!(outcome.params.is_none());
    }

    #[test]
    fn permission_buttons_dropdown_emits_allow_and_deny_per_choice_with_patterns() {
        let choice = acp_thread::PermissionOptionChoice {
            allow: opt("a", "Always allow", acp::PermissionOptionKind::AllowAlways),
            deny: opt("d", "Always deny", acp::PermissionOptionKind::RejectAlways),
            sub_patterns: vec!["^cargo build".to_string()],
        };
        let buttons = permission_buttons(&PermissionOptions::Dropdown(vec![choice]));
        assert_eq!(buttons.len(), 2);
        assert!(buttons[0].is_allow());
        assert!(!buttons[1].is_allow());
        // Patterns ride along on both the allow and deny buttons so the
        // answer applies them.
        assert_eq!(buttons[0].patterns, vec!["^cargo build".to_string()]);
        let outcome = buttons[0].outcome();
        match outcome.params {
            Some(SelectedPermissionParams::Terminal { patterns }) => {
                assert_eq!(patterns, vec!["^cargo build".to_string()]);
            }
            other => panic!("expected terminal params, got {other:?}"),
        }
    }

    #[test]
    fn pick_reject_button_none_when_only_allow_options() {
        // A malformed server response offering ONLY allow options must NOT
        // resolve to an auto-approve — `pick_reject_button` returns None so
        // the queue path leaves the turn stuck rather than approving the call.
        let options = PermissionOptions::Flat(vec![
            opt(
                "allow-once",
                "Allow once",
                acp::PermissionOptionKind::AllowOnce,
            ),
            opt(
                "allow-always",
                "Allow always",
                acp::PermissionOptionKind::AllowAlways,
            ),
        ]);
        assert!(pick_reject_button(&options).is_none());
    }

    #[test]
    fn pick_reject_button_prefers_reject_once() {
        let options = PermissionOptions::Flat(vec![
            opt("allow", "Allow", acp::PermissionOptionKind::AllowOnce),
            opt(
                "reject-always",
                "Reject always",
                acp::PermissionOptionKind::RejectAlways,
            ),
            opt(
                "reject-once",
                "Reject once",
                acp::PermissionOptionKind::RejectOnce,
            ),
        ]);
        let button = pick_reject_button(&options).expect("a reject button must be picked");
        assert_eq!(button.kind, acp::PermissionOptionKind::RejectOnce);
        assert_eq!(
            button.option_id,
            acp::PermissionOptionId::new("reject-once")
        );
    }

    #[test]
    fn matches_for_span_filters_and_finds_selected() {
        let matches = vec![
            FindMatch {
                entry_idx: 0,
                span_idx: 0,
                range: 0..5,
            },
            FindMatch {
                entry_idx: 0,
                span_idx: 1,
                range: 0..3,
            },
            FindMatch {
                entry_idx: 1,
                span_idx: 0,
                range: 5..8,
            },
            FindMatch {
                entry_idx: 0,
                span_idx: 0,
                range: 10..15,
            },
        ];
        let (ranges, sel) = matches_for_span(&matches, Some(3), 0, 0);
        assert_eq!(ranges, vec![0..5, 10..15]);
        assert_eq!(sel, Some(1));

        let (ranges, sel) = matches_for_span(&matches, Some(3), 1, 0);
        assert_eq!(ranges, vec![5..8]);
        assert_eq!(sel, None);

        let (ranges, sel) = matches_for_span(&matches, Some(2), 1, 0);
        assert_eq!(ranges, vec![5..8]);
        assert_eq!(sel, Some(0));
    }
}
