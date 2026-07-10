//! Detached "expanded compose" popup window lifecycle (open / close).
//! Relocated verbatim from the view root as `impl SolutionSessionView`
//! methods; `self`/fields stay owned by the struct.

use gpui::{AppContext as _, Context, Focusable, Window, px};

use super::SolutionSessionView;
use crate::expanded_compose::{
    EXPANDED_COMPOSE_DEFAULT_H, EXPANDED_COMPOSE_DEFAULT_W, EXPANDED_COMPOSE_HEIGHT_RATIO,
    ExpandedComposeWindowView,
};

impl SolutionSessionView {
    /// Opens the compose buffer in a detached OS popup window. Picked over
    /// a workspace modal so the user can keep reading the conversation /
    /// browse code while writing a long prompt. While the popup is alive
    /// the inline compose row swaps to a placeholder + Cancel button (see
    /// `render` for the swap). If the popup is already open this call
    /// just brings it to the foreground.
    pub(super) fn open_expanded_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handle) = self.expanded_window {
            // Already open — activate it and bail. If the window has been
            // closed behind our back (OS close button), the update fails
            // and we fall through to opening a fresh one.
            let alive = handle
                .update(cx, |_, window, _| {
                    window.activate_window();
                })
                .is_ok();
            if alive {
                return;
            }
            self.expanded_window = None;
        }
        let target = self.compose_editor.clone();
        let initial_text = target.read(cx).text(cx);
        let owner = cx.weak_entity();
        // Height tracks `EXPANDED_COMPOSE_HEIGHT_RATIO` of the *physical*
        // screen height. `display.bounds().size.height` is in logical
        // pixels (already physical / scale_factor on X11/Wayland), and
        // GPUI multiplies window bounds by scale_factor when handing them
        // to the platform — so a logical-pixel ratio comes out as the
        // same physical-pixel ratio on screen, regardless of HiDPI scale.
        // Manual origin math used to broke on multi-monitor / HiDPI mixes
        // (popup landed off-centre), so we hand off to the platform's
        // native centring via `WindowBounds::centered` — costs us
        // "non-primary monitor" placement on multi-display setups, but
        // wins us reliable centring everywhere else.
        let display_height = window
            .display(cx)
            .or_else(|| cx.primary_display())
            .map(|d| d.bounds().size.height)
            .unwrap_or(px(
                EXPANDED_COMPOSE_DEFAULT_H / EXPANDED_COMPOSE_HEIGHT_RATIO
            ));
        let size = gpui::Size {
            width: px(EXPANDED_COMPOSE_DEFAULT_W),
            height: display_height * EXPANDED_COMPOSE_HEIGHT_RATIO,
        };
        let bounds = gpui::WindowBounds::centered(size, cx);
        let opened = cx.open_window(
            gpui::WindowOptions {
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Edit prompt".into()),
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
                let view = cx.new(|cx| {
                    ExpandedComposeWindowView::new(
                        initial_text,
                        target.downgrade(),
                        owner,
                        window,
                        cx,
                    )
                });
                window.activate_window();
                let focus_handle = view.read(cx).editor.focus_handle(cx);
                focus_handle.focus(window, cx);
                // Closing via the OS title-bar X commits the draft —
                // hitting X on a long edit and losing the text was the
                // most surprising/punishing thing about an earlier
                // version. Cancel button stays as the explicit-discard
                // path. We do this by intercepting `should_close` and
                // running the save path before allowing the close;
                // returning `true` lets the framework finish closing
                // (which calls remove_window in the deferred close path).
                let weak = view.downgrade();
                window.on_window_should_close(cx, move |window, cx| {
                    if let Some(view) = weak.upgrade() {
                        view.update(cx, |this, cx| {
                            this.save(window, cx);
                        });
                    }
                    true
                });
                view
            },
        );
        match opened {
            Ok(handle) => self.expanded_window = Some(handle),
            Err(err) => log::error!("failed to open expanded compose window: {err:?}"),
        }
    }

    /// Closes the popup window without applying its text. Called from the
    /// inline Cancel button so users don't have to hunt the popup down on
    /// the desktop just to discard it. Handle is cleared either way (if
    /// the popup has already been closed externally, `update` errors and
    /// we just drop the stale handle).
    pub(super) fn close_expanded_compose(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.expanded_window.take() else {
            return;
        };
        handle
            .update(cx, |_, window, _| {
                window.remove_window();
            })
            .ok();
        cx.notify();
    }
}
