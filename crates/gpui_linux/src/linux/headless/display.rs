use gpui::{Bounds, DisplayId, Pixels, PlatformDisplay, Point, px};

/// Synthetic 1920×1080 display the native headless platform reports.
///
/// We don't reuse `gpui::HeadlessDisplay` here because `gpui_linux`'s
/// `PlatformDisplay` needs to be a concrete type with `Debug` for the
/// LinuxClient API surface and a stable `DisplayId(1)`.
#[derive(Debug)]
pub(crate) struct HeadlessDisplay {
    id: DisplayId,
    uuid: uuid::Uuid,
    bounds: Bounds<Pixels>,
}

impl HeadlessDisplay {
    pub fn new() -> Self {
        Self {
            id: DisplayId::new(1),
            uuid: uuid::Uuid::new_v4(),
            bounds: Bounds::from_corners(Point::default(), Point::new(px(1920.), px(1080.))),
        }
    }
}

impl PlatformDisplay for HeadlessDisplay {
    fn id(&self) -> DisplayId {
        self.id
    }

    fn uuid(&self) -> anyhow::Result<uuid::Uuid> {
        Ok(self.uuid)
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }
}
