//! User-message render cluster for the Solution conversation view.
//!
//! Relocated verbatim from `conversation_render.rs` (Tier-1 god-object split).

use super::*;

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
        && stamp
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 2 || i == 5 || b.is_ascii_digit());
    if is_hms {
        &rest[close + crate::store::TS_PREFIX_CLOSE.len()..]
    } else {
        segment
    }
}

pub(crate) fn render_user_message(
    entry_idx: usize,
    content_md: &str,
    chunks: &[acp::ContentBlock],
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
    let text = clean_user_message_text(content_md);
    let bubble_bg = cx.theme().colors().text_accent.opacity(0.12);
    // A supervisor Observer nudge is delivered to the agent AS a user message
    // (so the agent acts on it), but it is NOT the human's own message — the
    // `spk_observer_nudge` `_meta` marker tags it so we render it as an Observer
    // comment (eye plaque) instead of a plain user bubble.
    let is_observer = acp_thread::is_observer_nudge_blocks(chunks);
    let group_name = SharedString::from(format!("user-msg-{entry_idx}"));

    let images: Vec<std::sync::Arc<gpui::Image>> = chunks
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

    if is_observer {
        // Observer comment: full-width plaque bubble (eye badge + tag over the
        // markdown body), tinted + left-bordered with the accent color — mirrors
        // the System/Observer note render (FORK.md #29) so a supervisor
        // intervention reads as the OBSERVER speaking, not the human.
        let color = Color::Accent;
        return v_flex()
            .group(group_name.clone())
            .relative()
            .mx_2()
            .mb_3()
            .px_2p5()
            .py_1p5()
            .gap_1()
            .rounded_md()
            .bg(color.color(cx).opacity(0.08))
            .border_l_2()
            .border_color(color.color(cx))
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(Icon::new(IconName::Eye).size(IconSize::XSmall).color(color))
                    .child(
                        // Agent-VISIBLE observer nudge (delivered into the thread
                        // AS a message the agent acts on) — plain eye + solid
                        // border (above) + "агенту", distinct from the
                        // agent-invisible operator-only note (EyeOff, dashed).
                        Label::new("Наблюдатель · агенту")
                            .size(LabelSize::XSmall)
                            .color(color),
                    ),
            )
            .child(div().w_full().min_w_0().child(body))
            .child(render_floating_copy_button(
                SharedString::from(format!("copy-observer-{entry_idx}")),
                text,
                group_name.clone(),
            ))
            .when_some(
                render_message_time(created_ms, is_last, group_name),
                |this, time| this.child(time),
            )
            .into_any_element();
    }

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
                        Some(cx.new(|cx| CompactPromptPopover::new(markdown, style, raw_text, cx)))
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
