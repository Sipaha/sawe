//! Pending-message ghost section rendered beneath the conversation list.
//!
//! When the user types a follow-up while the agent is still working, the
//! message is parked in `SolutionSession::pending_messages`. This module
//! paints the ghost bubble (selectable text + clickable image links) plus
//! a footer strip with a "queued message" label and a Bolt-button
//! (interrupt-and-flush). The section returns `None` when the queue is
//! empty so the caller can `when_some(...)` it into the layout without
//! reserving spacing.
//!
//! The bubble is always shown in full. (An earlier collapse-to-one-line
//! affordance existed for when delivery could lag minutes-to-hours and the
//! ghost cluttered the view; the queue-unification work made delivery
//! prompt, so the collapse was pure friction and was removed.)

use std::sync::Arc;

use agent_client_protocol::schema as acp;
use gpui::{AnyElement, Context, Div, IntoElement, ParentElement, SharedString, Styled, relative};
use markdown::MarkdownElement;
use ui::prelude::*;
use ui::{Color, IconName, IconSize, Label, LabelSize, Tooltip};

use super::SolutionSessionView;
use crate::conversation_render::{decode_image_local, open_image_preview};
use crate::model::SessionState;

impl SolutionSessionView {
    /// Build the pending-message ghost section. Reads `pending_messages`
    /// off `self.session` internally rather than accepting a snapshot:
    /// the surrounding render path holds an immutable borrow on `cx` via
    /// `self.session.read(cx)`, so passing the snapshot back in would
    /// double-borrow when `cx.listener` (mutable) constructs the click
    /// handler.
    pub(super) fn render_pending_section(
        &self,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let bundles = self.visible_pending_bundles(cx);
        if bundles.is_empty() {
            return None;
        }
        let is_running = matches!(self.session.read(cx).state, SessionState::Running { .. });

        // Ghost bubble: selectable markdown text + clickable `[image #N]`
        // links wired through `spk-image://` to `open_image_preview`. Reuses
        // the cached `pending_markdown` entity refreshed by
        // `ensure_pending_markdown` in the render pre-pass — building a fresh
        // `Markdown::new` per frame would never finish parsing. `None` only
        // until that entity is ready (filled in on the next frame).
        let bubble = (|| {
            let entity = self.pending_markdown.as_ref()?.clone();
            let style = self.markdown_style_for_render.as_ref()?.clone();
            let bubble_bg = cx.theme().colors().text_accent.opacity(0.06);
            let border_color = cx.theme().colors().text_accent.opacity(0.4);
            // Decode any image blocks in the bundle so the
            // `spk-image://idx` URL handler can pop them up. Mirrors
            // the live-user-message path in
            // `render_user_message`.
            let mut images: Vec<Arc<gpui::Image>> = Vec::new();
            for bundle in &bundles {
                for block in &bundle.blocks {
                    if let agent_client_protocol::schema::ContentBlock::Image(img) = block
                        && let Some(decoded) = decode_image_local(img)
                    {
                        images.push(decoded);
                    }
                }
            }
            let images_for_handler = images;
            let body = MarkdownElement::new(entity, style).on_url_click(move |url, window, cx| {
                if let Some(idx_str) = url.strip_prefix("spk-image://")
                    && let Ok(idx) = idx_str.parse::<usize>()
                    && let Some(image) = images_for_handler.get(idx).cloned()
                {
                    open_image_preview(image, window, cx);
                    return;
                }
                cx.open_url(url.as_ref());
            });
            Some(
                    h_flex().w_full().child(
                        div()
                            .relative()
                            .w_full()
                            .px_2p5()
                            .py_1()
                            .bg(bubble_bg)
                            .border_1()
                            .border_dashed()
                            .border_color(border_color)
                            .rounded_md()
                            .child(body)
                            // The queue is NOT a dead letter: it is pulled into the
                            // running turn at the agent's next hook boundary
                            // (`PostToolUse`, or `Stop` at turn end). Users read a
                            // silent "Queued" badge as "my message was swallowed" —
                            // especially while the agent sits inside a 40-minute
                            // build, where the next boundary is that build's end.
                            // Say when it lands, and that ⚡ is the way to not wait.
                            .when(is_running, |this| {
                                let hint = match crate::status_row::in_progress_tool(
                                    self.session.read(cx),
                                ) {
                                    Some((tool, _)) => format!(
                                        "Delivered when {tool} finishes — or ⚡ to interrupt now"
                                    ),
                                    None => "Delivered at the agent's next step — or ⚡ to \
                                             interrupt now"
                                        .to_string(),
                                };
                                this.child(
                                    Label::new(SharedString::from(hint))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            })
                            // Send-now bolt floats in the bubble's bottom-right
                            // corner (anchored absolutely on the `relative`
                            // bubble) rather than in a separate strip beneath
                            // it: the action belongs to *this* queued message,
                            // so it reads as part of the bubble. Bottom-right
                            // (like the floating copy button) keeps it clear of
                            // the usually-long first text line. Only shown while
                            // the agent is running — cancels the current turn
                            // and immediately flushes the queue (same affordance
                            // as the Bolt next to Stop in the compose row).
                            .when(is_running, |this| {
                                this.child(
                                    div().absolute().bottom_0p5().right_0p5().child(
                                        ui::IconButton::new(
                                            "solution-queue-send-now",
                                            IconName::BoltFilled,
                                        )
                                        .icon_size(IconSize::Small)
                                        .icon_color(Color::Accent)
                                        .tooltip(Tooltip::text(
                                            "Send now — interrupts the current turn and runs your queued follow-up",
                                        ))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.submit_compose_and_interrupt(window, cx);
                                        })),
                                    ),
                                )
                            }),
                    ),
                )
        })();

        // Compose: just the ghost bubble (the send-now bolt now lives on the
        // bubble itself; see above).
        let _ = window;
        // Cap the queued-message ghost at a fraction of the panel height and
        // let it scroll internally. A queued follow-up (typed while the agent
        // is mid-turn) is painted as a non-flex sibling *beneath* the
        // scrollable transcript; without a height cap a long message grows to
        // its full intrinsic height and squeezes the flex_1 transcript to
        // zero, so the user can't see the conversation at all. `relative(0.4)`
        // resolves against the outer flex_col's definite height, keeping ~60%
        // for the transcript, and `overflow_y_scroll` lets the ghost itself
        // scroll when it exceeds the cap.
        let mut section = v_flex()
            .id("solution-pending-queue")
            .w_full()
            .px_1()
            .max_h(relative(0.4))
            .overflow_y_scroll();
        if let Some(bubble) = bubble {
            section = section.child(bubble);
        }
        Some(section.into_any_element())
    }

    /// Optimistic-resume section painted while a cold tab is doing its
    /// 3-4 s ACP handshake after the user clicked Send. Shows the
    /// queued text as a muted ghost bubble plus a "Starting agent…"
    /// spinner so the chat reflects the action immediately — without
    /// this the cold-resume path looked like Send did nothing for
    /// several seconds. Returns `None` outside the resuming window;
    /// `pending_send`/`resuming` are both cleared by
    /// `flush_pending_send_if_ready` once the live thread attaches,
    /// at which point `acp_thread.send` re-emits the message as a
    /// real `UserMessage` entry and the ghost goes away.
    pub(super) fn render_resuming_section(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !self.resuming {
            return None;
        }
        let blocks = self.pending_send.as_ref()?;
        let images: Vec<Arc<gpui::Image>> = blocks
            .iter()
            .filter_map(|b| match b {
                acp::ContentBlock::Image(img) => decode_image_local(img),
                _ => None,
            })
            .collect();

        // Match the live `render_user_message` bubble styling exactly
        // — `text_accent.opacity(0.12)` background, `max_w(0.85)`
        // wrapper, no dashed border. The user wanted the optimistic
        // message to look identical to a normal sent message; the
        // "agent is attaching" cue is delivered by the status row's
        // "Resuming…" badge instead, so there's no need for a
        // visually distinct ghost.
        let bubble_bg = cx.theme().colors().text_accent.opacity(0.12);
        let body: gpui::AnyElement = match (
            self.resuming_markdown.clone(),
            self.markdown_style_for_render.clone(),
        ) {
            (Some(entity), Some(style)) => {
                let images_for_handler = images;
                MarkdownElement::new(entity, style)
                    .on_url_click(move |url, window, cx| {
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
            }
            _ if !images.is_empty() => Label::new(SharedString::from("(image only)"))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            _ => return None,
        };

        let bubble = h_flex().child(
            div()
                .relative()
                .max_w(relative(0.85))
                .px_2p5()
                .py_1()
                .bg(bubble_bg)
                .rounded_md()
                .child(body),
        );
        // `px_3` (12px) matches the *effective* horizontal inset live
        // user messages get: the conversation list wrapper supplies
        // `.px_2()` (8px) and `render_user_message`'s own `v_flex`
        // adds `.px_1()` (4px) on top, totalling 12px. The resuming
        // section sits as a sibling *outside* the conversation
        // wrapper (so the bubble pins to the bottom and doesn't
        // scroll with messages), so we recreate that inset directly
        // here — otherwise the optimistic bubble paints flush against
        // the panel's left edge while the live message above it is
        // indented, which the user noticed immediately.
        // `mb_3` mirrors `render_user_message`'s bottom margin.
        Some(v_flex().w_full().px_3().mb_3().child(bubble))
    }
}
