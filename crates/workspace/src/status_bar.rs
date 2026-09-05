use crate::{
    ItemHandle, MultiWorkspace, Pane, SidebarSide, ToggleWorkspaceSidebar,
    sidebar_side_context_menu,
};
use gpui::{
    Anchor, AnyView, App, Context, Decorations, Entity, IntoElement, ParentElement, Pixels, Render,
    SharedString, Styled, Subscription, WeakEntity, Window,
};
use settings::{SettingsContent, update_settings_file};
use std::{any::TypeId, sync::Arc};
use theme::CLIENT_SIDE_DECORATION_ROUNDING;
use ui::{
    ContextMenu, Divider, IconPosition, Indicator, Tooltip, prelude::*, right_click_menu,
    utils::WithRemSize,
};

/// The status bar's fixed row height. Sawe runs the bar ~10% taller than
/// upstream's 30px (maintainer request, 2026-09-03) — see
/// [`STATUS_BAR_UI_SCALE`] for the other half of that change, which grows the
/// bar's *contents* by the same factor so the row still reads as one system.
///
/// Anything sizing itself against the status bar (e.g.
/// `solution_agent::model::BAND_RESERVED_HEIGHT`) must be re-derived when this
/// changes.
pub const STATUS_BAR_HEIGHT: Pixels = px(33.);

/// Rem multiplier applied to the status bar's whole subtree, so its contents
/// grow with [`STATUS_BAR_HEIGHT`] rather than rattling around in a taller row.
///
/// Every UI metric inside the bar is expressed in rems — label sizes
/// (`TextSize`), icon sizes (`IconSize`), button heights (`ButtonSize`) and
/// padding/gaps (`DynamicSpacing`) all resolve through `rems_from_px` — so
/// overriding the rem size for the subtree scales all of them by exactly one
/// factor. The alternative was hand-bumping a dozen size tokens across the
/// dozen crates that contribute status-bar items, which would drift the first
/// time one of them was edited.
///
/// The multiplier does not leak into the popovers the bar opens, but for two
/// *different* reasons, and only one of them generalises. Tooltips are
/// prepainted at window level (`Window::prepaint_tooltip`), outside any rem
/// override, so any status item's tooltip is unscaled. A right-click menu is
/// unscaled only because `ui::ContextMenu::render` wraps itself in its own
/// `WithRemSize(ui_font_size)` — **deferral does not reset the rem by
/// itself**: `DeferredDraw` captures the ambient rem size and restores it when
/// the deferred subtree is drawn (`gpui::Window`), so a future status-bar item
/// that defers something which is NOT a `ContextMenu` will inherit this 1.1×
/// and must reset it the way `ContextMenu` does.
const STATUS_BAR_UI_SCALE: f32 = 1.1;

/// How much more eagerly the left group yields width than the right one.
///
/// Both groups are shrinkable — the right group has to be, or its outermost
/// item slides off the window instead of its inboard items clipping (see
/// [`StatusBar::render_right_tools`]) — and flexbox distributes a deficit
/// across every shrinkable item in proportion to `shrink × base size`. A
/// weight this much larger makes the left group absorb essentially the whole
/// deficit until it is frozen at `min-width: 0`, at which point the remainder
/// falls to the right group. That is the previous ordering (left clips first),
/// kept without the `flex_shrink_0` that made the right group unable to clip
/// at all.
const LEFT_TOOLS_SHRINK_WEIGHT: f32 = 1000.;

/// Describes how a status-bar item can be hidden by the user.
///
/// Every [`StatusItemView`] must either provide this (so that the user gets a
/// "Hide Button" entry in the right-click menu) or explicitly return `None`
/// to opt out. Returning `None` should be reserved for items that are
/// already conditional on some other setting exposed elsewhere (e.g., the
/// activity indicator, which disappears on its own once there's no work to
/// display).
#[derive(Clone)]
pub struct HideStatusItem {
    hide: Arc<dyn Fn(&mut SettingsContent) + Send + Sync>,
}

impl HideStatusItem {
    pub fn new(hide: impl Fn(&mut SettingsContent) + Send + Sync + 'static) -> Self {
        Self {
            hide: Arc::new(hide),
        }
    }

    /// Persists the hide by updating the user settings file.
    pub fn apply(&self, cx: &App) {
        let hide = self.hide.clone();
        let fs = <dyn fs::Fs>::global(cx);
        update_settings_file(fs, cx, move |settings, _cx| (hide)(settings));
    }
}

pub trait StatusItemView: Render {
    /// Event callback that is triggered when the active pane item changes.
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn crate::ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    );

    /// Returns metadata describing how this item can be hidden from the
    /// status bar by writing to the user settings file.
    ///
    /// Implementors that return `None` must be inherently conditional on
    /// another user-exposed setting; otherwise, they should return `Some` so
    /// that the status bar can show a "Hide Button" entry in its
    /// right-click menu.
    fn hide_setting(&self, cx: &App) -> Option<HideStatusItem>;
}

trait StatusItemViewHandle: Send {
    fn to_any(&self) -> AnyView;
    fn set_active_pane_item(
        &self,
        active_pane_item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut App,
    );
    fn item_type(&self) -> TypeId;
    fn hide_setting(&self, cx: &App) -> Option<HideStatusItem>;
}

#[derive(Default)]
struct SidebarStatus {
    open: bool,
    side: SidebarSide,
    #[allow(dead_code)]
    has_notifications: bool,
    #[allow(dead_code)]
    show_toggle: bool,
}

impl SidebarStatus {
    fn query(multi_workspace: &Option<WeakEntity<MultiWorkspace>>, cx: &App) -> Self {
        multi_workspace
            .as_ref()
            .and_then(|mw| mw.upgrade())
            .map(|mw| {
                let mw = mw.read(cx);
                let enabled = mw.multi_workspace_enabled(cx);
                Self {
                    open: mw.sidebar_open() && enabled,
                    side: mw.sidebar_side(cx),
                    has_notifications: mw.sidebar_has_notifications(cx),
                    show_toggle: enabled,
                }
            })
            .unwrap_or_default()
    }
}

pub struct StatusBar {
    left_items: Vec<Box<dyn StatusItemViewHandle>>,
    right_items: Vec<Box<dyn StatusItemViewHandle>>,
    active_pane: Entity<Pane>,
    multi_workspace: Option<WeakEntity<MultiWorkspace>>,
    _observe_active_pane: Subscription,
}

impl Render for StatusBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = SidebarStatus::query(&self.multi_workspace, cx);

        // Two elements, not one. The outer `div` owns the row's *box* — the
        // fixed height, the background and the window-decoration corners — and
        // the inner `WithRemSize` owns everything laid out inside it at the
        // scaled rem, including its own padding and gap. Splitting them keeps
        // the row height independent of the scale (a taller bar is a decision,
        // not a consequence) and gives the box a `debug_selector`, which
        // `WithRemSize` cannot carry: it implements `Styled`/`ParentElement`
        // but not `InteractiveElement`, so `STATUS_BAR_HEIGHT` would otherwise
        // be unassertable from a paint test.
        div()
            .debug_selector(|| "STATUS-BAR".into())
            .w_full()
            .h(STATUS_BAR_HEIGHT)
            // The status bar is a fixed-height row and must never be the thing
            // that yields when the workspace column overflows — without this it
            // shrinks silently (default `flex-shrink: 1`) and an over-tall
            // Solution band ate it a few pixels at a time.
            .flex_none()
            .bg(cx.theme().colors().status_bar_background)
            .map(|el| match window.window_decorations() {
                Decorations::Server => el,
                Decorations::Client { tiling, .. } => el
                    .when(
                        !(tiling.bottom || tiling.right)
                            && !(sidebar.open && sidebar.side == SidebarSide::Right),
                        |el| el.rounded_br(CLIENT_SIDE_DECORATION_ROUNDING),
                    )
                    .when(
                        !(tiling.bottom || tiling.left)
                            && !(sidebar.open && sidebar.side == SidebarSide::Left),
                        |el| el.rounded_bl(CLIENT_SIDE_DECORATION_ROUNDING),
                    )
                    // This border is to avoid a transparent gap in the rounded corners
                    .mb(px(-1.))
                    .mt({
                        #[cfg(target_os = "linux")]
                        let needs_gap_fix = {
                            // Running on Wayland and using some scaling levels other than 100% causes a
                            // 1px gap above the status bar; adding a margin avoids this.
                            gpui::guess_compositor() == "Wayland" && window.scale_factor() != 1.0
                        };
                        #[cfg(not(target_os = "linux"))]
                        let needs_gap_fix = false;
                        if needs_gap_fix { px(-1.) } else { px(0.) }
                    })
                    .border_b(px(1.0))
                    .border_color(cx.theme().colors().status_bar_background),
            })
            .child(
                WithRemSize::new(theme::theme_settings(cx).ui_font_size(cx) * STATUS_BAR_UI_SCALE)
                    .size_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(DynamicSpacing::Base08.rems(cx))
                    .p(DynamicSpacing::Base04.rems(cx))
                    .child(self.render_left_tools(&sidebar, cx))
                    .child(self.render_right_tools(&sidebar, cx)),
            )
    }
}

impl StatusBar {
    fn render_left_tools(
        &self,
        _sidebar: &SidebarStatus,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // SPK fork: sidebar is disabled (see `zed::zed::initialize_workspace`),
        // so the status-bar toggle that normally re-opens it is hidden.
        h_flex()
            .gap_1()
            .min_w_0()
            .overflow_x_hidden()
            // Weighted against the right group's `1`, so the left group is
            // still what gives up width first and only collapses to nothing
            // before the right group starts clipping anything of its own.
            .flex_shrink(LEFT_TOOLS_SHRINK_WEIGHT)
            .debug_selector(|| "STATUS-BAR-LEFT".into())
            .children(self.left_items.iter().enumerate().map(|(index, item)| {
                render_hideable_item("status-bar-left", index, item.as_ref(), cx)
            }))
    }

    /// The right group paints `right_items` in **reverse** registration order,
    /// so `right_items[0]` is the item flush against the window's right edge.
    /// That outermost item is the one this bar must not clip — the fork mounts
    /// the band's utility buttons there because they are the only mouse path
    /// to the git graph (`zed::zed::initialize_workspace`) — so it is painted
    /// as a `flex_none` sibling of everything inboard of it, which is the part
    /// allowed to shrink and clip.
    ///
    /// Before this split the whole group was `flex_shrink_0`: once the left
    /// group had collapsed to zero the group simply overflowed the window and
    /// the outermost item — the one deliberately made unclippable — was the
    /// first thing off the right edge.
    fn render_right_tools(
        &self,
        _sidebar: &SidebarStatus,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (anchor_item, inboard_items) = match self.right_items.split_first() {
            Some((anchor, rest)) => (Some(anchor), rest),
            None => (None, &[] as &[Box<dyn StatusItemViewHandle>]),
        };

        h_flex()
            .min_w_0()
            .flex_shrink_1()
            .gap_1()
            .child(
                h_flex()
                    .min_w_0()
                    .overflow_x_hidden()
                    .gap_1()
                    .debug_selector(|| "STATUS-BAR-RIGHT-INBOARD".into())
                    .children(inboard_items.iter().enumerate().rev().map(|(index, item)| {
                        // `+ 1`: `index` is into `inboard_items`, but the
                        // per-item menu id must stay keyed on the item's
                        // position in `right_items`.
                        render_hideable_item("status-bar-right", index + 1, item.as_ref(), cx)
                    })),
            )
            .children(anchor_item.map(|item| {
                h_flex()
                    .flex_none()
                    .debug_selector(|| "STATUS-BAR-RIGHT-ANCHOR".into())
                    .child(render_hideable_item(
                        "status-bar-right",
                        0,
                        item.as_ref(),
                        cx,
                    ))
            }))
    }

    #[allow(dead_code)]
    fn render_sidebar_toggle(
        &self,
        sidebar: &SidebarStatus,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let on_right = sidebar.side == SidebarSide::Right;
        let has_notifications = sidebar.has_notifications;
        let indicator_border = cx.theme().colors().status_bar_background;

        let toggle = sidebar_side_context_menu("sidebar-status-toggle-menu", cx)
            .anchor(if on_right {
                Anchor::BottomRight
            } else {
                Anchor::BottomLeft
            })
            .attach(if on_right {
                Anchor::TopRight
            } else {
                Anchor::TopLeft
            })
            .trigger(move |_is_active, _window, _cx| {
                IconButton::new(
                    "toggle-workspace-sidebar",
                    if on_right {
                        IconName::ThreadsSidebarRightClosed
                    } else {
                        IconName::ThreadsSidebarLeftClosed
                    },
                )
                .icon_size(IconSize::Small)
                .when(has_notifications, |this| {
                    this.indicator(Indicator::dot().color(Color::Accent))
                        .indicator_border_color(Some(indicator_border))
                })
                .tooltip(move |_, cx| {
                    Tooltip::for_action("Open Threads Sidebar", &ToggleWorkspaceSidebar, cx)
                })
                .on_click(move |_, window, cx| {
                    if let Some(multi_workspace) = window.root::<MultiWorkspace>().flatten() {
                        multi_workspace.update(cx, |multi_workspace, cx| {
                            multi_workspace.toggle_sidebar(window, cx);
                        });
                    }
                })
            });

        h_flex()
            .gap_0p5()
            .when(on_right, |this| {
                this.child(Divider::vertical().color(ui::DividerColor::Border))
            })
            .child(toggle)
            .when(!on_right, |this| {
                this.child(Divider::vertical().color(ui::DividerColor::Border))
            })
    }
}

fn render_hideable_item(
    side: &'static str,
    index: usize,
    item: &dyn StatusItemViewHandle,
    cx: &App,
) -> impl IntoElement {
    let view = item.to_any();
    let Some(hide) = item.hide_setting(cx) else {
        return view.into_any_element();
    };

    let menu_id: SharedString = format!("{side}-item-menu-{index}").into();
    right_click_menu(menu_id)
        .trigger(move |_is_active, _window, _cx| view)
        .menu(move |window, cx| {
            let hide = hide.clone();
            ContextMenu::build(window, cx, move |menu, _window, _cx| {
                add_hide_button_entry(menu, hide)
            })
        })
        .into_any_element()
}

/// Appends a "Hide Button" entry aligned with surrounding toggleable entries.
pub fn add_hide_button_entry(menu: ContextMenu, hide: HideStatusItem) -> ContextMenu {
    menu.toggleable_entry(
        "Hide Button",
        false,
        IconPosition::Start,
        None,
        move |_window, cx| hide.apply(cx),
    )
}

impl StatusBar {
    pub fn new(
        active_pane: &Entity<Pane>,
        multi_workspace: Option<WeakEntity<MultiWorkspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            left_items: Default::default(),
            right_items: Default::default(),
            active_pane: active_pane.clone(),
            multi_workspace,
            _observe_active_pane: cx.observe_in(active_pane, window, |this, _, window, cx| {
                this.update_active_pane_item(window, cx)
            }),
        };
        this.update_active_pane_item(window, cx);
        this
    }

    pub fn set_multi_workspace(
        &mut self,
        multi_workspace: WeakEntity<MultiWorkspace>,
        cx: &mut Context<Self>,
    ) {
        self.multi_workspace = Some(multi_workspace);
        cx.notify();
    }

    pub fn add_left_item<T>(&mut self, item: Entity<T>, window: &mut Window, cx: &mut Context<Self>)
    where
        T: 'static + StatusItemView,
    {
        let active_pane_item = self.active_pane.read(cx).active_item();
        item.set_active_pane_item(active_pane_item.as_deref(), window, cx);

        self.left_items.push(Box::new(item));
        cx.notify();
    }

    pub fn item_of_type<T: StatusItemView>(&self) -> Option<Entity<T>> {
        self.left_items
            .iter()
            .chain(self.right_items.iter())
            .find_map(|item| item.to_any().downcast().ok())
    }

    /// How many items are in the left group. [`Self::position_of_item`]
    /// flattens both groups into one index space (left items first), so this
    /// is what tells a caller which group a returned position landed in.
    pub fn left_item_count(&self) -> usize {
        self.left_items.len()
    }

    pub fn position_of_item<T>(&self) -> Option<usize>
    where
        T: StatusItemView,
    {
        for (index, item) in self.left_items.iter().enumerate() {
            if item.item_type() == TypeId::of::<T>() {
                return Some(index);
            }
        }
        for (index, item) in self.right_items.iter().enumerate() {
            if item.item_type() == TypeId::of::<T>() {
                return Some(index + self.left_items.len());
            }
        }
        None
    }

    pub fn insert_item_after<T>(
        &mut self,
        position: usize,
        item: Entity<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        T: 'static + StatusItemView,
    {
        let active_pane_item = self.active_pane.read(cx).active_item();
        item.set_active_pane_item(active_pane_item.as_deref(), window, cx);

        if position < self.left_items.len() {
            self.left_items.insert(position + 1, Box::new(item))
        } else {
            self.right_items
                .insert(position + 1 - self.left_items.len(), Box::new(item))
        }
        cx.notify()
    }

    pub fn remove_item_at(&mut self, position: usize, cx: &mut Context<Self>) {
        if position < self.left_items.len() {
            self.left_items.remove(position);
        } else {
            self.right_items.remove(position - self.left_items.len());
        }
        cx.notify();
    }

    pub fn add_right_item<T>(
        &mut self,
        item: Entity<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        T: 'static + StatusItemView,
    {
        let active_pane_item = self.active_pane.read(cx).active_item();
        item.set_active_pane_item(active_pane_item.as_deref(), window, cx);

        self.right_items.push(Box::new(item));
        cx.notify();
    }

    pub fn set_active_pane(
        &mut self,
        active_pane: &Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_pane = active_pane.clone();
        self._observe_active_pane = cx.observe_in(active_pane, window, |this, _, window, cx| {
            this.update_active_pane_item(window, cx)
        });
        self.update_active_pane_item(window, cx);
    }

    fn update_active_pane_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active_pane_item = self.active_pane.read(cx).active_item();
        for item in self.left_items.iter().chain(&self.right_items) {
            item.set_active_pane_item(active_pane_item.as_deref(), window, cx);
        }
    }
}

impl<T: StatusItemView> StatusItemViewHandle for Entity<T> {
    fn to_any(&self) -> AnyView {
        self.clone().into()
    }

    fn set_active_pane_item(
        &self,
        active_pane_item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.update(cx, |this, cx| {
            this.set_active_pane_item(active_pane_item, window, cx)
        });
    }

    fn item_type(&self) -> TypeId {
        TypeId::of::<T>()
    }

    fn hide_setting(&self, cx: &App) -> Option<HideStatusItem> {
        self.read(cx).hide_setting(cx)
    }
}

impl From<&dyn StatusItemViewHandle> for AnyView {
    fn from(val: &dyn StatusItemViewHandle) -> Self {
        val.to_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Workspace;
    use fs::FakeFs;
    use gpui::{TestAppContext, size};
    use project::Project;
    use serde_json::json;
    use util::path;

    /// Smallest possible status-bar item: one default-size `IconButton`, whose
    /// painted height IS `ButtonSize::Default` resolved against whatever rem
    /// size the bar established. `IconButton` registers `ICON-{IconName:?}`
    /// for `debug_bounds` on its own, so nothing test-only is needed.
    struct ScaleProbeItem;

    impl Render for ScaleProbeItem {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            IconButton::new("status-bar-scale-probe", IconName::Check)
        }
    }

    impl StatusItemView for ScaleProbeItem {
        fn set_active_pane_item(
            &mut self,
            _active_pane_item: Option<&dyn ItemHandle>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
        }

        fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
            None
        }
    }

    /// A status item that is simply too wide, so that the right group has to
    /// overflow and something has to give.
    struct WideProbeItem {
        selector: &'static str,
        width: Pixels,
    }

    impl Render for WideProbeItem {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let selector = self.selector;
            div()
                .flex_none()
                .w(self.width)
                .h(px(10.))
                .debug_selector(move || selector.into())
        }
    }

    impl StatusItemView for WideProbeItem {
        fn set_active_pane_item(
            &mut self,
            _active_pane_item: Option<&dyn ItemHandle>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
        }

        fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
            None
        }
    }

    /// `right_items` paints in reverse registration order, so the item
    /// registered first is the one flush against the window's right edge —
    /// which is where this fork mounts the band's utility buttons precisely
    /// because they must never be clipped (they are the only mouse path to the
    /// git graph). The whole group used to be `flex_shrink_0`, so once the left
    /// group had collapsed the group overflowed the window and that outermost
    /// item was the first thing to leave the viewport.
    #[gpui::test]
    async fn the_outermost_right_status_item_survives_a_narrow_window(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ "file.rs": "fn main() {}\n" }))
            .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.status_bar().update(cx, |status_bar, cx| {
                let anchor = cx.new(|_| WideProbeItem {
                    selector: "RIGHT-ANCHOR-PROBE",
                    width: px(300.),
                });
                let inboard = cx.new(|_| WideProbeItem {
                    selector: "RIGHT-INBOARD-PROBE",
                    width: px(900.),
                });
                status_bar.add_right_item(anchor, window, cx);
                status_bar.add_right_item(inboard, window, cx);
            });
        });
        cx.simulate_resize(size(px(500.), px(400.)));
        cx.run_until_parked();

        let bar = cx
            .debug_bounds("STATUS-BAR")
            .expect("the status bar must paint");
        let anchor = cx
            .debug_bounds("RIGHT-ANCHOR-PROBE")
            .expect("the outermost right item must paint");
        let inboard = cx
            .debug_bounds("RIGHT-INBOARD-PROBE")
            .expect("the inboard right items must still paint (clipped, not dropped)");

        assert!(
            anchor.right() <= bar.right(),
            "the item registered first is the one that must not leave the \
             viewport: it ends at {:?} but the bar ends at {:?}",
            anchor.right(),
            bar.right()
        );
        assert_eq!(
            anchor.size.width,
            px(300.),
            "…and it must not be squeezed either — it is `flex_none`"
        );
        assert!(
            inboard.right() > bar.right(),
            "the items inboard of it are the ones that overflow and clip \
             ({:?} vs {:?}) — if nothing overflowed, this test is not \
             exercising a narrow window at all",
            inboard.right(),
            bar.right()
        );
    }

    /// Both halves of the ~10% change, read off the tree that was actually
    /// painted rather than off the constants that were supposed to produce it:
    /// the row is [`STATUS_BAR_HEIGHT`] tall, and an item inside it is
    /// [`STATUS_BAR_UI_SCALE`] times the size it would be anywhere else. The
    /// second half is the one with no other guard — `STATUS_BAR_UI_SCALE` could
    /// be deleted, or the `WithRemSize` wrapper dropped, and every other test in
    /// this crate would stay green.
    #[gpui::test]
    async fn the_status_bar_paints_at_its_height_with_a_scaled_subtree(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ "file.rs": "fn main() {}\n" }))
            .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));

        workspace.update_in(cx, |workspace, window, cx| {
            let probe = cx.new(|_| ScaleProbeItem);
            workspace.status_bar().update(cx, |status_bar, cx| {
                status_bar.add_left_item(probe, window, cx);
            });
        });
        cx.run_until_parked();

        let bar = cx
            .debug_bounds("STATUS-BAR")
            .expect("the status bar must paint");
        assert_eq!(
            bar.size.height, STATUS_BAR_HEIGHT,
            "the row's painted height must be the declared one — \
             `solution_agent::model::BAND_RESERVED_HEIGHT` is derived from it"
        );

        let button = cx
            .debug_bounds("ICON-Check")
            .expect("a status-bar item must paint inside the bar");
        let ui_font_size = cx.update(|_window, cx| theme::theme_settings(cx).ui_font_size(cx));
        let unscaled = ButtonSize::Default.rems().to_pixels(ui_font_size);
        let expected = unscaled * STATUS_BAR_UI_SCALE;
        // Half-pixel tolerance, not exact equality: painted bounds are snapped
        // to the physical-pixel grid, so at the test window's scale factor an
        // expected 21.175px lands as 21.0px. Tight enough to fail on a wrong
        // scale factor (a 1.2 would be ~1.9px away), loose enough not to be a
        // rounding test.
        assert!(
            (f32::from(button.size.height) - f32::from(expected)).abs() <= 0.5,
            "an item in the bar must paint at {expected:?} (= {unscaled:?} × \
             {STATUS_BAR_UI_SCALE}), got {:?}",
            button.size.height
        );
        assert!(
            button.size.height > unscaled,
            "…and strictly larger than the same button outside the bar ({unscaled:?}), \
             or the rem override is not being applied at all"
        );
    }
}
