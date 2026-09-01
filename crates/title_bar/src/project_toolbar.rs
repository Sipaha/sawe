use gpui::{
    App, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, WeakEntity,
    Window, div, px,
};
use project::Project;
use solutions::mcp::{StructureSlot, VisualNode, register_structure_provider};
use solutions_ui::project_tab_strip::ProjectTabStrip;
use ui::{
    ContextMenu, Divider, DividerColor, IconPosition, PopoverMenu, PopoverMenuHandle, Tooltip,
    prelude::*,
};
use workspace::{MultiWorkspace, Workspace, dock::PanelButtons};

/// Sawe fork: a full-width toolbar row mounted by `Workspace` directly
/// below the title bar (see `Workspace::project_toolbar_item`). It hosts the
/// per-solution `ProjectTabStrip` on the left and the relocated git-branch
/// widget + run-config strip on the right.
///
/// Lives in the `title_bar` crate because `workspace` cannot depend on
/// `solutions_ui`/`git_ui`/`run_config_ui` (they depend on `workspace` — a
/// cycle), while `title_bar` already depends on all of them.
pub struct ProjectToolbar {
    workspace: WeakEntity<Workspace>,
    multi_workspace: Option<WeakEntity<MultiWorkspace>>,
    project: Entity<Project>,
    // Created lazily once both `workspace` and `multi_workspace` are
    // resolved (mirrors `TitleBar::ensure_solution_tab_strip`).
    project_tab_strip: Option<Entity<ProjectTabStrip>>,
    /// Toggles for the project-zone docks (ProjectPanel / OutlinePanel /
    /// GitPanel), one `PanelButtons` per dock so a panel moved between docks
    /// keeps exactly one button. They live here, in the project toolbar,
    /// instead of in the vertical edge strips this fork used to flank the
    /// workspace with.
    ///
    /// Each group is painted on the side its dock opens on: left and bottom at
    /// the row's leading edge, right at its trailing edge. Because a group is
    /// bound to a `Dock` *entity* and not to a list of panels, dragging a panel
    /// between docks moves its toggle across the row with no code here — see
    /// `leading_dock_buttons` / `trailing_dock_buttons`.
    dock_buttons: [Entity<PanelButtons>; 3],
    branch_popover_handle: PopoverMenuHandle<git_ui::branch_picker::BranchesPopup>,
    repository_popover_handle: PopoverMenuHandle<ContextMenu>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectToolbar {
    pub fn new(
        workspace: &Workspace,
        multi_workspace: Option<WeakEntity<MultiWorkspace>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();

        let mut subscriptions = Vec::new();
        // Re-render when the active repository or its branch changes so the
        // relocated branch widget stays current.
        subscriptions.push(
            cx.subscribe(&git_store, move |_, _, event, cx| match event {
                project::git_store::GitStoreEvent::ActiveRepositoryChanged(_)
                | project::git_store::GitStoreEvent::RepositoryUpdated(_, _, true)
                // The repository selector only renders when the active member
                // owns more than one repo, so it has to re-render as the
                // worktree scanner discovers (or drops) nested repositories.
                | project::git_store::GitStoreEvent::RepositoryAdded
                | project::git_store::GitStoreEvent::RepositoryRemoved(_) => {
                    cx.notify();
                }
                _ => {}
            }),
        );
        if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
            subscriptions.push(cx.observe(&workspace_entity, |_, _, cx| cx.notify()));
        }
        // Re-render the branch widget when the solution-wide active project
        // changes so it follows the active member's repository.
        if let Some(store) = solutions::SolutionStore::try_global(cx) {
            subscriptions.push(cx.subscribe(&store, |_, _, event, cx| {
                if let solutions::SolutionStoreEvent::ActiveMemberChanged { .. } = event {
                    cx.notify();
                }
            }));
        }

        // Built from the `&Workspace` parameter rather than by upgrading the
        // weak handle: `ProjectToolbar::new` runs under a live `&mut Workspace`
        // borrow, so reading the Workspace entity here would double-lease.
        // The three dock entities live as long as the workspace does, so the
        // buttons can hold them directly.
        let dock_buttons = [
            cx.new(|cx| PanelButtons::new(workspace.left_dock().clone(), cx)),
            cx.new(|cx| PanelButtons::new(workspace.bottom_dock().clone(), cx)),
            cx.new(|cx| PanelButtons::new(workspace.right_dock().clone(), cx)),
        ];

        // The divider after the group is gated on the group having any button
        // (`has_visible_buttons`), so this view must re-render on every event
        // that can change that answer: a panel loading into a dock, and a
        // `"button": false` settings edit. Observing the `PanelButtons` rather
        // than the docks and the `SettingsStore` separately means the two
        // cannot drift — those are exactly the two sources `PanelButtons`
        // itself subscribes to.
        for buttons in &dock_buttons {
            subscriptions.push(cx.observe(buttons, |_, _, cx| cx.notify()));
        }

        Self {
            workspace: workspace.weak_handle(),
            multi_workspace,
            project,
            project_tab_strip: None,
            dock_buttons,
            branch_popover_handle: PopoverMenuHandle::default(),
            repository_popover_handle: PopoverMenuHandle::default(),
            _subscriptions: subscriptions,
        }
    }

    pub fn toggle_branch_popover(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.branch_popover_handle.toggle(window, cx);
    }

    /// Build (or return the cached) `ProjectTabStrip` entity. Mirrors
    /// `TitleBar::ensure_solution_tab_strip`: the strip is created lazily
    /// because `multi_workspace` may arrive after construction.
    fn ensure_project_tab_strip(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ProjectTabStrip>> {
        if self.project_tab_strip.is_none() {
            let workspace = self.workspace.clone();
            let multi_workspace = self.multi_workspace.clone()?;
            let strip = cx.new(|cx| ProjectTabStrip::new(workspace, multi_workspace, cx));
            self.project_tab_strip = Some(strip);
        }
        self.project_tab_strip.clone()
    }

    /// The dock-toggle groups painted at the row's leading edge, each paired
    /// with the name `structure_node` reports it under, in painted order.
    ///
    /// Destructured rather than indexed so the fixed left/bottom/right order
    /// `new` builds is checked by the compiler: adding a fourth dock stops
    /// compiling here instead of silently relabelling a group in the dump.
    fn leading_dock_buttons(&self) -> [(&'static str, &Entity<PanelButtons>); 2] {
        let [left, bottom, _right] = &self.dock_buttons;
        [("left", left), ("bottom", bottom)]
    }

    /// The dock-toggle group painted at the row's trailing edge, on the side
    /// its panels actually open on.
    fn trailing_dock_buttons(&self) -> (&'static str, &Entity<PanelButtons>) {
        let [_left, _bottom, right] = &self.dock_buttons;
        ("right", right)
    }

    /// Whether the leading dock-toggle cluster paints anything, which is what
    /// the divider between that cluster and the project tabs is gated on. The
    /// trailing group deliberately does not count: it is painted at the far
    /// end of the row, so counting it would draw the divider with nothing to
    /// its left.
    fn has_leading_dock_buttons(&self, window: &Window, cx: &App) -> bool {
        self.leading_dock_buttons()
            .into_iter()
            .any(|(_, buttons)| buttons.read(cx).has_visible_buttons(window, cx))
    }

    /// Resolve the repository the branch widget should display: the shared
    /// member-scoped resolution (the user's explicit per-member pick, else the
    /// member's *outermost* repository — see
    /// `solutions::member_repository`), falling back to
    /// `project.active_repository(cx)` when there is no active solution or no
    /// active member, so a plain non-solution project still shows its branch.
    /// Every surface of this toolbar uses this single resolution.
    fn resolve_repository(
        project: &Entity<Project>,
        cx: &App,
    ) -> Option<Entity<project::git_store::Repository>> {
        solutions::active_member_repository(project, cx)
            .or_else(|| project.read(cx).active_repository(cx))
    }

    /// Repository picker, shown to the LEFT of the branch widget and ONLY when
    /// the active member owns more than one repository (a member worktree that
    /// vendors its own git repo — e.g. a plugin with its own `.git`). With a
    /// single repo there is nothing to pick, so nothing renders at all.
    /// The repository the selector would show, plus how many the active
    /// member owns. `None` is exactly the case where the selector renders
    /// nothing. Shared with `structure_node` so the dump cannot claim a
    /// control the toolbar is not painting.
    fn repository_selector_state(
        &self,
        cx: &App,
    ) -> Option<(Entity<project::git_store::Repository>, usize)> {
        let repositories = solutions::active_member_repositories(&self.project, cx);
        if repositories.len() < 2 {
            return None;
        }
        Some((
            Self::resolve_repository(&self.project, cx)?,
            repositories.len(),
        ))
    }

    /// The branch widget's label and its `behind` count, or `None` when the
    /// widget renders nothing. Shared with `structure_node` — see
    /// `repository_selector_state`.
    fn branch_summary(&self, cx: &App) -> Option<(SharedString, u32)> {
        let repository = Self::resolve_repository(&self.project, cx)?;
        let snapshot = repository.read(cx);
        // Only the `behind` count is shown on the branch widget now; the
        // `ahead` (unpushed) count moved to the dedicated Push button.
        match &snapshot.branch {
            Some(branch) => {
                let behind = branch.tracking_status().map(|s| s.behind).unwrap_or(0);
                Some((SharedString::from(branch.name().to_string()), behind))
            }
            // Detached HEAD: show short commit SHA, no upstream tracking indicators.
            None => Some((snapshot.head_commit.as_ref().map(|c| c.short_sha())?, 0)),
        }
    }

    /// The unpushed-commit count, or `None` when the Push button renders
    /// nothing. Shared with `structure_node` — see `repository_selector_state`.
    fn unpushed_commits(&self, cx: &App) -> Option<u32> {
        let repository = Self::resolve_repository(&self.project, cx)?;
        let ahead = repository
            .read(cx)
            .branch
            .as_ref()
            .and_then(|branch| branch.tracking_status())
            .map(|status| status.ahead)
            .unwrap_or(0);
        (ahead > 0).then_some(ahead)
    }

    fn render_repository_selector(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (current, _) = self.repository_selector_state(cx)?;
        let current_path = current.read(cx).work_directory_abs_path.clone();
        let name = SharedString::from(
            current
                .read(cx)
                .display_name()
                .trim_end_matches('/')
                .to_string(),
        );
        let tooltip_path = SharedString::from(current_path.to_string_lossy().to_string());
        let project = self.project.clone();
        Some(
            PopoverMenu::new("repository-selector")
                .with_handle(self.repository_popover_handle.clone())
                .trigger(
                    ui::ButtonLike::new("repository-selector-trigger")
                        .child(
                            h_flex()
                                .gap_1()
                                // Bounded so a long repository name truncates
                                // instead of pushing the branch button out of
                                // the toolbar.
                                .max_w(px(140.))
                                .overflow_hidden()
                                // A folder, not `GitBranch`: this button sits
                                // directly beside the branch widget, and two
                                // adjacent buttons wearing the same glyph read
                                // as one control. A repository is a VCS root,
                                // which is what a folder says here.
                                .child(
                                    Icon::new(IconName::Folder)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(Label::new(name).size(LabelSize::Small).truncate())
                                .child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
                        )
                        .tooltip(Tooltip::text(tooltip_path))
                        .toggle_state(self.repository_popover_handle.is_deployed()),
                )
                .menu(move |window, cx| {
                    let repositories = solutions::active_member_repositories(&project, cx);
                    if repositories.len() < 2 {
                        return None;
                    }
                    let selected = solutions::active_member_repository(&project, cx)
                        .map(|repo| repo.read(cx).work_directory_abs_path.clone());
                    Some(ContextMenu::build(window, cx, |mut menu, _window, cx| {
                        for repository in repositories {
                            let work_directory =
                                repository.read(cx).work_directory_abs_path.clone();
                            let label = SharedString::from(
                                repository
                                    .read(cx)
                                    .display_name()
                                    .trim_end_matches('/')
                                    .to_string(),
                            );
                            let is_selected = selected
                                .as_ref()
                                .is_some_and(|path| *path == work_directory);
                            menu = menu.toggleable_entry(
                                label,
                                is_selected,
                                IconPosition::End,
                                None,
                                {
                                    let project = project.clone();
                                    move |_window, cx| {
                                        solutions::set_active_member_repository(
                                            &project,
                                            &repository,
                                            cx,
                                        );
                                    }
                                },
                            );
                        }
                        menu
                    }))
                }),
        )
    }

    fn render_branch_widget(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (name, behind) = self.branch_summary(cx)?;
        let workspace_weak = self.workspace.clone();
        let project = self.project.clone();
        Some(
            PopoverMenu::new("branch-widget")
                .with_handle(self.branch_popover_handle.clone())
                .trigger(
                    ui::ButtonLike::new("branch-widget-trigger")
                        .child(
                            h_flex()
                                .gap_1()
                                .child(Icon::new(IconName::GitBranch).size(IconSize::Small))
                                .child(Label::new(name).size(LabelSize::Small))
                                // The unpushed-commit count (`↑ahead`) now lives
                                // on the dedicated Push button (`render_push_button`),
                                // so it's intentionally not shown here anymore.
                                .when(behind > 0, |this| {
                                    this.child(
                                        Label::new(format!("↓{behind}"))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                })
                                .child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
                        )
                        .toggle_state(self.branch_popover_handle.is_deployed()),
                )
                .menu(move |window, cx| {
                    let workspace = workspace_weak.upgrade()?;
                    let repository = Self::resolve_repository(&project, cx);
                    let weak = workspace.downgrade();
                    Some(cx.new(|cx| {
                        git_ui::branch_picker::BranchesPopup::new(weak, repository, window, cx)
                    }))
                }),
        )
    }

    /// "Update Project" button — sits to the LEFT of the branch-widget
    /// dropdown. Fetches + pulls ONLY the active project's repo (dispatches
    /// `git::Fetch` then `git::Pull` — they route through the git panel's
    /// `active_repository`, which is scoped to the active member). Solution-
    /// wide "Update All Projects" was deliberately dropped: a fetch+pull that
    /// spans every member can leave half the repos in a conflicted state with
    /// no good way to resolve it from this surface. Only shown when the active
    /// project has a git repository (mirrors the branch widget's gating).
    fn render_update_button(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        Self::resolve_repository(&self.project, cx)?;
        Some(
            // Same down-left arrow the git panel's Fetch button uses
            // (`git_ui::render_fetch_button`) — this button really does fetch,
            // so the two surfaces should read alike.
            IconButton::new("update-project-trigger", IconName::ArrowDownLeft)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text(
                    "Update Project — fetch updates from remote, then pull",
                ))
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(git::Fetch), cx);
                    window.dispatch_action(Box::new(git::Pull), cx);
                }),
        )
    }

    /// "Push" button — sits beside the Update button. Shown ONLY when the
    /// active project's branch has unpushed commits (`ahead > 0`); the count
    /// renders next to the arrow icon (this is the indicator that used to sit
    /// on the branch-widget dropdown). Click dispatches `git::Push`, scoped to
    /// the git panel's active repository.
    fn render_push_button(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let ahead = self.unpushed_commits(cx)?;
        Some(
            ui::ButtonLike::new("push-trigger")
                .child(
                    h_flex()
                        .gap_0p5()
                        .child(Icon::new(IconName::ArrowUp).size(IconSize::Small))
                        .child(Label::new(format!("{ahead}")).size(LabelSize::Small)),
                )
                .tooltip(Tooltip::text("Push unpushed commits"))
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(git::Push), cx);
                }),
        )
    }
}

/// Teach `workspace.dump_visual_structure` about the project-toolbar row.
/// Called from `title_bar::init`. The dump lives in `solutions`, which
/// cannot depend on this crate, and `Workspace::project_toolbar_item` is an
/// `AnyView` — so only this crate can turn it back into something
/// describable.
pub fn register_toolbar_structure_provider(cx: &mut App) {
    register_structure_provider(
        cx,
        StructureSlot::ProjectToolbar,
        |workspace, window, cx| {
            let toolbar = workspace
                .project_toolbar_item()?
                .downcast::<ProjectToolbar>()
                .ok()?;
            Some(toolbar.read(cx).structure_node(workspace, window, cx))
        },
    );
}

impl ProjectToolbar {
    /// The toolbar's node for `workspace.dump_visual_structure`.
    ///
    /// Every child's `visible` is computed from the same helper `render`
    /// gates that child on, so the dump cannot describe a control the
    /// toolbar is not painting — the failure mode that made
    /// `build_title_bar_node` and `build_status_bar_node` give up on
    /// synthesizing children at all. Children are emitted in painted order,
    /// left to right, including the ones currently hidden.
    ///
    /// `workspace` is the caller's already-borrowed `&Workspace` rather than
    /// `self.workspace.upgrade()`: the dump holds that borrow across this
    /// call.
    pub fn structure_node(&self, workspace: &Workspace, window: &Window, cx: &App) -> VisualNode {
        let dock_button_node = |(side, buttons): (&str, &Entity<PanelButtons>)| {
            VisualNode::new(format!("DockButtons({side})"))
                .with_visible(buttons.read(cx).has_visible_buttons(window, cx))
        };
        let mut children: Vec<VisualNode> = self
            .leading_dock_buttons()
            .into_iter()
            .map(dock_button_node)
            .collect();

        // `render` builds the strip through `ensure_project_tab_strip`, so a
        // toolbar that has never painted reports `visible: false` here even
        // though the next frame will show the strip. A live window always has
        // painted before an agent can dump it; a test that constructs the
        // toolbar without drawing it has to build the strip itself.
        children.push(
            VisualNode::new("ProjectTabStrip").with_visible(self.project_tab_strip.is_some()),
        );
        children.push(
            VisualNode::new("UpdateButton")
                .with_visible(Self::resolve_repository(&self.project, cx).is_some()),
        );

        let unpushed = self.unpushed_commits(cx);
        children.push(
            VisualNode::new("PushButton")
                .with_visible(unpushed.is_some())
                .with_attribute("ahead", unpushed.unwrap_or(0)),
        );

        let repository_selector = self.repository_selector_state(cx);
        let mut selector_node =
            VisualNode::new("RepositorySelector").with_visible(repository_selector.is_some());
        if let Some((repository, count)) = repository_selector {
            selector_node = selector_node
                .with_label(repository.read(cx).display_name().trim_end_matches('/'))
                .with_attribute("repository_count", count);
        }
        children.push(selector_node);

        let branch = self.branch_summary(cx);
        let mut branch_node = VisualNode::new("BranchWidget").with_visible(branch.is_some());
        if let Some((name, behind)) = branch {
            branch_node = branch_node
                .with_label(name.to_string())
                .with_attribute("behind", behind);
        }
        children.push(branch_node);

        children.push(
            VisualNode::new("RunConfigStrip")
                .with_visible(workspace.run_config_strip().is_some())
                // A type-erased `AnyView` slot: this crate can say whether one
                // is installed, not what it paints.
                .with_attribute("occupant_introspectable", false),
        );

        children.push(dock_button_node(self.trailing_dock_buttons()));

        VisualNode::new("ProjectToolbar").with_children(children)
    }
}

impl Render for ProjectToolbar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Late-bind `multi_workspace` if it was not available at construction
        // (mirrors `TitleBar::render`).
        if self.multi_workspace.is_none() {
            if let Some(mw) = self
                .workspace
                .upgrade()
                .and_then(|ws| ws.read(cx).multi_workspace().cloned())
            {
                self.multi_workspace = Some(mw);
            }
        }
        // Use the title-bar background (not the more saturated
        // `toolbar_background`) so this row reads as a continuation of the
        // title bar above it rather than a separate, prominent band.
        let toolbar_background = cx.theme().colors().title_bar_background;
        let border_color = cx.theme().colors().border;
        let project_tab_strip = self.ensure_project_tab_strip(cx);

        let run_config = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).run_config_strip().cloned());

        let has_leading_dock_buttons = self.has_leading_dock_buttons(window, cx);
        let (_, trailing_dock_buttons) = self.trailing_dock_buttons();
        let trailing_dock_buttons = trailing_dock_buttons.clone();

        h_flex()
            .w_full()
            .h(px(30.))
            .items_center()
            .bg(toolbar_background)
            // Top border separates this row from the solution-tab row in the
            // title bar above — needed now that the two share a background
            // (without it the project tabs visually merge into the title bar).
            // The bottom border separates it from the body below.
            .border_t_1()
            .border_b_1()
            .border_color(border_color)
            // `pl_2` (8px) puts the first dock toggle at the same inset as the
            // project panel's own header buttons directly below it. (It used to
            // be followed by a 32px spacer that pushed the first project tab
            // past the 40px vertical dock strip; that strip is gone.)
            .pl_2()
            .child(
                h_flex().gap_1().children(
                    self.leading_dock_buttons()
                        .into_iter()
                        .map(|(_, buttons)| buttons.clone().into_any_element()),
                ),
            )
            // The one separator between the leading dock toggles and the
            // project tabs; `PanelButtons` deliberately draws none of its own
            // so the per-dock groups read as a single group here. Drawn only
            // when there is in fact a group to separate: the toggles can be
            // hidden (`"project_panel": {"button": false}` and friends) and are
            // all absent while the panels load asynchronously at startup, and a
            // divider with nothing to its left is just a stray vertical rule.
            .when(has_leading_dock_buttons, |this| {
                this.child(
                    div()
                        .px_1p5()
                        .h_full()
                        .py_1()
                        .child(Divider::vertical().color(DividerColor::Border)),
                )
            })
            .when_some(project_tab_strip, |this, strip| this.child(strip))
            .child(div().flex_1())
            .child(
                h_flex()
                    .gap_1()
                    .children(
                        self.render_update_button(cx)
                            .map(IntoElement::into_any_element),
                    )
                    .children(
                        self.render_push_button(cx)
                            .map(IntoElement::into_any_element),
                    )
                    .children(
                        self.render_repository_selector(cx)
                            .map(IntoElement::into_any_element),
                    )
                    .children(
                        self.render_branch_widget(cx)
                            .map(IntoElement::into_any_element),
                    ),
            )
            .children(run_config)
            // Last child in the row, so the right dock's toggles sit flush
            // against the trailing padding — the mirror of the left dock's
            // toggle at `pl_2`, and on the side those panels open on. Placing
            // them after the run-config strip rather than inside the git
            // cluster keeps them pinned to the window edge instead of drifting
            // as the strip's width and the conditional git widgets change.
            .child(trailing_dock_buttons)
            .pr_1p5()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{Action as _, Focusable as _, TestAppContext};
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::{Path, PathBuf};
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    /// A one-member Solution whose single member worktree holds `tree`.
    /// Returns the project plus the member's root, which is also the work
    /// directory of the member's own (outermost) repository.
    async fn setup_solution_member(
        solution_name: &str,
        tree: serde_json::Value,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, PathBuf) {
        let member_path = cx.update(|cx| {
            solutions::member_repository::clear_repository_choices_for_test(cx);
            let store = solutions::SolutionStore::for_test(PathBuf::new(), cx);
            let member_path = store.update(cx, |store, cx| {
                let solution_id = store.create_for_test_minimal(solution_name, cx);
                let root = store
                    .solutions()
                    .last()
                    .expect("solution was just created")
                    .root
                    .clone();
                let member_path = root.join("member");
                let member_id =
                    store.test_add_member_with_path(solution_id, "member", member_path.clone());
                store.set_active_member(solution_id, member_id, cx);
                member_path
            });
            solutions::install_global_for_test(store, cx);
            member_path
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&member_path, tree).await;
        // The member's own repository is on a branch; anything vendored inside
        // it is left at FakeFs' default detached HEAD, which is exactly the
        // shape that used to make the title bar show a raw short sha.
        fs.set_branch_name(&member_path.join(".git"), Some("main"));

        let project = Project::test(fs.clone(), [member_path.as_path()], cx).await;
        cx.run_until_parked();
        (project, member_path)
    }

    /// `ProjectToolbar` needs a live `Workspace`, so build one around `project`
    /// and hand back the toolbar entity.
    fn toolbar_for(
        project: &Entity<Project>,
        cx: &mut TestAppContext,
    ) -> Entity<super::ProjectToolbar> {
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, cx| {
            cx.new(|cx| super::ProjectToolbar::new(workspace, None, cx))
        })
    }

    /// Like `toolbar_for`, but keeps the window context and the `Workspace`
    /// entity: `structure_node` needs both a live `Window` (dock-button
    /// visibility) and the caller's `&Workspace` (the run-config slot).
    fn toolbar_in_window<'a>(
        project: &Entity<Project>,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<super::ProjectToolbar>,
        Entity<Workspace>,
        &'a mut gpui::VisualTestContext,
    ) {
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let toolbar = workspace.update(cx, |workspace, cx| {
            cx.new(|cx| {
                super::ProjectToolbar::new(workspace, Some(multi_workspace.downgrade()), cx)
            })
        });
        (toolbar, workspace, cx)
    }

    fn kinds(node: &VisualNode) -> Vec<&str> {
        node.children
            .iter()
            .map(|child| child.kind.as_str())
            .collect()
    }

    fn child<'a>(node: &'a VisualNode, kind: &str) -> &'a VisualNode {
        node.children
            .iter()
            .find(|child| child.kind == kind)
            .unwrap_or_else(|| panic!("no {kind:?} child in {:?}", kinds(node)))
    }

    /// The dump's answer to "what does the project-toolbar row contain".
    /// Order is painted order, and every child is emitted whether or not it
    /// is currently shown, so an agent can tell "hidden" from "gone".
    #[gpui::test]
    async fn the_toolbar_structure_node_lists_its_row_in_painted_order(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, _) = setup_solution_member("toolbar-structure", nested_repo_tree(), cx).await;

        let (toolbar, workspace, cx) = toolbar_in_window(&project, cx);
        // Stands in for the first paint: the fixture never draws the toolbar,
        // and `render` is what builds the tab strip on a live window.
        toolbar.update(cx, |toolbar, cx| {
            toolbar
                .ensure_project_tab_strip(cx)
                .expect("the window has a MultiWorkspace, so the strip builds")
        });

        let node = workspace.update_in(cx, |workspace, window, cx| {
            toolbar.read(cx).structure_node(workspace, window, cx)
        });

        assert_eq!(node.kind, "ProjectToolbar");
        assert_eq!(
            kinds(&node),
            vec![
                "DockButtons(left)",
                "DockButtons(bottom)",
                "ProjectTabStrip",
                "UpdateButton",
                "PushButton",
                "RepositorySelector",
                "BranchWidget",
                "RunConfigStrip",
                // The right dock's toggles are painted last, at the trailing
                // edge, on the side the panels they open live on.
                "DockButtons(right)",
            ]
        );

        assert!(child(&node, "ProjectTabStrip").visible);
        assert!(
            child(&node, "UpdateButton").visible,
            "the member owns a repository, so the fetch+pull button renders"
        );

        let selector = child(&node, "RepositorySelector");
        assert!(selector.visible, "the member owns a vendored repo as well");
        assert_eq!(
            selector.attributes.get("repository_count"),
            Some(&serde_json::json!(2))
        );

        let branch = child(&node, "BranchWidget");
        assert!(branch.visible);
        assert_eq!(branch.label.as_deref(), Some("main"));

        let run_config = child(&node, "RunConfigStrip");
        assert!(
            !run_config.visible,
            "nothing installed the run-config slot in this test window"
        );
        assert_eq!(
            run_config.attributes.get("occupant_introspectable"),
            Some(&serde_json::json!(false)),
            "the strip is an AnyView; the dump must say it cannot see inside"
        );
    }

    /// The probe and `render` must gate on the same thing — a selector the
    /// toolbar is not painting must not read as present.
    #[gpui::test]
    async fn the_structure_node_hides_the_selector_exactly_when_render_does(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let (project, _) =
            setup_solution_member("toolbar-structure-one-repo", single_repo_tree(), cx).await;

        let (toolbar, workspace, cx) = toolbar_in_window(&project, cx);
        let node = workspace.update_in(cx, |workspace, window, cx| {
            toolbar.read(cx).structure_node(workspace, window, cx)
        });
        let rendered = toolbar.update(cx, |toolbar, cx| {
            toolbar.render_repository_selector(cx).is_some()
        });

        assert!(!rendered);
        assert_eq!(child(&node, "RepositorySelector").visible, rendered);
    }

    fn work_directory(
        repository: &Entity<project::git_store::Repository>,
        cx: &TestAppContext,
    ) -> PathBuf {
        repository.read_with(cx, |repository, _| {
            repository.work_directory_abs_path.to_path_buf()
        })
    }

    fn single_repo_tree() -> serde_json::Value {
        json!({
            ".git": {},
            "a.txt": "A\n",
        })
    }

    fn nested_repo_tree() -> serde_json::Value {
        json!({
            ".git": {},
            "a.txt": "A\n",
            "vendor": {
                "plugin": {
                    ".git": {},
                    "lib.rs": "",
                },
            },
        })
    }

    #[gpui::test]
    async fn test_repository_selector_hidden_with_one_repository(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, member_path) =
            setup_solution_member("toolbar-one-repo", single_repo_tree(), cx).await;

        let repositories = cx.update(|cx| solutions::active_member_repositories(&project, cx));
        assert_eq!(
            repositories.len(),
            1,
            "expected exactly one repository under {member_path:?}"
        );

        let toolbar = toolbar_for(&project, cx);
        let rendered = toolbar.update(cx, |toolbar, cx| {
            toolbar.render_repository_selector(cx).is_some()
        });
        assert!(
            !rendered,
            "the repository selector must not render when there is nothing to pick"
        );
    }

    #[gpui::test]
    async fn test_repository_selector_shown_with_nested_repository(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, _) =
            setup_solution_member("toolbar-nested-repo", nested_repo_tree(), cx).await;

        let repositories = cx.update(|cx| solutions::active_member_repositories(&project, cx));
        assert_eq!(
            repositories.len(),
            2,
            "expected the member repo and the vendored one"
        );

        let toolbar = toolbar_for(&project, cx);
        let rendered = toolbar.update(cx, |toolbar, cx| {
            toolbar.render_repository_selector(cx).is_some()
        });
        assert!(
            rendered,
            "the repository selector must render once the member owns more than one repository"
        );
    }

    #[gpui::test]
    async fn test_resolution_defaults_to_the_outermost_repository(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, member_path) =
            setup_solution_member("toolbar-outermost", nested_repo_tree(), cx).await;

        let repositories = cx.update(|cx| solutions::active_member_repositories(&project, cx));
        assert_eq!(repositories.len(), 2);
        assert_eq!(
            work_directory(&repositories[0], cx),
            member_path,
            "the member's own repository must sort first"
        );

        let resolved = cx
            .update(|cx| super::ProjectToolbar::resolve_repository(&project, cx))
            .expect("a repository should resolve inside a Solution");
        assert_eq!(work_directory(&resolved, cx), member_path);

        // The regression this guards: the nested plugin repo is detached, so
        // latching onto it made the title bar show a raw short sha.
        let branch = resolved.read_with(cx, |repository, _| {
            repository
                .branch
                .as_ref()
                .map(|branch| branch.name().to_string())
        });
        assert_eq!(branch.as_deref(), Some("main"));

        let nested_branch = repositories[1].read_with(cx, |repository, _| {
            repository
                .branch
                .as_ref()
                .map(|branch| branch.name().to_string())
        });
        assert_eq!(
            nested_branch, None,
            "the vendored repo is the detached one this test is about"
        );
    }

    #[gpui::test]
    async fn test_resolution_honours_an_explicit_repository_pick(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, member_path) =
            setup_solution_member("toolbar-explicit-pick", nested_repo_tree(), cx).await;

        let repositories = cx.update(|cx| solutions::active_member_repositories(&project, cx));
        assert_eq!(repositories.len(), 2);
        let nested = repositories[1].clone();
        let nested_path = work_directory(&nested, cx);
        assert_eq!(nested_path, member_path.join("vendor").join("plugin"));

        cx.update(|cx| solutions::set_active_member_repository(&project, &nested, cx));
        cx.run_until_parked();

        let resolved = cx
            .update(|cx| super::ProjectToolbar::resolve_repository(&project, cx))
            .expect("a repository should resolve inside a Solution");
        assert_eq!(
            work_directory(&resolved, cx),
            nested_path,
            "an explicit pick must win over the outermost default"
        );
    }

    #[gpui::test]
    async fn test_resolution_falls_back_outside_a_solution(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(Path::new("/plain"), single_repo_tree())
            .await;
        fs.set_branch_name(Path::new("/plain/.git"), Some("main"));
        let project = Project::test(fs.clone(), [Path::new("/plain")], cx).await;
        cx.run_until_parked();

        assert!(
            cx.update(|cx| solutions::active_member_repositories(&project, cx))
                .is_empty(),
            "no Solution means no member repositories"
        );
        let repository = cx
            .update(|cx| super::ProjectToolbar::resolve_repository(&project, cx))
            .expect("outside a Solution the plain project's repository must still resolve");
        assert_eq!(work_directory(&repository, cx), PathBuf::from("/plain"));
    }

    /// A panel that actually renders a dock button, which
    /// `workspace::dock::test::TestPanel` deliberately does not (its `icon`
    /// is `None`, so it is invisible to `has_visible_buttons`).
    struct ButtonedPanel {
        position: workspace::dock::DockPosition,
        focus_handle: gpui::FocusHandle,
    }
    gpui::actions!(title_bar_test_only, [ToggleButtonedPanel]);

    impl gpui::EventEmitter<workspace::dock::PanelEvent> for ButtonedPanel {}

    impl gpui::Focusable for ButtonedPanel {
        fn focus_handle(&self, _cx: &App) -> gpui::FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for ButtonedPanel {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().track_focus(&self.focus_handle(cx))
        }
    }

    impl workspace::dock::Panel for ButtonedPanel {
        fn persistent_name() -> &'static str {
            "ButtonedPanel"
        }

        fn panel_key() -> &'static str {
            "ButtonedPanel"
        }

        fn position(&self, _window: &Window, _cx: &App) -> workspace::dock::DockPosition {
            self.position
        }

        fn position_is_valid(&self, _position: workspace::dock::DockPosition) -> bool {
            true
        }

        fn set_position(
            &mut self,
            position: workspace::dock::DockPosition,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
            self.position = position;
        }

        fn default_size(&self, _window: &Window, _cx: &App) -> gpui::Pixels {
            px(240.)
        }

        fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
            Some(IconName::Folder)
        }

        fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
            Some("Buttoned Panel")
        }

        fn toggle_action(&self) -> Box<dyn gpui::Action> {
            ToggleButtonedPanel.boxed_clone()
        }

        fn activation_priority(&self) -> u32 {
            0
        }
    }

    /// The divider between the *leading* dock toggles and the project tabs is
    /// gated on those toggles existing. Two states leave the leading cluster
    /// empty and must not draw it: no panel loaded into any dock (both the
    /// startup state, since panels load asynchronously, and the state with
    /// every `"button": false`), and a panel loaded only into the right dock —
    /// whose toggles are painted at the far end of the row, so they leave
    /// nothing to the divider's left.
    #[gpui::test]
    async fn test_only_leading_dock_buttons_gate_the_divider(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, _) = setup_solution_member("Divider", single_repo_tree(), cx).await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let toolbar = workspace.update(cx, |workspace, cx| {
            cx.new(|cx| super::ProjectToolbar::new(workspace, None, cx))
        });

        let has_leading_dock_buttons = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| toolbar.read(cx).has_leading_dock_buttons(window, cx))
        };

        assert!(
            !has_leading_dock_buttons(cx),
            "no panel is loaded into any dock, so the leading toggle cluster \
             is empty and the divider must not draw"
        );

        workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| ButtonedPanel {
                position: workspace::dock::DockPosition::Right,
                focus_handle: cx.focus_handle(),
            });
            workspace.add_panel(panel, window, cx);
        });
        assert!(
            !has_leading_dock_buttons(cx),
            "the right dock's toggles are painted at the trailing edge, so a \
             right-dock panel leaves nothing to the divider's left"
        );

        workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| ButtonedPanel {
                position: workspace::dock::DockPosition::Left,
                focus_handle: cx.focus_handle(),
            });
            workspace.add_panel(panel, window, cx);
        });
        assert!(
            has_leading_dock_buttons(cx),
            "a left-dock panel puts a toggle at the leading edge, which is \
             exactly what the divider separates from the project tabs"
        );
    }
}
