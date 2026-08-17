mod image_info;
mod image_viewer_settings;

use std::path::Path;

use anyhow::Context as _;
use editor::{EditorSettings, RevealInFileManager, items::entry_git_aware_label_color};
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Bounds, Context, DispatchPhase, Element, ElementId, Entity, EventEmitter,
    FocusHandle, Focusable, Font, GlobalElementId, InspectorElementId, InteractiveElement,
    IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, PinchEvent, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, Style, Styled,
    Task, WeakEntity, Window, actions, checkerboard, div, img, point, px, size,
};
use language::File as _;
use persistence::ImageViewerDb;
use project::{ImageItem, Project, ProjectPath, image_store::ImageItemEvent};
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::{Tooltip, prelude::*};
use util::paths::PathExt;
use workspace::{
    ItemId, ItemSettings, Pane, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace,
    WorkspaceId, delete_unloaded_items,
    invalid_item_view::InvalidItemView,
    item::{HighlightedText, Item, ItemHandle, ProjectItem, SerializableItem, TabContentParams},
};

pub use crate::image_info::*;
pub use crate::image_viewer_settings::*;

actions!(
    image_viewer,
    [
        /// Zoom in the image.
        ZoomIn,
        /// Zoom out the image.
        ZoomOut,
        /// Reset the zoom to the default, which is fitting the image to the
        /// window.
        ResetZoom,
        /// Fit the image to view.
        FitToView,
        /// Zoom to actual size (100%).
        ZoomToActualSize
    ]
);

const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 20.0;
const ZOOM_STEP: f32 = 1.1;
const SCROLL_LINE_MULTIPLIER: f32 = 20.0;
const BASE_SQUARE_SIZE: f32 = 32.0;

/// Zoom/pan state of an image view.
///
/// `fit_to_window` is a persistent *mode*, not a one-shot zoom: while it is set the
/// displayed zoom is re-derived from the current container bounds on every layout, so
/// resizing the pane keeps the image fitted. Any explicit zoom leaves the mode.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ZoomState {
    level: f32,
    pan_offset: Point<Pixels>,
    fit_to_window: bool,
}

impl Default for ZoomState {
    fn default() -> Self {
        Self {
            level: 1.0,
            pan_offset: Point::default(),
            fit_to_window: true,
        }
    }
}

impl ZoomState {
    /// The zoom that fits `image_size` inside `container_bounds`. The same factor is
    /// applied to both axes so the aspect ratio is preserved, and it is capped at 1.0 so
    /// a small image is never blown up.
    fn fit_zoom(container_bounds: Bounds<Pixels>, image_size: (u32, u32)) -> f32 {
        let (image_width, image_height) = image_size;
        if image_width == 0 || image_height == 0 {
            return 1.0;
        }
        let container_width: f32 = container_bounds.size.width.into();
        let container_height: f32 = container_bounds.size.height.into();
        let scale_x = container_width / image_width as f32;
        let scale_y = container_height / image_height as f32;
        scale_x.min(scale_y).min(1.0).clamp(MIN_ZOOM, MAX_ZOOM)
    }

    /// The zoom to render with at `container_bounds`, re-fitting when the mode is active.
    fn level_for_layout(
        &self,
        container_bounds: Bounds<Pixels>,
        image_size: Option<(u32, u32)>,
    ) -> f32 {
        match image_size {
            Some(image_size) if self.fit_to_window => Self::fit_zoom(container_bounds, image_size),
            _ => self.level,
        }
    }

    /// Adopts the zoom that a layout at `container_bounds` actually renders with, so the
    /// toolbar readout and relative zoom steps start from the fitted level instead of a
    /// stale one. Returns whether the level changed, i.e. whether observers need a notify.
    fn adopt_layout_level(
        &mut self,
        container_bounds: Bounds<Pixels>,
        image_size: Option<(u32, u32)>,
    ) -> bool {
        let level = self.level_for_layout(container_bounds, image_size);
        let changed = self.level != level;
        self.level = level;
        changed
    }

    fn set_level(
        &mut self,
        new_level: f32,
        zoom_center: Option<Point<Pixels>>,
        container_bounds: Option<Bounds<Pixels>>,
    ) {
        let old_level = self.level;
        self.level = new_level.clamp(MIN_ZOOM, MAX_ZOOM);
        self.fit_to_window = false;

        if let Some((center, bounds)) = zoom_center.zip(container_bounds) {
            let relative_center = point(
                center.x - bounds.origin.x - bounds.size.width / 2.0,
                center.y - bounds.origin.y - bounds.size.height / 2.0,
            );

            let mouse_offset_from_image = relative_center - self.pan_offset;
            let zoom_ratio = self.level / old_level;

            self.pan_offset += mouse_offset_from_image * (1.0 - zoom_ratio);
        }
    }

    fn set_actual_size(&mut self) {
        self.level = 1.0;
        self.pan_offset = Point::default();
        self.fit_to_window = false;
    }

    fn enable_fit_to_window(
        &mut self,
        container_bounds: Option<Bounds<Pixels>>,
        image_size: Option<(u32, u32)>,
    ) {
        self.fit_to_window = true;
        self.pan_offset = Point::default();
        if let Some((bounds, image_size)) = container_bounds.zip(image_size) {
            self.level = Self::fit_zoom(bounds, image_size);
        }
    }

    /// Whether the image is already shown at its true size. A non-zero pan counts as "not
    /// actual size" because going to actual size also re-centres. Fit mode does not: a
    /// small image fitted at 1.0 *is* displayed at its true size, so the control that
    /// would take it there has nothing left to do.
    fn is_at_actual_size(&self) -> bool {
        self.level == 1.0 && self.pan_offset == Point::default()
    }
}

pub struct ImageView {
    image_item: Entity<ImageItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    zoom: ZoomState,
    last_mouse_position: Option<Point<Pixels>>,
    container_bounds: Option<Bounds<Pixels>>,
    image_size: Option<(u32, u32)>,
}

impl ImageView {
    fn is_dragging(&self) -> bool {
        self.last_mouse_position.is_some()
    }

    pub fn new(
        image_item: Entity<ImageItem>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Start loading the image to render in the background to prevent the view
        // from flickering in most cases.
        let _ = image_item.update(cx, |image, cx| {
            image.image.clone().get_render_image(window, cx)
        });

        cx.subscribe(&image_item, Self::on_image_event).detach();
        cx.on_release_in(window, |this, window, cx| {
            let image_data = this.image_item.read(cx).image.clone();
            if let Some(image) = image_data.clone().get_render_image(window, cx) {
                cx.drop_image(image, None);
            }
            image_data.remove_asset(cx);
        })
        .detach();

        let image_size = image_item
            .read(cx)
            .image_metadata
            .map(|m| (m.width, m.height));

        Self {
            image_item,
            project,
            focus_handle: cx.focus_handle(),
            zoom: ZoomState::default(),
            last_mouse_position: None,
            container_bounds: None,
            image_size,
        }
    }

    fn on_image_event(
        &mut self,
        _: Entity<ImageItem>,
        event: &ImageItemEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ImageItemEvent::MetadataUpdated
            | ImageItemEvent::FileHandleChanged
            | ImageItemEvent::Reloaded => {
                self.image_size = self
                    .image_item
                    .read(cx)
                    .image_metadata
                    .map(|m| (m.width, m.height));
                cx.emit(ImageViewEvent::TitleChanged);
                cx.notify();
            }
            ImageItemEvent::ReloadNeeded => {}
        }
    }

    fn zoom_in(&mut self, _: &ZoomIn, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom.level * ZOOM_STEP, None, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom.level / ZOOM_STEP, None, cx);
    }

    /// `ResetZoom` restores the *default* view, and since fit-to-window became
    /// the default the reset is a re-fit — not 100%, which is what
    /// `ZoomToActualSize` is for.
    fn reset_zoom(&mut self, _: &ResetZoom, window: &mut Window, cx: &mut Context<Self>) {
        self.fit_to_view(&FitToView, window, cx);
    }

    fn fit_to_view(&mut self, _: &FitToView, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom
            .enable_fit_to_window(self.container_bounds, self.image_size);
        cx.notify();
    }

    fn zoom_to_actual_size(
        &mut self,
        _: &ZoomToActualSize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.zoom.set_actual_size();
        cx.notify();
    }

    fn reveal_in_file_manager(
        &mut self,
        _: &RevealInFileManager,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self.image_item.read(cx).abs_path(cx) {
            self.project
                .update(cx, |project, cx| project.reveal_path(&path, cx));
        }
    }

    fn set_zoom(
        &mut self,
        new_zoom: f32,
        zoom_center: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        self.zoom
            .set_level(new_zoom, zoom_center, self.container_bounds);
        cx.notify();
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.modifiers.control || event.modifiers.platform {
            let delta: f32 = match event.delta {
                ScrollDelta::Pixels(pixels) => pixels.y.into(),
                ScrollDelta::Lines(lines) => lines.y * SCROLL_LINE_MULTIPLIER,
            };
            let zoom_factor = if delta > 0.0 {
                1.0 + delta.abs() * 0.01
            } else {
                1.0 / (1.0 + delta.abs() * 0.01)
            };
            self.set_zoom(self.zoom.level * zoom_factor, Some(event.position), cx);
        } else {
            let delta = match event.delta {
                ScrollDelta::Pixels(pixels) => pixels,
                ScrollDelta::Lines(lines) => lines.map(|d| px(d * SCROLL_LINE_MULTIPLIER)),
            };
            self.zoom.pan_offset += delta;
            cx.notify();
        }
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button == MouseButton::Left || event.button == MouseButton::Middle {
            self.last_mouse_position = Some(event.position);
            cx.notify();
        }
    }

    fn handle_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.last_mouse_position = None;
        cx.notify();
    }

    fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_dragging() {
            if let Some(last_pos) = self.last_mouse_position {
                let delta = event.position - last_pos;
                self.zoom.pan_offset += delta;
            }
            self.last_mouse_position = Some(event.position);
            cx.notify();
        }
    }

    fn handle_pinch(&mut self, event: &PinchEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let zoom_factor = 1.0 + event.delta;
        self.set_zoom(self.zoom.level * zoom_factor, Some(event.position), cx);
    }
}

struct ImageContentElement {
    image_view: Entity<ImageView>,
}

impl ImageContentElement {
    fn new(image_view: Entity<ImageView>) -> Self {
        Self { image_view }
    }
}

impl IntoElement for ImageContentElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ImageContentElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<(AnyElement, bool)>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (
            window.request_layout(
                Style {
                    size: size(relative(1.).into(), relative(1.).into()),
                    ..Default::default()
                },
                [],
                cx,
            ),
            (),
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let level_changed = self.image_view.update(cx, |this, _| {
            this.container_bounds = Some(bounds);
            this.zoom.adopt_layout_level(bounds, this.image_size)
        });
        if level_changed {
            // `Window::invalidate_view` drops any invalidation raised while a draw is in
            // flight, so notifying from here would never reach the toolbar and its zoom
            // readout would keep showing the pre-resize level. Deferring lands the notify
            // once the draw is over. This must stay guarded by `level_changed`: an
            // unconditional notify would re-render, re-prepaint and notify again forever.
            let image_view = self.image_view.clone();
            cx.defer(move |cx| image_view.update(cx, |_, cx| cx.notify()));
        }

        let image_view = self.image_view.read(cx);
        let image = image_view.image_item.read(cx).image.clone();

        let zoom_level = image_view.zoom.level;
        let pan_offset = image_view.zoom.pan_offset;
        let border_color = cx.theme().colors().border;

        let is_dragging = image_view.is_dragging();

        let scaled_size = image_view
            .image_size
            .map(|(w, h)| (px(w as f32 * zoom_level), px(h as f32 * zoom_level)));

        let (mut left, mut top) = (px(0.0), px(0.0));
        let mut scaled_width = px(0.0);
        let mut scaled_height = px(0.0);

        if let Some((width, height)) = scaled_size {
            scaled_width = width;
            scaled_height = height;

            let center_x = bounds.size.width / 2.0;
            let center_y = bounds.size.height / 2.0;

            left = center_x - (scaled_width / 2.0) + pan_offset.x;
            top = center_y - (scaled_height / 2.0) + pan_offset.y;
        }

        let mut image_content = div()
            .relative()
            .size_full()
            .child(
                div()
                    .absolute()
                    .left(left)
                    .top(top)
                    .w(scaled_width)
                    .h(scaled_height)
                    .child(
                        div()
                            .size_full()
                            .absolute()
                            .top_0()
                            .left_0()
                            .child(div().size_full().bg(checkerboard(
                                cx.theme().colors().panel_background,
                                BASE_SQUARE_SIZE * zoom_level,
                            )))
                            .border_1()
                            .border_color(border_color),
                    )
                    .child({
                        img(image)
                            .id(("image-viewer-image", self.image_view.entity_id()))
                            .size_full()
                    }),
            )
            .into_any_element();

        image_content.prepaint_as_root(bounds.origin, bounds.size.into(), window, cx);
        Some((image_content, is_dragging))
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some((mut element, is_dragging)) = prepaint.take() else {
            return;
        };

        if is_dragging {
            let image_view = self.image_view.downgrade();
            window.on_mouse_event(move |_event: &MouseUpEvent, phase, _window, cx| {
                if phase == DispatchPhase::Bubble
                    && let Some(entity) = image_view.upgrade()
                {
                    entity.update(cx, |this, cx| {
                        this.last_mouse_position = None;
                        cx.notify();
                    });
                }
            });
        }

        element.paint(window, cx);
    }
}

pub enum ImageViewEvent {
    TitleChanged,
}

impl EventEmitter<ImageViewEvent> for ImageView {}

impl Item for ImageView {
    type Event = ImageViewEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(workspace::item::ItemEvent)) {
        match event {
            ImageViewEvent::TitleChanged => {
                f(workspace::item::ItemEvent::UpdateTab);
                f(workspace::item::ItemEvent::UpdateBreadcrumbs);
            }
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.image_item.entity_id(), self.image_item.read(cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let abs_path = self.image_item.read(cx).abs_path(cx)?;
        let file_path = abs_path.compact().to_string_lossy().into_owned();
        Some(file_path.into())
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let project_path = self.image_item.read(cx).project_path(cx);

        let label_color = if ItemSettings::get_global(cx).git_status {
            let git_status = self
                .project
                .read(cx)
                .project_path_git_status(&project_path, cx)
                .map(|status| status.summary())
                .unwrap_or_default();

            self.project
                .read(cx)
                .entry_for_path(&project_path, cx)
                .map(|entry| {
                    entry_git_aware_label_color(git_status, entry.is_ignored, params.selected)
                })
                .unwrap_or_else(|| params.text_color())
        } else {
            params.text_color()
        };

        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(label_color)
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.image_item
            .read(cx)
            .file
            .file_name(cx)
            .to_string()
            .into()
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        let path = self.image_item.read(cx).abs_path(cx)?;
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| FileIcons::get_icon(&path, cx))
            .flatten()
            .map(Icon::from_path)
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        let show_breadcrumb = EditorSettings::get_global(cx).toolbar.breadcrumbs;
        if show_breadcrumb {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        let text = breadcrumbs_text_for_image(self.project.read(cx), self.image_item.read(cx), cx);
        let font = ThemeSettings::get_global(cx).buffer_font.clone();

        Some((
            vec![HighlightedText {
                text: text.into(),
                highlights: vec![],
            }],
            Some(font),
        ))
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        Task::ready(Some(cx.new(|cx| Self {
            image_item: self.image_item.clone(),
            project: self.project.clone(),
            focus_handle: cx.focus_handle(),
            zoom: self.zoom,
            last_mouse_position: None,
            container_bounds: None,
            image_size: self.image_size,
        })))
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.image_item.read(cx).file.disk_state().is_deleted()
    }
    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

fn breadcrumbs_text_for_image(project: &Project, image: &ImageItem, cx: &App) -> String {
    let mut path = image.file.path().clone();
    if project.visible_worktrees(cx).count() > 1
        && let Some(worktree) = project.worktree_for_id(image.project_path(cx).worktree_id, cx)
    {
        path = worktree.read(cx).root_name().join(&path);
    }

    path.display(project.path_style(cx)).to_string()
}

impl SerializableItem for ImageView {
    fn serialized_item_kind() -> &'static str {
        "ImageView"
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let db = ImageViewerDb::global(cx);
        window.spawn(cx, async move |cx| {
            let image_path = db
                .get_image_path(item_id, workspace_id)?
                .context("No image path found")?;

            let (worktree, relative_path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(image_path.clone(), false, cx)
                })
                .await
                .context("Path not found")?;
            let worktree_id = worktree.update(cx, |worktree, _cx| worktree.id());

            let project_path = ProjectPath {
                worktree_id,
                path: relative_path,
            };

            let image_item = project
                .update(cx, |project, cx| project.open_image(project_path, cx))
                .await?;

            cx.update(
                |window, cx| Ok(cx.new(|cx| ImageView::new(image_item, project, window, cx))),
            )?
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let db = ImageViewerDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "image_viewers", &db, cx)
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let image_path = self.image_item.read(cx).abs_path(cx)?;

        let db = ImageViewerDb::global(cx);
        Some(cx.background_spawn({
            async move {
                log::debug!("Saving image at path {image_path:?}");
                db.save_image_path(item_id, workspace_id, image_path).await
            }
        }))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}

impl EventEmitter<()> for ImageView {}
impl Focusable for ImageView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ImageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle(cx))
            .key_context("ImageViewer")
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
            .on_action(cx.listener(Self::fit_to_view))
            .on_action(cx.listener(Self::zoom_to_actual_size))
            .on_action(cx.listener(Self::reveal_in_file_manager))
            .size_full()
            .relative()
            .bg(cx.theme().colors().editor_background)
            .child({
                let container = div()
                    .id("image-container")
                    .size_full()
                    .overflow_hidden()
                    .cursor(if self.is_dragging() {
                        gpui::CursorStyle::ClosedHand
                    } else {
                        gpui::CursorStyle::OpenHand
                    })
                    .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
                    .on_pinch(cx.listener(Self::handle_pinch))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
                    .on_mouse_down(MouseButton::Middle, cx.listener(Self::handle_mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
                    .on_mouse_up(MouseButton::Middle, cx.listener(Self::handle_mouse_up))
                    .on_mouse_move(cx.listener(Self::handle_mouse_move))
                    .child(ImageContentElement::new(cx.entity()));

                container
            })
    }
}

impl ProjectItem for ImageView {
    type Item = ImageItem;

    fn for_project_item(
        project: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self
    where
        Self: Sized,
    {
        Self::new(item, project, window, cx)
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        e: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView>
    where
        Self: Sized,
    {
        Some(InvalidItemView::new(abs_path, is_local, e, window, cx))
    }
}

pub struct ImageViewToolbarControls {
    image_view: Option<WeakEntity<ImageView>>,
    _subscription: Option<gpui::Subscription>,
}

impl ImageViewToolbarControls {
    pub fn new() -> Self {
        Self {
            image_view: None,
            _subscription: None,
        }
    }
}

impl Render for ImageViewToolbarControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(image_view) = self.image_view.as_ref().and_then(|v| v.upgrade()) else {
            return div().into_any_element();
        };

        let zoom = image_view.read(cx).zoom;
        let zoom_percentage = format!("{}%", (zoom.level * 100.0).round() as i32);

        h_flex()
            .gap_1()
            .child(
                IconButton::new("zoom-out", IconName::Dash)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| Tooltip::for_action("Zoom Out", &ZoomOut, cx))
                    .on_click({
                        let image_view = image_view.downgrade();
                        move |_, window, cx| {
                            if let Some(view) = image_view.upgrade() {
                                view.update(cx, |this, cx| {
                                    this.zoom_out(&ZoomOut, window, cx);
                                });
                            }
                        }
                    }),
            )
            .child(
                Button::new("actual-size", zoom_percentage)
                    .label_size(LabelSize::Small)
                    .disabled(zoom.is_at_actual_size())
                    .tooltip(|_window, cx| {
                        Tooltip::for_action("Actual Size (1:1)", &ZoomToActualSize, cx)
                    })
                    .on_click({
                        let image_view = image_view.downgrade();
                        move |_, window, cx| {
                            if let Some(view) = image_view.upgrade() {
                                view.update(cx, |this, cx| {
                                    this.zoom_to_actual_size(&ZoomToActualSize, window, cx);
                                });
                            }
                        }
                    }),
            )
            .child(
                IconButton::new("zoom-in", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| Tooltip::for_action("Zoom In", &ZoomIn, cx))
                    .on_click({
                        let image_view = image_view.downgrade();
                        move |_, window, cx| {
                            if let Some(view) = image_view.upgrade() {
                                view.update(cx, |this, cx| {
                                    this.zoom_in(&ZoomIn, window, cx);
                                });
                            }
                        }
                    }),
            )
            .child(
                IconButton::new("fit-to-view", IconName::Maximize)
                    .icon_size(IconSize::Small)
                    .toggle_state(zoom.fit_to_window)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action("Fit Zoom to Window", &FitToView, cx)
                    })
                    .on_click({
                        let image_view = image_view.downgrade();
                        move |_, window, cx| {
                            if let Some(view) = image_view.upgrade() {
                                view.update(cx, |this, cx| {
                                    this.fit_to_view(&FitToView, window, cx);
                                });
                            }
                        }
                    }),
            )
            .into_any_element()
    }
}

impl EventEmitter<ToolbarItemEvent> for ImageViewToolbarControls {}

impl ToolbarItemView for ImageViewToolbarControls {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.image_view = None;
        self._subscription = None;

        if let Some(item) = active_pane_item.and_then(|i| i.downcast::<ImageView>()) {
            self._subscription = Some(cx.observe(&item, |_, _, cx| {
                cx.notify();
            }));
            self.image_view = Some(item.downgrade());
            cx.notify();
            return ToolbarItemLocation::PrimaryRight;
        }

        ToolbarItemLocation::Hidden
    }
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<ImageView>(cx);
    workspace::register_serializable_item::<ImageView>(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(width), px(height)),
        }
    }

    #[test]
    fn fit_to_window_is_the_default_mode() {
        let zoom = ZoomState::default();
        assert!(zoom.fit_to_window);
        assert_eq!(
            zoom.level_for_layout(bounds(400., 400.), Some((800, 800))),
            0.5
        );
    }

    #[test]
    fn fit_mode_refits_when_bounds_change() {
        let mut zoom = ZoomState::default();
        let image_size = Some((800, 800));

        let first = zoom.level_for_layout(bounds(400., 400.), image_size);
        assert_eq!(first, 0.5);
        zoom.level = first;

        // A resized pane must produce a new fit without any further user interaction.
        assert_eq!(zoom.level_for_layout(bounds(200., 200.), image_size), 0.25);
        assert_eq!(zoom.level_for_layout(bounds(1600., 1600.), image_size), 1.0);
    }

    #[test]
    fn explicit_zoom_clears_fit_mode() {
        let image_size = Some((800, 800));
        let container = bounds(400., 400.);

        let mut zoom = ZoomState::default();
        zoom.level = zoom.level_for_layout(container, image_size);
        zoom.set_level(zoom.level * ZOOM_STEP, None, Some(container));
        assert!(!zoom.fit_to_window);
        // With fit mode off, a resize no longer changes the zoom.
        assert_eq!(
            zoom.level_for_layout(bounds(100., 100.), image_size),
            zoom.level
        );

        let mut zoom = ZoomState::default();
        zoom.set_actual_size();
        assert!(!zoom.fit_to_window);
        assert_eq!(zoom.level, 1.0);

        zoom.enable_fit_to_window(Some(container), image_size);
        assert!(zoom.fit_to_window);
        assert_eq!(zoom.level, 0.5);
    }

    #[test]
    fn fit_never_upscales_and_never_distorts() {
        let image_size = (400, 200);

        // A container larger than the image on both axes must stay at 1:1.
        assert_eq!(ZoomState::fit_zoom(bounds(4000., 4000.), image_size), 1.0);

        for container in [
            bounds(200., 1000.),
            bounds(1000., 50.),
            bounds(37., 991.),
            bounds(400., 200.),
        ] {
            let zoom = ZoomState::fit_zoom(container, image_size);
            assert!(zoom <= 1.0, "fit upscaled to {zoom} for {container:?}");
            assert!(zoom >= MIN_ZOOM && zoom <= MAX_ZOOM);

            // Same factor on both axes => rendered box keeps the image's aspect ratio.
            let rendered_width = image_size.0 as f32 * zoom;
            let rendered_height = image_size.1 as f32 * zoom;
            let image_ratio = image_size.0 as f32 / image_size.1 as f32;
            assert!(
                (rendered_width / rendered_height - image_ratio).abs() < 1e-5,
                "aspect ratio changed for {container:?}"
            );
            let container_width: f32 = container.size.width.into();
            let container_height: f32 = container.size.height.into();
            assert!(
                rendered_width <= container_width + 1e-3
                    && rendered_height <= container_height + 1e-3
                    || zoom == MIN_ZOOM,
                "fitted image overflows {container:?}"
            );
        }
    }

    #[test]
    fn fit_zoom_is_clamped_and_tolerates_a_degenerate_image() {
        // Would fit at 0.001 without the clamp.
        assert_eq!(ZoomState::fit_zoom(bounds(10., 10.), (10_000, 10_000)), 0.1);
        assert_eq!(ZoomState::fit_zoom(bounds(10., 10.), (0, 0)), 1.0);
    }

    #[test]
    fn readout_level_tracks_the_rendered_level_in_both_directions() {
        let image_size = Some((1600, 400));
        let mut zoom = ZoomState::default();

        // Each call stands in for one layout pass; the bool is what drives the notify that
        // refreshes the toolbar readout.
        assert!(zoom.adopt_layout_level(bounds(1240., 900.), image_size));
        assert_eq!(zoom.level, 0.775);
        // Growing the pane past the image width pins the readout at 100%.
        assert!(zoom.adopt_layout_level(bounds(1600., 900.), image_size));
        assert_eq!(zoom.level, 1.0);
        // And shrinking it again brings the readout back down.
        assert!(zoom.adopt_layout_level(bounds(800., 900.), image_size));
        assert_eq!(zoom.level, 0.5);

        // Re-laying out at unchanged bounds must report "no change", otherwise fit mode
        // would notify on every frame and redraw forever.
        assert!(!zoom.adopt_layout_level(bounds(800., 900.), image_size));

        // Outside fit mode a layout must never move the level the user chose.
        zoom.set_level(2.0, None, None);
        assert!(!zoom.adopt_layout_level(bounds(100., 100.), image_size));
        assert_eq!(zoom.level, 2.0);
    }

    #[test]
    fn actual_size_predicate() {
        let mut zoom = ZoomState::default();
        // A small image fitted at 1.0 is already displayed at its true size.
        zoom.adopt_layout_level(bounds(1000., 1000.), Some((64, 64)));
        assert!(zoom.fit_to_window);
        assert!(zoom.is_at_actual_size());

        // A large image fitted below 1.0 is not.
        zoom.adopt_layout_level(bounds(1000., 1000.), Some((4000, 4000)));
        assert!(!zoom.is_at_actual_size());

        zoom.set_actual_size();
        assert!(zoom.is_at_actual_size());

        zoom.pan_offset += point(px(12.), px(0.));
        assert!(
            !zoom.is_at_actual_size(),
            "a panned image is not at actual size: going to actual size re-centres it"
        );

        zoom.set_actual_size();
        zoom.set_level(2.0, None, None);
        assert!(!zoom.is_at_actual_size());
    }
}

mod persistence {
    use std::path::PathBuf;

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct ImageViewerDb(ThreadSafeConnection);

    impl Domain for ImageViewerDb {
        const NAME: &str = stringify!(ImageViewerDb);

        const MIGRATIONS: &[&str] = &[sql!(
                CREATE TABLE image_viewers (
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,

                    image_path BLOB,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
        )];
    }

    db::static_connection!(ImageViewerDb, [WorkspaceDb]);

    impl ImageViewerDb {
        query! {
            pub async fn save_image_path(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                image_path: PathBuf
            ) -> Result<()> {
                INSERT OR REPLACE INTO image_viewers(item_id, workspace_id, image_path)
                VALUES (?, ?, ?)
            }
        }

        query! {
            pub fn get_image_path(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
                SELECT image_path
                FROM image_viewers
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}
