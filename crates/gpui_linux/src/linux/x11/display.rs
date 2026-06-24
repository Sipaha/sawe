use anyhow::Context as _;
use uuid::Uuid;
use x11rb::{
    connection::Connection as _, protocol::randr::ConnectionExt as _, xcb_ffi::XCBConnection,
};

use gpui::{Bounds, DisplayId, Pixels, PlatformDisplay, Point, Size, px};

#[derive(Debug)]
pub(crate) struct X11Display {
    x_screen_index: usize,
    bounds: Bounds<Pixels>,
    uuid: Uuid,
}

impl X11Display {
    pub(crate) fn new(
        xcb: &XCBConnection,
        scale_factor: f32,
        x_screen_index: usize,
    ) -> anyhow::Result<Self> {
        let screen = xcb
            .setup()
            .roots
            .get(x_screen_index)
            .with_context(|| format!("No screen found with index {x_screen_index}"))?;
        // On multi-monitor X11 the X screen covers ALL monitors as one big
        // logical surface (e.g. 3840×1080 across two side-by-side 1920×1080
        // panels). Centering relative to that ends up exactly on the seam
        // between the two physical monitors. XRandr exposes per-monitor
        // info — query it and prefer the primary monitor's bounds; fall
        // back to the screen bounds when XRandr is unavailable (nested X
        // server, screen with no active output, etc.).
        let bounds =
            primary_monitor_bounds(xcb, screen.root, scale_factor).unwrap_or_else(|| Bounds {
                origin: Default::default(),
                size: Size {
                    width: px(screen.width_in_pixels as f32 / scale_factor),
                    height: px(screen.height_in_pixels as f32 / scale_factor),
                },
            });
        Ok(Self {
            x_screen_index,
            bounds,
            uuid: Uuid::from_bytes([0; 16]),
        })
    }
}

fn primary_monitor_bounds(
    xcb: &XCBConnection,
    root: x11rb::protocol::xproto::Window,
    scale_factor: f32,
) -> Option<Bounds<Pixels>> {
    let reply = xcb.randr_get_monitors(root, true).ok()?.reply().ok()?;
    let monitor = reply
        .monitors
        .iter()
        .find(|m| m.primary)
        .or_else(|| reply.monitors.first())?;
    Some(Bounds {
        origin: Point {
            x: px(monitor.x as f32 / scale_factor),
            y: px(monitor.y as f32 / scale_factor),
        },
        size: Size {
            width: px(monitor.width as f32 / scale_factor),
            height: px(monitor.height as f32 / scale_factor),
        },
    })
}

impl PlatformDisplay for X11Display {
    fn id(&self) -> DisplayId {
        DisplayId::new(self.x_screen_index as u64)
    }

    fn uuid(&self) -> anyhow::Result<Uuid> {
        Ok(self.uuid)
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }
}
