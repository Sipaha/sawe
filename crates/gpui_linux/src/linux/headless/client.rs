use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use calloop::{EventLoop, LoopHandle};
use util::ResultExt;

use crate::linux::headless::HeadlessDisplay;
use crate::linux::{LinuxClient, LinuxCommon, LinuxKeyboardLayout};
use gpui::{
    AnyWindowHandle, Bounds, CursorStyle, DisplayId, HeadlessWindow, Pixels, PlatformDisplay,
    PlatformKeyboardLayout, PlatformWindow, Point, RequestFrameOptions, Size, WindowParams, px,
};

/// Default canonical viewport for the native headless platform. Matches
/// `gpui_wgpu::{DEFAULT_OFFSCREEN_WIDTH, DEFAULT_OFFSCREEN_HEIGHT}` and the
/// size baked into `HeadlessDisplay::new`. Overridable via env vars for
/// sized-canvas tests (e.g. mobile-narrow simulation):
///   - `SPK_HEADLESS_WIDTH=1280`
///   - `SPK_HEADLESS_HEIGHT=720`
const DEFAULT_HEADLESS_WIDTH: f32 = 1920.0;
const DEFAULT_HEADLESS_HEIGHT: f32 = 1080.0;

fn headless_window_bounds(display: &Rc<dyn PlatformDisplay>) -> Bounds<Pixels> {
    let env_or = |key: &str, default: f32| -> f32 {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(default)
    };
    // Prefer env override; fall back to the display's reported bounds (set
    // by `HeadlessDisplay::new`); ultimate fallback hard-codes 1920×1080.
    let display_bounds = display.bounds();
    let default_w = if f32::from(display_bounds.size.width) > 0.0 {
        f32::from(display_bounds.size.width)
    } else {
        DEFAULT_HEADLESS_WIDTH
    };
    let default_h = if f32::from(display_bounds.size.height) > 0.0 {
        f32::from(display_bounds.size.height)
    } else {
        DEFAULT_HEADLESS_HEIGHT
    };
    let width = env_or("SPK_HEADLESS_WIDTH", default_w);
    let height = env_or("SPK_HEADLESS_HEIGHT", default_h);
    Bounds {
        origin: Point::new(px(0.0), px(0.0)),
        size: Size::new(px(width), px(height)),
    }
}

#[cfg(feature = "x11")]
use gpui_wgpu::{DEFAULT_OFFSCREEN_HEIGHT, DEFAULT_OFFSCREEN_WIDTH, WgpuHeadlessRenderer};

/// One open window's tracked state in `HeadlessClient`.
///
/// We retain the `HeadlessWindow` itself (not just the `AnyWindowHandle`) so
/// the refresh timer can call `window.refresh()` directly — that fires the
/// `request_frame` callback gpui registered, which drives `Window::draw` →
/// scene build → atlas upload → `rendered_frame.scene` populated. Without
/// the timer, gpui would paint once at startup and stay frozen on async
/// state changes (file loads, git status arriving, etc.), and a subsequent
/// `workspace.screenshot` would see a stale first-paint scene.
struct TrackedWindow {
    handle: AnyWindowHandle,
    window: HeadlessWindow,
}

pub struct HeadlessClientState {
    pub(crate) _loop_handle: LoopHandle<'static, HeadlessClient>,
    pub(crate) event_loop: Option<calloop::EventLoop<'static, HeadlessClient>>,
    pub(crate) common: LinuxCommon,
    /// Open windows, in z-order (single-window today, kept as a Vec so adding
    /// multi-window later is a one-line change).
    windows: Vec<TrackedWindow>,
    /// Cached display so multiple `displays()` calls return the same `Rc`.
    display: Rc<dyn PlatformDisplay>,
}

#[derive(Clone)]
pub(crate) struct HeadlessClient(Rc<RefCell<HeadlessClientState>>);

impl HeadlessClient {
    pub(crate) fn new() -> Self {
        let event_loop = EventLoop::try_new().unwrap();

        let (common, main_receiver) = LinuxCommon::new(event_loop.get_signal());

        let handle = event_loop.handle();

        handle
            .insert_source(main_receiver, |event, _, _: &mut HeadlessClient| {
                if let calloop::channel::Event::Msg(runnable) = event {
                    runnable.run();
                }
            })
            .ok();

        // ~60Hz refresh — mirrors the X11 client's loop. Drives
        // `HeadlessWindow::refresh()` on every open window so gpui's
        // draw cycle fires whenever entities `cx.notify()`. Without
        // this the editor paints once and stays frozen — async state
        // changes (file loads, git status arriving) never re-render.
        let refresh_rate = Duration::from_millis(16);
        handle
            .insert_source(
                calloop::timer::Timer::immediate(),
                move |mut instant, (), client: &mut HeadlessClient| {
                    let windows: Vec<HeadlessWindow> = client
                        .0
                        .borrow()
                        .windows
                        .iter()
                        .map(|tracked| tracked.window.clone())
                        .collect();
                    for window in windows {
                        window.refresh(RequestFrameOptions {
                            require_presentation: false,
                            force_render: false,
                        });
                    }
                    let now = std::time::Instant::now();
                    while instant < now {
                        instant += refresh_rate;
                    }
                    calloop::timer::TimeoutAction::ToInstant(instant)
                },
            )
            .expect("Failed to register headless refresh timer");

        let display: Rc<dyn PlatformDisplay> = Rc::new(HeadlessDisplay::new());

        HeadlessClient(Rc::new(RefCell::new(HeadlessClientState {
            event_loop: Some(event_loop),
            _loop_handle: handle,
            common,
            windows: Vec::new(),
            display,
        })))
    }
}

impl LinuxClient for HeadlessClient {
    fn with_common<R>(&self, f: impl FnOnce(&mut LinuxCommon) -> R) -> R {
        f(&mut self.0.borrow_mut().common)
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(LinuxKeyboardLayout::new("unknown".into()))
    }

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.0.borrow().display.clone()]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.0.borrow().display.clone())
    }

    fn display(&self, id: DisplayId) -> Option<Rc<dyn PlatformDisplay>> {
        let state = self.0.borrow();
        if state.display.id() == id {
            Some(state.display.clone())
        } else {
            None
        }
    }

    #[cfg(feature = "screen-capture")]
    fn screen_capture_sources(
        &self,
    ) -> futures::channel::oneshot::Receiver<anyhow::Result<Vec<Rc<dyn gpui::ScreenCaptureSource>>>>
    {
        let (tx, rx) = futures::channel::oneshot::channel();
        tx.send(Err(anyhow::anyhow!(
            "Headless mode does not support screen capture."
        )))
        .ok();
        rx
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        // Last-opened window, matching the X11/Wayland behaviour where the
        // most recently focused window is the "active" one. `dispatch_action`
        // routes through here, so returning `None` (the old stub) silently
        // dropped action dispatches in headless mode.
        self.0.borrow().windows.last().map(|t| t.handle)
    }

    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        let state = self.0.borrow();
        if state.windows.is_empty() {
            None
        } else {
            Some(state.windows.iter().map(|t| t.handle).collect())
        }
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        params: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        let display = self.0.borrow().display.clone();

        // Override the caller-supplied bounds with the full HeadlessDisplay
        // surface. Workspace persistence restores the previous on-screen
        // bounds (e.g. 1379×852 from the user's prior interactive session),
        // which makes agent-driven layout assertions size-dependent. In
        // headless mode every run gets the same canonical viewport so
        // pixel-coordinate assertions, screenshot diffs, and clickable
        // layouts are deterministic. Optionally overridable via env vars
        // for sized-canvas tests (e.g. mobile-narrow simulation).
        let bounds = headless_window_bounds(&display);
        let mut params = params;
        params.bounds = bounds;

        // The wgpu offscreen renderer is the *only* headless renderer the fork
        // ships. Gated on the `x11` feature for build-graph hygiene (that's
        // what brings `gpui_wgpu` in on Linux); leaving it off means the
        // user explicitly asked for a no-GPU build, in which case headless
        // open_window has to bail.
        #[cfg(feature = "x11")]
        let renderer: Option<Box<dyn gpui::PlatformHeadlessRenderer>> = {
            let width =
                (params.bounds.size.width.as_f32() as u32).clamp(1, DEFAULT_OFFSCREEN_WIDTH);
            let height =
                (params.bounds.size.height.as_f32() as u32).clamp(1, DEFAULT_OFFSCREEN_HEIGHT);
            match WgpuHeadlessRenderer::new(width, height) {
                Ok(r) => Some(Box::new(r) as Box<dyn gpui::PlatformHeadlessRenderer>),
                Err(e) => {
                    log::warn!(
                        "Headless wgpu renderer init failed ({e}); proceeding without offscreen \
                         rendering — `workspace.screenshot` will return an error."
                    );
                    None
                }
            }
        };
        #[cfg(not(feature = "x11"))]
        let renderer: Option<Box<dyn gpui::PlatformHeadlessRenderer>> = {
            log::warn!(
                "Headless build has no wgpu feature; offscreen rendering disabled. \
                 Build with the `x11` feature to enable `workspace.screenshot`."
            );
            None
        };

        let window = HeadlessWindow::new(
            handle, params, display, /* scale_factor */ 1.0, renderer,
        );

        // Track the window (not just the handle) so the refresh timer can
        // call `refresh()` on it directly — `AnyWindowHandle` alone won't
        // let us reach the request_frame callback.
        self.0.borrow_mut().windows.push(TrackedWindow {
            handle,
            window: window.clone(),
        });

        Ok(Box::new(window))
    }

    fn compositor_name(&self) -> &'static str {
        "headless"
    }

    fn set_cursor_style(&self, _style: CursorStyle) {}

    fn open_uri(&self, _uri: &str) {}

    fn reveal_path(&self, _path: std::path::PathBuf) {}

    fn write_to_primary(&self, _item: gpui::ClipboardItem) {}

    fn write_to_clipboard(&self, _item: gpui::ClipboardItem) {}

    fn read_from_primary(&self) -> Option<gpui::ClipboardItem> {
        None
    }

    fn read_from_clipboard(&self) -> Option<gpui::ClipboardItem> {
        None
    }

    fn run(&self) {
        let mut event_loop = self
            .0
            .borrow_mut()
            .event_loop
            .take()
            .expect("App is already running");

        event_loop.run(None, &mut self.clone(), |_| {}).log_err();
    }
}
