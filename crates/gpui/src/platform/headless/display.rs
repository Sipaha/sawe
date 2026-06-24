use crate::{Bounds, DisplayId, Pixels, PlatformDisplay, Point, px};
use anyhow::Result;

/// A virtual display for the native headless platform. Single screen, default
/// 1920×1080 at scale 1.0 — enough for `Bounds::default_centered` /
/// `Workspace::render_titlebar` etc. to lay out a real-looking window
/// without an X server.
#[derive(Debug)]
pub struct HeadlessDisplay {
    id: DisplayId,
    uuid: uuid::Uuid,
    bounds: Bounds<Pixels>,
}

impl HeadlessDisplay {
    /// Default-size (1920×1080) headless display.
    pub fn new() -> Self {
        Self::with_bounds(Bounds::from_corners(
            Point::default(),
            Point::new(px(1920.), px(1080.)),
        ))
    }

    /// Headless display with a caller-specified `bounds`.
    pub fn with_bounds(bounds: Bounds<Pixels>) -> Self {
        Self {
            id: DisplayId(1),
            uuid: uuid::Uuid::new_v4(),
            bounds,
        }
    }
}

impl Default for HeadlessDisplay {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformDisplay for HeadlessDisplay {
    fn id(&self) -> DisplayId {
        self.id
    }

    fn uuid(&self) -> Result<uuid::Uuid> {
        Ok(self.uuid)
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }
}
