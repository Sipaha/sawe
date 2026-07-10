//! Pure rendering helpers and shared types for the Solution session conversation view. Extracted from session_view.rs to keep that file focused on view state + input handling.

use std::collections::HashMap;
use std::ops::Range;

use crate::session_entry::{
    AssistantChunk, SessionEntry, SessionEntryKind, SystemEntryLevel, ToolStatus,
};
use acp_thread::{
    AcpThread, AgentThreadEntry, ContentBlock, PermissionOptions, SelectedPermissionOutcome,
    SelectedPermissionParams, ToolCall, ToolCallContent, ToolCallStatus, UserMessageId,
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

mod image;
mod tool_call;
mod user_message;

#[cfg(test)]
mod tests;

// Re-export the submodule surface at `crate::conversation_render::*`. Cross-module
// callers (`session_view`, `event_sources`, `session_entry`, `mcp/*`, `store/queue`,
// `cold_persistence`, `compact`) reference these via `crate::conversation_render::<name>`,
// so the paths must keep resolving after the split. Looks unused to a lib-only build.
#[allow(unused_imports)]
pub(crate) use {image::*, tool_call::*, user_message::*};
#[derive(Clone, Debug)]
pub(crate) struct FindMatch {
    pub(crate) entry_idx: usize,
    pub(crate) span_idx: usize,
    pub(crate) range: Range<usize>,
}

/// Pure backward-walk that computes, for each entry index, the id of the
/// next user message *after* it (the rewind target). Caller pre-projects
/// the entries list to `Option<String>` per slot — `Some(id)` for a
/// user message that carries an id (the `SessionEntry::UserMessage.id`),
/// `None` for everything else (assistant, tool, plan, or a user message
/// without an id). The `String` id is resolved back to a live
/// `UserMessageId` at the rewind action site.
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
pub(crate) fn compute_rewind_table(user_ids: &[Option<String>]) -> Vec<Option<String>> {
    let mut table = vec![None; user_ids.len()];
    let mut current: Option<String> = None;
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
pub(crate) fn entry_text_spans(entry: &SessionEntry) -> Vec<String> {
    match &entry.kind {
        SessionEntryKind::UserMessage { content_md, .. } => {
            vec![clean_user_message_text(content_md)]
        }
        SessionEntryKind::AssistantMessage { chunks } => {
            let has_message = chunks
                .iter()
                .any(|c| matches!(c, AssistantChunk::Message(_)));
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
                for chunk in chunks {
                    if let AssistantChunk::Message(text) = chunk {
                        if text.is_empty() {
                            continue;
                        }
                        if !combined.is_empty() {
                            combined.push_str("\n\n");
                        }
                        combined.push_str(text);
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
                for chunk in chunks {
                    if let AssistantChunk::Thought(text) = chunk {
                        if !text.is_empty() {
                            spans.push(format!("thinking: {text}"));
                        }
                    }
                }
                spans
            }
        }
        SessionEntryKind::ToolCall {
            label_md,
            status,
            content_md,
            ..
        } => {
            let status_text = tool_status_text(status);
            let mut spans = vec![format!("Tool: {label_md} ({status_text})")];
            for summary in content_md {
                if !summary.is_empty() {
                    spans.push(summary.clone());
                }
            }
            spans
        }
        SessionEntryKind::Plan(items) => {
            let mut spans = vec!["Plan".to_string()];
            for item in items {
                spans.push(format!("• {}", item.content_md));
            }
            spans
        }
        SessionEntryKind::ContextCompaction { summary_md, .. } => match summary_md {
            Some(summary) => vec![format!("Context compaction: {summary}")],
            None => vec!["Context compaction".to_string()],
        },
        SessionEntryKind::System { text_md, .. } => vec![text_md.clone()],
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

/// Status label for an owned [`ToolStatus`] — drives the tool-card status
/// badge and the find-bar span text.
pub(crate) fn tool_status_text(status: &ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending => "pending",
        ToolStatus::WaitingForConfirmation => "waiting for confirmation",
        ToolStatus::InProgress => "running",
        ToolStatus::Completed => "done",
        ToolStatus::Failed => "failed",
        ToolStatus::Rejected => "rejected",
        ToolStatus::Canceled => "canceled",
    }
}

/// Pure predicate used by the view's per-second elapsed-badge tick: does
/// any entry hold a tool call still `InProgress`? Reads owned
/// [`SessionEntry`]s so it works on both cold and live transcripts.
pub(crate) fn entries_have_in_progress_tool_call(entries: &[SessionEntry]) -> bool {
    entries.iter().any(|entry| {
        matches!(
            &entry.kind,
            SessionEntryKind::ToolCall {
                status: ToolStatus::InProgress,
                ..
            }
        )
    })
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

/// Serialize a [`UserMessageId`] to the same `String` shape
/// `session_entry::to_session_entry` stores in
/// `SessionEntryKind::UserMessage.id`, so the two can be compared.
fn user_message_id_to_string(id: &UserMessageId) -> String {
    serde_json::to_value(id)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// Find the live `UserMessageId` whose serialized form matches the
/// `SessionEntry::UserMessage.id` string `target`. Returns `None` when no
/// live user message carries that id (e.g. the live thread was replaced
/// out from under a stale render) so the rewind is a no-op rather than a
/// truncate against the wrong turn.
fn resolve_user_message_id(thread: &AcpThread, target: &str) -> Option<UserMessageId> {
    thread.entries().iter().find_map(|entry| match entry {
        AgentThreadEntry::UserMessage(message) => {
            let id = message.id.as_ref()?;
            (user_message_id_to_string(id) == target).then(|| id.clone())
        }
        _ => None,
    })
}

pub(crate) fn render_entry(
    entry_idx: usize,
    entry: &SessionEntry,
    is_last: bool,
    date_separator: Option<String>,
    markdown_for: &HashMap<(usize, usize), Entity<Markdown>>,
    style: &MarkdownStyle,
    assistant_label: &SharedString,
    rewind_target: Option<String>,
    thread: gpui::WeakEntity<AcpThread>,
    cx: &App,
) -> AnyElement {
    // `created_ms == 0` is the "unknown time" sentinel (replayed gap /
    // pre-feature / drill-in); `render_message_time` filters `ms > 0`, so
    // forward it through `Option` to keep the same downstream behaviour.
    let created_ms = Some(entry.created_ms).filter(|&ms| ms > 0);
    let inner: AnyElement = match &entry.kind {
        SessionEntryKind::UserMessage {
            content_md, chunks, ..
        } => render_user_message(
            entry_idx,
            content_md,
            chunks,
            created_ms,
            is_last,
            markdown_for,
            style,
            cx,
        ),
        SessionEntryKind::AssistantMessage { chunks } => render_assistant_message(
            entry_idx,
            chunks,
            created_ms,
            is_last,
            markdown_for,
            style,
            assistant_label,
        ),
        SessionEntryKind::ToolCall {
            id,
            label_md,
            status,
            content_md,
            raw_input,
            status_started_at,
            ..
        } => render_tool_call(
            entry_idx,
            id,
            label_md,
            status,
            content_md,
            raw_input.as_ref(),
            *status_started_at,
            markdown_for,
            style,
            thread.clone(),
            cx,
        ),
        SessionEntryKind::Plan(items) => render_plan(entry_idx, items, markdown_for, style, cx),
        // Context compaction is a lightweight divider marking where the model
        // summarized its own history; render it as a muted single-line label.
        SessionEntryKind::ContextCompaction { .. } => gpui::div()
            .px_2()
            .py_1()
            .child(ui::Label::new("Context compacted").color(ui::Color::Muted))
            .into_any_element(),
        // Editor-originated annotation — render distinctly so the user can tell
        // it apart from the agent's own messages. Info = neutral, Error = red,
        // Observer (supervisor) = accent.
        SessionEntryKind::System { level, text_md } => {
            let (icon, color, tag) = match level {
                SystemEntryLevel::Info => (IconName::Info, Color::Muted, "System"),
                SystemEntryLevel::Error => (IconName::Warning, Color::Error, "System"),
                // Agent-INVISIBLE observer note (the agent never sees this) — a
                // crossed-eye icon + "только вам" tag + a dashed border mark it
                // as private to the operator, visually distinct from the
                // agent-VISIBLE observer nudge (plain eye, solid border,
                // "агенту") rendered in `render_user_message` below.
                SystemEntryLevel::Observer => {
                    (IconName::EyeOff, Color::Accent, "Наблюдатель · только вам")
                }
            };
            let is_observer_note = matches!(level, SystemEntryLevel::Observer);
            // Editor-injected note (watchdog / supervisor / system) — NOT part
            // of the agent's context; the agent never sees these. Render it as a
            // readable message bubble with a system "plaque" badge (icon + tag)
            // rather than a cramped one-line breadcrumb: a supervisor summary is
            // full markdown (bold, links, lists) and was previously dumped as a
            // raw, non-wrapping `Label` — hence unreadable. The body now goes
            // through `render_span` (same markdown path as user/assistant
            // messages); the level color tints a subtle background + left border
            // so Info / Error / Observer stay distinguishable in the dialog.
            let bubble_bg = color.color(cx).opacity(0.08);
            v_flex()
                .group(SharedString::from(format!("sys-msg-{entry_idx}")))
                .mx_2()
                .my_1p5()
                .px_2p5()
                .py_1p5()
                .gap_1()
                .rounded_md()
                .bg(bubble_bg)
                .border_l_2()
                .border_color(color.color(cx))
                // Dashed left border = "not part of the agent's conversation
                // stream" — reinforces that an observer note is operator-only.
                .when(is_observer_note, |this| this.border_dashed())
                .child(
                    // The "plaque": icon + tag, mirroring the system-note badge.
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                        .child(Label::new(tag).size(LabelSize::XSmall).color(color)),
                )
                .child(
                    // Width-bounded, `min_w_0` so the markdown body wraps.
                    div().w_full().min_w_0().child(render_span(
                        (entry_idx, 0),
                        text_md,
                        markdown_for,
                        style,
                    )),
                )
                .into_any_element()
        }
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
                                        // The rewind target is the
                                        // `SessionEntry::UserMessage.id` (a
                                        // String). Resolve it back to the live
                                        // thread's `UserMessageId` by matching
                                        // the user message that carries it, so
                                        // `rewind` gets exactly the id the live
                                        // thread holds.
                                        if let Some(id) =
                                            resolve_user_message_id(thread, &target_id)
                                        {
                                            thread.rewind(id, cx).detach_and_log_err(cx);
                                        }
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

pub(crate) fn render_assistant_message(
    entry_idx: usize,
    chunks: &[AssistantChunk],
    created_ms: Option<i64>,
    is_last: bool,
    markdown_for: &HashMap<(usize, usize), Entity<Markdown>>,
    style: &MarkdownStyle,
    _assistant_label: &SharedString,
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
    let has_message = chunks
        .iter()
        .any(|c| matches!(c, AssistantChunk::Message(_)));
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
        for chunk in chunks {
            if let AssistantChunk::Message(text) = chunk {
                if text.is_empty() {
                    continue;
                }
                if !combined.is_empty() {
                    combined.push_str("\n\n");
                }
                combined.push_str(text);
            }
        }
        if !combined.is_empty() {
            container =
                container.child(render_span((entry_idx, 0), &combined, markdown_for, style));
            container = container.child(render_floating_copy_button(
                SharedString::from(format!("copy-assistant-{entry_idx}")),
                combined,
                group_name.clone(),
            ));
        }
    } else {
        // Thought-only: one "thinking…" block per reasoning chunk.
        let mut span_idx = 0;
        for chunk in chunks {
            if let AssistantChunk::Thought(text) = chunk {
                if text.is_empty() {
                    continue;
                }
                let element = render_span((entry_idx, span_idx), text, markdown_for, style);
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
