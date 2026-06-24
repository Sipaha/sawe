<<<<<<< ours
use crate::{
    NewFile, Open, OpenMode, PathList, RecentWorkspace, SerializedWorkspaceLocation,
    ToggleWorkspaceSidebar, Workspace, WorkspaceSettings,
    item::{Item, ItemEvent},
    persistence::WorkspaceDb,
};
use agent_settings::AgentSettings;
use git::Clone as GitClone;
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    ParentElement, Render, Styled, Task, TaskExt, Window, actions,
=======
//! `WelcomeWindow` — the SPK Editor launcher.
//!
//! Welcome is a top-level window in its own right (root view =
//! `WelcomeWindow`), NOT a workspace tab. The previous design embedded
//! a `WelcomePage` Item inside a regular `Workspace`, which forced a
//! pile of conditional gates (hide dock strips, hide status bar, hide
//! project panel, close all docks, …) to keep the launcher chrome-
//! free. Now the launcher window doesn't share any structure with a
//! Solution workspace, so chrome can't accidentally bleed in.
//!
//! Sibling crates (notably `solutions_ui`) plug content into the
//! launcher via `register_welcome_section`, which keeps the
//! `workspace → solutions` direction unchanged.
//!
//! Opening the window is the responsibility of the `onboarding`
//! crate (which has the `ShowWelcome` action handler) or anyone else
//! who calls `WelcomeWindow::open(...)`.

use crate::AppState;
use gpui::{
    AnyElement, AnyWindowHandle, App, Context, FocusHandle, Focusable, Global, InteractiveElement,
    ParentElement, Render, Styled, Window, WindowDecorations, WindowHandle, WindowKind, actions,
    px,
>>>>>>> theirs
};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

<<<<<<< ours
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{DefaultOpenBehavior, Settings};
use ui::{ButtonLike, Divider, DividerColor, KeyBinding, Vector, VectorName, prelude::*};
use util::ResultExt;
use zed_actions::{
    Extensions, OpenKeymap, OpenOnboarding, OpenSettings, assistant::ToggleFocus, command_palette,
};

#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = welcome)]
#[serde(transparent)]
pub struct OpenRecentProject {
    pub index: usize,
}
=======
use ui::{ContextMenu, Divider, DividerColor, PopoverMenu, Vector, VectorName, prelude::*};
use zed_actions::{Extensions, OpenKeymap, OpenSettings};
>>>>>>> theirs

actions!(
    zed,
    [
        /// Show the SPK Editor welcome / launcher window.
        ShowWelcome
    ]
);

/// Header above a section list, used by registered sections via
/// `SectionHeader::new(...)`.
#[derive(IntoElement)]
pub struct SectionHeader {
    title: SharedString,
}

impl SectionHeader {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
        }
    }
}

impl RenderOnce for SectionHeader {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        h_flex()
            .px_1()
            .mb_2()
            .gap_2()
            .child(
                Label::new(self.title.to_ascii_uppercase())
                    .buffer_font(cx)
                    .color(Color::Muted)
                    .size(LabelSize::XSmall),
            )
            .child(Divider::horizontal().color(DividerColor::BorderVariant))
    }
}

/// Closure that renders an extra section into the launcher. Returns
/// `None` when the section has nothing to show this frame.
///
/// Lives behind an `Rc` so the registry can hand out clones that
/// outlive the borrow on the registry itself (rendering iterates
/// registered sections one at a time and each call needs `&mut App`).
pub type WelcomeSectionRenderer = Rc<dyn Fn(&mut App) -> Option<AnyElement>>;

#[derive(Default)]
struct WelcomeSectionRegistry {
    sections: RefCell<Vec<WelcomeSectionRenderer>>,
}

impl Global for WelcomeSectionRegistry {}

/// Register an extra section to render in the launcher window. Used
/// by sibling crates (e.g. `solutions_ui`) to plug Recent Solutions
/// in without `workspace` having to depend on `solutions`.
pub fn register_welcome_section(
    cx: &mut App,
    renderer: impl Fn(&mut App) -> Option<AnyElement> + 'static,
) {
    if cx.try_global::<WelcomeSectionRegistry>().is_none() {
        cx.set_global(WelcomeSectionRegistry::default());
    }
    cx.global::<WelcomeSectionRegistry>()
        .sections
        .borrow_mut()
        .push(Rc::new(renderer));
}

fn render_registered_sections(cx: &mut App) -> Vec<AnyElement> {
    let renderers: Vec<WelcomeSectionRenderer> = cx
        .try_global::<WelcomeSectionRegistry>()
        .map(|reg| reg.sections.borrow().iter().cloned().collect())
        .unwrap_or_default();
    renderers
        .into_iter()
        .filter_map(|render| render(cx))
        .collect()
}

/// Root view of the SPK Editor launcher window. Owns its own focus
/// handle and renders the sections registered via
/// `register_welcome_section`.
pub struct WelcomeWindow {
    focus_handle: FocusHandle,
<<<<<<< ours
    fallback_to_recent_projects: bool,
    recent_workspaces: Option<Vec<RecentWorkspace>>,
=======
    _appearance_subscription: gpui::Subscription,
>>>>>>> theirs
}

impl WelcomeWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        cx.on_focus(&focus_handle, window, |_, _, cx| cx.notify())
            .detach();

        // Push the OS appearance into the `SystemAppearance` global
        // up front — without this the launcher paints with the
        // default Light value and picks the wrong theme variant on a
        // dark system.
        let appearance_subscription = theme_settings::track_window_appearance(window, cx);

        Self {
            focus_handle,
            _appearance_subscription: appearance_subscription,
        }
    }

<<<<<<< ours
    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_next(cx);
        cx.notify();
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_prev(cx);
        cx.notify();
    }

    fn open_recent_project(
        &mut self,
        action: &OpenRecentProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(recent_workspaces) = &self.recent_workspaces {
            if let Some(workspace) = recent_workspaces.get(action.index) {
                let is_local = matches!(workspace.location, SerializedWorkspaceLocation::Local);

                if is_local {
                    let paths = workspace.paths.paths().to_vec();
                    let open_mode = match WorkspaceSettings::get_global(cx).default_open_behavior {
                        DefaultOpenBehavior::ExistingWindow => OpenMode::Activate,
                        DefaultOpenBehavior::NewWindow => OpenMode::NewWindow,
                    };
                    self.workspace
                        .update(cx, |workspace, cx| {
                            workspace
                                .open_workspace_for_paths(open_mode, paths, window, cx)
                                .detach_and_log_err(cx);
                        })
                        .log_err();
                } else {
                    use zed_actions::OpenRecent;
                    window.dispatch_action(OpenRecent::default().boxed_clone(), cx);
                }
            }
=======
    /// Opens (or focuses, if one already exists) the launcher window.
    /// Centred 720×720 by default — wider tends to feel half-empty
    /// because the content column is 40rem.
    pub fn open(
        app_state: Arc<AppState>,
        cx: &mut App,
    ) -> anyhow::Result<WindowHandle<WelcomeWindow>> {
        // Reuse the existing welcome window when one is already open
        // — opening a second copy is never what the user wants.
        if let Some(existing) = find_existing(cx) {
            existing
                .update(cx, |_, window, _| window.activate_window())
                .ok();
            return Ok(existing);
>>>>>>> theirs
        }

        let bounds = gpui::WindowBounds::centered(
            gpui::Size {
                width: px(720.),
                height: px(720.),
            },
            cx,
        );
        // Start from the same Workspace options to inherit the
        // theme-aware `window_background` (otherwise the launcher
        // falls back to the OS default — white on most Linux setups).
        // Then ask for *server-side* decorations: the launcher has no
        // custom titlebar item to host close / minimize / maximize
        // buttons, so the OS draws a plain title bar with those
        // controls for us. The Workspace builds its own titlebar
        // (`titlebar_item`) and uses client-side decorations to fit
        // tabs / project name in there — that machinery isn't here.
        let mut options = (app_state.build_window_options)(None, cx);
        options.window_bounds = Some(bounds);
        options.show = true;
        options.focus = true;
        options.kind = WindowKind::Normal;
        options.window_decorations = Some(WindowDecorations::Server);
        options.titlebar = None;

        let window = cx.open_window(options, |window, cx| cx.new(|cx| Self::new(window, cx)))?;
        Ok(window)
    }

    fn render_configure_menu(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus_handle.clone();
        PopoverMenu::new("welcome-configure-menu")
            .trigger(
                IconButton::new("welcome-configure-trigger", IconName::Settings)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(ui::Tooltip::text("Configure")),
            )
            .menu(move |window, cx| {
                let focus = focus.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.action("Open Settings", Box::new(OpenSettings))
                        .action("Customize Keymaps", Box::new(OpenKeymap))
                        .action(
                            "Explore Extensions",
                            Box::new(Extensions {
                                category_filter: None,
                                id: None,
                            }),
                        )
                        .context(focus)
                }))
            })
            .anchor(gpui::Anchor::TopRight)
    }
}

<<<<<<< ours
impl Render for WelcomePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (first_section, second_section) = CONTENT;
        let first_section_entries = first_section.entries.len();
        let mut next_tab_index = first_section_entries + second_section.entries.len();

        let ai_enabled = AgentSettings::get_global(cx).enabled(cx);

        let recent_projects = self
            .recent_workspaces
            .as_ref()
            .into_iter()
            .flatten()
            .take(5)
            .enumerate()
            .map(|(index, workspace)| {
                self.render_recent_project(
                    index,
                    first_section_entries + index,
                    &workspace.location,
                    &workspace.identity_paths,
                )
            })
            .collect::<Vec<_>>();

        let showing_recent_projects =
            self.fallback_to_recent_projects && !recent_projects.is_empty();
        let second_section = if showing_recent_projects {
            self.render_recent_project_section(recent_projects)
                .into_any_element()
        } else {
            second_section
                .render(first_section_entries, &self.focus_handle)
                .into_any_element()
        };

        let welcome_label = if self.fallback_to_recent_projects {
            "Welcome back to Zed"
        } else {
            "Welcome to Zed"
        };

=======
impl Render for WelcomeWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Wire the UI font + theme into this window the same way
        // `Workspace::render` does. Without this, the launcher draws
        // on top of the default platform theme (white background),
        // since neither `Workspace` nor anyone else has set the
        // window's text style or background for it.
        let ui_font = theme_settings::setup_ui_font(window, cx);
        let theme = cx.theme().clone();
        let colors = theme.colors();
>>>>>>> theirs
        h_flex()
            .key_context("Welcome")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .font(ui_font)
            .bg(colors.editor_background)
            .text_color(colors.text)
            .justify_center()
            .child(
                v_flex()
                    .id("welcome-content")
                    .px_8()
                    .py_8()
                    .w(rems(40.))
                    .h_full()
                    .gap_6()
                    .overflow_y_scroll()
                    .child(
                        h_flex()
                            .w_full()
                            .justify_between()
                            .items_center()
                            .mb_4()
                            .gap_4()
                            .child(
                                h_flex()
                                    .gap_4()
                                    .items_center()
                                    .child(Vector::square(VectorName::SpkLogo, rems_from_px(45.)))
                                    .child(
                                        v_flex()
                                            .child(Headline::new("Welcome to SPK Editor"))
                                            .child(
                                                Label::new("The editor for what's next")
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted)
                                                    .italic(),
                                            ),
                                    ),
                            )
                            .child(self.render_configure_menu(cx)),
                    )
                    .children(render_registered_sections(cx)),
            )
    }
}

impl Focusable for WelcomeWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Returns the first window in the app whose root view is a
/// `WelcomeWindow`, if any. Used to deduplicate the launcher window so
/// repeatedly invoking `ShowWelcome` doesn't pile up extra copies.
pub fn find_existing(cx: &App) -> Option<WindowHandle<WelcomeWindow>> {
    for handle in cx.windows() {
        if let Some(welcome) = handle.downcast::<WelcomeWindow>() {
            return Some(welcome);
        }
    }
    None
}

/// Convenience for callers that just need to know whether *some*
/// welcome window is up — useful in code paths that want to decide
/// between "open new" and "do nothing".
pub fn any_welcome_window_open(cx: &App) -> bool {
    find_existing(cx).is_some()
}

/// Returns true when the given window handle is a launcher window
/// (root view = `WelcomeWindow`). Used by callers that have an
/// `AnyWindowHandle` and want to special-case the launcher.
pub fn is_welcome_window(handle: AnyWindowHandle) -> bool {
    handle.downcast::<WelcomeWindow>().is_some()
}
