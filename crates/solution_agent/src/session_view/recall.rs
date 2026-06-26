//! Up-arrow recall path for the compose editor: pulls the queued
//! follow-up message back into the editor so the user can edit it
//! before it sends. Wired to `editor::actions::MoveUp` via
//! `capture_action` on the session-view root (top-down dispatch); if
//! conditions don't match, the handler does NOT call `stop_propagation`
//! and the editor's default cursor-up behavior runs as usual.

use agent_client_protocol::schema as acp;
use gpui::{Context, Focusable, SharedString, Window};

use super::{PendingImage, SolutionSessionView};
use crate::store::SolutionAgentStore;

/// Reverse of `submit_compose_now`'s blocks-from-draft step: takes a queued
/// bundle (already-merged user submissions, possibly with a leading
/// timestamp marker and embedded images) and rebuilds the inputs the user
/// originally had — concatenated text + a list of `PendingImage`s.
///
/// The injected metadata (timestamp prefix and optional hint line baked in by
/// the queue) is stripped so the recovered draft is just what the user
/// typed. Image labels are re-derived from the `[image #N]` placeholders
/// already present in the text — they're only used for paste-time inserts
/// and never displayed otherwise, so a missing tag falls back to "image
/// #?" (won't appear in the UI; a follow-up paste will assign the next
/// real number from `image_count_so_far`).
pub(super) fn unpack_recalled_bundle(
    blocks: Vec<acp::ContentBlock>,
) -> (String, Vec<PendingImage>) {
    let mut raw_text = String::new();
    let mut images: Vec<PendingImage> = Vec::new();
    for block in blocks {
        match block {
            acp::ContentBlock::Text(t) => {
                raw_text.push_str(&t.text);
            }
            acp::ContentBlock::Image(img) => {
                images.push(PendingImage {
                    mime_type: img.mime_type,
                    data_base64: img.data,
                    label: SharedString::from("image #?"),
                });
            }
            _ => {}
        }
    }
    // Strip injected metadata from the FULLY concatenated text, not just the
    // first block: each merged follow-up carries its own `[HH:MM:SS]` stamp as
    // a separate block, so a first-block-only strip would leak the 2nd+
    // timestamps into the recalled draft. `strip_injected_meta` is per-segment
    // (splits on `\n\n`), so it cleans every follow-up's stamp.
    let text = crate::conversation_render::strip_injected_meta(&raw_text);
    let placeholders: Vec<usize> = crate::conversation_render::IMAGE_PLACEHOLDER_RE
        .captures_iter(&text)
        .filter_map(|c| c.get(1)?.as_str().parse::<usize>().ok())
        .collect();
    for (img, n) in images.iter_mut().zip(placeholders.iter()) {
        img.label = SharedString::from(format!("image #{n}"));
    }
    (text, images)
}

impl SolutionSessionView {
    /// `Up` keystroke in the compose editor. When the editor is empty and a
    /// queued follow-up is sitting in `pending_messages` (typed while the
    /// agent was still working), pull that draft back into the editor —
    /// "I changed my mind, let me edit this before it sends."
    ///
    /// In every other case (editor non-empty, attached images, no queue,
    /// focus is elsewhere) returns without consuming the action so the
    /// editor's default `MoveUp` handler runs and the cursor moves up
    /// as the user expects.
    pub(super) fn recall_queued_message(
        &mut self,
        _: &zed_actions::editor::MoveUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let compose_focus = self.compose_editor.read(cx).focus_handle(cx);
        if !compose_focus.is_focused(window) {
            return;
        }
        let compose_empty = self.compose_editor.read(cx).text(cx).is_empty();
        if !compose_empty || !self.pending_images.is_empty() {
            return;
        }
        let session_id = self.session_id;
        // Recall the most-recent bundle addressed to the CURRENT tab. The
        // queue can hold bundles for several addressees at once (main agent +
        // a teammate), and the ghost the user is looking at is the per-tab
        // filtered one, so recall must pull the matching bundle — not blindly
        // the back of the queue (which might belong to another tab).
        //
        // Peek before remove: `unpack_recalled_bundle` can produce empty
        // `(text, images)` if the bundle is somehow marker-only (defensive —
        // shouldn't happen since `submit_compose_now` rejects empty
        // submissions). Keeping the queue intact instead of silently draining
        // it is the safer default if that invariant ever breaks.
        let target = self.selected_subagent.queue_target();
        let idx = self
            .session
            .read(cx)
            .pending_messages
            .iter()
            .rposition(|bundle| bundle.target == target);
        let Some(idx) = idx else {
            return;
        };
        let bundle = self.session.read(cx).pending_messages[idx].clone();
        let (text, images) = unpack_recalled_bundle(bundle.blocks.clone());
        if text.is_empty() && images.is_empty() {
            return;
        }
        // Stash the original bundle so an `Esc` press in the compose
        // editor can put it back into the queue (cancel-edit).
        self.recalled_bundle = Some(bundle);
        self.session.update(cx, |session, _| {
            session.pending_messages.remove(idx);
        });
        if !text.is_empty() {
            self.compose_editor.update(cx, |editor, cx| {
                editor.set_text(text, window, cx);
            });
        }
        if !images.is_empty() {
            self.pending_images.extend(images);
        }
        // Emit a state-changed event so any listeners (navigator tab indicator,
        // status row, …) refresh from the new state — the bundle just moved
        // out of `pending_messages`.
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            // `SolutionSession.state` is unchanged here (only the queue moved) —
            // emit the bare `SessionStateChanged` for the desktop re-render
            // without bumping `state_seq`. The queue change is carried by
            // `mark_queue_changed` below.
            cx.emit(crate::store::SolutionAgentStoreEvent::SessionStateChanged(
                session_id,
            ));
            // The bundle just left `pending_messages` — broadcast so
            // paired clients (mobile) drop the matching Queued bubble.
            store.mark_queue_changed(session_id, cx);
        });
        cx.stop_propagation();
        cx.notify();
    }
}
