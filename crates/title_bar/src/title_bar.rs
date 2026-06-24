mod application_menu;
pub mod collab;
mod fork_height;
mod onboarding_banner;
mod plan_chip;
mod project_toolbar;
mod title_bar_settings;
mod update_version;

pub use fork_height::{FORK_TITLE_BAR_CONTENT_HEIGHT_PX, fork_title_bar_content_height};

use crate::application_menu::{ApplicationMenu, show_menus};
use crate::plan_chip::PlanChip;
use agent_settings::{AgentSettings, WindowLayout};
use arrayvec::ArrayVec;
pub use platform_title_bar::{
    self, DraggedWindowTab, MergeAllWindows, MoveTabToNewWindow, PlatformTitleBar,
    ShowNextWindowTab, ShowPreviousWindowTab,
};

#[cfg(not(target_os = "macos"))]
use crate::application_menu::{
    ActivateDirection, ActivateMenuLeft, ActivateMenuRight, OpenApplicationMenu,
};

use auto_update::AutoUpdateStatus;
use call::ActiveCall;
use client::{Client, UserStore, zed_urls};
use command_palette_hooks::CommandPaletteFilter;

use gpui::{
    Action, Anchor, Animation, AnimationExt, AnyElement, App, Context, Element, Entity,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, TaskExt, WeakEntity, Window, actions, div,
    pulsating_between,
};
use onboarding_banner::OnboardingBanner;
use project::{Project, git_store::GitStoreEvent, trusted_worktrees::TrustedWorktrees};
use project_toolbar::ProjectToolbar;
use settings::{Settings as _, SettingsStore};
use solutions_ui::solution_tab_strip::SolutionTabStrip;

use std::any::TypeId;
use std::sync::Arc;
use std::time::Duration;
use title_bar_settings::TitleBarSettings;
use ui::{
    Avatar, ButtonLike, ContextMenu, ContextMenuEntry, Indicator, PopoverMenu, PopoverMenuHandle,
    TintColor, Tooltip, prelude::*,
};
use update_version::UpdateVersion;
use util::ResultExt;
use workspace::{
    MultiWorkspace, ToggleWorktreeSecurity, Workspace,
    notifications::{NotifyResultExt, NotifyTaskExt as _},
};

pub use onboarding_banner::restore_banner;

actions!(
    collab,
    [
        /// Toggles the user menu dropdown.
        ToggleUserMenu,
        /// Toggles the project menu dropdown.
        ToggleProjectMenu,
        /// Switches to a different git branch.
        SwitchBranch,
        /// A debug action to simulate an update being available to test the update banner UI.
        SimulateUpdateAvailable
    ]
);

actions!(
    workspace,
    [
        /// Switches to the classic, editor-focused panel layout.
        UseClassicLayout,
        /// Switches to the agentic panel layout.
        UseAgenticLayout,
    ]
);

pub fn init(cx: &mut App) {
    platform_title_bar::PlatformTitleBar::init(cx);

    update_layout_action_filter(cx);

    cx.observe_global::<SettingsStore>(update_layout_action_filter)
        .detach();

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let multi_workspace = workspace.multi_workspace().cloned();
        let item =
            cx.new(|cx| TitleBar::new("title-bar", workspace, multi_workspace.clone(), window, cx));
        workspace.set_titlebar_item(item.into(), window, cx);

        workspace.register_action(|_workspace, _: &UseClassicLayout, _window, cx| {
            set_window_layout(WindowLayout::Editor(None), cx);
        });

        workspace.register_action(|_workspace, _: &UseAgenticLayout, _window, cx| {
            set_window_layout(WindowLayout::Agent(None), cx);
        });

        // SPK Editor fork: the full-width project toolbar row that sits below
        // the title bar (hosts the project-tab strip + relocated branch widget
        // + run-config strip). Created here alongside the title bar because the
        // `workspace` crate can't depend on `solutions_ui`/`git_ui`/`run_config`.
        let project_toolbar = cx.new(|cx| ProjectToolbar::new(workspace, multi_workspace, cx));
        workspace.set_project_toolbar_item(project_toolbar.into(), window, cx);

        workspace.register_action(|workspace, _: &SimulateUpdateAvailable, _window, cx| {
            if let Some(titlebar) = workspace
                .titlebar_item()
                .and_then(|item| item.downcast::<TitleBar>().ok())
            {
                titlebar.update(cx, |titlebar, cx| {
                    titlebar.toggle_update_simulation(cx);
                });
            }
        });

        #[cfg(not(target_os = "macos"))]
        workspace.register_action(|workspace, action: &OpenApplicationMenu, window, cx| {
            if let Some(titlebar) = workspace
                .titlebar_item()
                .and_then(|item| item.downcast::<TitleBar>().ok())
            {
                titlebar.update(cx, |titlebar, cx| {
                    if let Some(ref menu) = titlebar.application_menu {
                        menu.update(cx, |menu, cx| menu.open_menu(action, window, cx));
                    }
                });
            }
        });

        #[cfg(not(target_os = "macos"))]
        workspace.register_action(|workspace, _: &ActivateMenuRight, window, cx| {
            if let Some(titlebar) = workspace
                .titlebar_item()
                .and_then(|item| item.downcast::<TitleBar>().ok())
            {
                titlebar.update(cx, |titlebar, cx| {
                    if let Some(ref menu) = titlebar.application_menu {
                        menu.update(cx, |menu, cx| {
                            menu.navigate_menus_in_direction(ActivateDirection::Right, window, cx)
                        });
                    }
                });
            }
        });

        #[cfg(not(target_os = "macos"))]
        workspace.register_action(|workspace, _: &ActivateMenuLeft, window, cx| {
            if let Some(titlebar) = workspace
                .titlebar_item()
                .and_then(|item| item.downcast::<TitleBar>().ok())
            {
                titlebar.update(cx, |titlebar, cx| {
                    if let Some(ref menu) = titlebar.application_menu {
                        menu.update(cx, |menu, cx| {
                            menu.navigate_menus_in_direction(ActivateDirection::Left, window, cx)
                        });
                    }
                });
            }
        });

        workspace.register_action(
            |workspace, _: &git_ui::branch_picker::BranchesPopupOpen, window, cx| {
                // SPK Editor fork: the branch widget moved from the title bar to
                // the project toolbar row, so reach it via `project_toolbar_item`.
                if let Some(toolbar) = workspace
                    .project_toolbar_item()
                    .and_then(|item| item.downcast::<ProjectToolbar>().ok())
                {
                    // Defer via `Window::defer` (callback gets `&mut Window,
                    // &mut App` — crucially NOT `&mut Workspace`) so the popover's
                    // `.menu()` closure, which reads the `Workspace` entity at show
                    // time, doesn't double-lease it. `cx.defer_in` would re-lease
                    // `Workspace` in its callback and panic just the same.
                    window.defer(cx, move |window, cx| {
                        toolbar.update(cx, |toolbar, cx| {
                            toolbar.toggle_branch_popover(window, cx);
                        });
                    });
                    return;
                }
                // Fallback: no title bar (e.g. headless) — open a centered modal.
                let repository = workspace.project().read(cx).active_repository(cx);
                let handle = workspace.weak_handle();
                workspace.toggle_modal(window, cx, |window, cx| {
                    git_ui::branch_picker::BranchesPopup::new(handle, repository, window, cx)
                });
            },
        );
    })
    .detach();
}

/// Hides or shows the panel layout actions in the command palette based on
/// whether AI is currently disabled.
fn update_layout_action_filter(cx: &mut App) {
    let disable_ai = project::DisableAiSettings::get_global(cx).disable_ai;
    let layout_actions = [
        TypeId::of::<UseClassicLayout>(),
        TypeId::of::<UseAgenticLayout>(),
    ];
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if disable_ai {
            filter.hide_action_types(&layout_actions);
        } else {
            filter.show_action_types(layout_actions.iter());
        }
    });
}

fn set_window_layout(layout: WindowLayout, cx: &App) {
    let fs = <dyn fs::Fs>::global(cx);
    drop(AgentSettings::set_layout(layout, fs, cx));
}

pub struct TitleBar {
    platform_titlebar: Entity<PlatformTitleBar>,
    project: Entity<Project>,
    user_store: Entity<UserStore>,
    client: Arc<Client>,
    workspace: WeakEntity<Workspace>,
    multi_workspace: Option<WeakEntity<MultiWorkspace>>,
    application_menu: Option<Entity<ApplicationMenu>>,
    // Created lazily once both `workspace` and `multi_workspace` are
    // resolved — `multi_workspace` may be `None` at construction and is
    // populated in `render` (see the top of `Render::render`).
    solution_tab_strip: Option<Entity<SolutionTabStrip>>,
    _subscriptions: Vec<Subscription>,
    banner: Option<Entity<OnboardingBanner>>,
    update_version: Entity<UpdateVersion>,
    screen_share_popover_handle: PopoverMenuHandle<ContextMenu>,
    _diagnostics_subscription: Option<gpui::Subscription>,
}

impl Render for TitleBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.multi_workspace.is_none() {
            if let Some(mw) = self
                .workspace
                .upgrade()
                .and_then(|ws| ws.read(cx).multi_workspace().cloned())
            {
                self.multi_workspace = Some(mw.clone());
                self.platform_titlebar.update(cx, |titlebar, _cx| {
                    titlebar.set_multi_workspace(mw);
                });
            }
        }

        let title_bar_settings = *TitleBarSettings::get_global(cx);
        let button_layout = title_bar_settings.button_layout;

        let show_menus = show_menus(cx);

        let solution_tab_strip = self.ensure_solution_tab_strip(cx);

        let mut children = <ArrayVec<_, 4>>::new();

        children.push(
            h_flex()
                .h_full()
                .gap_0p5()
                .map(|title_bar| {
                    title_bar
                        .when_some(
                            self.application_menu.clone().filter(|_| !show_menus),
                            |title_bar, menu| title_bar.child(menu),
                        )
                        // SPK Editor fork: the "Restricted Mode" badge in
                        // the title bar competes for attention with little
                        // upside — trust is granted at the solution
                        // catalog layer (see `solutions::auto_trust`), so
                        // any project inside a Solution's root is trusted
                        // automatically. The badge stayed surprising for
                        // single-file ad-hoc opens. Render-site disabled,
                        // function intact for upstream-merge friendliness.
                        // .children(self.render_restricted_mode(cx))
                        // SPK Editor fork: the upstream project-info chain
                        // (solution name + project name + worktree/branch)
                        // is replaced by the horizontal solution-tab strip.
                        // The strip's tabs are the per-solution surface
                        // for switching between open solutions in this
                        // window; the active solution + branch surface
                        // moves into the new fork status bar (Phase 2
                        // Task 9). The `show_branch_name` /
                        // `show_project_items` settings no longer have a
                        // surface to gate here.
                        .when_some(solution_tab_strip, |title_bar, strip| {
                            // Nudge the strip right so its first tab lines up
                            // with the project tabs / left-panel edge below it
                            // (the hamburger sits in the gutter to its left).
                            title_bar.child(div().w(px(5.))).child(strip)
                        })
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .into_any_element(),
        );

        children.push(self.render_collaborator_list(window, cx).into_any_element());

        if title_bar_settings.show_onboarding_banner {
            if let Some(banner) = &self.banner {
                children.push(banner.clone().into_any_element())
            }
        }

        let status = self.client.status();
        let status = &*status.borrow();
        let user = self.user_store.read(cx).current_user();

        let signed_in = user.is_some();
        let is_signing_in = user.is_none()
            && matches!(
                status,
                client::Status::Authenticating
                    | client::Status::Authenticated
                    | client::Status::Connecting
            );
        // Sign-in UI is hidden in spk-editor — Zed accounts are not used.
        // let is_signed_out_or_auth_error = user.is_none()
        //     && matches!(
        //         status,
        //         client::Status::SignedOut | client::Status::AuthenticationError
        //     );

        children.push(
            h_flex()
                .map(|this| {
                    if signed_in {
                        this.pr_1p5()
                    } else {
                        this.pr_1()
                    }
                })
                .gap_1()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                // SPK Editor fork: the branch widget and run-config strip moved
                // out of the title bar into the new project toolbar row
                // (`ProjectToolbar`, mounted below the title bar by `Workspace`).
                .child(self.render_call_controls(window, cx))
                .children(self.render_connection_status(status, cx))
                .child(self.update_version.clone())
                // Sign-in UI is hidden in spk-editor — Zed accounts are not used.
                // .when(
                //     user.is_none()
                //         && is_signed_out_or_auth_error
                //         && TitleBarSettings::get_global(cx).show_sign_in,
                //     |this| this.child(self.render_sign_in_button(cx)),
                // )
                .when(is_signing_in, |this| {
                    this.child(
                        Label::new("Signing in…")
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .with_animation(
                                "signing-in",
                                Animation::new(Duration::from_secs(2))
                                    .repeat()
                                    .with_easing(pulsating_between(0.4, 0.8)),
                                |label, delta| label.alpha(delta),
                            ),
                    )
                })
                .when(TitleBarSettings::get_global(cx).show_user_menu, |this| {
                    this.child(self.render_user_menu_button(cx))
                })
                .into_any_element(),
        );

        if show_menus {
            self.platform_titlebar.update(cx, |this, _| {
                this.set_button_layout(button_layout);
                this.set_children(
                    self.application_menu
                        .clone()
                        .map(|menu| menu.into_any_element()),
                );
            });

            // SPK Editor fork: the content row uses the fork-local
            // height so we can resize it for the solution-tab strip
            // without also enlarging the platform window-controls row.
            let height = fork_title_bar_content_height();
            let title_bar_color = self.platform_titlebar.update(cx, |platform_titlebar, cx| {
                platform_titlebar.title_bar_color(window, cx)
            });

            v_flex()
                .w_full()
                .child(self.platform_titlebar.clone().into_any_element())
                .child(
                    h_flex()
                        .bg(title_bar_color)
                        .h(height)
                        .pl_2()
                        .justify_between()
                        .w_full()
                        .children(children),
                )
                .into_any_element()
        } else {
            self.platform_titlebar.update(cx, |this, _| {
                this.set_button_layout(button_layout);
                this.set_children(children);
            });
            self.platform_titlebar.clone().into_any_element()
        }
    }
}

impl TitleBar {
    pub fn new(
        id: impl Into<ElementId>,
        workspace: &Workspace,
        multi_workspace: Option<WeakEntity<MultiWorkspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let user_store = workspace.app_state().user_store.clone();
        let client = workspace.app_state().client.clone();
        let active_call = ActiveCall::global(cx);

        let platform_style = PlatformStyle::platform();
        let application_menu = match platform_style {
            PlatformStyle::Mac => {
                if option_env!("ZED_USE_CROSS_PLATFORM_MENU").is_some() {
                    Some(cx.new(|cx| ApplicationMenu::new(window, cx)))
                } else {
                    None
                }
            }
            PlatformStyle::Linux | PlatformStyle::Windows => {
                Some(cx.new(|cx| ApplicationMenu::new(window, cx)))
            }
        };

        let mut subscriptions = Vec::new();
        subscriptions.push(
            cx.observe(&workspace.weak_handle().upgrade().unwrap(), |_, _, cx| {
                cx.notify()
            }),
        );

        subscriptions.push(cx.observe(&active_call, |this, _, cx| this.active_call_changed(cx)));
        subscriptions.push(cx.observe_window_activation(window, Self::window_activation_changed));
        subscriptions.push(
            cx.subscribe(&git_store, move |_, _, event, cx| match event {
                GitStoreEvent::ActiveRepositoryChanged(_)
                | GitStoreEvent::RepositoryUpdated(_, _, true) => {
                    cx.notify();
                }
                _ => {}
            }),
        );
        subscriptions.push(cx.observe(&user_store, |_a, _, cx| cx.notify()));
        if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
            subscriptions.push(cx.subscribe(
                &workspace_entity,
                |_, _, event: &workspace::Event, cx| {
                    if matches!(event, workspace::Event::WorktreeCreationChanged) {
                        cx.notify();
                    }
                },
            ));
        }
        subscriptions.push(cx.observe_button_layout_changed(window, |_, _, cx| cx.notify()));
        if let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) {
            subscriptions.push(cx.subscribe(&trusted_worktrees, |_, _, _, cx| {
                cx.notify();
            }));
        }

        let update_version = cx.new(|cx| UpdateVersion::new(cx));
        let platform_titlebar = cx.new(|cx| {
            let mut titlebar = PlatformTitleBar::new(id, cx);
            if let Some(mw) = multi_workspace.clone() {
                titlebar = titlebar.with_multi_workspace(mw);
            }
            titlebar
        });

        let banner = None;

        let mut this = Self {
            platform_titlebar,
            application_menu,
            workspace: workspace.weak_handle(),
            multi_workspace,
            solution_tab_strip: None,
            project,
            user_store,
            client,
            _subscriptions: subscriptions,
            banner,
            update_version,
            screen_share_popover_handle: PopoverMenuHandle::default(),
            _diagnostics_subscription: None,
        };

        this.observe_diagnostics(cx);

        this
    }

    fn toggle_update_simulation(&mut self, cx: &mut Context<Self>) {
        self.update_version
            .update(cx, |banner, cx| banner.update_simulation(cx));
        cx.notify();
    }

    /// Build (or return the cached) `SolutionTabStrip` entity. Called from
    /// `render`. The strip is created lazily because `multi_workspace` may
    /// arrive after `TitleBar::new` (see the late-set fallback at the top
    /// of `Render::render`); once both `workspace` and `multi_workspace`
    /// are resolved the entity is cached for the lifetime of the title bar.
    fn ensure_solution_tab_strip(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<Entity<SolutionTabStrip>> {
        if self.solution_tab_strip.is_none() {
            let workspace = self.workspace.clone();
            let multi_workspace = self.multi_workspace.clone()?;
            let strip = cx.new(|cx| SolutionTabStrip::new(workspace, multi_workspace, cx));
            self.solution_tab_strip = Some(strip);
        }
        self.solution_tab_strip.clone()
    }

    // SPK Editor fork: render site is disabled (see comment near the
    // title-bar layout above). Function is kept for upstream-merge
    // friendliness; allow-dead-code so the unused-warn doesn't fire.
    #[allow(dead_code)]
    pub fn render_restricted_mode(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let has_restricted_worktrees =
            TrustedWorktrees::has_restricted_worktrees(&self.project.read(cx).worktree_store(), cx);
        if !has_restricted_worktrees {
            return None;
        }

        let button = Button::new("restricted_mode_trigger", "Restricted Mode")
            .style(ButtonStyle::Tinted(TintColor::Warning))
            .label_size(LabelSize::Small)
            .color(Color::Warning)
            .start_icon(
                Icon::new(IconName::Warning)
                    .size(IconSize::Small)
                    .color(Color::Warning),
            )
            .tooltip(|_, cx| {
                Tooltip::with_meta(
                    "You're in Restricted Mode",
                    Some(&ToggleWorktreeSecurity),
                    "Mark this project as trusted and unlock all features",
                    cx,
                )
            })
            .on_click({
                cx.listener(move |this, _, window, cx| {
                    this.workspace
                        .update(cx, |workspace, cx| {
                            workspace.show_worktree_trust_security_modal(true, window, cx)
                        })
                        .log_err();
                })
            });

        if ui::utils::MACOS_SDK_26_OR_LATER {
            // Make up for Tahoe's traffic light buttons having less spacing around them
            Some(div().child(button).ml_0p5().into_any_element())
        } else {
            Some(button.into_any_element())
        }
    }

    fn window_activation_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.is_window_active() {
            ActiveCall::global(cx)
                .update(cx, |call, cx| call.set_location(Some(&self.project), cx))
                .detach_and_log_err(cx);
        } else if cx.active_window().is_none() {
            ActiveCall::global(cx)
                .update(cx, |call, cx| call.set_location(None, cx))
                .detach_and_log_err(cx);
        }
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.update_active_view_for_followers(window, cx);
            })
            .ok();
    }

    fn active_call_changed(&mut self, cx: &mut Context<Self>) {
        self.observe_diagnostics(cx);
        cx.notify();
    }

    fn observe_diagnostics(&mut self, cx: &mut Context<Self>) {
        let diagnostics = ActiveCall::global(cx)
            .read(cx)
            .room()
            .and_then(|room| room.read(cx).diagnostics().cloned());

        if let Some(diagnostics) = diagnostics {
            self._diagnostics_subscription = Some(cx.observe(&diagnostics, |_, _, cx| cx.notify()));
        } else {
            self._diagnostics_subscription = None;
        }
    }

    fn share_project(&mut self, cx: &mut Context<Self>) {
        let active_call = ActiveCall::global(cx);
        let project = self.project.clone();
        active_call
            .update(cx, |call, cx| call.share_project(project, cx))
            .detach_and_log_err(cx);
    }

    fn unshare_project(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let active_call = ActiveCall::global(cx);
        let project = self.project.clone();
        active_call
            .update(cx, |call, cx| call.unshare_project(project, cx))
            .log_err();
    }

    fn render_connection_status(
        &self,
        status: &client::Status,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        match status {
            client::Status::ConnectionError
            | client::Status::ConnectionLost
            | client::Status::Reauthenticating
            | client::Status::Reconnecting
            | client::Status::ReconnectionError { .. } => Some(
                div()
                    .id("disconnected")
                    .child(Icon::new(IconName::Disconnected).size(IconSize::Small))
                    .tooltip(Tooltip::text("Disconnected"))
                    .into_any_element(),
            ),
            client::Status::UpgradeRequired => {
                let auto_updater = auto_update::AutoUpdater::get(cx);
                let label = match auto_updater.map(|auto_update| auto_update.read(cx).status()) {
                    Some(AutoUpdateStatus::Updated { .. }) => "Please restart Zed to Collaborate",
                    Some(AutoUpdateStatus::Installing { .. })
                    | Some(AutoUpdateStatus::Downloading { .. })
                    | Some(AutoUpdateStatus::Checking) => "Updating...",
                    Some(AutoUpdateStatus::Idle)
                    | Some(AutoUpdateStatus::Errored { .. })
                    | None => "Please update Zed to Collaborate",
                };

                Some(
                    Button::new("connection-status", label)
                        .label_size(LabelSize::Small)
                        .on_click(|_, window, cx| {
                            if let Some(auto_updater) = auto_update::AutoUpdater::get(cx)
                                && auto_updater.read(cx).status().is_updated()
                            {
                                workspace::reload(cx);
                                return;
                            }
                            auto_update::check(&Default::default(), window, cx);
                        })
                        .into_any_element(),
                )
            }
            _ => None,
        }
    }

    pub fn render_sign_in_button(&mut self, _: &mut Context<Self>) -> Button {
        let client = self.client.clone();
        let workspace = self.workspace.clone();
        Button::new("sign_in", "Sign In")
            .label_size(LabelSize::Small)
            .on_click(move |_, window, cx| {
                let client = client.clone();
                let workspace = workspace.clone();
                window
                    .spawn(cx, async move |mut cx| {
                        client
                            .sign_in_with_optional_connect(true, cx)
                            .await
                            .notify_workspace_async_err(workspace, &mut cx);
                    })
                    .detach();
            })
    }

    pub fn render_user_menu_button(&mut self, cx: &mut Context<Self>) -> impl Element {
        let show_update_button = self.update_version.read(cx).show_update_in_menu_bar();

        let user_store = self.user_store.clone();
        let workspace = self.workspace.clone();
        let user = user_store.read(cx).current_user();

        let user_avatar = user.as_ref().map(|u| u.avatar_uri.clone());
        let user_login = user.as_ref().map(|u| u.github_login.clone());

        let is_signed_in = user.is_some();

        let current_organization = user_store.read(cx).current_organization();
        let business_organization = current_organization
            .as_ref()
            .filter(|organization| !organization.is_personal);
        let organizations: Vec<_> = user_store
            .read(cx)
            .organizations()
            .iter()
            .map(|organization| {
                let plan = user_store.read(cx).plan_for_organization(&organization.id);
                (organization.clone(), plan)
            })
            .collect();

        let show_user_picture = TitleBarSettings::get_global(cx).show_user_picture;

        let trigger = if is_signed_in && show_user_picture {
            let avatar = user_avatar.map(|avatar| Avatar::new(avatar)).map(|avatar| {
                if show_update_button {
                    avatar.indicator(
                        div()
                            .absolute()
                            .bottom_0()
                            .right_0()
                            .child(Indicator::dot().color(Color::Accent)),
                    )
                } else {
                    avatar
                }
            });

            ButtonLike::new("user-menu").child(
                h_flex()
                    .when_some(business_organization, |this, organization| {
                        this.gap_2()
                            .child(Label::new(&organization.name).size(LabelSize::Small))
                    })
                    .children(avatar),
            )
        } else {
            ButtonLike::new("user-menu")
                .child(Icon::new(IconName::ChevronDown).size(IconSize::Small))
        };

        PopoverMenu::new("user-menu")
            .trigger(trigger)
            .menu(move |window, cx| {
                let user_login = user_login.clone();
                let current_organization = current_organization.clone();
                let organizations = organizations.clone();
                let user_store = user_store.clone();
                let workspace = workspace.clone();

                let ai_enabled = !project::DisableAiSettings::get_global(cx).disable_ai;
                let current_layout = AgentSettings::get_layout(cx);
                let is_editor = matches!(current_layout, WindowLayout::Editor(_));
                let is_agent = matches!(current_layout, WindowLayout::Agent(_));
                let is_custom = matches!(current_layout, WindowLayout::Custom(_));

                ContextMenu::build(window, cx, |menu, _, _cx| {
                    menu.when(is_signed_in, |this| {
                        let user_login = user_login.clone();
                        this.custom_entry(
                            move |_window, _cx| {
                                let user_login = user_login.clone().unwrap_or_default();

                                h_flex()
                                    .w_full()
                                    .justify_between()
                                    .child(Label::new(user_login))
                                    .into_any_element()
                            },
                            move |_, cx| {
                                cx.open_url(&zed_urls::account_url(cx));
                            },
                        )
                        .separator()
                    })
                    .when(show_update_button, |this| {
                        this.custom_entry(
                            move |_window, _cx| {
                                h_flex()
                                    .w_full()
                                    .gap_1()
                                    .justify_between()
                                    .child(Label::new("Restart to update Zed").color(Color::Accent))
                                    .child(
                                        Icon::new(IconName::Download)
                                            .size(IconSize::Small)
                                            .color(Color::Accent),
                                    )
                                    .into_any_element()
                            },
                            move |_, cx| {
                                workspace::reload(cx);
                            },
                        )
                        .separator()
                    })
                    .map(|this| {
                        let mut this = this.header("Organization");

                        for (organization, plan) in &organizations {
                            let organization = organization.clone();
                            let plan = *plan;

                            let is_current =
                                current_organization
                                    .as_ref()
                                    .is_some_and(|current_organization| {
                                        current_organization.id == organization.id
                                    });

                            this = this.custom_entry(
                                {
                                    let organization = organization.clone();
                                    move |_window, _cx| {
                                        h_flex()
                                            .w_full()
                                            .gap_4()
                                            .justify_between()
                                            .child(
                                                h_flex()
                                                    .gap_1()
                                                    .child(Label::new(&organization.name))
                                                    .when(is_current, |this| {
                                                        this.child(
                                                            Icon::new(IconName::Check)
                                                                .color(Color::Accent),
                                                        )
                                                    }),
                                            )
                                            .children(plan.map(|plan| PlanChip::new(plan)))
                                            .into_any_element()
                                    }
                                },
                                {
                                    let user_store = user_store.clone();
                                    let organization = organization.clone();
                                    let workspace = workspace.clone();
                                    move |window, cx| {
                                        let task = user_store.update(cx, |user_store, cx| {
                                            user_store
                                                .set_current_organization(organization.clone(), cx)
                                        });
                                        task.detach_and_notify_err(workspace.clone(), window, cx);
                                    }
                                },
                            );
                        }

                        this.separator()
                    })
                    .action("Settings", zed_actions::OpenSettings.boxed_clone())
                    .action("Keymap", Box::new(zed_actions::OpenKeymap))
                    .action(
                        "Themes…",
                        zed_actions::theme_selector::Toggle::default().boxed_clone(),
                    )
                    .action(
                        "Icon Themes…",
                        zed_actions::icon_theme_selector::Toggle::default().boxed_clone(),
                    )
                    .action(
                        "Extensions",
                        zed_actions::Extensions::default().boxed_clone(),
                    )
                    .when(ai_enabled, |menu| {
                        menu.separator()
                            .submenu("Panel Layout", move |menu, _window, _cx| {
                                menu.toggleable_entry(
                                    "Classic",
                                    is_editor,
                                    IconPosition::Start,
                                    Some(UseClassicLayout.boxed_clone()),
                                    move |window, cx| {
                                        window.dispatch_action(UseClassicLayout.boxed_clone(), cx);
                                    },
                                )
                                .toggleable_entry(
                                    "Agentic",
                                    is_agent,
                                    IconPosition::Start,
                                    Some(UseAgenticLayout.boxed_clone()),
                                    move |window, cx| {
                                        window.dispatch_action(UseAgenticLayout.boxed_clone(), cx);
                                    },
                                )
                                .when(is_custom, |menu| {
                                    menu.item(
                                        ContextMenuEntry::new("Custom")
                                            .toggleable(IconPosition::Start, true)
                                            .disabled(true),
                                    )
                                })
                            })
                    })
                    .when(is_signed_in, |this| {
                        this.separator()
                            .action("Sign Out", client::SignOut.boxed_clone())
                    })
                })
                .into()
            })
            .anchor(Anchor::TopRight)
    }
}
