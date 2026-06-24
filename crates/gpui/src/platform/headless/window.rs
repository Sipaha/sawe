use crate::{
    AnyWindowHandle, Bounds, Capslock, DevicePixels, DispatchEventResult, GpuSpecs, Modifiers,
    Pixels, PlatformAtlas, PlatformDisplay, PlatformHeadlessRenderer, PlatformInput,
    PlatformInputHandler, PlatformWindow, Point, PromptButton, PromptLevel, RequestFrameOptions,
    Scene, Size, WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowControlArea,
    WindowParams,
};
use image::RgbaImage;
use parking_lot::Mutex;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::rc::Rc;
use std::sync::Arc;

/// Inner mutable state for a `HeadlessWindow`.
///
/// Held behind `Rc<Mutex<...>>` so the `Box<dyn PlatformWindow>` returned to
/// gpui can be `.clone()`d into the platform's `active_window` slot without
/// reaching back into the platform on every callback.
pub(crate) struct HeadlessWindowState {
    bounds: Bounds<Pixels>,
    scale_factor: f32,
    display: Rc<dyn PlatformDisplay>,
    handle: AnyWindowHandle,
    title: Option<String>,
    edited: bool,
    document_path: Option<std::path::PathBuf>,
    sprite_atlas: Arc<dyn PlatformAtlas>,
    renderer: Option<Box<dyn PlatformHeadlessRenderer>>,
    should_close_handler: Option<Box<dyn FnMut() -> bool>>,
    hit_test_window_control_callback: Option<Box<dyn FnMut() -> Option<WindowControlArea>>>,
    input_callback: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    request_frame_callback: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    active_status_change_callback: Option<Box<dyn FnMut(bool)>>,
    hover_status_change_callback: Option<Box<dyn FnMut(bool)>>,
    resize_callback: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    moved_callback: Option<Box<dyn FnMut()>>,
    appearance_changed_callback: Option<Box<dyn FnMut()>>,
    close_callback: Option<Box<dyn FnOnce()>>,
    input_handler: Option<PlatformInputHandler>,
    is_fullscreen: bool,
}

/// A real `PlatformWindow` for the native headless backend (no X / Wayland
/// display server, no on-screen swap chain). The window owns an optional
/// `PlatformHeadlessRenderer` (typically an offscreen wgpu pipeline on Linux
/// or a headless Metal pipeline on macOS) which is invoked by
/// `render_to_image` to satisfy `workspace.screenshot` over MCP.
///
/// Sibling of `gpui::TestWindow` (test-support only) — both implement the
/// same `PlatformWindow` surface, but `HeadlessWindow` is independent of the
/// test-harness prompt / input-simulation plumbing.
#[derive(Clone)]
pub struct HeadlessWindow(Rc<Mutex<HeadlessWindowState>>);

impl HeadlessWindow {
    /// Build a new headless window. `display` is used for `display()` /
    /// scale-factor reporting; `renderer`, when present, satisfies
    /// `render_to_image`.
    ///
    /// `scale_factor` mirrors what the host display would have reported; the
    /// default Linux/macOS dev path is 1.0 (rendering at logical pixels =
    /// device pixels keeps the readback path simple for screenshot tests).
    pub fn new(
        handle: AnyWindowHandle,
        params: WindowParams,
        display: Rc<dyn PlatformDisplay>,
        scale_factor: f32,
        renderer: Option<Box<dyn PlatformHeadlessRenderer>>,
    ) -> Self {
        let sprite_atlas: Arc<dyn PlatformAtlas> = match &renderer {
            Some(r) => r.sprite_atlas(),
            None => Arc::new(NoopAtlas),
        };
        Self(Rc::new(Mutex::new(HeadlessWindowState {
            bounds: params.bounds,
            scale_factor,
            display,
            handle,
            sprite_atlas,
            renderer,
            title: None,
            edited: false,
            document_path: None,
            should_close_handler: None,
            hit_test_window_control_callback: None,
            input_callback: None,
            request_frame_callback: None,
            active_status_change_callback: None,
            hover_status_change_callback: None,
            resize_callback: None,
            moved_callback: None,
            appearance_changed_callback: None,
            close_callback: None,
            input_handler: None,
            is_fullscreen: false,
        })))
    }

    /// The window handle this window was opened for. Useful for the
    /// platform's `active_window()` / `window_stack()` accessors.
    pub fn handle(&self) -> AnyWindowHandle {
        self.0.lock().handle
    }

    /// Deliver a synthetic input event to the window's `on_input` callback,
    /// the same way the real X11/Wayland clients deliver pointer/keyboard
    /// events. Returns `true` when the application's handler asked to stop
    /// propagation (matches `TestWindow::simulate_input` semantics).
    pub fn deliver_input(&self, event: PlatformInput) -> bool {
        let mut lock = self.0.lock();
        let Some(mut callback) = lock.input_callback.take() else {
            return false;
        };
        drop(lock);
        let result = callback(event);
        self.0.lock().input_callback = Some(callback);
        !result.propagate
    }

    /// Fire the `request_frame` callback so gpui runs its draw cycle
    /// (build elements → paint scene → swap `next_frame` → `rendered_frame`).
    /// The native headless platform's refresh timer calls this at
    /// ~60Hz so async state changes (file loads, git status arriving,
    /// etc.) get a chance to re-render — without it the editor would
    /// paint exactly once at startup and never again.
    ///
    /// Equivalent to X11Window::refresh.
    pub fn refresh(&self, options: RequestFrameOptions) {
        let mut lock = self.0.lock();
        let Some(mut callback) = lock.request_frame_callback.take() else {
            return;
        };
        drop(lock);
        callback(options);
        self.0.lock().request_frame_callback = Some(callback);
    }
}

impl HasWindowHandle for HeadlessWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        // No host window — the wgpu offscreen path does not need one
        // (it never creates a `wgpu::Surface`).
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl HasDisplayHandle for HeadlessWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl PlatformWindow for HeadlessWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.0.lock().bounds
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(self.bounds())
    }

    fn is_maximized(&self) -> bool {
        false
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds().size
    }

    fn resize(&mut self, size: Size<Pixels>) {
        let mut lock = self.0.lock();
        lock.bounds.size = size;
    }

    fn scale_factor(&self) -> f32 {
        self.0.lock().scale_factor
    }

    fn appearance(&self) -> WindowAppearance {
        WindowAppearance::Light
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.0.lock().display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        Point::default()
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers::default()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.0.lock().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.lock().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        // No interactive UI in headless mode — return None and let the
        // caller surface the prompt via logs / MCP if needed.
        None
    }

    fn activate(&self) {
        // No-op: the platform's `active_window` is set when `open_window`
        // returns; headless never reorders windows on the user's screen.
    }

    fn is_active(&self) -> bool {
        true
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn set_title(&mut self, title: &str) {
        self.0.lock().title = Some(title.to_owned());
    }

    fn set_app_id(&mut self, _app_id: &str) {}

    fn set_background_appearance(&self, _background: WindowBackgroundAppearance) {}

    fn set_edited(&mut self, edited: bool) {
        self.0.lock().edited = edited;
    }

    fn set_document_path(&self, path: Option<&std::path::Path>) {
        self.0.lock().document_path = path.map(|p| p.to_path_buf());
    }

    fn show_character_palette(&self) {}

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {
        let mut lock = self.0.lock();
        lock.is_fullscreen = !lock.is_fullscreen;
    }

    fn is_fullscreen(&self) -> bool {
        self.0.lock().is_fullscreen
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.lock().request_frame_callback = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.0.lock().input_callback = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.lock().active_status_change_callback = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.lock().hover_status_change_callback = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.lock().resize_callback = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.0.lock().moved_callback = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.lock().should_close_handler = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.0.lock().close_callback = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.0.lock().hit_test_window_control_callback = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.0.lock().appearance_changed_callback = Some(callback);
    }

    fn draw(&self, _scene: &Scene) {
        // No swap chain to present to. `render_to_image` rasterises the
        // caller-provided scene out-of-band (the gpui `Window` keeps the
        // last `rendered_frame.scene` for that), so we don't need to stash
        // anything here.
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.0.lock().sprite_atlas.clone()
    }

    fn render_to_image(&self, scene: &Scene) -> anyhow::Result<RgbaImage> {
        let mut state = self.0.lock();
        let scale_factor = state.scale_factor;
        let size = state.bounds.size;
        let Some(renderer) = state.renderer.as_mut() else {
            anyhow::bail!("HeadlessWindow has no PlatformHeadlessRenderer configured");
        };
        let device_size: Size<DevicePixels> = size.to_device_pixels(scale_factor);
        renderer.render_scene_to_image(scene, device_size)
    }

    #[cfg(target_os = "windows")]
    fn get_raw_handle(&self) -> windows::Win32::Foundation::HWND {
        unimplemented!("HeadlessWindow has no raw HWND")
    }

    fn show_window_menu(&self, _position: Point<Pixels>) {}

    fn start_window_move(&self) {}

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }
}

/// Minimal no-op atlas used when the headless window has no renderer
/// configured (e.g. on a platform without a `PlatformHeadlessRenderer`
/// implementation). Real rendering paths supply the renderer's own atlas;
/// this exists only so the `sprite_atlas()` accessor has something to
/// return without panicking.
struct NoopAtlas;

impl PlatformAtlas for NoopAtlas {
    fn get_or_insert_with<'a>(
        &self,
        _key: &crate::AtlasKey,
        _build: &mut dyn FnMut() -> anyhow::Result<
            Option<(Size<DevicePixels>, std::borrow::Cow<'a, [u8]>)>,
        >,
    ) -> anyhow::Result<Option<crate::AtlasTile>> {
        // Returning `Ok(None)` matches the contract — callers treat it as
        // "the sprite isn't ready yet" and skip the draw rather than crash.
        Ok(None)
    }

    fn remove(&self, _key: &crate::AtlasKey) {}
}
