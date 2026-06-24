//! UI for the Remote Control surface: right-aligned status-bar entry
//! (`RemoteControlStatusItem`) + workspace modal (`RemoteControlModal`).
//!
//! R-1 wires only the UI + state plumbing — no listener, no QR.

mod modal;
mod qr_popover;
mod status_item;

pub use modal::RemoteControlModal;
pub use qr_popover::QrPopover;
pub use status_item::RemoteControlStatusItem;

use gpui::App;

pub fn init(_cx: &mut App) {
    // R-1: no global init needed (no actions, no global registration).
    // The status-bar item is constructed per-workspace from `zed::zed`,
    // and the modal is toggled by the status item's click handler.
}
