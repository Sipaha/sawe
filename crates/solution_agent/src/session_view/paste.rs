//! Clipboard/paste + external-paths-drop handling for the compose editor:
//! image auto-attach, paste-without-formatting, and drag-dropped file
//! mentions. Relocated verbatim from the view root as `impl
//! SolutionSessionView` methods; `self`/fields stay owned by the struct.

use base64::Engine;
use gpui::{ClipboardEntry, Context, ExternalPaths, Focusable, SharedString, Window};

use super::{PendingImage, SolutionSessionView};
use crate::actions::PasteWithoutFormatting;

impl SolutionSessionView {
    /// Paste only the text portion of the clipboard, skipping any
    /// image / file-path entries that `paste_intercept` would have
    /// turned into a pending image. Used to bypass the auto-image
    /// flow when a user has copied "image + caption" from a browser
    /// and wants only the caption.
    pub(super) fn paste_without_formatting(
        &mut self,
        _: &PasteWithoutFormatting,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        // `ClipboardItem::text()` concatenates every `ClipboardEntry::String`
        // and falls back to ExternalPaths if no string entry exists.
        // Image entries are skipped, which is exactly the "without
        // formatting" semantic we want.
        let Some(text) = clipboard.text() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        self.compose_editor.update(cx, |editor, cx| {
            editor.insert(&text, window, cx);
        });
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn paste_intercept(
        &mut self,
        _: &editor::actions::Paste,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        // Respect source-app priority: if the first entry is text, fall through
        // to the editor's default text-paste action. Returning without
        // consuming via `cx.stop_propagation()` lets the action propagate.
        let first = clipboard.entries().first();
        let has_image = matches!(
            first,
            Some(ClipboardEntry::Image(_)) | Some(ClipboardEntry::ExternalPaths(_))
        );
        if !has_image {
            return;
        }

        let mut new_images: Vec<PendingImage> = Vec::new();
        let mut next_idx = self.image_count_so_far;
        for entry in clipboard.into_entries() {
            if let ClipboardEntry::Image(image) = entry {
                next_idx += 1;
                let mime_type = image.format().mime_type().to_string();
                let data = base64::engine::general_purpose::STANDARD.encode(image.bytes());
                // Session-wide counter (`image_count_so_far`) instead of
                // pending-list length — the latter resets to 0 on submit
                // and made every fresh-compose paste show "image #1"
                // again. Now images carry a stable monotonic label
                // matching the user's "1, 2, 3 across the chat" model.
                let label = SharedString::from(format!("image #{next_idx}"));
                new_images.push(PendingImage {
                    mime_type,
                    data_base64: data,
                    label,
                });
            }
            // Other entries (paths, strings) — ignore for v1. File paths from
            // drag-drop are handled separately by handle_external_paths_drop.
        }

        if new_images.is_empty() {
            return;
        }

        let placeholder_text = new_images
            .iter()
            .map(|img| format!("[{}]", img.label))
            .collect::<Vec<_>>()
            .join(" ");
        self.image_count_so_far = next_idx;
        self.pending_images.extend(new_images);
        self.compose_editor.update(cx, |editor, cx| {
            editor.insert(&placeholder_text, window, cx);
            editor.insert(" ", window, cx);
        });
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn handle_external_paths_drop(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.0.is_empty() {
            return;
        }
        let workspace_root = self.workspace.upgrade().and_then(|workspace| {
            workspace
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|w| w.read(cx).abs_path().to_path_buf())
        });
        let mention_text = paths
            .0
            .iter()
            .map(|abs_path| {
                let display = workspace_root
                    .as_ref()
                    .and_then(|root| abs_path.strip_prefix(root).ok())
                    .map(|rel| rel.to_string_lossy().to_string())
                    .unwrap_or_else(|| abs_path.to_string_lossy().to_string());
                format!("@{display}")
            })
            .collect::<Vec<_>>()
            .join(" ");
        self.compose_editor.update(cx, |editor, cx| {
            editor.insert(&mention_text, window, cx);
            editor.insert(" ", window, cx);
        });
        let focus = self.compose_editor.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }
}
