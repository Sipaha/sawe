//! Native headless platform primitives — a real `PlatformWindow` /
//! `PlatformDisplay` pair that runs without an X server or compositor.
//!
//! The window is non-test sibling of `TestWindow` (intentional code
//! duplication — `TestWindow` is wired to `TestPlatform`'s prompt /
//! input-simulation surface, which has no place in a real headless run).
//! When a `PlatformHeadlessRenderer` is provided (typically
//! `gpui_wgpu::WgpuHeadlessRenderer` on Linux, `gpui_macos::MetalHeadlessRenderer`
//! on macOS — gated behind `test-support` there for now), `render_to_image`
//! drives it; otherwise it returns a bail.
mod display;
mod window;

pub use display::HeadlessDisplay;
pub use window::HeadlessWindow;
