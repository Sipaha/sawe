//! The one askpass bridge for this crate.
//!
//! Any git operation that talks to a remote (push / pull / fetch, and
//! the server-side branch delete spelled as a push) can trip the
//! credential helper. Without a delegate the git subprocess blocks
//! forever on a prompt that has no terminal to appear on, so every call
//! site that spawns one of those operations needs to hand it a delegate
//! that opens [`AskPassModal`] over the workspace.
//!
//! This used to be copy-pasted per call site — `GitPanel`,
//! `PushDialog`, `worktree_service` and the commit context menu each
//! carried their own byte-for-byte copy, three of which admitted as
//! much in a doc comment. They are all this function now.

use askpass::AskPassDelegate;
use gpui::{App, SharedString, WeakEntity, Window};
use workspace::Workspace;

use crate::askpass_modal::AskPassModal;

/// Build an [`AskPassDelegate`] that shows `operation`'s credential
/// prompt as a modal over `workspace`.
///
/// `operation` is the human-readable command the prompt is attributed
/// to (`"git push origin"`), not something that gets executed.
///
/// Takes `&mut App` rather than a `Context<T>` so it serves entity
/// methods (which deref to `App`) and free functions alike.
pub fn askpass_delegate(
    workspace: WeakEntity<Workspace>,
    operation: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) -> AskPassDelegate {
    let operation = operation.into();
    let window = window.window_handle();
    AskPassDelegate::new(&mut cx.to_async(), move |prompt, tx, cx| {
        let shown = window.update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    AskPassModal::new(operation.clone(), prompt.into(), tx, window, cx)
                });
            })
        });
        // Both failures say the same thing: the window or the workspace
        // went away before git asked for credentials, so nobody can
        // answer. That leaves the git subprocess blocked until it gives
        // up on its own, which is worth a line in the log rather than
        // silence.
        if let Err(error) = shown.and_then(|inner| inner) {
            log::warn!("askpass prompt for `{operation}` could not be shown: {error}");
        }
    })
}
