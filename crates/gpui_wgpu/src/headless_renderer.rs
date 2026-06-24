//! Headless wgpu renderer: a `PlatformHeadlessRenderer` impl that owns a
//! surfaceless `WgpuRenderer` and forwards `render_scene_to_image` /
//! `sprite_atlas` to it. Used by the native Linux/FreeBSD headless platform
//! (no X / Wayland connection, no on-screen swap chain).
//!
//! See `docs/architecture/decisions/0002-native-headless-platform.md` and
//! `crates/gpui/src/platform/headless/window.rs` for the rest of the wiring.

use crate::WgpuRenderer;
use gpui::{DevicePixels, PlatformAtlas, PlatformHeadlessRenderer, Scene, Size};
use std::sync::Arc;

/// Default offscreen target dimensions. Matches the headless display size
/// in `gpui::HeadlessDisplay::new` so the renderer's `surface_config`
/// reflects the same viewport the platform reports.
pub const DEFAULT_OFFSCREEN_WIDTH: u32 = 1920;
pub const DEFAULT_OFFSCREEN_HEIGHT: u32 = 1080;

/// `PlatformHeadlessRenderer` adapter around `WgpuRenderer::new_offscreen`.
pub struct WgpuHeadlessRenderer {
    inner: WgpuRenderer,
}

impl WgpuHeadlessRenderer {
    /// Build a renderer targeting an offscreen `width × height` (device-pixel)
    /// surface. Use `DEFAULT_OFFSCREEN_WIDTH × DEFAULT_OFFSCREEN_HEIGHT` if
    /// you don't have a more specific size from the caller.
    pub fn new(width: u32, height: u32) -> anyhow::Result<Self> {
        let inner = WgpuRenderer::new_offscreen(width, height)?;
        Ok(Self { inner })
    }
}

impl PlatformHeadlessRenderer for WgpuHeadlessRenderer {
    fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
    ) -> anyhow::Result<image::RgbaImage> {
        // `render_to_image` uses the renderer's stored `surface_config` size.
        // If the requested size differs from the offscreen target, grow the
        // target first so the produced image matches the caller's expectation
        // (the workspace screenshot path passes the window's device-pixel size).
        let width = (size.width.0 as u32).max(1);
        let height = (size.height.0 as u32).max(1);
        self.inner.update_drawable_size(size);
        let _ = (width, height);
        self.inner.render_to_image(scene)
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.inner.sprite_atlas().clone()
    }
}
