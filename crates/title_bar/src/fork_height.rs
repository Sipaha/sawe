//! Fork-local title bar content-row height. Decoupled from upstream's
//! `platform_title_bar_height` so increasing our content row doesn't
//! also enlarge the platform window-controls row. Matches the project
//! toolbar row height (30 px) so the selected solution tab reads as
//! tightly as the selected project tab below it — a taller row left an
//! awkward gap under the active-tab highlight.

use gpui::{Pixels, px};

pub const FORK_TITLE_BAR_CONTENT_HEIGHT_PX: f32 = 30.0;

pub fn fork_title_bar_content_height() -> Pixels {
    px(FORK_TITLE_BAR_CONTENT_HEIGHT_PX)
}
