use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use calloop::{EventLoop, LoopHandle};
use gpui_util::ResultExt;

use crate::linux::headless::HeadlessDisplay;
use crate::linux::{LinuxClient, LinuxCommon, LinuxKeyboardLayout};
use gpui::{
    AnyWindowHandle, Bounds, CursorStyle, DisplayId, HeadlessWindow, Pixels, PlatformDisplay,
    PlatformKeyboardLayout, PlatformWindow, Point, RequestFrameOptions, Size, WeakHeadlessWindow,
    WindowParams, px,
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
/// We retain a handle to the `HeadlessWindow` itself (not just the
/// `AnyWindowHandle`) so the refresh timer can call `window.refresh()`
/// directly — that fires the `request_frame` callback gpui registered, which
/// drives `Window::draw` → scene build → atlas upload → `rendered_frame.scene`
/// populated. Without the timer, gpui would paint once at startup and stay
/// frozen on async state changes (file loads, git status arriving, etc.), and
/// a subsequent `workspace.screenshot` would see a stale first-paint scene.
///
/// WEAK on purpose. A closed window is closed by gpui dropping its
/// `Box<dyn PlatformWindow>` — there is no platform-side close callback to
/// hook, the way X11 has `X11ClientStatePtr::drop_window`. Holding a clone
/// here made that drop a no-op, so nothing ever left `windows`: see
/// [`HeadlessClient::live_windows`].
struct TrackedWindow {
    handle: AnyWindowHandle,
    window: WeakHeadlessWindow,
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

        let (common, main_receiver, power_receiver) = LinuxCommon::new(event_loop.get_signal());

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
                    for window in client.live_windows() {
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

        handle
            .insert_source(power_receiver, |event, _, client: &mut HeadlessClient| {
                if let calloop::channel::Event::Msg(event) = event {
                    client.with_common(|common| common.handle_system_power_event(event));
                }
            })
            .ok();

        HeadlessClient(Rc::new(RefCell::new(HeadlessClientState {
            event_loop: Some(event_loop),
            _loop_handle: handle,
            common,
            windows: Vec::new(),
            display,
        })))
    }

    /// The windows gpui still owns, dropping any that have closed.
    ///
    /// This is the ONLY removal path. The headless platform has no close
    /// event: gpui closes a window by dropping its `Box<dyn PlatformWindow>`,
    /// so "is it still open" is exactly "does the weak handle still upgrade".
    /// Without this the refresh timer kept ticking closed windows forever —
    /// each tick firing a dead `request_frame` callback whose three
    /// `handle.update()` calls all failed, measured at 187 "window not found"
    /// ERROR lines per second, enough to rotate the log away in a minute, plus
    /// a leaked offscreen renderer per closed window.
    ///
    /// Returns owned windows and releases the borrow before the caller uses
    /// them: `refresh()` re-enters gpui, which can call back into this client.
    fn live_windows(&self) -> Vec<HeadlessWindow> {
        let mut state = self.0.borrow_mut();
        let mut live = Vec::with_capacity(state.windows.len());
        state
            .windows
            .retain(|tracked| match tracked.window.upgrade() {
                Some(window) => {
                    live.push(window);
                    true
                }
                None => false,
            });
        live
    }

    /// Drop closed windows without collecting the live ones — for the handle
    /// accessors, which must not answer with a window that is already gone.
    fn prune_closed_windows(&self) {
        self.0
            .borrow_mut()
            .windows
            .retain(|tracked| tracked.window.upgrade().is_some());
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
        let display = self.0.borrow().display.clone();
        (display.id() == id).then_some(display)
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
        // dropped action dispatches in headless mode — and answering with a
        // CLOSED window's handle drops them just as silently, which is what
        // tracking a never-removed clone used to do.
        self.prune_closed_windows();
        self.0.borrow().windows.last().map(|t| t.handle)
    }

    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        self.prune_closed_windows();
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
        // let us reach the request_frame callback. Weakly, so that the box
        // returned below is the only strong reference and closing the window
        // is enough to untrack it.
        self.0.borrow_mut().windows.push(TrackedWindow {
            handle,
            window: window.downgrade(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        AnyWindowHandle, Context, IntoElement, Render, WindowHandle, WindowId, WindowKind, div,
    };

    /// `WindowHandle::<V>::new` is the only public way to mint an
    /// `AnyWindowHandle`, and it needs a concrete view type. The window is
    /// never drawn here — only the platform-side bookkeeping is under test.
    struct TestRoot;

    impl Render for TestRoot {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            _cx: &mut Context<Self>,
        ) -> impl IntoElement {
            div()
        }
    }

    fn window_params() -> WindowParams {
        WindowParams {
            bounds: Bounds {
                origin: Point::new(px(0.0), px(0.0)),
                size: Size::new(px(320.0), px(240.0)),
            },
            titlebar: None,
            kind: WindowKind::Normal,
            is_movable: false,
            app_owns_titlebar_drag: false,
            is_resizable: false,
            is_minimizable: false,
            focus: false,
            show: false,
            icon: None,
            display_id: None,
            app_id: None,
            window_min_size: None,
        }
    }

    fn handle(id: u64) -> AnyWindowHandle {
        WindowHandle::<TestRoot>::new(WindowId::from(id)).into()
    }

    /// A window gpui has dropped must stop being tracked. Before this was
    /// enforced, `open_window` pushed a clone that nothing ever removed, so the
    /// 60Hz refresh timer kept firing the dead window's `request_frame`
    /// callback forever — three failed `handle.update()` per frame, measured at
    /// 187 "window not found" ERROR lines per second, which rotated the real
    /// log away in about a minute. `active_window()` also kept answering with
    /// the dead handle, so action dispatch had nowhere to go.
    #[test]
    fn a_window_gpui_dropped_stops_being_tracked() {
        let client = HeadlessClient::new();
        let first = handle(1);
        let second = handle(2);

        let first_window = client.open_window(first, window_params()).unwrap();
        let second_window = client.open_window(second, window_params()).unwrap();
        assert_eq!(client.window_stack().unwrap(), vec![first, second]);
        assert_eq!(client.active_window(), Some(second));

        // What `solutions.open` does: the replacement window is up and the old
        // one is closed. gpui drops its `Box<dyn PlatformWindow>`; nothing else
        // may keep the platform side alive.
        drop(first_window);
        assert_eq!(
            client.window_stack().unwrap(),
            vec![second],
            "a dropped window must leave the stack"
        );
        assert_eq!(client.active_window(), Some(second));
        assert_eq!(
            client.live_windows().len(),
            1,
            "the refresh timer must not keep ticking a dropped window"
        );

        drop(second_window);
        assert!(
            client.window_stack().is_none(),
            "no windows left means no window stack"
        );
        assert_eq!(client.active_window(), None);
        assert!(client.live_windows().is_empty());
    }
}
